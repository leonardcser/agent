//! `CompleterKind::ArgPicker` — the Rust-backed version of what Lua
//! plugins used to build by hand on top of `ui::Picker` + prompt-local
//! keymaps. Opening one is a single op round-trip; the completer owns
//! navigation, filtering, and accept/dismiss semantics, and resumes
//! the caller's parked task with `{index, item}` (accept) or `nil`
//! (dismiss) on close.

use super::{Completer, CompleterKind, CompletionItem};

/// Lua-side handles attached to an ArgPicker session.
///
/// * `task_id` — allocated by Lua before it queued the open op.
///   Resuming this id with a value unparks the coroutine that called
///   `smelt.prompt.open_picker`.
/// * `on_select` — optional Lua function fired on every move so the
///   plugin can live-preview the selection (theme picker flashes the
///   accent). `None` when the plugin didn't pass `on_select`.
/// * `stay_open_on_accept` — when true, Enter fires `on_accept` (if
///   provided) and rebuilds the item list without closing. Used by
///   the settings picker so toggling one setting keeps the UI open.
/// * `on_accept` — fired on Enter when `stay_open_on_accept` is true.
///   The callback can return a new items array to refresh the list.
/// * `command_prefix` — e.g. `"/theme"`. When set, Enter resolves the
///   task AND auto-submits `/theme <label>` as a full command. When
///   `None`, the session just resolves the task and closes (caller
///   handles the command side-effect themselves).
#[derive(Debug, Clone)]
pub struct ArgPickerHandles {
    pub task_id: u64,
    pub on_select: Option<ArgPickerKey>,
    pub on_accept: Option<ArgPickerKey>,
    pub stay_open_on_accept: bool,
    pub command_prefix: Option<String>,
}

/// Opaque handle to a Lua function stored in the runtime registry.
/// Dropped via `LuaRuntime::remove_callback` when the session ends.
#[derive(Debug, Clone, Copy)]
pub struct ArgPickerKey(pub u64);

impl Completer {
    /// Build an ArgPicker completer. `anchor` is the byte offset in the
    /// prompt buffer where the filter query starts — usually 0 so the
    /// entire buffer becomes the query, but a plugin could anchor after
    /// a command prefix if it wanted.
    pub fn arg_picker(anchor: usize, items: Vec<CompletionItem>) -> Self {
        let results = items.clone();
        Self {
            anchor,
            kind: CompleterKind::ArgPicker,
            query: String::new(),
            results,
            selected: 0,
            all_items: items,
            selected_key: None,
        }
    }
}
