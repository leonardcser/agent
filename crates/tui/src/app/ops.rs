//! Typed effect ops applied by the app reducer.
//!
//! `AppOp` is the single funnel for deferred state mutations. Lua
//! handlers and Rust dialog callbacks both queue ops into the same
//! channel; the app drains them each tick and dispatches through
//! `App::apply_ops`. One reducer, one mutation log.
//!
//! Ops partition into two buckets:
//!
//! - **`UiOp`** — pure compositor / buffer / window primitives plus
//!   ephemeral UI state (notifications, ghost text). No engine calls,
//!   no app-state mutation beyond the ui layer. Safe to apply in any
//!   order with respect to domain effects as long as the ui dispatch
//!   is ordered among itself.
//! - **`DomainOp`** — app-state mutations, engine commands, session
//!   lifecycle, permission + agent + process control, tool resolution.
//!   These reach into the agent loop, persistence, or the Lua runtime.
//!
//! Handlers push into the shared queue via `Into<AppOp>`: callers
//! write `ops.push(UiOp::BufCreate { id })` or `ops.push(DomainOp::
//! LoadSession(id))` and the queue stays a single ordered stream so
//! the reducer can apply ui + domain effects in the exact sequence a
//! handler emitted them.

use std::sync::Arc;

use crate::lua::LuaShared;

/// Cloneable push-only handle to the shared `AppOp` queue. Rust
/// dialog callbacks clone this and call [`OpsHandle::push`] from
/// inside their closures to request App-level effects. Obtained
/// via `LuaRuntime::ops_handle()`.
#[derive(Clone)]
pub struct OpsHandle(pub(crate) Arc<LuaShared>);

impl OpsHandle {
    /// Push any op that converts into an `AppOp` — `UiOp`, `DomainOp`,
    /// or a pre-built `AppOp`.
    pub fn push<O: Into<AppOp>>(&self, op: O) {
        if let Ok(mut o) = self.0.ops.lock() {
            o.ops.push(op.into());
        }
    }

    /// Push a task-runtime event (dialog resolution, keymap-fired
    /// callback, …). These drain through `LuaRuntime::pump_task_events`
    /// each tick, *not* through the `AppOp` reducer — the reducer
    /// doesn't need to know about Lua-task lifecycle.
    pub fn push_task_event(&self, ev: crate::lua::TaskEvent) {
        if let Ok(mut inbox) = self.0.task_inbox.lock() {
            inbox.push(ev);
        }
    }

    /// Remove a callback id from `shared.callbacks`. Used by dialog
    /// close paths to clean up `on_press` handles so the registry
    /// doesn't leak.
    pub fn remove_callback(&self, id: u64) {
        if let Ok(mut cbs) = self.0.callbacks.lock() {
            cbs.remove(&id);
        }
    }
}

/// Top-level tagged op carried on the shared queue. Handlers almost
/// never construct this directly — they push a `UiOp` or `DomainOp`
/// and rely on `Into<AppOp>`. The wrapper exists so the reducer can
/// dispatch each variant through the correct sub-handler while
/// preserving handler emission order across buckets.
pub enum AppOp {
    Ui(UiOp),
    Domain(DomainOp),
}

impl From<UiOp> for AppOp {
    fn from(op: UiOp) -> Self {
        AppOp::Ui(op)
    }
}

impl From<DomainOp> for AppOp {
    fn from(op: DomainOp) -> Self {
        AppOp::Domain(op)
    }
}

/// Pure compositor / window / buffer primitives plus UI chrome.
pub enum UiOp {
    /// Show a transient notification toast.
    Notify(String),
    /// Show a transient error toast (accent-styled).
    NotifyError(String),
    /// Close a float. Fires any Lua dismiss callback, removes the
    /// window from the compositor, deletes the primary buf.
    CloseFloat(ui::WinId),
    /// Set ghost text (predicted input) on the prompt.
    SetGhostText(String),
    /// Clear ghost text from the prompt.
    ClearGhostText,
    BufCreate {
        id: u64,
    },
    BufSetLines {
        id: u64,
        lines: Vec<String>,
    },
    /// Paint a highlight over `[col_start, col_end)` on `line`. The
    /// plugin side resolves theme role names to `Color` at push time,
    /// so the reducer just stitches a `SpanStyle` together. Out-of-
    /// range line indices are silently dropped — the buffer may have
    /// shrunk between the plugin's queue and the op's apply.
    BufAddHighlight {
        id: u64,
        line: usize,
        col_start: u16,
        col_end: u16,
        fg: Option<crossterm::style::Color>,
        bold: bool,
        italic: bool,
        dim: bool,
    },
    /// Register a Lua fn (previously stashed in `shared.callbacks`
    /// under `callback_id`) as a `Callback::Lua` keymap on `win`.
    /// Pushed by `smelt.win.set_keymap`; the reducer does the
    /// actual `ui.win_set_keymap` call with a `Callback::Lua(LuaHandle
    /// (callback_id))`.
    WinBindLuaKeymap {
        win: ui::WinId,
        key: ui::KeyBind,
        callback_id: u64,
    },
    /// Register a Lua fn as a `Callback::Lua` lifecycle event handler on
    /// `win`. Pushed by `smelt.win.on_event`; the reducer calls
    /// `ui.win_on_event(win, event, Callback::Lua(LuaHandle(id)))`.
    WinBindLuaEvent {
        win: ui::WinId,
        event: ui::WinEvent,
        callback_id: u64,
    },
    /// Drive the `ui::Picker` at `win` to a new 0-based `index`. Pushed
    /// by `smelt.ui.picker.set_selected` from Lua-side nav keymaps in
    /// `runtime/lua/smelt/picker.lua`; the reducer calls
    /// `ui.picker_mut(win).set_selected(index)`.
    PickerSetSelected {
        win: ui::WinId,
        index: usize,
    },
    /// Open a Lua-described dialog float. Pushed by
    /// `smelt.ui.dialog._request_open(task_id, opts)`. The reducer
    /// runs `lua_dialog::open`, then resolves the parked
    /// `TaskWait::External(task_id)` with `{win_id = …}` (or `nil` on
    /// error). Lua code owns all keymap/event wiring in
    /// `runtime/lua/smelt/dialog.lua`.
    OpenLuaDialog {
        task_id: u64,
        opts: mlua::RegistryKey,
    },
    /// Open a Lua-described picker float. Same shape as `OpenLuaDialog`,
    /// dispatching through `lua_picker::open`.
    OpenLuaPicker {
        task_id: u64,
        opts: mlua::RegistryKey,
    },
}

/// App-state mutations, engine commands, and session/agent/permission
/// control. Anything that reaches past the compositor into the agent
/// loop, persistence, or the Lua runtime.
pub enum DomainOp {
    RunCommand(String),
    SetMode(String),
    SetModel(String),
    SetReasoningEffort(String),
    Cancel,
    Compact(Option<String>),
    Submit(String),
    SetPromptSection(String, String),
    RemovePromptSection(String),
    /// Rewind to a transcript block (Rewind dialog). `block_idx=None`
    /// means "kept at current"; `restore_vim_insert` re-enters Insert
    /// mode when the dialog was opened from there.
    RewindToBlock {
        block_idx: Option<usize>,
        restore_vim_insert: bool,
    },
    /// Sync the App's permission state with what the Permissions
    /// dialog has in memory. Fired on dismiss.
    SyncPermissions {
        session_entries: Vec<crate::app::transcript_model::PermissionEntry>,
        workspace_rules: Vec<crate::workspace_permissions::Rule>,
    },
    /// Load a saved session by id (Resume dialog, `smelt.session.load`).
    LoadSession(String),
    /// Delete a saved session by id (`smelt.session.delete`). No-op
    /// if the session is the active one.
    DeleteSession(String),
    /// Kill a running subagent by PID (`smelt.agent.kill`). Runs
    /// SIGTERM to the whole subtree, deregisters, and cleans up its
    /// socket. No-op when the PID isn't in the registry anymore.
    KillAgent(u32),
    /// Resolve an open Confirm dialog with the user's choice. Sets
    /// `pending_agent_cancel` internally when the resolution asks
    /// the turn to cancel.
    ResolveConfirm {
        choice: crate::app::transcript_model::ConfirmChoice,
        message: Option<String>,
        request_id: u64,
        call_id: String,
        tool_name: String,
    },
    /// BackTab pressed on an open Confirm dialog. Toggles the app
    /// mode and, if the new mode auto-allows the pending tool call,
    /// sends approval + closes the dialog. Otherwise the dialog stays
    /// open so the user can still choose manually.
    ConfirmBackTab {
        win: ui::WinId,
        request_id: u64,
        call_id: String,
        tool_name: String,
        args: std::collections::HashMap<String, serde_json::Value>,
    },
    /// One-shot LLM call initiated by a Lua plugin
    /// (`smelt.engine.ask`). Applied by sending a matching
    /// `UiCommand::EngineAsk` to the engine.
    EngineAsk {
        id: u64,
        system: String,
        messages: Vec<protocol::Message>,
        task: protocol::AuxiliaryTask,
    },
    /// Send a deferred plugin tool result.
    ResolveToolResult {
        request_id: u64,
        call_id: String,
        content: String,
        is_error: bool,
    },
    /// Kill a background process by id (e.g. from `/ps`). The actual
    /// `ProcessRegistry::stop` runs on the tokio runtime, fire-and-forget.
    KillProcess(String),
    /// Copy the transcript block under the cursor to the clipboard.
    /// Notifies success / failure. Used by `/yank-block`.
    YankBlockAtCursor,
}
