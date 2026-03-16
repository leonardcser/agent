use super::*;

impl App {
    // ── Agent lifecycle ──────────────────────────────────────────────────

    pub(super) fn begin_agent_turn(&mut self, display: &str, content: Content) -> TurnState {
        self.screen.begin_turn();
        self.show_user_message(display);
        let text = content.text_content();
        if self.session.first_user_message.is_none() {
            self.session.first_user_message = Some(text.clone());
        }
        self.screen.set_throbber(render::Throbber::Working);

        let turn_id = self.next_turn_id;
        self.next_turn_id += 1;

        self.engine.send(UiCommand::StartTurn {
            turn_id,
            input: text.clone(),
            mode: self.mode,
            model: self.model.clone(),
            reasoning_effort: self.reasoning_effort,
            history: self.history.clone(),
            api_base: Some(self.api_base.clone()),
            api_key: Some(std::env::var(&self.api_key_env).unwrap_or_default()),
            session_id: self.session.id.clone(),
            model_config_overrides: None,
            permission_overrides: None,
        });

        TurnState {
            turn_id,
            pending: None,
            steered_count: 0,
            _perf: crate::perf::begin("agent_turn"),
        }
    }

    pub(super) fn begin_custom_command_turn(
        &mut self,
        cmd: crate::custom_commands::CustomCommand,
    ) -> TurnState {
        let evaluated = crate::custom_commands::evaluate(&cmd.body);
        let display = format!("/{}", cmd.name);

        // Resolve model/provider overrides
        let (model, api_base, api_key) = if cmd.overrides.model.is_some()
            || cmd.overrides.provider.is_some()
        {
            let target_model = cmd.overrides.model.as_deref();
            let target_provider = cmd.overrides.provider.as_deref();
            if let Some(resolved) = self.available_models.iter().find(|m| {
                let model_match = target_model.is_none_or(|tm| m.model_name == tm || m.key == tm);
                let prov_match = target_provider.is_none_or(|tp| m.provider_name == tp);
                model_match && prov_match
            }) {
                (
                    resolved.model_name.clone(),
                    resolved.api_base.clone(),
                    std::env::var(&resolved.api_key_env).unwrap_or_default(),
                )
            } else {
                (
                    self.model.clone(),
                    self.api_base.clone(),
                    std::env::var(&self.api_key_env).unwrap_or_default(),
                )
            }
        } else {
            (
                self.model.clone(),
                self.api_base.clone(),
                std::env::var(&self.api_key_env).unwrap_or_default(),
            )
        };

        let reasoning = cmd
            .overrides
            .reasoning_effort
            .as_deref()
            .map(|s| match s.to_lowercase().as_str() {
                "low" => protocol::ReasoningEffort::Low,
                "medium" => protocol::ReasoningEffort::Medium,
                "high" => protocol::ReasoningEffort::High,
                _ => protocol::ReasoningEffort::Off,
            })
            .unwrap_or(self.reasoning_effort);

        let model_config_overrides = {
            let o = &cmd.overrides;
            if o.temperature.is_some()
                || o.top_p.is_some()
                || o.top_k.is_some()
                || o.min_p.is_some()
                || o.repeat_penalty.is_some()
            {
                Some(protocol::ModelConfigOverrides {
                    temperature: o.temperature,
                    top_p: o.top_p,
                    top_k: o.top_k,
                    min_p: o.min_p,
                    repeat_penalty: o.repeat_penalty,
                })
            } else {
                None
            }
        };

        let permission_overrides = {
            let o = &cmd.overrides;
            if o.tools.is_some() || o.bash.is_some() || o.web_fetch.is_some() {
                Some(protocol::PermissionOverrides {
                    tools: o.tools.as_ref().map(|r| protocol::RuleSetOverride {
                        allow: r.allow.clone(),
                        ask: r.ask.clone(),
                        deny: r.deny.clone(),
                    }),
                    bash: o.bash.as_ref().map(|r| protocol::RuleSetOverride {
                        allow: r.allow.clone(),
                        ask: r.ask.clone(),
                        deny: r.deny.clone(),
                    }),
                    web_fetch: o.web_fetch.as_ref().map(|r| protocol::RuleSetOverride {
                        allow: r.allow.clone(),
                        ask: r.ask.clone(),
                        deny: r.deny.clone(),
                    }),
                })
            } else {
                None
            }
        };

        self.screen.begin_turn();
        self.show_user_message(&display);
        if self.session.first_user_message.is_none() {
            self.session.first_user_message = Some(display.clone());
        }
        self.screen.set_throbber(render::Throbber::Working);

        let turn_id = self.next_turn_id;
        self.next_turn_id += 1;

        self.engine.send(UiCommand::StartTurn {
            turn_id,
            input: evaluated,
            mode: self.mode,
            model,
            reasoning_effort: reasoning,
            history: self.history.clone(),
            api_base: Some(api_base),
            api_key: Some(api_key),
            session_id: self.session.id.clone(),
            model_config_overrides,
            permission_overrides,
        });

        TurnState {
            turn_id,
            pending: None,
            steered_count: 0,
            _perf: crate::perf::begin("agent_turn"),
        }
    }

    /// Lightweight cancel: stop the engine turn without saving session,
    /// generating titles, or triggering auto-compact. Used before rewind/clear
    /// where the history will be mutated immediately after.
    pub(super) fn cancel_agent(&mut self) {
        self.engine.send(UiCommand::Cancel);
        self.screen.set_throbber(render::Throbber::Interrupted);
        self.queued_messages.clear();
    }

    pub(super) fn finish_turn(&mut self, cancelled: bool) {
        self.screen.flush_blocks();
        if cancelled {
            self.engine.send(UiCommand::Cancel);
            self.screen.set_throbber(render::Throbber::Interrupted);
            let leftover = std::mem::take(&mut self.queued_messages);
            if !leftover.is_empty() {
                let mut combined = leftover.join("\n");
                if !self.input.buf.is_empty() {
                    combined.push('\n');
                    combined.push_str(&self.input.buf);
                }
                self.input.buf = combined;
                self.input.cpos = self.input.buf.len();
            }
        } else {
            self.screen.set_throbber(render::Throbber::Done);
        }
        self.save_session();
        self.maybe_generate_title();
        state::set_mode(self.mode);
        self.maybe_auto_compact();
    }

    // ── Engine events ────────────────────────────────────────────────────

    pub fn handle_engine_event(
        &mut self,
        ev: EngineEvent,
        turn_id: u64,
        pending: &mut Option<PendingTool>,
        steered_count: &mut usize,
    ) -> SessionControl {
        match ev {
            EngineEvent::Ready => SessionControl::Continue,
            EngineEvent::TokenUsage {
                prompt_tokens,
                completion_tokens,
                tokens_per_sec,
            } => {
                if prompt_tokens > 0 {
                    self.screen.set_context_tokens(prompt_tokens);
                    self.session.context_tokens = Some(prompt_tokens);
                }
                if let Some(tps) = tokens_per_sec {
                    self.screen.record_tokens_per_sec(tps);
                }
                crate::metrics::append(&crate::metrics::MetricsEntry {
                    timestamp_ms: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64,
                    prompt_tokens,
                    completion_tokens: completion_tokens.unwrap_or(0),
                    model: self.model.clone(),
                });
                self.screen.set_throbber(render::Throbber::Working);
                SessionControl::Continue
            }
            EngineEvent::ToolOutput { chunk, .. } => {
                self.screen.append_active_output(&chunk);
                SessionControl::Continue
            }
            EngineEvent::Steered { text, count } => {
                let drain_n = count.min(self.queued_messages.len());
                self.queued_messages.drain(..drain_n);
                *steered_count = steered_count.saturating_sub(drain_n);
                // Only render if the message is still queued (not unqueued by Esc).
                if drain_n > 0 {
                    self.screen.push(Block::User { text });
                }
                SessionControl::Continue
            }
            EngineEvent::Thinking { content } => {
                self.screen.push(Block::Thinking { content });
                SessionControl::Continue
            }
            EngineEvent::Text { content } => {
                self.screen.push(Block::Text { content });
                SessionControl::Continue
            }
            EngineEvent::ToolStarted {
                tool_name,
                args,
                summary,
                ..
            } => {
                self.screen.start_tool(tool_name.clone(), summary, args);
                *pending = Some(PendingTool { name: tool_name });
                SessionControl::Continue
            }
            EngineEvent::ToolFinished { result, .. } => {
                if pending.is_some() {
                    let status = if result.is_error {
                        ToolStatus::Err
                    } else {
                        ToolStatus::Ok
                    };
                    let output = Some(ToolOutput {
                        content: result.content,
                        is_error: result.is_error,
                    });
                    self.screen.finish_tool(status, output);
                }
                *pending = None;
                self.screen
                    .set_running_procs(self.engine.processes.running_count());
                SessionControl::Continue
            }
            EngineEvent::RequestPermission {
                request_id,
                tool_name,
                args,
                confirm_message,
                approval_pattern,
                summary,
                ..
            } => SessionControl::NeedsConfirm {
                tool_name,
                desc: confirm_message,
                args,
                approval_pattern,
                summary,
                request_id,
            },
            EngineEvent::RequestAnswer { request_id, args } => {
                SessionControl::NeedsAskQuestion { args, request_id }
            }
            EngineEvent::Retrying { delay_ms, attempt } => {
                self.screen.set_throbber(render::Throbber::Retrying {
                    delay: Duration::from_millis(delay_ms),
                    attempt,
                });
                SessionControl::Continue
            }
            EngineEvent::ProcessCompleted { id, exit_code } => {
                let msg = match exit_code {
                    Some(0) => format!("Background process {id} has finished."),
                    Some(c) => format!("Background process {id} exited with code {c}."),
                    None => format!("Background process {id} exited."),
                };
                self.screen.push(Block::Text { content: msg });
                self.screen
                    .set_running_procs(self.engine.processes.running_count());
                SessionControl::Continue
            }
            EngineEvent::CompactionComplete { messages } => {
                if self.pending_compact_epoch != self.compact_epoch {
                    self.screen.set_throbber(render::Throbber::Done);
                    return SessionControl::Continue;
                }
                self.history = messages;
                self.save_session();
                self.screen.push(Block::Text {
                    content: "conversation compacted".into(),
                });
                self.screen.set_throbber(render::Throbber::Done);
                SessionControl::Continue
            }
            EngineEvent::TitleGenerated { title } => {
                self.session.title = Some(title);
                self.pending_title = false;
                self.save_session();
                SessionControl::Continue
            }
            EngineEvent::Messages {
                turn_id: id,
                messages,
            } => {
                if id == turn_id {
                    self.history = messages;
                }
                SessionControl::Continue
            }
            EngineEvent::TurnComplete {
                turn_id: id,
                messages,
            } => {
                if id != turn_id {
                    // Stale event from a previous (cancelled) turn — ignore.
                    return SessionControl::Continue;
                }
                self.history = messages;
                SessionControl::Done
            }
            EngineEvent::TurnError { message } => {
                self.screen.push(Block::Error { message });
                SessionControl::Done
            }
            EngineEvent::Shutdown { .. } => SessionControl::Done,
        }
    }

    /// Handle engine events that arrive when no agent turn is active.
    pub(super) fn handle_engine_event_idle(&mut self, ev: EngineEvent) {
        match ev {
            // Ignore stale Messages snapshots from cancelled/completed turns.
            // These would overwrite a freshly cleared history (e.g. after /clear).
            EngineEvent::Messages { .. } => {}
            EngineEvent::TurnComplete { messages, .. } => {
                // Accept final messages from a just-cancelled turn so that
                // denial tool results are persisted before the session is saved.
                if !self.history.is_empty() && !messages.is_empty() {
                    self.history = messages;
                    self.rebuild_screen_from_history();
                    self.save_session();
                }
            }
            EngineEvent::CompactionComplete { messages } => {
                if self.pending_compact_epoch != self.compact_epoch {
                    self.screen.set_throbber(render::Throbber::Done);
                    return;
                }
                self.history = messages;
                self.save_session();
                self.screen.push(Block::Text {
                    content: "conversation compacted".into(),
                });
                self.screen.set_throbber(render::Throbber::Done);
            }
            EngineEvent::TitleGenerated { title } => {
                self.session.title = Some(title);
                self.pending_title = false;
                self.save_session();
            }
            EngineEvent::ProcessCompleted { id, exit_code } => {
                let msg = match exit_code {
                    Some(0) => format!("Background process {id} has finished."),
                    Some(c) => format!("Background process {id} exited with code {c}."),
                    None => format!("Background process {id} exited."),
                };
                self.screen.push(Block::Text { content: msg });
                self.screen
                    .set_running_procs(self.engine.processes.running_count());
            }
            _ => {}
        }
    }

    // ── Dialog resolution ────────────────────────────────────────────────

    pub(super) fn handle_dialog_result(
        &mut self,
        result: render::DialogResult,
        anchor: Option<u16>,
        agent: &mut Option<TurnState>,
    ) {
        match result {
            render::DialogResult::Confirm {
                choice,
                message,
                tool_name,
                request_id,
            } => {
                self.confirm_context = None;
                let should_cancel =
                    self.resolve_confirm((choice, message), request_id, &tool_name, agent);
                self.screen.clear_dialog_area(anchor);
                if should_cancel && agent.is_some() {
                    self.finish_turn(true);
                    *agent = None;
                }
            }
            render::DialogResult::Question { answer, request_id } => {
                let should_cancel = self.resolve_question(answer, request_id, agent);
                self.screen.clear_dialog_area(anchor);
                if should_cancel && agent.is_some() {
                    self.finish_turn(true);
                    *agent = None;
                }
            }
            render::DialogResult::Rewind {
                block_idx,
                restore_vim_insert,
            } => {
                if let Some(idx) = block_idx {
                    if agent.is_some() {
                        self.cancel_agent();
                        *agent = None;
                    }
                    if let Some((text, images)) = self.rewind_to(idx) {
                        self.input.restore_from_rewind(text, images);
                    }
                    // rewind_to → redraw(true) already purged the screen;
                    // drain stale engine events and save the truncated state.
                    while self.engine.try_recv().is_ok() {}
                    self.save_session();
                    self.screen.set_show_tool_in_dialog(false);
                } else {
                    // Dialog was cancelled — clean up the dialog overlay.
                    if restore_vim_insert {
                        self.input.set_vim_mode(vim::ViMode::Insert);
                    }
                    self.screen.clear_dialog_area(anchor);
                }
            }
            render::DialogResult::Resume { session_id } => {
                let mut clear = true;
                if let Some(id) = session_id {
                    if let Some(loaded) = session::load(&id) {
                        self.load_session(loaded);
                        self.rebuild_screen_from_history();
                        if let Some(tokens) = self.session.context_tokens {
                            self.screen.set_context_tokens(tokens);
                        }
                        self.screen.flush_blocks();
                        clear = false;
                    }
                }
                if clear {
                    self.screen.clear_dialog_area(anchor);
                }
            }
            render::DialogResult::PsClosed | render::DialogResult::Dismissed => {
                self.screen.clear_dialog_area(anchor);
            }
        }
    }

    /// Resolve a completed confirm dialog choice.
    /// Returns `true` if the agent should be cancelled.
    pub(super) fn resolve_confirm(
        &mut self,
        (choice, message): (ConfirmChoice, Option<String>),
        request_id: u64,
        tool_name: &str,
        agent: &mut Option<TurnState>,
    ) -> bool {
        let label = match &choice {
            ConfirmChoice::Yes => "approved",
            ConfirmChoice::Always => "always",
            ConfirmChoice::AlwaysPattern(pat) => pat.as_str(),
            ConfirmChoice::No => "denied",
        };
        if let Some(ref msg) = message {
            self.screen
                .set_active_user_message(format!("{label}: {msg}"));
        }
        match choice {
            ConfirmChoice::Yes => {
                self.screen.set_active_status(ToolStatus::Pending);
                self.engine.send(UiCommand::PermissionDecision {
                    request_id,
                    approved: true,
                    message,
                });
                // Auto-switch to Apply mode when a plan is approved.
                if tool_name == "exit_plan_mode" {
                    self.mode = Mode::Apply;
                    state::set_mode(self.mode);
                    self.engine.send(UiCommand::SetMode { mode: self.mode });
                    self.screen.mark_dirty();
                }
                false
            }
            ConfirmChoice::Always => {
                self.auto_approved.insert(tool_name.to_string(), vec![]);
                self.screen.set_active_status(ToolStatus::Pending);
                self.engine.send(UiCommand::PermissionDecision {
                    request_id,
                    approved: true,
                    message,
                });
                false
            }
            ConfirmChoice::AlwaysPattern(ref pattern) => {
                if let Ok(compiled) = glob::Pattern::new(pattern) {
                    self.auto_approved
                        .entry(tool_name.to_string())
                        .or_default()
                        .push(compiled);
                }
                self.screen.set_active_status(ToolStatus::Pending);
                self.engine.send(UiCommand::PermissionDecision {
                    request_id,
                    approved: true,
                    message,
                });
                false
            }
            ConfirmChoice::No => {
                let has_message = message.is_some();
                self.engine.send(UiCommand::PermissionDecision {
                    request_id,
                    approved: false,
                    message,
                });
                self.screen.finish_tool(ToolStatus::Denied, None);
                if has_message {
                    // Deny with feedback — let the agent continue with the message.
                    false
                } else {
                    // Deny without message — stop the agent.
                    engine::log::entry(
                        engine::log::Level::Info,
                        "agent_stop",
                        &serde_json::json!({
                            "reason": "confirm_denied",
                            "tool": tool_name,
                        }),
                    );
                    if let Some(ref mut ag) = agent {
                        ag.pending = None;
                    }
                    true
                }
            }
        }
    }

    /// Resolve a completed question dialog.
    /// `answer` is `Some(json)` on confirm, `None` on cancel.
    /// Returns `true` if the agent should be cancelled.
    pub(super) fn resolve_question(
        &mut self,
        answer: Option<String>,
        request_id: u64,
        agent: &mut Option<TurnState>,
    ) -> bool {
        match answer {
            Some(json) => {
                self.engine.send(UiCommand::QuestionAnswer {
                    request_id,
                    answer: Some(json),
                });
                false
            }
            None => {
                engine::log::entry(
                    engine::log::Level::Info,
                    "agent_stop",
                    &serde_json::json!({
                        "reason": "question_cancelled",
                    }),
                );
                self.engine.send(UiCommand::QuestionAnswer {
                    request_id,
                    answer: None,
                });
                self.screen.finish_tool(ToolStatus::Denied, None);
                if let Some(ref mut ag) = agent {
                    ag.pending = None;
                }
                true
            }
        }
    }

    // ── Control dispatch ─────────────────────────────────────────────────

    pub(super) fn dispatch_control(
        &mut self,
        ctrl: SessionControl,
        pending: &mut Option<PendingTool>,
        deferred_dialog: &mut Option<DeferredDialog>,
        active_dialog: &mut Option<Box<dyn render::Dialog>>,
        last_keypress: Option<Instant>,
    ) -> LoopAction {
        match ctrl {
            SessionControl::Continue => LoopAction::Continue,
            SessionControl::Done => LoopAction::Done,
            SessionControl::NeedsConfirm {
                tool_name,
                desc,
                args,
                approval_pattern,
                summary,
                request_id,
            } => {
                let tool_name = if tool_name.is_empty() {
                    pending.as_ref().map(|p| p.name.clone()).unwrap_or_default()
                } else {
                    tool_name
                };

                // Check auto-approvals (doesn't need UI).
                if let Some(patterns) = self.auto_approved.get(&tool_name) {
                    if patterns.is_empty() || patterns.iter().any(|p| p.matches(&desc)) {
                        self.engine.send(UiCommand::PermissionDecision {
                            request_id,
                            approved: true,
                            message: None,
                        });
                        return LoopAction::Continue;
                    }
                }

                // If the user is actively typing, defer the dialog.
                let recently_typed = last_keypress
                    .is_some_and(|t| t.elapsed() < Duration::from_millis(CONFIRM_DEFER_MS));
                if recently_typed && !self.input.buf.is_empty() {
                    self.screen.set_active_status(ToolStatus::Confirm);
                    self.screen.set_pending_dialog(true);
                    *deferred_dialog = Some(DeferredDialog::Confirm {
                        tool_name,
                        desc,
                        args,
                        approval_pattern,
                        summary,
                        request_id,
                    });
                    return LoopAction::Continue;
                }

                // Close any non-blocking dialog (e.g. Ps) to make room.
                if let Some(prev) = active_dialog.take() {
                    self.screen.clear_dialog_area(prev.anchor_row());
                }
                self.confirm_context = Some(ConfirmContext {
                    tool_name: tool_name.clone(),
                    args: args.clone(),
                    request_id,
                });
                self.screen.set_active_status(ToolStatus::Confirm);
                let dialog = Box::new(ConfirmDialog::new(
                    &tool_name,
                    &desc,
                    &args,
                    approval_pattern.as_deref(),
                    summary.as_deref(),
                    request_id,
                ));
                self.open_blocking_dialog(dialog, active_dialog);
                LoopAction::Continue
            }
            SessionControl::NeedsAskQuestion { args, request_id } => {
                // If the user is actively typing, defer the dialog.
                let recently_typed = last_keypress
                    .is_some_and(|t| t.elapsed() < Duration::from_millis(CONFIRM_DEFER_MS));
                if recently_typed && !self.input.buf.is_empty() {
                    self.screen.set_pending_dialog(true);
                    *deferred_dialog = Some(DeferredDialog::AskQuestion { args, request_id });
                    return LoopAction::Continue;
                }

                // Close any non-blocking dialog (e.g. Ps) to make room.
                if let Some(prev) = active_dialog.take() {
                    self.screen.clear_dialog_area(prev.anchor_row());
                }
                self.screen.set_active_status(ToolStatus::Confirm);
                let questions = render::parse_questions(&args);
                let dialog = Box::new(QuestionDialog::new(questions, request_id));
                self.open_blocking_dialog(dialog, active_dialog);
                LoopAction::Continue
            }
        }
    }
}
