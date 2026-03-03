use crate::input::{resolve_agent_esc, Action, EscAction, History, InputState, MenuResult};
use crate::render::{
    tool_arg_summary, Block, ConfirmChoice, ConfirmDialog, FramePrompt, QuestionDialog,
    ResumeEntry, Screen, ToolOutput, ToolStatus,
};
use crate::session::Session;
use crate::{render, session, state, vim};
use engine::EngineHandle;
use protocol::{EngineEvent, Message, Mode, ReasoningEffort, Role, UiCommand};

use crossterm::{
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyCode, KeyEvent,
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
    pub available_models: Vec<crate::config::ResolvedModel>,
    pub engine: EngineHandle,
    pending_title: bool,
    last_width: u16,
    last_height: u16,
}

struct TurnState {
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
    Submit(String),
    MenuResult(MenuResult),
    OpenDialog(Box<ActiveDialog>),
}

enum CommandAction {
    Continue,
    Quit,
    CancelAndClear,
    Compact,
    OpenDialog(Box<ActiveDialog>),
}

/// Check whether a command is allowed while the agent is running.
/// Returns `Err(reason)` for commands that are blocked.
fn is_allowed_while_running(input: &str) -> Result<(), String> {
    match input {
        "/compact" => Err("cannot compact while agent is working".into()),
        "/resume" => Err("cannot resume while agent is working".into()),
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
    OpenDialog(Box<ActiveDialog>),
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

/// A dialog currently being shown (non-blocking).
enum ActiveDialog {
    Confirm {
        dialog: ConfirmDialog,
        tool_name: String,
        request_id: u64,
    },
    AskQuestion {
        dialog: QuestionDialog,
        request_id: u64,
    },
    Ps(render::PsDialog),
    Rewind(render::RewindDialog),
    Resume(render::ResumeDialog),
}

impl ActiveDialog {
    /// Whether the agent is blocked on a reply channel for this dialog.
    /// When true, no agent events will arrive so draining can be paused.
    fn blocks_agent(&self) -> bool {
        matches!(
            self,
            ActiveDialog::Confirm { .. } | ActiveDialog::AskQuestion { .. }
        )
    }

    fn mark_dirty(&mut self) {
        match self {
            ActiveDialog::Confirm { dialog, .. } => dialog.mark_dirty(),
            ActiveDialog::AskQuestion { dialog, .. } => dialog.mark_dirty(),
            ActiveDialog::Ps(d) => d.mark_dirty(),
            ActiveDialog::Rewind(d) => d.mark_dirty(),
            ActiveDialog::Resume(d) => d.mark_dirty(),
        }
    }

    fn draw(&mut self, start_row: u16) -> u16 {
        match self {
            ActiveDialog::Confirm { dialog, .. } => dialog.draw(start_row),
            ActiveDialog::AskQuestion { dialog, .. } => dialog.draw(start_row),
            ActiveDialog::Ps(d) => d.draw(start_row),
            ActiveDialog::Rewind(d) => d.draw(start_row),
            ActiveDialog::Resume(d) => d.draw(start_row),
        }
    }

    fn handle_resize(&mut self, _w: u16, h: u16) {
        match self {
            ActiveDialog::Confirm { dialog, .. } => dialog.mark_dirty(),
            ActiveDialog::AskQuestion { dialog, .. } => dialog.mark_dirty(),
            ActiveDialog::Ps(d) => d.handle_resize(h),
            ActiveDialog::Rewind(d) => d.handle_resize(h),
            ActiveDialog::Resume(d) => d.handle_resize(h),
        }
    }
}

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
        let reasoning_effort = saved.reasoning_effort;
        let mut screen = Screen::new();
        screen.set_model_label(model.clone());
        screen.set_reasoning_effort(reasoning_effort);
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
            available_models,
            engine,
            pending_title: false,
            last_width: terminal::size().map(|(w, _)| w).unwrap_or(80),
            last_height: terminal::size().map(|(_, h)| h).unwrap_or(24),
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
            self.screen.flush_blocks();
        }
        self.screen
            .draw_prompt(&self.input, self.mode, render::term_width());

        let mut term_events = EventStream::new();
        let mut agent: Option<TurnState> = None;

        let mut active_dialog: Option<ActiveDialog> = None;

        // Auto-submit initial message if provided (e.g. `agent "fix the bug"`).
        if let Some(msg) = initial_message {
            let trimmed = msg.trim();
            if let Some(cmd) = trimmed.strip_prefix('!') {
                self.run_shell_escape(cmd);
            } else if trimmed == "/resume" {
                if let CommandAction::OpenDialog(dlg) = self.handle_command(trimmed) {
                    active_dialog = Some(*dlg);
                }
            } else if trimmed == "/settings" {
                self.input
                    .open_settings(self.input.vim_enabled(), self.auto_compact);
                self.screen.mark_dirty();
            } else if let Some(reason) = classify_startup_command(trimmed) {
                self.screen.push(Block::Error {
                    message: format!("\"{}\" {}", trimmed, reason),
                });
                self.screen.flush_blocks();
            } else {
                self.screen.erase_prompt();
                agent = Some(self.begin_agent_turn(&msg));
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
                        let ctrl =
                            self.handle_engine_event(ev, &mut ag.pending, &mut ag.steered_count);
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
                let mut parts = std::mem::take(&mut self.queued_messages);
                let buf = std::mem::take(&mut self.input.buf);
                self.input.cpos = 0;
                if !buf.trim().is_empty() {
                    parts.push(buf);
                }
                let text = parts.join("\n");
                if !text.trim().is_empty() {
                    self.screen.erase_prompt();
                    match self.process_input(&text) {
                        InputOutcome::StartAgent => {
                            agent = Some(self.begin_agent_turn(&text));
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
                            active_dialog = Some(*dlg);
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
                // Auto-approve deferred confirms in Yolo mode.
                if self.mode == Mode::Yolo {
                    if let Some(DeferredDialog::Confirm { request_id, .. }) = deferred_dialog.take()
                    {
                        self.screen.set_pending_dialog(false);
                        self.engine.send(UiCommand::PermissionDecision {
                            request_id,
                            approved: true,
                            message: None,
                        });
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
                            self.screen.set_active_status(ToolStatus::Confirm);
                            self.render_screen();
                            active_dialog = Some(ActiveDialog::Confirm {
                                dialog: ConfirmDialog::new(
                                    &tool_name,
                                    &desc,
                                    &args,
                                    approval_pattern.as_deref(),
                                    summary.as_deref(),
                                ),
                                tool_name,
                                request_id,
                            });
                        }
                        DeferredDialog::AskQuestion { args, request_id } => {
                            self.render_screen();
                            let questions = render::parse_questions(&args);
                            active_dialog = Some(ActiveDialog::AskQuestion {
                                dialog: QuestionDialog::new(questions),
                                request_id,
                            });
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
                let scroll = d.draw(self.screen.dialog_row());
                self.screen.adjust_for_dialog_scroll(scroll);
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

                    // If we just switched to Yolo, auto-approve any deferred confirm.
                    if self.mode == Mode::Yolo {
                        if let Some(DeferredDialog::Confirm { request_id, .. }) =
                            deferred_dialog.take()
                        {
                            self.screen.set_pending_dialog(false);
                            self.engine.send(UiCommand::PermissionDecision {
                                request_id,
                                approved: true,
                                message: None,
                            });
                        }
                    }

                    // Render immediately after terminal events for responsive typing.
                    let redirtied = self.tick(agent.is_some(), active_dialog.is_some());
                    if let Some(d) = active_dialog.as_mut() {
                        if redirtied { d.mark_dirty(); }
                        let scroll = d.draw(self.screen.dialog_row());
                        self.screen.adjust_for_dialog_scroll(scroll);
                    }
                }

                Some(ev) = self.engine.recv(), if !active_dialog.as_ref().is_some_and(|d| d.blocks_agent()) => {
                    if let Some(ref mut ag) = agent {
                        let ctrl = self.handle_engine_event(ev, &mut ag.pending, &mut ag.steered_count);
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
                        let scroll = d.draw(self.screen.dialog_row());
                        self.screen.adjust_for_dialog_scroll(scroll);
                    }
                }

                _ = tokio::time::sleep(Duration::from_millis(80)) => {
                    // Timer tick for spinner animation.
                    // Mark PsDialog dirty so elapsed timers update live.
                    if let Some(ActiveDialog::Ps(d)) = active_dialog.as_mut() {
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

        self.push_user_message(message.clone());
        if self.session.first_user_message.is_none() {
            self.session.first_user_message = Some(message.clone());
        }

        self.engine.send(UiCommand::StartTurn {
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
                EngineEvent::TurnError { message } => {
                    eprintln!("[error] {message}");
                }
                EngineEvent::TurnComplete { messages } => {
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

    // ── Terminal event dispatch ───────────────────────────────────────────

    /// Handle a single terminal event, potentially starting/stopping agents.
    /// Returns `true` if the app should quit.
    fn dispatch_terminal_event(
        &mut self,
        ev: Event,
        agent: &mut Option<TurnState>,
        t: &mut Timers,
        active_dialog: &mut Option<ActiveDialog>,
    ) -> bool {
        // Route events to the active dialog if one is showing.
        if active_dialog.is_some() {
            // Terminal resize: full clear + redraw screen + redraw dialog.
            if let Event::Resize(w, h) = ev {
                if w != self.last_width || h != self.last_height {
                    self.last_width = w;
                    self.last_height = h;
                    self.screen.redraw(true);
                }
                active_dialog.as_mut().unwrap().handle_resize(w, h);
                return false;
            }
            if let Event::Key(KeyEvent {
                code, modifiers, ..
            }) = ev
            {
                match active_dialog.take().unwrap() {
                    ActiveDialog::Ps(mut d) => {
                        if let Some(_killed) = d.handle_key(code, modifiers) {
                            self.screen.clear_dialog_area();
                        } else {
                            *active_dialog = Some(ActiveDialog::Ps(d));
                        }
                        return false;
                    }
                    ActiveDialog::Rewind(mut d) => {
                        let restore = d.restore_vim_insert;
                        if let Some(maybe_idx) = d.handle_key(code, modifiers) {
                            if let Some(idx) = maybe_idx {
                                if let Some(text) = self.rewind_to(idx) {
                                    self.input.buf = text;
                                    self.input.cpos = self.input.buf.len();
                                }
                            } else if restore {
                                self.input.set_vim_mode(vim::ViMode::Insert);
                            }
                            self.screen.clear_dialog_area();
                        } else {
                            *active_dialog = Some(ActiveDialog::Rewind(d));
                        }
                        return false;
                    }
                    ActiveDialog::Resume(mut d) => {
                        if let Some(maybe_id) = d.handle_key(code, modifiers) {
                            if let Some(id) = maybe_id {
                                if let Some(loaded) = session::load(&id) {
                                    self.load_session(loaded);
                                    self.rebuild_screen_from_history();
                                    self.screen.flush_blocks();
                                }
                            }
                            self.screen.clear_dialog_area();
                        } else {
                            *active_dialog = Some(ActiveDialog::Resume(d));
                        }
                        return false;
                    }
                    ActiveDialog::Confirm {
                        mut dialog,
                        tool_name,
                        request_id,
                    } => {
                        if let Some((choice, message)) = dialog.handle_key(code, modifiers) {
                            let should_cancel = self.resolve_confirm(
                                (choice, message),
                                request_id,
                                &tool_name,
                                agent,
                            );
                            self.screen.clear_dialog_area();
                            if should_cancel && agent.is_some() {
                                self.finish_turn(true);
                                *agent = None;
                            }
                        } else {
                            *active_dialog = Some(ActiveDialog::Confirm {
                                dialog,
                                tool_name,
                                request_id,
                            });
                        }
                    }
                    ActiveDialog::AskQuestion {
                        mut dialog,
                        request_id,
                    } => {
                        if let Some(answer) = dialog.handle_key(code, modifiers) {
                            let should_cancel = self.resolve_question(answer, request_id, agent);
                            self.screen.clear_dialog_area();
                            if should_cancel && agent.is_some() {
                                self.finish_turn(true);
                                *agent = None;
                            }
                        } else {
                            *active_dialog = Some(ActiveDialog::AskQuestion { dialog, request_id });
                        }
                    }
                }
            }
            return false;
        }

        let outcome = if agent.is_some() {
            self.handle_event_running(ev, t)
        } else {
            self.handle_event_idle(ev, t)
        };

        match outcome {
            EventOutcome::Noop | EventOutcome::Redraw => false,
            EventOutcome::Quit => {
                if agent.is_some() {
                    self.finish_turn(true);
                    *agent = None;
                }
                true
            }
            EventOutcome::CancelAgent => {
                engine::log::entry(
                    engine::log::Level::Info,
                    "agent_stop",
                    &serde_json::json!({
                        "reason": "user_cancel",
                    }),
                );
                if agent.is_some() {
                    self.finish_turn(true);
                    *agent = None;
                }
                false
            }
            EventOutcome::CancelAndClear => {
                engine::log::entry(
                    engine::log::Level::Info,
                    "agent_stop",
                    &serde_json::json!({
                        "reason": "user_cancel_and_clear",
                    }),
                );
                if agent.is_some() {
                    self.finish_turn(true);
                    *agent = None;
                }
                self.reset_session();
                false
            }
            EventOutcome::MenuResult(result) => {
                match result {
                    MenuResult::Settings { vim, auto_compact } => {
                        self.input.set_vim_enabled(vim);
                        state::set_vim_enabled(vim);
                        self.auto_compact = auto_compact;
                    }
                    MenuResult::ModelSelect(key) => {
                        if let Some(resolved) = self.available_models.iter().find(|m| m.key == key)
                        {
                            self.model = resolved.model_name.clone();
                            self.api_base = resolved.api_base.clone();
                            self.api_key_env = resolved.api_key_env.clone();
                            self.screen.set_model_label(resolved.model_name.clone());
                            state::set_selected_model(key);
                        }
                        self.screen.erase_prompt();
                    }
                    MenuResult::Dismissed => {}
                }
                self.screen.mark_dirty();
                false
            }
            EventOutcome::OpenDialog(dlg) => {
                self.screen.erase_prompt();
                *active_dialog = Some(*dlg);
                false
            }
            EventOutcome::Submit(text) => {
                if !text.trim().is_empty() {
                    self.screen.erase_prompt();
                    match self.process_input(&text) {
                        InputOutcome::StartAgent => {
                            *agent = Some(self.begin_agent_turn(&text));
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
                        InputOutcome::Continue => {}
                        InputOutcome::Quit => return true,
                        InputOutcome::OpenDialog(dlg) => {
                            *active_dialog = Some(*dlg);
                        }
                    }
                }
                false
            }
        }
    }

    // ── Idle event handler ───────────────────────────────────────────────

    fn handle_event_idle(&mut self, ev: Event, t: &mut Timers) -> EventOutcome {
        // Resize
        if let Event::Resize(w, h) = ev {
            if w != self.last_width || h != self.last_height {
                self.last_width = w;
                self.last_height = h;
                self.screen.redraw(true);
            }
            return EventOutcome::Noop;
        }

        // Ctrl+R: open history fuzzy search (not in vim normal mode).
        if matches!(
            ev,
            Event::Key(KeyEvent {
                code: KeyCode::Char('r'),
                modifiers: KeyModifiers::CONTROL,
                ..
            })
        ) && self.input.history_search_query().is_none()
            && !self
                .input
                .vim_mode()
                .is_some_and(|m| m == vim::ViMode::Normal)
        {
            self.input.open_history_search(&self.input_history);
            self.screen.mark_dirty();
            return EventOutcome::Redraw;
        }

        // Ctrl+C: dismiss the topmost UI element, or quit if nothing is open.
        if matches!(
            ev,
            Event::Key(KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                ..
            })
        ) {
            // Menu open → dismiss it.
            if let Some(result) = self.input.dismiss_menu() {
                self.screen.mark_dirty();
                return EventOutcome::MenuResult(result);
            }
            // Completer open → close it.
            if self.input.completer.is_some() {
                self.input.completer = None;
                self.screen.mark_dirty();
                return EventOutcome::Redraw;
            }
            // Non-empty prompt → clear it.
            if !self.input.buf.is_empty() {
                t.last_ctrlc = Some(Instant::now());
                self.input.buf.clear();
                self.input.cpos = 0;
                self.input.pastes.clear();
                self.screen.mark_dirty();
                return EventOutcome::Redraw;
            }
            // Nothing open, empty prompt → quit.
            let double_tap = t
                .last_ctrlc
                .is_some_and(|prev| prev.elapsed() < Duration::from_millis(500));
            if double_tap {
                return EventOutcome::Quit;
            }
            t.last_ctrlc = Some(Instant::now());
            return EventOutcome::Quit;
        }

        // Ctrl+S: toggle stash.
        if matches!(
            ev,
            Event::Key(KeyEvent {
                code: KeyCode::Char('s'),
                modifiers: KeyModifiers::CONTROL,
                ..
            })
        ) {
            self.input.toggle_stash();
            self.screen.mark_dirty();
            return EventOutcome::Redraw;
        }

        // Esc / double-Esc (skip when a modal menu is open — let it handle Esc)
        if !self.input.has_modal()
            && matches!(
                ev,
                Event::Key(KeyEvent {
                    code: KeyCode::Esc,
                    ..
                })
            )
        {
            let in_normal = !self.input.vim_enabled() || !self.input.vim_in_insert_mode();
            if in_normal {
                let double = t
                    .last_esc
                    .is_some_and(|prev| prev.elapsed() < Duration::from_millis(500));
                if double {
                    t.last_esc = None;
                    let restore_mode = t.esc_vim_mode.take();
                    let turns = self.screen.user_turns();
                    if turns.is_empty() {
                        return EventOutcome::Noop;
                    }
                    self.screen.erase_prompt();
                    let restore_vim_insert = restore_mode == Some(vim::ViMode::Insert);
                    return EventOutcome::OpenDialog(Box::new(ActiveDialog::Rewind(
                        render::RewindDialog::new(turns, restore_vim_insert),
                    )));
                }
                // Single Esc in normal mode — start timer.
                t.last_esc = Some(Instant::now());
                t.esc_vim_mode = self.input.vim_mode();
                if !self.input.vim_enabled() {
                    return EventOutcome::Noop;
                }
                // Vim normal mode — fall through to handle_event (resets pending op).
            } else {
                // Vim insert mode — start double-Esc timer, fall through so
                // handle_event processes the Esc and switches vim to normal.
                t.esc_vim_mode = Some(vim::ViMode::Insert);
                t.last_esc = Some(Instant::now());
            }
        } else {
            t.last_esc = None;
        }

        // Delegate to InputState::handle_event
        match self.input.handle_event(ev, Some(&mut self.input_history)) {
            Action::Submit(text) if text.trim() == "/model" => {
                let models: Vec<(String, String, String)> = self
                    .available_models
                    .iter()
                    .map(|m| (m.key.clone(), m.model_name.clone(), m.provider_name.clone()))
                    .collect();
                if !models.is_empty() {
                    self.input.open_model_picker(models);
                    self.screen.mark_dirty();
                }
                EventOutcome::Redraw
            }
            Action::Submit(text) if text.trim() == "/settings" => {
                self.input
                    .open_settings(self.input.vim_enabled(), self.auto_compact);
                self.screen.mark_dirty();
                EventOutcome::Redraw
            }
            Action::Submit(text) => {
                self.input.restore_stash();
                EventOutcome::Submit(text)
            }
            Action::MenuResult(result) => EventOutcome::MenuResult(result),
            Action::ToggleMode => {
                self.toggle_mode();
                EventOutcome::Redraw
            }
            Action::CycleReasoning => {
                self.set_reasoning_effort(self.reasoning_effort.cycle());
                EventOutcome::Redraw
            }
            Action::Resize {
                width: w,
                height: h,
            } => {
                let (w16, h16) = (w as u16, h as u16);
                if w16 != self.last_width || h16 != self.last_height {
                    self.last_width = w16;
                    self.last_height = h16;
                    self.screen.redraw(true);
                }
                EventOutcome::Noop
            }
            Action::Redraw => {
                self.screen.mark_dirty();
                EventOutcome::Redraw
            }
            Action::Noop => EventOutcome::Noop,
        }
    }

    // ── Running event handler ────────────────────────────────────────────

    fn handle_event_running(&mut self, ev: Event, t: &mut Timers) -> EventOutcome {
        // Resize
        if let Event::Resize(w, h) = ev {
            if w != self.last_width || h != self.last_height {
                self.last_width = w;
                self.last_height = h;
                self.screen.redraw(true);
            }
            return EventOutcome::Noop;
        }

        // Track last keypress for deferring permission dialogs.
        if matches!(ev, Event::Key(_)) {
            t.last_keypress = Some(Instant::now());
        }

        // Ctrl+C: dismiss UI elements first, then cancel agent.
        if matches!(
            ev,
            Event::Key(KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                ..
            })
        ) {
            // Completer open → close it.
            if self.input.completer.is_some() {
                self.input.completer = None;
                self.screen.mark_dirty();
                return EventOutcome::Noop;
            }
            // Non-empty prompt → clear it + queued messages.
            if !self.input.buf.is_empty() {
                t.last_ctrlc = Some(Instant::now());
                self.input.clear();
                self.queued_messages.clear();
                self.screen.mark_dirty();
                return EventOutcome::Noop;
            }
            // Nothing open → double-tap cancel agent, single clears queued.
            let double_tap = t
                .last_ctrlc
                .is_some_and(|prev| prev.elapsed() < Duration::from_millis(500));
            if double_tap {
                t.last_ctrlc = None;
                self.screen.mark_dirty();
                return EventOutcome::CancelAgent;
            }
            t.last_ctrlc = Some(Instant::now());
            self.queued_messages.clear();
            self.screen.mark_dirty();
            return EventOutcome::CancelAgent;
        }

        // Esc: use resolve_agent_esc for the running-mode logic.
        if matches!(
            ev,
            Event::Key(KeyEvent {
                code: KeyCode::Esc,
                ..
            })
        ) {
            match resolve_agent_esc(
                self.input.vim_mode(),
                !self.queued_messages.is_empty(),
                &mut t.last_esc,
                &mut t.esc_vim_mode,
            ) {
                EscAction::VimToNormal => {
                    self.input.handle_event(ev, None);
                    self.screen.mark_dirty();
                }
                EscAction::Unqueue => {
                    let mut combined = self.queued_messages.join("\n");
                    if !self.input.buf.is_empty() {
                        combined.push('\n');
                        combined.push_str(&self.input.buf);
                    }
                    self.input.buf = combined;
                    self.input.cpos = self.input.buf.len();
                    self.queued_messages.clear();
                    self.screen.mark_dirty();
                }
                EscAction::Cancel { restore_vim } => {
                    if let Some(mode) = restore_vim {
                        self.input.set_vim_mode(mode);
                    }
                    self.screen.mark_dirty();
                    return EventOutcome::CancelAgent;
                }
                EscAction::StartTimer => {}
            }
            return EventOutcome::Noop;
        }

        // Everything else → InputState::handle_event (type-ahead with history).
        match self.input.handle_event(ev, Some(&mut self.input_history)) {
            Action::Submit(text) => {
                if let Some(outcome) = self.try_command_while_running(text.trim()) {
                    return outcome;
                }
                // Not a command — queue as a user message.
                if !text.is_empty() {
                    self.queued_messages.push(text);
                }
                self.screen.mark_dirty();
            }
            Action::ToggleMode => {
                self.toggle_mode();
            }
            Action::Redraw => {
                self.screen.mark_dirty();
            }
            Action::CycleReasoning => {
                self.set_reasoning_effort(self.reasoning_effort.cycle());
            }
            Action::MenuResult(_) | Action::Noop | Action::Resize { .. } => {}
        }
        EventOutcome::Noop
    }

    // ── Input processing (commands, settings, rewind, shell) ─────────────

    fn process_input(&mut self, input: &str) -> InputOutcome {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return InputOutcome::Continue;
        }

        self.input_history.push(trimmed.to_string());
        state::set_mode(self.mode);

        match self.handle_command(trimmed) {
            CommandAction::Quit => return InputOutcome::Quit,
            CommandAction::CancelAndClear => return InputOutcome::Continue, // already cleared
            CommandAction::Compact => return InputOutcome::Compact,
            CommandAction::OpenDialog(dlg) => return InputOutcome::OpenDialog(dlg),
            CommandAction::Continue => {}
        }
        if trimmed.starts_with('/') && crate::completer::Completer::is_command(trimmed) {
            return InputOutcome::Continue;
        }

        // Regular user message → start agent
        InputOutcome::StartAgent
    }

    // ── Agent lifecycle ──────────────────────────────────────────────────

    fn begin_agent_turn(&mut self, input: &str) -> TurnState {
        self.screen.begin_turn();
        self.show_user_message(input);
        if self.session.first_user_message.is_none() {
            self.session.first_user_message = Some(input.to_string());
        }
        self.push_user_message(input.to_string());
        self.save_session();
        self.screen.set_throbber(render::Throbber::Working);

        self.engine.send(UiCommand::StartTurn {
            input: input.to_string(),
            mode: self.mode,
            model: self.model.clone(),
            reasoning_effort: self.reasoning_effort,
            history: self.history.clone(),
            api_base: Some(self.api_base.clone()),
            api_key: Some(std::env::var(&self.api_key_env).unwrap_or_default()),
        });

        TurnState {
            pending: None,
            steered_count: 0,
            _perf: crate::perf::begin("agent_turn"),
        }
    }

    fn finish_turn(&mut self, cancelled: bool) {
        self.screen.flush_blocks();
        if cancelled {
            self.engine.send(UiCommand::Cancel);
            self.screen.set_throbber(render::Throbber::Interrupted);
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
        }
        self.save_session();
        self.maybe_generate_title();
        state::set_mode(self.mode);
        self.maybe_auto_compact();
    }

    // ── Commands ─────────────────────────────────────────────────────────

    fn handle_command(&mut self, input: &str) -> CommandAction {
        match input {
            "/exit" | "/quit" => CommandAction::Quit,
            "/clear" | "/new" => {
                self.reset_session();
                CommandAction::CancelAndClear
            }
            "/compact" => CommandAction::Compact,
            "/resume" => {
                let entries = self.resume_entries();
                if entries.is_empty() {
                    self.screen.push(Block::Error {
                        message: "no saved sessions".into(),
                    });
                    self.screen.flush_blocks();
                    CommandAction::Continue
                } else {
                    let cwd = std::env::current_dir()
                        .ok()
                        .and_then(|p| p.to_str().map(String::from))
                        .unwrap_or_default();
                    CommandAction::OpenDialog(Box::new(ActiveDialog::Resume(
                        render::ResumeDialog::new(entries, cwd),
                    )))
                }
            }
            "/vim" => {
                let enabled = !self.input.vim_enabled();
                self.input.set_vim_enabled(enabled);
                state::set_vim_enabled(enabled);
                CommandAction::Continue
            }
            "/export" => {
                self.export_to_clipboard();
                CommandAction::Continue
            }
            "/ps" => {
                if self.engine.processes.list().is_empty() {
                    self.screen.push(Block::Error {
                        message: "no background processes".into(),
                    });
                    self.screen.flush_blocks();
                    CommandAction::Continue
                } else {
                    CommandAction::OpenDialog(Box::new(ActiveDialog::Ps(render::PsDialog::new(
                        self.engine.processes.clone(),
                    ))))
                }
            }
            _ if input.starts_with('!') => {
                self.run_shell_escape(&input[1..]);
                CommandAction::Continue
            }
            _ => CommandAction::Continue,
        }
    }

    /// Execute a command while the agent is running.
    /// Returns the `EventOutcome` to use, or `None` to queue as a message.
    fn try_command_while_running(&mut self, input: &str) -> Option<EventOutcome> {
        // Not a command — will be queued as a user message.
        if !input.starts_with('/') && !input.starts_with('!') {
            return None;
        }
        if input.starts_with('/') && !crate::completer::Completer::is_command(input) {
            return None;
        }

        // Access control: some commands are blocked while running.
        if let Err(reason) = is_allowed_while_running(input) {
            self.screen.push(Block::Error { message: reason });
            self.screen.flush_blocks();
            return Some(EventOutcome::Noop);
        }

        // Delegate to the unified handler.
        match self.handle_command(input) {
            CommandAction::Quit => Some(EventOutcome::Quit),
            CommandAction::CancelAndClear => Some(EventOutcome::CancelAndClear),
            CommandAction::OpenDialog(dlg) => Some(EventOutcome::OpenDialog(dlg)),
            CommandAction::Continue => Some(EventOutcome::Noop),
            CommandAction::Compact => unreachable!(), // blocked above
        }
    }

    fn run_shell_escape(&mut self, raw: &str) {
        let cmd = raw.trim();
        if cmd.is_empty() {
            return;
        }
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .output()
            .map(|o| {
                let mut s = String::from_utf8_lossy(&o.stdout).to_string();
                let stderr = String::from_utf8_lossy(&o.stderr);
                if !stderr.is_empty() {
                    if !s.is_empty() {
                        s.push('\n');
                    }
                    s.push_str(&stderr);
                }
                s.truncate(s.trim_end().len());
                s
            })
            .unwrap_or_else(|e| format!("error: {}", e));
        self.screen.push(Block::Exec {
            command: cmd.to_string(),
            output,
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
        let mut dialog = render::ResumeDialog::new(entries, cwd);
        terminal::enable_raw_mode().ok();
        loop {
            let _ = dialog.draw(0);
            match event::read() {
                Ok(Event::Key(KeyEvent {
                    code, modifiers, ..
                })) => {
                    if let Some(maybe_id) = dialog.handle_key(code, modifiers) {
                        terminal::disable_raw_mode().ok();
                        if let Some(id) = maybe_id {
                            if let Some(loaded) = session::load(&id) {
                                self.load_session(loaded);
                            }
                        }
                        return;
                    }
                }
                Ok(Event::Resize(_, h)) => {
                    dialog.handle_resize(h);
                }
                _ => {}
            }
        }
    }

    fn resume_entries(&self) -> Vec<ResumeEntry> {
        session::list_sessions()
            .into_iter()
            .map(|s| ResumeEntry {
                id: s.id,
                title: s.title.unwrap_or_default(),
                subtitle: s.first_user_message,
                updated_at_ms: s.updated_at_ms,
                created_at_ms: s.created_at_ms,
                cwd: s.cwd,
            })
            .collect()
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
                    let content = msg.content.clone().unwrap_or_default();
                    tool_outputs.insert(
                        id.clone(),
                        ToolOutput {
                            content,
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
                            text: content.clone(),
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
                                content: content.clone(),
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
                        if let Some(summary) =
                            content.strip_prefix("Summary of prior conversation:\n\n")
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

    fn maybe_generate_title(&mut self) {
        if self.session.title.is_some() || self.pending_title {
            return;
        }
        if let Some(ref msg) = self.session.first_user_message {
            self.pending_title = true;
            self.engine.send(UiCommand::GenerateTitle {
                first_message: msg.clone(),
            });
        }
    }

    pub fn compact_history(&mut self) {
        self.screen.set_throbber(render::Throbber::Compacting);
        self.engine.send(UiCommand::Compact {
            keep_turns: 3,
            history: self.history.clone(),
        });
    }

    fn maybe_auto_compact(&mut self) {
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

    pub fn rewind_to(&mut self, block_idx: usize) -> Option<String> {
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
        self.history.truncate(hist_idx);
        self.screen.truncate_to(block_idx);
        self.screen.clear_context_tokens();
        self.auto_approved.clear();

        turn_text
    }

    // ── Agent internals ──────────────────────────────────────────────────

    pub fn show_user_message(&mut self, input: &str) {
        self.screen.push(Block::User {
            text: input.to_string(),
        });
    }

    pub fn push_user_message(&mut self, input: String) {
        let expanded = crate::expand_at_refs(&input);
        self.history.push(Message {
            role: Role::User,
            content: Some(expanded),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
        });
    }

    fn toggle_mode(&mut self) {
        self.mode = self.mode.toggle();
        state::set_mode(self.mode);
        self.engine.send(UiCommand::SetMode { mode: self.mode });
        self.screen.mark_dirty();
    }

    fn set_reasoning_effort(&mut self, effort: ReasoningEffort) {
        self.reasoning_effort = effort;
        self.screen.set_reasoning_effort(effort);
        state::set_reasoning_effort(effort);
        self.engine.send(UiCommand::SetReasoningEffort { effort });
    }

    pub fn render_screen(&mut self) {
        self.screen.draw_frame(
            render::term_width(),
            Some(FramePrompt {
                state: &self.input,
                mode: self.mode,
                queued: &self.queued_messages,
            }),
        );
    }

    pub fn handle_engine_event(
        &mut self,
        ev: EngineEvent,
        pending: &mut Option<PendingTool>,
        steered_count: &mut usize,
    ) -> SessionControl {
        match ev {
            EngineEvent::Ready => SessionControl::Continue,
            EngineEvent::TokenUsage { prompt_tokens } => {
                if prompt_tokens > 0 {
                    self.screen.set_context_tokens(prompt_tokens);
                }
                self.screen.set_throbber(render::Throbber::Working);
                SessionControl::Continue
            }
            EngineEvent::ToolOutput { chunk, .. } => {
                self.screen.append_active_output(&chunk);
                SessionControl::Continue
            }
            EngineEvent::Steered { text, count } => {
                let drain_n = count.min(self.queued_messages.len());
                self.queued_messages.drain(..drain_n);
                *steered_count = steered_count.saturating_sub(drain_n);
                self.screen.push(Block::User { text });
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
                tool_name,
                args,
                summary,
                ..
            } => {
                self.screen.start_tool(tool_name.clone(), summary, args);
                *pending = Some(PendingTool { name: tool_name });
                SessionControl::Continue
            }
            EngineEvent::ToolFinished { result, .. } => {
                if pending.is_some() {
                    let status = if result.is_error {
                        ToolStatus::Err
                    } else {
                        ToolStatus::Ok
                    };
                    let output = Some(ToolOutput {
                        content: result.content,
                        is_error: result.is_error,
                    });
                    self.screen.finish_tool(status, output);
                }
                *pending = None;
                SessionControl::Continue
            }
            EngineEvent::RequestPermission {
                request_id,
                tool_name,
                args,
                confirm_message,
                approval_pattern,
                summary,
                ..
            } => SessionControl::NeedsConfirm {
                tool_name,
                desc: confirm_message,
                args,
                approval_pattern,
                summary,
                request_id,
            },
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
                let msg = match exit_code {
                    Some(0) => format!("Background process {id} has finished."),
                    Some(c) => format!("Background process {id} exited with code {c}."),
                    None => format!("Background process {id} exited."),
                };
                self.screen.push(Block::Text { content: msg });
                SessionControl::Continue
            }
            EngineEvent::CompactionComplete { messages } => {
                self.history = messages;
                self.save_session();
                self.screen.push(Block::Text {
                    content: "conversation compacted".into(),
                });
                self.screen.set_throbber(render::Throbber::Done);
                SessionControl::Continue
            }
            EngineEvent::TitleGenerated { title } => {
                self.session.title = Some(title);
                self.pending_title = false;
                self.save_session();
                SessionControl::Continue
            }
            EngineEvent::TurnComplete { messages } => {
                self.history = messages;
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
    fn handle_engine_event_idle(&mut self, ev: EngineEvent) {
        match ev {
            EngineEvent::CompactionComplete { messages } => {
                self.history = messages;
                self.save_session();
                self.screen.push(Block::Text {
                    content: "conversation compacted".into(),
                });
                self.screen.set_throbber(render::Throbber::Done);
            }
            EngineEvent::TitleGenerated { title } => {
                self.session.title = Some(title);
                self.pending_title = false;
                self.save_session();
            }
            EngineEvent::ProcessCompleted { id, exit_code } => {
                let msg = match exit_code {
                    Some(0) => format!("Background process {id} has finished."),
                    Some(c) => format!("Background process {id} exited with code {c}."),
                    None => format!("Background process {id} exited."),
                };
                self.screen.push(Block::Text { content: msg });
            }
            _ => {}
        }
    }

    /// Resolve a completed confirm dialog choice.
    /// Returns `true` if the agent should be cancelled.
    fn resolve_confirm(
        &mut self,
        (choice, message): (ConfirmChoice, Option<String>),
        request_id: u64,
        tool_name: &str,
        agent: &mut Option<TurnState>,
    ) -> bool {
        let label = match &choice {
            ConfirmChoice::Yes => "approved",
            ConfirmChoice::Always => "always",
            ConfirmChoice::AlwaysPattern(pat) => pat.as_str(),
            ConfirmChoice::No => "denied",
        };
        if let Some(ref msg) = message {
            self.screen
                .set_active_user_message(format!("{label}: {msg}"));
        }
        match choice {
            ConfirmChoice::Yes => {
                self.screen.set_active_status(ToolStatus::Pending);
                self.engine.send(UiCommand::PermissionDecision {
                    request_id,
                    approved: true,
                    message,
                });
                false
            }
            ConfirmChoice::Always => {
                self.auto_approved.insert(tool_name.to_string(), vec![]);
                self.screen.set_active_status(ToolStatus::Pending);
                self.engine.send(UiCommand::PermissionDecision {
                    request_id,
                    approved: true,
                    message,
                });
                false
            }
            ConfirmChoice::AlwaysPattern(ref pattern) => {
                if let Ok(compiled) = glob::Pattern::new(pattern) {
                    self.auto_approved
                        .entry(tool_name.to_string())
                        .or_default()
                        .push(compiled);
                }
                self.screen.set_active_status(ToolStatus::Pending);
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
                self.screen.finish_tool(ToolStatus::Denied, None);
                if has_message {
                    // Deny with feedback — let the agent continue with the message.
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
                        ag.pending = None;
                    }
                    true
                }
            }
        }
    }

    /// Resolve a completed question dialog.
    /// `answer` is `Some(json)` on confirm, `None` on cancel.
    /// Returns `true` if the agent should be cancelled.
    fn resolve_question(
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
                self.screen.finish_tool(ToolStatus::Denied, None);
                if let Some(ref mut ag) = agent {
                    ag.pending = None;
                }
                true
            }
        }
    }

    fn dispatch_control(
        &mut self,
        ctrl: SessionControl,
        pending: &mut Option<PendingTool>,
        deferred_dialog: &mut Option<DeferredDialog>,
        active_dialog: &mut Option<ActiveDialog>,
        last_keypress: Option<Instant>,
    ) -> LoopAction {
        match ctrl {
            SessionControl::Continue => LoopAction::Continue,
            SessionControl::Done => LoopAction::Done,
            SessionControl::NeedsConfirm {
                tool_name,
                desc,
                args,
                approval_pattern,
                summary,
                request_id,
            } => {
                // Yolo mode: auto-approve everything.
                if self.mode == Mode::Yolo {
                    self.engine.send(UiCommand::PermissionDecision {
                        request_id,
                        approved: true,
                        message: None,
                    });
                    return LoopAction::Continue;
                }

                let tool_name = if tool_name.is_empty() {
                    pending.as_ref().map(|p| p.name.clone()).unwrap_or_default()
                } else {
                    tool_name
                };

                // Check auto-approvals first (doesn't need UI).
                if let Some(patterns) = self.auto_approved.get(&tool_name) {
                    if patterns.is_empty() || patterns.iter().any(|p| p.matches(&desc)) {
                        self.engine.send(UiCommand::PermissionDecision {
                            request_id,
                            approved: true,
                            message: None,
                        });
                        return LoopAction::Continue;
                    }
                }

                // If the user is actively typing, defer the dialog.
                let recently_typed = last_keypress
                    .is_some_and(|t| t.elapsed() < Duration::from_millis(CONFIRM_DEFER_MS));
                if recently_typed && !self.input.buf.is_empty() {
                    self.screen.set_active_status(ToolStatus::Confirm);
                    self.screen.set_pending_dialog(true);
                    *deferred_dialog = Some(DeferredDialog::Confirm {
                        tool_name,
                        desc,
                        args,
                        approval_pattern,
                        summary,
                        request_id,
                    });
                    return LoopAction::Continue;
                }

                // Close any non-blocking dialog (e.g. Ps) to make room.
                if active_dialog.take().is_some() {
                    self.screen.clear_dialog_area();
                }
                self.screen.set_active_status(ToolStatus::Confirm);
                self.render_screen();
                *active_dialog = Some(ActiveDialog::Confirm {
                    dialog: ConfirmDialog::new(
                        &tool_name,
                        &desc,
                        &args,
                        approval_pattern.as_deref(),
                        summary.as_deref(),
                    ),
                    tool_name,
                    request_id,
                });
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
                if active_dialog.take().is_some() {
                    self.screen.clear_dialog_area();
                }
                self.render_screen();
                let questions = render::parse_questions(&args);
                *active_dialog = Some(ActiveDialog::AskQuestion {
                    dialog: QuestionDialog::new(questions),
                    request_id,
                });
                LoopAction::Continue
            }
        }
    }

    /// Returns true if a dialog overlay needs to be re-dirtied (because
    /// `draw_frame` cleared the area underneath it).
    fn tick(&mut self, agent_running: bool, has_dialog: bool) -> bool {
        let w = render::term_width();
        if has_dialog {
            // Render blocks + active tool but skip the prompt — the dialog
            // covers the bottom and must stay at the highest z-index.
            return self.screen.draw_frame(w, None);
        }
        if agent_running {
            self.screen.draw_frame(
                w,
                Some(FramePrompt {
                    state: &self.input,
                    mode: self.mode,
                    queued: &self.queued_messages,
                }),
            );
        } else {
            self.screen.draw_frame(
                w,
                Some(FramePrompt {
                    state: &self.input,
                    mode: self.mode,
                    queued: &[],
                }),
            );
        }
        false
    }

    fn export_to_clipboard(&mut self) {
        let text = self.format_conversation_text();
        if text.is_empty() {
            self.screen.push(Block::Error {
                message: "nothing to export".into(),
            });
            self.screen.flush_blocks();
            return;
        }
        match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(&text)) {
            Ok(()) => {
                self.screen.push(Block::Text {
                    content: "conversation copied to clipboard".into(),
                });
                self.screen.flush_blocks();
            }
            Err(e) => {
                self.screen.push(Block::Error {
                    message: format!("clipboard error: {}", e),
                });
                self.screen.flush_blocks();
            }
        }
    }

    fn format_conversation_text(&self) -> String {
        let mut out = String::new();
        for msg in &self.history {
            match msg.role {
                Role::System | Role::Tool => continue,
                Role::User => {
                    if let Some(c) = &msg.content {
                        out.push_str("User: ");
                        out.push_str(c);
                        out.push_str("\n\n");
                    }
                }
                Role::Assistant => {
                    if let Some(c) = &msg.content {
                        if !c.is_empty() {
                            out.push_str("Assistant: ");
                            out.push_str(c);
                            out.push_str("\n\n");
                        }
                    }
                    if let Some(calls) = &msg.tool_calls {
                        for tc in calls {
                            out.push_str(&format!("[Tool call: {}]\n\n", tc.function.name));
                        }
                    }
                }
            }
        }
        out.trim_end().to_string()
    }
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
