pub mod buffer;
pub mod buffer_view;
pub mod component;
pub mod compositor;
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
pub use component::{Component, DrawContext, KeyResult};
pub use compositor::Compositor;
pub use edit_buffer::EditBuffer;
pub use float_dialog::{FloatDialog, FloatDialogConfig};
pub use flush::flush_diff;
pub use grid::{Cell, Grid, GridSlice, Style};
pub use id::{BufId, WinId};
pub use kill_ring::KillRing;
pub use layout::{Anchor, Border, Constraint, FloatRelative, Gutters, LayoutTree, Rect};
pub use list_select::{ListItem, ListSelect};
pub use status_bar::{StatusBar, StatusSegment};
pub use style::{HlAttrs, HlGroup};
pub use text_input::TextInput;
pub use undo::{UndoEntry, UndoHistory};
pub use vim::{ViMode, Vim};
pub use window::{FloatConfig, SplitConfig, WinConfig, Window};
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
        let win = Window::new(id, buf, WinConfig::Float(config));
        self.wins.insert(id, win);
        Some(id)
    }

    pub fn win_close(&mut self, id: WinId) {
        self.wins.remove(&id);
        if self.current_win == Some(id) {
            self.current_win = self.wins.keys().next().copied();
        }
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
    let w = resolve_constraint_dim(fc.width, term_w);
    let h = resolve_constraint_dim(fc.height, term_h);

    let (anchor_row, anchor_col) = match fc.relative {
        FloatRelative::Editor => (0i32, 0i32),
        FloatRelative::Cursor => (0, 0),
    };

    let (top, left) = match fc.anchor {
        Anchor::NW => (anchor_row + fc.row, anchor_col + fc.col),
        Anchor::NE => (anchor_row + fc.row, anchor_col + fc.col - w as i32),
        Anchor::SW => (anchor_row + fc.row - h as i32, anchor_col + fc.col),
        Anchor::SE => (
            anchor_row + fc.row - h as i32,
            anchor_col + fc.col - w as i32,
        ),
    };

    let top = top.max(0) as u16;
    let left = left.max(0) as u16;
    let w = w.min(term_w.saturating_sub(left));
    let h = h.min(term_h.saturating_sub(top));

    Rect::new(top, left, w, h)
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
        // Default: 80% width, 50% height, NW anchor at (0,0)
        assert_eq!(rect.width, 64); // 80% of 80
        assert_eq!(rect.height, 12); // 50% of 24
        assert_eq!(rect.top, 0);
        assert_eq!(rect.left, 0);
    }

    #[test]
    fn float_centered() {
        let mut ui = make_ui();
        let buf = ui.buf_create(buffer::BufCreateOpts::default());
        let win = ui
            .win_open_float(
                buf,
                FloatConfig {
                    anchor: Anchor::NW,
                    row: 4,
                    col: 10,
                    width: Constraint::Fixed(60),
                    height: Constraint::Fixed(16),
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
                    anchor: Anchor::SE,
                    row: 24,
                    col: 80,
                    width: Constraint::Fixed(40),
                    height: Constraint::Fixed(10),
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
                    anchor: Anchor::NW,
                    row: 20,
                    col: 70,
                    width: Constraint::Fixed(30),
                    height: Constraint::Fixed(10),
                    ..Default::default()
                },
            )
            .unwrap();
        let rect = ui.resolve_float(win).unwrap();
        // Width clamped: 80 - 70 = 10
        assert_eq!(rect.width, 10);
        // Height clamped: 24 - 20 = 4
        assert_eq!(rect.height, 4);
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
