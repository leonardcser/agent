pub mod buffer;
pub mod cursor;
pub mod layout;
pub mod style;
pub mod window;

mod id;

pub use buffer::{BufType, Buffer};
pub use cursor::Cursor;
pub use id::{BufId, WinId};
pub use layout::{Anchor, Border, Constraint, FloatRelative, Gutters, LayoutTree, Rect};
pub use style::{HlAttrs, HlGroup};
pub use window::{FloatConfig, SplitConfig, WinConfig, Window};

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
}

impl Default for Ui {
    fn default() -> Self {
        Self::new()
    }
}
