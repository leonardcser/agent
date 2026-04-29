mod agent;
pub mod commands;
mod headless;
mod transcript;
pub(crate) mod transcript_cache;
pub(crate) mod transcript_model;
pub(crate) mod transcript_present;
pub(crate) mod working;
pub(crate) use commands::copy_to_clipboard;
mod cmdline;
mod content_keys;
pub(crate) mod dialogs;
mod events;
mod history;
mod lua_bridge;
mod lua_handlers;
mod mouse;
mod pane_focus;
mod render_loop;
mod status_bar;

pub use headless::{ColorMode, OutputFormat};

/// Snapshot of a tracked agent's state, published by the main loop
/// and consumed by the agents dialog.
#[derive(Clone)]
pub struct AgentSnapshot {
    pub agent_id: String,
    pub prompt: Arc<String>,
    pub tool_calls: Vec<AgentToolEntry>,
    pub context_tokens: Option<u32>,
    pub cost_usd: f64,
}

/// Shared, live-updating list of agent snapshots.
pub type SharedSnapshots = Arc<Mutex<Vec<AgentSnapshot>>>;

pub(crate) use crate::app::transcript_model::{
    AgentBlockStatus, ApprovalScope, Block, BlockId, ConfirmChoice, ConfirmRequest,
    PermissionEntry, ToolOutput, ToolState, ToolStatus, ViewState,
};
pub(crate) use crate::app::working::{TurnOutcome, TurnPhase};
use crate::input::{resolve_agent_esc, Action, EscAction, History, PromptState};
use crate::session::Session;
use crate::{content, session, state, vim};
use engine::tools::tool_arg_summary;
use engine::{permissions::Decision, EngineHandle, Permissions};
use protocol::{Content, EngineEvent, Message, Mode, ReasoningEffort, Role, UiCommand};

use crossterm::{
    cursor,
    event::{
        self, DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
        EnableFocusChange, EnableMouseCapture, EventStream, KeyCode, KeyEvent, KeyModifiers,
    },
    terminal::{self, DisableLineWrap, EnableLineWrap, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ── Tracked agent state ──────────────────────────────────────────────────────

/// A single tool call recorded from a subagent's event stream.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
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
    /// Latest prompt-token count reported for this subagent.
    pub context_tokens: Option<u32>,
    /// Accumulated cost in USD from this subagent's TokenUsage events.
    pub cost_usd: f64,
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
    /// Block history, tool states, layout cache — the committed transcript.
    pub(crate) transcript: crate::content::transcript::Transcript,
    /// Streaming parser state (active text/thinking/tool/agent/exec blocks).
    pub(crate) parser: crate::content::stream_parser::StreamParser,
    /// Buffer-backed projection of the transcript into a ui::Buffer.
    pub(crate) transcript_projection: crate::content::transcript_buf::TranscriptProjection,
    /// Plain-text snapshot of each visible row (top to bottom) captured
    /// during `project_transcript_buffer`. Read by
    /// `compute_transcript_cursor` to look up the glyph under the soft
    /// cursor.
    pub(crate) last_viewport_text: Vec<String>,
    pub history: Vec<Message>,
    pub input_history: History,
    pub input: PromptState,
    exec_rx: Option<tokio::sync::mpsc::UnboundedReceiver<commands::ExecEvent>>,
    exec_kill: Option<std::sync::Arc<tokio::sync::Notify>>,
    pub queued_messages: Vec<String>,
    /// Agent messages waiting to trigger a turn.
    pending_agent_messages: Vec<protocol::Message>,
    /// Runtime approvals shared with the engine. The engine checks these
    /// during `decide()` to auto-approve tools without sending
    /// `RequestPermission`. The TUI writes to them when the user approves.
    pub runtime_approvals: Arc<std::sync::RwLock<engine::permissions::RuntimeApprovals>>,
    /// Current working directory (cached at startup).
    pub(crate) cwd: String,
    pub session: session::Session,
    pub shared_session: Arc<Mutex<Option<Session>>>,
    pub context_window: Option<u32>,
    /// Latest observed input-token count for the active session.
    pub context_tokens: Option<u32>,
    /// Short task label (slug) shown on the status bar after the throbber.
    pub task_label: Option<String>,
    /// A permission dialog is waiting for the user to stop typing.
    pub pending_dialog: bool,
    /// Set by reducer handlers (e.g. `DomainOp::RunCommand` dispatching
    /// `/quit`) to request the main loop break out on its next check.
    pub(crate) pending_quit: bool,
    /// Items returned by Lua-registered statusline sources. Appended
    /// after the Rust-side built-in spans each frame; priority /
    /// align_right on each item controls layout.
    pub custom_status_items: Vec<content::status::StatusItem>,
    /// Last error message reported per statusline source. Used to
    /// rate-limit notifications so a perpetually-broken source doesn't
    /// spam one toast per frame — only re-notify when the message
    /// changes; clear the entry on a successful tick.
    statusline_last_errors: HashMap<String, String>,
    /// Open `ui::Notification` float, if one is visible. Dismissed on
    /// any key (see `handle_overlay_keys`). `None` when no toast.
    pub notification: Option<ui::WinId>,
    /// Nvim-style `:` command line as a compositor float. `Some(win)`
    /// while the prompt is active; the `ui::Cmdline` component on that
    /// window owns all edit / history state. Events.rs pre-dispatches
    /// Enter / Esc / Tab while the cmdline is focused and drives the
    /// shared `cmdline_completer` + `cmdline_history` below.
    pub cmdline_win: Option<ui::WinId>,
    /// Persistent `:` history across open/close cycles. The component
    /// only sees this as a snapshot at open time; on close / submit we
    /// merge the component's working history back in.
    pub cmdline_history: Vec<String>,
    /// Shared completer instance for `:` command completion. Lazily
    /// constructed on first Tab press (it queries Lua command names),
    /// dropped on cmdline close.
    pub cmdline_completer: Option<crate::completer::Completer>,
    /// Terminal focus (FocusGained / FocusLost). Cursor is suppressed
    /// when the terminal isn't focused, so input from other apps
    /// doesn't draw a stale cursor in our window.
    pub term_focused: bool,
    /// Live-turn + last-turn state driving the status bar spinner and
    /// result line. `begin(TurnPhase::...)` / `finish(TurnOutcome::...)`
    /// are the write paths, mirrored from engine lifecycle events.
    pub working: working::WorkingState,
    /// Gutter reservation for the transcript window (left padding +
    /// right scrollbar column).
    pub transcript_gutters: crate::window::WindowGutters,
    /// Last-computed viewport layout (status / transcript / prompt
    /// rows). Updated each frame in `render_normal`; read by mouse
    /// hit-testing and viewport-rows estimation.
    pub layout: content::layout::LayoutState,
    /// Buffer viewport for the prompt input text area, recorded after
    /// paint. Used by scrollbar click/drag and input-focus mouse
    /// routing.
    pub(crate) prompt_viewport: Option<ui::WindowViewport>,
    /// Buffer viewport for the transcript window, recorded after each
    /// render. Used by mouse hit-testing, transcript cursor positioning,
    /// and focus-mouse routing.
    pub(crate) transcript_viewport: Option<ui::WindowViewport>,
    pub settings: state::ResolvedSettings,
    pub multi_agent: bool,
    /// Human-readable name for this agent.
    pub agent_id: String,
    /// All tracked subagents (blocking and background).
    pub agents: Vec<TrackedAgent>,
    /// Shared agent snapshots for live dialog updates.
    pub agent_snapshots: crate::app::SharedSnapshots,
    pub available_models: Vec<crate::config::ResolvedModel>,
    pub engine: EngineHandle,
    pub(crate) permissions: Arc<Permissions>,
    /// The active turn's state, or `None` when the app is idle.
    /// Owned by `App` so reducer handlers (`apply_ops`) can mutate
    /// it directly rather than threading `&mut Option<TurnState>`
    /// through every call chain.
    pub(crate) agent: Option<TurnState>,
    /// Pending Confirm dialog requests, keyed by Lua-side handle id.
    /// `agent.rs` inserts a request before firing `smelt.confirm.open
    /// (handle_id)`; the Lua dialog reads it back through Rust
    /// primitives and removes it on resolve / dismiss.
    pub(crate) confirm_requests: HashMap<u64, crate::app::dialogs::confirm::ConfirmEntry>,
    pub(crate) next_confirm_handle: u64,
    /// Ghost text prediction for the input field.
    pub input_prediction: Option<String>,
    /// Monotonic counter to discard stale predictions.
    predict_generation: u64,
    sleep_inhibit: crate::sleep_inhibit::SleepInhibitor,
    persister: crate::persist::Persister,
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
    /// Whether model was explicitly provided via CLI (takes precedence over session).
    cli_model_override: bool,
    /// Whether api_base was explicitly provided via CLI (takes precedence over session).
    cli_api_base_override: bool,
    /// Whether api_key_env was explicitly provided via CLI (takes precedence over session).
    cli_api_key_env_override: bool,
    startup_auth_error: Option<String>,
    /// App-level focus (Prompt = editing buffer; History = navigating transcript).
    pub app_focus: AppFocus,
    /// Readonly pane showing the transcript. Owns its `Buffer`
    /// (vim + kill ring + undo) and the viewport scroll / cursor
    /// position.
    pub transcript_window: ui::Window,
    /// Last prompt-buffer text we dispatched a `TextChanged` event for.
    /// After each event, if `input.buf` differs from this, we fire
    /// `WinEvent::TextChanged` on `PROMPT_WIN` so Lua subscribers
    /// (`smelt.win.on_event(prompt, "text_changed", …)`) get called.
    pub last_prompt_text: String,
    /// Last primary-mouse-Down time, cell, and click count. Successive
    /// clicks on the same cell within a short window increment the
    /// count: 2 → word-select, 3 → line-select.
    pub last_click: Option<(Instant, u16, u16, u8)>,
    /// Primary mouse button is held — we're mid-drag. The transcript
    /// stays frozen from Down to Up so selected text can't shift
    /// under the user's cursor while the agent streams new rows.
    pub mouse_drag_active: bool,
    /// When drag-autoscroll is currently engaged (cursor parked at a
    /// viewport edge while the user holds mouse-1), the timestamp it
    /// started. Used by `tick_drag_autoscroll` to ramp the scroll speed
    /// up the longer the cursor stays at the edge.
    pub drag_autoscroll_since: Option<std::time::Instant>,
    /// When the initial mouse-down landed on a scrollbar (prompt,
    /// transcript, or a dialog-panel float), every subsequent drag tick
    /// re-maps the pointer row to a scroll offset instead of extending
    /// a visual selection — even if the pointer wanders off the track
    /// column. The stored value records which surface's scrollbar owns
    /// the gesture.
    pub drag_on_scrollbar: Option<ScrollbarDragTarget>,
    /// Layer that captured the mouse on its last `Down(Left)` (because
    /// `Component::handle_mouse` returned `KeyResult::Capture`).
    /// Subsequent `Drag` and `Up` events route to that layer until
    /// release, so a component (e.g. `TextInput` drag-select) can
    /// extend a gesture even when the pointer wanders off its rect.
    pub drag_on_layer: Option<ui::WinId>,
    /// Prompt vim mode at the start of a mouse-drag. Set on mouse-down
    /// inside the prompt viewport (only when vim is enabled) before the
    /// drag enters `Visual`, restored on mouse-up so a drag from Insert
    /// lands the user back in Insert rather than Normal. `None` outside
    /// an active prompt drag.
    pub prompt_drag_return_vim_mode: Option<crate::vim::ViMode>,
    /// Lua runtime — loads `~/.config/smelt/init.lua`, dispatches
    /// user-registered commands / keymaps / autocmds.
    pub lua: crate::lua::LuaRuntime,
    /// Extra instructions from AGENTS.md / config, injected into the system
    /// prompt as a section. Set during app initialization.
    pub extra_instructions: Option<String>,
    /// Pre-rendered skills prompt section. Set during app initialization.
    pub skill_section: Option<String>,
    /// Multi-agent prompt config (agent identity, parent, siblings).
    pub agent_prompt_config: Option<engine::AgentPromptConfig>,
    /// Prompt sections built from app state. Rebuilt on mode changes.
    pub prompt_sections: crate::prompt_sections::PromptSections,
    pub ui: ui::Ui,
    /// Stable `BufId` for the prompt input's styled display buffer.
    /// Populated each frame by `compute_prompt` and read by the
    /// `prompt_input` WindowView layer.
    pub(super) input_display_buf: ui::BufId,
}

/// Which pane currently holds focus (nvim-style window split).
///
/// * `Prompt` — the bottom input pane owns focus; vim Insert/Normal/Visual
///   live inside and regular typing goes there.
/// * `Content` — the transcript pane owns focus; motions target it and
///   the prompt is frozen until focus returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppFocus {
    Prompt,
    Content,
}

/// Which surface's scrollbar is driving an in-flight drag gesture.
/// `Focus(_)` points at the transcript or prompt window; `DialogPanel`
/// points at a specific buffer-backed panel inside a compositor float
/// dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollbarDragTarget {
    Focus(AppFocus),
    DialogPanel { win: ui::WinId, panel: usize },
}

pub(super) struct TurnState {
    turn_id: u64,
    pending: Vec<PendingTool>,
    _perf: Option<crate::perf::Guard>,
}

enum EventOutcome {
    Noop,
    Redraw,
    Quit,
    CancelAgent,
    CancelAndClear,
    /// Interrupt the running agent and immediately start a new turn
    /// with the oldest queued message.
    InterruptWithQueued,
    Submit {
        content: Content,
        display: String,
    },
    Exec(
        tokio::sync::mpsc::UnboundedReceiver<commands::ExecEvent>,
        std::sync::Arc<tokio::sync::Notify>,
    ),
}

pub enum CommandAction {
    Continue,
    Quit,
    CancelAndClear,
    Compact {
        instructions: Option<String>,
    },
    Exec(
        tokio::sync::mpsc::UnboundedReceiver<commands::ExecEvent>,
        std::sync::Arc<tokio::sync::Notify>,
    ),
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
    /// Pending `Ctrl-W` pane chord. When set, the next key consumes the
    /// chord to navigate panes instead of flowing to input handling.
    pending_pane_chord: Option<Instant>,
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
    Confirm(Box<ConfirmRequest>),
}

// ── Supporting types ─────────────────────────────────────────────────────────

pub enum SessionControl {
    Continue,
    NeedsConfirm(Box<ConfirmRequest>),
    Done,
}

enum LoopAction {
    Continue,
    Done,
}

pub struct PendingTool {
    pub call_id: String,
    pub name: String,
    pub args: HashMap<String, serde_json::Value>,
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
        settings: state::ResolvedSettings,
        multi_agent: bool,
        reasoning_effort: protocol::ReasoningEffort,
        reasoning_cycle: Vec<protocol::ReasoningEffort>,
        mode_cycle: Vec<protocol::Mode>,
        shared_session: Arc<Mutex<Option<Session>>>,
        available_models: Vec<crate::config::ResolvedModel>,
        cli_model_override: bool,
        cli_api_base_override: bool,
        cli_api_key_env_override: bool,
        startup_auth_error: Option<String>,
    ) -> Self {
        let saved = state::State::load();
        let mode = saved.mode();
        let mut input = PromptState::new();
        let vim_enabled = settings.vim;
        if vim_enabled {
            input.set_vim_enabled(true);
        }
        // Arg sources for the CommandArg inline completer (type `/cmd arg`).
        // The picker-style commands (`/model`, `/theme`, `/color`, `/settings`)
        // now live in Lua plugins and open real `ui::Picker` windows via
        // `smelt.prompt.open_picker`, so they're not listed here.
        input.command_arg_sources = Vec::new();
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

        let cwd = std::env::current_dir()
            .ok()
            .and_then(|p| p.to_str().map(String::from))
            .unwrap_or_default();
        // Runtime approvals are shared with the engine via Arc<RwLock>.
        // Load workspace rules from disk into them at startup.
        let runtime_approvals = engine.runtime_approvals();

        let (ui, input_display_buf) = {
            let (w, h) = terminal::size().unwrap_or((80, 24));
            let mut ui = ui::Ui::new();
            ui.set_terminal_size(w, h);
            // One source of truth for selection bg: theme resolves it
            // once, every Window-driven surface (transcript, prompt,
            // dialog content panels) reads from here.
            ui.set_selection_bg(crate::theme::selection_bg());
            ui.set_layout(ui::LayoutTree::Split {
                direction: ui::layout::Direction::Vertical,
                children: vec![
                    ui::LayoutTree::Leaf {
                        name: "transcript".into(),
                        constraint: ui::Constraint::Fill,
                    },
                    ui::LayoutTree::Leaf {
                        name: "prompt".into(),
                        constraint: ui::Constraint::Pct(25),
                    },
                ],
            });
            let input_display_buf = ui.buf_create(ui::buffer::BufCreateOpts {
                modifiable: true,
                buftype: ui::buffer::BufType::Prompt,
            });
            let transcript_view = crate::content::window_view::WindowView::new();
            let prompt_chrome_view = crate::content::window_view::WindowView::new();
            let prompt_input_view = crate::content::window_view::WindowView::new();
            let status_bar = ui::StatusBar::new();
            ui.add_layer(
                "transcript",
                Box::new(transcript_view),
                ui::Rect::new(0, 0, w, h),
                0,
            );
            ui.add_layer(
                "prompt",
                Box::new(prompt_chrome_view),
                ui::Rect::new(0, 0, w, 1),
                1,
            );
            ui.add_layer(
                "prompt_input",
                Box::new(prompt_input_view),
                ui::Rect::new(0, 0, w, 1),
                2,
            );
            // Status bar rides above all floats (default float zindex = 50;
            // completer float uses 30, notification 40, cmdline 600). Picking
            // 500 leaves headroom for any future always-above-status overlay.
            ui.add_layer(
                "status",
                Box::new(status_bar),
                ui::Rect::new(h.saturating_sub(1), 0, w, 1),
                500,
            );
            ui.register_split("prompt_input", ui::PROMPT_WIN);
            ui.register_split("transcript", ui::TRANSCRIPT_WIN);
            ui.focus_layer("prompt_input");
            (ui, input_display_buf)
        };

        Self {
            model,
            api_base,
            api_key_env,
            provider_type,
            reasoning_effort,
            reasoning_cycle,
            mode,
            mode_cycle,
            transcript: crate::content::transcript::Transcript::new(),
            parser: crate::content::stream_parser::StreamParser::new(),
            transcript_projection: crate::content::transcript_buf::TranscriptProjection::new(
                ui::buffer::Buffer::new(
                    ui::BufId(0),
                    ui::buffer::BufCreateOpts {
                        modifiable: true,
                        buftype: ui::buffer::BufType::Nofile,
                    },
                ),
            ),
            last_viewport_text: Vec::new(),
            history: Vec::new(),
            input_history: History::load(),
            input,
            exec_rx: None,
            exec_kill: None,
            queued_messages: Vec::new(),
            pending_agent_messages: Vec::new(),
            runtime_approvals,
            cwd,
            session: session::Session::new(),
            shared_session,
            context_window: None,
            context_tokens: None,
            task_label: None,
            pending_dialog: false,
            pending_quit: false,
            custom_status_items: Vec::new(),
            statusline_last_errors: HashMap::new(),
            notification: None,
            cmdline_win: None,
            cmdline_history: Vec::new(),
            cmdline_completer: None,
            term_focused: true,
            working: working::WorkingState::new(),
            transcript_gutters: crate::window::TRANSCRIPT_GUTTERS,
            layout: content::layout::LayoutState::compute(&content::layout::LayoutInput {
                term_width: 80,
                term_height: 24,
                prompt_height: 3,
            }),
            prompt_viewport: None,
            transcript_viewport: None,
            settings,
            multi_agent,
            agent_id: String::new(),
            agents: Vec::new(),
            agent_snapshots: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            available_models,
            engine,
            permissions,
            agent: None,
            confirm_requests: HashMap::new(),
            next_confirm_handle: 1,
            input_prediction: None,
            predict_generation: 0,
            sleep_inhibit: crate::sleep_inhibit::SleepInhibitor::new(),
            persister: crate::persist::Persister::spawn(),
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
            cli_model_override,
            cli_api_base_override,
            cli_api_key_env_override,
            startup_auth_error,
            app_focus: AppFocus::Prompt,
            transcript_window: {
                let mut w = ui::Window::new(
                    ui::TRANSCRIPT_WIN,
                    ui::BufId(0),
                    ui::WinConfig::Split(ui::SplitConfig {
                        region: "transcript".into(),
                        gutters: ui::Gutters::default(),
                    }),
                );
                w.set_vim_enabled(vim_enabled);
                w
            },
            last_prompt_text: String::new(),
            last_click: None,
            mouse_drag_active: false,
            drag_autoscroll_since: None,
            drag_on_scrollbar: None,
            drag_on_layer: None,
            prompt_drag_return_vim_mode: None,
            lua: crate::lua::LuaRuntime::new(),
            extra_instructions: None,
            skill_section: None,
            agent_prompt_config: None,
            prompt_sections: crate::prompt_sections::PromptSections::default(),
            ui,
            input_display_buf,
        }
    }

    /// Rebuild prompt sections from current app state (mode, instructions, etc.)
    /// and return the assembled system prompt string.
    pub fn rebuild_system_prompt(&mut self) -> String {
        let cwd = std::path::Path::new(&self.cwd);
        self.prompt_sections = crate::prompt_sections::build_defaults(
            cwd,
            self.mode,
            true, // TUI is always interactive
            self.agent_prompt_config.as_ref(),
            self.skill_section.as_deref(),
            self.extra_instructions.as_deref(),
        );
        self.prompt_sections.assemble()
    }

    pub fn settings_state(&self) -> state::ResolvedSettings {
        let mut s = self.settings.clone();
        s.vim = self.input.vim_enabled();
        s
    }

    /// Width available for transcript content. Reserves the rightmost
    /// column for the scrollbar track so the scrollbar never overpaints
    /// rendered content and mouse hit-testing has a stable target.
    pub fn transcript_width(&self) -> usize {
        let (w, _) = self.ui.terminal_size();
        (self.transcript_gutters.content_width(w) as usize).max(1)
    }

    pub fn notify(&mut self, message: String) {
        self.open_notification(message, ui::NotificationLevel::Info);
    }

    pub fn notify_error(&mut self, message: String) {
        self.open_notification(message, ui::NotificationLevel::Error);
    }

    fn open_notification(&mut self, message: String, level: ui::NotificationLevel) {
        // Replace any existing toast — one at a time.
        if let Some(win) = self.notification.take() {
            self.close_float(win);
        }
        let style = ui::NotificationStyle {
            info_label: ui::Style {
                bold: true,
                ..Default::default()
            },
            error_label: ui::Style {
                fg: Some(crate::theme::ERROR),
                bold: true,
                ..Default::default()
            },
            message: ui::Style::dim(),
            background: ui::Style::default(),
        };
        // Position: one row above the prompt, full width. Rect is
        // refreshed each frame by `sync_notification_float`.
        let (tw, _th) = self.ui.terminal_size();
        let config = ui::FloatConfig {
            placement: ui::Placement::Manual {
                anchor: ui::Anchor::NW,
                row: 0,
                col: 0,
                width: ui::Constraint::Fixed(tw),
                height: ui::Constraint::Fixed(1),
            },
            border: ui::Border::None,
            title: None,
            // Sits below dialogs (default float zindex 50) so a toast
            // never obscures a modal asking for input.
            zindex: 40,
            focusable: false,
            blocks_agent: false,
        };
        self.notification = self.ui.notification_open(config, message, level, style);
    }

    pub fn dismiss_notification(&mut self) {
        if let Some(win) = self.notification.take() {
            self.close_float(win);
        }
    }

    pub fn set_task_label(&mut self, label: String) {
        self.task_label = if label.trim().is_empty() {
            None
        } else {
            Some(label)
        };
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
        let _ = io::stdout().execute(EnterAlternateScreen);
        // Disable DECAWM so writing to the bottom-right cell doesn't
        // trigger the terminal's auto-scroll (which would push a whole
        // row up and break the status bar's last char — see "1:1 100%"
        // wrapping regression).
        let _ = io::stdout().execute(DisableLineWrap);
        let _ = io::stdout().execute(cursor::Hide);
        let _ = io::stdout().execute(EnableBracketedPaste);
        let _ = io::stdout().execute(EnableFocusChange);
        let _ = io::stdout().execute(EnableMouseCapture);

        if !self.history.is_empty() {
            self.restore_screen();
            if let Some(tokens) = self.session.context_tokens {
                self.context_tokens = Some(tokens);
            }
            if let Some(ref slug) = self.session.slug {
                self.set_task_label(slug.clone());
            }
            self.finish_transcript_turn();
            self.transcript_window.scroll_to_bottom();
        }
        if let Some(message) = self.startup_auth_error.take() {
            self.notify_error(message);
        }

        // Plugins read live App state via `with_app` at registration
        // time — e.g. `model.lua` declares `args = smelt.engine.models()`
        // for its arg picker. Install the TLS app pointer before any
        // Lua runs at startup so those reads land on the real App.
        {
            let _guard = crate::lua::install_app_ptr(self);
            self.lua.load_plugins();
            self.lua.emit(crate::lua::AutocmdEvent::SessionStart);
        }
        if let Some(err) = self.lua.load_error.take() {
            self.notify_error(format!("lua init: {err}"));
        }
        self.flush_lua_callbacks();
        // Plugins have now registered their commands — pull every
        // declared `args = {...}` list so the CommandArg picker opens
        // when the user types `/name ` (space).
        self.input.command_arg_sources = self.lua.list_command_args();

        let mut term_events = EventStream::new();

        // Auto-submit initial message if provided (e.g. `agent "fix the bug"`).
        if let Some(msg) = initial_message {
            let trimmed = msg.trim();
            if let Some(cmd) = trimmed.strip_prefix('!') {
                if let Some((rx, kill)) = self.start_shell_escape(cmd) {
                    self.exec_rx = Some(rx);
                    self.exec_kill = Some(kill);
                }
            } else if trimmed == "/resume" || trimmed == "/settings" {
                // Both are Lua-registered commands now; run through the
                // normal dispatcher so the Lua handler fires.
                self.handle_command(trimmed);
            } else if let Some(reason) = classify_startup_command(trimmed) {
                self.notify_error(format!("\"{}\" {}", trimmed, reason));
            } else {
                let content = Content::text(msg.clone());
                let turn = self.begin_agent_turn(&msg, content);
                self.agent = Some(turn);
            }
        }

        let mut t = Timers {
            last_esc: None,
            esc_vim_mode: None,
            last_ctrlc: None,
            last_keypress: None,
            pending_pane_chord: None,
        };
        let mut pending_dialogs: VecDeque<DeferredDialog> = VecDeque::new();
        const MIN_FRAME_INTERVAL: Duration = Duration::from_millis(16);

        'main: loop {
            if self.pending_quit {
                self.discard_turn(true);
                break 'main;
            }
            // Install the TLS app pointer for the whole tick. Any Lua
            // binding firing during this iteration can reach `&mut App`
            // via `crate::lua::with_app`. Guard drops at end of the
            // iteration scope and restores the previous slot (usually
            // None). The pointer is the Neovim-equivalent to Vim's
            // globals — Rust code itself never reads it; only Lua
            // bindings do, and only when their enclosing &Rust borrow
            // is field-disjoint from whatever the binding writes.
            let _app_guard = crate::lua::install_app_ptr(self);
            // ── Lua timer + notification pump ────────────────────────────
            self.lua.tick_timers();
            self.drive_lua_tasks();
            let (items, tick_errors) = self.lua.tick_statusline();
            self.custom_status_items = items;
            for (name, msg) in tick_errors {
                match msg {
                    Some(new_msg) => {
                        if self.statusline_last_errors.get(&name) != Some(&new_msg) {
                            self.notify_error(new_msg.clone());
                            self.statusline_last_errors.insert(name, new_msg);
                        }
                    }
                    None => {
                        self.statusline_last_errors.remove(&name);
                    }
                }
            }
            for _id in self.drain_finished_blocks() {
                self.lua.emit(crate::lua::AutocmdEvent::BlockDone);
            }
            self.flush_lua_callbacks();
            // Fire `WinEvent::Tick` on every window with a registered
            // Tick callback — e.g. Agents pulls a fresh subagent
            // snapshot here each frame.
            {
                let lua = &self.lua;
                let mut lua_invoke =
                    |handle: ui::LuaHandle,
                     win: ui::WinId,
                     payload: &ui::Payload,
                     panels: &[ui::PanelSnapshot]| {
                        lua.queue_invocation(handle, win, payload, panels);
                    };
                self.ui.dispatch_tick(&mut lua_invoke);
            }
            self.flush_lua_callbacks();

            // ── Background polls ─────────────────────────────────────────
            if let Some(ref mut rx) = ctx_rx {
                if let Ok(result) = rx.try_recv() {
                    self.context_window = result;
                    ctx_rx = None;
                }
            }

            // ── Drain engine events (paused only for Confirm) ──
            if !self.focused_float_blocks_agent() {
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
                            self.discard_turn(false);
                            break;
                        }
                    };
                    // Take the TurnState out for the duration of the
                    // dispatch so `handle_engine_event` can borrow its
                    // fields while we still hold `&mut self`.
                    let action = if let Some(mut ag) = self.agent.take() {
                        let ctrl = self.handle_engine_event(ev, ag.turn_id, &mut ag.pending);
                        let action = self.dispatch_control(
                            ctrl,
                            &ag.pending,
                            &mut pending_dialogs,
                            t.last_keypress,
                        );
                        self.agent = Some(ag);
                        action
                    } else {
                        // No active turn — handle out-of-band events.
                        self.handle_engine_event_idle(ev);
                        LoopAction::Continue
                    };
                    match action {
                        LoopAction::Continue => {}
                        LoopAction::Done => {
                            self.discard_turn(false);
                            break;
                        }
                    }
                }
            }

            // ── Auto-start from leftover queued messages (one per turn) ──
            if self.agent.is_none() && !self.queued_messages.is_empty() && !self.is_compacting() {
                let text = self.queued_messages.remove(0);
                if let Some(cmd) = crate::custom_commands::resolve(text.trim(), self.multi_agent) {
                    let turn = self.begin_custom_command_turn(cmd);
                    self.agent = Some(turn);
                } else if !text.is_empty() {
                    let outcome = self.process_input(&text);
                    let content = Content::text(text.clone());
                    // Quit is ignored here: a queued "/exit" shouldn't terminate
                    // the main loop from this re-entry point.
                    self.apply_input_outcome(outcome, content, &text);
                }
            }

            // ── Auto-start from pending agent messages ─────────────────
            if self.agent.is_none() && !self.pending_agent_messages.is_empty() {
                let msgs = std::mem::take(&mut self.pending_agent_messages);
                self.history.extend(msgs);
                let turn = self.begin_agent_message_turn();
                self.agent = Some(turn);
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

                let ctrl = SessionControl::NeedsConfirm(Box::new(ConfirmRequest {
                    call_id: format!("child-perm-{request_id}"),
                    tool_name,
                    desc: confirm_message,
                    args,
                    approval_patterns,
                    outside_dir: None,
                    summary,
                    request_id,
                }));
                let taken = self.agent.take();
                let pending_ref: &[crate::app::PendingTool] =
                    taken.as_ref().map(|a| a.pending.as_slice()).unwrap_or(&[]);
                let action =
                    self.dispatch_control(ctrl, pending_ref, &mut pending_dialogs, t.last_keypress);
                self.agent = taken;
                if matches!(action, LoopAction::Done) {
                    self.discard_turn(false);
                }
            }

            // ── Process pending permission dialogs ──────────────────────
            // If agent was cancelled while dialogs were pending, discard them.
            if self.agent.is_none() && !pending_dialogs.is_empty() {
                pending_dialogs.clear();
                self.pending_dialog = false;
            }
            // Re-dispatch queued dialogs.  Each goes through dispatch_control
            // so auto-approval checks re-run ("always allow" → auto-approve rest).
            if !pending_dialogs.is_empty()
                && !self.focused_float_blocks_agent()
                && self.agent.is_some()
            {
                let idle = t
                    .last_keypress
                    .map(|lk| lk.elapsed() >= Duration::from_millis(CONFIRM_DEFER_MS))
                    .unwrap_or(true);
                while idle
                    && !pending_dialogs.is_empty()
                    && !self.focused_float_blocks_agent()
                    && self.agent.is_some()
                {
                    let deferred = pending_dialogs.pop_front().unwrap();
                    let ctrl = match deferred {
                        DeferredDialog::Confirm(req) => SessionControl::NeedsConfirm(req),
                    };
                    let taken = self.agent.take();
                    let pending_ref: &[crate::app::PendingTool] =
                        taken.as_ref().map(|a| a.pending.as_slice()).unwrap_or(&[]);
                    let action = self.dispatch_control(
                        ctrl,
                        pending_ref,
                        &mut pending_dialogs,
                        t.last_keypress,
                    );
                    self.agent = taken;
                    if matches!(action, LoopAction::Done) {
                        self.discard_turn(false);
                    }
                }
                self.pending_dialog = !pending_dialogs.is_empty();
            }

            // ── Render ───────────────────────────────────────────────────
            self.render_normal(self.agent.is_some());
            let last_frame = Instant::now();

            // Pre-compute animation signal here so the sleep expression
            // inside `tokio::select!` below can read it without holding
            // a borrow on `self` that conflicts with other branches.
            let now = Instant::now();
            let yank_flash_active = self
                .input
                .win
                .kill_ring
                .yank_flash_until()
                .is_some_and(|t| t > now)
                || self
                    .transcript_window
                    .kill_ring
                    .yank_flash_until()
                    .is_some_and(|t| t > now);
            let has_animation = self.ui.focused_float().is_some()
                || self.has_active_exec()
                || self.working.is_animating()
                || yank_flash_active
                || self
                    .agents
                    .iter()
                    .any(|a| a.status == AgentTrackStatus::Working);

            // ── Wait for next event ──────────────────────────────────────
            tokio::select! {
                biased;

                Some(Ok(ev)) = stream_next(&mut term_events) => {
                    // Batch scroll wheel ticks across the drain so a
                    // rapid burst collapses into a single motion + one
                    // render — otherwise each tick repaints the whole
                    // screen and the terminal can't keep up, making
                    // fast scrolling feel laggy or frozen.
                    //
                    // The coalescer only fires when there's no focused
                    // float. With a dialog up, wheel events need to
                    // reach `dispatch_terminal_event` → `handle_mouse`
                    // so they route into the float instead of bleeding
                    // past it into the transcript behind.
                    let coalesce_scroll = self.ui.focused_float().is_none();
                    let mut scroll_delta: isize = 0;
                    let mut scroll_row: u16 = 0;
                    let absorb = |ev: event::Event,
                                      delta: &mut isize,
                                      row: &mut u16|
                     -> Option<event::Event> {
                        if !coalesce_scroll {
                            return Some(ev);
                        }
                        if let event::Event::Mouse(m) = &ev {
                            match m.kind {
                                event::MouseEventKind::ScrollUp => {
                                    *delta -= 3;
                                    *row = m.row;
                                    return None;
                                }
                                event::MouseEventKind::ScrollDown => {
                                    *delta += 3;
                                    *row = m.row;
                                    return None;
                                }
                                _ => {}
                            }
                        }
                        Some(ev)
                    };

                    if let Some(ev) = absorb(ev, &mut scroll_delta, &mut scroll_row) {
                        if self.dispatch_terminal_event(ev, &mut t) {
                            break 'main;
                        }
                    }

                    // Drain buffered terminal events (coalesce scroll).
                    while event::poll(Duration::ZERO).unwrap_or(false) {
                        if let Ok(ev) = event::read() {
                            if let Some(ev) = absorb(ev, &mut scroll_delta, &mut scroll_row) {
                                if self.dispatch_terminal_event(ev, &mut t) {
                                    break 'main;
                                }
                            }
                        }
                    }

                    // Apply any accumulated scroll as a single motion.
                    if scroll_delta != 0 {
                        self.scroll_under_mouse(scroll_row, scroll_delta);
                    }

                    self.render_normal(self.agent.is_some());
                }

                Some(ev) = self.engine.recv(), if !self.focused_float_blocks_agent() => {
                    if let Some(mut ag) = self.agent.take() {
                        let ctrl = self.handle_engine_event(ev, ag.turn_id, &mut ag.pending);
                        let action = self.dispatch_control(
                            ctrl,
                            &ag.pending,
                            &mut pending_dialogs,
                            t.last_keypress,
                        );
                        self.agent = Some(ag);
                        if matches!(action, LoopAction::Done) {
                            self.discard_turn(false);
                        }
                    } else {
                        // No active turn — handle out-of-band events.
                        self.handle_engine_event_idle(ev);
                    }
                    // Don't render here — deferred to the frame timer or
                    // top-of-loop render to batch rapid engine events into
                    // fewer frames and reduce flicker.
                }

                Some(ev) = async {
                    match self.exec_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    match ev {
                        commands::ExecEvent::Output(line) => {
                            self.append_exec_output(&line);
                        }
                        commands::ExecEvent::Done(code) => {
                            self.finish_exec(code);
                            self.finalize_exec();
                            self.exec_rx = None;
                            self.exec_kill = None;
                        }
                    }
                }

                _ = tokio::time::sleep({
                    // Fires only when there's real time-driven work:
                    // drag-autoscroll advances one line per tick (the
                    // interval ramps down the longer the cursor is
                    // parked at the edge), and animations (spinner,
                    // exec output, working agent, yank flash) drive
                    // frames at `MIN_FRAME_INTERVAL`. When neither is
                    // active, this arm is disabled via the `if` guard
                    // below — the loop parks on terminal / engine /
                    // exec channels and CPU goes to ~0% until the next
                    // event. An idle timer here would wake the loop
                    // every 80ms to redraw the same screen.
                    let since = last_frame.elapsed();
                    let want = if let Some(started) = self.drag_autoscroll_since {
                        let held = started.elapsed().as_millis() as u64;
                        // Start at ~33 lines/sec (30 ms), ramp to ~200 lines/sec (5 ms).
                        let ms = 30u64.saturating_sub(held / 120).max(5);
                        Duration::from_millis(ms)
                    } else {
                        MIN_FRAME_INTERVAL
                    };
                    want.saturating_sub(since)
                }), if has_animation || self.drag_autoscroll_since.is_some() => {
                    // Auto-scroll while the user is mid-drag with the
                    // cursor parked on the top/bottom row of the
                    // transcript — extends selection past the viewport
                    // without requiring further mouse motion.
                    self.tick_drag_autoscroll();
                    // Render deferred engine events + animations.
                    self.render_normal(self.agent.is_some());
                }
            }
        }

        // Cleanup
        if self.agent.is_some() {
            self.finish_turn(true);
        }
        self.lua.emit(crate::lua::AutocmdEvent::Shutdown);
        self.save_session();

        let _ = io::stdout().execute(DisableMouseCapture);
        let _ = io::stdout().execute(EnableLineWrap);
        let _ = io::stdout().execute(LeaveAlternateScreen);
        let _ = io::stdout().execute(cursor::Show);
        let _ = io::stdout().execute(DisableBracketedPaste);
        let _ = io::stdout().execute(DisableFocusChange);
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

        headless::init_color_mode(color_mode);

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

        // Commands require interactive mode.
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
            system_prompt: None,
            plugin_tools: vec![],
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
                    headless::emit_json(&ev);

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
                        EngineEvent::TurnError { .. } | EngineEvent::TurnComplete { .. } => {
                            break;
                        }
                        _ => {}
                    }
                }
                OutputFormat::Text => match ev {
                    EngineEvent::ThinkingDelta { .. } => {}
                    EngineEvent::Thinking { content } => {
                        headless::log_thinking(&content);
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
                    EngineEvent::ToolOutput { call_id, chunk } if verbose => {
                        if let Some((_, _, output)) = pending_tools.get_mut(&call_id) {
                            output.push_str(&chunk);
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
                            String::new()
                        } else if result.is_error {
                            result.content.clone()
                        } else {
                            output
                        };
                        headless::log_tool(
                            &name,
                            &summary,
                            &display_output,
                            result.is_error,
                            elapsed_ms,
                        );
                    }
                    EngineEvent::TokenUsage {
                        usage,
                        tokens_per_sec,
                        cost_usd,
                        ..
                    } => {
                        total_cost += cost_usd.unwrap_or(0.0);
                        total_usage.accumulate(&usage);
                        last_tps = tokens_per_sec.or(last_tps);
                    }
                    EngineEvent::Retrying { delay_ms, attempt } => {
                        headless::log_retry(attempt, delay_ms);
                    }
                    EngineEvent::RequestPermission { request_id, .. } => {
                        let approved = self.mode == Mode::Yolo;
                        self.engine.send(UiCommand::PermissionDecision {
                            request_id,
                            approved,
                            message: None,
                        });
                    }
                    EngineEvent::Messages { .. } => {}
                    EngineEvent::TurnError { message } => {
                        headless::log_error(&message);
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
            headless::log_token_usage(&total_usage, last_tps, total_cost);
        }

        // Text mode: write the final message to stdout (only when piped).
        // `final_message` is model-generated and passes through unredacted.
        if format == OutputFormat::Text && !final_message.is_empty() {
            use std::io::IsTerminal;
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
        headless::emit_json(&EngineEvent::AgentMessage {
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

    /// Send a Btw query to the engine on behalf of a querying peer.
    fn send_btw_query(&self, question: String) {
        self.engine.send(UiCommand::Btw {
            question,
            history: self.history.clone(),
            reasoning_effort: self.reasoning_effort,
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
                                headless::emit_json(&ev);
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
                last_user_message: text,
                assistant_tail: String::new(),
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
            system_prompt: None,
            plugin_tools: vec![],
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
                    headless::emit_json(&ev);

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
