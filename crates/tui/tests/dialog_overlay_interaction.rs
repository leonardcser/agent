//! Integration tests for the dialog ↔ overlay interaction.
//!
//! These exercise the unified overlay path in dialog mode: parallel
//! tools all visible during a confirm dialog, multi-line bash commands
//! tail-cropping above the dialog, fullscreen dialogs that collapse the
//! overlay entirely, streaming thinking/text alongside dialogs, and
//! resize cases.

mod harness;

use harness::TestHarness;
use tui::render::{Block, Dialog};

// ── Tests ───────────────────────────────────────────────────────────

/// Three parallel bash tools, plus a confirm dialog. All three tool
/// summaries should be visible above the dialog — no single-tool
/// filter, no fit-check.
#[test]
fn dialog_with_three_parallel_tools_all_visible() {
    let mut h = TestHarness::new(80, 30, "dialog_with_three_parallel_tools_all_visible");
    h.push_and_render(Block::User {
        text: "run three things".into(),
        image_labels: vec![],
    });
    h.draw_prompt();

    h.start_bash_tool("c1", "MARKER_AAA");
    h.start_bash_tool("c2", "MARKER_BBB");
    h.start_bash_tool("c3", "MARKER_CCC");

    let (_dialog, _da) = h.open_confirm_dialog("c2", "bash", "MARKER_BBB");

    let v = h.visible();
    for marker in ["MARKER_AAA", "MARKER_BBB", "MARKER_CCC"] {
        assert!(
            v.contains(marker),
            "{marker} should be visible above dialog:\n{v}"
        );
    }
}

/// A bash tool with a multi-line command (header spans many rows)
/// rendered alongside a confirm dialog. The tool header should appear,
/// possibly tail-cropped, but the dialog must still fit cleanly below.
#[test]
fn dialog_with_multiline_bash_command() {
    let mut h = TestHarness::new(80, 30, "dialog_with_multiline_bash_command");
    h.push_and_render(Block::User {
        text: "run a multi-line script".into(),
        image_labels: vec![],
    });
    h.draw_prompt();

    let multi_line = "for i in 1 2 3; do\n  echo line_$i\n  sleep 1\ndone\necho all_done";
    h.start_bash_tool("c1", multi_line);

    let (_dialog, _da) = h.open_confirm_dialog("c1", "bash", multi_line);

    let v = h.visible();
    // The most recent line of the command (the tail) should be visible.
    assert!(
        v.contains("all_done"),
        "tail of multi-line command missing:\n{v}"
    );
    // The dialog confirm prompt should also be visible.
    assert!(
        v.to_lowercase().contains("yes") || v.to_lowercase().contains("confirm"),
        "dialog confirm hint missing:\n{v}"
    );
}

/// Multiple multi-line bash tool commands together overflow the
/// viewport. The overlay should tail-crop above the dialog so the
/// dialog is fully visible — and the freshest content (latest tool's
/// tail) should be the part that survives.
#[test]
fn dialog_with_overflowing_parallel_multiline_tools() {
    let mut h = TestHarness::new(80, 20, "dialog_with_overflowing_parallel_multiline_tools");
    h.push_and_render(Block::User {
        text: "many big things".into(),
        image_labels: vec![],
    });
    h.draw_prompt();

    // Each tool: 6-line command. Three of them ⇒ 18+ rows of overlay
    // alone, well past 20-row viewport once the dialog is reserved.
    let mk_cmd = |tag: &str| {
        format!("FIRST_{tag}\nSECOND_{tag}\nTHIRD_{tag}\nFOURTH_{tag}\nFIFTH_{tag}\nLAST_{tag}")
    };
    h.start_bash_tool("c1", &mk_cmd("AAA"));
    h.start_bash_tool("c2", &mk_cmd("BBB"));
    h.start_bash_tool("c3", &mk_cmd("CCC"));

    let (_dialog, _da) = h.open_confirm_dialog("c3", "bash", &mk_cmd("CCC"));

    let v = h.visible();
    // The last tool's tail should be present (freshest).
    assert!(v.contains("LAST_CCC"), "freshest tool tail missing:\n{v}");
    // The first tool's first line should be cropped away.
    assert!(
        !v.contains("FIRST_AAA"),
        "oldest line should be cropped:\n{v}"
    );
    // No content from the dialog band should bleed into scrollback —
    // i.e. the visible viewport contains the dialog hint.
    assert!(
        v.to_lowercase().contains("yes") || v.to_lowercase().contains("confirm"),
        "dialog hint missing:\n{v}"
    );
}

/// Streaming assistant text + an active tool, all visible while a
/// dialog is open. None of the streaming overlay content should leak
/// into terminal scrollback.
#[test]
fn dialog_with_streaming_text_and_tool() {
    let mut h = TestHarness::new(80, 30, "dialog_with_streaming_text_and_tool");
    h.push_and_render(Block::User {
        text: "stream + tool".into(),
        image_labels: vec![],
    });
    h.draw_prompt();

    h.screen
        .append_streaming_text("Live streaming paragraph from the assistant.\n");
    h.start_bash_tool("c1", "echo hi");

    let (_dialog, _da) = h.open_confirm_dialog("c1", "bash", "echo hi");

    let v = h.visible();
    assert!(
        v.contains("Live streaming paragraph"),
        "streaming text missing in dialog overlay:\n{v}"
    );
    assert!(v.contains("echo hi"), "tool summary missing:\n{v}");

    // The streaming text appears exactly once across viewport+scrollback.
    let full = h.full_text();
    assert_eq!(
        full.matches("Live streaming paragraph").count(),
        1,
        "streaming text duplicated into scrollback:\n{full}"
    );
}

/// When the dialog is so tall that it leaves zero rows for the overlay,
/// the overlay collapses cleanly. Committed content gets pushed up
/// into terminal scrollback (handled by the dialog's own ScrollUp /
/// our overlay ScrollUp), and the dialog occupies the screen.
#[test]
fn fullscreen_dialog_collapses_overlay() {
    // Use a small terminal where dialog_height >= term_h - 1 — the
    // ConfirmDialog clamps itself to terminal height, so an extremely
    // small terminal forces the fullscreen path.
    let mut h = TestHarness::new(80, 8, "fullscreen_dialog_collapses_overlay");
    h.push_and_render(Block::User {
        text: "tiny term".into(),
        image_labels: vec![],
    });
    h.draw_prompt();

    h.start_bash_tool("c1", "echo SMALL");

    let (_dialog, _da) = h.open_confirm_dialog("c1", "bash", "echo SMALL");

    let v = h.visible();
    // Dialog hint should still be visible (the dialog clamped to fit).
    assert!(
        v.to_lowercase().contains("yes") || v.to_lowercase().contains("confirm"),
        "dialog hint missing in fullscreen dialog:\n{v}"
    );
}

/// Open the dialog when committed content already fills the viewport.
/// The overlay reserves dialog_height + 1 rows; committed content
/// scrolls up via the unified ScrollUp path so the dialog fits.
#[test]
fn dialog_opens_when_committed_fills_viewport() {
    let mut h = TestHarness::new(80, 14, "dialog_opens_when_committed_fills_viewport");
    // Push enough content to fill the whole viewport.
    for i in 0..12 {
        h.push_and_render(Block::User {
            text: format!("USER_{i:02}"),
            image_labels: vec![],
        });
        h.push_and_render(Block::Text {
            content: format!("REPLY_{i:02}"),
        });
    }
    h.draw_prompt();
    h.drain_sink();

    h.start_bash_tool("c1", "echo HELLO");
    let (_dialog, _da) = h.open_confirm_dialog("c1", "bash", "echo HELLO");

    let v = h.visible();
    // The tool overlay must be visible above the dialog.
    assert!(v.contains("echo HELLO"), "tool overlay missing:\n{v}");
    // The dialog hint must be visible.
    assert!(
        v.to_lowercase().contains("yes") || v.to_lowercase().contains("confirm"),
        "dialog hint missing:\n{v}"
    );

    // Earlier history should have moved into scrollback — combined
    // (viewport + scrollback) text still contains it exactly once.
    let full = h.full_text();
    for i in 0..12 {
        assert_eq!(
            full.matches(&format!("USER_{i:02}")).count(),
            1,
            "USER_{i:02} duplicated or lost across scrollback:\n{full}"
        );
    }
}

/// After the user dismisses a dialog that pushed the overlay around,
/// the next prompt redraw should be coherent — every active tool
/// reappears, no ghost rows, no duplicates in scrollback.
#[test]
fn dialog_dismiss_restores_clean_overlay() {
    let mut h = TestHarness::new(80, 24, "dialog_dismiss_restores_clean_overlay");
    h.push_and_render(Block::User {
        text: "two tools".into(),
        image_labels: vec![],
    });
    h.draw_prompt();

    h.start_bash_tool("c1", "MARKER_X");
    h.start_bash_tool("c2", "MARKER_Y");

    let (_dialog, da) = h.open_confirm_dialog("c2", "bash", "MARKER_Y");

    // Dismiss.
    h.screen.clear_dialog_area(da);
    h.drain_sink();
    h.draw_prompt();

    let v = h.visible();
    assert!(v.contains("MARKER_X"), "tool X missing after dismiss:\n{v}");
    assert!(v.contains("MARKER_Y"), "tool Y missing after dismiss:\n{v}");

    let full = h.full_text();
    assert_eq!(
        full.matches("MARKER_X").count(),
        1,
        "tool X duplicated after dismiss:\n{full}"
    );
    assert_eq!(
        full.matches("MARKER_Y").count(),
        1,
        "tool Y duplicated after dismiss:\n{full}"
    );
}

/// Resize the terminal while a dialog + overlay are active. After
/// resize the visible viewport should remain coherent (no duplicate
/// rows, dialog still fits).
#[test]
fn resize_during_dialog_keeps_overlay_coherent() {
    let mut h = TestHarness::new(80, 30, "resize_during_dialog_keeps_overlay_coherent");
    h.push_and_render(Block::User {
        text: "resize test".into(),
        image_labels: vec![],
    });
    h.draw_prompt();

    h.start_bash_tool("c1", "MARKER_RESIZE");

    let (mut dialog, _da) = h.open_confirm_dialog("c1", "bash", "MARKER_RESIZE");

    // Shrink the terminal — the dialog handles its own resize, and
    // the next draw should still keep the overlay tail visible.
    h.resize(80, 18);
    dialog.set_term_size(h.width, h.height);
    dialog.handle_resize();

    // Re-draw a frame with the new dialog height.
    let dh = dialog.height();
    {
        let mut frame = tui::render::Frame::begin(h.screen.backend());
        h.screen
            .draw_frame(&mut frame, h.width as usize, None, Some(dh));
        let dr = h.screen.dialog_row();
        dialog.draw(&mut frame, dr, h.width, h.height);
    }
    h.drain_sink();

    let v = h.visible();
    assert!(
        v.contains("MARKER_RESIZE"),
        "tool overlay lost after dialog resize:\n{v}"
    );
}
