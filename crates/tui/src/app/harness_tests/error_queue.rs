use super::*;

#[test]
fn turn_error_preserves_request_queue() {
    let mut app = TestApp::builder().build();
    app.start_turn(1);
    app.steer("steer during error");

    app.feed_one(SourceEvent::engine(EngineEvent::TurnError {
        message: "connection failed".to_string(),
        kind: None,
        retry_at_ms: None,
    }));

    assert!(!app.agent_running(), "error should end the active turn");
    assert_eq!(
        app.queued_message_count(),
        1,
        "request-stage queued input should be preserved on error"
    );
    assert_eq!(
        app.working_probe().last_outcome(),
        Some(smelt_core::working::TurnOutcome::Errored),
        "error should archive an error outcome"
    );

    // Plugins observe the turn_end event; it must signal interruption on error.
    assert!(
        app.core_probe()
            .signals
            .get::<smelt_core::signals::TurnEnd>("turn_end")
            .is_some_and(|end| end.cancelled),
        "turn_end event should be cancelled on error"
    );
}

#[test]
fn turn_error_preserves_turn_queue() {
    let mut app = TestApp::builder().build();
    app.start_turn(1);
    app.push_queued_message("next turn after error".to_string());

    app.feed_one(SourceEvent::engine(EngineEvent::TurnError {
        message: "quota exceeded".to_string(),
        kind: None,
        retry_at_ms: None,
    }));

    assert!(!app.agent_running(), "error should end the active turn");
    let state = app.state();
    assert_eq!(
        state.queued_inputs,
        vec!["next turn after error".to_string()],
        "turn-stage queued input should be preserved on error"
    );
    // A queued turn-stage message must not auto-start after an error.
    let started_turn = app.actions().iter().any(|action| match action {
        Action::EngineSend(cmd) => matches!(cmd.as_ref(), protocol::UiCommand::StartTurn(_)),
        _ => false,
    });
    assert!(
        !started_turn,
        "queued turn should not auto-start after an error"
    );

    // The status bar should record an error outcome, not done.
    assert_eq!(
        app.working_probe().last_outcome(),
        Some(smelt_core::working::TurnOutcome::Errored)
    );
}

#[test]
fn public_status_cancelled_turn_is_idle_interrupted() {
    let mut app = TestApp::builder().build();
    app.set_terminal_focus_for_harness(false);
    app.start_turn(1);

    app.discard_turn(crate::app::TurnEnd::Cancelled);
    let (state, reason) = app.public_status_state_reason();
    assert_eq!(state, smelt_core::public_status::PublicState::Idle);
    assert_eq!(
        reason,
        Some(smelt_core::public_status::PublicReason::Interrupted)
    );
}

#[test]
fn public_status_turn_error_needs_attention() {
    let mut app = TestApp::builder().build();
    app.start_turn(1);

    app.feed_one(SourceEvent::engine(EngineEvent::TurnError {
        message: "connection failed".to_string(),
        kind: None,
        retry_at_ms: None,
    }));
    let (state, reason) = app.public_status_state_reason();
    assert_eq!(
        state,
        smelt_core::public_status::PublicState::NeedsAttention
    );
    assert_eq!(reason, Some(smelt_core::public_status::PublicReason::Error));
}

#[test]
fn resumable_turn_error_publishes_continuation_token() {
    let mut app = TestApp::builder().build();
    app.start_turn(1);

    app.feed_one(SourceEvent::engine(EngineEvent::TurnError {
        message: "quota exceeded".to_string(),
        kind: Some(protocol::EngineAskErrorKind::Quota),
        retry_at_ms: Some(123_000),
    }));

    let turn_end = app
        .core_probe()
        .signals
        .get::<smelt_core::signals::TurnEnd>("turn_end")
        .expect("turn_end should be published");
    assert!(turn_end.cancelled);
    assert_eq!(turn_end.error_kind.as_deref(), Some("quota"));
    assert_eq!(turn_end.retry_at_ms, Some(123_000));
    assert!(turn_end.continuation_token.is_some());
}

#[test]
fn non_quota_retry_metadata_does_not_publish_continuation_token() {
    let mut app = TestApp::builder().build();
    app.start_turn(1);

    app.feed_one(SourceEvent::engine(EngineEvent::TurnError {
        message: "network failed".to_string(),
        kind: Some(protocol::EngineAskErrorKind::Network),
        retry_at_ms: Some(123_000),
    }));

    let turn_end = app
        .core_probe()
        .signals
        .get::<smelt_core::signals::TurnEnd>("turn_end")
        .expect("turn_end should be published");
    assert!(turn_end.cancelled);
    assert_eq!(turn_end.error_kind.as_deref(), Some("network"));
    assert_eq!(turn_end.retry_at_ms, Some(123_000));
    assert!(turn_end.continuation_token.is_none());
}

fn run_due_timers(app: &mut TestApp, ms: u64) -> Vec<protocol::UiCommand> {
    app.feed_one(SourceEvent::Tick(ms));
    app.tick_timers();
    app.drain_engine_sends()
}

fn has_started_turn(cmds: &[protocol::UiCommand]) -> bool {
    cmds.iter()
        .any(|cmd| matches!(cmd, protocol::UiCommand::StartTurn(_)))
}

fn start_canonical_turn(app: &mut TestApp) -> u64 {
    app.type_text("initial request");
    app.press(crossterm::event::KeyCode::Enter);
    let turn_id = app.current_turn_id().expect("canonical turn starts");
    let _ = app.drain_engine_sends();
    turn_id
}

fn clear_goal(app: &mut TestApp) {
    assert!(app.run_lua(r#"require("smelt.goal").clear()"#));
}

fn create_auto_goal(app: &mut TestApp, objective: &str) {
    clear_goal(app);
    assert!(app.run_lua(&format!(
        r#"assert(require("smelt.goal").create({objective:?}, {{ auto_continue = true }}))"#
    )));
}

fn isolated_app() -> TestApp {
    with_advancing_clock(TestApp::builder().build())
}

fn with_advancing_clock(app: TestApp) -> TestApp {
    // Timer scenarios use the advancing host clock rather than the frozen storybook clock.
    let clock = app.clock.clone();
    let lua = &app.lua_probe().lua;
    lua.globals()
        .get::<mlua::Table>("smelt")
        .unwrap()
        .get::<mlua::Table>("time")
        .unwrap()
        .set(
            "now_ms",
            lua.create_function(move |_, ()| Ok(engine::clock::unix_time_ms(clock.as_ref())))
                .unwrap(),
        )
        .unwrap();
    app
}

#[test]
fn goal_auto_continues_after_recoverable_quota_error() {
    let mut app = isolated_app();
    create_auto_goal(&mut app, "finish quota test");
    start_canonical_turn(&mut app);

    app.feed_one(SourceEvent::engine(EngineEvent::TurnError {
        message: "quota exceeded".to_string(),
        kind: Some(protocol::EngineAskErrorKind::Quota),
        retry_at_ms: Some(0),
    }));

    assert!(has_started_turn(&run_due_timers(&mut app, 1300)));
}

#[test]
fn auto_continue_off_disables_quota_retry() {
    let mut app = isolated_app();
    assert!(app.run_lua(r#"smelt.settings.auto_continue = "off""#));
    create_auto_goal(&mut app, "finish quota test");
    start_canonical_turn(&mut app);

    app.feed_one(SourceEvent::engine(EngineEvent::TurnError {
        message: "quota exceeded".to_string(),
        kind: Some(protocol::EngineAskErrorKind::Quota),
        retry_at_ms: Some(0),
    }));

    assert!(!has_started_turn(&run_due_timers(&mut app, 1300)));
}

#[test]
fn auto_continue_always_continues_without_goal() {
    let mut app = isolated_app();
    assert!(app.run_lua(r#"smelt.settings.auto_continue = "always""#));
    clear_goal(&mut app);
    let turn_id = start_canonical_turn(&mut app);

    app.feed_one(SourceEvent::engine(EngineEvent::TurnComplete {
        turn_id,
        history: None,
        meta: None,
    }));

    assert!(has_started_turn(&run_due_timers(&mut app, 1300)));
}

#[test]
fn auto_continue_goal_mode_ignores_sessions_without_goal() {
    let mut app = isolated_app();
    clear_goal(&mut app);
    let turn_id = start_canonical_turn(&mut app);

    app.feed_one(SourceEvent::engine(EngineEvent::TurnComplete {
        turn_id,
        history: None,
        meta: None,
    }));

    assert!(!has_started_turn(&run_due_timers(&mut app, 1300)));
}

#[test]
fn goal_auto_continue_ignores_non_quota_errors() {
    let mut app = isolated_app();
    create_auto_goal(&mut app, "finish quota test");
    start_canonical_turn(&mut app);

    app.feed_one(SourceEvent::engine(EngineEvent::TurnError {
        message: "network failed".to_string(),
        kind: Some(protocol::EngineAskErrorKind::Network),
        retry_at_ms: Some(0),
    }));

    assert!(!has_started_turn(&run_due_timers(&mut app, 1300)));
}

fn quota_error(app: &mut TestApp, retry_at_ms: Option<u64>) {
    app.feed_one(SourceEvent::engine(EngineEvent::TurnError {
        message: "quota exceeded".into(),
        kind: Some(protocol::EngineAskErrorKind::Quota),
        retry_at_ms,
    }));
}

fn complete_background_job(app: &mut TestApp) {
    app.app
        .handle_job_completed(smelt_core::process::JobCompletion {
            id: "quota-test-job".into(),
            exit_code: Some(0),
            termination: protocol::JobTermination::Exited,
        });
}

#[test]
fn quota_pause_blocks_idle_dispatch_and_keeps_both_queue_stages_visible() {
    let mut app = isolated_app();
    start_canonical_turn(&mut app);
    app.steer("steer after recovery");
    app.type_text("next turn");
    app.press(crossterm::event::KeyCode::Enter);
    let _ = app.drain_engine_sends();
    quota_error(&mut app, Some(123_000));

    assert!(!app.start_next_queued_input_if_idle());
    assert_eq!(app.state().queued_inputs.len(), 2);
    assert!(app.run_lua(
        r#"
        local rows = smelt.prompt.queued_rows()
        assert(#rows == 2)
        assert(rows[1].kind == "request" and rows[1].text == "steer after recovery")
        assert(rows[2].kind == "turn" and rows[2].text == "next turn")
    "#
    ));
    assert!(!has_started_turn(&run_due_timers(&mut app, 1300)));
}

#[test]
fn quota_retry_resumes_original_work_without_consuming_the_turn_queue() {
    let mut app = isolated_app();
    create_auto_goal(&mut app, "finish original work");
    start_canonical_turn(&mut app);
    let original_history = app.session_history().to_vec();
    app.feed_one(SourceEvent::Tick(1_000));
    let sent_at_ms = engine::clock::unix_time_ms(app.clock.as_ref());
    app.steer("pending steering");
    app.push_queued_message("next turn".into());
    let _ = app.drain_engine_sends();
    quota_error(&mut app, Some(0));
    let cmds = run_due_timers(&mut app, 1300);
    assert_eq!(
        cmds.iter()
            .filter(|cmd| matches!(cmd, protocol::UiCommand::StartTurn(_)))
            .count(),
        1
    );
    assert!(cmds.iter().any(|cmd| matches!(cmd, protocol::UiCommand::StartTurn(payload) if payload.input.provider_content().is_empty())));
    assert!(cmds
        .iter()
        .any(|cmd| matches!(cmd, protocol::UiCommand::Steer { input }
        if input.provider_content().text_content() == "pending steering"
            && input.sent_at_ms() == Some(sent_at_ms))));
    assert_eq!(app.state().queued_inputs.len(), 2);
    assert_eq!(app.session_history(), original_history);
}

#[test]
fn quota_resume_preserves_command_scope_across_reload_and_cancellation() {
    let config = tempfile::tempdir().unwrap();
    let init = config.path().join("init.lua");
    std::fs::write(
        &init,
        r#"
        smelt.provider.register("test", {
            type = "openai-compatible", api_base = "https://example.invalid/v1",
            models = { "test-model" },
        })
    "#,
    )
    .unwrap();
    for (manual, cancel) in [(false, false), (true, false), (true, true)] {
        let mut app = with_advancing_clock(TestApp::builder().with_init_lua(&init).build());
        create_auto_goal(&mut app, "resume scoped request");
        assert!(app.run_lua(
            r#"
            smelt.engine.submit_command("scoped", "scoped request", {
                model = smelt.model.current(), temperature = 0.3,
                reasoning_effort = "high", tools = { deny = { "bash" } },
            })
        "#
        ));
        assert!(app.agent_running());
        app.push_queued_message("next turn".into());
        let _ = app.drain_engine_sends();
        quota_error(&mut app, Some(0));
        if cancel {
            assert!(app.run_lua("smelt.engine.cancel()"));
        }
        app.reload_lua();
        let commands = if manual {
            app.press(crossterm::event::KeyCode::Enter);
            Vec::new()
        } else {
            run_due_timers(&mut app, 1300)
        };
        assert!(app.agent_running(), "{}", app.render_to_frame().text());
        let resumed = commands
            .iter()
            .chain(app.actions().iter().filter_map(|action| {
                if let Action::EngineSend(command) = action {
                    Some(command.as_ref())
                } else {
                    None
                }
            }))
            .find_map(|command| match command {
                protocol::UiCommand::StartTurn(payload)
                    if payload.input.provider_content().is_empty() =>
                {
                    Some(payload)
                }
                _ => None,
            })
            .expect("resumed request");
        assert_eq!(resumed.model_target.config.temperature, Some(0.3));
        assert_eq!(resumed.reasoning_effort, protocol::ReasoningEffort::High);
        assert_eq!(
            resumed
                .permission_overrides
                .as_ref()
                .unwrap()
                .tools
                .as_ref()
                .unwrap()
                .deny,
            ["bash"]
        );
        assert_eq!(app.state().queued_inputs, vec!["next turn"]);
        let frame = app.render_to_frame().text();
        assert!(!frame.lines().any(|line| line.trim() == "/"), "{frame}");

        let turn_id = app.current_turn_id().unwrap();
        app.feed_one(SourceEvent::engine(EngineEvent::TurnComplete {
            turn_id,
            history: None,
            meta: None,
        }));
        assert!(app
            .actions()
            .iter()
            .any(|action| matches!(action, Action::EngineSend(cmd)
            if matches!(cmd.as_ref(), protocol::UiCommand::StartTurn(payload)
                if payload.input.provider_content().text_content() == "next turn"
                    && payload.permission_overrides.is_none()
                    && payload.model_target.config.temperature != Some(0.3)))));
    }
}

#[test]
fn quota_resume_uses_updated_global_settings_only_without_command_overrides() {
    for scoped in [false, true] {
        let mut app = isolated_app();
        let original = app.core_probe().config.available_models[0].clone();
        let mut alternate = original.clone();
        alternate.key = "test/alternate-model".into();
        alternate.model_name = "alternate-model".into();
        app.set_available_models(vec![original.clone(), alternate]);
        assert!(app.run_lua(&format!(
            r#"
            smelt.engine.submit_command("scoped", "scoped request", {{
                model = {scoped} and smelt.model.current() or nil,
                reasoning_effort = {scoped} and "high" or nil,
            }})
        "#
        )));
        assert!(app.agent_running());
        quota_error(&mut app, Some(0));
        app.apply_model("test/alternate-model", true);
        assert!(app.run_lua(r#"smelt.reasoning.set("low")"#));
        app.press(crossterm::event::KeyCode::Enter);
        let expected_model = if scoped {
            original.model_name.as_str()
        } else {
            "alternate-model"
        };
        let expected_effort = if scoped {
            protocol::ReasoningEffort::High
        } else {
            protocol::ReasoningEffort::Low
        };
        assert!(app
            .actions()
            .iter()
            .any(|action| matches!(action, Action::EngineSend(command)
            if matches!(command.as_ref(), protocol::UiCommand::StartTurn(payload)
                if payload.input.provider_content().is_empty()
                    && payload.model_target.model == expected_model
                    && payload.reasoning_effort == expected_effort))));
    }
}

#[test]
fn cancelling_quota_pause_also_cancels_foreground_busy_work() {
    let mut app = isolated_app();
    create_auto_goal(&mut app, "cancel foreground work");
    start_canonical_turn(&mut app);
    app.push_queued_message("keep queued".into());
    quota_error(&mut app, Some(0));
    assert!(app.run_lua(r#"_G.quota_busy = smelt.work.busy("foreground work")"#));
    app.press(crossterm::event::KeyCode::Esc);
    app.press(crossterm::event::KeyCode::Esc);
    assert!(app.run_lua("assert(not smelt.work.is_busy())"));
    assert!(!has_started_turn(&run_due_timers(&mut app, 300_000)));
    assert_eq!(app.state().queued_inputs, vec!["keep queued"]);
    app.press(crossterm::event::KeyCode::Enter);
    assert!(app.agent_running());
}

#[test]
fn cancelling_from_quota_turn_end_does_not_finalize_the_turn_twice() {
    let mut app = isolated_app();
    create_auto_goal(&mut app, "cancel from a callback");
    assert!(app.run_lua(
        r#"
        _G.quota_turn_ends = 0
        smelt.events.on("turn_end", function(ev)
            _G.quota_turn_ends = _G.quota_turn_ends + 1
            if ev.error_kind == "quota" then
                _G.quota_busy = smelt.work.busy("foreground work")
                local guard = smelt.work.guard()
                smelt.engine.cancel()
                assert(not smelt.work.guard_current(guard))
            end
        end)
    "#
    ));
    start_canonical_turn(&mut app);
    app.push_queued_message("keep queued".into());
    quota_error(&mut app, Some(0));
    assert!(app.run_lua(
        r#"
        assert(_G.quota_turn_ends == 1)
        local state = smelt.engine.continuation_state()
        assert(state.paused and state.error_kind == "quota" and state.token == nil)
        assert(not smelt.work.is_busy())
    "#
    ));
    assert_eq!(
        app.working_probe().last_outcome(),
        Some(smelt_core::working::TurnOutcome::Errored)
    );
    assert!(!has_started_turn(&run_due_timers(&mut app, 300_000)));
    assert_eq!(app.state().queued_inputs, vec!["keep queued"]);
    app.press(crossterm::event::KeyCode::Enter);
    assert!(app.agent_running());
}

#[test]
fn quota_recovery_keeps_original_deadline_when_a_retry_omits_metadata() {
    for kind in [
        protocol::EngineAskErrorKind::Quota,
        protocol::EngineAskErrorKind::RateLimited,
    ] {
        let mut app = isolated_app();
        create_auto_goal(&mut app, "retain known reset");
        start_canonical_turn(&mut app);
        let reset = engine::clock::unix_time_ms(app.clock.as_ref()) + 150_000;
        quota_error(&mut app, Some(reset));
        assert!(has_started_turn(&run_due_timers(&mut app, 60_000)));
        app.feed_one(SourceEvent::engine(EngineEvent::TurnError {
            message: "provider limit".into(),
            kind: Some(kind),
            retry_at_ms: None,
        }));
        assert!(app.run_lua(r#"smelt.settings.auto_continue = "off""#));
        app.reload_lua();
        assert!(app.run_lua(r#"smelt.settings.auto_continue = "goal""#));
        assert!(!has_started_turn(&run_due_timers(&mut app, 90_999)));
        assert!(has_started_turn(&run_due_timers(&mut app, 1)));
        quota_error(&mut app, None);
        assert!(!has_started_turn(&run_due_timers(&mut app, 239_999)));
        assert!(has_started_turn(&run_due_timers(&mut app, 1)));
    }
}

#[test]
fn quota_without_reset_requires_manual_resume_even_in_always_mode() {
    let mut app = isolated_app();
    assert!(app.run_lua(r#"smelt.settings.auto_continue = "always""#));
    start_canonical_turn(&mut app);
    app.push_queued_message("next turn".into());
    quota_error(&mut app, None);
    assert!(!has_started_turn(&run_due_timers(&mut app, 60_000)));
    app.press(crossterm::event::KeyCode::Enter);
    assert!(app.agent_running());
    assert!(app.actions().iter().any(|action| matches!(action, Action::EngineSend(cmd)
        if matches!(cmd.as_ref(), protocol::UiCommand::StartTurn(payload) if payload.input.provider_content().is_empty()))));
    assert_eq!(app.state().queued_inputs.len(), 1);
}

#[test]
fn quota_retry_waits_for_busy_work_and_drafts_without_losing_the_continuation() {
    for (reset_delay, first_attempt_delay) in [(0, 1300), (3_600_000, 60_000)] {
        for draft in [false, true] {
            let mut app = isolated_app();
            create_auto_goal(&mut app, "wait for idle");
            start_canonical_turn(&mut app);
            let reset = engine::clock::unix_time_ms(app.clock.as_ref()) + reset_delay;
            quota_error(&mut app, Some(reset));
            if draft {
                app.type_text("unfinished draft");
            } else {
                assert!(app.run_lua(r#"_G.quota_busy = smelt.work.busy("background work")"#));
            }
            assert!(!has_started_turn(&run_due_timers(
                &mut app,
                first_attempt_delay
            )));
            if draft {
                assert!(app.run_lua(r#"smelt.prompt.set_text("")"#));
            } else {
                assert!(app.run_lua("_G.quota_busy:remove()"));
            }
            assert!(has_started_turn(&run_due_timers(&mut app, 300)));
            quota_error(&mut app, Some(reset));
            assert!(!has_started_turn(&run_due_timers(&mut app, 119_999)));
            assert!(has_started_turn(&run_due_timers(&mut app, 1)));
        }
    }
}

#[test]
fn normal_auto_continue_waits_for_background_busy_token() {
    let mut app = isolated_app();
    create_auto_goal(&mut app, "wait for background work");
    let turn_id = start_canonical_turn(&mut app);
    app.feed_one(SourceEvent::engine(EngineEvent::TurnComplete {
        turn_id,
        history: None,
        meta: None,
    }));
    assert!(app.run_lua(r#"_G.quota_busy = smelt.work.busy("background work")"#));
    assert!(!has_started_turn(&run_due_timers(&mut app, 1300)));
    assert!(app.run_lua("_G.quota_busy:remove()"));
    assert!(has_started_turn(&run_due_timers(&mut app, 300)));
}

#[test]
fn background_completion_cannot_bypass_quota_pause_and_is_not_lost() {
    for before_error in [false, true] {
        let mut app = isolated_app();
        create_auto_goal(&mut app, "resume with process result");
        start_canonical_turn(&mut app);
        if before_error {
            complete_background_job(&mut app);
        }
        quota_error(&mut app, Some(0));
        if !before_error {
            complete_background_job(&mut app);
        }
        assert!(!app.agent_running());
        assert!(!has_started_turn(&app.drain_engine_sends()));
        assert!(has_started_turn(&run_due_timers(&mut app, 1300)));
        let history = app.session_snapshot().history;
        assert_eq!(
            history
                .iter()
                .filter_map(protocol::HistoryItem::as_note)
                .filter(|note| note.text().contains("quota-test-job"))
                .count(),
            1
        );
    }
}

#[test]
fn background_follow_up_invalidates_old_auto_continue_timer() {
    let mut app = isolated_app();
    create_auto_goal(&mut app, "handle background result");
    let turn_id = start_canonical_turn(&mut app);
    app.feed_one(SourceEvent::engine(EngineEvent::TurnComplete {
        turn_id,
        history: None,
        meta: None,
    }));
    complete_background_job(&mut app);
    assert!(has_started_turn(&app.drain_engine_sends()));
    assert!(!has_started_turn(&run_due_timers(&mut app, 1300)));
    let turn_id = app.current_turn_id().unwrap();
    app.feed_one(SourceEvent::engine(EngineEvent::TurnComplete {
        turn_id,
        history: None,
        meta: None,
    }));
    assert!(has_started_turn(&run_due_timers(&mut app, 1300)));
}

#[test]
fn cancelling_quota_wait_preserves_queue_and_blocks_background_wakeups() {
    let mut app = isolated_app();
    create_auto_goal(&mut app, "cancel wait");
    start_canonical_turn(&mut app);
    app.push_queued_message("keep this message".into());
    quota_error(&mut app, Some(0));
    assert!(app.run_lua("smelt.engine.cancel()"));
    complete_background_job(&mut app);
    assert!(!has_started_turn(&run_due_timers(&mut app, 1300)));
    assert_eq!(app.state().queued_inputs.len(), 1);
    assert!(!app.start_next_queued_input_if_idle());
}

#[test]
fn changing_auto_continue_setting_updates_quota_wait() {
    let mut app = isolated_app();
    assert!(app.run_lua(r#"smelt.settings.auto_continue = "always""#));
    start_canonical_turn(&mut app);
    quota_error(&mut app, Some(0));
    assert!(app.run_lua(r#"smelt.settings.auto_continue = "off""#));
    assert!(!has_started_turn(&run_due_timers(&mut app, 1300)));
    assert!(app.run_lua(r#"smelt.settings.auto_continue = "always""#));
    assert!(has_started_turn(&run_due_timers(&mut app, 1300)));
}

#[test]
fn repeated_expired_quota_deadline_keeps_backoff_across_reload() {
    let mut app = isolated_app();
    create_auto_goal(&mut app, "bounded quota recovery");
    start_canonical_turn(&mut app);
    quota_error(&mut app, Some(0));
    assert!(has_started_turn(&run_due_timers(&mut app, 1300)));
    quota_error(&mut app, Some(0));
    assert!(!has_started_turn(&run_due_timers(&mut app, 60_000)));

    let generation = app.lua_probe().id;
    app.type_text("/reload");
    app.press(crossterm::event::KeyCode::Enter);
    assert_eq!(app.lua_probe().id, generation + 1);
    assert!(!has_started_turn(&run_due_timers(&mut app, 59_999)));
    assert!(has_started_turn(&run_due_timers(&mut app, 1)));
    quota_error(&mut app, Some(0));
    assert!(!has_started_turn(&run_due_timers(&mut app, 239_999)));
    assert!(has_started_turn(&run_due_timers(&mut app, 1)));
}

#[test]
fn hybrid_quota_retry_backs_off_to_five_minutes_before_reset() {
    for kind in [
        protocol::EngineAskErrorKind::Quota,
        protocol::EngineAskErrorKind::RateLimited,
    ] {
        let mut app = isolated_app();
        create_auto_goal(&mut app, "detect an early reset");
        start_canonical_turn(&mut app);
        app.push_queued_message("keep queued".into());
        let reset = engine::clock::unix_time_ms(app.clock.as_ref()) + 3_600_000;
        for delay in [60_000, 120_000, 240_000, 300_000, 300_000] {
            app.feed_one(SourceEvent::engine(EngineEvent::TurnError {
                message: "provider limit".into(),
                kind: Some(kind),
                retry_at_ms: Some(reset),
            }));
            assert!(!has_started_turn(&run_due_timers(&mut app, delay - 1)));
            let cmds = run_due_timers(&mut app, 1);
            assert_eq!(
                cmds.iter()
                    .filter(|cmd| matches!(cmd, protocol::UiCommand::StartTurn(_)))
                    .count(),
                1,
                "retry after {delay}ms"
            );
            assert_eq!(app.state().queued_inputs, vec!["keep queued"]);
        }
    }
}

#[test]
fn hybrid_quota_retry_preserves_original_reset_when_estimates_move_later() {
    let mut app = isolated_app();
    create_auto_goal(&mut app, "honor the original reset");
    start_canonical_turn(&mut app);
    let now = engine::clock::unix_time_ms(app.clock.as_ref());
    let reset = now + 150_000;
    quota_error(&mut app, Some(reset));
    assert!(has_started_turn(&run_due_timers(&mut app, 60_000)));
    quota_error(&mut app, Some(now + 3_600_000));
    assert!(!has_started_turn(&run_due_timers(&mut app, 90_999)));
    assert!(has_started_turn(&run_due_timers(&mut app, 1)));

    quota_error(&mut app, Some(reset));
    assert!(!has_started_turn(&run_due_timers(&mut app, 239_999)));
    assert!(has_started_turn(&run_due_timers(&mut app, 1)));
}

#[test]
fn hybrid_quota_retry_keeps_next_attempt_across_policy_changes_and_reload() {
    let mut app = isolated_app();
    create_auto_goal(&mut app, "preserve the next attempt");
    start_canonical_turn(&mut app);
    let now = engine::clock::unix_time_ms(app.clock.as_ref());
    quota_error(&mut app, Some(now + 3_600_000));
    assert!(!has_started_turn(&run_due_timers(&mut app, 30_000)));
    assert!(app.run_lua(r#"smelt.settings.auto_continue = "off""#));
    assert!(!has_started_turn(&run_due_timers(&mut app, 10_000)));
    assert!(app.run_lua(r#"smelt.settings.auto_continue = "goal""#));
    assert!(!has_started_turn(&run_due_timers(&mut app, 10_000)));
    app.reload_lua();
    assert!(!has_started_turn(&run_due_timers(&mut app, 9_999)));
    assert!(has_started_turn(&run_due_timers(&mut app, 1)));
}

#[test]
fn cancelling_hybrid_quota_retry_clears_backoff_without_losing_queued_work() {
    let mut app = isolated_app();
    create_auto_goal(&mut app, "cancel periodic retries");
    start_canonical_turn(&mut app);
    app.push_queued_message("keep queued".into());
    let reset = engine::clock::unix_time_ms(app.clock.as_ref()) + 3_600_000;
    quota_error(&mut app, Some(reset));
    assert!(has_started_turn(&run_due_timers(&mut app, 60_000)));
    quota_error(&mut app, Some(reset));
    app.press(crossterm::event::KeyCode::Esc);
    app.press(crossterm::event::KeyCode::Esc);
    complete_background_job(&mut app);
    app.reload_lua();
    assert!(!has_started_turn(&run_due_timers(&mut app, 3_600_000)));
    assert_eq!(app.state().queued_inputs, vec!["keep queued"]);
    assert!(!app.start_next_queued_input_if_idle());
    app.press(crossterm::event::KeyCode::Enter);
    assert!(app.agent_running());
}

#[test]
fn hybrid_quota_retry_resets_backoff_after_success_or_manual_resume() {
    for manual in [false, true] {
        let mut app = isolated_app();
        create_auto_goal(&mut app, "reset recovered backoff");
        start_canonical_turn(&mut app);
        let reset = engine::clock::unix_time_ms(app.clock.as_ref()) + 3_600_000;
        quota_error(&mut app, Some(reset));
        assert!(has_started_turn(&run_due_timers(&mut app, 60_000)));
        if manual {
            quota_error(&mut app, Some(reset));
            app.press(crossterm::event::KeyCode::Enter);
        } else {
            let turn_id = app.current_turn_id().unwrap();
            app.feed_one(SourceEvent::engine(EngineEvent::TurnComplete {
                turn_id,
                history: None,
                meta: None,
            }));
            start_canonical_turn(&mut app);
        }
        quota_error(&mut app, Some(reset));
        assert!(!has_started_turn(&run_due_timers(&mut app, 59_999)));
        assert!(has_started_turn(&run_due_timers(&mut app, 1)));
    }
}

#[test]
fn session_change_invalidates_a_scheduled_quota_retry() {
    let mut app = isolated_app();
    assert!(app.run_lua(r#"smelt.settings.auto_continue = "always""#));
    start_canonical_turn(&mut app);
    quota_error(&mut app, Some(0));
    assert!(app.run_lua("smelt.session.reset()"));
    assert!(!has_started_turn(&run_due_timers(&mut app, 1300)));
    assert!(app.run_lua(r#"assert(not smelt.engine.continuation_state().paused)"#));
}

#[test]
fn cancellation_after_completion_prevents_auto_continue_and_background_restart() {
    let mut app = isolated_app();
    create_auto_goal(&mut app, "cancel completed turn continuation");
    let turn_id = start_canonical_turn(&mut app);
    app.feed_one(SourceEvent::engine(EngineEvent::TurnComplete {
        turn_id,
        history: None,
        meta: None,
    }));
    assert!(app.run_lua("smelt.engine.cancel()"));
    complete_background_job(&mut app);
    assert!(!has_started_turn(&run_due_timers(&mut app, 1300)));
}

#[test]
fn nearby_quota_and_rate_limit_resets_preempt_the_first_periodic_retry() {
    for kind in [
        protocol::EngineAskErrorKind::Quota,
        protocol::EngineAskErrorKind::RateLimited,
    ] {
        let mut app = isolated_app();
        create_auto_goal(&mut app, "wait for the reset");
        start_canonical_turn(&mut app);
        let now = engine::clock::unix_time_ms(app.clock.as_ref());
        app.feed_one(SourceEvent::engine(EngineEvent::TurnError {
            message: "provider limit".into(),
            kind: Some(kind),
            retry_at_ms: Some(now + 5_000),
        }));
        assert!(!has_started_turn(&run_due_timers(&mut app, 5_999)));
        assert!(has_started_turn(&run_due_timers(&mut app, 1)));
        assert!(!has_started_turn(&run_due_timers(&mut app, 10_000)));
    }
}

#[test]
fn auto_continue_waits_for_modal_prompt_ownership() {
    for paused in [false, true] {
        let mut app = isolated_app();
        create_auto_goal(&mut app, "wait for a dialog");
        let turn_id = start_canonical_turn(&mut app);
        if paused {
            quota_error(&mut app, Some(0));
        } else {
            app.feed_one(SourceEvent::engine(EngineEvent::TurnComplete {
                turn_id,
                history: None,
                meta: None,
            }));
        }
        assert!(app.run_lua("_G.modal_owner = smelt.prompt.acquire()"));
        assert!(!has_started_turn(&run_due_timers(&mut app, 1300)));
        assert!(app.run_lua("_G.modal_owner:remove()"));
        assert!(has_started_turn(&run_due_timers(&mut app, 300)));
    }
}

#[test]
fn double_escape_stops_quota_retry_but_empty_enter_can_still_resume() {
    for vim in [false, true] {
        let mut app = isolated_app();
        assert!(app.run_lua(&format!("smelt.settings.vim = {vim}")));
        create_auto_goal(&mut app, "manual recovery");
        start_canonical_turn(&mut app);
        app.push_queued_message("keep queued".into());
        quota_error(&mut app, Some(0));
        app.press(crossterm::event::KeyCode::Esc);
        app.press(crossterm::event::KeyCode::Esc);
        assert!(!has_started_turn(&run_due_timers(&mut app, 1300)));
        assert_eq!(app.state().queued_inputs, vec!["keep queued"]);
        assert!(app.run_lua(
            r#"
            local status = smelt.signal.get("auto_continue_status")
            assert(status.kind == "quota" and status.phase == "paused")
            assert(status.next_attempt_at_ms == nil)
            "#
        ));
        app.press(crossterm::event::KeyCode::Enter);
        assert!(app.agent_running());
        assert_eq!(app.state().queued_inputs, vec!["keep queued"]);
    }
}

#[test]
fn quota_pause_status_redraws_on_deadline_and_cancellation() {
    let mut app = isolated_app();
    assert!(app.run_lua(r#"smelt.settings.auto_continue = "always""#));
    start_canonical_turn(&mut app);
    let now = engine::clock::unix_time_ms(app.clock.as_ref());
    quota_error(&mut app, Some(now + 5_000));
    assert!(app.run_lua(
        r#"
        local status = smelt.signal.get("auto_continue_status")
        assert(status.kind == "quota" and status.phase == "scheduled")
        assert(status.next_attempt_at_ms == smelt.time.now_ms() + 6000)
        _G.quota_status_updates = 0
        smelt.signal.subscribe("auto_continue_status", function()
            _G.quota_status_updates = _G.quota_status_updates + 1
        end)
        require("smelt.auto_continue").refresh()
        require("smelt.auto_continue").refresh()
    "#
    ));
    assert!(app.run_lua("assert(_G.quota_status_updates == 0)"));
    app.type_text("unfinished draft");
    let scheduled = app.render_to_frame().text();
    assert!(
        scheduled.contains("quota exceeded · resuming at"),
        "{scheduled}"
    );

    assert!(!has_started_turn(&run_due_timers(&mut app, 6_000)));
    let due = app.render_to_frame().text();
    assert!(due.contains("quota exceeded · resuming when idle"), "{due}");
    assert!(!due.contains("resuming at"), "{due}");
    assert!(app.run_lua(
        r#"
        assert(smelt.signal.get("auto_continue_status").phase == "waiting_for_idle")
        assert(_G.quota_status_updates == 1)
    "#
    ));

    assert!(app.run_lua("smelt.engine.cancel()"));
    let cancelled = app.render_to_frame().text();
    assert!(cancelled.contains("quota exceeded · paused"), "{cancelled}");
    assert!(!cancelled.contains("resuming"), "{cancelled}");
    assert!(app.run_lua(
        r#"
        local status = smelt.signal.get("auto_continue_status")
        assert(status.phase == "paused" and status.next_attempt_at_ms == nil)
        assert(_G.quota_status_updates == 2)
        smelt.prompt.set_text("")
    "#
    ));
    app.press(crossterm::event::KeyCode::Enter);
    assert!(app.agent_running());
    let resumed = app.render_to_frame().text();
    assert!(!resumed.contains("quota exceeded"), "{resumed}");
    assert!(app.run_lua("assert(smelt.signal.get('auto_continue_status') == nil)"));
}

#[test]
fn withdrawing_queued_goal_creation_does_not_activate_or_replace_a_goal() {
    for existing_goal in [false, true] {
        for modifiers in [KeyModifiers::NONE, KeyModifiers::CONTROL] {
            let mut app = isolated_app();
            if existing_goal {
                assert!(app.run_lua(
                    r#"
                        local goal = require("smelt.goal")
                        assert(goal.create("original objective", { auto_continue = false }))
                        assert(goal.pause())
                    "#,
                ));
            }
            let turn_id = start_canonical_turn(&mut app);
            app.feed_one(SourceEvent::engine(EngineEvent::TextDelta {
                delta: "Working on the original request".into(),
            }));
            app.type_text("/goal withdrawn objective");
            app.press_mod(KeyCode::Enter, modifiers);
            app.press(KeyCode::Esc);
            app.press(KeyCode::Esc);
            assert_eq!(app.state().prompt_text, "/goal withdrawn objective");
            app.press_mod(KeyCode::Char('c'), KeyModifiers::CONTROL);
            assert!(app.state().prompt_text.is_empty());
            assert!(app.state().queued_inputs.is_empty());
            assert_eq!(app.current_turn_id(), Some(turn_id));
            assert!(app.agent_running());
            assert!(app.run_lua(if existing_goal {
                r#"
                    local current = assert(require("smelt.goal").current())
                    assert(current.objective == "original objective")
                    assert(current.state == "paused" and not current.auto_continue)
                "#
            } else {
                r#"assert(require("smelt.goal").current() == nil)"#
            }));
            app.feed_one(SourceEvent::engine(EngineEvent::TurnComplete {
                turn_id,
                history: None,
                meta: None,
            }));
            assert!(!has_started_turn(&run_due_timers(&mut app, 1300)));
            assert!(!app.agent_running());
        }
    }
}

#[test]
fn goal_stop_controls_while_running_prevent_auto_continuation_after_turn_end() {
    for command in [
        "auto off",
        "pause",
        "block waiting",
        "done",
        "clear",
        "stop",
    ] {
        for quota in [false, true] {
            let mut app = isolated_app();
            app.type_text("/goal finish the current work");
            app.press(KeyCode::Enter);
            let turn_id = app.current_turn_id().expect("goal starts a turn");
            let _ = app.drain_engine_sends();

            app.type_text(&format!("/goal {command}"));
            app.press(KeyCode::Enter);
            assert!(app.state().queued_inputs.is_empty(), "{command}");
            assert_eq!(app.current_turn_id(), Some(turn_id));
            assert!(!has_started_turn(&run_due_timers(&mut app, 1300)));

            if quota {
                quota_error(&mut app, Some(0));
            } else {
                app.feed_one(SourceEvent::engine(EngineEvent::TurnComplete {
                    turn_id,
                    history: None,
                    meta: None,
                }));
            }
            assert!(
                !has_started_turn(&run_due_timers(&mut app, 1300)),
                "/goal {command} must prevent auto-continuation (quota={quota})"
            );
            assert!(!app.agent_running());
        }
    }
}

#[test]
fn goal_resume_controls_while_running_continue_only_after_turn_end() {
    for command in ["resume", "auto on"] {
        let mut app = isolated_app();
        app.type_text("/goal finish the current work");
        app.press(KeyCode::Enter);
        let turn_id = app.current_turn_id().expect("goal starts a turn");
        app.type_text("/goal pause");
        app.press(KeyCode::Enter);
        let _ = app.drain_engine_sends();

        app.type_text(&format!("/goal {command}"));
        app.press(KeyCode::Enter);
        assert!(app.state().queued_inputs.is_empty());
        assert!(!has_started_turn(&run_due_timers(&mut app, 1300)));
        assert_eq!(app.current_turn_id(), Some(turn_id));

        app.feed_one(SourceEvent::engine(EngineEvent::TurnComplete {
            turn_id,
            history: None,
            meta: None,
        }));
        assert!(
            has_started_turn(&run_due_timers(&mut app, 1300)),
            "{command}"
        );
    }
}

#[test]
fn goal_auto_setting_changes_update_a_paused_continuation() {
    let mut app = isolated_app();
    create_auto_goal(&mut app, "change goal policy");
    start_canonical_turn(&mut app);
    quota_error(&mut app, Some(0));
    app.type_text("/goal auto off");
    app.press(KeyCode::Enter);
    assert!(!has_started_turn(&run_due_timers(&mut app, 1300)));
    app.type_text("/goal auto on");
    app.press(KeyCode::Enter);
    assert!(has_started_turn(&run_due_timers(&mut app, 1300)));
}

#[test]
fn a_new_quota_reset_can_schedule_another_retry() {
    let mut app = isolated_app();
    create_auto_goal(&mut app, "wait for a new quota window");
    start_canonical_turn(&mut app);
    quota_error(&mut app, Some(0));
    assert!(has_started_turn(&run_due_timers(&mut app, 1300)));
    let now = engine::clock::unix_time_ms(app.clock.as_ref());
    quota_error(&mut app, Some(now + 5_000));
    assert!(!has_started_turn(&run_due_timers(&mut app, 5_999)));
    assert!(has_started_turn(&run_due_timers(&mut app, 1)));
}

#[test]
fn stale_continuation_tokens_cannot_resume_quota_pauses() {
    let mut app = isolated_app();
    start_canonical_turn(&mut app);
    quota_error(&mut app, Some(0));
    assert!(app.run_lua(
        r#"
        local state = smelt.engine.continuation_state()
        assert(not smelt.engine.resume_paused(state.token + 1))
        smelt.engine.cancel()
        assert(not smelt.engine.resume_paused(state.token))
    "#
    ));
    assert!(!app.agent_running());
}

#[test]
fn lua_reload_rebuilds_quota_schedule_without_reviving_cancelled_retries() {
    for cancelled in [false, true] {
        let mut app = isolated_app();
        create_auto_goal(&mut app, "reload while paused");
        start_canonical_turn(&mut app);
        let now = engine::clock::unix_time_ms(app.clock.as_ref());
        quota_error(&mut app, Some(now + 5_000));
        if cancelled {
            assert!(app.run_lua("smelt.engine.cancel()"));
        }
        let generation = app.lua_probe().id;
        app.reload_lua();
        assert_eq!(app.lua_probe().id, generation + 1);
        assert!(app.run_lua("assert(smelt.engine.continuation_state().paused)"));
        assert!(!has_started_turn(&run_due_timers(&mut app, 5_999)));
        assert_eq!(has_started_turn(&run_due_timers(&mut app, 1)), !cancelled);
    }
}

#[test]
fn turn_complete_still_chains_queued_turn() {
    let mut app = TestApp::builder().build();
    app.start_turn(1);
    app.push_queued_message("next turn after complete".to_string());

    app.feed_one(SourceEvent::engine(EngineEvent::TurnComplete {
        turn_id: 1,
        history: None,
        meta: None,
    }));

    assert!(
        app.agent_running(),
        "queued turn should start on clean completion"
    );
    assert!(app.queued_message_count() == 0 || app.state().queued_inputs.is_empty());
}
