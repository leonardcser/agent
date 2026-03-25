use super::*;

impl App {
    // ── Agent lifecycle ──────────────────────────────────────────────────

    pub(super) fn begin_agent_turn(&mut self, display: &str, content: Content) -> TurnState {
        self.sleep_inhibit.acquire();
        self.input_prediction = None;
        self.screen.begin_turn();
        self.show_user_message(display, content.image_labels());
        let text = content.text_content();
        if self.session.first_user_message.is_none() {
            self.session.first_user_message = Some(text.clone());
        }
        self.maybe_generate_title(Some(&text));
        self.screen.set_throbber(render::Throbber::Working);

        let turn_id = self.next_turn_id;
        self.next_turn_id += 1;

        self.engine.send(UiCommand::StartTurn {
            turn_id,
            content,
            mode: self.mode,
            model: self.model.clone(),
            reasoning_effort: self.reasoning_effort,
            history: self.history.clone(),
            api_base: Some(self.api_base.clone()),
            api_key: Some(self.api_key()),
            session_id: self.session.id.clone(),
            session_dir: crate::session::dir_for(&self.session),
            model_config_overrides: None,
            permission_overrides: None,
        });

        TurnState {
            turn_id,
            pending: Vec::new(),
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
        let (model, api_base, api_key) = {
            let target_model = cmd.overrides.model.as_deref();
            let target_provider = cmd.overrides.provider.as_deref();
            let resolved = (target_model.is_some() || target_provider.is_some())
                .then(|| {
                    self.available_models.iter().find(|m| {
                        let model_match =
                            target_model.is_none_or(|tm| m.model_name == tm || m.key == tm);
                        let prov_match = target_provider.is_none_or(|tp| m.provider_name == tp);
                        model_match && prov_match
                    })
                })
                .flatten();
            match resolved {
                Some(r) => (
                    r.model_name.clone(),
                    r.api_base.clone(),
                    std::env::var(&r.api_key_env).unwrap_or_default(),
                ),
                None => (self.model.clone(), self.api_base.clone(), self.api_key()),
            }
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

        self.sleep_inhibit.acquire();
        self.screen.begin_turn();
        self.show_user_message(&display, vec![]);
        if self.session.first_user_message.is_none() {
            self.session.first_user_message = Some(display.clone());
        }
        self.maybe_generate_title(Some(&evaluated));
        self.screen.set_throbber(render::Throbber::Working);

        let turn_id = self.next_turn_id;
        self.next_turn_id += 1;

        self.engine.send(UiCommand::StartTurn {
            turn_id,
            content: Content::text(evaluated),
            mode: self.mode,
            model,
            reasoning_effort: reasoning,
            history: self.history.clone(),
            api_base: Some(api_base),
            api_key: Some(api_key),
            session_id: self.session.id.clone(),
            session_dir: crate::session::dir_for(&self.session),
            model_config_overrides,
            permission_overrides,
        });

        TurnState {
            turn_id,
            pending: Vec::new(),
            steered_count: 0,
            _perf: crate::perf::begin("agent_turn"),
        }
    }

    /// Lightweight cancel: stop the engine turn without saving session,
    /// generating titles, or triggering auto-compact. Used before rewind/clear
    /// where the history will be mutated immediately after.
    pub(super) fn cancel_agent(&mut self) {
        self.sleep_inhibit.release();
        self.engine.send(UiCommand::Cancel);
        self.screen.set_throbber(render::Throbber::Interrupted);
        self.queued_messages.clear();
    }

    pub(super) fn finish_turn(&mut self, cancelled: bool) {
        self.sleep_inhibit.release();
        self.screen.flush_blocks();
        if cancelled {
            self.engine.send(UiCommand::Cancel);
            self.screen.set_throbber(render::Throbber::Interrupted);
            // If a title/slug generation was in-flight, discard it so stale
            // TitleGenerated events don't update the session. But if a slug
            // was already set before this turn, keep it.
            if self.pending_title {
                self.pending_title = false;
                self.session.slug = None;
                self.screen.clear_task_label();
            }
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
            // Fire async prediction for the user's next input.
            self.input_prediction = None;
            if self.show_prediction {
                // Collect last 3 user messages + last assistant message for
                // richer prediction context.
                let mut context: Vec<protocol::Message> = self
                    .history
                    .iter()
                    .rev()
                    .filter(|m| m.role == protocol::Role::User)
                    .take(3)
                    .cloned()
                    .collect();
                context.reverse();
                if let Some(msg) = self
                    .history
                    .iter()
                    .rev()
                    .find(|m| m.role == protocol::Role::Assistant)
                    .cloned()
                {
                    context.push(msg);
                }
                if !context.is_empty() {
                    self.predict_generation += 1;
                    self.engine.send(UiCommand::PredictInput {
                        history: context,
                        model: self.model.clone(),
                        api_base: Some(self.api_base.clone()),
                        api_key: Some(self.api_key()),
                        generation: self.predict_generation,
                    });
                }
            }
        }
        let meta = self
            .pending_turn_meta
            .take()
            .or_else(|| self.screen.turn_meta());
        if let Some(meta) = meta {
            self.turn_metas.push((self.history.len(), meta));
        }
        self.snapshot_tokens();
        self.save_session();
        state::set_mode(self.mode);
        self.maybe_auto_compact();
    }

    // ── Engine events ────────────────────────────────────────────────────

    pub fn handle_engine_event(
        &mut self,
        ev: EngineEvent,
        turn_id: u64,
        pending: &mut Vec<PendingTool>,
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
            EngineEvent::ToolOutput { call_id, chunk } => {
                self.screen.append_active_output(&call_id, &chunk);
                SessionControl::Continue
            }
            EngineEvent::Steered { text, count } => {
                let drain_n = count.min(self.queued_messages.len());
                self.queued_messages.drain(..drain_n);
                *steered_count = steered_count.saturating_sub(drain_n);
                // Only render if the message is still queued (not unqueued by Esc).
                if drain_n > 0 {
                    self.screen.push(Block::User {
                        text,
                        image_labels: vec![],
                    });
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
                call_id,
                tool_name,
                args,
                summary,
            } => {
                self.screen
                    .start_tool(call_id.clone(), tool_name.clone(), summary, args);
                pending.push(PendingTool {
                    call_id,
                    name: tool_name,
                });
                SessionControl::Continue
            }
            EngineEvent::ToolFinished {
                call_id,
                result,
                elapsed_ms,
            } => {
                if let Some(idx) = pending.iter().position(|p| p.call_id == call_id) {
                    pending.remove(idx);
                    let status = if result.is_error {
                        ToolStatus::Err
                    } else {
                        ToolStatus::Ok
                    };
                    let output = Some(ToolOutput {
                        content: result.content,
                        is_error: result.is_error,
                    });
                    let elapsed = elapsed_ms.map(Duration::from_millis);
                    self.screen.finish_tool(&call_id, status, output, elapsed);
                }
                self.screen
                    .set_running_procs(self.engine.processes.running_count());
                SessionControl::Continue
            }
            EngineEvent::RequestPermission {
                request_id,
                call_id,
                tool_name,
                args,
                confirm_message,
                approval_patterns,
                summary,
            } => SessionControl::NeedsConfirm(ConfirmRequest {
                call_id,
                tool_name,
                desc: confirm_message,
                args,
                approval_patterns,
                outside_dir: None,
                summary,
                request_id,
            }),
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
                self.handle_process_completed(id, exit_code);
                SessionControl::Continue
            }
            EngineEvent::CompactionComplete { messages } => {
                if self.pending_compact_epoch != self.compact_epoch {
                    self.screen.set_throbber(render::Throbber::Done);
                    return SessionControl::Continue;
                }
                self.apply_compaction(messages);
                SessionControl::Continue
            }
            EngineEvent::TitleGenerated { title, slug } => {
                self.handle_title_generated(title, slug);
                SessionControl::Continue
            }
            EngineEvent::BtwResponse { content } => {
                self.screen.set_btw_response(content);
                SessionControl::Continue
            }
            EngineEvent::InputPrediction { text, generation } => {
                if generation == self.predict_generation {
                    self.handle_input_prediction(text);
                }
                SessionControl::Continue
            }
            EngineEvent::Messages {
                turn_id: id,
                messages,
            } => {
                if id == turn_id {
                    self.set_history(messages);
                }
                SessionControl::Continue
            }
            EngineEvent::TurnComplete {
                turn_id: id,
                messages,
                meta,
            } => {
                if id != turn_id {
                    return SessionControl::Continue;
                }
                self.set_history(messages);
                self.pending_turn_meta = meta;
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
                // tool results are persisted. Don't rebuild the screen —
                // the displayed blocks already reflect what the user saw
                // at cancel time, and rebuilding would cause visual flicker.
                if !self.history.is_empty() && !messages.is_empty() {
                    self.set_history(messages);
                    self.save_session();
                }
            }
            EngineEvent::CompactionComplete { messages } => {
                if self.pending_compact_epoch != self.compact_epoch {
                    self.screen.set_throbber(render::Throbber::Done);
                    return;
                }
                self.apply_compaction(messages);
            }
            EngineEvent::TitleGenerated { title, slug } => {
                self.handle_title_generated(title, slug);
            }
            EngineEvent::BtwResponse { content } => {
                self.screen.set_btw_response(content);
            }
            EngineEvent::InputPrediction { text, generation } => {
                if generation == self.predict_generation {
                    self.handle_input_prediction(text);
                }
            }
            EngineEvent::ProcessCompleted { id, exit_code } => {
                self.handle_process_completed(id, exit_code);
            }
            EngineEvent::TurnError { message } => {
                self.screen.set_throbber(render::Throbber::Done);
                self.screen
                    .notify_error(format!("compaction failed: {message}"));
            }
            _ => {}
        }
    }

    fn handle_title_generated(&mut self, title: String, slug: String) {
        if !self.pending_title {
            return;
        }
        self.session.title = Some(title);
        self.session.slug = Some(slug.clone());
        self.screen.set_task_label(slug);
        self.pending_title = false;
        self.save_session();
    }

    fn handle_input_prediction(&mut self, text: String) {
        if self.input.buf.is_empty() {
            self.input_prediction = Some(text);
            self.screen.mark_dirty();
        }
    }

    pub(super) fn api_key(&self) -> String {
        std::env::var(&self.api_key_env).unwrap_or_default()
    }

    fn handle_process_completed(&mut self, id: String, exit_code: Option<i32>) {
        let msg = match exit_code {
            Some(0) => format!("Background process {id} has finished."),
            Some(c) => format!("Background process {id} exited with code {c}."),
            None => format!("Background process {id} exited."),
        };
        self.screen.push(Block::Text { content: msg });
        self.screen
            .set_running_procs(self.engine.processes.running_count());
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
                let call_id = self
                    .confirm_context
                    .as_ref()
                    .map(|c| c.call_id.clone())
                    .unwrap_or_default();
                self.confirm_context = None;
                let should_cancel = self.resolve_confirm(
                    (choice, message),
                    &call_id,
                    request_id,
                    &tool_name,
                    agent,
                );
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
            render::DialogResult::PermissionsClosed {
                session_remaining,
                workspace_remaining,
            } => {
                self.sync_permissions(session_remaining, workspace_remaining);
                self.screen.clear_dialog_area(anchor);
            }
            render::DialogResult::PsClosed | render::DialogResult::Dismissed => {
                self.screen.clear_dialog_area(anchor);
            }
        }
    }

    pub(super) fn session_permission_entries(&self) -> Vec<render::PermissionEntry> {
        let mut entries = Vec::new();
        let mut tools: Vec<_> = self.session_approved.keys().collect();
        tools.sort();
        for tool in tools {
            let patterns = &self.session_approved[tool];
            if patterns.is_empty() {
                entries.push(render::PermissionEntry {
                    tool: tool.clone(),
                    pattern: "*".into(),
                });
            } else {
                for p in patterns {
                    entries.push(render::PermissionEntry {
                        tool: tool.clone(),
                        pattern: p.as_str().to_string(),
                    });
                }
            }
        }
        for dir in &self.session_approved_dirs {
            entries.push(render::PermissionEntry {
                tool: "directory".into(),
                pattern: dir.display().to_string(),
            });
        }
        entries
    }

    fn sync_permissions(
        &mut self,
        session_entries: Vec<render::PermissionEntry>,
        workspace_rules: Vec<crate::workspace_permissions::Rule>,
    ) {
        // Rebuild session approvals from flattened entries.
        self.session_approved.clear();
        self.session_approved_dirs.clear();
        for entry in session_entries {
            if entry.tool == "directory" {
                self.session_approved_dirs
                    .push(std::path::PathBuf::from(&entry.pattern));
            } else if entry.pattern == "*" {
                self.session_approved.entry(entry.tool).or_default();
            } else if let Ok(pat) = glob::Pattern::new(&entry.pattern) {
                self.session_approved
                    .entry(entry.tool)
                    .or_default()
                    .push(pat);
            }
        }

        // Persist and reload workspace rules.
        self.workspace_rules = workspace_rules;
        crate::workspace_permissions::save(&self.cwd, &self.workspace_rules);
        let (ws_tools, ws_dirs) =
            crate::workspace_permissions::into_approvals(&self.workspace_rules);
        self.workspace_approved = ws_tools;
        self.workspace_approved_dirs = ws_dirs;
    }

    fn reload_workspace_permissions(&mut self) {
        self.workspace_rules = crate::workspace_permissions::load(&self.cwd);
        let (ws_tools, ws_dirs) =
            crate::workspace_permissions::into_approvals(&self.workspace_rules);
        self.workspace_approved = ws_tools;
        self.workspace_approved_dirs = ws_dirs;
    }

    pub(super) fn reset_session_permissions(&mut self) {
        self.session_approved.clear();
        self.session_approved_dirs.clear();
    }

    /// Resolve a completed confirm dialog choice.
    /// Returns `true` if the agent should be cancelled.
    pub(super) fn resolve_confirm(
        &mut self,
        (choice, message): (ConfirmChoice, Option<String>),
        call_id: &str,
        request_id: u64,
        tool_name: &str,
        agent: &mut Option<TurnState>,
    ) -> bool {
        let label = match &choice {
            ConfirmChoice::Yes | ConfirmChoice::YesAutoApply => "approved",
            ConfirmChoice::Always(_) => "always",
            ConfirmChoice::AlwaysPatterns(ref pats, _) => {
                pats.first().map(|s| s.as_str()).unwrap_or("pattern")
            }
            ConfirmChoice::AlwaysDir(dir, _) => dir.as_str(),
            ConfirmChoice::No => "denied",
        };
        if let Some(ref msg) = message {
            self.screen
                .set_active_user_message(call_id, format!("{label}: {msg}"));
        }
        match choice {
            ConfirmChoice::Yes | ConfirmChoice::YesAutoApply => {
                self.screen.set_active_status(call_id, ToolStatus::Pending);
                self.engine.send(UiCommand::PermissionDecision {
                    request_id,
                    approved: true,
                    message,
                });
                if matches!(choice, ConfirmChoice::YesAutoApply) {
                    self.mode = Mode::Apply;
                    state::set_mode(self.mode);
                    self.engine.send(UiCommand::SetMode { mode: self.mode });
                    self.screen.mark_dirty();
                }
                false
            }
            ConfirmChoice::Always(scope) => {
                match scope {
                    ApprovalScope::Session => {
                        self.session_approved.insert(tool_name.to_string(), vec![]);
                    }
                    ApprovalScope::Workspace => {
                        crate::workspace_permissions::add_tool(&self.cwd, tool_name, vec![]);
                        self.reload_workspace_permissions();
                    }
                }
                self.screen.set_active_status(call_id, ToolStatus::Pending);
                self.engine.send(UiCommand::PermissionDecision {
                    request_id,
                    approved: true,
                    message,
                });
                false
            }
            ConfirmChoice::AlwaysPatterns(ref patterns, scope) => {
                match scope {
                    ApprovalScope::Session => {
                        let compiled: Vec<glob::Pattern> = patterns
                            .iter()
                            .filter_map(|p| glob::Pattern::new(p).ok())
                            .collect();
                        self.session_approved
                            .entry(tool_name.to_string())
                            .or_default()
                            .extend(compiled);
                    }
                    ApprovalScope::Workspace => {
                        crate::workspace_permissions::add_tool(
                            &self.cwd,
                            tool_name,
                            patterns.clone(),
                        );
                        self.reload_workspace_permissions();
                    }
                }
                self.screen.set_active_status(call_id, ToolStatus::Pending);
                self.engine.send(UiCommand::PermissionDecision {
                    request_id,
                    approved: true,
                    message,
                });
                false
            }
            ConfirmChoice::AlwaysDir(ref dir, scope) => {
                match scope {
                    ApprovalScope::Session => {
                        self.session_approved_dirs
                            .push(std::path::PathBuf::from(dir));
                    }
                    ApprovalScope::Workspace => {
                        crate::workspace_permissions::add_dir(&self.cwd, dir);
                        self.reload_workspace_permissions();
                    }
                }
                self.screen.set_active_status(call_id, ToolStatus::Pending);
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
                self.screen
                    .finish_tool(call_id, ToolStatus::Denied, None, None);
                if has_message {
                    // Deny with feedback — let the agent continue with the message.
                    // Clear pending so the engine's ToolFinished event doesn't
                    // overwrite the Denied status.
                    if let Some(ref mut ag) = agent {
                        ag.pending.retain(|p| p.call_id != call_id);
                    }
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
                        ag.pending.clear();
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
                self.screen.finish_tool("", ToolStatus::Denied, None, None);
                if let Some(ref mut ag) = agent {
                    ag.pending.clear();
                }
                true
            }
        }
    }

    // ── Control dispatch ─────────────────────────────────────────────────

    pub(super) fn dispatch_control(
        &mut self,
        ctrl: SessionControl,
        pending: &[PendingTool],
        deferred_dialog: &mut Option<DeferredDialog>,
        active_dialog: &mut Option<Box<dyn render::Dialog>>,
        last_keypress: Option<Instant>,
    ) -> LoopAction {
        match ctrl {
            SessionControl::Continue => LoopAction::Continue,
            SessionControl::Done => LoopAction::Done,
            SessionControl::NeedsConfirm(mut req) => {
                if req.tool_name.is_empty() {
                    // Invariant: permission checks run sequentially in the
                    // engine's Phase 1, so the most recently pushed pending
                    // tool is always the one awaiting permission.
                    req.tool_name = pending.last().map(|p| p.name.clone()).unwrap_or_default();
                }

                // Check auto-approvals (doesn't need UI).
                // Split compound commands into sub-commands and check each
                // individually against the stored patterns.
                let session_pats = self.session_approved.get(&req.tool_name);
                let workspace_pats = self.workspace_approved.get(&req.tool_name);
                if session_pats.is_some() || workspace_pats.is_some() {
                    // Empty vec means "allow all" for that tool.
                    let blanket = session_pats.is_some_and(|p| p.is_empty())
                        || workspace_pats.is_some_and(|p| p.is_empty());
                    if blanket || {
                        let subcmds = engine::permissions::split_shell_commands(&req.desc);
                        let all_pats = session_pats.into_iter().chain(workspace_pats).flatten();
                        !subcmds.is_empty()
                            && subcmds
                                .iter()
                                .all(|sc| all_pats.clone().any(|p| p.matches(sc)))
                    } {
                        self.engine.send(UiCommand::PermissionDecision {
                            request_id: req.request_id,
                            approved: true,
                            message: None,
                        });
                        return LoopAction::Continue;
                    }
                }

                // Check directory-based auto-approvals (global across tools).
                let outside_paths = self
                    .permissions
                    .outside_workspace_paths(&req.tool_name, &req.args);
                let all_dirs = self
                    .session_approved_dirs
                    .iter()
                    .chain(self.workspace_approved_dirs.iter());
                if !outside_paths.is_empty()
                    && outside_paths.iter().all(|p| {
                        let dir = std::path::Path::new(p)
                            .parent()
                            .unwrap_or(std::path::Path::new(p));
                        all_dirs.clone().any(|ad| dir.starts_with(ad))
                    })
                {
                    self.engine.send(UiCommand::PermissionDecision {
                        request_id: req.request_id,
                        approved: true,
                        message: None,
                    });
                    return LoopAction::Continue;
                }

                // Determine the outside-dir option for the "always allow" button.
                let downgraded =
                    self.permissions
                        .was_downgraded(self.mode, &req.tool_name, &req.args);
                req.outside_dir = if !outside_paths.is_empty() {
                    let dir = std::path::Path::new(&outside_paths[0])
                        .parent()
                        .unwrap_or(std::path::Path::new(&outside_paths[0]))
                        .to_path_buf();
                    if downgraded || self.seen_outside_dirs.contains(&dir) {
                        self.seen_outside_dirs.insert(dir.clone());
                        Some(dir)
                    } else {
                        self.seen_outside_dirs.insert(dir);
                        None
                    }
                } else {
                    None
                };

                // Strip patterns the user already approved (session or workspace).
                if !req.approval_patterns.is_empty() {
                    let approved: Vec<&glob::Pattern> = self
                        .session_approved
                        .get(&req.tool_name)
                        .into_iter()
                        .chain(self.workspace_approved.get(&req.tool_name))
                        .flatten()
                        .collect();
                    req.approval_patterns
                        .retain(|p| !approved.iter().any(|g| g.as_str() == p));
                }

                // If the user is actively typing, defer the dialog.
                let recently_typed = last_keypress
                    .is_some_and(|t| t.elapsed() < Duration::from_millis(CONFIRM_DEFER_MS));
                if recently_typed && !self.input.buf.is_empty() {
                    self.screen
                        .set_active_status(&req.call_id, ToolStatus::Confirm);
                    self.screen.set_pending_dialog(true);
                    *deferred_dialog = Some(DeferredDialog::Confirm(req));
                    return LoopAction::Continue;
                }

                // Close any non-blocking dialog (e.g. Ps) to make room.
                if let Some(prev) = active_dialog.take() {
                    self.screen.clear_dialog_area(prev.anchor_row());
                }
                self.confirm_context = Some(ConfirmContext {
                    call_id: req.call_id.clone(),
                    tool_name: req.tool_name.clone(),
                    args: req.args.clone(),
                    request_id: req.request_id,
                });
                self.screen
                    .set_active_status(&req.call_id, ToolStatus::Confirm);
                let dialog = Box::new(ConfirmDialog::new(&req, self.input.vim_enabled()));
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
                // ask_user_question doesn't have a call_id in the permission flow,
                // use empty string (it targets the last active tool via fallback).
                self.screen.set_active_status("", ToolStatus::Confirm);
                let questions = render::parse_questions(&args);
                let dialog = Box::new(QuestionDialog::new(questions, request_id));
                self.open_blocking_dialog(dialog, active_dialog);
                LoopAction::Continue
            }
        }
    }
}
