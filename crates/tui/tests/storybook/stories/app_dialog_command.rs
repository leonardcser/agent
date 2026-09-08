//! Slash-command-driven dialogs and overlays. Each story seeds
//! whatever state the command reads (turns, permission rules,
//! persisted sessions, messages) and then calls
//! `AppStoryCtx::run_command`, which goes through the real
//! `smelt.cmd.run` → command handler → `smelt.spawn` →
//! `smelt.dialog.open` flow. The coroutine yields at `dialog.open`;
//! that's what we snapshot.
//!
//! `ask_user_question_dialog` is the lone tool-driven entry: the
//! dialog is opened from inside the tool's `execute` callback via a
//! real `ToolDispatch` engine event.

use protocol::EngineEvent;
use serde_json::json;

use crate::app_story;
use crate::storybook::app_ctx::AppStoryCtx;
use crate::storybook::args;

fn open_ask_user_question_dialog(ctx: &mut AppStoryCtx) {
    ctx.engine(EngineEvent::ToolDispatch {
        invocation_id: protocol::InvocationId::new(1),
        request_id: 1,
        call_id: "aq-1".into(),
        tool_name: "ask_user_question".into(),
        args: args([(
            "questions",
            json!([{
                "header": "Auth method",
                "question": "Which authentication method should I use?",
                "options": [
                    { "label": "OAuth",     "description": "redirect to provider login" },
                    { "label": "API key",   "description": "paste a secret token" },
                    { "label": "Anonymous", "description": "skip auth entirely" },
                ],
                "multiSelect": false,
            }]),
        )]),
    });
}

app_story!(help_overlay, |ctx| {
    // `/help` opens a centered overlay (not a dialog) built from
    // `smelt.keymap.help()`. The overlay snapshot captures the
    // production layout (accented labels, section gaps, centered
    // skills-sized overlay).
    ctx.set_viewport(60, 18);
    ctx.run_lua("smelt.settings.show_tips = false");
    ctx.run_command("help");
    ctx.assert_snapshot();
});

app_story!(diff_overlay, |ctx| {
    ctx.set_viewport(110, 28);
    ctx.run_lua(r#"
        local patch = 'diff --git a/src/parser.rs b/src/parser.rs\n@@ -1,22 +1,22 @@\n'
          .. string.rep(' unchanged context\n', 20)
          .. '-let value = parse(input);\n+let value = parse_checked(input)?;\n value\n'
          .. 'diff --git a/tests/parser.rs b/tests/parser.rs\nnew file mode 100644\n@@ -0,0 +1,3 @@\n+#[test]\n+fn rejects_invalid_input() {\n+    assert!(parse_checked("bad").is_err());\n'
        smelt.git.diff = function() return { branch = 'feature/parser', diff = smelt.diff.parse(patch) } end
    "#);
    ctx.run_command("diff");
    ctx.wait_for_document("smelt.diff.preview");
    ctx.assert_snapshot();
});

app_story!(diff_overlay_grouped, |ctx| {
    let dir = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .current_dir(dir.path())
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "-q"]);
    git(&["config", "user.name", "test"]);
    git(&["config", "user.email", "test@example.invalid"]);
    std::fs::write(dir.path().join("parser.rs"), "let strict = false;\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "base"]);
    std::fs::write(dir.path().join("parser.rs"), "let strict = true;\n").unwrap();
    git(&["add", "."]);
    std::fs::write(dir.path().join("parser.rs"), "let strict = configured();\n").unwrap();
    std::fs::write(dir.path().join("test.rs"), "assert!(strict);\n").unwrap();
    ctx.set_viewport(110, 34);
    ctx.run_lua(&format!(
        "local load = smelt.git.diff; smelt.git.diff = function() return load({{cwd = {}}}) end",
        serde_json::to_string(&dir.path().to_str().unwrap()).unwrap()
    ));
    ctx.run_command("diff");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !ctx.frame_text().contains("configured()") {
        assert!(std::time::Instant::now() < deadline, "{}", ctx.frame_text());
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        ctx.pump_lua();
    }
    ctx.wait_for_document("smelt.diff.preview");
    ctx.assert_snapshot();
    ctx.press_tab();
    for _ in 0..3 {
        ctx.press_key(
            crossterm::event::KeyCode::Char('w'),
            crossterm::event::KeyModifiers::CONTROL,
        );
        ctx.press_char('>');
    }
    ctx.wait_for_document("smelt.diff.preview");
    ctx.assert_snapshot_named("resized");
});

app_story!(diff_overlay_refresh, |ctx| {
    ctx.set_viewport(100, 24);
    ctx.run_lua(r#"
        local patch = 'diff --git a/parser.rs b/parser.rs\n@@ -1 +1 @@\n-let strict = false;\n+let strict = true;\n'
        smelt.git.diff = function()
            while refresh_blocked do smelt.sleep(1) end
            if refresh_error then return nil, refresh_error end
            return {diff = smelt.diff.parse(patch)}
        end
    "#);
    ctx.run_command("diff");
    ctx.wait_for_document("smelt.diff.preview");
    ctx.press_tab();
    ctx.run_lua("refresh_blocked = true");
    ctx.press_char('r');
    ctx.assert_snapshot_named("refreshing");
    ctx.run_lua("refresh_error = 'repository temporarily unavailable'; refresh_blocked = false");
    ctx.advance_time(2);
    ctx.pump_lua();
    assert!(ctx.frame_text().contains("stale"));
    ctx.assert_snapshot_named("stale");
});

app_story!(diff_overlay_empty, |ctx| {
    ctx.set_viewport(80, 20);
    ctx.run_lua("smelt.git.diff = function() return {diff = smelt.diff.parse('')} end");
    ctx.run_command("diff");
    ctx.assert_snapshot();
});

app_story!(diff_overlay_narrow, |ctx| {
    ctx.set_viewport(60, 18);
    ctx.run_lua(r#"
        smelt.git.diff = function() return { branch = 'main', diff = smelt.diff.parse('diff --git a/界.lua b/界.lua\n@@ -1 +1 @@\n-old\n+new\n') } end
    "#);
    ctx.run_command("diff");
    ctx.wait_for_document("smelt.diff.preview");
    ctx.assert_snapshot();
});

app_story!(stats_dialog, |ctx| {
    // `/stats` opens a docked-bottom dialog with
    // `smelt.metrics.stats_text()` as the body. Pin the rendered
    // panel so any drift in `stats_text` or the dialog chrome lands
    // in this snapshot.
    ctx.set_viewport(70, 12);
    ctx.run_command("stats");
    ctx.assert_snapshot();
});

app_story!(ps_dialog_list_and_details_wrap_command, |ctx| {
    ctx.set_viewport(70, 22);
    ctx.run_lua(
        r#"
        local command = "ls -ld /Users/leo/.dotfiles; realpath /Users/leo/.dotfiles 2>/dev/null || true; cd /Users/leo/dev/rust/smelt && cargo test -p smelt-tui"
        local row = { id = "proc_123", pid = 4242, command = command, elapsed_secs = 125 }
        smelt.process.list = function() return { row } end
        smelt.process.output = function()
          return { text = "running tests...\n", running = true, pid = 4242, elapsed_secs = 125 }
        end
        smelt.process.kill = function() end
        "#,
    );
    ctx.run_command("ps");
    ctx.assert_snapshot();
    ctx.press_enter();
    ctx.assert_snapshot();
});

app_story!(messages_dialog_empty, |ctx| {
    // `/messages` with zero entries - the dialog shows the placeholder
    // body. Pins the empty-state branch in `format_lines`.
    ctx.set_viewport(70, 12);
    ctx.run_command("messages");
    ctx.assert_snapshot();
});

app_story!(messages_dialog_with_entries, |ctx| {
    // `/messages` after seeding two entries through the real
    // `smelt.messages.append` path so the dialog renders the
    // production multi-entry layout (kind+source header line then
    // 2-space indented body).
    ctx.set_viewport(70, 14);
    ctx.run_lua(
        r#"
        smelt.messages.append("error", "parser", "first error message")
        smelt.messages.append("warn",  "renderer", "second warning")
        "#,
    );
    ctx.run_command("messages");
    ctx.assert_snapshot();
});

app_story!(rewind_dialog, |ctx| {
    ctx.set_viewport(60, 16);
    // /rewind reads `smelt.session.turns()`. Seed three user turns so
    // the real dialog handler builds a non-empty option list and the
    // snapshot captures the production rendering (numbered prefixes
    // dim, current marker, default focus on the last entry).
    ctx.push_user_turn("write the parser");
    ctx.push_user_turn("add the renderer");
    ctx.push_user_turn("wire up the CLI flag");
    ctx.run_command("rewind");
    ctx.assert_snapshot();
});

app_story!(rewind_dialog_long_messages, |ctx| {
    ctx.set_viewport(60, 20);
    ctx.push_user_turn("write the parser and handle invalid input with helpful error messages");
    ctx.push_user_turn(
        "add the renderer and keep every wrapped continuation aligned with its message",
    );
    ctx.push_user_turn("wire up the CLI flag and document the expected behavior for new users");
    ctx.run_command("rewind");

    let frame = ctx.frame_text();
    let dialog = frame.split("─ rewind ").nth(1).expect("rewind dialog");
    assert!(dialog.contains("1. write the parser"), "{frame}");
    assert!(dialog.contains("4. (current)"), "{frame}");
    assert!(dialog.contains("aligned with its message"), "{frame}");
    assert!(dialog.contains("behavior for new users"), "{frame}");
    assert!(
        dialog
            .lines()
            .any(|line| line.starts_with("     ") && line.ends_with("messages")),
        "{frame}"
    );
    ctx.assert_snapshot();
    ctx.press_key(
        crossterm::event::KeyCode::Up,
        crossterm::event::KeyModifiers::NONE,
    );
    ctx.assert_snapshot_named("selected");
});

app_story!(rewind_dialog_unicode_narrow, |ctx| {
    ctx.set_viewport(32, 12);
    ctx.push_user_turn("render 界界界 and e\u{301} without splitting characters");
    ctx.push_user_turn("preserve the full input\nincluding the second line");
    ctx.run_command("rewind");
    ctx.press_char('1');
    ctx.assert_snapshot();
});

app_story!(btw_dialog_with_answer, |ctx| {
    ctx.set_viewport(70, 16);
    ctx.use_test_model();
    // /btw spawns a coroutine that calls `smelt.engine.ask` and then
    // `smelt.dialog.open`. With no real engine task, the ask sits
    // pending. We harvest the pending ask id from `ask_callbacks` and
    // synthesise an `EngineAskResponse` so the dialog body renders the
    // completed markdown answer - the steady state users see once the side
    // question completes.
    ctx.push_user_turn("how do I render a buffer?");
    ctx.push_assistant_text("Call `buf:lines(...)` or set `buf:source(text)`.");
    ctx.run_command("btw what is the difference between source and lines?");
    let ask_id = ctx.pending_ask_id().expect("/btw registered ask callback");
    ctx.engine(EngineEvent::EngineAskResponse {
        id: ask_id,
        message: Some(protocol::Message::assistant(
            Some(protocol::Content::text(
                "`buf:source(text)` replaces the whole buffer with a single string and reparses; "
                    .to_string()
                    + "`buf:lines(...)` overwrites the row array directly without reparsing. Use "
                    + "`source` for markdown/code where formatting depends on the whole text, "
                    + "`lines` for list/picker buffers that are already shaped row-by-row.",
            )),
            None,
            None,
        )),
        error: None,
    });
    ctx.assert_snapshot();
});

app_story!(btw_dialog_streaming_answer, |ctx| {
    ctx.set_viewport(70, 22);
    ctx.use_test_model();
    // EngineAskDelta -> Lua on_delta -> smelt.transcript.stream -> dialog buffer.
    ctx.push_user_turn("how do I render a buffer?");
    ctx.push_assistant_text("Call `buf:lines(...)` or set `buf:source(text)`.");
    ctx.run_command("btw show me a tiny rust example");
    ctx.assert_snapshot();

    let ask_id = ctx.pending_ask_id().expect("/btw registered ask callback");
    ctx.engine(EngineEvent::EngineAskDelta {
        id: ask_id,
        delta: "Here is `".into(),
    });
    ctx.engine(EngineEvent::EngineAskDelta {
        id: ask_id,
        delta: "inline".into(),
    });
    ctx.engine(EngineEvent::EngineAskDelta {
        id: ask_id,
        delta: "` and a block:\n\n```rust\nfn main() {\n".into(),
    });
    ctx.assert_snapshot();

    let final_text =
        "Here is `inline` and a block:\n\n```rust\nfn main() {\n    println!(\"hi\");\n}\n```";
    ctx.engine(EngineEvent::EngineAskResponse {
        id: ask_id,
        message: Some(protocol::Message::assistant(
            Some(protocol::Content::text(final_text)),
            None,
            None,
        )),
        error: None,
    });
    ctx.assert_snapshot();
});

app_story!(btw_dialog_streams_table_in_tiny_deltas, |ctx| {
    ctx.set_viewport(70, 18);
    ctx.use_test_model();
    ctx.push_user_turn("show table output");
    ctx.push_assistant_text("Tables should stream without raw delimiter frames.");
    ctx.run_command("btw show a tiny table");

    let ask_id = ctx.pending_ask_id().expect("/btw registered ask callback");
    for ch in "| A | B |\n|---|---|\n| 1 | 2 |\n".chars() {
        ctx.engine(EngineEvent::EngineAskDelta {
            id: ask_id,
            delta: ch.to_string(),
        });
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

app_story!(ask_user_question_dialog, |ctx| {
    ctx.set_viewport(80, 22);
    // ask_user_question's `execute` calls `smelt.dialog.open`; driving
    // a real `ToolDispatch` event makes the tool registry run that
    // execute on the task runtime. The coroutine yields at
    // `dialog.open`; pumping `LuaWakeup` events lets it advance to the
    // yield. The snapshot then captures the production dialog (header
    // = question, markdown panel, options panel, free-text "Other").
    open_ask_user_question_dialog(ctx);
    // Coroutine has yielded at `dialog.open`; render captures it.
    ctx.assert_snapshot();
});

app_story!(ask_user_question_wrapped_options, |ctx| {
    ctx.set_viewport(46, 26);
    ctx.engine(EngineEvent::ToolDispatch {
        invocation_id: protocol::InvocationId::new(1),
        request_id: 1,
        call_id: "aq-wrapped".into(),
        tool_name: "ask_user_question".into(),
        args: args([("questions", json!([{
            "header": "Auth method",
            "question": "Which authentication method should we use for browser sign-in and automated clients?",
            "options": [
                {"label": "Browser sign-in", "description": "Redirect to the provider and preserve the original destination when the user returns to the application."},
                {"label": "API key automation fallback", "description": "Use a token for automated clients and support scripts, including 界 and e\u{301} in account names."},
                {"label": "Anonymous", "description": "Skip authentication entirely."}
            ],
            "multiSelect": false
        }]))]),
    });
    ctx.assert_snapshot();
    ctx.press_key(
        crossterm::event::KeyCode::Down,
        crossterm::event::KeyModifiers::NONE,
    );
    ctx.assert_snapshot_named("selected");
    ctx.set_viewport(32, 18);
    ctx.assert_snapshot_named("narrow");
});

app_story!(ask_user_question_dialog_expanded_max_height, |ctx| {
    ctx.set_viewport(80, 22);
    open_ask_user_question_dialog(ctx);
    ctx.expand_active_dialog_to_max_height();
    ctx.assert_snapshot();
});

app_story!(ask_user_question_custom_input_wraps, |ctx| {
    ctx.set_viewport(52, 18);
    open_ask_user_question_dialog(ctx);
    ctx.press_tab();
    ctx.type_prompt(
        "Use OAuth for browser sign-in, but keep an API-key fallback for automation and support scripts.",
    );
    ctx.assert_snapshot();
});

app_story!(resume_dialog, |ctx| {
    ctx.set_viewport(72, 18);
    // /resume drives `smelt.session.list()`. Seed real `SessionMeta`
    // fixtures through the app-owned session storage so the production
    // list path returns them, then run the real command. Going
    // through the typed struct (rather than hand-rolled JSON) means
    // a `SessionMeta` field rename breaks compilation, not silently
    // the snapshot.
    //
    // `time_ago` reads `os.time()` at dialog-open; patch it to a fixed
    // epoch so the rendered "5m / 2h / 3d" column is byte-stable.
    let now_s: u64 = 1_700_000_000;
    let now_ms = now_s * 1000;
    ctx.run_lua(&format!("os.time = function() return {now_s} end"));

    let cwd = ctx.app_cwd().to_string();
    let root_old = "1111111111111111111111111111111111111111111111111111111111111111";
    let root_forked = "2222222222222222222222222222222222222222222222222222222222222222";
    let fork_a = "3333333333333333333333333333333333333333333333333333333333333333";
    let fork_b = "4444444444444444444444444444444444444444444444444444444444444444";
    let nested = "5555555555555555555555555555555555555555555555555555555555555555";
    let entries = [
        (
            root_old,
            "spike: notebook preview",
            now_ms - 5 * 86400 * 1000,
            1_500_000,
            None::<&str>,
        ),
        (
            root_forked,
            "first pass at the resume picker",
            now_ms - 26 * 3600 * 1000,
            87_654,
            None,
        ),
        (
            fork_a,
            "fix prompt keybindings",
            now_ms - 7 * 3600 * 1000,
            1_037_000,
            Some(root_forked),
        ),
        (
            fork_b,
            "wire up the diff renderer",
            now_ms - 2 * 3600 * 1000,
            4_096,
            Some(root_forked),
        ),
        (
            nested,
            "investigate parser regression",
            now_ms - 5 * 60 * 1000,
            12_345u64,
            Some(fork_b),
        ),
    ];
    for (id, title, ts, bytes, parent_id) in entries {
        let meta = smelt_core::session::SessionMeta {
            id: id.to_string(),
            title: Some(title.to_string()),
            slug: None,
            first_user_message: Some(title.to_string()),
            created_at_ms: ts,
            updated_at_ms: ts,
            mode: None,
            reasoning_effort: None,
            model: None,
            fast_mode: None,
            cwd: Some(cwd.clone()),
            parent_id: parent_id.map(str::to_string),
            authoritative_context_tokens: None,
            display_context_tokens: None,
            history_len: None,
            checkpoint: None,
            checkpoint_events: Vec::new(),
            text_bytes: Some(bytes),
        };
        ctx.write_session_meta(&meta);
    }
    ctx.wait_for_session_catalog();
    ctx.run_command("resume");
    ctx.assert_snapshot();

    ctx.type_prompt("parser");
    ctx.assert_snapshot();
});

app_story!(permissions_dialog, |ctx| {
    ctx.set_viewport(70, 16);
    // /permissions reads `smelt.permissions.list()`. Seed session and
    // workspace rules so their scope labels are visible.
    ctx.run_lua(
        r#"
        local permissions = smelt.permissions.list()
        smelt.permissions.sync({
          session = {
            { tool = "bash", pattern = "ls/*" },
            { tool = "web_fetch", pattern = "https://example.com/*" },
          },
          workspace = {
            revision = permissions.workspace_revision,
            rules = {
              { tool = "bash", patterns = { "cat/*", "cat/* /etc/*" } },
              { tool = "write_file", patterns = { "src/**/*.rs" } },
            },
          },
        })
        "#,
    );
    ctx.run_command("permissions");
    ctx.assert_snapshot();
});
