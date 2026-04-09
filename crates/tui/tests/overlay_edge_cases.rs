//! Integration tests for the ephemeral overlay / live region.
//!
//! These target edge cases around live content (streaming text, active
//! tools, active exec, parallel tool calls, mid-stream resizes, long
//! commits) that aren't covered by the scrollback/dialog-lifecycle
//! suites. Each test drives the `Screen` through a realistic sequence
//! of events, feeds the bytes into a `vt100::Parser`, and inspects
//! either the visible viewport or the full viewport+scrollback text.

mod harness;

use harness::{visible_content, TestHarness};
use std::collections::HashMap;
use std::time::Duration;
use tui::render::{Block, ToolOutput, ToolStatus};

fn visible(h: &mut TestHarness) -> String {
    h.draw_prompt();
    visible_content(&h.parser)
}

fn make_output(content: &str) -> Option<Box<ToolOutput>> {
    Some(Box::new(ToolOutput {
        content: content.into(),
        is_error: false,
        metadata: None,
        render_cache: None,
    }))
}

fn lines_range(start: usize, end: usize) -> String {
    (start..=end)
        .map(|i| format!("line_{i:03}"))
        .collect::<Vec<_>>()
        .join("\n")
}

// ── Tests ───────────────────────────────────────────────────────────

/// Two bash tools running in parallel, each with growing output.
/// Both should be visible in the viewport (as long as the combined
/// height fits in the budget). Neither should leak into scrollback.
#[test]
fn parallel_bash_tools_both_visible_when_fits() {
    let mut h = TestHarness::new(80, 40, "parallel_bash_tools_both_visible_when_fits");
    h.push_and_render(Block::User {
        text: "run two things".into(),
        image_labels: vec![],
    });
    h.draw_prompt();

    h.screen
        .start_tool("c1".into(), "bash".into(), "echo A".into(), HashMap::new());
    h.screen
        .start_tool("c2".into(), "bash".into(), "echo B".into(), HashMap::new());
    // Stream a few lines into each.
    h.screen.append_active_output("c1", "AAA_line1");
    h.screen.append_active_output("c1", "AAA_line2");
    h.screen.append_active_output("c1", "AAA_line3");
    h.screen.append_active_output("c2", "BBB_line1");
    h.screen.append_active_output("c2", "BBB_line2");
    h.screen.append_active_output("c2", "BBB_line3");

    let v = visible(&mut h);
    // Both tool summaries visible.
    assert!(v.contains("echo A"), "tool1 summary missing:\n{v}");
    assert!(v.contains("echo B"), "tool2 summary missing:\n{v}");
    // Output of each visible.
    assert!(v.contains("AAA_line3"), "tool1 tail output missing:\n{v}");
    assert!(v.contains("BBB_line3"), "tool2 tail output missing:\n{v}");
}

/// Three parallel tools with output that together exceeds the viewport.
/// The oldest rows should be cropped from the top; the tail stays
/// visible. No duplication, no garbage.
#[test]
fn parallel_bash_tools_cropped_when_overflow() {
    let mut h = TestHarness::new(80, 20, "parallel_bash_tools_cropped_when_overflow");
    h.push_and_render(Block::User {
        text: "run three big things".into(),
        image_labels: vec![],
    });
    h.draw_prompt();

    for cid in ["c1", "c2", "c3"] {
        h.screen.start_tool(
            cid.into(),
            "bash".into(),
            format!("echo {cid}"),
            HashMap::new(),
        );
        // Each tool gets 10 lines of output (10 × 3 = 30 rows, plus
        // headers/gaps ⇒ well over a 20-row terminal).
        for i in 0..10 {
            h.screen
                .append_active_output(cid, &format!("{cid}_out_{i:02}"));
        }
    }

    let v = visible(&mut h);
    // The LAST tool's tail must be visible (it's the freshest content).
    assert!(
        v.contains("c3_out_09"),
        "most-recent tool tail missing:\n{v}"
    );
    // The FIRST tool's early output should be cropped away — tail-crop
    // drops oldest rows first. We expect c1_out_00 not to be visible.
    assert!(
        !v.contains("c1_out_00"),
        "oldest row should be cropped from overlay:\n{v}"
    );
    // Prompt bar still visible at the bottom.
    assert!(v.contains("normal"), "prompt/status missing:\n{v}");
}

/// A single tool whose output streams past the viewport. The committed
/// block cap (`MAX_VISUAL_ROWS = 20`) still applies — the overlay
/// shows at most 20 data rows + the header.
#[test]
fn single_bash_tool_long_output_capped() {
    let mut h = TestHarness::new(80, 40, "single_bash_tool_long_output_capped");
    h.push_and_render(Block::User {
        text: "ls -la".into(),
        image_labels: vec![],
    });
    h.draw_prompt();

    h.screen
        .start_tool("c1".into(), "bash".into(), "ls -la".into(), HashMap::new());
    // Append 50 lines of output.
    for i in 0..50 {
        h.screen
            .append_active_output("c1", &format!("out_line_{i:02}"));
    }

    let v = visible(&mut h);
    // The tool's 20-row cap should keep only the most recent 20.
    assert!(v.contains("out_line_49"), "last output line missing:\n{v}");
    // The oldest lines should not be in the overlay window.
    assert!(
        !v.contains("out_line_00"),
        "oldest output should be dropped by tool cap:\n{v}"
    );
    // And the skipped-count marker should be present.
    assert!(
        v.contains("above"),
        "truncation marker missing from output:\n{v}"
    );
}

/// Finishing a streaming tool and then starting a second one in the
/// same frame should not duplicate the first tool. The committed block
/// replaces the live overlay cleanly.
#[test]
fn tool_finish_then_new_tool_no_duplication() {
    let mut h = TestHarness::new(80, 30, "tool_finish_then_new_tool_no_duplication");
    h.push_and_render(Block::User {
        text: "run two sequential".into(),
        image_labels: vec![],
    });
    h.draw_prompt();

    // Tool 1.
    h.screen.start_tool(
        "c1".into(),
        "bash".into(),
        "first_cmd".into(),
        HashMap::new(),
    );
    h.screen.append_active_output("c1", "tool1_line_A");
    h.draw_prompt();

    h.screen.finish_tool(
        "c1",
        ToolStatus::Ok,
        make_output("tool1_line_A\ntool1_line_B"),
        Some(Duration::from_millis(100)),
    );
    h.screen.flush_blocks();
    h.drain_sink();

    // Tool 2.
    h.screen.start_tool(
        "c2".into(),
        "bash".into(),
        "second_cmd".into(),
        HashMap::new(),
    );
    h.screen.append_active_output("c2", "tool2_line_X");

    let v = visible(&mut h);
    assert!(v.contains("first_cmd"), "tool1 summary missing:\n{v}");
    assert!(v.contains("second_cmd"), "tool2 summary missing:\n{v}");
    assert!(v.contains("tool2_line_X"), "tool2 output missing:\n{v}");
    // tool1 should appear exactly once (just the committed block).
    let full = h.full_text();
    assert_eq!(
        full.matches("first_cmd").count(),
        1,
        "tool1 summary duplicated after commit+new tool:\n{full}"
    );
}

/// Active exec (the `! command` path) with streaming output must
/// render live in the overlay and not leak into scrollback as it grows.
#[test]
fn active_exec_streams_into_overlay() {
    let mut h = TestHarness::new(80, 25, "active_exec_streams_into_overlay");
    h.push_and_render(Block::User {
        text: "!ls".into(),
        image_labels: vec![],
    });
    h.draw_prompt();

    h.screen.start_exec("ls -la".into());
    for i in 0..15 {
        h.screen.append_exec_output(&format!("exec_line_{i:02}"));
    }

    let v = visible(&mut h);
    // Header (command) visible.
    assert!(v.contains("ls -la"), "exec command missing:\n{v}");
    // Tail of output visible.
    assert!(v.contains("exec_line_14"), "exec tail missing:\n{v}");
}

/// Width resize during streaming must keep content coherent: the
/// redraw re-lays-out committed blocks at the new width.
#[test]
fn width_resize_during_streaming_keeps_content() {
    let mut h = TestHarness::new(120, 30, "width_resize_during_streaming_keeps_content");
    h.push_and_render(Block::User {
        text: "long streaming answer".into(),
        image_labels: vec![],
    });
    h.draw_prompt();

    // Start streaming text.
    h.screen
        .append_streaming_text("This is a first paragraph of the streamed text.\n");
    h.draw_prompt();
    h.drain_sink();

    // Width resize to 60 cols forces re-layout at the new width.
    h.resize(60, 30);
    h.drain_sink();

    // Continue streaming.
    h.screen
        .append_streaming_text("More content arrives after the width changed.\n");
    h.draw_prompt();
    h.drain_sink();

    let full = h.full_text();
    assert!(
        full.contains("long streaming answer"),
        "committed user message lost after width resize:\n{full}"
    );
    // The assistant content from both phases is present.
    assert!(
        full.contains("first paragraph"),
        "pre-resize streamed text lost:\n{full}"
    );
    assert!(
        full.contains("More content"),
        "post-resize streamed text missing:\n{full}"
    );
}

/// Commit a tall block (many output lines) after streaming. The
/// committed block may overflow the viewport; it should render in
/// scroll mode and go into scrollback naturally.
#[test]
fn tall_tool_commit_into_scrollback() {
    let mut h = TestHarness::new(80, 14, "tall_tool_commit_into_scrollback");
    h.push_and_render(Block::User {
        text: "run it".into(),
        image_labels: vec![],
    });
    h.draw_prompt();

    // A tool with 30 lines of output (> 20 cap → displays the last 20).
    let long_output = lines_range(1, 30);
    h.screen
        .start_tool("c1".into(), "bash".into(), "big_cmd".into(), HashMap::new());
    h.screen
        .finish_tool("c1", ToolStatus::Ok, make_output(&long_output), None);
    h.screen.flush_blocks();
    h.drain_sink();
    h.draw_prompt();

    let full = h.full_text();
    // Tool block visible (capped output).
    assert!(
        full.contains("big_cmd"),
        "tool header missing after commit:\n{full}"
    );
    // The tail of the output is what's shown.
    assert!(
        full.contains("line_030"),
        "last output line missing:\n{full}"
    );
    // User message still somewhere in viewport or scrollback.
    assert!(
        full.contains("run it"),
        "user message lost across commit:\n{full}"
    );
    // No duplicate tool header (scroll-mode commit shouldn't duplicate).
    assert_eq!(
        full.matches("big_cmd").count(),
        1,
        "tool header duplicated:\n{full}"
    );
}

/// Streaming paragraph commits mid-stream (blank line → commit), then
/// more streams. The committed paragraph should be visible along with
/// the new streaming content.
#[test]
fn mid_stream_paragraph_commit_then_more_streaming() {
    let mut h = TestHarness::new(80, 25, "mid_stream_paragraph_commit_then_more_streaming");
    h.push_and_render(Block::User {
        text: "tell me a story".into(),
        image_labels: vec![],
    });
    h.draw_prompt();

    // First paragraph.
    h.screen
        .append_streaming_text("First paragraph, some words.\n");
    h.draw_prompt();
    // Blank line commits the first paragraph to history.
    h.screen.append_streaming_text("\n");
    // Second paragraph starts streaming.
    h.screen
        .append_streaming_text("Second paragraph continues the tale.\n");
    h.draw_prompt();

    let v = visible(&mut h);
    assert!(
        v.contains("First paragraph"),
        "first committed paragraph missing:\n{v}"
    );
    assert!(
        v.contains("Second paragraph"),
        "second streaming paragraph missing:\n{v}"
    );
    // Each should appear exactly once (no streaming+committed duplicate).
    let full = h.full_text();
    assert_eq!(
        full.matches("First paragraph").count(),
        1,
        "first paragraph duplicated:\n{full}"
    );
    assert_eq!(
        full.matches("Second paragraph").count(),
        1,
        "second paragraph duplicated:\n{full}"
    );
}

/// Narrow terminal with streaming content — make sure wrapping and
/// cropping both work at small widths.
#[test]
fn narrow_terminal_streaming_overlay() {
    let mut h = TestHarness::new(30, 20, "narrow_terminal_streaming_overlay");
    h.push_and_render(Block::User {
        text: "narrow".into(),
        image_labels: vec![],
    });
    h.draw_prompt();

    h.screen.append_streaming_text(
        "This is an intentionally long line that should wrap at a small width.\n",
    );
    h.draw_prompt();

    let v = visible(&mut h);
    assert!(
        v.contains("wrap") || v.contains("narrow"),
        "streamed text missing at narrow width:\n{v}"
    );
}

/// Grow the terminal height while committed content already overflows
/// the viewport. No row of committed history should appear twice in
/// the combined (scrollback + viewport) output.
#[test]
fn grow_height_with_overflowing_committed_no_duplicates() {
    let mut h = TestHarness::new(
        80,
        14,
        "grow_height_with_overflowing_committed_no_duplicates",
    );
    // Push enough content that committed history overflows the 14-row
    // viewport — earlier rows end up in scrollback via scroll-mode
    // rendering.
    for i in 0..10 {
        h.push_and_render(Block::User {
            text: format!("USER_MSG_{i:02}"),
            image_labels: vec![],
        });
        h.push_and_render(Block::Text {
            content: format!("REPLY_{i:02}"),
        });
    }
    h.draw_prompt();
    h.drain_sink();

    // Grow the viewport — should NOT duplicate any rows.
    h.resize_then_tick_prompt(80, 40);

    let full = h.full_text();
    for i in 0..10 {
        let user = format!("USER_MSG_{i:02}");
        let reply = format!("REPLY_{i:02}");
        assert_eq!(
            full.matches(&user).count(),
            1,
            "user message {user} duplicated after height grow:\n{full}"
        );
        assert_eq!(
            full.matches(&reply).count(),
            1,
            "reply {reply} duplicated after height grow:\n{full}"
        );
    }
}

/// Shrink + grow cycle with overflowing committed content. Every
/// committed row should still appear exactly once after the cycle.
#[test]
fn shrink_then_grow_with_overflowing_committed_no_duplicates() {
    let mut h = TestHarness::new(
        80,
        14,
        "shrink_then_grow_with_overflowing_committed_no_duplicates",
    );
    for i in 0..10 {
        h.push_and_render(Block::User {
            text: format!("USER_MSG_{i:02}"),
            image_labels: vec![],
        });
        h.push_and_render(Block::Text {
            content: format!("REPLY_{i:02}"),
        });
    }
    h.draw_prompt();
    h.drain_sink();

    h.resize_then_tick_prompt(80, 8);
    h.resize_then_tick_prompt(80, 30);

    let full = h.full_text();
    for i in 0..10 {
        let user = format!("USER_MSG_{i:02}");
        assert_eq!(
            full.matches(&user).count(),
            1,
            "user message {user} duplicated after shrink+grow:\n{full}"
        );
    }
}

/// Incremental grow (several small height increases) must not
/// duplicate committed rows on each step.
#[test]
fn stepwise_grow_height_no_duplicates() {
    let mut h = TestHarness::new(80, 12, "stepwise_grow_height_no_duplicates");
    for i in 0..8 {
        h.push_and_render(Block::User {
            text: format!("STEP_USER_{i:02}"),
            image_labels: vec![],
        });
        h.push_and_render(Block::Text {
            content: format!("STEP_REPLY_{i:02}"),
        });
    }
    h.draw_prompt();
    h.drain_sink();

    for new_h in [16u16, 20, 24, 28] {
        h.resize_then_tick_prompt(80, new_h);
    }

    let full = h.full_text();
    for i in 0..8 {
        let user = format!("STEP_USER_{i:02}");
        assert_eq!(
            full.matches(&user).count(),
            1,
            "STEP_USER_{i:02} duplicated after stepwise grow:\n{full}"
        );
    }
}

/// When an overlay is shrinking (e.g., a tool commits and a second
/// active tool remains smaller), the stale rows from the previous
/// taller overlay must be cleared.
#[test]
fn overlay_shrinks_clears_stale_rows() {
    let mut h = TestHarness::new(80, 30, "overlay_shrinks_clears_stale_rows");
    h.push_and_render(Block::User {
        text: "three tools".into(),
        image_labels: vec![],
    });
    h.draw_prompt();

    // Three parallel tools with distinct markers.
    for cid in ["c1", "c2", "c3"] {
        h.screen.start_tool(
            cid.into(),
            "bash".into(),
            format!("cmd_{cid}"),
            HashMap::new(),
        );
        h.screen.append_active_output(cid, &format!("output_{cid}"));
    }
    h.draw_prompt();

    // Finish the first two — overlay shrinks to just c3.
    h.screen.finish_tool(
        "c1",
        ToolStatus::Ok,
        make_output("final_c1"),
        Some(Duration::from_millis(50)),
    );
    h.screen.finish_tool(
        "c2",
        ToolStatus::Ok,
        make_output("final_c2"),
        Some(Duration::from_millis(50)),
    );
    h.screen.flush_blocks();
    h.drain_sink();
    h.draw_prompt();

    let v = visible(&mut h);
    // c3 still active, should be visible.
    assert!(v.contains("cmd_c3"), "remaining tool missing:\n{v}");
    // Committed c1 / c2 visible (from history).
    assert!(v.contains("cmd_c1"), "committed tool1 missing:\n{v}");
    assert!(v.contains("cmd_c2"), "committed tool2 missing:\n{v}");
    // No duplicates.
    let full = h.full_text();
    for cid in ["c1", "c2", "c3"] {
        assert_eq!(
            full.matches(&format!("cmd_{cid}")).count(),
            1,
            "tool {cid} duplicated after shrink:\n{full}"
        );
    }
}
