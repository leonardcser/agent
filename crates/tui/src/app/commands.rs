use super::*;

pub(super) enum ExecEvent {
    Output(String),
    Done(Option<i32>),
}

impl App {
    // ── Commands ─────────────────────────────────────────────────────────

    pub(super) fn handle_command(&mut self, input: &str) -> CommandAction {
        match input {
            "/exit" | "/quit" | ":q" | ":qa" | ":wq" | ":wqa" => CommandAction::Quit,
            "/clear" | "/new" => CommandAction::CancelAndClear,
            "/compact" => CommandAction::Compact { instructions: None },
            _ if input.starts_with("/compact ") => {
                let instructions = input.strip_prefix("/compact ").unwrap().trim().to_string();
                CommandAction::Compact {
                    instructions: if instructions.is_empty() {
                        None
                    } else {
                        Some(instructions)
                    },
                }
            }
            "/resume" => {
                let entries = self.resume_entries();
                if entries.is_empty() {
                    self.screen.notify_error("no saved sessions".into());
                    CommandAction::Continue
                } else {
                    CommandAction::OpenDialog(Box::new(render::ResumeDialog::new(
                        entries,
                        self.cwd.clone(),
                        self.input.vim_enabled(),
                    )))
                }
            }
            "/rewind" => {
                let turns = self.screen.user_turns();
                if turns.is_empty() {
                    self.screen.notify_error("nothing to rewind".into());
                    CommandAction::Continue
                } else {
                    self.screen.erase_prompt();
                    let restore_vim_insert =
                        self.input.vim_enabled() && self.input.vim_in_insert_mode();
                    CommandAction::OpenDialog(Box::new(render::RewindDialog::new(
                        turns,
                        restore_vim_insert,
                    )))
                }
            }
            "/vim" => {
                let enabled = !self.input.vim_enabled();
                self.input.set_vim_enabled(enabled);
                self.settings.vim = enabled;
                state::save_settings(&self.settings);
                self.screen.mark_dirty();
                CommandAction::Continue
            }
            "/thinking" => {
                self.settings.show_thinking = !self.settings.show_thinking;
                self.screen.apply_settings(&self.settings);
                state::save_settings(&self.settings);
                self.screen.redraw();
                CommandAction::Continue
            }
            "/export" => {
                self.export_to_clipboard();
                CommandAction::Continue
            }
            "/agents" if self.multi_agent => {
                let my_pid = std::process::id();
                let children = engine::registry::children_of(my_pid);
                if children.is_empty() {
                    self.screen.notify_error("no subagents running".into());
                    CommandAction::Continue
                } else {
                    CommandAction::OpenDialog(Box::new(render::AgentsDialog::new(
                        my_pid,
                        self.agent_snapshots.clone(),
                        self.input.vim_enabled(),
                    )))
                }
            }
            "/ps" => {
                if self.engine.processes.list().is_empty() {
                    self.screen.notify_error("no background processes".into());
                    CommandAction::Continue
                } else {
                    CommandAction::OpenDialog(Box::new(render::PsDialog::new(
                        self.engine.processes.clone(),
                    )))
                }
            }
            "/permissions" => {
                let session_entries = self.session_permission_entries();
                let workspace_rules = crate::workspace_permissions::load(&self.cwd);
                if session_entries.is_empty() && workspace_rules.is_empty() {
                    self.screen.notify_error("no permissions".into());
                    CommandAction::Continue
                } else {
                    CommandAction::OpenDialog(Box::new(render::PermissionsDialog::new(
                        session_entries,
                        workspace_rules,
                        self.input.vim_enabled(),
                    )))
                }
            }
            "/fork" | "/branch" => {
                self.fork_session();
                CommandAction::Continue
            }
            "/model" => {
                let models: Vec<(String, String, String)> = self
                    .available_models
                    .iter()
                    .map(|m| (m.key.clone(), m.model_name.clone(), m.provider_name.clone()))
                    .collect();
                if !models.is_empty() {
                    self.input.open_model_completer(&models);
                    self.screen.mark_dirty();
                }
                CommandAction::Continue
            }
            _ if input.starts_with("/model ") => {
                let reference = input.strip_prefix("/model ").unwrap().trim();
                match crate::config::resolve_model_ref(&self.available_models, reference) {
                    Ok(model) => {
                        let key = model.key.clone();
                        self.apply_model(&key);
                    }
                    Err(err) => {
                        self.screen.notify_error(err.to_string());
                    }
                }
                CommandAction::Continue
            }
            "/settings" => {
                self.input.open_settings(&self.settings_state());
                self.screen.mark_dirty();
                CommandAction::Continue
            }
            "/theme" => {
                self.input.open_theme_completer();
                self.screen.mark_dirty();
                CommandAction::Continue
            }
            "/color" => {
                self.input.open_color_completer();
                self.screen.mark_dirty();
                CommandAction::Continue
            }
            "/stats" => {
                let entries = crate::metrics::load();
                let stats = crate::metrics::render_stats(&entries);
                self.input.open_stats(stats);
                self.screen.mark_dirty();
                CommandAction::Continue
            }
            "/cost" => {
                let turns = self.screen.user_turns().len();
                let resolved =
                    engine::pricing::resolve(&self.model, &self.provider_type, &self.model_config);
                let lines = crate::metrics::render_session_cost(
                    self.session_cost_usd,
                    &self.model,
                    turns,
                    &resolved,
                );
                self.input.open_cost(lines);
                self.screen.mark_dirty();
                CommandAction::Continue
            }
            _ if input.starts_with("/theme ") => {
                let name = input.strip_prefix("/theme ").unwrap().trim();
                if let Some(value) = crate::theme::preset_by_name(name) {
                    crate::theme::set_accent(value);
                    state::set_accent(value);
                    self.screen.redraw();
                } else {
                    self.screen.notify_error(format!("unknown theme: {}", name));
                }
                CommandAction::Continue
            }
            _ if input.starts_with("/color ") => {
                let name = input.strip_prefix("/color ").unwrap().trim();
                if let Some(value) = crate::theme::preset_by_name(name) {
                    crate::theme::set_slug_color(value);
                    self.screen.mark_dirty();
                } else {
                    self.screen.notify_error(format!("unknown color: {}", name));
                }
                CommandAction::Continue
            }
            _ if input.starts_with("/btw ") => {
                let question = input.strip_prefix("/btw ").unwrap().trim().to_string();
                if question.is_empty() {
                    self.screen.notify_error("usage: /btw <question>".into());
                } else {
                    self.start_btw(question.clone(), question, vec![]);
                }
                CommandAction::Continue
            }
            _ if input.starts_with('!') && !self.input.skip_shell_escape() => {
                if let Some((rx, kill)) = self.start_shell_escape(input.strip_prefix('!').unwrap())
                {
                    CommandAction::Exec(rx, kill)
                } else {
                    CommandAction::Continue
                }
            }
            _ => CommandAction::Continue,
        }
    }

    /// Execute a command while the agent is running.
    /// Returns the `EventOutcome` to use, or `None` to queue as a message.
    pub(super) fn try_command_while_running(&mut self, input: &str) -> Option<EventOutcome> {
        // Not a command — will be queued as a user message.
        // Skip shell escape check for pasted content
        let is_from_paste = self.input.skip_shell_escape();
        if !input.starts_with('/')
            && (!input.starts_with('!') || is_from_paste)
            && !matches!(input, ":q" | ":qa" | ":wq" | ":wqa")
        {
            return None;
        }
        if input.starts_with('/') && !crate::completer::Completer::is_command(input) {
            return None;
        }

        // Custom commands need their own agent turn — queue them like regular
        // messages so they run after the current turn finishes.
        if input.starts_with('/') && crate::custom_commands::is_custom_command(input) {
            return None;
        }

        // Access control: some commands are blocked while running.
        if let Err(reason) = is_allowed_while_running(input) {
            self.screen.notify_error(reason);
            return Some(EventOutcome::Noop);
        }

        // Delegate to the unified handler.
        match self.handle_command(input) {
            CommandAction::Quit => Some(EventOutcome::Quit),
            CommandAction::CancelAndClear => Some(EventOutcome::CancelAndClear),
            CommandAction::OpenDialog(dlg) => Some(EventOutcome::OpenDialog(dlg)),
            CommandAction::Exec(rx, kill) => Some(EventOutcome::Exec(rx, kill)),
            CommandAction::Continue => Some(EventOutcome::Noop),
            CommandAction::Compact { .. } => unreachable!(), // blocked above
        }
    }

    /// Spawn a shell command asynchronously. Returns a receiver for output
    /// lines and the child process handle (for killing on Ctrl+C).
    pub(super) fn start_shell_escape(
        &mut self,
        raw: &str,
    ) -> Option<(
        tokio::sync::mpsc::UnboundedReceiver<ExecEvent>,
        std::sync::Arc<tokio::sync::Notify>,
    )> {
        let cmd = raw.trim();
        if cmd.is_empty() {
            return None;
        }
        self.screen.start_exec(cmd.to_string());

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let kill = std::sync::Arc::new(tokio::sync::Notify::new());
        let kill2 = kill.clone();
        let cmd = cmd.to_string();
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let child = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(&cmd)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn();

            let mut child = match child {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(ExecEvent::Output(format!("error: {e}")));
                    let _ = tx.send(ExecEvent::Done(None));
                    return;
                }
            };

            let stdout = child.stdout.take().unwrap();
            let stderr = child.stderr.take().unwrap();
            let mut stdout_lines = tokio::io::BufReader::new(stdout).lines();
            let mut stderr_lines = tokio::io::BufReader::new(stderr).lines();
            let mut stdout_done = false;
            let mut stderr_done = false;

            loop {
                tokio::select! {
                    biased;
                    _ = kill2.notified() => {
                        let _ = child.kill().await;
                        let _ = tx.send(ExecEvent::Done(Some(130)));
                        return;
                    }
                    line = stdout_lines.next_line(), if !stdout_done => {
                        match line {
                            Ok(Some(l)) => { let _ = tx.send(ExecEvent::Output(l)); }
                            _ => { stdout_done = true; }
                        }
                    }
                    line = stderr_lines.next_line(), if !stderr_done => {
                        match line {
                            Ok(Some(l)) => { let _ = tx.send(ExecEvent::Output(l)); }
                            _ => { stderr_done = true; }
                        }
                    }
                }
                if stdout_done && stderr_done {
                    break;
                }
            }
            let status = child.wait().await.ok();
            let _ = tx.send(ExecEvent::Done(status.and_then(|s| s.code())));
        });

        Some((rx, kill))
    }

    /// Switch to a model by key, updating all relevant state. Silently does
    /// nothing if the key is not found.
    pub(super) fn apply_model(&mut self, key: &str) {
        let Some(resolved) = self.available_models.iter().find(|m| m.key == key).cloned() else {
            return;
        };
        self.model = resolved.model_name.clone();
        self.api_base = resolved.api_base.clone();
        self.api_key_env = resolved.api_key_env.clone();
        self.provider_type = resolved.provider_type.clone();
        self.model_config = (&resolved.config).into();
        self.screen.set_model_label(self.model.clone());
        let api_key = self.resolve_api_key().unwrap_or_default();
        state::set_selected_model(resolved.key.clone());
        self.engine.send(UiCommand::SetModel {
            model: self.model.clone(),
            api_base: self.api_base.clone(),
            api_key,
            provider_type: self.provider_type.clone(),
        });
    }

    pub(super) fn start_btw(
        &mut self,
        question: String,
        display_question: String,
        image_labels: Vec<String>,
    ) {
        self.screen.set_btw(display_question, image_labels);
        self.engine.send(UiCommand::Btw {
            question,
            history: self.history.clone(),
            reasoning_effort: self.reasoning_effort,
        });
    }

    pub(super) fn toggle_mode(&mut self) {
        self.mode = self.mode.cycle_within(&self.mode_cycle);
        state::set_mode(self.mode);
        self.engine.send(UiCommand::SetMode { mode: self.mode });
        self.screen.mark_dirty();
    }

    pub(super) fn cycle_reasoning(&mut self) {
        let next = self.reasoning_effort.cycle_within(&self.reasoning_cycle);
        self.set_reasoning_effort(next);
    }

    pub(super) fn set_reasoning_effort(&mut self, effort: ReasoningEffort) {
        self.reasoning_effort = effort;
        self.screen.set_reasoning_effort(effort);
        state::set_reasoning_effort(effort);
        self.engine.send(UiCommand::SetReasoningEffort { effort });
    }

    pub(super) fn export_to_clipboard(&mut self) {
        let text = self.format_conversation_text();
        if text.is_empty() {
            self.screen.notify_error("nothing to export".into());
            return;
        }
        match copy_to_clipboard(&text) {
            Ok(()) => {
                self.screen
                    .notify("conversation copied to clipboard".into());
            }
            Err(e) => {
                self.screen.notify_error(format!("clipboard error: {}", e));
            }
        }
    }

    pub(super) fn format_conversation_text(&self) -> String {
        // History is redacted at ingress; export reads straight from it.
        format_conversation_markdown(&self.history, &self.session)
    }
}

// ── Markdown export ─────────────────────────────────────────────────────────

fn format_conversation_markdown(history: &[Message], session: &crate::session::Session) -> String {
    use std::collections::HashMap;
    use std::fmt::Write;

    // Build lookup: tool_call_id → (content, is_error).
    let mut tool_results: HashMap<&str, (&str, bool)> = HashMap::new();
    for msg in history {
        if msg.role == Role::Tool {
            if let (Some(id), Some(content)) = (&msg.tool_call_id, &msg.content) {
                tool_results.insert(id.as_str(), (content.as_text(), msg.is_error));
            }
        }
    }

    let mut out = String::new();

    // Header with session metadata.
    if let Some(title) = &session.title {
        let _ = writeln!(out, "# {title}\n");
    }
    let mut meta_parts: Vec<String> = Vec::new();
    if let Some(model) = &session.model {
        meta_parts.push(format!("**Model:** {model}"));
    }
    if let Some(cwd) = &session.cwd {
        meta_parts.push(format!("**CWD:** `{cwd}`"));
    }
    if session.created_at_ms > 0 {
        meta_parts.push(format!(
            "**Date:** {}",
            format_timestamp(session.created_at_ms)
        ));
    }
    if !meta_parts.is_empty() {
        let _ = writeln!(out, "{}\n", meta_parts.join(" · "));
        let _ = writeln!(out, "---\n");
    }

    for msg in history {
        match msg.role {
            Role::System => {
                let _ = writeln!(out, "## System\n");
                if let Some(c) = &msg.content {
                    let _ = writeln!(out, "{}\n", c.as_text());
                }
            }
            Role::User => {
                let _ = writeln!(out, "## User\n");
                if let Some(c) = &msg.content {
                    let _ = writeln!(out, "{}\n", c.text_content());
                    for label in c.image_labels() {
                        let _ = writeln!(out, "*{label}*\n");
                    }
                }
            }
            Role::Assistant => {
                let _ = writeln!(out, "## Assistant\n");

                // Thinking / reasoning.
                if let Some(reasoning) = &msg.reasoning_content {
                    if !reasoning.is_empty() {
                        let _ = writeln!(out, "<details><summary>thinking</summary>\n");
                        let _ = writeln!(out, "{reasoning}\n");
                        let _ = writeln!(out, "</details>\n");
                    }
                }

                // Text content.
                if let Some(c) = &msg.content {
                    if !c.is_empty() {
                        let _ = writeln!(out, "{}\n", c.text_content());
                    }
                }

                // Tool calls with inline results.
                if let Some(calls) = &msg.tool_calls {
                    for tc in calls {
                        format_tool_call(&mut out, tc, &tool_results);
                    }
                }
            }
            Role::Tool => {
                // Already inlined under their tool call — skip.
            }
            Role::Agent => {
                let id = msg.agent_from_id.as_deref().unwrap_or("smelt");
                let slug = msg.agent_from_slug.as_deref().unwrap_or("");
                if slug.is_empty() {
                    let _ = writeln!(out, "## Agent: {id}\n");
                } else {
                    let _ = writeln!(out, "## Agent: {id} ({slug})\n");
                }
                if let Some(c) = &msg.content {
                    let _ = writeln!(out, "{}\n", c.text_content());
                }
            }
        }
    }

    out.trim_end().to_string()
}

fn format_tool_call(
    out: &mut String,
    tc: &protocol::ToolCall,
    tool_results: &std::collections::HashMap<&str, (&str, bool)>,
) {
    use std::fmt::Write;

    let name = &tc.function.name;
    let args: std::collections::HashMap<String, serde_json::Value> =
        serde_json::from_str(&tc.function.arguments).unwrap_or_default();
    let summary = engine::tools::tool_arg_summary(name, &args);

    let _ = writeln!(out, "### {name}");
    if !summary.is_empty() {
        let _ = writeln!(out, "`{summary}`");
    }

    // Show full arguments for tools where the summary loses detail.
    match name.as_str() {
        "edit_file" => {
            let file = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
            let old = args
                .get("old_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let new = args
                .get("new_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !file.is_empty() {
                let _ = writeln!(out, "\n```diff");
                for line in old.lines() {
                    let _ = writeln!(out, "- {line}");
                }
                for line in new.lines() {
                    let _ = writeln!(out, "+ {line}");
                }
                let _ = writeln!(out, "```");
            }
        }
        "write_file" => {
            let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
            if !content.is_empty() {
                let ext = summary.rsplit('.').next().unwrap_or("");
                let _ = writeln!(out, "\n```{ext}");
                let _ = writeln!(out, "{content}");
                let _ = writeln!(out, "```");
            }
        }
        "bash" => {
            let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
            if cmd.contains('\n') {
                // Multi-line command — show full thing.
                let _ = writeln!(out, "\n```bash\n{cmd}\n```");
            }
        }
        _ => {}
    }

    // Inline the tool result.
    if let Some((result_text, is_error)) = tool_results.get(tc.id.as_str()) {
        let _ = writeln!(out);
        if *is_error {
            let _ = writeln!(out, "**Error:**");
        }
        let trimmed = result_text.trim();
        if trimmed.is_empty() {
            let _ = writeln!(out, "*(empty)*\n");
        } else if trimmed.lines().count() > engine::tools::MAX_TOOL_OUTPUT_LINES {
            let truncated: String = trimmed
                .lines()
                .take(engine::tools::MAX_TOOL_OUTPUT_LINES)
                .collect::<Vec<_>>()
                .join("\n");
            let remaining = trimmed.lines().count() - engine::tools::MAX_TOOL_OUTPUT_LINES;
            let _ = writeln!(out, "```\n{truncated}\n```\n");
            let _ = writeln!(out, "*({remaining} lines truncated)*\n");
        } else {
            let _ = writeln!(out, "```\n{trimmed}\n```\n");
        }
    } else {
        let _ = writeln!(out);
    }
}

fn format_timestamp(epoch_ms: u64) -> String {
    let s = epoch_ms / 1000;
    // Days since Unix epoch.
    let days = s / 86400;
    let time = s % 86400;
    let h = time / 3600;
    let m = (time % 3600) / 60;

    // Civil date from day count (algorithm from Howard Hinnant).
    let z = days as i64 + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };

    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02} UTC")
}

/// Copy text to the system clipboard using platform commands.
pub(crate) fn copy_to_clipboard(text: &str) -> Result<(), String> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let (cmd, args): (&str, &[&str]) = if cfg!(target_os = "macos") {
        ("pbcopy", &[])
    } else if std::env::var("WAYLAND_DISPLAY").is_ok() {
        ("wl-copy", &[])
    } else {
        ("xclip", &["-selection", "clipboard"])
    };

    let mut child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("{cmd}: {e}"))?;

    child
        .stdin
        .take()
        .unwrap()
        .write_all(text.as_bytes())
        .map_err(|e| e.to_string())?;

    let status = child.wait().map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{cmd} exited with {status}"))
    }
}
