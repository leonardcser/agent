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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

type SharedVec<T> = Arc<Mutex<Vec<T>>>;
type SharedMap<K, V> = Arc<Mutex<HashMap<K, V>>>;

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
struct LuaHandle {
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

type SharedOps = Arc<Mutex<LuaOps>>;

/// User-scoped Lua state + any recorded startup error.
pub struct LuaRuntime {
    pub lua: Lua,
    pub load_error: Option<String>,
    /// Commands registered from Lua, keyed by command name.
    commands: SharedMap<String, LuaHandle>,
    /// Key chord → handler mapping, keyed by `(mode, chord)`.
    /// An empty mode string `""` matches any mode.
    keymaps: SharedMap<(String, String), LuaHandle>,
    /// Autocmd handlers, keyed by event kind.
    autocmds: SharedMap<AutocmdEvent, Vec<LuaHandle>>,
    /// `smelt.defer(ms, fn)` timers. `Instant` is the due time; the
    /// tick loop fires handlers whose due time has passed.
    pending_timers: SharedVec<(Instant, LuaHandle)>,
    /// Unified read/write bridge between Lua handlers and the app loop.
    ops: SharedOps,
    /// Lua function registered via `smelt.statusline(fn)`. Called each
    /// tick to produce custom status items.
    statusline_provider: Arc<Mutex<Option<LuaHandle>>>,
    /// Plugin tool handlers, keyed by tool name.
    plugin_tools: SharedMap<String, LuaHandle>,
    /// Confirm specs for plugin tools that need user confirmation.
    plugin_confirm_specs: Arc<Mutex<HashMap<String, PluginConfirmSpec>>>,
    /// Callbacks for `engine.ask()` responses, keyed by request ID.
    ask_callbacks: SharedMap<u64, LuaHandle>,
    /// Monotonic counter for background ask request IDs.
    ask_next_id: Arc<std::sync::atomic::AtomicU64>,
}

impl LuaRuntime {
    /// Build a fresh runtime, register the `smelt` global, and try to
    /// run `~/.config/smelt/init.lua`. Missing config files are not
    /// errors; syntax / runtime errors are captured on `load_error`.
    pub fn new() -> Self {
        let lua = Lua::new();
        let commands: SharedMap<String, LuaHandle> = Arc::new(Mutex::new(HashMap::new()));
        let keymaps: SharedMap<(String, String), LuaHandle> = Arc::new(Mutex::new(HashMap::new()));
        let autocmds: SharedMap<AutocmdEvent, Vec<LuaHandle>> =
            Arc::new(Mutex::new(HashMap::new()));
        let pending_timers: SharedVec<(Instant, LuaHandle)> = Arc::new(Mutex::new(Vec::new()));
        let ops: SharedOps = Arc::new(Mutex::new(LuaOps::default()));
        let statusline_provider: Arc<Mutex<Option<LuaHandle>>> = Arc::new(Mutex::new(None));
        let plugin_tools: SharedMap<String, LuaHandle> = Arc::new(Mutex::new(HashMap::new()));
        let plugin_confirm_specs: Arc<Mutex<HashMap<String, PluginConfirmSpec>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let ask_callbacks: SharedMap<u64, LuaHandle> = Arc::new(Mutex::new(HashMap::new()));
        let ask_next_id = Arc::new(std::sync::atomic::AtomicU64::new(1));

        let load_error = Self::register_api(
            &lua,
            commands.clone(),
            keymaps.clone(),
            autocmds.clone(),
            pending_timers.clone(),
            ops.clone(),
            statusline_provider.clone(),
            plugin_tools.clone(),
            plugin_confirm_specs.clone(),
            ask_callbacks.clone(),
            ask_next_id.clone(),
        )
        .err()
        .map(|e| e.to_string());

        let mut rt = Self {
            lua,
            load_error,
            commands,
            keymaps,
            autocmds,
            pending_timers,
            ops,
            statusline_provider,
            plugin_tools,
            plugin_confirm_specs,
            ask_callbacks,
            ask_next_id,
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

    #[allow(clippy::too_many_arguments)]
    fn register_api(
        lua: &Lua,
        commands: SharedMap<String, LuaHandle>,
        keymaps: SharedMap<(String, String), LuaHandle>,
        autocmds: SharedMap<AutocmdEvent, Vec<LuaHandle>>,
        pending_timers: SharedVec<(Instant, LuaHandle)>,
        ops: SharedOps,
        statusline_provider: Arc<Mutex<Option<LuaHandle>>>,
        plugin_tools: SharedMap<String, LuaHandle>,
        plugin_confirm_specs: Arc<Mutex<HashMap<String, PluginConfirmSpec>>>,
        ask_callbacks: SharedMap<u64, LuaHandle>,
        ask_next_id: Arc<std::sync::atomic::AtomicU64>,
    ) -> LuaResult<()> {
        let smelt = lua.create_table()?;

        let api = lua.create_table()?;
        api.set("version", crate::api::VERSION)?;

        // smelt.api.transcript.text()
        let transcript_tbl = lua.create_table()?;
        let ops_clone = ops.clone();
        let transcript_text = lua.create_function(move |_, ()| {
            let o = ops_clone
                .lock()
                .map_err(|e| LuaError::RuntimeError(e.to_string()))?;
            Ok(o.transcript_text.clone().unwrap_or_default())
        })?;
        transcript_tbl.set("text", transcript_text)?;
        api.set("transcript", transcript_tbl)?;

        // smelt.api.buf.text()
        let buf_tbl = lua.create_table()?;
        let ops_clone = ops.clone();
        let buf_text = lua.create_function(move |_, ()| {
            let o = ops_clone
                .lock()
                .map_err(|e| LuaError::RuntimeError(e.to_string()))?;
            Ok(o.prompt_text.clone().unwrap_or_default())
        })?;
        buf_tbl.set("text", buf_text)?;
        api.set("buf", buf_tbl)?;

        // smelt.api.win.focus() / smelt.api.win.mode()
        let win_tbl = lua.create_table()?;
        let ops_clone = ops.clone();
        let win_focus = lua.create_function(move |_, ()| {
            let o = ops_clone
                .lock()
                .map_err(|e| LuaError::RuntimeError(e.to_string()))?;
            Ok(o.focused_window.clone().unwrap_or_default())
        })?;
        win_tbl.set("focus", win_focus)?;
        let ops_clone = ops.clone();
        let win_mode = lua.create_function(move |_, ()| {
            let o = ops_clone
                .lock()
                .map_err(|e| LuaError::RuntimeError(e.to_string()))?;
            Ok(o.vim_mode.clone().unwrap_or_default())
        })?;
        win_tbl.set("mode", win_mode)?;
        api.set("win", win_tbl)?;

        // smelt.api.cmd.register(name, fn)
        let cmd_tbl = lua.create_table()?;
        let commands_clone = commands.clone();
        let cmd_register =
            lua.create_function(move |lua, (name, handler): (String, mlua::Function)| {
                let key = lua.create_registry_value(handler)?;
                if let Ok(mut map) = commands_clone.lock() {
                    map.insert(name, LuaHandle { key, dead: false });
                }
                Ok(())
            })?;
        cmd_tbl.set("register", cmd_register)?;

        // smelt.api.cmd.run(line)
        let ops_clone = ops.clone();
        let cmd_run = lua.create_function(move |_, line: String| {
            if let Ok(mut o) = ops_clone.lock() {
                o.ops.push(PendingOp::RunCommand(line));
            }
            Ok(())
        })?;
        cmd_tbl.set("run", cmd_run)?;

        // smelt.api.cmd.list()
        let commands_list = commands.clone();
        let cmd_list = lua.create_function(move |lua, ()| {
            let names: Vec<String> = commands_list
                .lock()
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default();
            let table = lua.create_table()?;
            for (i, name) in names.iter().enumerate() {
                table.set(i + 1, name.as_str())?;
            }
            Ok(table)
        })?;
        cmd_tbl.set("list", cmd_list)?;

        api.set("cmd", cmd_tbl)?;

        // smelt.api.engine.*
        let engine_tbl = lua.create_table()?;

        // Helper: create a read-only engine accessor that locks ops and
        // extracts a field from the EngineSnapshot.
        macro_rules! engine_read {
            ($lua:expr, $ops:expr, $field:ident) => {{
                let o = $ops.clone();
                $lua.create_function(move |_, ()| {
                    let o = o
                        .lock()
                        .map_err(|e| LuaError::RuntimeError(e.to_string()))?;
                    Ok(o.engine.$field.clone())
                })?
            }};
        }

        macro_rules! engine_op {
            ($lua:expr, $ops:expr, $variant:ident, $ty:ty) => {{
                let o = $ops.clone();
                $lua.create_function(move |_, val: $ty| {
                    if let Ok(mut o) = o.lock() {
                        o.ops.push(PendingOp::$variant(val));
                    }
                    Ok(())
                })?
            }};
        }

        engine_tbl.set("model", engine_read!(lua, ops, model))?;
        engine_tbl.set("mode", engine_read!(lua, ops, mode))?;
        engine_tbl.set("reasoning_effort", engine_read!(lua, ops, reasoning_effort))?;
        engine_tbl.set("is_busy", engine_read!(lua, ops, is_busy))?;
        engine_tbl.set("cost", engine_read!(lua, ops, session_cost))?;
        engine_tbl.set("context_tokens", engine_read!(lua, ops, context_tokens))?;
        engine_tbl.set("context_window", engine_read!(lua, ops, context_window))?;
        engine_tbl.set("session_dir", engine_read!(lua, ops, session_dir))?;
        engine_tbl.set("session_id", engine_read!(lua, ops, session_id))?;

        engine_tbl.set("set_model", engine_op!(lua, ops, SetModel, String))?;
        engine_tbl.set("set_mode", engine_op!(lua, ops, SetMode, String))?;
        engine_tbl.set(
            "set_reasoning_effort",
            engine_op!(lua, ops, SetReasoningEffort, String),
        )?;
        engine_tbl.set("submit", engine_op!(lua, ops, Submit, String))?;

        let ops_clone = ops.clone();
        engine_tbl.set(
            "cancel",
            lua.create_function(move |_, ()| {
                if let Ok(mut o) = ops_clone.lock() {
                    o.ops.push(PendingOp::Cancel);
                }
                Ok(())
            })?,
        )?;

        let ops_clone = ops.clone();
        engine_tbl.set(
            "compact",
            lua.create_function(move |_, instructions: Option<String>| {
                if let Ok(mut o) = ops_clone.lock() {
                    o.ops.push(PendingOp::Compact(instructions));
                }
                Ok(())
            })?,
        )?;

        // smelt.api.engine.ask({ system, messages?, question?, task?, on_response })
        {
            let ops_clone = ops.clone();
            let cb_clone = ask_callbacks.clone();
            let id_counter = ask_next_id.clone();
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
                                "user" => protocol::Message::user(protocol::Content::text(&content)),
                                "assistant" => protocol::Message::assistant(Some(content), None),
                                _ => continue,
                            };
                            messages.push(msg);
                        }
                    }
                    if let Ok(question) = spec.get::<String>("question") {
                        messages.push(protocol::Message::user(protocol::Content::text(&question)));
                    }

                    let id = id_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                    if let Some(func) = on_response {
                        let key = lua.create_registry_value(func)?;
                        if let Ok(mut cbs) = cb_clone.lock() {
                            cbs.insert(id, LuaHandle { key, dead: false });
                        }
                    }

                    if let Ok(mut o) = ops_clone.lock() {
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

        api.set("engine", engine_tbl)?;

        // smelt.api.ui — ghost text + notifications
        let ui_tbl = lua.create_table()?;
        {
            let ops_clone = ops.clone();
            ui_tbl.set(
                "set_ghost_text",
                lua.create_function(move |_, text: String| {
                    if let Ok(mut o) = ops_clone.lock() {
                        o.ops.push(PendingOp::SetGhostText(text));
                    }
                    Ok(())
                })?,
            )?;
        }
        {
            let ops_clone = ops.clone();
            ui_tbl.set(
                "clear_ghost_text",
                lua.create_function(move |_, ()| {
                    if let Ok(mut o) = ops_clone.lock() {
                        o.ops.push(PendingOp::ClearGhostText);
                    }
                    Ok(())
                })?,
            )?;
        }
        {
            let ops_clone = ops.clone();
            ui_tbl.set(
                "notify",
                lua.create_function(move |_, msg: String| {
                    if let Ok(mut o) = ops_clone.lock() {
                        o.ops.push(PendingOp::Notify(msg));
                    }
                    Ok(())
                })?,
            )?;
        }
        {
            let ops_clone = ops.clone();
            ui_tbl.set(
                "notify_error",
                lua.create_function(move |_, msg: String| {
                    if let Ok(mut o) = ops_clone.lock() {
                        o.ops.push(PendingOp::NotifyError(msg));
                    }
                    Ok(())
                })?,
            )?;
        }
        api.set("ui", ui_tbl)?;

        // smelt.api.prompt.set_section(name, content) / remove_section(name)
        let prompt_tbl = lua.create_table()?;
        let ops_clone = ops.clone();
        let set_section = lua.create_function(move |_, (name, content): (String, String)| {
            if let Ok(mut o) = ops_clone.lock() {
                o.ops.push(PendingOp::SetPromptSection(name, content));
            }
            Ok(())
        })?;
        prompt_tbl.set("set_section", set_section)?;
        let ops_clone = ops.clone();
        let remove_section = lua.create_function(move |_, name: String| {
            if let Ok(mut o) = ops_clone.lock() {
                o.ops.push(PendingOp::RemovePromptSection(name));
            }
            Ok(())
        })?;
        prompt_tbl.set("remove_section", remove_section)?;
        api.set("prompt", prompt_tbl)?;

        // smelt.api.tools.register(def) / unregister(name)
        let tools_tbl = lua.create_table()?;
        let pt_clone = plugin_tools.clone();
        let pcs_clone = plugin_confirm_specs.clone();
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
        let pt_clone = plugin_tools.clone();
        let pcs_clone2 = plugin_confirm_specs.clone();
        let tools_unregister = lua.create_function(move |_, name: String| {
            if let Ok(mut map) = pt_clone.lock() {
                map.remove(&name);
            }
            if let Ok(mut map) = pcs_clone2.lock() {
                map.remove(&name);
            }
            Ok(())
        })?;
        tools_tbl.set("unregister", tools_unregister)?;
        api.set("tools", tools_tbl)?;

        smelt.set("api", api)?;

        // smelt.notify(msg)
        let ops_clone = ops.clone();
        let notify = lua.create_function(move |_, msg: String| {
            if let Ok(mut o) = ops_clone.lock() {
                o.ops.push(PendingOp::Notify(msg));
            }
            Ok(())
        })?;
        smelt.set("notify", notify)?;

        // smelt.clipboard(text) — copy text to system clipboard.
        let clipboard_fn = lua.create_function(|_, text: String| {
            crate::app::commands::copy_to_clipboard(&text).map_err(LuaError::RuntimeError)?;
            Ok(())
        })?;
        smelt.set("clipboard", clipboard_fn)?;

        let keymaps_clone = keymaps.clone();
        let keymap_fn = lua.create_function(
            move |lua, (mode, chord, handler): (String, String, mlua::Function)| {
                let key = lua.create_registry_value(handler)?;
                if let Ok(mut map) = keymaps_clone.lock() {
                    map.insert((mode, chord), LuaHandle { key, dead: false });
                }
                Ok(())
            },
        )?;
        smelt.set("keymap", keymap_fn)?;

        // smelt.on(event, fn) — register an autocmd handler.
        let autocmds_clone = autocmds.clone();
        let on_fn =
            lua.create_function(move |lua, (event, handler): (String, mlua::Function)| {
                let Some(kind) = AutocmdEvent::from_lua_name(&event) else {
                    return Err(LuaError::RuntimeError(format!("unknown event: {event}")));
                };
                let key = lua.create_registry_value(handler)?;
                if let Ok(mut map) = autocmds_clone.lock() {
                    map.entry(kind)
                        .or_default()
                        .push(LuaHandle { key, dead: false });
                }
                Ok(())
            })?;
        smelt.set("on", on_fn)?;

        // smelt.defer(ms, fn) — schedule a one-shot timer.
        let timers_clone = pending_timers.clone();
        let defer_fn = lua.create_function(move |lua, (ms, handler): (u64, mlua::Function)| {
            let key = lua.create_registry_value(handler)?;
            if let Ok(mut q) = timers_clone.lock() {
                q.push((
                    Instant::now() + Duration::from_millis(ms),
                    LuaHandle { key, dead: false },
                ));
            }
            Ok(())
        })?;
        smelt.set("defer", defer_fn)?;

        // smelt.statusline(fn) — register a status line provider.
        let sl_clone = statusline_provider.clone();
        let statusline_fn = lua.create_function(move |lua, handler: mlua::Function| {
            let key = lua.create_registry_value(handler)?;
            if let Ok(mut slot) = sl_clone.lock() {
                *slot = Some(LuaHandle { key, dead: false });
            }
            Ok(())
        })?;
        smelt.set("statusline", statusline_fn)?;

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
        if let Ok(mut o) = self.ops.lock() {
            o.set_context(transcript_text, prompt_text, focused_window, vim_mode);
        }
    }

    /// Populate the engine snapshot fields. Called once at startup and
    /// whenever the engine state changes (mode, model, cost, tokens).
    pub fn set_engine_context(&self, snap: EngineSnapshot) {
        if let Ok(mut o) = self.ops.lock() {
            o.engine = snap;
        }
    }

    /// Clear the snapshot fields after dispatching.
    pub fn clear_context(&self) {
        if let Ok(mut o) = self.ops.lock() {
            o.clear_context();
        }
    }

    /// Drain all pending ops queued by Lua handlers.
    pub fn drain_ops(&self) -> Vec<PendingOp> {
        let Ok(mut o) = self.ops.lock() else {
            return Vec::new();
        };
        o.drain()
    }

    /// Fire the `on_response` callback for a completed `engine.ask()` call.
    /// Returns any queued ops produced by the callback.
    pub fn fire_ask_callback(&self, id: u64, content: &str) -> Vec<PendingOp> {
        let handle = {
            let Ok(mut cbs) = self.ask_callbacks.lock() else {
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
            if let Ok(mut o) = self.ops.lock() {
                o.ops
                    .push(PendingOp::NotifyError(format!("ask callback: {e}")));
            }
        }
        self.drain_ops()
    }

    /// Invoke a registered command by name. Returns `true` when the
    /// command exists and was dispatched (regardless of whether the
    /// handler succeeded); `false` when the name isn't bound.
    pub fn run_command(&self, name: &str, arg: Option<String>) -> bool {
        let func = {
            let Ok(map) = self.commands.lock() else {
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
            let Ok(map) = self.keymaps.lock() else {
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
        let Ok(map) = self.autocmds.lock() else {
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
        let due: Vec<LuaHandle> = match self.pending_timers.lock() {
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
        let handle = self.statusline_provider.lock().ok()?;
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
        if let Ok(mut o) = self.ops.lock() {
            o.ops.push(PendingOp::NotifyError(msg));
        }
    }

    /// Whether a command with `name` is registered via Lua.
    pub fn has_command(&self, name: &str) -> bool {
        self.commands
            .lock()
            .map(|m| m.contains_key(name))
            .unwrap_or(false)
    }

    /// Names of all Lua-registered commands (for completion).
    pub fn command_names(&self) -> Vec<String> {
        self.commands
            .lock()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Return protocol-level plugin tool definitions for registered tools.
    /// The TUI sends these with `StartTurn` so the engine includes them in
    /// LLM tool definitions.
    pub fn plugin_tool_defs(&self, _mode: protocol::Mode) -> Vec<protocol::PluginToolDef> {
        let handlers = self.plugin_tools.lock().unwrap_or_else(|e| e.into_inner());
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
        let handlers = self.plugin_tools.lock().unwrap_or_else(|e| e.into_inner());
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
            .plugin_confirm_specs
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
            .plugin_confirm_specs
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(spec) = specs.get(tool_name) {
            if let Some(opt) = spec.options.get(option_index) {
                if let Some(ref key) = opt.on_select_key {
                    if let Ok(func) = self.lua.registry_value::<mlua::Function>(key) {
                        if let Err(e) = func.call::<()>(()) {
                            if let Ok(mut o) = self.ops.lock() {
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
const EMBEDDED_MODULES: &[(&str, &str)] = &[(
    "smelt.plugins.plan_mode",
    include_str!("../../../runtime/lua/smelt/plugins/plan_mode.lua"),
)];

/// Register a custom Lua package searcher that resolves `require("smelt.…")`
/// from modules embedded in the binary. Falls back to the default searchers
/// for anything not in `EMBEDDED_MODULES`, so user files on disk win when
/// they shadow an embedded module (the user searcher runs first).
fn register_embedded_searcher(lua: &Lua) -> LuaResult<()> {
    let searcher = lua.create_function(|lua, module: String| {
        for &(name, source) in EMBEDDED_MODULES {
            if name == module {
                let loader = lua
                    .load(source)
                    .set_name(name)
                    .into_function()?;
                return Ok(mlua::Value::Function(loader));
            }
        }
        Ok(mlua::Value::String(
            lua.create_string(format!("\n\tno embedded module '{module}'"))?,
        ))
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
