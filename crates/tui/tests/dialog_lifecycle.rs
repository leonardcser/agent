//! Dialog lifecycle tests (vt100 harness).
//!
//! Verifies that content survives the confirm dialog open/dismiss cycle
//! and that no extra gaps are introduced.

mod harness;

use harness::TestHarness;
use tui::render::{Block, Dialog};

/// Check that no double blank lines (3+ consecutive empty lines) appear.
fn assert_no_double_gaps(text: &str, test_name: &str) {
    let lines: Vec<&str> = text.lines().collect();
    for (i, window) in lines.windows(3).enumerate() {
        if window.iter().all(|l| l.trim().is_empty()) {
            let start = i.saturating_sub(3);
            let end = (i + 6).min(lines.len());
            let context: String = lines[start..end]
                .iter()
                .enumerate()
                .map(|(j, l)| format!("{:3}│{l}", start + j + 1))
                .collect::<Vec<_>>()
                .join("\n");
            panic!(
                "{test_name}: double blank line at line {}\n\n{context}\n",
                i + 1
            );
        }
    }
}

#[test]
fn confirm_simple() {
    let mut h = TestHarness::new(80, 24, "confirm_simple");
    h.push_and_render(Block::User {
        text: "Edit the file".into(),
        image_labels: vec![],
    });
    h.push_and_render(Block::Text {
        content: "I'll edit that for you.".into(),
    });
    h.confirm_cycle("c1", "write", "Writing main.rs", "fn main() {}");
    h.assert_contains_all(&["Edit the file", "I'll edit that for you", "Writing main.rs"]);
}

#[test]
fn confirm_back_to_back() {
    let mut h = TestHarness::new(80, 24, "confirm_back_to_back");
    h.push_and_render(Block::User {
        text: "Write files".into(),
        image_labels: vec![],
    });
    for i in 0..3 {
        let id = format!("c{i}");
        h.confirm_cycle(&id, "write", &format!("file_{i}.rs"), &format!("// {i}"));
    }
    h.assert_contains_all(&["Write files", "// 0", "// 1", "// 2"]);
}

#[test]
fn tool_overlay_at_bottom_does_not_move_prompt_up() {
    // Scenario: terminal is full, prompt is at the bottom row.
    // A tool starts (Pending overlay). The prompt should not jump up —
    // it should either stay at the same row or move down (scroll).
    let height = 16;
    let mut h = TestHarness::new(80, height, "tool_overlay_no_prompt_jitter");

    // Fill the terminal so the prompt is at the very bottom.
    for i in 0..6 {
        h.push_and_render(Block::User {
            text: format!("Question {i}"),
            image_labels: vec![],
        });
        h.push_and_render(Block::Text {
            content: format!("Answer {i}"),
        });
    }
    h.draw_prompt();

    // Record the prompt bar row before the tool starts.
    let bar_before = find_bar_row(&h.parser, height);

    // Start a tool (Pending status) — this adds the overlay.
    h.screen.start_tool(
        "t1".into(),
        "bash".into(),
        "ls -la".into(),
        std::collections::HashMap::new(),
    );
    h.draw_prompt();

    // Record the prompt bar row after the tool overlay appears.
    let bar_after = find_bar_row(&h.parser, height);

    // The prompt bar must NOT have moved up.
    assert!(
        bar_after >= bar_before,
        "Prompt bar moved UP from row {bar_before} to {bar_after} when tool overlay appeared.\n\
         Screen:\n{}",
        visible_rows(&h.parser, height),
    );
}

/// Variant: tool transitions from Pending → Confirm.
/// The staged code skips rendering Confirm tools in normal mode to avoid
/// duplication. But this must not cause the prompt bar to jump up — if the
/// tool was visible, removing it from the overlay shrinks active_rows.
#[test]
fn tool_confirm_transition_does_not_move_prompt_up() {
    let height = 16;
    let mut h = TestHarness::new(80, height, "tool_confirm_no_jitter");

    // Fill terminal.
    for i in 0..6 {
        h.push_and_render(Block::User {
            text: format!("Q{i}"),
            image_labels: vec![],
        });
        h.push_and_render(Block::Text {
            content: format!("A{i}"),
        });
    }

    // Tool starts as Pending — overlay visible.
    h.screen.start_tool(
        "t1".into(),
        "bash".into(),
        "ls -la".into(),
        std::collections::HashMap::new(),
    );
    h.draw_prompt();
    let bar_pending = find_bar_row(&h.parser, height);

    // Tool transitions to Confirm (dialog about to open).
    // In the real app, a normal frame may be drawn before the dialog opens
    // (e.g., deferred dialog, or spinner tick).
    h.screen
        .set_active_status("t1", tui::render::ToolStatus::Confirm);
    h.draw_prompt();
    let bar_confirm = find_bar_row(&h.parser, height);

    assert!(
        bar_confirm >= bar_pending,
        "Prompt bar moved UP from row {bar_pending} to {bar_confirm} \
         when tool transitioned to Confirm.\n\
         Screen:\n{}",
        visible_rows(&h.parser, height),
    );
}

/// Find the row index of the prompt bar (a line starting with "─") in the
/// visible terminal area.
fn find_bar_row(parser: &vt100::Parser, height: u16) -> u16 {
    let cols = parser.screen().size().1;
    // Scan from the bottom up — the bar is usually near the bottom.
    for row in (0..height).rev() {
        let text: String = parser
            .screen()
            .rows(0, cols)
            .nth(row as usize)
            .unwrap_or_default();
        if text.starts_with('─') || text.starts_with('\u{2500}') {
            return row;
        }
    }
    panic!(
        "Could not find prompt bar in visible area.\nScreen:\n{}",
        visible_rows(parser, height),
    );
}

/// Dump all visible rows for diagnostic output.
fn visible_rows(parser: &vt100::Parser, height: u16) -> String {
    let cols = parser.screen().size().1;
    let mut out = String::new();
    for row in 0..height {
        let text: String = parser
            .screen()
            .rows(0, cols)
            .nth(row as usize)
            .unwrap_or_default();
        out.push_str(&format!("{row:2}│{text}\n"));
    }
    out
}

#[test]
fn single_gap_above_confirm_tool_overlay() {
    // When a tool call opens a confirm dialog, there should be exactly
    // one blank line between the preceding text and the tool call overlay.
    let height = 24;
    let mut h = TestHarness::new(80, height, "single_gap_above_confirm_tool");

    h.push_and_render(Block::User {
        text: "Edit the file".into(),
        image_labels: vec![],
    });
    h.push_and_render(Block::Text {
        content: "I'll edit that for you.".into(),
    });
    h.draw_prompt();

    // Start a tool with Confirm status and draw a prompt frame.
    h.screen.start_tool(
        "c1".into(),
        "write".into(),
        "main.rs".into(),
        std::collections::HashMap::new(),
    );
    h.screen
        .set_active_status("c1", tui::render::ToolStatus::Confirm);
    h.draw_prompt();

    // Check the visible screen for double gaps above the tool.
    let screen = visible_rows(&h.parser, height);

    // Find the tool line.
    let cols = h.parser.screen().size().1;
    let mut tool_row = None;
    for row in 0..height {
        let text: String = h
            .parser
            .screen()
            .rows(0, cols)
            .nth(row as usize)
            .unwrap_or_default();
        if text.contains("write") && text.contains("main.rs") {
            tool_row = Some(row);
            break;
        }
    }
    let tool_row =
        tool_row.unwrap_or_else(|| panic!("Could not find tool line in screen:\n{screen}"));

    // Count consecutive blank lines immediately above the tool.
    let mut blanks_above = 0u16;
    for row in (0..tool_row).rev() {
        let text: String = h
            .parser
            .screen()
            .rows(0, cols)
            .nth(row as usize)
            .unwrap_or_default();
        if text.trim().is_empty() {
            blanks_above += 1;
        } else {
            break;
        }
    }

    assert_eq!(
        blanks_above, 1,
        "Expected 1 blank line above tool overlay (before dialog), found {blanks_above}.\n\
         Screen:\n{screen}"
    );

    // Now run the full confirm cycle and check the committed block.
    h.confirm_cycle("c1", "write", "main.rs", "fn main() {}");

    let screen_after = visible_rows(&h.parser, height);
    let mut tool_row_after = None;
    for row in 0..height {
        let text: String = h
            .parser
            .screen()
            .rows(0, cols)
            .nth(row as usize)
            .unwrap_or_default();
        if text.contains("write") && text.contains("main.rs") {
            tool_row_after = Some(row);
            break;
        }
    }
    let tool_row_after = tool_row_after
        .unwrap_or_else(|| panic!("Could not find committed tool in screen:\n{screen_after}"));

    let mut blanks_after = 0u16;
    for row in (0..tool_row_after).rev() {
        let text: String = h
            .parser
            .screen()
            .rows(0, cols)
            .nth(row as usize)
            .unwrap_or_default();
        if text.trim().is_empty() {
            blanks_after += 1;
        } else {
            break;
        }
    }

    assert_eq!(
        blanks_after, 1,
        "Expected 1 blank line above committed tool block, found {blanks_after}.\n\
         Screen:\n{screen_after}"
    );
}

#[test]
fn no_double_gap_nearly_full_terminal() {
    let mut h = TestHarness::new(80, 24, "no_double_gap_nearly_full_terminal");
    for i in 0..5 {
        h.push_and_render(Block::User {
            text: format!("Q{i}"),
            image_labels: vec![],
        });
        h.push_and_render(Block::Text {
            content: format!("A{i}"),
        });
    }
    h.draw_prompt();
    h.confirm_cycle("c1", "bash", "cmd", "output");

    let text = h.full_text();
    assert_no_double_gaps(&text, "no_double_gap_nearly_full_terminal");
}

/// With parallel tool calls, the confirm dialog overlay shows every
/// active tool. The viewport reserves dialog height + 1 (status bar)
/// and the overlay tail-crops above that band.
#[test]
fn parallel_tools_all_visible_during_dialog() {
    let height = 24;
    let mut h = TestHarness::new(80, height, "parallel_tools_all_visible_during_dialog");

    h.push_and_render(Block::Text {
        content: "I'll run three commands.".into(),
    });
    h.draw_prompt();

    // Start 3 parallel tools.
    for (id, summary) in [
        ("c1", "MARKER_AAA"),
        ("c2", "MARKER_BBB"),
        ("c3", "MARKER_CCC"),
    ] {
        h.screen.start_tool(
            id.into(),
            "bash".into(),
            summary.into(),
            std::collections::HashMap::new(),
        );
    }
    // c2 needs confirmation — the other two are still pending.
    h.screen
        .set_active_status("c2", tui::render::ToolStatus::Confirm);
    h.screen.mark_blocks_dirty();
    h.drain_sink();

    // Open confirm dialog for c2.
    let req = tui::render::ConfirmRequest {
        call_id: "c2".into(),
        tool_name: "bash".into(),
        desc: "MARKER_BBB".into(),
        args: std::collections::HashMap::new(),
        approval_patterns: vec![],
        outside_dir: None,
        summary: Some("MARKER_BBB".into()),
        request_id: 2,
    };
    let mut dialog = tui::render::ConfirmDialog::new(&req, false);
    dialog.set_term_size(80, height);

    h.screen.mark_blocks_dirty();
    h.screen.erase_prompt();
    let dialog_height = dialog.height();
    h.screen.set_dialog_open(true);
    h.screen.set_constrain_dialog(dialog.constrain_height());
    {
        let mut frame = tui::render::Frame::begin(h.screen.backend());
        let (_redirtied, placement) =
            h.screen
                .draw_viewport_dialog_frame(&mut frame, 80, dialog_height);
        if let Some(p) = placement {
            dialog.draw(&mut frame, p.row, 80, p.granted_rows);
        }
    }
    h.drain_sink();

    let text = h.full_text();

    // Every parallel tool is visible above the dialog.
    for marker in ["MARKER_AAA", "MARKER_BBB", "MARKER_CCC"] {
        assert!(
            text.contains(marker),
            "Parallel tool {marker} should be visible during dialog.\n{text}"
        );
    }

    // After dialog dismiss, every tool is still there.
    h.screen.clear_dialog_area();
    h.drain_sink();
    h.draw_prompt();

    let text_after = h.full_text();
    for marker in ["MARKER_AAA", "MARKER_BBB", "MARKER_CCC"] {
        assert!(
            text_after.contains(marker),
            "All tools should remain after dialog dismiss. Missing: {marker}\n{text_after}"
        );
    }
}

/// Focus toggles while a dialog is open must be byte-for-byte no-ops.
/// Any output at all flashes the dialog (the bottom-gap cleanup clears
/// the screen below the dialog's anchor row).
#[test]
fn focus_toggle_while_dialog_open_emits_nothing() {
    let mut h = TestHarness::new(80, 16, "focus_toggle_while_dialog_open");

    h.push_and_render(Block::User {
        text: "Write a poem".into(),
        image_labels: vec![],
    });
    h.push_and_render(Block::Text {
        content: "Sure, here it is.".into(),
    });
    h.draw_prompt();

    let file_content = (1..=30)
        .map(|i| format!("Line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut args = std::collections::HashMap::new();
    args.insert(
        "file_path".into(),
        serde_json::Value::String("poem.txt".into()),
    );
    args.insert("content".into(), serde_json::Value::String(file_content));
    h.screen.start_tool(
        "c1".into(),
        "write_file".into(),
        "poem.txt".into(),
        args.clone(),
    );
    let mut dialog = h.open_confirm_dialog_with_args("c1", "write_file", "poem.txt", args);

    let before = harness::visible_content(&h.parser);

    // Toggle focus. Since the dialog is open and nothing about the
    // dialog depends on focus, this must emit zero output.
    h.screen.set_focused(false);
    h.draw_dialog_tick(&mut dialog);
    let bytes_after_unfocus = h.take_sink_bytes();
    assert!(
        bytes_after_unfocus.is_empty(),
        "focus-lost while dialog open emitted {} bytes:\n{}",
        bytes_after_unfocus.len(),
        String::from_utf8_lossy(&bytes_after_unfocus)
    );

    h.screen.set_focused(true);
    h.draw_dialog_tick(&mut dialog);
    let bytes_after_refocus = h.take_sink_bytes();
    assert!(
        bytes_after_refocus.is_empty(),
        "focus-gained while dialog open emitted {} bytes:\n{}",
        bytes_after_refocus.len(),
        String::from_utf8_lossy(&bytes_after_refocus)
    );

    let after = harness::visible_content(&h.parser);
    assert_eq!(before, after, "dialog viewport changed after focus toggle");
}
