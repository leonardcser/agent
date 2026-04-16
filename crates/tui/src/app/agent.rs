use super::*;

impl App {
    /// Send a permission decision — either to a child agent (via socket reply)
    /// or to the local engine. This is the single routing point for all
    /// permission verdicts.
    pub(super) fn send_permission_decision(
        &mut self,
        request_id: u64,
        approved: bool,
        message: Option<String>,
    ) {
        if let Some(reply_tx) = self.child_permission_replies.remove(&request_id) {
            let _ = reply_tx.send(engine::socket::PermissionReply { approved, message });
        } else {
            self.engine.send(UiCommand::PermissionDecision {
                request_id,
                approved,
                message,
            });
        }
    }

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
        if !content.is_empty() {
            self.history.push(Message::user(content.clone()));
            self.sync_session_snapshot();
            self.history.pop();
        }
        self.maybe_generate_title(Some(&text));
        self.dispatch_turn(content)
    }

    /// Start a turn triggered by agent messages already in history.
    /// No user message block is shown — the agent messages are already
    /// rendered as AgentMessage blocks.
    pub(super) fn begin_agent_message_turn(&mut self) -> TurnState {
        self.input_prediction = None;
        self.screen.begin_turn();
        self.sync_session_snapshot();
        self.dispatch_turn(Content::text(""))
    }

    /// Mark the engine busy, allocate a turn id, and send `StartTurn` with the
    /// current app state. Callers own any history/session prep before this.
    fn dispatch_turn(&mut self, content: Content) -> TurnState {
        let Some(api_key) = self.resolve_api_key() else {
            self.screen.set_throbber(render::Throbber::Done);
            return TurnState {
                turn_id: 0,
                pending: Vec::new(),
                _perf: crate::perf::begin("agent:turn"),
            };
        };

        self.screen.set_throbber(render::Throbber::Working);
        engine::registry::update_status(std::process::id(), engine::registry::AgentStatus::Working);

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
            api_key: Some(api_key),
            session_id: self.session.id.clone(),
            session_dir: crate::session::dir_for(&self.session),
            model_config_overrides: None,
            permission_overrides: None,
        });

        TurnState {
            turn_id,
            pending: Vec::new(),
            _perf: crate::perf::begin("agent:turn"),
        }
    }

    pub(super) fn begin_custom_command_turn(
        &mut self,
        cmd: crate::custom_commands::CustomCommand,
    ) -> TurnState {
        // Ingress: custom command bodies may inline file contents via
        // {file:...} substitutions, so scrub before the content lands in
        // history or is dispatched to the engine.
        let evaluated = crate::custom_commands::evaluate(&cmd.body);
        let evaluated = if self.settings.redact_secrets {
            engine::redact::redact(&evaluated)
        } else {
            evaluated
        };
        let display = format!("/{}", cmd.name);

        if !evaluated.is_empty() {
            self.history
                .push(Message::user(Content::text(evaluated.clone())));
            self.sync_session_snapshot();
            self.history.pop();
        }

        // Resolve model/provider overrides
        let (model, api_base, api_key) = {
            let target_model = cmd.overrides.model.as_deref();
            let target_provider = cmd.overrides.provider.as_deref();
            let resolved = match (target_model, target_provider) {
                (Some(reference), provider) => {
                    match crate::config::resolve_model_ref_with_provider(
                        &self.available_models,
                        reference,
                        provider,
                    ) {
                        Ok(model) => Some(model),
                        Err(err) => {
                            self.screen.notify_error(err.to_string());
                            None
                        }
                    }
                }
                (None, Some(provider)) => {
                    match crate::config::resolve_provider_ref(&self.available_models, provider) {
                        Ok(model) => Some(model),
                        Err(err) => {
                            self.screen.notify_error(err.to_string());
                            None
                        }
                    }
                }
                (None, None) => None,
            }
            .map(|resolved| {
                (
                    resolved.model_name.clone(),
                    resolved.api_base.clone(),
                    resolved.api_key_env.clone(),
                )
            });
            match resolved {
                Some((model_name, api_base, api_key_env)) => (
                    model_name,
                    api_base,
                    self.resolve_api_key_for_env(&api_key_env)
                        .unwrap_or_default(),
                ),
                None => (
                    self.model.clone(),
                    self.api_base.clone(),
                    self.resolve_api_key().unwrap_or_default(),
                ),
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
            _perf: crate::perf::begin("agent:turn"),
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
        if cancelled {
            self.engine.send(UiCommand::Cancel);
            self.kill_blocking_agents();
        }
        // Flush any in-flight streaming content before committing tools.
        self.screen.flush_streaming_thinking();
        self.screen.flush_streaming_text();
        // Commit active tools to block history but don't render yet —
        // the next draw_frame renders blocks + prompt atomically in one
        // synchronized update, avoiding a flash where the prompt disappears.
        self.screen.commit_active_tools();
        if cancelled {
            self.screen.set_throbber(render::Throbber::Interrupted);
            // If a title/slug generation was in-flight, discard it so stale
            // TitleGenerated events don't update the session. But if a slug
            // was already set before this turn, keep it.
            if self.pending_title {
                self.pending_title = false;
                // Only clear the slug if it wasn't already set before this
                // turn's title generation request. If a slug existed before,
                // keep it — we're just discarding the in-flight update.
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
            // Skip predictions on subagent tabs — they're independent.
            self.input_prediction = None;
            if self.settings.show_prediction {
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
                        generation: self.predict_generation,
                    });
                }
            }
        }
        let meta = self
            .pending_turn_meta
            .take()
            .or_else(|| self.screen.turn_meta());
        if let Some(mut meta) = meta {
            for (agent_id, data) in self.pending_agent_blocks.drain(..) {
                meta.agent_blocks.insert(agent_id, data);
            }
            self.turn_metas.push((self.history.len(), meta));
        }
        self.snapshot_tokens();
        self.save_session();
        state::set_mode(self.mode);
        self.maybe_auto_compact();
        engine::registry::update_status(std::process::id(), engine::registry::AgentStatus::Idle);
    }

    // ── Engine events ────────────────────────────────────────────────────

    pub fn handle_engine_event(
        &mut self,
        ev: EngineEvent,
        turn_id: u64,
        pending: &mut Vec<PendingTool>,
    ) -> SessionControl {
        match ev {
            EngineEvent::Ready => SessionControl::Continue,
            EngineEvent::TokenUsage {
                usage,
                tokens_per_sec,
                cost_usd,
                background,
            } => {
                if !background {
                    if let Some(tokens) = usage.prompt_tokens {
                        if tokens > 0 {
                            self.screen.set_context_tokens(tokens);
                            self.session.context_tokens = Some(tokens);
                        }
                    }
                    if let Some(tps) = tokens_per_sec {
                        self.screen.record_tokens_per_sec(tps);
                    }
                    self.screen.set_throbber(render::Throbber::Working);
                }
                let cost = cost_usd.unwrap_or(0.0);
                self.session_cost_usd += cost;
                self.screen.set_session_cost(self.session_cost_usd);
                crate::metrics::append(&crate::metrics::MetricsEntry {
                    timestamp_ms: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64,
                    prompt_tokens: usage.prompt_tokens.unwrap_or(0),
                    completion_tokens: usage.completion_tokens.unwrap_or(0),
                    model: self.model.clone(),
                    cost_usd,
                    cache_read_tokens: usage.cache_read_tokens,
                    cache_write_tokens: usage.cache_write_tokens,
                    reasoning_tokens: usage.reasoning_tokens,
                });
                SessionControl::Continue
            }
            EngineEvent::ToolOutput { call_id, chunk } => {
                self.screen.append_active_output(&call_id, &chunk);
                SessionControl::Continue
            }
            EngineEvent::Steered { text, count } => {
                self.screen.flush_streaming_thinking();
                self.screen.flush_streaming_text();
                let drain_n = count.min(self.queued_messages.len());
                self.queued_messages.drain(..drain_n);
                if drain_n > 0 {
                    self.screen.push(Block::User {
                        text,
                        image_labels: vec![],
                    });
                }
                SessionControl::Continue
            }
            EngineEvent::ThinkingDelta { delta } => {
                self.screen.append_streaming_thinking(&delta);
                SessionControl::Continue
            }
            EngineEvent::Thinking { content } => {
                self.screen.push(Block::Thinking { content });
                SessionControl::Continue
            }
            EngineEvent::TextDelta { delta } => {
                self.screen.append_streaming_text(&delta);
                SessionControl::Continue
            }
            EngineEvent::Text { content } => {
                self.screen.flush_streaming_text();
                self.screen.push(Block::Text { content });
                SessionControl::Continue
            }
            EngineEvent::ToolStarted {
                call_id,
                tool_name,
                args,
                summary,
            } => {
                // Flush any remaining streaming content before tools render.
                self.screen.flush_streaming_thinking();
                self.screen.flush_streaming_text();
                if tool_name != "spawn_agent" {
                    self.screen.start_tool(
                        call_id.clone(),
                        tool_name.clone(),
                        summary,
                        args.clone(),
                    );
                }
                pending.push(PendingTool {
                    call_id,
                    name: tool_name,
                    args,
                });
                SessionControl::Continue
            }
            EngineEvent::ToolFinished {
                call_id,
                result,
                elapsed_ms,
            } => {
                if let Some(idx) = pending.iter().position(|p| p.call_id == call_id) {
                    let removed = pending.remove(idx);
                    if removed.name == "spawn_agent" {
                        // Extract agent_id from result and kill if blocking.
                        let agent_id = result
                            .content
                            .strip_prefix("agent ")
                            .and_then(|s| s.split_whitespace().next())
                            .unwrap_or("")
                            .to_string();
                        self.screen.finish_active_agent(&agent_id);
                        if let Some(idx) = self
                            .agents
                            .iter()
                            .position(|a| a.agent_id == agent_id && a.blocking)
                        {
                            let agent = &self.agents[idx];
                            self.pending_agent_blocks.push((
                                agent.agent_id.clone(),
                                protocol::AgentBlockData {
                                    slug: agent.slug.clone(),
                                    tool_calls: agent
                                        .tool_calls
                                        .iter()
                                        .map(|tc| protocol::AgentToolData {
                                            tool_name: tc.tool_name.clone(),
                                            summary: tc.summary.clone(),
                                            elapsed_ms: tc.elapsed.map(|d| d.as_millis() as u64),
                                            is_error: matches!(tc.status, ToolStatus::Err),
                                        })
                                        .collect(),
                                },
                            ));
                            let pid = agent.pid;
                            engine::registry::kill_agent(pid);
                            self.agents.remove(idx);
                            self.refresh_agent_counts();
                            self.sync_agent_snapshots();
                        }
                    } else {
                        let status = if result.is_error {
                            ToolStatus::Err
                        } else {
                            ToolStatus::Ok
                        };
                        let render_cache = render::build_tool_output_render_cache(
                            &removed.name,
                            &removed.args,
                            &result.content,
                            result.is_error,
                            result.metadata.as_ref(),
                        );
                        let output = Some(Box::new(ToolOutput {
                            content: result.content,
                            is_error: result.is_error,
                            metadata: result.metadata,
                            render_cache,
                        }));
                        let elapsed = elapsed_ms.map(Duration::from_millis);
                        self.screen.finish_tool(&call_id, status, output, elapsed);
                    }
                }
                self.screen
                    .set_running_procs(self.engine.processes.running_count());
                self.refresh_agent_counts();
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
                self.screen.set_throbber(render::Throbber::Done);
                self.screen.notify_error(message);
                SessionControl::Done
            }
            EngineEvent::Shutdown { .. } => SessionControl::Done,
            EngineEvent::AgentExited {
                agent_id,
                exit_code,
            } => {
                self.handle_agent_exited(&agent_id, exit_code);
                SessionControl::Continue
            }
            EngineEvent::AgentMessage {
                from_id,
                from_slug,
                message,
            } => {
                // Suppress AgentMessage rendering for blocking agents — their
                // result flows through the spawn_agent tool output instead.
                let is_blocking = self
                    .agents
                    .iter()
                    .any(|a| a.agent_id == from_id && a.blocking);
                if !is_blocking {
                    self.screen.push(Block::AgentMessage {
                        from_id: from_id.clone(),
                        from_slug: from_slug.clone(),
                        content: message.clone(),
                    });
                }
                // Forward to engine so it enters the conversation history
                // (deferred until current tool batch completes).
                self.engine.send(protocol::UiCommand::AgentMessage {
                    from_id,
                    from_slug,
                    message,
                });
                SessionControl::Continue
            }
        }
    }

    /// Handle engine events that arrive when no agent turn is active.
    pub(super) fn handle_engine_event_idle(&mut self, ev: EngineEvent) {
        match ev {
            // Ignore stale Messages snapshots from cancelled/completed turns.
            // These would overwrite a freshly cleared history (e.g. after /clear).
            EngineEvent::Messages { .. } => {}
            EngineEvent::TurnComplete { messages, .. }
                // Accept final messages from a just-cancelled turn so that
                // partial assistant content and tool results are persisted.
                // Don't rebuild the screen — the displayed blocks already
                // reflect what the user saw at cancel time.
                if !messages.is_empty() =>
            {
                self.set_history(messages);
                self.save_session();
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
            EngineEvent::InputPrediction { text, generation }
                if generation == self.predict_generation =>
            {
                self.handle_input_prediction(text);
            }
            EngineEvent::ProcessCompleted { id, exit_code } => {
                self.handle_process_completed(id, exit_code);
            }
            EngineEvent::TurnError { message } => {
                self.screen.set_throbber(render::Throbber::Done);
                self.screen.notify_error(message);
            }
            EngineEvent::AgentExited {
                agent_id,
                exit_code,
            } => {
                self.handle_agent_exited(&agent_id, exit_code);
            }
            EngineEvent::AgentMessage {
                from_id,
                from_slug,
                message,
            } => {
                let is_blocking = self
                    .agents
                    .iter()
                    .any(|a| a.agent_id == from_id && a.blocking);
                if !is_blocking {
                    self.screen.push(Block::AgentMessage {
                        from_id: from_id.clone(),
                        from_slug: from_slug.clone(),
                        content: message.clone(),
                    });
                    // Queue as an Agent role message to trigger a turn without
                    // rendering a duplicate User block.
                    self.pending_agent_messages
                        .push(protocol::Message::agent(&from_id, &from_slug, &message));
                }
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
        self.screen.set_task_label(slug.clone());
        self.pending_title = false;
        self.save_session();

        // Update registry with new task slug.
        engine::registry::update_slug(std::process::id(), &slug);
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

    pub(super) fn resolve_api_key(&mut self) -> Option<String> {
        if self.api_key_env.is_empty() {
            return Some(String::new());
        }
        match std::env::var(&self.api_key_env) {
            Ok(key) => Some(key),
            Err(std::env::VarError::NotPresent) => {
                self.screen.notify_error(format!(
                    "environment variable '{}' is not set but is required for API authentication",
                    self.api_key_env
                ));
                None
            }
            Err(std::env::VarError::NotUnicode(_)) => {
                self.screen.notify_error(format!(
                    "environment variable '{}' contains non-Unicode data and cannot be used as an API key",
                    self.api_key_env
                ));
                None
            }
        }
    }

    pub(super) fn resolve_api_key_for_env(&mut self, key_env: &str) -> Option<String> {
        if key_env.is_empty() {
            return Some(String::new());
        }
        match std::env::var(key_env) {
            Ok(key) => Some(key),
            Err(std::env::VarError::NotPresent) => {
                self.screen.notify_error(format!(
                    "environment variable '{}' is not set but is required for API authentication",
                    key_env
                ));
                None
            }
            Err(std::env::VarError::NotUnicode(_)) => {
                self.screen.notify_error(format!(
                    "environment variable '{}' contains non-Unicode data and cannot be used as an API key",
                    key_env
                ));
                None
            }
        }
    }

    fn handle_agent_exited(&mut self, agent_id: &str, exit_code: Option<i32>) {
        if let Some(c) = exit_code {
            if c != 0 {
                self.screen.push(Block::Hint {
                    content: format!("{agent_id} exited with code {c}."),
                });
            }
        }
        self.agents.retain(|a| a.agent_id != agent_id);
        self.refresh_agent_counts();
        self.sync_agent_snapshots();
    }

    /// Kill all blocking (wait=true) agents and commit their blocks.
    fn kill_blocking_agents(&mut self) {
        let blocking_pids: Vec<u32> = self
            .agents
            .iter()
            .filter(|a| a.blocking && a.status == super::AgentTrackStatus::Working)
            .map(|a| a.pid)
            .collect();
        for pid in blocking_pids {
            engine::registry::kill_agent(pid);
        }
        for agent in &mut self.agents {
            if agent.blocking && agent.status == super::AgentTrackStatus::Working {
                agent.status = super::AgentTrackStatus::Error;
            }
        }
        self.screen.cancel_active_agents();
    }

    pub(super) fn refresh_agent_counts(&mut self) {
        self.screen.set_agent_count(self.agents.len());
    }

    // ── Agent tracking ────────────────────────────────────────────────

    /// Drain newly spawned child handles and create TrackedAgent entries.
    pub(super) fn drain_spawned_children(&mut self) {
        let children = self.engine.drain_spawned();
        for child in children {
            let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();

            // Spawn a reader task that deserializes JSON events from stdout.
            let stdout = child.stdout;
            tokio::spawn(async move {
                use tokio::io::{AsyncBufReadExt, BufReader};
                let async_stdout =
                    tokio::process::ChildStdout::from_std(stdout).expect("async stdout");
                let reader = BufReader::new(async_stdout);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Ok(ev) = serde_json::from_str::<protocol::EngineEvent>(&line) {
                        if event_tx.send(ev).is_err() {
                            break;
                        }
                    }
                }
            });

            if child.blocking {
                // Blocking agents render as a live overlay (like active tools).
                self.screen.start_active_agent(child.agent_id.clone());
            } else {
                // Non-blocking agents get a one-line static block.
                self.screen.push(Block::Agent {
                    agent_id: child.agent_id.clone(),
                    slug: None,
                    blocking: false,
                    tool_calls: Vec::new(),
                    status: render::AgentBlockStatus::Running,
                    elapsed: None,
                });
            }

            self.agents.push(super::TrackedAgent {
                agent_id: child.agent_id,
                pid: child.pid,
                prompt: std::sync::Arc::new(child.prompt),
                slug: None,
                event_rx,
                tool_calls: Vec::new(),
                status: super::AgentTrackStatus::Working,
                blocking: child.blocking,
                started_at: Instant::now(),
                context_tokens: None,
                cost_usd: 0.0,
            });
        }
        self.refresh_agent_counts();
    }

    /// Drain stdout events for all tracked agents.
    pub(super) fn drain_agent_events(&mut self) {
        let mut changed = false;
        let mut session_cost_delta = 0.0;

        for agent in &mut self.agents {
            while let Ok(ev) = agent.event_rx.try_recv() {
                changed = true;
                match ev {
                    EngineEvent::ToolStarted {
                        call_id,
                        tool_name,
                        summary,
                        ..
                    } => {
                        agent.status = super::AgentTrackStatus::Working;
                        agent.tool_calls.push(super::AgentToolEntry {
                            call_id,
                            tool_name,
                            summary,
                            status: ToolStatus::Pending,
                            elapsed: None,
                        });
                    }
                    EngineEvent::ToolFinished {
                        call_id,
                        result,
                        elapsed_ms,
                    } => {
                        if let Some(entry) =
                            agent.tool_calls.iter_mut().find(|t| t.call_id == call_id)
                        {
                            entry.status = if result.is_error {
                                ToolStatus::Err
                            } else {
                                ToolStatus::Ok
                            };
                            entry.elapsed = elapsed_ms.map(Duration::from_millis);
                        }
                    }
                    EngineEvent::TitleGenerated { slug, .. } => {
                        agent.slug = Some(slug);
                    }
                    EngineEvent::TurnComplete { .. } => {
                        agent.status = super::AgentTrackStatus::Idle;
                    }
                    EngineEvent::TokenUsage {
                        cost_usd,
                        usage,
                        background,
                        ..
                    } => {
                        let cost = cost_usd.unwrap_or(0.0);
                        agent.cost_usd += cost;
                        session_cost_delta += cost;
                        if !background {
                            if let Some(tokens) = usage.prompt_tokens {
                                if tokens > 0 {
                                    agent.context_tokens = Some(tokens);
                                }
                            }
                        }
                    }
                    EngineEvent::TurnError { .. } => {
                        agent.status = super::AgentTrackStatus::Error;
                    }
                    _ => {}
                }
            }
        }

        if session_cost_delta > 0.0 {
            self.session_cost_usd += session_cost_delta;
            self.screen.set_session_cost(self.session_cost_usd);
        }

        if !changed {
            return;
        }

        // Update active blocking agent overlays on screen.
        for agent in &self.agents {
            if agent.blocking {
                let status = match agent.status {
                    super::AgentTrackStatus::Working => render::AgentBlockStatus::Running,
                    super::AgentTrackStatus::Idle => render::AgentBlockStatus::Done,
                    super::AgentTrackStatus::Error => render::AgentBlockStatus::Error,
                };
                self.screen.update_active_agent(
                    &agent.agent_id,
                    agent.slug.as_deref(),
                    &agent.tool_calls,
                    status,
                );
            }
        }

        self.refresh_agent_counts();
        self.sync_agent_snapshots();
    }

    /// Update the shared snapshots so the /agents dialog sees live data.
    fn sync_agent_snapshots(&self) {
        let snaps: Vec<render::AgentSnapshot> = self
            .agents
            .iter()
            .map(|a| render::AgentSnapshot {
                agent_id: a.agent_id.clone(),
                prompt: a.prompt.clone(),
                tool_calls: a.tool_calls.clone(),
                context_tokens: a.context_tokens,
                cost_usd: a.cost_usd,
            })
            .collect();
        *self.agent_snapshots.lock().unwrap() = snaps;
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
        agent: &mut Option<TurnState>,
    ) {
        // Resume the spinner if a blocking dialog paused it.
        self.screen.resume_spinner();
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
                self.screen.clear_dialog_area();
                if should_cancel && agent.is_some() {
                    self.finish_turn(true);
                    *agent = None;
                }
            }
            render::DialogResult::Question { answer, request_id } => {
                let should_cancel = self.resolve_question(answer, request_id, agent);
                self.screen.clear_dialog_area();
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
                } else {
                    // Dialog was cancelled — clean up the dialog overlay.
                    if restore_vim_insert {
                        self.input.set_vim_mode(vim::ViMode::Insert);
                    }
                    self.screen.clear_dialog_area();
                }
            }
            render::DialogResult::Resume { session_id } => {
                let mut clear = true;
                if let Some(id) = session_id {
                    if let Some(loaded) = session::load(&id) {
                        self.load_session(loaded);
                        self.restore_screen();
                        if let Some(tokens) = self.session.context_tokens {
                            self.screen.set_context_tokens(tokens);
                        }
                        self.screen.flush_blocks();
                        clear = false;
                    }
                }
                if clear {
                    self.screen.clear_dialog_area();
                }
            }
            render::DialogResult::PermissionsClosed {
                session_remaining,
                workspace_remaining,
            } => {
                self.sync_permissions(session_remaining, workspace_remaining);
                self.screen.clear_dialog_area();
            }
            render::DialogResult::AgentsClosed => {
                self.screen.clear_dialog_area();
                self.refresh_agent_counts();
            }
            render::DialogResult::PsClosed | render::DialogResult::Dismissed => {
                self.screen.clear_dialog_area();
            }
        }
    }

    pub(super) fn session_permission_entries(&self) -> Vec<render::PermissionEntry> {
        let rt = self.runtime_approvals.read().unwrap();
        let mut entries = Vec::new();
        for (tool, patterns) in rt.session_tool_entries() {
            if patterns.is_empty() {
                entries.push(render::PermissionEntry {
                    tool,
                    pattern: "*".into(),
                });
            } else {
                for p in patterns {
                    entries.push(render::PermissionEntry {
                        tool: tool.clone(),
                        pattern: p,
                    });
                }
            }
        }
        for dir in rt.session_dirs() {
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
        let mut session_tools: HashMap<String, Vec<glob::Pattern>> = HashMap::new();
        let mut session_dirs: Vec<PathBuf> = Vec::new();
        for entry in session_entries {
            if entry.tool == "directory" {
                session_dirs.push(std::path::PathBuf::from(&entry.pattern));
            } else if entry.pattern == "*" {
                session_tools.entry(entry.tool).or_default();
            } else if let Ok(pat) = glob::Pattern::new(&entry.pattern) {
                session_tools.entry(entry.tool).or_default().push(pat);
            }
        }

        // Persist and reload workspace rules.
        crate::workspace_permissions::save(&self.cwd, &workspace_rules);
        let (ws_tools, ws_dirs) = crate::workspace_permissions::into_approvals(&workspace_rules);
        let mut rt = self.runtime_approvals.write().unwrap();
        rt.set_session(session_tools, session_dirs);
        rt.load_workspace(ws_tools, ws_dirs);
    }

    fn reload_workspace_permissions(&mut self) {
        let rules = crate::workspace_permissions::load(&self.cwd);
        let (ws_tools, ws_dirs) = crate::workspace_permissions::into_approvals(&rules);
        self.runtime_approvals
            .write()
            .unwrap()
            .load_workspace(ws_tools, ws_dirs);
    }

    pub(super) fn reset_session_permissions(&mut self) {
        self.runtime_approvals.write().unwrap().clear_session();
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
                self.send_permission_decision(request_id, true, message);
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
                        self.runtime_approvals
                            .write()
                            .unwrap()
                            .add_session_tool(tool_name, vec![]);
                    }
                    ApprovalScope::Workspace => {
                        crate::workspace_permissions::add_tool(&self.cwd, tool_name, vec![]);
                        self.reload_workspace_permissions();
                    }
                }
                self.screen.set_active_status(call_id, ToolStatus::Pending);
                self.send_permission_decision(request_id, true, message);
                false
            }
            ConfirmChoice::AlwaysPatterns(ref patterns, scope) => {
                let compiled: Vec<glob::Pattern> = patterns
                    .iter()
                    .filter_map(|p| glob::Pattern::new(p).ok())
                    .collect();
                match scope {
                    ApprovalScope::Session => {
                        self.runtime_approvals
                            .write()
                            .unwrap()
                            .add_session_tool(tool_name, compiled);
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
                self.send_permission_decision(request_id, true, message);
                false
            }
            ConfirmChoice::AlwaysDir(ref dir, scope) => {
                match scope {
                    ApprovalScope::Session => {
                        self.runtime_approvals
                            .write()
                            .unwrap()
                            .add_session_dir(std::path::PathBuf::from(dir));
                    }
                    ApprovalScope::Workspace => {
                        crate::workspace_permissions::add_dir(&self.cwd, dir);
                        self.reload_workspace_permissions();
                    }
                }
                self.screen.set_active_status(call_id, ToolStatus::Pending);
                self.send_permission_decision(request_id, true, message);
                false
            }
            ConfirmChoice::No => {
                let has_message = message.is_some();
                self.send_permission_decision(request_id, false, message);
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
        pending_dialogs: &mut VecDeque<DeferredDialog>,
        active_dialog: &mut Option<Box<dyn render::Dialog>>,
        last_keypress: Option<Instant>,
    ) -> LoopAction {
        // Queue dialogs when a blocking dialog is active or the user is typing.
        // The queue is drained in the main loop via re-dispatch, so auto-approval
        // checks re-run (handles "always allow" → recheck).
        let should_queue = active_dialog.as_ref().is_some_and(|d| d.blocks_agent())
            || (last_keypress
                .is_some_and(|t| t.elapsed() < Duration::from_millis(CONFIRM_DEFER_MS))
                && !self.input.buf.is_empty());

        match ctrl {
            SessionControl::Continue => LoopAction::Continue,
            SessionControl::Done => LoopAction::Done,
            SessionControl::NeedsConfirm(mut req) => {
                if req.tool_name.is_empty() {
                    req.tool_name = pending.last().map(|p| p.name.clone()).unwrap_or_default();
                }

                // Check runtime auto-approvals. For local engine requests this
                // is normally handled by the engine itself, but child agent
                // permission requests arrive via socket and bypass the engine's
                // decision flow, so we check here too.
                let auto_approved = {
                    let rt = self.runtime_approvals.read().unwrap();
                    rt.is_auto_approved(
                        &self.permissions,
                        self.mode,
                        &req.tool_name,
                        &req.args,
                        &req.desc,
                    )
                };
                if auto_approved {
                    self.send_permission_decision(req.request_id, true, None);
                    return LoopAction::Continue;
                }

                // Check mode-based permissions (e.g. Apply mode auto-allows writes).
                if self
                    .permissions
                    .decide(self.mode, &req.tool_name, &req.args, false)
                    == Decision::Allow
                {
                    self.send_permission_decision(req.request_id, true, None);
                    return LoopAction::Continue;
                }

                let outside_paths = self
                    .permissions
                    .outside_workspace_paths(&req.tool_name, &req.args);

                // Auto-approval didn't match — queue if we can't show a dialog now.
                if should_queue {
                    self.screen
                        .set_active_status(&req.call_id, ToolStatus::Confirm);
                    self.screen.set_pending_dialog(true);
                    pending_dialogs.push_back(DeferredDialog::Confirm(req));
                    return LoopAction::Continue;
                }

                // Prepare dialog options.
                let downgraded =
                    self.permissions
                        .was_downgraded(self.mode, &req.tool_name, &req.args);
                req.outside_dir = if downgraded && !outside_paths.is_empty() {
                    // Only offer the dir option when the Ask is specifically
                    // from the workspace restriction (downgraded from Allow).
                    // When the Ask is from the command itself (e.g. `for` loop),
                    // a dir approval won't help — show tool patterns instead.
                    let raw = std::path::Path::new(&outside_paths[0]);
                    let expanded = engine::paths::expand_tilde(raw);
                    let abs_dir = if expanded.is_dir() {
                        expanded
                    } else {
                        expanded.parent().unwrap_or(&expanded).to_path_buf()
                    };
                    Some(engine::paths::collapse_tilde(&abs_dir))
                } else {
                    None
                };

                if !req.approval_patterns.is_empty() {
                    let rt = self.runtime_approvals.read().unwrap();
                    req.approval_patterns
                        .retain(|p| !rt.has_pattern(&req.tool_name, p));
                }

                // Close any non-blocking dialog (e.g. Ps) to make room.
                if active_dialog.take().is_some() {
                    self.screen.clear_dialog_area();
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
                if should_queue {
                    self.screen.set_pending_dialog(true);
                    pending_dialogs.push_back(DeferredDialog::AskQuestion { args, request_id });
                    return LoopAction::Continue;
                }

                // Close any non-blocking dialog (e.g. Ps) to make room.
                if active_dialog.take().is_some() {
                    self.screen.clear_dialog_area();
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
