//! Cooperative Lua task runtime. A task wraps `mlua::Thread`; it runs until it yields
//! a discriminated `{ __yield = "...", ... }` table and parks on a typed `TaskWait`.

use mlua::prelude::*;
use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

enum TaskWait {
    Ready(LuaMultiValue),
    Sleep(Instant),
    External { id: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskScope {
    App,
    Turn,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandQueueTarget {
    Turn,
    Request,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolInvocationContext {
    pub invocation_id: protocol::InvocationId,
    pub request_id: u64,
    pub execution_mode: protocol::ToolExecutionMode,
}

pub enum TaskCompletion {
    FireAndForget,
    /// Slash-command dispatch. `name` carries the cmd name so an error
    /// surfaces as `cmd `<name>`: …` instead of the opaque `task <id>: …`.
    Command {
        name: String,
        queue_target: CommandQueueTarget,
        sent_at_ms: Option<u64>,
    },
    ToolResult {
        invocation: ToolInvocationContext,
        call_id: String,
    },
}

impl TaskCompletion {
    fn notification_label(&self, task_id: u64) -> String {
        match self {
            TaskCompletion::Command { name, .. } => format!("cmd `{name}`"),
            _ => format!("task {task_id}"),
        }
    }

    fn command_queue_target(&self) -> Option<CommandQueueTarget> {
        match self {
            TaskCompletion::Command { queue_target, .. } => Some(*queue_target),
            _ => None,
        }
    }

    fn command_sent_at_ms(&self) -> Option<u64> {
        match self {
            TaskCompletion::Command { sent_at_ms, .. } => *sent_at_ms,
            _ => None,
        }
    }

    fn tool_invocation(&self) -> Option<ToolInvocationContext> {
        match self {
            TaskCompletion::ToolResult { invocation, .. } => Some(*invocation),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct TaskDeadline {
    pub at: Instant,
    pub label_ms: u64,
    pub(crate) paused_at: Option<Instant>,
}

pub(crate) struct LuaTask {
    id: u64,
    thread: mlua::Thread,
    wait: TaskWait,
    completion: TaskCompletion,
    scope: TaskScope,
    cancel: CancellationToken,
    deadline: Option<TaskDeadline>,
}

#[derive(Debug)]
pub enum TaskDriveOutput {
    ToolComplete {
        invocation: ToolInvocationContext,
        call_id: String,
        content: String,
        is_error: bool,
        metadata: Option<serde_json::Value>,
        display_content: Vec<protocol::ToolDisplayContent>,
        attachment: Option<Box<protocol::ToolAttachment>>,
    },
    NotifyError(String),
}

pub enum TaskEvent {
    /// In-thread resume via `smelt.task.resume(id, value)`.
    ExternalResolved {
        external_id: u64,
        value: mlua::RegistryKey,
    },
    /// Cross-thread resume from a tokio task; JSON is `Send` and converted on the main thread.
    ExternalResolvedJson {
        external_id: u64,
        value: serde_json::Value,
    },
}

/// Second argument of `execute(args, ctx)`.
pub struct ToolEnv<'a> {
    pub mode: protocol::AgentMode,
    pub session_id: &'a str,
    pub artifact_dir: &'a std::path::Path,
}

/// Single-threaded task runtime; all methods must run on the Lua owner thread.
///
/// **Invariant**: the `LuaShared::tasks` mutex guarding this struct must never
/// be held across a call into Lua code. Lua callbacks can re-enter the runtime
/// (e.g. `smelt.spawn` from inside a coroutine, `Reg:remove()` cancelling a
/// sibling task), and `std::sync::Mutex` is non-reentrant - holding the lock
/// across a resume would deadlock. Task drivers enforce this by popping a
/// task out via `take_next_ready`, dropping the lock for the duration of
/// `step_task_owned`, then reacquiring to `put_back` the parked task.
pub struct LuaTaskRuntime {
    tasks: Vec<LuaTask>,
    next_task_id: AtomicU64,
}

impl LuaTaskRuntime {
    pub fn new() -> Self {
        Self {
            tasks: Vec::new(),
            next_task_id: AtomicU64::new(1),
        }
    }

    pub fn spawn(
        &mut self,
        lua: &Lua,
        func: mlua::Function,
        initial_args: LuaMultiValue,
        completion: TaskCompletion,
    ) -> LuaResult<u64> {
        self.spawn_scoped(
            lua,
            func,
            initial_args,
            completion,
            current_task_scope().unwrap_or(TaskScope::App),
            None,
        )
    }

    pub(crate) fn spawn_scoped(
        &mut self,
        lua: &Lua,
        func: mlua::Function,
        initial_args: LuaMultiValue,
        completion: TaskCompletion,
        scope: TaskScope,
        deadline: Option<TaskDeadline>,
    ) -> LuaResult<u64> {
        let thread = lua.create_thread(func)?;
        let id = self.next_task_id.fetch_add(1, Ordering::Relaxed);
        self.tasks.push(LuaTask {
            id,
            thread,
            wait: TaskWait::Ready(initial_args),
            completion,
            scope,
            cancel: CancellationToken::new(),
            deadline,
        });
        Ok(id)
    }

    /// Resolve a `TaskWait::External(id)` wait; returns `true` if found.
    pub fn resolve_external(&mut self, external_id: u64, value: LuaValue) -> bool {
        for task in &mut self.tasks {
            if matches!(&task.wait, TaskWait::External { id, .. } if *id == external_id) {
                let mut mv = LuaMultiValue::new();
                mv.push_back(value);
                task.wait = TaskWait::Ready(mv);
                return true;
            }
        }
        false
    }

    /// Cancel a single task by id. Returns `true` if the task was found.
    /// Sleeping/external tasks are unparked with the cancelled marker; the
    /// next `drive()` step delivers the marker to the coroutine so user
    /// code in `smelt.sleep` / `task.wait` raises `cancelled` and unwinds.
    pub fn cancel_task(&mut self, lua: &Lua, id: u64) -> bool {
        let marker = cancelled_marker(lua);
        for task in &mut self.tasks {
            if task.id == id {
                task.cancel.cancel();
                if matches!(&task.wait, TaskWait::Sleep(_) | TaskWait::External { .. }) {
                    let mut mv = LuaMultiValue::new();
                    mv.push_back(marker);
                    task.wait = TaskWait::Ready(mv);
                }
                return true;
            }
        }
        false
    }

    pub fn cancel_scope(&mut self, lua: &Lua, scope: TaskScope) {
        self.cancel_matching(lua, |task| task.scope == scope);
    }

    pub fn cancel_all(&mut self, lua: &Lua) {
        self.cancel_matching(lua, |_| true);
    }

    fn cancel_matching(&mut self, lua: &Lua, mut matches: impl FnMut(&LuaTask) -> bool) {
        let marker = cancelled_marker(lua);
        for task in &mut self.tasks {
            if !matches(task) {
                continue;
            }
            task.cancel.cancel();
            match &task.wait {
                TaskWait::Sleep(_) | TaskWait::External { .. } => {
                    let mut mv = LuaMultiValue::new();
                    mv.push_back(marker.clone());
                    task.wait = TaskWait::Ready(mv);
                }
                TaskWait::Ready(_) => {}
            }
        }
    }

    /// Cancel every task and drop the entries immediately. Used by
    /// `/reload`: the surviving Lua handles are about to be wiped, so
    /// letting tasks resume on the next `drive()` would invoke stale
    /// closures.
    pub fn cancel_and_clear(&mut self) {
        for task in &mut self.tasks {
            task.cancel.cancel();
        }
        self.tasks.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    pub(crate) fn next_wakeup(&self, now: Instant) -> Option<Instant> {
        self.tasks
            .iter()
            .filter_map(|task| task.next_wakeup(now))
            .min()
    }

    /// Pop a ready task out of the runtime, returning it by value. `drive_tasks`
    /// uses this to step a task without holding the `tasks` mutex - so Lua code
    /// that re-enters the runtime (e.g. `smelt.spawn` from inside a coroutine)
    /// can acquire the lock synchronously instead of deadlocking.
    pub(crate) fn take_next_ready(&mut self, now: Instant) -> Option<LuaTask> {
        let idx = self.tasks.iter().position(|t| t.ready_at(now))?;
        self.tasks[idx].resume_deadline_if_paused(now);
        Some(self.tasks.swap_remove(idx))
    }

    pub(crate) fn take_task(&mut self, id: u64) -> Option<LuaTask> {
        let idx = self.tasks.iter().position(|t| t.id == id)?;
        Some(self.tasks.swap_remove(idx))
    }

    pub(crate) fn put_back(&mut self, task: LuaTask) {
        self.tasks.push(task);
    }

    /// Drive every ready task once. Used by tests; production callers use
    /// `take_next_ready` + `step_task_owned` + `put_back` so the lock can be
    /// dropped during the resume.
    pub fn drive(&mut self, lua: &Lua, now: Instant) -> Vec<TaskDriveOutput> {
        let mut outputs = Vec::new();
        while let Some(task) = self.take_next_ready(now) {
            if let Some(parked) = step_task_owned(lua, task, now, &mut outputs) {
                self.tasks.push(parked);
            }
        }
        outputs
    }
}

impl LuaTask {
    fn timed_out(&self, now: Instant) -> bool {
        self.deadline
            .is_some_and(|deadline| deadline.paused_at.is_none() && deadline.at <= now)
    }

    fn ready_at(&self, now: Instant) -> bool {
        self.timed_out(now)
            || match &self.wait {
                TaskWait::Ready(_) => true,
                TaskWait::Sleep(deadline) => self.cancel.is_cancelled() || *deadline <= now,
                TaskWait::External { .. } => false,
            }
    }

    fn next_wakeup(&self, now: Instant) -> Option<Instant> {
        if self.ready_at(now) {
            return Some(now);
        }
        let wait_at = match &self.wait {
            TaskWait::Sleep(deadline) => Some(*deadline),
            TaskWait::Ready(_) | TaskWait::External { .. } => None,
        };
        match (wait_at, self.deadline) {
            (Some(a), Some(deadline)) if deadline.paused_at.is_none() => Some(a.min(deadline.at)),
            (Some(a), _) => Some(a),
            (None, Some(deadline)) if deadline.paused_at.is_none() => Some(deadline.at),
            (None, _) => None,
        }
    }

    fn pause_deadline(&mut self, now: Instant) {
        if let Some(deadline) = &mut self.deadline {
            if deadline.paused_at.is_none() {
                deadline.paused_at = Some(now);
            }
        }
    }

    fn resume_deadline_if_paused(&mut self, now: Instant) {
        if let Some(deadline) = &mut self.deadline {
            if let Some(paused_at) = deadline.paused_at.take() {
                deadline.at += now.saturating_duration_since(paused_at);
            }
        }
    }

    fn timeout_message(&self) -> String {
        match self.deadline {
            Some(deadline) => format!("timed out after {:.1}s", deadline.label_ms as f64 / 1000.0),
            None => "timed out".to_string(),
        }
    }
}

/// Resume one task without holding the runtime. Returns the task if it parked
/// again (sleep/external), `None` if it finished or errored.
pub(crate) fn step_task_owned(
    lua: &Lua,
    mut task: LuaTask,
    now: Instant,
    outputs: &mut Vec<TaskDriveOutput>,
) -> Option<LuaTask> {
    if task.timed_out(now) {
        task.cancel.cancel();
        emit_task_timeout(&task, outputs);
        return None;
    }

    let resume_args = match std::mem::replace(&mut task.wait, TaskWait::Ready(LuaMultiValue::new()))
    {
        TaskWait::Ready(mv) => mv,
        TaskWait::Sleep(_) => LuaMultiValue::new(),
        TaskWait::External { .. } => LuaMultiValue::new(),
    };
    let cancel = task.cancel.clone();
    let queue_target = task.completion.command_queue_target();
    let sent_at_ms = task.completion.command_sent_at_ms();
    let tool_invocation = task.completion.tool_invocation();
    let scope = task.scope;
    let result: LuaResult<LuaValue> = with_task_context(
        cancel,
        queue_target,
        sent_at_ms,
        tool_invocation,
        scope,
        || task.thread.resume(resume_args),
    );

    match result {
        Ok(v) => {
            if task.thread.is_finished() {
                match &task.completion {
                    TaskCompletion::FireAndForget | TaskCompletion::Command { .. } => {}
                    TaskCompletion::ToolResult {
                        invocation,
                        call_id,
                    } => match coerce_tool_result(lua, &v) {
                        Ok(result) => outputs.push(TaskDriveOutput::ToolComplete {
                            invocation: *invocation,
                            call_id: call_id.clone(),
                            content: result.content,
                            is_error: result.is_error,
                            metadata: result.metadata,
                            display_content: result.display_content,
                            attachment: result.attachment.map(Box::new),
                        }),
                        Err(error) => emit_task_failure(
                            &task,
                            &format!("invalid tool result: {error}"),
                            outputs,
                        ),
                    },
                }
                return None;
            }
            match decode_yield(lua, v) {
                Ok(Yield::Sleep(d)) => {
                    if task.cancel.is_cancelled() {
                        let mut mv = LuaMultiValue::new();
                        mv.push_back(cancelled_marker(lua));
                        task.wait = TaskWait::Ready(mv);
                    } else {
                        task.wait = TaskWait::Sleep(now + d);
                    }
                    Some(task)
                }
                Ok(Yield::External {
                    id,
                    pauses_deadline,
                }) => {
                    if task.cancel.is_cancelled() {
                        let mut mv = LuaMultiValue::new();
                        mv.push_back(cancelled_marker(lua));
                        task.wait = TaskWait::Ready(mv);
                    } else {
                        if pauses_deadline {
                            task.pause_deadline(now);
                        }
                        task.wait = TaskWait::External { id };
                    }
                    Some(task)
                }
                Err(msg) => {
                    emit_task_failure(&task, &msg, outputs);
                    None
                }
            }
        }
        Err(e) => {
            if task.cancel.is_cancelled() && is_cancelled_lua_error(&e) {
                return None;
            }
            let msg = e.to_string();
            emit_task_failure(&task, &msg, outputs);
            None
        }
    }
}

// Thread-local context for the executing coroutine; read by async Lua bindings.
thread_local! {
    static CURRENT_TASK_CANCEL: RefCell<Option<CancellationToken>> = const { RefCell::new(None) };
    static CURRENT_COMMAND_QUEUE_TARGET: RefCell<Option<CommandQueueTarget>> = const { RefCell::new(None) };
    static CURRENT_COMMAND_SENT_AT_MS: RefCell<Option<u64>> = const { RefCell::new(None) };
    static CURRENT_TOOL_INVOCATION: RefCell<Option<ToolInvocationContext>> = const { RefCell::new(None) };
    static CURRENT_TASK_SCOPE: RefCell<Option<TaskScope>> = const { RefCell::new(None) };
}

/// Install task context for the closure's duration.
fn with_task_context<R>(
    cancel: CancellationToken,
    queue_target: Option<CommandQueueTarget>,
    sent_at_ms: Option<u64>,
    tool_invocation: Option<ToolInvocationContext>,
    scope: TaskScope,
    f: impl FnOnce() -> R,
) -> R {
    let previous_cancel = CURRENT_TASK_CANCEL.with(|c| c.replace(Some(cancel)));
    let previous_target = CURRENT_COMMAND_QUEUE_TARGET.with(|c| c.replace(queue_target));
    let previous_sent_at_ms = CURRENT_COMMAND_SENT_AT_MS.with(|c| c.replace(sent_at_ms));
    let previous_tool_invocation = CURRENT_TOOL_INVOCATION.with(|c| c.replace(tool_invocation));
    let previous_scope = CURRENT_TASK_SCOPE.with(|c| c.replace(Some(scope)));
    let r = f();
    CURRENT_TASK_SCOPE.with(|c| c.replace(previous_scope));
    CURRENT_TOOL_INVOCATION.with(|c| c.replace(previous_tool_invocation));
    CURRENT_COMMAND_SENT_AT_MS.with(|c| c.replace(previous_sent_at_ms));
    CURRENT_COMMAND_QUEUE_TARGET.with(|c| c.replace(previous_target));
    CURRENT_TASK_CANCEL.with(|c| c.replace(previous_cancel));
    r
}

/// Install the task's cancellation token for the closure's duration.
pub fn with_task_cancel<R>(cancel: CancellationToken, f: impl FnOnce() -> R) -> R {
    with_task_context(cancel, None, None, None, TaskScope::App, f)
}

/// Current task's cancellation token; `None` when called outside `step_task`.
pub fn current_task_cancel() -> Option<CancellationToken> {
    CURRENT_TASK_CANCEL.with(|c| c.borrow().clone())
}

/// Current slash-command queue target; `None` outside slash-command tasks.
pub fn current_command_queue_target() -> Option<CommandQueueTarget> {
    CURRENT_COMMAND_QUEUE_TARGET.with(|c| *c.borrow())
}

/// Submission time of the current slash command, preserved across coroutine yields.
pub fn current_command_sent_at_ms() -> Option<u64> {
    CURRENT_COMMAND_SENT_AT_MS.with(|c| *c.borrow())
}

pub(crate) fn with_tool_invocation_context<R>(
    invocation: ToolInvocationContext,
    f: impl FnOnce() -> R,
) -> R {
    let previous = CURRENT_TOOL_INVOCATION.with(|current| current.replace(Some(invocation)));
    let result = f();
    CURRENT_TOOL_INVOCATION.with(|current| current.replace(previous));
    result
}

/// Current model tool invocation; `None` outside a tool callback or middleware.
pub fn current_tool_invocation() -> Option<ToolInvocationContext> {
    CURRENT_TOOL_INVOCATION.with(|current| *current.borrow())
}

/// Current task scope; `None` outside the Lua task runtime.
pub fn current_task_scope() -> Option<TaskScope> {
    CURRENT_TASK_SCOPE.with(|c| *c.borrow())
}

fn cancelled_marker(lua: &Lua) -> LuaValue {
    lua.create_table()
        .and_then(|t| {
            t.set("__cancelled", true)?;
            Ok(LuaValue::Table(t))
        })
        .unwrap_or(LuaValue::Nil)
}

fn emit_task_timeout(task: &LuaTask, outputs: &mut Vec<TaskDriveOutput>) {
    let message = task.timeout_message();
    if matches!(&task.completion, TaskCompletion::ToolResult { .. }) {
        fail_completion(&task.completion, &message, outputs);
    } else {
        emit_task_failure(task, &message, outputs);
    }
}

fn emit_task_failure(task: &LuaTask, msg: &str, outputs: &mut Vec<TaskDriveOutput>) {
    outputs.push(TaskDriveOutput::NotifyError(format!(
        "{}: {msg}",
        task.completion.notification_label(task.id)
    )));
    fail_completion(&task.completion, msg, outputs);
}

fn is_cancelled_lua_error(err: &mlua::Error) -> bool {
    match err {
        // mlua appends a Lua traceback after the error string, so the
        // RuntimeError payload is "cancelled\nstack traceback:\n...".
        mlua::Error::RuntimeError(msg) => msg.lines().next() == Some("cancelled"),
        mlua::Error::CallbackError { cause, .. } => is_cancelled_lua_error(cause.as_ref()),
        _ => false,
    }
}

fn fail_completion(completion: &TaskCompletion, msg: &str, outputs: &mut Vec<TaskDriveOutput>) {
    if let TaskCompletion::ToolResult {
        invocation,
        call_id,
    } = completion
    {
        outputs.push(TaskDriveOutput::ToolComplete {
            invocation: *invocation,
            call_id: call_id.clone(),
            content: format!("tool error: {msg}"),
            is_error: true,
            metadata: None,
            display_content: Vec::new(),
            attachment: None,
        });
    }
}

impl Default for LuaTaskRuntime {
    fn default() -> Self {
        Self::new()
    }
}

enum Yield {
    Sleep(Duration),
    External { id: u64, pauses_deadline: bool },
}

fn decode_yield(_lua: &Lua, v: LuaValue) -> Result<Yield, String> {
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
        "external" => {
            let id: u64 = table.get("id").map_err(|e| format!("external: {e}"))?;
            let pauses_deadline = table
                .get::<Option<bool>>("pauses_deadline")
                .ok()
                .flatten()
                .or_else(|| table.get::<Option<bool>>("interactive").ok().flatten())
                .unwrap_or(false);
            Ok(Yield::External {
                id,
                pauses_deadline,
            })
        }
        other => Err(format!("unknown yield kind: {other}")),
    }
}

/// Coerce a task return value to content, status, bounded JSON metadata, and retained display
/// content. Large presentation payloads and attachments are extracted before metadata limits.
fn coerce_tool_result(lua: &Lua, value: &LuaValue) -> LuaResult<crate::lua::LuaToolResultParts> {
    match value {
        LuaValue::String(content) => Ok(crate::lua::LuaToolResultParts {
            content: content.to_string_lossy(),
            is_error: false,
            metadata: None,
            display_content: Vec::new(),
            attachment: None,
        }),
        LuaValue::Table(result) => crate::lua::tool_result_from_lua_table(lua, result),
        LuaValue::Nil => Ok(crate::lua::LuaToolResultParts {
            content: String::new(),
            is_error: false,
            metadata: None,
            display_content: Vec::new(),
            attachment: None,
        }),
        other => Ok(crate::lua::LuaToolResultParts {
            content: format!("tool returned non-string value: {}", other.type_name()),
            is_error: true,
            metadata: None,
            display_content: Vec::new(),
            attachment: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_invocation(request_id: u64) -> ToolInvocationContext {
        ToolInvocationContext {
            invocation_id: protocol::InvocationId::new(request_id),
            request_id,
            execution_mode: protocol::ToolExecutionMode::Concurrent,
        }
    }

    fn lua_with_sleep() -> Lua {
        let lua = Lua::new();
        lua.load(
            r#"
            smelt = {}
            function smelt.sleep(ms)
              if not coroutine.isyieldable() then
                error("smelt.sleep: not inside a task", 2)
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
        rt.spawn(
            &lua,
            func,
            LuaMultiValue::new(),
            TaskCompletion::FireAndForget,
        )
        .unwrap();
        let out = rt.drive(&lua, Instant::now());
        assert!(out.is_empty());
        assert_eq!(rt.tasks.len(), 0);
    }

    #[test]
    fn noninteractive_external_wait_counts_against_deadline() {
        let lua = lua_with_sleep();
        let mut rt = LuaTaskRuntime::new();
        let func: mlua::Function = lua
            .load(
                r#"function()
                coroutine.yield({__yield = "external", id = 7})
                return "done"
              end"#,
            )
            .eval()
            .unwrap();
        let t0 = Instant::now();
        rt.spawn_scoped(
            &lua,
            func,
            LuaMultiValue::new(),
            TaskCompletion::FireAndForget,
            TaskScope::Turn,
            Some(TaskDeadline {
                at: t0 + Duration::from_millis(100),
                label_ms: 100,
                paused_at: None,
            }),
        )
        .unwrap();

        assert!(rt.drive(&lua, t0).is_empty());
        assert_eq!(rt.tasks.len(), 1);
        assert!(matches!(rt.tasks[0].wait, TaskWait::External { id: 7, .. }));

        let out = rt.drive(&lua, t0 + Duration::from_millis(101));
        assert!(
            matches!(out.as_slice(), [TaskDriveOutput::NotifyError(msg)] if msg.contains("timed out"))
        );
        assert!(rt.tasks.is_empty());
    }

    #[test]
    fn tool_timeout_reports_only_failing_completion() {
        let lua = lua_with_sleep();
        let mut rt = LuaTaskRuntime::new();
        let func: mlua::Function = lua.load("function() smelt.sleep(1000) end").eval().unwrap();
        let t0 = Instant::now();
        rt.spawn_scoped(
            &lua,
            func,
            LuaMultiValue::new(),
            TaskCompletion::ToolResult {
                invocation: tool_invocation(7),
                call_id: "slow-tool".into(),
            },
            TaskScope::Turn,
            Some(TaskDeadline {
                at: t0 + Duration::from_millis(100),
                label_ms: 100,
                paused_at: None,
            }),
        )
        .unwrap();

        assert!(rt.drive(&lua, t0).is_empty());
        let out = rt.drive(&lua, t0 + Duration::from_millis(101));

        assert!(matches!(
            out.as_slice(),
            [TaskDriveOutput::ToolComplete {
                call_id,
                content,
                is_error: true,
                ..
            }] if call_id == "slow-tool" && content == "tool error: timed out after 0.1s"
        ));
        assert!(rt.tasks.is_empty());
    }

    #[test]
    fn interactive_external_wait_pauses_deadline() {
        let lua = lua_with_sleep();
        let mut rt = LuaTaskRuntime::new();
        let func: mlua::Function = lua
            .load(
                r#"function()
                coroutine.yield({__yield = "external", id = 7, interactive = true})
                return "done"
              end"#,
            )
            .eval()
            .unwrap();
        let t0 = Instant::now();
        rt.spawn_scoped(
            &lua,
            func,
            LuaMultiValue::new(),
            TaskCompletion::FireAndForget,
            TaskScope::Turn,
            Some(TaskDeadline {
                at: t0 + Duration::from_millis(100),
                label_ms: 100,
                paused_at: None,
            }),
        )
        .unwrap();

        assert!(rt.drive(&lua, t0).is_empty());
        assert_eq!(rt.tasks.len(), 1);
        assert!(rt.tasks[0]
            .deadline
            .is_some_and(|deadline| deadline.paused_at == Some(t0)));

        assert!(rt.drive(&lua, t0 + Duration::from_millis(1_000)).is_empty());
        assert_eq!(rt.tasks.len(), 1);

        assert!(rt.resolve_external(7, LuaValue::Nil));
        let out = rt.drive(&lua, t0 + Duration::from_millis(1_000));
        assert!(out.is_empty());
        assert!(rt.tasks.is_empty());
    }

    #[test]
    fn sleep_yields_and_parks() {
        let lua = lua_with_sleep();
        let mut rt = LuaTaskRuntime::new();
        let func: mlua::Function = lua
            .load(
                r#"function()
                smelt.sleep(100)
                return "done"
              end"#,
            )
            .eval()
            .unwrap();
        rt.spawn(
            &lua,
            func,
            LuaMultiValue::new(),
            TaskCompletion::FireAndForget,
        )
        .unwrap();

        // First drive: task runs, yields sleep, parks.
        let t0 = Instant::now();
        let out = rt.drive(&lua, t0);
        assert!(out.is_empty());
        assert_eq!(rt.tasks.len(), 1);
        assert!(matches!(rt.tasks[0].wait, TaskWait::Sleep(_)));

        // Second drive before deadline - still parked.
        let out = rt.drive(&lua, t0 + Duration::from_millis(50));
        assert!(out.is_empty());
        assert_eq!(rt.tasks.len(), 1);

        // Third drive past deadline - resumes and completes.
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
            LuaMultiValue::new(),
            TaskCompletion::ToolResult {
                invocation: tool_invocation(7),
                call_id: "c1".into(),
            },
        )
        .unwrap();
        let out = rt.drive(&lua, Instant::now());
        assert_eq!(out.len(), 1);
        match &out[0] {
            TaskDriveOutput::ToolComplete {
                invocation,
                call_id,
                content,
                is_error,
                metadata,
                display_content,
                attachment,
            } => {
                assert_eq!(invocation.request_id, 7);
                assert_eq!(call_id, "c1");
                assert_eq!(content, "hello");
                assert!(!*is_error);
                assert!(metadata.is_none());
                assert!(display_content.is_empty());
                assert!(attachment.is_none());
            }
            _ => panic!("expected ToolComplete"),
        }
    }

    #[test]
    fn tool_result_extracts_retained_display_content_outside_metadata() {
        let lua = lua_with_sleep();
        let mut rt = LuaTaskRuntime::new();
        let func: mlua::Function = lua
            .load(
                r#"function()
                    return {
                      content = "edited",
                      metadata = { path = "src/lib.rs" },
                      display_content = {
                        old_content = "before",
                        new_content = "after",
                      },
                    }
                end"#,
            )
            .eval()
            .unwrap();
        rt.spawn(
            &lua,
            func,
            LuaMultiValue::new(),
            TaskCompletion::ToolResult {
                invocation: tool_invocation(8),
                call_id: "edit-1".into(),
            },
        )
        .unwrap();

        let out = rt.drive(&lua, Instant::now());
        let TaskDriveOutput::ToolComplete {
            metadata,
            display_content,
            ..
        } = &out[0]
        else {
            panic!("expected tool completion");
        };
        assert_eq!(metadata, &Some(serde_json::json!({ "path": "src/lib.rs" })));
        assert_eq!(display_content.len(), 2);
        assert!(display_content
            .iter()
            .any(|field| field.name == "old_content" && field.content.as_str() == "before"));
        assert!(display_content
            .iter()
            .any(|field| field.name == "new_content" && field.content.as_str() == "after"));
        assert!(metadata
            .as_ref()
            .is_none_or(|metadata| metadata.get("old_content").is_none()));
    }

    #[test]
    fn cyclic_tool_metadata_fails_without_recursing() {
        let lua = lua_with_sleep();
        let mut runtime = LuaTaskRuntime::new();
        let function: mlua::Function = lua
            .load(
                r#"function()
                    local metadata = {}
                    metadata.self = metadata
                    return { content = "bad", metadata = metadata }
                end"#,
            )
            .eval()
            .unwrap();
        runtime
            .spawn(
                &lua,
                function,
                LuaMultiValue::new(),
                TaskCompletion::ToolResult {
                    invocation: tool_invocation(9),
                    call_id: "cycle".into(),
                },
            )
            .unwrap();

        let output = runtime.drive(&lua, Instant::now());
        assert!(output.iter().any(
            |output| matches!(output, TaskDriveOutput::NotifyError(message) if message.contains("table cycle"))
        ));
        assert!(output.iter().any(|output| matches!(
            output,
            TaskDriveOutput::ToolComplete { content, is_error: true, metadata: None, .. }
                if content.contains("table cycle")
        )));
    }

    #[test]
    fn oversized_generic_tool_metadata_is_rejected_atomically() {
        let lua = lua_with_sleep();
        let mut runtime = LuaTaskRuntime::new();
        let function: mlua::Function = lua
            .load(format!(
                "function() return {{ content = 'bad', metadata = {{ payload = string.rep('x', {}) }} }} end",
                protocol::TOOL_METADATA_MAX_BYTES + 1
            ))
            .eval()
            .unwrap();
        runtime
            .spawn(
                &lua,
                function,
                LuaMultiValue::new(),
                TaskCompletion::ToolResult {
                    invocation: tool_invocation(10),
                    call_id: "oversized".into(),
                },
            )
            .unwrap();

        let output = runtime.drive(&lua, Instant::now());
        let completion = output
            .iter()
            .find_map(|output| match output {
                TaskDriveOutput::ToolComplete {
                    content,
                    is_error,
                    metadata,
                    display_content,
                    ..
                } => Some((content, is_error, metadata, display_content)),
                TaskDriveOutput::NotifyError(_) => None,
            })
            .unwrap();
        assert!(*completion.1);
        assert!(completion.0.contains("tool metadata exceeds"));
        assert!(completion.2.is_none());
        assert!(completion.3.is_empty());
    }

    #[test]
    fn historical_large_edit_metadata_is_promoted_before_bounding() {
        let lua = lua_with_sleep();
        let mut runtime = LuaTaskRuntime::new();
        let function: mlua::Function = lua
            .load(format!(
                "function() return {{ content = 'edited', metadata = {{ path = 'a.rs', old_content = string.rep('x', {}), new_content = 'after' }} }} end",
                protocol::TOOL_METADATA_MAX_BYTES * 2
            ))
            .eval()
            .unwrap();
        runtime
            .spawn(
                &lua,
                function,
                LuaMultiValue::new(),
                TaskCompletion::ToolResult {
                    invocation: tool_invocation(11),
                    call_id: "historical-edit".into(),
                },
            )
            .unwrap();

        let output = runtime.drive(&lua, Instant::now());
        let TaskDriveOutput::ToolComplete {
            metadata,
            display_content,
            is_error,
            ..
        } = &output[0]
        else {
            panic!("expected tool completion");
        };
        assert!(!is_error);
        assert_eq!(metadata, &Some(serde_json::json!({ "path": "a.rs" })));
        assert_eq!(
            display_content
                .iter()
                .find(|field| field.name == "old_content")
                .unwrap()
                .content
                .len(),
            protocol::TOOL_METADATA_MAX_BYTES * 2
        );
    }

    #[test]
    fn attachment_payload_bypasses_generic_metadata_budget() {
        let lua = lua_with_sleep();
        let mut runtime = LuaTaskRuntime::new();
        let function: mlua::Function = lua
            .load(format!(
                "function() return {{ content = 'attached', metadata = {{ kind = 'file_attachment', modality = 'image', mime = 'image/png', data_url = 'data:image/png;base64,' .. string.rep('a', {}) }} }} end",
                protocol::TOOL_METADATA_MAX_BYTES * 2
            ))
            .eval()
            .unwrap();
        runtime
            .spawn(
                &lua,
                function,
                LuaMultiValue::new(),
                TaskCompletion::ToolResult {
                    invocation: tool_invocation(12),
                    call_id: "attachment".into(),
                },
            )
            .unwrap();

        let output = runtime.drive(&lua, Instant::now());
        let TaskDriveOutput::ToolComplete {
            metadata,
            attachment,
            is_error,
            ..
        } = &output[0]
        else {
            panic!("expected tool completion");
        };
        assert!(!is_error);
        assert!(metadata.as_ref().unwrap().get("data_url").is_none());
        assert_eq!(
            attachment.as_ref().unwrap().data_url.len(),
            "data:image/png;base64,".len() + protocol::TOOL_METADATA_MAX_BYTES * 2
        );
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
            LuaMultiValue::new(),
            TaskCompletion::ToolResult {
                invocation: tool_invocation(1),
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
    fn handler_error_reports_notification_and_tool_error() {
        let lua = lua_with_sleep();
        let mut rt = LuaTaskRuntime::new();
        let func: mlua::Function = lua.load(r#"function() error("bang") end"#).eval().unwrap();
        rt.spawn(
            &lua,
            func,
            LuaMultiValue::new(),
            TaskCompletion::ToolResult {
                invocation: tool_invocation(2),
                call_id: "y".into(),
            },
        )
        .unwrap();
        let out = rt.drive(&lua, Instant::now());
        let has_notification = out
            .iter()
            .any(|o| matches!(o, TaskDriveOutput::NotifyError(m) if m.contains("bang")));
        let has_tool_err = out
            .iter()
            .any(|o| matches!(o, TaskDriveOutput::ToolComplete { is_error: true, .. }));
        assert!(has_notification);
        assert!(has_tool_err);
        assert_eq!(rt.tasks.len(), 0);
    }

    #[test]
    fn sleep_outside_task_errors() {
        let lua = lua_with_sleep();
        let res: LuaResult<()> = lua.load("smelt.sleep(10)").exec();
        assert!(res.is_err());
        let msg = format!("{}", res.unwrap_err());
        assert!(msg.contains("not inside a task"));
    }

    #[test]
    fn cancel_all_resolves_sleep_with_cancel_marker() {
        let lua = lua_with_sleep();
        let mut rt = LuaTaskRuntime::new();
        let func: mlua::Function = lua
            .load(
                r#"function()
                local r = smelt.sleep(100)
                return r
              end"#,
            )
            .eval()
            .unwrap();
        rt.spawn(
            &lua,
            func,
            LuaMultiValue::new(),
            TaskCompletion::FireAndForget,
        )
        .unwrap();

        // First drive parks on sleep.
        let out = rt.drive(&lua, Instant::now());
        assert!(out.is_empty());
        assert_eq!(rt.tasks.len(), 1);
        assert!(matches!(rt.tasks[0].wait, TaskWait::Sleep(_)));

        // Cancel all tasks.
        rt.cancel_all(&lua);
        assert!(matches!(rt.tasks[0].wait, TaskWait::Ready(_)));

        // Next drive resumes with cancel marker and finishes.
        let out = rt.drive(&lua, Instant::now());
        assert!(out.is_empty());
        assert_eq!(rt.tasks.len(), 0);
    }

    #[test]
    fn cancel_all_resolves_external_with_cancel_marker() {
        let lua = lua_with_sleep();
        let mut rt = LuaTaskRuntime::new();
        let func: mlua::Function = lua
            .load(
                r#"function()
                local r = coroutine.yield({__yield = "external", id = 42})
                return r
              end"#,
            )
            .eval()
            .unwrap();
        rt.spawn(
            &lua,
            func,
            LuaMultiValue::new(),
            TaskCompletion::FireAndForget,
        )
        .unwrap();

        // First drive parks on external.
        let out = rt.drive(&lua, Instant::now());
        assert!(out.is_empty());
        assert_eq!(rt.tasks.len(), 1);
        assert!(matches!(
            rt.tasks[0].wait,
            TaskWait::External { id: 42, .. }
        ));

        // Cancel all tasks.
        rt.cancel_all(&lua);
        assert!(matches!(rt.tasks[0].wait, TaskWait::Ready(_)));

        // Next drive resumes with cancel marker and finishes.
        let out = rt.drive(&lua, Instant::now());
        assert!(out.is_empty());
        assert_eq!(rt.tasks.len(), 0);
    }

    #[test]
    fn cancelled_task_that_yields_again_gets_marker() {
        let lua = lua_with_sleep();
        let mut rt = LuaTaskRuntime::new();
        // Task does some sync work, then sleeps.
        let func: mlua::Function = lua
            .load(
                r#"function()
                local x = 1
                local r = smelt.sleep(100)
                return r
              end"#,
            )
            .eval()
            .unwrap();
        rt.spawn(
            &lua,
            func,
            LuaMultiValue::new(),
            TaskCompletion::FireAndForget,
        )
        .unwrap();

        // Cancel before first drive.
        rt.cancel_all(&lua);

        // Drive: task runs, yields sleep (rewritten to Ready(cancel_marker)
        // by step_task_owned), then is picked up again in the same `drive`
        // loop, resumed with the cancel marker, and finishes.
        let out = rt.drive(&lua, Instant::now());
        assert!(out.is_empty());
        assert_eq!(rt.tasks.len(), 0);
    }

    #[test]
    fn cancel_scope_only_cancels_matching_tasks() {
        let lua = lua_with_sleep();
        let mut rt = LuaTaskRuntime::new();
        let app_func: mlua::Function = lua
            .load(r#"function() smelt.sleep(1000) end"#)
            .eval()
            .unwrap();
        let turn_func: mlua::Function = lua
            .load(r#"function() smelt.sleep(1000) end"#)
            .eval()
            .unwrap();
        rt.spawn_scoped(
            &lua,
            app_func,
            LuaMultiValue::new(),
            TaskCompletion::FireAndForget,
            TaskScope::App,
            None,
        )
        .unwrap();
        rt.spawn_scoped(
            &lua,
            turn_func,
            LuaMultiValue::new(),
            TaskCompletion::FireAndForget,
            TaskScope::Turn,
            None,
        )
        .unwrap();

        let now = Instant::now();
        assert!(rt.drive(&lua, now).is_empty());
        assert_eq!(rt.tasks.len(), 2);

        rt.cancel_scope(&lua, TaskScope::Turn);
        assert!(rt.drive(&lua, now).is_empty());
        assert_eq!(rt.tasks.len(), 1);
        assert_eq!(rt.tasks[0].scope, TaskScope::App);
    }

    #[test]
    fn cancelled_tool_completion_is_not_task_error() {
        let lua = Lua::new();
        lua.load(
            r#"
            smelt = {}
            local function yield_with_cancel(payload)
              local result = coroutine.yield(payload)
              if type(result) == "table" and result.__cancelled then
                error("cancelled", 0)
              end
              return result
            end
            function smelt.sleep(ms)
              return yield_with_cancel({__yield = "sleep", ms = ms})
            end
            "#,
        )
        .exec()
        .unwrap();
        let mut rt = LuaTaskRuntime::new();
        let func: mlua::Function = lua
            .load(r#"function() smelt.sleep(1000); return "done" end"#)
            .eval()
            .unwrap();
        rt.spawn_scoped(
            &lua,
            func,
            LuaMultiValue::new(),
            TaskCompletion::ToolResult {
                invocation: tool_invocation(42),
                call_id: "call-cancel".into(),
            },
            TaskScope::Turn,
            None,
        )
        .unwrap();

        let now = Instant::now();
        assert!(rt.drive(&lua, now).is_empty());
        rt.cancel_scope(&lua, TaskScope::Turn);
        let out = rt.drive(&lua, now);
        assert!(
            out.is_empty(),
            "cancelled tool task should not emit Lua task output: {out:?}"
        );
    }

    #[test]
    fn uncancelled_cancelled_error_is_reported() {
        let lua = lua_with_sleep();
        let mut rt = LuaTaskRuntime::new();
        let func: mlua::Function = lua
            .load(r#"function() error("cancelled") end"#)
            .eval()
            .unwrap();
        rt.spawn(
            &lua,
            func,
            LuaMultiValue::new(),
            TaskCompletion::FireAndForget,
        )
        .unwrap();

        let out = rt.drive(&lua, Instant::now());
        assert!(
            out.iter().any(
                |o| matches!(o, TaskDriveOutput::NotifyError(msg) if msg.contains("cancelled"))
            ),
            "plain user error named cancelled should be reported: {out:?}"
        );
    }
}
