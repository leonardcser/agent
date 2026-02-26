mod agent;
pub mod completer;
mod config;
pub mod input;
mod log;
mod permissions;
mod provider;
pub mod render;
mod state;
mod theme;
mod tools;
pub mod vim;

use agent::{run_agent, AgentEvent};
use std::collections::HashMap;
use clap::Parser;
use crossterm::{
    cursor,
    event::{self, EnableBracketedPaste, DisableBracketedPaste},
    terminal, ExecutableCommand,
};
use input::{Action, EscAction, InputState, History, Mode, read_input, resolve_agent_esc};
use provider::{Message, Provider, Role};
use render::{tool_arg_summary, Block, ConfirmChoice, Screen, ToolOutput, ToolStatus};
use std::collections::HashSet;
use std::io::{self, Write};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use tokio::sync::mpsc;

#[derive(Parser)]
#[command(name = "agent", about = "Coding agent TUI")]
struct Args {
    #[arg(long)]
    api_base: Option<String>,
    #[arg(long)]
    api_key: Option<String>,
    #[arg(long)]
    api_key_env: Option<String>,
    #[arg(long)]
    model: Option<String>,
}

struct App {
    api_base: String,
    api_key: String,
    model: String,
    mode: Mode,
    permissions: permissions::Permissions,
    screen: Screen,
    history: Vec<Message>,
    input_history: History,
    app_state: state::State,
    input: InputState,
    queued_messages: Vec<String>,
    auto_approved: HashSet<String>,
}

impl App {
    fn new(api_base: String, api_key: String, model: String, vim_from_config: bool) -> Self {
        let app_state = state::State::load();
        let mode = app_state.mode();
        let vim_enabled = app_state.vim_enabled() || vim_from_config;
        let mut input = InputState::new();
        if vim_enabled {
            input.set_vim_enabled(true);
        }
        Self {
            api_base,
            api_key,
            model,
            mode,
            permissions: permissions::Permissions::load(),
            screen: Screen::new(),
            history: Vec::new(),
            input_history: History::load(),
            app_state,
            input,
            queued_messages: Vec::new(),
            auto_approved: HashSet::new(),
        }
    }

    fn read_input(&mut self) -> Option<String> {
        let result = read_input(&mut self.screen, &mut self.mode, &mut self.input_history, &mut self.input);
        self.input.clear();
        result
    }

    fn handle_command(&mut self, input: &str) -> bool {
        match input {
            "/exit" | "/quit" => return false,
            "/clear" | "/new" => {
                self.history.clear();
                self.auto_approved.clear();
                self.screen.clear();
            }
            "/vim" => {
                let enabled = !self.input.vim_enabled();
                self.input.set_vim_enabled(enabled);
                self.app_state.set_vim_enabled(enabled);
            }
            _ => {}
        }
        true
    }

    fn show_user_message(&mut self, input: &str) {
        self.screen.push(Block::User { text: input.to_string() });
    }

    fn push_user_message(&mut self, input: String) {
        self.history.push(Message {
            role: Role::User,
            content: Some(input),
            tool_calls: None,
            tool_call_id: None,
        });
    }

    fn spawn_agent(&self, tx: mpsc::UnboundedSender<AgentEvent>, cancel: CancellationToken) -> tokio::task::JoinHandle<Vec<Message>> {
        let api_base = self.api_base.clone();
        let api_key = self.api_key.clone();
        let model = self.model.clone();
        let mode = self.mode;
        let permissions = self.permissions.clone();
        let history = self.history.clone();

        tokio::spawn(async move {
            let provider = Provider::new(&api_base, &api_key);
            let registry = match mode {
                Mode::Apply => tools::apply_tools(),
                Mode::Normal => tools::normal_tools(),
            };
            run_agent(&provider, &model, &history, &registry, mode, &permissions, &tx, cancel).await
        })
    }

    fn render_screen(&mut self) {
        let mut out = io::stdout();
        let _ = out.execute(cursor::Hide);
        self.screen.draw_prompt_with_queued(&self.input, self.mode, render::term_width(), &self.queued_messages);
        let _ = out.execute(cursor::Show);
        let _ = out.flush();
    }

    fn handle_agent_event(&mut self, ev: AgentEvent) -> EventResult {
        match ev {
            AgentEvent::TokenUsage { prompt_tokens } => {
                self.screen.set_context_tokens(prompt_tokens);
                // A successful response clears any "retrying" throbber state
                self.screen.set_throbber(render::Throbber::Working);
                EventResult::Continue
            }
            AgentEvent::ToolOutputChunk(chunk) => {
                self.screen.append_tool_output(&chunk);
                EventResult::Continue
            }
            AgentEvent::Text(content) => {
                self.screen.push(Block::Text { content });
                EventResult::Continue
            }
            AgentEvent::ToolCall { name, args } => {
                let summary = tool_arg_summary(&name, &args);
                self.screen.push(Block::ToolCall {
                    name: name.clone(),
                    summary,
                    args,
                    status: ToolStatus::Pending,
                    elapsed: None,
                    output: None,
                });
                EventResult::ToolStarted { name }
            }
            AgentEvent::ToolResult { content, is_error } => {
                EventResult::ToolFinished { content, is_error }
            }
            AgentEvent::Confirm { desc, args, reply } => {
                EventResult::NeedsConfirm { desc, args, reply }
            }
            AgentEvent::Retrying(delay) => {
                self.screen.set_throbber(render::Throbber::Retrying(delay));
                EventResult::Continue
            }
            AgentEvent::Done => EventResult::Done,
            AgentEvent::Error(e) => {
                self.screen.push(Block::Error { message: e });
                EventResult::Done
            }
        }
    }

    fn handle_confirm(
        &mut self,
        tool_name: &str,
        desc: &str,
        args: &HashMap<String, serde_json::Value>,
        reply: tokio::sync::oneshot::Sender<bool>,
    ) -> ConfirmAction {
        if self.auto_approved.contains(tool_name) {
            let _ = reply.send(true);
            return ConfirmAction::Approved;
        }

        self.screen.set_last_tool_status(ToolStatus::Confirm);
        self.render_screen();
        self.screen.erase_prompt();
        let choice = render::show_confirm(tool_name, desc, args);
        self.screen.redraw_all();
        self.render_screen();

        match choice {
            ConfirmChoice::Yes => {
                let _ = reply.send(true);
                ConfirmAction::Approved
            }
            ConfirmChoice::Always => {
                self.auto_approved.insert(tool_name.to_string());
                let _ = reply.send(true);
                ConfirmAction::Approved
            }
            ConfirmChoice::No => {
                let _ = reply.send(false);
                ConfirmAction::Denied
            }
        }
    }

    fn update_tool_result(&mut self, pending: &PendingTool, content: String, is_error: bool, denied: bool) {
        let elapsed = pending.start.elapsed();
        let status = if denied { ToolStatus::Denied } else if is_error { ToolStatus::Err } else { ToolStatus::Ok };
        let output = if !denied { Some(ToolOutput { content, is_error }) } else { None };
        self.screen.update_last_tool(
            status,
            output,
            if pending.name == "bash" { Some(elapsed) } else { None },
        );
    }

    fn handle_terminal_event(
        &mut self,
        ev: event::Event,
        last_esc: &mut Option<Instant>,
        vim_mode_at_esc: &mut Option<vim::ViMode>,
        last_ctrlc: &mut Option<Instant>,
        resize_at: &mut Option<Instant>,
    ) -> TermAction {
        if matches!(ev, event::Event::Resize(..)) {
            self.screen.erase_prompt();
            *resize_at = Some(Instant::now());
        }

        // Ctrl+C: if empty or double-tap, cancel; otherwise clear prompt.
        if matches!(ev, event::Event::Key(crossterm::event::KeyEvent { code: crossterm::event::KeyCode::Char('c'), modifiers: crossterm::event::KeyModifiers::CONTROL, .. })) {
            let double_tap = last_ctrlc.map_or(false, |prev| prev.elapsed() < Duration::from_millis(500));
            if self.input.buf.is_empty() || double_tap {
                *last_ctrlc = None;
                self.screen.erase_prompt();
                return TermAction::Cancel;
            }
            *last_ctrlc = Some(Instant::now());
            self.input.clear();
            self.queued_messages.clear();
            self.render_screen();
            return TermAction::None;
        }

        // Agent-mode Esc handling: vim mode switch, unqueue, or double-Esc cancel.
        if matches!(ev, event::Event::Key(crossterm::event::KeyEvent { code: crossterm::event::KeyCode::Esc, .. })) {
            match resolve_agent_esc(
                self.input.vim_mode(),
                !self.queued_messages.is_empty(),
                last_esc,
                vim_mode_at_esc,
            ) {
                EscAction::VimToNormal => {
                    self.input.handle_event(ev, None);
                    self.screen.mark_dirty();
                    self.render_screen();
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
                    self.render_screen();
                }
                EscAction::Cancel { restore_vim } => {
                    if let Some(mode) = restore_vim {
                        self.input.set_vim_mode(mode);
                    }
                    self.screen.erase_prompt();
                    return TermAction::Cancel;
                }
                EscAction::StartTimer => {}
            }
            return TermAction::None;
        }

        match self.input.handle_event(ev, None) {
            Action::Submit(text) => {
                if !text.is_empty() {
                    self.queued_messages.push(text);
                }
                self.screen.mark_dirty();
                self.render_screen();
            }
            Action::Cancel => {
                self.screen.erase_prompt();
                return TermAction::Cancel;
            }
            Action::ToggleMode => {
                self.mode = self.mode.toggle();
                self.screen.mark_dirty();
                self.render_screen();
            }
            Action::Redraw => {
                self.screen.mark_dirty();
                self.render_screen();
            }
            Action::Resize(_) | Action::Noop => {}
        }
        TermAction::None
    }

    fn tick(&mut self, resize_at: &mut Option<Instant>) {
        if let Some(t) = *resize_at {
            if t.elapsed() >= Duration::from_millis(150) {
                self.screen.redraw_all();
                *resize_at = None;
            } else {
                return;
            }
        }
        self.render_screen();
    }

    fn finish_turn(&mut self, cancelled: bool, cancel_token: CancellationToken, agent_handle: tokio::task::JoinHandle<Vec<Message>>) -> tokio::task::JoinHandle<Vec<Message>> {
        if cancelled {
            self.screen.set_throbber(render::Throbber::Interrupted);
            self.input.clear();
            self.queued_messages.clear();
            cancel_token.cancel();
            agent_handle.abort();
        } else {
            self.screen.set_throbber(render::Throbber::Done);
        }
        let _ = io::stdout().execute(DisableBracketedPaste);
        terminal::disable_raw_mode().ok();
        self.app_state.set_mode(self.mode);
        agent_handle
    }
}

enum EventResult {
    Continue,
    ToolStarted { name: String },
    ToolFinished { content: String, is_error: bool },
    NeedsConfirm { desc: String, args: HashMap<String, serde_json::Value>, reply: tokio::sync::oneshot::Sender<bool> },
    Done,
}

enum ConfirmAction {
    Approved,
    Denied,
}

enum TermAction {
    None,
    Cancel,
}

struct PendingTool {
    name: String,
    start: Instant,
    denied: bool,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let cfg = config::Config::load();

    let api_base = args.api_base
        .or(cfg.api_base)
        .unwrap_or_else(|| "http://localhost:11434/v1".into());
    let api_key_env = args.api_key_env.or(cfg.api_key_env).unwrap_or_default();
    let api_key = args.api_key
        .or(cfg.api_key)
        .unwrap_or_else(|| std::env::var(&api_key_env).unwrap_or_default());
    let model = args.model
        .or(cfg.model)
        .expect("model must be set via --model or config file");

    let vim_enabled = cfg.vim_mode.unwrap_or(false);
    let mut app = App::new(api_base, api_key, model, vim_enabled);

    eprintln!("log: {}", log::path().display());
    println!();
    loop {
        let input = if !app.queued_messages.is_empty() {
            let mut parts = std::mem::take(&mut app.queued_messages);
            let buf = std::mem::take(&mut app.input.buf);
            app.input.cpos = 0;
            if !buf.trim().is_empty() {
                parts.push(buf);
            }
            parts.join("\n")
        } else {
            match app.read_input() {
                Some(s) => s,
                None => break,
            }
        };
        app.app_state.set_mode(app.mode);

        let input = input.trim().to_string();
        if input.is_empty() { continue; }
        app.input_history.push(input.clone());
        if !app.handle_command(&input) { break; }
        if input.starts_with('/') { continue; }

        app.screen.begin_turn();
        app.show_user_message(&input);
        app.push_user_message(input);

        terminal::enable_raw_mode().ok();
        let _ = io::stdout().execute(EnableBracketedPaste);
        app.screen.set_throbber(render::Throbber::Working);
        app.render_screen();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let cancel_token = CancellationToken::new();
        let agent_handle = app.spawn_agent(tx, cancel_token.clone());

        let mut pending: Option<PendingTool> = None;
        let mut agent_done = false;
        let mut resize_at: Option<Instant> = None;
        let mut cancelled = false;
        let mut last_esc: Option<Instant> = None;
        let mut vim_mode_at_esc: Option<vim::ViMode> = None;
        let mut last_ctrlc: Option<Instant> = None;

        loop {
            loop {
                match rx.try_recv() {
                    Ok(ev) => {
                        match app.handle_agent_event(ev) {
                            EventResult::Continue => {}
                            EventResult::ToolStarted { name } => {
                                pending = Some(PendingTool { name, start: Instant::now(), denied: false });
                            }
                            EventResult::ToolFinished { content, is_error } => {
                                if let Some(ref p) = pending {
                                    app.update_tool_result(p, content, is_error, p.denied);
                                }
                                pending = None;
                            }
                            EventResult::NeedsConfirm { desc, args, reply } => {
                                let tool_name = pending.as_ref().map(|p| p.name.as_str()).unwrap_or("");
                                match app.handle_confirm(tool_name, &desc, &args, reply) {
                                    ConfirmAction::Approved => {
                                        if let Some(ref mut p) = pending { p.start = Instant::now(); }
                                    }
                                    ConfirmAction::Denied => {
                                        if let Some(ref p) = pending {
                                            app.update_tool_result(p, "denied by user".into(), false, true);
                                        }
                                        pending = None;
                                        cancelled = true;
                                        agent_done = true;
                                        break;
                                    }
                                }
                            }
                            EventResult::Done => { agent_done = true; break; }
                        }
                    }
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => { agent_done = true; break; }
                }
            }

            if agent_done { break; }

            while event::poll(Duration::ZERO).unwrap_or(false) {
                if let Ok(ev) = event::read() {
                    if let TermAction::Cancel = app.handle_terminal_event(ev, &mut last_esc, &mut vim_mode_at_esc, &mut last_ctrlc, &mut resize_at) {
                        cancelled = true;
                        agent_done = true;
                        break;
                    }
                }
            }

            if agent_done { break; }

            app.tick(&mut resize_at);
            tokio::time::sleep(Duration::from_millis(80)).await;
        }

        app.screen.flush_blocks();
        let agent_handle = app.finish_turn(cancelled, cancel_token, agent_handle);
        if let Ok(new_messages) = agent_handle.await {
            app.history = new_messages;
        }
    }
}
