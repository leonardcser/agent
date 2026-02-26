mod agent;
pub mod completer;
mod config;
pub mod input;
mod permissions;
mod provider;
pub mod render;
mod state;
mod theme;
mod tools;

use agent::{run_agent, AgentEvent};
use clap::Parser;
use crossterm::{
    cursor,
    event::{self, EnableBracketedPaste, DisableBracketedPaste},
    terminal, ExecutableCommand,
};
use input::{char_pos, handle_term_event, read_input, History, Mode};
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
    pre_buf: String,
    pre_cursor: usize,
    pastes: Vec<String>,
    queued_messages: Vec<String>,
    auto_approved: HashSet<String>,
}

impl App {
    fn new(api_base: String, api_key: String, model: String) -> Self {
        let app_state = state::State::load();
        let mode = app_state.mode();
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
            pre_buf: String::new(),
            pre_cursor: 0,
            pastes: Vec::new(),
            queued_messages: Vec::new(),
            auto_approved: HashSet::new(),
        }
    }

    fn read_input(&mut self) -> Option<String> {
        let initial = std::mem::take(&mut self.pre_buf);
        let cursor = std::mem::replace(&mut self.pre_cursor, 0);
        let result = read_input(&mut self.screen, &mut self.mode, &mut self.input_history, initial, cursor, &mut self.pastes);
        self.pastes.clear();
        result
    }

    /// Handle a user command. Returns false if the app should exit.
    fn handle_command(&mut self, input: &str) -> bool {
        match input {
            "/exit" | "/quit" => return false,
            "/clear" | "/new" => {
                self.history.clear();
                self.auto_approved.clear();
                self.screen.clear();
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
        let cursor_char = char_pos(&self.pre_buf, self.pre_cursor);
        let mut out = io::stdout();
        let _ = out.execute(cursor::Hide);
        self.screen.render(&self.pre_buf, cursor_char, self.mode, render::term_width(), &self.queued_messages, &self.pastes);
        let _ = out.execute(cursor::Show);
        let _ = out.flush();
    }

    fn handle_agent_event(&mut self, ev: AgentEvent) -> EventResult {
        match ev {
            AgentEvent::TokenUsage { prompt_tokens } => {
                self.screen.set_context_tokens(prompt_tokens);
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
                    summary: summary.clone(),
                    args: args.clone(),
                    status: ToolStatus::Pending,
                    elapsed: None,
                    output: None,
                });
                EventResult::ToolStarted { name }
            }
            AgentEvent::ToolResult { content, is_error } => {
                EventResult::ToolFinished { content, is_error }
            }
            AgentEvent::Confirm { desc, reply } => {
                EventResult::NeedsConfirm { desc, reply }
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
        reply: tokio::sync::oneshot::Sender<bool>,
    ) -> ConfirmAction {
        if self.auto_approved.contains(tool_name) {
            let _ = reply.send(true);
            return ConfirmAction::Approved;
        }

        // Mark the tool as awaiting confirmation and flush.
        self.screen.set_last_tool_status(ToolStatus::Confirm);
        self.render_screen();
        self.screen.erase_prompt();
        let choice = render::show_confirm(tool_name, desc);

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

    fn update_tool_result(
        &mut self,
        pending: &PendingTool,
        content: String,
        is_error: bool,
        denied: bool,
    ) {
        let elapsed = pending.start.elapsed();
        let status = if denied {
            ToolStatus::Denied
        } else if is_error {
            ToolStatus::Err
        } else {
            ToolStatus::Ok
        };

        let output = if !denied {
            Some(ToolOutput { content, is_error })
        } else {
            None
        };

        self.screen.update_last_tool(
            status,
            output,
            if pending.name == "bash" { Some(elapsed) } else { None },
        );
    }

    fn handle_terminal_event(&mut self, ev: event::Event, last_esc: &mut Option<Instant>, resize_at: &mut Option<Instant>) -> TermAction {
        if matches!(ev, event::Event::Resize(..)) {
            self.screen.erase_prompt();
            *resize_at = Some(Instant::now());
        }
        let (needs_redraw, cancel) = handle_term_event(ev, &mut self.pre_buf, &mut self.pre_cursor, &mut self.mode, last_esc, &mut self.queued_messages, &mut self.pastes);
        if cancel {
            self.screen.erase_prompt();
            return TermAction::Cancel;
        }
        if needs_redraw {
            self.render_screen();
        }
        TermAction::None
    }

    fn tick(&mut self, resize_at: &mut Option<Instant>) {
        if let Some(t) = *resize_at {
            if t.elapsed() >= Duration::from_millis(150) {
                self.screen.redraw_all();
                *resize_at = None;
            } else {
                return; // Still debouncing — don't render yet.
            }
        }
        self.render_screen();
    }

    fn finish_turn(&mut self, cancelled: bool, cancel_token: CancellationToken, agent_handle: tokio::task::JoinHandle<Vec<Message>>) -> tokio::task::JoinHandle<Vec<Message>> {
        if cancelled {
            self.screen.set_throbber(render::Throbber::Interrupted);
            self.pre_buf.clear();
            self.pre_cursor = 0;
            self.pastes.clear();
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
    ToolFinished {
        content: String,
        is_error: bool,
    },
    NeedsConfirm {
        desc: String,
        reply: tokio::sync::oneshot::Sender<bool>,
    },
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

    let mut app = App::new(api_base, api_key, model);

    println!();
    loop {
        // If there are queued messages from while the agent was working, auto-send them
        let input = if !app.queued_messages.is_empty() {
            let mut parts = std::mem::take(&mut app.queued_messages);
            let buf = std::mem::take(&mut app.pre_buf);
            app.pre_cursor = 0;
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
        if input.is_empty() {
            continue;
        }
        app.input_history.push(input.clone());
        if !app.handle_command(&input) {
            break;
        }
        if input.starts_with('/') {
            continue;
        }

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

        loop {
            // Process all pending agent events
            loop {
                match rx.try_recv() {
                    Ok(ev) => {
                        match app.handle_agent_event(ev) {
                            EventResult::Continue => {}
                            EventResult::ToolStarted { name } => {
                                pending = Some(PendingTool {
                                    name,
                                    start: Instant::now(),
                                    denied: false,
                                });
                            }
                            EventResult::ToolFinished { content, is_error } => {
                                if let Some(ref p) = pending {
                                    app.update_tool_result(p, content, is_error, p.denied);
                                }
                                pending = None;
                            }
                            EventResult::NeedsConfirm { desc, reply } => {
                                let tool_name = pending.as_ref().map(|p| p.name.as_str()).unwrap_or("");
                                match app.handle_confirm(tool_name, &desc, reply) {
                                    ConfirmAction::Approved => {
                                        if let Some(ref mut p) = pending {
                                            p.start = Instant::now();
                                        }
                                    }
                                    ConfirmAction::Denied => {
                                        cancelled = true;
                                        agent_done = true;
                                        break;
                                    }
                                }
                            }
                            EventResult::Done => {
                                agent_done = true;
                                break;
                            }
                        }
                    }
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        agent_done = true;
                        break;
                    }
                }
            }

            if agent_done { break; }

            // Process terminal events
            while event::poll(Duration::ZERO).unwrap_or(false) {
                if let Ok(ev) = event::read() {
                    if let TermAction::Cancel = app.handle_terminal_event(ev, &mut last_esc, &mut resize_at) {
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

        // Final flush: render any remaining dirty blocks, erase prompt
        app.screen.flush_blocks();

        let agent_handle = app.finish_turn(cancelled, cancel_token, agent_handle);

        if let Ok(new_messages) = agent_handle.await {
            app.history = new_messages;
        }
    }
}
