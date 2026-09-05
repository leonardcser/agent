use crate::app::{
    CommandAction, ContextWindowUpdate, EventOutcome, InputOutcome, QueueStage, QueuedInput, TuiApp,
};
use protocol::{AgentMode, Content, ReasoningEffort, UiCommand};

mod parse;

pub(crate) use parse::{parse_command_line, ParsedCommand};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommandSource {
    Prompt,
    Cmdline,
    Lua,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ShellSink {
    Transcript,
    Overlay,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CommandContext {
    pub(crate) source: CommandSource,
    pub(crate) queue_target: QueueStage,
}

impl CommandContext {
    pub(crate) fn prompt() -> Self {
        Self {
            source: CommandSource::Prompt,
            queue_target: QueueStage::Turn,
        }
    }

    pub(crate) fn cmdline() -> Self {
        Self {
            source: CommandSource::Cmdline,
            queue_target: QueueStage::Turn,
        }
    }

    pub(crate) fn lua() -> Self {
        Self {
            source: CommandSource::Lua,
            queue_target: QueueStage::Turn,
        }
    }

    pub(crate) fn with_queue_target(mut self, queue_target: QueueStage) -> Self {
        self.queue_target = queue_target;
        self
    }
}

pub(crate) enum ExecEvent {
    Output(String),
    Done {
        output: String,
        exit_code: Option<i32>,
        termination: Option<protocol::JobTermination>,
    },
}

/// Live view of a supervisor-owned shell job. Output is bounded before it
/// reaches this channel; `kill` terminates the job's containment boundary.
pub(crate) struct ExecHandle {
    pub rx: tokio::sync::mpsc::UnboundedReceiver<ExecEvent>,
    pub kill: tokio_util::sync::CancellationToken,
    pub sink: ShellSink,
}

pub(crate) enum CommandEffect {
    Continue,
    QuitApp,
    CloseFocusedContext,
    QuitFocusedContextOrApp,
    RunCommand {
        name: String,
        arg: Option<String>,
        ctx: CommandContext,
    },
    StartShell {
        script: String,
        sink: ShellSink,
    },
}

pub(crate) fn prompt_quit_alias(line: &str) -> bool {
    let ParsedCommand::Ex { name, bang: _, arg } = parse_command_line(line) else {
        return false;
    };
    arg.is_none() && matches!(name, "q" | "quit" | "qa" | "qall" | "quitall" | "wq" | "x")
}

fn parse_for_context<'a>(line: &'a str, ctx: CommandContext) -> ParsedCommand<'a> {
    if ctx.source == CommandSource::Prompt
        && smelt_buffer::text::trim_start_whitespace(line).starts_with(':')
        && !prompt_quit_alias(line)
    {
        return ParsedCommand::Bare {
            text: smelt_buffer::text::trim_whitespace(line),
        };
    }
    parse_command_line(line)
}

impl CommandEffect {
    fn from_parsed(parsed: ParsedCommand<'_>, ctx: CommandContext) -> Self {
        match parsed {
            ParsedCommand::Shell { script, sink } => {
                let sink = match (ctx.source, sink) {
                    (CommandSource::Prompt, ShellSink::Overlay) => ShellSink::Transcript,
                    _ => sink,
                };
                Self::StartShell {
                    script: script.to_string(),
                    sink,
                }
            }
            ParsedCommand::Slash { name, arg } => Self::RunCommand {
                name: name.to_string(),
                arg: arg.map(str::to_string),
                ctx,
            },
            ParsedCommand::Ex { name, bang, arg } => ex_command_effect(name, bang, arg, ctx),
            ParsedCommand::Empty | ParsedCommand::Bare { .. } => Self::Continue,
        }
    }
}

fn ex_command_effect(
    name: &str,
    bang: bool,
    arg: Option<&str>,
    ctx: CommandContext,
) -> CommandEffect {
    let name = smelt_buffer::text::trim_whitespace(name);
    match name {
        "q" | "quit" => {
            if bang {
                CommandEffect::QuitApp
            } else {
                CommandEffect::QuitFocusedContextOrApp
            }
        }
        "qa" | "qall" | "quitall" => CommandEffect::QuitApp,
        "close" => CommandEffect::CloseFocusedContext,
        "wq" | "x" => CommandEffect::QuitApp,
        _ => CommandEffect::RunCommand {
            name: name.to_string(),
            arg: arg.map(str::to_string),
            ctx,
        },
    }
}

/// Dispatch a raw command line with prompt semantics.
pub(crate) fn run_command(app: &mut TuiApp, line: &str) -> CommandAction {
    run_command_with_context(app, line, CommandContext::prompt())
}

pub(crate) fn run_command_with_context(
    app: &mut TuiApp,
    line: &str,
    ctx: CommandContext,
) -> CommandAction {
    let _perf = smelt_perf::perf::begin("cmd:dispatch");
    let effect = CommandEffect::from_parsed(parse_for_context(line, ctx), ctx);
    app.apply_command_effect(effect)
}

enum CloseTarget {
    Cmdline,
    ShellPanel,
    Overlay(crate::smelt_edit::OverlayId),
}

enum ModeNotePolicy {
    Append,
    Suppress,
}

impl TuiApp {
    fn focused_close_target(&self) -> Option<CloseTarget> {
        if self.cmdline_is_focused() {
            return Some(CloseTarget::Cmdline);
        }
        let overlay = self.ui.focused_overlay()?;
        if self
            .overlays
            .shell_panel()
            .is_some_and(|panel| panel.overlay == overlay)
        {
            return Some(CloseTarget::ShellPanel);
        }
        Some(CloseTarget::Overlay(overlay))
    }

    pub(crate) fn apply_command_effect(&mut self, effect: CommandEffect) -> CommandAction {
        match effect {
            CommandEffect::Continue => CommandAction::Continue,
            CommandEffect::QuitApp => {
                self.pending_quit = true;
                CommandAction::Continue
            }
            CommandEffect::CloseFocusedContext => {
                if !self.close_focused_context() {
                    self.notify_error("no focused window to close".into());
                }
                CommandAction::Continue
            }
            CommandEffect::QuitFocusedContextOrApp => {
                self.quit_focused_context_or_app();
                CommandAction::Continue
            }
            CommandEffect::RunCommand { name, arg, ctx } => {
                self.run_command_by_name(&name, arg.as_deref(), ctx)
            }
            CommandEffect::StartShell { script, sink } => {
                if self.prompt.skip_shell_escape() {
                    return CommandAction::Continue;
                }
                match self.start_shell_escape_with_sink(&script, sink) {
                    Some(handle) => CommandAction::Exec(handle),
                    None => CommandAction::Continue,
                }
            }
        }
    }

    pub(crate) fn has_command_name(&self, name: &str) -> bool {
        !name.is_empty() && self.lua.has_command(name)
    }

    pub(crate) fn run_command_by_name(
        &mut self,
        name: &str,
        arg: Option<&str>,
        ctx: CommandContext,
    ) -> CommandAction {
        let name = name.to_string();
        let arg = arg.map(str::to_string);
        let next_turn_id = self.conversation.next_turn_id();
        self.core
            .signals
            .emit_dyn("cmd_pre", std::rc::Rc::new(name.clone()));
        self.drain_signals_pending();
        if self.has_command_name(&name) {
            let lua = self.lua.execution();
            let lua_name = name.clone();
            crate::lua::scope_app(self, move || {
                lua.run_command_with_queue_target(&lua_name, arg, ctx.queue_target.into());
            });
        } else {
            let prefix = match ctx.source {
                CommandSource::Cmdline => ':',
                _ => '/',
            };
            self.notify_error(format!("unknown command: {prefix}{name}"));
        }
        self.core
            .signals
            .emit_dyn("cmd_post", std::rc::Rc::new(name));
        self.drain_signals_pending();
        self.flush_lua_callbacks();
        if self.conversation.next_turn_id() == next_turn_id {
            self.invalidate_prompt_prediction();
        }
        CommandAction::Continue
    }

    pub(crate) fn quit_focused_context_or_app(&mut self) {
        if !self.close_focused_context() {
            self.pending_quit = true;
        }
    }

    pub(crate) fn close_focused_context(&mut self) -> bool {
        match self.focused_close_target() {
            Some(CloseTarget::Cmdline) => {
                self.close_cmdline();
                true
            }
            Some(CloseTarget::ShellPanel) => self.close_shell_panel_and_stop_job(),
            Some(CloseTarget::Overlay(overlay)) => {
                self.close_overlay(overlay);
                true
            }
            None => false,
        }
    }

    /// Apply a resolved `InputOutcome` after the submitted prompt has been
    /// committed. Command handlers can then safely open prompt-owned UI.
    pub(crate) fn apply_input_outcome(
        &mut self,
        outcome: InputOutcome,
        content: Content,
        display: &str,
    ) {
        match outcome {
            InputOutcome::StartAgent => {
                let turn = self.begin_agent_turn(display, content);
                self.conversation.set_active(turn);
            }
            InputOutcome::Command(line) => {
                if let CommandAction::Exec(handle) = run_command(self, &line) {
                    self.overlays.install_execution(handle);
                }
            }
            InputOutcome::Continue => {}
        }
    }

    fn run_command_with_queue_target(
        &mut self,
        input: &str,
        queue_target: QueueStage,
    ) -> CommandAction {
        run_command_with_context(
            self,
            input,
            CommandContext::prompt().with_queue_target(queue_target),
        )
    }

    /// Attempt to execute a command mid-run. Returns the outcome, or `None`
    /// to queue the input as a regular user message.
    pub(crate) fn try_command_while_running(
        &mut self,
        input: &str,
        queue_target: QueueStage,
    ) -> Option<EventOutcome> {
        let is_from_paste = self.prompt.skip_shell_escape();

        if input.starts_with('!') && !is_from_paste {
            return match self.run_command_with_queue_target(input, queue_target) {
                CommandAction::Exec(handle) => Some(EventOutcome::Exec(handle)),
                CommandAction::Continue => Some(EventOutcome::Noop),
            };
        }

        if prompt_quit_alias(input) {
            return Some(EventOutcome::Quit);
        }

        let parsed = parse_command_line(input);
        let (name, normalized) = match parsed {
            ParsedCommand::Slash { name, .. } if self.has_command_name(name) => {
                (name.to_string(), input.to_string())
            }
            _ => return None,
        };
        match self
            .lua
            .command_busy_behavior(&name)
            .unwrap_or(smelt_core::lua::CommandBusyBehavior::Run)
        {
            smelt_core::lua::CommandBusyBehavior::QueueCommand => {
                let queued = QueuedInput::command(normalized);
                match queue_target {
                    QueueStage::Turn => {
                        self.prompt.try_queue_turn(queued);
                    }
                    QueueStage::Request => {
                        self.queue_input_for_request(queued);
                    }
                }
                return Some(EventOutcome::Noop);
            }
            smelt_core::lua::CommandBusyBehavior::QueueRequest => {
                return match self.run_command_with_queue_target(&normalized, queue_target) {
                    CommandAction::Exec(handle) => Some(EventOutcome::Exec(handle)),
                    CommandAction::Continue => Some(EventOutcome::Noop),
                };
            }
            smelt_core::lua::CommandBusyBehavior::Reject => {
                self.notify_error(format!("cannot run /{name} while agent is working"));
                return Some(EventOutcome::Noop);
            }
            smelt_core::lua::CommandBusyBehavior::Run => {}
        }

        match self.run_command_with_queue_target(&normalized, queue_target) {
            CommandAction::Exec(handle) => Some(EventOutcome::Exec(handle)),
            CommandAction::Continue => Some(EventOutcome::Noop),
        }
    }

    /// Spawn a supervised shell job. Returns a handle for streaming bounded
    /// output and terminating the containment boundary on Ctrl+C.
    pub(crate) fn start_shell_escape(&mut self, raw: &str) -> Option<ExecHandle> {
        self.start_shell_escape_with_sink(raw, ShellSink::Transcript)
    }

    pub(crate) fn start_shell_escape_with_sink(
        &mut self,
        raw: &str,
        sink: ShellSink,
    ) -> Option<ExecHandle> {
        let cmd = smelt_buffer::text::trim_whitespace(raw);
        if cmd.is_empty() {
            return None;
        }
        if self.overlays.execution_is_running() {
            self.notify_error("a shell command is already running".into());
            return None;
        }
        match sink {
            ShellSink::Transcript => {
                self.start_exec(cmd.to_string());
                self.publish_input_submit(format!("!{cmd}"));
            }
            ShellSink::Overlay => self.open_shell_panel(cmd),
        }

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let kill = tokio_util::sync::CancellationToken::new();
        let cancel = kill.clone();
        let cmd = cmd.to_string();
        let cwd = self.workspace.cwd_path().to_owned();
        let started_at = self.core.clock.instant_now();
        let supervisor = self.core.jobs.clone();
        tokio::spawn(async move {
            let out = supervisor
                .run(
                    &cmd,
                    smelt_core::process::JobRunConfig {
                        timeout: None,
                        shell: smelt_core::process::ShellSpec::default(),
                        cwd,
                        started_at,
                        cancel: Some(cancel),
                        background_on_timeout: false,
                        detachable: false,
                    },
                    |line| {
                        let _ = tx.send(ExecEvent::Output(line));
                    },
                )
                .await;
            let output = if out.termination.is_none() && out.is_error {
                format!("error: {}", out.content)
            } else {
                out.content
            };
            let _ = tx.send(ExecEvent::Done {
                output,
                exit_code: out.exit_code,
                termination: out.termination,
            });
        });

        Some(ExecHandle { rx, kill, sink })
    }

    fn warn_if_recent_write_failed(&mut self, choice: &str, result: std::io::Result<()>) {
        if let Err(error) = result {
            self.notify_warn(format!("failed to remember {choice}: {error}"));
        }
    }

    /// Switch to a model by key. No-op if the key is not found.
    /// `record=false` skips the `recent.json` write so session
    /// resume doesn't overwrite the user's last explicit pick.
    pub(crate) fn apply_model(&mut self, key: &str, record: bool) {
        let Some(resolved) = self
            .core
            .config
            .available_models
            .iter()
            .find(|m| m.key == key)
            .cloned()
        else {
            return;
        };
        let mut active = smelt_core::ActiveModel::from_resolved(&resolved);
        self.core
            .startup_overrides
            .apply_to_active_model(&mut active);
        let api_key = self.resolve_api_key_for_env(&active.api_key_env);
        if api_key.is_none() {
            active.availability = smelt_core::ModelAvailability::Unavailable {
                reason: smelt_core::ModelUnavailableReason::MissingCredentials,
            };
        }
        if self.core.config.active_model() == Some(&active)
            && self.core.config.model_selection.requested_key.as_deref()
                == Some(resolved.key.as_str())
        {
            return;
        }
        if record && self.block_read_only_mutation("change read-only session settings") {
            return;
        }
        let old = self
            .core
            .config
            .active_model()
            .map(|model| model.key.clone());
        let old_reasoning = self.core.config.reasoning_effort.clone();
        self.core.config.revision = self.core.config.revision.wrapping_add(1);
        self.core.config.set_model_selection(
            smelt_core::ModelSelectionState {
                requested_key: Some(resolved.key.clone()),
                requested_by: if record {
                    smelt_core::ModelSelectionSource::User
                } else {
                    smelt_core::ModelSelectionSource::Session
                },
                active: Some(active),
            },
            &self.core.startup_overrides,
        );
        if record {
            self.update_session_persist_metadata();
        }
        if record && self.core.config.remember.model {
            let result = self.core.recent.set_selected_model(resolved.key.clone());
            self.warn_if_recent_write_failed("model selection", result);
        }
        self.warn_if_api_base_normalized();
        if self.active_agent_turn_id().is_some() {
            if let Some(api_key) = api_key {
                let target = self
                    .core
                    .config
                    .active_model()
                    .expect("model selection was just installed")
                    .target(api_key);
                self.core.engine.send(UiCommand::SetTurnModel {
                    target: Box::new(target),
                    system_prompt: self.assemble_system_prompt(),
                });
            }
        }
        if self.core.config.reasoning_effort != old_reasoning {
            self.publish_reasoning_effort_change();
            if record {
                self.notify(format!(
                    "reasoning: {} ({} unavailable on this model)",
                    self.core.config.reasoning_effort.label(),
                    old_reasoning.label(),
                ));
            }
        }
        self.core.engine.send(UiCommand::SetFastMode {
            enabled: self.fast_mode_active(),
        });
        if old.as_deref() != Some(resolved.key.as_str()) {
            self.core
                .signals
                .set_dyn("model", std::rc::Rc::new(Some(resolved.key.clone())));
        }
        let identity = self.active_context_token_identity();
        if record {
            self.conversation.clear_token_baseline(identity);
        } else {
            self.conversation
                .clear_token_baseline_for_loaded_model(identity);
        }
        self.refresh_context_window();
    }

    /// Kick off a background fetch for the current model's context window.
    /// The platform owner rejects stale responses when model identity changes.
    pub(crate) fn refresh_context_window(&mut self) {
        let Some(active) = self.core.config.active_model().cloned() else {
            if self.clear_context_window_target() {
                self.core.config.context_window = None;
            }
            return;
        };
        let Ok(api_key) =
            crate::app::agent::lookup_api_key(&active.api_key_env, |key| std::env::var(key))
        else {
            return;
        };
        let target = crate::app::ContextWindowTarget::from_active(&active);
        let Some(refresh) = self.prepare_context_window_refresh(target) else {
            return;
        };
        let api_base = refresh.target.api_base.clone();
        let provider_type = refresh.target.provider_type.clone();
        let model = refresh.target.model.clone();
        let clock = std::sync::Arc::clone(&self.core.clock);
        tokio::spawn(async move {
            let provider = engine::EngineProvider::new(
                api_base,
                api_key,
                &provider_type,
                refresh.client,
                clock,
            )
            .with_model_config(refresh.target.config.clone());
            let value = provider.fetch_context_window(&model).await;
            let _ = refresh.sender.send(ContextWindowUpdate {
                revision: refresh.revision,
                target: refresh.target,
                value,
            });
        });
    }

    pub(crate) fn apply_settings_effects(&mut self, old: &smelt_core::config::ResolvedSettings) {
        let settings = self.core.config.settings.clone();
        let system_clipboard_changed = settings.system_clipboard != old.system_clipboard;
        let vim_changed = settings.vim != old.vim;
        let prediction_disabled = old.show_prediction && !settings.show_prediction;
        let file_icons_changed = (old.file_icons, old.file_icon_colors)
            != (settings.file_icons, settings.file_icon_colors);
        let terminal_title_changed = settings.terminal_title != old.terminal_title;
        let auto_reload_changed = settings.auto_reload != old.auto_reload;
        let system_clipboard = settings.system_clipboard;
        let vim = settings.vim;
        let auto_reload = settings.auto_reload;

        if system_clipboard_changed {
            self.core.set_system_clipboard_enabled(system_clipboard);
        }
        if vim_changed {
            let prompt_win = self
                .ui
                .win_mut(crate::app::PROMPT_WIN)
                .expect("prompt window");
            self.prompt.set_vim_enabled(prompt_win, vim);
            self.transcript_win_mut().set_vim_enabled(vim);
        }
        if prediction_disabled {
            self.invalidate_prompt_prediction();
        }
        if file_icons_changed {
            self.sync_inline_options();
            self.sync_transcript_renderer_generation();
        }
        if terminal_title_changed {
            self.core
                .signals
                .publish_if_changed("settings_terminal_title", settings.terminal_title);
        }
        if auto_reload_changed {
            self.set_auto_reload_enabled(auto_reload);
        }
    }

    fn committed_watch_paths(&self) -> crate::auto_reload::WatchPaths {
        crate::auto_reload::WatchPaths::from_manifest(
            self.lua.manifest.roots.clone(),
            self.lua.manifest.target_cwd.as_deref(),
        )
    }

    fn set_auto_reload_enabled(&mut self, enabled: bool) {
        let paths = self.committed_watch_paths();
        self.auto_reload.set_desired(enabled, paths);
    }

    pub(crate) fn reconcile_auto_reload(&mut self) {
        self.set_auto_reload_enabled(self.core.config.settings.auto_reload);
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn set_settings_for_harness(&mut self, new: smelt_core::config::ResolvedSettings) {
        if self.core.config.settings == new {
            return;
        }
        let old = std::mem::replace(&mut self.core.config.settings, new);
        self.core.config.revision = self.core.config.revision.wrapping_add(1);
        if self.core.startup_overrides.request_audit_env.is_none() {
            self.core.config.request_audit =
                protocol::RequestAuditMode::parse(&self.core.config.settings.request_audit)
                    .unwrap_or_default();
        }
        self.apply_settings_effects(&old);
    }

    pub(crate) fn fast_mode(&self) -> bool {
        self.conversation
            .session()
            .fast_mode
            .unwrap_or(self.core.config.settings.fast_mode)
    }

    pub(crate) fn fast_mode_supported(&self) -> bool {
        self.core.config.active_model().is_some_and(|model| {
            model.provider_type == "codex" && model.config.supports_fast_mode == Some(true)
        })
    }

    pub(crate) fn fast_mode_active(&self) -> bool {
        self.fast_mode_supported() && self.fast_mode()
    }

    pub(crate) fn set_fast_mode(&mut self, enabled: bool) {
        self.conversation.set_fast_mode(enabled);
        self.core.engine.send(UiCommand::SetFastMode {
            enabled: self.fast_mode_active(),
        });
    }

    /// `record=false` skips the `recent.json` write so session
    /// resume doesn't overwrite the user's last explicit pick.
    pub(crate) fn set_mode(&mut self, mode: AgentMode, record: bool) {
        self.apply_mode(mode, record, ModeNotePolicy::Append);
    }

    pub(crate) fn restore_mode_after_rewind(&mut self, mode: AgentMode) {
        self.apply_mode(mode, false, ModeNotePolicy::Suppress);
    }

    fn apply_mode(&mut self, mode: AgentMode, record: bool, note_policy: ModeNotePolicy) {
        if record && self.block_read_only_mutation("change read-only session mode") {
            return;
        }
        let old = self.core.config.mode.clone();
        if old == mode {
            return;
        }
        self.core.config.revision = self.core.config.revision.wrapping_add(1);
        self.core.config.mode = mode.clone();
        if record && self.core.config.remember.mode {
            let result = self.core.recent.set_mode(self.core.config.mode.clone());
            self.warn_if_recent_write_failed("mode", result);
        }
        // Publish new mode before Lua/tool snapshots for future requests.
        if old != mode {
            self.core
                .signals
                .set_dyn("agent_mode", std::rc::Rc::new(mode.as_str().to_string()));
            self.drain_signals_pending();
            self.core
                .engine
                .send(UiCommand::SetMode { mode: mode.clone() });
            if matches!(note_policy, ModeNotePolicy::Append) {
                // Queue an internal mode-change note so the next LLM request learns about
                // the new mode without regenerating the cached prompt prefix. If a
                // turn is active, the engine applies the same note when it reaches
                // its next request boundary; otherwise we apply it locally before
                // the next turn starts.
                let mode_name = self.core.config.mode.as_str().to_string();
                let lua = self.lua.execution();
                let note_text = crate::lua::scope_app(self, || lua.mode_note(mode_name.as_str()));
                self.queue_history_append(crate::app::PendingHistoryAppend::mode_change(
                    mode_name, note_text,
                ));
            }
            if record {
                self.update_session_persist_metadata();
            }
        }
    }

    fn publish_reasoning_effort_change(&mut self) {
        let effort = self.core.config.reasoning_effort.clone();
        self.core
            .signals
            .set_dyn("reasoning", std::rc::Rc::new(effort.label().to_string()));
        if !self.conversation.is_active() {
            self.sync_reasoning_effort_applied();
        }
        self.core
            .engine
            .send(UiCommand::SetReasoningEffort { effort });
    }

    /// `record=false` skips the `recent.json` write so session
    /// resume doesn't overwrite the user's last explicit pick.
    pub(crate) fn set_reasoning_effort(
        &mut self,
        effort: ReasoningEffort,
        record: bool,
    ) -> Result<(), String> {
        let effort = match self.core.config.active_model() {
            Some(model)
                if record && !model.reasoning_catalog().supports_reasoning_effort(&effort) =>
            {
                return Err(format!(
                    "reasoning effort '{}' is not supported by model '{}'",
                    effort.label(),
                    model.key
                ));
            }
            Some(model) if !record => model.reasoning_catalog().reconcile_reasoning_effort(effort),
            _ => effort,
        };
        if self.core.config.reasoning_effort == effort {
            return Ok(());
        }
        if record && self.block_read_only_mutation("change read-only session reasoning effort") {
            return Ok(());
        }
        self.core.config.revision = self.core.config.revision.wrapping_add(1);
        self.core.config.reasoning_effort = effort.clone();
        if record {
            self.update_session_persist_metadata();
        }
        if record && self.core.config.remember.reasoning_effort {
            let result = self.core.recent.set_reasoning_effort(effort.clone());
            self.warn_if_recent_write_failed("reasoning effort", result);
        }
        self.publish_reasoning_effort_change();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use smelt_core::transcript_model::Block;

    fn slash<'a>(name: &'a str, arg: Option<&'a str>) -> ParsedCommand<'a> {
        ParsedCommand::Slash { name, arg }
    }

    fn ex<'a>(name: &'a str, bang: bool, arg: Option<&'a str>) -> ParsedCommand<'a> {
        ParsedCommand::Ex { name, bang, arg }
    }

    fn shell(script: &str, sink: ShellSink) -> ParsedCommand<'_> {
        ParsedCommand::Shell { script, sink }
    }

    fn mode_blocks(app: &crate::app::TuiApp) -> Vec<&str> {
        let history = app.conversation.transcript().history();
        history
            .order
            .iter()
            .filter_map(|id| match history.block(*id) {
                Some(Block::Mode { text, .. }) => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn pending_mode_appends(app: &crate::app::TuiApp) -> Vec<&crate::app::PendingHistoryAppend> {
        app.conversation
            .pending_history_appends()
            .iter()
            .filter(|append| {
                append.coalescing_note_kind() == Some(protocol::HistoryNoteKind::ModeChange)
            })
            .collect()
    }

    fn normal_app() -> crate::app::test_harness::TestApp {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.core.config.mode = AgentMode::parse("normal").unwrap();
        app.app
            .conversation
            .set_session_mode_for_harness(Some("normal".into()));
        app
    }

    #[test]
    fn self_queued_lua_command_stops_at_flush_limit() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.run_lua_result(
            r#"
                _G.fuzz_command_runs = 0
                smelt.cmd.register("fuzz.self_queue", function()
                    _G.fuzz_command_runs = _G.fuzz_command_runs + 1
                    smelt.cmd.run("fuzz.self_queue")
                end)
                smelt.cmd.run("fuzz.self_queue")
            "#,
        )
        .expect("run self-queued command");

        assert_eq!(
            app.lua_int_global("fuzz_command_runs"),
            Some(crate::lua::MAX_PENDING_LUA_COMMANDS as i64)
        );
        assert!(app.app.lua.shared().drain_commands().is_empty());
    }

    fn set_active_model(
        app: &mut crate::app::TuiApp,
        model: &str,
        api_base: &str,
        api_key_env: &str,
        provider_type: &str,
    ) {
        let active = app.core.config.active_model_mut().unwrap();
        active.model_name = model.into();
        active.api_base = api_base.into();
        active.api_key_env = api_key_env.into();
        active.provider_type = provider_type.into();
    }

    fn user(text: &str) -> protocol::HistoryItem {
        protocol::HistoryItem::user(Content::text(text))
    }

    fn assistant(text: &str) -> protocol::HistoryItem {
        protocol::HistoryItem::Assistant(protocol::AssistantStep::terminal(
            Some(Content::text(text)),
            None,
            Vec::new(),
        ))
    }

    fn set_history(
        app: &mut crate::app::test_harness::TestApp,
        history: Vec<protocol::HistoryItem>,
    ) {
        app.app.conversation.replace_history_for_harness(history);
        app.app.restore_screen();
    }

    #[test]
    fn mode_change_during_turn_commits_when_history_reaches_next_request() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.start_turn(1);

        let note = app.mode_note("apply");
        app.app.set_mode(AgentMode::parse("apply").unwrap(), false);
        assert!(mode_blocks(&app.app).is_empty());

        app.feed_one(crate::event_source::SourceEvent::engine(
            protocol::EngineEvent::HistoryUpdated {
                turn_id: 1,
                update: protocol::CanonicalHistoryDelta::new(
                    0,
                    vec![protocol::HistoryItem::note(
                        protocol::HistoryNote::mode_change(note),
                    )],
                ),
            },
        ));

        assert_eq!(mode_blocks(&app.app), vec!["now in apply mode"]);
    }

    #[test]
    fn multiple_mode_changes_during_turn_commit_only_the_last_request_note() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.start_turn(1);

        app.app.set_mode(AgentMode::parse("apply").unwrap(), false);
        let note = app.mode_note("yolo");
        app.app.set_mode(AgentMode::parse("yolo").unwrap(), false);

        app.feed_one(crate::event_source::SourceEvent::engine(
            protocol::EngineEvent::HistoryUpdated {
                turn_id: 1,
                update: protocol::CanonicalHistoryDelta::new(
                    0,
                    vec![protocol::HistoryItem::note(
                        protocol::HistoryNote::mode_change(note),
                    )],
                ),
            },
        ));

        assert_eq!(mode_blocks(&app.app), vec!["now in yolo mode"]);
    }

    #[test]
    fn mode_pending_clears_when_cycling_back_to_applied_mode() {
        let mut app = normal_app();
        app.app
            .conversation
            .replace_history_for_harness(vec![user("hello")]);
        app.start_turn(1);

        app.app.set_mode(AgentMode::parse("apply").unwrap(), false);
        assert!(app.app.mode_pending());

        app.app.set_mode(AgentMode::parse("normal").unwrap(), false);
        assert!(!app.app.mode_pending());
    }

    #[test]
    fn recorded_mode_change_during_turn_sets_pending_marker() {
        let mut app = normal_app();
        app.app
            .conversation
            .replace_history_for_harness(vec![user("hello")]);
        app.start_turn(1);

        app.app.set_mode(AgentMode::parse("apply").unwrap(), true);

        assert!(app.app.mode_pending());
        assert_eq!(pending_mode_appends(&app.app).len(), 1);
    }

    #[test]
    fn recorded_mode_pending_clears_when_cycling_back_to_applied_mode() {
        let mut app = normal_app();
        app.app
            .conversation
            .replace_history_for_harness(vec![user("hello")]);
        app.start_turn(1);

        app.app.set_mode(AgentMode::parse("apply").unwrap(), true);
        assert!(app.app.mode_pending());

        app.app.set_mode(AgentMode::parse("normal").unwrap(), true);
        assert!(!app.app.mode_pending());
        assert!(pending_mode_appends(&app.app).is_empty());
    }

    #[test]
    fn recorded_mode_change_with_history_pushes_mode_block() {
        let mut app = normal_app();
        app.app
            .conversation
            .replace_history_for_harness(vec![user("hello")]);

        app.app.set_mode(AgentMode::parse("apply").unwrap(), true);

        assert_eq!(mode_blocks(&app.app), vec!["now in apply mode"]);
    }

    #[test]
    fn recorded_mode_change_without_another_turn_request_commits_at_turn_end() {
        let mut app = normal_app();
        app.start_turn(1);

        app.app.set_mode(AgentMode::parse("apply").unwrap(), true);
        app.app.discard_turn(crate::app::TurnEnd::Complete);

        assert_eq!(mode_blocks(&app.app), vec!["now in apply mode"]);
    }

    #[test]
    fn reasoning_pending_clears_when_cycling_back_to_applied_effort() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.core.config.reasoning_effort = protocol::ReasoningEffort::Off;
        app.app.sync_reasoning_effort_applied();
        app.start_turn(1);

        app.app
            .set_reasoning_effort(protocol::ReasoningEffort::High, false)
            .unwrap();
        assert!(app.app.reasoning_effort_pending());

        app.app
            .set_reasoning_effort(protocol::ReasoningEffort::Off, false)
            .unwrap();
        assert!(!app.app.reasoning_effort_pending());
    }

    #[test]
    fn restored_reasoning_is_reconciled_to_the_active_model() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let mut active = app
            .app
            .core
            .config
            .active_model()
            .expect("test app has an active model")
            .clone();
        active.catalog.default_reasoning_effort = Some(protocol::ReasoningEffort::Low);
        active.catalog.supported_reasoning_efforts = vec![
            protocol::ReasoningEffort::Low,
            protocol::ReasoningEffort::High,
        ];
        app.replace_active_model_for_harness(active);
        app.app.core.config.reasoning_effort = protocol::ReasoningEffort::High;
        let _ = app.drain_engine_sends();

        app.app
            .set_reasoning_effort(protocol::ReasoningEffort::Ultra, false)
            .unwrap();

        assert_eq!(
            app.app.core.config.reasoning_effort,
            protocol::ReasoningEffort::Low
        );
        let commands = app.drain_engine_sends();
        assert!(commands.iter().any(|command| matches!(
            command,
            protocol::UiCommand::SetReasoningEffort {
                effort: protocol::ReasoningEffort::Low,
            }
        )));
        assert!(commands.iter().all(|command| !matches!(
            command,
            protocol::UiCommand::SetReasoningEffort {
                effort: protocol::ReasoningEffort::Ultra,
            }
        )));
    }

    #[test]
    fn switching_back_to_context_token_identity_clears_stale_marker() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        set_active_model(
            &mut app.app,
            "old-model",
            "https://old.example",
            "OLD_KEY",
            "openai",
        );
        app.app.core.config.available_models = vec![
            smelt_core::config::ResolvedModel {
                key: "new/new-model".into(),
                provider_name: "new".into(),
                model_name: "new-model".into(),
                display_name: None,
                api_base: "https://new.example".into(),
                api_key_env: "NEW_KEY".into(),
                provider_type: "openai".into(),
                config: smelt_core::config::ModelConfig::default(),
                catalog: protocol::ModelCatalogMetadata::default(),
            },
            smelt_core::config::ResolvedModel {
                key: "old/old-model".into(),
                provider_name: "old".into(),
                model_name: "old-model".into(),
                display_name: None,
                api_base: "https://old.example".into(),
                api_key_env: "OLD_KEY".into(),
                provider_type: "openai".into(),
                config: smelt_core::config::ModelConfig::default(),
                catalog: protocol::ModelCatalogMetadata::default(),
            },
        ];
        let old_identity = app.app.active_context_token_identity();
        app.app
            .conversation
            .replace_history_for_harness(vec![user("hello")]);
        app.app
            .conversation
            .record_context_tokens_for_harness(100, old_identity);

        app.app.apply_model("new/new-model", false);
        assert!(app
            .app
            .conversation
            .session()
            .display_context_tokens_stale(&app.app.active_context_token_identity()));

        app.app.apply_model("old/old-model", false);
        assert!(!app
            .app
            .conversation
            .session()
            .display_context_tokens_stale(&app.app.active_context_token_identity()));
    }

    #[test]
    fn apply_model_clears_context_baseline_when_provider_identity_changes() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        set_active_model(
            &mut app.app,
            "same-model",
            "https://old.example",
            "OLD_KEY",
            "openai",
        );
        app.app.core.config.available_models = vec![smelt_core::config::ResolvedModel {
            key: "new/same-model".into(),
            provider_name: "new".into(),
            model_name: "same-model".into(),
            display_name: None,
            api_base: "https://new.example".into(),
            api_key_env: "NEW_KEY".into(),
            provider_type: "openai".into(),
            config: smelt_core::config::ModelConfig::default(),
            catalog: protocol::ModelCatalogMetadata::default(),
        }];
        let old_identity = app.app.active_context_token_identity();
        app.app
            .conversation
            .replace_history_for_harness(vec![user("hello")]);
        app.app
            .conversation
            .record_context_tokens_for_harness(100, old_identity);

        app.app.apply_model("new/same-model", false);

        assert!(app.app.conversation.session().context_tokens.is_none());
        assert_eq!(
            app.app.conversation.session().display_context_tokens(),
            Some(100)
        );
        assert!(app
            .app
            .conversation
            .session()
            .display_context_tokens_stale(&app.app.active_context_token_identity()));
    }

    #[test]
    fn returning_to_history_mode_removes_mode_change_note() {
        let mut app = normal_app();
        app.app
            .conversation
            .replace_history_for_harness(vec![user("hello")]);

        app.app.set_mode(AgentMode::parse("apply").unwrap(), false);
        assert_eq!(mode_blocks(&app.app), vec!["now in apply mode"]);

        app.app.set_mode(AgentMode::parse("normal").unwrap(), false);

        assert!(mode_blocks(&app.app).is_empty());
        assert!(app
            .app
            .conversation
            .session()
            .history
            .iter()
            .all(|item| item.note_kind() != Some(protocol::HistoryNoteKind::ModeChange)));
    }

    #[test]
    fn returning_to_history_mode_clears_pending_mode_change_during_turn() {
        let mut app = normal_app();
        app.app
            .conversation
            .replace_history_for_harness(vec![user("hello")]);
        app.start_turn(1);

        let before = app
            .app
            .conversation
            .pending_history_appends()
            .iter()
            .map(|append| append.history_item())
            .collect::<Vec<_>>();

        app.app.set_mode(AgentMode::parse("apply").unwrap(), false);
        let after_apply = app
            .app
            .conversation
            .pending_history_appends()
            .iter()
            .map(|append| append.history_item())
            .collect::<Vec<_>>();
        let expected_apply =
            protocol::HistoryItem::note(protocol::HistoryNote::mode_change_for_transition(
                "normal",
                "apply",
                app.mode_note("apply"),
            ));
        assert_eq!(&after_apply[..before.len()], before.as_slice());
        assert_eq!(&after_apply[before.len()..], [expected_apply]);
        assert_eq!(pending_mode_appends(&app.app).len(), 1);

        app.app.set_mode(AgentMode::parse("normal").unwrap(), false);

        let after_normal = app
            .app
            .conversation
            .pending_history_appends()
            .iter()
            .map(|append| append.history_item())
            .collect::<Vec<_>>();
        assert_eq!(after_normal, before);
        assert!(pending_mode_appends(&app.app).is_empty());
        assert!(mode_blocks(&app.app).is_empty());
    }

    #[test]
    fn mode_change_without_another_turn_request_commits_at_turn_end() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.start_turn(1);

        app.app.set_mode(AgentMode::parse("apply").unwrap(), false);
        app.app.discard_turn(crate::app::TurnEnd::Complete);

        assert_eq!(mode_blocks(&app.app), vec!["now in apply mode"]);
    }

    #[test]
    fn cancelled_turn_preserves_deferred_mode_change_for_replacement_turn() {
        let mut app = normal_app();
        set_history(&mut app, vec![user("hello")]);
        app.start_turn(1);

        app.app.set_mode(AgentMode::parse("apply").unwrap(), false);
        assert_eq!(app.app.conversation.pending_history_appends().len(), 1);

        app.app.discard_turn(crate::app::TurnEnd::Cancelled);
        assert_eq!(app.app.conversation.pending_history_appends().len(), 1);
        assert_eq!(
            app.app.conversation.pending_history_appends()[0].mode(),
            Some("apply")
        );

        app.app.apply_pending_history_appends_for_request();
        assert_eq!(
            app.app
                .conversation
                .session()
                .history
                .last()
                .and_then(protocol::HistoryItem::as_note)
                .and_then(protocol::HistoryNote::mode),
            Some("apply")
        );
        app.app.restore_screen();

        assert_eq!(mode_blocks(&app.app), vec!["now in apply mode"]);
        assert_eq!(
            app.app
                .conversation
                .session()
                .history
                .last()
                .and_then(protocol::HistoryItem::as_note)
                .and_then(protocol::HistoryNote::mode),
            Some("apply")
        );
    }

    #[test]
    fn mode_change_before_first_user_message_does_not_push_mode_block() {
        let mut app = crate::app::test_harness::TestApp::builder().build();

        app.app.set_mode(AgentMode::parse("apply").unwrap(), false);

        assert!(app.app.conversation.session().history.is_empty());
        assert!(mode_blocks(&app.app).is_empty());
    }

    #[test]
    fn mode_change_after_rewinding_to_first_turn_does_not_push_mode_block() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        set_history(
            &mut app,
            vec![
                protocol::HistoryItem::note(protocol::HistoryNote::context(
                    "Current working directory: /tmp.",
                )),
                user("first"),
                assistant("reply"),
            ],
        );
        let first_turn = app.app.rewind_turns().unwrap()[0].history_idx;

        app.app.rewind_to_history(first_turn);
        app.app.set_mode(AgentMode::parse("apply").unwrap(), false);

        assert!(mode_blocks(&app.app).is_empty());
        assert_eq!(
            app.app.conversation.session().history,
            vec![protocol::HistoryItem::note(protocol::HistoryNote::context(
                "Current working directory: /tmp.",
            ))]
        );
    }

    #[test]
    fn rewind_to_message_before_mode_change_restores_boundary_mode() {
        let mut app = normal_app();
        set_history(
            &mut app,
            vec![
                user("first"),
                assistant("first reply"),
                user("second"),
                assistant("second reply"),
            ],
        );
        app.app.set_mode(AgentMode::parse("apply").unwrap(), false);
        app.app.conversation.append_history_item(user("third"));
        app.app
            .conversation
            .append_history_item(assistant("third reply"));
        app.app.sync_session_snapshot();
        app.app.restore_screen();
        let second_turn = app.app.rewind_turns().unwrap()[1].history_idx;

        let restored = app.app.rewind_to_history(second_turn).expect("second turn");

        assert_eq!(restored.0, "second");
        assert_eq!(app.app.core.config.mode.as_str(), "normal");

        app.app.set_mode(AgentMode::parse("apply").unwrap(), false);

        assert_eq!(mode_blocks(&app.app), vec!["now in apply mode"]);
    }

    #[test]
    fn rewind_to_first_message_restores_session_base_mode() {
        let mut app = normal_app();
        set_history(&mut app, vec![user("first"), assistant("first reply")]);
        app.app.set_mode(AgentMode::parse("apply").unwrap(), false);
        app.app
            .conversation
            .set_session_mode_for_harness(Some("apply".into()));
        app.app.conversation.append_history_item(user("second"));
        app.app
            .conversation
            .append_history_item(assistant("second reply"));
        app.app.sync_session_snapshot();
        app.app.restore_screen();
        let first_turn = app.app.rewind_turns().unwrap()[0].history_idx;

        let restored = app.app.rewind_to_history(first_turn).expect("first turn");

        assert_eq!(restored.0, "first");
        assert_eq!(app.app.core.config.mode.as_str(), "normal");
    }

    #[test]
    fn returning_to_mode_note_base_after_rewind_removes_mode_block() {
        let mut app = normal_app();
        set_history(&mut app, vec![user("first"), assistant("first reply")]);
        app.app.set_mode(AgentMode::parse("apply").unwrap(), false);
        app.app.conversation.append_history_item(user("second"));
        app.app
            .conversation
            .append_history_item(assistant("second reply"));
        app.app.sync_session_snapshot();
        app.app.restore_screen();
        let second_turn = app.app.rewind_turns().unwrap()[1].history_idx;

        app.app.rewind_to_history(second_turn);
        app.app.set_mode(AgentMode::parse("normal").unwrap(), false);

        assert!(mode_blocks(&app.app).is_empty());
        assert!(app
            .app
            .conversation
            .session()
            .history
            .iter()
            .all(|item| item.note_kind() != Some(protocol::HistoryNoteKind::ModeChange)));
    }

    #[tokio::test]
    async fn shell_escape_uses_cwd_captured_at_submission() {
        let environment_guard = crate::app::test_harness::test_environment_guard();
        let shell_cwd = tempfile::TempDir::new().expect("shell cwd");
        let later_cwd = tempfile::TempDir::new().expect("later cwd");
        let shell_cwd = std::fs::canonicalize(shell_cwd.path()).expect("canonical shell cwd");
        let later_cwd = std::fs::canonicalize(later_cwd.path()).expect("canonical later cwd");

        let mut app = crate::app::test_harness::TestApp::builder()
            .with_cwd(&shell_cwd)
            .build_with_test_environment_guard(&environment_guard);
        let mut handle = app
            .app
            .start_shell_escape("pwd")
            .expect("shell escape starts");
        environment_guard
            .set_current_dir(&later_cwd)
            .expect("move process cwd after submission");

        let mut output = Vec::new();
        while let Some(event) = handle.rx.recv().await {
            match event {
                ExecEvent::Output(line) => output.push(line),
                ExecEvent::Done {
                    output: final_output,
                    exit_code,
                    termination,
                } => {
                    assert_eq!(exit_code, Some(0));
                    assert_eq!(termination, Some(protocol::JobTermination::Exited));
                    assert_eq!(final_output, shell_cwd.to_string_lossy());
                    break;
                }
            }
        }
        assert_eq!(output, vec![shell_cwd.to_string_lossy().into_owned()]);
    }

    #[tokio::test]
    async fn shell_escape_preserves_unicode_and_replaces_invalid_utf8() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let mut handle = app
            .app
            .start_shell_escape("printf 'besta\\314\\210tigt\\n\\377x\\n'")
            .expect("shell escape starts");

        let mut output = Vec::new();
        while let Some(event) = handle.rx.recv().await {
            match event {
                ExecEvent::Output(line) => output.push(line),
                ExecEvent::Done {
                    output: final_output,
                    exit_code,
                    termination,
                } => {
                    assert_eq!(exit_code, Some(0));
                    assert_eq!(termination, Some(protocol::JobTermination::Exited));
                    assert_eq!(final_output, "besta\u{308}tigt\n\u{fffd}x");
                    break;
                }
            }
        }
        assert_eq!(output, vec!["besta\u{308}tigt", "\u{fffd}x"]);
    }

    #[test]
    fn empty_or_whitespace_lines_parse_as_empty() {
        assert_eq!(parse_command_line(""), ParsedCommand::Empty);
        assert_eq!(parse_command_line("   "), ParsedCommand::Empty);
        assert_eq!(parse_command_line("\n\t  \n"), ParsedCommand::Empty);
    }

    #[test]
    fn bang_prefix_parses_as_shell_escape() {
        assert_eq!(
            parse_command_line("!echo hi"),
            shell("echo hi", ShellSink::Transcript)
        );
    }

    #[test]
    fn shell_escape_trims_leading_whitespace_after_bang() {
        // `! echo hi` and `!echo hi` produce the same script.
        assert_eq!(
            parse_command_line("!   echo hi"),
            shell("echo hi", ShellSink::Transcript)
        );
    }

    #[test]
    fn shell_escape_keeps_an_empty_script_for_bare_bang() {
        assert_eq!(parse_command_line("!"), shell("", ShellSink::Transcript));
    }

    #[test]
    fn slash_prefix_extracts_name_with_no_arg() {
        assert_eq!(parse_command_line("/quit"), slash("quit", None));
    }

    #[test]
    fn slash_with_argument_splits_on_first_whitespace() {
        assert_eq!(
            parse_command_line("/model claude-haiku"),
            slash("model", Some("claude-haiku"))
        );
    }

    #[test]
    fn command_parser_never_splits_whitespace_graphemes() {
        assert_eq!(
            parse_command_line("/name\u{600}  arg"),
            slash("name\u{600} ", Some("arg"))
        );
        assert_eq!(
            parse_command_line("/name  \u{301}arg"),
            slash("name", Some(" \u{301}arg"))
        );
        assert_eq!(
            parse_command_line(" \u{301}!echo"),
            ParsedCommand::Bare {
                text: " \u{301}!echo"
            }
        );
    }

    #[test]
    fn slash_with_multiline_argument_preserves_body() {
        assert_eq!(
            parse_command_line("/btw first line\nsecond line"),
            slash("btw", Some("first line\nsecond line"))
        );
    }

    #[test]
    fn slash_collapses_extra_whitespace_in_argument() {
        // Extra interior whitespace inside the arg is preserved; only the
        // boundary between name and arg, plus trailing whitespace, is trimmed.
        assert_eq!(
            parse_command_line("/cmd   a    b  "),
            slash("cmd", Some("a    b"))
        );
    }

    #[test]
    fn colon_parses_as_ex_command() {
        assert_eq!(parse_command_line(":quit"), ex("quit", false, None));
        assert_eq!(parse_command_line(":q!"), ex("q", true, None));
        assert_eq!(
            parse_command_line(":model x"),
            ex("model", false, Some("x"))
        );
    }

    #[test]
    fn bare_name_without_a_sigil_parses_as_bare_text() {
        // Without a leading `/` or `:`, the line is plain text headed for
        // the agent - even if its first word matches a registered command.
        assert_eq!(
            parse_command_line("foo bar"),
            ParsedCommand::Bare { text: "foo bar" }
        );
        assert_eq!(
            parse_command_line("foo"),
            ParsedCommand::Bare { text: "foo" }
        );
        assert_eq!(
            parse_command_line("  btw what's up  "),
            ParsedCommand::Bare {
                text: "btw what's up"
            }
        );
    }

    #[test]
    fn slash_with_only_trailing_whitespace_has_no_arg() {
        // `"/foo   "` is the same as `"/foo"` once trimmed.
        assert_eq!(parse_command_line("/foo   "), slash("foo", None));
    }

    #[test]
    fn shell_takes_priority_over_slash_when_both_prefixes_present() {
        // The leading char wins - `!/foo` is a shell escape for `/foo`.
        assert_eq!(
            parse_command_line("!/foo bar"),
            shell("/foo bar", ShellSink::Transcript)
        );
    }

    #[test]
    fn outer_whitespace_does_not_change_the_parse() {
        assert_eq!(
            parse_command_line("   /foo bar   "),
            slash("foo", Some("bar"))
        );
        assert_eq!(
            parse_command_line("  !echo hi  "),
            shell("echo hi", ShellSink::Transcript)
        );
    }

    #[test]
    fn ex_bang_parses_as_overlay_shell() {
        assert_eq!(
            parse_command_line(":!echo hi"),
            shell("echo hi", ShellSink::Overlay)
        );
    }

    #[test]
    fn prompt_colon_commands_are_only_quit_aliases() {
        assert_eq!(
            parse_for_context(":help", CommandContext::prompt()),
            ParsedCommand::Bare { text: ":help" }
        );
        assert_eq!(
            parse_for_context(":q", CommandContext::prompt()),
            ex("q", false, None)
        );
        assert_eq!(
            parse_for_context(":wq", CommandContext::prompt()),
            ex("wq", false, None)
        );
    }

    #[test]
    fn cmdline_colon_commands_use_command_namespace() {
        assert_eq!(
            parse_for_context(":help", CommandContext::cmdline()),
            ex("help", false, None)
        );
    }

    #[test]
    fn slash_commands_remain_prompt_commands() {
        assert_eq!(
            parse_for_context("/help", CommandContext::prompt()),
            slash("help", None)
        );
    }
}
