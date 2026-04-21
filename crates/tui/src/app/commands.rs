use super::*;

pub enum ExecEvent {
    Output(String),
    Done(Option<i32>),
}

/// Stable command outcome exposed through `api::cmd::run`. Internally
/// this is the same as the private `CommandAction` — kept as a public
/// alias so the API surface can evolve independently of the internal
/// enum variants. For now it's just a re-export.
pub type CommandOutcome = CommandAction;

/// Public command runner used by `crate::api::cmd::run`. Accepts raw
/// command lines (`/quit`, `:q`, `/compact foo`, etc.) and dispatches
/// through the existing `handle_command` match. When the command
/// registry lands, this grows a lookup step *before* the legacy
/// fallback — existing call sites don't need to change.
///
/// Normalises a leading `:` to `/` so `/quit` and `:quit` hit the
/// same match arm.
pub fn run_command(app: &mut App, line: &str) -> CommandOutcome {
    let line = line.trim();
    let normalized: String = if let Some(rest) = line.strip_prefix(':') {
        format!("/{rest}")
    } else {
        line.to_string()
    };
    // Lua-registered commands take precedence over built-ins so
    // users can override `/help`, `/export`, etc. from init.lua.
    let name_arg = normalized.trim_start_matches('/');
    let (name, arg) = match name_arg.find(char::is_whitespace) {
        Some(idx) => (
            &name_arg[..idx],
            Some(name_arg[idx + 1..].trim().to_string()),
        ),
        None => (name_arg, None),
    };
    app.snapshot_lua_context();
    app.lua.emit(crate::lua::AutocmdEvent::CmdPre);
    let outcome = if !name.is_empty() && app.lua.has_command(name) {
        app.lua.run_command(name, arg);
        CommandAction::Continue
    } else {
        app.handle_command(&normalized)
    };
    app.lua.emit(crate::lua::AutocmdEvent::CmdPost);
    app.lua.clear_context();
    app.apply_lua_ops();
    outcome
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
                } else {
                    let restore_vim_insert =
                        self.input.vim_enabled() && self.input.vim_in_insert_mode();
                    self.open_rewind_float(turns, restore_vim_insert);
                }
                CommandAction::Continue
            }
            "/vim" => {
                self.update_settings(|s| s.vim = !s.vim);
                CommandAction::Continue
            }
            "/thinking" => {
                self.update_settings(|s| s.show_thinking = !s.show_thinking);
                CommandAction::Continue
            }
            "/export" => {
                if self.history.is_empty() {
                    self.screen.notify_error("nothing to export".into());
                } else {
                    self.open_export_float();
                }
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
                    self.apply_accent(value);
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
            "/yank-block" => {
                let abs_row = self.transcript_window.cursor_abs_row();
                if let Some(text) = self.screen.block_text_at_row(abs_row) {
                    let _ = copy_to_clipboard(&text);
                    self.screen.notify("block copied".into());
                } else {
                    self.screen.notify_error("no block at cursor".into());
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

    /// Apply the result of `process_input` to app state (starting an agent,
    /// running a command, opening a dialog, etc.). Returns `true` if the app
    /// should quit. Centralizes the dispatch previously duplicated across the
    /// Submit path, queued-message fallback, and auto-start-from-queued loop.
    pub(super) fn apply_input_outcome(
        &mut self,
        outcome: InputOutcome,
        content: Content,
        display: &str,
        agent: &mut Option<TurnState>,
        active_dialog: &mut Option<Box<dyn render::Dialog>>,
    ) -> bool {
        match outcome {
            InputOutcome::StartAgent => {
                self.screen.erase_prompt();
                *agent = Some(self.begin_agent_turn(display, content));
            }
            InputOutcome::CustomCommand(cmd) => {
                self.screen.erase_prompt();
                *agent = Some(self.begin_custom_command_turn(*cmd));
            }
            InputOutcome::Compact { instructions } => {
                self.screen.erase_prompt();
                if self.history.is_empty() {
                    self.screen.notify_error("nothing to compact".into());
                } else {
                    self.compact_history(instructions);
                }
            }
            InputOutcome::Exec(rx, kill) => {
                self.screen.erase_prompt();
                self.exec_rx = Some(rx);
                self.exec_kill = Some(kill);
            }
            InputOutcome::CancelAndClear => {
                self.screen.erase_prompt();
                self.reset_session();
                *agent = None;
            }
            InputOutcome::OpenDialog(dlg) => {
                self.open_dialog(dlg, active_dialog);
            }
            InputOutcome::Continue => {}
            InputOutcome::Quit => return true,
        }
        false
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
        match run_command(self, input) {
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
        let old = self.model.clone();
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
        if old != self.model {
            let from = old;
            let to = self.model.clone();
            self.lua
                .emit_data(crate::lua::AutocmdEvent::ModelChange, |lua| {
                    let t = lua.create_table()?;
                    t.set("from", from)?;
                    t.set("to", to)?;
                    Ok(t)
                });
        }
    }

    /// Mutate resolved settings in place, then persist + propagate to
    /// input/screen. Centralizes the pattern that used to be scattered across
    /// the command handlers.
    pub(super) fn update_settings<F: FnOnce(&mut state::ResolvedSettings)>(&mut self, f: F) {
        let prev_show_thinking = self.settings.show_thinking;
        f(&mut self.settings);
        self.input.set_vim_enabled(self.settings.vim);
        self.transcript_window.set_vim_enabled(self.settings.vim);
        self.screen.apply_settings(&self.settings);
        state::save_settings(&self.settings);
        if self.settings.show_thinking != prev_show_thinking {
            self.screen.redraw();
        } else {
            self.screen.mark_dirty();
        }
    }

    /// Replace all resolved settings at once (from a settings dialog result),
    /// persisting + propagating to input/screen.
    pub(super) fn set_settings(&mut self, new: state::ResolvedSettings) {
        self.update_settings(|slot| *slot = new);
    }

    /// Set the agent mode, persist it, and notify the engine. Marks the
    /// screen dirty so the mode indicator refreshes.
    pub(super) fn set_mode(&mut self, mode: Mode) {
        let old = self.mode;
        self.mode = mode;
        state::set_mode(self.mode);
        let system_prompt = self.rebuild_system_prompt();
        let plugin_tools = self.lua.plugin_tool_defs(self.mode);
        self.engine.send(UiCommand::SetMode {
            mode: self.mode,
            system_prompt: Some(system_prompt),
            plugin_tools: Some(plugin_tools),
        });
        self.screen.mark_dirty();
        if old != mode {
            let from = old.as_str().to_string();
            let to = mode.as_str().to_string();
            self.lua
                .emit_data(crate::lua::AutocmdEvent::ModeChange, |lua| {
                    let t = lua.create_table()?;
                    t.set("from", from)?;
                    t.set("to", to)?;
                    Ok(t)
                });
        }
    }

    pub(super) fn toggle_mode(&mut self) {
        let next = self.mode.cycle_within(&self.mode_cycle);
        self.set_mode(next);
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

    /// Apply an accent color: update the global theme, persist, and redraw.
    pub(super) fn apply_accent(&mut self, value: u8) {
        crate::theme::set_accent(value);
        state::set_accent(value);
        self.screen.redraw();
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

    pub(super) fn export_to_file(&mut self) {
        let text = self.format_conversation_text();
        if text.is_empty() {
            self.screen.notify_error("nothing to export".into());
            return;
        }
        let dir = std::path::PathBuf::from(&self.cwd);
        let slug = export_filename_slug(self.session.title.as_deref());
        let stamp = file_timestamp(self.session.created_at_ms);
        let path = unique_export_path(&dir, &slug, &stamp);
        match std::fs::write(&path, &text) {
            Ok(()) => {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default();
                self.screen.notify(format!("exported to {name}"));
            }
            Err(e) => {
                self.screen.notify_error(format!("export failed: {e}"));
            }
        }
    }

    pub(super) fn format_conversation_text(&self) -> String {
        // History is redacted at ingress; export reads straight from it.
        format_conversation_markdown(&self.history, &self.session)
    }
}

fn export_filename_slug(title: Option<&str>) -> String {
    let raw = title
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("conversation");
    let mut slug = String::with_capacity(raw.len());
    let mut prev_dash = false;
    for c in raw.chars() {
        let keep = if c.is_ascii_alphanumeric() {
            Some(c.to_ascii_lowercase())
        } else if c.is_ascii_whitespace() || matches!(c, '-' | '_' | '/') {
            Some('-')
        } else {
            None
        };
        if let Some(ch) = keep {
            if ch == '-' {
                if prev_dash || slug.is_empty() {
                    continue;
                }
                prev_dash = true;
            } else {
                prev_dash = false;
            }
            slug.push(ch);
            if slug.len() >= 40 {
                break;
            }
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        slug.push_str("conversation");
    }
    slug
}

fn file_timestamp(epoch_ms: u64) -> String {
    let ms = if epoch_ms == 0 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    } else {
        epoch_ms
    };
    let s = ms / 1000;
    let days = s / 86400;
    let time = s % 86400;
    let h = time / 3600;
    let mi = (time % 3600) / 60;
    let sec = time % 60;

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

    format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{sec:02}")
}

fn unique_export_path(dir: &std::path::Path, slug: &str, stamp: &str) -> std::path::PathBuf {
    let base = dir.join(format!("smelt-{slug}-{stamp}.md"));
    if !base.exists() {
        return base;
    }
    for n in 1..1000 {
        let candidate = dir.join(format!("smelt-{slug}-{stamp}-{n}.md"));
        if !candidate.exists() {
            return candidate;
        }
    }
    base
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
