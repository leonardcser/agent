use crate::agent::{run_agent, AgentContext, AgentEvent};
use crate::input::{
    resolve_agent_esc, Action, EscAction, History, InputState, MenuResult, Mode, SharedMode,
};
use crate::provider::{Message, Provider, Role};
use crate::render::{
    tool_arg_summary, Block, ConfirmChoice, ConfirmDialog, QuestionDialog, ResumeEntry, Screen,
    ToolOutput, ToolStatus,
};
use crate::session::Session;
use crate::{permissions, render, session, state, tools, vim};

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
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

// ── App ──────────────────────────────────────────────────────────────────────

pub struct App {
    pub api_base: String,
    pub api_key: String,
    pub model: String,
    pub model_config: crate::config::ModelConfig,
    pub reasoning_effort: crate::provider::ReasoningEffort,
    pub client: reqwest::Client,
    pub mode: Mode,
    pub permissions: permissions::Permissions,
    pub screen: Screen,
    pub history: Vec<Message>,
    pub input_history: History,
    pub app_state: state::State,
    pub input: InputState,
    pub queued_messages: Vec<String>,
    pub auto_approved: HashMap<String, Vec<glob::Pattern>>,
    pub session: session::Session,
    pub shared_session: Arc<Mutex<Option<Session>>>,
    pub context_window: Option<u32>,
    pub auto_compact: bool,
    pub available_models: Vec<crate::config::ResolvedModel>,
    pending_title: Option<tokio::sync::oneshot::Receiver<String>>,
    last_width: u16,
    last_height: u16,
}

struct AgentState {
    cancel: CancellationToken,
    handle: tokio::task::JoinHandle<Vec<Message>>,
    steering: Arc<Mutex<Vec<String>>>,
    shared_mode: SharedMode,
    pending: Option<PendingTool>,
    steered_count: usize,
    _perf: Option<crate::perf::Guard>,
}

enum EventOutcome {
    Noop,
    Redraw,
    Quit,
    CancelAgent,
    Submit(String),
    MenuResult(MenuResult),
    Rewind(usize),
}

enum InputOutcome {
    Continue,
    StartAgent,
    Compact,
    Quit,
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
        reply: tokio::sync::oneshot::Sender<(bool, Option<String>)>,
    },
    AskQuestion {
        dialog: QuestionDialog,
        reply: tokio::sync::oneshot::Sender<String>,
    },
}

/// A permission dialog deferred because the user was actively typing.
enum DeferredDialog {
    Confirm {
        tool_name: String,
        desc: String,
        args: HashMap<String, serde_json::Value>,
        approval_pattern: Option<String>,
        reply: tokio::sync::oneshot::Sender<(bool, Option<String>)>,
    },
    AskQuestion {
        args: HashMap<String, serde_json::Value>,
        reply: tokio::sync::oneshot::Sender<String>,
    },
}

// ── App impl ─────────────────────────────────────────────────────────────────

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        api_base: String,
        api_key: String,
        model: String,
        model_config: crate::config::ModelConfig,
        vim_from_config: bool,
        auto_compact: bool,
        shared_session: Arc<Mutex<Option<Session>>>,
        available_models: Vec<crate::config::ResolvedModel>,
    ) -> Self {
        let app_state = state::State::load();
        let mode = app_state.mode();
        let vim_enabled = app_state.vim_enabled() || vim_from_config;
        let mut input = InputState::new();
        if vim_enabled {
            input.set_vim_enabled(true);
        }
        let reasoning_effort = app_state.reasoning_effort;
        let mut screen = Screen::new();
        screen.set_model_label(model.clone());
        screen.set_reasoning_effort(reasoning_effort);
        Self {
            api_base,
            api_key,
            model,
            model_config,
            reasoning_effort,
            client: reqwest::Client::new(),
            mode,
            permissions: permissions::Permissions::load(),
            screen,
            history: Vec::new(),
            input_history: History::load(),
            app_state,
            input,
            queued_messages: Vec::new(),
            auto_approved: HashMap::new(),
            session: session::Session::new(),
            shared_session,
            context_window: None,
            auto_compact,
            available_models,
            pending_title: None,
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

        self.screen
            .draw_prompt(&self.input, self.mode, render::term_width());

        let mut term_events = EventStream::new();
        let mut agent: Option<AgentState> = None;
        // Dummy receiver — replaced with the real one each time an agent starts.
        let mut agent_rx: mpsc::UnboundedReceiver<AgentEvent> = mpsc::unbounded_channel().1;

        // Auto-submit initial message if provided (e.g. `agent "fix the bug"`).
        if let Some(msg) = initial_message {
            self.screen.erase_prompt();
            let (rx, ag) = self.begin_agent_turn(&msg);
            agent_rx = rx;
            agent = Some(ag);
        }

        let mut t = Timers {
            last_esc: None,
            esc_vim_mode: None,
            last_ctrlc: None,
            last_keypress: None,
        };
        let mut deferred_dialog: Option<DeferredDialog> = None;
        let mut active_dialog: Option<ActiveDialog> = None;

        'main: loop {
            // ── Background polls ─────────────────────────────────────────
            self.poll_pending_title();
            if let Some(ref mut rx) = ctx_rx {
                if let Ok(result) = rx.try_recv() {
                    self.context_window = result;
                    ctx_rx = None;
                }
            }

            // ── Drain agent events (only when no dialog is showing) ─────
            if agent.is_some() && active_dialog.is_none() {
                loop {
                    let ev = match agent_rx.try_recv() {
                        Ok(ev) => ev,
                        Err(mpsc::error::TryRecvError::Empty) => break,
                        Err(mpsc::error::TryRecvError::Disconnected) => {
                            self.finish_agent(agent.take().unwrap(), false).await;
                            break;
                        }
                    };
                    let action = {
                        let ag = agent.as_mut().unwrap();
                        let ctrl =
                            self.handle_agent_event(ev, &mut ag.pending, &mut ag.steered_count);
                        self.dispatch_control(
                            ctrl,
                            &mut ag.pending,
                            &mut deferred_dialog,
                            &mut active_dialog,
                            t.last_keypress,
                        )
                    };
                    match action {
                        LoopAction::Continue => {}
                        LoopAction::Done => {
                            self.finish_agent(agent.take().unwrap(), false).await;
                            break;
                        }
                    }
                }
            }

            // ── Sync steering ────────────────────────────────────────────
            if let Some(ref mut ag) = agent {
                if self.queued_messages.len() > ag.steered_count {
                    let new = self.queued_messages[ag.steered_count..].to_vec();
                    ag.steering.lock().unwrap().extend(new);
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
                            let (rx, ag) = self.begin_agent_turn(&text);
                            agent_rx = rx;
                            agent = Some(ag);
                        }
                        InputOutcome::Compact => {
                            self.compact_history().await;
                        }
                        InputOutcome::Continue | InputOutcome::Quit => {}
                    }
                }
            }

            // ── Show deferred dialog once user stops typing ──────────────
            if deferred_dialog.is_some() && active_dialog.is_none() && agent.is_some() {
                // Auto-approve deferred confirms in Yolo mode.
                if self.mode == Mode::Yolo {
                    if let Some(DeferredDialog::Confirm { reply, .. }) = deferred_dialog.take() {
                        self.screen.set_pending_dialog(false);
                        let _ = reply.send((true, None));
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
                            reply,
                        } => {
                            self.screen.set_active_status(ToolStatus::Confirm);
                            self.render_screen();
                            active_dialog = Some(ActiveDialog::Confirm {
                                dialog: ConfirmDialog::new(
                                    &tool_name,
                                    &desc,
                                    &args,
                                    approval_pattern.as_deref(),
                                ),
                                tool_name,
                                reply,
                            });
                        }
                        DeferredDialog::AskQuestion { args, reply } => {
                            self.render_screen();
                            let questions = render::parse_questions(&args);
                            active_dialog = Some(ActiveDialog::AskQuestion {
                                dialog: QuestionDialog::new(questions),
                                reply,
                            });
                        }
                    }
                }
            }

            // ── Render ───────────────────────────────────────────────────
            let has_dialog = active_dialog.is_some();
            self.tick(agent.is_some(), has_dialog);
            draw_active_dialog(&mut active_dialog);

            // ── Wait for next event ──────────────────────────────────────
            tokio::select! {
                biased;

                Some(Ok(ev)) = term_events.next() => {
                    if self.dispatch_terminal_event(
                        ev, &mut agent, &mut agent_rx, &mut t, &mut active_dialog,
                    ).await {
                        break 'main;
                    }

                    // Drain buffered terminal events
                    while event::poll(Duration::ZERO).unwrap_or(false) {
                        if let Ok(ev) = event::read() {
                            if self.dispatch_terminal_event(
                                ev, &mut agent, &mut agent_rx, &mut t, &mut active_dialog,
                            ).await {
                                break 'main;
                            }
                        }
                    }

                    // Keep the agent's shared mode in sync with app mode.
                    if let Some(ag) = agent.as_ref() {
                        ag.shared_mode.store(self.mode);
                    }

                    // If we just switched to Yolo, auto-approve any deferred confirm.
                    if self.mode == Mode::Yolo {
                        if let Some(DeferredDialog::Confirm { reply, .. }) = deferred_dialog.take()
                        {
                            self.screen.set_pending_dialog(false);
                            let _ = reply.send((true, None));
                        }
                    }

                    // Render immediately after terminal events for responsive typing.
                    let has_dialog = active_dialog.is_some();
                    self.tick(agent.is_some(), has_dialog);
                    draw_active_dialog(&mut active_dialog);
                }

                Some(ev) = agent_rx.recv(), if agent.is_some() && active_dialog.is_none() => {
                    let action = {
                        let ag = agent.as_mut().unwrap();
                        let ctrl = self.handle_agent_event(ev, &mut ag.pending, &mut ag.steered_count);
                        self.dispatch_control(
                            ctrl,
                            &mut ag.pending,
                            &mut deferred_dialog,
                            &mut active_dialog,
                            t.last_keypress,
                        )
                    };
                    match action {
                        LoopAction::Continue => {}
                        LoopAction::Done => {
                            self.finish_agent(agent.take().unwrap(), false).await;
                        }
                    }
                    self.tick(agent.is_some(), active_dialog.is_some());
                    draw_active_dialog(&mut active_dialog);
                }

                _ = tokio::time::sleep(Duration::from_millis(80)) => {
                    // Timer tick for spinner animation.
                }
            }
        }

        // Cleanup
        if let Some(ag) = agent {
            self.finish_agent(ag, true).await;
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

        self.push_user_message(message.clone());
        if self.session.first_user_message.is_none() {
            self.session.first_user_message = Some(message);
        }

        let (tx, mut rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let steering: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let shared_mode = SharedMode::new(self.mode);
        let ctx = self.build_agent_context(cancel, steering, shared_mode);
        let handle = self.spawn_agent(tx, ctx);

        // Drain events, printing text to stdout.
        while let Some(ev) = rx.recv().await {
            match ev {
                AgentEvent::Thinking(content) => {
                    eprintln!("[thinking] {content}");
                }
                AgentEvent::Text(content) => {
                    print!("{content}");
                    let _ = io::stdout().flush();
                }
                AgentEvent::ToolCall { name, args } => {
                    let summary = tool_arg_summary(&name, &args);
                    eprintln!("[tool: {name}] {summary}");
                }
                AgentEvent::ToolResult {
                    content, is_error, ..
                } => {
                    if is_error {
                        eprintln!("[tool error] {content}");
                    }
                }
                AgentEvent::Confirm { reply, .. } => {
                    // No user to ask — deny. (Allow/Deny are already
                    // resolved by the agent; only Ask reaches here.)
                    let _ = reply.send((false, None));
                }
                AgentEvent::AskQuestion { reply, .. } => {
                    let _ = reply.send("User is not available (headless mode).".into());
                }
                AgentEvent::Error(e) => {
                    eprintln!("[error] {e}");
                }
                AgentEvent::Done => break,
                _ => {}
            }
        }

        // Wait for the agent task to finish and collect messages.
        if let Ok(msgs) = handle.await {
            self.history = msgs;
        }
        self.save_session();

        // Ensure output ends with a newline.
        println!();
    }

    // ── Terminal event dispatch ───────────────────────────────────────────

    /// Handle a single terminal event, potentially starting/stopping agents.
    /// Returns `true` if the app should quit.
    async fn dispatch_terminal_event(
        &mut self,
        ev: Event,
        agent: &mut Option<AgentState>,
        agent_rx: &mut mpsc::UnboundedReceiver<AgentEvent>,
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
                match active_dialog.as_mut().unwrap() {
                    ActiveDialog::Confirm { dialog, .. } => {
                        dialog.mark_dirty();
                        dialog.draw();
                    }
                    ActiveDialog::AskQuestion { dialog, .. } => {
                        dialog.mark_dirty();
                        dialog.draw();
                    }
                }
                return false;
            }
            if let Event::Key(KeyEvent {
                code, modifiers, ..
            }) = ev
            {
                let result = match active_dialog.as_mut().unwrap() {
                    ActiveDialog::Confirm { dialog, .. } => dialog
                        .handle_key(code, modifiers)
                        .map(|(c, m)| DialogOutcome::Confirm(c, m)),
                    ActiveDialog::AskQuestion { dialog, .. } => dialog
                        .handle_key(code, modifiers)
                        .map(DialogOutcome::Question),
                };
                if let Some(outcome) = result {
                    let dlg = active_dialog.take().unwrap();
                    let should_cancel = match dlg {
                        ActiveDialog::Confirm {
                            dialog,
                            tool_name,
                            reply,
                        } => {
                            dialog.cleanup();
                            self.resolve_confirm(outcome.into_confirm(), reply, &tool_name, agent)
                        }
                        ActiveDialog::AskQuestion { dialog, reply } => {
                            dialog.cleanup();
                            self.resolve_question(outcome.into_question(), reply, agent)
                        }
                    };
                    self.screen.redraw(self.screen.has_scrollback);
                    if should_cancel {
                        if let Some(ag) = agent.take() {
                            self.finish_agent(ag, true).await;
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
                if let Some(ag) = agent.take() {
                    self.finish_agent(ag, true).await;
                }
                true
            }
            EventOutcome::CancelAgent => {
                if let Some(ag) = agent.take() {
                    self.finish_agent(ag, true).await;
                }
                false
            }
            EventOutcome::MenuResult(result) => {
                match result {
                    MenuResult::Settings { vim, auto_compact } => {
                        self.input.set_vim_enabled(vim);
                        self.app_state.set_vim_enabled(vim);
                        self.auto_compact = auto_compact;
                    }
                    MenuResult::ModelSelect(key) => {
                        if let Some(resolved) = self.available_models.iter().find(|m| m.key == key)
                        {
                            self.model = resolved.model_name.clone();
                            self.model_config = resolved.config.clone();
                            self.api_base = resolved.api_base.clone();
                            self.api_key = std::env::var(&resolved.api_key_env).unwrap_or_default();
                            self.screen.set_model_label(resolved.model_name.clone());
                            self.app_state.set_selected_model(key);
                        }
                        self.screen.erase_prompt();
                    }
                    MenuResult::Dismissed => {}
                }
                self.screen.mark_dirty();
                false
            }
            EventOutcome::Rewind(block_idx) => {
                if let Some(text) = self.rewind_to(block_idx) {
                    self.input.buf = text;
                    self.input.cpos = self.input.buf.len();
                }
                false
            }
            EventOutcome::Submit(text) => {
                if !text.trim().is_empty() {
                    self.screen.erase_prompt();
                    match self.process_input(&text) {
                        InputOutcome::StartAgent => {
                            let (rx, ag) = self.begin_agent_turn(&text);
                            *agent_rx = rx;
                            *agent = Some(ag);
                        }
                        InputOutcome::Compact => {
                            self.compact_history().await;
                        }
                        InputOutcome::Continue => {}
                        InputOutcome::Quit => return true,
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
                    if let Some(block_idx) = render::show_rewind(&turns) {
                        return EventOutcome::Rewind(block_idx);
                    }
                    // Rewind cancelled — restore vim mode if we started from insert.
                    if restore_mode == Some(vim::ViMode::Insert) {
                        self.input.set_vim_mode(vim::ViMode::Insert);
                    }
                    self.screen.redraw(self.screen.has_scrollback);
                    return EventOutcome::Redraw;
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

        // Everything else → InputState::handle_event (type-ahead, no history).
        match self.input.handle_event(ev, None) {
            Action::Submit(text) => {
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
        self.app_state.set_mode(self.mode);

        if !self.handle_command(trimmed) {
            return InputOutcome::Quit;
        }
        if trimmed == "/compact" {
            return InputOutcome::Compact;
        }
        if trimmed.starts_with('/') && crate::completer::Completer::is_command(trimmed) {
            return InputOutcome::Continue;
        }

        // Shell command
        if let Some(cmd) = trimmed.strip_prefix('!') {
            let cmd = cmd.trim();
            if !cmd.is_empty() {
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
            }
            return InputOutcome::Continue;
        }

        // Regular user message → start agent
        InputOutcome::StartAgent
    }

    // ── Agent lifecycle ──────────────────────────────────────────────────

    fn begin_agent_turn(
        &mut self,
        input: &str,
    ) -> (mpsc::UnboundedReceiver<AgentEvent>, AgentState) {
        self.screen.begin_turn();
        self.show_user_message(input);
        if self.session.first_user_message.is_none() {
            self.session.first_user_message = Some(input.to_string());
        }
        self.push_user_message(input.to_string());
        self.save_session();

        self.screen.set_throbber(render::Throbber::Working);

        let (tx, rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let steering: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let shared_mode = SharedMode::new(self.mode);
        let ctx = self.build_agent_context(cancel.clone(), steering.clone(), shared_mode.clone());
        let handle = self.spawn_agent(tx, ctx);

        let state = AgentState {
            cancel,
            handle,
            steering,
            shared_mode,
            pending: None,
            steered_count: 0,
            _perf: crate::perf::begin("agent_turn"),
        };
        (rx, state)
    }

    async fn finish_agent(&mut self, agent: AgentState, cancelled: bool) {
        self.screen.flush_blocks();
        if cancelled {
            self.screen.set_throbber(render::Throbber::Interrupted);
            let mut leftover: Vec<String> = agent.steering.lock().unwrap().drain(..).collect();
            leftover.append(&mut self.queued_messages);
            if !leftover.is_empty() {
                let mut combined = leftover.join("\n");
                if !self.input.buf.is_empty() {
                    combined.push('\n');
                    combined.push_str(&self.input.buf);
                }
                self.input.buf = combined;
                self.input.cpos = self.input.buf.len();
            }
            agent.cancel.cancel();
            agent.handle.abort();
        } else {
            self.screen.set_throbber(render::Throbber::Done);
            if let Ok(new_messages) = agent.handle.await {
                self.history = new_messages;
            }
        }
        self.save_session();
        self.maybe_generate_title();
        self.app_state.set_mode(self.mode);
        self.maybe_auto_compact().await;
    }

    // ── Commands ─────────────────────────────────────────────────────────

    pub fn handle_command(&mut self, input: &str) -> bool {
        match input {
            "/exit" | "/quit" => return false,
            "/clear" | "/new" => {
                self.reset_session();
            }
            "/resume" => {
                self.resume_session();
            }
            "/vim" => {
                let enabled = !self.input.vim_enabled();
                self.input.set_vim_enabled(enabled);
                self.app_state.set_vim_enabled(enabled);
            }
            "/compact" => {} // handled via InputOutcome::Compact
            "/export" => {
                self.export_to_clipboard();
            }
            _ => {}
        }
        true
    }

    pub fn reset_session(&mut self) {
        self.history.clear();
        self.auto_approved.clear();
        self.queued_messages.clear();
        self.screen.clear();
        self.input.clear();
        self.session = session::Session::new();
    }

    pub fn resume_session(&mut self) {
        let sessions = session::list_sessions();
        if sessions.is_empty() {
            self.screen.push(Block::Error {
                message: "no saved sessions".into(),
            });
            self.screen.flush_blocks();
            return;
        }

        let entries: Vec<ResumeEntry> = sessions
            .into_iter()
            .map(|s| ResumeEntry {
                id: s.id,
                title: s.title.unwrap_or_default(),
                subtitle: s.first_user_message,
                updated_at_ms: s.updated_at_ms,
                created_at_ms: s.created_at_ms,
            })
            .collect();

        if let Some(id) = render::show_resume(&entries) {
            if let Some(loaded) = session::load(&id) {
                self.session = loaded;
                self.history = self.session.messages.clone();
                self.auto_approved.clear();
                self.queued_messages.clear();
                self.input.clear();
                self.rebuild_screen_from_history();
                self.screen.flush_blocks();
            }
        } else {
            // Resume cancelled — restore the screen (show_resume clears it).
            self.screen.redraw(self.screen.has_scrollback);
        }
        // show_resume manages its own raw mode — re-enable.
        terminal::enable_raw_mode().ok();
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
        session::save(&self.session);
        if let Ok(mut guard) = self.shared_session.lock() {
            *guard = Some(self.session.clone());
        }
    }

    pub fn maybe_generate_title(&mut self) {
        let has_title = self
            .session
            .title
            .as_ref()
            .is_some_and(|t| !t.trim().is_empty());
        if has_title || self.pending_title.is_some() {
            return;
        }
        let Some(first) = self.session.first_user_message.clone() else {
            return;
        };
        let provider = self.build_provider();
        let model = self.model.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending_title = Some(rx);
        tokio::spawn(async move {
            let title = match provider.complete_title(&first, &model).await {
                Ok(t) if !t.is_empty() => t,
                _ => {
                    let fallback = first.lines().next().unwrap_or("Untitled");
                    let mut trimmed = fallback.to_string();
                    if trimmed.len() > 48 {
                        trimmed.truncate(48);
                        trimmed = trimmed.trim().to_string();
                    }
                    trimmed
                }
            };
            let _ = tx.send(title);
        });
    }

    pub fn poll_pending_title(&mut self) {
        if let Some(ref mut rx) = self.pending_title {
            match rx.try_recv() {
                Ok(title) => {
                    self.session.title = Some(title);
                    self.pending_title = None;
                    self.save_session();
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    self.pending_title = None;
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
            }
        }
    }

    pub async fn compact_history(&mut self) {
        const KEEP_TURNS: usize = 2;

        let cut = {
            let mut turns_seen = 0;
            let mut idx = self.history.len();
            for (i, msg) in self.history.iter().enumerate().rev() {
                if matches!(msg.role, Role::User) {
                    turns_seen += 1;
                    if turns_seen == KEEP_TURNS {
                        idx = i;
                        break;
                    }
                }
            }
            idx
        };

        if cut == 0 {
            self.screen.push(Block::Error {
                message: "not enough history to compact".into(),
            });
            self.screen.flush_blocks();
            return;
        }

        let to_summarize = self.history[..cut].to_vec();

        let provider = self.build_provider();
        let model = self.model.clone();
        let cancel = CancellationToken::new();
        let task = tokio::spawn(async move {
            provider.compact(&to_summarize, &model, &cancel).await
        });

        self.screen.set_throbber(render::Throbber::Compacting);
        loop {
            self.render_screen();
            if task.is_finished() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(80)).await;
        }

        let result = task.await.unwrap_or_else(|_| Err("task panicked".into()));

        match result {
            Ok(summary) => {
                let summary_msg = Message {
                    role: Role::System,
                    content: Some(format!("Summary of prior conversation:\n\n{}", summary)),
                    tool_calls: None,
                    tool_call_id: None,
                };
                let tail = self.history[cut..].to_vec();
                self.history = vec![summary_msg];
                self.history.extend(tail);
                self.save_session();
                self.screen.clear();
                self.screen.push(Block::Text {
                    content: summary.clone(),
                });
                self.screen.flush_blocks();
                self.screen.set_throbber(render::Throbber::Done);
            }
            Err(e) => {
                self.screen.push(Block::Error {
                    message: format!("compact failed: {}", e),
                });
                self.screen.flush_blocks();
            }
        }
    }

    pub async fn maybe_auto_compact(&mut self) {
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
            self.compact_history().await;
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
            tool_calls: None,
            tool_call_id: None,
        });
    }

    pub fn build_provider(&self) -> Provider {
        Provider::new(
            self.api_base.clone(),
            self.api_key.clone(),
            self.client.clone(),
        )
        .with_model_config(self.model_config.clone())
        .with_reasoning_effort(self.reasoning_effort)
    }

    fn build_agent_context(
        &self,
        cancel: CancellationToken,
        steering: Arc<Mutex<Vec<String>>>,
        shared_mode: SharedMode,
    ) -> AgentContext {
        AgentContext {
            provider: self.build_provider(),
            model: self.model.clone(),
            registry: tools::build_tools(),
            permissions: self.permissions.clone(),
            shared_mode,
            cancel,
            steering,
        }
    }

    fn spawn_agent(
        &self,
        tx: mpsc::UnboundedSender<AgentEvent>,
        ctx: AgentContext,
    ) -> tokio::task::JoinHandle<Vec<Message>> {
        let mode = self.mode;
        let history = self.history.clone();
        tokio::spawn(async move { run_agent(&ctx, mode, &history, &tx).await })
    }

    fn toggle_mode(&mut self) {
        self.mode = self.mode.toggle();
        self.app_state.set_mode(self.mode);
        self.screen.mark_dirty();
    }

    fn set_reasoning_effort(&mut self, effort: crate::provider::ReasoningEffort) {
        self.reasoning_effort = effort;
        self.screen.set_reasoning_effort(effort);
        self.app_state.set_reasoning_effort(effort);
    }

    pub fn render_screen(&mut self) {
        self.screen.draw_prompt_with_queued(
            &self.input,
            self.mode,
            render::term_width(),
            &self.queued_messages,
        );
    }

    pub fn handle_agent_event(
        &mut self,
        ev: AgentEvent,
        pending: &mut Option<PendingTool>,
        steered_count: &mut usize,
    ) -> SessionControl {
        match ev {
            AgentEvent::TokenUsage { prompt_tokens } => {
                if prompt_tokens > 0 {
                    self.screen.set_context_tokens(prompt_tokens);
                }
                self.screen.set_throbber(render::Throbber::Working);
                SessionControl::Continue
            }
            AgentEvent::ToolOutputChunk(chunk) => {
                self.screen.append_active_output(&chunk);
                SessionControl::Continue
            }
            AgentEvent::Steered { text, count } => {
                let drain_n = count.min(self.queued_messages.len());
                self.queued_messages.drain(..drain_n);
                *steered_count = steered_count.saturating_sub(drain_n);
                self.screen.push(Block::User { text });
                SessionControl::Continue
            }
            AgentEvent::Thinking(content) => {
                self.screen.push(Block::Thinking { content });
                SessionControl::Continue
            }
            AgentEvent::Text(content) => {
                self.screen.push(Block::Text { content });
                SessionControl::Continue
            }
            AgentEvent::ToolCall { name, args } => {
                let summary = tool_arg_summary(&name, &args);
                self.screen.start_tool(name.clone(), summary, args);
                *pending = Some(PendingTool { name });
                SessionControl::Continue
            }
            AgentEvent::ToolResult { content, is_error } => {
                if pending.is_some() {
                    let status = if is_error {
                        ToolStatus::Err
                    } else {
                        ToolStatus::Ok
                    };
                    let output = Some(ToolOutput { content, is_error });
                    self.screen.finish_tool(status, output);
                }
                *pending = None;
                SessionControl::Continue
            }
            AgentEvent::Confirm {
                desc,
                args,
                approval_pattern,
                reply,
            } => SessionControl::NeedsConfirm {
                desc,
                args,
                approval_pattern,
                reply,
            },
            AgentEvent::AskQuestion { args, reply } => {
                SessionControl::NeedsAskQuestion { args, reply }
            }
            AgentEvent::Retrying { delay, attempt } => {
                self.screen
                    .set_throbber(render::Throbber::Retrying { delay, attempt });
                SessionControl::Continue
            }
            AgentEvent::Done => SessionControl::Done,
            AgentEvent::Error(e) => {
                self.screen.push(Block::Error { message: e });
                SessionControl::Done
            }
        }
    }

    /// Resolve a completed confirm dialog choice.
    /// Returns `true` if the agent should be cancelled.
    fn resolve_confirm(
        &mut self,
        (choice, message): (ConfirmChoice, Option<String>),
        reply: tokio::sync::oneshot::Sender<(bool, Option<String>)>,
        tool_name: &str,
        agent: &mut Option<AgentState>,
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
                let _ = reply.send((true, message));
                false
            }
            ConfirmChoice::Always => {
                self.auto_approved.insert(tool_name.to_string(), vec![]);
                self.screen.set_active_status(ToolStatus::Pending);
                let _ = reply.send((true, message));
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
                let _ = reply.send((true, message));
                false
            }
            ConfirmChoice::No => {
                let _ = reply.send((false, message));
                self.screen.finish_tool(ToolStatus::Denied, None);
                if let Some(ref mut ag) = agent {
                    ag.pending = None;
                }
                true
            }
        }
    }

    /// Resolve a completed question dialog.
    /// `answer` is `Some(json)` on confirm, `None` on cancel.
    /// Returns `true` if the agent should be cancelled.
    fn resolve_question(
        &mut self,
        answer: Option<String>,
        reply: tokio::sync::oneshot::Sender<String>,
        agent: &mut Option<AgentState>,
    ) -> bool {
        match answer {
            Some(json) => {
                let _ = reply.send(json);
                false
            }
            None => {
                let _ = reply.send("User cancelled the question.".into());
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
                desc,
                args,
                approval_pattern,
                reply,
            } => {
                // Yolo mode: auto-approve everything.
                if self.mode == Mode::Yolo {
                    let _ = reply.send((true, None));
                    return LoopAction::Continue;
                }

                let tool_name = pending.as_ref().map(|p| p.name.clone()).unwrap_or_default();

                // Check auto-approvals first (doesn't need UI).
                if let Some(patterns) = self.auto_approved.get(&tool_name) {
                    if patterns.is_empty() || patterns.iter().any(|p| p.matches(&desc)) {
                        let _ = reply.send((true, None));
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
                        reply,
                    });
                    return LoopAction::Continue;
                }

                // Show dialog immediately (non-blocking).
                self.screen.set_active_status(ToolStatus::Confirm);
                self.render_screen();
                *active_dialog = Some(ActiveDialog::Confirm {
                    dialog: ConfirmDialog::new(
                        &tool_name,
                        &desc,
                        &args,
                        approval_pattern.as_deref(),
                    ),
                    tool_name,
                    reply,
                });
                LoopAction::Continue
            }
            SessionControl::NeedsAskQuestion { args, reply } => {
                // If the user is actively typing, defer the dialog.
                let recently_typed = last_keypress
                    .is_some_and(|t| t.elapsed() < Duration::from_millis(CONFIRM_DEFER_MS));
                if recently_typed && !self.input.buf.is_empty() {
                    self.screen.set_pending_dialog(true);
                    *deferred_dialog = Some(DeferredDialog::AskQuestion { args, reply });
                    return LoopAction::Continue;
                }

                // Show dialog immediately (non-blocking).
                self.render_screen();
                let questions = render::parse_questions(&args);
                *active_dialog = Some(ActiveDialog::AskQuestion {
                    dialog: QuestionDialog::new(questions),
                    reply,
                });
                LoopAction::Continue
            }
        }
    }

    fn tick(&mut self, agent_running: bool, has_dialog: bool) {
        // Skip prompt/spinner render when a dialog overlay is showing —
        // the dialog covers the bottom and drawing underneath causes flicker.
        if has_dialog {
            return;
        }
        if agent_running {
            self.render_screen();
        } else {
            self.screen
                .draw_prompt(&self.input, self.mode, render::term_width());
        }
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
        desc: String,
        args: HashMap<String, serde_json::Value>,
        approval_pattern: Option<String>,
        reply: tokio::sync::oneshot::Sender<(bool, Option<String>)>,
    },
    NeedsAskQuestion {
        args: HashMap<String, serde_json::Value>,
        reply: tokio::sync::oneshot::Sender<String>,
    },
    Done,
}

enum LoopAction {
    Continue,
    Done,
}

/// Intermediate type for dispatching dialog results in dispatch_terminal_event.
enum DialogOutcome {
    Confirm(ConfirmChoice, Option<String>),
    Question(Option<String>), // Some(json) on confirm, None on cancel
}

impl DialogOutcome {
    fn into_confirm(self) -> (ConfirmChoice, Option<String>) {
        match self {
            DialogOutcome::Confirm(c, m) => (c, m),
            _ => unreachable!(),
        }
    }

    fn into_question(self) -> Option<String> {
        match self {
            DialogOutcome::Question(a) => a,
            _ => unreachable!(),
        }
    }
}

pub struct PendingTool {
    pub name: String,
}

fn draw_active_dialog(dlg: &mut Option<ActiveDialog>) {
    if let Some(dlg) = dlg {
        match dlg {
            ActiveDialog::Confirm { dialog, .. } => dialog.draw(),
            ActiveDialog::AskQuestion { dialog, .. } => dialog.draw(),
        }
    }
}
