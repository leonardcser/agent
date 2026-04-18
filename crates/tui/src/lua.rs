//! Lua bindings (Phase D). Wraps the `api::*` surface so users can
//! script smelt from `~/.config/smelt/init.lua`.
//!
//! Current scope (D1 bootstrap):
//! - Loads `~/.config/smelt/init.lua` at startup if present.
//! - Exposes `smelt.api.version` and `smelt.notify(msg)` as a first
//!   round-trip smoke test.
//! - Errors during load surface through `LuaRuntime::load_error`;
//!   callers convert to a user-visible notification.
//!
//! The remainder of D (D2 full `api::*` shim, D3 autocmds,
//! D4 user-command / keymap registration, D5 re-entrancy, D6 error
//! UX) builds on this scaffold — each slice lands without changing
//! the load path below.

use mlua::prelude::*;
use std::path::PathBuf;

/// User-scoped Lua state + any recorded startup error.
pub struct LuaRuntime {
    pub lua: Lua,
    pub load_error: Option<String>,
    /// Notifications queued by `smelt.notify` calls. Polled by the
    /// app each tick and forwarded to the Screen's notification band.
    pub pending_notifications: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl LuaRuntime {
    /// Build a fresh runtime, register the `smelt` global, and try to
    /// run `~/.config/smelt/init.lua`. Missing config files are not
    /// errors; syntax / runtime errors are captured on `load_error`.
    pub fn new() -> Self {
        let lua = Lua::new();
        let pending: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let load_error = Self::register_api(&lua, pending.clone())
            .err()
            .map(|e| e.to_string());

        let mut rt = Self {
            lua,
            load_error,
            pending_notifications: pending,
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

    fn register_api(
        lua: &Lua,
        pending: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) -> LuaResult<()> {
        let smelt = lua.create_table()?;

        let api = lua.create_table()?;
        api.set("version", crate::api::VERSION)?;
        smelt.set("api", api)?;

        // smelt.notify(msg) — queue a user-visible notification. The
        // app drains the queue each tick and forwards to the Screen.
        let pending_clone = pending.clone();
        let notify = lua.create_function(move |_, msg: String| {
            if let Ok(mut q) = pending_clone.lock() {
                q.push(msg);
            }
            Ok(())
        })?;
        smelt.set("notify", notify)?;

        lua.globals().set("smelt", smelt)?;
        Ok(())
    }

    fn load_init(&mut self, path: &std::path::Path) -> LuaResult<()> {
        let src = std::fs::read_to_string(path)
            .map_err(|e| LuaError::RuntimeError(format!("read init.lua: {e}")))?;
        self.lua.load(&src).set_name("init.lua").exec()
    }

    /// Drain any pending notifications queued from Lua callbacks.
    pub fn drain_notifications(&self) -> Vec<String> {
        let Ok(mut q) = self.pending_notifications.lock() else {
            return Vec::new();
        };
        std::mem::take(&mut *q)
    }
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
        let lua = Lua::new();
        let pending = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        LuaRuntime::register_api(&lua, pending.clone()).unwrap();
        let mut rt = LuaRuntime {
            lua,
            load_error: None,
            pending_notifications: pending,
        };
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        std::fs::write(tmp.path(), "this is not valid lua @@@").unwrap();
        let err = rt.load_init(tmp.path());
        assert!(err.is_err(), "expected syntax error");
    }
}
