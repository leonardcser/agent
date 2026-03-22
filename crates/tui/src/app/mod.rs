mod agent;
mod commands;
mod events;
mod history;

use crate::input::{
    resolve_agent_esc, Action, EscAction, History, InputState, MenuKind, MenuResult,
};
use crate::render::{
    tool_arg_summary, ApprovalScope, Block, ConfirmChoice, ConfirmDialog, ConfirmRequest,
    Dialog as _, FramePrompt, QuestionDialog, ResumeEntry, Screen, ToolOutput, ToolStatus,
};
use crate::session::Session;
use crate::{render, session, state, vim};
use engine::{permissions::Decision, EngineHandle, Permissions};
use protocol::{Content, EngineEvent, Message, Mode, ReasoningEffort, Role, UiCommand};

use crossterm::{
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, EventStream, KeyCode, KeyEvent,
        KeyModifiers,
    },
    terminal, ExecutableCommand,
};
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
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
    exec_rx: Option<tokio::sync::mpsc::UnboundedReceiver<commands::ExecEvent>>,
    exec_kill: Option<std::sync::Arc<tokio::sync::Notify>>,
    pub queued_messages: Vec<String>,
    /// Session-scoped auto-approvals (cleared on /clear, /new, rewind).
    pub session_approved: HashMap<String, Vec<glob::Pattern>>,
    pub session_approved_dirs: Vec<PathBuf>,
    /// Workspace-persisted approvals (survive across sessions).
    pub workspace_approved: HashMap<String, Vec<glob::Pattern>>,
    pub workspace_approved_dirs: Vec<PathBuf>,
    /// Workspace rules as loaded from disk (source of truth for persistence).
    pub workspace_rules: Vec<crate::workspace_permissions::Rule>,
    /// Current working directory (cached at startup).
    cwd: String,
    /// Directories outside the workspace that have appeared in confirm dialogs.
    pub seen_outside_dirs: HashSet<PathBuf>,
    pub session: session::Session,
    pub shared_session: Arc<Mutex<Option<Session>>>,
    pub context_window: Option<u32>,
    pub auto_compact: bool,
    pub show_speed: bool,
    pub show_prediction: bool,
    pub show_slug: bool,
    pub restrict_to_workspace: bool,
    pub available_models: Vec<crate::config::ResolvedModel>,
    pub engine: EngineHandle,
    permissions: Arc<Permissions>,
    /// Context for the currently-open confirm dialog, used to re-check
    /// permissions when the user toggles mode.
    confirm_context: Option<ConfirmContext>,
    /// Ghost text prediction for the input field.
    pub input_prediction: Option<String>,
    pending_title: bool,
    last_width: u16,
    last_height: u16,
    next_turn_id: u64,
    /// Incremented on rewind/clear/load to invalidate in-flight compactions.
    compact_epoch: u64,
    /// The `compact_epoch` value when the last compaction was requested.
    pending_compact_epoch: u64,
    /// Token count snapshots: `(history_len, tokens)` recorded after each turn
    /// and before each compaction. On rewind, the most recent snapshot at or
    /// before the truncation point is restored.
    token_snapshots: Vec<(usize, u32)>,
}

/// Retained subset of the confirm request for mode-toggle re-checks.
struct ConfirmContext {
    tool_name: String,
    args: HashMap<String, serde_json::Value>,
    request_id: u64,
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
    Submit {
        content: Content,
        display: String,
    },
    MenuResult(MenuResult),
    OpenDialog(Box<dyn render::Dialog>),
    Exec(
        tokio::sync::mpsc::UnboundedReceiver<commands::ExecEvent>,
        std::sync::Arc<tokio::sync::Notify>,
    ),
}

enum CommandAction {
    Continue,
    Quit,
    CancelAndClear,
    Compact {
        focus: Option<String>,
    },
    OpenDialog(Box<dyn render::Dialog>),
    Exec(
        tokio::sync::mpsc::UnboundedReceiver<commands::ExecEvent>,
        std::sync::Arc<tokio::sync::Notify>,
    ),
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
        _ if input == "/compact" || input.starts_with("/compact ") => {
            Err("cannot compact while agent is working".into())
        }
        "/resume" => Err("cannot resume while agent is working".into()),
        "/fork" => Err("cannot fork while agent is working".into()),
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
    Compact {
        focus: Option<String>,
    },
    Quit,
    OpenDialog(Box<dyn render::Dialog>),
    CustomCommand(Box<crate::custom_commands::CustomCommand>),
    Exec(
        tokio::sync::mpsc::UnboundedReceiver<commands::ExecEvent>,
        std::sync::Arc<tokio::sync::Notify>,
    ),
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
    Confirm(ConfirmRequest),
    AskQuestion {
        args: HashMap<String, serde_json::Value>,
        request_id: u64,
    },
}

// ── Supporting types ─────────────────────────────────────────────────────────

pub enum SessionControl {
    Continue,
    NeedsConfirm(ConfirmRequest),
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
        permissions: Arc<Permissions>,
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
        let theme_names: Vec<String> = crate::theme::PRESETS
            .iter()
            .map(|(n, _, _)| (*n).to_string())
            .collect();
        let model_keys: Vec<String> = available_models.iter().map(|m| m.key.clone()).collect();
        input.command_arg_sources = vec![
            ("/model".into(), model_keys),
            ("/theme".into(), theme_names.clone()),
            ("/color".into(), theme_names),
        ];
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

        let cwd = std::env::current_dir()
            .ok()
            .and_then(|p| p.to_str().map(String::from))
            .unwrap_or_default();
        let workspace_rules = crate::workspace_permissions::load(&cwd);
        let (workspace_approved, workspace_approved_dirs) =
            crate::workspace_permissions::into_approvals(&workspace_rules);

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
            exec_rx: None,
            exec_kill: None,
            queued_messages: Vec::new(),
            session_approved: HashMap::new(),
            session_approved_dirs: Vec::new(),
            workspace_approved,
            workspace_approved_dirs,
            workspace_rules,
            cwd,
            seen_outside_dirs: HashSet::new(),
            session: session::Session::new(),
            shared_session,
            context_window: None,
            auto_compact,
            show_speed,
            show_prediction: true,
            show_slug: true,
            restrict_to_workspace,
            available_models,
            engine,
            permissions,
            confirm_context: None,
            input_prediction: None,
            pending_title: false,
            last_width: terminal::size().map(|(w, _)| w).unwrap_or(80),
            last_height: terminal::size().map(|(_, h)| h).unwrap_or(24),
            next_turn_id: 1,
            compact_epoch: 0,
            pending_compact_epoch: 0,
            token_snapshots: Vec::new(),
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
            if let Some(ref slug) = self.session.slug {
                self.screen.set_task_label(slug.clone());
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
                if let Some((rx, kill)) = self.start_shell_escape(cmd) {
                    self.exec_rx = Some(rx);
                    self.exec_kill = Some(kill);
                }
            } else if trimmed == "/resume" {
                if let CommandAction::OpenDialog(dlg) = self.handle_command(trimmed) {
                    active_dialog = Some(dlg);
                }
            } else if trimmed == "/settings" {
                self.input.open_settings(
                    self.input.vim_enabled(),
                    self.auto_compact,
                    self.show_speed,
                    self.show_prediction,
                    self.show_slug,
                    self.restrict_to_workspace,
                );
                self.screen.mark_dirty();
            } else if let Some(reason) = classify_startup_command(trimmed) {
                self.screen
                    .notify_error(format!("\"{}\" {}", trimmed, reason));
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
                        // Custom commands need their own turn — don't steer them
                        // into the current one.
                        if crate::custom_commands::resolve(msg.trim()).is_none() {
                            self.engine.send(UiCommand::Steer { text: msg.clone() });
                        }
                    }
                    ag.steered_count = self.queued_messages.len();
                }
            }

            // ── Auto-start from leftover queued messages ─────────────────
            if agent.is_none() && !self.queued_messages.is_empty() {
                let items = std::mem::take(&mut self.queued_messages);

                // Find the first custom command in the queue.
                let cmd_idx = items
                    .iter()
                    .position(|m| crate::custom_commands::resolve(m.trim()).is_some());

                match cmd_idx {
                    // First item is a custom command — start it, keep the rest.
                    Some(0) => {
                        let cmd = crate::custom_commands::resolve(items[0].trim()).unwrap();
                        self.screen.erase_prompt();
                        agent = Some(self.begin_custom_command_turn(cmd));
                        self.queued_messages = items[1..].to_vec();
                    }
                    // Regular messages before a custom command — process them,
                    // keep the command (and everything after) for next round.
                    Some(idx) => {
                        let text = items[..idx].join("\n");
                        self.queued_messages = items[idx..].to_vec();
                        if !text.is_empty() {
                            self.screen.erase_prompt();
                            let content = Content::text(text.clone());
                            agent = Some(self.begin_agent_turn(&text, content));
                        }
                    }
                    // No custom commands — original behavior.
                    None => {
                        let text = items.join("\n");
                        if !text.is_empty() {
                            self.screen.erase_prompt();
                            match self.process_input(&text) {
                                InputOutcome::StartAgent => {
                                    let content = Content::text(text.clone());
                                    agent = Some(self.begin_agent_turn(&text, content));
                                }
                                InputOutcome::Compact { focus } => {
                                    if self.history.is_empty() {
                                        self.screen.notify_error("nothing to compact".into());
                                    } else {
                                        self.compact_history(focus);
                                    }
                                }
                                InputOutcome::CustomCommand(cmd) => {
                                    agent = Some(self.begin_custom_command_turn(*cmd));
                                }
                                InputOutcome::Exec(rx, kill) => {
                                    self.exec_rx = Some(rx);
                                    self.exec_kill = Some(kill);
                                }
                                InputOutcome::Continue | InputOutcome::Quit => {}
                                InputOutcome::OpenDialog(dlg) => {
                                    active_dialog = Some(dlg);
                                }
                            }
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
                let idle = t
                    .last_keypress
                    .map(|lk| lk.elapsed() >= Duration::from_millis(CONFIRM_DEFER_MS))
                    .unwrap_or(true);
                if idle && deferred_dialog.is_some() {
                    self.screen.set_pending_dialog(false);
                    let deferred = deferred_dialog.take().unwrap();
                    match deferred {
                        DeferredDialog::Confirm(req) => {
                            self.confirm_context = Some(ConfirmContext {
                                tool_name: req.tool_name.clone(),
                                args: req.args.clone(),
                                request_id: req.request_id,
                            });
                            self.screen.set_active_status(ToolStatus::Confirm);
                            let dialog = Box::new(ConfirmDialog::new(&req));
                            self.open_blocking_dialog(dialog, &mut active_dialog);
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
                let sync = self.screen.take_sync_started();
                d.draw(self.screen.dialog_row(), sync);
                self.screen.sync_dialog_anchor(d.anchor_row());
            }

            // ── Wait for next event ──────────────────────────────────────
            tokio::select! {
                biased;

                Some(Ok(ev)) = stream_next(&mut term_events) => {
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

                    // Render immediately after terminal events for responsive typing.
                    let redirtied = self.tick(agent.is_some(), active_dialog.is_some());
                    if let Some(d) = active_dialog.as_mut() {
                        if redirtied { d.mark_dirty(); }
                        let sync = self.screen.take_sync_started();
                        d.draw(self.screen.dialog_row(), sync);
                        self.screen.sync_dialog_anchor(d.anchor_row());
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
                        let sync = self.screen.take_sync_started();
                        d.draw(self.screen.dialog_row(), sync);
                    }
                }

                Some(ev) = async {
                    match self.exec_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    match ev {
                        commands::ExecEvent::Output(line) => {
                            self.screen.append_exec_output(&line);
                        }
                        commands::ExecEvent::Done(code) => {
                            self.screen.finish_exec(code);
                            self.screen.commit_exec();
                            self.exec_rx = None;
                            self.exec_kill = None;
                        }
                    }
                }

                _ = tokio::time::sleep(Duration::from_millis(80)) => {
                    // Timer tick for spinner animation.
                    // Mark dialog dirty so elapsed timers update live (e.g. PsDialog).
                    if let Some(d) = active_dialog.as_mut() {
                        d.mark_dirty();
                    }
                    // Animate btw "thinking..." dots.
                    if self.screen.has_btw() {
                        self.screen.mark_dirty();
                    }
                    // Redraw active exec for elapsed time update.
                    if self.screen.has_active_exec() {
                        self.screen.mark_dirty();
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
            content: Content::text(message),
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
                    let approved = self.mode == Mode::Yolo;
                    self.engine.send(UiCommand::PermissionDecision {
                        request_id,
                        approved,
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
                    self.set_history(messages);
                }
                EngineEvent::TurnError { message } => {
                    eprintln!("[error] {message}");
                }
                EngineEvent::TurnComplete { messages, .. } => {
                    self.set_history(messages);
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
        mut dialog: Box<dyn render::Dialog>,
        active_dialog: &mut Option<Box<dyn render::Dialog>>,
    ) {
        // Flush any pending blocks (e.g. Thinking) to scroll mode so they
        // persist in the viewport after the dialog is dismissed.
        self.screen.render_pending_blocks();
        let height = crossterm::terminal::size().map(|(_, h)| h).unwrap_or(24);
        let fits = self.screen.active_tool_rows() + dialog.height() <= height;
        if !fits {
            self.screen.erase_prompt();
        }
        self.screen.set_show_tool_in_dialog(fits);
        // Share the kill ring so Ctrl+K/Y work across input ↔ dialog.
        dialog.set_kill_ring(self.input.take_kill_ring());
        *active_dialog = Some(dialog);
    }
}

/// Poll one item from a `futures_core::Stream`, equivalent to `StreamExt::next`.
async fn stream_next<S>(stream: &mut S) -> Option<S::Item>
where
    S: futures_core::Stream + Unpin,
{
    std::future::poll_fn(|cx| Pin::new(&mut *stream).poll_next(cx)).await
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
        assert!(is_allowed_while_running("/model").is_ok());
        assert!(is_allowed_while_running("/settings").is_ok());
        assert!(is_allowed_while_running("/theme").is_ok());
        assert!(is_allowed_while_running("/stats").is_ok());
        assert!(is_allowed_while_running("!ls").is_ok());
    }

    #[test]
    fn running_blocked_commands() {
        assert!(is_allowed_while_running("/compact").is_err());
        assert!(is_allowed_while_running("/resume").is_err());
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
