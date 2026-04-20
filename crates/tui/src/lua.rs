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

use mlua::prelude::*;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

/// Confirm dialog spec for a plugin tool, declared during registration.
#[derive(Debug)]
pub struct PluginConfirmSpec {
    pub message: String,
    pub action_label: String,
    pub preview_field: Option<String>,
    pub accent_color: Option<crossterm::style::Color>,
    pub options: Vec<PluginConfirmOption>,
}

#[derive(Debug)]
pub struct PluginConfirmOption {
    pub label: String,
    /// "approve" or "deny". Determines whether the tool executes.
    pub action: String,
    /// Lua callback name stored in registry, called after the option is
    /// selected (e.g. to switch mode). None means no side effect.
    pub on_select_key: Option<mlua::RegistryKey>,
}

/// Deferred mutation queued by a Lua handler. Applied by the app loop
/// after the handler returns, avoiding nested borrows on `App`.
pub enum PendingOp {
    Notify(String),
    NotifyError(String),
    RunCommand(String),
    SetMode(String),
    SetModel(String),
    SetReasoningEffort(String),
    Cancel,
    Compact(Option<String>),
    Submit(String),
    SetPromptSection(String, String),
    RemovePromptSection(String),
    SetPermissionOverrides(protocol::PermissionOverrides),
    /// Background LLM call from a Lua plugin.
    BackgroundAsk {
        id: u64,
        system: String,
        messages: Vec<protocol::Message>,
        task: Option<String>,
    },
    /// Set ghost text (predicted input) on the prompt.
    SetGhostText(String),
    /// Clear ghost text from the prompt.
    ClearGhostText,
    /// Open or update a named float.
    OpenFloat {
        id: u64,
        title: String,
        lines: Vec<String>,
        loading: bool,
        footer_items: Vec<String>,
        accent: Option<crossterm::style::Color>,
    },
    /// Update the content of an existing float.
    UpdateFloat {
        id: u64,
        title: Option<String>,
        lines: Option<Vec<String>>,
        loading: Option<bool>,
    },
    /// Close a named float.
    CloseFloat {
        id: u64,
    },
    /// Send a deferred plugin tool result.
    ResolveToolResult {
        request_id: u64,
        call_id: String,
        content: String,
        is_error: bool,
    },
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
}

/// Shared state between Lua closures and the app loop.
///
/// **Reads**: snapshot fields populated by `set_context()` before a
/// handler runs. Lua reads these via `smelt.api.transcript.text()` etc.
///
/// **Writes**: `ops` collects deferred mutations (`PendingOp`) that the
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
    pub ops: Vec<PendingOp>,
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

    pub fn drain(&mut self) -> Vec<PendingOp> {
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
    pub(crate) confirm_specs: Mutex<HashMap<String, PluginConfirmSpec>>,
    pub(crate) callbacks: Mutex<HashMap<u64, LuaHandle>>,
    pub(crate) next_id: AtomicU64,
    pub(crate) history: Mutex<Arc<Vec<protocol::Message>>>,
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
            confirm_specs: Mutex::new(HashMap::new()),
            callbacks: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            history: Mutex::new(Arc::new(Vec::new())),
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

        // smelt.api.transcript.text()
        let transcript_tbl = lua.create_table()?;
        transcript_tbl.set(
            "text",
            snap_read!(lua, shared, |o| o
                .transcript_text
                .clone()
                .unwrap_or_default()),
        )?;
        api.set("transcript", transcript_tbl)?;

        // smelt.api.win.focus() / smelt.api.win.mode()
        // Helper macro: lock shared.ops and push a PendingOp.
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

        // smelt.api.cmd
        let cmd_tbl = lua.create_table()?;
        {
            let s = shared.clone();
            cmd_tbl.set(
                "register",
                lua.create_function(move |lua, (name, handler): (String, mlua::Function)| {
                    let key = lua.create_registry_value(handler)?;
                    if let Ok(mut map) = s.commands.lock() {
                        map.insert(name, LuaHandle { key, dead: false });
                    }
                    Ok(())
                })?,
            )?;
        }
        cmd_tbl.set(
            "run",
            push_op!(lua, shared, |line: String| PendingOp::RunCommand(line)),
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

        engine_tbl.set(
            "set_model",
            push_op!(lua, shared, |v: String| PendingOp::SetModel(v)),
        )?;
        engine_tbl.set(
            "set_mode",
            push_op!(lua, shared, |v: String| PendingOp::SetMode(v)),
        )?;
        engine_tbl.set(
            "set_reasoning_effort",
            push_op!(lua, shared, |v: String| PendingOp::SetReasoningEffort(v)),
        )?;
        engine_tbl.set(
            "submit",
            push_op!(lua, shared, |v: String| PendingOp::Submit(v)),
        )?;
        engine_tbl.set("cancel", push_op!(lua, shared, || PendingOp::Cancel))?;
        {
            let s = shared.clone();
            engine_tbl.set(
                "compact",
                lua.create_function(move |_, instructions: Option<String>| {
                    if let Ok(mut o) = s.ops.lock() {
                        o.ops.push(PendingOp::Compact(instructions));
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
                    let task: Option<String> = spec.get("task")?;
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
                        o.ops.push(PendingOp::BackgroundAsk {
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

        // smelt.api.ui
        let ui_tbl = lua.create_table()?;
        ui_tbl.set(
            "set_ghost_text",
            push_op!(lua, shared, |text: String| PendingOp::SetGhostText(text)),
        )?;
        ui_tbl.set(
            "clear_ghost_text",
            push_op!(lua, shared, || PendingOp::ClearGhostText),
        )?;
        ui_tbl.set(
            "notify",
            push_op!(lua, shared, |msg: String| PendingOp::Notify(msg)),
        )?;
        ui_tbl.set(
            "notify_error",
            push_op!(lua, shared, |msg: String| PendingOp::NotifyError(msg)),
        )?;
        api.set("ui", ui_tbl)?;

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
                    Ok(s.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
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
                        o.ops.push(PendingOp::UpdateFloat {
                            id,
                            title: None,
                            lines: Some(lines),
                            loading: None,
                        });
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
        // smelt.api.win.open_float(buf_id, opts) → win_id (same as buf_id for now)
        {
            let s = shared.clone();
            win_tbl.set(
                "open_float",
                lua.create_function(move |lua, (buf_id, opts): (u64, mlua::Table)| {
                    let title: String = opts.get("title").unwrap_or_default();
                    let loading: bool = opts.get("loading").unwrap_or(false);
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

                    // Store callbacks
                    if let Some(key) = on_select_handle {
                        if let Ok(mut cbs) = s.callbacks.lock() {
                            cbs.insert(buf_id, LuaHandle { key, dead: false });
                        }
                    }
                    if let Some(key) = on_dismiss_handle {
                        let dismiss_id = buf_id | (1 << 63); // high bit = dismiss callback
                        if let Ok(mut cbs) = s.callbacks.lock() {
                            cbs.insert(dismiss_id, LuaHandle { key, dead: false });
                        }
                    }

                    if let Ok(mut o) = s.ops.lock() {
                        o.ops.push(PendingOp::OpenFloat {
                            id: buf_id,
                            title,
                            lines: Vec::new(),
                            loading,
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
                        o.ops.push(PendingOp::CloseFloat { id });
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
                        o.ops.push(PendingOp::UpdateFloat {
                            id,
                            title: Some(title),
                            lines: None,
                            loading: None,
                        });
                    }
                    Ok(())
                })?,
            )?;
        }
        // smelt.api.win.set_loading(win_id, loading)
        {
            let s = shared.clone();
            win_tbl.set(
                "set_loading",
                lua.create_function(move |_, (id, loading): (u64, bool)| {
                    if let Ok(mut o) = s.ops.lock() {
                        o.ops.push(PendingOp::UpdateFloat {
                            id,
                            title: None,
                            lines: None,
                            loading: Some(loading),
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
                        o.ops.push(PendingOp::SetPromptSection(name, content));
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
                        o.ops.push(PendingOp::RemovePromptSection(name));
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
            let (pt_clone, pcs_clone) = (&s.plugin_tools, &s.confirm_specs);
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
            lua.set_named_registry_value(&format!("__pt_meta_{name}"), meta)?;

            // Parse optional confirm spec.
            if let Ok(confirm_tbl) = def.get::<mlua::Table>("confirm") {
                let display_name: String = confirm_tbl.get("display_name").unwrap_or(name.clone());
                let action_label: String =
                    confirm_tbl.get("action_label").unwrap_or("Allow?".into());
                let preview_field: Option<String> = confirm_tbl.get("preview_field").ok();
                let accent_color = confirm_tbl
                    .get::<mlua::Table>("accent_color")
                    .ok()
                    .and_then(|t| ansi_color_from_lua(&t, "ansi"));

                let mut options = Vec::new();
                if let Ok(opts_tbl) = confirm_tbl.get::<mlua::Table>("options") {
                    for opt in opts_tbl.sequence_values::<mlua::Table>().flatten() {
                        let label: String = opt.get("label").unwrap_or_default();
                        let action: String = opt.get("action").unwrap_or("approve".into());
                        let on_select: Option<mlua::RegistryKey> = opt
                            .get::<mlua::Function>("on_select")
                            .ok()
                            .and_then(|f| lua.create_registry_value(f).ok());
                        options.push(PluginConfirmOption {
                            label,
                            action,
                            on_select_key: on_select,
                        });
                    }
                }
                if options.is_empty() {
                    options.push(PluginConfirmOption {
                        label: "yes".into(),
                        action: "approve".into(),
                        on_select_key: None,
                    });
                    options.push(PluginConfirmOption {
                        label: "no".into(),
                        action: "deny".into(),
                        on_select_key: None,
                    });
                }

                let spec = PluginConfirmSpec {
                    message: display_name.clone(),
                    action_label,
                    preview_field,
                    accent_color,
                    options,
                };
                if let Ok(mut map) = pcs_clone.lock() {
                    map.insert(name.clone(), spec);
                }
            }

            if let Ok(mut map) = pt_clone.lock() {
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
                    if let Ok(mut map) = s.confirm_specs.lock() {
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
                            o.ops.push(PendingOp::ResolveToolResult {
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

        smelt.set("api", api)?;

        smelt.set(
            "notify",
            push_op!(lua, shared, |msg: String| PendingOp::Notify(msg)),
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

        lua.globals().set("smelt", smelt)?;
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

    /// Clear the snapshot fields after dispatching.
    pub fn clear_context(&self) {
        if let Ok(mut o) = self.shared.ops.lock() {
            o.clear_context();
        }
    }

    /// Drain all pending ops queued by Lua handlers.
    pub fn drain_ops(&self) -> Vec<PendingOp> {
        let Ok(mut o) = self.shared.ops.lock() else {
            return Vec::new();
        };
        o.drain()
    }

    /// Fire the `on_response` callback for a completed `engine.ask()` call.
    /// Returns any queued ops produced by the callback.
    pub fn fire_callback(&self, id: u64, content: &str) -> Vec<PendingOp> {
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
                o.ops
                    .push(PendingOp::NotifyError(format!("ask callback: {e}")));
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
            o.ops.push(PendingOp::NotifyError(msg));
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
                defs.push(protocol::PluginToolDef {
                    name: name.clone(),
                    description,
                    parameters,
                    modes,
                });
            }
        }
        defs
    }

    /// Execute a plugin tool by calling the registered Lua handler.
    /// Returns `(content, is_error)`.
    pub fn execute_plugin_tool(
        &self,
        tool_name: &str,
        args: &std::collections::HashMap<String, serde_json::Value>,
    ) -> (String, bool) {
        let handlers = self
            .shared
            .plugin_tools
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(handle) = handlers.get(tool_name) {
            if let Ok(func) = self.lua.registry_value::<mlua::Function>(&handle.key) {
                let args_table = self.lua.create_table().unwrap();
                for (k, v) in args {
                    if let Ok(lua_val) = json_to_lua(&self.lua, v) {
                        let _ = args_table.set(k.as_str(), lua_val);
                    }
                }
                match func.call::<mlua::Value>(args_table) {
                    Ok(mlua::Value::String(s)) => (s.to_string_lossy().to_string(), false),
                    Ok(mlua::Value::Table(t)) => {
                        let content: String = t.get("content").unwrap_or_default();
                        let is_error: bool = t.get("is_error").unwrap_or(false);
                        (content, is_error)
                    }
                    Ok(_) => ("plugin tool returned non-string value".to_string(), true),
                    Err(e) => (format!("plugin tool error: {e}"), true),
                }
            } else {
                (format!("plugin tool handler not found: {tool_name}"), true)
            }
        } else {
            (format!("no plugin tool registered: {tool_name}"), true)
        }
    }

    /// Build a `PluginConfirmMeta` for the given tool (if it has a confirm spec).
    pub fn get_confirm_meta(&self, tool_name: &str) -> Option<crate::render::PluginConfirmMeta> {
        let specs = self
            .shared
            .confirm_specs
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let spec = specs.get(tool_name)?;
        Some(crate::render::PluginConfirmMeta {
            display_name: spec.message.clone(),
            action_label: spec.action_label.clone(),
            preview_field: spec.preview_field.clone(),
            accent_color: spec.accent_color,
            options: spec
                .options
                .iter()
                .map(|o| crate::render::PluginOptionMeta {
                    label: o.label.clone(),
                    approves: o.action == "approve",
                })
                .collect(),
        })
    }

    /// Invoke the on_select callback for the given plugin tool confirm option.
    pub fn invoke_confirm_callback(&self, tool_name: &str, option_index: usize) {
        let specs = self
            .shared
            .confirm_specs
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(spec) = specs.get(tool_name) {
            if let Some(opt) = spec.options.get(option_index) {
                if let Some(ref key) = opt.on_select_key {
                    if let Ok(func) = self.lua.registry_value::<mlua::Function>(key) {
                        if let Err(e) = func.call::<()>(()) {
                            if let Ok(mut o) = self.shared.ops.lock() {
                                o.ops.push(PendingOp::NotifyError(format!(
                                    "confirm callback error: {e}"
                                )));
                            }
                        }
                    }
                }
            }
        }
    }
}

fn ansi_color_from_lua(table: &mlua::Table, key: &str) -> Option<crossterm::style::Color> {
    let val: u8 = table.get(key).ok()?;
    Some(crossterm::style::Color::AnsiValue(val))
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
    let _ = lua;
    let mut map = serde_json::Map::new();
    for pair in table.pairs::<mlua::Value, mlua::Value>() {
        let Ok((key, val)) = pair else { continue };
        let key_str = match &key {
            mlua::Value::String(s) => s.to_string_lossy().to_string(),
            mlua::Value::Integer(i) => i.to_string(),
            _ => continue,
        };
        map.insert(key_str, lua_value_to_json(lua, &val));
    }
    serde_json::Value::Object(map)
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

impl Default for LuaRuntime {
    fn default() -> Self {
        Self::new()
    }
}

/// Modules embedded in the binary, available via `require("smelt.plugins.X")`.
const EMBEDDED_MODULES: &[(&str, &str)] = &[
    (
        "smelt.plugins.plan_mode",
        include_str!("../../../runtime/lua/smelt/plugins/plan_mode.lua"),
    ),
    (
        "smelt.plugins.btw",
        include_str!("../../../runtime/lua/smelt/plugins/btw.lua"),
    ),
    (
        "smelt.plugins.predict",
        include_str!("../../../runtime/lua/smelt/plugins/predict.lua"),
    ),
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
                PendingOp::Notify(msg) => Some(msg),
                _ => None,
            })
            .collect()
    }

    fn drain_errors(rt: &LuaRuntime) -> Vec<String> {
        rt.drain_ops()
            .into_iter()
            .filter_map(|op| match op {
                PendingOp::NotifyError(msg) => Some(msg),
                _ => None,
            })
            .collect()
    }

    fn drain_commands(rt: &LuaRuntime) -> Vec<String> {
        rt.drain_ops()
            .into_iter()
            .filter_map(|op| match op {
                PendingOp::RunCommand(line) => Some(line),
                _ => None,
            })
            .collect()
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
        assert!(matches!(&ops[0], PendingOp::SetMode(m) if m == "plan"));
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
        assert!(matches!(&ops[0], PendingOp::SetModel(m) if m == "gpt-4o"));
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
        assert!(matches!(&ops[0], PendingOp::Cancel));
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
        assert!(matches!(&ops[0], PendingOp::Submit(t) if t == "hello"));
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
        assert!(matches!(&ops[0], PendingOp::Compact(Some(s)) if s == "keep tests"));
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
