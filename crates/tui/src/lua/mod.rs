//! Lua bindings (Phase D). Wraps the `api::*` surface so users can
//! script smelt from `~/.config/smelt/init.lua`.
//!
//! Current scope:
//! - **D1 bootstrap** — loads `~/.config/smelt/init.lua` at startup
//!   (honouring `XDG_CONFIG_HOME`). Missing files are not errors.
//! - **D2 api shim** — `smelt.version`, `smelt.cmd.register`,
//!   `smelt.keymap.set`, `smelt.on` all accept Lua callables and store
//!   them in per-category registries that the app polls on the tick.
//! - **D3 autocmd dispatch** — `AutocmdRegistry` + `emit_autocmd` run
//!   handlers synchronously; errors are logged and the next handler
//!   runs.
//! - **D4 user-command + keymap registration** — registration stores
//!   `LuaRef` handles keyed by `(mode, chord)`; mode `"n"` matches
//!   Normal, `"i"` Insert, `"v"` Visual, `""` matches any mode.
//! - **D5 re-entrancy** — pending ops queue defers state mutations
//!   until after the dispatching handler returns. `smelt.defer(ms, fn)`
//!   posts to `pending_timers`; the tick loop fires them when due.
//! - **D6 error UX** — every callable is wrapped in `try_call`;
//!   errors append to `lua_errors` and the app surfaces the first as a
//!   notification on the next tick.

mod api;
pub mod app_ref;
mod confirm_ops;
mod task;
mod tasks;
pub mod ui_ops;

pub use app_ref::{install_app_ptr, try_with_app, with_app, AppPtrGuard};

pub use task::{LuaTaskRuntime, TaskCompletion, TaskDriveOutput};

/// Outcome of invoking a plugin tool handler.
pub enum ToolExecResult {
    /// Handler returned without yielding — caller forwards this
    /// content to the engine immediately.
    Immediate { content: String, is_error: bool },
    /// Handler yielded (called an API that suspends on the
    /// `LuaTask` runtime). The result will arrive later via
    /// `drive_tasks() -> TaskDriveOutput::ToolComplete`.
    Pending,
}

use mlua::prelude::*;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

/// Metadata a Lua plugin attaches to a `/command` via
/// `smelt.cmd.register(name, fn, opts)`. `description` powers the
/// `/`-completer hint; `args` drives the secondary `CommandArg`
/// completer that opens after the user types `/name ` (space).
#[derive(Clone, Default)]
pub struct CommandMeta {
    pub description: Option<String>,
    pub args: Vec<String>,
}

/// Process-global snapshot of Lua-registered `/command` names and their
/// metadata. Written by `smelt.cmd.register`, read by
/// `Completer::commands` / `Completer::is_command` and by the
/// `CommandArg` arg picker — same free-function pattern as
/// `custom_commands::list` / `builtin_commands::list`.
///
/// We keep a separate plain-data snapshot (instead of exposing
/// `LuaShared` directly) because `LuaHandle` contains `!Send`
/// `mlua::RegistryKey` and cannot live in a static, and because
/// completers only need strings.
fn lua_commands_snapshot() -> &'static Mutex<HashMap<String, CommandMeta>> {
    static S: OnceLock<Mutex<HashMap<String, CommandMeta>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

/// List all Lua-registered `/commands` as `(name, description)`.
/// Sorted by name. Used by the `/` completer.
pub fn list_commands() -> Vec<(String, Option<String>)> {
    let mut items: Vec<(String, Option<String>)> = lua_commands_snapshot()
        .lock()
        .map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), v.description.clone()))
                .collect()
        })
        .unwrap_or_default();
    items.sort_by(|a, b| a.0.cmp(&b.0));
    items
}

/// Return every Lua-registered command that declared an `args` list,
/// as `("/cmd", args)` pairs. Used by `PromptState::command_arg_sources`
/// to drive the secondary arg picker that opens after `/cmd <space>`.
pub fn list_command_args() -> Vec<(String, Vec<String>)> {
    let mut items: Vec<(String, Vec<String>)> = lua_commands_snapshot()
        .lock()
        .map(|m| {
            m.iter()
                .filter(|(_, v)| !v.args.is_empty())
                .map(|(k, v)| (format!("/{k}"), v.args.clone()))
                .collect()
        })
        .unwrap_or_default();
    items.sort_by(|a, b| a.0.cmp(&b.0));
    items
}

/// True if `input` (e.g. `/pick-test` or `/pick-test arg`) matches a
/// Lua-registered command name.
pub fn is_lua_command(input: &str) -> bool {
    let name = input
        .strip_prefix('/')
        .and_then(|s| s.split_whitespace().next())
        .unwrap_or("");
    if name.is_empty() {
        return false;
    }
    lua_commands_snapshot()
        .lock()
        .map(|m| m.contains_key(name))
        .unwrap_or(false)
}

/// Event kinds the app emits into the Lua autocmd dispatcher.
///
/// "Simple" events carry no data — handlers receive the event name as
/// a string argument.  "Data" events carry a Lua table with structured
/// fields — handlers receive `(event_name, data_table)`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum AutocmdEvent {
    // ── simple (no payload) ─────────────────────────────────────────
    BlockDone,
    CmdPre,
    CmdPost,
    SessionStart,
    Shutdown,
    // ── data-carrying ───────────────────────────────────────────────
    TurnStart,
    TurnEnd,
    ModeChange,
    ModelChange,
    ToolStart,
    ToolEnd,
    InputSubmit,
}

/// Format a `crossterm::KeyEvent` into an nvim-style chord string
/// (`<C-g>`, `<S-Tab>`, `<M-x>`, printable `j`, etc). Unrecognized
/// chords return `None` so the dispatcher falls through to the normal
/// handlers. This is the lookup key for `smelt.keymap.set(_, chord, fn)`.
pub fn chord_string(key: crossterm::event::KeyEvent) -> Option<String> {
    use crossterm::event::{KeyCode, KeyModifiers as M};
    let mods = key.modifiers;
    let has_ctrl = mods.contains(M::CONTROL);
    let has_alt = mods.contains(M::ALT);
    let has_shift = mods.contains(M::SHIFT);
    let base = match key.code {
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Tab => "Tab".to_string(),
        KeyCode::BackTab => "Tab".to_string(),
        KeyCode::Enter => "CR".to_string(),
        KeyCode::Esc => "Esc".to_string(),
        KeyCode::Backspace => "BS".to_string(),
        KeyCode::Delete => "Del".to_string(),
        KeyCode::Up => "Up".to_string(),
        KeyCode::Down => "Down".to_string(),
        KeyCode::Left => "Left".to_string(),
        KeyCode::Right => "Right".to_string(),
        KeyCode::Home => "Home".to_string(),
        KeyCode::End => "End".to_string(),
        KeyCode::PageUp => "PageUp".to_string(),
        KeyCode::PageDown => "PageDown".to_string(),
        KeyCode::F(n) => format!("F{n}"),
        KeyCode::Insert => "Insert".to_string(),
        _ => return None,
    };
    let is_named = !matches!(key.code, KeyCode::Char(_));
    if !has_ctrl && !has_alt && (!has_shift || matches!(key.code, KeyCode::Char(_))) && !is_named {
        // Plain printable char — no angle-bracket wrap.
        return Some(base);
    }
    let mut prefix = String::new();
    if has_ctrl {
        prefix.push_str("C-");
    }
    if has_alt {
        prefix.push_str("M-");
    }
    if has_shift && is_named {
        prefix.push_str("S-");
    }
    Some(format!("<{prefix}{base}>"))
}

/// Parse a plugin-facing key spec like `"enter"`, `"esc"`, `"tab"`,
/// `"bs"`, `"space"`, `"up"`, `"c-j"` (ctrl-j), `"a-x"` / `"m-x"`
/// (alt-x), `"s-tab"` (shift-tab), or a single printable char into a
/// [`ui::KeyBind`]. Modifiers separate with `-`; the final token is
/// the key name. Case-insensitive for names and modifiers. Returns
/// `None` for unknown keys — the caller surfaces a Lua error.
pub(crate) fn parse_keybind(spec: &str) -> Option<ui::KeyBind> {
    use crossterm::event::{KeyCode, KeyModifiers};
    let raw = spec.trim();
    if raw.is_empty() {
        return None;
    }
    let (mods, name) = match raw.rsplit_once('-') {
        Some((prefix, name)) => {
            let mut mods = KeyModifiers::NONE;
            for part in prefix.split('-') {
                match part.to_ascii_lowercase().as_str() {
                    "ctrl" | "c" => mods |= KeyModifiers::CONTROL,
                    "alt" | "a" | "meta" | "m" => mods |= KeyModifiers::ALT,
                    "shift" | "s" => mods |= KeyModifiers::SHIFT,
                    _ => return None,
                }
            }
            (mods, name)
        }
        None => (KeyModifiers::NONE, raw),
    };
    let code = match name.to_ascii_lowercase().as_str() {
        "bs" | "backspace" => KeyCode::Backspace,
        "tab" => {
            if mods.contains(KeyModifiers::SHIFT) {
                return Some(ui::KeyBind::new(
                    KeyCode::BackTab,
                    mods - KeyModifiers::SHIFT,
                ));
            }
            KeyCode::Tab
        }
        "del" | "delete" => KeyCode::Delete,
        "enter" | "return" | "cr" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "space" => KeyCode::Char(' '),
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" => KeyCode::PageDown,
        s if s.chars().count() == 1 => KeyCode::Char(name.chars().next().unwrap()),
        _ => return None,
    };
    Some(ui::KeyBind::new(code, mods))
}

/// Normalize a mode string from a Lua plugin into the canonical
/// single-char form the dispatcher stores (`"n" | "i" | "v" | ""`).
/// Accepts long names (`"normal"`, `"insert"`, `"visual"`), short
/// names (`"n"`, `"i"`, `"v"`), the empty string (mode-independent),
/// or `"any" / "*"` as aliases for "". Case-insensitive. Returns
/// `None` on unknown input so the caller surfaces a Lua error.
pub(crate) fn normalize_mode(mode: &str) -> Option<String> {
    Some(
        match mode.trim().to_ascii_lowercase().as_str() {
            "" | "*" | "any" | "all" => "",
            "n" | "normal" => "n",
            "i" | "insert" => "i",
            "v" | "visual" => "v",
            _ => return None,
        }
        .to_string(),
    )
}

/// Canonicalize a Lua-supplied chord string into the nvim-angle-bracket
/// form that `chord_string` emits from key events. Accepts plain Lua
/// shorthand (`"c-r"`, `"s-tab"`, `"enter"`) *and* already-canonical
/// (`"<C-r>"`, `"<S-Tab>"`) input. Plain printable chars stay plain
/// (`"j"`). Returns `None` on unknown keys so the caller raises a Lua
/// error at registration.
pub(crate) fn canonicalize_chord(chord: &str) -> Option<String> {
    use crossterm::event::KeyEvent;
    let stripped = chord
        .trim()
        .strip_prefix('<')
        .and_then(|s| s.strip_suffix('>'))
        .unwrap_or(chord.trim());
    let kb = parse_keybind(stripped)?;
    chord_string(KeyEvent::new(kb.code, kb.mods))
}

/// Parse a Lua-facing window-event name into a [`ui::WinEvent`]. Names
/// match the Neovim-adjacent naming Lua plugins use for autocmd-style
/// hooks. Returns `None` for unknown names so the caller surfaces a
/// Lua error.
pub(crate) fn parse_win_event(name: &str) -> Option<ui::WinEvent> {
    Some(match name {
        "open" => ui::WinEvent::Open,
        "close" => ui::WinEvent::Close,
        "focus" | "focus_gained" => ui::WinEvent::FocusGained,
        "blur" | "focus_lost" => ui::WinEvent::FocusLost,
        "selection_changed" | "select_changed" => ui::WinEvent::SelectionChanged,
        "submit" => ui::WinEvent::Submit,
        "text_changed" | "change" => ui::WinEvent::TextChanged,
        "dismiss" | "cancel" => ui::WinEvent::Dismiss,
        "tick" => ui::WinEvent::Tick,
        _ => return None,
    })
}

impl AutocmdEvent {
    pub fn lua_name(&self) -> &'static str {
        match self {
            Self::BlockDone => "block_done",
            Self::CmdPre => "cmd_pre",
            Self::CmdPost => "cmd_post",
            Self::SessionStart => "session_start",
            Self::Shutdown => "shutdown",
            Self::TurnStart => "turn_start",
            Self::TurnEnd => "turn_end",
            Self::ModeChange => "mode_change",
            Self::ModelChange => "model_change",
            Self::ToolStart => "tool_start",
            Self::ToolEnd => "tool_end",
            Self::InputSubmit => "input_submit",
        }
    }

    fn from_lua_name(s: &str) -> Option<Self> {
        match s {
            "block_done" => Some(Self::BlockDone),
            "cmd_pre" => Some(Self::CmdPre),
            "cmd_post" => Some(Self::CmdPost),
            "session_start" => Some(Self::SessionStart),
            "shutdown" => Some(Self::Shutdown),
            "turn_start" => Some(Self::TurnStart),
            "turn_end" => Some(Self::TurnEnd),
            "mode_change" => Some(Self::ModeChange),
            "model_change" => Some(Self::ModelChange),
            "tool_start" => Some(Self::ToolStart),
            "tool_end" => Some(Self::ToolEnd),
            "input_submit" => Some(Self::InputSubmit),
            // Legacy aliases
            "stream_start" => Some(Self::TurnStart),
            "stream_end" => Some(Self::TurnEnd),
            _ => None,
        }
    }
}

/// A Lua callable registered via `smelt.cmd.register` / `smelt.keymap` /
/// `smelt.on`. Stored as a mlua `RegistryKey` so references survive
/// across GC cycles and can be invoked from Rust handlers.
pub(crate) struct LuaHandle {
    key: mlua::RegistryKey,
}

/// Stash a Lua callable in `shared.callbacks` under a fresh u64 id.
/// Used by every `smelt.win.*` binding that takes a callback — pulls
/// the registry-value + atomic-id + insert dance out of the bindings.
pub(crate) fn register_callback_handle(
    shared: &Arc<LuaShared>,
    lua: &Lua,
    func: mlua::Function,
) -> mlua::Result<u64> {
    let key = lua.create_registry_value(func)?;
    let id = shared
        .next_id
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if let Ok(mut cbs) = shared.callbacks.lock() {
        cbs.insert(id, LuaHandle { key });
    }
    Ok(id)
}

/// Drop the Lua handle id stashed in a displaced `Callback::Lua`, if
/// the option is one. Used wherever a `win_set_keymap` / `win_clear_*`
/// returns the callback that was just replaced or removed.
pub(crate) fn drop_displaced_lua_handle(
    app: &mut crate::app::App,
    displaced: Option<ui::Callback>,
) {
    if let Some(ui::Callback::Lua(ui::LuaHandle(old))) = displaced {
        app.lua.remove_callback(old);
    }
}

/// Snapshot of engine-level state (model, mode, cost, tokens).
/// Populated by `snapshot_engine_context` in the app loop.
#[derive(Clone, Default)]
pub struct EngineSnapshot {
    pub model: String,
    pub mode: String,
    pub reasoning_effort: String,
    pub is_busy: bool,
    pub session_cost: f64,
    pub context_tokens: Option<u32>,
    pub context_window: Option<u32>,
    pub session_dir: String,
    pub session_id: String,
    pub session_title: Option<String>,
    pub session_cwd: String,
    pub session_created_at_ms: u64,
    /// User turn positions: `(block_idx, text)` for each `Block::User`.
    pub session_turns: Vec<(usize, String)>,
    /// Vim emulation setting (from settings, not the current vim mode).
    pub vim_enabled: bool,
    /// Current-session permission rules: `(tool, pattern)` with
    /// `pattern = "*"` meaning blanket-allow for the tool, or `tool =
    /// "directory"` for path-based approvals.
    pub permission_session_entries: Vec<(String, String)>,
}

/// Snapshot fields populated by `set_context()` before a handler
/// runs. Lua reads these via `smelt.transcript.text()`,
/// `smelt.engine.snapshot()`, etc. Writes go through `with_app` —
/// Lua callbacks reach `&mut App` directly and don't queue here.
#[derive(Default)]
pub struct LuaOps {
    // ── reads (snapshot) — UI state ─────────────────────────────────
    pub transcript_text: Option<String>,
    pub prompt_text: Option<String>,
    pub focused_window: Option<String>,
    pub vim_mode: Option<String>,
    // ── reads (snapshot) — engine state ─────────────────────────────
    pub engine: EngineSnapshot,
    /// `(key, display_name, provider_name)` for every model the user
    /// can switch to — exposed via `smelt.engine.models()`. Written
    /// once at startup; session-immutable.
    pub available_models: Vec<(String, String, String)>,
    /// User-facing boolean settings as `(key, is_on)`. Exposed via
    /// `smelt.settings.snapshot()`. Refreshed only when settings
    /// mutate (not every tick).
    pub settings: Vec<(String, bool)>,
    /// Input history (past submitted prompts, oldest first). Exposed
    /// via `smelt.history.entries()` and scored by `smelt.history.search()`.
    /// Refreshed on every submit + at startup.
    pub input_history: Vec<String>,
}

impl LuaOps {
    pub fn set_context(
        &mut self,
        transcript_text: Option<String>,
        prompt_text: Option<String>,
        focused_window: Option<String>,
        vim_mode: Option<String>,
    ) {
        self.transcript_text = transcript_text;
        self.prompt_text = prompt_text;
        self.focused_window = focused_window;
        self.vim_mode = vim_mode;
    }

    pub fn clear_context(&mut self) {
        self.transcript_text = None;
        self.prompt_text = None;
        self.focused_window = None;
        self.vim_mode = None;
    }
}

/// One registered `smelt.statusline.register` entry. `default_align`
/// applies to items the source returns without an explicit
/// `align_right` field; items can still override per-item.
pub(crate) struct StatusSource {
    pub(crate) handle: LuaHandle,
    pub(crate) default_align_right: bool,
}

/// All shared state between Lua closures and the app loop.
/// One `Arc<LuaShared>` replaces N separate `Arc<Mutex<…>>` fields.
pub(crate) struct LuaShared {
    pub(crate) ops: Mutex<LuaOps>,
    pub(crate) commands: Mutex<HashMap<String, LuaHandle>>,
    pub(crate) keymaps: Mutex<HashMap<(String, String), LuaHandle>>,
    pub(crate) autocmds: Mutex<HashMap<AutocmdEvent, Vec<LuaHandle>>>,
    pub(crate) timers: Mutex<Vec<(Instant, LuaHandle)>>,
    /// Statusline sources in registration order. A `Vec` (not a
    /// `HashMap`) so the on-screen left-to-right order matches the
    /// order plugins called `smelt.statusline.register`. Re-registering
    /// an existing name updates in place without changing position.
    pub(crate) statusline_sources: Mutex<Vec<(String, StatusSource)>>,
    /// Last error message reported per statusline source. Used to
    /// rate-limit `notify_error` so a perpetually-broken source doesn't
    /// spam one toast per frame — only re-notify when the message
    /// changes; clear the entry on a successful tick.
    pub(crate) statusline_last_errors: Mutex<HashMap<String, String>>,
    pub(crate) plugin_tools: Mutex<HashMap<String, LuaHandle>>,
    pub(crate) callbacks: Mutex<HashMap<u64, LuaHandle>>,
    pub(crate) next_id: AtomicU64,
    /// Separate counter for buffer IDs minted by `smelt.buf.create`.
    /// Starts at `1 << 32` so Lua-allocated `BufId`s never collide with
    /// Rust-side buffers (prompt input, scratch, etc.) that are minted
    /// by `ui.buf_create` from 1.
    pub(crate) next_buf_id: AtomicU64,
    /// Lock-free counter for `smelt.task.alloc`. Lives on the
    /// shared arc (not in `LuaTaskRuntime`) so a Lua coroutine running
    /// *inside* `drive_tasks` — which already holds the `tasks` lock —
    /// can mint an id without re-entering the same mutex.
    pub(crate) next_external_id: AtomicU64,
    pub(crate) history: Mutex<Arc<Vec<protocol::Message>>>,
    pub(crate) tasks: Mutex<LuaTaskRuntime>,
    /// Background process registry. Installed by `App::new` so Lua
    /// plugins (e.g. `/ps`) can enumerate and kill procs.
    pub(crate) processes: Mutex<Option<engine::tools::ProcessRegistry>>,
    /// Shared list of subagent snapshots, installed by `App::new` so
    /// `smelt.agent.snapshots` can return live prompt / tool-call /
    /// cost data without touching App directly.
    pub(crate) agent_snapshots: Mutex<Option<crate::app::SharedSnapshots>>,
    /// Task-runtime inbox. Dialog callbacks / other UI events that need
    /// to *resume a Lua coroutine* push here instead of through `ops`.
    /// Keeps the reducer's `AppOp` enum free of Lua-task variants; the
    /// Lua module pumps its own inbox each tick.
    pub(crate) task_inbox: Mutex<Vec<TaskEvent>>,
    /// Pending Lua keymap / event callback invocations. Recorded during
    /// `ui.handle_key` / `ui.dispatch_event` (where `&mut Ui` is held
    /// and the Lua body therefore cannot call back into App state),
    /// drained by App right after the ui call returns so each Lua body
    /// runs with the TLS app pointer installed and sole access to App.
    /// Without this deferral, a Lua callback that calls
    /// `smelt.ui.dialog.open` would collide with the ui borrow.
    pub(crate) pending_invocations: Mutex<Vec<PendingInvocation>>,
}

/// A callback invocation recorded by the ui dispatch path while
/// `&mut Ui` is held. Drained by the host App between ui calls so each
/// Lua fn body runs with the TLS app pointer installed.
pub struct PendingInvocation {
    pub handle: ui::LuaHandle,
    pub win: ui::WinId,
    pub payload: ui::Payload,
    pub panels: Vec<ui::PanelSnapshot>,
}

/// Events that drive the Lua task runtime. After the D3 dialog + D2b
/// picker ports both runtime files (`runtime/lua/smelt/dialog.lua`,
/// `runtime/lua/smelt/picker.lua`) register `Callback::Lua` handlers
/// directly via `smelt.win.set_keymap` / `on_event` and resolve
/// themselves via `smelt.task.resume`, so the only remaining
/// event is the externally-allocated resume itself.
pub enum TaskEvent {
    /// `smelt.task.resume(id, value)` posts this to route the
    /// resume through the Lua pump. The pump looks up the parked task
    /// by `id` and resumes it with the stored value on the next
    /// `pump_task_events` drain.
    ExternalResolved {
        external_id: u64,
        value: mlua::RegistryKey,
    },
}

impl Default for LuaShared {
    fn default() -> Self {
        Self {
            ops: Mutex::new(LuaOps::default()),
            commands: Mutex::new(HashMap::new()),
            keymaps: Mutex::new(HashMap::new()),
            autocmds: Mutex::new(HashMap::new()),
            timers: Mutex::new(Vec::new()),
            statusline_sources: Mutex::new(Vec::new()),
            statusline_last_errors: Mutex::new(HashMap::new()),
            plugin_tools: Mutex::new(HashMap::new()),
            callbacks: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            next_buf_id: AtomicU64::new(ui::LUA_BUF_ID_BASE),
            next_external_id: AtomicU64::new(1),
            history: Mutex::new(Arc::new(Vec::new())),
            tasks: Mutex::new(LuaTaskRuntime::new()),
            processes: Mutex::new(None),
            agent_snapshots: Mutex::new(None),
            task_inbox: Mutex::new(Vec::new()),
            pending_invocations: Mutex::new(Vec::new()),
        }
    }
}

/// Convert a slice of protocol messages to a Lua table array.
pub fn messages_to_lua(lua: &Lua, msgs: &[protocol::Message]) -> LuaResult<mlua::Table> {
    let tbl = lua.create_table()?;
    for (i, msg) in msgs.iter().enumerate() {
        let entry = lua.create_table()?;
        let role = match msg.role {
            protocol::Role::System => "system",
            protocol::Role::User => "user",
            protocol::Role::Assistant => "assistant",
            protocol::Role::Tool => "tool",
            protocol::Role::Agent => "agent",
        };
        entry.set("role", role)?;
        if let Some(ref c) = msg.content {
            entry.set("content", c.text_content())?;
        }
        if let Some(ref tc) = msg.tool_calls {
            let calls = lua.create_table()?;
            for (j, call) in tc.iter().enumerate() {
                let ct = lua.create_table()?;
                ct.set("id", call.id.as_str())?;
                ct.set("name", call.function.name.as_str())?;
                ct.set("arguments", call.function.arguments.as_str())?;
                calls.set(j + 1, ct)?;
            }
            entry.set("tool_calls", calls)?;
        }
        if let Some(ref id) = msg.tool_call_id {
            entry.set("tool_call_id", id.as_str())?;
        }
        if msg.is_error {
            entry.set("is_error", true)?;
        }
        tbl.set(i + 1, entry)?;
    }
    Ok(tbl)
}

/// User-scoped Lua state + any recorded startup error.
pub struct LuaRuntime {
    pub lua: Lua,
    pub load_error: Option<String>,
    shared: Arc<LuaShared>,
}

impl LuaRuntime {
    /// Build a fresh runtime and register the `smelt` global.
    ///
    /// *Does not* load plugins or `init.lua` — call
    /// [`LuaRuntime::load_plugins`] after pushing startup snapshots
    /// (available models, settings, history) so plugins that read those
    /// at registration time (e.g. `/model` declaring `args = model_keys`)
    /// see real data.
    pub fn new() -> Self {
        let lua = Lua::new();
        // `Arc<LuaShared>` is single-threaded in practice (all Lua
        // callbacks fire on the TUI thread). The task runtime holds
        // `mlua::Thread` which is !Send, so the Arc is flagged by
        // clippy. Allow explicitly — we never clone across threads.
        #[allow(clippy::arc_with_non_send_sync)]
        let shared = Arc::new(LuaShared::default());

        let load_error = Self::register_api(&lua, &shared)
            .err()
            .map(|e| e.to_string());

        let mut rt = Self {
            lua,
            load_error,
            shared,
        };

        if rt.load_error.is_none() {
            if let Err(e) = register_embedded_searcher(&rt.lua) {
                rt.load_error = Some(format!("embedded searcher: {e}"));
            }
        }

        rt
    }

    /// Run autoload plugins and `~/.config/smelt/init.lua`. Call
    /// *after* pushing startup snapshots so plugins see populated
    /// `smelt.engine.models()` etc.
    pub fn load_plugins(&mut self) {
        if self.load_error.is_some() {
            return;
        }
        for &name in AUTOLOAD_MODULES {
            let code = format!("require('{name}')");
            if let Err(e) = self.lua.load(&code).set_name(name).exec() {
                self.load_error = Some(format!("autoload {name}: {e}"));
                return;
            }
        }
        if let Some(path) = init_lua_path() {
            if path.exists() {
                if let Err(e) = self.load_init(&path) {
                    self.load_error = Some(format!("~/.config/smelt/init.lua: {e}"));
                }
            }
        }
    }

    fn load_init(&mut self, path: &std::path::Path) -> LuaResult<()> {
        let src = std::fs::read_to_string(path)
            .map_err(|e| LuaError::RuntimeError(format!("read init.lua: {e}")))?;
        self.lua.load(&src).set_name("init.lua").exec()
    }

    /// Populate the snapshot fields before dispatching a Lua callback.
    pub fn set_context(
        &self,
        transcript_text: Option<String>,
        prompt_text: Option<String>,
        focused_window: Option<String>,
        vim_mode: Option<String>,
    ) {
        if let Ok(mut o) = self.shared.ops.lock() {
            o.set_context(transcript_text, prompt_text, focused_window, vim_mode);
        }
    }

    /// Push just the prompt text into the read-side snapshot. Used when
    /// firing `WinEvent::TextChanged` so handlers reading
    /// `smelt.prompt.text()` see the fresh buffer without going through
    /// a full `set_context` pass.
    pub fn set_prompt_text_snapshot(&self, text: String) {
        if let Ok(mut o) = self.shared.ops.lock() {
            o.prompt_text = Some(text);
        }
    }

    /// Populate the engine snapshot fields. Called once at startup and
    /// whenever the engine state changes (mode, model, cost, tokens).
    pub fn set_engine_context(&self, snap: EngineSnapshot) {
        if let Ok(mut o) = self.shared.ops.lock() {
            o.engine = snap;
        }
    }

    /// Snapshot the set of models the user can switch to. Called once
    /// at startup — this list is session-immutable.
    pub fn set_available_models(&self, models: Vec<(String, String, String)>) {
        if let Ok(mut o) = self.shared.ops.lock() {
            o.available_models = models;
        }
    }

    /// Snapshot current user-facing settings. Called at startup and
    /// whenever a setting toggles — not every tick.
    pub fn set_settings_snapshot(&self, settings: Vec<(String, bool)>) {
        if let Ok(mut o) = self.shared.ops.lock() {
            o.settings = settings;
        }
    }

    /// Snapshot the input history (past submitted prompts). Called at
    /// startup and whenever a new entry is pushed.
    pub fn set_input_history(&self, entries: Vec<String>) {
        if let Ok(mut o) = self.shared.ops.lock() {
            o.input_history = entries;
        }
    }

    /// Update the history snapshot. Called from `snapshot_engine_context`.
    pub fn set_history(&self, history: Vec<protocol::Message>) {
        if let Ok(mut h) = self.shared.history.lock() {
            *h = Arc::new(history);
        }
    }

    /// Install the background process registry so `smelt.process.*`
    /// primitives can enumerate and kill procs. Called once at App start.
    pub fn set_process_registry(&self, registry: engine::tools::ProcessRegistry) {
        if let Ok(mut p) = self.shared.processes.lock() {
            *p = Some(registry);
        }
    }

    /// Install the shared subagent-snapshot list so
    /// `smelt.agent.snapshots()` can return live prompt /
    /// tool-call / cost data. Called once at App start.
    pub fn set_agent_snapshots(&self, snaps: crate::app::SharedSnapshots) {
        if let Ok(mut s) = self.shared.agent_snapshots.lock() {
            *s = Some(snaps);
        }
    }

    /// Clear the snapshot fields after dispatching.
    pub fn clear_context(&self) {
        if let Ok(mut o) = self.shared.ops.lock() {
            o.clear_context();
        }
    }

    /// Invoke a registered command by name. Returns `true` when the
    /// command exists and was dispatched (regardless of whether the
    /// handler succeeded); `false` when the name isn't bound.
    pub fn run_command(&self, name: &str, arg: Option<String>) -> bool {
        let func = {
            let Ok(map) = self.shared.commands.lock() else {
                return false;
            };
            let Some(handle) = map.get(name) else {
                return false;
            };
            let Ok(f) = self.lua.registry_value::<mlua::Function>(&handle.key) else {
                return false;
            };
            f
        };
        let result: LuaResult<()> = match arg {
            Some(a) => func.call::<()>(a),
            None => func.call::<()>(()),
        };
        if let Err(e) = result {
            self.record_error(format!("cmd `{name}`: {e}"));
        }
        true
    }

    /// Dispatch a keymap chord to any Lua-registered handler. `current_mode`
    /// is the vim mode name (e.g. "Normal", "Insert", "Visual") or `None`
    /// when vim mode is disabled. A handler registered with mode `""` matches
    /// any mode; `"n"` matches Normal, `"i"` Insert, `"v"` Visual.
    pub fn run_keymap(&self, chord: &str, current_mode: Option<&str>) -> bool {
        let func = {
            let Ok(map) = self.shared.keymaps.lock() else {
                return false;
            };
            let mode_char = current_mode.map(|m| match m {
                "Normal" => "n",
                "Insert" => "i",
                "Visual" => "v",
                _ => "n",
            });
            let handle = mode_char
                .and_then(|mc| map.get(&(mc.to_string(), chord.to_string())))
                .or_else(|| map.get(&(String::new(), chord.to_string())));
            let Some(handle) = handle else {
                return false;
            };
            let Ok(f) = self.lua.registry_value::<mlua::Function>(&handle.key) else {
                return false;
            };
            f
        };
        if let Err(e) = func.call::<()>(()) {
            self.record_error(format!("keymap `{chord}`: {e}"));
        }
        true
    }

    /// Fire all handlers registered for `event` (simple — no data payload).
    /// Handlers receive `(event_name)`.
    pub fn emit(&self, event: AutocmdEvent) {
        let funcs = self.collect_handlers(&event);
        for func in funcs {
            if let Err(e) = func.call::<()>(event.lua_name()) {
                self.record_error(format!("autocmd `{}`: {e}", event.lua_name()));
            }
        }
    }

    /// Fire all handlers for `event` with a data table.
    /// Handlers receive `(event_name, data_table)`.
    /// `build_data` is called once to construct the table (only if handlers exist).
    pub fn emit_data<F>(&self, event: AutocmdEvent, build_data: F)
    where
        F: FnOnce(&Lua) -> LuaResult<mlua::Table>,
    {
        let funcs = self.collect_handlers(&event);
        if funcs.is_empty() {
            return;
        }
        let data = match build_data(&self.lua) {
            Ok(t) => t,
            Err(e) => {
                self.record_error(format!("autocmd `{}` data: {e}", event.lua_name()));
                return;
            }
        };
        for func in funcs {
            if let Err(e) = func.call::<()>((event.lua_name(), data.clone())) {
                self.record_error(format!("autocmd `{}`: {e}", event.lua_name()));
            }
        }
    }

    fn collect_handlers(&self, event: &AutocmdEvent) -> Vec<mlua::Function> {
        let Ok(map) = self.shared.autocmds.lock() else {
            return Vec::new();
        };
        let Some(list) = map.get(event) else {
            return Vec::new();
        };
        list.iter()
            .filter_map(|h| self.lua.registry_value::<mlua::Function>(&h.key).ok())
            .collect()
    }

    /// Access the underlying Lua state so callers can build result
    /// tables (e.g. for `resolve_dialog`).
    pub fn lua(&self) -> &Lua {
        &self.lua
    }

    /// Fire any `smelt.defer` timers whose deadline has passed.
    pub fn tick_timers(&self) {
        let now = Instant::now();
        let due: Vec<LuaHandle> = match self.shared.timers.lock() {
            Ok(mut q) => {
                let mut keep = Vec::with_capacity(q.len());
                let mut due = Vec::new();
                for (deadline, handle) in q.drain(..) {
                    if deadline > now {
                        keep.push((deadline, handle));
                    } else {
                        due.push(handle);
                    }
                }
                *q = keep;
                due
            }
            Err(_) => return,
        };
        for handle in due {
            let Ok(func) = self.lua.registry_value::<mlua::Function>(&handle.key) else {
                continue;
            };
            if let Err(e) = func.call::<()>(()) {
                self.record_error(format!("defer: {e}"));
            }
        }
    }

    /// Call every registered statusline source and return the combined
    /// item list (appended to Rust-side built-ins at the status-bar
    /// layer). Each source returns either a single item table or a list
    /// of items; empty-text items are skipped.
    pub fn tick_statusline(&self) -> Vec<crate::render::StatusItem> {
        let Ok(sources) = self.shared.statusline_sources.lock() else {
            return Vec::new();
        };
        let mut items = Vec::new();
        // (source_name, error_msg) pairs from this tick. Compared
        // against `statusline_last_errors` after the loop so we only
        // notify on transitions (new error, or message changed).
        let mut tick_errors: Vec<(String, Option<String>)> = Vec::new();
        for (name, source) in sources.iter() {
            let Ok(func) = self
                .lua
                .registry_value::<mlua::Function>(&source.handle.key)
            else {
                continue;
            };
            match func.call::<mlua::Value>(()) {
                Ok(mlua::Value::Nil) => {
                    tick_errors.push((name.clone(), None));
                }
                Ok(mlua::Value::Table(t)) => {
                    collect_statusline_items(&t, source.default_align_right, &mut items);
                    tick_errors.push((name.clone(), None));
                }
                Ok(_) => {
                    tick_errors.push((
                        name.clone(),
                        Some(format!("statusline[{name}]: expected table")),
                    ));
                }
                Err(e) => {
                    tick_errors.push((name.clone(), Some(format!("statusline[{name}]: {e}"))));
                }
            }
        }
        drop(sources);
        if let Ok(mut last) = self.shared.statusline_last_errors.lock() {
            for (name, msg) in tick_errors {
                match msg {
                    Some(new_msg) => {
                        let prev = last.get(&name);
                        if prev != Some(&new_msg) {
                            self.record_error(new_msg.clone());
                            last.insert(name, new_msg);
                        }
                    }
                    None => {
                        last.remove(&name);
                    }
                }
            }
        }
        items
    }

    /// Fire `smelt.confirm.open(handle_id)`. Called by the agent loop
    /// when an LLM tool call needs user approval — `agent.rs` registers
    /// the request in `App::confirm_requests` first, then this method
    /// hands the handle to the Lua dialog runner.
    pub fn fire_confirm_open(&self, handle_id: u64) {
        let result: mlua::Result<()> = (|| {
            let smelt: mlua::Table = self.lua.globals().get("smelt")?;
            let confirm: mlua::Table = smelt.get("confirm")?;
            let open: mlua::Function = confirm.get("open")?;
            open.call::<()>(handle_id)
        })();
        if let Err(e) = result {
            self.record_error(format!("smelt.confirm.open: {e}"));
        }
    }

    fn record_error(&self, msg: String) {
        // Route through `smelt.notify_error` so tests that override
        // it (`install_test_notify`) capture errors emitted by the
        // runtime itself, not just user `smelt.notify_error(...)`
        // calls. Production sees the same routing — Lua dispatches
        // through `with_app` to `App::notify_error`.
        if let Ok(smelt) = self.lua.globals().get::<mlua::Table>("smelt") {
            if let Ok(func) = smelt.get::<mlua::Function>("notify_error") {
                let _ = func.call::<()>(msg);
            }
        }
    }

    /// Whether a command with `name` is registered via Lua.
    pub fn has_command(&self, name: &str) -> bool {
        self.shared
            .commands
            .lock()
            .map(|m| m.contains_key(name))
            .unwrap_or(false)
    }

    /// Names of all Lua-registered commands (for completion).
    pub fn command_names(&self) -> Vec<String> {
        self.shared
            .commands
            .lock()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// `("/cmd", [arg, ...])` pairs for every Lua-registered command that
    /// declared an `args` list via `smelt.cmd.register("name", fn, {args = {...}})`.
    /// Drives the secondary `CommandArg` picker that opens after
    /// `/name <space>`.
    pub fn command_args(&self) -> Vec<(String, Vec<String>)> {
        list_command_args()
    }
}

fn ansi_color_from_lua(table: &mlua::Table, key: &str) -> Option<crossterm::style::Color> {
    let val: u8 = table.get(key).ok()?;
    Some(crossterm::style::Color::AnsiValue(val))
}

/// Parse a single-item or list-of-items Lua table into `StatusItem`s
/// and append them to `out`. Empty-text items are skipped.
fn collect_statusline_items(
    table: &mlua::Table,
    default_align_right: bool,
    out: &mut Vec<crate::render::StatusItem>,
) {
    let looks_like_item = table.contains_key("text").unwrap_or(false);
    if looks_like_item {
        if let Some(item) = statusline_item_from(table, default_align_right) {
            out.push(item);
        }
        return;
    }
    for pair in table.sequence_values::<mlua::Table>() {
        let Ok(entry) = pair else { continue };
        if let Some(item) = statusline_item_from(&entry, default_align_right) {
            out.push(item);
        }
    }
}

fn statusline_item_from(
    entry: &mlua::Table,
    default_align_right: bool,
) -> Option<crate::render::StatusItem> {
    let text: String = entry.get("text").ok()?;
    if text.is_empty() {
        return None;
    }
    // Per-item `align_right` wins over the source-level default; falls
    // back to the source's `align` opt when the item omits the field.
    let align_right = if entry.contains_key("align_right").unwrap_or(false) {
        entry.get("align_right").unwrap_or(default_align_right)
    } else {
        default_align_right
    };
    Some(crate::render::StatusItem {
        text,
        fg: ansi_color_from_lua(entry, "fg"),
        bg: ansi_color_from_lua(entry, "bg"),
        bold: entry.get("bold").unwrap_or(false),
        priority: entry.get("priority").unwrap_or(0),
        align_right,
        truncatable: entry.get("truncatable").unwrap_or(false),
        group: entry.get("group").unwrap_or(false),
    })
}

/// Convert a `serde_json::Value` to a `mlua::Value`.
pub fn json_to_lua(lua: &Lua, v: &serde_json::Value) -> LuaResult<mlua::Value> {
    match v {
        serde_json::Value::Null => Ok(mlua::Value::Nil),
        serde_json::Value::Bool(b) => Ok(mlua::Value::Boolean(*b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(mlua::Value::Integer(i))
            } else {
                Ok(mlua::Value::Number(n.as_f64().unwrap_or(0.0)))
            }
        }
        serde_json::Value::String(s) => Ok(mlua::Value::String(lua.create_string(s)?)),
        serde_json::Value::Array(arr) => {
            let t = lua.create_table()?;
            for (i, elem) in arr.iter().enumerate() {
                t.set(i + 1, json_to_lua(lua, elem)?)?;
            }
            Ok(mlua::Value::Table(t))
        }
        serde_json::Value::Object(map) => {
            let t = lua.create_table()?;
            for (k, val) in map {
                t.set(k.as_str(), json_to_lua(lua, val)?)?;
            }
            Ok(mlua::Value::Table(t))
        }
    }
}

impl Default for LuaRuntime {
    fn default() -> Self {
        Self::new()
    }
}

/// Bootstrap Lua chunks loaded at `register_api` time, after the
/// `smelt` global is fully populated but before any plugin or user
/// init.lua runs. Not `require`-able — they extend `smelt` directly
/// (e.g. `smelt.sleep`, the thick `smelt.ui.dialog.open` /
/// `smelt.ui.picker.open` wrappers around the Rust primitives).
const BOOTSTRAP_CHUNKS: &[(&str, &str)] = &[
    (
        "smelt/_bootstrap.lua",
        include_str!("../../../../runtime/lua/smelt/_bootstrap.lua"),
    ),
    (
        "smelt/dialog.lua",
        include_str!("../../../../runtime/lua/smelt/dialog.lua"),
    ),
    (
        "smelt/picker.lua",
        include_str!("../../../../runtime/lua/smelt/picker.lua"),
    ),
    (
        "smelt/prompt_picker.lua",
        include_str!("../../../../runtime/lua/smelt/prompt_picker.lua"),
    ),
    (
        "smelt/cmd.lua",
        include_str!("../../../../runtime/lua/smelt/cmd.lua"),
    ),
    (
        "smelt/confirm.lua",
        include_str!("../../../../runtime/lua/smelt/confirm.lua"),
    ),
];

/// Load all `BOOTSTRAP_CHUNKS` into the given Lua state. Called from
/// `register_api` once the `smelt` global is in place.
pub(super) fn load_bootstrap_chunks(lua: &Lua) -> mlua::Result<()> {
    for (name, src) in BOOTSTRAP_CHUNKS {
        lua.load(*src).set_name(*name).exec()?;
    }
    Ok(())
}

/// Modules embedded in the binary, available via `require("smelt.plugins.X")`.
/// Bootstrap primitives (`_bootstrap.lua`, `dialog.lua`, `picker.lua`) are
/// loaded via `load_bootstrap_chunks`, not here — they don't need to be
/// `require`-able.
const EMBEDDED_MODULES: &[(&str, &str)] = &[
    (
        "smelt.plugins.plan_mode",
        include_str!("../../../../runtime/lua/smelt/plugins/plan_mode.lua"),
    ),
    (
        "smelt.plugins.btw",
        include_str!("../../../../runtime/lua/smelt/plugins/btw.lua"),
    ),
    (
        "smelt.plugins.predict",
        include_str!("../../../../runtime/lua/smelt/plugins/predict.lua"),
    ),
    (
        "smelt.plugins.ask_user_question",
        include_str!("../../../../runtime/lua/smelt/plugins/ask_user_question.lua"),
    ),
    (
        "smelt.plugins.export",
        include_str!("../../../../runtime/lua/smelt/plugins/export.lua"),
    ),
    (
        "smelt.plugins.rewind",
        include_str!("../../../../runtime/lua/smelt/plugins/rewind.lua"),
    ),
    (
        "smelt.plugins.ps",
        include_str!("../../../../runtime/lua/smelt/plugins/ps.lua"),
    ),
    (
        "smelt.plugins.help",
        include_str!("../../../../runtime/lua/smelt/plugins/help.lua"),
    ),
    (
        "smelt.plugins.yank_block",
        include_str!("../../../../runtime/lua/smelt/plugins/yank_block.lua"),
    ),
    (
        "smelt.plugins.permissions",
        include_str!("../../../../runtime/lua/smelt/plugins/permissions.lua"),
    ),
    (
        "smelt.plugins.resume",
        include_str!("../../../../runtime/lua/smelt/plugins/resume.lua"),
    ),
    (
        "smelt.plugins.agents",
        include_str!("../../../../runtime/lua/smelt/plugins/agents.lua"),
    ),
    (
        "smelt.plugins.theme",
        include_str!("../../../../runtime/lua/smelt/plugins/theme.lua"),
    ),
    (
        "smelt.plugins.color",
        include_str!("../../../../runtime/lua/smelt/plugins/color.lua"),
    ),
    (
        "smelt.plugins.model",
        include_str!("../../../../runtime/lua/smelt/plugins/model.lua"),
    ),
    (
        "smelt.plugins.settings",
        include_str!("../../../../runtime/lua/smelt/plugins/settings.lua"),
    ),
    (
        "smelt.plugins.history_search",
        include_str!("../../../../runtime/lua/smelt/plugins/history_search.lua"),
    ),
];

/// Plugins that must always be active (the user can't opt out via
/// init.lua). These are former Rust built-ins migrated to Lua. Required
/// after the embedded searcher is set up, before user init.lua runs.
const AUTOLOAD_MODULES: &[&str] = &[
    "smelt.plugins.ask_user_question",
    "smelt.plugins.btw",
    "smelt.plugins.export",
    "smelt.plugins.rewind",
    "smelt.plugins.ps",
    "smelt.plugins.help",
    "smelt.plugins.permissions",
    "smelt.plugins.resume",
    "smelt.plugins.agents",
    "smelt.plugins.theme",
    "smelt.plugins.color",
    "smelt.plugins.model",
    "smelt.plugins.settings",
    "smelt.plugins.history_search",
];

/// Register a custom Lua package searcher that resolves `require("smelt.…")`
/// from modules embedded in the binary. Falls back to the default searchers
/// for anything not in `EMBEDDED_MODULES`, so user files on disk win when
/// they shadow an embedded module (the user searcher runs first).
fn register_embedded_searcher(lua: &Lua) -> LuaResult<()> {
    let searcher = lua.create_function(|lua, module: String| {
        for &(name, source) in EMBEDDED_MODULES {
            if name == module {
                let loader = lua.load(source).set_name(name).into_function()?;
                return Ok(mlua::Value::Function(loader));
            }
        }
        Ok(mlua::Value::String(lua.create_string(format!(
            "\n\tno embedded module '{module}'"
        ))?))
    })?;

    let package: mlua::Table = lua.globals().get("package")?;
    let searchers: mlua::Table = package.get("searchers")?;
    let len = searchers.raw_len();
    // Insert at the end — filesystem searchers run first, so user overrides win.
    searchers.raw_set(len + 1, searcher)?;
    Ok(())
}

fn init_lua_path() -> Option<PathBuf> {
    // Honour XDG_CONFIG_HOME, falling back to ~/.config.
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))?;
    Some(base.join("smelt").join("init.lua"))
}

#[cfg(test)]
mod tests {
    use super::api::lua_table_to_json;
    use super::*;

    /// Install a Lua-level `smelt.notify` / `smelt.notify_error` stub
    /// that pushes into `_G.test_log` instead of routing through `App`
    /// (no App exists in unit tests). Tests that observe handler
    /// behaviour through these calls should call this once at the
    /// start, then read [`drain_notifications`] / [`drain_errors`].
    fn install_test_notify(rt: &LuaRuntime) {
        rt.lua
            .load(
                r#"
                    _G.test_log = {}
                    _G.test_err = {}
                    smelt.notify = function(msg) table.insert(_G.test_log, msg) end
                    smelt.notify_error = function(msg) table.insert(_G.test_err, msg) end
                "#,
            )
            .exec()
            .expect("install_test_notify");
    }

    fn drain_notifications(rt: &LuaRuntime) -> Vec<String> {
        let log: mlua::Table = match rt.lua.globals().get("test_log") {
            Ok(t) => t,
            Err(_) => return Vec::new(),
        };
        let out: Vec<String> = log
            .sequence_values::<String>()
            .filter_map(|r| r.ok())
            .collect();
        let _ = rt
            .lua
            .globals()
            .set("test_log", rt.lua.create_table().unwrap());
        out
    }

    fn drain_errors(rt: &LuaRuntime) -> Vec<String> {
        let log: mlua::Table = match rt.lua.globals().get("test_err") {
            Ok(t) => t,
            Err(_) => return Vec::new(),
        };
        let out: Vec<String> = log
            .sequence_values::<String>()
            .filter_map(|r| r.ok())
            .collect();
        let _ = rt
            .lua
            .globals()
            .set("test_err", rt.lua.create_table().unwrap());
        out
    }

    #[test]
    fn invoke_callback_runs_registered_fn_with_selection_payload() {
        let rt = LuaRuntime::new();
        rt.lua
            .load(
                r#"
                _G.recorded = nil
                _G.test_cb = function(ctx) _G.recorded = ctx.index end
            "#,
            )
            .exec()
            .unwrap();
        let func: mlua::Function = rt.lua.load("test_cb").eval().unwrap();
        let id = rt.register_callback(func).unwrap();
        rt.invoke_callback(
            ui::LuaHandle(id),
            ui::WinId(0),
            &ui::Payload::Selection { index: 2 },
            &[],
        );
        let recorded: u64 = rt.lua.load("return _G.recorded").eval().unwrap();
        // Payload is 0-indexed; Lua gets 1-based.
        assert_eq!(recorded, 3);
    }

    #[test]
    fn invoke_callback_text_payload() {
        let rt = LuaRuntime::new();
        rt.lua
            .load(
                r#"
                _G.t = nil
                _G.cb = function(ctx) _G.t = ctx.text end
            "#,
            )
            .exec()
            .unwrap();
        let func: mlua::Function = rt.lua.load("cb").eval().unwrap();
        let id = rt.register_callback(func).unwrap();
        rt.invoke_callback(
            ui::LuaHandle(id),
            ui::WinId(0),
            &ui::Payload::Text {
                content: "hi".into(),
            },
            &[],
        );
        let t: String = rt.lua.load("return _G.t").eval().unwrap();
        assert_eq!(t, "hi");
    }

    #[test]
    fn invoke_callback_unknown_handle_is_noop() {
        let rt = LuaRuntime::new();
        // Nothing registered under id 9999 — should silently succeed.
        rt.invoke_callback(ui::LuaHandle(9999), ui::WinId(0), &ui::Payload::None, &[]);
    }

    /// Regression: every code path that drops a Lua-backed callback
    /// (window close, displaced keymap binding) must funnel through
    /// `remove_callback` to evict the entry from `shared.callbacks`,
    /// otherwise the registry grows unbounded over a long session. Tests
    /// the floor invariant: register inserts, remove evicts, and a
    /// removed handle is a no-op when invoked.
    #[test]
    fn remove_callback_evicts_handle_from_registry() {
        let rt = LuaRuntime::new();
        rt.lua
            .load(
                r#"
                _G.fired = 0
                _G.cb = function() _G.fired = _G.fired + 1 end
            "#,
            )
            .exec()
            .unwrap();
        let func: mlua::Function = rt.lua.load("cb").eval().unwrap();
        let id = rt.register_callback(func).unwrap();
        assert_eq!(rt.shared.callbacks.lock().unwrap().len(), 1);

        rt.remove_callback(id);
        assert!(rt.shared.callbacks.lock().unwrap().is_empty());

        // Invoking the dropped handle must not resurrect the call.
        rt.invoke_callback(ui::LuaHandle(id), ui::WinId(0), &ui::Payload::None, &[]);
        let fired: u64 = rt.lua.load("return _G.fired").eval().unwrap();
        assert_eq!(fired, 0);
    }

    #[test]
    fn parse_win_event_covers_common_names() {
        assert!(matches!(
            parse_win_event("submit"),
            Some(ui::WinEvent::Submit)
        ));
        assert!(matches!(
            parse_win_event("text_changed"),
            Some(ui::WinEvent::TextChanged)
        ));
        assert!(matches!(
            parse_win_event("change"),
            Some(ui::WinEvent::TextChanged)
        ));
        assert!(matches!(
            parse_win_event("dismiss"),
            Some(ui::WinEvent::Dismiss)
        ));
        assert!(matches!(parse_win_event("tick"), Some(ui::WinEvent::Tick)));
        assert!(matches!(
            parse_win_event("focus"),
            Some(ui::WinEvent::FocusGained)
        ));
        assert!(parse_win_event("bogus").is_none());
    }

    #[test]
    fn invoke_callback_exposes_panels_snapshot() {
        let rt = LuaRuntime::new();
        rt.lua
            .load(
                r#"
                _G.sel = nil
                _G.txt = nil
                _G.win = nil
                _G.cb  = function(ctx)
                    _G.win = ctx.win
                    _G.sel = ctx.panels[2].selected
                    _G.txt = ctx.panels[3].text
                end
            "#,
            )
            .exec()
            .unwrap();
        let func: mlua::Function = rt.lua.load("cb").eval().unwrap();
        let id = rt.register_callback(func).unwrap();
        let panels = vec![
            ui::PanelSnapshot {
                kind: ui::PanelKind::Content,
                selected: None,
                text: String::new(),
            },
            ui::PanelSnapshot {
                kind: ui::PanelKind::List { multi: false },
                selected: Some(3),
                text: String::new(),
            },
            ui::PanelSnapshot {
                kind: ui::PanelKind::Content,
                selected: None,
                text: "hello".into(),
            },
        ];
        rt.invoke_callback(
            ui::LuaHandle(id),
            ui::WinId(42),
            &ui::Payload::None,
            &panels,
        );
        let win: u64 = rt.lua.load("return _G.win").eval().unwrap();
        let sel: u64 = rt.lua.load("return _G.sel").eval().unwrap();
        let txt: String = rt.lua.load("return _G.txt").eval().unwrap();
        assert_eq!(win, 42);
        assert_eq!(sel, 4); // 1-based
        assert_eq!(txt, "hello");
    }

    #[test]
    fn theme_accent_round_trip() {
        let rt = LuaRuntime::new();
        assert!(rt.load_error.is_none(), "load_error: {:?}", rt.load_error);
        let old_accent = crate::theme::accent_value();
        rt.lua
            .load("smelt.theme.set('accent', { ansi = 42 })")
            .exec()
            .unwrap();
        let ansi: u8 = rt
            .lua
            .load("return smelt.theme.accent().ansi")
            .eval()
            .unwrap();
        assert_eq!(ansi, 42);
        crate::theme::set_accent(old_accent);
    }

    #[test]
    fn theme_preset_sets_accent() {
        let rt = LuaRuntime::new();
        let old_accent = crate::theme::accent_value();
        rt.lua
            .load("smelt.theme.set('accent', { preset = 'sage' })")
            .exec()
            .unwrap();
        let ansi: u8 = rt
            .lua
            .load("return smelt.theme.accent().ansi")
            .eval()
            .unwrap();
        assert_eq!(ansi, 108); // sage
        crate::theme::set_accent(old_accent);
    }

    #[test]
    fn theme_snapshot_lists_all_roles() {
        let rt = LuaRuntime::new();
        let names: Vec<String> = rt
            .lua
            .load(
                r#"
                local snap = smelt.theme.snapshot()
                local t = {}
                for k, _ in pairs(snap) do t[#t+1] = k end
                table.sort(t)
                return t
                "#,
            )
            .eval::<mlua::Table>()
            .unwrap()
            .sequence_values::<String>()
            .filter_map(|r| r.ok())
            .collect();
        for expected in [
            "accent",
            "bar",
            "code_block_bg",
            "muted",
            "reason_off",
            "slug",
            "tool_pending",
            "user_bg",
        ] {
            assert!(
                names.iter().any(|n| n == expected),
                "snapshot missing {expected}: {names:?}"
            );
        }
    }

    #[test]
    fn theme_unknown_role_is_error() {
        let rt = LuaRuntime::new();
        let err = rt.lua.load("smelt.theme.get('bogus')").exec().unwrap_err();
        assert!(err.to_string().contains("unknown theme role"));
    }

    #[test]
    fn theme_read_only_role_set_fails() {
        let rt = LuaRuntime::new();
        let err = rt
            .lua
            .load("smelt.theme.set('muted', { ansi = 1 })")
            .exec()
            .unwrap_err();
        assert!(err.to_string().contains("read-only"));
    }

    #[test]
    fn runtime_exposes_api_version() {
        let rt = LuaRuntime::new();
        assert!(rt.load_error.is_none(), "load_error: {:?}", rt.load_error);
        let version: String = rt.lua.load("return smelt.version").eval().expect("eval");
        assert_eq!(version, crate::api::VERSION);
    }

    #[test]
    fn lua_sequence_tables_serialize_as_json_arrays() {
        let lua = Lua::new();
        let tbl: mlua::Table = lua
            .load(r#"return { "label", "description" }"#)
            .eval()
            .expect("eval");
        let json = lua_table_to_json(&lua, &tbl);
        assert_eq!(
            json,
            serde_json::json!(["label", "description"]),
            "1..N integer keys must become JSON array"
        );

        let obj: mlua::Table = lua
            .load(r#"return { type = "object", properties = {} }"#)
            .eval()
            .expect("eval");
        let json2 = lua_table_to_json(&lua, &obj);
        assert_eq!(json2["type"], serde_json::json!("object"));
    }

    #[test]
    fn autoload_registers_export_command() {
        let mut rt = LuaRuntime::new();
        rt.load_plugins();
        assert!(rt.load_error.is_none(), "load_error: {:?}", rt.load_error);
        assert!(
            rt.has_command("export"),
            "/export should be registered by the autoloaded plugin"
        );
    }

    #[test]
    fn autoload_registers_ps_command() {
        let mut rt = LuaRuntime::new();
        rt.load_plugins();
        assert!(rt.load_error.is_none(), "load_error: {:?}", rt.load_error);
        assert!(
            rt.has_command("ps"),
            "/ps should be registered by the autoloaded plugin"
        );
    }

    #[test]
    fn autoload_registers_rewind_command() {
        let mut rt = LuaRuntime::new();
        rt.load_plugins();
        assert!(rt.load_error.is_none(), "load_error: {:?}", rt.load_error);
        assert!(
            rt.has_command("rewind"),
            "/rewind should be registered by the autoloaded plugin"
        );
    }

    #[test]
    fn autoload_registers_ask_user_question_as_sequential() {
        let mut rt = LuaRuntime::new();
        rt.load_plugins();
        assert!(rt.load_error.is_none(), "load_error: {:?}", rt.load_error);
        let defs = rt.plugin_tool_defs(protocol::Mode::Normal);
        let ask = defs
            .iter()
            .find(|d| d.name == "ask_user_question")
            .expect("ask_user_question should be auto-registered");
        assert_eq!(ask.execution_mode, protocol::ToolExecutionMode::Sequential);
    }

    #[test]
    fn dialog_open_outside_task_errors() {
        // Calling `smelt.ui.dialog.open` outside a yieldable coroutine
        // (the runtime file's first guard) must raise. With plugins
        // loaded the Lua wrapper is in place; `isyieldable()` is false
        // at the top level, so the call errors before reaching the
        // Rust `_open` binding.
        let mut rt = LuaRuntime::new();
        rt.load_plugins();
        assert!(rt.load_error.is_none(), "load_error: {:?}", rt.load_error);
        let res: LuaResult<()> = rt.lua.load("smelt.ui.dialog.open({panels = {}})").exec();
        assert!(res.is_err());
    }

    #[test]
    fn plugin_tool_runs_as_task_immediate() {
        let rt = LuaRuntime::new();
        rt.lua
            .load(
                r#"
                smelt.tools.register({
                  name = "echo",
                  description = "",
                  parameters = { type = "object", properties = {} },
                  execute = function(args) return "hi " .. (args.who or "?") end,
                })
                "#,
            )
            .exec()
            .unwrap();
        let mut args = std::collections::HashMap::new();
        args.insert("who".into(), serde_json::json!("world"));
        match rt.execute_plugin_tool("echo", &args, 1, "c1") {
            ToolExecResult::Immediate { content, is_error } => {
                assert_eq!(content, "hi world");
                assert!(!is_error);
            }
            ToolExecResult::Pending => panic!("expected immediate"),
        }
    }

    #[test]
    fn plugin_tool_yield_returns_pending_then_tool_complete() {
        let rt = LuaRuntime::new();
        rt.lua
            .load(
                r#"
                smelt.tools.register({
                  name = "wait_then_yes",
                  description = "",
                  parameters = { type = "object", properties = {} },
                  execute = function()
                    smelt.sleep(0)
                    return "yes"
                  end,
                })
                "#,
            )
            .exec()
            .unwrap();
        let args = std::collections::HashMap::new();
        match rt.execute_plugin_tool("wait_then_yes", &args, 7, "c9") {
            ToolExecResult::Pending => {}
            ToolExecResult::Immediate { .. } => panic!("expected pending after yield"),
        }
        // Drive again — the sleep(0) is elapsed, so the task resumes and completes.
        let outs = rt.drive_tasks();
        let complete = outs
            .iter()
            .find(|o| matches!(o, TaskDriveOutput::ToolComplete { .. }))
            .expect("expected ToolComplete");
        match complete {
            TaskDriveOutput::ToolComplete {
                request_id,
                call_id,
                content,
                is_error,
            } => {
                assert_eq!(*request_id, 7);
                assert_eq!(call_id, "c9");
                assert_eq!(content, "yes");
                assert!(!*is_error);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn notify_queues_for_drain() {
        let rt = LuaRuntime::new();
        install_test_notify(&rt);
        rt.lua
            .load("smelt.notify('hello from lua')")
            .exec()
            .expect("exec");
        let msgs = drain_notifications(&rt);
        assert_eq!(msgs, vec!["hello from lua".to_string()]);
        assert!(drain_notifications(&rt).is_empty());
    }

    #[test]
    fn syntax_error_captured_not_panicked() {
        let mut rt = LuaRuntime::new();
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        std::fs::write(tmp.path(), "this is not valid lua @@@").unwrap();
        let err = rt.load_init(tmp.path());
        assert!(err.is_err(), "expected syntax error");
    }

    #[test]
    fn cmd_register_and_run() {
        let rt = LuaRuntime::new();
        install_test_notify(&rt);
        rt.lua
            .load(
                r#"
                    smelt.cmd.register("hello", function()
                        smelt.notify("hello world")
                    end)
                "#,
            )
            .exec()
            .expect("exec");
        assert!(rt.has_command("hello"));
        assert!(rt.run_command("hello", None));
        assert_eq!(drain_notifications(&rt), vec!["hello world".to_string()]);
        assert!(!rt.run_command("unknown", None));
    }

    #[test]
    fn keymap_register_and_run() {
        let rt = LuaRuntime::new();
        install_test_notify(&rt);
        rt.lua
            .load(
                r#"
                    smelt.keymap.set("n", "<C-g>", function()
                        smelt.notify("ctrl-g")
                    end)
                "#,
            )
            .exec()
            .expect("exec");
        assert!(rt.run_keymap("<C-g>", Some("Normal")));
        assert_eq!(drain_notifications(&rt), vec!["ctrl-g".to_string()]);
        assert!(!rt.run_keymap("<C-g>", Some("Insert")));
        assert!(!rt.run_keymap("<C-x>", Some("Normal")));
    }

    #[test]
    fn keymap_wildcard_mode() {
        let rt = LuaRuntime::new();
        install_test_notify(&rt);
        rt.lua
            .load(
                r#"
                    smelt.keymap.set("", "<C-h>", function()
                        smelt.notify("any-mode")
                    end)
                "#,
            )
            .exec()
            .expect("exec");
        assert!(rt.run_keymap("<C-h>", Some("Normal")));
        assert_eq!(drain_notifications(&rt), vec!["any-mode".to_string()]);
        assert!(rt.run_keymap("<C-h>", Some("Insert")));
        assert!(rt.run_keymap("<C-h>", None));
    }

    #[test]
    fn autocmd_emit_fires_handlers() {
        let rt = LuaRuntime::new();
        install_test_notify(&rt);
        rt.lua
            .load(
                r#"
                    smelt.on("block_done", function(event)
                        smelt.notify("fired: " .. event)
                    end)
                "#,
            )
            .exec()
            .expect("exec");
        rt.emit(AutocmdEvent::BlockDone);
        assert_eq!(
            drain_notifications(&rt),
            vec!["fired: block_done".to_string()]
        );
    }

    #[test]
    fn defer_timer_fires_after_deadline() {
        let rt = LuaRuntime::new();
        install_test_notify(&rt);
        rt.lua
            .load(
                r#"
                    smelt.defer(0, function()
                        smelt.notify("deferred")
                    end)
                "#,
            )
            .exec()
            .expect("exec");
        assert!(drain_notifications(&rt).is_empty());
        std::thread::sleep(std::time::Duration::from_millis(2));
        rt.tick_timers();
        assert_eq!(drain_notifications(&rt), vec!["deferred".to_string()]);
    }

    #[test]
    fn chord_string_formats_nvim_style() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers as M};
        let ev = |code, mods| KeyEvent::new(code, mods);
        assert_eq!(
            chord_string(ev(KeyCode::Char('j'), M::NONE)).as_deref(),
            Some("j")
        );
        assert_eq!(
            chord_string(ev(KeyCode::Char('g'), M::CONTROL)).as_deref(),
            Some("<C-g>")
        );
        assert_eq!(
            chord_string(ev(KeyCode::Tab, M::SHIFT)).as_deref(),
            Some("<S-Tab>")
        );
        assert_eq!(
            chord_string(ev(KeyCode::Esc, M::NONE)).as_deref(),
            Some("<Esc>")
        );
        assert_eq!(
            chord_string(ev(KeyCode::Char('x'), M::ALT)).as_deref(),
            Some("<M-x>")
        );
    }

    #[test]
    fn parse_keybind_handles_names_and_modifiers() {
        use crossterm::event::{KeyCode, KeyModifiers};
        assert_eq!(
            parse_keybind("enter"),
            Some(ui::KeyBind::new(KeyCode::Enter, KeyModifiers::NONE))
        );
        assert_eq!(
            parse_keybind("esc"),
            Some(ui::KeyBind::new(KeyCode::Esc, KeyModifiers::NONE))
        );
        assert_eq!(
            parse_keybind("c-j"),
            Some(ui::KeyBind::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        );
        assert_eq!(
            parse_keybind("a-x"),
            Some(ui::KeyBind::new(KeyCode::Char('x'), KeyModifiers::ALT))
        );
        // shift-tab collapses to BackTab without the SHIFT bit so
        // crossterm's event matches lookups done elsewhere.
        assert_eq!(
            parse_keybind("s-tab"),
            Some(ui::KeyBind::new(KeyCode::BackTab, KeyModifiers::NONE))
        );
        assert_eq!(
            parse_keybind("k"),
            Some(ui::KeyBind::new(KeyCode::Char('k'), KeyModifiers::NONE))
        );
        assert_eq!(parse_keybind("bogus"), None);
        assert_eq!(parse_keybind("ctrl-nope"), None);
        assert_eq!(parse_keybind(""), None);
    }

    #[test]
    fn normalize_mode_accepts_long_and_short_names() {
        assert_eq!(normalize_mode("n").as_deref(), Some("n"));
        assert_eq!(normalize_mode("normal").as_deref(), Some("n"));
        assert_eq!(normalize_mode("Normal").as_deref(), Some("n"));
        assert_eq!(normalize_mode("INSERT").as_deref(), Some("i"));
        assert_eq!(normalize_mode("visual").as_deref(), Some("v"));
        assert_eq!(normalize_mode("").as_deref(), Some(""));
        assert_eq!(normalize_mode("*").as_deref(), Some(""));
        assert_eq!(normalize_mode("any").as_deref(), Some(""));
        assert_eq!(normalize_mode("bogus"), None);
    }

    #[test]
    fn canonicalize_chord_folds_all_supported_forms() {
        assert_eq!(canonicalize_chord("c-r").as_deref(), Some("<C-r>"));
        assert_eq!(canonicalize_chord("C-r").as_deref(), Some("<C-r>"));
        assert_eq!(canonicalize_chord("<C-r>").as_deref(), Some("<C-r>"));
        assert_eq!(canonicalize_chord("<c-r>").as_deref(), Some("<C-r>"));
        assert_eq!(canonicalize_chord("enter").as_deref(), Some("<CR>"));
        assert_eq!(canonicalize_chord("<Enter>").as_deref(), Some("<CR>"));
        assert_eq!(canonicalize_chord("esc").as_deref(), Some("<Esc>"));
        assert_eq!(canonicalize_chord("s-tab").as_deref(), Some("<Tab>"));
        assert_eq!(canonicalize_chord("j").as_deref(), Some("j"));
        assert_eq!(canonicalize_chord("bogus"), None);
    }

    #[test]
    fn keymap_accepts_plugin_friendly_spellings() {
        // The Ctrl-R class of bug: history_search.lua registers
        // `"normal" + "c-r"` but dispatch uses `"n" + "<C-r>"`.
        // Canonicalization at registration closes the gap.
        let rt = LuaRuntime::new();
        install_test_notify(&rt);
        rt.lua
            .load(
                r#"
                    for _, mode in ipairs({ "normal", "insert", "visual" }) do
                        smelt.keymap.set(mode, "c-r", function()
                            smelt.notify("history: " .. mode)
                        end)
                    end
                "#,
            )
            .exec()
            .expect("exec");
        assert!(rt.run_keymap("<C-r>", Some("Normal")));
        assert!(rt.run_keymap("<C-r>", Some("Insert")));
        assert!(rt.run_keymap("<C-r>", Some("Visual")));
        let msgs = drain_notifications(&rt);
        assert_eq!(msgs.len(), 3);
    }

    #[test]
    fn keymap_set_errors_on_bad_input() {
        let rt = LuaRuntime::new();
        let err = rt
            .lua
            .load(r#"smelt.keymap.set("bogus", "c-r", function() end)"#)
            .exec()
            .expect_err("should error on unknown mode");
        assert!(format!("{err}").contains("unknown mode"), "err: {err}");
        let err = rt
            .lua
            .load(r#"smelt.keymap.set("n", "c-wtf", function() end)"#)
            .exec()
            .expect_err("should error on unknown chord");
        assert!(format!("{err}").contains("unknown chord"), "err: {err}");
    }

    #[test]
    fn callback_error_surfaces_without_panic() {
        let rt = LuaRuntime::new();
        install_test_notify(&rt);
        rt.lua
            .load(
                r#"
                    smelt.cmd.register("broken", function()
                        error("kaboom")
                    end)
                "#,
            )
            .exec()
            .expect("exec");
        assert!(rt.run_command("broken", None));
        let errs = drain_errors(&rt);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("broken"), "err: {}", errs[0]);
    }

    #[test]
    fn transcript_text_reads_context() {
        let rt = LuaRuntime::new();
        rt.set_context(Some("hello world".into()), None, None, None);
        let text: String = rt
            .lua
            .load("return smelt.transcript.text()")
            .eval()
            .expect("eval");
        assert_eq!(text, "hello world");
        rt.clear_context();
        let text: String = rt
            .lua
            .load("return smelt.transcript.text()")
            .eval()
            .expect("eval");
        assert_eq!(text, "");
    }

    #[test]
    fn buf_text_reads_context() {
        let rt = LuaRuntime::new();
        rt.set_context(None, Some("prompt content".into()), None, None);
        let text: String = rt.lua.load("return smelt.buf.text()").eval().expect("eval");
        assert_eq!(text, "prompt content");
    }

    #[test]
    fn engine_model_reads_snapshot() {
        let rt = LuaRuntime::new();
        rt.set_engine_context(EngineSnapshot {
            model: "claude-opus-4".into(),
            ..Default::default()
        });
        let model: String = rt
            .lua
            .load("return smelt.engine.model()")
            .eval()
            .expect("eval");
        assert_eq!(model, "claude-opus-4");
    }

    #[test]
    fn engine_mode_reads_snapshot() {
        let rt = LuaRuntime::new();
        rt.set_engine_context(EngineSnapshot {
            mode: "plan".into(),
            ..Default::default()
        });
        let mode: String = rt
            .lua
            .load("return smelt.engine.mode()")
            .eval()
            .expect("eval");
        assert_eq!(mode, "plan");
    }

    #[test]
    fn engine_is_busy_reads_snapshot() {
        let rt = LuaRuntime::new();
        rt.set_engine_context(EngineSnapshot {
            is_busy: true,
            ..Default::default()
        });
        let busy: bool = rt
            .lua
            .load("return smelt.engine.is_busy()")
            .eval()
            .expect("eval");
        assert!(busy);
    }

    #[test]
    fn engine_cost_reads_snapshot() {
        let rt = LuaRuntime::new();
        rt.set_engine_context(EngineSnapshot {
            session_cost: 1.23,
            ..Default::default()
        });
        let cost: f64 = rt
            .lua
            .load("return smelt.engine.cost()")
            .eval()
            .expect("eval");
        assert!((cost - 1.23).abs() < 0.001);
    }

    #[test]
    fn engine_context_tokens_reads_snapshot() {
        let rt = LuaRuntime::new();
        rt.set_engine_context(EngineSnapshot {
            context_tokens: Some(5000),
            context_window: Some(128000),
            ..Default::default()
        });
        let tokens: u32 = rt
            .lua
            .load("return smelt.engine.context_tokens()")
            .eval()
            .expect("eval");
        assert_eq!(tokens, 5000);
        let window: u32 = rt
            .lua
            .load("return smelt.engine.context_window()")
            .eval()
            .expect("eval");
        assert_eq!(window, 128000);
    }

    #[test]
    fn emit_data_passes_table_to_handler() {
        let rt = LuaRuntime::new();
        install_test_notify(&rt);
        rt.lua
            .load(
                r#"
                    smelt.on("mode_change", function(event, data)
                        smelt.notify(event .. ":" .. data.from .. "->" .. data.to)
                    end)
                "#,
            )
            .exec()
            .expect("exec");
        rt.emit_data(AutocmdEvent::ModeChange, |lua| {
            let t = lua.create_table()?;
            t.set("from", "normal")?;
            t.set("to", "plan")?;
            Ok(t)
        });
        assert_eq!(
            drain_notifications(&rt),
            vec!["mode_change:normal->plan".to_string()]
        );
    }

    #[test]
    fn legacy_stream_start_maps_to_turn_start() {
        let rt = LuaRuntime::new();
        install_test_notify(&rt);
        rt.lua
            .load(
                r#"
                    smelt.on("stream_start", function(event)
                        smelt.notify("got: " .. event)
                    end)
                "#,
            )
            .exec()
            .expect("exec");
        rt.emit(AutocmdEvent::TurnStart);
        assert_eq!(
            drain_notifications(&rt),
            vec!["got: turn_start".to_string()]
        );
    }

    #[test]
    fn reasoning_effort_reads_snapshot() {
        let rt = LuaRuntime::new();
        rt.set_engine_context(EngineSnapshot {
            reasoning_effort: "high".into(),
            ..Default::default()
        });
        let effort: String = rt
            .lua
            .load("return smelt.engine.reasoning_effort()")
            .eval()
            .expect("eval");
        assert_eq!(effort, "high");
    }
}
