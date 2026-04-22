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
}
