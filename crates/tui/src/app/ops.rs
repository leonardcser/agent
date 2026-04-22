//! Typed effect ops applied by the app reducer.
//!
//! `AppOp` is the single funnel for deferred state mutations. Lua
//! handlers and Rust dialog callbacks both queue ops into the same
//! channel; the app drains them each tick and dispatches through
//! `App::apply_ops`. One reducer, one mutation log.

use std::sync::Arc;

use crate::lua::LuaShared;

/// Cloneable push-only handle to the shared `AppOp` queue. Rust
/// dialog callbacks clone this and call [`OpsHandle::push`] from
/// inside their closures to request App-level effects. Obtained
/// via `LuaRuntime::ops_handle()`.
#[derive(Clone)]
pub struct OpsHandle(pub(crate) Arc<LuaShared>);

impl OpsHandle {
    pub fn push(&self, op: AppOp) {
        if let Ok(mut o) = self.0.ops.lock() {
            o.ops.push(op);
        }
    }
}

/// Deferred mutation queued by a handler. Applied by the app loop
/// after the handler returns, avoiding nested borrows on `App`.
pub enum AppOp {
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
    // ── Dialog effects (queued by migrated dialog callbacks) ───────
    /// Close a float (universal). Runs the same cleanup path as the
    /// legacy `close_float`: fires any Lua dismiss callback, removes
    /// the window from the compositor, deletes the primary buf.
    CloseFloat(ui::WinId),
    /// Export transcript to clipboard (Export dialog).
    ExportClipboard,
    /// Export transcript to file (Export dialog).
    ExportFile,
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
        session_entries: Vec<crate::render::PermissionEntry>,
        workspace_rules: Vec<crate::workspace_permissions::Rule>,
    },
    /// Load a saved session by id (Resume dialog).
    LoadSession(String),
    /// Resolve an open `ask_user_question` dialog. `answer=None`
    /// cancels (denial path clears queued tool calls via the
    /// `pending_agent_clear_pending` flag).
    ResolveQuestion {
        answer: Option<String>,
        request_id: u64,
    },
    /// Open the Agents list dialog. Used both at command entry and
    /// as the back-navigation from the Agents detail view.
    OpenAgentsList {
        initial_selected: usize,
    },
    /// Swap the Agents list for the detail view of a specific
    /// subagent. `parent_selected` preserves the list cursor for
    /// when detail is dismissed.
    OpenAgentsDetail {
        agent_id: String,
        parent_selected: usize,
    },
    /// Refresh the cached subagent counts on the status bar — fires
    /// when the Agents list is dismissed.
    RefreshAgentCounts,
    /// Resolve an open Confirm dialog with the user's choice. Drives
    /// the same logic as the legacy `App::resolve_confirm`. Sets
    /// `pending_agent_cancel` internally when the resolution asks
    /// the turn to cancel.
    ResolveConfirm {
        choice: crate::render::ConfirmChoice,
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
    /// (`smelt.api.engine.ask`). Applied by sending a matching
    /// `UiCommand::EngineAsk` to the engine.
    EngineAsk {
        id: u64,
        system: String,
        messages: Vec<protocol::Message>,
        task: protocol::AuxiliaryTask,
    },
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
    WinOpenFloat {
        buf_id: u64,
        title: String,
        footer_items: Vec<String>,
        accent: Option<crossterm::style::Color>,
    },
    WinUpdate {
        id: u64,
        title: Option<String>,
    },
    WinClose {
        id: u64,
    },
    /// Send a deferred plugin tool result.
    ResolveToolResult {
        request_id: u64,
        call_id: String,
        content: String,
        is_error: bool,
    },
    /// Resolve a Lua-driven dialog opened via `smelt.api.dialog.open`.
    /// Fires the optional per-option `on_select` callback, then resumes
    /// the parked task with a `{action, option_index, inputs}` table.
    ResolveLuaDialog {
        dialog_id: u64,
        action: String,
        option_index: Option<usize>,
        inputs: Vec<(String, String)>,
        on_select: Option<mlua::RegistryKey>,
    },
}
