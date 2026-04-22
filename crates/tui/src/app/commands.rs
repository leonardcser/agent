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
                    self.notify_error("no saved sessions".into());
                } else {
                    super::dialogs::resume::open(self, entries);
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
            "/agents" if self.multi_agent => {
                let my_pid = std::process::id();
                let children = engine::registry::children_of(my_pid);
                if children.is_empty() {
                    self.notify_error("no subagents running".into());
                } else {
                    super::dialogs::agents::open(self);
                }
                CommandAction::Continue
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
                        self.notify_error(err.to_string());
                    }
                }
                CommandAction::Continue
            }
            "/settings" => {
                self.input.open_settings(&self.settings_state());
                CommandAction::Continue
            }
            "/theme" => {
                self.input.open_theme_completer();
                CommandAction::Continue
            }
            "/color" => {
                self.input.open_color_completer();
                CommandAction::Continue
            }
            "/stats" => {
                let entries = crate::metrics::load();
                let stats = crate::metrics::render_stats(&entries);
                self.input.open_stats(stats);
                CommandAction::Continue
            }
            "/cost" => {
                let turns = self.user_turns().len();
                let resolved =
                    engine::pricing::resolve(&self.model, &self.provider_type, &self.model_config);
                let lines = crate::metrics::render_session_cost(
                    self.session_cost_usd,
                    &self.model,
                    turns,
                    &resolved,
                );
                self.input.open_cost(lines);
                CommandAction::Continue
            }
            _ if input.starts_with("/theme ") => {
                let name = input.strip_prefix("/theme ").unwrap().trim();
                if let Some(value) = crate::theme::preset_by_name(name) {
                    self.apply_accent(value);
                } else {
                    self.notify_error(format!("unknown theme: {}", name));
                }
                CommandAction::Continue
            }
            _ if input.starts_with("/color ") => {
                let name = input.strip_prefix("/color ").unwrap().trim();
                if let Some(value) = crate::theme::preset_by_name(name) {
                    crate::theme::set_slug_color(value);
                } else {
                    self.notify_error(format!("unknown color: {}", name));
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
    ) -> bool {
        match outcome {
            InputOutcome::StartAgent => {
                let turn = self.begin_agent_turn(display, content);
                self.agent = Some(turn);
            }
            InputOutcome::CustomCommand(cmd) => {
                let turn = self.begin_custom_command_turn(*cmd);
                self.agent = Some(turn);
            }
            InputOutcome::Compact { instructions } => {
                if self.history.is_empty() {
                    self.notify_error("nothing to compact".into());
                } else {
                    self.compact_history(instructions);
                }
            }
            InputOutcome::Exec(rx, kill) => {
                self.exec_rx = Some(rx);
                self.exec_kill = Some(kill);
            }
            InputOutcome::CancelAndClear => {
                self.reset_session();
                self.agent = None;
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
            self.notify_error(reason);
            return Some(EventOutcome::Noop);
        }

        // Delegate to the unified handler.
        match run_command(self, input) {
            CommandAction::Quit => Some(EventOutcome::Quit),
            CommandAction::CancelAndClear => Some(EventOutcome::CancelAndClear),
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
        self.start_exec(cmd.to_string());

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
        f(&mut self.settings);
        self.input.set_vim_enabled(self.settings.vim);
        self.transcript_window.set_vim_enabled(self.settings.vim);
        state::save_settings(&self.settings);
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
        state::set_reasoning_effort(effort);
        self.engine.send(UiCommand::SetReasoningEffort { effort });
    }

    /// Apply an accent color: update the global theme, persist, and redraw.
    pub(super) fn apply_accent(&mut self, value: u8) {
        crate::theme::set_accent(value);
        state::set_accent(value);
    }
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
