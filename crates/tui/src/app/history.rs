use super::*;

use crossterm::{event, event::Event, event::KeyEvent, terminal};
use std::collections::HashMap;

impl App {
    fn reset_subagents_for_new_session(&mut self) {
        let my_pid = std::process::id();
        engine::registry::kill_descendants(my_pid);
        self.agents.clear();
        self.refresh_agent_counts();
    }

    pub(super) fn set_history(&mut self, messages: Vec<Message>) {
        self.history = messages;
        self.sync_session_snapshot();
    }

    pub(super) fn sync_session_snapshot(&mut self) {
        self.session.messages = self.history.clone();
        self.session.updated_at_ms = session::now_ms();
        self.session.mode = Some(self.mode.as_str().to_string());
        self.session.reasoning_effort = Some(self.reasoning_effort);
        self.session.model = Some(self.model.clone());
        if let Ok(mut guard) = self.shared_session.lock() {
            *guard = Some(self.session.clone());
        }
    }

    /// Record current token count and cost so they can be restored on rewind.
    pub(super) fn snapshot_tokens(&mut self) {
        if let Some(tokens) = self.screen.context_tokens() {
            self.token_snapshots.push((self.history.len(), tokens));
        }
        self.cost_snapshots
            .push((self.history.len(), self.session_cost_usd));
    }

    pub(super) fn fork_session(&mut self) {
        if self.history.is_empty() {
            self.screen.notify_error("nothing to fork".into());
            return;
        }
        // Save current session first.
        self.save_session();
        let original_id = self.session.id.clone();
        // Create the fork and switch to it.
        let forked = self.session.fork();
        self.session = forked;
        self.save_session();
        self.screen.notify(format!("forked from {original_id}"));
    }

    pub fn reset_session(&mut self) {
        // Cancel any in-flight engine work (agent turn, title generation, etc.)
        // before clearing state so stale events don't restore old data.
        self.engine.send(UiCommand::Cancel);
        self.history.clear();
        self.token_snapshots.clear();
        self.cost_snapshots.clear();
        self.turn_metas.clear();
        self.pending_agent_blocks.clear();
        self.reset_session_permissions();
        self.queued_messages.clear();
        self.screen.clear();
        self.input.clear();
        self.input.store.clear();
        self.engine.processes.clear();
        self.reset_subagents_for_new_session();
        self.session = session::Session::new();
        self.session_cost_usd = 0.0;
        self.screen.set_session_cost(0.0);
        self.pending_title = false;
        self.compact_epoch += 1;
        if let Ok(mut guard) = self.shared_session.lock() {
            *guard = None;
        }
        // Drain stale engine events so old Messages snapshots don't
        // restore history into the freshly cleared session.
        while self.engine.try_recv().is_ok() {}
    }

    pub fn load_session(&mut self, loaded: session::Session) {
        // Resume starts a fresh session view: stop/clear existing subagents tabs.
        self.reset_subagents_for_new_session();

        // Restore per-session settings
        if let Some(ref mode_str) = loaded.mode {
            if let Some(mode) = Mode::parse(mode_str) {
                self.mode = mode;
            }
        }
        if let Some(effort) = loaded.reasoning_effort {
            self.reasoning_effort = effort;
            self.screen.set_reasoning_effort(effort);
        }
        // Only restore model/API settings if not overridden by CLI.
        if !self.cli_model_override && !self.cli_api_base_override && !self.cli_api_key_env_override
        {
            if let Some(ref model_key) = loaded.model {
                if let Some(resolved) = self
                    .available_models
                    .iter()
                    .find(|m| m.key == *model_key || m.model_name == *model_key)
                {
                    self.model = resolved.model_name.clone();
                    self.api_base = resolved.api_base.clone();
                    self.api_key_env = resolved.api_key_env.clone();
                    self.screen.set_model_label(resolved.model_name.clone());
                }
            }
        }

        self.session = loaded;
        if let Some(ref slug) = self.session.slug {
            self.screen.set_task_label(slug.clone());
        }
        self.history = self.session.messages.clone();
        self.token_snapshots = self.session.token_snapshots.clone();
        self.token_snapshots
            .retain(|(len, _)| *len <= self.history.len());
        self.cost_snapshots = self.session.cost_snapshots.clone();
        self.cost_snapshots
            .retain(|(len, _)| *len <= self.history.len());
        if let Some(&(_, cost)) = self.cost_snapshots.last() {
            self.session_cost_usd = cost;
            self.screen.set_session_cost(cost);
        }
        self.turn_metas = self.session.turn_metas.clone();
        self.reset_session_permissions();
        self.queued_messages.clear();
        self.input.clear();
        self.input.store.clear();
        self.pending_title = false;
        self.engine.processes.clear();
        self.compact_epoch += 1;
        self.sync_session_snapshot();
        // Drain stale engine events so old snapshots don't overwrite
        // the loaded session's state.
        while self.engine.try_recv().is_ok() {}
    }

    pub fn resume_session_before_run(&mut self) {
        let entries = self.resume_entries();
        if entries.is_empty() {
            eprintln!("no saved sessions");
            return;
        }

        let mut dialog = render::ResumeDialog::new(
            entries,
            self.cwd.clone(),
            Some(terminal::size().map(|(_, h)| h / 2).unwrap_or(12)),
            self.input.vim_enabled(),
        );
        terminal::enable_raw_mode().ok();
        loop {
            dialog.draw(0, false, &render::StdioBackend);
            match event::read() {
                Ok(Event::Key(KeyEvent {
                    code, modifiers, ..
                })) => {
                    if let Some(result) = dialog.handle_key(code, modifiers) {
                        terminal::disable_raw_mode().ok();
                        let _ = std::io::stdout().execute(crossterm::cursor::Show);
                        if let render::DialogResult::Resume {
                            session_id: Some(id),
                        } = result
                        {
                            if let Some(loaded) = session::load(&id) {
                                self.load_session(loaded);
                            }
                        }
                        return;
                    }
                }
                Ok(Event::Resize(..)) => {
                    dialog.handle_resize();
                }
                _ => {}
            }
        }
    }

    pub(super) fn resume_entries(&self) -> Vec<ResumeEntry> {
        let sessions = session::list_sessions();
        let current_id = &self.session.id;
        let flat: Vec<ResumeEntry> = sessions
            .into_iter()
            .filter(|s| s.id != *current_id)
            .map(|s| ResumeEntry {
                id: s.id,
                title: s.title.unwrap_or_default(),
                subtitle: s.first_user_message,
                updated_at_ms: s.updated_at_ms,
                created_at_ms: s.created_at_ms,
                cwd: s.cwd,
                parent_id: s.parent_id,
                depth: 0,
            })
            .collect();
        super::build_session_tree(flat)
    }

    // ── History / session ────────────────────────────────────────────────

    /// Rebuild the screen from session history and import persisted render cache.
    pub fn restore_screen(&mut self) {
        self.rebuild_screen_from_history();
        if let Some(cache) = session::load_render_cache(&self.session, self.settings.show_thinking)
        {
            self.screen.import_render_cache(cache);
        }
    }

    fn rebuild_screen_from_history(&mut self) {
        self.screen.clear();
        if let Some(ref slug) = self.session.slug {
            self.screen.set_task_label(slug.clone());
        }
        if self.history.is_empty() {
            return;
        }

        let mut tool_outputs: HashMap<String, ToolOutput> = HashMap::new();
        let mut tool_elapsed: HashMap<String, u64> = HashMap::new();
        let mut agent_blocks: HashMap<String, protocol::AgentBlockData> = HashMap::new();
        for msg in &self.history {
            if matches!(msg.role, Role::Tool) {
                if let Some(ref id) = msg.tool_call_id {
                    let text = msg
                        .content
                        .as_ref()
                        .map(|c| c.text_content())
                        .unwrap_or_default();
                    tool_outputs.insert(
                        id.clone(),
                        ToolOutput {
                            content: text,
                            is_error: msg.is_error,
                            metadata: None,
                        },
                    );
                }
            }
        }
        for (_, meta) in &self.turn_metas {
            tool_elapsed.extend(meta.tool_elapsed.iter().map(|(k, v)| (k.clone(), *v)));
            agent_blocks.extend(
                meta.agent_blocks
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone())),
            );
        }
        // Track blocking agent IDs so we can suppress their AgentMessage blocks.
        let mut blocking_agent_ids: std::collections::HashSet<String> =
            std::collections::HashSet::new();

        for msg in &self.history {
            match msg.role {
                Role::User => {
                    if let Some(ref content) = msg.content {
                        let text = content.text_content();
                        if let Some(summary) =
                            text.strip_prefix("Summary of prior conversation:\n\n")
                        {
                            self.screen.push(Block::Compacted {
                                summary: summary.to_string(),
                            });
                        } else {
                            let image_labels = content.image_labels();
                            let display_text = if image_labels.is_empty() {
                                text
                            } else {
                                let suffix = image_labels.join(" ");
                                if text.is_empty() {
                                    suffix
                                } else {
                                    format!("{text} {suffix}")
                                }
                            };
                            self.screen.push(Block::User {
                                text: display_text,
                                image_labels,
                            });
                        }
                    }
                }
                Role::Assistant => {
                    if let Some(ref reasoning) = msg.reasoning_content {
                        if !reasoning.is_empty() {
                            self.screen.push(Block::Thinking {
                                content: reasoning.clone(),
                            });
                        }
                    }
                    if let Some(ref content) = msg.content {
                        self.screen.push(Block::Text {
                            content: content.text_content(),
                        });
                    }
                    if let Some(ref calls) = msg.tool_calls {
                        for tc in calls {
                            let args: HashMap<String, serde_json::Value> =
                                serde_json::from_str(&tc.function.arguments).unwrap_or_default();
                            let output = tool_outputs.get(&tc.id).cloned();

                            if tc.function.name == "spawn_agent" {
                                let meta = output.as_ref().and_then(|o| o.metadata.as_ref());
                                let result_text =
                                    output.as_ref().map(|o| o.content.as_str()).unwrap_or("");
                                let agent_id = meta
                                    .and_then(|m| m["agent_id"].as_str())
                                    .or_else(|| {
                                        result_text
                                            .strip_prefix("agent ")
                                            .and_then(|s| s.split_whitespace().next())
                                    })
                                    .unwrap_or("?")
                                    .to_string();
                                let is_blocking = meta
                                    .and_then(|m| m["blocking"].as_bool())
                                    .unwrap_or_else(|| result_text.contains("finished:"));
                                let is_error = output.as_ref().is_some_and(|o| o.is_error);
                                let block_status = if is_error {
                                    render::AgentBlockStatus::Error
                                } else {
                                    render::AgentBlockStatus::Done
                                };
                                let elapsed = tool_elapsed
                                    .get(&tc.id)
                                    .map(|ms| Duration::from_millis(*ms));
                                // Restore slug and tool calls from persisted agent block data.
                                let block_data = agent_blocks.get(&agent_id);
                                let slug = block_data.and_then(|d| d.slug.clone());
                                let tool_calls = block_data
                                    .map(|d| {
                                        d.tool_calls
                                            .iter()
                                            .map(|t| crate::app::AgentToolEntry {
                                                call_id: String::new(),
                                                tool_name: t.tool_name.clone(),
                                                summary: t.summary.clone(),
                                                elapsed: t.elapsed_ms.map(Duration::from_millis),
                                                status: if t.is_error {
                                                    ToolStatus::Err
                                                } else {
                                                    ToolStatus::Ok
                                                },
                                            })
                                            .collect()
                                    })
                                    .unwrap_or_default();
                                if is_blocking {
                                    blocking_agent_ids.insert(agent_id.clone());
                                }
                                self.screen.push(Block::Agent {
                                    agent_id,
                                    slug,
                                    blocking: is_blocking,
                                    tool_calls,
                                    status: block_status,
                                    elapsed,
                                });
                                continue;
                            }

                            let summary = tool_arg_summary(&tc.function.name, &args);
                            let status = if let Some(ref out) = output {
                                if out.content.contains("denied this tool call")
                                    || out.content.contains("blocked this tool call")
                                {
                                    ToolStatus::Denied
                                } else if out.is_error {
                                    ToolStatus::Err
                                } else {
                                    ToolStatus::Ok
                                }
                            } else {
                                ToolStatus::Pending
                            };
                            let elapsed = tool_elapsed
                                .get(&tc.id)
                                .map(|ms| Duration::from_millis(*ms));
                            self.screen.push(Block::ToolCall {
                                name: tc.function.name.clone(),
                                summary,
                                args,
                                status,
                                elapsed,
                                output,
                                user_message: None,
                            });
                        }
                    }
                }
                Role::Tool => {}
                Role::System => {}
                Role::Agent => {
                    let from_id = msg.agent_from_id.clone().unwrap_or_default();
                    // Suppress AgentMessage for blocking agents — their result
                    // is already shown in the spawn_agent block.
                    if !blocking_agent_ids.contains(&from_id) {
                        if let Some(ref content) = msg.content {
                            self.screen.push(Block::AgentMessage {
                                from_id,
                                from_slug: msg.agent_from_slug.clone().unwrap_or_default(),
                                content: content.text_content(),
                            });
                        }
                    }
                }
            }
        }

        if let Some((_, meta)) = self.turn_metas.last() {
            self.screen.restore_from_turn_meta(meta);
        }
    }

    pub fn save_session(&mut self) {
        let _perf = crate::perf::begin("save_session");
        if self.history.is_empty() {
            return;
        }
        self.session.token_snapshots = self.token_snapshots.clone();
        self.session.cost_snapshots = self.cost_snapshots.clone();
        self.session.turn_metas = self.turn_metas.clone();
        self.sync_session_snapshot();
        session::save(&self.session, &self.input.store);
        self.persist_render_cache();
    }

    pub(super) fn maybe_generate_title(&mut self, current_message: Option<&str>) {
        if self.pending_title {
            engine::log::entry(
                engine::log::Level::Debug,
                "title_skip",
                &serde_json::json!({"reason": "pending"}),
            );
            return;
        }
        let mut user_messages: Vec<String> = self
            .history
            .iter()
            .filter(|m| matches!(m.role, protocol::Role::User))
            .filter_map(|m| m.content.as_ref().map(|c| c.text_content()))
            .filter(|t| !t.is_empty())
            .collect();
        if let Some(msg) = current_message {
            if !msg.is_empty() {
                user_messages.push(msg.to_string());
            }
        }
        if user_messages.is_empty() {
            engine::log::entry(
                engine::log::Level::Debug,
                "title_skip",
                &serde_json::json!({"reason": "no_user_messages"}),
            );
            return;
        }
        // Send last 5 user messages for title generation (recency-weighted).
        let start = user_messages.len().saturating_sub(5);
        let recent: Vec<String> = user_messages
            .drain(start..)
            .map(|s| {
                if s.len() > 500 {
                    s[..s.floor_char_boundary(500)].to_string()
                } else {
                    s
                }
            })
            .collect();
        engine::log::entry(
            engine::log::Level::Info,
            "title_generate",
            &serde_json::json!({"message_count": recent.len(), "current_title": self.session.title}),
        );
        self.pending_title = true;
        self.engine.send(UiCommand::GenerateTitle {
            user_messages: recent,
            model: self.model.clone(),
            api_base: Some(self.api_base.clone()),
            api_key: Some(self.api_key()),
        });
    }

    pub fn compact_history(&mut self, instructions: Option<String>) {
        self.pending_compact_epoch = self.compact_epoch;
        self.screen.set_throbber(render::Throbber::Compacting);
        self.engine.send(UiCommand::Compact {
            keep_turns: 2,
            history: self.history.clone(),
            model: self.model.clone(),
            instructions,
        });
    }

    pub(super) fn apply_compaction(&mut self, messages: Vec<protocol::Message>) {
        // The first message is the summary; the rest are kept turns already
        // in history. Just append the summary as a regular history entry.
        let Some(marker) = messages.into_iter().next() else {
            self.screen.set_throbber(render::Throbber::Done);
            return;
        };

        let summary = marker
            .content
            .as_ref()
            .map(|c| c.as_text())
            .and_then(|t| t.strip_prefix("Summary of prior conversation:\n\n"))
            .unwrap_or("")
            .trim()
            .to_string();

        self.snapshot_tokens();
        self.history.push(marker);

        if !summary.is_empty() {
            self.screen.push(Block::Compacted { summary });
        }

        self.screen.clear_context_tokens();
        self.save_session();
        self.screen.set_throbber(render::Throbber::Done);
    }

    pub(super) fn maybe_auto_compact(&mut self) {
        if !self.settings.auto_compact {
            return;
        }
        let Some(ctx) = self.context_window else {
            return;
        };
        let Some(tokens) = self.screen.context_tokens() else {
            return;
        };
        if tokens as u64 * 100 >= ctx as u64 * engine::COMPACT_THRESHOLD_PERCENT {
            self.compact_history(None);
        }
    }

    pub fn rewind_to(&mut self, block_idx: usize) -> Option<(String, Vec<(String, String)>)> {
        let turns = self.screen.user_turns();
        let turn_text = turns
            .iter()
            .find(|(i, _)| *i == block_idx)
            .map(|(_, t)| t.clone());
        let user_turns_to_keep = turns.iter().filter(|(i, _)| *i < block_idx).count();

        let mut user_count = 0;
        let mut hist_idx = 0;
        for (i, msg) in self.history.iter().enumerate() {
            if matches!(msg.role, Role::User) {
                user_count += 1;
                if user_count > user_turns_to_keep {
                    hist_idx = i;
                    break;
                }
            }
            hist_idx = i + 1;
        }

        // Extract image (label, data_url) pairs from the target message before truncating.
        let images: Vec<(String, String)> = self
            .history
            .get(hist_idx)
            .and_then(|msg| msg.content.as_ref())
            .map(|content| match content {
                Content::Parts(parts) => parts
                    .iter()
                    .filter_map(|p| match p {
                        protocol::ContentPart::ImageUrl { url, label } => {
                            Some((label.clone().unwrap_or_else(|| "image".into()), url.clone()))
                        }
                        _ => None,
                    })
                    .collect(),
                _ => Vec::new(),
            })
            .unwrap_or_default();

        self.history.truncate(hist_idx);
        truncate_keyed(&mut self.token_snapshots, hist_idx);
        truncate_keyed(&mut self.cost_snapshots, hist_idx);
        truncate_keyed(&mut self.turn_metas, hist_idx);
        if let Some(&(_, cost)) = self.cost_snapshots.last() {
            self.session_cost_usd = cost;
            self.screen.set_session_cost(cost);
        } else {
            self.session_cost_usd = 0.0;
            self.screen.set_session_cost(0.0);
        }
        if let Some(&(_, tokens)) = self.token_snapshots.last() {
            self.screen.set_context_tokens(tokens);
        } else {
            self.screen.clear_context_tokens();
        }
        self.screen.truncate_to(block_idx);
        self.reset_session_permissions();
        self.compact_epoch += 1;

        turn_text.map(|t| (t, images))
    }

    // ── Agent internals ──────────────────────────────────────────────────

    pub fn show_user_message(&mut self, input: &str, image_labels: Vec<String>) {
        self.screen.push(Block::User {
            text: input.to_string(),
            image_labels,
        });
    }
}

/// Drop entries whose history-length key exceeds `hist_idx`.
fn truncate_keyed<T>(snapshots: &mut Vec<(usize, T)>, hist_idx: usize) {
    while snapshots.last().is_some_and(|(len, _)| *len > hist_idx) {
        snapshots.pop();
    }
}
