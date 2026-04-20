use crate::grid::GridSlice;
use crate::layout::Rect;
use crossterm::event::{KeyCode, KeyModifiers};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyResult {
    Consumed,
    Ignored,
    Action(String),
}

pub struct DrawContext {
    pub terminal_width: u16,
    pub terminal_height: u16,
    pub focused: bool,
}

pub trait Component {
    fn draw(&self, area: Rect, grid: &mut GridSlice<'_>, ctx: &DrawContext);
    fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> KeyResult;
    fn cursor(&self) -> Option<(u16, u16)> {
        None
    }
}
