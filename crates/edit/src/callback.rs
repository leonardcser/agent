//! Per-window keymap and event callback registry. Also hosts group-scoped
//! keymaps for overlays and modal dialogs, letting a container own key handling
//! without registering the same callback on every leaf.
use super::{ModalId, OverlayId, RowIndex, WinId};
use crossterm::event::{KeyCode, KeyModifiers};
use std::collections::HashMap;

/// A keyboard chord (`Hash + Eq` for registry keying).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct KeyBind {
    pub code: KeyCode,
    pub mods: KeyModifiers,
}

impl KeyBind {
    pub fn new(code: KeyCode, mods: KeyModifiers) -> Self {
        // Uppercase text already encodes Shift; terminals may also report it.
        let mods =
            if code == KeyCode::BackTab || matches!(code, KeyCode::Char(c) if c.is_uppercase()) {
                mods - KeyModifiers::SHIFT
            } else {
                mods
            };
        Self { code, mods }
    }

    #[cfg(test)]
    pub(crate) fn plain(code: KeyCode) -> Self {
        Self {
            code,
            mods: KeyModifiers::NONE,
        }
    }

    #[cfg(test)]
    pub(crate) fn char(c: char) -> Self {
        Self::plain(KeyCode::Char(c))
    }

    #[cfg(test)]
    pub(crate) fn ctrl(c: char) -> Self {
        Self {
            code: KeyCode::Char(c),
            mods: KeyModifiers::CONTROL,
        }
    }
}

/// Window lifecycle / semantic events.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum WinEvent {
    Open,
    Close,
    FocusGained,
    FocusLost,
    SelectionChanged,
    /// Enter pressed on a List or Input; payload carries `index` / `text`.
    Submit,
    /// Input buffer edited; payload carries the new text.
    TextChanged,
    Dismiss,
    /// Fired once per event-loop iteration for live-refresh overlays.
    Tick,
    /// Mouse-down landed inside this leaf. Payload carries leaf-relative `row`/`col`
    /// and the button. Fires before focus promotion; non-focusable leaves still receive it.
    Press,
    /// Mouse-up after a `Press` on this leaf. Fires on the leaf that owned the
    /// press, even if the pointer drifted out (capture). Same payload as `Press`.
    Release,
    /// Mouse motion while a button is held after a `Press` on this leaf. Same
    /// payload as `Press`; `row`/`col` are leaf-relative for the new position.
    Drag,
    /// Window's scroll state changed. Payload carries the new `top` and `follow` flag.
    Scrolled,
    /// Leaf's viewport rect changed (first paint, terminal resize, layout
    /// reflow). Payload carries the new `{ row, col, width, height }` rect
    /// and the inner `content_width` in `Payload::Rect`.
    Resized,
    /// User accepted a placeholder suggestion via an `accept_keys` chord.
    /// Payload carries the accepted text in `Payload::Text`.
    PlaceholderAccepted,
    /// User dismissed the placeholder via a `dismiss_keys` chord.
    /// Payload carries the dismissed text in `Payload::Text`.
    PlaceholderDismissed,
}

/// Mouse button identity carried in `Payload::Mouse`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// Payload attached to a callback invocation.
#[derive(Clone, Debug)]
pub enum Payload {
    None,
    Key {
        code: KeyCode,
        mods: KeyModifiers,
    },
    Paste {
        content: String,
    },
    Selection {
        index: usize,
    },
    Text {
        content: String,
    },
    /// Mouse event hit a leaf. `row`/`col` are leaf-relative cell coordinates.
    /// Used for `Press`, `Release`, and `Drag`.
    Mouse {
        row: u16,
        col: u16,
        button: MouseButton,
    },
    /// Scroll state changed. `top` is the new `scroll_top`; `follow` is tail mode.
    Scroll {
        top: RowIndex,
        follow: bool,
    },
    /// Resize payload. `row`/`col`/`width`/`height` describe the new outer
    /// rect (matches `win:rect()`); `content_width` is the inner cell
    /// budget after gutter and pad subtraction (matches `win:content_width()`).
    Rect {
        row: u16,
        col: u16,
        width: u16,
        height: u16,
        content_width: u16,
    },
}

/// Result returned by a callback.
#[derive(Clone, Debug)]
pub enum CallbackResult {
    Consumed,
    Pass,
    /// Consumed, and fire a follow-up `WinEvent` on the same window.
    Event(WinEvent, Payload),
}

/// Opaque handle to a Lua callback; the Lua runtime owns the actual function.
#[derive(Clone, Copy, Debug)]
pub struct LuaHandle(pub u64);

/// Rust-side callback closure.
pub(crate) type RustCallback = Box<dyn FnMut(&mut CallbackCtx<'_>) -> CallbackResult>;

/// A callback: either a Rust closure or a Lua handle.
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

impl Callback {
    fn lua_id(&self) -> Option<u64> {
        match self {
            Callback::Lua(LuaHandle(id)) => Some(*id),
            Callback::Rust(_) => None,
        }
    }
}

/// Context passed to Rust callbacks. Provides full `&mut Ui` access.
pub struct CallbackCtx<'a> {
    pub ui: &'a mut super::Ui,
    pub win: WinId,
    pub payload: Payload,
}

/// Identity for a keymap shared by all leaves in one UI scope.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum KeymapScope {
    Overlay(OverlayId),
    Modal(ModalId),
}

/// Per-window callback registry owned by `Ui`.
#[derive(Default)]
pub(crate) struct Callbacks {
    keymaps: HashMap<WinId, HashMap<KeyBind, Callback>>,
    events: HashMap<WinId, HashMap<WinEvent, Vec<Callback>>>,
    /// Per-window fallback key handler tried after specific keymaps miss.
    key_fallback: HashMap<WinId, Callback>,
    /// Keymaps shared by every leaf in an overlay or modal focus scope.
    scoped_keymaps: HashMap<KeymapScope, HashMap<KeyBind, Callback>>,
}

impl Callbacks {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Install a (win, key) keymap. Returns the displaced `Callback`, if any.
    #[must_use]
    pub(crate) fn set_keymap(
        &mut self,
        win: WinId,
        key: KeyBind,
        cb: Callback,
    ) -> Option<Callback> {
        self.keymaps.entry(win).or_default().insert(key, cb)
    }

    pub(crate) fn clear_keymap(&mut self, win: WinId, key: KeyBind) -> Option<Callback> {
        self.keymaps.get_mut(&win).and_then(|t| t.remove(&key))
    }

    /// Remove a specific event callback by Lua handle id.
    pub(crate) fn clear_event_by_id(
        &mut self,
        win: WinId,
        ev: WinEvent,
        id: u64,
    ) -> Option<Callback> {
        let list = self.events.get_mut(&win)?.get_mut(&ev)?;
        let pos = list
            .iter()
            .position(|cb| matches!(cb, Callback::Lua(LuaHandle(h)) if *h == id))?;
        Some(list.remove(pos))
    }

    pub(crate) fn on_event(&mut self, win: WinId, ev: WinEvent, cb: Callback) {
        self.events
            .entry(win)
            .or_default()
            .entry(ev)
            .or_default()
            .push(cb);
    }

    pub(crate) fn has_event(&self, win: WinId, ev: WinEvent) -> bool {
        self.events
            .get(&win)
            .and_then(|events| events.get(&ev))
            .is_some_and(|callbacks| !callbacks.is_empty())
    }

    /// Remove every binding for `win`. Returns Lua handle IDs for caller cleanup.
    #[must_use]
    pub(crate) fn clear_all(&mut self, win: WinId) -> Vec<u64> {
        let mut lua_ids = Vec::new();
        if let Some(table) = self.keymaps.remove(&win) {
            lua_ids.extend(table.into_values().filter_map(|cb| cb.lua_id()));
        }
        if let Some(events) = self.events.remove(&win) {
            for cbs in events.into_values() {
                lua_ids.extend(cbs.into_iter().filter_map(|cb| cb.lua_id()));
            }
        }
        if let Some(cb) = self.key_fallback.remove(&win) {
            if let Some(id) = cb.lua_id() {
                lua_ids.push(id);
            }
        }
        lua_ids
    }

    /// Move the Rust-owned callbacks out of this registry, dropping every Lua
    /// callback with the retired generation. Candidate registries start with no
    /// callbacks and merge these back only after a successful commit.
    pub(crate) fn take_rust_callbacks(&mut self) -> Self {
        let mut callbacks = std::mem::take(self);
        callbacks.keymaps.retain(|_, table| {
            table.retain(|_, callback| matches!(callback, Callback::Rust(_)));
            !table.is_empty()
        });
        callbacks.events.retain(|_, events| {
            events.retain(|_, callbacks| {
                callbacks.retain(|callback| matches!(callback, Callback::Rust(_)));
                !callbacks.is_empty()
            });
            !events.is_empty()
        });
        callbacks
            .key_fallback
            .retain(|_, callback| matches!(callback, Callback::Rust(_)));
        callbacks.scoped_keymaps.retain(|_, table| {
            table.retain(|_, callback| matches!(callback, Callback::Rust(_)));
            !table.is_empty()
        });
        callbacks
    }

    /// Merge callbacks owned by the Rust UI into a committed candidate.
    /// Candidate keymaps win exact conflicts, matching registration against a
    /// live UI, while Rust event callbacks retain their original first position.
    pub(crate) fn merge_rust_callbacks(&mut self, callbacks: Self) {
        for (win, table) in callbacks.keymaps {
            let target = self.keymaps.entry(win).or_default();
            for (key, callback) in table {
                target.entry(key).or_insert(callback);
            }
        }
        for (win, events) in callbacks.events {
            let target_events = self.events.entry(win).or_default();
            for (event, mut callbacks) in events {
                callbacks.extend(target_events.remove(&event).unwrap_or_default());
                target_events.insert(event, callbacks);
            }
        }
        for (win, callback) in callbacks.key_fallback {
            self.key_fallback.entry(win).or_insert(callback);
        }
        for (scope, table) in callbacks.scoped_keymaps {
            let target = self.scoped_keymaps.entry(scope).or_default();
            for (key, callback) in table {
                target.entry(key).or_insert(callback);
            }
        }
    }

    /// Register a per-window fallback key handler. Returns the displaced `Callback`, if any.
    #[must_use]
    pub(crate) fn set_key_fallback(&mut self, win: WinId, cb: Callback) -> Option<Callback> {
        self.key_fallback.insert(win, cb)
    }

    pub(crate) fn take_key_fallback(&mut self, win: WinId) -> Option<Callback> {
        self.key_fallback.remove(&win)
    }

    pub(crate) fn restore_key_fallback(&mut self, win: WinId, cb: Callback) {
        self.key_fallback.insert(win, cb);
    }

    /// List every window with at least one callback registered for `ev`.
    pub(crate) fn wins_with_event(&self, ev: WinEvent) -> Vec<WinId> {
        self.events
            .iter()
            .filter_map(|(win, table)| table.get(&ev).filter(|v| !v.is_empty()).map(|_| *win))
            .collect()
    }

    /// Remove a keymap callback for invocation. Caller must restore it after.
    /// Removal + restore avoids a reentrant-borrow conflict with `&mut Ui`.
    pub(crate) fn take_keymap(&mut self, win: WinId, key: KeyBind) -> Option<Callback> {
        self.keymaps.get_mut(&win)?.remove(&key)
    }

    pub(crate) fn restore_keymap(&mut self, win: WinId, key: KeyBind, cb: Callback) {
        self.keymaps.entry(win).or_default().insert(key, cb);
    }

    // ── Scoped keymaps ───────────────────────────────────────────────

    #[must_use]
    pub(crate) fn set_scoped_keymap(
        &mut self,
        scope: KeymapScope,
        key: KeyBind,
        cb: Callback,
    ) -> Option<Callback> {
        self.scoped_keymaps
            .entry(scope)
            .or_default()
            .insert(key, cb)
    }

    pub(crate) fn clear_scoped_keymap(
        &mut self,
        scope: KeymapScope,
        key: KeyBind,
    ) -> Option<Callback> {
        self.scoped_keymaps
            .get_mut(&scope)
            .and_then(|table| table.remove(&key))
    }

    pub(crate) fn take_scoped_keymap(
        &mut self,
        scope: KeymapScope,
        key: KeyBind,
    ) -> Option<Callback> {
        self.scoped_keymaps.get_mut(&scope)?.remove(&key)
    }

    pub(crate) fn restore_scoped_keymap(&mut self, scope: KeymapScope, key: KeyBind, cb: Callback) {
        self.scoped_keymaps
            .entry(scope)
            .or_default()
            .insert(key, cb);
    }

    /// Remove every keymap in one scope. Returns Lua handle ids for caller cleanup.
    #[must_use]
    pub(crate) fn clear_scope(&mut self, scope: KeymapScope) -> Vec<u64> {
        self.scoped_keymaps
            .remove(&scope)
            .into_iter()
            .flat_map(HashMap::into_values)
            .filter_map(|callback| callback.lua_id())
            .collect()
    }

    /// Same take/restore pattern for event callbacks (takes the whole Vec).
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

    fn oid(n: u32) -> OverlayId {
        OverlayId(n)
    }

    #[test]
    fn set_and_take_scoped_keymap() {
        let mut cbs = Callbacks::new();
        let key = KeyBind::plain(KeyCode::Tab);
        let scope = KeymapScope::Overlay(oid(1));
        let _ = cbs.set_scoped_keymap(scope, key, Callback::Lua(LuaHandle(7)));
        let taken = cbs.take_scoped_keymap(scope, key);
        assert!(matches!(taken, Some(Callback::Lua(LuaHandle(7)))));
        assert!(cbs.take_scoped_keymap(scope, key).is_none());
    }

    #[test]
    fn typed_scopes_keep_matching_raw_ids_independent() {
        let mut cbs = Callbacks::new();
        let key = KeyBind::plain(KeyCode::Enter);
        let overlay = KeymapScope::Overlay(OverlayId(3));
        let modal = KeymapScope::Modal(ModalId(3));
        let _ = cbs.set_scoped_keymap(overlay, key, Callback::Lua(LuaHandle(1)));
        let _ = cbs.set_scoped_keymap(modal, key, Callback::Lua(LuaHandle(2)));

        assert!(matches!(
            cbs.take_scoped_keymap(overlay, key),
            Some(Callback::Lua(LuaHandle(1)))
        ));
        assert!(matches!(
            cbs.take_scoped_keymap(modal, key),
            Some(Callback::Lua(LuaHandle(2)))
        ));
    }

    #[test]
    fn clear_scope_returns_lua_ids() {
        let mut cbs = Callbacks::new();
        let scope = KeymapScope::Overlay(oid(2));
        let _ = cbs.set_scoped_keymap(scope, KeyBind::char('q'), Callback::Lua(LuaHandle(11)));
        let _ = cbs.set_scoped_keymap(scope, KeyBind::char('w'), Callback::Lua(LuaHandle(12)));
        let mut ids = cbs.clear_scope(scope);
        ids.sort();
        assert_eq!(ids, vec![11, 12]);
        assert!(cbs.take_scoped_keymap(scope, KeyBind::char('q')).is_none());
    }

    #[test]
    fn backtab_keybind_ignores_shift_modifier() {
        assert_eq!(
            KeyBind::new(KeyCode::BackTab, KeyModifiers::SHIFT),
            KeyBind::new(KeyCode::BackTab, KeyModifiers::NONE)
        );
        assert_eq!(
            KeyBind::new(KeyCode::BackTab, KeyModifiers::SHIFT | KeyModifiers::ALT),
            KeyBind::new(KeyCode::BackTab, KeyModifiers::ALT)
        );
    }

    #[test]
    fn uppercase_keybinds_ignore_redundant_shift() {
        for ch in ['G', 'H', 'L', 'É', 'Ω'] {
            for mods in [KeyModifiers::NONE, KeyModifiers::CONTROL, KeyModifiers::ALT] {
                assert_eq!(
                    KeyBind::new(KeyCode::Char(ch), mods | KeyModifiers::SHIFT),
                    KeyBind::new(KeyCode::Char(ch), mods)
                );
            }
        }
        for code in [KeyCode::Left, KeyCode::Char('g'), KeyCode::Char(' ')] {
            assert_ne!(
                KeyBind::new(code, KeyModifiers::SHIFT),
                KeyBind::new(code, KeyModifiers::NONE)
            );
        }
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
