//! Conversation-flow message blocks: user, assistant text/thinking,
//! streaming deltas, turn errors, compacted summaries, plus the
//! full-turn composite. Every story drives the production event path
//! (`push_user_message` or an `EngineEvent`).

use protocol::EngineEvent;
use serde_json::json;

use crate::app_story;

// ── Assistant output: Text / Thinking / streaming ─────────────────

app_story!(text_block_plain, |ctx| {
    ctx.set_viewport(40, 12);
    ctx.engine(EngineEvent::Text {
        content: "hello, world.".into(),
    });
    ctx.assert_snapshot();
});

app_story!(thinking_block_then_answer, |ctx| {
    ctx.set_viewport(40, 12);
    ctx.engine(EngineEvent::Reasoning {
        kind: protocol::ReasoningKind::Raw,
        title: None,
        content: "let me think about this carefully.".into(),
    });
    ctx.engine(EngineEvent::Text {
        content: "the answer is 42.".into(),
    });
    ctx.assert_snapshot();
});

app_story!(text_streams_via_deltas, |ctx| {
    ctx.set_viewport(40, 12);
    for chunk in ["stream ", "of ", "tokens."] {
        ctx.engine(EngineEvent::TextDelta {
            delta: chunk.into(),
        });
    }
    ctx.assert_snapshot();
});

app_story!(text_block_code_fence_no_language, |ctx| {
    // Fenced block without a language tag - the renderer must still
    // emit the code chrome (gutter, monospace body) but skip the
    // syntax highlighting pipeline. Pins the fallback path.
    ctx.set_viewport(60, 12);
    ctx.engine(EngineEvent::Text {
        content: "Raw output:\n\n```\nhello\nworld\n```\n\nDone.".into(),
    });
    ctx.assert_snapshot();
});

app_story!(thinking_block_renders_full_markdown_when_expanded, |ctx| {
    ctx.set_viewport(70, 18);
    ctx.engine(EngineEvent::Reasoning {
        kind: protocol::ReasoningKind::Raw,
        title: None,
        content: "Plan:\n\n```rust\nfn main() {}\n```\n\n| A | B |\n|---|---|\n| 1 | 2 |".into(),
    });
    ctx.run_lua("smelt.transcript.fold_all('open')");

    let frame = ctx.frame_text();
    assert!(frame.contains("fn main()"), "frame: {frame}");
    assert!(frame.contains("A") && frame.contains("B"), "frame: {frame}");
    assert!(frame.contains("1") && frame.contains("2"), "frame: {frame}");
    assert!(!frame.contains("```"), "frame: {frame}");
    assert!(!frame.contains("---"), "frame: {frame}");
});

app_story!(thinking_streams_table_in_tiny_deltas, |ctx| {
    ctx.set_viewport(70, 18);
    for ch in "| A | B |\n|---|---|\n| 1 | 2 |\n".chars() {
        ctx.engine(EngineEvent::ReasoningPartDelta {
            id: "raw:0".into(),
            kind: protocol::ReasoningKind::Raw,
            title: None,
            delta: ch.to_string(),
        });
        ctx.run_lua("smelt.transcript.fold_all('open')");
        let frame = ctx.frame_text();
        let rows: Vec<&str> = frame.lines().collect();
        assert!(
            !rows.iter().any(|row| row.contains("---")),
            "frame: {frame}"
        );
        assert!(!rows.iter().any(|row| row.trim() == "|"), "frame: {frame}");
    }

    let frame = ctx.frame_text();
    assert!(frame.contains("A") && frame.contains("B"), "frame: {frame}");
    assert!(frame.contains("1") && frame.contains("2"), "frame: {frame}");
});

app_story!(thinking_peek_suppresses_blank_only_omission, |ctx| {
    ctx.set_viewport(72, 12);
    ctx.engine(EngineEvent::ReasoningPartDelta {
        id: "raw:0".into(),
        kind: protocol::ReasoningKind::Raw,
        title: None,
        delta: "first paragraph\n\nsecond paragraph\nthird paragraph\nfourth paragraph".into(),
    });
    ctx.run_lua("smelt.transcript.fold_kind('thinking', 'peek')");
    ctx.assert_snapshot();
});

app_story!(thinking_stream_title_sections, |ctx| {
    ctx.set_viewport(88, 16);
    ctx.engine(EngineEvent::ReasoningPartDelta {
        id: "raw:0".into(),
        kind: protocol::ReasoningKind::Raw,
        title: None,
        delta: concat!(
            "I’m thinking about deriving directories from paths.\n\n",
            "Regarding the watcher, it needs to rescan on changes.\n",
            "**Assessing directory exclusions**\n\n",
            "I'm considering which directories to include or exclude."
        )
        .into(),
    });
    ctx.run_lua("smelt.transcript.fold_all('open')");
    ctx.assert_snapshot();
});

app_story!(reasoning_summary_titles_update_thinking_block, |ctx| {
    ctx.set_viewport(100, 12);
    let titles = [
        "Removing unused BlockIndex and assessing index wrappers",
        "Simplifying save acknowledgment and error handling layers",
        "Outlining consolidation and rewrite options",
        "Summarizing completed system components",
        "Identifying partial implementations and open concerns",
        "Confirming audit and accounting table retention",
    ];

    for (index, title) in titles.iter().enumerate() {
        let id = format!("reasoning:summary:{index}");
        ctx.engine(EngineEvent::ReasoningPartStarted {
            id: id.clone(),
            kind: protocol::ReasoningKind::Summary,
        });
        ctx.engine(EngineEvent::ReasoningPartDelta {
            id: id.clone(),
            kind: protocol::ReasoningKind::Summary,
            delta: format!("**{title}**\n\n<!-- -->"),
            title: Some((*title).to_string()),
        });
        ctx.engine(EngineEvent::ReasoningPartFinished {
            id,
            kind: protocol::ReasoningKind::Summary,
            title: Some((*title).to_string()),
            content: if index == titles.len() - 1 {
                "The accounting table is still required.".into()
            } else {
                String::new()
            },
        });
    }

    let latest_title = titles.last().unwrap();
    let frame = ctx.frame_text();
    assert!(
        frame
            .lines()
            .any(|line| line.contains("│ ") && line.contains(latest_title)),
        "latest summary title should be inside a thinking block: {frame}"
    );
    assert!(
        frame.lines().any(|line| line.contains("─ ✿ working… ")),
        "prompt title changed: {frame}"
    );
    assert!(
        frame.contains("The accounting table is still required."),
        "thinking body missing: {frame}"
    );
    assert!(!frame.contains("<!-- -->"), "frame: {frame}");
    for stale_title in &titles[..titles.len() - 1] {
        assert!(
            !frame.contains(stale_title),
            "stale title appeared in peek view: {frame}"
        );
    }
    ctx.assert_snapshot_named("peek");

    ctx.run_lua("smelt.transcript.fold_kind('thinking', 'close')");
    ctx.assert_snapshot_named("collapsed");

    ctx.run_lua("smelt.transcript.fold_kind('thinking', 'open')");
    let frame = ctx.frame_text();
    for title in titles {
        assert!(frame.contains(title), "expanded title missing: {frame}");
    }
    ctx.assert_snapshot_named("expanded");

    ctx.engine(EngineEvent::ReasoningPartStarted {
        id: "reasoning:raw:0".into(),
        kind: protocol::ReasoningKind::Raw,
    });
    let frame = ctx.frame_text();
    assert!(
        frame.contains(latest_title),
        "thinking block disappeared: {frame}"
    );
});

// ── User messages ─────────────────────────────────────────────────

app_story!(user_message_block_single_line, |ctx| {
    ctx.set_viewport(50, 8);
    ctx.push_user_turn("refactor the parser into its own module");
    ctx.assert_snapshot();
});

app_story!(user_message_timestamp_submission, |ctx| {
    ctx.run_lua("assert(smelt.settings.transcript.show_timestamps == true)");
    ctx.set_viewport(60, 12);
    ctx.use_test_model();
    ctx.type_prompt("Keep the parser API stable.");
    ctx.press_enter();

    let visible = ctx.frame_text();
    assert!(visible.contains("Keep the parser API stable."), "{visible}");
    assert!(visible.contains("14:37:03"), "{visible}");
    ctx.assert_snapshot_named("visible");

    ctx.run_lua("smelt.settings.transcript.show_timestamps = false");
    let hidden = ctx.frame_text();
    assert!(!hidden.contains("14:37:03"), "{hidden}");
    assert!(hidden.contains("Keep the parser API stable."), "{hidden}");
    ctx.assert_snapshot_named("hidden");

    ctx.run_lua("smelt.settings.transcript.show_timestamps = true");
    assert_eq!(ctx.frame_text(), visible);
});

app_story!(user_message_timestamp_wrapping_and_themes, |ctx| {
    ctx.set_viewport(44, 16);
    ctx.push_user_turn("Preserve wrapping for Unicode 語界 and a long user message.\nKeep this second line intact.");
    let visible = ctx.frame_text();
    assert!(visible.lines().next().unwrap().ends_with("14:37:03"));
    ctx.assert_snapshot_named("dark");

    ctx.run_lua("smelt.settings.transcript.show_timestamps = false");
    let hidden = ctx.frame_text();
    assert_eq!(
        visible.lines().skip(1).collect::<Vec<_>>(),
        hidden.lines().skip(1).collect::<Vec<_>>()
    );
    ctx.run_lua("smelt.settings.transcript = {}");
    assert_eq!(
        ctx.frame_text(),
        visible,
        "omitting the setting enables timestamps"
    );

    ctx.run_lua(
        "local t = require('smelt.colorschemes.default'); t.light = true; smelt.theme.apply(t)",
    );
    ctx.assert_snapshot_named("light");
    for name in ["catppuccin-mocha", "catppuccin-latte"] {
        ctx.run_lua(&format!("smelt.theme.use('{name}')"));
        ctx.assert_snapshot_named(name);
    }
    ctx.set_viewport(18, 18);
    ctx.assert_snapshot_named("narrow");
    for width in 1..12 {
        ctx.set_viewport(width, 18);
        ctx.frame_text();
    }
});

app_story!(
    transcript_timestamp_toggle_includes_tools_and_groups,
    |ctx| {
        ctx.set_viewport(72, 18);
        ctx.push_user_turn("Inspect the files, then run the tests.");
        ctx.tool_call(
            "read_file",
            &[("file_path", json!("src/a.rs"))],
            "first",
            Some(2_400),
        );
        ctx.tool_call(
            "read_file",
            &[("file_path", json!("src/b.rs"))],
            "second",
            Some(2_400),
        );
        ctx.engine(EngineEvent::ToolStarted {
            invocation_id: protocol::InvocationId::new(100),
            call_id: "timestamp-bash".into(),
            tool_name: "bash".into(),
            args: std::collections::HashMap::from([("command".into(), json!("cargo test"))]),
            called_at_ms: 1_742_567_823_000,
        });
        ctx.advance_time(2_400);
        ctx.engine(EngineEvent::ToolFinished {
            invocation_id: protocol::InvocationId::new(100),
            call_id: "timestamp-bash".into(),
            result: protocol::ToolOutcome::new("passed".into(), false, None),
            elapsed_ms: Some(2_400),
        });

        let visible = ctx.frame_text();
        assert_eq!(visible.matches("14:37:03").count(), 3, "{visible}");
        assert!(visible.contains("2.4s"), "{visible}");
        ctx.assert_snapshot_named("visible");

        ctx.run_lua("smelt.settings.transcript.show_timestamps = false");
        let hidden = ctx.frame_text();
        assert!(!hidden.contains("14:37:03"), "{hidden}");
        assert!(hidden.contains("2.4s"), "{hidden}");
        ctx.assert_snapshot_named("hidden");
        ctx.run_lua("smelt.transcript.fold_all('open')");
        assert!(!ctx.frame_text().contains("14:37:03"));
        ctx.run_lua("smelt.settings.transcript.show_timestamps = true");
        assert!(ctx.frame_text().contains("14:37:03"));
    }
);

app_story!(user_message_block_multiline_chrome, |ctx| {
    // Multi-line user messages get the panel chrome (rounded box,
    // `block_w` width). Exercises `UserBlockGeometry::new` + the
    // shared `chrome::render` path. Height leaves room for the idle
    // prompt tip without clipping the message header.
    ctx.set_viewport(50, 11);
    ctx.push_user_turn("let's plan this out.\nstep 1: parse\nstep 2: render\nstep 3: ship");
    ctx.assert_snapshot();
});

app_story!(user_message_block_slash_command_accent, |ctx| {
    // Command semantics are stored on the user block, so `user::render`
    // paints the slash token with `SmeltAccent` instead of the body color.
    ctx.set_viewport(40, 8);
    ctx.push_command_turn("/permissions");
    ctx.assert_snapshot();
});

// ── Error / compaction ────────────────────────────────────────────

app_story!(turn_error_block, |ctx| {
    // `TurnError` renders as the assistant's error block - its own
    // chrome and color group, distinct from a regular text block.
    ctx.set_viewport(60, 10);
    ctx.engine(EngineEvent::Text {
        content: "Working on it…".into(),
    });
    ctx.engine(EngineEvent::TurnError {
        message: "provider returned 503: service unavailable".into(),
        kind: None,
        retry_at_ms: None,
    });
    ctx.assert_snapshot();
});

app_story!(compacted_block_summary, |ctx| {
    // Compacted history defaults collapsed: the summary is available by
    // expanding the block, but the normal transcript shows only the divider.
    ctx.set_viewport(60, 9);
    ctx.push_compacted(
        "Compacted 8 earlier turns: parser refactor, renderer wiring, three bug fixes.",
    );
    ctx.assert_snapshot();
});

app_story!(compaction_preview_waiting_for_summary, |ctx| {
    // The block is created before the provider emits its first text delta so
    // slow-starting Responses providers still show transcript progress.
    ctx.set_viewport(64, 8);
    ctx.push_compaction_preview("");
    ctx.assert_snapshot();
});

app_story!(compaction_preview_streaming_summary, |ctx| {
    // Live compaction preview is a transient peeked block. It should show
    // the tail of the summary as deltas arrive, then be replaced by the
    // committed compacted marker when checkpointing succeeds.
    ctx.set_viewport(64, 16);
    ctx.push_compaction_preview(
        "# Goal\nSummarize earlier turns while preserving the active task.",
    );
    ctx.push_compaction_preview(concat!(
        "# Goal\nSummarize earlier turns while preserving the active task.\n\n",
        "# Progress\n- Read the transcript model.\n- Added a streaming preview block.\n",
        "- Kept the final checkpoint marker separate.\n\n",
        "# Next steps\nUpdate stories and run focused tests."
    ));
    ctx.assert_snapshot();
});

// ── Full agent turn composite ─────────────────────────────────────

app_story!(full_agent_turn_composite, |ctx| {
    // End-to-end smoke for the transcript: every block kind the user
    // sees in a single turn lined up in production order. If any
    // block's chrome / wrap / spacing regresses, the diff lands here
    // before any per-block story catches it.
    ctx.set_viewport(70, 22);
    ctx.push_user_turn("rename `add` to `checked_add`");
    ctx.engine(EngineEvent::Reasoning {
        kind: protocol::ReasoningKind::Raw,
        title: None,
        content: "scan the file, then issue an edit_file with the renamed signature.".into(),
    });
    ctx.engine(EngineEvent::Text {
        content: "I'll rename the function and harden it against overflow.".into(),
    });
    ctx.tool_call_with_display_content(
        "edit_file",
        &[
            ("file_path", json!("src/lib.rs")),
            ("old_string", json!("fn add(a: i32, b: i32)")),
            ("new_string", json!("fn checked_add(a: i64, b: i64)")),
        ],
        "ok",
        json!({ "path": "src/lib.rs" }),
        &[
            ("old_content", "fn add(a: i32, b: i32)"),
            ("new_content", "fn checked_add(a: i64, b: i64)"),
        ],
        Some(9),
    );
    ctx.engine(EngineEvent::Text {
        content: "Done. Want me to update the call sites too?".into(),
    });
    ctx.assert_snapshot();
});
