//! `OptionList` — a `PanelWidget` for selectable lists.
//!
//! Covers the cases where a `PanelKind::List` (static buffer with
//! cursor) isn't enough: multi-select with checkboxes, per-item
//! shortcut keys (Confirm's `a` / `n` / `e` / `l`), and
//! widget-managed cursor state.

use crate::component::{CursorInfo, DrawContext, KeyResult};
use crate::dialog::PanelWidget;
use crate::grid::{GridSlice, Style};
use crate::layout::Rect;
use crossterm::event::{KeyCode, KeyModifiers};

#[derive(Clone, Debug)]
pub struct OptionItem {
    pub label: String,
    /// Single character shortcut. When the user types this key (no
    /// modifiers), the widget emits `Action("shortcut:{char}")`.
    pub shortcut: Option<char>,
}

impl OptionItem {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            shortcut: None,
        }
    }

    pub fn with_shortcut(mut self, c: char) -> Self {
        self.shortcut = Some(c);
        self
    }
}

pub struct OptionList {
    items: Vec<OptionItem>,
    cursor: usize,
    scroll_top: usize,
    multi: bool,
    toggles: Vec<bool>,
    row_style: Style,
    cursor_style: Style,
    shortcut_style: Style,
    checkbox_style: Style,
    viewport_rows: u16,
}

impl OptionList {
    pub fn new(items: Vec<OptionItem>) -> Self {
        let n = items.len();
        Self {
            items,
            cursor: 0,
            scroll_top: 0,
            multi: false,
            toggles: vec![false; n],
            row_style: Style::default(),
            cursor_style: Style::default(),
            shortcut_style: Style::default(),
            checkbox_style: Style::default(),
            viewport_rows: 0,
        }
    }

    pub fn multi(mut self, multi: bool) -> Self {
        self.multi = multi;
        self
    }

    pub fn with_row_style(mut self, style: Style) -> Self {
        self.row_style = style;
        self
    }

    pub fn with_cursor_style(mut self, style: Style) -> Self {
        self.cursor_style = style;
        self
    }

    pub fn with_shortcut_style(mut self, style: Style) -> Self {
        self.shortcut_style = style;
        self
    }

    pub fn with_checkbox_style(mut self, style: Style) -> Self {
        self.checkbox_style = style;
        self
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn set_cursor(&mut self, idx: usize) {
        if self.items.is_empty() {
            return;
        }
        self.cursor = idx.min(self.items.len() - 1);
        self.ensure_visible();
    }

    pub fn toggles(&self) -> &[bool] {
        &self.toggles
    }

    pub fn toggled_indices(&self) -> Vec<usize> {
        self.toggles
            .iter()
            .enumerate()
            .filter_map(|(i, &t)| if t { Some(i) } else { None })
            .collect()
    }

    pub fn set_toggled(&mut self, idx: usize, on: bool) {
        if let Some(slot) = self.toggles.get_mut(idx) {
            *slot = on;
        }
    }

    pub fn items(&self) -> &[OptionItem] {
        &self.items
    }

    fn move_cursor(&mut self, delta: isize) {
        if self.items.is_empty() {
            return;
        }
        let max = self.items.len() as isize - 1;
        let new = (self.cursor as isize + delta).clamp(0, max);
        self.cursor = new as usize;
        self.ensure_visible();
    }

    fn ensure_visible(&mut self) {
        let rows = self.viewport_rows as usize;
        if rows == 0 {
            return;
        }
        if self.cursor < self.scroll_top {
            self.scroll_top = self.cursor;
        } else if self.cursor >= self.scroll_top + rows {
            self.scroll_top = self.cursor + 1 - rows;
        }
    }
}

impl PanelWidget for OptionList {
    fn prepare(&mut self, area: Rect, _ctx: &DrawContext) {
        self.viewport_rows = area.height;
        self.ensure_visible();
    }

    fn draw(&self, _area: Rect, slice: &mut GridSlice<'_>, _ctx: &DrawContext) {
        let w = slice.width();
        let h = slice.height();
        if w == 0 || h == 0 {
            return;
        }
        let visible = (h as usize).min(self.items.len().saturating_sub(self.scroll_top));
        for row in 0..visible {
            let idx = self.scroll_top + row;
            let item = &self.items[idx];
            let is_cursor = idx == self.cursor;
            let base = if is_cursor {
                self.cursor_style
            } else {
                self.row_style
            };

            let mut col: u16 = 0;
            // Checkbox for multi-select.
            if self.multi {
                let glyph = if self.toggles[idx] { '☒' } else { '☐' };
                slice.set(col, row as u16, glyph, self.checkbox_style);
                col = col.saturating_add(1);
                if col < w {
                    slice.set(col, row as u16, ' ', base);
                    col = col.saturating_add(1);
                }
            }

            // Shortcut, e.g. `(a)`.
            if let Some(sc) = item.shortcut {
                for ch in ['(', sc, ')'] {
                    if col >= w {
                        break;
                    }
                    slice.set(col, row as u16, ch, self.shortcut_style);
                    col = col.saturating_add(1);
                }
                if col < w {
                    slice.set(col, row as u16, ' ', base);
                    col = col.saturating_add(1);
                }
            }

            // Label.
            for ch in item.label.chars() {
                if col >= w {
                    break;
                }
                slice.set(col, row as u16, ch, base);
                col = col.saturating_add(1);
            }
            // Fill remainder so cursor-row highlight extends full width.
            while col < w {
                slice.set(col, row as u16, ' ', base);
                col = col.saturating_add(1);
            }
        }
    }

    fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> KeyResult {
        if self.items.is_empty() {
            return KeyResult::Ignored;
        }
        // Per-item shortcut characters take priority over default
        // navigation keys. Enables Confirm's `a` / `n` / `e` / `l`.
        if mods == KeyModifiers::NONE {
            if let KeyCode::Char(c) = code {
                if self.items.iter().any(|it| it.shortcut == Some(c)) {
                    return KeyResult::Action(format!("shortcut:{c}"));
                }
            }
        }
        match (code, mods) {
            (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                self.move_cursor(-1);
                KeyResult::Consumed
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                self.move_cursor(1);
                KeyResult::Consumed
            }
            (KeyCode::PageUp, _) | (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                let page = (self.viewport_rows.max(1) as isize) / 2;
                self.move_cursor(-page);
                KeyResult::Consumed
            }
            (KeyCode::PageDown, _) | (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                let page = (self.viewport_rows.max(1) as isize) / 2;
                self.move_cursor(page);
                KeyResult::Consumed
            }
            (KeyCode::Home, _) | (KeyCode::Char('g'), KeyModifiers::NONE) => {
                self.move_cursor(isize::MIN / 2);
                KeyResult::Consumed
            }
            (KeyCode::End, _) | (KeyCode::Char('G'), KeyModifiers::SHIFT) => {
                self.move_cursor(isize::MAX / 2);
                KeyResult::Consumed
            }
            (KeyCode::Char(' '), KeyModifiers::NONE) if self.multi => {
                self.toggles[self.cursor] = !self.toggles[self.cursor];
                KeyResult::Consumed
            }
            (KeyCode::Enter, _) => {
                if self.multi {
                    KeyResult::Action("submit".into())
                } else {
                    KeyResult::Action(format!("select:{}", self.cursor))
                }
            }
            (KeyCode::Char(c), KeyModifiers::NONE) if c.is_ascii_digit() && c != '0' => {
                let idx = (c as u8 - b'1') as usize;
                if idx < self.items.len() {
                    KeyResult::Action(format!("select:{idx}"))
                } else {
                    KeyResult::Ignored
                }
            }
            _ => KeyResult::Ignored,
        }
    }

    fn cursor(&self) -> Option<CursorInfo> {
        // The cursor row is drawn with the accent row style; no
        // hardware cursor glyph is shown.
        None
    }

    fn content_rows(&self) -> usize {
        self.items.len()
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::Grid;

    fn items3() -> Vec<OptionItem> {
        vec![
            OptionItem::new("Yes").with_shortcut('y'),
            OptionItem::new("No").with_shortcut('n'),
            OptionItem::new("Always").with_shortcut('a'),
        ]
    }

    fn ctx(w: u16, h: u16) -> DrawContext {
        DrawContext {
            terminal_width: w,
            terminal_height: h,
            focused: true,
        }
    }

    #[test]
    fn up_down_moves_cursor() {
        let mut ol = OptionList::new(items3());
        assert_eq!(ol.cursor(), 0);
        ol.handle_key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(ol.cursor(), 1);
        ol.handle_key(KeyCode::Down, KeyModifiers::NONE);
        ol.handle_key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(ol.cursor(), 2); // clamped
        ol.handle_key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(ol.cursor(), 1);
    }

    #[test]
    fn enter_returns_select_action() {
        let mut ol = OptionList::new(items3());
        ol.handle_key(KeyCode::Down, KeyModifiers::NONE);
        let r = ol.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(r, KeyResult::Action("select:1".into()));
    }

    #[test]
    fn shortcut_char_returns_shortcut_action() {
        let mut ol = OptionList::new(items3());
        let r = ol.handle_key(KeyCode::Char('a'), KeyModifiers::NONE);
        assert_eq!(r, KeyResult::Action("shortcut:a".into()));
    }

    #[test]
    fn digit_selects_by_index() {
        let mut ol = OptionList::new(items3());
        let r = ol.handle_key(KeyCode::Char('2'), KeyModifiers::NONE);
        assert_eq!(r, KeyResult::Action("select:1".into()));
    }

    #[test]
    fn space_toggles_in_multi_mode() {
        let mut ol = OptionList::new(items3()).multi(true);
        ol.handle_key(KeyCode::Char(' '), KeyModifiers::NONE);
        ol.handle_key(KeyCode::Down, KeyModifiers::NONE);
        ol.handle_key(KeyCode::Char(' '), KeyModifiers::NONE);
        assert_eq!(ol.toggled_indices(), vec![0, 1]);
        let r = ol.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(r, KeyResult::Action("submit".into()));
    }

    #[test]
    fn renders_label_with_shortcut() {
        let ol = OptionList::new(items3());
        let mut grid = Grid::new(20, 3);
        let area = Rect::new(0, 0, 20, 3);
        let mut slice = grid.slice_mut(area);
        ol.draw(area, &mut slice, &ctx(20, 3));
        // Row 0: "(y) Yes" then spaces.
        assert_eq!(grid.cell(0, 0).symbol, '(');
        assert_eq!(grid.cell(1, 0).symbol, 'y');
        assert_eq!(grid.cell(2, 0).symbol, ')');
        assert_eq!(grid.cell(4, 0).symbol, 'Y');
        assert_eq!(grid.cell(5, 0).symbol, 'e');
        assert_eq!(grid.cell(6, 0).symbol, 's');
    }

    #[test]
    fn renders_multi_checkboxes() {
        let mut ol = OptionList::new(items3()).multi(true);
        ol.set_toggled(0, true);
        let mut grid = Grid::new(20, 3);
        let area = Rect::new(0, 0, 20, 3);
        let mut slice = grid.slice_mut(area);
        ol.draw(area, &mut slice, &ctx(20, 3));
        assert_eq!(grid.cell(0, 0).symbol, '☒');
        assert_eq!(grid.cell(0, 1).symbol, '☐');
    }
}
