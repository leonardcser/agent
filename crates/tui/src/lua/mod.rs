//! Lua bindings (Phase D). Wraps the `api::*` surface so users can
//! script smelt from `~/.config/smelt/init.lua`.
//!
//! Current scope:
//! - **D1 bootstrap** — loads `~/.config/smelt/init.lua` at startup
//!   (honouring `XDG_CONFIG_HOME`). Missing files are not errors.
//! - **D2 api shim** — `smelt.api.version`, `smelt.api.cmd.register`,
//!   `smelt.keymap`, `smelt.on` all accept Lua callables and store
//!   them in per-category registries that the app polls on the tick.
//! - **D3 autocmd dispatch** — `AutocmdRegistry` + `emit_autocmd` run
//!   handlers synchronously; errors are logged and the next handler
//!   runs (handler-dead tracking defers to D6).
//! - **D4 user-command + keymap registration** — registration stores
//!   `LuaRef` handles keyed by `(mode, chord)`; mode `"n"` matches
//!   Normal, `"i"` Insert, `"v"` Visual, `""` matches any mode.
//! - **D5 re-entrancy** — pending ops queue defers state mutations
//!   until after the dispatching handler returns. `smelt.defer(ms, fn)`
//!   posts to `pending_timers`; the tick loop fires them when due.
//! - **D6 error UX** — every callable is wrapped in `try_call`;
//!   errors append to `lua_errors` and the app surfaces the first as a
//!   notification on the next tick.

mod task;

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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Process-global snapshot of Lua-registered `/command` names and their
/// optional descriptions. Written by `smelt.api.cmd.register`, read by
/// `Completer::commands` / `Completer::is_command` — same free-function
/// pattern as `custom_commands::list` / `builtin_commands::list`.
///
/// We keep a separate string-only snapshot (instead of exposing
/// `LuaShared` directly) because `LuaHandle` contains `!Send`
/// `mlua::RegistryKey` and cannot live in a static, and because the
/// completer only needs labels + descriptions.
fn lua_commands_snapshot() -> &'static Mutex<HashMap<String, Option<String>>> {
    static S: OnceLock<Mutex<HashMap<String, Option<String>>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

/// List all Lua-registered `/commands` as `(name, description)`.
/// Sorted by name. Used by the `/` completer.
pub fn list_commands() -> Vec<(String, Option<String>)> {
    let mut items: Vec<(String, Option<String>)> = lua_commands_snapshot()
        .lock()
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
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
/// handlers. This is the lookup key for `smelt.keymap(_, chord, fn)`.
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
    dead: bool,
}

pub use crate::app::ops::{AppOp, OpsHandle};

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
}

/// Shared state between Lua closures and the app loop.
///
/// **Reads**: snapshot fields populated by `set_context()` before a
/// handler runs. Lua reads these via `smelt.api.transcript.text()` etc.
///
/// **Writes**: `ops` collects deferred mutations (`AppOp`) that the
/// app drains and applies after the handler returns.
///
/// One `Arc<Mutex<LuaOps>>` replaces the old separate
/// `LuaContext` + `pending_notifications` + `pending_commands` +
/// `lua_errors`.
#[derive(Default)]
pub struct LuaOps {
    // ── reads (snapshot) — UI state ─────────────────────────────────
    pub transcript_text: Option<String>,
    pub prompt_text: Option<String>,
    pub focused_window: Option<String>,
    pub vim_mode: Option<String>,
    // ── reads (snapshot) — engine state ─────────────────────────────
    pub engine: EngineSnapshot,
    // ── writes (queued mutations) ───────────────────────────────────
    pub ops: Vec<AppOp>,
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

    pub fn drain(&mut self) -> Vec<AppOp> {
        std::mem::take(&mut self.ops)
    }
}

/// All shared state between Lua closures and the app loop.
/// One `Arc<LuaShared>` replaces N separate `Arc<Mutex<…>>` fields.
pub(crate) struct LuaShared {
    pub(crate) ops: Mutex<LuaOps>,
    pub(crate) commands: Mutex<HashMap<String, LuaHandle>>,
    pub(crate) keymaps: Mutex<HashMap<(String, String), LuaHandle>>,
    pub(crate) autocmds: Mutex<HashMap<AutocmdEvent, Vec<LuaHandle>>>,
    pub(crate) timers: Mutex<Vec<(Instant, LuaHandle)>>,
    pub(crate) statusline: Mutex<Option<LuaHandle>>,
    pub(crate) plugin_tools: Mutex<HashMap<String, LuaHandle>>,
    pub(crate) callbacks: Mutex<HashMap<u64, LuaHandle>>,
    pub(crate) next_id: AtomicU64,
    pub(crate) history: Mutex<Arc<Vec<protocol::Message>>>,
    pub(crate) tasks: Mutex<LuaTaskRuntime>,
    /// Background process registry. Installed by `App::new` so Lua
    /// plugins (e.g. `/ps`) can enumerate and kill procs.
    pub(crate) processes: Mutex<Option<engine::tools::ProcessRegistry>>,
    /// Task-runtime inbox. Dialog callbacks / other UI events that need
    /// to *resume a Lua coroutine* push here instead of through `ops`.
    /// Keeps the reducer's `AppOp` enum free of Lua-task variants; the
    /// Lua module pumps its own inbox each tick.
    pub(crate) task_inbox: Mutex<Vec<TaskEvent>>,
}

/// Events that drive the Lua task runtime — dialog resolutions,
/// keymap-fired callbacks, anything that resumes or invokes a Lua
/// coroutine / handler. Lives on [`LuaShared::task_inbox`].
pub enum TaskEvent {
    /// A compositor dialog was submitted or dismissed. The Lua module
    /// fires the optional per-option `on_select` (consuming its key)
    /// and resumes the parked task with `{action, option_index,
    /// inputs}`.
    DialogResolved {
        dialog_id: u64,
        action: String,
        option_index: Option<usize>,
        inputs: Vec<(String, String)>,
        on_select: Option<mlua::RegistryKey>,
    },
    /// A plugin-registered keymap on an open Lua dialog fired. The Lua
    /// module looks up the `on_press` callback by `callback_id`, builds
    /// a `ctx` table (selected_index, inputs, `close()`), and invokes
    /// it. Does *not* close the dialog — the callback decides whether
    /// to call `ctx.close()`.
    KeymapFired {
        callback_id: u64,
        dialog_id: u64,
        win_id: ui::WinId,
        selected_index: Option<usize>,
        inputs: Vec<(String, String)>,
    },
    /// A picker opened via `smelt.api.picker.open` was resolved. The Lua
    /// module looks up the stored items under `opts_key`, builds
    /// `{ index = 1-based, item = <entry> }` (or `nil` on dismiss), and
    /// resumes the parked task.
    PickerResolved {
        picker_id: u64,
        /// 0-based index into the original items list. `None` on dismiss.
        selected_index: Option<usize>,
        /// Registry key for the original `opts.items` table so the
        /// resolver can look up the picked entry by index.
        opts: mlua::RegistryKey,
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
            statusline: Mutex::new(None),
            plugin_tools: Mutex::new(HashMap::new()),
            callbacks: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            history: Mutex::new(Arc::new(Vec::new())),
            tasks: Mutex::new(LuaTaskRuntime::new()),
            processes: Mutex::new(None),
            task_inbox: Mutex::new(Vec::new()),
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
    /// Build a fresh runtime, register the `smelt` global, and try to
    /// run `~/.config/smelt/init.lua`. Missing config files are not
    /// errors; syntax / runtime errors are captured on `load_error`.
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

        if rt.load_error.is_none() {
            for &name in AUTOLOAD_MODULES {
                let code = format!("require('{name}')");
                if let Err(e) = rt.lua.load(&code).set_name(name).exec() {
                    rt.load_error = Some(format!("autoload {name}: {e}"));
                    break;
                }
            }
        }

        if rt.load_error.is_none() {
            if let Some(path) = init_lua_path() {
                if path.exists() {
                    if let Err(e) = rt.load_init(&path) {
                        rt.load_error = Some(format!("~/.config/smelt/init.lua: {e}"));
                    }
                }
            }
        }

        rt
    }

    fn register_api(lua: &Lua, shared: &Arc<LuaShared>) -> LuaResult<()> {
        let smelt = lua.create_table()?;

        let api = lua.create_table()?;
        api.set("version", crate::api::VERSION)?;

        // Helper macro: lock shared.ops and read a snapshot field.
        macro_rules! snap_read {
            ($lua:expr, $s:expr, |$o:ident| $body:expr) => {{
                let s = $s.clone();
                $lua.create_function(move |_, ()| {
                    let $o = s
                        .ops
                        .lock()
                        .map_err(|e| LuaError::RuntimeError(e.to_string()))?;
                    Ok($body)
                })?
            }};
        }

        macro_rules! push_op {
            ($lua:expr, $s:expr, |$val:ident : $ty:ty| $op:expr) => {{
                let s = $s.clone();
                $lua.create_function(move |_, $val: $ty| {
                    if let Ok(mut o) = s.ops.lock() {
                        o.ops.push($op);
                    }
                    Ok(())
                })?
            }};
            ($lua:expr, $s:expr, || $op:expr) => {{
                let s = $s.clone();
                $lua.create_function(move |_, ()| {
                    if let Ok(mut o) = s.ops.lock() {
                        o.ops.push($op);
                    }
                    Ok(())
                })?
            }};
        }

        // smelt.api.transcript.text()
        let transcript_tbl = lua.create_table()?;
        transcript_tbl.set(
            "text",
            snap_read!(lua, shared, |o| o
                .transcript_text
                .clone()
                .unwrap_or_default()),
        )?;
        transcript_tbl.set(
            "yank_block",
            push_op!(lua, shared, || AppOp::YankBlockAtCursor),
        )?;
        api.set("transcript", transcript_tbl)?;

        // smelt.api.win.focus() / smelt.api.win.mode()
        // Helper macro: lock shared.ops and push an AppOp.
        // smelt.api.cmd
        let cmd_tbl = lua.create_table()?;
        {
            let s = shared.clone();
            cmd_tbl.set(
                "register",
                lua.create_function(
                    move |lua,
                          (name, handler, opts): (
                        String,
                        mlua::Function,
                        Option<mlua::Table>,
                    )| {
                        let desc: Option<String> = opts
                            .as_ref()
                            .and_then(|t| t.get::<Option<String>>("desc").ok().flatten());
                        let key = lua.create_registry_value(handler)?;
                        if let Ok(mut map) = s.commands.lock() {
                            map.insert(name.clone(), LuaHandle { key, dead: false });
                        }
                        if let Ok(mut snap) = lua_commands_snapshot().lock() {
                            snap.insert(name, desc);
                        }
                        Ok(())
                    },
                )?,
            )?;
        }
        cmd_tbl.set(
            "run",
            push_op!(lua, shared, |line: String| AppOp::RunCommand(line)),
        )?;
        {
            let s = shared.clone();
            cmd_tbl.set(
                "list",
                lua.create_function(move |lua, ()| {
                    let names: Vec<String> = s
                        .commands
                        .lock()
                        .map(|m| m.keys().cloned().collect())
                        .unwrap_or_default();
                    let table = lua.create_table()?;
                    for (i, name) in names.iter().enumerate() {
                        table.set(i + 1, name.as_str())?;
                    }
                    Ok(table)
                })?,
            )?;
        }
        api.set("cmd", cmd_tbl)?;

        // smelt.api.engine.*
        let engine_tbl = lua.create_table()?;

        engine_tbl.set("model", snap_read!(lua, shared, |o| o.engine.model.clone()))?;
        engine_tbl.set("mode", snap_read!(lua, shared, |o| o.engine.mode.clone()))?;
        engine_tbl.set(
            "reasoning_effort",
            snap_read!(lua, shared, |o| o.engine.reasoning_effort.clone()),
        )?;
        engine_tbl.set("is_busy", snap_read!(lua, shared, |o| o.engine.is_busy))?;
        engine_tbl.set("cost", snap_read!(lua, shared, |o| o.engine.session_cost))?;
        engine_tbl.set(
            "context_tokens",
            snap_read!(lua, shared, |o| o.engine.context_tokens),
        )?;
        engine_tbl.set(
            "context_window",
            snap_read!(lua, shared, |o| o.engine.context_window),
        )?;
        engine_tbl.set(
            "session_dir",
            snap_read!(lua, shared, |o| o.engine.session_dir.clone()),
        )?;
        engine_tbl.set(
            "session_id",
            snap_read!(lua, shared, |o| o.engine.session_id.clone()),
        )?;

        // smelt.api.session.* — current session metadata. Primitives only;
        // plugins compose features (export, rewind-list etc.) on top.
        let session_tbl = lua.create_table()?;
        session_tbl.set(
            "title",
            snap_read!(lua, shared, |o| o.engine.session_title.clone()),
        )?;
        session_tbl.set(
            "cwd",
            snap_read!(lua, shared, |o| o.engine.session_cwd.clone()),
        )?;
        session_tbl.set(
            "created_at_ms",
            snap_read!(lua, shared, |o| o.engine.session_created_at_ms),
        )?;
        session_tbl.set(
            "id",
            snap_read!(lua, shared, |o| o.engine.session_id.clone()),
        )?;
        session_tbl.set(
            "dir",
            snap_read!(lua, shared, |o| o.engine.session_dir.clone()),
        )?;
        // smelt.api.session.turns() → [{block_idx, label}]. Label is the
        // first line of each user turn (matches the /rewind display).
        {
            let s = shared.clone();
            session_tbl.set(
                "turns",
                lua.create_function(move |lua, ()| {
                    let turns = s
                        .ops
                        .lock()
                        .map(|o| o.engine.session_turns.clone())
                        .unwrap_or_default();
                    let out = lua.create_table()?;
                    for (i, (block_idx, text)) in turns.into_iter().enumerate() {
                        let row = lua.create_table()?;
                        row.set("block_idx", block_idx)?;
                        let label = text.lines().next().unwrap_or("").to_string();
                        row.set("label", label)?;
                        out.set(i + 1, row)?;
                    }
                    Ok(out)
                })?,
            )?;
        }
        // smelt.api.session.rewind_to(block_idx_or_nil, { restore_vim_insert = bool })
        {
            let s = shared.clone();
            session_tbl.set(
                "rewind_to",
                lua.create_function(
                    move |_, (block_idx, opts): (Option<usize>, Option<mlua::Table>)| {
                        let restore_vim_insert = opts
                            .and_then(|t| t.get::<bool>("restore_vim_insert").ok())
                            .unwrap_or(false);
                        if let Ok(mut o) = s.ops.lock() {
                            o.ops.push(AppOp::RewindToBlock {
                                block_idx,
                                restore_vim_insert,
                            });
                        }
                        Ok(())
                    },
                )?,
            )?;
        }
        // smelt.api.session.list() → [{id, title, subtitle, cwd,
        //   parent_id, updated_at_ms, created_at_ms, size_bytes?}]
        // Every session on disk except the current one, oldest first in
        // the raw list — plugins can sort. Reads straight off disk via
        // `crate::session::list_sessions`.
        {
            let s = shared.clone();
            session_tbl.set(
                "list",
                lua.create_function(move |lua, ()| {
                    let current_id = s
                        .ops
                        .lock()
                        .map(|o| o.engine.session_id.clone())
                        .unwrap_or_default();
                    let sessions = crate::session::list_sessions();
                    let out = lua.create_table()?;
                    let mut idx = 1;
                    for meta in sessions {
                        if meta.id == current_id {
                            continue;
                        }
                        let row = lua.create_table()?;
                        row.set("id", meta.id)?;
                        row.set("title", meta.title.unwrap_or_default())?;
                        row.set("subtitle", meta.first_user_message.unwrap_or_default())?;
                        row.set("cwd", meta.cwd.unwrap_or_default())?;
                        row.set("parent_id", meta.parent_id.unwrap_or_default())?;
                        row.set("updated_at_ms", meta.updated_at_ms)?;
                        row.set("created_at_ms", meta.created_at_ms)?;
                        if let Some(size) = meta.text_bytes {
                            row.set("size_bytes", size)?;
                        }
                        out.set(idx, row)?;
                        idx += 1;
                    }
                    Ok(out)
                })?,
            )?;
        }
        // smelt.api.session.load(id) — swap the running session to the
        // one stored on disk at `id` (accepts full id or a prefix).
        session_tbl.set(
            "load",
            push_op!(lua, shared, |id: String| AppOp::LoadSession(id)),
        )?;
        // smelt.api.session.delete(id) — remove a session from disk.
        // No-op when `id` matches the running session.
        session_tbl.set(
            "delete",
            push_op!(lua, shared, |id: String| AppOp::DeleteSession(id)),
        )?;
        api.set("session", session_tbl)?;

        engine_tbl.set(
            "set_model",
            push_op!(lua, shared, |v: String| AppOp::SetModel(v)),
        )?;
        engine_tbl.set(
            "set_mode",
            push_op!(lua, shared, |v: String| AppOp::SetMode(v)),
        )?;
        engine_tbl.set(
            "set_reasoning_effort",
            push_op!(lua, shared, |v: String| AppOp::SetReasoningEffort(v)),
        )?;
        engine_tbl.set(
            "submit",
            push_op!(lua, shared, |v: String| AppOp::Submit(v)),
        )?;
        engine_tbl.set("cancel", push_op!(lua, shared, || AppOp::Cancel))?;
        {
            let s = shared.clone();
            engine_tbl.set(
                "compact",
                lua.create_function(move |_, instructions: Option<String>| {
                    if let Ok(mut o) = s.ops.lock() {
                        o.ops.push(AppOp::Compact(instructions));
                    }
                    Ok(())
                })?,
            )?;
        }

        // smelt.api.engine.ask({ system, messages?, question?, task?, on_response })
        {
            let s = shared.clone();
            engine_tbl.set(
                "ask",
                lua.create_function(move |lua, spec: mlua::Table| {
                    let system: String = spec.get("system")?;
                    let task_str: Option<String> = spec.get("task")?;
                    let task = match task_str.as_deref() {
                        Some("title") => protocol::AuxiliaryTask::Title,
                        Some("prediction") => protocol::AuxiliaryTask::Prediction,
                        Some("compaction") => protocol::AuxiliaryTask::Compaction,
                        Some("btw") | None => protocol::AuxiliaryTask::Btw,
                        Some(other) => {
                            return Err(mlua::Error::external(format!(
                                "engine.ask: unknown task {other:?}; expected one of title / prediction / compaction / btw"
                            )));
                        }
                    };
                    let on_response: Option<mlua::Function> = spec.get("on_response")?;

                    let mut messages = Vec::new();
                    if let Ok(msgs) = spec.get::<mlua::Table>("messages") {
                        for pair in msgs.sequence_values::<mlua::Table>().flatten() {
                            let role: String = pair.get("role")?;
                            let content: String = pair.get("content")?;
                            let msg = match role.as_str() {
                                "user" => {
                                    protocol::Message::user(protocol::Content::text(&content))
                                }
                                "assistant" => protocol::Message::assistant(
                                    Some(protocol::Content::text(&content)),
                                    None,
                                    None,
                                ),
                                _ => continue,
                            };
                            messages.push(msg);
                        }
                    }
                    if let Ok(question) = spec.get::<String>("question") {
                        messages.push(protocol::Message::user(protocol::Content::text(&question)));
                    }

                    let id = s.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                    if let Some(func) = on_response {
                        let key = lua.create_registry_value(func)?;
                        if let Ok(mut cbs) = s.callbacks.lock() {
                            cbs.insert(id, LuaHandle { key, dead: false });
                        }
                    }

                    if let Ok(mut o) = s.ops.lock() {
                        o.ops.push(AppOp::EngineAsk {
                            id,
                            system,
                            messages,
                            task,
                        });
                    }
                    Ok(id)
                })?,
            )?;
        }

        // smelt.api.engine.history() → [{role, content, tool_calls?, tool_call_id?}]
        {
            let s = shared.clone();
            engine_tbl.set(
                "history",
                lua.create_function(move |lua, ()| {
                    let Ok(guard) = s.history.lock() else {
                        return lua.create_table();
                    };
                    let history = Arc::clone(&*guard);
                    drop(guard);
                    messages_to_lua(lua, &history)
                })?,
            )?;
        }

        api.set("engine", engine_tbl)?;

        // smelt.api.process.* — background process registry bridge.
        let process_tbl = lua.create_table()?;
        {
            let s = shared.clone();
            process_tbl.set(
                "list",
                lua.create_function(move |lua, ()| {
                    let Ok(guard) = s.processes.lock() else {
                        return lua.create_table();
                    };
                    let Some(registry) = guard.as_ref() else {
                        return lua.create_table();
                    };
                    let procs = registry.list();
                    drop(guard);
                    let out = lua.create_table()?;
                    for (i, p) in procs.into_iter().enumerate() {
                        let row = lua.create_table()?;
                        row.set("id", p.id)?;
                        row.set("command", p.command)?;
                        row.set("elapsed_secs", p.started_at.elapsed().as_secs())?;
                        out.set(i + 1, row)?;
                    }
                    Ok(out)
                })?,
            )?;
        }
        {
            let s = shared.clone();
            process_tbl.set(
                "kill",
                lua.create_function(move |_, id: String| {
                    if let Ok(mut o) = s.ops.lock() {
                        o.ops.push(AppOp::KillProcess(id));
                    }
                    Ok(())
                })?,
            )?;
        }
        api.set("process", process_tbl)?;

        // smelt.api.keymap.help_sections() → [{title, entries: [{label, detail}]}]
        // Pure lookup over the built-in hint tables; toggles the vim row
        // based on the current `vim_enabled` setting.
        let keymap_tbl = lua.create_table()?;
        {
            let s = shared.clone();
            keymap_tbl.set(
                "help_sections",
                lua.create_function(move |lua, ()| {
                    let vim_enabled = s.ops.lock().map(|o| o.engine.vim_enabled).unwrap_or(false);
                    let sections = crate::keymap::hints::help_sections(vim_enabled);
                    let out = lua.create_table()?;
                    for (i, (title, entries)) in sections.into_iter().enumerate() {
                        let row = lua.create_table()?;
                        row.set("title", title)?;
                        let entries_tbl = lua.create_table()?;
                        for (j, (label, detail)) in entries.into_iter().enumerate() {
                            let entry = lua.create_table()?;
                            entry.set("label", label)?;
                            entry.set("detail", detail)?;
                            entries_tbl.set(j + 1, entry)?;
                        }
                        row.set("entries", entries_tbl)?;
                        out.set(i + 1, row)?;
                    }
                    Ok(out)
                })?,
            )?;
        }
        api.set("keymap", keymap_tbl)?;

        // smelt.api.ui
        let ui_tbl = lua.create_table()?;
        ui_tbl.set(
            "set_ghost_text",
            push_op!(lua, shared, |text: String| AppOp::SetGhostText(text)),
        )?;
        ui_tbl.set(
            "clear_ghost_text",
            push_op!(lua, shared, || AppOp::ClearGhostText),
        )?;
        ui_tbl.set(
            "notify",
            push_op!(lua, shared, |msg: String| AppOp::Notify(msg)),
        )?;
        ui_tbl.set(
            "notify_error",
            push_op!(lua, shared, |msg: String| AppOp::NotifyError(msg)),
        )?;
        api.set("ui", ui_tbl)?;

        // smelt.api.theme
        let theme_tbl = lua.create_table()?;
        theme_tbl.set(
            "accent",
            lua.create_function(|lua, ()| color_to_lua(lua, crate::theme::accent()))?,
        )?;
        theme_tbl.set(
            "get",
            lua.create_function(|lua, role: String| {
                let color = theme_role_get(&role)
                    .ok_or_else(|| LuaError::RuntimeError(format!("unknown theme role: {role}")))?;
                color_to_lua(lua, color)
            })?,
        )?;
        theme_tbl.set(
            "set",
            lua.create_function(|_, (role, value): (String, mlua::Table)| {
                let ansi = color_ansi_from_lua(&value)?;
                theme_role_set(&role, ansi)
            })?,
        )?;
        theme_tbl.set(
            "snapshot",
            lua.create_function(|lua, ()| {
                let t = lua.create_table()?;
                for (name, color) in theme_snapshot_pairs() {
                    t.set(name, color_to_lua(lua, color)?)?;
                }
                Ok(t)
            })?,
        )?;
        theme_tbl.set(
            "is_light",
            lua.create_function(|_, ()| Ok(crate::theme::is_light()))?,
        )?;
        api.set("theme", theme_tbl)?;

        // smelt.api.buf
        let buf_tbl = lua.create_table()?;
        buf_tbl.set(
            "text",
            snap_read!(lua, shared, |o| o.prompt_text.clone().unwrap_or_default()),
        )?;
        // smelt.api.buf.create() → buf_id
        {
            let s = shared.clone();
            buf_tbl.set(
                "create",
                lua.create_function(move |_, ()| {
                    let id = s.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if let Ok(mut o) = s.ops.lock() {
                        o.ops.push(AppOp::BufCreate { id });
                    }
                    Ok(id)
                })?,
            )?;
        }
        // smelt.api.buf.set_lines(buf_id, lines)
        {
            let s = shared.clone();
            buf_tbl.set(
                "set_lines",
                lua.create_function(move |_, (id, lines): (u64, mlua::Table)| {
                    let lines: Vec<String> = lines
                        .sequence_values::<String>()
                        .filter_map(|v| v.ok())
                        .collect();
                    if let Ok(mut o) = s.ops.lock() {
                        o.ops.push(AppOp::BufSetLines { id, lines });
                    }
                    Ok(())
                })?,
            )?;
        }
        api.set("buf", buf_tbl)?;

        // smelt.api.win
        let win_tbl = lua.create_table()?;
        win_tbl.set(
            "focus",
            snap_read!(lua, shared, |o| o
                .focused_window
                .clone()
                .unwrap_or_default()),
        )?;
        win_tbl.set(
            "mode",
            snap_read!(lua, shared, |o| o.vim_mode.clone().unwrap_or_default()),
        )?;
        // smelt.api.win.open_float(buf_id, opts) → win_id
        {
            let s = shared.clone();
            win_tbl.set(
                "open_float",
                lua.create_function(move |lua, (buf_id, opts): (u64, mlua::Table)| {
                    let title: String = opts.get("title").unwrap_or_default();
                    let accent: Option<crossterm::style::Color> =
                        opts.get::<mlua::Table>("accent").ok().and_then(|t| {
                            t.get::<u8>("ansi")
                                .ok()
                                .map(crossterm::style::Color::AnsiValue)
                        });

                    let mut footer_items = Vec::new();
                    let mut on_select_handle = None;
                    let mut on_dismiss_handle = None;
                    if let Ok(footer) = opts.get::<mlua::Table>("footer") {
                        if let Ok(items) = footer.get::<mlua::Table>("items") {
                            footer_items = items
                                .sequence_values::<String>()
                                .filter_map(|v| v.ok())
                                .collect();
                        }
                        if let Ok(func) = footer.get::<mlua::Function>("on_select") {
                            let key = lua.create_registry_value(func)?;
                            on_select_handle = Some(key);
                        }
                    }
                    if let Ok(func) = opts.get::<mlua::Function>("on_dismiss") {
                        let key = lua.create_registry_value(func)?;
                        on_dismiss_handle = Some(key);
                    }

                    if let Some(key) = on_select_handle {
                        if let Ok(mut cbs) = s.callbacks.lock() {
                            cbs.insert(buf_id, LuaHandle { key, dead: false });
                        }
                    }
                    if let Some(key) = on_dismiss_handle {
                        let dismiss_id = buf_id | (1 << 63);
                        if let Ok(mut cbs) = s.callbacks.lock() {
                            cbs.insert(dismiss_id, LuaHandle { key, dead: false });
                        }
                    }

                    if let Ok(mut o) = s.ops.lock() {
                        o.ops.push(AppOp::WinOpenFloat {
                            buf_id,
                            title,
                            footer_items,
                            accent,
                        });
                    }
                    Ok(buf_id)
                })?,
            )?;
        }
        // smelt.api.win.close(win_id)
        {
            let s = shared.clone();
            win_tbl.set(
                "close",
                lua.create_function(move |_, id: u64| {
                    if let Ok(mut o) = s.ops.lock() {
                        o.ops.push(AppOp::WinClose { id });
                    }
                    Ok(())
                })?,
            )?;
        }
        // smelt.api.win.set_title(win_id, title)
        {
            let s = shared.clone();
            win_tbl.set(
                "set_title",
                lua.create_function(move |_, (id, title): (u64, String)| {
                    if let Ok(mut o) = s.ops.lock() {
                        o.ops.push(AppOp::WinUpdate {
                            id,
                            title: Some(title),
                        });
                    }
                    Ok(())
                })?,
            )?;
        }
        api.set("win", win_tbl)?;

        // smelt.api.prompt.set_section(name, content) / remove_section(name)
        let prompt_tbl = lua.create_table()?;
        {
            let s = shared.clone();
            prompt_tbl.set(
                "set_section",
                lua.create_function(move |_, (name, content): (String, String)| {
                    if let Ok(mut o) = s.ops.lock() {
                        o.ops.push(AppOp::SetPromptSection(name, content));
                    }
                    Ok(())
                })?,
            )?;
        }
        {
            let s = shared.clone();
            prompt_tbl.set(
                "remove_section",
                lua.create_function(move |_, name: String| {
                    if let Ok(mut o) = s.ops.lock() {
                        o.ops.push(AppOp::RemovePromptSection(name));
                    }
                    Ok(())
                })?,
            )?;
        }
        api.set("prompt", prompt_tbl)?;

        // smelt.api.tools.register(def) / unregister(name)
        let tools_tbl = lua.create_table()?;
        let s = shared.clone();
        let tools_register = lua.create_function(move |lua, def: mlua::Table| {
            let name: String = def.get("name")?;
            let handler: mlua::Function = def.get("execute")?;
            let key = lua.create_registry_value(handler)?;

            // Store metadata for plugin_tool_defs() lookups.
            let meta = lua.create_table()?;
            let desc: String = def.get("description").unwrap_or_default();
            meta.set("description", desc)?;
            if let Ok(params) = def.get::<mlua::Table>("parameters") {
                if let Ok(json_str) = serde_json::to_string(&lua_table_to_json(lua, &params)) {
                    meta.set("parameters_json", json_str)?;
                }
            }
            if let Ok(modes) = def.get::<mlua::Table>("modes") {
                meta.set("modes", modes)?;
            }
            if let Ok(mode_str) = def.get::<String>("execution_mode") {
                meta.set("execution_mode", mode_str)?;
            }
            lua.set_named_registry_value(&format!("__pt_meta_{name}"), meta)?;

            if let Ok(mut map) = s.plugin_tools.lock() {
                map.insert(name, LuaHandle { key, dead: false });
            }
            Ok(())
        })?;
        tools_tbl.set("register", tools_register)?;
        {
            let s = shared.clone();
            tools_tbl.set(
                "unregister",
                lua.create_function(move |_, name: String| {
                    if let Ok(mut map) = s.plugin_tools.lock() {
                        map.remove(&name);
                    }
                    Ok(())
                })?,
            )?;
        }
        // smelt.api.tools.resolve(request_id, call_id, result)
        {
            let s = shared.clone();
            tools_tbl.set(
                "resolve",
                lua.create_function(
                    move |_, (request_id, call_id, result): (u64, String, mlua::Table)| {
                        let content: String = result.get("content").unwrap_or_default();
                        let is_error: bool = result.get("is_error").unwrap_or(false);
                        if let Ok(mut o) = s.ops.lock() {
                            o.ops.push(AppOp::ResolveToolResult {
                                request_id,
                                call_id,
                                content,
                                is_error,
                            });
                        }
                        Ok(())
                    },
                )?,
            )?;
        }
        api.set("tools", tools_tbl)?;

        // smelt.api.fuzzy.score(text, query) → number | nil
        // Thin wrapper over crate::fuzzy::fuzzy_score. Lower = better.
        // Returns nil when query does not match.
        let fuzzy_tbl = lua.create_table()?;
        fuzzy_tbl.set(
            "score",
            lua.create_function(
                |_, (text, query): (String, String)| match crate::fuzzy::fuzzy_score(&text, &query)
                {
                    Some(s) => Ok(Some(s)),
                    None => Ok(None),
                },
            )?,
        )?;
        api.set("fuzzy", fuzzy_tbl)?;

        // smelt.api.picker placeholder — populated by TASK_YIELD_PRIMITIVES
        // with `open(opts)`.
        api.set("picker", lua.create_table()?)?;

        smelt.set("api", api)?;

        smelt.set(
            "notify",
            push_op!(lua, shared, |msg: String| AppOp::Notify(msg)),
        )?;

        smelt.set(
            "clipboard",
            lua.create_function(|_, text: String| {
                crate::app::commands::copy_to_clipboard(&text).map_err(LuaError::RuntimeError)?;
                Ok(())
            })?,
        )?;

        {
            let s = shared.clone();
            smelt.set(
                "keymap",
                lua.create_function(
                    move |lua, (mode, chord, handler): (String, String, mlua::Function)| {
                        let key = lua.create_registry_value(handler)?;
                        if let Ok(mut map) = s.keymaps.lock() {
                            map.insert((mode, chord), LuaHandle { key, dead: false });
                        }
                        Ok(())
                    },
                )?,
            )?;
        }

        {
            let s = shared.clone();
            smelt.set(
                "on",
                lua.create_function(move |lua, (event, handler): (String, mlua::Function)| {
                    let Some(kind) = AutocmdEvent::from_lua_name(&event) else {
                        return Err(LuaError::RuntimeError(format!("unknown event: {event}")));
                    };
                    let key = lua.create_registry_value(handler)?;
                    if let Ok(mut map) = s.autocmds.lock() {
                        map.entry(kind)
                            .or_default()
                            .push(LuaHandle { key, dead: false });
                    }
                    Ok(())
                })?,
            )?;
        }

        {
            let s = shared.clone();
            smelt.set(
                "defer",
                lua.create_function(move |lua, (ms, handler): (u64, mlua::Function)| {
                    let key = lua.create_registry_value(handler)?;
                    if let Ok(mut q) = s.timers.lock() {
                        q.push((
                            Instant::now() + Duration::from_millis(ms),
                            LuaHandle { key, dead: false },
                        ));
                    }
                    Ok(())
                })?,
            )?;
        }

        {
            let s = shared.clone();
            smelt.set(
                "statusline",
                lua.create_function(move |lua, handler: mlua::Function| {
                    let key = lua.create_registry_value(handler)?;
                    if let Ok(mut slot) = s.statusline.lock() {
                        *slot = Some(LuaHandle { key, dead: false });
                    }
                    Ok(())
                })?,
            )?;
        }

        {
            let s = shared.clone();
            smelt.set(
                "task",
                lua.create_function(move |lua, handler: mlua::Function| {
                    if let Ok(mut rt) = s.tasks.lock() {
                        rt.spawn(lua, handler, LuaValue::Nil, TaskCompletion::FireAndForget)?;
                    }
                    Ok(())
                })?,
            )?;
        }

        lua.globals().set("smelt", smelt)?;

        // Install the yielding primitives as Lua wrappers around
        // `coroutine.yield`. Each checks `coroutine.isyieldable()` so
        // calls from a non-task context raise a clear error instead of
        // yielding into the void.
        lua.load(TASK_YIELD_PRIMITIVES)
            .set_name("smelt/task_primitives")
            .exec()?;

        Ok(())
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

    /// Populate the engine snapshot fields. Called once at startup and
    /// whenever the engine state changes (mode, model, cost, tokens).
    pub fn set_engine_context(&self, snap: EngineSnapshot) {
        if let Ok(mut o) = self.shared.ops.lock() {
            o.engine = snap;
        }
    }

    /// Update the history snapshot. Called from `snapshot_engine_context`.
    pub fn set_history(&self, history: Vec<protocol::Message>) {
        if let Ok(mut h) = self.shared.history.lock() {
            *h = Arc::new(history);
        }
    }

    /// Install the background process registry so `smelt.api.process.*`
    /// primitives can enumerate and kill procs. Called once at App start.
    pub fn set_process_registry(&self, registry: engine::tools::ProcessRegistry) {
        if let Ok(mut p) = self.shared.processes.lock() {
            *p = Some(registry);
        }
    }

    /// Clear the snapshot fields after dispatching.
    pub fn clear_context(&self) {
        if let Ok(mut o) = self.shared.ops.lock() {
            o.clear_context();
        }
    }

    /// Drain all pending ops queued by Lua handlers.
    pub fn drain_ops(&self) -> Vec<AppOp> {
        let Ok(mut o) = self.shared.ops.lock() else {
            return Vec::new();
        };
        o.drain()
    }

    /// Get a cloneable handle to the shared `AppOp` queue. Rust
    /// dialog callbacks clone this and push typed effects from
    /// inside their closures. Lua and Rust share the same channel
    /// so the reducer in `App::apply_ops` drains them uniformly.
    pub fn ops_handle(&self) -> OpsHandle {
        OpsHandle(self.shared.clone())
    }

    /// Fire the `on_response` callback for a completed `engine.ask()` call.
    /// Returns any queued ops produced by the callback.
    pub fn fire_callback(&self, id: u64, content: &str) -> Vec<AppOp> {
        let handle = {
            let Ok(mut cbs) = self.shared.callbacks.lock() else {
                return vec![];
            };
            match cbs.remove(&id) {
                Some(h) => h,
                None => return vec![],
            }
        };
        let Ok(func) = self.lua.registry_value::<mlua::Function>(&handle.key) else {
            return vec![];
        };
        if let Err(e) = func.call::<()>(content.to_string()) {
            if let Ok(mut o) = self.shared.ops.lock() {
                o.ops.push(AppOp::NotifyError(format!("ask callback: {e}")));
            }
        }
        self.drain_ops()
    }

    pub fn remove_callback(&self, id: u64) {
        if let Ok(mut cbs) = self.shared.callbacks.lock() {
            cbs.remove(&id);
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
            if handle.dead {
                return false;
            }
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
            if handle.dead {
                return false;
            }
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
            .filter(|h| !h.dead)
            .filter_map(|h| self.lua.registry_value::<mlua::Function>(&h.key).ok())
            .collect()
    }

    /// Satisfy a `smelt.api.dialog.open` wait: hand the result
    /// table back to the parked task so it resumes on the next
    /// `drive_tasks` call. Returns `true` if a matching task was
    /// found. Called by the app when a Lua-driven dialog resolves.
    pub fn resolve_dialog(&self, dialog_id: u64, value: mlua::Value) -> bool {
        let Ok(mut rt) = self.shared.tasks.lock() else {
            return false;
        };
        rt.resolve_dialog(dialog_id, value)
    }

    /// Satisfy a `smelt.api.picker.open` wait: hand the result back to
    /// the parked task. Returns `true` if a matching task was found.
    pub fn resolve_picker(&self, picker_id: u64, value: mlua::Value) -> bool {
        let Ok(mut rt) = self.shared.tasks.lock() else {
            return false;
        };
        rt.resolve_picker(picker_id, value)
    }

    /// Drain the task-runtime inbox and apply each event. Dialog
    /// callbacks push here (instead of `AppOp`) so the reducer doesn't
    /// know about Lua-task lifecycle; the Lua module handles its own
    /// resumption. Returns the list of `AppOp`s produced by firing
    /// `on_select` callbacks (notifications etc.) so the caller can
    /// drain them into the main op queue.
    pub fn pump_task_events(&self) -> Vec<AppOp> {
        let events: Vec<TaskEvent> = {
            let Ok(mut inbox) = self.shared.task_inbox.lock() else {
                return Vec::new();
            };
            std::mem::take(&mut *inbox)
        };
        let mut extra_ops = Vec::new();
        for ev in events {
            match ev {
                TaskEvent::DialogResolved {
                    dialog_id,
                    action,
                    option_index,
                    inputs,
                    on_select,
                } => {
                    if let Some(key) = on_select {
                        if let Ok(func) = self.lua.registry_value::<mlua::Function>(&key) {
                            if let Err(e) = func.call::<()>(()) {
                                extra_ops
                                    .push(AppOp::NotifyError(format!("dialog on_select: {e}")));
                            }
                        }
                    }
                    match crate::app::dialogs::lua_dialog::build_result(
                        &self.lua,
                        &action,
                        option_index,
                        inputs,
                    ) {
                        Ok(v) => {
                            self.resolve_dialog(dialog_id, v);
                        }
                        Err(e) => {
                            extra_ops.push(AppOp::NotifyError(format!("dialog resolve: {e}")));
                            self.resolve_dialog(dialog_id, mlua::Value::Nil);
                        }
                    }
                }
                TaskEvent::KeymapFired {
                    callback_id,
                    dialog_id,
                    win_id,
                    selected_index,
                    inputs,
                } => {
                    let func = {
                        let Ok(cbs) = self.shared.callbacks.lock() else {
                            continue;
                        };
                        cbs.get(&callback_id)
                            .filter(|h| !h.dead)
                            .and_then(|h| self.lua.registry_value::<mlua::Function>(&h.key).ok())
                    };
                    let Some(func) = func else { continue };
                    match crate::app::dialogs::lua_dialog::build_keymap_ctx(
                        &self.lua,
                        self.shared.clone(),
                        dialog_id,
                        win_id,
                        selected_index,
                        inputs,
                    ) {
                        Ok(ctx) => {
                            if let Err(e) = func.call::<()>(ctx) {
                                extra_ops.push(AppOp::NotifyError(format!("keymap: {e}")));
                            }
                        }
                        Err(e) => {
                            extra_ops.push(AppOp::NotifyError(format!("keymap ctx: {e}")));
                        }
                    }
                }
                TaskEvent::PickerResolved {
                    picker_id,
                    selected_index,
                    opts,
                } => {
                    let value = match selected_index {
                        None => mlua::Value::Nil,
                        Some(idx0) => match build_picker_result(&self.lua, &opts, idx0) {
                            Ok(v) => v,
                            Err(e) => {
                                extra_ops.push(AppOp::NotifyError(format!("picker resolve: {e}")));
                                mlua::Value::Nil
                            }
                        },
                    };
                    self.resolve_picker(picker_id, value);
                }
            }
        }
        extra_ops
    }

    /// Access the underlying Lua state so callers can build result
    /// tables (e.g. for `resolve_dialog`).
    pub fn lua(&self) -> &Lua {
        &self.lua
    }

    /// Register a Lua callable under a fresh u64 id in
    /// `shared.callbacks`. Used by `lua_dialog.rs` to hand on_press
    /// handlers to `pump_task_events` — the keymap's Rust closure
    /// pushes a `TaskEvent::KeymapFired { callback_id }`, and the pump
    /// invokes the registered function.
    pub fn register_callback(&self, func: mlua::Function) -> mlua::Result<u64> {
        let key = self.lua.create_registry_value(func)?;
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut cbs) = self.shared.callbacks.lock() {
            cbs.insert(id, LuaHandle { key, dead: false });
        }
        Ok(id)
    }

    /// Drive the LuaTask runtime: resume any tasks whose waits have
    /// been satisfied (sleep elapsed, dialog resolved, …), park any
    /// new yields, and return the outputs for the app to act on.
    /// Errors are recorded via `NotifyError` directly; callers only
    /// need to handle `OpenDialog` and `ToolComplete` variants.
    pub fn drive_tasks(&self) -> Vec<TaskDriveOutput> {
        let Ok(mut rt) = self.shared.tasks.lock() else {
            return Vec::new();
        };
        let outs = rt.drive(&self.lua, Instant::now());
        let mut forward = Vec::with_capacity(outs.len());
        for out in outs {
            match out {
                TaskDriveOutput::Error(msg) => self.record_error(msg),
                other => forward.push(other),
            }
        }
        forward
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

    /// Call the Lua statusline provider (if registered) and return
    /// the resulting status items. Returns `None` if no provider is set.
    pub fn tick_statusline(&self) -> Option<Vec<crate::render::StatusItem>> {
        let handle = self.shared.statusline.lock().ok()?;
        let handle = handle.as_ref()?;
        if handle.dead {
            return None;
        }
        let func = self
            .lua
            .registry_value::<mlua::Function>(&handle.key)
            .ok()?;
        match func.call::<mlua::Table>(()) {
            Ok(table) => {
                let mut items = Vec::new();
                for pair in table.sequence_values::<mlua::Table>() {
                    let Ok(entry) = pair else { continue };
                    let text: String = entry.get("text").unwrap_or_default();
                    if text.is_empty() {
                        continue;
                    }
                    items.push(crate::render::StatusItem {
                        text,
                        fg: ansi_color_from_lua(&entry, "fg"),
                        bg: ansi_color_from_lua(&entry, "bg"),
                        bold: entry.get("bold").unwrap_or(false),
                        priority: entry.get("priority").unwrap_or(0),
                        align_right: entry.get("align_right").unwrap_or(false),
                        truncatable: entry.get("truncatable").unwrap_or(false),
                        group: entry.get("group").unwrap_or(false),
                    });
                }
                Some(items)
            }
            Err(e) => {
                self.record_error(format!("statusline: {e}"));
                None
            }
        }
    }

    fn record_error(&self, msg: String) {
        if let Ok(mut o) = self.shared.ops.lock() {
            o.ops.push(AppOp::NotifyError(msg));
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

    /// Return protocol-level plugin tool definitions for registered tools.
    /// The TUI sends these with `StartTurn` so the engine includes them in
    /// LLM tool definitions.
    pub fn plugin_tool_defs(&self, _mode: protocol::Mode) -> Vec<protocol::PluginToolDef> {
        let handlers = self
            .shared
            .plugin_tools
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut defs = Vec::new();
        for name in handlers.keys() {
            // Tool metadata (description, parameters, modes) is stored on a
            // separate Lua table registered alongside the handler. Look it up.
            if let Ok(meta_table) = self
                .lua
                .named_registry_value::<mlua::Table>(&format!("__pt_meta_{name}"))
            {
                let description: String = meta_table.get("description").unwrap_or_default();
                let parameters: serde_json::Value = meta_table
                    .get::<mlua::String>("parameters_json")
                    .ok()
                    .and_then(|s| serde_json::from_str(&s.to_string_lossy()).ok())
                    .unwrap_or(serde_json::json!({"type": "object", "properties": {}}));
                let modes: Option<Vec<protocol::Mode>> =
                    meta_table.get::<mlua::Table>("modes").ok().map(|t| {
                        t.sequence_values::<String>()
                            .filter_map(|r| r.ok())
                            .filter_map(|s| protocol::Mode::parse(&s))
                            .collect()
                    });
                let execution_mode = meta_table
                    .get::<String>("execution_mode")
                    .ok()
                    .and_then(|s| match s.as_str() {
                        "sequential" => Some(protocol::ToolExecutionMode::Sequential),
                        "concurrent" => Some(protocol::ToolExecutionMode::Concurrent),
                        _ => None,
                    })
                    .unwrap_or_default();
                defs.push(protocol::PluginToolDef {
                    name: name.clone(),
                    description,
                    parameters,
                    modes,
                    execution_mode,
                });
            }
        }
        defs
    }

    /// Execute a plugin tool by spawning a `LuaTask` around the
    /// registered handler.
    ///
    /// If the handler runs to completion without yielding (the common
    /// case — today's plugins don't yield), the result is returned as
    /// `ToolExecResult::Immediate` and the caller forwards it to the
    /// engine right away.
    ///
    /// If the handler yields (e.g. calls `smelt.api.dialog.open` —
    /// step iv), the task is parked and the result is delivered later
    /// via `drive_tasks() -> TaskDriveOutput::ToolComplete`; the
    /// caller receives `ToolExecResult::Pending` and forwards nothing.
    pub fn execute_plugin_tool(
        &self,
        tool_name: &str,
        args: &std::collections::HashMap<String, serde_json::Value>,
        request_id: u64,
        call_id: &str,
    ) -> ToolExecResult {
        // Resolve the handler.
        let func = {
            let handlers = self
                .shared
                .plugin_tools
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let Some(handle) = handlers.get(tool_name) else {
                return ToolExecResult::Immediate {
                    content: format!("no plugin tool registered: {tool_name}"),
                    is_error: true,
                };
            };
            match self.lua.registry_value::<mlua::Function>(&handle.key) {
                Ok(f) => f,
                Err(_) => {
                    return ToolExecResult::Immediate {
                        content: format!("plugin tool handler not found: {tool_name}"),
                        is_error: true,
                    };
                }
            }
        };

        // Bake args into a table. The table becomes the handler's
        // first argument on initial `resume`, so it passes directly
        // without a Rust thunk (which would block coroutine yield).
        let args_table = match self.lua.create_table() {
            Ok(t) => t,
            Err(e) => {
                return ToolExecResult::Immediate {
                    content: format!("plugin tool arg table: {e}"),
                    is_error: true,
                };
            }
        };
        for (k, v) in args {
            if let Ok(lua_val) = json_to_lua(&self.lua, v) {
                let _ = args_table.set(k.as_str(), lua_val);
            }
        }

        // Spawn + drive once. If the task finishes immediately we can
        // return the result inline; otherwise report Pending.
        let mut rt = match self.shared.tasks.lock() {
            Ok(g) => g,
            Err(_) => {
                return ToolExecResult::Immediate {
                    content: "task runtime poisoned".into(),
                    is_error: true,
                };
            }
        };
        if let Err(e) = rt.spawn(
            &self.lua,
            func,
            mlua::Value::Table(args_table),
            TaskCompletion::ToolResult {
                request_id,
                call_id: call_id.to_string(),
            },
        ) {
            return ToolExecResult::Immediate {
                content: format!("plugin tool spawn: {e}"),
                is_error: true,
            };
        }
        let outputs = rt.drive(&self.lua, Instant::now());
        drop(rt);

        // Partition outputs: a `ToolComplete` matching our request/call
        // is the immediate result; anything else (Error / future
        // OpenDialog) gets forwarded via the normal side channels.
        let mut immediate: Option<(String, bool)> = None;
        for out in outputs {
            match out {
                TaskDriveOutput::ToolComplete {
                    request_id: rid,
                    call_id: cid,
                    content,
                    is_error,
                } if rid == request_id && cid == call_id => {
                    immediate = Some((content, is_error));
                }
                TaskDriveOutput::Error(msg) => self.record_error(msg),
                other => self.queue_task_output(other),
            }
        }
        match immediate {
            Some((content, is_error)) => ToolExecResult::Immediate { content, is_error },
            None => ToolExecResult::Pending,
        }
    }

    /// Forward a non-terminal task output produced by an inline
    /// drive (inside `execute_plugin_tool`) onto the runtime's
    /// deferred queue so the next app-level `drive_tasks` sees it
    /// and routes appropriately (e.g. opens the dialog).
    fn queue_task_output(&self, out: TaskDriveOutput) {
        match out {
            TaskDriveOutput::OpenDialog { .. } | TaskDriveOutput::OpenPicker { .. } => {
                if let Ok(mut rt) = self.shared.tasks.lock() {
                    rt.defer_output(out);
                }
            }
            TaskDriveOutput::ToolComplete { .. } => {
                // Orphaned completion (unmatched id) — swallow.
            }
            TaskDriveOutput::Error(msg) => self.record_error(msg),
        }
    }
}

fn ansi_color_from_lua(table: &mlua::Table, key: &str) -> Option<crossterm::style::Color> {
    let val: u8 = table.get(key).ok()?;
    Some(crossterm::style::Color::AnsiValue(val))
}

// ── theme API helpers ─────────────────────────────────────────────────

/// Encode a `crossterm::style::Color` as a Lua table.
///
/// Shapes: `{ ansi = u8 }` for palette colors, `{ rgb = { r, g, b } }`
/// for truecolor, `{ named = "red" }` for the 16 legacy names.
fn color_to_lua(lua: &Lua, color: crossterm::style::Color) -> LuaResult<mlua::Table> {
    use crossterm::style::Color;
    let t = lua.create_table()?;
    match color {
        Color::AnsiValue(v) => t.set("ansi", v)?,
        Color::Rgb { r, g, b } => {
            let rgb = lua.create_table()?;
            rgb.set("r", r)?;
            rgb.set("g", g)?;
            rgb.set("b", b)?;
            t.set("rgb", rgb)?;
        }
        Color::Reset => t.set("named", "reset")?,
        Color::Black => t.set("named", "black")?,
        Color::DarkGrey => t.set("named", "dark_grey")?,
        Color::Red => t.set("named", "red")?,
        Color::DarkRed => t.set("named", "dark_red")?,
        Color::Green => t.set("named", "green")?,
        Color::DarkGreen => t.set("named", "dark_green")?,
        Color::Yellow => t.set("named", "yellow")?,
        Color::DarkYellow => t.set("named", "dark_yellow")?,
        Color::Blue => t.set("named", "blue")?,
        Color::DarkBlue => t.set("named", "dark_blue")?,
        Color::Magenta => t.set("named", "magenta")?,
        Color::DarkMagenta => t.set("named", "dark_magenta")?,
        Color::Cyan => t.set("named", "cyan")?,
        Color::DarkCyan => t.set("named", "dark_cyan")?,
        Color::White => t.set("named", "white")?,
        Color::Grey => t.set("named", "grey")?,
    }
    Ok(t)
}

/// Decode a Lua color table to an ANSI palette index. Accepts
/// `{ ansi = u8 }`, `{ preset = "name" }`, or `{ rgb = { r, g, b } }`
/// (rgb is down-sampled via the nearest-palette approximation — we
/// only store ansi values in theme atomics today).
fn color_ansi_from_lua(table: &mlua::Table) -> LuaResult<u8> {
    if let Ok(v) = table.get::<u8>("ansi") {
        return Ok(v);
    }
    if let Ok(name) = table.get::<String>("preset") {
        return crate::theme::preset_by_name(&name)
            .ok_or_else(|| LuaError::RuntimeError(format!("unknown preset: {name}")));
    }
    if let Ok(rgb) = table.get::<mlua::Table>("rgb") {
        let r: u8 = rgb.get("r")?;
        let g: u8 = rgb.get("g")?;
        let b: u8 = rgb.get("b")?;
        return Ok(rgb_to_ansi_256(r, g, b));
    }
    Err(LuaError::RuntimeError(
        "color table must have one of: ansi, preset, rgb".into(),
    ))
}

/// Nearest 6×6×6 palette index for an sRGB triple. The 216-color
/// block starts at 16; each channel maps to the nearest of
/// [0, 95, 135, 175, 215, 255].
fn rgb_to_ansi_256(r: u8, g: u8, b: u8) -> u8 {
    fn band(c: u8) -> u8 {
        let levels = [0u8, 95, 135, 175, 215, 255];
        levels
            .iter()
            .enumerate()
            .min_by_key(|(_, v)| (c as i32 - **v as i32).abs())
            .map(|(i, _)| i as u8)
            .unwrap_or(0)
    }
    16 + 36 * band(r) + 6 * band(g) + band(b)
}

/// Read a named theme role. Returns `None` for unknown names.
fn theme_role_get(role: &str) -> Option<crossterm::style::Color> {
    use crate::theme;
    Some(match role {
        "accent" => theme::accent(),
        "slug" => theme::slug_color(),
        "user_bg" => theme::user_bg(),
        "code_block_bg" => theme::code_block_bg(),
        "bar" => theme::bar(),
        "tool_pending" => theme::tool_pending(),
        "reason_off" => theme::reason_off(),
        "muted" => theme::muted(),
        _ => return None,
    })
}

/// Set a writable theme role. Only `accent` and `slug` are mutable
/// today; everything else is derived from light/dark and returns an
/// error.
fn theme_role_set(role: &str, ansi: u8) -> LuaResult<()> {
    use crate::theme;
    match role {
        "accent" => {
            theme::set_accent(ansi);
            Ok(())
        }
        "slug" => {
            theme::set_slug_color(ansi);
            Ok(())
        }
        other => Err(LuaError::RuntimeError(format!(
            "theme role is read-only: {other}"
        ))),
    }
}

/// List of (role_name, current_color) pairs for `theme.snapshot()`.
fn theme_snapshot_pairs() -> Vec<(&'static str, crossterm::style::Color)> {
    use crate::theme;
    vec![
        ("accent", theme::accent()),
        ("slug", theme::slug_color()),
        ("user_bg", theme::user_bg()),
        ("code_block_bg", theme::code_block_bg()),
        ("bar", theme::bar()),
        ("tool_pending", theme::tool_pending()),
        ("reason_off", theme::reason_off()),
        ("muted", theme::muted()),
    ]
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

/// Convert a Lua table to a `serde_json::Value`.
fn lua_table_to_json(lua: &Lua, table: &mlua::Table) -> serde_json::Value {
    // Treat tables with contiguous 1..N integer keys as JSON arrays;
    // anything else (string keys, gaps, mixed) becomes a JSON object.
    // Matches Lua convention where `{ "a", "b" }` is a sequence and
    // `{ k = v }` is a map. Empty tables serialize as empty arrays,
    // which is the safer default for JSON-schema fields like `required`.
    let mut pairs: Vec<(mlua::Value, mlua::Value)> = Vec::new();
    for pair in table.pairs::<mlua::Value, mlua::Value>() {
        let Ok(kv) = pair else { continue };
        pairs.push(kv);
    }

    let is_array = !pairs.is_empty()
        && pairs
            .iter()
            .all(|(k, _)| matches!(k, mlua::Value::Integer(_)))
        && {
            let mut ints: Vec<i64> = pairs
                .iter()
                .filter_map(|(k, _)| match k {
                    mlua::Value::Integer(i) => Some(*i),
                    _ => None,
                })
                .collect();
            ints.sort_unstable();
            ints.first().copied() == Some(1) && ints.windows(2).all(|w| w[1] == w[0] + 1)
        };

    if is_array || pairs.is_empty() {
        let len = table.raw_len();
        let mut arr = Vec::with_capacity(len);
        for i in 1..=len {
            let val: mlua::Value = table.raw_get(i).unwrap_or(mlua::Value::Nil);
            arr.push(lua_value_to_json(lua, &val));
        }
        serde_json::Value::Array(arr)
    } else {
        let mut map = serde_json::Map::new();
        for (key, val) in pairs {
            let key_str = match &key {
                mlua::Value::String(s) => s.to_string_lossy().to_string(),
                mlua::Value::Integer(i) => i.to_string(),
                _ => continue,
            };
            map.insert(key_str, lua_value_to_json(lua, &val));
        }
        serde_json::Value::Object(map)
    }
}

fn lua_value_to_json(lua: &Lua, val: &mlua::Value) -> serde_json::Value {
    match val {
        mlua::Value::Nil => serde_json::Value::Null,
        mlua::Value::Boolean(b) => serde_json::Value::Bool(*b),
        mlua::Value::Integer(i) => serde_json::json!(*i),
        mlua::Value::Number(n) => serde_json::json!(*n),
        mlua::Value::String(s) => serde_json::Value::String(s.to_string_lossy().to_string()),
        mlua::Value::Table(t) => lua_table_to_json(lua, t),
        _ => serde_json::Value::Null,
    }
}

/// Build the resume value for a resolved picker. Looks up
/// `opts.items[index0 + 1]` and returns `{ index = 1-based, item = <entry> }`.
/// Returns an error if the opts key was stale or missing `items`.
fn build_picker_result(
    lua: &Lua,
    opts_key: &mlua::RegistryKey,
    index0: usize,
) -> Result<mlua::Value, String> {
    let opts: mlua::Table = lua
        .registry_value(opts_key)
        .map_err(|e| format!("opts: {e}"))?;
    let items: mlua::Table = opts.get("items").map_err(|e| format!("items: {e}"))?;
    let item: mlua::Value = items
        .get(index0 + 1)
        .map_err(|e| format!("item[{}]: {e}", index0 + 1))?;
    let out = lua.create_table().map_err(|e| e.to_string())?;
    out.set("index", index0 + 1).map_err(|e| e.to_string())?;
    out.set("item", item).map_err(|e| e.to_string())?;
    Ok(mlua::Value::Table(out))
}

/// Lua source injected at bootstrap to install the task-yielding
/// primitives. Each checks `coroutine.isyieldable()` so calls from
/// outside a task raise a clear error rather than failing later.
const TASK_YIELD_PRIMITIVES: &str = r#"
smelt.api = smelt.api or {}
smelt.api.dialog = smelt.api.dialog or {}
smelt.api.picker = smelt.api.picker or {}

function smelt.api.sleep(ms)
  if not coroutine.isyieldable() then
    error("smelt.api.sleep: call from inside smelt.task(fn) or tool.execute", 2)
  end
  return coroutine.yield({__yield = "sleep", ms = ms})
end

-- Open a dialog and wait for the user's answer. Blocks the task
-- coroutine. Returns a table:
--   { action = "<option.action> | 'dismiss'",
--     option_index = <1-based int | nil>,
--     inputs       = { [name] = "<text>", … } }
function smelt.api.dialog.open(opts)
  if not coroutine.isyieldable() then
    error("smelt.api.dialog.open: call from inside smelt.task(fn) or tool.execute", 2)
  end
  if type(opts) ~= "table" then
    error("smelt.api.dialog.open: expected table of options", 2)
  end
  return coroutine.yield({__yield = "dialog", opts = opts})
end

-- Open a focusable picker (single-column selection list) and wait for
-- the user's choice. Blocks the task coroutine.
-- opts = {
--   items = { "str" | { label=..., description?=..., prefix?=... }, ... },
--   title? = string,
--   placement? = "center" | "cursor" | "bottom" (default "center"),
-- }
-- Returns { index = <1-based int>, item = <original entry> } on select,
-- or nil on dismiss.
function smelt.api.picker.open(opts)
  if not coroutine.isyieldable() then
    error("smelt.api.picker.open: call from inside smelt.task(fn) or tool.execute", 2)
  end
  if type(opts) ~= "table" then
    error("smelt.api.picker.open: expected table of options", 2)
  end
  if type(opts.items) ~= "table" then
    error("smelt.api.picker.open: opts.items must be a table", 2)
  end
  return coroutine.yield({__yield = "picker", opts = opts})
end
"#;

impl Default for LuaRuntime {
    fn default() -> Self {
        Self::new()
    }
}

/// Modules embedded in the binary, available via `require("smelt.plugins.X")`.
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
];

/// Plugins that must always be active (the user can't opt out via
/// init.lua). These are former Rust built-ins migrated to Lua. Required
/// after the embedded searcher is set up, before user init.lua runs.
const AUTOLOAD_MODULES: &[&str] = &[
    "smelt.plugins.ask_user_question",
    "smelt.plugins.export",
    "smelt.plugins.rewind",
    "smelt.plugins.ps",
    "smelt.plugins.help",
    "smelt.plugins.yank_block",
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
    use super::*;

    fn drain_notifications(rt: &LuaRuntime) -> Vec<String> {
        rt.drain_ops()
            .into_iter()
            .filter_map(|op| match op {
                AppOp::Notify(msg) => Some(msg),
                _ => None,
            })
            .collect()
    }

    fn drain_errors(rt: &LuaRuntime) -> Vec<String> {
        rt.drain_ops()
            .into_iter()
            .filter_map(|op| match op {
                AppOp::NotifyError(msg) => Some(msg),
                _ => None,
            })
            .collect()
    }

    fn drain_commands(rt: &LuaRuntime) -> Vec<String> {
        rt.drain_ops()
            .into_iter()
            .filter_map(|op| match op {
                AppOp::RunCommand(line) => Some(line),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn theme_accent_round_trip() {
        let rt = LuaRuntime::new();
        assert!(rt.load_error.is_none(), "load_error: {:?}", rt.load_error);
        let old_accent = crate::theme::accent_value();
        rt.lua
            .load("smelt.api.theme.set('accent', { ansi = 42 })")
            .exec()
            .unwrap();
        let ansi: u8 = rt
            .lua
            .load("return smelt.api.theme.accent().ansi")
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
            .load("smelt.api.theme.set('accent', { preset = 'sage' })")
            .exec()
            .unwrap();
        let ansi: u8 = rt
            .lua
            .load("return smelt.api.theme.accent().ansi")
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
                local snap = smelt.api.theme.snapshot()
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
        let err = rt
            .lua
            .load("smelt.api.theme.get('bogus')")
            .exec()
            .unwrap_err();
        assert!(err.to_string().contains("unknown theme role"));
    }

    #[test]
    fn theme_read_only_role_set_fails() {
        let rt = LuaRuntime::new();
        let err = rt
            .lua
            .load("smelt.api.theme.set('muted', { ansi = 1 })")
            .exec()
            .unwrap_err();
        assert!(err.to_string().contains("read-only"));
    }

    #[test]
    fn runtime_exposes_api_version() {
        let rt = LuaRuntime::new();
        assert!(rt.load_error.is_none(), "load_error: {:?}", rt.load_error);
        let version: String = rt
            .lua
            .load("return smelt.api.version")
            .eval()
            .expect("eval");
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
        let rt = LuaRuntime::new();
        assert!(rt.load_error.is_none(), "load_error: {:?}", rt.load_error);
        assert!(
            rt.has_command("export"),
            "/export should be registered by the autoloaded plugin"
        );
    }

    #[test]
    fn autoload_registers_ps_command() {
        let rt = LuaRuntime::new();
        assert!(rt.load_error.is_none(), "load_error: {:?}", rt.load_error);
        assert!(
            rt.has_command("ps"),
            "/ps should be registered by the autoloaded plugin"
        );
    }

    #[test]
    fn autoload_registers_rewind_command() {
        let rt = LuaRuntime::new();
        assert!(rt.load_error.is_none(), "load_error: {:?}", rt.load_error);
        assert!(
            rt.has_command("rewind"),
            "/rewind should be registered by the autoloaded plugin"
        );
    }

    #[test]
    fn autoload_registers_ask_user_question_as_sequential() {
        let rt = LuaRuntime::new();
        assert!(rt.load_error.is_none(), "load_error: {:?}", rt.load_error);
        let defs = rt.plugin_tool_defs(protocol::Mode::Normal);
        let ask = defs
            .iter()
            .find(|d| d.name == "ask_user_question")
            .expect("ask_user_question should be auto-registered");
        assert_eq!(ask.execution_mode, protocol::ToolExecutionMode::Sequential);
    }

    #[test]
    fn dialog_open_yield_parks_task_and_resume_delivers_result() {
        let rt = LuaRuntime::new();
        rt.lua
            .load(
                r#"
                smelt.api.tools.register({
                  name = "confirm_then_return",
                  description = "",
                  parameters = { type = "object", properties = {} },
                  execute = function()
                    local r = smelt.api.dialog.open({
                      panels = {
                        { kind = "content", text = "please confirm" },
                        { kind = "options", items = {
                          { label = "yes", action = "approve" },
                          { label = "no",  action = "deny"    },
                        }},
                      },
                    })
                    return r.action
                  end,
                })
                "#,
            )
            .exec()
            .unwrap();
        let args = std::collections::HashMap::new();
        assert!(matches!(
            rt.execute_plugin_tool("confirm_then_return", &args, 1, "c"),
            ToolExecResult::Pending
        ));
        // Drive picks up the deferred OpenDialog from the inline execute.
        let outs = rt.drive_tasks();
        let (task_dialog_id, _opts_key) = outs
            .iter()
            .find_map(|o| match o {
                TaskDriveOutput::OpenDialog { dialog_id, .. } => Some((*dialog_id, 0u8)),
                _ => None,
            })
            .expect("expected OpenDialog yield");
        // Now resolve the dialog as if the user chose "approve".
        let result = rt
            .lua
            .load(r#"return { action = "approve", option_index = 1, inputs = {} }"#)
            .eval::<mlua::Value>()
            .unwrap();
        assert!(rt.resolve_dialog(task_dialog_id, result));
        // Next drive resumes the task; it should complete with "approve".
        let outs = rt.drive_tasks();
        assert!(outs.iter().any(|o| matches!(
            o,
            TaskDriveOutput::ToolComplete { content, is_error: false, .. } if content == "approve"
        )));
    }

    #[test]
    fn dialog_open_outside_task_errors() {
        let rt = LuaRuntime::new();
        let res: LuaResult<()> = rt.lua.load("smelt.api.dialog.open({panels = {}})").exec();
        assert!(res.is_err());
    }

    #[test]
    fn plugin_tool_runs_as_task_immediate() {
        let rt = LuaRuntime::new();
        rt.lua
            .load(
                r#"
                smelt.api.tools.register({
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
                smelt.api.tools.register({
                  name = "wait_then_yes",
                  description = "",
                  parameters = { type = "object", properties = {} },
                  execute = function()
                    smelt.api.sleep(0)
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
        rt.lua
            .load(
                r#"
                    smelt.api.cmd.register("hello", function()
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
        rt.lua
            .load(
                r#"
                    smelt.keymap("n", "<C-g>", function()
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
        rt.lua
            .load(
                r#"
                    smelt.keymap("", "<C-h>", function()
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
    fn cmd_run_queues_for_dispatch() {
        let rt = LuaRuntime::new();
        rt.lua
            .load(r#"smelt.api.cmd.run("/compact")"#)
            .exec()
            .expect("exec");
        let queued = drain_commands(&rt);
        assert_eq!(queued, vec!["/compact".to_string()]);
        assert!(drain_commands(&rt).is_empty());
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
    fn callback_error_surfaces_without_panic() {
        let rt = LuaRuntime::new();
        rt.lua
            .load(
                r#"
                    smelt.api.cmd.register("broken", function()
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
            .load("return smelt.api.transcript.text()")
            .eval()
            .expect("eval");
        assert_eq!(text, "hello world");
        rt.clear_context();
        let text: String = rt
            .lua
            .load("return smelt.api.transcript.text()")
            .eval()
            .expect("eval");
        assert_eq!(text, "");
    }

    #[test]
    fn buf_text_reads_context() {
        let rt = LuaRuntime::new();
        rt.set_context(None, Some("prompt content".into()), None, None);
        let text: String = rt
            .lua
            .load("return smelt.api.buf.text()")
            .eval()
            .expect("eval");
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
            .load("return smelt.api.engine.model()")
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
            .load("return smelt.api.engine.mode()")
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
            .load("return smelt.api.engine.is_busy()")
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
            .load("return smelt.api.engine.cost()")
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
            .load("return smelt.api.engine.context_tokens()")
            .eval()
            .expect("eval");
        assert_eq!(tokens, 5000);
        let window: u32 = rt
            .lua
            .load("return smelt.api.engine.context_window()")
            .eval()
            .expect("eval");
        assert_eq!(window, 128000);
    }

    #[test]
    fn engine_set_mode_queues_op() {
        let rt = LuaRuntime::new();
        rt.lua
            .load(r#"smelt.api.engine.set_mode("plan")"#)
            .exec()
            .expect("exec");
        let ops = rt.drain_ops();
        assert_eq!(ops.len(), 1);
        assert!(matches!(&ops[0], AppOp::SetMode(m) if m == "plan"));
    }

    #[test]
    fn engine_set_model_queues_op() {
        let rt = LuaRuntime::new();
        rt.lua
            .load(r#"smelt.api.engine.set_model("gpt-4o")"#)
            .exec()
            .expect("exec");
        let ops = rt.drain_ops();
        assert_eq!(ops.len(), 1);
        assert!(matches!(&ops[0], AppOp::SetModel(m) if m == "gpt-4o"));
    }

    #[test]
    fn engine_cancel_queues_op() {
        let rt = LuaRuntime::new();
        rt.lua
            .load("smelt.api.engine.cancel()")
            .exec()
            .expect("exec");
        let ops = rt.drain_ops();
        assert_eq!(ops.len(), 1);
        assert!(matches!(&ops[0], AppOp::Cancel));
    }

    #[test]
    fn engine_submit_queues_op() {
        let rt = LuaRuntime::new();
        rt.lua
            .load(r#"smelt.api.engine.submit("hello")"#)
            .exec()
            .expect("exec");
        let ops = rt.drain_ops();
        assert_eq!(ops.len(), 1);
        assert!(matches!(&ops[0], AppOp::Submit(t) if t == "hello"));
    }

    #[test]
    fn engine_compact_queues_op() {
        let rt = LuaRuntime::new();
        rt.lua
            .load(r#"smelt.api.engine.compact("keep tests")"#)
            .exec()
            .expect("exec");
        let ops = rt.drain_ops();
        assert_eq!(ops.len(), 1);
        assert!(matches!(&ops[0], AppOp::Compact(Some(s)) if s == "keep tests"));
    }

    #[test]
    fn emit_data_passes_table_to_handler() {
        let rt = LuaRuntime::new();
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
            .load("return smelt.api.engine.reasoning_effort()")
            .eval()
            .expect("eval");
        assert_eq!(effort, "high");
    }
}
