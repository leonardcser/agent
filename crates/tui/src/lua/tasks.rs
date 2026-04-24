//! Task + callback lifecycle methods on `LuaRuntime`. Covers Lua
//! callbacks (`register_callback`, `invoke_callback`, `fire_callback`),
//! the parked-task resume API (`resolve_external` + `pump_task_events`),
//! the `LuaTaskRuntime` bridge (`drive_tasks`), and plugin-tool
//! execution (`plugin_tool_defs`, `execute_plugin_tool`).

use super::{
    AppOp, LuaHandle, LuaRuntime, TaskCompletion, TaskDriveOutput, TaskEvent, ToolExecResult, UiOp,
};
use mlua::prelude::*;
use std::sync::atomic::Ordering;
use std::time::Instant;

impl LuaRuntime {
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
                o.push(UiOp::NotifyError(format!("ask callback: {e}")));
            }
        }
        self.drain_ops()
    }

    pub fn remove_callback(&self, id: u64) {
        if let Ok(mut cbs) = self.shared.callbacks.lock() {
            cbs.remove(&id);
        }
    }

    /// Invoke a registered Lua callback with a single value. Used by
    /// event sources that aren't tied to a window (e.g. ArgPicker
    /// `on_select` previews). Errors from the callback surface as an
    /// error notification.
    pub fn invoke_callback_value(&self, id: u64, arg: mlua::Value) {
        let func: mlua::Function = {
            let Ok(cbs) = self.shared.callbacks.lock() else {
                return;
            };
            let Some(h) = cbs.get(&id) else {
                return;
            };
            match self.lua.registry_value(&h.key) {
                Ok(f) => f,
                Err(_) => return,
            }
        };
        if let Err(e) = func.call::<()>(arg) {
            if let Ok(mut o) = self.shared.ops.lock() {
                o.push(UiOp::NotifyError(format!("picker callback: {e}")));
            }
        }
    }

    /// Satisfy a `TaskWait::External(id)` from outside the runtime.
    pub fn resolve_external(&self, external_id: u64, value: mlua::Value) -> bool {
        let Ok(mut rt) = self.shared.tasks.lock() else {
            return false;
        };
        rt.resolve_external(external_id, value)
    }

    /// Drain the task-runtime inbox and apply each event. Returns
    /// the list of `AppOp`s produced by firing any on-select
    /// callbacks.
    pub fn pump_task_events(&self) -> Vec<AppOp> {
        let events: Vec<TaskEvent> = {
            let Ok(mut inbox) = self.shared.task_inbox.lock() else {
                return Vec::new();
            };
            std::mem::take(&mut *inbox)
        };
        let extra_ops: Vec<AppOp> = Vec::new();
        for ev in events {
            match ev {
                TaskEvent::ExternalResolved { external_id, value } => {
                    let v = self.lua.registry_value(&value).unwrap_or(mlua::Value::Nil);
                    self.resolve_external(external_id, v);
                }
            }
        }
        extra_ops
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
        let Some(func) = (match self.shared.callbacks.lock() {
            Ok(cbs) => cbs
                .get(&handle.0)
                .and_then(|h| self.lua.registry_value::<mlua::Function>(&h.key).ok()),
            Err(_) => None,
        }) else {
            return;
        };
        let payload_table = match self.lua.create_table() {
            Ok(t) => t,
            Err(e) => {
                self.record_error(format!("callback payload: {e}"));
                return;
            }
        };
        if let Err(e) = populate_payload_table(&payload_table, payload) {
            self.record_error(format!("callback payload: {e}"));
            return;
        }
        if let Err(e) = payload_table.set("win", win.0) {
            self.record_error(format!("callback payload: {e}"));
            return;
        }
        match build_panels_table(&self.lua, panels) {
            Ok(t) => {
                if let Err(e) = payload_table.set("panels", t) {
                    self.record_error(format!("callback payload: {e}"));
                    return;
                }
            }
            Err(e) => {
                self.record_error(format!("callback payload: {e}"));
                return;
            }
        }
        if let Err(e) = func.call::<()>(payload_table) {
            self.record_error(format!("callback `{}`: {e}", handle.0));
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
    /// registered handler. Returns `Immediate` if the handler
    /// completes synchronously, `Pending` if it yields and will
    /// deliver later via `drive_tasks`.
    pub fn execute_plugin_tool(
        &self,
        tool_name: &str,
        args: &std::collections::HashMap<String, serde_json::Value>,
        request_id: u64,
        call_id: &str,
    ) -> ToolExecResult {
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
            if let Ok(lua_val) = super::json_to_lua(&self.lua, v) {
                let _ = args_table.set(k.as_str(), lua_val);
            }
        }

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

/// Build a Lua sequence describing a dialog window's panels at
/// callback-fire time. Each entry is `{ kind = "content" | "list" |
/// "input", selected = <1-based | nil>, text = "…" }`.
fn build_panels_table(lua: &Lua, panels: &[ui::PanelSnapshot]) -> mlua::Result<mlua::Table> {
    let out = lua.create_table()?;
    for (i, p) in panels.iter().enumerate() {
        let entry = lua.create_table()?;
        let kind = match p.kind {
            ui::PanelKind::Content => "content",
            ui::PanelKind::List { .. } => "list",
        };
        entry.set("kind", kind)?;
        if let Some(sel) = p.selected {
            entry.set("selected", sel + 1)?;
        }
        entry.set("text", p.text.clone())?;
        out.set(i + 1, entry)?;
    }
    Ok(out)
}
