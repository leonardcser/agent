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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AutocmdEvent {
    BlockDone,
    CmdPre,
    CmdPost,
    StreamStart,
    StreamEnd,
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
    fn lua_name(self) -> &'static str {
        match self {
            AutocmdEvent::BlockDone => "block_done",
            AutocmdEvent::CmdPre => "cmd_pre",
            AutocmdEvent::CmdPost => "cmd_post",
            AutocmdEvent::StreamStart => "stream_start",
            AutocmdEvent::StreamEnd => "stream_end",
        }
    }

    fn from_lua_name(s: &str) -> Option<Self> {
        match s {
            "block_done" => Some(Self::BlockDone),
            "cmd_pre" => Some(Self::CmdPre),
            "cmd_post" => Some(Self::CmdPost),
            "stream_start" => Some(Self::StreamStart),
            "stream_end" => Some(Self::StreamEnd),
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

/// Snapshot of app state populated before dispatching a Lua callback.
/// Lua functions can read from this to access transcript text, prompt
/// text, etc. without holding borrows on `App`.
type SharedContext = Arc<Mutex<LuaContext>>;

#[derive(Default)]
pub struct LuaContext {
    pub transcript_text: Option<String>,
    pub prompt_text: Option<String>,
    pub focused_window: Option<String>,
    pub vim_mode: Option<String>,
}

/// User-scoped Lua state + any recorded startup error.
pub struct LuaRuntime {
    pub lua: Lua,
    pub load_error: Option<String>,
    /// Notifications queued by `smelt.notify` calls. Polled by the
    /// app each tick and forwarded to the Screen's notification band.
    pub pending_notifications: SharedVec<String>,
    /// Errors raised by any Lua callback. The app surfaces the first
    /// one per tick as a notification; subsequent errors accumulate for
    /// `~/.cache/smelt/lua.log` persistence.
    pub lua_errors: SharedVec<String>,
    /// User commands registered from Lua, keyed by command name.
    commands: SharedMap<String, LuaHandle>,
    /// Key chord → handler mapping, keyed by `(mode, chord)`.
    /// An empty mode string `""` matches any mode.
    keymaps: SharedMap<(String, String), LuaHandle>,
    /// Autocmd handlers, keyed by event kind.
    autocmds: SharedMap<AutocmdEvent, Vec<LuaHandle>>,
    /// `smelt.defer(ms, fn)` timers. `Instant` is the due time; the
    /// tick loop fires handlers whose due time has passed.
    pending_timers: SharedVec<(Instant, LuaHandle)>,
    /// Command lines queued by `smelt.api.cmd.run` from inside Lua
    /// callbacks. Drained by the app loop and dispatched through
    /// `commands::run_command` — the re-entrancy queue that lets
    /// a Lua handler trigger a built-in without nesting borrows.
    pub pending_commands: SharedVec<String>,
    /// Shared context populated by the app before dispatching callbacks.
    context: SharedContext,
    /// Lua function registered via `smelt.statusline(fn)`. Called each
    /// tick to produce custom status items.
    statusline_provider: Arc<Mutex<Option<LuaHandle>>>,
}

impl LuaRuntime {
    /// Build a fresh runtime, register the `smelt` global, and try to
    /// run `~/.config/smelt/init.lua`. Missing config files are not
    /// errors; syntax / runtime errors are captured on `load_error`.
    pub fn new() -> Self {
        let lua = Lua::new();
        let pending: SharedVec<String> = Arc::new(Mutex::new(Vec::new()));
        let errors: SharedVec<String> = Arc::new(Mutex::new(Vec::new()));
        let commands: SharedMap<String, LuaHandle> = Arc::new(Mutex::new(HashMap::new()));
        let keymaps: SharedMap<(String, String), LuaHandle> = Arc::new(Mutex::new(HashMap::new()));
        let autocmds: SharedMap<AutocmdEvent, Vec<LuaHandle>> =
            Arc::new(Mutex::new(HashMap::new()));
        let pending_timers: SharedVec<(Instant, LuaHandle)> = Arc::new(Mutex::new(Vec::new()));
        let pending_commands: SharedVec<String> = Arc::new(Mutex::new(Vec::new()));
        let statusline_provider: Arc<Mutex<Option<LuaHandle>>> = Arc::new(Mutex::new(None));

        let context: SharedContext = Arc::new(Mutex::new(LuaContext::default()));

        let load_error = Self::register_api(
            &lua,
            pending.clone(),
            commands.clone(),
            keymaps.clone(),
            autocmds.clone(),
            pending_timers.clone(),
            pending_commands.clone(),
            statusline_provider.clone(),
            context.clone(),
        )
        .err()
        .map(|e| e.to_string());

        let mut rt = Self {
            lua,
            load_error,
            pending_notifications: pending,
            lua_errors: errors,
            commands,
            keymaps,
            autocmds,
            pending_timers,
            pending_commands,
            context,
            statusline_provider,
        };

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
        pending: SharedVec<String>,
        commands: SharedMap<String, LuaHandle>,
        keymaps: SharedMap<(String, String), LuaHandle>,
        autocmds: SharedMap<AutocmdEvent, Vec<LuaHandle>>,
        pending_timers: SharedVec<(Instant, LuaHandle)>,
        pending_commands: SharedVec<String>,
        statusline_provider: Arc<Mutex<Option<LuaHandle>>>,
        context: SharedContext,
    ) -> LuaResult<()> {
        let smelt = lua.create_table()?;

        let api = lua.create_table()?;
        api.set("version", crate::api::VERSION)?;

        // smelt.api.transcript.text() — read the full transcript text.
        let transcript_tbl = lua.create_table()?;
        let ctx_clone = context.clone();
        let transcript_text = lua.create_function(move |_, ()| {
            let ctx = ctx_clone
                .lock()
                .map_err(|e| LuaError::RuntimeError(e.to_string()))?;
            Ok(ctx.transcript_text.clone().unwrap_or_default())
        })?;
        transcript_tbl.set("text", transcript_text)?;
        api.set("transcript", transcript_tbl)?;

        // smelt.api.buf.text() — read the prompt buffer text.
        let buf_tbl = lua.create_table()?;
        let ctx_clone = context.clone();
        let buf_text = lua.create_function(move |_, ()| {
            let ctx = ctx_clone
                .lock()
                .map_err(|e| LuaError::RuntimeError(e.to_string()))?;
            Ok(ctx.prompt_text.clone().unwrap_or_default())
        })?;
        buf_tbl.set("text", buf_text)?;
        api.set("buf", buf_tbl)?;

        // smelt.api.win.focus() / smelt.api.win.mode()
        let win_tbl = lua.create_table()?;
        let ctx_clone = context.clone();
        let win_focus = lua.create_function(move |_, ()| {
            let ctx = ctx_clone
                .lock()
                .map_err(|e| LuaError::RuntimeError(e.to_string()))?;
            Ok(ctx.focused_window.clone().unwrap_or_default())
        })?;
        win_tbl.set("focus", win_focus)?;
        let ctx_clone = context.clone();
        let win_mode = lua.create_function(move |_, ()| {
            let ctx = ctx_clone
                .lock()
                .map_err(|e| LuaError::RuntimeError(e.to_string()))?;
            Ok(ctx.vim_mode.clone().unwrap_or_default())
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

        // smelt.api.cmd.run(line) — queue a command line for the app
        // loop to dispatch. Running it inline would nest App borrows;
        // the queue drains on the next tick.
        let pending_commands_clone = pending_commands.clone();
        let cmd_run = lua.create_function(move |_, line: String| {
            if let Ok(mut q) = pending_commands_clone.lock() {
                q.push(line);
            }
            Ok(())
        })?;
        cmd_tbl.set("run", cmd_run)?;

        api.set("cmd", cmd_tbl)?;

        smelt.set("api", api)?;

        // smelt.notify(msg) — queue a user-visible notification.
        let pending_clone = pending.clone();
        let notify = lua.create_function(move |_, msg: String| {
            if let Ok(mut q) = pending_clone.lock() {
                q.push(msg);
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

    /// Populate the shared context before dispatching a Lua callback.
    /// The app calls this to make transcript/prompt text available to
    /// Lua functions like `smelt.api.transcript.text()`.
    pub fn set_context(&self, ctx: LuaContext) {
        if let Ok(mut c) = self.context.lock() {
            *c = ctx;
        }
    }

    /// Clear the shared context after dispatching.
    pub fn clear_context(&self) {
        if let Ok(mut c) = self.context.lock() {
            *c = LuaContext::default();
        }
    }

    /// Drain any pending notifications queued from Lua callbacks.
    pub fn drain_notifications(&self) -> Vec<String> {
        let Ok(mut q) = self.pending_notifications.lock() else {
            return Vec::new();
        };
        std::mem::take(&mut *q)
    }

    /// Drain command lines queued by `smelt.api.cmd.run`. The app loop
    /// dispatches each line through `commands::run_command` after the
    /// current handler returns, avoiding nested `&mut App` borrows.
    pub fn drain_pending_commands(&self) -> Vec<String> {
        let Ok(mut q) = self.pending_commands.lock() else {
            return Vec::new();
        };
        std::mem::take(&mut *q)
    }

    /// Drain any errors recorded while dispatching Lua callbacks.
    pub fn drain_errors(&self) -> Vec<String> {
        let Ok(mut q) = self.lua_errors.lock() else {
            return Vec::new();
        };
        std::mem::take(&mut *q)
    }

    /// Invoke a registered user command by name. Returns `true` when
    /// the command exists and was dispatched (regardless of whether
    /// the handler succeeded); `false` when the name isn't bound.
    pub fn run_command(&self, name: &str, arg: Option<String>) -> bool {
        let Ok(map) = self.commands.lock() else {
            return false;
        };
        let Some(handle) = map.get(name) else {
            return false;
        };
        if handle.dead {
            return false;
        }
        let Ok(func) = self.lua.registry_value::<mlua::Function>(&handle.key) else {
            return false;
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
        let Ok(map) = self.keymaps.lock() else {
            return false;
        };
        // Try mode-specific match first, then wildcard ""
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
        let Ok(func) = self.lua.registry_value::<mlua::Function>(&handle.key) else {
            return false;
        };
        if let Err(e) = func.call::<()>(()) {
            self.record_error(format!("keymap `{chord}`: {e}"));
        }
        true
    }

    /// Fire all handlers registered for `event`. Errors are captured
    /// per-handler and don't stop subsequent handlers.
    pub fn emit(&self, event: AutocmdEvent) {
        let Ok(map) = self.autocmds.lock() else {
            return;
        };
        let Some(list) = map.get(&event) else {
            return;
        };
        for handle in list {
            if handle.dead {
                continue;
            }
            let Ok(func) = self.lua.registry_value::<mlua::Function>(&handle.key) else {
                continue;
            };
            if let Err(e) = func.call::<()>(event.lua_name()) {
                self.record_error(format!("autocmd `{}`: {e}", event.lua_name()));
            }
        }
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
        if let Ok(mut q) = self.lua_errors.lock() {
            q.push(msg);
        }
    }

    /// Whether a user command with `name` is registered via Lua.
    pub fn has_command(&self, name: &str) -> bool {
        self.commands
            .lock()
            .map(|m| m.contains_key(name))
            .unwrap_or(false)
    }

    /// Names of all Lua-registered user commands (for completion).
    pub fn command_names(&self) -> Vec<String> {
        self.commands
            .lock()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    }
}

fn ansi_color_from_lua(table: &mlua::Table, key: &str) -> Option<crossterm::style::Color> {
    let val: u8 = table.get(key).ok()?;
    Some(crossterm::style::Color::AnsiValue(val))
}

impl Default for LuaRuntime {
    fn default() -> Self {
        Self::new()
    }
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
        let msgs = rt.drain_notifications();
        assert_eq!(msgs, vec!["hello from lua".to_string()]);
        assert!(rt.drain_notifications().is_empty());
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
        assert_eq!(rt.drain_notifications(), vec!["hello world".to_string()]);
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
        // Mode "n" matches Normal
        assert!(rt.run_keymap("<C-g>", Some("Normal")));
        assert_eq!(rt.drain_notifications(), vec!["ctrl-g".to_string()]);
        // Wrong mode doesn't match
        assert!(!rt.run_keymap("<C-g>", Some("Insert")));
        // Unregistered chord
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
        assert_eq!(rt.drain_notifications(), vec!["any-mode".to_string()]);
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
            rt.drain_notifications(),
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
        // Even with 0ms, the handler hasn't run yet — tick_timers triggers it.
        assert!(rt.drain_notifications().is_empty());
        std::thread::sleep(std::time::Duration::from_millis(2));
        rt.tick_timers();
        assert_eq!(rt.drain_notifications(), vec!["deferred".to_string()]);
    }

    #[test]
    fn cmd_run_queues_for_dispatch() {
        let rt = LuaRuntime::new();
        rt.lua
            .load(r#"smelt.api.cmd.run("/compact")"#)
            .exec()
            .expect("exec");
        let queued = rt.drain_pending_commands();
        assert_eq!(queued, vec!["/compact".to_string()]);
        assert!(rt.drain_pending_commands().is_empty());
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
        let errs = rt.drain_errors();
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("broken"), "err: {}", errs[0]);
    }

    #[test]
    fn transcript_text_reads_context() {
        let rt = LuaRuntime::new();
        rt.set_context(LuaContext {
            transcript_text: Some("hello world".to_string()),
            ..Default::default()
        });
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
        rt.set_context(LuaContext {
            prompt_text: Some("prompt content".to_string()),
            ..Default::default()
        });
        let text: String = rt
            .lua
            .load("return smelt.api.buf.text()")
            .eval()
            .expect("eval");
        assert_eq!(text, "prompt content");
    }
}
