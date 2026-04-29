pub mod buffer;
pub mod buffer_list;
pub mod buffer_view;
pub mod callback;
pub mod clipboard;
pub mod cmdline;
pub mod component;
pub mod compositor;
pub mod dialog;
pub mod edit_buffer;
pub mod flush;
pub mod grid;
pub mod kill_ring;
pub mod layout;
pub mod notification;
pub mod option_list;
pub mod picker;
pub mod status_bar;
pub mod style;
pub mod text;
pub mod text_input;
pub mod undo;
pub mod vim;
pub mod window;
pub mod window_cursor;

mod id;

pub type AttachmentId = u64;

/// Callback shape for routing `Callback::Lua` handles out of Ui into
/// the host's Lua runtime. Receives the handle, the focused window,
/// the event payload, and a live snapshot of the window's dialog
/// panels for pull-read inside the Lua callback. Empty slice when the
/// window isn't a dialog.
pub type LuaInvoke<'a> =
    dyn FnMut(callback::LuaHandle, id::WinId, &callback::Payload, &[dialog::PanelSnapshot]) + 'a;

pub use buffer::{BufType, Buffer, BufferFormatter, Span, SpanStyle};
pub use buffer_list::BufferList;
pub use buffer_view::BufferView;
pub use callback::{
    Callback, CallbackCtx, CallbackResult, Callbacks, KeyBind, LuaHandle, Payload, RustCallback,
    WinEvent,
};
pub use cmdline::{Cmdline, CmdlineStyle};
pub use component::{Component, CursorInfo, CursorStyle, DrawContext, KeyResult, WidgetEvent};
pub use compositor::Compositor;
pub use dialog::{
    Dialog, DialogConfig, PanelContent, PanelHeight, PanelSnapshot, PanelSpec, PanelWidget,
    SeparatorStyle,
};
pub use edit_buffer::EditBuffer;
pub use flush::flush_diff;
pub use grid::{Cell, Grid, GridSlice, Style};
pub use id::{BufId, WinId, LUA_BUF_ID_BASE};
pub use kill_ring::KillRing;
pub use layout::{
    Anchor, Border, Constraint, FitMax, FloatRelative, Gutters, LayoutTree, Placement, Rect,
};
pub use notification::{Notification, NotificationLevel, NotificationStyle};
pub use option_list::{OptionItem, OptionList};
pub use picker::{Picker, PickerItem, PickerStyle};
pub use status_bar::{StatusBar, StatusSegment};
pub use style::{HlAttrs, HlGroup};
pub use text_input::TextInput;
pub use undo::{UndoEntry, UndoHistory};
pub use vim::{ViMode, Vim};
pub use window::{
    FloatConfig, MouseAction, MouseCtx, ScrollbarState, SplitConfig, ViewportHit, WinConfig,
    Window, WindowViewport,
};
pub use window_cursor::WindowCursor;

use std::collections::HashMap;

pub struct Ui {
    bufs: HashMap<BufId, Buffer>,
    wins: HashMap<WinId, Window>,
    current_win: Option<WinId>,
    next_buf_id: u64,
    next_win_id: u64,
    layout: Option<LayoutTree>,
    terminal_size: (u16, u16),
    compositor: Compositor,
    callbacks: Callbacks,
    /// Rects for split / non-float windows (PROMPT_WIN, TRANSCRIPT_WIN,
    /// …). Float rects are computed per-frame from `FloatConfig`; split
    /// rects are laid out by the host app (TUI render loop) and pushed
    /// in via [`Ui::set_window_rect`] so `Placement::DockedAbove` can
    /// look them up without knowing layout specifics.
    split_rects: HashMap<WinId, Rect>,
    /// Compositor layer-id ↔ `WinId` for split windows. Lets the focused
    /// split's per-window keymap fire from `handle_key_with_lua`, the
    /// same dispatch path used by floats. Floats use the `"float:N"`
    /// layer-id prefix instead of this map.
    splits: HashMap<String, WinId>,
    /// Theme-resolved background color used by every `Window`-driven
    /// selection overlay (transcript, prompt, dialog content panels).
    /// Populated by the host once at startup; one source of truth so
    /// the look stays consistent across surfaces. `None` falls back to
    /// `DarkGrey` at the call site.
    selection_bg: Option<crossterm::style::Color>,
}

/// Reserved `WinId` for the main prompt input window. The prompt is
/// rendered as a compositor layer (not inserted into `Ui::wins`), but
/// callbacks (keymaps, `WinEvent` subscribers) are keyed by `WinId`,
/// so we reserve a stable id so Lua can `smelt.win.on_event(prompt, …)`
/// and `smelt.win.set_keymap(prompt, …)` like any other window.
pub const PROMPT_WIN: WinId = WinId(0);

/// Reserved `WinId` for the transcript (scroll-back) window. Same
/// rationale as [`PROMPT_WIN`] — stable id for callback registration.
pub const TRANSCRIPT_WIN: WinId = WinId(1);

impl Ui {
    pub fn new() -> Self {
        Self {
            bufs: HashMap::new(),
            wins: HashMap::new(),
            current_win: None,
            next_buf_id: 1,
            // 0 is reserved for PROMPT_WIN, 1 for TRANSCRIPT_WIN.
            next_win_id: 2,
            layout: None,
            terminal_size: (80, 24),
            compositor: Compositor::new(80, 24),
            callbacks: Callbacks::new(),
            split_rects: HashMap::new(),
            splits: HashMap::new(),
            selection_bg: None,
        }
    }

    /// Set the background color used for selection overlays across
    /// every `Window`-backed surface. Call once at host startup with
    /// `theme::selection_bg()`; future theme overrides flow through
    /// the same slot.
    pub fn set_selection_bg(&mut self, color: crossterm::style::Color) {
        self.selection_bg = Some(color);
        let style = self.selection_style();
        self.compositor.set_selection_style(style);
    }

    /// Selection overlay background as a `Style` ready to feed into
    /// `BufferView::add_highlight`. Falls back to `DarkGrey` if the
    /// host never set one.
    pub fn selection_style(&self) -> grid::Style {
        grid::Style {
            bg: Some(
                self.selection_bg
                    .unwrap_or(crossterm::style::Color::DarkGrey),
            ),
            ..grid::Style::default()
        }
    }

    /// Bind a compositor split-layer id to a `WinId` so the focused
    /// split flows through [`Ui::handle_key_with_lua`] like floats do.
    /// Call once per split at startup.
    pub fn register_split(&mut self, layer_id: impl Into<String>, win_id: WinId) {
        self.splits.insert(layer_id.into(), win_id);
    }

    fn layer_to_win(&self, layer_id: &str) -> Option<WinId> {
        parse_float_layer_id(layer_id).or_else(|| self.splits.get(layer_id).copied())
    }

    /// Publish a split window's current rect. Call each frame from the
    /// host app's layout pass so `Placement::DockedAbove` can resolve
    /// correctly. Floats don't need this — their rects come from
    /// `FloatConfig`.
    pub fn set_window_rect(&mut self, win_id: WinId, rect: Rect) {
        self.split_rects.insert(win_id, rect);
    }

    pub fn buf_create(&mut self, opts: buffer::BufCreateOpts) -> BufId {
        let id = BufId(self.next_buf_id);
        self.next_buf_id += 1;
        let buf = Buffer::new(id, opts);
        self.bufs.insert(id, buf);
        id
    }

    /// Create a buffer at an explicit id. Returns `Err` if a buffer
    /// with that id already exists — callers should mint a fresh id
    /// via `buf_create`. Plugin-facing IDs (Lua `smelt.buf.create`)
    /// live above `LUA_BUF_ID_BASE` so they can't collide with
    /// sequentially-allocated Rust buffers.
    pub fn buf_create_with_id(
        &mut self,
        id: BufId,
        opts: buffer::BufCreateOpts,
    ) -> Result<BufId, BufId> {
        if self.bufs.contains_key(&id) {
            return Err(id);
        }
        let buf = Buffer::new(id, opts);
        self.bufs.insert(id, buf);
        if id.0 >= self.next_buf_id {
            self.next_buf_id = id.0 + 1;
        }
        Ok(id)
    }

    pub fn buf_delete(&mut self, id: BufId) {
        self.wins.retain(|_, w| w.buf != id);
        self.bufs.remove(&id);
    }

    pub fn buf(&self, id: BufId) -> Option<&Buffer> {
        self.bufs.get(&id)
    }

    pub fn buf_mut(&mut self, id: BufId) -> Option<&mut Buffer> {
        self.bufs.get_mut(&id)
    }

    pub fn win_open_split(&mut self, buf: BufId, config: SplitConfig) -> Option<WinId> {
        if !self.bufs.contains_key(&buf) {
            return None;
        }
        let id = WinId(self.next_win_id);
        self.next_win_id += 1;
        let win = Window::new(id, buf, WinConfig::Split(config));
        self.wins.insert(id, win);
        if self.current_win.is_none() {
            self.current_win = Some(id);
        }
        Some(id)
    }

    /// Close a float window. Returns the Lua callback IDs that were
    /// attached (keymaps, events, fallback) so the caller can drop
    /// them from the Lua-side registry.
    #[must_use]
    pub fn win_close(&mut self, id: WinId) -> Vec<u64> {
        let layer_id = float_layer_id(id);
        self.compositor.remove(&layer_id);
        self.wins.remove(&id);
        let lua_ids = self.callbacks.clear_all(id);
        if self.current_win == Some(id) {
            self.current_win = self.wins.keys().next().copied();
        }
        lua_ids
    }

    // ── Picker ───────────────────────────────────────────────────────
    //
    // Non-focusable dropdown component. One primitive reused by the
    // prompt `/` completer, the cmdline `:` completer, and Lua
    // `smelt.ui.picker.open`. Mirrors Neovim's `pum_grid`: caller drives
    // selection, component paints.

    /// Open a `Picker` float. Accepts any `FloatConfig`; typically
    /// callers pass `focusable: false` and a manually-positioned
    /// `Placement`. `reversed` paints logical index 0 on the bottom
    /// visual row — used by pickers that dock *above* the prompt
    /// (completer `/`, cmdline `:`) so the best match is closest to
    /// where the user is typing. Returns the new `WinId` for later
    /// updates via `picker_mut` and eventual `win_close`.
    pub fn picker_open(
        &mut self,
        config: FloatConfig,
        items: Vec<picker::PickerItem>,
        selected: usize,
        style: picker::PickerStyle,
        reversed: bool,
    ) -> Option<WinId> {
        let id = WinId(self.next_win_id);
        self.next_win_id += 1;

        let (tw, th) = self.terminal_size;
        let rect = resolve_float_rect(&config, tw, th, None, &self.split_rects);
        let zindex = config.zindex;

        let mut p = picker::Picker::new()
            .with_style(style)
            .with_reversed(reversed);
        p.set_items(items);
        p.set_selected(selected);

        // Pickers don't own a buffer; they render from internal state.
        // Still register a placeholder window so focus / close / keymap
        // paths stay uniform with other floats.
        let placeholder_buf = BufId(0);
        let focusable = config.focusable;
        let mut win = Window::new(id, placeholder_buf, WinConfig::Float(config));
        win.focusable = focusable;
        self.wins.insert(id, win);

        let layer_id = float_layer_id(id);
        // Pickers are non-focusable in the keymap sense; mouse clicks
        // should not promote them to focused or raise their zindex
        // (they sit beneath dialogs/cmdline by design).
        let opts = compositor::LayerOpts {
            focus_on_click: focusable,
            raise_on_click: focusable,
        };
        self.compositor
            .add_with_opts(&layer_id, Box::new(p), rect, zindex, opts);
        if focusable {
            self.compositor.focus(&layer_id);
        }

        Some(id)
    }

    /// Mutable access to an open `Picker` component. Used to update
    /// items and selection as the caller's filter/selection state
    /// changes.
    pub fn picker_mut(&mut self, win_id: WinId) -> Option<&mut picker::Picker> {
        let layer_id = float_layer_id(win_id);
        let comp = self.compositor.component_mut(&layer_id)?;
        comp.as_any_mut().downcast_mut::<picker::Picker>()
    }

    // ── Notification ─────────────────────────────────────────────────
    //
    // Non-focusable ephemeral toast. Sibling to `Picker` in the named-
    // components family. Caller controls lifecycle (open / update /
    // dismiss on key → `win_close`).

    pub fn notification_open(
        &mut self,
        config: FloatConfig,
        message: impl Into<String>,
        level: notification::NotificationLevel,
        style: notification::NotificationStyle,
    ) -> Option<WinId> {
        let id = WinId(self.next_win_id);
        self.next_win_id += 1;

        let (tw, th) = self.terminal_size;
        let rect = resolve_float_rect(&config, tw, th, None, &self.split_rects);
        let zindex = config.zindex;

        let n = notification::Notification::new(message, level).with_style(style);

        let placeholder_buf = BufId(0);
        let focusable = config.focusable;
        let mut win = Window::new(id, placeholder_buf, WinConfig::Float(config));
        win.focusable = focusable;
        self.wins.insert(id, win);

        let layer_id = float_layer_id(id);
        // Toast: non-focusable, sits below dialogs by design. Click
        // dispatches `Dismiss` (handled by App), so suppress focus +
        // raise so the click doesn't bump the toast above a real modal.
        let opts = compositor::LayerOpts {
            focus_on_click: false,
            raise_on_click: false,
        };
        self.compositor
            .add_with_opts(&layer_id, Box::new(n), rect, zindex, opts);
        if focusable {
            self.compositor.focus(&layer_id);
        }

        Some(id)
    }

    pub fn notification_mut(&mut self, win_id: WinId) -> Option<&mut notification::Notification> {
        let layer_id = float_layer_id(win_id);
        let comp = self.compositor.component_mut(&layer_id)?;
        comp.as_any_mut()
            .downcast_mut::<notification::Notification>()
    }

    // ── Cmdline ──────────────────────────────────────────────────────
    //
    // Focusable single-row compositor float for `:` / `/`-style command
    // entry. Sibling to `Picker` and `Notification`. Callers that want
    // Tab-complete register per-window keymaps and drive completion via
    // `Cmdline::{text, set_text}`.

    pub fn cmdline_open(
        &mut self,
        config: FloatConfig,
        cmdline: cmdline::Cmdline,
    ) -> Option<WinId> {
        let id = WinId(self.next_win_id);
        self.next_win_id += 1;

        let (tw, th) = self.terminal_size;
        let rect = resolve_float_rect(&config, tw, th, None, &self.split_rects);
        let zindex = config.zindex;

        let placeholder_buf = BufId(0);
        let focusable = config.focusable;
        let mut win = Window::new(id, placeholder_buf, WinConfig::Float(config));
        win.focusable = focusable;
        self.wins.insert(id, win);

        let layer_id = float_layer_id(id);
        self.compositor
            .add(&layer_id, Box::new(cmdline), rect, zindex);
        if focusable {
            self.compositor.focus(&layer_id);
        }

        Some(id)
    }

    pub fn cmdline_mut(&mut self, win_id: WinId) -> Option<&mut cmdline::Cmdline> {
        let layer_id = float_layer_id(win_id);
        let comp = self.compositor.component_mut(&layer_id)?;
        comp.as_any_mut().downcast_mut::<cmdline::Cmdline>()
    }

    pub fn cmdline(&self, win_id: WinId) -> Option<&cmdline::Cmdline> {
        let layer_id = float_layer_id(win_id);
        let comp = self.compositor.component(&layer_id)?;
        comp.as_any().downcast_ref::<cmdline::Cmdline>()
    }

    // ── Callbacks ────────────────────────────────────────────────────
    //
    // Per-window keymap + event callbacks. The registry is the single
    // behavior mechanism shared by Rust and Lua.

    /// Bind a key chord on a specific window to a callback. Returns
    /// the displaced callback (if any) so Lua-side handles can be
    /// cleaned up by the caller.
    #[must_use]
    pub fn win_set_keymap(&mut self, win: WinId, key: KeyBind, cb: Callback) -> Option<Callback> {
        self.callbacks.set_keymap(win, key, cb)
    }

    /// Remove a keymap binding. Returns the removed callback, if any.
    #[must_use]
    pub fn win_clear_keymap(&mut self, win: WinId, key: KeyBind) -> Option<Callback> {
        self.callbacks.clear_keymap(win, key)
    }

    /// Register a catch-all key handler for a window. Runs after
    /// specific keymaps miss and before `Component::handle_key`.
    /// Returns the displaced callback (if any).
    #[must_use]
    pub fn win_set_key_fallback(&mut self, win: WinId, cb: Callback) -> Option<Callback> {
        self.callbacks.set_key_fallback(win, cb)
    }

    /// Register a callback for a window lifecycle / semantic event.
    /// Multiple callbacks per (win, event) are supported and fire
    /// in registration order.
    pub fn win_on_event(&mut self, win: WinId, ev: WinEvent, cb: Callback) {
        self.callbacks.on_event(win, ev, cb);
    }

    /// Remove a specific event callback by its Lua handle id. Used by
    /// plugins that install cross-window handlers (e.g. a picker that
    /// listens to `text_changed` on the prompt) and need to tear down
    /// exactly their own binding on close.
    #[must_use]
    pub fn win_clear_event_by_id(&mut self, win: WinId, ev: WinEvent, id: u64) -> Option<Callback> {
        self.callbacks.clear_event_by_id(win, ev, id)
    }

    /// Dispatch a window event to its registered callbacks.
    /// `lua_invoke` is called for each `Callback::Lua` with
    /// (handle, payload). Side effects flow through the `AppOp` queue
    /// that Rust callbacks have via `shared.ops` — no return channel.
    pub fn dispatch_event(
        &mut self,
        win: WinId,
        ev: WinEvent,
        payload: Payload,
        lua_invoke: &mut LuaInvoke,
    ) {
        let Some(mut cbs) = self.callbacks.take_event(win, ev) else {
            return;
        };
        for cb in cbs.iter_mut() {
            match cb {
                Callback::Rust(inner) => {
                    let mut ctx = CallbackCtx {
                        ui: self,
                        win,
                        payload: payload.clone(),
                    };
                    let _ = inner(&mut ctx);
                }
                Callback::Lua(handle) => {
                    let panels = self.snapshot_dialog_panels(win);
                    lua_invoke(*handle, win, &payload, &panels);
                }
            }
        }
        self.callbacks.restore_event(win, ev, cbs);
    }

    /// Open a multi-panel `Dialog` as a compositor float layer.
    ///
    /// Panels' buffers stay in `Ui::bufs`; each panel's interaction
    /// state (cursor, scroll) lives in a dialog-local `Window`. The
    /// dialog's own `WinId` is registered in `Ui::wins` as a float so
    /// focus / close paths match `win_open_float`.
    pub fn dialog_open(
        &mut self,
        float_config: FloatConfig,
        mut dialog_config: dialog::DialogConfig,
        panels: Vec<dialog::PanelSpec>,
    ) -> Option<WinId> {
        let all_bufs_registered = panels.iter().all(|p| match &p.content {
            dialog::PanelContent::Buffer(b) => self.bufs.contains_key(b),
            dialog::PanelContent::Widget(_) => true,
        });
        if !all_bufs_registered {
            return None;
        }
        // Inherit the host's selection style so interactive buffer
        // panels paint their highlight with the same bg the transcript
        // and prompt use.
        if dialog_config.selection_style == grid::Style::default() {
            dialog_config.selection_style = self.selection_style();
        }
        let id = WinId(self.next_win_id);
        self.next_win_id += 1;

        let (tw, th) = self.terminal_size;
        let zindex = float_config.zindex;

        // Use the first buffer-backed panel's buffer as the dialog
        // window's "buf" pointer for registry purposes (dialogs are
        // multi-buffer and may be widget-only).
        let primary_buf = panels
            .iter()
            .find_map(|p| match &p.content {
                dialog::PanelContent::Buffer(b) => Some(*b),
                dialog::PanelContent::Widget(_) => None,
            })
            .unwrap_or(BufId(0));

        let panel_structs = dialog::build_panels(panels, &self.bufs);
        let mut dlg = dialog::Dialog::new(dialog_config, panel_structs);
        // Pre-sync so `FitContent` sees a populated `line_count` on
        // the first frame — otherwise the dialog lands at the cap
        // height on open, then snaps to fit on the next render.
        //
        // The float rect's width is independent of `natural_h` (every
        // `Placement` resolves width from terminal size + its width
        // constraint, not the content height), so we compute it first
        // with `natural_h = None` and feed the content width into
        // formatter-backed buffers before sampling `natural_height`.
        let pre_rect = resolve_float_rect(&float_config, tw, th, None, &self.split_rects);
        let content_width = pre_rect.width.saturating_sub(1);
        dlg.sync_from_bufs_mut(content_width, &mut self.bufs);
        let natural_h = Some(dlg.natural_height());
        let rect = resolve_float_rect(&float_config, tw, th, natural_h, &self.split_rects);

        let focusable = float_config.focusable;
        let mut win = Window::new(id, primary_buf, WinConfig::Float(float_config));
        win.focusable = focusable;
        self.wins.insert(id, win);

        let layer_id = float_layer_id(id);
        self.compositor.add(&layer_id, Box::new(dlg), rect, zindex);
        self.compositor.focus(&layer_id);

        Some(id)
    }

    /// Access a `Dialog` compositor layer by its WinId for
    /// post-creation configuration (hints, dismiss keys, etc.).
    pub fn dialog_mut(&mut self, win_id: WinId) -> Option<&mut dialog::Dialog> {
        let layer_id = float_layer_id(win_id);
        let comp = self.compositor.component_mut(&layer_id)?;
        comp.as_any_mut().downcast_mut::<dialog::Dialog>()
    }

    pub fn dialog(&self, win_id: WinId) -> Option<&dialog::Dialog> {
        let layer_id = float_layer_id(win_id);
        let comp = self.compositor.component(&layer_id)?;
        comp.as_any().downcast_ref::<dialog::Dialog>()
    }

    /// Build a live snapshot of every panel on the `Dialog` at `win_id`
    /// for Lua pull-read. Empty vec when the window isn't a dialog.
    /// `selected` is the 0-based selection index when the panel is a
    /// list widget; `text` is the current text for input widgets.
    pub fn snapshot_dialog_panels(&self, win_id: WinId) -> Vec<dialog::PanelSnapshot> {
        let Some(dlg) = self.dialog(win_id) else {
            return Vec::new();
        };
        (0..dlg.panel_count())
            .map(|i| dialog::PanelSnapshot {
                selected: dlg.selected_index_at(i),
                text: dlg.panel_widget_text(i).unwrap_or_default(),
            })
            .collect()
    }

    pub fn win(&self, id: WinId) -> Option<&Window> {
        self.wins.get(&id)
    }

    pub fn win_mut(&mut self, id: WinId) -> Option<&mut Window> {
        self.wins.get_mut(&id)
    }

    pub fn win_list(&self) -> Vec<WinId> {
        self.wins.keys().copied().collect()
    }

    pub fn current_win(&self) -> Option<WinId> {
        self.current_win
    }

    pub fn set_current_win(&mut self, id: WinId) {
        if self.wins.contains_key(&id) {
            self.current_win = Some(id);
        }
    }

    pub fn set_terminal_size(&mut self, w: u16, h: u16) {
        self.terminal_size = (w, h);
        self.compositor.resize(w, h);
    }

    pub fn terminal_size(&self) -> (u16, u16) {
        self.terminal_size
    }

    pub fn set_layout(&mut self, tree: LayoutTree) {
        self.layout = Some(tree);
    }

    pub fn layout(&self) -> Option<&LayoutTree> {
        self.layout.as_ref()
    }

    pub fn floats_z_ordered(&self) -> Vec<WinId> {
        let mut floats: Vec<_> = self
            .wins
            .iter()
            .filter(|(_, w)| matches!(w.config, WinConfig::Float(_)))
            .map(|(id, w)| {
                let z = match &w.config {
                    WinConfig::Float(f) => f.zindex,
                    _ => 0,
                };
                (*id, z)
            })
            .collect();
        floats.sort_by_key(|(_, z)| *z);
        floats.into_iter().map(|(id, _)| id).collect()
    }

    pub fn resolve_splits(&self) -> HashMap<String, Rect> {
        let (w, h) = self.terminal_size;
        match &self.layout {
            Some(tree) => layout::resolve_layout(tree, Rect::new(0, 0, w, h)),
            None => HashMap::new(),
        }
    }

    pub fn resolve_float(&self, win_id: WinId) -> Option<Rect> {
        let win = self.wins.get(&win_id)?;
        let fc = match &win.config {
            WinConfig::Float(f) => f,
            _ => return None,
        };
        let (tw, th) = self.terminal_size;
        let natural_h = self.natural_layer_height(win_id);
        Some(resolve_float_rect(fc, tw, th, natural_h, &self.split_rects))
    }

    pub fn resolve_float_rects(&self) -> Vec<(WinId, Rect)> {
        let (tw, th) = self.terminal_size;
        self.floats_z_ordered()
            .into_iter()
            .filter_map(|id| {
                let win = self.wins.get(&id)?;
                let fc = match &win.config {
                    WinConfig::Float(f) => f,
                    _ => return None,
                };
                let natural_h = self.natural_layer_height(id);
                Some((
                    id,
                    resolve_float_rect(fc, tw, th, natural_h, &self.split_rects),
                ))
            })
            .collect()
    }

    /// Peek at a float layer's natural height for placement. Dialogs
    /// expose `line_count`-derived height (used by `FitContent`); pickers
    /// clamp item-count to `max_visible_rows` (used by `DockedAbove`).
    /// Returns `None` for other layer kinds — placement variants that
    /// don't consult natural height treat this as "use the max cap."
    fn natural_layer_height(&self, win_id: WinId) -> Option<u16> {
        let layer_id = float_layer_id(win_id);
        let comp = self.compositor.component(&layer_id)?;
        if let Some(dlg) = comp.as_any().downcast_ref::<dialog::Dialog>() {
            return Some(dlg.natural_height());
        }
        if let Some(p) = comp.as_any().downcast_ref::<picker::Picker>() {
            return Some(p.natural_height());
        }
        None
    }

    // ── Layer management ─────────────────────────────────────────

    pub fn add_layer(
        &mut self,
        id: impl Into<String>,
        component: Box<dyn Component>,
        rect: Rect,
        zindex: u16,
    ) {
        self.compositor.add(id, component, rect, zindex);
    }

    pub fn set_layer_rect(&mut self, id: &str, rect: Rect) {
        self.compositor.set_rect(id, rect);
    }

    pub fn focus_layer(&mut self, id: impl Into<String>) {
        self.compositor.focus(id);
    }

    pub fn layer_mut<T: 'static>(&mut self, id: &str) -> Option<&mut T> {
        self.compositor
            .component_mut(id)?
            .as_any_mut()
            .downcast_mut::<T>()
    }

    // ── Compositor delegation ──────────────────────────────────────

    pub fn render<W: std::io::Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        self.sync_float_content();
        self.sync_float_rects();
        self.compositor.render(w)
    }

    pub fn handle_key(
        &mut self,
        code: crossterm::event::KeyCode,
        mods: crossterm::event::KeyModifiers,
    ) -> KeyResult {
        self.handle_key_with_lua(code, mods, &mut |_, _, _, _| {})
    }

    /// Dispatch a key event through the focused window's keymap
    /// table, falling back to `Component::handle_key` if no binding
    /// matches. Floats and registered splits both flow through here —
    /// the focused layer's `WinId` is resolved via [`Ui::layer_to_win`].
    /// `lua_invoke` is called for each `Callback::Lua` with (handle,
    /// payload). Side effects (app-level commands, etc.) flow through
    /// `AppOp` from the host side.
    pub fn handle_key_with_lua(
        &mut self,
        code: crossterm::event::KeyCode,
        mods: crossterm::event::KeyModifiers,
        lua_invoke: &mut LuaInvoke,
    ) -> KeyResult {
        let focused = self
            .compositor
            .focused()
            .and_then(|id| self.layer_to_win(id));
        let Some(win) = focused else {
            return self.compositor.handle_key(code, mods);
        };
        let key = KeyBind::new(code, mods);
        let result = if let Some(mut cb) = self.callbacks.take_keymap(win, key) {
            let r = match &mut cb {
                Callback::Rust(inner) => {
                    let mut ctx = CallbackCtx {
                        ui: self,
                        win,
                        payload: Payload::Key { code, mods },
                    };
                    let r = inner(&mut ctx);
                    match r {
                        CallbackResult::Consumed => KeyResult::Consumed,
                        CallbackResult::Pass => self.compositor.handle_key(code, mods),
                    }
                }
                Callback::Lua(handle) => {
                    let payload = Payload::Key { code, mods };
                    let panels = self.snapshot_dialog_panels(win);
                    lua_invoke(*handle, win, &payload, &panels);
                    KeyResult::Consumed
                }
            };
            self.callbacks.restore_keymap(win, key, cb);
            r
        } else if let Some(mut cb) = self.callbacks.take_key_fallback(win) {
            let r = match &mut cb {
                Callback::Rust(inner) => {
                    let mut ctx = CallbackCtx {
                        ui: self,
                        win,
                        payload: Payload::Key { code, mods },
                    };
                    let r = inner(&mut ctx);
                    match r {
                        CallbackResult::Consumed => KeyResult::Consumed,
                        CallbackResult::Pass => self.compositor.handle_key(code, mods),
                    }
                }
                Callback::Lua(handle) => {
                    let payload = Payload::Key { code, mods };
                    let panels = self.snapshot_dialog_panels(win);
                    lua_invoke(*handle, win, &payload, &panels);
                    KeyResult::Consumed
                }
            };
            self.callbacks.restore_key_fallback(win, cb);
            r
        } else {
            self.compositor.handle_key(code, mods)
        };

        // Auto-translate widget events into typed `WinEvent` callbacks
        // when the focused window has a matching callback registered.
        if let KeyResult::Action(action) = &result {
            if let Some((ev, payload)) = classify_widget_action(action) {
                if self.callbacks.has_event(win, ev) {
                    self.dispatch_event(win, ev, payload, lua_invoke);
                    return KeyResult::Consumed;
                }
            }
        }
        result
    }

    /// Dispatch a mouse event through the compositor, then fan widget
    /// actions out to the dispatched-to window's `WinEvent` callbacks
    /// (mirrors the keyboard path in `handle_key_with_lua`). Returns
    /// `(WinId, KeyResult)` for the dispatched-to layer, or `None` when
    /// no float was hit — the host then routes the event to its own
    /// pane mouse logic (transcript/prompt drag-select).
    pub fn handle_mouse_with_lua(
        &mut self,
        event: crossterm::event::MouseEvent,
        lua_invoke: &mut LuaInvoke,
    ) -> Option<(WinId, KeyResult)> {
        let (layer_id, result) = self.compositor.handle_mouse(event)?;
        let win = parse_float_layer_id(&layer_id)?;
        if let KeyResult::Action(action) = &result {
            if let Some((ev, payload)) = classify_widget_action(action) {
                if self.callbacks.has_event(win, ev) {
                    self.dispatch_event(win, ev, payload, lua_invoke);
                    return Some((win, KeyResult::Consumed));
                }
            }
        }
        Some((win, result))
    }

    /// Dispatch a mouse event directly to `win`'s component, bypassing
    /// hit-testing. Used while a drag-capture is in flight to deliver
    /// `Drag` and `Up` events to the same layer that received `Down`.
    pub fn handle_mouse_for(
        &mut self,
        win: WinId,
        event: crossterm::event::MouseEvent,
        lua_invoke: &mut LuaInvoke,
    ) -> Option<KeyResult> {
        let layer_id = float_layer_id(win);
        let result = self.compositor.handle_mouse_to(&layer_id, event)?;
        if let KeyResult::Action(action) = &result {
            if let Some((ev, payload)) = classify_widget_action(action) {
                if self.callbacks.has_event(win, ev) {
                    self.dispatch_event(win, ev, payload, lua_invoke);
                    return Some(KeyResult::Consumed);
                }
            }
        }
        Some(result)
    }

    /// Fire `WinEvent::Tick` on every window that has a registered
    /// Tick callback. Used by the app event loop to drive per-frame
    /// refresh of dialogs with live external state (subagent list,
    /// process registry, …).
    pub fn dispatch_tick(&mut self, lua_invoke: &mut LuaInvoke) {
        let wins: Vec<WinId> = self.callbacks.wins_with_event(WinEvent::Tick);
        for win in wins {
            self.dispatch_event(win, WinEvent::Tick, Payload::None, lua_invoke);
        }
    }

    pub fn focused_float(&self) -> Option<WinId> {
        let focused = self.compositor.focused()?;
        parse_float_layer_id(focused)
    }

    /// Buffer-bearing `Window` driving the user's current view, if any.
    /// Walks the focused float (a dialog) into its focused panel and
    /// returns that panel's `Window` when the panel is interactive.
    /// Falls through to `None` when the focused float is a picker /
    /// notification / widget panel — the host then decides whether to
    /// fall back to a split (transcript / prompt) for status purposes.
    /// Same model nvim uses: a focused float that isn't a "real" buffer
    /// view contributes no mode to the statusline.
    pub fn focused_dialog_buffer_window(&self) -> Option<&Window> {
        let win_id = self.focused_float()?;
        self.dialog(win_id)?.focused_buffer_window()
    }

    /// Topmost float (by zindex) whose rect contains (row, col). Used
    /// by the host's mouse dispatcher to route wheel / click events
    /// onto the float they actually land on — independent of focus —
    /// so scrolling an unfocused-but-visible float just works.
    pub fn float_at(&self, row: u16, col: u16) -> Option<WinId> {
        self.compositor
            .hit_test(row, col)
            .and_then(parse_float_layer_id)
    }

    /// Snapshot of every float window's `WinId`, in HashMap iteration
    /// order. Callers shouldn't rely on the ordering for painting —
    /// that's the compositor's job.
    pub fn float_ids(&self) -> Vec<WinId> {
        self.wins
            .iter()
            .filter_map(|(id, w)| matches!(w.config, WinConfig::Float(_)).then_some(*id))
            .collect()
    }

    /// Look up the `FloatConfig` for a window, if it's a float.
    /// Non-float windows (splits) return `None`.
    pub fn float_config(&self, win: WinId) -> Option<&FloatConfig> {
        match &self.wins.get(&win)?.config {
            WinConfig::Float(cfg) => Some(cfg),
            WinConfig::Split(_) => None,
        }
    }

    /// Mutable access to a float's `FloatConfig`. Used by callers that
    /// need to reposition a float in response to layout changes (e.g.
    /// the completer follows the prompt's top row). The next `render`
    /// call re-resolves the `Placement` automatically.
    pub fn float_config_mut(&mut self, win: WinId) -> Option<&mut FloatConfig> {
        match &mut self.wins.get_mut(&win)?.config {
            WinConfig::Float(cfg) => Some(cfg),
            WinConfig::Split(_) => None,
        }
    }

    pub fn force_redraw(&mut self) {
        self.compositor.force_redraw();
    }

    fn sync_float_content(&mut self) {
        let (tw, th) = self.terminal_size;
        // Pair each dialog layer with the content width it should
        // render at. Width is resolved from `Placement` alone — it
        // never depends on `natural_h`, so we can compute it before
        // the formatter runs — then we shave a column for the
        // potential scrollbar so wrapped content never bleeds under
        // the thumb.
        let targets: Vec<(String, u16)> = self
            .wins
            .iter()
            .filter(|(_, w)| w.is_float())
            .map(|(id, w)| {
                let width = match &w.config {
                    WinConfig::Float(fc) => {
                        resolve_float_rect(fc, tw, th, None, &self.split_rects).width
                    }
                    _ => tw,
                };
                (float_layer_id(*id), width.saturating_sub(1))
            })
            .collect();
        for (layer_id, content_width) in targets {
            // Split-borrow: `self.bufs` and `self.compositor` are
            // disjoint fields, so both mutable borrows coexist.
            let bufs = &mut self.bufs;
            if let Some(comp) = self.compositor.component_mut(&layer_id) {
                if let Some(dlg) = comp.as_any_mut().downcast_mut::<dialog::Dialog>() {
                    dlg.sync_from_bufs_mut(content_width, bufs);
                }
            }
        }
    }

    /// Re-resolve every float's rect from its `Placement` against the
    /// current terminal size and push it into the compositor. Called
    /// each frame from `render`, so floats track terminal resizes,
    /// prompt geometry changes, and dialog natural-height changes
    /// without any per-component wiring. Order matters: must run after
    /// `sync_float_content` so `Dialog::natural_height` reflects the
    /// current panel contents.
    fn sync_float_rects(&mut self) {
        for (id, rect) in self.resolve_float_rects() {
            self.compositor.set_rect(&float_layer_id(id), rect);
        }
    }
}

/// Map a widget-emitted `WidgetEvent` to a `(WinEvent, Payload)` pair
/// for auto-dispatch. Widgets (`OptionList`, `TextInput`, `Dialog`)
/// return typed events; this just fans them out to the callback
/// system's event names.
fn classify_widget_action(ev: &WidgetEvent) -> Option<(WinEvent, Payload)> {
    use WidgetEvent::*;
    Some(match ev {
        Dismiss => (WinEvent::Dismiss, Payload::None),
        Cancel => (WinEvent::Dismiss, Payload::None),
        Submit | SelectDefault => (WinEvent::Submit, Payload::None),
        TextChanged => (WinEvent::TextChanged, Payload::None),
        Select(index) => (WinEvent::Submit, Payload::Selection { index: *index }),
        SubmitText(content) => (
            WinEvent::Submit,
            Payload::Text {
                content: content.clone(),
            },
        ),
        // `Yank` is App-side only (clipboard copy) — caller handles it
        // off the returned `KeyResult::Action`, no dispatch path.
        Yank(_) => return None,
    })
}

fn resolve_constraint_dim(c: Constraint, total: u16) -> u16 {
    match c {
        Constraint::Fixed(n) => n.min(total),
        Constraint::Pct(pct) => ((total as u32 * pct as u32) / 100) as u16,
        Constraint::Fill => total,
    }
}

fn resolve_float_rect(
    fc: &FloatConfig,
    term_w: u16,
    term_h: u16,
    natural_h: Option<u16>,
    split_rects: &HashMap<WinId, Rect>,
) -> Rect {
    resolve_placement(&fc.placement, term_w, term_h, natural_h, split_rects)
}

fn resolve_placement(
    p: &layout::Placement,
    term_w: u16,
    term_h: u16,
    natural_h: Option<u16>,
    split_rects: &HashMap<WinId, Rect>,
) -> Rect {
    match p {
        layout::Placement::DockBottom {
            above_rows,
            full_width,
            max_width,
            max_height,
        } => {
            let avail_h = term_h.saturating_sub(*above_rows);
            let cap = resolve_constraint_dim(*max_height, avail_h).min(avail_h);
            // Dialogs / pickers report a `natural_h` derived from panel
            // content; clamp to the cap so the rect grows with content
            // up to the cap instead of always allocating the cap.
            // Floats without a natural height (cmdline) keep the cap.
            let h = match natural_h {
                Some(n) => n.min(cap),
                None => cap,
            };
            let w = if *full_width {
                term_w
            } else {
                resolve_constraint_dim(*max_width, term_w).min(term_w)
            };
            let left = (term_w.saturating_sub(w)) / 2;
            let top = term_h.saturating_sub(*above_rows).saturating_sub(h);
            Rect::new(top, left, w, h)
        }
        layout::Placement::FitContent { max } => {
            // Dock at bottom, full width, 1 status-bar row reserved.
            // Height = natural_h clamped to the `max` cap; natural_h
            // comes from `Ui::natural_dialog_height(win_id)` at the
            // call site. If the caller didn't supply one (rect is
            // being computed before the dialog registered — first
            // open), fall back to the cap so the dialog lands somewhere
            // sensible and the next render corrects it.
            let above_rows = 1u16;
            let avail_h = term_h.saturating_sub(above_rows);
            let cap = match max {
                layout::FitMax::HalfScreen => avail_h / 2,
                layout::FitMax::FullScreen => avail_h,
            };
            let h = natural_h.unwrap_or(cap).min(cap).min(avail_h).max(1);
            let top = term_h.saturating_sub(above_rows).saturating_sub(h);
            Rect::new(top, 0, term_w, h)
        }
        layout::Placement::Centered { width, height } => {
            let w = resolve_constraint_dim(*width, term_w).min(term_w);
            let h = resolve_constraint_dim(*height, term_h).min(term_h);
            let top = term_h.saturating_sub(h) / 2;
            let left = term_w.saturating_sub(w) / 2;
            Rect::new(top, left, w, h)
        }
        layout::Placement::AnchorCursor {
            row_offset,
            col_offset,
            width,
            height,
        } => {
            // Without an explicit caret position in the ui crate, treat
            // AnchorCursor as an offset from the top-left. The tui layer
            // sets the layer rect after construction.
            let w = resolve_constraint_dim(*width, term_w).min(term_w);
            let h = resolve_constraint_dim(*height, term_h).min(term_h);
            let top = (*row_offset).max(0) as u16;
            let left = (*col_offset).max(0) as u16;
            let w = w.min(term_w.saturating_sub(left));
            let h = h.min(term_h.saturating_sub(top));
            Rect::new(top, left, w, h)
        }
        layout::Placement::Manual {
            anchor,
            row,
            col,
            width,
            height,
        } => {
            let w = resolve_constraint_dim(*width, term_w);
            let h = resolve_constraint_dim(*height, term_h);
            let (top, left) = match anchor {
                layout::Anchor::NW => (*row, *col),
                layout::Anchor::NE => (*row, *col - w as i32),
                layout::Anchor::SW => (*row - h as i32, *col),
                layout::Anchor::SE => (*row - h as i32, *col - w as i32),
            };
            let top = top.max(0) as u16;
            let left = left.max(0) as u16;
            let w = w.min(term_w.saturating_sub(left));
            let h = h.min(term_h.saturating_sub(top));
            Rect::new(top, left, w, h)
        }
        layout::Placement::DockedAbove { target, max_height } => {
            // Target rect comes from `split_rects` (host app publishes
            // split-window rects each frame). If missing, fall back to
            // a centered-ish placement so the float still renders.
            let anchor_rect = split_rects
                .get(target)
                .copied()
                .unwrap_or_else(|| Rect::new(term_h / 2, 0, term_w, 1));
            let max_h = resolve_constraint_dim(*max_height, anchor_rect.top).max(1);
            let h = natural_h.unwrap_or(max_h).min(max_h).max(1);
            let top = anchor_rect.top.saturating_sub(h);
            Rect::new(top, anchor_rect.left, anchor_rect.width, h)
        }
    }
}

fn float_layer_id(win_id: WinId) -> String {
    format!("float:{}", win_id.0)
}

fn parse_float_layer_id(id: &str) -> Option<WinId> {
    id.strip_prefix("float:")?.parse::<u64>().ok().map(WinId)
}

impl Default for Ui {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ui() -> Ui {
        let mut ui = Ui::new();
        ui.set_terminal_size(80, 24);
        ui
    }

    fn open_stub_float(ui: &mut Ui, config: FloatConfig) -> WinId {
        ui.picker_open(
            config,
            vec![picker::PickerItem::new("stub")],
            0,
            picker::PickerStyle::default(),
            false,
        )
        .unwrap()
    }

    #[test]
    fn float_default_config() {
        let mut ui = make_ui();
        let win = open_stub_float(&mut ui, FloatConfig::default());
        let rect = ui.resolve_float(win).unwrap();
        // Default placement: Centered 80%x50%.
        assert_eq!(rect.width, 64);
        assert_eq!(rect.height, 12);
    }

    #[test]
    fn float_manual_placement() {
        let mut ui = make_ui();
        let win = open_stub_float(
            &mut ui,
            FloatConfig {
                placement: Placement::Manual {
                    anchor: Anchor::NW,
                    row: 4,
                    col: 10,
                    width: Constraint::Fixed(60),
                    height: Constraint::Fixed(16),
                },
                ..Default::default()
            },
        );
        let rect = ui.resolve_float(win).unwrap();
        assert_eq!(rect, Rect::new(4, 10, 60, 16));
    }

    #[test]
    fn float_se_anchor() {
        let mut ui = make_ui();
        let win = open_stub_float(
            &mut ui,
            FloatConfig {
                placement: Placement::Manual {
                    anchor: Anchor::SE,
                    row: 24,
                    col: 80,
                    width: Constraint::Fixed(40),
                    height: Constraint::Fixed(10),
                },
                ..Default::default()
            },
        );
        let rect = ui.resolve_float(win).unwrap();
        assert_eq!(rect, Rect::new(14, 40, 40, 10));
    }

    #[test]
    fn float_clamped_to_terminal() {
        let mut ui = make_ui();
        let win = open_stub_float(
            &mut ui,
            FloatConfig {
                placement: Placement::Manual {
                    anchor: Anchor::NW,
                    row: 20,
                    col: 70,
                    width: Constraint::Fixed(30),
                    height: Constraint::Fixed(10),
                },
                ..Default::default()
            },
        );
        let rect = ui.resolve_float(win).unwrap();
        assert_eq!(rect.width, 10);
        assert_eq!(rect.height, 4);
    }

    #[test]
    fn dock_bottom_full_width() {
        let mut ui = make_ui();
        let win = open_stub_float(
            &mut ui,
            FloatConfig {
                placement: Placement::dock_bottom_full_width(Constraint::Fixed(6)),
                ..Default::default()
            },
        );
        let rect = ui.resolve_float(win).unwrap();
        assert_eq!(rect.width, 80);
        // Fits content: stub picker has 1 item → natural_height = 1,
        // capped (but not raised) by the 6-row max.
        assert_eq!(rect.height, 1);
        assert_eq!(rect.left, 0);
        // above_rows=1 by default → top = 24 - 1 - 1 = 22
        assert_eq!(rect.top, 22);
    }

    #[test]
    fn floats_z_ordered_returns_sorted() {
        let mut ui = make_ui();
        let w1 = open_stub_float(
            &mut ui,
            FloatConfig {
                zindex: 100,
                ..Default::default()
            },
        );
        let w2 = open_stub_float(
            &mut ui,
            FloatConfig {
                zindex: 10,
                ..Default::default()
            },
        );
        let w3 = open_stub_float(
            &mut ui,
            FloatConfig {
                zindex: 50,
                ..Default::default()
            },
        );
        let ordered = ui.floats_z_ordered();
        assert_eq!(ordered, vec![w2, w3, w1]);
    }

    #[test]
    fn resolve_float_rects_matches_z_order() {
        let mut ui = make_ui();
        let w1 = open_stub_float(
            &mut ui,
            FloatConfig {
                zindex: 100,
                ..Default::default()
            },
        );
        let w2 = open_stub_float(
            &mut ui,
            FloatConfig {
                zindex: 10,
                ..Default::default()
            },
        );
        let rects = ui.resolve_float_rects();
        assert_eq!(rects.len(), 2);
        assert_eq!(rects[0].0, w2);
        assert_eq!(rects[1].0, w1);
    }

    /// Dismiss dispatch chain regression: dialog widget on Esc /
    /// Ctrl-C → `WidgetEvent::Dismiss` → `WinEvent::Dismiss` →
    /// registered callback fires via `lua_invoke`. Guards against
    /// silent breakage of the path Lua dialogs use to know when the
    /// user backed out.
    #[test]
    fn focused_dialog_esc_invokes_dismiss_callback() {
        use crate::dialog::{DialogConfig, PanelHeight, PanelSpec};
        use crossterm::event::{KeyCode, KeyModifiers};

        let mut ui = make_ui();
        let buf = ui.buf_create(buffer::BufCreateOpts {
            buftype: buffer::BufType::Scratch,
            modifiable: false,
        });
        let win = ui
            .dialog_open(
                FloatConfig {
                    focusable: true,
                    placement: Placement::DockBottom {
                        above_rows: 0,
                        full_width: true,
                        max_width: Constraint::Pct(100),
                        max_height: Constraint::Pct(100),
                    },
                    ..Default::default()
                },
                DialogConfig::default(),
                vec![PanelSpec::content(buf, PanelHeight::Fit)],
            )
            .unwrap();

        let invoked = std::sync::Arc::new(std::sync::Mutex::new(0u32));
        let invoked_cb = invoked.clone();
        ui.win_on_event(
            win,
            WinEvent::Dismiss,
            Callback::Rust(Box::new(move |_| {
                *invoked_cb.lock().unwrap() += 1;
                CallbackResult::Consumed
            })),
        );

        let _ = ui.handle_key_with_lua(KeyCode::Esc, KeyModifiers::NONE, &mut |_, _, _, _| {});
        assert_eq!(
            *invoked.lock().unwrap(),
            1,
            "Esc should invoke the registered Dismiss callback exactly once"
        );

        // Reopen and confirm Ctrl-C does the same.
        let buf2 = ui.buf_create(buffer::BufCreateOpts {
            buftype: buffer::BufType::Scratch,
            modifiable: false,
        });
        let win2 = ui
            .dialog_open(
                FloatConfig {
                    focusable: true,
                    placement: Placement::DockBottom {
                        above_rows: 0,
                        full_width: true,
                        max_width: Constraint::Pct(100),
                        max_height: Constraint::Pct(100),
                    },
                    ..Default::default()
                },
                DialogConfig::default(),
                vec![PanelSpec::content(buf2, PanelHeight::Fit)],
            )
            .unwrap();
        let invoked2 = std::sync::Arc::new(std::sync::Mutex::new(0u32));
        let invoked2_cb = invoked2.clone();
        ui.win_on_event(
            win2,
            WinEvent::Dismiss,
            Callback::Rust(Box::new(move |_| {
                *invoked2_cb.lock().unwrap() += 1;
                CallbackResult::Consumed
            })),
        );
        let _ = ui.handle_key_with_lua(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
            &mut |_, _, _, _| {},
        );
        assert_eq!(*invoked2.lock().unwrap(), 1, "Ctrl-C should fire Dismiss");
    }
}
