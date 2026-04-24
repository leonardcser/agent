//! Per-window keymap and event callback registry.
//!
//! Every window can register callbacks keyed by:
//! - `(WinId, KeyBind)` — a specific key chord on this window.
//! - `(WinId, WinEvent)` — a lifecycle / semantic event.
//!
//! Callbacks are either Rust closures (`FnMut(&mut CallbackCtx) ->
//! CallbackResult`) or Lua handles. Both run through the same
//! dispatcher in `Ui::handle_key` / `Ui::dispatch_event`. Side
//! effects flow through the app-owned `AppOp` queue that Rust
//! callbacks see via their shared ops handle, or through direct
//! `ui::Ui` mutations — no return channel for effect strings.
//!
//! This is the single behavior mechanism. No `Dialog` /
//! `DialogBehavior` trait exists; `Component::handle_key` remains as
//! the fallback for generic nav when no keymap matches.
use crate::WinId;
use crossterm::event::{KeyCode, KeyModifiers};
use std::collections::HashMap;

/// A keyboard chord. Stored as the key plus modifier bitset so the
/// lookup key is `Hash + Eq` without depending on crossterm's own
/// hashing behavior.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct KeyBind {
    pub code: KeyCode,
    pub mods: KeyModifiers,
}

impl KeyBind {
    pub fn new(code: KeyCode, mods: KeyModifiers) -> Self {
        Self { code, mods }
    }

    pub fn plain(code: KeyCode) -> Self {
        Self {
            code,
            mods: KeyModifiers::NONE,
        }
    }

    pub fn char(c: char) -> Self {
        Self::plain(KeyCode::Char(c))
    }

    pub fn ctrl(c: char) -> Self {
        Self {
            code: KeyCode::Char(c),
            mods: KeyModifiers::CONTROL,
        }
    }
}

/// Window lifecycle / semantic events. Dialogs with typed payloads
/// use the richer variants in `Payload` at invocation time.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum WinEvent {
    /// Fired after the window opens and is registered in the
    /// compositor.
    Open,
    /// Fired just before the window closes; callback may push close-
    /// cleanup actions.
    Close,
    /// Fired when the focus stack moves to this window.
    FocusGained,
    /// Fired when focus leaves this window.
    FocusLost,
    /// List/Dialog cursor moved to a different row.
    SelectionChanged,
    /// Enter pressed on a List (payload carries `index`) or Input
    /// (payload carries `text`). Apps bind this instead of binding
    /// `Enter` directly so the dialog doesn't need to parse its own
    /// selection.
    Submit,
    /// Input buffer edited (payload carries the new text).
    TextChanged,
    /// User triggered dismissal (Esc or a configured dismiss key).
    Dismiss,
    /// Fired once per event-loop iteration on each registered window.
    /// Used by floats that need to refresh their content from live
    /// external state (subagent registry, process list, etc.).
    Tick,
}

/// Payload attached to a callback invocation. The variants map 1:1
/// to the invocation sites in the routing layer.
#[derive(Clone, Debug)]
pub enum Payload {
    None,
    Key { code: KeyCode, mods: KeyModifiers },
    Selection { index: usize },
    Text { content: String },
}

impl Payload {
    pub fn as_selection(&self) -> Option<usize> {
        match self {
            Payload::Selection { index } => Some(*index),
            _ => None,
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Payload::Text { content } => Some(content.as_str()),
            _ => None,
        }
    }

    /// String form used when routing to Lua callbacks.
    pub fn as_lua_string(&self) -> String {
        match self {
            Payload::None => String::new(),
            Payload::Key { code, mods } => format!("{code:?}:{mods:?}"),
            Payload::Selection { index } => index.to_string(),
            Payload::Text { content } => content.clone(),
        }
    }
}

/// Result returned by a callback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CallbackResult {
    /// Callback handled the event; no further routing.
    Consumed,
    /// Callback passes; fall through to `Component::handle_key`
    /// (for keymap callbacks) or do nothing (for event callbacks).
    Pass,
}

/// Handle to a Lua callback, opaque to the `ui` crate. The `tui`
/// crate's Lua runtime owns the actual function; this registry only
/// stores the handle and routes `(WinId, payload)` back.
#[derive(Clone, Copy, Debug)]
pub struct LuaHandle(pub u64);

/// Rust-side callback closure. Boxed `FnMut` with full mutable
/// access to `Ui` via the ctx, plus the shared `actions` buffer
/// for app-level effects.
pub type RustCallback = Box<dyn FnMut(&mut CallbackCtx<'_>) -> CallbackResult>;

/// A callback is either a Rust closure or a Lua handle. Both share
/// the same (WinId, KeyBind/WinEvent) registry and dispatch path.
pub enum Callback {
    Rust(RustCallback),
    Lua(LuaHandle),
}

impl std::fmt::Debug for Callback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Callback::Rust(_) => f.write_str("Callback::Rust(<closure>)"),
            Callback::Lua(h) => write!(f, "Callback::Lua({})", h.0),
        }
    }
}

/// Context passed to Rust callbacks. `ui` is full `&mut Ui`;
/// callbacks can mutate buffers, open/close floats, change focus,
/// and queue `AppOp`s via the shared ops handle. No return channel
/// for effect strings — all side effects flow through `AppOp` or
/// direct `ui::Ui` mutation.
pub struct CallbackCtx<'a> {
    pub ui: &'a mut crate::Ui,
    pub win: WinId,
    pub payload: Payload,
}

/// Per-window callback registry owned by `Ui`. Keyed by WinId so
/// closing a window removes all its bindings cleanly.
#[derive(Default)]
pub struct Callbacks {
    keymaps: HashMap<WinId, HashMap<KeyBind, Callback>>,
    events: HashMap<WinId, HashMap<WinEvent, Vec<Callback>>>,
    /// Per-window fallback key handler tried after specific `keymaps`
    /// miss and before `Component::handle_key` runs. Useful for
    /// catch-all filter inputs (`Resume` types any printable char
    /// into its query buffer) where enumerating every chord would
    /// be absurd.
    key_fallback: HashMap<WinId, Callback>,
}

impl Callbacks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install a (win, key) keymap. Returns the displaced `Callback`
    /// (if any), so callers with `Callback::Lua` bindings can drop the
    /// stale `LuaHandle` from their side registry.
    #[must_use]
    pub fn set_keymap(&mut self, win: WinId, key: KeyBind, cb: Callback) -> Option<Callback> {
        self.keymaps.entry(win).or_default().insert(key, cb)
    }

    pub fn clear_keymap(&mut self, win: WinId, key: KeyBind) -> Option<Callback> {
        self.keymaps.get_mut(&win).and_then(|t| t.remove(&key))
    }

    /// Remove a specific event callback identified by its Lua handle id.
    /// Lua plugins attach picker-lifetime handlers to other windows (e.g.
    /// `on_event(prompt, "text_changed", …)`); they need a way to tear
    /// down exactly their own binding without nuking co-existing ones.
    pub fn clear_event_by_id(&mut self, win: WinId, ev: WinEvent, id: u64) -> Option<Callback> {
        let list = self.events.get_mut(&win)?.get_mut(&ev)?;
        let pos = list
            .iter()
            .position(|cb| matches!(cb, Callback::Lua(LuaHandle(h)) if *h == id))?;
        Some(list.remove(pos))
    }

    pub fn on_event(&mut self, win: WinId, ev: WinEvent, cb: Callback) {
        self.events
            .entry(win)
            .or_default()
            .entry(ev)
            .or_default()
            .push(cb);
    }

    /// Remove every binding for `win`. Returns the IDs of all
    /// `Callback::Lua` handles that were attached, so the caller can
    /// drop them from the Lua-side registry.
    #[must_use]
    pub fn clear_all(&mut self, win: WinId) -> Vec<u64> {
        let mut lua_ids = Vec::new();
        if let Some(table) = self.keymaps.remove(&win) {
            for cb in table.into_values() {
                if let Callback::Lua(LuaHandle(id)) = cb {
                    lua_ids.push(id);
                }
            }
        }
        if let Some(events) = self.events.remove(&win) {
            for cbs in events.into_values() {
                for cb in cbs {
                    if let Callback::Lua(LuaHandle(id)) = cb {
                        lua_ids.push(id);
                    }
                }
            }
        }
        if let Some(Callback::Lua(LuaHandle(id))) = self.key_fallback.remove(&win) {
            lua_ids.push(id);
        }
        lua_ids
    }

    /// Register a per-window fallback key handler. Runs after
    /// specific `keymaps` miss and before `Component::handle_key`.
    /// Returns the displaced `Callback` (if any) so Lua-side handles
    /// can be cleaned up.
    #[must_use]
    pub fn set_key_fallback(&mut self, win: WinId, cb: Callback) -> Option<Callback> {
        self.key_fallback.insert(win, cb)
    }

    pub(crate) fn take_key_fallback(&mut self, win: WinId) -> Option<Callback> {
        self.key_fallback.remove(&win)
    }

    pub(crate) fn restore_key_fallback(&mut self, win: WinId, cb: Callback) {
        self.key_fallback.insert(win, cb);
    }

    /// True when at least one callback is registered for `(win, ev)`.
    /// Used by the auto-dispatch path to decide whether to translate
    /// widget `KeyResult::Action` strings into event dispatches.
    pub fn has_event(&self, win: WinId, ev: WinEvent) -> bool {
        self.events
            .get(&win)
            .and_then(|t| t.get(&ev))
            .is_some_and(|v| !v.is_empty())
    }

    /// List every window that has at least one callback registered
    /// for `ev`. Used by `Ui::dispatch_tick`.
    pub fn wins_with_event(&self, ev: WinEvent) -> Vec<WinId> {
        self.events
            .iter()
            .filter_map(|(win, table)| table.get(&ev).filter(|v| !v.is_empty()).map(|_| *win))
            .collect()
    }

    /// Remove and return a keymap callback so it can be invoked with
    /// `&mut Ui`. Caller must put it back via `restore_keymap` after
    /// the callback returns. Removal + restore avoids a
    /// reentrant-borrow conflict with `&mut Ui` inside the callback.
    pub(crate) fn take_keymap(&mut self, win: WinId, key: KeyBind) -> Option<Callback> {
        self.keymaps.get_mut(&win)?.remove(&key)
    }

    pub(crate) fn restore_keymap(&mut self, win: WinId, key: KeyBind, cb: Callback) {
        self.keymaps.entry(win).or_default().insert(key, cb);
    }

    /// Same take/restore pattern for event callbacks. Multiple
    /// callbacks can be registered per event; we take the whole
    /// `Vec` and restore it after all are invoked.
    pub(crate) fn take_event(&mut self, win: WinId, ev: WinEvent) -> Option<Vec<Callback>> {
        self.events.get_mut(&win)?.remove(&ev)
    }

    pub(crate) fn restore_event(&mut self, win: WinId, ev: WinEvent, cbs: Vec<Callback>) {
        self.events.entry(win).or_default().insert(ev, cbs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wid(n: u64) -> WinId {
        WinId(n)
    }

    #[test]
    fn set_and_take_keymap() {
        let mut cbs = Callbacks::new();
        let key = KeyBind::plain(KeyCode::Enter);
        let _ = cbs.set_keymap(wid(1), key, Callback::Lua(LuaHandle(42)));
        let taken = cbs.take_keymap(wid(1), key);
        assert!(matches!(taken, Some(Callback::Lua(LuaHandle(42)))));
        assert!(cbs.take_keymap(wid(1), key).is_none());
    }

    #[test]
    fn clear_all_removes_both_tables() {
        let mut cbs = Callbacks::new();
        let _ = cbs.set_keymap(wid(1), KeyBind::char('q'), Callback::Lua(LuaHandle(1)));
        cbs.on_event(wid(1), WinEvent::Submit, Callback::Lua(LuaHandle(2)));
        let _ = cbs.clear_all(wid(1));
        assert!(cbs.take_keymap(wid(1), KeyBind::char('q')).is_none());
        assert!(cbs.take_event(wid(1), WinEvent::Submit).is_none());
    }

    #[test]
    fn payload_lua_string() {
        assert_eq!(Payload::None.as_lua_string(), "");
        assert_eq!(Payload::Selection { index: 3 }.as_lua_string(), "3");
        assert_eq!(
            Payload::Text {
                content: "hi".into()
            }
            .as_lua_string(),
            "hi"
        );
    }

    #[test]
    fn keybind_constructors() {
        assert_eq!(
            KeyBind::char('w'),
            KeyBind {
                code: KeyCode::Char('w'),
                mods: KeyModifiers::NONE,
            }
        );
        assert_eq!(
            KeyBind::ctrl('a'),
            KeyBind {
                code: KeyCode::Char('a'),
                mods: KeyModifiers::CONTROL,
            }
        );
    }
}
