//! `OptionList` — a `PanelWidget` for selectable lists.
//!
//! Covers the cases where a `PanelKind::List` (static buffer with
//! cursor) isn't enough: multi-select with checkboxes, per-item
//! shortcut keys (Confirm's `a` / `n` / `e` / `l`), and
//! widget-managed cursor state.

use crate::component::{Component, CursorInfo, DrawContext, KeyResult, WidgetEvent};
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
    /// Per-item wrapped label lines computed in `prepare`. Each item
    /// occupies `wrapped[i].len().max(1)` visual rows.
    wrapped: Vec<Vec<String>>,
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
            wrapped: Vec::new(),
        }
    }

    /// Byte width of the checkbox/shortcut prefix before the label.
    fn prefix_cols(&self, item: &OptionItem) -> u16 {
        let mut cols: u16 = 0;
        if self.multi {
            cols += 2; // checkbox + space
        }
        if item.shortcut.is_some() {
            cols += 4; // "(X) "
        }
        cols
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

impl Component for OptionList {
    fn prepare(&mut self, area: Rect, _ctx: &DrawContext) {
        self.viewport_rows = area.height;
        // Re-wrap labels for the current width so multi-line items
        // render correctly. `prefix_cols` depends on `multi` +
        // per-item shortcut; both are stable for a widget instance.
        self.wrapped = self
            .items
            .iter()
            .map(|item| {
                let avail = area.width.saturating_sub(self.prefix_cols(item));
                if avail == 0 {
                    vec![item.label.clone()]
                } else {
                    crate::text::wrap_line(&item.label, avail as usize)
                }
            })
            .collect();
        self.ensure_visible();
    }

    fn draw(&self, _area: Rect, slice: &mut GridSlice<'_>, _ctx: &DrawContext) {
        let w = slice.width();
        let h = slice.height();
        if w == 0 || h == 0 {
            return;
        }
        let mut row: u16 = 0;
        let mut idx = self.scroll_top;
        while row < h && idx < self.items.len() {
            let item = &self.items[idx];
            let is_cursor = idx == self.cursor;
            let base = if is_cursor {
                self.cursor_style
            } else {
                self.row_style
            };
            let wrapped = self
                .wrapped
                .get(idx)
                .cloned()
                .unwrap_or_else(|| vec![item.label.clone()]);
            let prefix_w = self.prefix_cols(item);
            for (line_i, line_text) in wrapped.iter().enumerate() {
                if row >= h {
                    break;
                }
                let mut col: u16 = 0;
                if line_i == 0 {
                    // Checkbox for multi-select on the first line.
                    if self.multi {
                        let glyph = if self.toggles[idx] { '☒' } else { '☐' };
                        slice.set(col, row, glyph, self.checkbox_style);
                        col = col.saturating_add(1);
                        if col < w {
                            slice.set(col, row, ' ', base);
                            col = col.saturating_add(1);
                        }
                    }
                    // Shortcut `(X) `.
                    if let Some(sc) = item.shortcut {
                        for ch in ['(', sc, ')'] {
                            if col >= w {
                                break;
                            }
                            slice.set(col, row, ch, self.shortcut_style);
                            col = col.saturating_add(1);
                        }
                        if col < w {
                            slice.set(col, row, ' ', base);
                            col = col.saturating_add(1);
                        }
                    }
                } else {
                    // Continuation rows: indent to match the label column.
                    while col < prefix_w && col < w {
                        slice.set(col, row, ' ', base);
                        col = col.saturating_add(1);
                    }
                }
                // Label chars for this wrapped line.
                for ch in line_text.chars() {
                    if col >= w {
                        break;
                    }
                    slice.set(col, row, ch, base);
                    col = col.saturating_add(1);
                }
                // Fill rest of row so cursor highlight extends full width.
                while col < w {
                    slice.set(col, row, ' ', base);
                    col = col.saturating_add(1);
                }
                row = row.saturating_add(1);
            }
            idx += 1;
        }
    }

    fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> KeyResult {
        if self.items.is_empty() {
            return KeyResult::Ignored;
        }
        // Per-item shortcut characters take priority over default
        // navigation keys. Moves the cursor to the matched item and
        // emits `select:N` — host code only has to handle the
        // Selection payload, never raw shortcut strings.
        if mods == KeyModifiers::NONE {
            if let KeyCode::Char(c) = code {
                if let Some(idx) = self.items.iter().position(|it| it.shortcut == Some(c)) {
                    self.cursor = idx;
                    return KeyResult::Action(WidgetEvent::Select(idx));
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
                    KeyResult::Action(WidgetEvent::Submit)
                } else {
                    KeyResult::Action(WidgetEvent::Select(self.cursor))
                }
            }
            (KeyCode::Char(c), KeyModifiers::NONE) if c.is_ascii_digit() && c != '0' => {
                let idx = (c as u8 - b'1') as usize;
                if idx < self.items.len() {
                    KeyResult::Action(WidgetEvent::Select(idx))
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

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

impl PanelWidget for OptionList {
    fn content_rows(&self) -> usize {
        if self.wrapped.is_empty() {
            self.items.len()
        } else {
            self.wrapped.iter().map(|l| l.len().max(1)).sum()
        }
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
        assert_eq!(r, KeyResult::Action(WidgetEvent::Select(1)));
    }

    #[test]
    fn shortcut_char_selects_matched_item() {
        let mut ol = OptionList::new(items3());
        let r = ol.handle_key(KeyCode::Char('a'), KeyModifiers::NONE);
        assert_eq!(r, KeyResult::Action(WidgetEvent::Select(2)));
        assert_eq!(ol.cursor(), 2);
    }

    #[test]
    fn digit_selects_by_index() {
        let mut ol = OptionList::new(items3());
        let r = ol.handle_key(KeyCode::Char('2'), KeyModifiers::NONE);
        assert_eq!(r, KeyResult::Action(WidgetEvent::Select(1)));
    }

    #[test]
    fn space_toggles_in_multi_mode() {
        let mut ol = OptionList::new(items3()).multi(true);
        ol.handle_key(KeyCode::Char(' '), KeyModifiers::NONE);
        ol.handle_key(KeyCode::Down, KeyModifiers::NONE);
        ol.handle_key(KeyCode::Char(' '), KeyModifiers::NONE);
        assert_eq!(ol.toggled_indices(), vec![0, 1]);
        let r = ol.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(r, KeyResult::Action(WidgetEvent::Submit));
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
    fn wraps_long_labels_after_prepare() {
        let mut ol = OptionList::new(vec![OptionItem::new(
            "always allow long-tool in /tmp/workspace/deeply/nested",
        )]);
        let area = Rect::new(0, 0, 20, 10);
        ol.prepare(area, &ctx(20, 10));
        // With width 20 and no shortcut/checkbox prefix, a ~50-char
        // label must wrap into multiple lines.
        assert!(
            ol.content_rows() > 1,
            "expected wrapped rows, got {}",
            ol.content_rows()
        );
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
