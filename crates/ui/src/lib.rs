pub mod buffer;
pub mod buffer_view;
pub mod component;
pub mod compositor;
pub mod dialog;
pub mod edit_buffer;
pub mod float_dialog;
pub mod float_render;
pub mod flush;
pub mod grid;
pub mod kill_ring;
pub mod layout;
pub mod list_select;
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

pub use buffer::{BufType, Buffer, Span, SpanStyle};
pub use buffer_view::BufferView;
pub use component::{Component, CursorInfo, CursorStyle, DrawContext, KeyResult};
pub use compositor::Compositor;
pub use dialog::{Dialog, DialogConfig, PanelHeight, PanelKind, PanelSpec, SeparatorStyle};
pub use edit_buffer::EditBuffer;
pub use float_dialog::{FloatDialog, FloatDialogConfig};
pub use flush::flush_diff;
pub use grid::{Cell, Grid, GridSlice, Style};
pub use id::{BufId, WinId};
pub use kill_ring::KillRing;
pub use layout::{Anchor, Border, Constraint, FloatRelative, Gutters, LayoutTree, Placement, Rect};
pub use list_select::{ListItem, ListSelect};
pub use status_bar::{StatusBar, StatusSegment};
pub use style::{HlAttrs, HlGroup};
pub use text_input::TextInput;
pub use undo::{UndoEntry, UndoHistory};
pub use vim::{ViMode, Vim};
pub use window::{
    FloatConfig, ScrollbarState, SplitConfig, ViewportHit, WinConfig, Window, WindowViewport,
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
}

impl Ui {
    pub fn new() -> Self {
        Self {
            bufs: HashMap::new(),
            wins: HashMap::new(),
            current_win: None,
            next_buf_id: 1,
            next_win_id: 1,
            layout: None,
            terminal_size: (80, 24),
            compositor: Compositor::new(80, 24),
        }
    }

    pub fn buf_create(&mut self, opts: buffer::BufCreateOpts) -> BufId {
        let id = BufId(self.next_buf_id);
        self.next_buf_id += 1;
        let buf = Buffer::new(id, opts);
        self.bufs.insert(id, buf);
        id
    }

    pub fn buf_create_with_id(&mut self, id: BufId, opts: buffer::BufCreateOpts) -> BufId {
        let buf = Buffer::new(id, opts);
        self.bufs.insert(id, buf);
        if id.0 >= self.next_buf_id {
            self.next_buf_id = id.0 + 1;
        }
        id
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

    pub fn win_open_float(&mut self, buf: BufId, config: FloatConfig) -> Option<WinId> {
        if !self.bufs.contains_key(&buf) {
            return None;
        }
        let id = WinId(self.next_win_id);
        self.next_win_id += 1;

        let (tw, th) = self.terminal_size;
        let rect = resolve_float_rect(&config, tw, th);
        let zindex = config.zindex;

        let dialog_config = FloatDialogConfig {
            title: config.title.clone(),
            border: config.border,
            ..FloatDialogConfig::default()
        };
        let mut dialog = FloatDialog::new(dialog_config);
        if let Some(b) = self.bufs.get(&buf) {
            dialog.sync_content_from_buffer(b);
        }

        let win = Window::new(id, buf, WinConfig::Float(config));
        self.wins.insert(id, win);

        let layer_id = float_layer_id(id);
        self.compositor
            .add(&layer_id, Box::new(dialog), rect, zindex);
        self.compositor.focus(&layer_id);

        Some(id)
    }

    pub fn win_close(&mut self, id: WinId) {
        let layer_id = float_layer_id(id);
        self.compositor.remove(&layer_id);
        self.wins.remove(&id);
        if self.current_win == Some(id) {
            self.current_win = self.wins.keys().next().copied();
        }
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
        dialog_config: dialog::DialogConfig,
        panels: Vec<dialog::PanelSpec>,
    ) -> Option<WinId> {
        if !panels.iter().all(|p| self.bufs.contains_key(&p.buf)) {
            return None;
        }
        let id = WinId(self.next_win_id);
        self.next_win_id += 1;

        let (tw, th) = self.terminal_size;
        let rect = resolve_float_rect(&float_config, tw, th);
        let zindex = float_config.zindex;

        // Use the first panel's buffer as the dialog window's "buf"
        // pointer for registry purposes (dialogs are multi-buffer).
        let primary_buf = panels.first().map(|p| p.buf).unwrap_or(BufId(0));

        let panel_structs = dialog::build_panels(panels, &self.bufs);
        let dlg = dialog::Dialog::new(dialog_config, panel_structs);

        let win = Window::new(id, primary_buf, WinConfig::Float(float_config));
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
        Some(resolve_float_rect(fc, tw, th))
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
                Some((id, resolve_float_rect(fc, tw, th)))
            })
            .collect()
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
        self.compositor.render(w)
    }

    pub fn handle_key(
        &mut self,
        code: crossterm::event::KeyCode,
        mods: crossterm::event::KeyModifiers,
    ) -> KeyResult {
        self.compositor.handle_key(code, mods)
    }

    pub fn focused_float(&self) -> Option<WinId> {
        let focused = self.compositor.focused()?;
        parse_float_layer_id(focused)
    }

    pub fn float_dialog_mut(&mut self, win_id: WinId) -> Option<&mut FloatDialog> {
        let layer_id = float_layer_id(win_id);
        let comp = self.compositor.component_mut(&layer_id)?;
        comp.as_any_mut().downcast_mut::<FloatDialog>()
    }

    pub fn force_redraw(&mut self) {
        self.compositor.force_redraw();
    }

    fn sync_float_content(&mut self) {
        let float_wins: Vec<(WinId, BufId)> = self
            .wins
            .iter()
            .filter(|(_, w)| w.is_float())
            .map(|(id, w)| (*id, w.buf))
            .collect();

        for (win_id, buf_id) in float_wins {
            let layer_id = float_layer_id(win_id);
            if let Some(buf) = self.bufs.get(&buf_id).cloned() {
                if let Some(comp) = self.compositor.component_mut(&layer_id) {
                    if let Some(fd) = comp.as_any_mut().downcast_mut::<FloatDialog>() {
                        fd.sync_content_from_buffer(&buf);
                    }
                }
            }
        }

        // Sync all Dialog panels from their buffers.
        let dialog_layers: Vec<String> = self
            .wins
            .iter()
            .filter(|(_, w)| w.is_float())
            .map(|(id, _)| float_layer_id(*id))
            .collect();
        for layer_id in dialog_layers {
            if let Some(comp) = self.compositor.component_mut(&layer_id) {
                if let Some(dlg) = comp.as_any_mut().downcast_mut::<dialog::Dialog>() {
                    dlg.sync_from_bufs(|bid| self.bufs.get(&bid));
                }
            }
        }
    }

    pub fn render_floats<W: std::io::Write>(&self, w: &mut W) -> std::io::Result<()> {
        for (win_id, rect) in self.resolve_float_rects() {
            let win = match self.wins.get(&win_id) {
                Some(w) => w,
                None => continue,
            };
            let buf = match self.bufs.get(&win.buf) {
                Some(b) => b,
                None => continue,
            };
            let (border, title_style) = match &win.config {
                WinConfig::Float(fc) => (fc.border, buffer::SpanStyle::default()),
                _ => continue,
            };
            let frame = float_render::FloatFrame {
                rect,
                border,
                title: win.title().map(String::from),
                title_style,
                scroll_offset: win.scroll_top as usize,
            };
            float_render::render_float(w, buf, &frame)?;
        }
        Ok(())
    }
}

fn resolve_constraint_dim(c: Constraint, total: u16) -> u16 {
    match c {
        Constraint::Fixed(n) => n.min(total),
        Constraint::Pct(pct) => ((total as u32 * pct as u32) / 100) as u16,
        Constraint::Fill => total,
    }
}

fn resolve_float_rect(fc: &FloatConfig, term_w: u16, term_h: u16) -> Rect {
    resolve_placement(&fc.placement, term_w, term_h)
}

fn resolve_placement(p: &layout::Placement, term_w: u16, term_h: u16) -> Rect {
    match p {
        layout::Placement::DockBottom {
            above_rows,
            full_width,
            max_width,
            max_height,
        } => {
            let avail_h = term_h.saturating_sub(*above_rows);
            let h = resolve_constraint_dim(*max_height, avail_h).min(avail_h);
            let w = if *full_width {
                term_w
            } else {
                resolve_constraint_dim(*max_width, term_w).min(term_w)
            };
            let left = (term_w.saturating_sub(w)) / 2;
            let top = term_h.saturating_sub(*above_rows).saturating_sub(h);
            Rect::new(top, left, w, h)
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

    #[test]
    fn float_default_config() {
        let mut ui = make_ui();
        let buf = ui.buf_create(buffer::BufCreateOpts::default());
        let win = ui.win_open_float(buf, FloatConfig::default()).unwrap();
        let rect = ui.resolve_float(win).unwrap();
        // Default placement: Centered 80%x50%.
        assert_eq!(rect.width, 64);
        assert_eq!(rect.height, 12);
    }

    #[test]
    fn float_manual_placement() {
        let mut ui = make_ui();
        let buf = ui.buf_create(buffer::BufCreateOpts::default());
        let win = ui
            .win_open_float(
                buf,
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
            )
            .unwrap();
        let rect = ui.resolve_float(win).unwrap();
        assert_eq!(rect, Rect::new(4, 10, 60, 16));
    }

    #[test]
    fn float_se_anchor() {
        let mut ui = make_ui();
        let buf = ui.buf_create(buffer::BufCreateOpts::default());
        let win = ui
            .win_open_float(
                buf,
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
            )
            .unwrap();
        let rect = ui.resolve_float(win).unwrap();
        assert_eq!(rect, Rect::new(14, 40, 40, 10));
    }

    #[test]
    fn float_clamped_to_terminal() {
        let mut ui = make_ui();
        let buf = ui.buf_create(buffer::BufCreateOpts::default());
        let win = ui
            .win_open_float(
                buf,
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
            )
            .unwrap();
        let rect = ui.resolve_float(win).unwrap();
        assert_eq!(rect.width, 10);
        assert_eq!(rect.height, 4);
    }

    #[test]
    fn dock_bottom_full_width() {
        let mut ui = make_ui();
        let buf = ui.buf_create(buffer::BufCreateOpts::default());
        let win = ui
            .win_open_float(
                buf,
                FloatConfig {
                    placement: Placement::dock_bottom_full_width(Constraint::Fixed(6)),
                    ..Default::default()
                },
            )
            .unwrap();
        let rect = ui.resolve_float(win).unwrap();
        assert_eq!(rect.width, 80);
        assert_eq!(rect.height, 6);
        assert_eq!(rect.left, 0);
        // above_rows=1 by default → top = 24 - 1 - 6 = 17
        assert_eq!(rect.top, 17);
    }

    #[test]
    fn floats_z_ordered_returns_sorted() {
        let mut ui = make_ui();
        let buf = ui.buf_create(buffer::BufCreateOpts::default());
        let w1 = ui
            .win_open_float(
                buf,
                FloatConfig {
                    zindex: 100,
                    ..Default::default()
                },
            )
            .unwrap();
        let w2 = ui
            .win_open_float(
                buf,
                FloatConfig {
                    zindex: 10,
                    ..Default::default()
                },
            )
            .unwrap();
        let w3 = ui
            .win_open_float(
                buf,
                FloatConfig {
                    zindex: 50,
                    ..Default::default()
                },
            )
            .unwrap();
        let ordered = ui.floats_z_ordered();
        assert_eq!(ordered, vec![w2, w3, w1]);
    }

    #[test]
    fn resolve_float_rects_matches_z_order() {
        let mut ui = make_ui();
        let buf = ui.buf_create(buffer::BufCreateOpts::default());
        let _w1 = ui
            .win_open_float(
                buf,
                FloatConfig {
                    zindex: 100,
                    ..Default::default()
                },
            )
            .unwrap();
        let _w2 = ui
            .win_open_float(
                buf,
                FloatConfig {
                    zindex: 10,
                    ..Default::default()
                },
            )
            .unwrap();
        let rects = ui.resolve_float_rects();
        assert_eq!(rects.len(), 2);
        assert_eq!(rects[0].0, _w2);
        assert_eq!(rects[1].0, _w1);
    }
}
