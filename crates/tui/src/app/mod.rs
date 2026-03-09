mod agent;
mod commands;
mod events;
mod history;

use crate::input::{resolve_agent_esc, Action, EscAction, History, InputState, MenuResult};
use crate::render::{
    tool_arg_summary, Block, ConfirmChoice, ConfirmDialog, Dialog as _, FramePrompt,
    QuestionDialog, ResumeEntry, Screen, ToolOutput, ToolStatus,
};
use crate::session::Session;
use crate::{render, session, state, vim};
use engine::{EngineHandle, Permissions};
use protocol::{Content, EngineEvent, Message, Mode, ReasoningEffort, Role, UiCommand};

use crossterm::{
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, EventStream, KeyCode, KeyEvent,
        KeyModifiers,
    },
    terminal, ExecutableCommand,
};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ── App ──────────────────────────────────────────────────────────────────────

pub struct App {
    pub model: String,
    pub api_base: String,
    pub api_key_env: String,
    pub reasoning_effort: ReasoningEffort,
    pub mode: Mode,
    pub screen: Screen,
    pub history: Vec<Message>,
    pub input_history: History,
    pub input: InputState,
    pub queued_messages: Vec<String>,
    pub auto_approved: HashMap<String, Vec<glob::Pattern>>,
    pub session: session::Session,
    pub shared_session: Arc<Mutex<Option<Session>>>,
    pub context_window: Option<u32>,
    pub auto_compact: bool,
    pub show_speed: bool,
    pub restrict_to_workspace: bool,
    pub available_models: Vec<crate::config::ResolvedModel>,
    pub engine: EngineHandle,
    pending_title: bool,
    last_width: u16,
    last_height: u16,
    permissions: Permissions,
    next_turn_id: u64,
}

struct TurnState {
    turn_id: u64,
    pending: Option<PendingTool>,
    steered_count: usize,
    _perf: Option<crate::perf::Guard>,
}

enum EventOutcome {
    Noop,
    Redraw,
    Quit,
    CancelAgent,
    CancelAndClear,
    Submit { content: Content, display: String },
    MenuResult(MenuResult),
    OpenDialog(Box<dyn render::Dialog>),
}

enum CommandAction {
    Continue,
    Quit,
    CancelAndClear,
    Compact,
    OpenDialog(Box<dyn render::Dialog>),
}

/// Arrange flat session entries into a tree: roots first (sorted by
/// updated_at descending), each followed by its forks (also sorted).
fn build_session_tree(mut flat: Vec<ResumeEntry>) -> Vec<ResumeEntry> {
    use std::collections::HashMap;

    // Index children by parent_id.
    let mut children: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, entry) in flat.iter().enumerate() {
        if let Some(ref pid) = entry.parent_id {
            children.entry(pid.clone()).or_default().push(i);
        }
    }

    // Collect root indices (no parent, or parent doesn't exist in the set).
    let ids: std::collections::HashSet<&str> = flat.iter().map(|e| e.id.as_str()).collect();
    let root_indices: Vec<usize> = flat
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            e.parent_id
                .as_ref()
                .is_none_or(|pid| !ids.contains(pid.as_str()))
        })
        .map(|(i, _)| i)
        .collect();

    // Recursively emit entries with depth.
    let mut result = Vec::with_capacity(flat.len());
    fn emit(
        idx: usize,
        depth: usize,
        flat: &mut Vec<ResumeEntry>,
        children: &HashMap<String, Vec<usize>>,
        result: &mut Vec<ResumeEntry>,
    ) {
        let mut entry = flat[idx].clone();
        entry.depth = depth;
        let id = entry.id.clone();
        result.push(entry);
        if let Some(child_indices) = children.get(&id) {
            let mut sorted: Vec<usize> = child_indices.clone();
            sorted.sort_by(|a, b| {
                let ta = flat[*b].updated_at_ms;
                let tb = flat[*a].updated_at_ms;
                ta.cmp(&tb)
            });
            for ci in sorted {
                emit(ci, depth + 1, flat, children, result);
            }
        }
    }

    for ri in root_indices {
        emit(ri, 0, &mut flat, &children, &mut result);
    }

    result
}

/// Check whether a command is allowed while the agent is running.
/// Returns `Err(reason)` for commands that are blocked.
fn is_allowed_while_running(input: &str) -> Result<(), String> {
    match input {
        "/compact" => Err("cannot compact while agent is working".into()),
        "/resume" => Err("cannot resume while agent is working".into()),
        "/fork" => Err("cannot fork while agent is working".into()),
        "/settings" => Err("cannot open settings while agent is working".into()),
        "/model" => Err("cannot switch model while agent is working".into()),
        _ => Ok(()),
    }
}

/// Classify input received as a CLI startup argument.
/// Returns `None` if it's a normal message that should go to the agent.
fn classify_startup_command(input: &str) -> Option<&'static str> {
    if input.starts_with('!') {
        return None; // handled separately (execute shell)
    }
    if !input.starts_with('/') || !crate::completer::Completer::is_command(input) {
        return None; // normal message
    }
    match input {
        "/resume" | "/settings" => None, // open their respective UI
        _ => Some("has no effect as a startup argument"),
    }
}

enum InputOutcome {
    Continue,
    StartAgent,
    Compact,
    Quit,
    OpenDialog(Box<dyn render::Dialog>),
}

/// Mutable timer state shared across event handlers.
struct Timers {
    last_esc: Option<Instant>,
    esc_vim_mode: Option<vim::ViMode>,
    last_ctrlc: Option<Instant>,
    last_keypress: Option<Instant>,
}

/// How long after the last keypress before we show a deferred permission dialog.
const CONFIRM_DEFER_MS: u64 = 1500;

/// A permission dialog deferred because the user was actively typing.
enum DeferredDialog {
    Confirm {
        tool_name: String,
        desc: String,
        args: HashMap<String, serde_json::Value>,
        approval_pattern: Option<String>,
        summary: Option<String>,
        request_id: u64,
    },
    AskQuestion {
        args: HashMap<String, serde_json::Value>,
        request_id: u64,
    },
}

// ── Supporting types ─────────────────────────────────────────────────────────

pub enum SessionControl {
    Continue,
    NeedsConfirm {
        tool_name: String,
        desc: String,
        args: HashMap<String, serde_json::Value>,
        approval_pattern: Option<String>,
        summary: Option<String>,
        request_id: u64,
    },
    NeedsAskQuestion {
        args: HashMap<String, serde_json::Value>,
        request_id: u64,
    },
    Done,
}

enum LoopAction {
    Continue,
    Done,
}

pub struct PendingTool {
    pub name: String,
}

// ── App impl ─────────────────────────────────────────────────────────────────

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        model: String,
        api_base: String,
        api_key_env: String,
        engine: EngineHandle,
        vim_from_config: bool,
        auto_compact: bool,
        show_speed: bool,
        restrict_to_workspace: bool,
        reasoning_effort: protocol::ReasoningEffort,
        shared_session: Arc<Mutex<Option<Session>>>,
        available_models: Vec<crate::config::ResolvedModel>,
    ) -> Self {
        let saved = state::State::load();
        let mode = saved.mode();
        let vim_enabled = saved.vim_enabled() || vim_from_config;
        let mut input = InputState::new();
        if vim_enabled {
            input.set_vim_enabled(true);
        }
        // Only load accent from state if not already set from config
        if crate::theme::accent_value() == crate::theme::DEFAULT_ACCENT {
            if let Some(accent) = saved.accent_color {
                crate::theme::set_accent(accent);
            }
        }
        // Use saved reasoning effort if not set from config
        let reasoning_effort = if reasoning_effort == protocol::ReasoningEffort::Off
            && saved.reasoning_effort != protocol::ReasoningEffort::Off
        {
            saved.reasoning_effort
        } else {
            reasoning_effort
        };
        let mut screen = Screen::new();
        screen.set_model_label(model.clone());
        screen.set_reasoning_effort(reasoning_effort);
        screen.set_show_speed(show_speed);
        Self {
            model,
            api_base,
            api_key_env,
            reasoning_effort,
            mode,
            screen,
            history: Vec::new(),
            input_history: History::load(),
            input,
            queued_messages: Vec::new(),
            auto_approved: HashMap::new(),
            session: session::Session::new(),
            shared_session,
            context_window: None,
            auto_compact,
            show_speed,
            restrict_to_workspace,
            available_models,
            engine,
            pending_title: false,
            last_width: terminal::size().map(|(w, _)| w).unwrap_or(80),
            last_height: terminal::size().map(|(_, h)| h).unwrap_or(24),
            permissions: {
                let mut p = Permissions::load();
                p.set_restrict_to_workspace(restrict_to_workspace);
                if let Ok(cwd) = std::env::current_dir() {
                    p.set_workspace(cwd);
                }
                p
            },
            next_turn_id: 1,
        }
    }

    // ── Unified event loop ───────────────────────────────────────────────

    pub async fn run(
        &mut self,
        mut ctx_rx: Option<tokio::sync::oneshot::Receiver<Option<u32>>>,
        initial_message: Option<String>,
    ) {
        terminal::enable_raw_mode().ok();
        let _ = io::stdout().execute(EnableBracketedPaste);

        if !self.history.is_empty() {
            self.rebuild_screen_from_history();
            if let Some(tokens) = self.session.context_tokens {
                self.screen.set_context_tokens(tokens);
            }
            self.screen.flush_blocks();
        }
        self.screen
            .draw_prompt(&self.input, self.mode, render::term_width());

        let mut term_events = EventStream::new();
        let mut agent: Option<TurnState> = None;

        let mut active_dialog: Option<Box<dyn render::Dialog>> = None;

        // Auto-submit initial message if provided (e.g. `agent "fix the bug"`).
        if let Some(msg) = initial_message {
            let trimmed = msg.trim();
            if let Some(cmd) = trimmed.strip_prefix('!') {
                self.run_shell_escape(cmd);
            } else if trimmed == "/resume" {
                if let CommandAction::OpenDialog(dlg) = self.handle_command(trimmed) {
                    active_dialog = Some(dlg);
                }
            } else if trimmed == "/settings" {
                self.input.open_settings(
                    self.input.vim_enabled(),
                    self.auto_compact,
                    self.show_speed,
                    self.restrict_to_workspace,
                );
                self.screen.mark_dirty();
            } else if let Some(reason) = classify_startup_command(trimmed) {
                self.screen.push(Block::Error {
                    message: format!("\"{}\" {}", trimmed, reason),
                });
                self.screen.flush_blocks();
            } else {
                self.screen.erase_prompt();
                let content = Content::text(msg.clone());
                agent = Some(self.begin_agent_turn(&msg, content));
            }
        }

        let mut t = Timers {
            last_esc: None,
            esc_vim_mode: None,
            last_ctrlc: None,
            last_keypress: None,
        };
        let mut deferred_dialog: Option<DeferredDialog> = None;

        'main: loop {
            // ── Background polls ─────────────────────────────────────────
            if let Some(ref mut rx) = ctx_rx {
                if let Ok(result) = rx.try_recv() {
                    self.context_window = result;
                    ctx_rx = None;
                }
            }

            // ── Drain engine events (paused only for Confirm/AskQuestion) ──
            if !active_dialog.as_ref().is_some_and(|d| d.blocks_agent()) {
                loop {
                    let ev = match self.engine.try_recv() {
                        Ok(ev) => ev,
                        Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                        Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                            engine::log::entry(
                                engine::log::Level::Warn,
                                "engine_stop",
                                &serde_json::json!({
                                    "reason": "channel_disconnected",
                                    "source": "try_recv_drain",
                                }),
                            );
                            if agent.is_some() {
                                self.finish_turn(false);
                                agent = None;
                            }
                            break;
                        }
                    };
                    let action = if let Some(ref mut ag) = agent {
                        let ctrl = self.handle_engine_event(
                            ev,
                            ag.turn_id,
                            &mut ag.pending,
                            &mut ag.steered_count,
                        );
                        self.dispatch_control(
                            ctrl,
                            &mut ag.pending,
                            &mut deferred_dialog,
                            &mut active_dialog,
                            t.last_keypress,
                        )
                    } else {
                        // No active turn — handle out-of-band events.
                        self.handle_engine_event_idle(ev);
                        LoopAction::Continue
                    };
                    match action {
                        LoopAction::Continue => {}
                        LoopAction::Done => {
                            self.finish_turn(false);
                            agent = None;
                            break;
                        }
                    }
                }
            }

            // ── Sync steering ────────────────────────────────────────────
            if let Some(ref mut ag) = agent {
                if self.queued_messages.len() > ag.steered_count {
                    for msg in &self.queued_messages[ag.steered_count..] {
                        self.engine.send(UiCommand::Steer { text: msg.clone() });
                    }
                    ag.steered_count = self.queued_messages.len();
                }
            }

            // ── Auto-start from leftover queued messages ─────────────────
            if agent.is_none() && !self.queued_messages.is_empty() {
                let text = std::mem::take(&mut self.queued_messages).join("\n");
                if !text.is_empty() {
                    self.screen.erase_prompt();
                    match self.process_input(&text) {
                        InputOutcome::StartAgent => {
                            let content = Content::text(text.clone());
                            agent = Some(self.begin_agent_turn(&text, content));
                        }
                        InputOutcome::Compact => {
                            if self.history.is_empty() {
                                self.screen.push(Block::Error {
                                    message: "nothing to compact".into(),
                                });
                                self.screen.flush_blocks();
                            } else {
                                self.compact_history();
                            }
                        }
                        InputOutcome::Continue | InputOutcome::Quit => {}
                        InputOutcome::OpenDialog(dlg) => {
                            active_dialog = Some(dlg);
                        }
                    }
                }
            }

            // ── Show deferred dialog once user stops typing ──────────────
            // If agent was cancelled while a dialog was deferred, discard it.
            if agent.is_none() && deferred_dialog.is_some() {
                deferred_dialog.take();
                self.screen.set_pending_dialog(false);
            }
            if deferred_dialog.is_some() && active_dialog.is_none() && agent.is_some() {
                // Try to auto-approve deferred confirms using full permission check.
                if let Some(DeferredDialog::Confirm {
                    ref tool_name,
                    ref args,
                    request_id,
                    ..
                }) = deferred_dialog
                {
                    let decision = self.permissions.decide(self.mode, tool_name, args);
                    if decision == engine::permissions::Decision::Allow {
                        self.screen.set_pending_dialog(false);
                        self.engine.send(UiCommand::PermissionDecision {
                            request_id,
                            approved: true,
                            message: None,
                        });
                        deferred_dialog.take();
                    }
                }

                let idle = t
                    .last_keypress
                    .map(|lk| lk.elapsed() >= Duration::from_millis(CONFIRM_DEFER_MS))
                    .unwrap_or(true);
                if idle && deferred_dialog.is_some() {
                    self.screen.set_pending_dialog(false);
                    let deferred = deferred_dialog.take().unwrap();
                    match deferred {
                        DeferredDialog::Confirm {
                            tool_name,
                            desc,
                            args,
                            approval_pattern,
                            summary,
                            request_id,
                        } => {
                            // Re-check permissions with current mode (includes workspace).
                            let decision = self.permissions.decide(self.mode, &tool_name, &args);
                            if decision == engine::permissions::Decision::Allow {
                                self.engine.send(UiCommand::PermissionDecision {
                                    request_id,
                                    approved: true,
                                    message: None,
                                });
                            } else {
                                self.screen.set_active_status(ToolStatus::Confirm);
                                let dialog = Box::new(ConfirmDialog::new(
                                    &tool_name,
                                    &desc,
                                    &args,
                                    approval_pattern.as_deref(),
                                    summary.as_deref(),
                                    request_id,
                                ));
                                self.open_blocking_dialog(dialog, &mut active_dialog);
                            }
                        }
                        DeferredDialog::AskQuestion { args, request_id } => {
                            self.screen.set_active_status(ToolStatus::Confirm);
                            let questions = render::parse_questions(&args);
                            let dialog = Box::new(QuestionDialog::new(questions, request_id));
                            self.open_blocking_dialog(dialog, &mut active_dialog);
                        }
                    }
                }
            }

            // ── Render ───────────────────────────────────────────────────
            let redirtied = self.tick(agent.is_some(), active_dialog.is_some());
            if let Some(d) = active_dialog.as_mut() {
                if redirtied {
                    d.mark_dirty();
                }
                d.draw(self.screen.dialog_row());
            }

            // ── Wait for next event ──────────────────────────────────────
            tokio::select! {
                biased;

                Some(Ok(ev)) = term_events.next() => {
                    if self.dispatch_terminal_event(
                        ev, &mut agent, &mut t, &mut active_dialog,
                    ) {
                        break 'main;
                    }

                    // Drain buffered terminal events
                    while event::poll(Duration::ZERO).unwrap_or(false) {
                        if let Ok(ev) = event::read() {
                            if self.dispatch_terminal_event(
                                ev, &mut agent, &mut t, &mut active_dialog,
                            ) {
                                break 'main;
                            }
                        }
                    }

                    // If mode changed, re-check deferred dialog with full permissions.
                    if let Some(DeferredDialog::Confirm { ref tool_name, ref args, request_id, .. }) = deferred_dialog {
                        let decision = self.permissions.decide(self.mode, tool_name, args);
                        if decision == engine::permissions::Decision::Allow {
                            self.screen.set_pending_dialog(false);
                            self.engine.send(UiCommand::PermissionDecision {
                                request_id,
                                approved: true,
                                message: None,
                            });
                            deferred_dialog.take();
                        }
                    }

                    // Render immediately after terminal events for responsive typing.
                    let redirtied = self.tick(agent.is_some(), active_dialog.is_some());
                    if let Some(d) = active_dialog.as_mut() {
                        if redirtied { d.mark_dirty(); }
                        d.draw(self.screen.dialog_row());
                    }
                }

                Some(ev) = self.engine.recv(), if !active_dialog.as_ref().is_some_and(|d| d.blocks_agent()) => {
                    if let Some(ref mut ag) = agent {
                        let ctrl = self.handle_engine_event(ev, ag.turn_id, &mut ag.pending, &mut ag.steered_count);
                        let action = self.dispatch_control(
                            ctrl,
                            &mut ag.pending,
                            &mut deferred_dialog,
                            &mut active_dialog,
                            t.last_keypress,
                        );
                        match action {
                            LoopAction::Continue => {}
                            LoopAction::Done => {
                                self.finish_turn(false);
                                agent = None;
                            }
                        }
                    } else {
                        // No active turn — handle out-of-band events.
                        self.handle_engine_event_idle(ev);
                    }
                    let redirtied = self.tick(agent.is_some(), active_dialog.is_some());
                    if let Some(d) = active_dialog.as_mut() {
                        if redirtied { d.mark_dirty(); }
                        d.draw(self.screen.dialog_row());
                    }
                }

                _ = tokio::time::sleep(Duration::from_millis(80)) => {
                    // Timer tick for spinner animation.
                    // Mark dialog dirty so elapsed timers update live (e.g. PsDialog).
                    if let Some(d) = active_dialog.as_mut() {
                        d.mark_dirty();
                    }
                }
            }
        }

        // Cleanup
        if agent.is_some() {
            self.finish_turn(true);
        }
        self.save_session();

        self.screen.move_cursor_past_prompt();
        let _ = io::stdout().execute(DisableBracketedPaste);
        terminal::disable_raw_mode().ok();
    }

    // ── Headless mode ─────────────────────────────────────────────────────

    /// Run a single message through the agent without any TUI.
    /// Prints the agent's text output to stdout.
    pub async fn run_headless(&mut self, message: String) {
        use std::io::Write;

        let trimmed = message.trim();

        // Shell escape: execute and print output.
        if let Some(cmd) = trimmed.strip_prefix('!') {
            let cmd = cmd.trim();
            if !cmd.is_empty() {
                let output = std::process::Command::new("sh").arg("-c").arg(cmd).output();
                match output {
                    Ok(o) => {
                        let _ = io::stdout().write_all(&o.stdout);
                        let _ = io::stderr().write_all(&o.stderr);
                    }
                    Err(e) => eprintln!("error: {e}"),
                }
            }
            return;
        }

        // Slash commands require interactive mode.
        if trimmed.starts_with('/') && crate::completer::Completer::is_command(trimmed) {
            eprintln!("\"{}\" requires interactive mode", trimmed);
            std::process::exit(1);
        }

        if self.session.first_user_message.is_none() {
            self.session.first_user_message = Some(message.clone());
        }

        let turn_id = self.next_turn_id;
        self.next_turn_id += 1;

        self.engine.send(UiCommand::StartTurn {
            turn_id,
            input: message,
            mode: self.mode,
            model: self.model.clone(),
            reasoning_effort: self.reasoning_effort,
            history: self.history.clone(),
            api_base: Some(self.api_base.clone()),
            api_key: Some(std::env::var(&self.api_key_env).unwrap_or_default()),
        });

        // Drain events, printing text to stdout.
        while let Some(ev) = self.engine.recv().await {
            match ev {
                EngineEvent::Thinking { content } => {
                    eprintln!("[thinking] {content}");
                }
                EngineEvent::Text { content } => {
                    print!("{content}");
                    let _ = io::stdout().flush();
                }
                EngineEvent::ToolStarted {
                    tool_name, summary, ..
                } => {
                    eprintln!("[tool: {tool_name}] {summary}");
                }
                EngineEvent::ToolFinished { result, .. } => {
                    if result.is_error {
                        eprintln!("[tool error] {}", result.content);
                    }
                }
                EngineEvent::RequestPermission { request_id, .. } => {
                    self.engine.send(UiCommand::PermissionDecision {
                        request_id,
                        approved: false,
                        message: None,
                    });
                }
                EngineEvent::RequestAnswer { request_id, .. } => {
                    self.engine.send(UiCommand::QuestionAnswer {
                        request_id,
                        answer: Some("User is not available (headless mode).".into()),
                    });
                }
                EngineEvent::Messages { messages, .. } => {
                    self.history = messages;
                }
                EngineEvent::TurnError { message } => {
                    eprintln!("[error] {message}");
                }
                EngineEvent::TurnComplete { messages, .. } => {
                    self.history = messages;
                    break;
                }
                _ => {}
            }
        }

        self.save_session();

        // Ensure output ends with a newline.
        println!();
    }

    fn open_blocking_dialog(
        &mut self,
        dialog: Box<dyn render::Dialog>,
        active_dialog: &mut Option<Box<dyn render::Dialog>>,
    ) {
        let height = crossterm::terminal::size().map(|(_, h)| h).unwrap_or(24);
        let fits = self.screen.active_tool_rows() + dialog.height() <= height;
        if !fits {
            self.screen.erase_prompt();
        }
        self.screen.set_show_tool_in_dialog(fits);
        *active_dialog = Some(dialog);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── is_allowed_while_running ─────────────────────────────────────

    #[test]
    fn running_allowed_commands() {
        assert!(is_allowed_while_running("/vim").is_ok());
        assert!(is_allowed_while_running("/export").is_ok());
        assert!(is_allowed_while_running("/ps").is_ok());
        assert!(is_allowed_while_running("/exit").is_ok());
        assert!(is_allowed_while_running("/quit").is_ok());
        assert!(is_allowed_while_running("/clear").is_ok());
        assert!(is_allowed_while_running("!ls").is_ok());
    }

    #[test]
    fn running_blocked_commands() {
        assert!(is_allowed_while_running("/compact").is_err());
        assert!(is_allowed_while_running("/resume").is_err());
        assert!(is_allowed_while_running("/settings").is_err());
        assert!(is_allowed_while_running("/model").is_err());
    }

    // ── classify_startup_command ──────────────────────────────────────

    #[test]
    fn startup_normal_message_is_none() {
        assert!(classify_startup_command("fix the bug").is_none());
    }

    #[test]
    fn startup_shell_escape_is_none() {
        assert!(classify_startup_command("!ls -la").is_none());
    }

    #[test]
    fn startup_resume_is_none() {
        // /resume opens its UI, not blocked
        assert!(classify_startup_command("/resume").is_none());
    }

    #[test]
    fn startup_settings_is_none() {
        // /settings opens its UI, not blocked
        assert!(classify_startup_command("/settings").is_none());
    }

    #[test]
    fn startup_vim_is_blocked() {
        assert!(classify_startup_command("/vim").is_some());
    }

    #[test]
    fn startup_exit_is_blocked() {
        assert!(classify_startup_command("/exit").is_some());
    }

    #[test]
    fn startup_compact_is_blocked() {
        assert!(classify_startup_command("/compact").is_some());
    }

    #[test]
    fn startup_clear_is_blocked() {
        assert!(classify_startup_command("/clear").is_some());
    }

    #[test]
    fn startup_unknown_slash_not_a_command() {
        // Not a recognized command — should pass through as a message
        assert!(classify_startup_command("/unknown").is_none());
    }
}
