use super::*;

fn assert_only_history_updates(app: &TestApp) {
    for action in app.actions() {
        if let Action::EngineSend(cmd) = action {
            assert!(
                matches!(cmd.as_ref(), protocol::UiCommand::AppendHistoryItem { .. }),
                "goal control sent an unexpected engine command: {cmd:?}"
            );
        }
    }
}

#[test]
fn goal_auto_off_applies_while_agent_is_running() {
    let mut app = TestApp::builder().build();
    app.type_text("/goal finish the current work");
    app.press(KeyCode::Enter);
    let turn_id = app.current_turn_id().expect("goal starts a turn");
    app.clear_actions();

    app.type_text("/goal auto off");
    app.press(KeyCode::Enter);

    assert!(
        app.state().queued_inputs.is_empty(),
        "goal controls must not enter the message queue: {:?}",
        app.state().queued_inputs
    );
    assert!(app.run_lua(
        r#"
            local current = assert(require("smelt.goal").current())
            assert(current.state == "paused")
            assert(current.auto_continue == false)
        "#,
    ));
    assert_eq!(app.current_turn_id(), Some(turn_id));
    assert!(app.agent_running());
    assert_only_history_updates(&app);
    let frame = app.render_to_frame();
    assert!(frame.rows[0].contains(" PAUSED "), "{}", frame.text());
}

#[test]
fn goal_controls_apply_while_agent_is_running_without_changing_queued_messages() {
    let cases = [
        ("", "current.state == 'active'", "auto-continue: on"),
        ("status", "current.state == 'active'", "auto-continue: on"),
        ("progress validating", "current.progress.label == 'validating'", "goal progress updated"),
        ("summary Short goal", "current.summary == 'Short goal'", "goal summary updated"),
        ("pause", "current.state == 'paused' and not current.auto_continue", "goal paused"),
        ("resume", "current.state == 'active' and current.auto_continue", "goal resumed"),
        ("block waiting", "current.state == 'blocked' and current.reason == 'waiting' and not current.auto_continue", "goal marked blocked"),
        ("blocked waiting", "current.state == 'blocked' and current.reason == 'waiting' and not current.auto_continue", "goal marked blocked"),
        ("done", "current.state == 'done' and not current.auto_continue", "goal marked done"),
        ("clear", "current == nil", "goal cleared"),
        ("stop", "current == nil", "goal cleared"),
        ("auto on", "current.state == 'active' and current.auto_continue", "goal auto-continue on"),
        ("auto off", "current.state == 'paused' and not current.auto_continue", "goal auto-continue off"),
        ("auto false", "current.state == 'paused' and not current.auto_continue", "goal auto-continue off"),
        ("auto 0", "current.state == 'paused' and not current.auto_continue", "goal auto-continue off"),
    ];
    for modifiers in [KeyModifiers::NONE, KeyModifiers::CONTROL] {
        for (arg, check, notice) in cases {
            let mut app = TestApp::builder().build();
            app.type_text("/goal finish the current work");
            app.press(KeyCode::Enter);
            if matches!(arg, "resume" | "auto on") {
                assert!(app.run_lua(r#"require("smelt.goal").pause()"#));
            }
            let turn_id = app.current_turn_id().expect("goal starts a turn");
            app.type_text("follow-up request");
            app.press(KeyCode::Enter);
            app.clear_actions();

            app.type_text(&format!("/goal {arg}"));
            app.press_mod(KeyCode::Enter, modifiers);

            assert_eq!(
                app.state().queued_inputs,
                vec!["follow-up request".to_string()],
                "/goal {arg} ({modifiers:?}) must leave existing queued messages alone"
            );
            assert!(
                app.run_lua(&format!(
                    r#"local current = require("smelt.goal").current(); assert({check})"#
                )),
                "/goal {arg} ({modifiers:?})"
            );
            assert!(
                app.lua_messages_contain(notice),
                "/goal {arg} ({modifiers:?})"
            );
            assert_eq!(app.current_turn_id(), Some(turn_id));
            assert!(app.agent_running());
            assert_only_history_updates(&app);
        }
    }
}

#[test]
fn goal_creation_while_running_waits_until_its_queued_request_is_consumed() {
    for prefix in ["/goal", "/goal set"] {
        for modifiers in [KeyModifiers::NONE, KeyModifiers::CONTROL] {
            let mut app = TestApp::builder().build();
            app.type_text("initial request");
            app.press(KeyCode::Enter);
            let turn_id = app.current_turn_id().expect("initial turn");
            app.clear_actions();

            let command = format!("{prefix} finish queued work");
            app.type_text(&command);
            app.press_mod(KeyCode::Enter, modifiers);

            assert!(app.run_lua(r#"assert(require("smelt.goal").current() == nil)"#));
            assert_eq!(app.current_turn_id(), Some(turn_id));
            assert!(app.agent_running());
            assert_eq!(app.state().queued_inputs, vec![command.clone()]);

            if modifiers == KeyModifiers::CONTROL {
                assert!(app.actions().iter().any(|action| matches!(
                    action,
                    Action::EngineSend(cmd) if matches!(
                        cmd.as_ref(),
                        protocol::UiCommand::Steer { input }
                            if input.provider_content().text_content() == command
                    )
                )));
                app.feed_one(SourceEvent::engine(EngineEvent::Steered {
                    text: command.clone(),
                    count: 1,
                    sent_at_ms: 1_742_567_823_000,
                }));
                assert_eq!(app.current_turn_id(), Some(turn_id));
            } else {
                assert_only_history_updates(&app);
                app.discard_turn(crate::app::TurnEnd::Complete);
                assert_ne!(app.current_turn_id(), Some(turn_id));
                assert!(app.drain_engine_sends().iter().any(|cmd| matches!(
                    cmd,
                    protocol::UiCommand::StartTurn(payload)
                        if payload.input.provider_content().text_content().contains("finish queued work")
                )));
                assert!(app.state().queued_inputs.is_empty());
            }
            assert!(app.agent_running());
            assert!(app.run_lua(
                r#"
                    local current = assert(require("smelt.goal").current())
                    assert(current.objective == "finish queued work")
                    assert(current.state == "active" and current.auto_continue)
                "#,
            ));
            let frame = app.render_to_frame();
            assert!(
                frame.rows[0].contains(" GOAL finish queued work"),
                "{}",
                frame.text()
            );
        }
    }
}

#[test]
fn lua_goal_renders_top_banner_not_statusline() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(60, 16);

    assert!(app.run_lua(
        r#"
            local goal = require("smelt.goal")
            assert(goal.create("finish the dedicated goal banner", { auto_continue = false }))
        "#,
    ));

    let frame = app.render_to_frame();
    assert!(
        frame.rows[0].contains(" GOAL finish the dedicated goal banner"),
        "frame:\n{}",
        frame.text()
    );
    assert!(
        frame.rows[0].trim_end().ends_with("manual"),
        "manual active goals should show right-side mode:\n{}",
        frame.text()
    );
    assert!(
        !frame.rows[15].contains("finish the dedicated goal banner"),
        "statusline should not contain the goal:\n{}",
        frame.text()
    );
    assert!(
        frame.styles[0].iter().any(|style| style.bg.is_some()),
        "banner should paint a dedicated background"
    );
}

#[test]
fn lua_goal_banner_is_fully_selectable() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(48, 16);

    assert!(app.run_lua(
        r#"
            local goal = require("smelt.goal")
            assert(goal.create("copy every cell in the top bar", { auto_continue = false }))
        "#,
    ));
    app.render_silent();

    let win_id = app
        .ui_probe()
        .named_win("smelt.headerline")
        .expect("headerline window");
    let buf_id = app.ui_probe().win(win_id).expect("headerline win").buf;
    let buf = app.ui_probe().buf(buf_id).expect("headerline buffer");
    let spans = buf.highlights_at(0);

    assert!(!spans.is_empty(), "banner should be highlighted");
    assert!(
        spans.iter().all(|span| span.meta.selectable),
        "every banner highlight should stay selectable: {spans:?}"
    );
}

#[test]
fn lua_goal_state_writes_nested_session_updates_immediately() {
    let mut app = TestApp::builder().build();
    let session_id = app.session_snapshot().id.clone();

    assert!(app.run_lua(
        r#"
            local goal = require("smelt.goal")
            assert(goal.create("persist goal state updates", { auto_continue = true }))
            assert(goal.update_status({ progress = "1/2" }))
            assert(goal.block("waiting for persisted blocker"))
        "#,
    ));

    let state_path = app.session_storage_root().join("plugins").join("goal.json");
    let raw = std::fs::read_to_string(&state_path).unwrap_or_else(|err| {
        panic!("read {}: {err}", state_path.display());
    });
    let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let goal = &json["sessions"][session_id.as_str()];

    assert_eq!(goal["objective"], "persist goal state updates");
    assert_eq!(goal["state"], "blocked");
    assert_eq!(goal["reason"], "waiting for persisted blocker");
    assert_eq!(goal["progress"]["label"], "1/2");
    assert_eq!(goal["auto_continue"], false);
}

#[test]
fn lua_goal_state_restores_for_same_resumed_session_id() {
    let runtime = tempfile::TempDir::new().expect("create shared runtime root");
    let session_id = {
        let mut app = TestApp::builder().with_runtime_home(runtime.path()).build();
        let session_id = app.session_snapshot().id.clone();
        assert!(app.run_lua(
            r#"
                local goal = require("smelt.goal")
                assert(goal.create("restore persisted goal on resume", { auto_continue = false }))
                assert(goal.update_status({ progress = "saved" }))
            "#,
        ));
        session_id
    };

    let mut resumed = TestApp::builder().with_runtime_home(runtime.path()).build();
    resumed.set_session_id_for_harness(session_id);

    assert!(resumed.run_lua(
        r#"
            local current = assert(require("smelt.goal").current())
            assert(current.objective == "restore persisted goal on resume")
            assert(current.state == "active")
            assert(current.auto_continue == false)
            assert(current.progress.label == "saved")
        "#,
    ));
}

#[test]
fn lua_goal_banner_prefers_live_progress() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(72, 16);

    assert!(app.run_lua(
        r#"
            local goal = require("smelt.goal")
            assert(goal.create("continue implementing the full goal progress plan", { auto_continue = true, summary = "Goal progress UI" }))
            assert(goal.update_status({ progress = "Step 3/7, wiring status banner" }))
        "#,
    ));

    let frame = app.render_to_frame();
    assert!(
        frame.rows[0].contains(" GOAL Goal progress UI · Step 3/7, wiring status banner"),
        "frame:\n{}",
        frame.text()
    );
    assert!(
        frame.rows[0].trim_end().ends_with("auto"),
        "frame:\n{}",
        frame.text()
    );
    assert!(
        !frame.rows[0].contains("continue implementing the full goal progress plan"),
        "banner should be glanceable and leave the full objective for /goal status:\n{}",
        frame.text()
    );
}

#[test]
fn lua_goal_banner_keeps_progress_visible_with_long_objective() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(48, 16);

    assert!(app.run_lua(
        r#"
            local goal = require("smelt.goal")
            assert(goal.create("finish a very long objective that would otherwise hide the stage label", { auto_continue = true }))
            assert(goal.update_status({ progress = "Step 1/3, diagnosing" }))
        "#,
    ));

    let frame = app.render_to_frame();
    assert!(
        frame.rows[0].contains("Step 1/3, diagnosing"),
        "progress should stay visible even when the objective is truncated:\n{}",
        frame.text()
    );
    assert!(frame.rows[0].contains('…'), "frame:\n{}", frame.text());
    assert!(
        frame.rows[0].trim_end().ends_with("auto"),
        "frame:\n{}",
        frame.text()
    );
}

#[test]
fn lua_goal_banner_truncates_long_progress_and_preserves_mode() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(32, 16);

    assert!(app.run_lua(
        r#"
            local goal = require("smelt.goal")
            assert(goal.create("keep this objective visible when possible", { auto_continue = true, summary = "Goal summary" }))
            assert(goal.update_status({ progress = "Step 123/456, validating extremely detailed migration output" }))
        "#,
    ));

    let frame = app.render_to_frame();
    assert_eq!(
        frame.rows[0].chars().count(),
        32,
        "frame:\n{}",
        frame.text()
    );
    assert!(frame.rows[0].contains('…'), "frame:\n{}", frame.text());
    assert!(
        frame.rows[0].trim_end().ends_with("auto"),
        "mode should remain visible when progress is truncated:\n{}",
        frame.text()
    );
}

#[test]
fn lua_goal_banner_preserves_mode_when_fixed_chrome_overflows() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(10, 16);

    assert!(app.run_lua(
        r#"
            local goal = require("smelt.goal")
            assert(goal.create("tiny", { auto_continue = false }))
        "#,
    ));

    let frame = app.render_to_frame();
    assert_eq!(
        frame.rows[0].chars().count(),
        10,
        "frame:\n{}",
        frame.text()
    );
    assert!(
        frame.rows[0].trim_end().ends_with("manual"),
        "mode should be preserved even when label and mode fill the row:\n{}",
        frame.text()
    );
}

#[test]
fn lua_goal_banner_stays_above_transcript_scroll_pill() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(60, 16);

    assert!(app.run_lua(
        r#"
            local goal = require("smelt.goal")
            assert(goal.create("keep the banner visible", { auto_continue = false }))
        "#,
    ));
    app.push_transcript_block(smelt_core::transcript_model::Block::User {
        text: "earlier user message".into(),
        image_labels: Vec::new(),
        command: false,
        sent_at_ms: None,
    });
    for i in 0..40 {
        app.push_transcript_block(smelt_core::transcript_model::Block::Text {
            content: format!("assistant row {i:02}").into(),
        });
    }
    app.render_silent();
    assert!(app.run_lua(r#"smelt.win.transcript():reveal(30, { cursor = true })"#));

    let frame = app.render_to_frame();
    assert!(
        frame.rows[0].contains(" GOAL keep the banner visible"),
        "frame:\n{}",
        frame.text()
    );
    assert!(
        !frame.rows[0].contains("earlier user message"),
        "scroll pill should not cover the goal banner:\n{}",
        frame.text()
    );
}

#[test]
fn lua_goal_banner_uses_status_labels_and_unicode_ellipsis() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(36, 16);

    assert!(app.run_lua(
        r#"
            local goal = require("smelt.goal")
            assert(goal.create("finish a very long objective that must be truncated", { auto_continue = true }))
        "#,
    ));
    let frame = app.render_to_frame();
    assert!(
        frame.rows[0].starts_with(" GOAL "),
        "frame:\n{}",
        frame.text()
    );
    assert!(frame.rows[0].contains('…'), "frame:\n{}", frame.text());
    assert!(
        frame.rows[0].trim_end().ends_with("auto"),
        "frame:\n{}",
        frame.text()
    );

    assert!(app.run_lua(r#"assert(require("smelt.goal").pause())"#));
    let frame = app.render_to_frame();
    assert!(
        frame.rows[0].starts_with(" PAUSED "),
        "frame:\n{}",
        frame.text()
    );
    assert!(
        !frame.rows[0].trim_end().ends_with("paused"),
        "paused label already communicates state:\n{}",
        frame.text()
    );

    assert!(app.run_lua(r#"assert(require("smelt.goal").block("waiting"))"#));
    let frame = app.render_to_frame();
    assert!(
        frame.rows[0].contains("waiting"),
        "blocked banner should show the blocker reason:\n{}",
        frame.text()
    );
    assert!(
        frame.rows[0].starts_with(" BLOCKED "),
        "frame:\n{}",
        frame.text()
    );
    assert!(
        !frame.rows[0].trim_end().ends_with("blocked"),
        "blocked label already communicates state:\n{}",
        frame.text()
    );
}

#[test]
fn lua_submit_command_continuation_carries_last_turn_elapsed_without_using_queue() {
    let mut app = TestApp::builder().build();
    let _ = app.drain_engine_sends();

    app.type_text("initial request");
    app.press(crossterm::event::KeyCode::Enter);
    assert!(app.agent_running());
    let _ = app.drain_engine_sends();
    app.feed_one(SourceEvent::Tick(750));
    app.discard_turn(crate::app::TurnEnd::Complete);
    let token = app
        .conversation_probe()
        .pending_continuation_token()
        .expect("completed turn continuation token");
    app.feed_one(SourceEvent::Tick(1200));

    assert!(app.run_lua(&format!(
        r#"
            assert(smelt.engine.submit_command_continuation("goal", "continue body", nil, "goal continue", {}) == false)
            assert(smelt.engine.submit_command_continuation("goal", "continue body", nil, "goal continue", {}))
        "#,
        token + 1,
        token
    )));

    assert!(app.prompt_probe().queue_is_empty());
    assert_eq!(
        app.working_probe().elapsed(),
        Some(std::time::Duration::from_millis(750))
    );

    app.feed_one(SourceEvent::Tick(250));
    assert_eq!(
        app.working_probe().elapsed(),
        Some(std::time::Duration::from_millis(1000))
    );
}
