use super::*;

#[test]
fn builds_a_fresh_test_app() {
    let app = TestApp::builder().build();
    let s = app.state();
    assert!(!s.cmdline_open);
    assert!(!s.quit_requested);
    assert!(!s.agent_running);
    assert_eq!(s.app_focus, AppFocus::Prompt);
    assert!(s.queued_inputs.is_empty());
}

#[test]
fn function_key_overlays_toggle_and_dismiss_without_mouse_focus() {
    let mut app = TestApp::builder().build();

    app.press(KeyCode::F(1));
    app.settle_lua();
    assert!(app.state().active_modal.is_some(), "F1 should open help");
    app.press(KeyCode::F(1));
    app.settle_lua();
    assert!(
        app.state().active_modal.is_none(),
        "pressing F1 again should close help"
    );

    app.press(KeyCode::F(1));
    app.settle_lua();
    app.type_char('q');
    app.settle_lua();
    assert!(
        app.state().active_modal.is_none(),
        "q should close help without a mouse click"
    );

    app.press(KeyCode::F(1));
    app.settle_lua();
    app.press(KeyCode::Esc);
    app.settle_lua();
    assert!(
        app.state().active_modal.is_none(),
        "Esc should close help without a mouse click"
    );

    assert!(app.run_lua(r#"require("smelt.examples.snake")"#));
    for key in [KeyCode::F(3), KeyCode::F(11), KeyCode::F(12)] {
        app.press(key);
        assert!(
            app.ui_probe().focused_overlay().is_some(),
            "{key:?} should focus its overlay"
        );
        app.press(key);
        assert!(
            app.ui_probe().focused_overlay().is_none(),
            "pressing {key:?} again should close its overlay"
        );

        app.press(key);
        app.type_char('q');
        assert!(
            app.ui_probe().focused_overlay().is_none(),
            "q should close the {key:?} overlay without a mouse click"
        );

        app.press(key);
        app.press(KeyCode::Esc);
        assert!(
            app.ui_probe().focused_overlay().is_none(),
            "Esc should close the {key:?} overlay without a mouse click"
        );
    }
}

#[test]
fn no_model_state_is_explicit_and_blocks_dispatch() {
    let mut app = TestApp::builder().with_vim(false).without_model().build();
    let initial_history_len = app.session_snapshot().history.len();

    assert!(app.run_lua(
        r#"
            assert(smelt.model.current() == nil)
            local status = smelt.model.status()
            assert(status.current == nil)
            assert(status.requested == nil)
            assert(status.availability == "none")
            assert(smelt.signal.get("model") == nil)
            assert(smelt.config.provider_type() == nil)
            assert(smelt.config.api_base() == nil)
            assert(smelt.config.api_key_env() == nil)
            assert(smelt.config.model_config() == nil)
            assert(smelt.model.capabilities() == nil)
        "#,
    ));

    app.press(KeyCode::F(3));
    assert!(app.render_to_frame().text().contains("debug"));
    app.press(KeyCode::Esc);

    app.type_text("hello without a model");
    app.press(KeyCode::Enter);

    assert_eq!(app.state().prompt_text, "hello without a model");
    assert!(!app.state().agent_running);
    assert_eq!(app.session_snapshot().history.len(), initial_history_len);
    assert!(app.notification_win().is_some());
    assert!(app.actions().iter().all(|action| {
        !matches!(
            action,
            Action::EngineSend(command)
                if matches!(command.as_ref(), protocol::UiCommand::StartTurn(_))
        )
    }));
}

#[test]
fn queued_input_is_retained_until_a_model_is_usable() {
    let mut app = TestApp::builder().without_model().build();
    app.push_queued_message("wait for a model".into());

    assert!(app.start_next_queued_input_if_idle());

    assert_eq!(app.state().queued_inputs, vec!["wait for a model"]);
    assert!(!app.state().agent_running);
    assert!(app.actions().iter().all(|action| {
        !matches!(
            action,
            Action::EngineSend(command)
                if matches!(command.as_ref(), protocol::UiCommand::StartTurn(_))
        )
    }));
}

#[test]
fn unavailable_model_is_reported_and_blocks_dispatch() {
    let mut app = TestApp::builder().build();
    let mut model = app
        .core_probe()
        .config
        .active_model()
        .expect("test app has an active model")
        .clone();
    model.availability = smelt_core::ModelAvailability::Unavailable {
        reason: smelt_core::ModelUnavailableReason::InvalidTransport,
    };
    app.replace_active_model_for_harness(model);

    assert!(app.run_lua(
        r#"
            assert(smelt.model.current() == "test/test-model")
            local status = smelt.model.status()
            assert(status.current == "test/test-model")
            assert(status.requested == "test/test-model")
            assert(status.availability == "unavailable")
            assert(status.reason == "invalid_transport")
        "#,
    ));

    app.type_text("do not dispatch");
    app.press(KeyCode::Enter);

    assert_eq!(app.state().prompt_text, "do not dispatch");
    assert!(!app.state().agent_running);
    assert!(app.actions().iter().all(|action| {
        !matches!(
            action,
            Action::EngineSend(command)
                if matches!(command.as_ref(), protocol::UiCommand::StartTurn(_))
        )
    }));
}

#[test]
fn model_switch_marks_missing_credentials_unavailable() {
    let mut app = TestApp::builder().build();
    let mut models = app.core_probe().config.available_models.clone();
    models.push(smelt_core::config::ResolvedModel {
        key: "missing/credentials".into(),
        provider_name: "missing".into(),
        model_name: "credentials".into(),
        display_name: None,
        api_base: "https://example.invalid/v1".into(),
        api_key_env: "SMELT_TEST_INTENTIONALLY_MISSING_MODEL_KEY_7F31A9".into(),
        provider_type: "openai-compatible".into(),
        config: protocol::ModelConfig::default(),
        catalog: protocol::ModelCatalogMetadata::default(),
    });
    app.set_available_models(models);

    app.apply_model("missing/credentials", true);

    assert_eq!(
        app.core_probe().recent.state_root(),
        app.core_probe().env.state_dir()
    );
    let recent = app.core_probe().recent.load();
    assert_eq!(
        recent.selected_model.as_deref(),
        Some("missing/credentials")
    );
    assert!(matches!(
        app.core_probe()
            .config
            .active_model()
            .map(|model| &model.availability),
        Some(smelt_core::ModelAvailability::Unavailable {
            reason: smelt_core::ModelUnavailableReason::MissingCredentials,
        })
    ));
    assert!(app.run_lua(
        r#"
        local status = smelt.model.status()
        assert(status.current == "missing/credentials")
        assert(status.availability == "unavailable")
        assert(status.reason == "missing_credentials")
        "#,
    ));
}

fn add_reasoning_capable_model(app: &mut TestApp) {
    let default = app.core_probe().config.available_models[0].clone();
    let mut capable = default.clone();
    capable.key = "openai/reasoning-model".into();
    capable.provider_name = "openai".into();
    capable.model_name = "reasoning-model".into();
    capable.provider_type = "openai".into();
    capable.catalog.supported_reasoning_efforts = vec![
        protocol::ReasoningEffort::Off,
        protocol::ReasoningEffort::Low,
        protocol::ReasoningEffort::Medium,
        protocol::ReasoningEffort::High,
        protocol::ReasoningEffort::XHigh,
        protocol::ReasoningEffort::Max,
        protocol::ReasoningEffort::Ultra,
        protocol::ReasoningEffort::Custom("persistent".into()),
    ];
    app.set_available_models(vec![default, capable]);
}

fn add_restricted_reasoning_model(app: &mut TestApp) {
    let current = app.core_probe().config.available_models[0].clone();
    let mut target = current.clone();
    target.key = "codex/restricted-model".into();
    target.provider_name = "codex".into();
    target.model_name = "restricted-model".into();
    target.provider_type = "codex".into();
    target.catalog.default_reasoning_effort = Some(protocol::ReasoningEffort::Low);
    target.catalog.supported_reasoning_efforts = vec![
        protocol::ReasoningEffort::Low,
        protocol::ReasoningEffort::High,
    ];
    app.set_available_models(vec![current, target]);
}

fn add_codex_gpt_5_6_sol_model(app: &mut TestApp) {
    let current = app.core_probe().config.available_models[0].clone();
    let mut target = current.clone();
    target.key = "codex/gpt-5.6-sol".into();
    target.provider_name = "codex".into();
    target.model_name = "gpt-5.6-sol".into();
    target.provider_type = "codex".into();
    target.catalog.default_reasoning_effort = Some(protocol::ReasoningEffort::Low);
    target.catalog.supported_reasoning_efforts = vec![
        protocol::ReasoningEffort::Low,
        protocol::ReasoningEffort::Medium,
        protocol::ReasoningEffort::High,
        protocol::ReasoningEffort::XHigh,
        protocol::ReasoningEffort::Max,
        protocol::ReasoningEffort::Ultra,
    ];
    app.set_available_models(vec![current, target]);
}

fn engine_ask_models_and_efforts(
    commands: Vec<protocol::UiCommand>,
) -> Vec<(String, protocol::ReasoningEffort)> {
    commands
        .into_iter()
        .filter_map(|command| match command {
            protocol::UiCommand::EngineAsk {
                target,
                reasoning_effort,
                ..
            } => Some((target.model, reasoning_effort)),
            _ => None,
        })
        .collect()
}

#[test]
fn command_argument_callbacks_are_lazy_and_live() {
    let mut app = TestApp::builder().build();
    assert!(app.run_lua(
        r#"
        local value = "first"
        local calls = 0
        local registration = smelt.cmd.register("live-args", function() end, {
            args = { "stale" },
            args_fn = function()
                calls = calls + 1
                assert(#smelt.cmd.list() > 0)
                smelt.cmd.register("from-args", function() end, { override = true })
                return { value }
            end,
        })
        local function command()
            for _, row in ipairs(smelt.cmd.list()) do
                if row.name == "live-args" then return row end
            end
        end
        local row = command()
        assert(row.args[1] == "stale")
        assert(calls == 0)
        assert(row.args_fn()[1] == "first")
        value = "second"
        assert(row.args_fn()[1] == "second")
        assert(calls == 2)
        registration:remove()
        assert(command() == nil)
    "#
    ));
}

#[test]
fn command_argument_callbacks_do_not_break_slash_completion() {
    let mut app = TestApp::builder().with_vim(false).build();
    assert!(app.run_lua(
        r#"
        hint_calls = 0
        smelt.cmd.register("broken-hint", function() end, {
            args_fn = function()
                hint_calls = hint_calls + 1
                error("broken argument provider")
            end,
        })
    "#
    ));

    app.type_text("/fast");
    let frame = app.render_to_frame().text();
    assert!(app.state().picker_count > 0, "{frame}");
    assert!(frame.contains("toggle accelerated inference"), "{frame}");
    app.press(KeyCode::Tab);
    let frame = app.render_to_frame().text();
    assert!(frame.contains("[on|off|toggle]"), "{frame}");
    assert!(app.run_lua("assert(hint_calls == 0)"));
}

#[test]
fn command_argument_callbacks_fall_back_to_static_hints() {
    for callback in [
        "error('broken argument provider')",
        "return nil",
        "return 'not a list'",
        "return { true }",
        "return { 'first', nil, 'third', 'fourth' }",
    ] {
        let mut app = TestApp::builder().with_vim(false).build();
        assert!(app.run_lua(&format!(
            r#"
            smelt.cmd.register("fallback-hint", function() end, {{
                args = {{ "first", "second" }},
                args_fn = function() {callback} end,
            }})
        "#
        )));

        app.type_text("/fallback-hint ");
        let frame = app.render_to_frame().text();
        assert!(frame.contains("[first|second]"), "{callback}: {frame}");
    }
}

#[test]
fn command_argument_callbacks_refresh_hints_and_allow_empty_choices() {
    let mut app = TestApp::builder().with_vim(false).build();
    assert!(app.run_lua(
        r#"
        hint_values = { "first" }
        smelt.cmd.register("live-hint", function() end, {
            args = { "fallback" },
            args_fn = function() return hint_values end,
        })
    "#
    ));

    app.type_text("/live-hint ");
    let frame = app.render_to_frame().text();
    assert!(frame.contains("[first]"), "{frame}");
    assert!(app.run_lua("hint_values = { 'second' }"));
    app.type_text(" ");
    let frame = app.render_to_frame().text();
    assert!(frame.contains("[second]"), "{frame}");
    assert!(app.run_lua("hint_values = {}"));
    app.type_text(" ");
    let frame = app.render_to_frame().text();
    assert!(!frame.contains("[second]"), "{frame}");
    assert!(!frame.contains("[fallback]"), "{frame}");
}

#[test]
fn budget_reasoning_preserves_max_when_budgets_match() {
    let mut app = TestApp::builder()
        .with_vim(false)
        .with_reasoning_cycle(vec![
            protocol::ReasoningEffort::Max,
            protocol::ReasoningEffort::Low,
        ])
        .build();
    let mut models = app.core_probe().config.available_models.clone();
    let mut claude = models[0].clone();
    claude.key = "anthropic/claude-sonnet-4-5".into();
    claude.model_name = "claude-sonnet-4-5".into();
    claude.provider_type = "anthropic".into();
    models.push(claude);
    app.set_available_models(models);

    app.type_text("/model anthropic/claude-sonnet-4-5");
    app.press(KeyCode::Enter);
    app.type_text("/reasoning max");
    app.press(KeyCode::Enter);
    let frame = app.render_to_frame().text();
    assert_eq!(
        app.core_probe().config.reasoning_effort,
        protocol::ReasoningEffort::Max,
        "{frame}"
    );

    app.press_mod(KeyCode::Char('t'), KeyModifiers::CONTROL);
    assert_eq!(
        app.core_probe().config.reasoning_effort,
        protocol::ReasoningEffort::Low
    );
    app.press_mod(KeyCode::Char('t'), KeyModifiers::CONTROL);
    assert_eq!(
        app.core_probe().config.reasoning_effort,
        protocol::ReasoningEffort::Max
    );
    assert!(!app.lua_messages_contain("not supported"));
}

#[test]
fn reasoning_hint_only_offers_active_model_levels() {
    let mut app = TestApp::builder().with_vim(false).build();
    add_restricted_reasoning_model(&mut app);
    app.type_text("/model codex/restricted-model");
    app.press(KeyCode::Enter);
    app.type_text("/reasoning ");

    let frame = app.render_to_frame().text();
    assert!(frame.contains("[low|high]"), "{frame}");
    assert!(!frame.contains("ultra"), "{frame}");
}

#[test]
fn reasoning_shortcut_skips_unsupported_configured_levels() {
    let mut app = TestApp::builder()
        .with_vim(false)
        .with_reasoning_cycle(vec![
            protocol::ReasoningEffort::Low,
            protocol::ReasoningEffort::Ultra,
            protocol::ReasoningEffort::High,
        ])
        .build();
    add_restricted_reasoning_model(&mut app);
    app.type_text("/model codex/restricted-model");
    app.press(KeyCode::Enter);

    app.press_mod(KeyCode::Char('t'), KeyModifiers::CONTROL);
    assert_eq!(
        app.core_probe().config.reasoning_effort,
        protocol::ReasoningEffort::High
    );
    app.press_mod(KeyCode::Char('t'), KeyModifiers::CONTROL);
    assert_eq!(
        app.core_probe().config.reasoning_effort,
        protocol::ReasoningEffort::Low
    );
    assert!(!app.lua_messages_contain("not supported"));
}

#[test]
fn reasoning_picker_selects_supported_levels_and_marks_the_default() {
    let mut app = TestApp::builder().with_vim(false).build();
    add_restricted_reasoning_model(&mut app);
    app.type_text("/model codex/restricted-model");
    app.press(KeyCode::Enter);
    app.type_text("/reasoning");
    app.press(KeyCode::Enter);

    assert!(app.state().picker_count > 0);
    let frame = app.render_to_frame().text();
    assert!(frame.contains("current, default"), "{frame}");
    assert!(!frame.contains("ultra"), "{frame}");
    app.press(KeyCode::Up);
    app.press(KeyCode::Enter);
    app.settle_lua();
    let frame = app.render_to_frame().text();
    assert_eq!(
        app.core_probe().config.reasoning_effort,
        protocol::ReasoningEffort::High,
        "{frame}"
    );
    assert_eq!(app.state().picker_count, 0);

    app.type_text("/reasoning");
    app.press(KeyCode::Enter);
    app.press(KeyCode::Enter);
    app.settle_lua();
    assert_eq!(
        app.core_probe().config.reasoning_effort,
        protocol::ReasoningEffort::High
    );

    app.type_text("/reasoning");
    app.press(KeyCode::Enter);
    app.press(KeyCode::Esc);
    assert_eq!(
        app.core_probe().config.reasoning_effort,
        protocol::ReasoningEffort::High
    );
}

#[test]
fn unknown_reasoning_levels_are_not_advertised_but_allow_explicit_overrides() {
    let mut app = TestApp::builder().with_vim(false).build();
    app.type_text("/reasoning");
    app.press(KeyCode::Enter);
    assert_eq!(app.state().picker_count, 0);
    assert!(app
        .render_to_frame()
        .text()
        .contains("reasoning levels unknown"));
    app.type_text("/reasoning persistent");
    app.press(KeyCode::Enter);
    app.press_mod(KeyCode::Char('t'), KeyModifiers::CONTROL);
    assert_eq!(
        app.core_probe().config.reasoning_effort,
        protocol::ReasoningEffort::Custom("persistent".into())
    );
    assert!(app.run_lua("assert(#smelt.reasoning.options().efforts == 0)"));
}

#[test]
fn reasoning_choices_follow_the_model_not_the_provider() {
    let mut app = TestApp::builder().with_vim(false).build();
    add_restricted_reasoning_model(&mut app);
    let mut models = app.core_probe().config.available_models.clone();
    let mut claude = models[0].clone();
    claude.key = "copilot/claude-opus-4.6".into();
    claude.model_name = "claude-opus-4.6".into();
    claude.provider_type = "copilot".into();
    models.push(claude);
    app.set_available_models(models);

    for (model, levels) in [
        ("codex/restricted-model", "low,high"),
        ("copilot/claude-opus-4.6", "off,low,medium,high,max"),
        ("test/test-model", ""),
    ] {
        app.type_text(&format!("/model {model}"));
        app.press(KeyCode::Enter);
        assert!(app.run_lua(&format!(
            "assert(table.concat(smelt.reasoning.options().efforts, ',') == {levels:?})"
        )));
        assert!(app.run_lua(&format!(
            r#"
            for _, command in ipairs(smelt.cmd.list()) do
                if command.name == "reasoning" then
                    assert(table.concat(command.args_fn(), ",") == {levels:?})
                end
            end
        "#
        )));
    }
}

#[test]
fn reasoning_disabled_model_rejects_explicit_effort_and_does_not_cycle() {
    let mut app = TestApp::builder().with_vim(false).build();
    let mut model = app.core_probe().config.available_models[0].clone();
    model.config.supports_reasoning = Some(false);
    app.set_available_models(vec![model]);
    app.apply_model("test/test-model", false);
    app.type_text("/reasoning high");
    app.press(KeyCode::Enter);
    app.press_mod(KeyCode::Char('t'), KeyModifiers::CONTROL);
    assert_eq!(
        app.core_probe().config.reasoning_effort,
        protocol::ReasoningEffort::Off
    );
    assert!(app.lua_messages_contain("not supported"));
}

#[test]
fn codex_model_accepts_all_advertised_reasoning_efforts() {
    let mut app = TestApp::builder().with_vim(false).build();
    add_codex_gpt_5_6_sol_model(&mut app);
    app.apply_model("codex/gpt-5.6-sol", false);
    let _ = app.drain_engine_sends();
    assert!(app.run_lua(
        r#"assert(table.concat(smelt.reasoning.cycle_list(), ",") ==
            "low,medium,high,xhigh,max,ultra")"#,
    ));

    for label in ["low", "medium", "high", "xhigh", "max", "ultra"] {
        assert!(app.run_lua(&format!("smelt.reasoning.set({label:?})")));
    }
}

#[test]
fn bundled_title_ask_uses_codex_default_when_off_is_unsupported() {
    let mut app = TestApp::builder().with_vim(false).build();
    add_codex_gpt_5_6_sol_model(&mut app);
    app.apply_model("codex/gpt-5.6-sol", false);
    assert!(app.run_lua(r#"smelt.reasoning.set("ultra")"#));
    let _ = app.drain_engine_sends();

    publish_input_submit(&mut app, "Submit with ultra reasoning");

    let requests = engine_ask_models_and_efforts(app.drain_engine_sends());
    assert_eq!(
        requests,
        vec![("gpt-5.6-sol".into(), protocol::ReasoningEffort::Low)]
    );
    assert!(!app.lua_messages_contain("reasoning effort"));
}

#[test]
fn bundled_prediction_ask_uses_codex_default_when_off_is_unsupported() {
    let mut app = TestApp::builder().with_vim(false).build();
    add_codex_gpt_5_6_sol_model(&mut app);
    app.apply_model("codex/gpt-5.6-sol", false);
    app.session_append_history(protocol::HistoryItem::user(protocol::Content::text(
        "How should I debug this failing test?",
    )));
    let _ = app.drain_engine_sends();

    publish_turn_end(&mut app);

    let requests = engine_ask_models_and_efforts(app.drain_engine_sends());
    assert_eq!(
        requests,
        vec![("gpt-5.6-sol".into(), protocol::ReasoningEffort::Low)]
    );
    assert!(!app.lua_messages_contain("reasoning effort"));
}

#[test]
fn model_switch_updates_reasoning_levels_for_active_provider() {
    let mut app = TestApp::builder().build();
    add_reasoning_capable_model(&mut app);

    assert!(app.run_lua(
        r#"
        local levels = smelt.reasoning.cycle_list()
        assert(#levels == 0)
        smelt.model.set("openai/reasoning-model")
        levels = smelt.reasoning.cycle_list()
        assert(#levels == 8)
        assert(levels[5] == "xhigh")
        assert(levels[6] == "max")
        assert(levels[7] == "ultra")
        assert(levels[8] == "persistent")
        smelt.model.set("test/test-model")
        levels = smelt.reasoning.cycle_list()
        assert(#levels == 0)
        "#,
    ));
}

#[test]
fn model_switch_uses_provider_default_when_current_reasoning_is_unsupported() {
    let mut app = TestApp::builder().build();
    add_restricted_reasoning_model(&mut app);

    assert!(app.run_lua(
        r#"
        smelt.reasoning.set("ultra")
        smelt.model.set("codex/restricted-model")
        assert(smelt.reasoning.current() == "low")
        assert(smelt.signal.get("reasoning") == "low")
        local levels = smelt.reasoning.cycle_list()
        assert(#levels == 2)
        assert(levels[1] == "low")
        assert(levels[2] == "high")
        "#,
    ));
    assert_eq!(
        app.conversation_probe().applied_reasoning_effort(),
        &protocol::ReasoningEffort::Low
    );
    assert_eq!(
        app.conversation_probe().session().reasoning_effort,
        Some(protocol::ReasoningEffort::Low)
    );
    assert!(app.drain_engine_sends().iter().any(|command| matches!(
        command,
        protocol::UiCommand::SetReasoningEffort {
            effort: protocol::ReasoningEffort::Low,
        }
    )));
}

#[test]
fn explicit_unsupported_reasoning_is_rejected_without_mutating_state() {
    let mut app = TestApp::builder().build();
    add_restricted_reasoning_model(&mut app);
    app.apply_model("codex/restricted-model", false);
    assert_eq!(
        app.core_probe().config.reasoning_effort,
        protocol::ReasoningEffort::Low
    );
    let _ = app.drain_engine_sends();

    let error = app
        .run_lua_result(r#"smelt.reasoning.set("ultra")"#)
        .expect_err("unsupported effort must fail");

    assert!(
        error.contains(
            "reasoning effort 'ultra' is not supported by model 'codex/restricted-model'"
        ),
        "unexpected error: {error}"
    );
    assert_eq!(
        app.core_probe().config.reasoning_effort,
        protocol::ReasoningEffort::Low
    );
    assert!(app.drain_engine_sends().iter().all(|command| !matches!(
        command,
        protocol::UiCommand::SetReasoningEffort {
            effort: protocol::ReasoningEffort::Ultra,
        }
    )));
}

#[test]
fn model_switch_preserves_explicit_reasoning_cycle() {
    let mut app = TestApp::builder()
        .with_reasoning_cycle(vec![
            protocol::ReasoningEffort::Off,
            protocol::ReasoningEffort::Max,
        ])
        .build();
    add_reasoning_capable_model(&mut app);

    assert!(app.run_lua(
        r#"
        smelt.model.set("openai/reasoning-model")
        local levels = smelt.reasoning.cycle_list()
        assert(#levels == 2)
        assert(levels[1] == "off")
        assert(levels[2] == "max")
        "#,
    ));
}

#[test]
fn recent_model_persistence_failure_is_reported() {
    let mut app = TestApp::builder().build();
    let mut model = app.core_probe().config.available_models[0].clone();
    model.key = "test/alternate-model".into();
    model.model_name = "alternate-model".into();
    let mut models = app.core_probe().config.available_models.clone();
    models.push(model);
    app.set_available_models(models);
    std::fs::create_dir_all(app.core_probe().recent.state_root().join("recent.lock")).unwrap();

    app.apply_model("test/alternate-model", true);

    assert_eq!(
        app.core_probe()
            .config
            .active_model()
            .map(|model| model.key.as_str()),
        Some("test/alternate-model")
    );
    assert!(app.lua_messages_contain("failed to remember model selection:"));
}

#[test]
fn context_window_discovery_does_not_repeat_missing_credentials_error() {
    const KEY_ENV: &str = "SMELT_TEST_CONTEXT_WINDOW_MISSING_KEY_6A41E2";
    let environment_guard = test_environment_guard();
    environment_guard.remove_var(KEY_ENV);
    let mut app = TestApp::builder().build_with_test_environment_guard(&environment_guard);

    app.refresh_context_window_twice_for_harness(KEY_ENV);

    assert!(!app.lua_messages_contain("required for API authentication"));
}

#[test]
fn restored_static_credentials_recover_without_model_reselection() {
    const KEY_ENV: &str = "SMELT_TEST_RESTORED_MODEL_KEY_9E76B1";
    let environment_guard = test_environment_guard();
    environment_guard.remove_var(KEY_ENV);
    let mut app = TestApp::builder().build_with_test_environment_guard(&environment_guard);
    let mut models = app.core_probe().config.available_models.clone();
    models.push(smelt_core::config::ResolvedModel {
        key: "restored/credentials".into(),
        provider_name: "restored".into(),
        model_name: "credentials".into(),
        display_name: None,
        api_base: "https://example.invalid/v1".into(),
        api_key_env: KEY_ENV.into(),
        provider_type: "openai-compatible".into(),
        config: protocol::ModelConfig::default(),
        catalog: protocol::ModelCatalogMetadata::default(),
    });
    app.set_available_models(models);
    app.apply_model("restored/credentials", true);
    assert!(matches!(
        app.core_probe()
            .config
            .active_model()
            .map(|model| &model.availability),
        Some(smelt_core::ModelAvailability::Unavailable {
            reason: smelt_core::ModelUnavailableReason::MissingCredentials,
        })
    ));

    environment_guard.set_var(KEY_ENV, "restored-secret");
    app.type_text("dispatch after credential restore");
    app.press(KeyCode::Enter);
    environment_guard.remove_var(KEY_ENV);

    assert!(app.state().agent_running);
    assert!(matches!(
        app.core_probe()
            .config
            .active_model()
            .map(|model| &model.availability),
        Some(smelt_core::ModelAvailability::Available)
    ));
    assert!(app.actions().iter().any(|action| matches!(
        action,
        Action::EngineSend(command)
            if matches!(command.as_ref(), protocol::UiCommand::StartTurn(_))
    )));
}

#[test]
fn api_base_endpoint_warning_is_persistent_and_deduped() {
    let mut app = TestApp::builder().build();
    let mut model = app
        .core_probe()
        .config
        .active_model()
        .expect("test app has an active model")
        .clone();
    model.provider_type = "openai-compatible".into();
    model.api_base = "https://api.cerebras.ai/v1/chat/completions".into();
    app.replace_active_model_for_harness(model);

    app.warn_if_api_base_normalized();
    app.warn_if_api_base_normalized();

    let messages = app.lua_probe().core_shared().messages.lock().unwrap();
    let entries: Vec<_> = messages
        .entries()
        .iter()
        .filter(|entry| entry.full.contains("api_base includes /chat/completions"))
        .collect();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].kind, smelt_core::messages::MessageKind::Warning);
    assert_eq!(entries[0].source, "config");
    assert!(entries[0]
        .full
        .contains("using https://api.cerebras.ai/v1 instead"));
}

#[test]
fn feed_one_records_alloc_delta_with_tick_near_zero() {
    let mut app = TestApp::builder().build();
    assert!(app.last_alloc_delta().is_none());
    app.feed_one(SourceEvent::Tick(10));
    let delta = app.last_alloc_delta().expect("delta after first event");
    assert!(
        delta.allocs < 32,
        "Tick should allocate near zero, got {} allocs / {} bytes",
        delta.allocs,
        delta.bytes_grown
    );
}

#[test]
fn keystroke_stays_within_default_alloc_budget() {
    let mut app = TestApp::builder().build();
    // Warm caches with a discarded first keystroke so this test
    // measures steady-state cost, not first-event init.
    app.type_char('a');
    app.feed_one_within_budget(
        SourceEvent::Term(Event::Key(KeyEvent {
            code: KeyCode::Char('b'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })),
        AllocBudget::DEFAULT,
    );
    let delta = app.last_alloc_delta().expect("delta recorded");
    // Print observed steady-state cost so it's visible in `cargo test
    // -- --nocapture` runs and during budget-tuning sweeps.
    eprintln!(
        "steady-state keystroke delta: {} allocs / {} bytes",
        delta.allocs, delta.bytes_grown
    );
}

#[test]
fn skill_backed_commands_submit_skill_body_and_focus() {
    let mut app = TestApp::builder().with_vim(false).build();
    let loader = std::sync::Arc::new(engine::SkillLoader::load_for_runtime(
        &[],
        app.core_probe().env.home(),
        app.core_probe().env.config_dir(),
        app.core_probe().env.data_dir(),
        &app.core_probe().env.cwd(),
    ));
    app.install_skill_loader_for_harness(loader);
    app.type_text("/reflect focus area");
    app.press(KeyCode::Enter);
    let payload = app
        .actions()
        .iter()
        .find_map(|action| match action {
            Action::EngineSend(cmd) => match cmd.as_ref() {
                protocol::UiCommand::StartTurn(payload) => Some((**payload).clone()),
                _ => None,
            },
            _ => None,
        })
        .expect("/reflect should start a turn");
    let content = payload.input.provider_content();
    let text = content.text_content();
    assert!(text.contains("# Reflect"));
    assert!(text.contains("## Additional Focus\n\nfocus area"));
    assert!(text.contains("<skill name=\"reflect\" included_by=\"smelt\""));
}

#[test]
fn custom_command_with_non_scalar_description_still_registers() {
    let dir = tempfile::tempdir().unwrap();
    let command_path = dir.path().join("security-audit.md");
    std::fs::write(
        &command_path,
        "---\ndescription:\n  audit the repository for malicious code\n  and data exfiltration\ntools:\n  allow: [read_file, glob, grep]\n---\n\nAudit the repository",
    )
    .unwrap();

    let mut app = TestApp::builder().with_vim(false).build();
    let dir_lua = format!("{:?}", dir.path().display().to_string());
    assert!(app.run_lua(&format!(
        "require('smelt.commands.custom_commands').register_dir({dir_lua})"
    )));
    let command_registered = app
        .lua_probe()
        .shared()
        .commands
        .lock()
        .unwrap()
        .contains_key("security-audit");
    assert!(command_registered, "security-audit command should register");
}

#[tokio::test(flavor = "current_thread")]
async fn custom_command_shell_output_is_marked_as_smelt_context() {
    let dir = tempfile::tempdir().unwrap();
    let command_path = dir.path().join("probe.md");
    std::fs::write(
        &command_path,
        "---\ndescription: probe\n---\n\nBefore\n\n```!\nprintf smelt-provenance\n```\n\nAfter",
    )
    .unwrap();

    let mut app = TestApp::builder().with_vim(false).build();
    let dir_lua = format!("{:?}", dir.path().display().to_string());
    assert!(app.run_lua(
        "smelt.process.run = function() return { stdout = 'smelt-provenance', stderr = '', exit_code = 0 } end"
    ));
    assert!(app.run_lua(&format!(
        "require('smelt.commands.custom_commands').register_dir({dir_lua})"
    )));
    app.type_text("/probe");
    app.press(KeyCode::Enter);
    for _ in 0..10 {
        tokio::task::yield_now().await;
        app.feed_one(SourceEvent::LuaWakeup);
        if app
            .actions()
            .iter()
            .any(|action| matches!(action, Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::StartTurn(_))))
        {
            break;
        }
    }

    let payload = app
        .actions()
        .iter()
        .find_map(|action| match action {
            Action::EngineSend(cmd) => match cmd.as_ref() {
                protocol::UiCommand::StartTurn(payload) => Some((**payload).clone()),
                _ => None,
            },
            _ => None,
        })
        .expect("/probe should start a turn");
    let content = payload.input.provider_content();
    let text = content.text_content();
    assert!(text.contains("<command_output "));
    assert!(text.contains("executed_by=\"smelt\""));
    assert!(text.contains("source=\"custom_command\""));
    assert!(text.contains("command=\"printf smelt-provenance\""));
    assert!(text.contains("smelt-provenance\n</command_output>"));
}

async fn read_file_result(
    path: &std::path::Path,
    provider_type: &str,
    modalities: &[&str],
    call_id: &str,
) -> (
    String,
    bool,
    Option<serde_json::Value>,
    Option<protocol::ToolAttachment>,
) {
    let mut app = TestApp::builder().with_vim(false).build();
    app.use_model(smelt_core::config::ResolvedModel {
        key: format!("{provider_type}/multimodal"),
        provider_name: provider_type.into(),
        model_name: "multimodal".into(),
        display_name: None,
        api_base: match provider_type {
            "codex" => "https://chatgpt.com/backend-api/codex".into(),
            _ => "https://api.anthropic.com/v1".into(),
        },
        api_key_env: String::new(),
        provider_type: provider_type.into(),
        config: protocol::ModelConfig {
            input_modalities: Some(modalities.iter().map(|value| (*value).into()).collect()),
            ..Default::default()
        },
        catalog: protocol::ModelCatalogMetadata::default(),
    });
    app.start_turn(1);
    app.feed_one(SourceEvent::engine(protocol::EngineEvent::ToolDispatch {
        invocation_id: protocol::InvocationId::new(81),
        request_id: 81,
        call_id: call_id.into(),
        tool_name: "read_file".into(),
        args: std::collections::HashMap::from([("file_path".into(), serde_json::json!(path))]),
    }));

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            tokio::task::yield_now().await;
            app.feed_one(SourceEvent::LuaWakeup);
            if let Some(result) = app.actions().iter().find_map(|action| match action {
                Action::EngineSend(command) => match command.as_ref() {
                    protocol::UiCommand::ToolResult {
                        call_id: completed,
                        content,
                        is_error,
                        metadata,
                        attachment,
                        ..
                    } if completed == call_id => Some((
                        content.clone(),
                        *is_error,
                        metadata.clone(),
                        attachment.clone(),
                    )),
                    _ => None,
                },
                _ => None,
            }) {
                return result;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("read_file should complete")
}

async fn read_file_attachment_result(
    path: &std::path::Path,
    provider_type: &str,
    modalities: &[&str],
    call_id: &str,
) -> (String, bool, serde_json::Value, protocol::ToolAttachment) {
    let (content, is_error, metadata, attachment) =
        read_file_result(path, provider_type, modalities, call_id).await;
    (
        content,
        is_error,
        metadata.expect("attachment metadata"),
        attachment.expect("typed attachment"),
    )
}

#[tokio::test(flavor = "current_thread")]
async fn codex_read_file_returns_image_attachment() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("console.png");
    std::fs::write(&path, b"\x89PNG\r\n\x1a\nimage-bytes").unwrap();

    let (content, is_error, metadata, attachment) =
        read_file_attachment_result(&path, "codex", &["text", "image"], "read-image").await;

    assert!(!is_error, "{content}");
    assert_eq!(content, format!("image file attached: {}", path.display()));
    assert_eq!(metadata["kind"], "file_attachment");
    assert_eq!(metadata["modality"], "image");
    assert_eq!(metadata["path"], path.to_string_lossy().as_ref());
    assert_eq!(metadata["mime"], "image/png");
    assert!(metadata.get("data_url").is_none());
    assert_eq!(attachment.modality, protocol::ToolAttachmentModality::Image);
    assert_eq!(attachment.mime, "image/png");
    assert!(attachment.data_url.starts_with("data:image/png;base64,"));
}

#[tokio::test(flavor = "current_thread")]
async fn codex_read_file_reads_svg_as_text() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("relay-gate.schematic.svg");
    std::fs::write(
        &path,
        r#"<svg xmlns="http://www.w3.org/2000/svg"><text>relay</text></svg>"#,
    )
    .unwrap();

    let (content, is_error, metadata, attachment) =
        read_file_result(&path, "codex", &["text", "image"], "read-svg").await;

    assert!(!is_error, "{content}");
    assert!(metadata.is_none(), "unexpected metadata: {metadata:?}");
    assert!(
        attachment.is_none(),
        "unexpected attachment: {attachment:?}"
    );
    assert!(
        content
            .contains("   1\t<svg xmlns=\"http://www.w3.org/2000/svg\"><text>relay</text></svg>"),
        "{content}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn anthropic_read_file_captures_pdf_attachment() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("document.pdf");
    std::fs::write(&path, b"%PDF-1.4").unwrap();

    let (content, is_error, metadata, attachment) =
        read_file_attachment_result(&path, "anthropic", &["text", "pdf"], "read-pdf").await;

    assert!(!is_error, "{content}");
    assert_eq!(metadata["modality"], "pdf");
    assert_eq!(metadata["mime"], "application/pdf");
    assert!(metadata.get("data_url").is_none());
    assert_eq!(attachment.modality, protocol::ToolAttachmentModality::Pdf);
    assert_eq!(attachment.mime, "application/pdf");
    assert!(attachment
        .data_url
        .starts_with("data:application/pdf;base64,"));
}

#[test]
fn explicit_model_switch_sends_complete_context_only_for_an_active_turn() {
    let mut app = TestApp::builder().with_vim(false).build();
    assert!(app.run_lua(r#"smelt.agent.add_system_prompt("model switch Lua prompt fragment")"#));
    let mut models = app.core_probe().config.available_models.clone();
    models.push(smelt_core::config::ResolvedModel {
        key: "other/switched".into(),
        provider_name: "other".into(),
        model_name: "switched".into(),
        display_name: None,
        api_base: "https://switch.example/v1".into(),
        api_key_env: String::new(),
        provider_type: "anthropic".into(),
        config: protocol::ModelConfig {
            context_window: Some(200_000),
            tool_calling: Some(false),
            ..Default::default()
        },
        catalog: protocol::ModelCatalogMetadata::default(),
    });
    app.set_available_models(models);
    let _ = app.drain_engine_sends();

    app.apply_model("other/switched", false);
    assert!(app
        .drain_engine_sends()
        .into_iter()
        .all(|cmd| !matches!(cmd, protocol::UiCommand::SetTurnModel { .. })));

    app.apply_model("test/test-model", false);
    let _ = app.drain_engine_sends();
    app.start_turn(1);
    app.apply_model("other/switched", false);
    let command = app
        .drain_engine_sends()
        .into_iter()
        .find(|cmd| matches!(cmd, protocol::UiCommand::SetTurnModel { .. }))
        .expect("active turn model switch should notify the engine");
    match command {
        protocol::UiCommand::SetTurnModel {
            target,
            system_prompt,
        } => {
            assert_eq!(target.model, "switched");
            assert_eq!(target.api_base, "https://switch.example/v1");
            assert_eq!(target.provider_type, "anthropic");
            assert_eq!(target.config.context_window, Some(200_000));
            assert_eq!(target.config.tool_calling, Some(false));
            assert!(system_prompt.contains("model switch Lua prompt fragment"));
        }
        _ => unreachable!(),
    }
}

#[test]
fn custom_command_override_dispatches_its_complete_target_and_request_config() {
    let mut app = TestApp::builder().with_vim(false).build();
    let mut active = app
        .core_probe()
        .config
        .active_model()
        .expect("test app has an active model")
        .clone();
    active.provider_type = "openai".into();
    active.config = protocol::ModelConfig {
        input_cost: Some(99.0),
        ..Default::default()
    };
    app.replace_active_model_for_harness(active);
    let mut settings = app.core_probe().config.settings.clone();
    settings.redact_secrets = true;
    settings.cache_ttl_long = true;
    app.set_settings_for_harness(settings);
    app.set_available_models(vec![smelt_core::config::ResolvedModel {
        key: "other/target-model".into(),
        provider_name: "other".into(),
        model_name: "target-model".into(),
        display_name: None,
        api_base: "https://other.example/v1".into(),
        api_key_env: String::new(),
        provider_type: "anthropic-compatible".into(),
        config: protocol::ModelConfig {
            temperature: Some(0.1),
            output_cost: Some(3.0),
            tool_calling: Some(false),
            ..Default::default()
        },
        catalog: protocol::ModelCatalogMetadata {
            default_reasoning_effort: Some(protocol::ReasoningEffort::Low),
            supported_reasoning_efforts: vec![
                protocol::ReasoningEffort::Low,
                protocol::ReasoningEffort::High,
            ],
            ..Default::default()
        },
    }]);
    let _ = app.drain_engine_sends();

    assert!(
        app.start_command_request_turn(
            "custom".into(),
            "body".into(),
            smelt_core::custom_commands::CommandOverrides {
                model: Some("other/target-model".into()),
                temperature: Some(0.8),
                ..Default::default()
            },
            crate::app::CommandTurnStart::Fresh,
        ),
        "custom command target resolves"
    );

    let payload = app
        .drain_engine_sends()
        .into_iter()
        .find_map(|cmd| match cmd {
            protocol::UiCommand::StartTurn(payload) => Some(*payload),
            _ => None,
        })
        .expect("custom command should send StartTurn");
    assert_eq!(payload.model_target.model, "target-model");
    assert_eq!(payload.model_target.api_base, "https://other.example/v1");
    assert_eq!(payload.model_target.provider_type, "anthropic-compatible");
    assert_eq!(payload.model_target.config.temperature, Some(0.8));
    assert_eq!(payload.model_target.config.output_cost, Some(3.0));
    assert_eq!(payload.model_target.config.input_cost, None);
    assert_eq!(payload.model_target.config.tool_calling, Some(false));
    assert_eq!(payload.reasoning_effort, protocol::ReasoningEffort::Low);
    assert!(payload.request_config.redact_secrets);
    assert!(payload.request_config.cache_ttl_long);
}

#[test]
fn custom_command_rejects_unsupported_reasoning_before_staging_turn() {
    let mut app = TestApp::builder().with_vim(false).build();
    add_restricted_reasoning_model(&mut app);
    let history_len = app.conversation_probe().session().history.len();
    let transcript_len = app.conversation_probe().transcript().history().order.len();
    let _ = app.drain_engine_sends();

    assert!(!app.start_command_request_turn(
        "custom".into(),
        "body".into(),
        smelt_core::custom_commands::CommandOverrides {
            model: Some("codex/restricted-model".into()),
            reasoning_effort: Some("ultra".into()),
            ..Default::default()
        },
        crate::app::CommandTurnStart::Fresh,
    ));

    assert_eq!(
        app.conversation_probe().session().history.len(),
        history_len
    );
    assert_eq!(
        app.conversation_probe().transcript().history().order.len(),
        transcript_len
    );
    assert!(!app.state().agent_running);
    assert!(app
        .drain_engine_sends()
        .iter()
        .all(|command| !matches!(command, protocol::UiCommand::StartTurn(_))));
}

#[test]
fn custom_command_turn_includes_registered_lua_tools() {
    let mut app = TestApp::builder().with_vim(false).build();
    let payload = app
        .start_custom_command_with_lua_tool(0)
        .expect("custom command should send StartTurn");
    assert!(
        payload.tools.iter().any(|t| t.name == "fuzz_custom_tool_0"),
        "registered Lua tool missing from custom command payload: {:?}",
        payload.tools.iter().map(|t| &t.name).collect::<Vec<_>>()
    );

    let payload = app
        .start_custom_command_with_lua_tool(1)
        .expect("repeated custom command should replace the synthetic turn");
    assert!(
        payload.tools.iter().any(|t| t.name == "fuzz_custom_tool_1"),
        "replacement Lua tool missing from custom command payload: {:?}",
        payload.tools.iter().map(|t| &t.name).collect::<Vec<_>>()
    );
}

#[test]
fn custom_command_starts_after_replaced_synthetic_history() {
    fn synth_history(count: usize) -> Vec<protocol::HistoryItem> {
        (0..count)
            .map(|i| {
                let body = format!("compacted-{i}");
                let reasoning_blocks = if i % 4 == 0 {
                    Vec::new()
                } else {
                    vec![protocol::ReasoningBlock {
                        provider: "fuzz".to_string(),
                        data: serde_json::Value::Null,
                    }]
                };
                match i % 3 {
                    0 => protocol::HistoryItem::user(protocol::Content::text(body)),
                    1 => protocol::HistoryItem::Assistant(protocol::AssistantStep::terminal(
                        Some(protocol::Content::text(body)),
                        None,
                        reasoning_blocks,
                    )),
                    _ => {
                        let invocation = protocol::ToolInvocation {
                            call_id: format!("synth-call-{i:02}"),
                            name: "synth".to_string(),
                            arguments: "{}".to_string(),
                            result: protocol::ToolOutcome::new(
                                format!("synth-result-{i}"),
                                false,
                                None,
                            ),
                            elapsed_ms: None,
                            called_at_ms: Some(i as u64),
                        };
                        protocol::HistoryItem::Assistant(protocol::AssistantStep::with_invocations(
                            Some(protocol::Content::text(body)),
                            None,
                            reasoning_blocks,
                            vec![invocation],
                        ))
                    }
                }
            })
            .collect()
    }

    fn engine_turn_complete(app: &mut TestApp, msg_count: usize) {
        let turn_id = app.current_turn_id().unwrap_or(0);
        app.feed_one(SourceEvent::engine(EngineEvent::TurnComplete {
            turn_id,
            history: Some(protocol::CanonicalHistoryDelta::new(
                0,
                synth_history(msg_count),
            )),
            meta: None,
        }));
    }

    let mut app = TestApp::builder()
        .with_vim(true)
        .with_mode(AgentMode::normal())
        .build();
    app.start_exec("");
    app.render_silent();
    engine_turn_complete(&mut app, 127);
    app.render_silent();
    app.start_custom_command_with_lua_tool(59)
        .expect("first custom command should send StartTurn");
    app.render_silent();
    engine_turn_complete(&mut app, 47);
    app.render_silent();
    app.start_custom_command_with_lua_tool(0)
        .expect("second custom command should send StartTurn");
    app.render_silent();
    app.start_custom_command_with_lua_tool(127)
        .expect("third custom command should send StartTurn");
    app.render_silent();
    engine_turn_complete(&mut app, 127);
    app.render_silent();

    let payload = app
        .start_custom_command_with_lua_tool(127)
        .expect("custom command should send StartTurn after replaced synthetic history");
    assert!(payload.tools.iter().any(|t| t.name == "fuzz_custom_tool_3"));
}

#[test]
fn engine_asks_validate_and_reconcile_reasoning_for_selected_model() {
    let mut app = TestApp::builder().with_vim(false).build();
    add_restricted_reasoning_model(&mut app);
    let _ = app.drain_engine_sends();

    for call in [
        r#"smelt.engine.ask({
            system = "test",
            model = "codex/restricted-model",
            reasoning_effort = "ultra",
        })"#,
        r#"smelt.engine.ask_inherited({
            model = "codex/restricted-model",
            reasoning_effort = "ultra",
        })"#,
    ] {
        let error = app
            .run_lua_result(call)
            .expect_err("unsupported ask reasoning must fail");
        assert!(
            error.contains("reasoning effort 'ultra' is not supported by model 'restricted-model'"),
            "unexpected error: {error}"
        );
    }
    assert!(app
        .drain_engine_sends()
        .iter()
        .all(|command| !matches!(command, protocol::UiCommand::EngineAsk { .. })));

    assert!(app.run_lua(
        r#"
        smelt.engine.ask({
            system = "test",
            model = "codex/restricted-model",
        })
        smelt.engine.ask_inherited({
            model = "codex/restricted-model",
        })
        "#,
    ));
    let requests = engine_ask_models_and_efforts(app.drain_engine_sends());
    assert_eq!(
        requests,
        vec![
            ("restricted-model".into(), protocol::ReasoningEffort::Low),
            ("restricted-model".into(), protocol::ReasoningEffort::Low),
        ]
    );
}

#[test]
fn engine_asks_inherit_reasoning_only_for_inherited_requests() {
    let mut app = TestApp::builder().with_vim(false).build();
    let mut primary = app.core_probe().config.available_models[0].clone();
    primary.catalog.default_reasoning_effort = Some(protocol::ReasoningEffort::Medium);
    primary.catalog.supported_reasoning_efforts = vec![
        protocol::ReasoningEffort::Medium,
        protocol::ReasoningEffort::High,
        protocol::ReasoningEffort::Max,
    ];
    let primary_model = primary.model_name.clone();
    let primary_key = primary.key.clone();
    app.use_model(primary);
    add_restricted_reasoning_model(&mut app);
    assert!(app.run_lua(r#"smelt.reasoning.set("max")"#));
    let _ = app.drain_engine_sends();

    assert!(app.run_lua(&format!(
        r#"
        smelt.engine.ask_inherited({{}})
        smelt.engine.ask_inherited({{ model = {primary_key:?} }})
        smelt.engine.ask_inherited({{ reasoning_effort = "medium" }})
        smelt.engine.ask_inherited({{ model = "codex/restricted-model" }})
        smelt.engine.ask_inherited({{
            model = "codex/restricted-model",
            reasoning_effort = "high",
        }})
        smelt.engine.ask({{ system = "standalone" }})
        "#,
    )));
    assert_eq!(
        engine_ask_models_and_efforts(app.drain_engine_sends()),
        vec![
            (primary_model.clone(), protocol::ReasoningEffort::Max),
            (primary_model.clone(), protocol::ReasoningEffort::Max),
            (primary_model.clone(), protocol::ReasoningEffort::Medium),
            ("restricted-model".into(), protocol::ReasoningEffort::Low),
            ("restricted-model".into(), protocol::ReasoningEffort::High),
            (primary_model, protocol::ReasoningEffort::Medium),
        ]
    );

    assert!(app.run_lua(
        r#"
        smelt.reasoning.set("high")
        smelt.engine.ask_inherited({ model = "codex/restricted-model" })
        "#,
    ));
    assert_eq!(
        engine_ask_models_and_efforts(app.drain_engine_sends()),
        vec![("restricted-model".into(), protocol::ReasoningEffort::High)]
    );
}

#[test]
fn engine_ask_probe_dispatches_complete_target_and_request_config() {
    let mut app = TestApp::builder().with_vim(false).build();
    let mut active = app
        .core_probe()
        .config
        .active_model()
        .expect("test app has an active model")
        .clone();
    active.model_name = "ask-model".into();
    active.api_base = "https://ask.example/v1".into();
    active.provider_type = "openai-compatible".into();
    active.config = protocol::ModelConfig {
        max_tokens: Some(1234),
        supports_reasoning: Some(true),
        ..Default::default()
    };
    app.replace_active_model_for_harness(active);
    let mut settings = app.core_probe().config.settings.clone();
    settings.cache_ttl_long = true;
    settings.request_audit = "off".into();
    app.set_settings_for_harness(settings);
    app.set_request_audit_for_harness(protocol::RequestAuditMode::Full);
    let _ = app.drain_engine_sends();

    app.start_engine_ask_probe("summarize this");
    assert!(app.pending_ask_id().is_some());
    let command = app
        .actions()
        .iter()
        .rev()
        .find_map(|action| match action {
            Action::EngineSend(command)
                if matches!(command.as_ref(), protocol::UiCommand::EngineAsk { .. }) =>
            {
                Some(command.as_ref().clone())
            }
            _ => None,
        })
        .expect("probe should send EngineAsk");
    match command {
        protocol::UiCommand::EngineAsk {
            target,
            request_config,
            ..
        } => {
            assert_eq!(target.model, "ask-model");
            assert_eq!(target.api_base, "https://ask.example/v1");
            assert_eq!(target.provider_type, "openai-compatible");
            assert_eq!(target.config.max_tokens, Some(1234));
            assert_eq!(target.config.supports_reasoning, Some(true));
            assert!(request_config.cache_ttl_long);
            assert_eq!(
                request_config.request_audit,
                protocol::RequestAuditMode::Full
            );
        }
        _ => unreachable!(),
    }
}

#[test]
fn request_settings_changes_only_affect_requests_created_after_reconciliation() {
    let mut app = TestApp::builder().with_vim(false).build();
    let revision_before = app.core_probe().config.revision;
    app.start_engine_ask_probe("before settings change");
    let before = app
        .actions()
        .iter()
        .rev()
        .find_map(|action| match action {
            Action::EngineSend(command) => match command.as_ref() {
                protocol::UiCommand::EngineAsk { request_config, .. } => Some(*request_config),
                _ => None,
            },
            _ => None,
        })
        .expect("first probe should send EngineAsk");

    assert!(app.run_lua(
        r#"
            smelt.settings.redact_secrets = true
            smelt.settings.cache_ttl_long = true
            smelt.settings.request_audit = "full"
        "#,
    ));
    assert_eq!(
        app.core_probe().config.revision,
        revision_before.wrapping_add(1),
        "one callback with multiple writes must commit one runtime revision"
    );
    app.start_engine_ask_probe("after settings change");
    let after = app
        .actions()
        .iter()
        .rev()
        .find_map(|action| match action {
            Action::EngineSend(command) => match command.as_ref() {
                protocol::UiCommand::EngineAsk { request_config, .. } => Some(*request_config),
                _ => None,
            },
            _ => None,
        })
        .expect("second probe should send EngineAsk");

    assert!(!before.redact_secrets);
    assert!(!before.cache_ttl_long);
    assert_eq!(before.request_audit, protocol::RequestAuditMode::Summary);
    assert!(after.redact_secrets);
    assert!(after.cache_ttl_long);
    assert_eq!(after.request_audit, protocol::RequestAuditMode::Full);
}

#[tokio::test]
async fn installing_the_runtime_http_client_starts_managed_refreshes() {
    let environment_guard = test_environment_guard();
    environment_guard.set_var(
        "SMELT_CODEX_TOKENS",
        r#"{"access_token":"test-access","refresh_token":"test-refresh","expires_at":9999999999,"account_id":"test-account","last_refresh":0}"#,
    );
    let mut app = TestApp::builder().build_with_test_environment_guard(&environment_guard);
    assert!(app.run_lua(
        r#"
        smelt.provider.register("codex", {
            type = "codex",
            api_base = "https://example.invalid",
            models = {},
        })
        "#,
    ));
    app.reconcile_committed_lua_runtime().unwrap();
    app.handle_managed_auth_checked(vec![(
        engine::auth::AuthProvider::Codex,
        engine::auth::credential_fingerprint(engine::auth::AuthProvider::Codex),
        Vec::new(),
    )]);

    app.install_http_client(engine::HttpClient::new());

    assert_eq!(
        app.managed_model_status(engine::auth::AuthProvider::Codex),
        smelt_core::ManagedModelsStatus::Refreshing
    );
}

#[tokio::test]
async fn managed_model_refresh_notifications_follow_error_and_revision_lifecycle() {
    let environment_guard = test_environment_guard();
    environment_guard.set_var(
        "SMELT_CODEX_TOKENS",
        r#"{"access_token":"test-access","refresh_token":"test-refresh","expires_at":9999999999,"account_id":"test-account","last_refresh":0}"#,
    );
    let mut app = TestApp::builder().build_with_test_environment_guard(&environment_guard);
    assert!(app.run_lua(
        r#"
        smelt.provider.register("codex", {
            type = "codex",
            api_base = "https://example.invalid",
            models = {},
        })
        "#,
    ));
    app.reconcile_committed_lua_runtime().unwrap();
    app.handle_managed_auth_checked(vec![(
        engine::auth::AuthProvider::Codex,
        engine::auth::credential_fingerprint(engine::auth::AuthProvider::Codex),
        Vec::new(),
    )]);
    let provider = engine::auth::AuthProvider::Codex;
    let first = app
        .begin_managed_model_refreshes()
        .pop()
        .expect("first refresh token");

    app.handle_managed_model_refresh(
        first,
        engine::auth::ManagedModelsRefreshOutcome::Failed(
            engine::auth::ManagedModelsRefreshFailure::Retryable("same failure".into()),
        ),
    );
    assert!(app.activate_managed_model_retry_for_harness(first));
    let second = app
        .begin_managed_model_refreshes()
        .pop()
        .expect("retry refresh token");
    app.handle_managed_model_refresh(
        second,
        engine::auth::ManagedModelsRefreshOutcome::Failed(
            engine::auth::ManagedModelsRefreshFailure::Retryable("same failure".into()),
        ),
    );

    assert!(app.activate_managed_model_retry_for_harness(second));
    let changed_error = app
        .begin_managed_model_refreshes()
        .pop()
        .expect("changed-error refresh token");
    app.handle_managed_model_refresh(
        changed_error,
        engine::auth::ManagedModelsRefreshOutcome::Failed(
            engine::auth::ManagedModelsRefreshFailure::Retryable("different failure".into()),
        ),
    );

    assert!(app.activate_managed_model_retry_for_harness(changed_error));
    let success = app
        .begin_managed_model_refreshes()
        .pop()
        .expect("successful refresh token");
    app.handle_managed_model_refresh(
        success,
        engine::auth::ManagedModelsRefreshOutcome::Fresh {
            models: Vec::new(),
            cache_warning: None,
        },
    );
    app.handle_managed_auth_checked(vec![(
        provider,
        engine::auth::credential_fingerprint(provider),
        vec![protocol::ModelMetadata {
            id: "cached-after-success".into(),
            display_name: None,
            context_window: None,
            max_output_tokens: None,
            supports_reasoning: None,
            supports_fast_mode: None,
            input_modalities: None,
            catalog: protocol::ModelCatalogMetadata::default(),
        }],
    )]);
    let after_success = app
        .begin_managed_model_refreshes()
        .pop()
        .expect("post-success refresh token");
    app.handle_managed_model_refresh(
        after_success,
        engine::auth::ManagedModelsRefreshOutcome::Failed(
            engine::auth::ManagedModelsRefreshFailure::Retryable("same failure".into()),
        ),
    );

    let enabled_config = smelt_core::config::Config {
        providers: vec![smelt_core::config::ProviderConfig {
            name: Some("codex".into()),
            provider_type: Some("codex".into()),
            ..Default::default()
        }],
        ..Default::default()
    };
    assert!(app.sync_managed_models_for_harness(
        &smelt_core::config::Config::default(),
        after_success.desired_revision + 1,
    ));
    assert!(
        app.sync_managed_models_for_harness(&enabled_config, after_success.desired_revision + 2,)
    );
    let after_reenable = app
        .begin_managed_model_refreshes()
        .pop()
        .expect("re-enabled refresh token");
    app.handle_managed_model_refresh(
        after_reenable,
        engine::auth::ManagedModelsRefreshOutcome::Failed(
            engine::auth::ManagedModelsRefreshFailure::Retryable("same failure".into()),
        ),
    );

    assert_eq!(
        app.lua_message_count("Codex model refresh: same failure"),
        3
    );
    assert_eq!(
        app.lua_message_count("Codex model refresh: different failure"),
        1
    );
}

#[test]
fn managed_model_refresh_event_updates_the_running_catalog() {
    let environment_guard = test_environment_guard();
    environment_guard.set_var(
        "SMELT_CODEX_TOKENS",
        r#"{"access_token":"test-access","refresh_token":"test-refresh","expires_at":9999999999,"account_id":"test-account","last_refresh":0}"#,
    );
    let mut app = TestApp::builder().build_with_test_environment_guard(&environment_guard);
    assert!(app.run_lua(
        r#"
        smelt.provider.register("codex", {
            type = "codex",
            api_base = "https://example.invalid",
            models = {},
        })
        "#,
    ));
    app.reconcile_committed_lua_runtime().unwrap();
    app.handle_managed_auth_checked(vec![(
        engine::auth::AuthProvider::Codex,
        engine::auth::credential_fingerprint(engine::auth::AuthProvider::Codex),
        Vec::new(),
    )]);
    app.set_model_selection_for_harness(smelt_core::ModelSelectionState {
        requested_key: Some("codex/fresh-model".into()),
        requested_by: smelt_core::ModelSelectionSource::Session,
        active: None,
    });
    let token = app
        .begin_managed_model_refreshes()
        .pop()
        .expect("codex refresh token");

    app.handle_app_event(crate::app::AppEvent::ManagedModelsRefreshCompleted {
        token,
        outcome: engine::auth::ManagedModelsRefreshOutcome::Fresh {
            models: vec![protocol::ModelMetadata {
                id: "fresh-model".into(),
                display_name: Some("Fresh Model".into()),
                context_window: Some(128_000),
                max_output_tokens: Some(8_192),
                supports_reasoning: Some(true),
                supports_fast_mode: Some(true),
                input_modalities: Some(vec!["text".into()]),
                catalog: protocol::ModelCatalogMetadata::default(),
            }],
            cache_warning: None,
        },
    });

    let model = app
        .core_probe()
        .config
        .available_models
        .iter()
        .find(|model| model.key == "codex/fresh-model")
        .expect("refresh should update the live picker catalog");
    assert_eq!(model.display_name.as_deref(), Some("Fresh Model"));
    assert_eq!(model.config.max_tokens, Some(8_192));
    let active = app
        .core_probe()
        .config
        .active_model()
        .expect("pending selection");
    assert_eq!(active.key, "codex/fresh-model");
    assert_eq!(active.config.context_window, Some(128_000));
    assert_eq!(active.config.supports_reasoning, Some(true));
    assert!(app.run_lua(
        r#"
        local status = smelt.model.status()
        assert(status.providers.codex.authenticated == true)
        assert(status.providers.codex.status == "fresh")
        assert(smelt.config.model_config().supports_fast_mode == true)
        "#,
    ));

    app.handle_managed_auth_checked(vec![(engine::auth::AuthProvider::Codex, None, Vec::new())]);
    assert!(app
        .core_probe()
        .config
        .available_models
        .iter()
        .all(|model| model.key != "codex/fresh-model"));
    assert!(app.run_lua(
        r#"
        local status = smelt.model.status()
        assert(status.providers.codex.authenticated == false)
        assert(status.providers.codex.status == "unauthenticated")
        assert(status.availability == "unavailable")
        assert(status.reason == "missing_credentials")
        "#,
    ));

    app.handle_managed_auth_checked(vec![(
        engine::auth::AuthProvider::Codex,
        engine::auth::credential_fingerprint(engine::auth::AuthProvider::Codex),
        vec![protocol::ModelMetadata {
            id: "fresh-model".into(),
            display_name: Some("Fresh Model".into()),
            context_window: Some(128_000),
            max_output_tokens: Some(8_192),
            supports_reasoning: Some(true),
            supports_fast_mode: Some(true),
            input_modalities: Some(vec!["text".into()]),
            catalog: protocol::ModelCatalogMetadata::default(),
        }],
    )]);
    assert_eq!(
        app.core_probe()
            .config
            .active_model()
            .map(|model| model.key.as_str()),
        Some("codex/fresh-model")
    );
}

#[test]
fn expired_kimi_auth_preserves_selected_codex_model() {
    let mut app = TestApp::builder().build();
    assert!(app.run_lua(
        r#"
        smelt.provider.register("codex", {
            type = "codex",
            api_base = "https://example.invalid",
            models = {},
        })
        "#,
    ));
    app.reconcile_committed_lua_runtime().unwrap();
    app.handle_managed_auth_checked(vec![
        (
            engine::auth::AuthProvider::Codex,
            Some(11),
            vec![protocol::ModelMetadata {
                id: "gpt-5.6-sol".into(),
                display_name: Some("GPT-5.6-Sol".into()),
                context_window: Some(272_000),
                max_output_tokens: None,
                supports_reasoning: Some(true),
                supports_fast_mode: Some(true),
                input_modalities: Some(vec!["text".into()]),
                catalog: protocol::ModelCatalogMetadata::default(),
            }],
        ),
        (
            engine::auth::AuthProvider::KimiCode,
            Some(22),
            vec![protocol::ModelMetadata {
                id: "kimi-for-coding".into(),
                display_name: Some("Kimi for Coding".into()),
                context_window: Some(262_144),
                max_output_tokens: None,
                supports_reasoning: Some(true),
                supports_fast_mode: None,
                input_modalities: Some(vec!["text".into()]),
                catalog: protocol::ModelCatalogMetadata::default(),
            }],
        ),
    ]);
    app.apply_model("codex/gpt-5.6-sol", true);

    let kimi_refresh = app
        .begin_managed_model_refreshes()
        .into_iter()
        .find(|token| token.provider == engine::auth::AuthProvider::KimiCode)
        .expect("Kimi refresh token");
    app.handle_managed_model_refresh(
        kimi_refresh,
        engine::auth::ManagedModelsRefreshOutcome::Unauthenticated(
            "Kimi refresh token expired".into(),
        ),
    );

    assert_eq!(
        app.managed_model_status(engine::auth::AuthProvider::KimiCode),
        smelt_core::ManagedModelsStatus::Unauthenticated
    );
    assert!(app
        .core_probe()
        .config
        .available_models
        .iter()
        .all(|model| !model.key.starts_with("kimi-code/")));
    assert_eq!(
        app.core_probe()
            .config
            .active_model()
            .map(|model| model.key.as_str()),
        Some("codex/gpt-5.6-sol")
    );
    assert_eq!(
        app.core_probe()
            .config
            .model_selection
            .requested_key
            .as_deref(),
        Some("codex/gpt-5.6-sol")
    );
    let recent = app.core_probe().recent.load();
    assert_eq!(recent.selected_model.as_deref(), Some("codex/gpt-5.6-sol"));
}

#[test]
fn ctrl_w_pane_chord_expires_after_tick_past_window() {
    let mut app = TestApp::builder().build();
    app.press_mod(KeyCode::Char('w'), KeyModifiers::CONTROL);
    assert!(
        app.timers_probe().pending_pane_chord.is_some(),
        "Ctrl-W arms the pane chord"
    );

    // 1000ms > PANE_CHORD_WINDOW (750ms).
    app.feed_one(SourceEvent::Tick(1000));
    // Follow-up key after expiry: handler drops the pending chord and
    // returns None so the key falls through to normal dispatch.
    app.press(KeyCode::Char('j'));
    assert!(
        app.timers_probe().pending_pane_chord.is_none(),
        "expired pane chord should be cleared on the next key"
    );
}
