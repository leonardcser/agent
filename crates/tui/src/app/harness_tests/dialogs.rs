use super::*;

fn open_root_test_dialog(app: &mut TestApp) {
    assert!(app.run_lua(
        r#"
        local leaf = smelt.dialog.content({ text = "Review this action" })
        smelt.dialog.new({
          panels = { { leaf = leaf, height = "fit" } },
          blocks_agent = true,
        })
        "#
    ));
}

fn assert_safe_dialog_fallback(app: &TestApp) {
    assert!(app.ui_probe().split_rect(crate::app::PROMPT_WIN).is_none());
    let statusline = app
        .ui_probe()
        .named_win("smelt.statusline")
        .expect("statusline window");
    let status_rect = app
        .ui_probe()
        .split_rect(statusline)
        .expect("fallback keeps statusline mounted");
    assert_eq!(
        status_rect.top + status_rect.height,
        app.ui_probe().terminal_size().1
    );
}

#[test]
fn splash_paint_stays_below_global_overlays() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(80, 24);
    app.drain_launch_ready_hooks();
    assert!(app.run_lua(
        r#"
        local buf = smelt.buf.new({ name = "test.paint_order.buf" })
        local lines = {}
        for _ = 1, 12 do lines[#lines + 1] = string.rep("X", 30) end
        buf:lines(lines)
        local win = smelt.win.new(buf, {
          name = "test.paint_order.win",
          scrollbar = false,
          wrap = false,
        })
        smelt.overlay.new({
          name = "test.paint_order.overlay",
          layout = smelt.ui.layout.leaf(win),
          anchor = "center",
          width = 30,
          height = 12,
          z = 100,
        })
        "#,
    ));

    let frame = app.render_to_frame();
    let overlay_win = app
        .ui_probe()
        .named_win("test.paint_order.win")
        .expect("overlay window");
    let overlay_rect = app
        .ui_probe()
        .win(overlay_win)
        .and_then(|win| win.viewport.map(|viewport| viewport.rect))
        .expect("overlay viewport");

    for row in overlay_rect.top..overlay_rect.bottom() {
        let cells: Vec<char> = frame.rows[row as usize].chars().collect();
        assert!(
            cells[overlay_rect.left as usize..overlay_rect.right() as usize]
                .iter()
                .all(|cell| *cell == 'X'),
            "splash paint escaped its transcript layer into the overlay:\n{}",
            frame.text()
        );
    }
}

#[test]
fn root_dialog_replaces_composer_and_restores_prompt_state() {
    let mut app = TestApp::builder().build();
    app.type_text("draft response");
    assert_eq!(app.state().prompt_text, "draft response");

    open_root_test_dialog(&mut app);
    app.render_silent();

    assert!(app.state().active_modal.is_some());
    assert!(app.ui_probe().split_rect(crate::app::PROMPT_WIN).is_none());
    assert_eq!(app.state().prompt_text, "draft response");
    assert!(!app.render_to_frame().text().contains("draft response"));

    assert!(app.run_lua("smelt.dialog.current().close()"));
    app.render_silent();

    assert!(app.state().active_modal.is_none());
    assert!(app.ui_probe().split_rect(crate::app::PROMPT_WIN).is_some());
    assert_eq!(app.state().prompt_text, "draft response");
    assert!(app.render_to_frame().text().contains("draft response"));
}

#[test]
fn dialog_input_edits_and_renders_unicode_by_grapheme() {
    let mut app = TestApp::builder().build();
    assert!(app.run_lua(
        r#"
        local leaf, buf = smelt.dialog.input("custom answer", { wrap = true })
        _G.__unicode_answer_buf = buf
        smelt.dialog.new({
          title = "question",
          focus = leaf,
          panels = { { leaf = leaf, height = "fit" } },
        })
        "#
    ));
    let input = "e\u{301}👩\u{200d}💻9\u{fe0f}🇨🇦";

    app.type_text(input);
    let frame = app.render_to_frame().text();
    for grapheme in ["e\u{301}", "👩\u{200d}💻", "9\u{fe0f}", "🇨🇦"] {
        assert!(frame.contains(grapheme), "frame: {frame}");
    }

    app.press(KeyCode::Left);
    app.press(KeyCode::Backspace);
    app.press(KeyCode::Delete);

    let edited = app
        .eval_lua::<String>("return _G.__unicode_answer_buf:line(1)")
        .unwrap();
    assert_eq!(edited, "e\u{301}👩\u{200d}💻");
    let frame = app.render_to_frame().text();
    assert!(frame.contains("e\u{301}"), "frame: {frame}");
    assert!(frame.contains("👩\u{200d}💻"), "frame: {frame}");
}

#[test]
fn dialogs_dismiss_from_the_keyboard_without_mouse_focus() {
    let mut app = TestApp::builder().build();

    open_root_test_dialog(&mut app);
    assert!(
        app.ui_probe().focused_modal().is_some(),
        "opening a dialog should move keyboard focus into it"
    );
    app.press(KeyCode::Esc);
    assert!(
        app.state().active_modal.is_none(),
        "Esc should close a dialog without a mouse click"
    );

    assert!(app.run_lua(r#"smelt.dialog.viewer({ title = "viewer", text = "read only" })"#));
    app.type_char('q');
    assert!(
        app.state().active_modal.is_none(),
        "q should close a read-only dialog without a mouse click"
    );
}

#[test]
fn usage_dialog_opened_during_active_turn_stays_dismissible() {
    for (code, modifiers, label) in [
        (KeyCode::Char('q'), KeyModifiers::NONE, "q"),
        (KeyCode::Esc, KeyModifiers::NONE, "Esc"),
        (KeyCode::Char('c'), KeyModifiers::CONTROL, "Ctrl-C"),
    ] {
        let mut app = TestApp::builder().build();
        app.type_text("start working");
        app.press(KeyCode::Enter);
        assert!(
            app.agent_running(),
            "submitting a prompt should start a turn"
        );

        app.type_text("/usage");
        assert!(
            app.state().picker_count > 0,
            "slash completer should be open"
        );
        app.press(KeyCode::Enter);
        assert_eq!(
            app.state().picker_count,
            0,
            "accepting /usage should close the completer"
        );
        assert!(
            app.state().active_modal.is_some(),
            "/usage should open a dialog"
        );
        assert!(
            app.ui_probe().focused_modal().is_some(),
            "/usage should own keyboard focus after prompt submission"
        );
        assert!(app.run_lua(
            r#"
            smelt.spawn(function()
              smelt.sleep(10)
              _G.__usage_focus_callback_ran = true
              smelt.win.TRANSCRIPT:focus()
            end)
            "#,
        ));
        app.settle_lua();

        app.engine_event(protocol::EngineEvent::TextDelta {
            delta: "working".into(),
        });
        app.feed_one(SourceEvent::Tick(10));
        app.tick_timers();
        app.settle_lua();
        app.render_silent();
        assert!(
            app.eval_lua::<bool>("return __usage_focus_callback_ran == true")
                .unwrap(),
            "background callback should run"
        );
        assert!(
            app.ui_probe().focused_modal().is_some(),
            "background callbacks must not move focus outside /usage"
        );
        assert_eq!(
            app.state().app_focus,
            AppFocus::Prompt,
            "a rejected focus request must not change the active app pane"
        );

        app.press_mod(code, modifiers);
        assert!(
            app.state().active_modal.is_none(),
            "{label} should close /usage while the active turn continues"
        );
        app.render_silent();
        assert_eq!(app.state().app_focus, AppFocus::Prompt);
        assert_eq!(app.ui_probe().focus(), Some(crate::app::PROMPT_WIN));
        assert!(app.agent_running());
    }
}

#[test]
fn deferred_lua_focus_commits_app_pane_only_after_layout_accepts_it() {
    let mut app = TestApp::builder().build();
    assert!(app.run_lua(
        r#"
        smelt.ui.layout.set(function()
          return smelt.ui.layout.leaf(smelt.win.PROMPT)
        end)
        "#,
    ));
    app.render_silent();
    assert!(app
        .ui_probe()
        .split_rect(crate::app::TRANSCRIPT_WIN)
        .is_none());
    assert_eq!(app.state().app_focus, AppFocus::Prompt);

    assert!(app
        .exec_lua_entry(
            r#"
            smelt.ui.layout.set(function()
              return smelt.ui.layout.leaf(smelt.win.TRANSCRIPT)
            end)
            smelt.win.TRANSCRIPT:focus()
            "#,
        )
        .is_ok());
    assert_eq!(
        app.state().app_focus,
        AppFocus::Prompt,
        "deferred focus must not commit before its target is mounted"
    );

    app.render_silent();
    assert!(app
        .ui_probe()
        .split_rect(crate::app::TRANSCRIPT_WIN)
        .is_some());
    assert_eq!(app.ui_probe().focus(), Some(crate::app::TRANSCRIPT_WIN));
    assert_eq!(app.state().app_focus, AppFocus::Content);
}

#[test]
fn failed_root_dialog_open_does_not_pollute_dialog_stack() {
    let mut app = TestApp::builder().build();

    assert!(app.run_lua(
        r#"
        local leaf = smelt.dialog.content({ text = "Review this action" })
        local ok = pcall(function()
          smelt.dialog.new({
            panels = { { leaf = leaf, height = "fit" } },
            min_height = "invalid",
          })
        end)
        assert(not ok, "invalid dialog constraint should fail")
        assert(smelt.dialog.current() == nil, "failed dialog leaked into stack")
        "#
    ));
    assert!(app.active_docked_dialog().is_none());
}

#[test]
fn root_dialog_can_disable_top_edge_resize() {
    let mut app = TestApp::builder().build();
    assert!(app.run_lua(
        r#"
        local leaf = smelt.dialog.content({ text = "Fixed dialog" })
        smelt.dialog.new({
          panels = { { leaf = leaf, height = "fit" } },
          resizable = false,
        })
        "#
    ));

    let dialog = app.active_docked_dialog().expect("active dialog");
    assert!(
        !app.ui_probe()
            .docked_surface(dialog)
            .expect("docked surface")
            .resize_config()
            .top
    );
}

#[test]
fn root_dialog_uses_safe_fallback_when_custom_composer_omits_it() {
    let mut app = TestApp::builder().build();
    open_root_test_dialog(&mut app);
    assert!(app.run_lua(
        r#"
        smelt.ui.layout.set(function()
          return smelt.ui.layout.leaf(smelt.win.PROMPT)
        end)
        "#
    ));

    app.render_silent();

    assert_safe_dialog_fallback(&app);
    let dialog = app.active_docked_dialog().expect("active dialog");
    let root = app
        .ui_probe()
        .modal_leaves(
            app.ui_probe()
                .docked_surface(dialog)
                .expect("docked surface")
                .modal(),
        )
        .and_then(|leaves| leaves.first())
        .copied()
        .expect("dialog root");
    assert!(app.ui_probe().split_rect(root).is_some());
}

#[test]
fn root_dialog_uses_safe_fallback_when_custom_composer_duplicates_it() {
    let mut app = TestApp::builder().build();
    open_root_test_dialog(&mut app);
    assert!(app.run_lua(
        r#"
        smelt.ui.layout.set(function(state)
          if not state.dialog then
            return smelt.ui.layout.leaf(smelt.win.PROMPT)
          end
          return smelt.ui.layout.vbox({
            { state.dialog, height = "fill" },
            { state.dialog, height = "fill" },
          })
        end)
        "#
    ));

    app.render_silent();

    assert_safe_dialog_fallback(&app);
}

#[test]
fn root_dialog_rejects_retained_stage_from_nested_dialog() {
    let mut app = TestApp::builder().build();
    open_root_test_dialog(&mut app);
    assert!(app.run_lua(
        r#"
        local retained
        smelt.ui.layout.set(function(state)
          if not state.dialog then
            return smelt.ui.layout.leaf(smelt.win.PROMPT)
          end
          if not retained then
            retained = state.dialog
            return state.dialog
          end
          return smelt.ui.layout.vbox({
            { retained, height = "fill" },
            { state.dialog, height = "fill" },
          })
        end)
        "#
    ));

    open_root_test_dialog(&mut app);
    app.render_silent();

    assert_safe_dialog_fallback(&app);
}

#[test]
fn root_dialog_hides_and_restores_queued_input_chrome() {
    let mut app = TestApp::builder().build();
    app.start_turn(1);
    app.type_text("queued follow-up");
    app.press(KeyCode::Enter);
    assert_eq!(app.state().queued_inputs, vec!["queued follow-up"]);
    assert!(app.render_to_frame().text().contains("queued follow-up"));

    open_root_test_dialog(&mut app);

    assert_eq!(app.state().queued_inputs, vec!["queued follow-up"]);
    assert!(!app.render_to_frame().text().contains("queued follow-up"));

    assert!(app.run_lua("smelt.dialog.current().close()"));

    assert_eq!(app.state().queued_inputs, vec!["queued follow-up"]);
    assert!(app.render_to_frame().text().contains("queued follow-up"));
}

#[test]
fn expanding_root_dialog_preserves_focused_panel() {
    let mut app = TestApp::builder().build();
    assert!(app.run_lua(
        r#"
        local first = smelt.dialog.content({ text = "First panel", interactive = true })
        local second = smelt.dialog.content({ text = "Second panel", interactive = true })
        smelt.dialog.new({
          panels = {
            { leaf = first, height = "fit" },
            { leaf = second, height = "fit" },
          },
        })
        second:focus()
        "#
    ));
    app.render_silent();
    let focused = app.ui_probe().focus().expect("second dialog panel focused");

    app.press_mod(KeyCode::Char('o'), KeyModifiers::CONTROL);

    assert_eq!(app.ui_probe().focus(), Some(focused));
    assert!(app
        .active_docked_dialog()
        .and_then(|dialog| app.ui_probe().docked_surface(dialog))
        .is_some_and(|dialog| dialog.expanded()));
}

#[test]
fn root_dialog_pauses_notification_visibility_and_ttl() {
    let mut app = TestApp::builder().build();
    app.notify_error("deferred notification".into());
    assert!(app.notification_win().is_some());

    open_root_test_dialog(&mut app);

    assert!(app.notification_win().is_none());
    assert!(app.overlays_probe().suspended_notification().is_some());
    app.clock.advance(std::time::Duration::from_millis(
        crate::app::NOTIFICATION_TTL_MS * 2,
    ));
    assert!(!app.dismiss_expired_notification());

    assert!(app.run_lua("smelt.dialog.current().close()"));

    assert!(app.overlays_probe().suspended_notification().is_none());
    assert!(app.notification_win().is_some());
}

#[test]
fn root_dialog_preserves_notification_scope() {
    let mut app = TestApp::builder().build();
    let session_id = app.session_snapshot().id.clone();
    app.notify_session_save_failure(&session_id, "database busy");

    open_root_test_dialog(&mut app);

    assert!(app
        .overlays_probe()
        .suspended_notification()
        .is_some_and(|notification| {
            matches!(
                &notification.scope,
                crate::app::NotificationScope::Operation(
                    crate::app::NotificationOperation::SessionPersistence(owner_session_id)
                ) if owner_session_id == &session_id
            )
        }));

    assert!(app.run_lua("smelt.dialog.current().close()"));

    assert!(app
        .overlays_probe()
        .notification()
        .is_some_and(|notification| {
            matches!(
                &notification.scope,
                crate::app::NotificationScope::Operation(
                    crate::app::NotificationOperation::SessionPersistence(owner_session_id)
                ) if owner_session_id == &session_id
            )
        }));
}

#[test]
fn reset_dismisses_suspended_session_notification_and_keeps_audit_log() {
    let mut app = TestApp::builder().build();
    app.notify_session_error_sticky("stale session failure".into());
    open_root_test_dialog(&mut app);
    assert!(app.overlays_probe().suspended_notification().is_some());

    app.reset_session();

    assert!(app.overlays_probe().suspended_notification().is_none());
    assert!(app.overlays_probe().notification().is_none());
    assert!(app.lua_messages_contain("stale session failure"));
}

#[test]
fn reset_preserves_suspended_application_notification() {
    let mut app = TestApp::builder().build();
    app.notify_application_error_sticky("application failure".into());
    open_root_test_dialog(&mut app);
    assert!(app
        .overlays_probe()
        .suspended_notification()
        .is_some_and(|notification| {
            matches!(
                &notification.scope,
                crate::app::NotificationScope::Application
            )
        }));

    app.reset_session();

    assert!(app
        .overlays_probe()
        .suspended_notification()
        .is_some_and(|notification| {
            matches!(
                &notification.scope,
                crate::app::NotificationScope::Application
            ) && notification.summary == "application failure"
        }));
}

#[test]
fn readonly_dialog_arrows_move_without_vim() {
    let mut app = TestApp::builder().build();
    assert!(app.run_lua(
        r#"
        local buf = smelt.buf.new({ readonly = true })
        buf:lines({ "alpha", "bravo", "charlie" })
        local leaf = smelt.dialog.content({ buf = buf, interactive = true, wrap = false })
        smelt.dialog.new({
          title = "viewer",
          height = 4,
          panels = { { leaf = leaf, height = "fill" } },
        })
        "#
    ));
    app.render_silent();

    let win_id = app.ui_probe().focus().expect("dialog leaf focused");
    assert_eq!(app.ui_probe().win(win_id).expect("window").cursor_row(), 0);
    assert_eq!(app.ui_probe().win(win_id).expect("window").cursor_col(), 0);

    app.press(KeyCode::Down);
    app.press(KeyCode::Right);
    assert_eq!(app.ui_probe().win(win_id).expect("window").cursor_row(), 1);
    assert_eq!(app.ui_probe().win(win_id).expect("window").cursor_col(), 1);

    app.press(KeyCode::Up);
    assert_eq!(app.ui_probe().win(win_id).expect("window").cursor_row(), 0);
    assert_eq!(app.ui_probe().win(win_id).expect("window").cursor_col(), 1);

    app.press(KeyCode::Left);
    assert_eq!(app.ui_probe().win(win_id).expect("window").cursor_col(), 0);
}

#[test]
fn vim_readonly_dialog_arrows_and_vim_motions_move_cursor() {
    let mut app = TestApp::builder().with_vim(true).build();
    assert!(app.run_lua(
        r#"
        local buf = smelt.buf.new({ readonly = true })
        buf:lines({ "alpha one", "bravo two", "charlie three", "delta four", "echo five", "foxtrot six" })
        local leaf = smelt.dialog.content({ buf = buf, interactive = true, wrap = false })
        smelt.dialog.new({
          title = "viewer",
          height = 4,
          panels = { { leaf = leaf, height = "fill" } },
        })
        "#
    ));
    app.render_silent();

    let win_id = app.ui_probe().focus().expect("dialog leaf focused");

    app.press(KeyCode::Down);
    app.press(KeyCode::Right);
    assert_eq!(app.ui_probe().win(win_id).expect("window").cursor_row(), 1);
    assert_eq!(app.ui_probe().win(win_id).expect("window").cursor_col(), 1);

    app.type_char('j');
    app.type_char('l');
    assert_eq!(app.ui_probe().win(win_id).expect("window").cursor_row(), 2);
    assert_eq!(app.ui_probe().win(win_id).expect("window").cursor_col(), 2);

    app.press(KeyCode::Up);
    app.press(KeyCode::Left);
    app.type_char('k');
    app.type_char('h');
    assert_eq!(app.ui_probe().win(win_id).expect("window").cursor_row(), 0);
    assert_eq!(app.ui_probe().win(win_id).expect("window").cursor_col(), 0);

    app.type_char('w');
    assert_eq!(app.ui_probe().win(win_id).expect("window").cursor_col(), 6);
    app.type_char('j');
    assert_eq!(app.ui_probe().win(win_id).expect("window").cursor_row(), 1);
    assert_eq!(app.ui_probe().win(win_id).expect("window").cursor_col(), 6);
}

#[test]
fn vim_readonly_dialog_visual_modes_copy_character_and_whole_buffer() {
    let mut app = TestApp::builder().with_vim(true).build();
    assert!(app.run_lua(
        r#"
        local buf = smelt.buf.new({ readonly = true })
        buf:lines({ "alpha beta", "gamma", "delta" })
        local leaf = smelt.dialog.content({ buf = buf, interactive = true, wrap = false })
        smelt.dialog.new({
          title = "viewer",
          height = 4,
          panels = { { leaf = leaf, height = "fill" } },
        })
        "#
    ));
    app.render_silent();

    let win_id = app.ui_probe().focus().expect("dialog leaf focused");

    app.type_char('v');
    app.type_char('e');
    assert_eq!(
        app.ui_probe().win(win_id).expect("window").vim_mode(),
        VimMode::Visual
    );
    app.type_char('y');
    assert_eq!(app.core_probe().clipboard.kill_ring.current(), "alpha");
    assert_eq!(
        app.core_probe().clipboard.kill_ring.last_clipboard_write(),
        Some("alpha")
    );
    assert_eq!(
        app.ui_probe().win(win_id).expect("window").vim_mode(),
        VimMode::Normal
    );

    app.type_char('g');
    app.type_char('g');
    app.type_char('V');
    assert_eq!(
        app.ui_probe().win(win_id).expect("window").vim_mode(),
        VimMode::VisualLine
    );
    app.type_char('G');
    app.type_char('y');

    assert_eq!(
        app.core_probe().clipboard.kill_ring.current(),
        "alpha beta\ngamma\ndelta"
    );
    assert_eq!(
        app.core_probe().clipboard.kill_ring.last_clipboard_write(),
        Some("alpha beta\ngamma\ndelta")
    );
    assert_eq!(
        app.ui_probe().win(win_id).expect("window").vim_mode(),
        VimMode::Normal
    );
    assert!(
        app.ui_probe().active_modal().is_some(),
        "copying must keep the dialog open"
    );
}

#[test]
fn vim_session_dialog_visual_yank_beats_copy_all_keymap() {
    let mut app = TestApp::builder().with_vim(true).build();
    assert!(app.run_lua(r#"smelt.cmd.run("session")"#));
    app.render_silent();

    let win_id = app.app.ui.focus().expect("session dialog focused");
    app.type_char('v');
    app.type_char('e');
    assert_eq!(
        app.app.ui.win(win_id).expect("session window").vim_mode(),
        VimMode::Visual
    );

    app.type_char('y');

    assert_eq!(app.app.core.clipboard.kill_ring.current(), "session");
    assert_eq!(
        app.app.ui.win(win_id).expect("session window").vim_mode(),
        VimMode::Normal
    );
    assert!(
        app.app.ui.active_modal().is_some(),
        "visual yank must not close the session dialog"
    );

    app.type_char('y');
    assert!(
        app.app
            .ui
            .win(win_id)
            .expect("session window")
            .vim_state()
            .is_idle(),
        "normal-mode y must retain the dialog's copy-all shortcut"
    );
}

#[test]
fn vim_visual_viewer_owned_keys_preempt_dialog_keymaps() {
    let mut app = TestApp::builder().with_vim(true).build();
    assert!(app.run_lua(
        r#"
        local buf = smelt.buf.new({ readonly = true })
        buf:lines({ "alpha beta", "gamma" })
        local leaf = smelt.dialog.content({ buf = buf, interactive = true, wrap = false })
        smelt.dialog.new({
          title = "viewer",
          height = 4,
          panels = { { leaf = leaf, height = "fill" } },
          keymaps = {
            { key = "l", on_press = function(ctx) ctx.close() end },
            { key = "q", on_press = function(ctx) ctx.close() end },
          },
        })
        "#
    ));
    app.render_silent();

    let win_id = app.app.ui.focus().expect("dialog leaf focused");
    app.type_char('v');
    app.type_char('l');

    assert!(
        app.app.ui.active_modal().is_some(),
        "a Vim-owned Visual key must not reach the dialog keymap"
    );
    assert_eq!(app.app.ui.win(win_id).expect("window").cursor_col(), 1);
    assert_eq!(
        app.app.ui.win(win_id).expect("window").vim_mode(),
        VimMode::Visual
    );

    app.type_char('q');
    assert!(
        app.app.ui.active_modal().is_none(),
        "a Visual key Vim passes through must reach the dialog keymap"
    );
}

#[test]
fn vim_visual_global_keymap_overrides_viewer_default() {
    let mut app = TestApp::builder().with_vim(true).build();
    assert!(app.run_lua(
        r#"
        smelt.keymap.set("v", "l", function()
          smelt.dialog.current().close()
        end)
        local buf = smelt.buf.new({ readonly = true })
        buf:lines({ "alpha beta", "gamma" })
        local leaf = smelt.dialog.content({ buf = buf, interactive = true, wrap = false })
        smelt.dialog.new({
          title = "viewer",
          height = 4,
          panels = { { leaf = leaf, height = "fill" } },
        })
        "#
    ));
    app.render_silent();

    app.type_char('v');
    app.type_char('l');

    assert!(
        app.app.ui.active_modal().is_none(),
        "an explicit global Visual mapping must override the viewer default"
    );
}

#[test]
fn vim_pending_viewer_sequence_preempts_dialog_keymap() {
    let mut app = TestApp::builder().with_vim(true).build();
    assert!(app.run_lua(
        r#"
        smelt.keymap.set("n", "q", function()
          smelt.dialog.current().close()
        end)
        local buf = smelt.buf.new({ readonly = true })
        buf:lines({ "alpha beta", "gamma" })
        local leaf = smelt.dialog.content({ buf = buf, interactive = true, wrap = false })
        smelt.dialog.new({
          title = "viewer",
          height = 4,
          panels = { { leaf = leaf, height = "fill" } },
          keymaps = {
            { key = "q", on_press = function(ctx) ctx.close() end },
          },
        })
        "#
    ));
    app.render_silent();

    let win_id = app.app.ui.focus().expect("dialog leaf focused");
    app.type_char('z');
    assert!(
        !app.app
            .ui
            .win(win_id)
            .expect("window")
            .vim_state()
            .is_idle(),
        "z should start a pending Vim sequence"
    );

    app.type_char('q');

    assert!(
        app.app.ui.active_modal().is_some(),
        "pending Vim input must not reach the dialog keymap"
    );
    assert!(
        app.app
            .ui
            .win(win_id)
            .expect("window")
            .vim_state()
            .is_idle(),
        "invalid pending input should cancel the Vim sequence"
    );
}

#[test]
fn vim_readonly_dialog_escape_exits_visual_before_dialog_keymap() {
    let mut app = TestApp::builder().with_vim(true).build();
    assert!(app.run_lua(
        r#"
        local buf = smelt.buf.new({ readonly = true })
        buf:lines({ "alpha beta", "gamma" })
        local leaf = smelt.dialog.content({ buf = buf, interactive = true, wrap = false })
        smelt.dialog.new({
          title = "viewer",
          height = 4,
          panels = { { leaf = leaf, height = "fill" } },
          keymaps = {
            { key = "<Esc>", on_press = function(ctx) ctx.close() end },
          },
        })
        "#
    ));
    app.render_silent();

    let win_id = app.ui_probe().focus().expect("dialog leaf focused");
    app.type_char('v');
    app.type_char('e');
    assert_eq!(
        app.ui_probe().win(win_id).expect("window").vim_mode(),
        VimMode::Visual
    );
    assert!(
        app.ui_probe()
            .win(win_id)
            .and_then(|win| app
                .ui_probe()
                .buf(win.buf)
                .and_then(|buf| win.selection_range(buf)))
            .is_some(),
        "visual-mode dialog viewer should have a selection range"
    );

    app.press(KeyCode::Esc);

    assert!(
        app.ui_probe().active_modal().is_some(),
        "Esc from Visual should not close the dialog"
    );
    assert_eq!(
        app.ui_probe().win(win_id).expect("window").vim_mode(),
        VimMode::Normal
    );
}

#[test]
fn vim_readonly_dialog_visual_selection_is_painted() {
    let mut app = TestApp::builder().with_vim(true).build();
    assert!(app.run_lua(
        r#"
        local buf = smelt.buf.new({ readonly = true })
        buf:lines({ "alpha beta", "gamma" })
        local leaf = smelt.dialog.content({ buf = buf, interactive = true, wrap = false })
        smelt.dialog.new({
          title = "viewer",
          height = 4,
          panels = { { leaf = leaf, height = "fill" } },
        })
        "#
    ));
    app.render_silent();

    let win_id = app.ui_probe().focus().expect("dialog leaf focused");
    app.type_char('v');
    app.type_char('e');
    assert!(
        !app.ui_probe()
            .win(win_id)
            .expect("window")
            .has_materialized_rows(),
        "dialog viewer should stay byte-backed after visual motions"
    );
    let frame = app.render_to_frame();
    let win = app.ui_probe().win(win_id).expect("window");
    let rect = win.viewport.expect("dialog viewport").rect;
    let selection_bg = app.ui_probe().theme().get("Visual").bg;
    let painted = (rect.top..rect.top + rect.height).any(|row| {
        (rect.left..rect.left + rect.width)
            .any(|col| frame.styles[row as usize][col as usize].bg == selection_bg)
    });

    assert!(painted, "visual selection should be painted inside dialog");
}
fn tool_result<'a>(app: &'a TestApp, call_id: &str) -> Option<(&'a str, bool)> {
    app.actions().iter().rev().find_map(|action| match action {
        Action::EngineSend(cmd) => match cmd.as_ref() {
            protocol::UiCommand::ToolResult {
                call_id: id,
                content,
                is_error,
                ..
            } if id == call_id => Some((content.as_str(), *is_error)),
            _ => None,
        },
        _ => None,
    })
}

fn confirm_test_permissions() -> smelt_core::permissions::Permissions {
    use smelt_core::permissions::rules::{RawModePerms, RawPerms, RawRuleSet, ToolDefaults};
    let mut modes = std::collections::HashMap::new();
    modes.insert(
        "apply".to_string(),
        RawModePerms {
            tools: RawRuleSet {
                allow: vec!["test_tool".into()],
                ..Default::default()
            },
            ..Default::default()
        },
    );
    modes.insert(
        "deny".to_string(),
        RawModePerms {
            tools: RawRuleSet {
                deny: vec!["test_tool".into()],
                ..Default::default()
            },
            ..Default::default()
        },
    );
    smelt_core::permissions::Permissions::from_raw(
        &RawPerms {
            default: RawModePerms::default(),
            modes,
        },
        &ToolDefaults::default(),
    )
}

fn confirm_req(request_id: u64) -> smelt_core::ConfirmRequest {
    smelt_core::ConfirmRequest {
        invocation_id: protocol::InvocationId::new(request_id),
        call_id: format!("call-{request_id}"),
        tool_name: "test_tool".into(),
        args: std::collections::HashMap::new(),
        tool_paths: Vec::new(),
        tool_paths_error: None,
        approval_candidates: Vec::new(),
        grant_options: Vec::new(),
        summary: protocol::StyledLines::from_plain("test tool"),
        request_id,
    }
}

fn install_confirm_test_permissions(app: &mut TestApp) {
    app.replace_permissions_for_harness(confirm_test_permissions());
}

fn permission_decisions(cmds: Vec<protocol::UiCommand>) -> Vec<(u64, bool)> {
    cmds.into_iter()
        .filter_map(|cmd| match cmd {
            protocol::UiCommand::PermissionDecision {
                request_id,
                approved,
                ..
            } => Some((request_id, approved)),
            _ => None,
        })
        .collect()
}

fn dispatch_confirm_request(
    app: &mut TestApp,
    req: smelt_core::ConfirmRequest,
    pending: &mut Vec<crate::app::PendingTool>,
) -> crate::app::SessionControl {
    app.dispatch_confirm_request(req, pending)
}

fn actions_permission_decisions(actions: &[Action]) -> Vec<(u64, bool)> {
    actions
        .iter()
        .filter_map(|action| match action {
            Action::EngineSend(cmd) => match cmd.as_ref() {
                protocol::UiCommand::PermissionDecision {
                    request_id,
                    approved,
                    ..
                } => Some((*request_id, *approved)),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn bash_permission_args(command: &str) -> std::collections::HashMap<String, serde_json::Value> {
    std::collections::HashMap::from([("command".into(), serde_json::json!(command))])
}

fn request_bash_permission(
    app: &mut TestApp,
    request_id: u64,
    command: &str,
    approval_patterns: Vec<&str>,
) {
    app.feed_one(SourceEvent::engine(
        protocol::EngineEvent::RequestPermission {
            invocation_id: protocol::InvocationId::new(request_id),
            request_id,
            call_id: format!("call-{request_id}"),
            tool_name: "bash".into(),
            args: bash_permission_args(command),
            approval_patterns: approval_patterns.into_iter().map(str::to_string).collect(),
            called_at_ms: 0,
            summary: protocol::StyledLines::from_plain(command.to_string()),
        },
    ));
}

struct GitWorktreeFixture {
    repo: tempfile::TempDir,
    worktrees: tempfile::TempDir,
    feature: std::path::PathBuf,
}

impl GitWorktreeFixture {
    fn new() -> Self {
        let repo = tempfile::tempdir().unwrap();
        let worktrees = tempfile::tempdir().unwrap();
        Self::run_git(repo.path(), &["init", "-b", "main"]);
        std::fs::write(repo.path().join("README.md"), "test\n").unwrap();
        Self::run_git(repo.path(), &["add", "README.md"]);
        Self::run_git(repo.path(), &["commit", "-m", "initial"]);
        let feature = worktrees.path().join("feature");
        Self::add_worktree(repo.path(), &feature, "feature");
        Self {
            repo,
            worktrees,
            feature,
        }
    }

    fn repository_root(&self) -> std::path::PathBuf {
        std::fs::canonicalize(self.repo.path()).unwrap()
    }

    fn repository_key(&self) -> std::path::PathBuf {
        std::fs::canonicalize(self.repo.path().join(".git")).unwrap()
    }

    fn add_sibling(&self) -> std::path::PathBuf {
        let sibling = self.worktrees.path().join("sibling");
        Self::add_worktree(self.repo.path(), &sibling, "sibling");
        sibling
    }

    fn add_worktree(repo: &std::path::Path, path: &std::path::Path, branch: &str) {
        Self::run_git(
            repo,
            &["worktree", "add", "-b", branch, path.to_str().unwrap()],
        );
    }

    fn run_git(cwd: &std::path::Path, args: &[&str]) {
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
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

fn open_repository_permission_dialog(fixture: &GitWorktreeFixture) -> TestApp {
    let mut app = TestApp::builder().with_cwd(&fixture.feature).build();
    app.start_submitted_turn("run tests");
    request_bash_permission(&mut app, 9, "cargo test", vec!["cargo test *"]);
    app
}

fn is_cargo_test_grant(grants: &[smelt_core::permissions::PermissionGrant]) -> bool {
    matches!(
        grants,
        [smelt_core::permissions::PermissionGrant::Command { tool, pattern }]
            if tool == "bash" && pattern == "cargo test *"
    )
}

#[test]
fn worktree_permission_dialog_offers_repository_scope_with_main_checkout_label() {
    let fixture = GitWorktreeFixture::new();
    let mut app = open_repository_permission_dialog(&fixture);
    let handle_id = app.first_pending_confirm().expect("permission dialog");
    let options = &app
        .core_probe()
        .confirms
        .get(handle_id)
        .expect("confirm request")
        .req
        .grant_options;
    let repository = options
        .iter()
        .find(|option| matches!(option.target, smelt_core::ApprovalTarget::Repository { .. }))
        .expect("repository approval option");

    assert_eq!(
        repository.target,
        smelt_core::ApprovalTarget::Repository {
            key: fixture.repository_key(),
        }
    );
    assert_eq!(
        repository.label,
        format!(
            "allow cargo test * in project {} (all worktrees)",
            fixture.repository_root().display()
        )
    );
    let workspace = options
        .iter()
        .find(|option| matches!(option.target, smelt_core::ApprovalTarget::Workspace { .. }))
        .expect("worktree approval option");
    assert_eq!(
        workspace.label,
        format!(
            "allow cargo test * in this worktree {}",
            fixture.feature.display()
        )
    );
    let frame = app.render_to_frame().text();
    assert!(frame.contains("in project"));
    assert!(frame.contains("all worktrees"));
}

#[test]
fn repository_approval_persists_under_git_common_directory_key() {
    let fixture = GitWorktreeFixture::new();
    let mut app = open_repository_permission_dialog(&fixture);

    assert!(app.resolve_first_grant_where(
        |target| matches!(target, smelt_core::ApprovalTarget::Repository { .. }),
        is_cargo_test_grant,
    ));
    let store = &app.core_probe().permission_store;
    assert!(store
        .load(
            &fixture.feature.to_string_lossy(),
            smelt_core::permissions::store::PersistenceScope::Workspace,
        )
        .unwrap()
        .is_empty());
    assert_eq!(
        store
            .load(
                &fixture.repository_key().to_string_lossy(),
                smelt_core::permissions::store::PersistenceScope::Repository,
            )
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn running_peer_in_same_workspace_lazily_loads_new_workspace_approval() {
    let runtime_home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let mut granter = TestApp::builder()
        .with_runtime_home(runtime_home.path())
        .with_cwd(workspace.path())
        .build();
    let mut peer = TestApp::builder()
        .with_runtime_home(runtime_home.path())
        .with_cwd(workspace.path())
        .build();

    granter.start_submitted_turn("run tests");
    request_bash_permission(&mut granter, 40, "cargo test", vec!["cargo test *"]);
    assert!(granter.resolve_first_grant_where(
        |target| matches!(target, smelt_core::ApprovalTarget::Workspace { .. }),
        is_cargo_test_grant,
    ));

    peer.start_submitted_turn("run tests");
    request_bash_permission(&mut peer, 41, "cargo test", vec!["cargo test *"]);

    assert_eq!(peer.pending_confirm_count(), 0);
    assert_eq!(
        actions_permission_decisions(peer.actions()),
        vec![(41, true)]
    );

    granter
        .core_probe()
        .permission_store
        .remove(
            granter.cwd_str(),
            smelt_core::permissions::store::PersistenceScope::Workspace,
            "bash",
            "cargo test *",
        )
        .unwrap();
    request_bash_permission(&mut peer, 42, "cargo test", vec!["cargo test *"]);
    assert_eq!(peer.pending_confirm_count(), 1);
}

#[test]
fn persistent_permission_failure_is_reported_and_does_not_run_tool() {
    let mut app = TestApp::builder().build();
    app.core_probe()
        .permission_store
        .add_tool(
            app.cwd_str(),
            smelt_core::permissions::store::PersistenceScope::Workspace,
            "bash",
            vec!["existing command".into()],
        )
        .unwrap();
    let permission_dir = std::fs::read_dir(
        app.runtime_home()
            .join("state")
            .join("smelt")
            .join("workspaces"),
    )
    .unwrap()
    .next()
    .unwrap()
    .unwrap()
    .path();
    let lock_path = permission_dir.join("permissions.lock");
    std::fs::remove_file(&lock_path).unwrap();
    std::fs::create_dir(&lock_path).unwrap();

    app.start_submitted_turn("run tests");
    request_bash_permission(&mut app, 45, "cargo test", vec!["cargo test *"]);
    assert!(app.resolve_first_grant_where(
        |target| matches!(target, smelt_core::ApprovalTarget::Workspace { .. }),
        is_cargo_test_grant,
    ));

    assert_eq!(
        actions_permission_decisions(app.actions()),
        vec![(45, false)]
    );
    assert!(app.lua_messages_contain("permission was not saved:"));
    assert_eq!(
        app.core_probe()
            .permission_store
            .load(
                app.cwd_str(),
                smelt_core::permissions::store::PersistenceScope::Workspace,
            )
            .unwrap(),
        vec![smelt_core::permissions::store::Rule {
            tool: "bash".into(),
            patterns: vec!["existing command".into()],
        }]
    );
}

#[test]
fn successful_persistent_grant_is_approved_when_other_scope_is_malformed() {
    let fixture = GitWorktreeFixture::new();
    let runtime_home = tempfile::tempdir().unwrap();
    let mut app = TestApp::builder()
        .with_runtime_home(runtime_home.path())
        .with_cwd(&fixture.feature)
        .build();
    app.core_probe()
        .permission_store
        .add_tool(
            &fixture.repository_key().to_string_lossy(),
            smelt_core::permissions::store::PersistenceScope::Repository,
            "bash",
            vec!["git status".into()],
        )
        .unwrap();
    let stores_dir = runtime_home.path().join("state/smelt/workspaces");
    let repository_permissions = std::fs::read_dir(stores_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path().join("repository-permissions.json"))
        .find(|path| path.is_file())
        .unwrap();
    app.start_submitted_turn("run tests");
    request_bash_permission(&mut app, 46, "cargo test", vec!["cargo test *"]);
    std::fs::write(repository_permissions, "{not json").unwrap();
    assert!(app.resolve_first_grant_where(
        |target| matches!(target, smelt_core::ApprovalTarget::Workspace { .. }),
        is_cargo_test_grant,
    ));

    assert_eq!(
        actions_permission_decisions(app.actions()),
        vec![(46, true)]
    );
    assert_eq!(
        app.core_probe()
            .permission_store
            .load(
                app.cwd_str(),
                smelt_core::permissions::store::PersistenceScope::Workspace,
            )
            .unwrap(),
        vec![smelt_core::permissions::store::Rule {
            tool: "bash".into(),
            patterns: vec!["cargo test *".into()],
        }]
    );
}

#[test]
fn running_sibling_worktree_lazily_loads_new_repository_approval() {
    let fixture = GitWorktreeFixture::new();
    let sibling = fixture.add_sibling();
    let runtime_home = tempfile::tempdir().unwrap();
    let mut granter = TestApp::builder()
        .with_runtime_home(runtime_home.path())
        .with_cwd(&fixture.feature)
        .build();
    let mut peer = TestApp::builder()
        .with_runtime_home(runtime_home.path())
        .with_cwd(sibling)
        .build();

    granter.start_submitted_turn("run tests");
    request_bash_permission(&mut granter, 50, "cargo test", vec!["cargo test *"]);
    assert!(granter.resolve_first_grant_where(
        |target| matches!(target, smelt_core::ApprovalTarget::Repository { .. }),
        is_cargo_test_grant,
    ));

    peer.start_submitted_turn("run tests");
    request_bash_permission(&mut peer, 51, "cargo test", vec!["cargo test *"]);

    assert_eq!(peer.pending_confirm_count(), 0);
    assert_eq!(
        actions_permission_decisions(peer.actions()),
        vec![(51, true)]
    );
}

#[test]
fn permissions_lua_transactional_revoke_preserves_concurrent_rules() {
    let fixture = GitWorktreeFixture::new();
    let mut app = open_repository_permission_dialog(&fixture);
    assert!(app.resolve_first_grant_where(
        |target| matches!(target, smelt_core::ApprovalTarget::Repository { .. }),
        is_cargo_test_grant,
    ));

    assert!(app.run_lua(
        r#"
        _G.stale_permissions = smelt.permissions.list()
        assert(#_G.stale_permissions.repository == 1)
        "#
    ));
    app.core_probe()
        .permission_store
        .add_tool(
            &fixture.repository_key().to_string_lossy(),
            smelt_core::permissions::store::PersistenceScope::Repository,
            "web_fetch",
            vec!["https://example.com/*".into()],
        )
        .unwrap();

    assert!(app.run_lua(
        r#"
        local stale = _G.stale_permissions.repository[1]
        assert(smelt.permissions.revoke({
          scope = "repository",
          tool = stale.tool,
          pattern = stale.patterns[1],
        }))
        "#
    ));
    assert_eq!(
        app.core_probe()
            .permission_store
            .load(
                &fixture.repository_key().to_string_lossy(),
                smelt_core::permissions::store::PersistenceScope::Repository,
            )
            .unwrap(),
        vec![smelt_core::permissions::store::Rule {
            tool: "web_fetch".into(),
            patterns: vec!["https://example.com/*".into()],
        }]
    );
}

#[test]
fn permissions_lua_sync_rejects_stale_repository_snapshot() {
    let fixture = GitWorktreeFixture::new();
    let mut app = open_repository_permission_dialog(&fixture);
    assert!(app.resolve_first_grant_where(
        |target| matches!(target, smelt_core::ApprovalTarget::Repository { .. }),
        is_cargo_test_grant,
    ));
    assert!(app.run_lua(
        r#"
        _G.stale_permissions = smelt.permissions.list()
        assert(#_G.stale_permissions.repository == 1)
        "#
    ));
    app.core_probe()
        .permission_store
        .add_tool(
            &fixture.repository_key().to_string_lossy(),
            smelt_core::permissions::store::PersistenceScope::Repository,
            "web_fetch",
            vec!["https://example.com/*".into()],
        )
        .unwrap();

    assert!(app.run_lua(
        r#"
        local ok, err = pcall(function()
          smelt.permissions.sync({
            repository = {
              revision = _G.stale_permissions.repository_revision,
              rules = {},
            },
          })
        end)
        assert(not ok)
        assert(tostring(err):find("changed since it was read", 1, true))
        "#
    ));
    assert_eq!(
        app.core_probe()
            .permission_store
            .load(
                &fixture.repository_key().to_string_lossy(),
                smelt_core::permissions::store::PersistenceScope::Repository,
            )
            .unwrap(),
        vec![
            smelt_core::permissions::store::Rule {
                tool: "bash".into(),
                patterns: vec!["cargo test *".into()],
            },
            smelt_core::permissions::store::Rule {
                tool: "web_fetch".into(),
                patterns: vec!["https://example.com/*".into()],
            },
        ]
    );
}

#[test]
fn permissions_lua_sync_rejects_two_persisted_scopes_without_writing_either() {
    let fixture = GitWorktreeFixture::new();
    let mut app = TestApp::builder().with_cwd(&fixture.feature).build();
    app.core_probe()
        .permission_store
        .add_tool(
            app.cwd_str(),
            smelt_core::permissions::store::PersistenceScope::Workspace,
            "bash",
            vec!["workspace command".into()],
        )
        .unwrap();
    app.core_probe()
        .permission_store
        .add_tool(
            &fixture.repository_key().to_string_lossy(),
            smelt_core::permissions::store::PersistenceScope::Repository,
            "bash",
            vec!["repository command".into()],
        )
        .unwrap();

    assert!(app.run_lua(
        r#"
        local permissions = smelt.permissions.list()
        local ok, err = pcall(function()
          smelt.permissions.sync({
            workspace = {
              revision = permissions.workspace_revision,
              rules = {},
            },
            repository = {
              revision = permissions.repository_revision,
              rules = {},
            },
          })
        end)
        assert(not ok)
        assert(tostring(err):find("one persisted permission scope", 1, true))
        "#
    ));
    assert_eq!(
        app.core_probe()
            .permission_store
            .load(
                app.cwd_str(),
                smelt_core::permissions::store::PersistenceScope::Workspace,
            )
            .unwrap()[0]
            .patterns,
        vec!["workspace command"]
    );
    assert_eq!(
        app.core_probe()
            .permission_store
            .load(
                &fixture.repository_key().to_string_lossy(),
                smelt_core::permissions::store::PersistenceScope::Repository,
            )
            .unwrap()[0]
            .patterns,
        vec!["repository command"]
    );
}

#[test]
fn external_sibling_worktree_inherits_repository_but_not_workspace_rules() {
    let fixture = GitWorktreeFixture::new();
    let app = TestApp::builder().with_cwd(&fixture.feature).build();
    let store = &app.core_probe().permission_store;
    store
        .add_grant(
            &fixture.repository_key().to_string_lossy(),
            smelt_core::permissions::store::PersistenceScope::Repository,
            smelt_core::permissions::PermissionGrant::Command {
                tool: "bash".into(),
                pattern: "cargo test *".into(),
            },
        )
        .unwrap();
    store
        .add_tool(
            &fixture.feature.to_string_lossy(),
            smelt_core::permissions::store::PersistenceScope::Workspace,
            "bash",
            vec!["worktree-only *".into()],
        )
        .unwrap();
    let sibling = fixture.add_sibling();

    let resolution = smelt_core::permissions::resolve_permissions(
        &smelt_core::permissions::rules::RawPerms::default(),
        &smelt_core::permissions::rules::ToolDefaults::default(),
        std::collections::HashMap::new(),
        &smelt_core::config::ResolvedSettings::default(),
        smelt_core::permissions::PermissionRuntimePaths {
            cwd: &sibling,
            home: app.runtime_home(),
        },
        store,
        None,
    );
    let permissions =
        smelt_core::permissions::PermissionsHandle::from_resolution(resolution.unwrap());
    let approvals = permissions.approvals();
    let approvals = approvals.read().unwrap();
    assert!(approvals.has_pattern("bash", "cargo test *"));
    assert!(!approvals.has_pattern("bash", "worktree-only *"));
}

#[test]
fn request_permission_auto_allows_when_applied_turn_mode_allows() {
    let mut app = TestApp::builder().build();
    install_confirm_test_permissions(&mut app);
    app.start_turn(1);
    app.set_applied_mode(protocol::AgentMode::parse("apply").unwrap());

    let mut pending = Vec::new();
    let ctrl = dispatch_confirm_request(&mut app, confirm_req(10), &mut pending);

    assert!(matches!(ctrl, crate::app::SessionControl::Continue));
    assert_eq!(app.pending_confirm_count(), 0);
    assert_eq!(
        permission_decisions(app.drain_engine_sends()),
        vec![(10, true)]
    );
}

#[test]
fn request_permission_auto_denies_when_applied_turn_mode_denies() {
    let mut app = TestApp::builder().build();
    install_confirm_test_permissions(&mut app);
    app.start_turn(1);
    app.set_applied_mode(protocol::AgentMode::parse("deny").unwrap());

    let mut pending = vec![crate::app::PendingTool {
        invocation_id: protocol::InvocationId::new(11),
        name: "test_tool".into(),
    }];
    let ctrl = dispatch_confirm_request(&mut app, confirm_req(11), &mut pending);

    assert!(matches!(ctrl, crate::app::SessionControl::Continue));
    assert_eq!(app.pending_confirm_count(), 0);
    assert!(pending.is_empty());
    assert!(app.agent_running());
    assert_eq!(
        permission_decisions(app.drain_engine_sends()),
        vec![(11, false)]
    );
}

#[test]
fn canonical_history_rebuild_waits_for_pending_permission_tool() {
    let mut app = TestApp::builder().build();
    install_confirm_test_permissions(&mut app);
    app.start_submitted_turn("prompt");
    let turn_id = app.current_turn_id().expect("submitted turn is active");

    app.feed_one(SourceEvent::engine(
        protocol::EngineEvent::RequestPermission {
            invocation_id: protocol::InvocationId::new(12),
            request_id: 12,
            call_id: "call-12".into(),
            tool_name: "test_tool".into(),
            args: std::collections::HashMap::new(),
            approval_patterns: Vec::new(),
            called_at_ms: 0,
            summary: protocol::StyledLines::from_plain("test tool"),
        },
    ));
    assert_eq!(
        app.pending_tool_invocation_ids(),
        vec![protocol::InvocationId::new(12)]
    );

    app.feed_one(SourceEvent::engine(protocol::EngineEvent::HistoryUpdated {
        turn_id,
        update: protocol::CanonicalHistoryDelta::new(0, Vec::new()),
    }));

    assert_eq!(
        app.app
            .conversation
            .pending_transcript_history_rebuild_from(),
        Some(0)
    );
    app.assert_invariants();

    app.resolve_first_confirm(false, Some("skip".into()));
    assert!(app.pending_tool_invocation_ids().is_empty());
    app.feed_one(SourceEvent::engine(protocol::EngineEvent::HistoryUpdated {
        turn_id,
        update: protocol::CanonicalHistoryDelta::new(0, Vec::new()),
    }));

    assert_eq!(
        app.app
            .conversation
            .pending_transcript_history_rebuild_from(),
        None
    );
    assert_eq!(app.transcript_block_count(), 0);
    app.assert_invariants();
}

#[test]
fn bash_session_grants_for_cd_prefixed_outside_path_survive_cancel_and_rewind() {
    let outside = tempfile::TempDir::new().expect("create outside-workspace directory");
    let outside_dir = outside.path().join("shared-data");
    std::fs::create_dir_all(&outside_dir).expect("create outside data directory");
    let outside_dir = std::fs::canonicalize(outside_dir).expect("canonicalize outside dir");
    let outside_file = outside_dir.join("sample.input");
    std::fs::write(&outside_file, "fixture\n").expect("write outside-workspace file");

    let mut app = TestApp::builder().build();
    let approval_pattern = "python3 *";
    let command = format!(
        "cd {} && python3 -m demo_tool --input {} --dry-run",
        app.cwd_str(),
        outside_file.display()
    );
    app.restrict_permissions_to_cwd();
    app.start_submitted_turn("process outside data");
    let history_idx = app.app.session_history_len().saturating_sub(1);

    request_bash_permission(&mut app, 21, &command, vec![approval_pattern]);
    assert_eq!(app.pending_confirm_count(), 1);
    let before = app.actions().len();
    assert!(
        app.resolve_first_session_grant_where(|grants| {
            let has_command_grant = grants.iter().any(|grant| {
                matches!(
                    grant,
                    smelt_core::permissions::PermissionGrant::Command { tool, pattern }
                        if tool == "bash" && pattern == approval_pattern
                )
            });
            let has_path_grant = grants.iter().any(|grant| {
                matches!(
                    grant,
                    smelt_core::permissions::PermissionGrant::PathPrefix { dir }
                        if dir == &outside_dir
                )
            });
            has_command_grant && has_path_grant
        }),
        "matching session command and path grant option"
    );
    assert_eq!(
        actions_permission_decisions(app.actions_since(before)),
        vec![(21, true)]
    );

    app.cancel();
    app.rewind_to_history_index(Some(history_idx), false);

    let before = app.actions().len();
    app.start_submitted_turn("process outside data");
    request_bash_permission(&mut app, 22, &command, vec![approval_pattern]);

    assert_eq!(app.pending_confirm_count(), 0);
    assert_eq!(
        actions_permission_decisions(app.actions_since(before)),
        vec![(22, true)]
    );
}

#[test]
fn command_session_grant_survives_cancel_and_rewind() {
    let approval_pattern = "python3 *";
    let mut app = TestApp::builder().build();
    let command = format!("cd {} && python3 --version", app.cwd_str());
    app.start_submitted_turn("check python");
    let history_idx = app.app.session_history_len().saturating_sub(1);

    request_bash_permission(&mut app, 31, &command, vec![approval_pattern]);
    assert_eq!(app.pending_confirm_count(), 1);
    let before = app.actions().len();
    assert!(
        app.resolve_first_session_grant_where(|grants| {
            grants.iter().any(|grant| {
                matches!(
                    grant,
                    smelt_core::permissions::PermissionGrant::Command { tool, pattern }
                        if tool == "bash" && pattern == approval_pattern
                )
            })
        }),
        "matching session command grant option"
    );
    assert_eq!(
        actions_permission_decisions(app.actions_since(before)),
        vec![(31, true)]
    );

    app.cancel();
    app.rewind_to_history_index(Some(history_idx), false);

    let before = app.actions().len();
    app.start_submitted_turn("check python");
    request_bash_permission(&mut app, 32, &command, vec![approval_pattern]);

    assert_eq!(app.pending_confirm_count(), 0);
    assert_eq!(
        actions_permission_decisions(app.actions_since(before)),
        vec![(32, true)]
    );
}

fn evaluate_tool(
    app: &mut TestApp,
    request_id: u64,
    call_id: &str,
    tool_name: &str,
    args: std::collections::HashMap<String, serde_json::Value>,
    mode: protocol::AgentMode,
) -> protocol::Decision {
    app.feed_one(SourceEvent::engine(
        protocol::EngineEvent::ToolEvaluationRequest {
            invocation_id: protocol::InvocationId::new(request_id),
            request_id,
            call_id: call_id.into(),
            tool_name: tool_name.into(),
            args,
            mode,
        },
    ));

    app.actions()
        .iter()
        .rev()
        .find_map(|action| match action {
            Action::EngineSend(command) => match command.as_ref() {
                protocol::UiCommand::ToolEvaluationResponse {
                    request_id: response_id,
                    evaluation,
                } if *response_id == request_id => Some(evaluation.decision.clone()),
                _ => None,
            },
            _ => None,
        })
        .expect("tool evaluation response")
}

#[test]
fn tool_evaluation_uses_the_mode_carried_by_the_turn_event() {
    let mut app = TestApp::builder().build();
    install_confirm_test_permissions(&mut app);
    app.start_turn(1);
    app.set_configured_agent_mode_for_harness(protocol::AgentMode::parse("deny").unwrap());
    let _ = app.drain_engine_sends();

    let decision = evaluate_tool(
        &mut app,
        91,
        "call-91",
        "test_tool",
        std::collections::HashMap::new(),
        protocol::AgentMode::parse("apply").unwrap(),
    );
    assert_eq!(decision, protocol::Decision::Allow);
}

#[test]
fn switch_cwd_outside_workspace_requires_destination_permission() {
    let environment_guard = test_environment_guard();
    let mut app = TestApp::builder().build_with_test_environment_guard(&environment_guard);
    let target = app.runtime_home().join("outside-project");
    std::fs::create_dir_all(&target).expect("create outside-workspace directory");
    let target = std::fs::canonicalize(target).expect("canonicalize target directory");
    let raw_target = "~/outside-project";
    let args = std::collections::HashMap::from([(
        "path".to_string(),
        serde_json::Value::String(raw_target.to_string()),
    )]);

    let original_cwd = app.cwd_str().to_owned();
    app.restrict_permissions_to_cwd();
    app.approve_tool_for_session("switch_cwd");
    app.start_turn(1);
    let _ = app.drain_engine_sends();

    let decision = evaluate_tool(
        &mut app,
        90,
        "switch-cwd",
        "switch_cwd",
        args.clone(),
        protocol::AgentMode::parse("yolo").unwrap(),
    );
    assert_eq!(decision, protocol::Decision::Ask);

    app.feed_one(SourceEvent::engine(
        protocol::EngineEvent::RequestPermission {
            invocation_id: protocol::InvocationId::new(90),
            request_id: 91,
            call_id: "switch-cwd".into(),
            tool_name: "switch_cwd".into(),
            args: args.clone(),
            approval_patterns: Vec::new(),
            called_at_ms: 0,
            summary: protocol::StyledLines::from_plain(target.to_string_lossy()),
        },
    ));

    let handle_id = app
        .first_pending_confirm()
        .expect("outside destination opens a permission dialog");
    let request = &app
        .core_probe()
        .confirms
        .get(handle_id)
        .expect("pending switch_cwd confirm")
        .req;
    assert_eq!(
        request.tool_paths,
        vec![smelt_core::permissions::ToolPath::directory(
            target.to_string_lossy()
        )]
    );
    assert!(request.grant_options.iter().any(|option| {
        matches!(option.target, smelt_core::ApprovalTarget::Session)
            && option.grants
                == vec![smelt_core::permissions::PermissionGrant::PathPrefix {
                    dir: target.clone(),
                }]
    }));
    assert!(request.grant_options.iter().any(|option| {
        matches!(option.target, smelt_core::ApprovalTarget::Workspace { .. })
            && option.grants
                == vec![smelt_core::permissions::PermissionGrant::PathPrefix {
                    dir: target.clone(),
                }]
    }));
    assert_eq!(app.cwd_str(), original_cwd);

    assert!(app.resolve_first_confirm(true, None));
    assert_eq!(app.cwd_str(), original_cwd);

    app.feed_one(SourceEvent::engine(protocol::EngineEvent::ToolDispatch {
        invocation_id: protocol::InvocationId::new(90),
        request_id: 92,
        call_id: "switch-cwd".into(),
        tool_name: "switch_cwd".into(),
        args,
    }));

    assert_eq!(app.core_probe().env.cwd(), target);
    assert_eq!(std::env::current_dir().unwrap(), target);
}

#[test]
fn switch_cwd_outside_repository_offers_repository_scoped_destination_grant() {
    let fixture = GitWorktreeFixture::new();
    let outside = tempfile::tempdir().expect("create outside repository target");
    let target = std::fs::canonicalize(outside.path()).expect("canonical outside target");
    let active = std::fs::canonicalize(&fixture.feature).expect("canonical active worktree");
    let mut app = TestApp::builder().with_cwd(active).build();
    let mut permissions = app.core_probe().permissions.snapshot().as_ref().clone();
    permissions.set_restrict_to_workspace(true);
    app.replace_permissions_for_harness(permissions);
    app.approve_tool_for_session("switch_cwd");
    app.start_turn(1);
    let args = std::collections::HashMap::from([(
        "path".to_string(),
        serde_json::Value::String(target.to_string_lossy().into_owned()),
    )]);
    let _ = app.drain_engine_sends();

    assert_eq!(
        evaluate_tool(
            &mut app,
            98,
            "switch-outside-repository",
            "switch_cwd",
            args.clone(),
            protocol::AgentMode::parse("yolo").unwrap(),
        ),
        protocol::Decision::Ask
    );
    app.feed_one(SourceEvent::engine(
        protocol::EngineEvent::RequestPermission {
            invocation_id: protocol::InvocationId::new(98),
            request_id: 99,
            call_id: "switch-outside-repository".into(),
            tool_name: "switch_cwd".into(),
            args,
            approval_patterns: Vec::new(),
            called_at_ms: 0,
            summary: protocol::StyledLines::from_plain(target.to_string_lossy()),
        },
    ));

    let handle_id = app
        .first_pending_confirm()
        .expect("outside destination opens a permission dialog");
    let options = &app
        .core_probe()
        .confirms
        .get(handle_id)
        .expect("switch_cwd confirm")
        .req
        .grant_options;
    let repository = options
        .iter()
        .find(|option| matches!(option.target, smelt_core::ApprovalTarget::Repository { .. }))
        .expect("repository-scoped switch_cwd grant");
    assert_eq!(
        repository.target,
        smelt_core::ApprovalTarget::Repository {
            key: fixture.repository_key(),
        }
    );
    assert_eq!(
        repository.grants,
        vec![smelt_core::permissions::PermissionGrant::PathPrefix { dir: target }]
    );
}

#[test]
fn switch_cwd_between_repository_worktrees_remains_allowed() {
    let fixture = GitWorktreeFixture::new();
    let environment_guard = test_environment_guard();
    let active = std::fs::canonicalize(&fixture.feature).expect("canonical active worktree");
    let target = std::fs::canonicalize(fixture.repo.path()).expect("canonical main checkout");
    let mut app = TestApp::builder()
        .with_cwd(&active)
        .build_with_test_environment_guard(&environment_guard);
    let mut permissions = app.core_probe().permissions.snapshot().as_ref().clone();
    permissions.set_restrict_to_workspace(true);
    app.replace_permissions_for_harness(permissions);
    app.approve_tool_for_session("switch_cwd");
    app.start_turn(1);
    let _ = app.drain_engine_sends();

    let decision = evaluate_tool(
        &mut app,
        93,
        "switch-worktree",
        "switch_cwd",
        std::collections::HashMap::from([(
            "path".to_string(),
            serde_json::Value::String(target.to_string_lossy().into_owned()),
        )]),
        protocol::AgentMode::parse("yolo").unwrap(),
    );
    assert_eq!(decision, protocol::Decision::Allow);
}

#[test]
fn switch_cwd_relative_path_is_evaluated_from_runtime_cwd() {
    let workspace = tempfile::tempdir().expect("create workspace");
    let active_root = std::fs::canonicalize(workspace.path()).expect("canonical workspace");
    let nested = active_root.join("nested");
    std::fs::create_dir(&nested).expect("create nested cwd");
    let environment_guard = test_environment_guard();
    let mut app = TestApp::builder()
        .with_cwd(&nested)
        .build_with_test_environment_guard(&environment_guard);
    let mut permissions = app.core_probe().permissions.snapshot().as_ref().clone();
    permissions.set_allowed_roots(active_root, Vec::new());
    permissions.set_restrict_to_workspace(true);
    app.replace_permissions_for_harness(permissions);
    app.approve_tool_for_session("switch_cwd");
    app.start_turn(1);
    let _ = app.drain_engine_sends();

    let decision = evaluate_tool(
        &mut app,
        94,
        "switch-parent",
        "switch_cwd",
        std::collections::HashMap::from([(
            "path".to_string(),
            serde_json::Value::String("..".to_string()),
        )]),
        protocol::AgentMode::parse("yolo").unwrap(),
    );
    assert_eq!(decision, protocol::Decision::Allow);
}

#[test]
fn lua_tool_paths_are_scoped_and_preserved_for_permission_rechecks() {
    let outside = tempfile::TempDir::new().expect("create outside-workspace directory");
    let outside_file = outside.path().join("secret.txt");
    std::fs::write(&outside_file, "secret").expect("write outside-workspace file");
    let outside_path = outside_file.to_string_lossy();
    let lua_path = serde_json::to_string(outside_path.as_ref()).expect("quote Lua path");

    let mut app = TestApp::builder().build();
    app.start_turn(1);
    app.run_lua_result(&format!(
        r#"
        path_callback_count = 0
        path_callback_session_id = nil
        smelt.tools.register({{
            name = "scoped_path_probe",
            description = "permission path probe",
            parameters = {{ type = "object", properties = {{}} }},
            effect = "read",
            permission_defaults = {{ normal = "allow" }},
            paths_for_workspace = function()
                local session_id = smelt.session.id()
                assert(type(session_id) == "string" and session_id ~= "")
                path_callback_count = path_callback_count + 1
                path_callback_session_id = session_id
                return {{ {{ path = {lua_path}, kind = "file" }} }}
            end,
            execute = function() return "ok" end,
        }})
        "#
    ))
    .expect("register path-aware tool");

    let _ = app.drain_engine_sends();
    app.feed_one(SourceEvent::engine(
        protocol::EngineEvent::ToolEvaluationRequest {
            invocation_id: protocol::InvocationId::new(91),
            request_id: 91,
            call_id: "call-91".into(),
            tool_name: "scoped_path_probe".into(),
            args: std::collections::HashMap::new(),
            mode: protocol::AgentMode::normal(),
        },
    ));

    let decision = app.actions().iter().rev().find_map(|action| match action {
        Action::EngineSend(command) => match command.as_ref() {
            protocol::UiCommand::ToolEvaluationResponse {
                request_id: 91,
                evaluation,
            } => Some(evaluation.decision.clone()),
            _ => None,
        },
        _ => None,
    });
    assert_eq!(decision, Some(protocol::Decision::Ask));
    assert_eq!(app.lua_int_global("path_callback_count"), Some(1));
    assert_eq!(
        app.eval_lua::<String>("return path_callback_session_id")
            .expect("read callback session id"),
        app.session_snapshot().id
    );

    app.feed_one(SourceEvent::engine(
        protocol::EngineEvent::RequestPermission {
            invocation_id: protocol::InvocationId::new(91),
            request_id: 92,
            call_id: "call-91".into(),
            tool_name: "scoped_path_probe".into(),
            args: std::collections::HashMap::new(),
            approval_patterns: Vec::new(),
            called_at_ms: 0,
            summary: protocol::StyledLines::from_plain("path probe"),
        },
    ));

    let handle_id = app
        .first_pending_confirm()
        .expect("permission dialog opens");
    let request = &app
        .core_probe()
        .confirms
        .get(handle_id)
        .expect("pending confirm")
        .req;
    assert_eq!(request.tool_paths.len(), 1);
    assert_eq!(request.tool_paths[0].path, outside_path);
    assert_eq!(app.lua_int_global("path_callback_count"), Some(2));

    assert!(!app.resolve_open_confirm_for_current_mode(handle_id));
    assert_eq!(app.lua_int_global("path_callback_count"), Some(2));
}

#[test]
fn tool_path_callback_failure_fails_closed_without_opening_a_dialog() {
    let mut app = TestApp::builder().build();
    app.restrict_permissions_to_cwd();
    app.start_turn(1);
    app.run_lua_result(
        r#"
        smelt.tools.register({
            name = "broken_path_callback",
            description = "broken workspace path callback",
            parameters = { type = "object", properties = {} },
            effect = "write",
            permission_defaults = { yolo = "allow" },
            paths_for_workspace = function()
                error("path callback exploded")
            end,
            execute = function() return "should not run" end,
        })
        "#,
    )
    .expect("register tool with failing path callback");
    app.approve_tool_for_session("broken_path_callback");
    let _ = app.drain_engine_sends();

    let decision = evaluate_tool(
        &mut app,
        95,
        "broken-path-callback",
        "broken_path_callback",
        std::collections::HashMap::new(),
        protocol::AgentMode::parse("yolo").unwrap(),
    );
    assert!(
        matches!(
            decision,
            protocol::Decision::Error(ref error)
                if error.contains("paths_for_workspace")
                    && error.contains("path callback exploded")
        ),
        "unexpected evaluation decision: {decision:?}"
    );

    let before = app.actions().len();
    app.feed_one(SourceEvent::engine(
        protocol::EngineEvent::RequestPermission {
            invocation_id: protocol::InvocationId::new(95),
            request_id: 96,
            call_id: "broken-path-callback".into(),
            tool_name: "broken_path_callback".into(),
            args: std::collections::HashMap::new(),
            approval_patterns: Vec::new(),
            called_at_ms: 0,
            summary: protocol::StyledLines::from_plain("broken path callback"),
        },
    ));

    assert_eq!(app.pending_confirm_count(), 0);
    let denial = app
        .actions_since(before)
        .iter()
        .find_map(|action| match action {
            Action::EngineSend(command) => match command.as_ref() {
                protocol::UiCommand::PermissionDecision {
                    request_id: 96,
                    approved,
                    message,
                } => Some((*approved, message.clone())),
                _ => None,
            },
            _ => None,
        });
    assert!(matches!(
        denial,
        Some((false, Some(ref message)))
            if message.contains("paths_for_workspace")
                && message.contains("path callback exploded")
    ));
}

#[test]
fn malformed_tool_path_callback_entry_fails_closed_in_yolo() {
    let mut app = TestApp::builder().build();
    app.restrict_permissions_to_cwd();
    app.start_turn(1);
    app.run_lua_result(
        r#"
        smelt.tools.register({
            name = "malformed_path_callback",
            description = "malformed workspace path callback",
            parameters = { type = "object", properties = {} },
            effect = "write",
            permission_defaults = { yolo = "allow" },
            paths_for_workspace = function()
                return { 42 }
            end,
            execute = function() return "should not run" end,
        })
        "#,
    )
    .expect("register tool with malformed path callback");
    app.approve_tool_for_session("malformed_path_callback");
    let _ = app.drain_engine_sends();

    let decision = evaluate_tool(
        &mut app,
        97,
        "malformed-path-callback",
        "malformed_path_callback",
        std::collections::HashMap::new(),
        protocol::AgentMode::parse("yolo").unwrap(),
    );
    assert!(
        matches!(
            decision,
            protocol::Decision::Error(ref error)
                if error.contains("invalid path entry 1")
                    && error.contains("got integer")
        ),
        "unexpected evaluation decision: {decision:?}"
    );
}

#[test]
fn open_confirm_recheck_keeps_dialog_when_mode_still_asks() {
    let mut app = TestApp::builder().build();
    install_confirm_test_permissions(&mut app);
    app.start_turn(1);

    {
        let mut pending = Vec::new();
        let ctrl = dispatch_confirm_request(&mut app, confirm_req(12), &mut pending);
        assert!(matches!(ctrl, crate::app::SessionControl::Continue));
    }

    let handle_id = app.first_pending_confirm().unwrap();
    assert!(!app.resolve_open_confirm_for_current_mode(handle_id));
    assert_eq!(app.pending_confirm_count(), 1);
    assert!(permission_decisions(app.drain_engine_sends()).is_empty());
}

#[test]
fn public_status_open_confirm_needs_attention() {
    let mut app = TestApp::builder().build();
    install_confirm_test_permissions(&mut app);
    app.start_turn(1);

    {
        let mut pending = Vec::new();
        let ctrl = dispatch_confirm_request(&mut app, confirm_req(13), &mut pending);
        assert!(matches!(ctrl, crate::app::SessionControl::Continue));
    }

    assert_eq!(app.pending_confirm_count(), 1);
    assert!(app.state().active_modal.is_some());

    let (state, reason) = app.public_status_state_reason();
    assert_eq!(
        state,
        smelt_core::public_status::PublicState::NeedsAttention
    );
    assert_eq!(
        reason,
        Some(smelt_core::public_status::PublicReason::Permission)
    );
}

#[test]
fn open_confirm_dialog_does_not_show_permission_pending_in_statusline() {
    let mut app = TestApp::builder().build();
    install_confirm_test_permissions(&mut app);
    app.start_turn(1);

    {
        let mut pending = Vec::new();
        let ctrl = dispatch_confirm_request(&mut app, confirm_req(130), &mut pending);
        assert!(matches!(ctrl, crate::app::SessionControl::Continue));
    }

    assert_eq!(app.pending_confirm_count(), 1);
    assert!(app.state().active_modal.is_some());
    let frame = app.render_to_frame();
    let text = frame.text();
    let statusline = text.lines().last().expect("rendered statusline");
    assert!(!statusline.contains("permission pending"), "{statusline:?}");
}

#[test]
fn deferred_confirm_dialog_shows_permission_pending_in_statusline() {
    let mut app = TestApp::builder().build();
    install_confirm_test_permissions(&mut app);
    app.start_turn(1);
    app.type_text("draft response");

    {
        let mut pending = Vec::new();
        let ctrl = dispatch_confirm_request(&mut app, confirm_req(131), &mut pending);
        assert!(matches!(ctrl, crate::app::SessionControl::Continue));
    }

    assert_eq!(app.pending_confirm_count(), 0);
    assert_eq!(app.pending_deferred_dialog_count(), 1);
    assert!(app.state().active_modal.is_none());
    let frame = app.render_to_frame();
    let text = frame.text();
    let statusline = text.lines().last().expect("rendered statusline");
    assert!(statusline.contains("permission pending"), "{statusline:?}");
}

#[test]
fn confirm_down_moves_selection_without_scrolling_options() {
    let mut app = TestApp::builder().build();
    install_confirm_test_permissions(&mut app);
    app.start_turn(1);

    {
        let mut pending = Vec::new();
        let ctrl = dispatch_confirm_request(&mut app, confirm_req(14), &mut pending);
        assert!(matches!(ctrl, crate::app::SessionControl::Continue));
    }
    app.render_silent();

    let options = app.ui_probe().focus().expect("confirm options focused");
    let before = app.ui_probe().win(options).expect("confirm options window");
    assert_eq!(before.cursor_abs_row(), 0);
    assert_eq!(before.scroll_top(), 0);
    assert!(
        before
            .viewport
            .expect("confirm options viewport")
            .rect
            .height
            >= 2,
        "both confirm options should be visible"
    );

    app.press(KeyCode::Down);

    let after = app.ui_probe().win(options).expect("confirm options window");
    assert_eq!(after.cursor_abs_row(), 1, "Down should select deny");
    assert_eq!(
        after.scroll_top(),
        0,
        "the options viewport should stay fixed while deny is visible"
    );
}

#[test]
fn confirm_top_border_drag_resizes_dialog() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let mut app = TestApp::builder().build();
    install_confirm_test_permissions(&mut app);
    app.start_turn(1);

    {
        let mut pending = Vec::new();
        let ctrl = dispatch_confirm_request(&mut app, confirm_req(15), &mut pending);
        assert!(matches!(ctrl, crate::app::SessionControl::Continue));
    }
    app.render_silent();

    let dialog = app.active_docked_dialog().expect("confirm dialog");
    let first_leaf = app
        .ui_probe()
        .modal_leaves(
            app.ui_probe()
                .docked_surface(dialog)
                .expect("docked surface")
                .modal(),
        )
        .and_then(|leaves| leaves.first())
        .copied()
        .expect("confirm dialog leaf");
    let before_top = app
        .ui_probe()
        .split_rect(first_leaf)
        .expect("confirm dialog leaf rect")
        .top
        .saturating_sub(1);
    let target_top = before_top.saturating_sub(3);

    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Drag(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
            kind,
            row: if matches!(kind, MouseEventKind::Down(_)) {
                before_top
            } else {
                target_top
            },
            column: 10,
            modifiers: KeyModifiers::empty(),
        })));
    }
    app.render_silent();

    let after_top = app
        .ui_probe()
        .split_rect(first_leaf)
        .expect("resized confirm dialog leaf rect")
        .top
        .saturating_sub(1);
    assert_eq!(after_top, target_top);
}

#[test]
fn custom_composer_dialog_chrome_resizes_and_reflows_with_terminal() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let mut app = TestApp::builder().build();
    open_root_test_dialog(&mut app);
    assert!(app.run_lua(
        r#"
        smelt.ui.layout.set(function(state)
          if not state.dialog then
            return smelt.ui.layout.leaf(smelt.win.PROMPT)
          end
          return smelt.ui.layout.vbox({
            { state.dialog, height = "fill" },
          })
        end)
        "#
    ));
    app.render_silent();

    let dialog = app.active_docked_dialog().expect("active dialog");
    let before = app
        .ui_probe()
        .docked_surface(dialog)
        .and_then(crate::smelt_edit::DockedSurface::resolved_rect)
        .expect("custom-composed dialog rect");
    let target_top = before.top.saturating_sub(2);

    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Drag(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
            kind,
            row: if matches!(kind, MouseEventKind::Down(_)) {
                before.top
            } else {
                target_top
            },
            column: 10,
            modifiers: KeyModifiers::empty(),
        })));
    }
    app.render_silent();

    let resized = app
        .ui_probe()
        .docked_surface(dialog)
        .and_then(crate::smelt_edit::DockedSurface::resolved_rect)
        .expect("resized custom-composed dialog rect");
    assert_eq!(resized.top, target_top);
    assert_eq!(resized.height, before.height + 2);

    app.set_terminal_size(80, 3);
    app.render_silent();
    let constrained = app
        .ui_probe()
        .docked_surface(dialog)
        .and_then(crate::smelt_edit::DockedSurface::resolved_rect)
        .expect("constrained dialog rect");
    assert!(constrained.height < resized.height);
    assert!(constrained.bottom() <= 3);

    app.set_terminal_size(80, 24);
    app.render_silent();
    let restored = app
        .ui_probe()
        .docked_surface(dialog)
        .and_then(crate::smelt_edit::DockedSurface::resolved_rect)
        .expect("restored dialog rect");
    assert_eq!(restored.height, resized.height);
}

#[test]
fn shift_tab_on_open_confirm_waits_for_turn_mode_boundary_before_allowing() {
    let mut app = TestApp::builder()
        .with_mode_cycle(vec![
            protocol::AgentMode::normal(),
            protocol::AgentMode::parse("apply").unwrap(),
        ])
        .build();
    install_confirm_test_permissions(&mut app);
    app.start_turn(1);

    {
        let mut pending = Vec::new();
        let ctrl = dispatch_confirm_request(&mut app, confirm_req(13), &mut pending);
        assert!(matches!(ctrl, crate::app::SessionControl::Continue));
    }
    assert_eq!(app.pending_confirm_count(), 1);
    app.clear_actions();

    app.press_mod(KeyCode::BackTab, KeyModifiers::SHIFT);

    assert_eq!(app.pending_confirm_count(), 1);
    assert_eq!(app.core_probe().config.mode.as_str(), "apply");
    assert!(app.mode_pending());
    assert!(actions_permission_decisions(app.actions()).is_empty());

    app.sync_agent_mode_applied();
    let handle_id = app.first_pending_confirm().unwrap();
    assert!(app.resolve_open_confirm_for_current_mode(handle_id));
    assert_eq!(app.pending_confirm_count(), 0);
    assert_eq!(
        permission_decisions(app.drain_engine_sends()),
        vec![(13, true)]
    );
}

#[test]
fn open_confirm_recheck_auto_denies_without_stopping_turn() {
    let mut app = TestApp::builder().build();
    install_confirm_test_permissions(&mut app);
    app.start_turn(1);

    {
        let mut pending = Vec::new();
        let ctrl = dispatch_confirm_request(&mut app, confirm_req(14), &mut pending);
        assert!(matches!(ctrl, crate::app::SessionControl::Continue));
    }

    let handle_id = app.first_pending_confirm().unwrap();
    app.set_applied_mode(protocol::AgentMode::parse("deny").unwrap());

    assert!(app.resolve_open_confirm_for_current_mode(handle_id));
    assert_eq!(app.pending_confirm_count(), 0);
    assert!(app.agent_running());
    assert_eq!(
        permission_decisions(app.drain_engine_sends()),
        vec![(14, false)]
    );
}

#[test]
fn present_plan_save_draft_writes_artifact_and_manifest() {
    let mut app = TestApp::builder().with_vim(false).build();
    app.start_turn(1);

    let mut args = std::collections::HashMap::new();
    args.insert("title".into(), serde_json::json!("Parser plan"));
    args.insert("slug".into(), serde_json::json!("parser-plan"));
    args.insert(
        "plan".into(),
        serde_json::json!("# Goal\nShip the parser change.\n"),
    );

    app.feed_one(SourceEvent::engine(EngineEvent::ToolDispatch {
        invocation_id: protocol::InvocationId::new(91),
        request_id: 91,
        call_id: "plan-draft".into(),
        tool_name: "present_plan".into(),
        args,
    }));
    assert!(app.state().active_modal.is_some());

    assert!(app.run_lua(r#"smelt.dialog.current().resolve("draft")"#));
    drive_lua_tasks(&mut app);

    let (content, is_error) = tool_result(&app, "plan-draft").expect("present_plan result");
    assert!(!is_error, "{content}");
    let plan_path = content
        .lines()
        .find_map(|line| line.strip_prefix("wrote plan to "))
        .expect("result includes plan path");
    assert!(plan_path.ends_with("/plan.md"));
    assert_eq!(
        std::fs::read_to_string(plan_path).unwrap(),
        "# Goal\nShip the parser change.\n"
    );

    let artifact_dir = std::path::Path::new(plan_path).parent().unwrap();
    let artifact_dir = std::fs::canonicalize(artifact_dir).unwrap();
    let grants = app.session_path_grants();
    assert!(grants.iter().any(|grant| {
        grant.mode.is_none()
            && grant.tool == "read_file"
            && grant.access == smelt_core::permissions::PathAccess::Read
            && grant.dir == artifact_dir
    }));
    assert!(grants.iter().any(|grant| {
        grant.mode.is_none()
            && grant.tool == "edit_file"
            && grant.access == smelt_core::permissions::PathAccess::Write
            && grant.dir == artifact_dir
    }));
    assert!(grants.iter().any(|grant| {
        grant
            .mode
            .as_ref()
            .is_some_and(|mode| mode.as_str() == "plan")
            && grant.tool == "edit_file"
            && grant.access == smelt_core::permissions::PathAccess::Write
            && grant.dir == artifact_dir
    }));

    let manifest_path = artifact_dir.join("manifest.json");
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(manifest_path).unwrap()).unwrap();
    assert_eq!(manifest["kind"], "smelt.plan");
    assert_eq!(manifest["status"], "draft");
    assert_eq!(manifest["title"], "Parser plan");
    assert_eq!(manifest["slug"], "parser-plan");
}

#[test]
fn present_plan_existing_path_approves_without_overwriting_plan_body() {
    let mut app = TestApp::builder().with_vim(false).build();
    app.start_turn(1);

    let session_dir = app
        .core_probe()
        .sessions
        .artifact_dir_for(&app.session_snapshot());
    let artifact_dir = session_dir.join("plans/20260101-000000-parser-plan");
    std::fs::create_dir_all(&artifact_dir).unwrap();
    let plan_path = artifact_dir.join("plan.md");
    std::fs::write(&plan_path, "# Revised\nUse the existing draft.\n").unwrap();
    std::fs::write(
        artifact_dir.join("manifest.json"),
        serde_json::json!({
            "version": 1,
            "kind": "smelt.plan",
            "title": "Parser plan",
            "slug": "parser-plan",
            "status": "draft",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "plan_path": "plan.md"
        })
        .to_string(),
    )
    .unwrap();

    let mut args = std::collections::HashMap::new();
    args.insert(
        "plan_path".into(),
        serde_json::json!(plan_path.to_string_lossy().to_string()),
    );

    app.feed_one(SourceEvent::engine(EngineEvent::ToolDispatch {
        invocation_id: protocol::InvocationId::new(92),
        request_id: 92,
        call_id: "plan-approve".into(),
        tool_name: "present_plan".into(),
        args,
    }));
    assert!(app.state().active_modal.is_some());

    app.press(KeyCode::Char('2'));
    drive_lua_tasks(&mut app);

    let (content, is_error) = tool_result(&app, "plan-approve").expect("present_plan result");
    assert!(!is_error, "{content}");
    assert!(content.contains("wrote plan to "));
    assert_eq!(
        std::fs::read_to_string(&plan_path).unwrap(),
        "# Revised\nUse the existing draft.\n"
    );

    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(artifact_dir.join("manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["status"], "approved");
    assert_eq!(manifest["created_at"], "2026-01-01T00:00:00Z");
}

#[test]
fn present_plan_dismiss_does_not_echo_plan_body() {
    let mut app = TestApp::builder().with_vim(false).build();
    app.start_turn(1);

    let mut args = std::collections::HashMap::new();
    args.insert("title".into(), serde_json::json!("Parser plan"));
    args.insert("slug".into(), serde_json::json!("parser-plan"));
    args.insert(
        "plan".into(),
        serde_json::json!("# Secret draft\nDo not keep this transcript copy.\n"),
    );

    app.feed_one(SourceEvent::engine(EngineEvent::ToolDispatch {
        invocation_id: protocol::InvocationId::new(94),
        request_id: 94,
        call_id: "plan-dismiss".into(),
        tool_name: "present_plan".into(),
        args,
    }));
    assert!(app.state().active_modal.is_some());

    app.press(KeyCode::Esc);
    drive_lua_tasks(&mut app);

    let (content, is_error) = tool_result(&app, "plan-dismiss").expect("present_plan result");
    assert!(is_error);
    assert!(content.contains("plan dismissed"));
    assert!(!content.contains("Secret draft"));
    assert!(!content.contains("Do not keep this transcript copy"));
}

#[test]
fn present_plan_dialog_tracks_terminal_width_on_resize() {
    let mut app = TestApp::builder().with_vim(false).build();
    app.start_turn(1);

    let mut args = std::collections::HashMap::new();
    args.insert("title".into(), serde_json::json!("Parser plan"));
    args.insert("slug".into(), serde_json::json!("parser-plan"));
    args.insert(
        "plan".into(),
        serde_json::json!("# Goal\nShip the parser change.\n"),
    );

    app.feed_one(SourceEvent::engine(EngineEvent::ToolDispatch {
        invocation_id: protocol::InvocationId::new(93),
        request_id: 93,
        call_id: "plan-resize".into(),
        tool_name: "present_plan".into(),
        args,
    }));
    assert!(app.state().active_modal.is_some());

    let before = app.render_to_frame();
    assert!(before
        .text()
        .lines()
        .any(|line| line.starts_with("─ plan ")));

    app.set_terminal_size(100, 24);
    let after = app.render_to_frame();
    let text = after.text();
    let title_row = text
        .lines()
        .find(|line| line.starts_with("─ plan "))
        .expect("dialog title row after resize");
    assert_eq!(title_row.chars().count(), 100);
}

#[test]
fn public_status_open_question_needs_attention() {
    let mut app = TestApp::builder().with_vim(false).build();
    app.start_turn(1);

    let mut args = std::collections::HashMap::new();
    args.insert(
        "questions".into(),
        serde_json::json!([
            {
                "header": "Choice",
                "question": "Pick one?",
                "options": [
                    { "label": "One", "description": "first option" },
                    { "label": "Two", "description": "second option" }
                ],
                "multiSelect": false
            }
        ]),
    );

    app.feed_one(SourceEvent::engine(EngineEvent::ToolDispatch {
        invocation_id: protocol::InvocationId::new(77),
        request_id: 77,
        call_id: "status-question".into(),
        tool_name: "ask_user_question".into(),
        args,
    }));
    assert!(app.state().active_modal.is_some());

    let (state, reason) = app.public_status_state_reason();
    assert_eq!(
        state,
        smelt_core::public_status::PublicState::NeedsAttention
    );
    assert_eq!(
        reason,
        Some(smelt_core::public_status::PublicReason::Question)
    );
}

#[test]
fn ask_user_question_custom_input_accepts_question_mark() {
    let mut app = TestApp::builder().with_vim(false).build();
    app.start_turn(1);
    app.push_user_block("Earlier transcript text");
    app.focus_transcript();

    let mut args = std::collections::HashMap::new();
    args.insert(
        "questions".into(),
        serde_json::json!([{
            "header": "Choice",
            "question": "Pick one?",
            "options": [
                { "label": "One", "description": "first option" },
                { "label": "Two", "description": "second option" }
            ],
            "multiSelect": false
        }]),
    );

    app.feed_one(SourceEvent::engine(EngineEvent::ToolDispatch {
        invocation_id: protocol::InvocationId::new(77),
        request_id: 77,
        call_id: "aq-custom-input".into(),
        tool_name: "ask_user_question".into(),
        args,
    }));
    assert!(app.state().active_modal.is_some());

    app.press(KeyCode::Tab);
    app.settle_lua();
    let input_win = app
        .ui_probe()
        .focus()
        .expect("custom input should be focused");
    let input = app.ui_probe().win(input_win).expect("custom input window");
    assert!(
        !input.surface().is_readonly_text(),
        "Tab should focus the editable custom input"
    );
    let input_buf = input.buf;

    app.type_text("why");
    assert_eq!(
        app.ui_probe()
            .buf(input_buf)
            .expect("custom input buffer")
            .get_line(0),
        Some("why")
    );

    app.press_mod(KeyCode::Char('?'), KeyModifiers::SHIFT);

    assert_eq!(
        app.ui_probe()
            .buf(input_buf)
            .expect("custom input buffer")
            .get_line(0),
        Some("why?")
    );
    assert_eq!(app.ui_probe().focus(), Some(input_win));
    assert!(
        !app.state().cmdline_open,
        "question mark must not open search"
    );
}

#[test]
fn ask_user_question_multiple_questions_wakes_between_dialogs() {
    let mut app = TestApp::builder().with_vim(false).build();
    app.start_turn(1);

    let mut args = std::collections::HashMap::new();
    args.insert(
        "questions".into(),
        serde_json::json!([
            {
                "header": "First",
                "question": "Pick first?",
                "options": [
                    { "label": "One", "description": "first option" },
                    { "label": "Two", "description": "second option" }
                ],
                "multiSelect": false
            },
            {
                "header": "Second",
                "question": "Pick second?",
                "options": [
                    { "label": "Three", "description": "third option" },
                    { "label": "Four", "description": "fourth option" }
                ],
                "multiSelect": false
            }
        ]),
    );

    app.feed_one(SourceEvent::engine(EngineEvent::ToolDispatch {
        invocation_id: protocol::InvocationId::new(77),
        request_id: 77,
        call_id: "aq-questions".into(),
        tool_name: "ask_user_question".into(),
        args,
    }));

    let first = app
        .state()
        .active_modal
        .expect("first question dialog should open");

    app.press(KeyCode::Enter);
    assert!(
        app.try_receive_lua_wakeup(),
        "resolving the first dialog should wake the Lua task runtime"
    );
    app.feed_one(SourceEvent::LuaWakeup);

    let second = app
        .state()
        .active_modal
        .expect("second question dialog should open after first answer");
    assert_ne!(first, second);

    app.press(KeyCode::Char('2'));
    assert!(
        app.try_receive_lua_wakeup(),
        "resolving the final dialog should wake the Lua task runtime"
    );
    app.feed_one(SourceEvent::LuaWakeup);

    assert!(app.state().active_modal.is_none());
    let result = app
        .actions()
        .iter()
        .filter_map(|action| match action {
            Action::EngineSend(cmd) => match cmd.as_ref() {
                protocol::UiCommand::ToolResult {
                    request_id,
                    call_id,
                    content,
                    is_error,
                    ..
                } => Some((*request_id, call_id.as_str(), content.as_str(), *is_error)),
                _ => None,
            },
            _ => None,
        })
        .next_back()
        .expect("ask_user_question should send a tool result");

    assert_eq!(result.0, 77);
    assert_eq!(result.1, "aq-questions");
    assert_eq!(
        result.2,
        "q: Pick first?\na: One\n\nq: Pick second?\na: Four"
    );
    assert!(!result.3);
}

#[test]
fn colon_in_non_vim_mode_does_not_open_cmdline() {
    let mut app = TestApp::builder().build();
    app.type_char(':');
    let s = app.state();
    assert!(!s.cmdline_open);
    assert_eq!(s.prompt_text, ":");
}

#[test]
fn colon_in_vim_insert_mode_does_not_open_cmdline() {
    let mut app = TestApp::builder().with_vim(true).build();
    // Fresh prompt is already in Insert.
    assert_eq!(app.state().vim_mode, VimMode::Insert);

    app.type_char(':');
    let s = app.state();
    assert!(!s.cmdline_open);
    assert_eq!(s.prompt_text, ":");
}

#[test]
fn colon_in_vim_normal_mode_opens_cmdline() {
    let mut app = TestApp::builder().with_vim(true).build();
    // Drop to Normal first since the prompt starts in Insert.
    app.press(KeyCode::Esc);
    app.type_char(':');
    let s = app.state();
    assert!(s.cmdline_open);
    assert_eq!(s.cmdline_text, "");
}

#[test]
fn esc_closes_cmdline() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.press(KeyCode::Esc);
    app.type_char(':');
    assert!(app.state().cmdline_open);

    app.press(KeyCode::Esc);
    assert!(!app.state().cmdline_open);
}

#[test]
fn cmdline_quit_command_requests_quit() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.press(KeyCode::Esc);
    app.type_char(':');
    app.type_text("quit");
    app.press(KeyCode::Enter);
    assert!(app.state().pending_quit);
}

#[test]
fn cmdline_paste_inserts_single_line_payload() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.press(KeyCode::Esc);
    app.type_char(':');

    app.feed_one(SourceEvent::Term(Event::Paste("hello\nworld".into())));

    let s = app.state();
    assert!(s.cmdline_open);
    assert_eq!(s.cmdline_text, "hello world");
}

#[test]
fn cmdline_unicode_editing_uses_byte_safe_cursor() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.press(KeyCode::Esc);
    app.type_char(':');
    app.feed_one(SourceEvent::Term(Event::Paste("a日本b".into())));
    app.press(KeyCode::Left);
    app.press(KeyCode::Backspace);

    let s = app.state();
    assert_eq!(s.cmdline_text, "a日b");
    app.assert_invariants();
}

#[test]
fn cmdline_typed_payload_does_not_trip_cursor_invariant() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.press(KeyCode::Esc);
    app.type_char(':');
    // Type more than any single-line buffer could ever encode in
    // source byte arithmetic from cell position alone.
    app.type_text("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    app.assert_invariants();
}

#[test]
fn btw_command_preserves_model_history_prefix_and_appends_question() {
    let mut app = TestApp::builder().build();
    stub_btw_ui(&mut app);
    app.session_append_history(protocol::HistoryItem::user(protocol::Content::text("u1")));
    app.push_assistant_text("a1");
    app.session_append_history(protocol::HistoryItem::user(protocol::Content::text("u2")));

    let expected_prefix = protocol::history_to_messages(&app.model_history());

    {
        app.apply_lua_command("btw what changed?");
        app.drive_lua_tasks();
    }

    let asks = ask_messages(app.drain_engine_sends());
    assert_eq!(asks.len(), 1, "/btw should issue one inherited ask");
    let (system, messages) = &asks[0];
    assert_eq!(system, &app.assemble_system_prompt());
    assert_eq!(
        &messages[..expected_prefix.len()],
        expected_prefix.as_slice(),
        "/btw must preserve the exact model-visible prefix"
    );
    let last_text = messages
        .last()
        .and_then(|m| m.content.as_ref())
        .map(|c| c.text_content())
        .expect("/btw question");
    assert!(last_text.contains("Under no circumstances use tools"));
    assert!(last_text.contains("Question: what changed?"));

    respond_pending_ask_with_text(&mut app, "done");
    app.clear_timers();
}

#[test]
fn btw_dialog_paints_a_selected_delta_before_a_coalesced_final_response() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(70, 22);
    app.push_user_block("how do I render a buffer?");
    app.push_assistant_text("Call `buf:source(text)`.");

    {
        app.apply_lua_command("btw show me a tiny example");
        app.drive_lua_tasks();
    }
    let ask_id = app.pending_ask_id().expect("/btw registered ask callback");
    app.render_to_frame();

    app.inject_engine(protocol::EngineEvent::EngineAskDelta {
        id: ask_id,
        delta: "coalesced live dialog marker".into(),
    })
    .expect("queue /btw delta");
    app.inject_engine(protocol::EngineEvent::EngineAskResponse {
        id: ask_id,
        message: Some(protocol::Message::assistant(
            Some(protocol::Content::text("coalesced final answer")),
            None,
            None,
        )),
        error: None,
    })
    .expect("queue /btw response");

    let selected_delta = app
        .try_receive_engine_output()
        .expect("select loop should receive the first delta");
    app.dispatch_engine_output_in_render_loop_to(selected_delta, &mut std::io::sink(), |_| {});
    let mut streamed_frame = None;
    app.drain_ready_engine_outputs_for_frame_to(&mut std::io::sink(), |frame| {
        streamed_frame = Some(frame.text())
    });

    let streamed_frame = streamed_frame.expect("final response should paint the pending delta");
    assert!(
        streamed_frame.contains("coalesced live dialog marker"),
        "no frame painted the selected delta: {streamed_frame}"
    );
    assert!(
        !streamed_frame.contains("coalesced final answer"),
        "the final callback ran before the streamed frame: {streamed_frame}"
    );

    let final_frame = app.render_to_frame();
    let text = "coalesced final answer";
    let row = final_frame
        .rows
        .iter()
        .position(|row| row.contains(text))
        .unwrap();
    let col = final_frame.rows[row].find(text).unwrap();
    assert!(
        final_frame.styles[row][col..col + text.len()]
            .iter()
            .all(|style| !style.dim && !style.italic),
        "waiting indicator styles leaked into the answer"
    );
    let final_frame = final_frame.text();
    assert!(
        final_frame.contains("coalesced final answer"),
        "frame: {final_frame}"
    );
    app.clear_timers();
}

#[test]
fn btw_dialog_streams_before_a_busy_engine_queue_reaches_the_final_response() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(70, 22);
    app.push_user_block("how do I render a buffer?");
    app.push_assistant_text("Call `buf:source(text)`.");

    {
        app.apply_lua_command("btw show me a tiny example");
        app.drive_lua_tasks();
    }
    let ask_id = app.pending_ask_id().expect("/btw registered ask callback");
    app.render_to_frame();

    let queued_deltas = crate::app::READY_QUEUE_DRAIN_MAX_ITEMS_PER_FRAME * 2;
    for index in 0..queued_deltas {
        let delta = if index == 0 {
            "live dialog marker ".to_string()
        } else {
            format!("chunk-{index} ")
        };
        app.inject_engine(protocol::EngineEvent::EngineAskDelta { id: ask_id, delta })
            .expect("queue /btw delta");
    }
    app.inject_engine(protocol::EngineEvent::EngineAskResponse {
        id: ask_id,
        message: Some(protocol::Message::assistant(
            Some(protocol::Content::text("final answer")),
            None,
            None,
        )),
        error: None,
    })
    .expect("queue /btw response");

    app.drain_ready_engine_outputs_for_frame_to(&mut std::io::sink(), |_| {});
    let frame = app.render_to_frame().text();
    assert!(frame.contains("live dialog marker"), "frame: {frame}");
    assert!(!frame.contains("final answer"), "frame: {frame}");

    for _ in 0..=queued_deltas {
        if app.pending_ask_id().is_none() {
            break;
        }
        app.drain_ready_engine_outputs_for_frame_to(&mut std::io::sink(), |_| {});
    }
    assert!(
        app.pending_ask_id().is_none(),
        "final response was not drained"
    );
    let frame = app.render_to_frame().text();
    assert!(frame.contains("final answer"), "frame: {frame}");
    app.clear_timers();
}

#[test]
fn btw_command_denies_tool_calls_then_retries_same_request_shape() {
    let mut app = TestApp::builder().build();
    stub_btw_ui(&mut app);
    app.session_append_history(protocol::HistoryItem::user(protocol::Content::text("u1")));
    app.push_assistant_text("a1");
    let expected_prefix = protocol::history_to_messages(&app.model_history());

    {
        app.apply_lua_command("btw quick summary");
        app.drive_lua_tasks();
    }

    let first = ask_messages(app.drain_engine_sends());
    assert_eq!(first.len(), 1);
    let first_messages = first[0].1.clone();

    respond_pending_ask_with_tool_call(&mut app, "call-1", "grep");

    let second = ask_messages(app.drain_engine_sends());
    assert_eq!(second.len(), 1);
    let second_messages = &second[0].1;
    assert_eq!(
        &second_messages[..first_messages.len()],
        first_messages.as_slice(),
        "/btw tool denial retry must keep the same request prefix"
    );
    assert_eq!(
        &second_messages[..expected_prefix.len()],
        expected_prefix.as_slice(),
        "/btw tool denial retry must keep the same inherited conversation prefix"
    );
    assert_eq!(
        second_messages[first_messages.len()].role,
        protocol::Role::Assistant
    );
    assert_eq!(
        second_messages[first_messages.len() + 1].role,
        protocol::Role::Tool
    );
    assert!(second_messages[first_messages.len() + 1].is_error);

    respond_pending_ask_with_text(&mut app, "done");
    app.clear_timers();
}
