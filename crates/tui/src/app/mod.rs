mod agent;
mod commands;
pub(crate) use commands::copy_to_clipboard;
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
use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ── Tracked agent state ──────────────────────────────────────────────────────

/// A single tool call recorded from a subagent's event stream.
#[derive(Clone)]
pub struct AgentToolEntry {
    pub call_id: String,
    pub tool_name: String,
    pub summary: String,
    pub status: ToolStatus,
    pub elapsed: Option<Duration>,
}

/// State for a spawned subagent (blocking or background).
pub struct TrackedAgent {
    pub agent_id: String,
    pub pid: u32,
    pub prompt: Arc<String>,
    pub slug: Option<String>,
    pub event_rx: tokio::sync::mpsc::UnboundedReceiver<EngineEvent>,
    /// Completed tool calls (for /agents dialog and blocking block rendering).
    pub tool_calls: Vec<AgentToolEntry>,
    pub status: AgentTrackStatus,
    /// Whether the parent LLM is waiting for this agent (blocking spawn).
    pub blocking: bool,
    pub started_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentTrackStatus {
    Working,
    Idle,
    Error,
}

// ── App ──────────────────────────────────────────────────────────────────────

pub struct App {
    pub model: String,
    pub api_base: String,
    pub api_key_env: String,
    pub provider_type: String,
    pub reasoning_effort: ReasoningEffort,
    pub reasoning_cycle: Vec<ReasoningEffort>,
    pub mode: Mode,
    pub mode_cycle: Vec<Mode>,
    pub screen: Screen,
    pub history: Vec<Message>,
    pub input_history: History,
    pub input: InputState,
    exec_rx: Option<tokio::sync::mpsc::UnboundedReceiver<commands::ExecEvent>>,
    exec_kill: Option<std::sync::Arc<tokio::sync::Notify>>,
    pub queued_messages: Vec<String>,
    /// Agent messages waiting to trigger a turn.
    pending_agent_messages: Vec<protocol::Message>,
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
    pub show_tps: bool,
    pub show_tokens: bool,
    pub show_cost: bool,
    pub show_prediction: bool,
    pub show_slug: bool,
    pub restrict_to_workspace: bool,
    pub multi_agent: bool,
    /// Human-readable name for this agent.
    pub agent_id: String,
    /// All tracked subagents (blocking and background).
    pub agents: Vec<TrackedAgent>,
    /// Shared agent snapshots for live dialog updates.
    pub agent_snapshots: render::SharedSnapshots,
    pub available_models: Vec<crate::config::ResolvedModel>,
    pub engine: EngineHandle,
    permissions: Arc<Permissions>,
    /// Context for the currently-open confirm dialog, used to re-check
    /// permissions when the user toggles mode.
    confirm_context: Option<ConfirmContext>,
    /// Ghost text prediction for the input field.
    pub input_prediction: Option<String>,
    /// Monotonic counter to discard stale predictions.
    predict_generation: u64,
    sleep_inhibit: crate::sleep_inhibit::SleepInhibitor,
    /// Receiver for child agent permission requests (fed by socket bridge).
    child_permission_rx: tokio::sync::mpsc::UnboundedReceiver<engine::socket::IncomingMessage>,
    /// Reply channels for pending child permission requests, keyed by synthetic request_id.
    child_permission_replies:
        HashMap<u64, tokio::sync::oneshot::Sender<engine::socket::PermissionReply>>,
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
    /// Accumulated cost snapshots: `(history_len, cost_usd)`.
    cost_snapshots: Vec<(usize, f64)>,
    /// Per-turn metadata (elapsed, tps, status) keyed by history length.
    turn_metas: Vec<(usize, protocol::TurnMeta)>,
    /// TurnMeta from the engine, consumed by `finish_turn`.
    pending_turn_meta: Option<protocol::TurnMeta>,
    pending_agent_blocks: Vec<(String, protocol::AgentBlockData)>,
    /// Accumulated cost for the current session in USD.
    pub session_cost_usd: f64,
    /// Active model config (for pricing lookups).
    pub model_config: engine::ModelConfig,
}

/// Retained subset of the confirm request for mode-toggle re-checks.
struct ConfirmContext {
    call_id: String,
    tool_name: String,
    args: HashMap<String, serde_json::Value>,
    request_id: u64,
}

struct TurnState {
    turn_id: u64,
    pending: Vec<PendingTool>,
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
        instructions: Option<String>,
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
    CancelAndClear,
    Compact {
        instructions: Option<String>,
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

/// Relay a permission check to a parent socket and return the result.
async fn relay_permission(
    parent_socket: Option<&std::path::Path>,
    from_id: &str,
    tool_name: &str,
    args: &HashMap<String, serde_json::Value>,
    confirm_message: &str,
    approval_patterns: &[String],
    summary: Option<&str>,
) -> (bool, Option<String>) {
    let Some(socket) = parent_socket else {
        return (false, Some("no parent socket available".into()));
    };
    let req = engine::socket::PermissionCheckRequest {
        from_id,
        tool_name,
        args,
        confirm_message,
        approval_patterns,
        summary,
    };
    match engine::socket::send_permission_check(socket, &req).await {
        Ok(reply) => (reply.approved, reply.message),
        Err(e) => (false, Some(format!("permission relay failed: {e}"))),
    }
}

/// Counter for synthetic request IDs assigned to child permission requests.
/// Uses a high starting offset to avoid colliding with engine-generated IDs.
static NEXT_CHILD_REQUEST_ID: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1_000_000_000);

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
    pub call_id: String,
    pub name: String,
}

// ── App impl ─────────────────────────────────────────────────────────────────

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        model: String,
        api_base: String,
        api_key_env: String,
        provider_type: String,
        permissions: Arc<Permissions>,
        engine: EngineHandle,
        vim_from_config: bool,
        auto_compact: bool,
        show_tps: bool,
        show_tokens: bool,
        show_cost: bool,
        input_prediction: bool,
        task_slug: bool,
        restrict_to_workspace: bool,
        multi_agent: bool,
        reasoning_effort: protocol::ReasoningEffort,
        reasoning_cycle: Vec<protocol::ReasoningEffort>,
        mode_cycle: Vec<protocol::Mode>,
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
        crate::completer::set_multi_agent(multi_agent);
        let mut screen = Screen::new();
        screen.set_model_label(model.clone());
        screen.set_reasoning_effort(reasoning_effort);
        screen.set_show_tps(show_tps);
        screen.set_show_tokens(show_tokens);
        screen.set_show_cost(show_cost);
        screen.set_show_slug(task_slug);

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
            provider_type,
            reasoning_effort,
            reasoning_cycle,
            mode,
            mode_cycle,
            screen,
            history: Vec::new(),
            input_history: History::load(),
            input,
            exec_rx: None,
            exec_kill: None,
            queued_messages: Vec::new(),
            pending_agent_messages: Vec::new(),
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
            show_tps,
            show_tokens,
            show_cost,
            show_prediction: input_prediction,
            show_slug: task_slug,
            restrict_to_workspace,
            multi_agent,
            agent_id: String::new(),
            agents: Vec::new(),
            agent_snapshots: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            available_models,
            engine,
            permissions,
            confirm_context: None,
            input_prediction: None,
            predict_generation: 0,
            sleep_inhibit: crate::sleep_inhibit::SleepInhibitor::new(),
            child_permission_rx: {
                let (_, rx) = tokio::sync::mpsc::unbounded_channel();
                rx
            },
            child_permission_replies: HashMap::new(),
            pending_title: false,
            last_width: terminal::size().map(|(w, _)| w).unwrap_or(80),
            last_height: terminal::size().map(|(_, h)| h).unwrap_or(24),
            next_turn_id: 1,
            compact_epoch: 0,
            pending_compact_epoch: 0,
            token_snapshots: Vec::new(),
            cost_snapshots: Vec::new(),
            turn_metas: Vec::new(),
            pending_turn_meta: None,
            pending_agent_blocks: Vec::new(),
            session_cost_usd: 0.0,
            model_config: engine::ModelConfig::default(),
        }
    }

    // ── Unified event loop ───────────────────────────────────────────────

    /// Set the receiver for child agent permission requests (from socket bridge).
    pub fn set_child_permission_rx(
        &mut self,
        rx: tokio::sync::mpsc::UnboundedReceiver<engine::socket::IncomingMessage>,
    ) {
        self.child_permission_rx = rx;
    }

    pub async fn run(
        &mut self,
        mut ctx_rx: Option<tokio::sync::oneshot::Receiver<Option<u32>>>,
        initial_message: Option<String>,
    ) {
        crate::theme::detect_background();
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
                    self.show_tps,
                    self.show_tokens,
                    self.show_cost,
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
        let mut pending_dialogs: VecDeque<DeferredDialog> = VecDeque::new();

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
                            &ag.pending,
                            &mut pending_dialogs,
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
                            match self.process_input(&text) {
                                InputOutcome::StartAgent => {
                                    self.screen.erase_prompt();
                                    let content = Content::text(text.clone());
                                    agent = Some(self.begin_agent_turn(&text, content));
                                }
                                InputOutcome::Compact { instructions } => {
                                    self.screen.erase_prompt();
                                    if self.history.is_empty() {
                                        self.screen.notify_error("nothing to compact".into());
                                    } else {
                                        self.compact_history(instructions);
                                    }
                                }
                                InputOutcome::CustomCommand(cmd) => {
                                    self.screen.erase_prompt();
                                    agent = Some(self.begin_custom_command_turn(*cmd));
                                }
                                InputOutcome::Exec(rx, kill) => {
                                    self.screen.erase_prompt();
                                    self.exec_rx = Some(rx);
                                    self.exec_kill = Some(kill);
                                }
                                InputOutcome::CancelAndClear => {
                                    self.screen.erase_prompt();
                                    self.reset_session();
                                    agent = None;
                                }
                                InputOutcome::Continue | InputOutcome::Quit => {}
                                InputOutcome::OpenDialog(dlg) => {
                                    self.screen.erase_prompt();
                                    active_dialog = Some(dlg);
                                }
                            }
                        }
                    }
                }
            }

            // ── Auto-start from pending agent messages ─────────────────
            if agent.is_none() && !self.pending_agent_messages.is_empty() {
                let msgs = std::mem::take(&mut self.pending_agent_messages);
                self.history.extend(msgs);
                self.screen.erase_prompt();
                agent = Some(self.begin_agent_message_turn());
            }

            // ── Drain spawned children → track agents ─────────────────────
            self.drain_spawned_children();

            // ── Drain subagent events ────────────────────────────────────
            self.drain_agent_events();

            // ── Drain child permission requests ──────────────────────────
            while let Ok(msg) = self.child_permission_rx.try_recv() {
                let engine::socket::IncomingMessage::PermissionCheck {
                    tool_name,
                    args,
                    confirm_message,
                    approval_patterns,
                    summary,
                    reply_tx,
                    ..
                } = msg
                else {
                    continue;
                };

                let request_id =
                    NEXT_CHILD_REQUEST_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                self.child_permission_replies.insert(request_id, reply_tx);

                let ctrl = SessionControl::NeedsConfirm(ConfirmRequest {
                    call_id: format!("child-perm-{request_id}"),
                    tool_name,
                    desc: confirm_message,
                    args,
                    approval_patterns,
                    outside_dir: None,
                    summary,
                    request_id,
                });
                let pending = agent.as_ref().map(|a| a.pending.as_slice()).unwrap_or(&[]);
                let action = self.dispatch_control(
                    ctrl,
                    pending,
                    &mut pending_dialogs,
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
            }

            // ── Process pending permission dialogs ──────────────────────
            // If agent was cancelled while dialogs were pending, discard them.
            if agent.is_none() && !pending_dialogs.is_empty() {
                pending_dialogs.clear();
                self.screen.set_pending_dialog(false);
            }
            // Re-dispatch queued dialogs.  Each goes through dispatch_control
            // so auto-approval checks re-run ("always allow" → auto-approve rest).
            if !pending_dialogs.is_empty() && active_dialog.is_none() && agent.is_some() {
                let idle = t
                    .last_keypress
                    .map(|lk| lk.elapsed() >= Duration::from_millis(CONFIRM_DEFER_MS))
                    .unwrap_or(true);
                while idle
                    && !pending_dialogs.is_empty()
                    && active_dialog.is_none()
                    && agent.is_some()
                {
                    let deferred = pending_dialogs.pop_front().unwrap();
                    let ctrl = match deferred {
                        DeferredDialog::Confirm(req) => SessionControl::NeedsConfirm(req),
                        DeferredDialog::AskQuestion { args, request_id } => {
                            SessionControl::NeedsAskQuestion { args, request_id }
                        }
                    };
                    let pending = agent.as_ref().map(|a| a.pending.as_slice()).unwrap_or(&[]);
                    let action = self.dispatch_control(
                        ctrl,
                        pending,
                        &mut pending_dialogs,
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
                }
                self.screen.set_pending_dialog(!pending_dialogs.is_empty());
            }

            // ── Render ───────────────────────────────────────────────────
            let redirtied = self.tick(agent.is_some(), active_dialog.is_some());
            if let Some(d) = active_dialog.as_mut() {
                if redirtied {
                    d.mark_dirty();
                }
                let scr = &mut self.screen;
                let sync = scr.take_sync_started();
                d.draw(scr.dialog_row(), sync, scr.backend());
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
                        let scr = &mut self.screen;
                        let sync = scr.take_sync_started();
                        d.draw(scr.dialog_row(), sync, scr.backend());
                        self.screen.sync_dialog_anchor(d.anchor_row());
                    }
                }

                Some(ev) = self.engine.recv(), if !active_dialog.as_ref().is_some_and(|d| d.blocks_agent()) => {
                    if let Some(ref mut ag) = agent {
                        let ctrl = self.handle_engine_event(ev, ag.turn_id, &mut ag.pending, &mut ag.steered_count);
                        let action = self.dispatch_control(
                            ctrl,
                            &ag.pending,
                            &mut pending_dialogs,
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
                        let scr = &mut self.screen;
                        let sync = scr.take_sync_started();
                        d.draw(scr.dialog_row(), sync, scr.backend());
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
                    // Redraw if any background agent's block may need updating.
                    if self.agents.iter().any(|a| a.status == AgentTrackStatus::Working) {
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

        // If no messages were ever sent, preserve the final prompt/tab bar on exit.
        // When there is session history, clear below for a clean resume hint area.
        let clear_below = !self.session.messages.is_empty();
        self.screen.move_cursor_past_prompt(clear_below);
        let _ = io::stdout().execute(DisableBracketedPaste);
        terminal::disable_raw_mode().ok();
    }

    // ── Headless mode ─────────────────────────────────────────────────────

    /// Run a single message through the agent without any TUI.
    ///
    /// **Text mode** (default): final assistant message → stdout,
    /// everything else (tools, thinking, progress) → stderr. Matches
    /// the Codex convention: stdout is sacred, only the answer.
    ///
    /// **JSON mode**: every `EngineEvent` is emitted as a JSONL line to
    /// stdout for programmatic consumers.
    pub async fn run_headless(
        &mut self,
        message: String,
        format: OutputFormat,
        color_mode: ColorMode,
        verbose: bool,
        cancel: std::sync::Arc<tokio::sync::Notify>,
    ) {
        use std::io::Write;

        init_color_mode(color_mode);

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

        // In text mode, buffer assistant text and only print to stdout at the end.
        let mut final_message = String::new();
        let mut total_usage = protocol::TokenUsage::default();
        let mut last_tps: Option<f64> = None;
        let mut total_cost = 0.0_f64;
        let mut pending_tools: HashMap<String, (String, String, String)> = HashMap::new();

        // Drain events. Break on cancellation (Ctrl+C) so the summary still prints.
        let mut interrupted = false;
        loop {
            let ev = tokio::select! {
                ev = self.engine.recv() => match ev {
                    Some(ev) => ev,
                    None => break,
                },
                _ = cancel.notified() => {
                    self.engine.send(protocol::UiCommand::Cancel);
                    interrupted = true;
                    break;
                }
            };
            match format {
                OutputFormat::Json => {
                    // Forward every event as JSONL.
                    emit_json(&ev);

                    // Still need to handle side-effect events.
                    match ev {
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
                        EngineEvent::TurnError { .. } | EngineEvent::TurnComplete { .. } => {
                            break;
                        }
                        _ => {}
                    }
                }
                OutputFormat::Text => match ev {
                    EngineEvent::ThinkingDelta { .. } => {}
                    EngineEvent::Thinking { content } => {
                        log_thinking(&content);
                    }
                    EngineEvent::TextDelta { delta } => {
                        final_message.push_str(&delta);
                    }
                    EngineEvent::Text { content } => {
                        // Full text block replaces any accumulated deltas.
                        final_message = content;
                    }
                    EngineEvent::ToolStarted {
                        call_id,
                        tool_name,
                        summary,
                        ..
                    } => {
                        pending_tools.insert(call_id, (tool_name, summary, String::new()));
                    }
                    EngineEvent::ToolOutput { call_id, chunk } => {
                        if verbose {
                            if let Some((_, _, output)) = pending_tools.get_mut(&call_id) {
                                output.push_str(&chunk);
                            }
                        }
                    }
                    EngineEvent::ToolFinished {
                        call_id,
                        result,
                        elapsed_ms,
                    } => {
                        let (name, summary, output) =
                            pending_tools.remove(&call_id).unwrap_or_default();
                        let display_output = if !verbose {
                            ""
                        } else if result.is_error {
                            &result.content
                        } else {
                            &output
                        };
                        log_tool(&name, &summary, display_output, result.is_error, elapsed_ms);
                    }
                    EngineEvent::TokenUsage {
                        usage,
                        tokens_per_sec,
                        cost_usd,
                    } => {
                        total_cost += cost_usd.unwrap_or(0.0);
                        total_usage.accumulate(&usage);
                        last_tps = tokens_per_sec.or(last_tps);
                    }
                    EngineEvent::Retrying { delay_ms, attempt } => {
                        log_retry(attempt, delay_ms);
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
                    EngineEvent::Messages { .. } => {}
                    EngineEvent::TurnError { message } => {
                        log_error(&message);
                        break;
                    }
                    EngineEvent::TurnComplete { .. } => {
                        break;
                    }
                    _ => {}
                },
            }
        }

        // Print accumulated token/cost summary.
        if format == OutputFormat::Text {
            log_token_usage(&total_usage, last_tps, total_cost);
        }

        // Text mode: write the final message to stdout (only when piped).
        if format == OutputFormat::Text && !final_message.is_empty() {
            let stdout_is_tty = std::io::stdout().is_terminal();
            let stderr_is_tty = std::io::stderr().is_terminal();

            if stdout_is_tty && stderr_is_tty {
                // Interactive: print to stderr so the answer appears in
                // chronological order after tool output, not on a separate stream.
                eprintln!();
                eprint!("{final_message}");
                if !final_message.ends_with('\n') {
                    eprintln!();
                }
            } else {
                // At least one stream is piped — stdout gets the clean answer.
                print!("{final_message}");
                if !final_message.ends_with('\n') {
                    println!();
                }
                let _ = io::stdout().flush();
            }
        }

        if interrupted {
            let _ = io::stderr().flush();
            std::process::exit(130);
        }
    }

    // ── Subagent mode ────────────────────────────────────────────────────

    fn shutdown_subagent(&mut self, parent_pid: u32) {
        eprintln!("[subagent] parent {parent_pid} is dead, exiting");
        engine::registry::cleanup_self(std::process::id());
    }

    /// Forward an inter-agent message: emit to stdout and inject into engine.
    fn forward_agent_message(&self, from_id: &str, from_slug: &str, message: &str) {
        emit_json(&EngineEvent::AgentMessage {
            from_id: from_id.to_string(),
            from_slug: from_slug.to_string(),
            message: message.to_string(),
        });
        self.engine.send(UiCommand::AgentMessage {
            from_id: from_id.to_string(),
            from_slug: from_slug.to_string(),
            message: message.to_string(),
        });
    }

    /// Resolve the model name to send to the engine. Some providers (notably
    /// Codex) may have a selected provider entry with an empty placeholder
    /// model name until a concrete model is chosen.
    fn effective_model_name(&self) -> String {
        if !self.model.is_empty() {
            return self.model.clone();
        }
        self.available_models
            .iter()
            .find(|m| m.api_base == self.api_base && m.provider_type == self.provider_type)
            .and_then(|m| (!m.model_name.is_empty()).then(|| m.model_name.clone()))
            .or_else(|| {
                self.available_models
                    .iter()
                    .find_map(|m| (!m.model_name.is_empty()).then(|| m.model_name.clone()))
            })
            .unwrap_or_default()
    }

    /// Send a Btw query to the engine on behalf of a querying peer.
    fn send_btw_query(&self, question: String) {
        self.engine.send(UiCommand::Btw {
            question,
            history: self.history.clone(),
            model: self.effective_model_name(),
            reasoning_effort: self.reasoning_effort,
            api_base: Some(self.api_base.clone()),
            api_key: Some(self.api_key()),
        });
    }

    /// Run as a persistent subagent. Each `EngineEvent` is written to
    /// stdout as a JSON line so the parent can parse and render it.
    /// Processes the initial message, then loops: go idle → wait for
    /// messages → run next turn → repeat.
    pub async fn run_subagent(
        &mut self,
        initial_message: String,
        parent_pid: u32,
        mut socket_rx: tokio::sync::mpsc::UnboundedReceiver<engine::socket::IncomingMessage>,
    ) {
        let parent_socket = engine::registry::read_entry(parent_pid)
            .ok()
            .map(|e| std::path::PathBuf::from(&e.socket_path));
        let my_pid = std::process::id();
        let my_agent_id = engine::registry::read_entry(my_pid)
            .ok()
            .map(|e| e.agent_id)
            .unwrap_or_default();

        // Run the initial turn.
        self.run_subagent_turn(
            Content::text(initial_message),
            &mut socket_rx,
            parent_pid,
            parent_socket.as_deref(),
            &my_agent_id,
        )
        .await;

        // Persistent loop: wait for incoming messages or parent death.
        loop {
            let parent_check = tokio::time::sleep(std::time::Duration::from_secs(5));
            tokio::pin!(parent_check);

            tokio::select! {
                Some(incoming) = socket_rx.recv() => {
                    match incoming {
                        engine::socket::IncomingMessage::Message { from_id, from_slug, message } => {
                            self.forward_agent_message(&from_id, &from_slug, &message);
                            self.history
                                .push(protocol::Message::agent(&from_id, &from_slug, &message));
                            self.run_subagent_turn(
                                Content::text(""),
                                &mut socket_rx,
                                parent_pid,
                                parent_socket.as_deref(),
                                &my_agent_id,
                            )
                            .await;
                        }
                        engine::socket::IncomingMessage::Query { from_id: _, question, reply_tx } => {
                            self.send_btw_query(question);
                            while let Some(ev) = self.engine.recv().await {
                                emit_json(&ev);
                                if let EngineEvent::BtwResponse { content } = ev {
                                    let _ = reply_tx.send(content);
                                    break;
                                }
                            }
                        }
                        engine::socket::IncomingMessage::PermissionCheck {
                            from_id, tool_name, args, confirm_message,
                            approval_patterns, summary, reply_tx,
                        } => {
                            let (approved, message) = relay_permission(
                                parent_socket.as_deref(), &from_id, &tool_name,
                                &args, &confirm_message, &approval_patterns, summary.as_deref(),
                            ).await;
                            let _ = reply_tx.send(engine::socket::PermissionReply { approved, message });
                        }
                    }
                }
                _ = &mut parent_check => {
                    if !engine::registry::is_pid_alive(parent_pid) {
                        self.shutdown_subagent(parent_pid);
                        return;
                    }
                }
            }
        }
    }

    async fn run_subagent_turn(
        &mut self,
        content: Content,
        socket_rx: &mut tokio::sync::mpsc::UnboundedReceiver<engine::socket::IncomingMessage>,
        parent_pid: u32,
        parent_socket: Option<&std::path::Path>,
        my_agent_id: &str,
    ) {
        let my_pid = std::process::id();
        engine::registry::update_status(my_pid, engine::registry::AgentStatus::Working);

        // Generate title/slug for the subagent.
        let text = content.text_content();
        if self.session.slug.is_none() && !text.is_empty() {
            self.engine.send(UiCommand::GenerateTitle {
                user_messages: vec![text],
                model: self.model.clone(),
                api_base: Some(self.api_base.clone()),
                api_key: Some(self.api_key()),
            });
        }

        let turn_id = self.next_turn_id;
        self.next_turn_id += 1;

        self.engine.send(UiCommand::StartTurn {
            turn_id,
            content,
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

        let mut pending_query_tx: Option<tokio::sync::oneshot::Sender<String>> = None;

        loop {
            let parent_check = tokio::time::sleep(std::time::Duration::from_secs(5));
            tokio::pin!(parent_check);

            tokio::select! {
                Some(incoming) = socket_rx.recv() => {
                    match incoming {
                        engine::socket::IncomingMessage::Message { from_id, from_slug, message } => {
                            self.forward_agent_message(&from_id, &from_slug, &message);
                        }
                        engine::socket::IncomingMessage::Query { from_id: _, question, reply_tx } => {
                            self.send_btw_query(question);
                            pending_query_tx = Some(reply_tx);
                        }
                        engine::socket::IncomingMessage::PermissionCheck {
                            from_id, tool_name, args, confirm_message,
                            approval_patterns, summary, reply_tx,
                        } => {
                            let (approved, message) = relay_permission(
                                parent_socket, &from_id, &tool_name,
                                &args, &confirm_message, &approval_patterns, summary.as_deref(),
                            ).await;
                            let _ = reply_tx.send(engine::socket::PermissionReply { approved, message });
                        }
                    }
                }
                _ = &mut parent_check => {
                    if !engine::registry::is_pid_alive(parent_pid) {
                        self.shutdown_subagent(parent_pid);
                        return;
                    }
                }
                maybe_ev = self.engine.recv() => {
                    let Some(ev) = maybe_ev else {
                        break;
                    };

                    // Forward every event to stdout as JSON.
                    emit_json(&ev);

                    // Handle side effects for events that need them.
                    match ev {
                        EngineEvent::RequestPermission {
                            request_id, tool_name, args, confirm_message,
                            approval_patterns, summary, ..
                        } => {
                            let (approved, message) = relay_permission(
                                parent_socket, my_agent_id, &tool_name,
                                &args, &confirm_message, &approval_patterns, summary.as_deref(),
                            ).await;
                            self.engine.send(UiCommand::PermissionDecision {
                                request_id, approved, message,
                            });
                        }
                        EngineEvent::RequestAnswer { request_id, .. } => {
                            self.engine.send(UiCommand::QuestionAnswer {
                                request_id,
                                answer: Some("User is not available (subagent mode).".into()),
                            });
                        }
                        EngineEvent::Messages { messages, .. } => {
                            self.history = messages;
                        }
                        EngineEvent::BtwResponse { content } => {
                            if let Some(tx) = pending_query_tx.take() {
                                let _ = tx.send(content);
                            }
                        }
                        EngineEvent::TitleGenerated { title, slug } => {
                            self.session.title = Some(title);
                            self.session.slug = Some(slug.clone());
                            engine::registry::update_slug(my_pid, &slug);
                        }
                        EngineEvent::TurnError { .. } => {
                            break;
                        }
                        EngineEvent::TurnComplete { messages, .. } => {
                            self.history = messages;

                            // Auto-return last assistant message to parent.
                            if let Some(socket) = parent_socket {
                                if let Some(last_asst) = self.history.iter().rev().find(|m| m.role == protocol::Role::Assistant) {
                                    let text = last_asst.content.as_ref().map(|c| c.text_content()).unwrap_or_default();
                                    if !text.is_empty() {
                                        let slug = self.session.slug.as_deref().unwrap_or("");
                                        let _ = engine::socket::send_message(socket, my_agent_id, slug, &text).await;
                                    }
                                }
                            }

                            break;
                        }
                        _ => {}
                    }
                }
            }
        }
        engine::registry::update_status(my_pid, engine::registry::AgentStatus::Idle);
    }

    fn open_blocking_dialog(
        &mut self,
        mut dialog: Box<dyn render::Dialog>,
        active_dialog: &mut Option<Box<dyn render::Dialog>>,
    ) {
        // Flush pending blocks (e.g. Thinking) to scroll mode so they
        // persist in scrollback.  Leave the sync frame open so that the
        // subsequent tool overlay + dialog draw is part of the same
        // atomic terminal update — no flicker between block flush and
        // dialog appearance.
        let scr = &mut self.screen;
        scr.render_pending_blocks_for_dialog();
        let height = crossterm::terminal::size().map(|(_, h)| h).unwrap_or(24);
        let fits = scr.active_tool_rows() + dialog.height() <= height;
        // Always clear the prompt section before drawing a blocking dialog.
        // Keeping old prompt rows (including tab bar) around and relying on
        // later overlay redraws can leave stale lines on some terminals.
        scr.erase_prompt_nosync();
        scr.set_show_tool_in_dialog(fits);
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

// ── Streaming subagent helper ────────────────────────────────────────────────

// ── Headless output types ───────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    Auto,
    Always,
    Never,
}

/// Write a single `EngineEvent` as a JSON line to stdout.
fn emit_json(ev: &EngineEvent) {
    println!("{}", serde_json::to_string(ev).unwrap());
}

// ── Headless / subagent log helpers ─────────────────────────────────────────
//
// Bare-minimum style. Assistant text flows undecorated; only tool lifecycle
// gets markers. Thinking is dim+italic. Colors match the TUI theme.
// Respects NO_COLOR, TERM=dumb, non-TTY stderr, and --color flag.

use std::io::IsTerminal;
use std::sync::OnceLock;

/// Explicit color mode set via --color flag. `None` means auto-detect.
static COLOR_OVERRIDE: OnceLock<Option<bool>> = OnceLock::new();

fn init_color_mode(mode: ColorMode) {
    let _ = COLOR_OVERRIDE.set(match mode {
        ColorMode::Auto => None,
        ColorMode::Always => Some(true),
        ColorMode::Never => Some(false),
    });
}

fn stderr_supports_color() -> bool {
    static RESULT: OnceLock<bool> = OnceLock::new();
    *RESULT.get_or_init(|| {
        // Explicit --color flag takes precedence.
        if let Some(Some(forced)) = COLOR_OVERRIDE.get() {
            return *forced;
        }
        if std::env::var_os("NO_COLOR").is_some() {
            return false;
        }
        if std::env::var("TERM").as_deref() == Ok("dumb") {
            return false;
        }
        // Subagents have stderr piped to a log file, but the parent TUI
        // renders the ANSI sequences — so honor FORCE_COLOR.
        if std::env::var_os("FORCE_COLOR").is_some() {
            return true;
        }
        std::io::stderr().is_terminal()
    })
}

/// Map a `crossterm::style::Color` to its ANSI escape foreground string.
fn ansi_fg(c: crossterm::style::Color) -> &'static str {
    if !stderr_supports_color() {
        return "";
    }
    use crossterm::style::Color;
    // Leak a small string per unique color (bounded by theme constants).
    match c {
        Color::AnsiValue(n) => {
            let s: String = format!("\x1b[38;5;{n}m");
            &*Box::leak(s.into_boxed_str())
        }
        Color::Red => "\x1b[31m",
        Color::DarkGrey => "\x1b[90m",
        _ => "",
    }
}

fn reset() -> &'static str {
    if stderr_supports_color() {
        "\x1b[0m"
    } else {
        ""
    }
}
fn dim() -> &'static str {
    if stderr_supports_color() {
        "\x1b[2m"
    } else {
        ""
    }
}
fn dim_italic() -> &'static str {
    if stderr_supports_color() {
        "\x1b[2;3m"
    } else {
        ""
    }
}

fn log_thinking(content: &str) {
    let di = dim_italic();
    let r = reset();
    for line in content.lines() {
        eprintln!("{di}{line}{r}");
    }
}

fn log_tool(tool_name: &str, summary: &str, output: &str, is_error: bool, elapsed_ms: Option<u64>) {
    let r = reset();
    let time = format_elapsed(elapsed_ms);
    let d = dim();
    let mark = if is_error {
        let c = ansi_fg(crate::theme::ERROR);
        format!("{c}✗{r}")
    } else {
        let c = ansi_fg(crate::theme::SUCCESS);
        format!("{c}✓{r}")
    };
    eprintln!("{mark} {d}{tool_name}{r} {summary} {d}({time}){r}");

    if !output.is_empty() {
        for line in output.lines() {
            eprintln!("{d}  {line}{r}");
        }
    }
}

fn log_retry(attempt: u32, delay_ms: u64) {
    let d = dim();
    let r = reset();
    let secs = delay_ms as f64 / 1000.0;
    eprintln!("{d}\u{27f3} retry #{attempt} ({secs:.1}s){r}");
}

fn log_error(message: &str) {
    let c = ansi_fg(crate::theme::ERROR);
    let r = reset();
    eprintln!("{c}! {message}{r}");
}

fn log_token_usage(usage: &protocol::TokenUsage, tokens_per_sec: Option<f64>, cost_usd: f64) {
    let d = dim();
    let r = reset();
    let mut parts = Vec::new();
    if let Some(p) = usage.prompt_tokens {
        parts.push(format!("{p} prompt"));
    }
    if let Some(c) = usage.completion_tokens {
        parts.push(format!("{c} completion"));
    }
    if let Some(cached) = usage.cache_read_tokens {
        if cached > 0 {
            parts.push(format!("{cached} cached"));
        }
    }
    if parts.is_empty() {
        return;
    }
    let mut line = format!("{d}tokens: {}", parts.join(", "));
    if let Some(tps) = tokens_per_sec {
        line.push_str(&format!(" ({tps:.0} tok/s)"));
    }
    if cost_usd > 0.0 {
        if cost_usd < 0.01 {
            line.push_str(&format!(" | cost: ${cost_usd:.4}"));
        } else {
            line.push_str(&format!(" | cost: ${cost_usd:.2}"));
        }
    }
    line.push_str(r);
    eprintln!("{line}");
}

fn format_elapsed(ms: Option<u64>) -> String {
    match ms {
        Some(ms) if ms >= 1000 => format!("{:.1}s", ms as f64 / 1000.0),
        Some(ms) => format!("{ms}ms"),
        None => String::new(),
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
        assert!(is_allowed_while_running("/model").is_ok());
        assert!(is_allowed_while_running("/settings").is_ok());
        assert!(is_allowed_while_running("/theme").is_ok());
        assert!(is_allowed_while_running("/stats").is_ok());
        assert!(is_allowed_while_running("/cost").is_ok());
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
