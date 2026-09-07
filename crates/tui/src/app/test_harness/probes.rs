use super::*;

impl TestApp {
    pub fn window_lines_containing(&self, needle: &str) -> Option<Vec<String>> {
        self.app.ui.iter_wins().find_map(|(_, win)| {
            let buf = self.app.ui.buf(win.buf)?;
            buf.lines()
                .iter()
                .any(|line| line.contains(needle))
                .then(|| buf.lines().to_vec())
        })
    }

    pub fn materialized_window_containing(
        &self,
        needle: &str,
    ) -> Option<MaterializedWindowSnapshot> {
        self.app.ui.iter_wins().find_map(|(id, _)| {
            let snapshot = self.materialized_window(id)?;
            snapshot
                .lines
                .iter()
                .any(|line| line.contains(needle))
                .then_some(snapshot)
        })
    }

    pub fn materialized_window(&self, id: WinId) -> Option<MaterializedWindowSnapshot> {
        let win = self.app.ui.win(id)?;
        let rows = win.materialized_rows()?;
        let viewport = win.viewport?;
        let buf = self.app.ui.buf(win.buf)?;
        Some(MaterializedWindowSnapshot {
            win: id,
            lines: buf.lines().to_vec(),
            rows,
            viewport,
            scroll_top: win.scroll_top(),
        })
    }

    /// Side-channel: install a hostile prompt `text_changed` callback that
    /// tries to move the prompt cursor away from the edit endpoint.
    pub fn install_prompt_cursor_trap(&mut self, variant: u8) {
        const SNIPPETS: &[&str] = &[
            r#"
            smelt.prompt.win():on("text_changed", function()
                smelt.prompt.cursor(0)
            end)
            "#,
            r#"
            smelt.prompt.win():on("text_changed", function()
                smelt.prompt.win():cursor(0)
            end)
            "#,
            r#"
            smelt.prompt.win():on("text_changed", function()
                smelt.prompt.cursor(999999)
            end)
            "#,
        ];
        let snippet = SNIPPETS[(variant as usize) % SNIPPETS.len()];
        let _ = self.run_lua(snippet);
    }

    /// Side-channel: register a small Lua tool and begin a synthetic custom
    /// command turn, returning the `StartTurn` payload that was sent.
    pub fn start_custom_command_with_lua_tool(
        &mut self,
        variant: u8,
    ) -> Option<protocol::StartTurnPayload> {
        if self.agent_running() {
            self.cancel();
        }

        let tool_name = format!("fuzz_custom_tool_{}", variant % 4);
        let snippet = format!(
            r#"
            smelt.tools.register({{
                name = "{tool_name}",
                description = "fuzz custom command tool",
                parameters = {{ type = "object", properties = {{}} }},
                execute = function(args) return "ok" end,
            }})
            "#,
        );
        let _ = self.run_lua(&snippet);

        let cmd = smelt_core::custom_commands::CustomCommand {
            name: "fuzz-custom".to_string(),
            display: "fuzz-custom".to_string(),
            body: "fuzz custom body".to_string(),
            overrides: smelt_core::custom_commands::CommandOverrides::default(),
        };
        let sent_at_ms = engine::clock::unix_time_ms(self.app.core.clock.as_ref());
        let turn = self.app.begin_custom_command_turn(cmd, sent_at_ms)?;
        self.app.conversation.set_active(Some(turn));
        self.drain_cmd();
        self.actions.iter().rev().find_map(|a| match a {
            Action::EngineSend(cmd) => match cmd.as_ref() {
                protocol::UiCommand::StartTurn(payload) => Some((**payload).clone()),
                _ => None,
            },
            _ => None,
        })
    }

    /// Side-channel: start a Lua `smelt.engine.ask` request so fuzzed
    /// `EngineAskResponsePending` ops can drive the callback path.
    pub fn start_engine_ask_probe(&mut self, question: &str) {
        let question = serde_json::to_string(question).unwrap_or_else(|_| "\"\"".to_string());
        let snippet = format!(
            r#"
            smelt.engine.ask({{
                system = "fuzz ask probe",
                question = {question},
                on_response = function(_message, _err) end,
            }})
            "#,
        );
        let _ = self.run_lua(&snippet);
        self.drain_cmd();
    }

    fn force_prompt_keyboard_focus(&mut self) {
        if self.app.well_known.cmdline.is_some() {
            self.app.close_cmdline();
        }
        while self.app.close_active_modal() {}
        while let Some(overlay) = self.app.ui.focused_overlay() {
            self.app.close_overlay(overlay);
        }
        // Prompt-docked pickers own the prompt through Lua registrations on
        // the prompt window, not through overlay focus. Reloading drops those
        // registrations before the probe installs its own clean prompt state.
        if self.app.overlays.has_pickers() {
            self.reload_lua();
        }
        self.app.timers.pending_chord = None;
        self.app.timers.pending_pane_chord = None;
        self.app.ui.cancel_pointer_interaction();
        self.app.app_focus = AppFocus::Prompt;
        self.app.set_terminal_focus_for_harness(true);
        self.app.clear_prompt_prediction();
        let _ = self.app.ui.set_focus(crate::app::PROMPT_WIN);
        if let Some(win) = self.app.ui.win_mut(crate::app::PROMPT_WIN) {
            if win.vim_enabled() {
                win.set_vim_mode(VimMode::Insert);
            }
            win.clear_mouse_state();
            win.clear_selection_anchor();
        }
    }

    fn drain_engine_ask_ids(&mut self) -> Vec<u64> {
        self.drain_engine_sends()
            .into_iter()
            .filter_map(|cmd| match cmd {
                protocol::UiCommand::EngineAsk { id, .. } => Some(id),
                _ => None,
            })
            .collect()
    }

    pub(crate) fn respond_ask_with_text(&mut self, id: u64, text: &str) {
        self.app
            .dispatch_engine_event(protocol::EngineEvent::EngineAskResponse {
                id,
                message: Some(protocol::Message::assistant(
                    Some(protocol::Content::text(text)),
                    None,
                    None,
                )),
                error: None,
            });
        self.app.drive_lua_tasks();
    }

    pub(crate) fn respond_ask_with_error(&mut self, id: u64, kind: protocol::EngineAskErrorKind) {
        self.app
            .dispatch_engine_event(protocol::EngineEvent::EngineAskResponse {
                id,
                message: None,
                error: Some(protocol::EngineAskError {
                    kind,
                    message: kind.as_str().to_string(),
                }),
            });
        self.app.drive_lua_tasks();
    }

    pub(crate) fn respond_ask_with_tool_call(&mut self, id: u64, call_id: &str, name: &str) {
        self.app
            .dispatch_engine_event(protocol::EngineEvent::EngineAskResponse {
                id,
                message: Some(protocol::Message::assistant(
                    None,
                    None,
                    Some(vec![protocol::ToolCall::new(
                        call_id.into(),
                        protocol::FunctionCall {
                            name: name.into(),
                            arguments: "{}".into(),
                        },
                    )]),
                )),
                error: None,
            });
        self.app.drive_lua_tasks();
    }

    pub(crate) fn publish_turn_end_for_probe(&mut self) {
        self.app.core.signals.emit_dyn(
            "turn_end",
            std::rc::Rc::new(smelt_core::signals::TurnEnd {
                cancelled: false,
                continuation_token: None,
                error_kind: None,
                retry_at_ms: None,
            }),
        );
        self.app.pump_lua();
    }

    fn bump_input_epoch_for_probe(&mut self) {
        self.app.bump_epoch("input_epoch");
        self.app.pump_lua();
    }

    fn probe_stale_prompt_prediction_response(&mut self, variant: u8) {
        let seq = self.app.conversation.session().history.len();
        self.app
            .conversation
            .append_history_item(protocol::HistoryItem::user(protocol::Content::text(
                format!("fuzz stale prompt prediction {variant}/{seq}"),
            )));
        {
            self.app.bump_epoch("history_epoch");
            self.app.pump_lua();
        }
        self.publish_turn_end_for_probe();
        let prediction_id = self
            .drain_engine_ask_ids()
            .last()
            .copied()
            .expect("prediction probe should issue EngineAsk");

        self.bump_input_epoch_for_probe();
        self.respond_ask_with_text(prediction_id, "stale prompt placeholder");
        assert_eq!(
            self.app.placeholder_text(crate::app::PROMPT_WIN),
            None,
            "stale prompt prediction response installed a placeholder in probe variant {variant}"
        );
    }

    fn prediction_history_for_probe(&self, variant: u8) -> Vec<protocol::HistoryItem> {
        vec![protocol::HistoryItem::user(protocol::Content::text(
            format!("fuzz prompt prediction history {variant}"),
        ))]
    }

    fn focus_prompt_without_clearing_transients(&mut self) {
        // Prompt-docked pickers can open from pending Lua tasks after the
        // probe's initial cleanup. Reloading drops their prompt keymaps while
        // leaving the prediction placeholder intact for the oracle below.
        if self.app.overlays.has_pickers() {
            self.reload_lua();
        }
        self.app.app_focus = AppFocus::Prompt;
        self.app.set_terminal_focus_for_harness(true);
        let _ = self.app.ui.set_focus(crate::app::PROMPT_WIN);
        if let Some(win) = self.app.ui.win_mut(crate::app::PROMPT_WIN) {
            if win.vim_enabled() {
                win.set_vim_mode(VimMode::Insert);
            }
        }
    }

    fn assert_prompt_typing_and_motion(&mut self, variant: u8) {
        self.render_silent();
        assert!(
            self.prompt_text_input_ready_for_turn_probe(),
            "prompt is not ready for text input in probe variant {variant}"
        );

        for (idx, ch) in "ab".chars().enumerate() {
            self.type_char(ch);
            let cpos_before_render = self.prompt_cpos();
            self.render_silent();
            let actual_cpos = self.prompt_cpos();
            let state = self.state();
            assert_eq!(
                actual_cpos,
                idx + 1,
                "prompt cursor did not advance after typing {ch:?} in probe variant {variant}; cpos_before_render {}, prompt_text {:?}, app_focus {:?}, overlay {:?}, cmdline {}, agent {}, pending_chord {}, pending_pane_chord {}, overlay_count {}, picker_count {}",
                cpos_before_render,
                state.prompt_text,
                state.app_focus,
                state.focused_overlay,
                state.cmdline_open,
                state.agent_running,
                self.app.timers.pending_chord.is_some(),
                self.app.timers.pending_pane_chord.is_some(),
                self.app.ui.overlay_count(),
                self.app.overlays.picker_count(),
            );
        }
        assert_eq!(self.state().prompt_text, "ab");

        self.press(KeyCode::Left);
        self.render_silent();
        assert_eq!(
            self.prompt_cpos(),
            1,
            "left motion did not move prompt cursor in probe variant {variant}",
        );
        self.type_char('X');
        self.render_silent();
        assert_eq!(self.state().prompt_text, "aXb");
        assert_eq!(self.prompt_cpos(), 2);

        self.press(KeyCode::End);
        self.type_text("cd");
        self.render_silent();
        assert_eq!(self.state().prompt_text, "aXbcd");
        assert_eq!(self.prompt_cpos(), 5);
    }

    /// Side-channel: drive the exact bug class from #15. After a turn lifecycle
    /// transition, typing must advance the prompt cursor; left motion must also
    /// move the insertion point. A stuck cursor reverses "ab" into "ba" and
    /// fails this probe immediately.
    pub fn probe_prompt_cursor_after_turn(&mut self, variant: u8) {
        if self.agent_running() {
            self.cancel();
        }
        self.force_prompt_keyboard_focus();
        self.app.prompt.clear_queue();
        let _ = self.run_lua(r#"smelt.prompt.set_text("")"#);
        if variant % 4 == 1 {
            self.install_prompt_cursor_trap(variant);
        }

        let mut turn_id = 10_000 + u64::from(variant);
        let prediction_probe = variant & 0x80 != 0 && matches!(variant % 4, 0 | 1);
        self.start_turn(turn_id);
        if variant & 0x40 != 0 {
            self.press(KeyCode::Esc);
            self.press(KeyCode::Esc);
            if !self.agent_running() {
                turn_id += 1;
                self.start_turn(turn_id);
            }
        } else if variant & 0x20 != 0 {
            self.press(KeyCode::Esc);
        }
        let mut prediction_ids = Vec::new();
        match variant % 4 {
            2 => self.feed_one(SourceEvent::engine(EngineEvent::TurnError {
                message: "fuzz turn error".into(),
                kind: None,
                retry_at_ms: None,
            })),
            3 => self.cancel(),
            _ => {
                let history = if prediction_probe {
                    self.prediction_history_for_probe(variant)
                } else {
                    vec![]
                };
                self.feed_one(SourceEvent::engine(EngineEvent::TurnComplete {
                    turn_id,
                    history: (!history.is_empty())
                        .then(|| protocol::CanonicalHistoryDelta::new(0, history)),
                    meta: None,
                }));
                if prediction_probe {
                    prediction_ids = self.drain_engine_ask_ids();
                }
            }
        }
        let reloaded = variant % 8 >= 4;
        if reloaded {
            self.reload_lua();
        }
        if variant & 0x10 != 0 {
            self.probe_stale_prompt_prediction_response(variant);
        }
        if prediction_probe {
            if !reloaded {
                if let Some(id) = prediction_ids.last().copied() {
                    self.respond_ask_with_text(id, "predicted follow-up");
                    assert_eq!(
                        self.app.placeholder_text(crate::app::PROMPT_WIN).as_deref(),
                        Some("predicted follow-up"),
                        "turn-end prediction response did not install placeholder in probe variant {variant}"
                    );
                }
            }
            self.focus_prompt_without_clearing_transients();
        } else {
            self.force_prompt_keyboard_focus();
            // Turn-end hooks can leave prompt-owned transients behind; clear after
            // they are quiesced so the typing oracle starts from a known buffer.
            let _ = self.run_lua(r#"smelt.prompt.set_text("")"#);
            assert!(
                self.prompt_plain_insert_ready(),
                "prompt is not ready for plain insertion in probe variant {variant}"
            );
        }

        self.assert_prompt_typing_and_motion(variant);
    }

    fn install_compaction_prepare_fixture(&mut self) {
        let mut settings = self.app.core.config.settings.clone();
        settings.auto_compact = true;
        settings.compact_threshold = 0.8;
        settings.compact_keep_recent_groups = 1.0;
        self.app.set_settings_for_harness(settings);

        self.app.core.config.context_window = Some(100);
        self.app.conversation.reset_context_accounting_for_harness();
        self.app
            .conversation
            .replace_history_for_harness(vec![protocol::HistoryItem::user(
                protocol::Content::text("u1"),
            )]);
        self.push_assistant_text("a1");
        self.app
            .conversation
            .append_history_item(protocol::HistoryItem::user(protocol::Content::text("u2")));
    }

    /// Side-channel: exercise the real compaction prepare-request path through
    /// `HostCall::PrepareRequest`, pair the generated EngineAsk with a response,
    /// and assert the replacement arrives while active-turn state survives.
    pub fn probe_compaction_prepare_request(&mut self, variant: u8) {
        // Production reaches host-call dispatch after draining Lua callbacks and
        // tasks for the tick. Mirror that before installing the synthetic
        // compaction history so stale callbacks from earlier random input are
        // not attributed to the prepare-request lifecycle.
        {
            self.app.flush_lua_callbacks();
            self.app.drive_lua_tasks();
        }
        let drained = self.drain_engine_sends();
        let drained_request = drained.iter().any(|cmd| {
            matches!(
                cmd,
                protocol::UiCommand::StartTurn(_) | protocol::UiCommand::EngineAsk { .. }
            )
        });
        let logged_request = self.actions().iter().rev().any(|action| {
            matches!(
                action,
                Action::EngineSend(cmd)
                    if matches!(cmd.as_ref(), protocol::UiCommand::StartTurn(_))
            )
        });
        // Prepare-request itself drains pending engine events before invoking
        // hooks. Do that before deciding whether the probe should preserve an
        // active turn so a queued completion from an earlier synthetic submit
        // is not misattributed to compaction.
        while let Ok(ev) = self.app.core.engine.try_recv() {
            self.app.dispatch_engine_event(ev);
        }
        // Prepare-request runs before a model request is dispatched. Keep the
        // probe on that lifecycle edge rather than injecting compaction while
        // a synthetic tool call or already-submitted model request is in flight.
        let in_flight = drained_request
            || logged_request
            || (self.agent_running() && !self.pending_tool_invocation_ids().is_empty());
        if in_flight {
            return;
        }

        self.install_compaction_prepare_fixture();
        if !self.agent_running() || variant % 2 == 1 {
            self.start_turn(20_000 + u64::from(variant));
        }

        let full_history = protocol::history_to_messages(&self.app.model_history());
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        {
            self.app
                .dispatch_host_call(engine::HostCall::PrepareRequest {
                    turn_id: self.current_turn_id().expect("active request turn"),
                    messages: engine::PreparedRequestMessages::model_only(full_history),
                    estimated_tokens: 200,
                    reply: tx,
                });
        }

        let sends = self.drain_engine_sends();
        let ask_id = match sends
            .into_iter()
            .filter_map(|cmd| match cmd {
                protocol::UiCommand::EngineAsk { id, .. } => Some(id),
                _ => None,
            })
            .next_back()
        {
            Some(id) => id,
            None => match rx.try_recv() {
                Ok(decision) => {
                    panic!("expected compaction EngineAsk, got {decision:?}")
                }
                Err(err) => panic!("compaction prepare request produced no EngineAsk: {err}"),
            },
        };

        {
            self.app
                .dispatch_engine_event(protocol::EngineEvent::EngineAskResponse {
                    id: ask_id,
                    message: Some(protocol::Message::assistant(
                        Some(protocol::Content::text("# Goal\nok")),
                        None,
                        None,
                    )),
                    error: None,
                });
            self.app.drive_lua_tasks();
        }

        let replacement = match rx
            .try_recv()
            .expect("compaction prepare reply should be ready")
        {
            engine::HostRequestDecision::Replace { messages, .. } => messages,
            decision => panic!("expected compaction replacement, got {decision:?}"),
        };
        assert!(!replacement.is_empty(), "compaction replacement is empty");
        self.tick_signals();
        assert!(self.agent_running(), "compaction ended the active turn");
    }
}
