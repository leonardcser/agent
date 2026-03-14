use super::*;

use crossterm::{event, event::Event, event::KeyEvent, terminal};
use std::collections::HashMap;

impl App {
    pub(super) fn fork_session(&mut self) {
        if self.history.is_empty() {
            self.screen.push(Block::Error {
                message: "nothing to fork".into(),
            });
            self.screen.flush_blocks();
            return;
        }
        // Save current session first.
        self.save_session();
        let original_id = self.session.id.clone();
        // Create the fork and switch to it.
        let forked = self.session.fork();
        self.session = forked;
        self.save_session();
        self.screen.push(Block::Hint {
            content: format!("forked from {original_id}"),
        });
        self.screen.flush_blocks();
    }

    pub fn reset_session(&mut self) {
        self.history.clear();
        self.auto_approved.clear();
        self.queued_messages.clear();
        self.screen.clear();
        self.input.clear();
        self.engine.processes.clear();
        self.session = session::Session::new();
        self.pending_title = false;
        self.compact_epoch += 1;
        // Drain stale engine events so old Messages snapshots don't
        // restore history into the freshly cleared session.
        while self.engine.try_recv().is_ok() {}
    }

    pub fn load_session(&mut self, loaded: session::Session) {
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

        self.session = loaded;
        self.history = self.session.messages.clone();
        self.auto_approved.clear();
        self.queued_messages.clear();
        self.input.clear();
        self.pending_title = false;
        self.engine.processes.clear();
        self.compact_epoch += 1;
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

        let cwd = std::env::current_dir()
            .ok()
            .and_then(|p| p.to_str().map(String::from))
            .unwrap_or_default();
        let mut dialog = render::ResumeDialog::new(
            entries,
            cwd,
            Some(terminal::size().map(|(_, h)| h / 2).unwrap_or(12)),
        );
        terminal::enable_raw_mode().ok();
        loop {
            dialog.draw(0, false);
            match event::read() {
                Ok(Event::Key(KeyEvent {
                    code, modifiers, ..
                })) => {
                    if let Some(result) = dialog.handle_key(code, modifiers) {
                        terminal::disable_raw_mode().ok();
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

    pub fn rebuild_screen_from_history(&mut self) {
        self.screen.clear();
        if self.history.is_empty() {
            return;
        }

        let mut tool_outputs: HashMap<String, ToolOutput> = HashMap::new();
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
                            is_error: false,
                        },
                    );
                }
            }
        }

        for msg in &self.history {
            match msg.role {
                Role::User => {
                    if let Some(ref content) = msg.content {
                        self.screen.push(Block::User {
                            text: content.text_content(),
                        });
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
                        if !content.is_empty() {
                            self.screen.push(Block::Text {
                                content: content.text_content(),
                            });
                        }
                    }
                    if let Some(ref calls) = msg.tool_calls {
                        for tc in calls {
                            let args: HashMap<String, serde_json::Value> =
                                serde_json::from_str(&tc.function.arguments).unwrap_or_default();
                            let summary = tool_arg_summary(&tc.function.name, &args);
                            let output = tool_outputs.get(&tc.id).cloned();
                            let status = if output.is_some() {
                                ToolStatus::Ok
                            } else {
                                ToolStatus::Pending
                            };
                            self.screen.push(Block::ToolCall {
                                name: tc.function.name.clone(),
                                summary,
                                args,
                                status,
                                elapsed: None,
                                output,
                                user_message: None,
                            });
                        }
                    }
                }
                Role::Tool => {}
                Role::System => {
                    if let Some(ref content) = msg.content {
                        let text = content.as_text();
                        if let Some(summary) =
                            text.strip_prefix("Summary of prior conversation:\n\n")
                        {
                            self.screen.push(Block::Text {
                                content: summary.to_string(),
                            });
                        }
                    }
                }
            }
        }
    }

    pub fn save_session(&mut self) {
        let _perf = crate::perf::begin("save_session");
        if self.history.is_empty() {
            return;
        }
        self.session.messages = self.history.clone();
        self.session.updated_at_ms = session::now_ms();
        self.session.mode = Some(self.mode.as_str().to_string());
        self.session.reasoning_effort = Some(self.reasoning_effort);
        self.session.model = Some(self.model.clone());
        session::save(&self.session);
        if let Ok(mut guard) = self.shared_session.lock() {
            *guard = Some(self.session.clone());
        }
    }

    pub(super) fn maybe_generate_title(&mut self) {
        if self.pending_title {
            engine::log::entry(
                engine::log::Level::Debug,
                "title_skip",
                &serde_json::json!({"reason": "pending"}),
            );
            return;
        }
        let user_messages: Vec<String> = self
            .history
            .iter()
            .filter(|m| matches!(m.role, protocol::Role::User))
            .filter_map(|m| m.content.as_ref().map(|c| c.text_content()))
            .filter(|t| !t.is_empty())
            .collect();
        if user_messages.is_empty() {
            engine::log::entry(
                engine::log::Level::Debug,
                "title_skip",
                &serde_json::json!({"reason": "no_user_messages"}),
            );
            return;
        }
        // Send last 5 user messages for title generation (recency-weighted).
        let recent: Vec<String> = user_messages
            .into_iter()
            .rev()
            .take(5)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|s| if s.len() > 500 { s[..s.floor_char_boundary(500)].to_string() } else { s })
            .collect();
        engine::log::entry(
            engine::log::Level::Info,
            "title_generate",
            &serde_json::json!({"message_count": recent.len(), "current_title": self.session.title}),
        );
        self.pending_title = true;
        self.engine.send(UiCommand::GenerateTitle {
            user_messages: recent,
        });
    }

    pub fn compact_history(&mut self) {
        self.pending_compact_epoch = self.compact_epoch;
        self.screen.set_throbber(render::Throbber::Compacting);
        self.engine.send(UiCommand::Compact {
            keep_turns: 3,
            history: self.history.clone(),
        });
    }

    pub(super) fn maybe_auto_compact(&mut self) {
        if !self.auto_compact {
            return;
        }
        let Some(ctx) = self.context_window else {
            return;
        };
        let Some(tokens) = self.screen.context_tokens() else {
            return;
        };
        if tokens as u64 * 100 >= ctx as u64 * 80 {
            self.compact_history();
        }
    }

    pub fn rewind_to(
        &mut self,
        block_idx: usize,
    ) -> Option<(String, Vec<crate::input::Attachment>)> {
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

        // Extract image attachments from the target message before truncating.
        let images = self
            .history
            .get(hist_idx)
            .and_then(|msg| msg.content.as_ref())
            .map(|content| {
                use crate::input::Attachment;
                match content {
                    Content::Parts(parts) => parts
                        .iter()
                        .filter_map(|p| match p {
                            protocol::ContentPart::ImageUrl { url, label } => {
                                Some(Attachment::Image {
                                    label: label.clone().unwrap_or_else(|| "image".into()),
                                    data_url: url.clone(),
                                })
                            }
                            _ => None,
                        })
                        .collect(),
                    _ => Vec::new(),
                }
            })
            .unwrap_or_default();

        self.history.truncate(hist_idx);
        self.screen.truncate_to(block_idx);
        self.screen.clear_context_tokens();
        self.auto_approved.clear();
        self.compact_epoch += 1;

        turn_text.map(|t| (t, images))
    }

    // ── Agent internals ──────────────────────────────────────────────────

    pub fn show_user_message(&mut self, input: &str) {
        self.screen.push(Block::User {
            text: input.to_string(),
        });
    }
}
