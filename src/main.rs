mod agent;
pub mod completer;
mod config;
pub mod input;
mod log;
mod permissions;
mod provider;
pub mod render;
mod session;
mod state;
mod theme;
mod tools;
pub mod vim;

use agent::{run_agent, AgentEvent};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use clap::Parser;
use crossterm::{
    cursor,
    event::{self, EnableBracketedPaste, DisableBracketedPaste},
    terminal, ExecutableCommand,
};
use input::{Action, EscAction, InputState, History, Mode, read_input, resolve_agent_esc};
use provider::{Message, Provider, Role};
use render::{tool_arg_summary, Block, ConfirmChoice, Screen, ToolOutput, ToolStatus, ResumeEntry};
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
    #[arg(long, default_value = "info", value_name = "LEVEL")]
    log_level: String,
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
    session: session::Session,
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
            session: session::Session::new(),
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
            _ => {}
        }
        true
    }

    fn reset_session(&mut self) {
        self.history.clear();
        self.auto_approved.clear();
        self.queued_messages.clear();
        self.screen.clear();
        self.input.clear();
        self.session = session::Session::new();
    }

    fn resume_session(&mut self) {
        let sessions = session::list_sessions();
        if sessions.is_empty() {
            self.screen.push(Block::Error { message: "no saved sessions".into() });
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
        }
    }

    fn rebuild_screen_from_history(&mut self) {
        self.screen.clear();
        if self.history.is_empty() {
            return;
        }

        let mut tool_outputs: HashMap<String, ToolOutput> = HashMap::new();
        for msg in &self.history {
            if matches!(msg.role, Role::Tool) {
                if let Some(ref id) = msg.tool_call_id {
                    let content = msg.content.clone().unwrap_or_default();
                    tool_outputs.insert(id.clone(), ToolOutput { content, is_error: false });
                }
            }
        }

        for msg in &self.history {
            match msg.role {
                Role::User => {
                    if let Some(ref content) = msg.content {
                        self.screen.push(Block::User { text: content.clone() });
                    }
                }
                Role::Assistant => {
                    if let Some(ref content) = msg.content {
                        if !content.is_empty() {
                            self.screen.push(Block::Text { content: content.clone() });
                        }
                    }
                    if let Some(ref calls) = msg.tool_calls {
                        for tc in calls {
                            let args: HashMap<String, serde_json::Value> =
                                serde_json::from_str(&tc.function.arguments).unwrap_or_default();
                            let summary = tool_arg_summary(&tc.function.name, &args);
                            let output = tool_outputs.get(&tc.id).cloned();
                            let status = if output.is_some() { ToolStatus::Ok } else { ToolStatus::Pending };
                            self.screen.push(Block::ToolCall {
                                name: tc.function.name.clone(),
                                summary,
                                args,
                                status,
                                elapsed: None,
                                output,
                            });
                        }
                    }
                }
                Role::Tool | Role::System => {}
            }
        }
    }

    fn save_session(&mut self) {
        self.session.messages = self.history.clone();
        self.session.updated_at_ms = session::now_ms();
        session::save(&self.session);
    }

    async fn maybe_generate_title(&mut self) {
        let has_title = self.session.title.as_ref().is_some_and(|t| !t.trim().is_empty());
        if has_title {
            return;
        }
        let Some(first) = self.session.first_user_message.clone() else {
            return;
        };
        let provider = Provider::new(&self.api_base, &self.api_key);
        match provider.complete_title(&first, &self.model).await {
            Ok(title) => {
                if !title.is_empty() {
                    self.session.title = Some(title);
                    self.save_session();
                }
            }
            Err(_) => {
                if self.session.title.is_none() {
                    let fallback = first.lines().next().unwrap_or("Untitled");
                    let mut trimmed = fallback.to_string();
                    if trimmed.len() > 48 {
                        trimmed.truncate(48);
                        trimmed = trimmed.trim().to_string();
                    }
                    self.session.title = Some(trimmed);
                    self.save_session();
                }
            }
        }
    }

    /// Rewind conversation to the turn starting at `block_idx`.
    /// Removes all blocks from `block_idx` onward, truncates history,
    /// and returns the user message text from the rewound turn.
    fn rewind_to(&mut self, block_idx: usize) -> Option<String> {
        let turns = self.screen.user_turns();

        // Find the turn at block_idx and get its text
        let turn_text = turns.iter().find(|(i, _)| *i == block_idx).map(|(_, t)| t.clone());

        // Count how many User blocks exist before block_idx
        let user_turns_to_keep = turns.iter().filter(|(i, _)| *i < block_idx).count();

        // Truncate history: find the Nth user message and cut there
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

    fn show_user_message(&mut self, input: &str) {
        self.screen.push(Block::User { text: input.to_string() });
    }

    fn push_user_message(&mut self, input: String) {
        let expanded = expand_at_refs(&input);
        self.history.push(Message {
            role: Role::User,
            content: Some(expanded),
            tool_calls: None,
            tool_call_id: None,
        });
    }

    fn spawn_agent(&self, tx: mpsc::UnboundedSender<AgentEvent>, cancel: CancellationToken, steering: Arc<Mutex<Vec<String>>>) -> tokio::task::JoinHandle<Vec<Message>> {
        let api_base = self.api_base.clone();
        let api_key = self.api_key.clone();
        let model = self.model.clone();
        let mode = self.mode;
        let permissions = self.permissions.clone();
        let history = self.history.clone();

        tokio::spawn(async move {
            let provider = Provider::new(&api_base, &api_key);
            let registry = tools::build_tools();
            run_agent(&provider, &model, &history, &registry, mode, &permissions, &tx, cancel, steering).await
        })
    }

    fn render_screen(&mut self) {
        let mut out = io::stdout();
        let _ = out.execute(cursor::Hide);
        self.screen.draw_prompt_with_queued(&self.input, self.mode, render::term_width(), &self.queued_messages);
        let _ = out.execute(cursor::Show);
        let _ = out.flush();
    }

    fn handle_agent_event(&mut self, ev: AgentEvent, pending: &mut Option<PendingTool>, steered_count: &mut usize) -> SessionControl {
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
                // Remove the injected messages from the display queue and
                // adjust the sync counter accordingly.
                let drain_n = count.min(self.queued_messages.len());
                self.queued_messages.drain(..drain_n);
                *steered_count = steered_count.saturating_sub(drain_n);
                self.screen.push(Block::User { text });
                SessionControl::Continue
            }
            AgentEvent::Text(content) => {
                self.screen.push(Block::Text { content });
                SessionControl::Continue
            }
            AgentEvent::ToolCall { name, args } => {
                let summary = tool_arg_summary(&name, &args);
                self.screen.start_tool(name.clone(), summary, args);
                *pending = Some(PendingTool { name, start: Instant::now() });
                SessionControl::Continue
            }
            AgentEvent::ToolResult { content, is_error } => {
                if let Some(ref p) = pending {
                    let elapsed = p.start.elapsed();
                    let status = if is_error { ToolStatus::Err } else { ToolStatus::Ok };
                    let output = Some(ToolOutput { content, is_error });
                    self.screen.finish_tool(
                        status,
                        output,
                        if p.name == "bash" { Some(elapsed) } else { None },
                    );
                }
                *pending = None;
                SessionControl::Continue
            }
            AgentEvent::Confirm { desc, args, reply } => {
                SessionControl::NeedsConfirm { desc, args, reply }
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

        self.screen.set_active_status(ToolStatus::Confirm);
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

        if matches!(ev, event::Event::Key(crossterm::event::KeyEvent { code: crossterm::event::KeyCode::Char('c'), modifiers: crossterm::event::KeyModifiers::CONTROL, .. })) {
            let double_tap = last_ctrlc.map_or(false, |prev| prev.elapsed() < Duration::from_millis(500));
            if self.input.buf.is_empty() || double_tap {
                *last_ctrlc = None;
                self.screen.mark_dirty();
                return TermAction::Cancel;
            }
            *last_ctrlc = Some(Instant::now());
            self.input.clear();
            self.queued_messages.clear();
            self.render_screen();
            return TermAction::None;
        }

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
                    self.screen.mark_dirty();
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

    async fn run_session(&mut self) {
        terminal::enable_raw_mode().ok();
        let _ = io::stdout().execute(EnableBracketedPaste);
        self.screen.set_throbber(render::Throbber::Working);
        self.render_screen();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let cancel_token = CancellationToken::new();
        let steering: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let agent_handle = self.spawn_agent(tx, cancel_token.clone(), steering.clone());

        let mut pending: Option<PendingTool> = None;
        let mut steered_count: usize = 0; // how many queued_messages have been synced to steering
        let mut agent_done = false;
        let mut resize_at: Option<Instant> = None;
        let mut cancelled = false;
        let mut last_esc: Option<Instant> = None;
        let mut vim_mode_at_esc: Option<vim::ViMode> = None;
        let mut last_ctrlc: Option<Instant> = None;

        loop {
            // Drain agent events
            loop {
                match rx.try_recv() {
                    Ok(ev) => match self.handle_agent_event(ev, &mut pending, &mut steered_count) {
                        SessionControl::Continue => {}
                        SessionControl::NeedsConfirm { desc, args, reply } => {
                            let tool_name = pending.as_ref().map(|p| p.name.as_str()).unwrap_or("");
                            match self.handle_confirm(tool_name, &desc, &args, reply) {
                                ConfirmAction::Approved => {
                                    if let Some(ref mut p) = pending { p.start = Instant::now(); }
                                }
                                ConfirmAction::Denied => {
                                    self.screen.finish_tool(ToolStatus::Denied, None, None);
                                    pending = None;
                                    cancelled = true;
                                    agent_done = true;
                                    break;
                                }
                            }
                        }
                        SessionControl::Done => { agent_done = true; break; }
                    },
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => { agent_done = true; break; }
                }
            }

            if agent_done { break; }

            // Drain terminal events
            while event::poll(Duration::ZERO).unwrap_or(false) {
                if let Ok(ev) = event::read() {
                    if let TermAction::Cancel = self.handle_terminal_event(
                        ev, &mut last_esc, &mut vim_mode_at_esc, &mut last_ctrlc, &mut resize_at,
                    ) {
                        cancelled = true;
                        agent_done = true;
                        break;
                    }
                }
            }

            if agent_done { break; }

            // Sync any newly queued messages into the steering queue without
            // removing them from queued_messages — they stay visible in the
            // prompt until the agent actually injects them (Steered event).
            if self.queued_messages.len() > steered_count {
                let new = self.queued_messages[steered_count..].to_vec();
                steering.lock().unwrap().extend(new);
                steered_count = self.queued_messages.len();
            }

            self.tick(&mut resize_at);
            tokio::time::sleep(Duration::from_millis(80)).await;
        }

        self.screen.flush_blocks();
        if cancelled {
            self.screen.set_throbber(render::Throbber::Interrupted);
            // Restore any messages that were queued but not yet injected into the
            // agent back to the input prompt so the user can edit and resend them.
            let mut leftover: Vec<String> = steering.lock().unwrap().drain(..).collect();
            leftover.extend(self.queued_messages.drain(..));
            if !leftover.is_empty() {
                let mut combined = leftover.join("\n");
                if !self.input.buf.is_empty() {
                    combined.push('\n');
                    combined.push_str(&self.input.buf);
                }
                self.input.buf = combined;
                self.input.cpos = self.input.buf.len();
            }
            cancel_token.cancel();
            agent_handle.abort();
            self.render_screen();
        } else {
            self.screen.set_throbber(render::Throbber::Done);
            if let Ok(new_messages) = agent_handle.await {
                self.history = new_messages;
            }
        }
        let _ = io::stdout().execute(DisableBracketedPaste);
        terminal::disable_raw_mode().ok();
        self.app_state.set_mode(self.mode);
    }
}

enum SessionControl {
    Continue,
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

    if let Some(level) = log::parse_level(&args.log_level) {
        log::set_level(level);
    } else {
        eprintln!("warning: invalid --log-level {}, defaulting to info", args.log_level);
    }

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

        // Handle rewind signal from double-Esc menu
        if let Some(idx_str) = input.strip_prefix("\x00rewind:") {
            if let Ok(block_idx) = idx_str.parse::<usize>() {
                if let Some(text) = app.rewind_to(block_idx) {
                    app.input.buf = text;
                    app.input.cpos = app.input.buf.len();
                }
            }
            continue;
        }

        app.input_history.push(input.clone());
        if !app.handle_command(&input) { break; }
        if input.starts_with('/') { continue; }

        if let Some(cmd) = input.strip_prefix('!') {
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
                            if !s.is_empty() { s.push('\n'); }
                            s.push_str(&stderr);
                        }
                        s.truncate(s.trim_end().len());
                        s
                    })
                    .unwrap_or_else(|e| format!("error: {}", e));
                app.screen.push(Block::Exec { command: cmd.to_string(), output });
            }
            continue;
        }

        app.screen.begin_turn();
        app.show_user_message(&input);
        if app.session.first_user_message.is_none() {
            app.session.first_user_message = Some(input.clone());
        }
        app.push_user_message(input);
        app.run_session().await;
        app.save_session();
        app.maybe_generate_title().await;
    }
}

/// Expand `@path` references in user input by appending file contents.
fn expand_at_refs(input: &str) -> String {
    let mut refs: Vec<String> = Vec::new();
    let mut chars = input.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c != '@' {
            continue;
        }
        // Collect non-whitespace chars after @
        let start = i + 1;
        let mut end = start;
        while let Some(&(j, nc)) = chars.peek() {
            if nc.is_whitespace() {
                break;
            }
            end = j + nc.len_utf8();
            chars.next();
        }
        if end > start {
            let path = &input[start..end];
            if std::path::Path::new(path).exists() {
                refs.push(path.to_string());
            }
        }
    }

    if refs.is_empty() {
        return input.to_string();
    }

    let mut result = input.to_string();
    for path in &refs {
        if let Ok(contents) = std::fs::read_to_string(path) {
            result.push_str(&format!("\n\nContents of {}:\n```\n{}\n```", path, contents));
        }
    }
    result
}
