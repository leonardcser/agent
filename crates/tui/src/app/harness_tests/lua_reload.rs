use super::*;

fn prompt_has_command_highlight(app: &mut TestApp) -> bool {
    app.render_silent();
    !app.ui_probe()
        .buf(crate::app::PROMPT_EDIT_BUF)
        .expect("prompt buffer")
        .highlights_at(0)
        .is_empty()
}

fn git(cwd: &std::path::Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .current_dir(cwd)
        .args(args)
        .env("GIT_AUTHOR_NAME", "smelt test")
        .env("GIT_AUTHOR_EMAIL", "smelt-test@example.invalid")
        .env("GIT_COMMITTER_NAME", "smelt test")
        .env("GIT_COMMITTER_EMAIL", "smelt-test@example.invalid")
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn worktree_repo() -> tempfile::TempDir {
    let repo = tempfile::TempDir::new().expect("create worktree repository");
    git(repo.path(), &["init", "-b", "main"]);
    std::fs::write(repo.path().join("README.md"), "fixture\n").unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-m", "initial"]);
    repo
}

#[test]
fn launch_and_reload_evaluate_each_config_phase_once_per_generation() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let config_dir = root.path().join("config");
    let runtime_dir = root.path().join("runtime");
    let runtime_smelt = runtime_dir.join("smelt");
    let project_dir = root.path().join("project");
    let project_config_dir = project_dir.join(".smelt");

    std::fs::create_dir_all(config_dir.join("plugins")).unwrap();
    std::fs::create_dir_all(runtime_smelt.join("commands")).unwrap();
    std::fs::create_dir_all(&project_config_dir).unwrap();

    let color_command = format!(
        "_G.__autoload_count = (_G.__autoload_count or 0) + 1\n{}",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../runtime/lua/smelt/commands/color.lua"
        ))
    );
    std::fs::write(runtime_smelt.join("commands/color.lua"), color_command).unwrap();
    std::fs::write(
        config_dir.join("early.lua"),
        "_G.__early_count = (_G.__early_count or 0) + 1\n",
    )
    .unwrap();
    std::fs::write(
        config_dir.join("init.lua"),
        "_G.__config_count = (_G.__config_count or 0) + 1\n",
    )
    .unwrap();
    std::fs::write(
        config_dir.join("plugins/count.lua"),
        "_G.__plugin_count = (_G.__plugin_count or 0) + 1\n",
    )
    .unwrap();
    std::fs::write(
        project_config_dir.join("early.lua"),
        "_G.__project_early_count = (_G.__project_early_count or 0) + 1\n",
    )
    .unwrap();
    std::fs::write(
        project_config_dir.join("init.lua"),
        "_G.__project_count = (_G.__project_count or 0) + 1\n",
    )
    .unwrap();

    smelt_core::trust::TrustStore::new(home.join("state/smelt"))
        .mark_trusted(&project_dir)
        .unwrap();

    let mut app = TestApp::builder()
        .with_runtime_home(&home)
        .with_lua_load_paths(&config_dir, Some(runtime_dir))
        .with_cwd(&project_dir)
        .build();

    assert_eq!(app.lua_probe().id, 0, "launch must retain generation zero");
    for name in [
        "__early_count",
        "__autoload_count",
        "__config_count",
        "__plugin_count",
        "__project_early_count",
        "__project_count",
    ] {
        assert_eq!(app.lua_int_global(name), Some(1), "{name} launch count");
    }

    app.reload_lua();

    assert_eq!(app.lua_probe().id, 1, "reload must commit generation one");
    for name in [
        "__autoload_count",
        "__config_count",
        "__plugin_count",
        "__project_count",
    ] {
        assert_eq!(app.lua_int_global(name), Some(1), "{name} reload count");
    }
}

#[test]
fn internal_lua_capabilities_stay_private_across_user_sources_and_reload() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let config_dir = root.path().join("config");
    let runtime_dir = root.path().join("runtime");
    let project_dir = root.path().join("project");
    let project_config_dir = project_dir.join(".smelt");
    let project_runtime_dir = project_config_dir.join("runtime/smelt");

    std::fs::create_dir_all(config_dir.join("plugins")).unwrap();
    std::fs::create_dir_all(runtime_dir.join("smelt/commands")).unwrap();
    std::fs::create_dir_all(&project_runtime_dir).unwrap();

    let probe = r#"
        assert(__smelt_internal == nil)
        assert(rawget(_G, "__smelt_internal") == nil)
        assert(rawget(_G, "__smelt_lifecycle_latest__") == nil)
        assert(rawget(_G, "__smelt_raw_engine_ask__") == nil)
        assert(rawget(_G, "__smelt_raw_engine_ask_inherited__") == nil)
        assert(rawget(_G, "__smelt_raw_tools_register__") == nil)
        assert(rawget(smelt, "__sweep_state") == nil)
        assert(rawget(smelt.confirm, "__resolve") == nil)
        assert(rawget(smelt.notebook, "preview_data") == nil)
        assert(rawget(smelt.session, "_rewind_active_turn_if_clean") == nil)
        assert(package.loaded.__smelt_internal == nil)
        assert(not pcall(require, "__smelt_internal"))
        assert(debug == nil or type(debug.getregistry) ~= "function")
        assert(type(smelt.state.get("internal-privacy-probe")) == "table")
        assert(type(smelt.ns("internal-privacy-probe")) == "number")
    "#;
    let source = |marker: &str| format!("{probe}\n_G.{marker} = true\n");

    std::fs::write(config_dir.join("init.lua"), source("user_init_private")).unwrap();
    std::fs::write(
        config_dir.join("plugins/privacy.lua"),
        source("user_plugin_private"),
    )
    .unwrap();
    std::fs::write(
        project_config_dir.join("init.lua"),
        format!(
            "{}\nrequire('smelt.project_privacy_probe')\n",
            source("project_init_private")
        ),
    )
    .unwrap();
    // A user override keeps the name of a bundled module. Source origin, not
    // module name, determines capability access.
    std::fs::write(
        runtime_dir.join("smelt/commands/color.lua"),
        source("user_override_private"),
    )
    .unwrap();
    std::fs::write(
        project_runtime_dir.join("project_privacy_probe.lua"),
        source("project_override_private"),
    )
    .unwrap();

    smelt_core::trust::TrustStore::new(home.join("state/smelt"))
        .mark_trusted(&project_dir)
        .unwrap();

    let mut app = TestApp::builder()
        .with_runtime_home(&home)
        .with_lua_load_paths(&config_dir, Some(runtime_dir))
        .with_cwd(&project_dir)
        .build();

    let markers = [
        "user_init_private",
        "user_plugin_private",
        "project_init_private",
        "user_override_private",
        "project_override_private",
    ];
    for marker in markers {
        assert!(
            app.eval_lua::<bool>(&format!("return {marker}"))
                .unwrap_or(false),
            "{marker} launch probe"
        );
    }

    let generation = app.lua_probe().id;
    app.reload_lua();
    assert_eq!(app.lua_probe().id, generation.wrapping_add(1));
    for marker in markers {
        assert!(
            app.eval_lua::<bool>(&format!("return {marker}"))
                .unwrap_or(false),
            "{marker} reload probe"
        );
    }

    let committed_generation = app.lua_probe().id;
    std::fs::write(
        config_dir.join("init.lua"),
        format!("{probe}\nerror('internal privacy candidate probe complete')\n"),
    )
    .unwrap();
    app.reload_lua();
    assert_eq!(app.lua_probe().id, committed_generation);
    assert!(app.lua_messages_contain("internal privacy candidate probe complete"));
    assert!(app.run_lua(probe));
}

#[test]
fn generation_zero_respects_lua_vim_config_unless_startup_overrides_it() {
    let root = tempfile::tempdir().unwrap();
    let init = root.path().join("init.lua");
    std::fs::write(&init, "smelt.settings.vim = true\n").unwrap();

    let app = TestApp::builder().with_init_lua(&init).build();
    assert!(app.core_probe().config.settings.vim);

    let app = TestApp::builder()
        .with_init_lua(&init)
        .with_vim(false)
        .build();
    assert!(!app.core_probe().config.settings.vim);
}

#[test]
fn generation_zero_filters_explicit_mode_cycle_against_final_lua_modes() {
    let root = tempfile::tempdir().unwrap();
    let init = root.path().join("init.lua");
    std::fs::write(
        &init,
        r#"smelt.mode.register({ name = "review", after = "normal" })"#,
    )
    .unwrap();
    let normal = AgentMode::normal();
    let review = AgentMode::parse("review").unwrap();
    let unavailable = AgentMode::parse("unavailable").unwrap();

    let app = TestApp::builder()
        .with_init_lua(&init)
        .with_mode_cycle(vec![normal.clone(), review.clone(), unavailable])
        .build();

    assert_eq!(app.core_probe().config.mode_cycle, vec![normal, review]);
}

#[test]
fn lua_config_auto_reload_success_notifies_for_real_edits() {
    let mut app = TestApp::builder().build();
    assert!(app.state().notification.is_none());

    app.reload_lua_config();

    assert!(app.state().notification.is_some());
}

#[test]
fn manual_lua_reload_success_notifies() {
    let mut app = TestApp::builder().build();

    app.reload_lua();

    assert!(app.state().notification.is_some());
}

#[test]
fn reloaded_transcript_view_watcher_receives_current_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    std::fs::write(
        &init,
        r#"
        smelt.transcript.watch_view(function(view)
          _G.watched_transcript_revision = view.revision
        end)
        "#,
    )
    .unwrap();
    let mut app = TestApp::builder().with_init_lua(&init).build();
    app.render_silent();
    let revision = app
        .lua_int_global("watched_transcript_revision")
        .expect("initial committed transcript view");

    app.reload_lua();
    assert_eq!(app.lua_int_global("watched_transcript_revision"), None);
    app.render_silent();
    assert_eq!(
        app.lua_int_global("watched_transcript_revision"),
        Some(revision)
    );
}

#[test]
fn candidate_layout_registration_does_not_invoke_committed_composer() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    std::fs::write(&init, "").unwrap();
    let mut app = TestApp::builder().with_init_lua(&init).build();
    let committed_generation = app.lua_probe().id;
    let marker = "committed composer invoked during candidate load";

    assert!(app.run_lua(&format!(
        r#"
        smelt.ui.layout.set(function()
          error({marker:?})
        end)
        "#
    )));
    app.clear_lua_messages();

    app.reload_lua();

    assert_eq!(app.lua_probe().id, committed_generation.wrapping_add(1));
    assert!(
        app.lua_probe()
            .core_shared()
            .messages
            .lock()
            .unwrap()
            .entries()
            .iter()
            .all(|entry| !entry.full.contains(marker)),
        "a staged layout declaration must not refresh the committed composer"
    );
}

#[test]
fn failed_lua_reload_preserves_the_committed_command_generation() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    std::fs::write(
        &init,
        r#"
        local buffer = smelt.buf.new({ name = "phase3.transaction.buffer" })
        buffer:source("committed")
        smelt.cmd.register("committed_command", function()
            _G.__committed_command_ran = true
            _G.__committed_buffer_source = buffer:source()
        end)
        "#,
    )
    .unwrap();
    let mut app = TestApp::builder().with_init_lua(&init).build();
    let command_names = app.lua_probe().command_names_handle();
    let committed_generation = app.lua_probe().id;
    let committed_runtime = app.core_probe().config.clone();
    assert!(command_names.lock().unwrap().contains("committed_command"));
    app.type_text("/committed_command");
    assert!(prompt_has_command_highlight(&mut app));

    std::fs::write(
        &init,
        r#"
        local buffer = smelt.buf.new({ name = "phase3.transaction.buffer" })
        buffer:source("discarded")
        smelt.settings.show_slug = false
        smelt.signal.new("phase3_candidate_only", 1)
        smelt.signal.subscribe("phase3_candidate_only", function() end)
        smelt.timer.set(60000, function() end)
        smelt.provider.register("discarded", {
            type = "openai-compatible",
            api_base = "https://discarded.invalid/v1",
            models = { "discarded-model" },
        })
        smelt.lifecycle.on("ready", function()
            error("discarded ready hook ran")
        end)
        error("candidate failed after declarations")
        "#,
    )
    .unwrap();
    app.reload_lua();

    assert_eq!(app.lua_probe().id, committed_generation);
    assert_eq!(app.core_probe().config, committed_runtime);
    assert!(
        prompt_has_command_highlight(&mut app),
        "a failed candidate must leave prompt recognition on the committed catalog"
    );
    assert!(!app
        .core_probe()
        .timers
        .contains_generation(committed_generation.wrapping_add(1)));
    assert!(!app.core_probe().signals.contains("phase3_candidate_only"));
    assert!(
        command_names.lock().unwrap().contains("committed_command"),
        "a failed candidate must leave the committed command callable"
    );
    let committed_buffer = app
        .ui_probe()
        .named_buf("phase3.transaction.buffer")
        .expect("committed named buffer");
    assert_eq!(
        app.ui_probe().buf(committed_buffer).unwrap().source(),
        "committed"
    );
    assert!(app.lua_probe().run_command("committed_command", None));
    assert!(app.run_lua("assert(_G.__committed_command_ran == true)"));
    let failure_message_count = app
        .lua_probe()
        .core_shared()
        .messages
        .lock()
        .unwrap()
        .count();
    app.reload_lua();
    assert_eq!(app.lua_probe().id, committed_generation);
    assert_eq!(
        app.lua_probe()
            .core_shared()
            .messages
            .lock()
            .unwrap()
            .count(),
        failure_message_count,
        "equal candidate failures should remain one sticky diagnostic"
    );

    std::fs::write(
        &init,
        r#"
        smelt.cmd.register("replacement_command", function() end)
        "#,
    )
    .unwrap();
    app.reload_lua();

    assert_eq!(app.lua_probe().id, committed_generation.wrapping_add(1));
    let replacement_names = app.lua_probe().command_names_handle();
    let replacement_names = replacement_names.lock().unwrap();
    assert!(replacement_names.contains("replacement_command"));
    assert!(!replacement_names.contains("committed_command"));
    drop(replacement_names);
    assert!(
        !prompt_has_command_highlight(&mut app),
        "a committed candidate must reparse the prompt against its catalog"
    );
}

#[test]
fn repeated_failed_candidates_do_not_leak_handles_or_resources() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    std::fs::write(
        &init,
        r#"
        smelt.cmd.register("committed_before_failed_leak_check", function() end)
        "#,
    )
    .unwrap();
    let mut app = TestApp::builder().with_init_lua(&init).build();

    app.assert_no_handle_leak_across_failed_reload(&init);
    app.reload_lua();

    assert!(app
        .lua_probe()
        .command_names_handle()
        .lock()
        .unwrap()
        .contains("committed_before_failed_leak_check"));
}

#[test]
fn runtime_status_sanitizes_and_clears_candidate_failure_diagnostics() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    std::fs::write(&init, "_G.committed = true\n").unwrap();
    let mut app = TestApp::builder().with_init_lua(&init).build();

    std::fs::write(&init, "error('private candidate source detail')\n").unwrap();
    app.reload_lua();
    assert!(app.run_lua(
        r#"
        local status = smelt.config.runtime_status()
        assert(type(status.lua_generation) == "number")
        assert(type(status.runtime_revision) == "number")
        assert(status.reload.failure.phase == "user")
        assert(type(status.reload.failure.path) == "string")
        assert(not status.reload.failure.path:find("private candidate source detail", 1, true))
        assert(type(status.controllers.lsp.status) == "string")
        local model_status = smelt.model.status()
        for name, provider in pairs(model_status.providers) do
          assert(type(name) == "string")
          assert(type(provider.auth_revision) == "number")
          assert(type(provider.desired_revision) == "number")
        end
        "#,
    ));

    std::fs::write(&init, "_G.recovered = true\n").unwrap();
    app.reload_lua();
    assert!(app.run_lua("assert(smelt.config.runtime_status().reload.failure == nil)"));
}

#[test]
fn failed_candidate_rejects_external_effects_and_recovers() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    let immediate_effect = tmp.path().join("candidate-effect.txt");
    let ready_effect = tmp.path().join("candidate-ready.txt");
    std::fs::write(
        &init,
        r#"
        smelt.cmd.register("committed_external_guard", function()
            _G.__committed_external_guard = true
        end)
        "#,
    )
    .unwrap();
    let mut app = TestApp::builder().with_init_lua(&init).build();
    let committed_generation = app.lua_probe().id;

    let immediate_path = serde_json::to_string(&immediate_effect.to_string_lossy()).unwrap();
    let ready_path = serde_json::to_string(&ready_effect.to_string_lossy()).unwrap();
    std::fs::write(
        &init,
        format!(
            r#"
            smelt.notify.info("discarded candidate notice", "phase3-candidate")
            smelt.log.info("discarded_candidate_log")
            smelt.lifecycle.on("ready", function()
                smelt.fs.write({ready_path}, "ran")
            end)
            smelt.fs.write({immediate_path}, "must not run")
            "#
        ),
    )
    .unwrap();
    app.reload_lua();

    assert_eq!(app.lua_probe().id, committed_generation);
    assert!(!immediate_effect.exists());
    assert!(!ready_effect.exists());
    assert!(app
        .lua_probe()
        .run_command("committed_external_guard", None));
    assert!(app.run_lua("assert(_G.__committed_external_guard == true)"));
    let messages = app.lua_probe().core_shared().messages.lock().unwrap();
    assert!(
        messages
            .entries()
            .iter()
            .all(|entry| entry.source != "phase3-candidate"),
        "discarded candidate notices must not reach the live message log"
    );
    drop(messages);

    std::fs::write(
        &init,
        format!(
            r#"
            smelt.cmd.register("recovered_external_guard", function() end)
            smelt.notify.info("committed candidate notice", "phase3-committed")
            smelt.log.info("committed_candidate_log")
            smelt.lifecycle.on("ready", function()
                smelt.fs.write({ready_path}, "ran")
            end)
            "#
        ),
    )
    .unwrap();
    app.reload_lua();

    assert_eq!(app.lua_probe().id, committed_generation.wrapping_add(1));
    assert_eq!(std::fs::read_to_string(ready_effect).unwrap(), "ran");
    assert!(app
        .lua_probe()
        .core_shared()
        .messages
        .lock()
        .unwrap()
        .entries()
        .iter()
        .any(|entry| entry.source == "phase3-committed"));
    assert!(app
        .lua_probe()
        .command_names_handle()
        .lock()
        .unwrap()
        .contains("recovered_external_guard"));
}

#[test]
fn bundled_composition_uses_live_effect_guards() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    std::fs::write(
        &init,
        r#"smelt.cmd.register("committed_before_bundled_guard", function() end)"#,
    )
    .unwrap();
    let mut app = TestApp::builder().with_init_lua(&init).build();
    let committed_generation = app.lua_probe().id;

    std::fs::write(&init, "smelt.mode.cycle()").unwrap();
    app.reload_lua();

    assert_eq!(app.lua_probe().id, committed_generation);
    let failure = app
        .app
        .lua_reload_failure()
        .expect("the direct effect should fail candidate loading");
    assert!(
        failure
            .message
            .contains("smelt.mode.set is unavailable while loading a Lua candidate"),
        "unexpected candidate failure: {failure}"
    );
    assert!(app
        .lua_probe()
        .command_names_handle()
        .lock()
        .unwrap()
        .contains("committed_before_bundled_guard"));

    std::fs::write(
        &init,
        r#"
        smelt.cmd.register("call_bundled_after_commit", function()
            smelt.mode.cycle()
            _G.__bundled_guard_called_after_commit = true
        end)
        "#,
    )
    .unwrap();
    app.reload_lua();

    assert_eq!(app.lua_probe().id, committed_generation.wrapping_add(1));
    assert!(app.app.lua_reload_failure().is_none());
    assert!(app.run_lua("smelt.mode.cycle(); _G.__bundled_guard_called_after_commit = true"));
    assert!(app.run_lua("assert(_G.__bundled_guard_called_after_commit == true)"));
}

#[test]
fn candidate_rejects_direct_perf_mutations() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    std::fs::write(
        &init,
        r#"smelt.cmd.register("committed_before_perf_candidate", function() end)"#,
    )
    .unwrap();
    let mut app = TestApp::builder().with_init_lua(&init).build();
    let committed_generation = app.lua_probe().id;

    for effect in [
        "smelt.metrics.perf.set_enabled(true)",
        "smelt.metrics.perf.clear()",
    ] {
        smelt_perf::perf::set_enabled(false);
        std::fs::write(&init, effect).unwrap();
        app.reload_lua();
        assert!(!smelt_perf::perf::enabled());
        assert_eq!(app.lua_probe().id, committed_generation);
        assert!(app
            .lua_probe()
            .command_names_handle()
            .lock()
            .unwrap()
            .contains("committed_before_perf_candidate"));
    }
}

#[test]
fn candidate_rejects_raw_lua_external_effects() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    let protected = tmp.path().join("protected.txt");
    let protected_lua = serde_json::to_string(&protected.to_string_lossy()).unwrap();
    std::fs::write(
        &init,
        r#"smelt.cmd.register("committed_before_raw_effect_candidate", function() end)"#,
    )
    .unwrap();
    let mut app = TestApp::builder().with_init_lua(&init).build();
    let committed_generation = app.lua_probe().id;

    for effect in [
        format!("local f = assert(io.open({protected_lua}, 'w')); f:write('mutated'); f:close()"),
        format!("assert(os.remove({protected_lua}))"),
    ] {
        std::fs::write(&protected, "preserved").unwrap();
        std::fs::write(&init, effect).unwrap();
        app.reload_lua();
        assert_eq!(std::fs::read_to_string(&protected).unwrap(), "preserved");
        assert_eq!(app.lua_probe().id, committed_generation);
        assert!(app
            .lua_probe()
            .command_names_handle()
            .lock()
            .unwrap()
            .contains("committed_before_raw_effect_candidate"));
    }
}

#[test]
fn candidate_filesystem_watchers_activate_only_at_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    std::fs::write(
        &init,
        r#"smelt.cmd.register("watcher_committed", function() end)"#,
    )
    .unwrap();
    let mut app = TestApp::builder().with_init_lua(&init).build();
    let committed_generation = app.lua_probe().id;
    let missing = tmp.path().join("missing");
    let missing = serde_json::to_string(&missing.to_string_lossy()).unwrap();
    std::fs::write(
        &init,
        format!(
            r#"
            smelt.fs.watch({missing}, function() end)
            error("discard watcher candidate")
            "#
        ),
    )
    .unwrap();

    app.reload_lua();

    assert_eq!(app.lua_probe().id, committed_generation);
    assert!(app
        .lua_probe()
        .command_names_handle()
        .lock()
        .unwrap()
        .contains("watcher_committed"));

    let watched = tmp.path().join("watched");
    std::fs::create_dir(&watched).unwrap();
    let watched = serde_json::to_string(&watched.to_string_lossy()).unwrap();
    std::fs::write(
        &init,
        format!(r#"smelt.fs.watch({watched}, function() end)"#),
    )
    .unwrap();
    app.reload_lua();
    assert_eq!(app.lua_probe().id, committed_generation.wrapping_add(1));
}

#[test]
fn mcp_and_lsp_declarations_apply_only_after_candidate_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    let config = |name: &str, fail: bool| {
        format!(
            r#"
            smelt.mcp.register("{name}", {{ command = {{ "cat" }} }})
            smelt.lsp.configure({{
                servers = {{
                    ["{name}"] = {{ cmd = {{ "{name}-language-server" }} }},
                }},
            }})
            {}
            "#,
            if fail {
                "error('discard candidate')"
            } else {
                ""
            }
        )
    };

    std::fs::write(&init, config("committed", false)).unwrap();
    let mut app = TestApp::builder().with_init_lua(&init).build();
    app.reload_lua();
    let committed_generation = app.lua_probe().id;
    assert!(app
        .lua_probe()
        .desired()
        .config
        .mcp
        .contains_key("committed"));
    assert!(app
        .lua_probe()
        .shared()
        .lsp
        .config_snapshot()
        .servers
        .contains_key("committed"));

    std::fs::write(&init, config("discarded", true)).unwrap();
    app.reload_lua();
    assert_eq!(app.lua_probe().id, committed_generation);
    assert!(app
        .lua_probe()
        .desired()
        .config
        .mcp
        .contains_key("committed"));
    let live_lsp = app.lua_probe().shared().lsp.config_snapshot();
    assert!(live_lsp.servers.contains_key("committed"));
    assert!(!live_lsp.servers.contains_key("discarded"));

    std::fs::write(&init, config("replacement", false)).unwrap();
    app.reload_lua();
    assert_eq!(app.lua_probe().id, committed_generation.wrapping_add(1));
    assert!(app
        .lua_probe()
        .desired()
        .config
        .mcp
        .contains_key("replacement"));
    let live_lsp = app.lua_probe().shared().lsp.config_snapshot();
    assert!(live_lsp.servers.contains_key("replacement"));
    assert!(!live_lsp.servers.contains_key("committed"));
}

#[test]
fn failed_early_lua_candidate_preserves_committed_generation_and_recovers() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    std::fs::write(
        &init,
        r#"
        smelt.cmd.register("early_phase_committed", function()
            _G.__early_phase_committed = true
        end)
        "#,
    )
    .unwrap();
    let config_dir = tmp.path().join("config");
    let mut app = TestApp::builder()
        .with_init_lua(&init)
        .with_lua_load_paths(&config_dir, None)
        .build();

    let early = config_dir.join("early.lua");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        &early,
        r#"
        smelt.provider.register("early-provider", {
            type = "openai-compatible",
            api_base = "https://early.invalid/v1",
            models = { "early-model" },
        })
        "#,
    )
    .unwrap();
    app.reload_lua();
    let committed_generation = app.lua_probe().id;
    assert!(app.lua_probe().manifest.files.contains(&early));
    assert!(app.lua_probe().manifest.files.contains(&init));
    assert!(app
        .core_probe()
        .config
        .available_models
        .iter()
        .any(|model| model.key == "early-provider/early-model"));

    std::fs::write(&early, "this is not valid Lua @@@").unwrap();
    app.reload_lua();

    assert_eq!(app.lua_probe().id, committed_generation);
    assert!(app.lua_probe().run_command("early_phase_committed", None));
    assert!(app.run_lua("assert(_G.__early_phase_committed == true)"));

    std::fs::write(&early, "-- fixed\n").unwrap();
    app.reload_lua();
    assert_eq!(app.lua_probe().id, committed_generation.wrapping_add(1));
}

#[test]
fn changed_early_launch_declarations_use_defaults_and_warn_for_restart() {
    let config = tempfile::tempdir().unwrap();
    let config_dir = config.path().join("smelt");
    let mut app = TestApp::builder()
        .with_lua_load_paths(&config_dir, None)
        .build();
    let early = config_dir.join("early.lua");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        &early,
        r#"
        smelt.cli.register_flag({
            name = "phase3_new_flag",
            kind = "string",
            default = "candidate-default",
        })
        "#,
    )
    .unwrap();

    app.reload_lua();

    assert!(app.run_lua("assert(smelt.cli.get('phase3_new_flag') == 'candidate-default')"));
    assert_eq!(app.lua_probe().warnings().len(), 1);
    assert!(app.lua_probe().warnings()[0].contains("restart smelt"));
}

#[test]
fn failed_candidate_at_each_filesystem_load_phase_recovers() {
    let project = tempfile::tempdir().unwrap();
    let smelt_dir = project.path().join(".smelt");
    std::fs::create_dir_all(&smelt_dir).unwrap();
    let project_init = smelt_dir.join("init.lua");
    std::fs::write(
        &project_init,
        r#"smelt.cmd.register("phase3_project", function() end)"#,
    )
    .unwrap();

    let init_dir = tempfile::tempdir().unwrap();
    let init = init_dir.path().join("init.lua");
    std::fs::write(
        &init,
        r#"smelt.cmd.register("phase3_committed", function() end)"#,
    )
    .unwrap();
    let config = tempfile::tempdir().unwrap();
    let config_dir = config.path().join("smelt");
    let runtime = tempfile::tempdir().unwrap();
    let runtime_smelt = runtime.path().join("smelt");
    std::fs::create_dir_all(runtime_smelt.join("commands")).unwrap();
    let mut app = TestApp::builder()
        .with_init_lua(&init)
        .with_lua_load_paths(&config_dir, Some(runtime.path().to_path_buf()))
        .with_cwd(project.path())
        .build();
    app.mark_project_trusted(project.path()).unwrap();
    app.reload_lua();
    let mut committed_generation = app.lua_probe().id;
    assert!(app
        .lua_probe()
        .command_names_handle()
        .lock()
        .unwrap()
        .contains("phase3_project"));

    let global_plugin_dir = config_dir.join("plugins");
    std::fs::create_dir_all(&global_plugin_dir).unwrap();
    let global_plugin = global_plugin_dir.join("phase3_failure.lua");
    std::fs::write(&global_plugin, "error('global plugin candidate failure')").unwrap();
    app.reload_lua();
    assert_eq!(app.lua_probe().id, committed_generation);
    assert!(app
        .lua_probe()
        .command_names_handle()
        .lock()
        .unwrap()
        .contains("phase3_project"));
    std::fs::remove_file(&global_plugin).unwrap();
    app.reload_lua();
    committed_generation = app.lua_probe().id;

    std::fs::write(&project_init, "this is not valid project Lua @@@").unwrap();
    app.mark_project_trusted(project.path()).unwrap();
    app.reload_lua();
    assert_eq!(app.lua_probe().id, committed_generation);
    assert!(app
        .lua_probe()
        .command_names_handle()
        .lock()
        .unwrap()
        .contains("phase3_project"));
    std::fs::write(
        &project_init,
        r#"smelt.cmd.register("phase3_project_recovered", function() end)"#,
    )
    .unwrap();
    app.mark_project_trusted(project.path()).unwrap();
    app.reload_lua();
    committed_generation = app.lua_probe().id;

    let autoload_override = runtime_smelt.join("commands/color.lua");
    std::fs::write(&autoload_override, "error('autoload candidate failure')").unwrap();
    app.reload_lua();
    assert_eq!(app.lua_probe().id, committed_generation);
    std::fs::write(&autoload_override, "return true\n").unwrap();
    app.reload_lua();
    committed_generation = app.lua_probe().id;
    assert!(app.lua_probe().manifest.files.contains(&autoload_override));
    std::fs::remove_file(&autoload_override).unwrap();

    let bootstrap_override = runtime_smelt.join("_bootstrap.lua");
    let bootstrap_effect = runtime.path().join("bootstrap-effect.txt");
    let bootstrap_effect_json = serde_json::to_string(&bootstrap_effect.to_string_lossy()).unwrap();
    std::fs::write(
        &bootstrap_override,
        format!(r#"smelt.fs.write({bootstrap_effect_json}, "must not run")"#),
    )
    .unwrap();
    app.reload_lua();
    assert_eq!(app.lua_probe().id, committed_generation);
    assert!(!bootstrap_effect.exists());

    std::fs::write(&bootstrap_override, "this is not valid bootstrap Lua @@@").unwrap();
    app.reload_lua();
    assert_eq!(app.lua_probe().id, committed_generation);
    std::fs::remove_file(&bootstrap_override).unwrap();
    app.reload_lua();
    assert_eq!(app.lua_probe().id, committed_generation.wrapping_add(1));
}

#[test]
fn provider_reload_rebuilds_the_running_model_catalog() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    std::fs::write(&init, "-- initially empty\n").unwrap();
    let mut app = TestApp::builder().with_init_lua(&init).build();

    std::fs::write(
        &init,
        r#"
        smelt.provider.register("phase0", {
            type = "openai-compatible",
            api_base = "https://example.invalid/v1",
            models = { "model-a" },
        })
        "#,
    )
    .unwrap();
    app.reload_lua();
    assert!(app.run_lua(
        r#"
        _G.__phase0_provider_count = #smelt.provider.list()
        _G.__phase0_model_count = #smelt.model.list()
        "#,
    ));
    assert_eq!(app.lua_int_global("__phase0_provider_count"), Some(1));
    assert_eq!(
        app.lua_int_global("__phase0_model_count"),
        Some(1),
        "committed provider declarations must immediately reach the model picker"
    );
}

#[test]
fn setting_write_updates_desired_state_before_live_effects() {
    let mut app = TestApp::builder().build();

    let succeeded = app
        .exec_lua_entry(
            r#"
                smelt.settings.show_slug = false
                assert(smelt.settings.show_slug == false)
                "#,
        )
        .is_ok();
    assert!(succeeded);
    assert!(app.lua_runtime_reconcile_pending());
    assert!(app.core_probe().config.settings.show_slug);
    assert!(!app.lua_probe().to_config().settings.show_slug);

    assert!(app.drain_idle_work());
    assert!(!app.core_probe().config.settings.show_slug);
}

#[test]
fn desired_state_reconstruction_lends_the_tui_host() {
    let mut app = TestApp::builder().build();

    app.run_lua_result(
        r#"
            local original_list = smelt.mode.list
            smelt.mode.list = function()
                assert(smelt.session.id() ~= "")
                return original_list()
            end
            smelt.settings.show_slug = false
            "#,
    )
    .expect("reconcile desired state through host-capable mode callback");

    assert!(app
        .lua_probe()
        .desired()
        .modes
        .cycle
        .iter()
        .any(|mode| mode.as_str() == "normal"));
}

#[test]
fn shutdown_hooks_and_state_flush_lend_the_tui_host() {
    let mut app = TestApp::builder().build();
    let expected_session_id = app.session_snapshot().id.clone();
    app.exec_lua_entry(
        r#"
            smelt.lifecycle.on_shutdown(function(ctx)
                _G.__shutdown_host_session = smelt.session.id()
                _G.__shutdown_ctx_session = ctx.session_id
            end)
            "#,
    )
    .expect("register shutdown callbacks");

    let (errors, flush_error) = app.shutdown_lua();

    assert!(errors.is_empty(), "shutdown errors: {errors:?}");
    assert_eq!(flush_error, None);
    let values: (String, String) = app
        .eval_lua("return __shutdown_host_session, __shutdown_ctx_session")
        .expect("read shutdown callback results");
    assert_eq!(values, (expected_session_id.clone(), expected_session_id));
}

#[test]
fn every_scalar_setting_write_reconciles_desired_state_and_live_effects() {
    use smelt_core::config::{SettingKind, SettingValue, SETTINGS};

    let mut app = TestApp::builder().build();
    app.set_prompt_placeholder_text("stale prediction".into());

    for declaration in SETTINGS {
        let current = (declaration.read)(&app.core_probe().config.settings);
        let next = match (declaration.key, declaration.kind, current) {
            (_, SettingKind::Bool, SettingValue::Bool(value)) => SettingValue::Bool(!value),
            ("compact_threshold", SettingKind::Number, _) => SettingValue::Number(0.65),
            ("compact_keep_recent_groups", SettingKind::Number, _) => SettingValue::Number(2.0),
            ("autoupgrade_interval", SettingKind::Number, _) => SettingValue::Number(120.0),
            (_, SettingKind::String, SettingValue::String(value)) => {
                let value = declaration
                    .choices
                    .and_then(|choices| choices.iter().find(|choice| **choice != value))
                    .map_or_else(
                        || format!("phase4_{}", declaration.key),
                        |value| value.to_string(),
                    );
                SettingValue::String(value)
            }
            _ => panic!("setting schema kind mismatch for {}", declaration.key),
        };
        let lua_value = match &next {
            SettingValue::Bool(value) => value.to_string(),
            SettingValue::Number(value) => value.to_string(),
            SettingValue::String(value) => serde_json::to_string(value).unwrap(),
        };
        assert!(
            app.run_lua(&format!(
                "smelt.settings[{}] = {lua_value}",
                serde_json::to_string(declaration.key).unwrap()
            )),
            "runtime write failed for {}",
            declaration.key
        );
        assert_eq!(
            app.core_probe().config.settings.get(declaration.key),
            Some(next.clone()),
            "resolved setting stayed stale for {}",
            declaration.key
        );
        assert_eq!(
            app.lua_probe().to_config().settings.get(declaration.key),
            Some(next),
            "desired setting stayed stale for {}",
            declaration.key
        );
    }

    assert!(app
        .ui_probe()
        .win(crate::app::PROMPT_WIN)
        .expect("prompt window")
        .vim_enabled());
    assert_eq!(
        app.placeholder_text(crate::app::PROMPT_WIN),
        None,
        "disabling prediction must clear existing ghost text"
    );
    assert_eq!(
        app.core_probe()
            .signals
            .get::<bool>("settings_terminal_title"),
        Some(false)
    );
    assert!(!app.auto_reload_probe().start_pending);
    assert!(app.auto_reload_probe().handle.is_none());
    assert!(app.auto_reload_probe().events.is_none());
    assert!(app.auto_reload_probe().setup.is_none());
    assert!(app.run_lua("smelt.settings.auto_reload = true"));
    assert!(app.auto_reload_probe().start_pending);
    assert!(app.run_lua("smelt.settings.auto_reload = false"));
    assert!(!app.auto_reload_probe().start_pending);
    assert!(
        app.core_probe()
            .config
            .request_runtime_config()
            .redact_secrets
    );
    assert!(
        app.core_probe()
            .config
            .request_runtime_config()
            .cache_ttl_long
    );
}

#[test]
fn permission_policy_is_snapshotted_per_turn_while_session_approvals_stay_live() {
    let mut app = TestApp::builder().build();
    app.start_turn(7);
    let turn_permissions = app
        .conversation_probe()
        .active()
        .unwrap()
        .permissions
        .clone();
    assert!(turn_permissions.restrict_to_workspace());

    assert!(app.run_lua("smelt.settings.restrict_to_workspace = false"));
    let current_permissions = app.core_probe().permissions.snapshot();
    assert!(!current_permissions.restrict_to_workspace());
    assert!(
        turn_permissions.restrict_to_workspace(),
        "active static policy must not change after a setting reconciliation"
    );
    assert!(std::sync::Arc::ptr_eq(
        &turn_permissions.approvals,
        &current_permissions.approvals,
    ));

    let trusted = std::env::temp_dir().join("smelt-phase4-session-approval");
    app.grant_session_path(
        None,
        "read_file".into(),
        smelt_core::permissions::PathAccess::Read,
        trusted.clone(),
    );
    assert!(turn_permissions
        .approvals
        .read()
        .unwrap()
        .session_path_grants()
        .iter()
        .any(|grant| grant.dir == trusted));
}

#[test]
fn relative_lua_session_path_grant_uses_runtime_cwd() {
    let mut app = TestApp::builder().build();
    let relative = std::path::Path::new("permission-fixtures/nested");
    let expected = std::path::Path::new(app.cwd_str()).join(relative);

    assert!(app.run_lua(
        r#"
        smelt.permissions.grant_session({
          kind = "path",
          tool = "read_file",
          access = "read",
          path_prefix = "permission-fixtures/nested",
        })
        "#,
    ));

    assert!(app.session_path_grants().iter().any(|grant| {
        grant.tool == "read_file"
            && grant.access == smelt_core::permissions::PathAccess::Read
            && grant.dir == expected
    }));
}

#[test]
fn lua_session_path_grant_survives_rewind_to_start() {
    let trusted_dir = tempfile::TempDir::new().expect("create trusted directory");
    let trusted =
        std::fs::canonicalize(trusted_dir.path()).expect("canonicalize trusted directory");
    let mut app = TestApp::builder().build();

    app.start_submitted_turn("read generated artifacts");
    app.grant_session_path(
        None,
        "read_file".into(),
        smelt_core::permissions::PathAccess::Read,
        trusted.clone(),
    );
    app.cancel();
    app.rewind_to_start();

    assert!(app.session_path_grants().iter().any(|grant| {
        grant.tool == "read_file"
            && grant.access == smelt_core::permissions::PathAccess::Read
            && grant.dir == trusted
    }));
}

#[test]
fn setting_write_reads_desired_value_while_preserving_startup_precedence() {
    let mut app = TestApp::builder().build();
    app.set_startup_setting_override_for_harness(
        "show_slug".into(),
        smelt_core::config::SettingValue::Bool(true),
    );
    let mut settings = app.core_probe().config.settings.clone();
    settings.show_slug = true;
    app.set_settings_for_harness(settings);

    assert!(app.run_lua(
        r#"
        smelt.settings.show_slug = false
        assert(smelt.settings.show_slug == false)
        local paired
        for key, value in pairs(smelt.settings) do
          if key == "show_slug" then paired = value end
        end
        assert(paired == false)
        "#,
    ));

    assert!(app.core_probe().config.settings.show_slug);
    assert!(!app.lua_probe().to_config().settings.show_slug);
}

#[test]
fn table_settings_are_owned_only_by_lua_state() {
    let mut app = TestApp::builder().build();
    let runtime_before = app.core_probe().config.clone();

    assert!(app.run_lua(
        r#"
        smelt.settings.notifications.turn_end = true
        smelt.settings.transcript.view.custom = "lua-owned"
        assert(smelt.settings.notifications.turn_end == true)
        assert(smelt.settings.transcript.view.custom == "lua-owned")
        "#,
    ));

    assert_eq!(app.core_probe().config, runtime_before);
}

#[test]
fn lua_config_session_and_transcript_contracts_are_available() {
    let mut app = TestApp::builder().build();
    app.push_user_block("hello from lua api test");
    app.push_transcript_block(smelt_core::Block::Text {
        content: "assistant line".into(),
    });
    app.render_normal_to(&mut std::io::sink());

    assert!(app.run_lua(
        r#"
            assert(type(smelt.config.provider_type()) == "string")
            assert(type(smelt.config.api_base()) == "string")
            assert(type(smelt.config.api_key_env()) == "string")
            assert(type(smelt.config.model_config()) == "table")

            assert(smelt.session.title.get() == nil)
            smelt.session.title.set("Lua Contract Title", "lua-contract-title")
            assert(smelt.session.title.get() == "Lua Contract Title")
            assert(smelt.session.slug.get() == "lua-contract-title")
            assert(type(smelt.session.cwd()) == "string")
            assert(type(smelt.session.tokens()) == "table")
            local info = smelt.session.info()
            assert(type(info.id) == "string")
            assert(info.id == smelt.session.id())
            assert(info.title == "Lua Contract Title")
            assert(type(info.artifact_dir) == "string")
            assert(info.artifact_dir == smelt.session.artifact_dir())
            assert(type(info.cwd) == "string")
            assert(type(info.tokens) == "table")
            assert(type(info.worktree) == "table")

            assert(smelt.transcript.is_empty() == false)
            local text = smelt.transcript.loaded_text_expensive()
            assert(text:find("hello from lua api test", 1, true))
            local blocks = smelt.transcript.loaded_blocks_expensive()
            assert(#blocks >= 1)
            assert(type(blocks[1].record_index) == "number")
            assert(type(blocks[1].role) == "string")
            assert(type(smelt.transcript.rows(0, 2)) == "table")

            local defaults = require("smelt.transcript.defaults")
            assert(defaults.display_count_text({ output = { metadata = { display_count = { value = 0, unit = "file" } } } }) == "0 files")
            assert(defaults.display_count_text({ output = { metadata = { display_count = { value = 1, unit = "match" } } } }) == "1 match")
            assert(defaults.display_count_text({ output = { metadata = { display_count = { value = 2, unit = "output line" } } } }) == "2 output lines")
            assert(defaults.display_count_text({}, { unit = "file" }) == "0 files")
        "#,
    ));
}

#[test]
fn lua_transcript_detail_apis_rehydrate_sparse_windows_and_release_pins() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(100, 32);
    for index in 0..700 {
        let marker = match index {
            10 => "lua sparse early exact marker",
            699 => "lua sparse tail exact marker",
            _ => "ordinary lua sparse content",
        };
        let content = format!("block {index}: {marker}");
        if index == 10 {
            app.push_transcript_block(smelt_core::Block::User {
                text: content,
                image_labels: Vec::new(),
                command: false,
            });
        } else {
            app.push_transcript_block(smelt_core::Block::Text {
                content: content.into(),
            });
        }
    }
    app.save_session_and_flush();
    let loaded = crate::app::history::load_transcript_tail_from_sqlite_id(
        &app.core_probe().sessions,
        &app.session_snapshot().id,
        100,
        32,
    )
    .expect("load sparse transcript");
    app.clear_transcript();
    app.replace_loaded_transcript_for_harness(loaded);
    app.set_transcript_memory_budget_for_harness(crate::app::transcript::TranscriptMemoryBudget {
        hydrated_blocks: 1,
        ..Default::default()
    });
    app.render_silent();

    app.run_lua_result(
        r#"
            _G.lua_sparse_tail_text = nil
            _G.lua_sparse_tail_block_count = nil
            _G.lua_sparse_tail_last_row = nil
            local immediate_text = smelt.transcript.loaded_text_expensive(function(text)
                _G.lua_sparse_tail_text = text
            end)
            local immediate_blocks = smelt.transcript.loaded_blocks_expensive(function(blocks)
                _G.lua_sparse_tail_block_count = #blocks
                _G.lua_sparse_tail_last_row = blocks[#blocks].first_row
            end)
            assert(type(immediate_text) == "string")
            assert(type(immediate_blocks) == "table")
            assert(lua_sparse_tail_text == nil)
            assert(lua_sparse_tail_block_count == nil)
        "#,
    )
    .expect("request sparse tail transcript details");
    app.render_silent();
    app.run_lua_result(
        r#"
            assert(lua_sparse_tail_text:find("lua sparse tail exact marker", 1, true))
            assert(lua_sparse_tail_block_count > 0)
            local rows = smelt.transcript.rows(lua_sparse_tail_last_row, 1)
            assert(type(rows) == "table")
        "#,
    )
    .expect("materialize sparse tail transcript details");
    let tail_snapshot = app.conversation_probe().transcript().memory_snapshot();
    assert!(tail_snapshot.hydration_reads > 0);
    assert!(
        tail_snapshot.hydration_reads < 100,
        "simultaneous Lua detail requests duplicated hydration reads: {tail_snapshot:?}"
    );
    assert!(
        tail_snapshot.hydrated_blocks < 100,
        "Lua detail operation retained its full loaded window: {tail_snapshot:?}"
    );

    assert!(app.run_lua(
        r#"
            local view = assert(smelt.transcript.view())
            local target = assert(view:previous_block({ role = "user" }))
            assert(target.first_line:find("lua sparse early exact marker", 1, true))
            assert(smelt.transcript.reveal(target, { move_cursor = true }))
        "#,
    ));
    app.render_silent();
    app.run_lua_result(
        r#"
            _G.lua_sparse_early_text = nil
            smelt.transcript.loaded_text_expensive(function(text)
                _G.lua_sparse_early_text = text
            end)
        "#,
    )
    .expect("request sparse early transcript text");
    app.render_silent();
    app.run_lua_result(
        r#"
            assert(lua_sparse_early_text:find("lua sparse early exact marker", 1, true))
        "#,
    )
    .expect("materialize sparse early transcript text");
    let early_snapshot = app.conversation_probe().transcript().memory_snapshot();
    assert!(early_snapshot.hydration_reads > tail_snapshot.hydration_reads);
    assert!(
        early_snapshot.hydrated_blocks < 100,
        "Lua reveal/detail pins did not converge: {early_snapshot:?}"
    );

    app.run_lua_result(
        r#"
            smelt.transcript.loaded_text_expensive(function(_)
                error("stale detail callback must not run")
            end)
        "#,
    )
    .expect("request transcript detail before Lua reload");
    app.reload_lua();
    app.render_silent();
    let stale_snapshot = app.conversation_probe().transcript().memory_snapshot();
    assert!(
        stale_snapshot.hydrated_blocks < 100,
        "stale Lua detail request retained operation pins: {stale_snapshot:?}"
    );

    app.run_lua_result(
        r#"
            smelt.transcript.loaded_text_expensive(function(_)
                error("intentional transcript detail callback failure")
            end)
        "#,
    )
    .expect("request failing transcript detail callback");
    app.render_silent();
    let error_snapshot = app.conversation_probe().transcript().memory_snapshot();
    assert!(
        error_snapshot.hydrated_blocks < 100,
        "failed Lua detail callback retained operation pins: {error_snapshot:?}"
    );
}

#[test]
fn lua_context_note_updates_named_history_notes_independently() {
    let mut app = TestApp::builder().build();
    app.session_append_history(protocol::HistoryItem::User {
        content: protocol::Content::text("hello"),
        display: None,
        command: false,
    });

    assert!(app.run_lua(
        r#"
            smelt.session.context_note("goal", "first goal")
            smelt.session.context_note("cwd", "custom cwd")
            smelt.session.context_note("goal", "second goal")
        "#,
    ));

    let history = &app.session_snapshot().history;
    assert!(history.iter().any(|item| matches!(
        item,
        protocol::HistoryItem::Note(note)
            if note.context_name() == Some("goal") && note.text() == "second goal"
    )));
    assert!(history.iter().any(|item| matches!(
        item,
        protocol::HistoryItem::Note(note)
            if note.context_name() == Some("cwd") && note.text() == "custom cwd"
    )));
    assert_eq!(
        history
            .iter()
            .filter_map(protocol::HistoryItem::as_note)
            .filter(|note| note.kind() == protocol::HistoryNoteKind::Context)
            .count(),
        3
    );
    assert_eq!(
        history
            .iter()
            .filter_map(protocol::HistoryItem::as_note)
            .filter(|note| note.context_name() == Some("goal"))
            .map(protocol::HistoryNote::text)
            .collect::<Vec<_>>(),
        vec!["first goal", "second goal"]
    );

    assert!(app.run_lua(r#"smelt.session.context_note("goal", nil)"#));
    let history = &app.session_snapshot().history;
    assert!(history
        .iter()
        .filter_map(protocol::HistoryItem::as_note)
        .rfind(|note| note.context_name() == Some("goal"))
        .is_some_and(|note| note.text().is_empty()));
    assert!(history.iter().any(|item| matches!(
        item,
        protocol::HistoryItem::Note(note)
            if note.context_name() == Some("cwd") && note.text() == "custom cwd"
    )));
}

#[test]
fn lua_goal_module_persists_and_updates_session_goal() {
    let mut app = TestApp::builder().build();

    assert!(app.run_lua(
        r#"
            local goal = require("smelt.goal")
            local created = assert(goal.create("finish named context notes", { auto_continue = false }))
            assert(created.id ~= nil and created.id ~= "")
            assert(type(created.created_at_ms) == "number")
            assert(goal.current().objective == "finish named context notes")
            assert(goal.current().state == "active")
            assert(goal.update_status({ summary = "Context notes", progress = "Phase 1/2" }))
            assert(goal.current().summary == "Context notes")
            assert(goal.current().progress.label == "Phase 1/2")
            assert(goal.describe():find("auto%-continue: off"))
            assert(goal.describe():find("state: active", 1, true))
            assert(goal.describe():find("summary: Context notes", 1, true))
            assert(goal.describe():find("progress: Phase 1/2", 1, true))
            assert(goal.describe():find("id:"))
            assert(goal.pause())
            assert(goal.current().state == "paused")
            assert(goal.status_text():find("goal paused", 1, true))
            assert(goal.block("waiting for input"))
            assert(goal.current().state == "blocked")
            assert(goal.current().auto_continue == false)
            assert(goal.current().reason == "waiting for input")
            assert(goal.status_text():find("goal blocked", 1, true))
            assert(goal.complete())
            assert(type(goal.current().completed_at_ms) == "number")
            assert(goal.status_text() == nil)
            goal.clear()
            assert(goal.current() == nil)
        "#,
    ));
}

#[test]
fn lua_goal_tools_limit_model_updates_to_done_or_blocked() {
    let mut app = TestApp::builder().build();

    assert!(app.run_lua(r#"require("smelt.goal").setup()"#));
    let tools = app.lua_probe().tool_defs(
        protocol::AgentMode::normal(),
        smelt_core::lua::ToolVisibility::Interactive,
    );
    let get = tools
        .iter()
        .find(|tool| tool.name == "get_goal")
        .expect("get_goal should be registered");
    assert_eq!(get.parameters["required"], serde_json::json!([]));

    let create = tools
        .iter()
        .find(|tool| tool.name == "create_goal")
        .expect("create_goal should be registered");
    assert!(create
        .description
        .contains("Do not infer goals from ordinary tasks"));

    let update = tools
        .iter()
        .find(|tool| tool.name == "update_goal")
        .expect("update_goal should be registered");
    assert!(update
        .description
        .contains("cannot pause, resume, clear, or rewrite"));
    assert_eq!(
        update.parameters["properties"]["state"]["enum"],
        serde_json::json!(["done", "blocked"])
    );
    assert_eq!(update.parameters["required"], serde_json::json!(["state"]));

    let status = tools
        .iter()
        .find(|tool| tool.name == "update_goal_progress")
        .expect("update_goal_progress should be registered");
    assert!(status.description.contains("existing active goal"));
    assert!(status.parameters["properties"].get("activity").is_none());
    assert!(status.parameters["properties"].get("summary").is_none());
    assert!(status.parameters["properties"]["progress"]
        .get("properties")
        .is_some());
    assert_eq!(
        status.parameters["properties"]["progress"]["required"],
        serde_json::json!([])
    );
    assert_eq!(
        status.parameters["required"],
        serde_json::json!(["progress"])
    );
}

#[test]
fn lua_goal_auto_continue_scheduled_during_turn_starts_when_idle() {
    let mut app = TestApp::builder().build();
    let _ = app.drain_engine_sends();

    assert!(app.run_lua(
        r#"
            local goal = require("smelt.goal")
            smelt.engine.has_active_turn = function() return _G.__goal_running == true end
            smelt.engine.continuation_state = function() return { token = 42, paused = false } end
            smelt.time.now_ms = function() return _G.__goal_now_ms or 0 end
            smelt.engine.submit_command_continuation = function(name, body, _overrides, display, continuation_token)
                _G.__goal_submit = { name = name, body = body, display = display, continuation_token = continuation_token }
                return continuation_token == 42
            end
            _G.__goal_running = true
            assert(goal.create("finish <the> & goal", { auto_continue = true }))
            goal.schedule_auto_continue(42)
            _G.__goal_now_ms = 1300
        "#,
    ));

    app.feed_one(SourceEvent::Tick(1300));
    app.tick_timers();
    assert!(app.run_lua(
        r#"
        assert(_G.__goal_submit == nil)
        _G.__goal_running = false
        _G.__goal_now_ms = 1600
    "#
    ));
    app.feed_one(SourceEvent::Tick(300));
    app.tick_timers();
    assert!(app.run_lua(
        r##"
            assert(_G.__goal_submit.name == "goal")
            assert(_G.__goal_submit.display == "goal continue")
            assert(_G.__goal_submit.continuation_token == 42)
            assert(_G.__goal_submit.body:find("# Continue goal", 1, true))
            assert(_G.__goal_submit.body:find("finish &lt;the&gt; &amp; goal", 1, true))
            assert(_G.__goal_submit.body:find("Call update_goal_progress when starting a meaningful phase", 1, true))
            assert(_G.__goal_submit.body:find("skip routine substeps and live activity", 1, true))
            assert(_G.__goal_submit.body:find("Preserve the original scope", 1, true))
            assert(_G.__goal_submit.body:find("Treat incomplete, indirect, weak, or missing evidence as not done", 1, true))
            assert(_G.__goal_submit.body:find("state=\"done\"", 1, true))
            assert(_G.__goal_submit.body:find("state=\"blocked\"", 1, true))
        "##,
    ));
}

#[test]
fn relative_lua_cwd_change_resolves_from_runtime_cwd() {
    let environment_guard = test_environment_guard();
    let mut app = TestApp::builder().build_with_test_environment_guard(&environment_guard);
    let target = std::path::Path::new(app.cwd_str()).join("nested-project");
    std::fs::create_dir_all(&target).expect("create nested runtime cwd");
    let expected = std::fs::canonicalize(&target).expect("canonical nested runtime cwd");

    assert!(app.run_lua(
        r#"
        local out = smelt.session.switch_cwd("nested-project")
        assert(out.pending == true)
        "#,
    ));
    assert!(app.drain_idle_work());

    assert_eq!(app.core_probe().env.cwd(), expected);
    assert_eq!(std::env::current_dir().unwrap(), expected);
    assert_eq!(
        std::env::var_os("PWD").as_deref(),
        Some(expected.as_os_str())
    );
}

#[test]
fn tilde_lua_cwd_change_resolves_from_runtime_home() {
    let environment_guard = test_environment_guard();
    let mut app = TestApp::builder().build_with_test_environment_guard(&environment_guard);
    let target = app.runtime_home().join("tilde-project");
    std::fs::create_dir_all(&target).expect("create tilde target cwd");
    let expected = std::fs::canonicalize(target).expect("canonical tilde target cwd");

    assert!(app.run_lua(
        r#"
        local out = smelt.session.switch_cwd("~/tilde-project")
        assert(out.pending == true)
        "#,
    ));
    assert!(app.drain_idle_work());

    assert_eq!(app.core_probe().env.cwd(), expected);
    assert_eq!(std::env::current_dir().unwrap(), expected);
}

#[test]
fn lua_switch_cwd_updates_runtime_state_and_engine_cwd() {
    let environment_guard = test_environment_guard();
    let target_dir = tempfile::TempDir::new().expect("create switch cwd tempdir");
    let mut app = TestApp::builder().build_with_test_environment_guard(&environment_guard);
    let target = std::fs::canonicalize(target_dir.path()).expect("canonical target cwd");
    let expected = target.to_string_lossy().into_owned();

    app.set_lua_string_global("__switch_cwd_target", expected.clone())
        .expect("set Lua target cwd");
    let _ = app.drain_engine_sends();

    assert!(app.run_lua(
        r#"
            local out = smelt.session.switch_cwd(_G.__switch_cwd_target)
            assert(out.cwd == _G.__switch_cwd_target)
            assert(out.pending == true)
            assert(smelt.session.cwd() ~= _G.__switch_cwd_target)
        "#,
    ));
    assert!(app.drain_idle_work());

    assert_eq!(app.workspace_probe().cwd(), expected);
    assert_eq!(
        app.session_snapshot().cwd.as_deref(),
        Some(expected.as_str())
    );
    assert_eq!(app.core_probe().env.cwd(), target);
    assert_eq!(std::env::current_dir().expect("process cwd"), target);
    assert_eq!(std::env::var_os("PWD").as_deref(), Some(target.as_os_str()));
    assert_eq!(
        app.core_probe().signals.get::<String>("cwd").as_deref(),
        Some(expected.as_str())
    );
    assert!(app
        .conversation_probe()
        .pending_history_appends()
        .iter()
        .any(|pending| matches!(
            pending.history_item(),
            protocol::HistoryItem::Note(protocol::HistoryNote::Context { ref text, .. })
                if text == &format!("Current working directory: {expected}.")
        )));
    assert!(app.drain_engine_sends().into_iter().any(|cmd| matches!(
        cmd,
        protocol::UiCommand::UpdateAgentProjectContext(context)
            if context.cwd.to_string_lossy() == expected
    )));

    let session_id = app.session_snapshot().id.clone();
    app.session_append_history(protocol::HistoryItem::user(protocol::Content::text(
        "persist cwd",
    )));
    app.save_session_and_flush();
    assert_eq!(
        crate::app::history::materialize_full_session(
            &app.core_probe().sessions,
            &session_id,
            crate::app::history::FullSessionMaterializationReason::TestSavedSessionAssertion,
        )
        .expect("explicit cwd change persisted")
        .cwd
        .as_deref(),
        Some(expected.as_str())
    );
}

#[test]
fn enter_worktree_tool_changes_the_process_cwd() {
    let repo = worktree_repo();
    let environment_guard = test_environment_guard();

    let mut app = TestApp::builder()
        .with_cwd(repo.path())
        .build_with_test_environment_guard(&environment_guard);
    let mut settings = app.core_probe().config.settings.clone();
    settings.worktree_root = ".worktrees".into();
    app.set_settings_for_harness(settings);
    app.start_turn(42);
    let _ = app.drain_engine_sends();
    app.clear_actions();

    app.feed_one(SourceEvent::engine(protocol::EngineEvent::ToolDispatch {
        invocation_id: protocol::InvocationId::new(96),
        request_id: 96,
        call_id: "enter-worktree".into(),
        tool_name: "enter_worktree".into(),
        args: std::collections::HashMap::from([(
            "name".into(),
            serde_json::Value::String("Tool Cwd".into()),
        )]),
    }));

    let target = std::fs::canonicalize(repo.path().join(".worktrees/tool-cwd")).unwrap();
    let expected = target.to_string_lossy();
    assert_eq!(app.workspace_probe().cwd(), expected);
    assert_eq!(app.core_probe().env.cwd(), target);
    assert_eq!(std::env::current_dir().unwrap(), target);
    assert_eq!(std::env::var_os("PWD").as_deref(), Some(target.as_os_str()));
    assert!(!app.workspace_probe().has_pending_change());
    assert!(app.actions().iter().any(|action| matches!(
        action,
        Action::EngineSend(command)
            if matches!(
                command.as_ref(),
                protocol::UiCommand::ToolResult {
                    request_id: 96,
                    call_id,
                    is_error: false,
                    metadata: Some(metadata),
                    ..
                } if call_id == "enter-worktree"
                    && metadata["path"] == expected.as_ref()
                    && metadata["cwd_committed"] == true
            )
    )));
}

#[test]
fn switch_cwd_tool_commits_project_context_before_releasing_its_result() {
    let environment_guard = test_environment_guard();
    let target_dir = tempfile::TempDir::new().expect("create tool cwd tempdir");
    let target = std::fs::canonicalize(target_dir.path()).unwrap();
    let expected = target.to_string_lossy().into_owned();
    let smelt_dir = target.join(".smelt");
    std::fs::create_dir_all(&smelt_dir).unwrap();
    std::fs::write(target.join("AGENTS.md"), "target cwd instructions").unwrap();
    std::fs::write(
        smelt_dir.join("init.lua"),
        r#"
            smelt.agent.add_system_prompt("target cwd prompt fragment")
            smelt.tools.register({
                name = "target_cwd_probe",
                description = "target-only tool",
                parameters = { type = "object", properties = {} },
                execute = function() return "target" end,
            })
        "#,
    )
    .unwrap();

    let mut app = TestApp::builder().build_with_test_environment_guard(&environment_guard);
    app.mark_project_trusted(&target).unwrap();
    app.start_turn(42);
    let original_generation = app.lua_probe().id;
    let original_permissions = app.active_permissions();
    let _ = app.drain_engine_sends();
    app.clear_actions();

    app.feed_one(SourceEvent::engine(protocol::EngineEvent::ToolDispatch {
        invocation_id: protocol::InvocationId::new(91),
        request_id: 91,
        call_id: "switch-cwd".into(),
        tool_name: "switch_cwd".into(),
        args: std::collections::HashMap::from([(
            "path".into(),
            serde_json::Value::String(expected.clone()),
        )]),
    }));

    assert_eq!(app.workspace_probe().cwd(), expected);
    assert_eq!(app.core_probe().env.cwd(), target);
    assert_eq!(std::env::current_dir().unwrap(), target);
    assert_eq!(std::env::var_os("PWD").as_deref(), Some(target.as_os_str()));
    assert_eq!(app.lua_probe().id, original_generation.wrapping_add(1));
    assert!(!app.workspace_probe().has_pending_change());
    assert!(app
        .lua_probe()
        .tool_defs(
            app.core_probe().config.mode.clone(),
            smelt_core::lua::ToolVisibility::Interactive,
        )
        .iter()
        .any(|tool| tool.name == "target_cwd_probe"));

    let active_permissions = app.active_permissions();
    assert!(!std::sync::Arc::ptr_eq(
        &active_permissions,
        &original_permissions
    ));
    assert!(std::sync::Arc::ptr_eq(
        &active_permissions,
        &app.core_probe().permissions.snapshot()
    ));

    let commands: Vec<_> = app
        .actions()
        .iter()
        .filter_map(|action| match action {
            Action::EngineSend(command) => Some(command.as_ref()),
            _ => None,
        })
        .collect();
    let context_index = commands
        .iter()
        .position(|command| matches!(
            command,
            protocol::UiCommand::UpdateAgentProjectContext(context)
                if context.cwd == target
                    && context.instructions.as_deref().is_some_and(|text| text.contains("target cwd instructions"))
                    && context.system_prompt.contains("target cwd prompt fragment")
                    && context.tools.iter().any(|tool| tool.name == "target_cwd_probe")
        ))
        .expect("committed project context command");
    let result_index = commands
        .iter()
        .position(|command| {
            matches!(
                command,
                protocol::UiCommand::ToolResult {
                    request_id: 91,
                    call_id,
                    is_error: false,
                    metadata: Some(metadata),
                    ..
                } if call_id == "switch-cwd"
                    && metadata["pending"] == false
                    && metadata["cwd_committed"] == true
            )
        })
        .expect("successful switch_cwd tool result");
    assert!(context_index < result_index);
}

#[test]
fn concurrent_model_tool_cannot_commit_a_cwd_change() {
    let environment_guard = test_environment_guard();
    let target_dir = tempfile::TempDir::new().expect("create concurrent tool cwd tempdir");
    let target = std::fs::canonicalize(target_dir.path()).unwrap();
    let mut app = TestApp::builder().build_with_test_environment_guard(&environment_guard);
    let original_cwd = app.workspace_probe().cwd().to_owned();
    let original_process_cwd = std::env::current_dir().unwrap();
    assert!(app.run_lua(
        r#"
            smelt.tools.register({
                name = "concurrent_cwd_probe",
                description = "attempt a concurrent cwd transition",
                parameters = {
                    type = "object",
                    properties = { path = { type = "string" } },
                    required = { "path" },
                },
                execute = function(args)
                    return smelt.session.switch_cwd(args.path)
                end,
            })
        "#,
    ));
    app.start_turn(42);
    let _ = app.drain_engine_sends();
    app.clear_actions();

    app.feed_one(SourceEvent::engine(protocol::EngineEvent::ToolDispatch {
        invocation_id: protocol::InvocationId::new(93),
        request_id: 93,
        call_id: "concurrent-cwd".into(),
        tool_name: "concurrent_cwd_probe".into(),
        args: std::collections::HashMap::from([(
            "path".into(),
            serde_json::Value::String(target.to_string_lossy().into_owned()),
        )]),
    }));

    assert_eq!(app.workspace_probe().cwd(), original_cwd);
    assert_eq!(std::env::current_dir().unwrap(), original_process_cwd);
    assert!(!app.workspace_probe().has_pending_change());
    assert!(app.actions().iter().any(|action| matches!(
        action,
        Action::EngineSend(command)
            if matches!(
                command.as_ref(),
                protocol::UiCommand::ToolResult {
                    request_id: 93,
                    call_id,
                    content,
                    is_error: true,
                    metadata: None,
                    ..
                } if call_id == "concurrent-cwd"
                    && content == "cwd-changing model tools must use sequential execution"
            )
    )));
}

#[test]
fn cancelled_sequential_tool_discards_its_pending_cwd_change() {
    let environment_guard = test_environment_guard();
    let target_dir = tempfile::TempDir::new().expect("create cancelled tool cwd tempdir");
    let target = std::fs::canonicalize(target_dir.path()).unwrap();
    let mut app = TestApp::builder().build_with_test_environment_guard(&environment_guard);
    let original_cwd = app.workspace_probe().cwd().to_owned();
    assert!(app.run_lua(
        r#"
            smelt.tools.register({
                name = "cancelled_cwd_probe",
                description = "request a cwd transition before sleeping",
                execution_mode = "sequential",
                parameters = {
                    type = "object",
                    properties = { path = { type = "string" } },
                    required = { "path" },
                },
                execute = function(args)
                    smelt.session.switch_cwd(args.path)
                    smelt.sleep(60000)
                    return "unexpected completion"
                end,
            })
        "#,
    ));
    app.start_turn(42);

    app.feed_one(SourceEvent::engine(protocol::EngineEvent::ToolDispatch {
        invocation_id: protocol::InvocationId::new(94),
        request_id: 94,
        call_id: "cancelled-cwd".into(),
        tool_name: "cancelled_cwd_probe".into(),
        args: std::collections::HashMap::from([(
            "path".into(),
            serde_json::Value::String(target.to_string_lossy().into_owned()),
        )]),
    }));
    assert!(app.workspace_probe().has_pending_change());

    app.discard_turn(crate::app::TurnEnd::Cancelled);
    assert_eq!(app.workspace_probe().cwd(), original_cwd);
    assert!(!app.workspace_probe().has_pending_change());
    app.drain_idle_work();
    assert_eq!(app.workspace_probe().cwd(), original_cwd);
}

#[test]
fn direct_cwd_request_is_not_committed_by_unrelated_tool_completion() {
    let environment_guard = test_environment_guard();
    let target_dir = tempfile::TempDir::new().expect("create deferred cwd tempdir");
    let target = std::fs::canonicalize(target_dir.path()).unwrap();
    let expected = target.to_string_lossy().into_owned();
    let expected_lua = serde_json::to_string(&expected).unwrap();
    let marker = target.join("candidate-context.txt");
    let marker_lua = serde_json::to_string(&marker.to_string_lossy()).unwrap();
    std::fs::write(&marker, "target project").unwrap();
    let smelt_dir = target.join(".smelt");
    std::fs::create_dir_all(smelt_dir.join("commands")).unwrap();
    std::fs::write(
        smelt_dir.join("commands/target-context.md"),
        "---\ndescription: Target cwd skill\nagent_skill: true\n---\n\nTarget cwd content.",
    )
    .unwrap();
    std::fs::write(
        smelt_dir.join("init.lua"),
        format!(
            r#"
            assert(smelt.os.cwd() == {expected_lua})
            assert(smelt.session.cwd() == {expected_lua})
            assert(smelt.trust.status() == "trusted")
            assert(smelt.fs.read("candidate-context.txt") == "target project")
            assert(smelt.path.canonical("candidate-context.txt") == {marker_lua})
            assert(smelt.files.status().root == {expected_lua})
            local file = assert(io.open("candidate-context.txt", "r"))
            assert(file:read("*a") == "target project")
            file:close()
            local skill = smelt.skills.content("target-context")
            assert(skill and skill:find("Target cwd content", 1, true))
            smelt.cmd.register("deferred_cwd_project", function() end)
            "#
        ),
    )
    .unwrap();
    let mut app = TestApp::builder().build_with_test_environment_guard(&environment_guard);
    app.mark_project_trusted(&target).unwrap();
    let original_cwd = app.workspace_probe().cwd().to_owned();
    let original_generation = app.lua_probe().id;
    app.set_lua_string_global("__deferred_cwd_target", expected.clone())
        .unwrap();
    let _ = app.drain_engine_sends();
    app.start_turn(42);

    assert!(app.run_lua(
        r#"
            local out = smelt.session.switch_cwd(_G.__deferred_cwd_target)
            assert(out.cwd == _G.__deferred_cwd_target)
            assert(out.pending == true)
            assert(smelt.session.cwd() ~= _G.__deferred_cwd_target)
        "#,
    ));
    assert_eq!(app.workspace_probe().cwd(), original_cwd);
    assert_eq!(app.lua_probe().id, original_generation);
    assert!(app.workspace_probe().has_pending_change());
    assert!(!app
        .drain_engine_sends()
        .into_iter()
        .any(|cmd| matches!(cmd, protocol::UiCommand::UpdateAgentProjectContext(_))));

    app.complete_lua_tool(
        smelt_core::lua::ToolInvocationContext {
            invocation_id: protocol::InvocationId::new(99),
            request_id: 99,
            execution_mode: protocol::ToolExecutionMode::Concurrent,
        },
        "unrelated-tool".into(),
        "unrelated result".into(),
        false,
        None,
    );
    assert_eq!(app.workspace_probe().cwd(), original_cwd);
    assert_eq!(app.lua_probe().id, original_generation);
    assert!(app.workspace_probe().has_pending_change());
    let commands = app.drain_engine_sends();
    assert!(!commands
        .iter()
        .any(|cmd| matches!(cmd, protocol::UiCommand::UpdateAgentProjectContext(_))));
    assert!(commands.iter().any(|cmd| matches!(
        cmd,
        protocol::UiCommand::ToolResult {
            request_id: 99,
            call_id,
            content,
            is_error: false,
            ..
        } if call_id == "unrelated-tool" && content == "unrelated result"
    )));

    app.discard_turn(crate::app::TurnEnd::Complete);
    assert!(app.drain_idle_work());

    assert_eq!(app.workspace_probe().cwd(), expected);
    assert_eq!(app.core_probe().env.cwd(), target);
    assert_eq!(std::env::current_dir().unwrap(), target);
    assert_eq!(std::env::var_os("PWD").as_deref(), Some(target.as_os_str()));
    assert_eq!(app.lua_probe().id, original_generation.wrapping_add(1));
    assert!(!app.workspace_probe().has_pending_change());
    assert!(app
        .lua_probe()
        .command_names_handle()
        .lock()
        .unwrap()
        .contains("deferred_cwd_project"));
    assert!(app.drain_engine_sends().into_iter().any(|cmd| matches!(
        cmd,
        protocol::UiCommand::UpdateAgentProjectContext(context)
            if context.cwd.to_string_lossy() == expected
    )));
}

#[test]
fn failed_switch_cwd_tool_reports_error_without_publishing_partial_context() {
    let environment_guard = test_environment_guard();
    let target_dir = tempfile::TempDir::new().expect("create failed tool cwd tempdir");
    let smelt_dir = target_dir.path().join(".smelt");
    std::fs::create_dir_all(&smelt_dir).unwrap();
    std::fs::write(smelt_dir.join("init.lua"), "this is invalid target Lua @@@").unwrap();

    let mut app = TestApp::builder().build_with_test_environment_guard(&environment_guard);
    app.mark_project_trusted(target_dir.path()).unwrap();
    app.start_turn(42);
    let target = std::fs::canonicalize(target_dir.path()).unwrap();
    let original_cwd = app.workspace_probe().cwd().to_owned();
    let original_runtime_cwd = app.core_probe().env.cwd();
    let original_process_cwd = std::env::current_dir().unwrap();
    let original_generation = app.lua_probe().id;
    let original_permissions = app.active_permissions();
    let _ = app.drain_engine_sends();
    app.clear_actions();

    app.feed_one(SourceEvent::engine(protocol::EngineEvent::ToolDispatch {
        invocation_id: protocol::InvocationId::new(92),
        request_id: 92,
        call_id: "failed-switch-cwd".into(),
        tool_name: "switch_cwd".into(),
        args: std::collections::HashMap::from([(
            "path".into(),
            serde_json::Value::String(target.to_string_lossy().into_owned()),
        )]),
    }));

    assert_eq!(app.workspace_probe().cwd(), original_cwd);
    assert_eq!(app.core_probe().env.cwd(), original_runtime_cwd);
    assert_eq!(std::env::current_dir().unwrap(), original_process_cwd);
    assert_eq!(app.lua_probe().id, original_generation);
    assert!(!app.workspace_probe().has_pending_change());
    assert!(std::sync::Arc::ptr_eq(
        &app.active_permissions(),
        &original_permissions
    ));

    let commands: Vec<_> = app
        .actions()
        .iter()
        .filter_map(|action| match action {
            Action::EngineSend(command) => Some(command.as_ref()),
            _ => None,
        })
        .collect();
    assert!(!commands
        .iter()
        .any(|command| matches!(command, protocol::UiCommand::UpdateAgentProjectContext(_))));
    assert!(commands.iter().any(|command| matches!(
        command,
        protocol::UiCommand::ToolResult {
            request_id: 92,
            call_id,
            content,
            is_error: true,
            metadata: Some(metadata),
            ..
        } if call_id == "failed-switch-cwd"
            && content.starts_with("cwd change:")
            && content.contains("Original tool result:\ncwd:")
            && metadata["pending"] == false
            && metadata["cwd_committed"] == false
            && metadata["cwd_error"].as_str().is_some_and(|error| error.starts_with("cwd change:"))
    )));
}

#[test]
fn failed_worktree_cwd_commit_preserves_creation_result_metadata() {
    let repo = worktree_repo();
    let environment_guard = test_environment_guard();

    let mut app = TestApp::builder()
        .with_cwd(repo.path())
        .build_with_test_environment_guard(&environment_guard);
    let mut settings = app.core_probe().config.settings.clone();
    settings.worktree_root = ".worktrees".into();
    app.set_settings_for_harness(settings);
    let original_cwd = app.workspace_probe().cwd().to_owned();
    let original_process_cwd = std::env::current_dir().unwrap();
    let invalid_init = "this is invalid target Lua @@@";
    let project_config = repo.path().join(".smelt");
    std::fs::create_dir_all(&project_config).unwrap();
    std::fs::write(project_config.join("init.lua"), invalid_init).unwrap();
    git(repo.path(), &["add", ".smelt/init.lua"]);
    git(repo.path(), &["commit", "-m", "add target config"]);

    let target = repo.path().join(".worktrees").join("broken-context");
    std::fs::create_dir_all(target.join(".smelt")).unwrap();
    std::fs::write(target.join(".smelt/init.lua"), invalid_init).unwrap();
    app.mark_project_trusted(&target).unwrap();
    std::fs::remove_dir_all(&target).unwrap();

    app.start_turn(42);
    let _ = app.drain_engine_sends();
    app.clear_actions();
    app.feed_one(SourceEvent::engine(protocol::EngineEvent::ToolDispatch {
        invocation_id: protocol::InvocationId::new(95),
        request_id: 95,
        call_id: "enter-broken-worktree".into(),
        tool_name: "enter_worktree".into(),
        args: std::collections::HashMap::from([(
            "name".into(),
            serde_json::Value::String("broken-context".into()),
        )]),
    }));

    assert_eq!(app.workspace_probe().cwd(), original_cwd);
    assert_eq!(std::env::current_dir().unwrap(), original_process_cwd);
    assert!(
        target.is_dir(),
        "worktree creation side effect should remain"
    );
    assert!(!app.workspace_probe().has_pending_change());
    assert!(!app.actions().iter().any(|action| matches!(
        action,
        Action::EngineSend(command)
            if matches!(command.as_ref(), protocol::UiCommand::UpdateAgentProjectContext(_))
    )));

    let result = app
        .actions()
        .iter()
        .find_map(|action| match action {
            Action::EngineSend(command) => match command.as_ref() {
                protocol::UiCommand::ToolResult {
                    request_id: 95,
                    call_id,
                    content,
                    is_error,
                    metadata,
                    ..
                } if call_id == "enter-broken-worktree" => {
                    Some((content, *is_error, metadata.as_ref()))
                }
                _ => None,
            },
            _ => None,
        })
        .expect("failed enter_worktree result");
    assert!(result.1);
    assert!(result.0.starts_with("cwd change:"), "{}", result.0);
    assert!(
        result
            .0
            .contains("Original tool result:\nentered managed worktree broken-context"),
        "{}",
        result.0
    );
    let metadata = result
        .2
        .expect("worktree metadata should survive cwd failure");
    assert_eq!(metadata["name"], "broken-context");
    assert_eq!(metadata["branch"], "broken-context");
    assert_eq!(metadata["base"], "main");
    assert_eq!(metadata["path"], target.to_string_lossy().as_ref());
    assert_eq!(metadata["created"], true);
    assert_eq!(metadata["pending"], false);
    assert_eq!(metadata["cwd_committed"], false);
    assert!(metadata["cwd_error"]
        .as_str()
        .is_some_and(|error| error.starts_with("cwd change:")));
}

#[test]
fn cwd_change_replaces_workspace_but_preserves_application_notification() {
    let environment_guard = test_environment_guard();
    let first_target = tempfile::TempDir::new().expect("create first cwd tempdir");
    let second_target = tempfile::TempDir::new().expect("create second cwd tempdir");
    let mut app = TestApp::builder().build_with_test_environment_guard(&environment_guard);

    app.notify_workspace_error_sticky("old workspace failure".into());
    app.set_lua_string_global(
        "__first_cwd_target",
        first_target.path().to_string_lossy().into_owned(),
    )
    .unwrap();
    assert!(app.run_lua("assert(smelt.session.switch_cwd(_G.__first_cwd_target).pending == true)"));
    assert!(app.drain_idle_work());
    assert!(app.overlays_probe().notification().is_none());

    app.notify_application_error_sticky("application failure".into());
    app.set_lua_string_global(
        "__second_cwd_target",
        second_target.path().to_string_lossy().into_owned(),
    )
    .unwrap();
    assert!(app.run_lua("assert(smelt.session.switch_cwd(_G.__second_cwd_target).pending == true)"));
    assert!(app.drain_idle_work());
    assert!(app
        .overlays_probe()
        .notification()
        .is_some_and(|notification| {
            matches!(
                &notification.scope,
                crate::app::NotificationScope::Application
            ) && notification.summary == "application failure"
        }));
}

#[test]
fn failed_cwd_candidate_preserves_the_complete_project_context() {
    let environment_guard = test_environment_guard();
    let target_dir = tempfile::TempDir::new().expect("create failed cwd tempdir");
    let smelt_dir = target_dir.path().join(".smelt");
    std::fs::create_dir_all(&smelt_dir).unwrap();
    let init = smelt_dir.join("init.lua");
    std::fs::write(&init, "this is invalid target Lua @@@").unwrap();
    let mut app = TestApp::builder().build_with_test_environment_guard(&environment_guard);
    app.mark_project_trusted(target_dir.path()).unwrap();
    let original_cwd = app.workspace_probe().cwd().to_owned();
    let original_runtime_cwd = app.core_probe().env.cwd();
    let original_process_cwd = std::env::current_dir().unwrap();
    let original_generation = app.lua_probe().id;
    let target = std::fs::canonicalize(target_dir.path()).unwrap();
    let expected = target.to_string_lossy().into_owned();
    app.set_lua_string_global("__failed_cwd_target", expected.clone())
        .unwrap();

    assert!(app.run_lua("assert(smelt.session.switch_cwd(_G.__failed_cwd_target).pending == true)"));
    assert!(app.drain_idle_work());
    assert_eq!(app.workspace_probe().cwd(), original_cwd);
    assert_eq!(app.core_probe().env.cwd(), original_runtime_cwd);
    assert_eq!(app.lua_probe().id, original_generation);
    assert_eq!(std::env::current_dir().unwrap(), original_process_cwd);
    assert!(app
        .overlays_probe()
        .notification()
        .is_some_and(|notification| {
            matches!(
                &notification.scope,
                crate::app::NotificationScope::Operation(
                    crate::app::NotificationOperation::CwdChange
                )
            )
        }));

    std::fs::write(
        &init,
        r#"smelt.cmd.register("recovered_cwd_project", function() end)"#,
    )
    .unwrap();
    app.mark_project_trusted(target_dir.path()).unwrap();
    assert!(app.run_lua("assert(smelt.session.switch_cwd(_G.__failed_cwd_target).pending == true)"));
    assert!(app.drain_idle_work());
    assert_eq!(app.workspace_probe().cwd(), expected);
    assert_eq!(app.core_probe().env.cwd(), target);
    assert_eq!(std::env::current_dir().unwrap(), target);
    assert_eq!(std::env::var_os("PWD").as_deref(), Some(target.as_os_str()));
    assert_eq!(app.lua_probe().id, original_generation.wrapping_add(1));
    assert!(!app
        .overlays_probe()
        .notification()
        .is_some_and(|notification| {
            notification.summary.starts_with("cwd change:")
                || notification.summary.starts_with("session cwd unavailable:")
        }));
    assert!(app
        .lua_probe()
        .command_names_handle()
        .lock()
        .unwrap()
        .contains("recovered_cwd_project"));
}

#[test]
fn loading_session_restores_persisted_cwd() {
    let environment_guard = test_environment_guard();
    let target_dir = tempfile::TempDir::new().expect("create resumed cwd tempdir");
    let mut app = TestApp::builder().build_with_test_environment_guard(&environment_guard);
    let target = std::fs::canonicalize(target_dir.path()).expect("canonical target cwd");
    let expected = target.to_string_lossy().into_owned();
    let _ = app.drain_engine_sends();

    let mut session = smelt_core::session::Session::new(app.core_probe().env.pid(), target.clone());
    session
        .history
        .push(protocol::HistoryItem::user(protocol::Content::text(
            "hello",
        )));

    app.load_session(session);
    assert!(app.drain_idle_work());

    assert_eq!(app.workspace_probe().cwd(), expected);
    assert_eq!(
        app.session_snapshot().cwd.as_deref(),
        Some(expected.as_str())
    );
    assert_eq!(app.core_probe().env.cwd(), target);
    assert_eq!(std::env::current_dir().expect("process cwd"), target);
    assert_eq!(std::env::var_os("PWD").as_deref(), Some(target.as_os_str()));
    assert_eq!(
        app.core_probe().signals.get::<String>("cwd").as_deref(),
        Some(expected.as_str())
    );
    assert!(!app.conversation_probe().has_live_session());
    assert!(app.drain_engine_sends().into_iter().any(|cmd| matches!(
        cmd,
        protocol::UiCommand::UpdateAgentProjectContext(context)
            if context.cwd.to_string_lossy() == expected
    )));

    let display_target_dir = tempfile::TempDir::new().expect("create display-only cwd tempdir");
    let display_target =
        std::fs::canonicalize(display_target_dir.path()).expect("canonical display-only cwd");
    let display_expected = display_target.to_string_lossy().into_owned();
    let display_session =
        smelt_core::session::Session::new(app.core_probe().env.pid(), display_target.clone());
    let display_session_id = display_session.id.clone();
    let transcript = smelt_core::content::transcript::Transcript::new();

    app.load_store_backed_session(
        crate::app::session_document::StoreBackedSessionDocument::new(
            display_session,
            crate::app::transcript::LoadedTranscript::full(transcript),
            crate::app::history::live_session_for_test(display_session_id.clone(), 0, None),
            smelt_store::StoreHead::default(),
        ),
    );
    assert!(app.drain_idle_work());

    assert_eq!(app.workspace_probe().cwd(), display_expected);
    assert_eq!(
        app.session_snapshot().cwd.as_deref(),
        Some(display_expected.as_str())
    );
    assert_eq!(app.core_probe().env.cwd(), display_target);
    assert_eq!(std::env::current_dir().unwrap(), display_target);
    assert_eq!(
        std::env::var_os("PWD").as_deref(),
        Some(display_target.as_os_str())
    );
    assert_eq!(
        app.conversation_probe()
            .live_session()
            .map(|live| live.id()),
        Some(display_session_id.as_str())
    );
    assert!(app.drain_engine_sends().into_iter().any(|cmd| matches!(
        cmd,
        protocol::UiCommand::UpdateAgentProjectContext(context)
            if context.cwd.to_string_lossy() == display_expected
    )));

    let missing_dir = tempfile::TempDir::new().expect("create missing cwd tempdir");
    let missing_path = missing_dir.path().join("gone");
    std::fs::create_dir(&missing_path).expect("create missing cwd before deletion");
    let missing_path = std::fs::canonicalize(&missing_path).expect("canonical missing cwd");
    drop(missing_dir);
    let fallback = app.workspace_probe().cwd().to_owned();
    let fallback_path = app.core_probe().env.cwd();
    let missing_cwd = missing_path.to_string_lossy().into_owned();
    let missing_session =
        smelt_core::session::Session::new(app.core_probe().env.pid(), missing_path.clone());

    app.load_session(missing_session);

    assert_eq!(app.workspace_probe().cwd(), fallback);
    assert_eq!(
        app.session_snapshot().cwd.as_deref(),
        Some(missing_cwd.as_str()),
        "runtime fallback must not rewrite canonical session metadata"
    );
    assert_eq!(app.core_probe().env.cwd(), fallback_path);
    assert_eq!(std::env::current_dir().unwrap(), fallback_path);
    assert_eq!(
        std::env::var_os("PWD").as_deref(),
        Some(fallback_path.as_os_str())
    );
    assert!(app
        .overlays_probe()
        .notification()
        .is_some_and(|notification| {
            notification.summary.contains("session cwd unavailable")
                && matches!(
                    &notification.scope,
                    crate::app::NotificationScope::Operation(
                        crate::app::NotificationOperation::CwdChange
                    )
                )
        }));

    app.set_lua_string_global("__fallback_cwd", fallback)
        .unwrap();
    assert!(app.run_lua("assert(smelt.session.switch_cwd(_G.__fallback_cwd).pending == true)"));
    assert!(app.drain_idle_work());
    assert!(app.overlays_probe().notification().is_none());
}

#[test]
fn lua_session_context_tokens_stays_visible_while_turn_history_is_ahead_of_baseline() {
    let mut app = TestApp::builder().build();

    assert!(app.run_lua("assert(smelt.session.context_tokens() == nil)"));

    let completed_history = vec![
        protocol::HistoryItem::user(protocol::Content::text("u1")),
        protocol::HistoryItem::assistant(protocol::AssistantStep::terminal(
            Some(protocol::Content::text("a1")),
            None,
            vec![],
        )),
    ];

    app.start_turn(1);
    app.feed_one(SourceEvent::engine(EngineEvent::TokenUsage {
        usage: protocol::TokenUsage {
            context_tokens: Some(123),
            ..Default::default()
        },
        tokens_per_sec: None,
        cost_usd: None,
        background: false,
    }));
    app.feed_one(SourceEvent::engine(EngineEvent::TurnComplete {
        turn_id: 1,
        history: Some(protocol::CanonicalHistoryDelta::new(
            0,
            completed_history.clone(),
        )),
        meta: None,
    }));
    assert_eq!(app.session_snapshot().current_context_tokens(), Some(123));
    assert!(app.run_lua("assert(smelt.session.context_tokens() == 123)"));

    let mut in_flight_history = completed_history;
    in_flight_history.push(protocol::HistoryItem::user(protocol::Content::text("u2")));
    app.start_turn(2);
    app.feed_one(SourceEvent::engine(EngineEvent::HistoryUpdated {
        turn_id: 2,
        update: protocol::CanonicalHistoryDelta::new(0, in_flight_history),
    }));

    assert_eq!(app.session_snapshot().current_context_tokens(), None);
    assert_eq!(app.session_snapshot().display_context_tokens(), Some(123));
    assert!(app.run_lua("assert(smelt.session.context_tokens() == 123)"));
}

#[test]
fn lua_history_entries_and_search_return_sequences() {
    let mut app = TestApp::builder().build();
    app.push_history_entry("first prompt".into());
    app.push_history_entry("older bun prompt".into());
    app.push_history_entry("newer bun prompt".into());
    app.push_history_entry("second search target".into());

    assert!(app.run_lua(
        r#"
            local entries = smelt.history.entries()
            assert(#entries >= 2)
            assert(entries[#entries] == "second search target")
            local matches = smelt.history.search("target")
            assert(#matches >= 1)
            assert(type(matches[1].index) == "number")
            assert(type(matches[1].score) == "number")
            local bun_matches = smelt.history.search("bun")
            assert(#bun_matches >= 2)
            assert(entries[bun_matches[1].index] == "newer bun prompt")
        "#,
    ));
}

#[test]
fn lua_model_settings_metrics_and_render_contracts_are_available() {
    let mut app = TestApp::builder().build();

    assert!(app.run_lua(
        r##"
            local model = smelt.model.current()
            assert(type(model) == "string")
            local models = smelt.model.list()
            assert(type(models) == "table")
            if #models > 0 then
                assert(type(models[1].key) == "string")
                assert(type(models[1].name) == "string")
                assert(type(models[1].provider_type) == "string")
            end
            local pricing = smelt.model.pricing()
            assert(type(pricing.input) == "number")
            assert(type(pricing.output) == "number")
            assert(type(pricing.cache_read) == "number")
            assert(type(pricing.cache_write) == "number")
            assert(type(pricing.source) == "string")
            local capabilities = smelt.model.capabilities()
            assert(type(capabilities) == "table")
            assert(capabilities.max_tokens == nil or type(capabilities.max_tokens) == "number")
            assert(smelt.model.preferred("coverage") == nil)
            assert(smelt.model.preferred("coverage", "test-model") == "test-model")
            assert(smelt.model.preferred("coverage") == "test-model")
            assert(smelt.model.preferred("coverage", nil) == nil)
            assert(smelt.model.preferred("coverage") == nil)

            local original = smelt.settings.show_slug
            assert(type(original) == "boolean")
            smelt.settings.show_slug = not original
            assert(smelt.settings.show_slug == not original)
            smelt.settings.show_slug = original
            assert(type(smelt.settings.notifications) == "table")
            assert(smelt.settings.notifications.turn_end == false)
            assert(smelt.settings.notifications.method == nil)
            smelt.settings.notifications = { turn_end = false }
            smelt.settings.notifications.turn_end = true
            assert(smelt.settings.notifications.turn_end == true)
            smelt.settings.notifications = { turn_end = false }
            assert(smelt.settings.notifications.turn_end == false)
            local seen_notifications = false
            for key in pairs(smelt.settings) do
              if key == "notifications" then seen_notifications = true end
            end
            assert(seen_notifications)
            local schema = smelt.settings.schema()
            assert(#schema > 0)
            assert(type(schema[1].key) == "string")
            assert(type(schema[1].kind) == "string")
            assert(not pcall(function() return smelt.settings.not_a_real_setting end))
            assert(not pcall(function() smelt.settings.show_slug = "bad" end))

            smelt.metrics.perf.clear()
            smelt.metrics.perf.set_enabled(true)
            local perf = smelt.metrics.perf.snapshot()
            assert(perf.enabled == true)
            assert(type(perf.durations) == "table")
            assert(type(perf.values) == "table")
            smelt.metrics.perf.set_enabled(false)
            assert(smelt.metrics.perf.snapshot().enabled == false)
            assert(type(smelt.metrics.entries()) == "table")

            local b = smelt.buf.new({ name = "coverage.render.text" })
            smelt.render.text(b, "alpha\nbeta", { width = 20, hl_group = "Normal" })
            assert(b:line(1) == "alpha")
            assert(b:line(2) == "beta")

            local md = smelt.buf.new({ name = "coverage.render.markdown" })
            smelt.render.markdown(md, "# Title")
            assert(type(md:line(1)) == "string")

            local code = smelt.buf.new({ name = "coverage.render.syntax" })
            smelt.render.syntax(code, { content = "fn main() {}", lang = "rust" })
            assert(type(code:line(1)) == "string")

            local left = smelt.buf.new({ name = "coverage.render.diff.left" })
            local right = smelt.buf.new({ name = "coverage.render.diff.right" })
            smelt.render.diff_split(left, right, { old = "one\ntwo", new = "one\nthree", lang = "text" })
            assert(left:line(1) ~= nil)
            assert(right:line(1) ~= nil)
        "##,
    ));
}

#[test]
fn lua_buf_win_overlay_contracts_are_available() {
    let mut app = TestApp::builder().build();

    app.run_lua_result(
        r##"
            local ns = smelt.ns("coverage.buf_win_overlay")
            assert(type(ns) == "number", "ns returns number")

            local buf = smelt.buf.new({ name = "coverage.contract.buf", readonly = true, editable = true })
            assert(tostring(buf):find("Buf#", 1, true) == 1, "buf tostring")
            assert(buf:readonly() == true, "initial readonly")
            assert(buf:readonly(false) == buf, "readonly setter chain")
            assert(buf:readonly() == false, "readonly setter value")
            assert(buf:source("alpha\nβeta") == buf, "source setter chain")
            assert(buf:source() == "alpha\nβeta", "source getter")
            assert(buf:lines({ "one", "two" }) == buf, "lines setter chain")
            local lines = buf:lines()
            assert(#lines == 2, "line count")
            assert(lines[1] == "one", "first line")
            assert(lines[2] == "two", "second line")
            assert(buf:line(1) == "one", "line getter")
            assert(buf:line(0) == nil, "line zero nil")
            local mark_id = buf:mark(ns, 1, 1, { end_row = 1, end_col = 3, hl_group = "Normal" })
            assert(type(mark_id) == "number", "mark id")
            assert(buf:clear_ns(ns) == buf, "clear ns chain")
            assert(buf:styled({
                { { text = "styled", style = { bold = true, fg = { 255, 0, 0 } } } },
            }) == buf, "styled chain")
            assert(type(buf:line(1)) == "string", "styled line")

            local win = smelt.win.new(buf, {
                name = "coverage.contract.win",
                surface = "selectable_text",
                wrap = false,
                scrollbar = false,
            })
            assert(tostring(win):find("Win#", 1, true) == 1)
            assert(win:buf():source() == buf:source())
            assert(win:cursor(1) == win)
            assert(type(win:cursor()) == "number")
            assert(win:move_cursor(-1) == win)
            assert(win:placeholder("ghost", { accept_keys = { "tab" }, dismiss_keys = { "esc" } }) == win)
            assert(win:placeholder_text() == "ghost")
            win:clear_placeholder()
            assert(win:placeholder_text() == nil)
            assert(type(win:scroll()) == "table")
            assert(win:scroll(0) == win)
            assert(win:scroll("tail") == win)
            local key_win = smelt.prompt.win()
            local win_reg = key_win:key("x", function() error("removed keymap fired") end)
            assert(win_reg:remove() == true)
            _G.coverage_replacement_hits = 0
            _G.coverage_replacement_reg = key_win:key("x", function()
                _G.coverage_replacement_hits = _G.coverage_replacement_hits + 1
            end)
            key_win:focus()

            local measure = smelt.ui.layout.measure(12, 3)
            local mw, mh = measure:get()
            assert(mw == 12 and mh == 3)
            measure:set(14, 4)
            mw, mh = measure:get()
            assert(mw == 14 and mh == 4)

            local layout = smelt.ui.layout.vbox({
                { smelt.ui.layout.leaf(win, { measure = measure, title = "Leaf" }), height = "fill" },
            }, { gap = 0, title = "VBox" })
            local overlay = smelt.overlay.new({
                name = "coverage.contract.overlay",
                title = "Coverage Overlay",
                anchor = "screen_at",
                corner = "nw",
                row = 0, col = 0,
                width = 30, height = 8,
                layout = layout,
            })
            assert(tostring(overlay):find("Overlay#", 1, true) == 1)
            local overlay_reg = overlay:key("y", function() end)
            assert(overlay_reg:remove() == true)
        "##,
    )
    .expect("lua buf/win/overlay contracts");

    app.press(KeyCode::Char('x'));
    let replacement_hits: u32 = app
        .eval_lua("return _G.coverage_replacement_hits")
        .expect("replacement keymap hit count");
    assert_eq!(replacement_hits, 1);

    assert!(app.ui_probe().named_buf("coverage.contract.buf").is_some());
    assert!(app.ui_probe().named_win("coverage.contract.win").is_some());
    assert!(app
        .ui_probe()
        .named_overlay("coverage.contract.overlay")
        .is_some());
}

#[test]
fn lua_prompt_text_theme_work_keymap_and_vim_contracts_are_available() {
    let mut app = TestApp::builder().with_vim(true).build();

    app.run_lua_result(
        r##"
            smelt.prompt.set_text("aéz")
            assert(smelt.prompt.text() == "aéz", "prompt text roundtrip")
            assert(smelt.prompt.cursor(999) == #"aéz", "prompt cursor clamps")
            assert(smelt.prompt.replace_range(1, 3, "X") == 2, "prompt replace cursor")
            assert(smelt.prompt.text() == "aXz", "prompt replace snaps utf8 range")
            assert(type(smelt.prompt.win()) == "userdata", "prompt win")
            assert(type(smelt.prompt.queued()) == "table", "prompt queued")
            assert(smelt.prompt.has_stash() == false, "prompt stash")

            assert(smelt.text.width("界") == 2, "wide char width")
            assert(smelt.text.line_count("a\nb") == 2, "line count")
            assert(smelt.text.slugify("Hello, Smelt!") == "hello-smelt", "slugify")
            assert(smelt.text.truncate("éx", 2) == "é", "truncate keeps utf8 boundary")
            assert(smelt.text.truncate("abcdef", 3, { keep = "tail", prefix = "…" }) == "…def", "tail truncate")
            assert(smelt.text.fit("x", 4, { align = "right", fill = "." }) == "...x", "fit right")
            assert(smelt.text.format_duration(65) == "1m 5s", "duration")
            assert(smelt.text.format_tokens(1200) == "1.2k", "tokens")
            assert(smelt.text.format_cost(1.25) == "$1.25", "cost")

            smelt.theme.set("CoverageContract", { fg = { ansi = 42 }, bold = true })
            local style = smelt.theme.get("CoverageContract")
            assert(style.fg.ansi == 42, "theme fg")
            assert(style.bold == true, "theme bold")
            assert(type(smelt.theme.snapshot().CoverageContract) == "table", "theme snapshot")
            assert(type(smelt.theme.is_light()) == "boolean", "theme light")

            assert(smelt.work.is_busy() == false, "work initially idle")
            local guard = smelt.work.guard()
            assert(smelt.work.guard_current(guard) == true, "work guard current")
            local busy = smelt.work.busy("coverage")
            assert(smelt.work.is_busy() == true, "work busy")
            assert(busy:remove() == true, "work remove")
            assert(smelt.work.is_busy() == false, "work idle after remove")

            local old_leader = smelt.keymap.leader()
            smelt.keymap.set_leader("<space>")
            assert(smelt.keymap.leader() == "<space>", "leader set")
            local reg = smelt.keymap.set("n", "<leader>t", function() end)
            local found = false
            for _, row in ipairs(smelt.keymap.list()) do
                if row.mode == "n" and row.chord == "<space>t" then found = true end
            end
            assert(found, "keymap listed")
            assert(reg:remove() == true, "keymap reg remove")
            assert(smelt.keymap.unset("n", "<leader>t") == false, "keymap already removed")
            smelt.keymap.set_leader(old_leader)
            assert(type(smelt.keymap.help_sections()) == "table", "help sections")

            assert(smelt.vim.mode() == "insert", "initial vim mode")
            smelt.vim.set_mode("normal")
            assert(smelt.vim.mode() == "normal", "vim mode set")
            smelt.vim.set_mode("insert")
        "##,
    )
    .expect("lua prompt/text/theme/work/keymap/vim contracts");

    assert_eq!(app.state().prompt_text, "aXz");
}

#[test]
fn lua_picker_permissions_notify_engine_and_ui_contracts_are_available() {
    let mut app = TestApp::builder().build();

    app.run_lua_result(
        r#"
            local size = smelt.ui.size()
            assert(type(size.width) == "number" and type(size.height) == "number", "ui size")
            assert(type(smelt.win.transcript()) == "userdata", "transcript win")
            assert(type(smelt.win.TRANSCRIPT) == "userdata", "transcript constant")
            assert(type(smelt.win.PROMPT) == "userdata", "prompt constant")

            local picker = smelt.picker.new({
                title = "Coverage Picker",
                items = {
                    "one",
                    { label = "two", description = "second" },
                },
            })
            assert(tostring(picker):find("Picker#", 1, true) == 1, "picker tostring")
            assert(type(picker:win()) == "userdata", "picker win")
            assert(picker:selected() == 0, "picker initial selected")
            assert(picker:move(1) == picker, "picker move chain")
            assert(picker:selected() == 1, "picker moved")
            assert(picker:items({ "replacement" }, 0) == picker, "picker items chain")
            assert(picker:selected() == 0, "picker selected reset")
            picker:close()

            local perms = smelt.permissions.list()
            assert(type(perms.session) == "table", "permission session list")
            assert(type(perms.workspace) == "table", "permission workspace list")
            assert(type(perms.repository) == "table", "permission repository list")
            local tool_decision = smelt.permissions.check_tool("default", "bash")
            local subcommand_decision = smelt.permissions.check("default", "bash", "git status")
            assert(type(tool_decision) == "string" and #tool_decision > 0, "permission tool decision")
            assert(type(subcommand_decision) == "string" and #subcommand_decision > 0, "permission subcommand decision")
            smelt.permissions.extend({
                default = {
                    tools = { ask = { "*" } },
                    effects = { read = "ask" },
                },
            })
            assert(not pcall(function()
                smelt.permissions.extend({ default = { effects = { read = "sometimes" } } })
            end), "permission effects reject unknown decisions")

            smelt.notify.info("hello from lua contract", "coverage")
            smelt.notify.warn("careful from lua contract", "coverage")
            smelt.notify.error("error from lua contract", "coverage")

            assert(smelt.engine.is_running() == false, "engine running")
            assert(type(smelt.engine.summary_prefix()) == "string", "summary prefix")
            local prep = smelt.engine.on_prepare_request(function(req, reply) reply(nil) end)
            assert(prep:remove() == true, "prepare hook remove")
            local limit = smelt.engine.on_context_limit(function(messages, reply) reply(nil) end)
            assert(limit:remove() == true, "context hook remove")
            assert(not pcall(function() smelt.engine.ask({ system = "" }) end), "ask validates system")
        "#,
    )
    .expect("lua picker/permissions/notify/engine/ui contracts");
}

#[test]
fn lua_layout_is_applied_before_first_render() {
    let mut app = TestApp::builder().build();

    app.run_lua_result(
        r#"
            local top = require("smelt.prompt_bar").top_win:rect()
            local transcript = smelt.win.transcript():rect()
            assert(top ~= nil, "prompt top bar has layout rect before first render")
            assert(transcript ~= nil, "transcript has layout rect before first render")
            assert(top.row > transcript.row, "top bar is below transcript")
        "#,
    )
    .expect("Lua layout is available before first render");
}

#[test]
fn empty_banner_returns_to_startup_position_after_resize_round_trip() {
    fn banner_label_rect(app: &TestApp) -> crate::smelt_edit::Rect {
        let win = app
            .ui_probe()
            .named_win("smelt.banner.label.win")
            .expect("banner label window");
        app.ui_probe()
            .win(win)
            .and_then(|win| win.viewport.map(|vp| vp.rect))
            .expect("banner label viewport")
    }

    fn paint_and_emit_resize(app: &mut TestApp) {
        app.render_silent();
        app.dispatch_ui_window_events(false);
    }

    let mut app = TestApp::builder().build();
    app.set_terminal_size(80, 24);
    app.drain_launch_ready_hooks();

    let startup = banner_label_rect(&app);

    app.set_terminal_size(100, 30);
    paint_and_emit_resize(&mut app);
    app.set_terminal_size(80, 24);
    paint_and_emit_resize(&mut app);

    assert_eq!(banner_label_rect(&app), startup);
}

#[test]
fn empty_banner_stays_inside_transcript_when_prompt_grows() {
    fn rects_intersect(a: crate::smelt_edit::Rect, b: crate::smelt_edit::Rect) -> bool {
        a.left < b.right() && a.right() > b.left && a.top < b.bottom() && a.bottom() > b.top
    }

    fn banner_label_rect(app: &TestApp) -> crate::smelt_edit::Rect {
        let win = app
            .ui_probe()
            .named_win("smelt.banner.label.win")
            .expect("banner label window");
        app.ui_probe()
            .win(win)
            .and_then(|win| win.viewport.map(|vp| vp.rect))
            .expect("banner label viewport")
    }

    fn paint_and_emit_resize(app: &mut TestApp) {
        app.render_silent();
        app.dispatch_ui_window_events(false);
    }

    let mut app = TestApp::builder().build();
    app.set_terminal_size(80, 24);
    app.drain_launch_ready_hooks();

    app.type_text("one\ntwo\nthree\nfour\nfive\nsix");
    paint_and_emit_resize(&mut app);

    let label = banner_label_rect(&app);
    let transcript = app
        .ui_probe()
        .split_rect(crate::app::TRANSCRIPT_WIN)
        .expect("transcript rect");
    let prompt = app
        .ui_probe()
        .split_rect(crate::app::PROMPT_WIN)
        .expect("prompt rect");

    assert!(
        label.top >= transcript.top && label.bottom() <= transcript.bottom(),
        "banner label should stay inside transcript: label={label:?} transcript={transcript:?}"
    );
    assert!(
        !rects_intersect(label, prompt),
        "banner label must not overlap prompt: label={label:?} prompt={prompt:?}"
    );
}

#[test]
fn win_rect_prefers_current_layout_after_resize_before_paint() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(80, 24);
    app.render_silent();

    app.set_terminal_size(100, 30);
    let expected = app
        .ui_probe()
        .split_rect(crate::app::TRANSCRIPT_WIN)
        .expect("transcript split rect after resize");

    let lua = format!(
        r#"
            local r = smelt.win.transcript():rect()
            assert(r ~= nil, "transcript rect")
            assert(r.row == {row}, "row " .. tostring(r.row))
            assert(r.col == {col}, "col " .. tostring(r.col))
            assert(r.width == {width}, "width " .. tostring(r.width))
            assert(r.height == {height}, "height " .. tostring(r.height))
        "#,
        row = expected.top,
        col = expected.left,
        width = expected.width,
        height = expected.height,
    );
    assert!(app.run_lua(&lua));
}

#[test]
fn reload_clears_surviving_prompt_keymaps() {
    let mut app = TestApp::builder().with_vim(false).build();
    assert!(app.run_lua(
        r#"
            smelt.prompt.win():key("left", function() end)
            "#,
    ));

    app.reload_lua();
    app.type_text("ab");
    app.press(KeyCode::Left);
    app.type_char('X');

    assert_eq!(app.state().prompt_text, "aXb");
    assert_eq!(app.prompt_cpos(), 2);
}

#[test]
fn reload_recompiles_transcript_renderer_extensions_and_rejects_stale_ir() {
    fn write_renderer(path: &std::path::Path, marker: Option<&str>) {
        let source = marker.map_or_else(
            || "-- default transcript renderer only\n".to_string(),
            |marker| {
                format!(
                    r#"
                    smelt.transcript.extend_renderer("reload-tool-marker", function(next, block, ctx)
                      if block.kind == "tool" then
                        return smelt.layout.text("{marker} " .. (block.name or "tool"))
                      end
                      return next(block, ctx)
                    end, {{ cache_key = "reload-tool-marker:{marker}" }})
                    "#,
                )
            },
        );
        std::fs::write(path, source).expect("write init.lua");
    }

    fn transcript_rows(app: &mut TestApp) -> Vec<String> {
        app.materialize_loaded_transcript_display_rows_expensive()
            .iter()
            .cloned()
            .collect()
    }

    let tmp = tempfile::tempdir().expect("temp init dir");
    let init = tmp.path().join("init.lua");
    write_renderer(&init, Some("reload-marker-v1"));

    let mut app = TestApp::builder().with_init_lua(&init).build();
    app.set_terminal_size(100, 30);
    let invocation_id = app.start_tool(
        "reload-call-1".into(),
        "bash".into(),
        protocol::StyledLines::from_plain("echo reload"),
        std::collections::HashMap::new(),
    );
    app.finish_tool(
        invocation_id,
        smelt_core::transcript_model::ToolStatus::Ok,
        Some(Box::new(smelt_core::transcript_model::ToolOutput {
            content: "reload output".into(),
            is_error: false,
            metadata: None,
            content_fields: Vec::new(),
        })),
        Some(Duration::from_millis(1200)),
    );

    let first = transcript_rows(&mut app).join("\n");
    assert!(first.contains("reload-marker-v1 bash"), "{first}");
    assert!(!first.contains("reload-marker-v2"), "{first}");

    write_renderer(&init, Some("reload-marker-v2"));
    app.reload_lua();
    let second = transcript_rows(&mut app).join("\n");
    assert!(second.contains("reload-marker-v2 bash"), "{second}");
    assert!(!second.contains("reload-marker-v1"), "{second}");

    write_renderer(&init, None);
    app.reload_lua();
    let default = transcript_rows(&mut app).join("\n");
    assert!(default.contains("* bash echo reload"), "{default}");
    assert!(!default.contains("reload-marker-v2"), "{default}");
}

#[test]
fn named_overlay_open_refreshes_title_in_place() {
    let mut app = TestApp::builder().build();

    app.run_lua_result(
        r#"
            local buf = smelt.buf.new({ name = "perf_panel.buf" })
            local win = smelt.win.new(buf, { name = "perf_panel.win", surface = "inert" })
            smelt.overlay.new({
                name = "perf_panel",
                title = "old title",
                anchor = "screen_at",
                corner = "ne",
                row = 0, col = 0,
                width = 44, height = 14,
                layout = smelt.ui.layout.leaf(win),
            })
            "#,
    )
    .expect("first open");

    let id1 = app
        .ui_probe()
        .named_overlay("perf_panel")
        .expect("named id");
    let title1 = app
        .ui_probe()
        .overlay(id1)
        .and_then(|ov| {
            ov.layout
                .chrome()
                .title
                .as_ref()
                .map(|l| l.spans.iter().map(|s| s.text.as_ref()).collect::<String>())
        })
        .unwrap_or_default();
    assert_eq!(title1, "old title");

    app.run_lua_result(
        r#"
            local buf = smelt.buf.new({ name = "perf_panel.buf" })
            local win = smelt.win.new(buf, { name = "perf_panel.win", surface = "inert" })
            smelt.overlay.new({
                name = "perf_panel",
                title = "new title",
                anchor = "screen_at",
                corner = "ne",
                row = 0, col = 0,
                width = 44, height = 14,
                layout = smelt.ui.layout.leaf(win),
            })
            "#,
    )
    .expect("second open");

    let id2 = app
        .ui_probe()
        .named_overlay("perf_panel")
        .expect("named id after refresh");
    assert_eq!(id1, id2, "same OverlayId across refresh");
    let title2 = app
        .ui_probe()
        .overlay(id2)
        .and_then(|ov| {
            ov.layout
                .chrome()
                .title
                .as_ref()
                .map(|l| l.spans.iter().map(|s| s.text.as_ref()).collect::<String>())
        })
        .unwrap_or_default();
    assert_eq!(title2, "new title", "title should refresh in place");
}

#[test]
fn named_win_refresh_preserves_wrap_when_omitted() {
    let mut app = TestApp::builder().build();

    app.run_lua_result(
        r#"
            local buf = smelt.buf.new({ name = "w.buf" })
            smelt.win.new(buf, { name = "w.win", wrap = false })
            "#,
    )
    .expect("first open");

    let wid = app.ui_probe().named_win("w.win").expect("named win");
    assert!(
        !app.ui_probe().win(wid).unwrap().wrap,
        "wrap should be false after explicit open"
    );

    // Re-open with the same name but no `wrap` key → wrap should stay false.
    app.run_lua_result(
        r#"
            local buf = smelt.buf.new({ name = "w.buf" })
            smelt.win.new(buf, { name = "w.win" })
            "#,
    )
    .expect("refresh");

    assert!(
        !app.ui_probe().win(wid).unwrap().wrap,
        "wrap must be preserved across named refresh (regression)"
    );
}

#[test]
fn named_buf_and_win_survive_across_open_calls() {
    let mut app = TestApp::builder().build();

    app.run_lua_result(
        r#"
            local buf = smelt.buf.new({ name = "x.buf" })
            smelt.win.new(buf, { name = "x.win" })
            "#,
    )
    .expect("first");
    let first_buf = app.ui_probe().named_buf("x.buf").expect("buf 1");
    let first_win = app.ui_probe().named_win("x.win").expect("win 1");

    app.run_lua_result(
        r#"
            local buf = smelt.buf.new({ name = "x.buf" })
            smelt.win.new(buf, { name = "x.win" })
            "#,
    )
    .expect("second");
    let second_buf = app.ui_probe().named_buf("x.buf").expect("buf 2");
    let second_win = app.ui_probe().named_win("x.win").expect("win 2");

    assert_eq!(
        first_buf, second_buf,
        "named buf id stable across re-create"
    );
    assert_eq!(first_win, second_win, "named win id stable across re-open");
}

#[test]
fn named_overlay_refresh_replaces_layout_structure() {
    let mut app = TestApp::builder().build();

    app.run_lua_result(
        r#"
            local buf = smelt.buf.new({ name = "a.buf" })
            local win = smelt.win.new(buf, { name = "a.win" })
            smelt.overlay.new({
                name = "ov",
                anchor = "screen_at", corner = "nw",
                row = 0, col = 0, width = 40, height = 10,
                layout = smelt.ui.layout.leaf(win),
            })
            "#,
    )
    .expect("first open");

    let id = app.ui_probe().named_overlay("ov").expect("named");
    let leaves_before = app
        .ui_probe()
        .overlay(id)
        .map(|ov| ov.layout.leaves_in_order().len())
        .unwrap_or(0);
    assert_eq!(leaves_before, 1);

    app.run_lua_result(
        r#"
            local b1 = smelt.buf.new({ name = "a.buf" })
            local b2 = smelt.buf.new({ name = "b.buf" })
            local w1 = smelt.win.new(b1, { name = "a.win" })
            local w2 = smelt.win.new(b2, { name = "b.win" })
            smelt.overlay.new({
                name = "ov",
                anchor = "screen_at", corner = "nw",
                row = 0, col = 0, width = 40, height = 10,
                layout = smelt.ui.layout.vbox({
                    { smelt.ui.layout.leaf(w1), height = "fill" },
                    { smelt.ui.layout.leaf(w2), height = "fill" },
                }),
            })
            "#,
    )
    .expect("structural refresh");

    let leaves_after = app
        .ui_probe()
        .overlay(id)
        .map(|ov| ov.layout.leaves_in_order().len())
        .unwrap_or(0);
    assert_eq!(leaves_after, 2, "layout should be swapped to 2-leaf vbox");
}

#[test]
fn sweep_state_prunes_untouched_entries() {
    let rt = crate::lua::LuaRuntime::new();
    rt.lua
        .load(
            r#"
                local s1 = smelt.state.get("alive")
                s1.open = true
                local s2 = smelt.state.get("dead")
                s2.open = true
                "#,
        )
        .exec()
        .expect("seed");

    // Re-running bootstrap starts a new touch cycle while preserving the
    // private state table. Simulate one plugin re-touching its state, then use
    // the host-only lifecycle hook to finish the cycle.
    smelt_core::lua::runtime::load_host_bootstrap_chunks(&rt.lua)
        .expect("start a new state touch cycle");
    rt.lua
        .load("smelt.state.get('alive')")
        .exec()
        .expect("touch alive state");
    smelt_core::lua::module::internal_api_function(&rt.lua, "smelt", "__sweep_state")
        .unwrap()
        .call::<()>(())
        .expect("sweep");

    let state = rt.state_snapshot();
    assert!(state.get("alive").is_some(), "touched entry survives");
    assert!(state.get("dead").is_none(), "untouched entry is swept");
}

#[test]
fn sweep_state_prunes_clean_untouched_persistent_entries() {
    let rt = crate::lua::LuaRuntime::new();
    rt.lua
        .load(
            r#"
                smelt.state.persistent("sweep_alive", { debounce_ms = 100000 })
                smelt.state.persistent("sweep_dead", { debounce_ms = 100000 })
                local dirty = smelt.state.persistent("sweep_dirty", { debounce_ms = 100000 })
                dirty.value = true
                "#,
        )
        .exec()
        .expect("seed persistent state");

    smelt_core::lua::runtime::load_host_bootstrap_chunks(&rt.lua)
        .expect("start a new persistent-state touch cycle");
    rt.lua
        .load("smelt.state.persistent('sweep_alive', { debounce_ms = 100000 })")
        .exec()
        .expect("touch alive persistent state");
    smelt_core::lua::module::internal_api_function(&rt.lua, "smelt", "__sweep_state")
        .unwrap()
        .call::<()>(())
        .expect("sweep");

    let internal = smelt_core::lua::module::internal_api_root(&rt.lua).unwrap();
    let persistent: mlua::Table = internal.raw_get("__persistent_state").unwrap();
    let exists = |name: &str| {
        !matches!(
            persistent.raw_get::<mlua::Value>(name).unwrap(),
            mlua::Value::Nil
        )
    };
    assert!(exists("sweep_alive"), "touched persistent entry survives");
    assert!(
        !exists("sweep_dead"),
        "clean untouched persistent entry is swept"
    );
    assert!(
        exists("sweep_dirty"),
        "dirty untouched persistent entry is kept for flushing"
    );
}

#[test]
fn reload_lua_refreshes_overlay_title_from_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");

    let body = |title: &str| {
        format!(
            r#"
                local state = smelt.state.get("plug")
                local function attach()
                    local buf = smelt.buf.new({{ name = "plug.buf" }})
                    local win = smelt.win.new(buf, {{ name = "plug.win" }})
                    smelt.overlay.new({{
                        name = "plug",
                        title = "{title}",
                        anchor = "screen_at", corner = "nw",
                        row = 0, col = 0, width = 40, height = 10,
                        layout = smelt.ui.layout.leaf(win),
                    }})
                end
                state.open = true
                attach()
                "#
        )
    };
    std::fs::write(&init, body("v1")).unwrap();

    let mut app = TestApp::builder().with_init_lua(&init).build();
    {
        assert_eq!(read_overlay_title(&app, "plug").as_deref(), Some("v1"));
    }
    let id1 = app.ui_probe().named_overlay("plug").unwrap();

    std::fs::write(&init, body("v2")).unwrap();
    {
        app.reload_lua();
    }
    let id2 = app
        .ui_probe()
        .named_overlay("plug")
        .expect("overlay survives");
    assert_eq!(id1, id2, "OverlayId preserved across reload");
    {
        assert_eq!(read_overlay_title(&app, "plug").as_deref(), Some("v2"));
    }
}

#[test]
fn reload_lua_preserves_nested_state_tables() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    std::fs::write(
        &init,
        r#"
            local s = smelt.state.get("nested")
            s.cfg = s.cfg or { panel = { width = 80, history = { 1, 2, 3 } } }
            "#,
    )
    .unwrap();

    let mut app = TestApp::builder().with_init_lua(&init).build();
    {
        app.reload_lua();
        app.reload_lua();
    }
    let width: u64 = app
        .eval_lua("return smelt.state.get('nested').cfg.panel.width")
        .unwrap();
    let last: u64 = app
        .eval_lua("return smelt.state.get('nested').cfg.panel.history[3]")
        .unwrap();
    assert_eq!(width, 80);
    assert_eq!(last, 3);
}

#[test]
fn reload_lua_flushes_pending_persistent_state_before_clearing_timers() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    std::fs::write(
        &init,
        r#"
            local s = smelt.state.persistent("flush_reload", { debounce_ms = 100000 })
            s.value = "before-reload"
            "#,
    )
    .unwrap();

    let mut app = TestApp::builder().with_init_lua(&init).build();
    let state_path = app
        .session_storage_root()
        .join("plugins")
        .join("flush_reload.json");
    assert!(
        !state_path.exists(),
        "debounced save should not have reached disk before reload"
    );
    std::fs::write(&init, "-- no persistent write on reload\n").unwrap();
    app.reload_lua();

    let raw = std::fs::read_to_string(&state_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", state_path.display()));
    let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(json["value"], "before-reload");
}

#[test]
fn direct_reload_clears_pending_scheduled_reload() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    std::fs::write(&init, "_G.reload_count = (_G.reload_count or 0) + 1\n").unwrap();

    let mut app = TestApp::builder().with_init_lua(&init).build();
    assert_eq!(app.lua_int_global("reload_count"), Some(1));

    assert!(app.schedule_lua_reload());
    app.reload_lua();

    assert!(!app.pending_lua_reload());
    assert_eq!(
        app.lua_int_global("reload_count"),
        Some(1),
        "a committed reload installs a fresh Lua global environment"
    );
}

#[test]
fn reload_lua_does_not_double_wrap_tools_register() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    std::fs::write(&init, "").unwrap();

    let mut app = TestApp::builder().with_init_lua(&init).build();
    {
        for _ in 0..5 {
            app.reload_lua();
        }
        // Register a tool with no `summary`; the bootstrap wrap should
        // populate it once. If the wrap had compounded across reloads
        // the call would still succeed - but every reload would add a
        // closure frame on top. The functional check: registration
        // works and the registered summary handler runs.
        app.exec_lua_entry(
            r#"
                smelt.tools.register({
                    name = "t",
                    description = "",
                    parameters = { type = "object", properties = {} },
                    execute = function() return "" end,
                })
                "#,
        )
        .expect("register after many reloads");
    }
    let summary = app.tool_summary("t", &std::collections::HashMap::new());
    // `default_summary` returns "" when args have no recognised keys.
    assert!(
        summary.is_empty(),
        "summary should be empty for no-arg tool"
    );
}

#[test]
fn reload_lua_reaps_anonymous_overlay_keeps_named() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    // First version opens both a named overlay and a plain
    // anonymous overlay. init.lua doesn't call `smelt.plugin(...)`,
    // so its loader frame is unnamed and anonymous resources stay
    // anonymous - they get reaped on /reload.
    std::fs::write(
        &init,
        r#"
            local state = smelt.state.get("mix")
            local function attach()
                local b1 = smelt.buf.new({ name = "mix.buf" })
                local w1 = smelt.win.new(b1, { name = "mix.win" })
                smelt.overlay.new({
                    name = "mix",
                    anchor = "screen_at", corner = "nw",
                    row = 0, col = 0, width = 30, height = 8,
                    layout = smelt.ui.layout.leaf(w1),
                })
            end
            state.open = true
            attach()

            -- Anonymous overlay: init.lua's frame is unnamed (no
            -- `smelt.plugin(...)` call), so this gets reaped on /reload.
            local b2 = smelt.buf.new()
            local w2 = smelt.win.new(b2, {})
            smelt.overlay.new({
                anchor = "screen_at", corner = "se",
                row = 0, col = 0, width = 20, height = 5,
                layout = smelt.ui.layout.leaf(w2),
            })
            "#,
    )
    .unwrap();

    let mut app = TestApp::builder().with_init_lua(&init).build();

    // Capture the anonymous overlay's id - we'll assert it's gone
    // after reload while the named one survives. (Total overlay
    // count is noisy: reload_lua emits a `notify(...)` toast which
    // adds its own short-lived overlay.)
    let named_id = app.ui_probe().named_overlay("mix").expect("named");
    let anon_id = (1u32..)
        .map(crate::smelt_edit::OverlayId)
        .find(|id| *id != named_id && app.ui_probe().overlay(*id).is_some())
        .expect("anonymous overlay present");

    // Second version drops the anonymous overlay; named one stays.
    std::fs::write(
        &init,
        r#"
            local state = smelt.state.get("mix")
            local function attach()
                local b1 = smelt.buf.new({ name = "mix.buf" })
                local w1 = smelt.win.new(b1, { name = "mix.win" })
                smelt.overlay.new({
                    name = "mix",
                    anchor = "screen_at", corner = "nw",
                    row = 0, col = 0, width = 30, height = 8,
                    layout = smelt.ui.layout.leaf(w1),
                })
            end
            if state.open then attach() end
            "#,
    )
    .unwrap();
    {
        app.reload_lua();
    }
    assert!(
        app.ui_probe().named_overlay("mix").is_some(),
        "named overlay survives reload"
    );
    assert!(
        app.ui_probe().overlay(anon_id).is_none(),
        "anonymous overlay {} should be reaped",
        anon_id.0
    );
}

#[test]
fn reload_lua_retires_named_resources_not_declared_by_the_candidate() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    std::fs::write(
        &init,
        r#"
        local buffer = smelt.buf.new({ name = "retired.buf" })
        local window = smelt.win.new(buffer, { name = "retired.win" })
        smelt.overlay.new({
            name = "retired.overlay",
            anchor = "screen_at",
            corner = "nw",
            row = 0,
            col = 0,
            width = 20,
            height = 5,
            layout = smelt.ui.layout.leaf(window),
        })
        smelt.paint.register(function() end, { name = "retired.paint" })
        "#,
    )
    .unwrap();
    let mut app = TestApp::builder().with_init_lua(&init).build();
    let overlay = app
        .ui_probe()
        .named_overlay("retired.overlay")
        .expect("committed named overlay");
    let paint = app
        .paint_registry_probe()
        .id_by_name("retired.paint")
        .expect("committed named paint");

    std::fs::write(&init, "-- resource declarations removed\n").unwrap();
    app.reload_lua();

    assert!(app.ui_probe().overlay(overlay).is_none());
    assert!(app.ui_probe().named_overlay("retired.overlay").is_none());
    assert!(app.ui_probe().named_win("retired.win").is_none());
    assert!(app.ui_probe().named_buf("retired.buf").is_none());
    assert!(!app.paint_registry_probe().contains(paint));
    assert!(app
        .paint_registry_probe()
        .id_by_name("retired.paint")
        .is_none());
}

#[test]
fn reload_lua_preserves_named_paint_slot() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    // Module-body code: capture the paint id in a state slot so we
    // can read it back from Rust after the reload cycle.
    std::fs::write(
        &init,
        r#"
            local state = smelt.state.get("paint_id_probe")
            local function painter(_slice, _ctx) end
            -- No `smelt.plugin(...)` call → init.lua's loader frame
            -- stays unnamed, so the unnamed register call below is
            -- anonymous and gets reaped on /reload. The explicit
            -- name = "probe.named" slot survives.
            smelt.paint.register(painter, { name = "probe.named" })
            smelt.paint.register(painter)
            state.dummy = true
            "#,
    )
    .unwrap();

    let mut app = TestApp::builder().with_init_lua(&init).build();

    let pre_named = app
        .paint_registry_probe()
        .id_by_name("probe.named")
        .expect("named pre id");
    // The anonymous slot has no name binding; locate it as the only
    // un-named PaintId currently registered.
    let pre_anon = find_anon_paint(&app);

    {
        app.reload_lua();
    }

    let post_named = app
        .paint_registry_probe()
        .id_by_name("probe.named")
        .expect("named post id");
    let post_anon = find_anon_paint(&app);
    assert_eq!(
        pre_named, post_named,
        "named paint slot must keep stable PaintId across reload"
    );
    assert_ne!(
        pre_anon, post_anon,
        "anonymous paint slot must allocate a fresh id on reload"
    );
    assert!(
        !app.paint_registry_probe().contains(pre_anon),
        "old anonymous PaintId must be reaped"
    );
    assert!(app.paint_registry_probe().contains(post_named));
    assert!(app.paint_registry_probe().contains(post_anon));
}

#[test]
fn reload_lua_drains_ready_hooks_with_kind_reload() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    std::fs::write(
        &init,
        r#"
            local state = smelt.state.get("ready_kind_probe")
            state.fires = (state.fires or 0)
            state.last_kind = nil
            smelt.lifecycle.on_ready(function(ctx)
                state.fires = state.fires + 1
                state.last_kind = ctx and ctx.kind or "<nil>"
            end)
            "#,
    )
    .unwrap();

    let mut app = TestApp::builder().with_init_lua(&init).build();
    // `TestApp` retains launch-ready hooks so storybook snapshots do not
    // receive interactive decoration such as the splash banner. This test
    // explicitly drains the generation-zero hooks.
    app.drain_launch_ready_hooks();

    let read = |app: &mut TestApp, k: &str| -> String {
        app.eval_lua(&format!(
            "return tostring(smelt.state.get('ready_kind_probe').{k})"
        ))
        .unwrap()
    };
    assert_eq!(read(&mut app, "fires"), "1");
    assert_eq!(read(&mut app, "last_kind"), "launch");

    {
        app.reload_lua();
    }
    assert_eq!(read(&mut app, "fires"), "2");
    assert_eq!(read(&mut app, "last_kind"), "reload");
}

#[test]
fn reload_lua_sweeps_state_for_deleted_plugins() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    std::fs::write(
        &init,
        r#"
            local a = smelt.state.get("kept")
            a.flag = true
            local b = smelt.state.get("dropped")
            b.flag = true
            "#,
    )
    .unwrap();

    let mut app = TestApp::builder().with_init_lua(&init).build();
    let exists = |app: &TestApp, key: &str| app.app.lua.state_snapshot().get(key).is_some();
    {
        assert!(exists(&mut app, "kept"));
        assert!(exists(&mut app, "dropped"));
    }

    // Edit: only the "kept" plugin remains.
    std::fs::write(
        &init,
        r#"
            local a = smelt.state.get("kept")
            "#,
    )
    .unwrap();
    {
        app.reload_lua();
        assert!(exists(&mut app, "kept"));
        assert!(
            !exists(&mut app, "dropped"),
            "dropped plugin's state should be swept"
        );
    }
}

#[test]
fn reload_clears_every_lua_surface() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    // Populate every observable surface from user init.lua so the
    // reload-with-empty-init test below can assert each is empty.
    std::fs::write(
        &init,
        r#"
            -- LuaShared registries
            smelt.cmd.register("seed_cmd", function() end)
            smelt.keymap.set("n", "<C-o>", function() end)
            smelt.tools.register({
                name = "seed_tool",
                description = "",
                parameters = { type = "object", properties = {} },
                permission_defaults = { normal = "deny" },
                effect = "config",
                default_allow = { "seed" },
                subpattern_parser = "bash",
                execute = function() return "" end,
            })
            smelt.permissions.extend({ normal = { tools = { deny = { "seed_tool" } } } })
            smelt.process.set_default_shell({ program = "/bin/zsh", args = { "-fc" } })
            smelt.provider.register("seed_provider", {
                type = "openai",
                api_base = "http://seed.invalid",
                models = { "seed-model" },
            })
            smelt.mcp.register("seed_mcp", {
                command = { "seed-mcp" },
                enabled = false,
            })
            smelt.provider.middleware({ on_response = function() end })

            -- core::timers (Lua-side)
            smelt.timer.every(100000, function() end)

            -- in-flight task (cancel_and_clear path)
            smelt.spawn(function()
                smelt.sleep(100000)
            end)

            -- Anonymous + named UI resources
            local b1 = smelt.buf.new({ name = "seed.buf" })
            local w1 = smelt.win.new(b1, { name = "seed.win" })
            smelt.overlay.new({
                name = "seed.ov",
                anchor = "screen_at", corner = "nw",
                row = 0, col = 0, width = 30, height = 8,
                layout = smelt.ui.layout.leaf(w1),
            })
            -- Anonymous overlay (init.lua frame unnamed): must be reaped.
            local b2 = smelt.buf.new()
            local w2 = smelt.win.new(b2, {})
            smelt.overlay.new({
                anchor = "screen_at", corner = "se",
                row = 0, col = 0, width = 20, height = 5,
                layout = smelt.ui.layout.leaf(w2),
            })

            -- smelt.state slot
            local s = smelt.state.get("seed_plugin")
            s.open = true
            "#,
    )
    .unwrap();

    let mut app = TestApp::builder().with_init_lua(&init).build();
    app.assert_no_handle_leak_across_reload();
    let shared = app.lua_probe().shared().core.clone();

    // Pre-reload: every surface has at least the seeded entry.
    assert!(shared.commands.lock().unwrap().contains_key("seed_cmd"));
    assert!(shared
        .keymaps
        .lock()
        .unwrap()
        .keys()
        .any(|(_, c)| c == "<C-o>"));
    assert!(shared.tools.lock().unwrap().contains_key("seed_tool"));
    assert!(shared
        .tool_defaults
        .lock()
        .unwrap()
        .tool_decisions
        .contains_key("seed_tool"));
    assert!(shared.permission_rules.lock().unwrap().is_some());
    assert!(shared.default_shell.lock().unwrap().is_some());
    assert!(shared
        .providers
        .lock()
        .unwrap()
        .iter()
        .any(|p| p.name.as_deref() == Some("seed_provider")));
    assert!(shared.mcp_configs.lock().unwrap().contains_key("seed_mcp"));
    assert!(!shared.hooks.provider_response.is_empty());
    assert!(!app.core_probe().timers.is_empty());
    assert!(!shared.tasks.lock().unwrap().is_empty());
    let anon_overlay = (1u32..)
        .map(crate::smelt_edit::OverlayId)
        .find(|id| {
            Some(*id) != app.ui_probe().named_overlay("seed.ov")
                && app.ui_probe().overlay(*id).is_some()
        })
        .expect("anonymous overlay present");

    // Edit init.lua to empty + drop the "seed_plugin" state slot.
    std::fs::write(&init, "").unwrap();
    {
        app.reload_lua();
    }

    // Post-reload: every user-registered surface and UI resource from the
    // dropped generation is gone, and its persistent state slot is swept.
    assert!(
        !shared.commands.lock().unwrap().contains_key("seed_cmd"),
        "user command cleared"
    );
    assert!(
        !shared
            .keymaps
            .lock()
            .unwrap()
            .keys()
            .any(|(_, c)| c == "<C-o>"),
        "user keymap cleared"
    );
    assert!(
        !shared.tools.lock().unwrap().contains_key("seed_tool"),
        "user tool cleared"
    );
    let defaults = shared.tool_defaults.lock().unwrap();
    assert!(
        !defaults.tool_decisions.contains_key("seed_tool")
            && !defaults.tool_effects.contains_key("seed_tool")
            && !defaults.subcommand_allow.contains_key("seed_tool")
            && !defaults.subpattern_parsers.contains_key("seed_tool"),
        "tool defaults cleared"
    );
    drop(defaults);
    assert!(
        shared.permission_rules.lock().unwrap().is_none(),
        "permission rules cleared"
    );
    assert!(
        shared.default_shell.lock().unwrap().is_none(),
        "default shell cleared"
    );
    assert!(
        !shared
            .providers
            .lock()
            .unwrap()
            .iter()
            .any(|p| p.name.as_deref() == Some("seed_provider")),
        "provider registry cleared"
    );
    assert!(
        !shared.mcp_configs.lock().unwrap().contains_key("seed_mcp"),
        "MCP registry cleared"
    );
    assert!(
        shared.hooks.provider_response.is_empty(),
        "provider middleware cleared"
    );
    assert!(app.core_probe().timers.is_empty(), "timers cleared");
    assert!(shared.tasks.lock().unwrap().is_empty(), "tasks cleared");
    assert!(
        shared.task_inbox.lock().unwrap().is_empty(),
        "task_inbox drained"
    );
    assert!(
        shared.json_inbox.lock().unwrap().is_empty(),
        "json_inbox drained"
    );
    assert!(
        app.ui_probe().named_overlay("seed.ov").is_none(),
        "stale named overlay retired"
    );
    assert!(
        app.ui_probe().named_win("seed.win").is_none(),
        "stale named window retired"
    );
    assert!(
        app.ui_probe().named_buf("seed.buf").is_none(),
        "stale named buffer retired"
    );
    assert!(
        app.ui_probe().overlay(anon_overlay).is_none(),
        "anonymous overlay reaped"
    );
    assert!(
        app.app.lua.state_snapshot().get("seed_plugin").is_none(),
        "dropped-plugin state slot swept"
    );
}

#[test]
fn reload_lua_cancels_in_flight_tasks() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    std::fs::write(
        &init,
        r#"
            _G.__task_completed__ = false
            smelt.spawn(function()
                smelt.sleep(10000)  -- long sleep so the task is still parked
                _G.__task_completed__ = true
            end)
            "#,
    )
    .unwrap();

    let mut app = TestApp::builder().with_init_lua(&init).build();
    // Sanity: task is parked but not complete.
    let completed: bool = app.eval_lua("return _G.__task_completed__").unwrap();
    assert!(!completed, "task shouldn't have completed yet");

    // Edit init.lua so reload doesn't re-spawn the task.
    std::fs::write(&init, "_G.__task_completed__ = false").unwrap();
    {
        app.reload_lua();
    }
    // Drive: cancelled tasks should be a no-op since we cleared them.
    let output_count = app.drive_lua_tasks_once();
    assert_eq!(output_count, 0, "no task outputs after reload cancellation");
    let completed: bool = app.eval_lua("return _G.__task_completed__").unwrap();
    assert!(!completed, "cancelled task must not have run to completion");
}

#[test]
fn removing_spawn_reg_exposes_cancellation_and_cleans_the_task() {
    let mut app = TestApp::builder().build();
    let baseline = app.lua_probe().core_shared().tasks.lock().unwrap().len();
    assert!(app
        .lua_probe()
        .core_shared()
        .task_inbox
        .lock()
        .unwrap()
        .is_empty());

    assert!(app.run_lua(
        r#"
            _G.cancel_observed = false
            _G.cancel_finished = false
            _G.cancel_reg = smelt.spawn(function()
                local ok, err = pcall(function() smelt.sleep(60000) end)
                _G.cancel_observed = not ok and smelt.task.is_cancelled(err)
                _G.cancel_finished = true
            end)
            "#,
    ));
    app.drive_lua_tasks_once();
    assert_eq!(
        app.lua_probe().core_shared().tasks.lock().unwrap().len(),
        baseline + 1
    );

    assert!(app.run_lua("_G.cancel_reg:remove()"));
    assert_eq!(app.drive_lua_tasks_once(), 0);
    assert!(app.eval_lua::<bool>("return _G.cancel_observed").unwrap());
    assert!(app.eval_lua::<bool>("return _G.cancel_finished").unwrap());
    assert_eq!(
        app.lua_probe().core_shared().tasks.lock().unwrap().len(),
        baseline
    );
    assert!(app
        .lua_probe()
        .core_shared()
        .task_inbox
        .lock()
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn reload_lua_via_engine_dismisses_open_modal() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    std::fs::write(
        &init,
        r#"
            smelt.cmd.register("open_modal", function()
                smelt.spawn(function()
                    local leaf = smelt.dialog.content({ text = "hello" })
                    smelt.dialog.open({
                        title = "test",
                        max_height = "50%",
                        panels = { { leaf = leaf } },
                    })
                end)
            end)
            "#,
    )
    .unwrap();

    let mut app = TestApp::builder().with_init_lua(&init).build();
    {
        app.apply_lua_command("open_modal");
        app.drive_lua_tasks();
    }
    assert!(
        app.ui_probe().active_modal().is_some(),
        "modal should be open after /open_modal"
    );

    // Drive the reload through the Lua binding (the gate lives there,
    // not in `TuiApp::reload_lua`). The binding should dismiss the
    // modal and call through to `reload_lua` instead of bailing out.
    app.exec_lua_entry("smelt.engine.reload()")
        .expect("reload succeeds even with modal open");
    assert!(app.pending_lua_reload());
    assert!(app.drain_idle_work());
    assert!(
        app.ui_probe().active_modal().is_none(),
        "modal must be dismissed after reload"
    );

    // Reload should have re-registered the command - reopen works.
    {
        app.apply_lua_command("open_modal");
        app.drive_lua_tasks();
    }
    assert!(
        app.ui_probe().active_modal().is_some(),
        "command survived reload and reopens modal"
    );
}

#[test]
fn reload_lua_preserves_user_size_override() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    std::fs::write(
        &init,
        r#"
            local state = smelt.state.get("res")
            local function attach()
                local b = smelt.buf.new({ name = "res.buf" })
                local w = smelt.win.new(b, { name = "res.win" })
                smelt.overlay.new({
                    name = "res",
                    anchor = "screen_at", corner = "nw",
                    row = 0, col = 0, width = 30, height = 10,
                    resizable = true,
                    layout = smelt.ui.layout.leaf(w),
                })
            end
            state.open = true
            attach()
            "#,
    )
    .unwrap();

    let mut app = TestApp::builder().with_init_lua(&init).build();
    let id = app.ui_probe().named_overlay("res").unwrap();
    // Simulate a user resize gesture.
    app.set_overlay_size_override(id, (50, 18));

    {
        app.reload_lua();
    }
    let id2 = app.ui_probe().named_overlay("res").expect("survives");
    assert_eq!(id, id2);
    let ov = app.ui_probe().overlay(id2).unwrap();
    assert_eq!(
        ov.size_override,
        Some((50, 18)),
        "user resize preserved across reload"
    );
}

#[test]
fn scheduled_reload_runs_after_turn_is_idle() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    std::fs::write(&init, "_G.reload_count = (_G.reload_count or 0) + 1\n").unwrap();

    let mut app = TestApp::builder().with_init_lua(&init).build();
    assert_eq!(app.lua_int_global("reload_count"), Some(1));

    app.start_turn(1);
    assert!(app.run_lua("return smelt.engine.reload_when_idle()"));
    assert!(app.pending_lua_reload());
    assert_eq!(app.lua_int_global("reload_count"), Some(1));

    app.feed_one(SourceEvent::engine(EngineEvent::TurnComplete {
        turn_id: 1,
        history: None,
        meta: None,
    }));

    assert!(!app.pending_lua_reload());
    assert_eq!(
        app.lua_int_global("reload_count"),
        Some(1),
        "scheduled reload commits a fresh Lua global environment"
    );
}

#[test]
fn hot_reload_reconciles_new_mode_cycle_and_permissions() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    std::fs::write(&init, "-- initially empty\n").unwrap();

    let mut app = TestApp::builder().with_init_lua(&init).build();
    let review = AgentMode::parse("review").unwrap();
    assert!(!app.core_probe().config.mode_cycle.contains(&review));

    std::fs::write(
        &init,
        r#"
        smelt.mode.register({ name = "review", after = "normal" })
        smelt.permissions.extend({
          review = {
            tools = {
              allow = { "review_result" },
              deny = { "smelt_reload" },
            },
          },
        })
        "#,
    )
    .unwrap();
    app.reload_lua();

    assert!(app.core_probe().config.mode_cycle.contains(&review));
    let outcome = app.core_probe().permissions.evaluate_tool(
        review.clone(),
        smelt_core::permissions::ToolOrigin::Lua,
        "smelt_reload",
        &std::collections::HashMap::new(),
    );
    assert_eq!(outcome.decision, protocol::Decision::Deny);
    let outcome = app.core_probe().permissions.evaluate_tool(
        review,
        smelt_core::permissions::ToolOrigin::Lua,
        "review_result",
        &std::collections::HashMap::new(),
    );
    assert_eq!(outcome.decision, protocol::Decision::Allow);
}

#[test]
fn plan_mode_reload_registers_present_plan_when_already_in_plan() {
    let tmp = tempfile::tempdir().unwrap();
    let init = tmp.path().join("init.lua");
    std::fs::write(&init, "require(\"smelt.plugins.plan_mode\")\n").unwrap();
    let plan = AgentMode::parse("plan").unwrap();

    let app = TestApp::builder()
        .with_init_lua(&init)
        .with_mode(plan.clone())
        .build();
    let tools = app
        .lua_probe()
        .tool_defs(plan, smelt_core::lua::ToolVisibility::Interactive);
    assert!(
        tools.iter().any(|t| t.name == "present_plan"),
        "present_plan should be present after reload while already in plan"
    );
}
