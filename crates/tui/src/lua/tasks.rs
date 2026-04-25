//! Task + callback lifecycle methods on `LuaRuntime`. Covers Lua
//! callbacks (`register_callback`, `invoke_callback`, `fire_callback`),
//! the parked-task resume API (`resolve_external` + `pump_task_events`),
//! the `LuaTaskRuntime` bridge (`drive_tasks`), and plugin-tool
//! execution (`plugin_tool_defs`, `execute_plugin_tool`).

use super::{LuaHandle, LuaRuntime, TaskCompletion, TaskDriveOutput, TaskEvent, ToolExecResult};
use mlua::prelude::*;
use std::sync::atomic::Ordering;
use std::time::Instant;

/// Per-call environment passed to a plugin tool's `execute` handler.
/// Mirrors the call-scoped fields of the Rust `ToolContext` and is
/// surfaced to Lua as the second argument of `execute(args, ctx)`.
pub struct PluginToolEnv<'a> {
    pub mode: protocol::Mode,
    pub session_id: &'a str,
    pub session_dir: &'a std::path::Path,
}

impl LuaRuntime {
    /// Fire the `on_response` callback for a completed `engine.ask()`
    /// call. Errors surface as `notify_error` toasts.
    pub fn fire_callback(&self, id: u64, content: &str) {
        let handle = {
            let Ok(mut cbs) = self.shared.callbacks.lock() else {
                return;
            };
            match cbs.remove(&id) {
                Some(h) => h,
                None => return,
            }
        };
        let Ok(func) = self.lua.registry_value::<mlua::Function>(&handle.key) else {
            return;
        };
        if let Err(e) = func.call::<()>(content.to_string()) {
            crate::lua::try_with_app(|app| app.notify_error(format!("ask callback: {e}")));
        }
    }

    pub fn remove_callback(&self, id: u64) {
        if let Ok(mut cbs) = self.shared.callbacks.lock() {
            cbs.remove(&id);
        }
    }

    /// Satisfy a `TaskWait::External(id)` from outside the runtime.
    pub fn resolve_external(&self, external_id: u64, value: mlua::Value) -> bool {
        let Ok(mut rt) = self.shared.tasks.lock() else {
            return false;
        };
        rt.resolve_external(external_id, value)
    }

    /// Resume a Lua coroutine that's parked on a `smelt.tools.call`
    /// side-call with the engine's `CoreToolResult`. Builds the
    /// `{ content, is_error, metadata }` table on the runtime's Lua
    /// context.
    pub fn resolve_core_tool_call(
        &self,
        request_id: u64,
        content: String,
        is_error: bool,
        metadata: Option<serde_json::Value>,
    ) {
        let table = match self.lua.create_table() {
            Ok(t) => t,
            Err(e) => {
                self.record_error(format!("tools.call result table: {e}"));
                return;
            }
        };
        if let Err(e) = table.set("content", content) {
            self.record_error(format!("tools.call result.content: {e}"));
            return;
        }
        if let Err(e) = table.set("is_error", is_error) {
            self.record_error(format!("tools.call result.is_error: {e}"));
            return;
        }
        if let Some(meta) = metadata {
            match super::json_to_lua(&self.lua, &meta) {
                Ok(v) => {
                    let _ = table.set("metadata", v);
                }
                Err(e) => self.record_error(format!("tools.call result.metadata: {e}")),
            }
        }
        self.resolve_external(request_id, mlua::Value::Table(table));
    }

    /// Drain the task-runtime inbox and apply each event.
    pub fn pump_task_events(&self) {
        let events: Vec<TaskEvent> = {
            let Ok(mut inbox) = self.shared.task_inbox.lock() else {
                return;
            };
            std::mem::take(&mut *inbox)
        };
        for ev in events {
            match ev {
                TaskEvent::ExternalResolved { external_id, value } => {
                    let v = self.lua.registry_value(&value).unwrap_or(mlua::Value::Nil);
                    self.resolve_external(external_id, v);
                }
            }
        }
    }

    /// Register a Lua callable under a fresh u64 id in
    /// `shared.callbacks`.
    pub fn register_callback(&self, func: mlua::Function) -> mlua::Result<u64> {
        let key = self.lua.create_registry_value(func)?;
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut cbs) = self.shared.callbacks.lock() {
            cbs.insert(id, LuaHandle { key });
        }
        Ok(id)
    }

    /// Invoke the Lua function registered under `handle.0` with a
    /// table derived from `payload`. The table shape is:
    /// - `Payload::None` → empty table.
    /// - `Payload::Key` → `{ code = "<KeyCode>", mods = "<KeyModifiers>" }`.
    /// - `Payload::Selection` → `{ index = <one-based usize> }`.
    /// - `Payload::Text` → `{ text = <string> }`.
    ///
    /// Adds `win` (the source WinId) and `panels` (a live snapshot of
    /// the dialog's panels) to the table.
    pub fn invoke_callback(
        &self,
        handle: ui::LuaHandle,
        win: ui::WinId,
        payload: &ui::Payload,
        panels: &[ui::PanelSnapshot],
    ) {
        if let Some((func, payload_table)) = self.prepare_invocation(handle, win, payload, panels) {
            if let Err(e) = func.call::<()>(payload_table) {
                self.record_error(format!("callback `{}`: {e}", handle.0));
            }
        }
    }

    /// Produce the `mlua::Function` + built payload table for a queued
    /// invocation, **without calling into Lua**. Splitting this out of
    /// [`Self::invoke_callback`] lets the host drain the queue in two
    /// phases: phase 1 prepares each invocation while `&LuaRuntime` is
    /// borrowed (mlua table construction needs it); phase 2 calls them
    /// after all Rust borrows on App have lapsed (so the Lua body can
    /// reach `&mut App` through [`crate::lua::with_app`]). Returns
    /// `None` when the handle is already dropped or when payload
    /// construction errored — both recorded as Lua errors.
    pub fn prepare_invocation(
        &self,
        handle: ui::LuaHandle,
        win: ui::WinId,
        payload: &ui::Payload,
        panels: &[ui::PanelSnapshot],
    ) -> Option<(mlua::Function, mlua::Table)> {
        let func = {
            let cbs = self.shared.callbacks.lock().ok()?;
            let h = cbs.get(&handle.0)?;
            self.lua.registry_value::<mlua::Function>(&h.key).ok()?
        };
        let payload_table = match self.lua.create_table() {
            Ok(t) => t,
            Err(e) => {
                self.record_error(format!("callback payload: {e}"));
                return None;
            }
        };
        if let Err(e) = populate_payload_table(&payload_table, payload) {
            self.record_error(format!("callback payload: {e}"));
            return None;
        }
        if let Err(e) = payload_table.set("win", win.0) {
            self.record_error(format!("callback payload: {e}"));
            return None;
        }
        match build_panels_table(&self.lua, panels) {
            Ok(t) => {
                if let Err(e) = payload_table.set("panels", t) {
                    self.record_error(format!("callback payload: {e}"));
                    return None;
                }
            }
            Err(e) => {
                self.record_error(format!("callback payload: {e}"));
                return None;
            }
        }
        Some((func, payload_table))
    }

    /// Record a callback error from outside the runtime's usual call
    /// path (e.g. a phase-2 invocation in `App::drain_lua_invocations`).
    pub fn record_callback_error(&self, handle_id: u64, err: impl std::fmt::Display) {
        self.record_error(format!("callback `{handle_id}`: {err}"));
    }

    /// Record a pending invocation from inside `ui.handle_key` /
    /// `ui.dispatch_event`. The ui dispatcher holds `&mut Ui` while the
    /// callback would fire, so firing Lua immediately would deny Lua
    /// bindings access to `&mut App` (they'd collide with the ui
    /// borrow). The host drains this queue right after the ui call
    /// returns and invokes each callback with the TLS app pointer
    /// installed, giving Lua bindings sole access to App state.
    pub fn queue_invocation(
        &self,
        handle: ui::LuaHandle,
        win: ui::WinId,
        payload: &ui::Payload,
        panels: &[ui::PanelSnapshot],
    ) {
        if let Ok(mut q) = self.shared.pending_invocations.lock() {
            q.push(crate::lua::PendingInvocation {
                handle,
                win,
                payload: payload.clone(),
                panels: panels.to_vec(),
            });
        }
    }

    /// Drain every queued callback invocation. Called by the host after
    /// `ui.handle_key` / `ui.dispatch_event` returns, under an
    /// [`crate::lua::install_app_ptr`] scope so each Lua body can reach
    /// `&mut App` through [`crate::lua::with_app`].
    pub fn drain_invocations(&self) -> Vec<crate::lua::PendingInvocation> {
        match self.shared.pending_invocations.lock() {
            Ok(mut q) => std::mem::take(&mut *q),
            Err(_) => Vec::new(),
        }
    }

    /// Drive the LuaTask runtime: resume any tasks whose waits have
    /// been satisfied, park any new yields, and return the outputs
    /// for the app to act on.
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

    /// Return protocol-level plugin tool definitions for registered
    /// tools. The TUI sends these with `StartTurn` so the engine
    /// includes them in LLM tool definitions.
    pub fn plugin_tool_defs(&self, _mode: protocol::Mode) -> Vec<protocol::PluginToolDef> {
        let handlers = self
            .shared
            .plugin_tools
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut defs = Vec::new();
        for name in handlers.keys() {
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
                let hooks = protocol::PluginToolHookFlags {
                    needs_confirm: meta_table.get("hook_needs_confirm").unwrap_or(false),
                    approval_patterns: meta_table.get("hook_approval_patterns").unwrap_or(false),
                    preflight: meta_table.get("hook_preflight").unwrap_or(false),
                };
                let override_core: bool = meta_table.get("override_core").unwrap_or(false);
                defs.push(protocol::PluginToolDef {
                    name: name.clone(),
                    description,
                    parameters,
                    modes,
                    execution_mode,
                    override_core,
                    hooks,
                });
            }
        }
        defs
    }

    /// Run the plugin tool's permission hooks for one invocation. Each
    /// hook is called synchronously and its result packaged into
    /// `PluginToolHooks`. Errors raised by a hook are recorded and the
    /// hook is treated as if it returned its no-op default (None /
    /// empty). The engine consumes the result via
    /// `UiCommand::PluginToolHooksResult`.
    pub fn evaluate_plugin_hooks(
        &self,
        tool_name: &str,
        args: &std::collections::HashMap<String, serde_json::Value>,
    ) -> protocol::PluginToolHooks {
        let mut out = protocol::PluginToolHooks::default();

        // Resolve hook functions inside the lock, then release before
        // calling — mlua functions are clonable Lua values, registry
        // keys are not.
        let (needs_confirm_fn, approval_patterns_fn, preflight_fn) = {
            let handlers = self
                .shared
                .plugin_tools
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let Some(h) = handlers.get(tool_name) else {
                return out;
            };
            let nc = h
                .needs_confirm
                .as_ref()
                .and_then(|h| self.lua.registry_value::<mlua::Function>(&h.key).ok());
            let ap = h
                .approval_patterns
                .as_ref()
                .and_then(|h| self.lua.registry_value::<mlua::Function>(&h.key).ok());
            let pf = h
                .preflight
                .as_ref()
                .and_then(|h| self.lua.registry_value::<mlua::Function>(&h.key).ok());
            (nc, ap, pf)
        };

        let args_table = match self.args_to_lua_table(args) {
            Ok(t) => t,
            Err(e) => {
                self.record_error(format!("plugin hooks: build args: {e}"));
                return out;
            }
        };

        if let Some(func) = needs_confirm_fn {
            match func.call::<Option<String>>(args_table.clone()) {
                Ok(s) => out.needs_confirm = s,
                Err(e) => self.record_error(format!("plugin hook needs_confirm: {e}")),
            }
        }
        if let Some(func) = approval_patterns_fn {
            match func.call::<Option<mlua::Table>>(args_table.clone()) {
                Ok(Some(t)) => {
                    out.approval_patterns = t
                        .sequence_values::<String>()
                        .filter_map(|r| r.ok())
                        .collect();
                }
                Ok(None) => {}
                Err(e) => self.record_error(format!("plugin hook approval_patterns: {e}")),
            }
        }
        if let Some(func) = preflight_fn {
            match func.call::<Option<String>>(args_table) {
                Ok(s) => out.preflight_error = s,
                Err(e) => self.record_error(format!("plugin hook preflight: {e}")),
            }
        }

        out
    }

    /// Build a Lua args table from a serde_json args map. Pulled out of
    /// `execute_plugin_tool` so `evaluate_plugin_hooks` shares the same
    /// conversion path.
    fn args_to_lua_table(
        &self,
        args: &std::collections::HashMap<String, serde_json::Value>,
    ) -> mlua::Result<mlua::Table> {
        let t = self.lua.create_table()?;
        for (k, v) in args {
            if let Ok(lua_val) = super::json_to_lua(&self.lua, v) {
                let _ = t.set(k.as_str(), lua_val);
            }
        }
        Ok(t)
    }

    /// Execute a plugin tool by spawning a `LuaTask` around the
    /// registered handler. Returns `Immediate` if the handler
    /// completes synchronously, `Pending` if it yields and will
    /// deliver later via `drive_tasks`.
    pub fn execute_plugin_tool(
        &self,
        tool_name: &str,
        args: &std::collections::HashMap<String, serde_json::Value>,
        request_id: u64,
        call_id: &str,
        env: PluginToolEnv<'_>,
    ) -> ToolExecResult {
        let PluginToolEnv {
            mode,
            session_id,
            session_dir,
        } = env;
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
            match self
                .lua
                .registry_value::<mlua::Function>(&handle.execute.key)
            {
                Ok(f) => f,
                Err(_) => {
                    return ToolExecResult::Immediate {
                        content: format!("plugin tool handler not found: {tool_name}"),
                        is_error: true,
                    };
                }
            }
        };

        let args_table = match self.args_to_lua_table(args) {
            Ok(t) => t,
            Err(e) => {
                return ToolExecResult::Immediate {
                    content: format!("plugin tool arg table: {e}"),
                    is_error: true,
                };
            }
        };

        // Per-call ctx table — equivalent to ToolContext for core tools.
        // call_id, mode, session_dir, session_id let the plugin tool
        // route output, scope filesystem operations, etc.
        let ctx_table = match build_plugin_ctx(&self.lua, call_id, mode, session_id, session_dir) {
            Ok(t) => t,
            Err(e) => {
                return ToolExecResult::Immediate {
                    content: format!("plugin tool ctx table: {e}"),
                    is_error: true,
                };
            }
        };

        let mut initial = mlua::MultiValue::new();
        initial.push_back(mlua::Value::Table(args_table));
        initial.push_back(mlua::Value::Table(ctx_table));

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
            initial,
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
                TaskDriveOutput::ToolComplete { .. } => {
                    // Orphaned completion (unmatched id) — swallow.
                }
                TaskDriveOutput::Error(msg) => self.record_error(msg),
            }
        }
        match immediate {
            Some((content, is_error)) => ToolExecResult::Immediate { content, is_error },
            None => ToolExecResult::Pending,
        }
    }
}

/// Fill a Lua table with fields from a `ui::Payload` for
/// `LuaRuntime::invoke_callback`.
fn populate_payload_table(table: &mlua::Table, payload: &ui::Payload) -> mlua::Result<()> {
    match payload {
        ui::Payload::None => Ok(()),
        ui::Payload::Key { code, mods } => {
            table.set("code", format!("{code:?}"))?;
            table.set("mods", format!("{mods:?}"))?;
            Ok(())
        }
        ui::Payload::Selection { index } => table.set("index", *index + 1),
        ui::Payload::Text { content } => table.set("text", content.clone()),
    }
}

/// Build the per-call ctx table passed as the second argument to
/// plugin tool `execute` handlers. Mirrors the shape of `ToolContext`
/// for core Rust tools but only exposes call-scoped data — fields
/// `event_tx`, `cancel`, `processes`, `file_locks` are reached via
/// dedicated `smelt.process.*` / `smelt.fs.*` primitives that resolve
/// the App state internally.
fn build_plugin_ctx(
    lua: &Lua,
    call_id: &str,
    mode: protocol::Mode,
    session_id: &str,
    session_dir: &std::path::Path,
) -> mlua::Result<mlua::Table> {
    let t = lua.create_table()?;
    t.set("call_id", call_id.to_string())?;
    t.set("mode", mode.as_str())?;
    t.set("session_id", session_id.to_string())?;
    t.set("session_dir", session_dir.to_string_lossy().into_owned())?;
    Ok(t)
}

/// Build a Lua sequence describing a dialog window's panels at
/// callback-fire time. Each entry is `{ selected = <1-based | nil>,
/// text = "…" }`. List panels have a 1-based selection; input panels
/// have non-empty text — plugins disambiguate by which field is
/// populated, since they configured the panel layout at open time.
fn build_panels_table(lua: &Lua, panels: &[ui::PanelSnapshot]) -> mlua::Result<mlua::Table> {
    let out = lua.create_table()?;
    for (i, p) in panels.iter().enumerate() {
        let entry = lua.create_table()?;
        if let Some(sel) = p.selected {
            entry.set("selected", sel + 1)?;
        }
        entry.set("text", p.text.clone())?;
        out.set(i + 1, entry)?;
    }
    Ok(out)
}
