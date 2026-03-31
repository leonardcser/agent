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
fn confirm_with_scrollback() {
    let mut h = TestHarness::new(80, 24, "confirm_with_scrollback");
    for i in 0..8 {
        h.push_and_render(Block::User {
            text: format!("Msg {i}"),
            image_labels: vec![],
        });
        h.push_and_render(Block::Text {
            content: format!("Reply {i}"),
        });
    }
    h.confirm_cycle("c1", "bash", "cmd", "output");

    let mut expected: Vec<String> = Vec::new();
    for i in 0..8 {
        expected.push(format!("Msg {i}"));
        expected.push(format!("Reply {i}"));
    }
    let refs: Vec<&str> = expected.iter().map(|s| s.as_str()).collect();
    h.assert_contains_all(&refs);
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
fn no_double_gap_after_confirm() {
    let mut h = TestHarness::new(80, 24, "no_double_gap_after_confirm");
    h.push_and_render(Block::User {
        text: "Edit".into(),
        image_labels: vec![],
    });
    h.push_and_render(Block::Text {
        content: "Sure.".into(),
    });
    h.confirm_cycle("c1", "write", "main.rs", "fn main() {}");
    h.push_and_render(Block::Text {
        content: "Done.".into(),
    });

    let text = h.full_text();
    assert_no_double_gaps(&text, "no_double_gap_after_confirm");
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

/// Variant: model is streaming text, then starts a tool call.
/// The streaming text gets committed to history and the tool overlay appears.
/// The prompt bar must not jump up during this transition.
#[test]
fn tool_after_streaming_does_not_move_prompt_up() {
    let height = 16;
    let mut h = TestHarness::new(80, height, "tool_after_streaming_no_jitter");

    // Fill terminal with conversation.
    for i in 0..5 {
        h.push_and_render(Block::User {
            text: format!("Question {i}"),
            image_labels: vec![],
        });
        h.push_and_render(Block::Text {
            content: format!("Answer {i}"),
        });
    }

    // User asks another question.
    h.push_and_render(Block::User {
        text: "Do something".into(),
        image_labels: vec![],
    });

    // Model streams a response with multiple lines.
    h.screen
        .append_streaming_text("Sure, I'll run that command.\n");
    h.draw_prompt();
    let bar_streaming = find_bar_row(&h.parser, height);

    // Model finishes text and starts a tool call — the streaming text
    // gets committed to history and the tool overlay appears.
    h.screen.flush_streaming_text();
    h.screen.render_pending_blocks();
    h.drain_sink();
    h.screen.start_tool(
        "t1".into(),
        "bash".into(),
        "ls -la".into(),
        std::collections::HashMap::new(),
    );
    h.draw_prompt();
    let bar_after_tool = find_bar_row(&h.parser, height);

    assert!(
        bar_after_tool >= bar_streaming,
        "Prompt bar moved UP from row {bar_streaming} to {bar_after_tool} \
         when streaming text was committed and tool overlay appeared.\n\
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

/// When the dialog is so tall that the tool overlay can't be shown
/// (show_tool_in_dialog=false), the committed tool block after the dialog
/// should still have exactly 1 blank line above it — not 2.
#[test]
fn no_double_gap_when_overlay_hidden() {
    // Terminal where tool+dialog doesn't fit → overlay hidden, but
    // enough room that the dialog doesn't need much ScrollUp.
    let height = 11;
    let mut h = TestHarness::new(80, height, "no_double_gap_overlay_hidden");

    h.push_and_render(Block::Text {
        content: "Sure.".into(),
    });
    // Call draw_prompt to establish anchor, then manually do confirm
    // cycle WITHOUT the extra draw_prompt at the start of confirm_cycle.
    h.draw_prompt();

    // Manual confirm cycle (no extra draw_prompt at start).
    h.screen.start_tool(
        "c1".into(),
        "bash".into(),
        "ls".into(),
        std::collections::HashMap::new(),
    );
    h.screen
        .set_active_status("c1", tui::render::ToolStatus::Confirm);
    h.screen.render_pending_blocks();
    h.drain_sink();

    let req = tui::render::ConfirmRequest {
        call_id: "c1".into(),
        tool_name: "bash".into(),
        desc: "ls".into(),
        args: std::collections::HashMap::new(),
        approval_patterns: vec![],
        outside_dir: None,
        summary: Some("ls".into()),
        request_id: 1,
    };
    let mut dialog = tui::render::ConfirmDialog::new(&req, false);
    dialog.set_term_size(80, height);

    h.screen.render_pending_blocks_for_dialog();
    h.screen.erase_prompt_nosync();
    let fits = h.screen.tool_overlay_fits_with_dialog(dialog.height());
    h.screen.set_show_tool_in_dialog(fits);
    h.screen.draw_frame(80, None);
    h.drain_sink();

    let sync = h.screen.take_sync_started();
    let dr = h.screen.dialog_row();
    dialog.draw(dr, sync, h.screen.backend());
    h.drain_sink();
    let da = dialog.anchor_row();
    h.screen.sync_dialog_anchor(da);
    h.drain_sink();

    h.screen.clear_dialog_area(da);
    h.drain_sink();
    h.screen.finish_tool(
        "c1",
        tui::render::ToolStatus::Ok,
        Some(tui::render::ToolOutput {
            content: "output".into(),
            is_error: false,
            metadata: None,
        }),
        Some(std::time::Duration::from_millis(100)),
    );
    h.screen.flush_blocks();
    h.drain_sink();
    h.draw_prompt();

    let text = h.full_text();

    // Find the tool line and count blank lines above it.
    let lines: Vec<&str> = text.lines().collect();
    let tool_idx = lines
        .iter()
        .position(|l| l.contains("bash") && l.contains("ls"))
        .unwrap_or_else(|| panic!("Could not find tool line in output:\n{text}"));

    let mut blanks = 0;
    for i in (0..tool_idx).rev() {
        if lines[i].trim().is_empty() {
            blanks += 1;
        } else {
            break;
        }
    }

    let vrows = visible_rows(&h.parser, height);
    let sb = {
        h.parser.screen_mut().set_scrollback(usize::MAX);
        let n = h.parser.screen().scrollback();
        h.parser.screen_mut().set_scrollback(0);
        n
    };
    assert_eq!(
        blanks,
        1,
        "Expected 1 blank line above tool when overlay was hidden, found {blanks}.\n\
         fits={fits}, dialog_height={}, scrollback={sb}\n\
         Full output:\n{text}\n\nVisible rows:\n{vrows}",
        dialog.height()
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
