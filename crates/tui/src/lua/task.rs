//! LuaTask runtime — one mechanism for plugin code that needs to suspend.
//!
//! A `LuaTask` wraps an `mlua::Thread`. The task runs until it yields a
//! discriminated table (`{ __yield = "...", ... }`); the runtime parks
//! the task on a typed `TaskWait`, and the app-loop driver resumes it
//! when the wait is satisfied (timer elapsed, dialog resolved, …).
//!
//! This replaces three ad-hoc patterns:
//! - `DomainOp::ResolveToolResult` + `callbacks: HashMap<u64, …>` +
//!   `smelt.api.tools.resolve(...)`.
//! - `smelt.defer(ms, fn)` callback timers.
//! - Declarative `confirm = {...}` specs bolted onto tool registration.
//!
//! Lua side, every suspending API is `coroutine.yield` under the hood,
//! so plugin code reads as synchronous:
//!
//! ```lua
//! smelt.task(function()
//!   smelt.api.sleep(200)
//!   local r = smelt.api.dialog.open({...})  -- step (iv)
//!   return r.action
//! end)
//! ```
//!
//! Step (i) (this module) supports only `Sleep`. `OpenDialog` and
//! `ToolResult` completion land in steps (iii) / (iv).

use mlua::prelude::*;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// What a parked task is waiting for. When the wait is satisfied the
/// driver resumes the thread with the stored value.
enum TaskWait {
    /// Resume on next drive tick with this value. New tasks start
    /// with `Ready(Nil)` (fire-and-forget) or `Ready(args_table)`
    /// (initial args for tool execute). External resolvers also set
    /// this (`resolve_dialog` stores the answer here).
    Ready(LuaValue),
    /// Resume with `nil` once `Instant` has passed.
    Sleep(Instant),
    /// Waiting for `resolve_dialog(dialog_id, …)` to be called.
    Dialog(u64),
    /// Waiting for `resolve_picker(picker_id, …)` to be called.
    Picker(u64),
    /// Waiting for a Lua runtime file to call `smelt.api.task.resume
    /// (id, value)`. Used by `runtime/lua/smelt/*.lua` helpers that
    /// own the coroutine dance themselves — no Rust-side TaskDriveOutput
    /// is emitted, because the Lua runtime file opens the float + binds
    /// keymaps + closes the float on its own.
    External(u64),
}

/// What to do when a task's top-level function returns.
pub enum TaskCompletion {
    /// `smelt.task(fn)` kickoff — return value is discarded, errors
    /// surface as notifications.
    FireAndForget,
    /// Tool `execute` handler — return value is delivered to the
    /// engine as the tool result.
    ToolResult { request_id: u64, call_id: String },
}

struct LuaTask {
    id: u64,
    thread: mlua::Thread,
    wait: TaskWait,
    completion: TaskCompletion,
}

/// One output per drive tick. The app loop consumes these and maps
/// them onto Rust-side side effects (open a dialog, deliver a tool
/// result, notify an error).
pub enum TaskDriveOutput {
    /// Task yielded `{ __yield = "dialog", opts = {...} }`. The app
    /// builds the dialog, pushes it onto the compositor, and later
    /// calls `LuaTaskRuntime::resolve_dialog(dialog_id, result)`.
    OpenDialog {
        task_id: u64,
        dialog_id: u64,
        opts: mlua::RegistryKey,
    },
    /// Task yielded `{ __yield = "picker", opts = {...} }`. The app
    /// opens a focusable picker float, registers Up/Down/Enter/Escape
    /// keymaps, and on resolution calls
    /// `LuaTaskRuntime::resolve_picker(picker_id, result)`.
    OpenPicker {
        task_id: u64,
        picker_id: u64,
        opts: mlua::RegistryKey,
    },
    /// Tool-execute task returned.
    ToolComplete {
        request_id: u64,
        call_id: String,
        content: String,
        is_error: bool,
    },
    /// Task errored (bad yield shape, handler panic, …). The app
    /// queues a `NotifyError`.
    Error(String),
}

/// Single-threaded task runtime. All methods must be called on the
/// thread that owns the `mlua::Lua`.
pub struct LuaTaskRuntime {
    tasks: Vec<LuaTask>,
    next_task_id: AtomicU64,
    next_dialog_id: AtomicU64,
    next_picker_id: AtomicU64,
    next_external_id: AtomicU64,
    /// Outputs that an in-line `drive` produced but whose caller
    /// doesn't route them itself — e.g. `execute_plugin_tool` runs
    /// one drive inline and consumes only its own `ToolComplete`;
    /// any `OpenDialog` or stray outputs get parked here for the
    /// next app-level `drive_tasks` to handle.
    deferred: Vec<TaskDriveOutput>,
}

impl LuaTaskRuntime {
    pub fn new() -> Self {
        Self {
            tasks: Vec::new(),
            next_task_id: AtomicU64::new(1),
            next_dialog_id: AtomicU64::new(1),
            next_picker_id: AtomicU64::new(1),
            next_external_id: AtomicU64::new(1),
            deferred: Vec::new(),
        }
    }

    /// Mint an external-wait id without spawning a task. The Lua
    /// runtime file calls this, holds the id, and later passes it to
    /// `resolve_external` when its keymaps fire.
    pub fn alloc_external_id(&self) -> u64 {
        self.next_external_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Queue outputs produced by an inline drive whose caller didn't
    /// consume them. Drained into the next `drive` return.
    pub fn defer_output(&mut self, output: TaskDriveOutput) {
        self.deferred.push(output);
    }

    /// Spawn a task from a Lua function. The task runs on the next
    /// `drive` call; `initial_arg` is passed to the handler on first
    /// resume (i.e. becomes its first argument). Pass `Nil` for
    /// zero-arg kickoffs (`smelt.task(fn)`) and the args table for
    /// tool execute.
    pub fn spawn(
        &mut self,
        lua: &Lua,
        func: mlua::Function,
        initial_arg: LuaValue,
        completion: TaskCompletion,
    ) -> LuaResult<u64> {
        let thread = lua.create_thread(func)?;
        let id = self.next_task_id.fetch_add(1, Ordering::Relaxed);
        self.tasks.push(LuaTask {
            id,
            thread,
            wait: TaskWait::Ready(initial_arg),
            completion,
        });
        Ok(id)
    }

    /// Satisfy a `TaskWait::Dialog(id)` wait with the given result
    /// value. Returns `true` if a matching task was found.
    pub fn resolve_dialog(&mut self, dialog_id: u64, value: LuaValue) -> bool {
        for task in &mut self.tasks {
            if matches!(&task.wait, TaskWait::Dialog(id) if *id == dialog_id) {
                task.wait = TaskWait::Ready(value);
                return true;
            }
        }
        false
    }

    /// Satisfy a `TaskWait::Picker(id)` wait with the given result
    /// value. Returns `true` if a matching task was found.
    pub fn resolve_picker(&mut self, picker_id: u64, value: LuaValue) -> bool {
        for task in &mut self.tasks {
            if matches!(&task.wait, TaskWait::Picker(id) if *id == picker_id) {
                task.wait = TaskWait::Ready(value);
                return true;
            }
        }
        false
    }

    /// Satisfy a `TaskWait::External(id)` wait with the given result
    /// value. Returns `true` if a matching task was found.
    pub fn resolve_external(&mut self, external_id: u64, value: LuaValue) -> bool {
        for task in &mut self.tasks {
            if matches!(&task.wait, TaskWait::External(id) if *id == external_id) {
                task.wait = TaskWait::Ready(value);
                return true;
            }
        }
        false
    }

    /// Drive all ready tasks once. Each ready task is resumed; if it
    /// yields, it's parked on a new wait; if it returns, its
    /// completion is reported. Any outputs deferred from a previous
    /// inline drive are flushed first, in order.
    pub fn drive(&mut self, lua: &Lua, now: Instant) -> Vec<TaskDriveOutput> {
        let mut outputs = std::mem::take(&mut self.deferred);
        let mut i = 0;
        while i < self.tasks.len() {
            let ready = match &self.tasks[i].wait {
                TaskWait::Ready(_) => true,
                TaskWait::Sleep(deadline) => *deadline <= now,
                TaskWait::Dialog(_) | TaskWait::Picker(_) | TaskWait::External(_) => false,
            };
            if !ready {
                i += 1;
                continue;
            }
            let drop_task = self.step_task(lua, i, &mut outputs);
            if drop_task {
                self.tasks.swap_remove(i);
            } else {
                i += 1;
            }
        }
        outputs
    }

    /// Resume task at `idx` once. Returns `true` when the task should
    /// be dropped (finished or errored).
    fn step_task(&mut self, lua: &Lua, idx: usize, outputs: &mut Vec<TaskDriveOutput>) -> bool {
        let task = &mut self.tasks[idx];
        let resume_arg = match std::mem::replace(&mut task.wait, TaskWait::Ready(LuaValue::Nil)) {
            TaskWait::Ready(v) => v,
            TaskWait::Sleep(_) => LuaValue::Nil,
            // unreachable per ready check above:
            TaskWait::Dialog(_) | TaskWait::Picker(_) | TaskWait::External(_) => LuaValue::Nil,
        };
        let result: LuaResult<LuaValue> = task.thread.resume(resume_arg);

        match result {
            Ok(v) => {
                if task.thread.status() == mlua::ThreadStatus::Finished {
                    match &task.completion {
                        TaskCompletion::FireAndForget => {}
                        TaskCompletion::ToolResult {
                            request_id,
                            call_id,
                        } => {
                            let (content, is_error) = coerce_tool_result(&v);
                            outputs.push(TaskDriveOutput::ToolComplete {
                                request_id: *request_id,
                                call_id: call_id.clone(),
                                content,
                                is_error,
                            });
                        }
                    }
                    return true;
                }
                // Still yielded — decode the yield table.
                match decode_yield(lua, v) {
                    Ok(Yield::Sleep(d)) => {
                        task.wait = TaskWait::Sleep(Instant::now() + d);
                        false
                    }
                    Ok(Yield::OpenDialog(opts_key)) => {
                        let did = self.next_dialog_id.fetch_add(1, Ordering::Relaxed);
                        let task_id = task.id;
                        task.wait = TaskWait::Dialog(did);
                        outputs.push(TaskDriveOutput::OpenDialog {
                            task_id,
                            dialog_id: did,
                            opts: opts_key,
                        });
                        false
                    }
                    Ok(Yield::OpenPicker(opts_key)) => {
                        let pid = self.next_picker_id.fetch_add(1, Ordering::Relaxed);
                        let task_id = task.id;
                        task.wait = TaskWait::Picker(pid);
                        outputs.push(TaskDriveOutput::OpenPicker {
                            task_id,
                            picker_id: pid,
                            opts: opts_key,
                        });
                        false
                    }
                    Ok(Yield::External(id)) => {
                        // No Rust-side output — the Lua runtime file
                        // owns the resume via `smelt.api.task.resume`.
                        task.wait = TaskWait::External(id);
                        false
                    }
                    Err(msg) => {
                        outputs.push(TaskDriveOutput::Error(format!("task {}: {msg}", task.id)));
                        fail_completion(&task.completion, &msg, outputs);
                        true
                    }
                }
            }
            Err(e) => {
                let msg = e.to_string();
                outputs.push(TaskDriveOutput::Error(format!("task {}: {msg}", task.id)));
                fail_completion(&task.completion, &msg, outputs);
                true
            }
        }
    }
}

fn fail_completion(completion: &TaskCompletion, msg: &str, outputs: &mut Vec<TaskDriveOutput>) {
    if let TaskCompletion::ToolResult {
        request_id,
        call_id,
    } = completion
    {
        outputs.push(TaskDriveOutput::ToolComplete {
            request_id: *request_id,
            call_id: call_id.clone(),
            content: format!("plugin tool error: {msg}"),
            is_error: true,
        });
    }
}

impl Default for LuaTaskRuntime {
    fn default() -> Self {
        Self::new()
    }
}

/// Decoded `coroutine.yield(...)` payload.
enum Yield {
    Sleep(Duration),
    OpenDialog(mlua::RegistryKey),
    OpenPicker(mlua::RegistryKey),
    /// Park the task on an externally-resolved wait. The id must have
    /// been minted via `alloc_external_id` beforehand, so the Lua
    /// runtime file can hand it to its keymaps before yielding.
    External(u64),
}

fn decode_yield(lua: &Lua, v: LuaValue) -> Result<Yield, String> {
    let table = match v {
        LuaValue::Table(t) => t,
        other => {
            return Err(format!("expected yield table, got {}", other.type_name()));
        }
    };
    let kind: String = table
        .get("__yield")
        .map_err(|e| format!("yield missing __yield discriminator: {e}"))?;
    match kind.as_str() {
        "sleep" => {
            let ms: u64 = table.get("ms").map_err(|e| format!("sleep: {e}"))?;
            Ok(Yield::Sleep(Duration::from_millis(ms)))
        }
        "dialog" => {
            let opts: mlua::Table = table.get("opts").map_err(|e| format!("dialog: {e}"))?;
            let key = lua
                .create_registry_value(opts)
                .map_err(|e| format!("dialog opts registry: {e}"))?;
            Ok(Yield::OpenDialog(key))
        }
        "picker" => {
            let opts: mlua::Table = table.get("opts").map_err(|e| format!("picker: {e}"))?;
            let key = lua
                .create_registry_value(opts)
                .map_err(|e| format!("picker opts registry: {e}"))?;
            Ok(Yield::OpenPicker(key))
        }
        "external" => {
            let id: u64 = table.get("id").map_err(|e| format!("external: {e}"))?;
            Ok(Yield::External(id))
        }
        other => Err(format!("unknown yield kind: {other}")),
    }
}

/// Turn a task return value into `(content, is_error)` for tool
/// results. Accepts either a string (`is_error = false`) or a table
/// `{ content = "...", is_error = bool }`.
fn coerce_tool_result(v: &LuaValue) -> (String, bool) {
    match v {
        LuaValue::String(s) => (s.to_string_lossy().to_string(), false),
        LuaValue::Table(t) => {
            let content: String = t.get("content").unwrap_or_default();
            let is_error: bool = t.get("is_error").unwrap_or(false);
            (content, is_error)
        }
        LuaValue::Nil => (String::new(), false),
        other => (
            format!(
                "plugin tool returned non-string value: {}",
                other.type_name()
            ),
            true,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lua_with_sleep() -> Lua {
        let lua = Lua::new();
        lua.load(
            r#"
            smelt = {}
            smelt.api = {}
            function smelt.api.sleep(ms)
              if not coroutine.isyieldable() then
                error("smelt.api.sleep: not inside a task", 2)
              end
              return coroutine.yield({__yield = "sleep", ms = ms})
            end
            "#,
        )
        .exec()
        .unwrap();
        lua
    }

    #[test]
    fn fire_and_forget_returns_immediately() {
        let lua = lua_with_sleep();
        let mut rt = LuaTaskRuntime::new();
        let func: mlua::Function = lua.load("function() end").eval().unwrap();
        rt.spawn(&lua, func, LuaValue::Nil, TaskCompletion::FireAndForget)
            .unwrap();
        let out = rt.drive(&lua, Instant::now());
        assert!(out.is_empty());
        assert_eq!(rt.tasks.len(), 0);
    }

    #[test]
    fn sleep_yields_and_parks() {
        let lua = lua_with_sleep();
        let mut rt = LuaTaskRuntime::new();
        let func: mlua::Function = lua
            .load(
                r#"function()
                smelt.api.sleep(100)
                return "done"
              end"#,
            )
            .eval()
            .unwrap();
        rt.spawn(&lua, func, LuaValue::Nil, TaskCompletion::FireAndForget)
            .unwrap();

        // First drive: task runs, yields sleep, parks.
        let t0 = Instant::now();
        let out = rt.drive(&lua, t0);
        assert!(out.is_empty());
        assert_eq!(rt.tasks.len(), 1);
        assert!(matches!(rt.tasks[0].wait, TaskWait::Sleep(_)));

        // Second drive before deadline — still parked.
        let out = rt.drive(&lua, t0 + Duration::from_millis(50));
        assert!(out.is_empty());
        assert_eq!(rt.tasks.len(), 1);

        // Third drive past deadline — resumes and completes.
        let out = rt.drive(&lua, t0 + Duration::from_millis(200));
        assert!(out.is_empty());
        assert_eq!(rt.tasks.len(), 0);
    }

    #[test]
    fn tool_result_string_return() {
        let lua = lua_with_sleep();
        let mut rt = LuaTaskRuntime::new();
        let func: mlua::Function = lua.load(r#"function() return "hello" end"#).eval().unwrap();
        rt.spawn(
            &lua,
            func,
            LuaValue::Nil,
            TaskCompletion::ToolResult {
                request_id: 7,
                call_id: "c1".into(),
            },
        )
        .unwrap();
        let out = rt.drive(&lua, Instant::now());
        assert_eq!(out.len(), 1);
        match &out[0] {
            TaskDriveOutput::ToolComplete {
                request_id,
                call_id,
                content,
                is_error,
            } => {
                assert_eq!(*request_id, 7);
                assert_eq!(call_id, "c1");
                assert_eq!(content, "hello");
                assert!(!*is_error);
            }
            _ => panic!("expected ToolComplete"),
        }
    }

    #[test]
    fn tool_result_error_table() {
        let lua = lua_with_sleep();
        let mut rt = LuaTaskRuntime::new();
        let func: mlua::Function = lua
            .load(r#"function() return {content = "boom", is_error = true} end"#)
            .eval()
            .unwrap();
        rt.spawn(
            &lua,
            func,
            LuaValue::Nil,
            TaskCompletion::ToolResult {
                request_id: 1,
                call_id: "x".into(),
            },
        )
        .unwrap();
        let out = rt.drive(&lua, Instant::now());
        assert!(matches!(
            &out[0],
            TaskDriveOutput::ToolComplete { is_error: true, content, .. } if content == "boom"
        ));
    }

    #[test]
    fn handler_error_reports_task_error_and_tool_error() {
        let lua = lua_with_sleep();
        let mut rt = LuaTaskRuntime::new();
        let func: mlua::Function = lua.load(r#"function() error("bang") end"#).eval().unwrap();
        rt.spawn(
            &lua,
            func,
            LuaValue::Nil,
            TaskCompletion::ToolResult {
                request_id: 2,
                call_id: "y".into(),
            },
        )
        .unwrap();
        let out = rt.drive(&lua, Instant::now());
        // Error notification + failing tool completion.
        let has_error = out
            .iter()
            .any(|o| matches!(o, TaskDriveOutput::Error(m) if m.contains("bang")));
        let has_tool_err = out
            .iter()
            .any(|o| matches!(o, TaskDriveOutput::ToolComplete { is_error: true, .. }));
        assert!(has_error);
        assert!(has_tool_err);
        assert_eq!(rt.tasks.len(), 0);
    }

    #[test]
    fn sleep_outside_task_errors() {
        let lua = lua_with_sleep();
        let res: LuaResult<()> = lua.load("smelt.api.sleep(10)").exec();
        assert!(res.is_err());
        let msg = format!("{}", res.unwrap_err());
        assert!(msg.contains("not inside a task"));
    }
}
