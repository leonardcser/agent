//! `Picker` — a non-focusable dropdown `Component` for selectable item
//! lists whose selection is driven externally.
//!
//! Mirrors Neovim's `pum_grid`: a compositor layer that paints a list
//! of items with one selected row, never steals focus, and is reused
//! across every caller that needs "pick one from a list" UX — the
//! prompt's `/` command completer, the cmdline's `:` completer, the
//! Lua `smelt.ui.picker.open` primitive.

use crate::component::{Component, CursorInfo, DrawContext, KeyResult, WidgetEvent};
use crate::grid::{GridSlice, Style};
use crate::layout::Rect;
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

#[derive(Clone, Debug, Default)]
pub struct PickerItem {
    pub label: String,
    /// Secondary descriptive text, dimmed, right-of-label in a padded
    /// column so descriptions line up across rows.
    pub description: Option<String>,
    /// Prefix appended to the label (e.g. `"/"` for commands, `"./"`
    /// for files). Participates in column-alignment width.
    pub prefix: String,
    /// Optional per-item accent. When set, overrides the picker's
    /// default label/description style for this row — drawn on prefix,
    /// label, and description alike.
    pub accent: Option<crossterm::style::Color>,
}

impl PickerItem {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            description: None,
            prefix: String::new(),
            accent: None,
        }
    }

    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }

    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    pub fn with_accent(mut self, color: crossterm::style::Color) -> Self {
        self.accent = Some(color);
        self
    }
}

#[derive(Clone, Debug)]
pub struct PickerStyle {
    /// Color for the selected row's label.
    pub selected_fg: Style,
    /// Color for the unselected rows' labels.
    pub unselected_fg: Style,
    /// Color for description text on every row.
    pub description_fg: Style,
    /// Background fill behind the whole picker rect.
    pub background: Style,
}

impl Default for PickerStyle {
    fn default() -> Self {
        Self {
            selected_fg: Style::default(),
            unselected_fg: Style::dim(),
            description_fg: Style::dim(),
            background: Style::default(),
        }
    }
}

pub struct Picker {
    items: Vec<PickerItem>,
    selected: usize,
    style: PickerStyle,
    max_visible_rows: u16,
    /// Cached scroll_top computed each `prepare`.
    scroll_top: usize,
    /// When true, logical index 0 is painted on the *bottom* visible
    /// row and the list grows upward. Used for pickers that dock above
    /// the prompt (completer `/`, cmdline `:`): the best match is
    /// closest to where the user is typing.
    reversed: bool,
    /// Rect from the last `prepare` — used by `handle_mouse` to map
    /// click rows to item indices (accounting for `reversed`).
    last_area: Rect,
}

impl Picker {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            selected: 0,
            style: PickerStyle::default(),
            max_visible_rows: u16::MAX,
            scroll_top: 0,
            reversed: false,
            last_area: Rect::new(0, 0, 0, 0),
        }
    }

    /// Resolve the item index at visual row `rel_row` (0 = top of the
    /// picker rect). Honors the `reversed` flag — reversed pickers
    /// paint logical 0 at the bottom row, so click row N maps to
    /// scroll_top + (h - 1 - N). Returns `None` when the row is past
    /// the last rendered item.
    fn item_at_visual_row(&self, rel_row: u16, height: u16) -> Option<usize> {
        if self.items.is_empty() || height == 0 {
            return None;
        }
        let row_i = if self.reversed {
            (height - 1).checked_sub(rel_row)?
        } else {
            rel_row
        };
        let idx = self.scroll_top + row_i as usize;
        (idx < self.items.len()).then_some(idx)
    }

    pub fn with_style(mut self, style: PickerStyle) -> Self {
        self.style = style;
        self
    }

    pub fn with_max_rows(mut self, rows: u16) -> Self {
        self.max_visible_rows = rows;
        self
    }

    pub fn with_reversed(mut self, reversed: bool) -> Self {
        self.reversed = reversed;
        self
    }

    pub fn reversed(&self) -> bool {
        self.reversed
    }

    pub fn set_items(&mut self, items: Vec<PickerItem>) {
        self.items = items;
        if self.selected >= self.items.len() {
            self.selected = self.items.len().saturating_sub(1);
        }
    }

    pub fn set_selected(&mut self, index: usize) {
        self.selected = if self.items.is_empty() {
            0
        } else {
            index.min(self.items.len() - 1)
        };
    }

    pub fn set_style(&mut self, style: PickerStyle) {
        self.style = style;
    }

    pub fn items(&self) -> &[PickerItem] {
        &self.items
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn content_rows(&self) -> usize {
        self.items.len().max(1)
    }

    pub fn max_visible_rows(&self) -> u16 {
        self.max_visible_rows
    }

    /// Natural height for placement: number of items, clamped by
    /// `max_visible_rows`, minimum 1. Used by `Placement::DockedAbove`
    /// so a picker float shrinks to fit its visible rows.
    pub fn natural_height(&self) -> u16 {
        let items = self.items.len() as u16;
        let cap = if self.max_visible_rows == u16::MAX {
            items.max(1)
        } else {
            self.max_visible_rows
        };
        items.min(cap).max(1)
    }

    fn max_label_chars(&self) -> usize {
        self.items
            .iter()
            .map(|i| i.prefix.chars().count() + i.label.chars().count())
            .max()
            .unwrap_or(0)
    }

    fn compute_scroll_top(&self, rows: usize) -> usize {
        if rows == 0 || self.items.is_empty() {
            return 0;
        }
        let n = self.items.len();
        if n <= rows {
            return 0;
        }
        let half = rows / 2;
        let mut top = self.selected.saturating_sub(half);
        if top + rows > n {
            top = n - rows;
        }
        top
    }
}

impl Default for Picker {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for Picker {
    fn prepare(&mut self, area: Rect, _ctx: &DrawContext) {
        let rows = area.height as usize;
        self.scroll_top = self.compute_scroll_top(rows);
        self.last_area = area;
    }

    fn draw(&self, _area: Rect, slice: &mut GridSlice<'_>, _ctx: &DrawContext) {
        let w = slice.width();
        let h = slice.height();
        if w == 0 || h == 0 {
            return;
        }
        // Background fill.
        slice.fill(Rect::new(0, 0, w, h), ' ', self.style.background);

        if self.items.is_empty() {
            let label = "(no matches)";
            let row: u16 = if self.reversed { h - 1 } else { 0 };
            let mut col: u16 = 1;
            for ch in label.chars() {
                if col >= w {
                    break;
                }
                slice.set(col, row, ch, self.style.unselected_fg);
                col = col.saturating_add(1);
            }
            return;
        }

        let max_label = self.max_label_chars();
        let indent: u16 = 1;
        let desc_gap: u16 = 2;

        let end = (self.scroll_top + h as usize).min(self.items.len());
        for (row_i, item_i) in (self.scroll_top..end).enumerate() {
            let item = &self.items[item_i];
            // Reversed pickers paint logical index 0 at the bottom row
            // so the "best match" sits closest to the prompt. Every
            // higher index steps one row upward. Non-reversed mode is
            // the regular top-down list.
            let row = if self.reversed {
                (h - 1).saturating_sub(row_i as u16)
            } else {
                row_i as u16
            };
            let is_selected = item_i == self.selected;
            let base_label_style = if is_selected {
                self.style.selected_fg
            } else {
                self.style.unselected_fg
            };
            // The per-item accent always paints the prefix (pill) so
            // users can see the full palette at a glance. The label
            // and description only pick up the accent on the selected
            // row, giving the "you are looking at this one" focus
            // cue without washing out the list.
            let prefix_style = match item.accent {
                Some(c) => Style {
                    fg: Some(c),
                    ..base_label_style
                },
                None => base_label_style,
            };
            let label_style = match (item.accent, is_selected) {
                (Some(c), true) => Style {
                    fg: Some(c),
                    ..base_label_style
                },
                _ => base_label_style,
            };
            let description_style = match (item.accent, is_selected) {
                (Some(c), true) => Style {
                    fg: Some(c),
                    ..self.style.description_fg
                },
                _ => self.style.description_fg,
            };

            let mut col: u16 = indent;

            for ch in item.prefix.chars() {
                if col >= w {
                    break;
                }
                slice.set(col, row, ch, prefix_style);
                col = col.saturating_add(1);
            }
            for ch in item.label.chars() {
                if col >= w {
                    break;
                }
                slice.set(col, row, ch, label_style);
                col = col.saturating_add(1);
            }

            if let Some(desc) = item.description.as_deref() {
                let label_chars = item.prefix.chars().count() + item.label.chars().count();
                let pad = max_label.saturating_sub(label_chars) + desc_gap as usize;
                for _ in 0..pad {
                    if col >= w {
                        break;
                    }
                    slice.set(col, row, ' ', self.style.background);
                    col = col.saturating_add(1);
                }
                for ch in desc.chars() {
                    if col >= w {
                        break;
                    }
                    slice.set(col, row, ch, description_style);
                    col = col.saturating_add(1);
                }
            }
        }
    }

    fn handle_key(&mut self, _code: KeyCode, _mods: KeyModifiers) -> KeyResult {
        // Picker is non-interactive. Selection is driven externally by
        // whichever surface owns input (prompt InputState, cmdline, Lua).
        KeyResult::Ignored
    }

    fn handle_mouse(&mut self, event: MouseEvent) -> KeyResult {
        let MouseEventKind::Down(MouseButton::Left) = event.kind else {
            return KeyResult::Ignored;
        };
        let rect = self.last_area;
        if rect.width == 0 || !rect.contains(event.row, event.column) {
            return KeyResult::Ignored;
        }
        let rel_row = event.row - rect.top;
        let Some(idx) = self.item_at_visual_row(rel_row, rect.height) else {
            return KeyResult::Consumed;
        };
        // Update internal selection so a subsequent re-render before the
        // caller's sync still draws the user's pick highlighted.
        self.selected = idx;
        // Submit the click outward — caller (prompt completer, Lua)
        // commits the selection. `Select(idx)` carries the index so the
        // caller doesn't need a separate read.
        KeyResult::Action(WidgetEvent::Select(idx))
    }

    fn cursor(&self) -> Option<CursorInfo> {
        None
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::Grid;

    fn ctx(w: u16, h: u16) -> DrawContext {
        DrawContext {
            terminal_width: w,
            terminal_height: h,
            focused: false,
        }
    }

    fn items() -> Vec<PickerItem> {
        vec![
            PickerItem::new("clear")
                .with_description("start new conversation")
                .with_prefix("/"),
            PickerItem::new("new")
                .with_description("start new conversation")
                .with_prefix("/"),
            PickerItem::new("resume")
                .with_description("resume saved session")
                .with_prefix("/"),
        ]
    }

    #[test]
    fn paints_selected_row_with_selected_fg() {
        use crossterm::style::Color;
        let mut p = Picker::new().with_style(PickerStyle {
            selected_fg: Style::fg(Color::Red),
            unselected_fg: Style::dim(),
            description_fg: Style::dim(),
            background: Style::default(),
        });
        p.set_items(items());
        p.set_selected(1);

        let mut grid = Grid::new(40, 3);
        let area = Rect::new(0, 0, 40, 3);
        let mut slice = grid.slice_mut(area);
        let c = ctx(40, 3);
        p.prepare(area, &c);
        p.draw(area, &mut slice, &c);

        // Row 1 is selected — label "/" at col 1 should be red.
        let cell = grid.cell(1, 1);
        assert_eq!(cell.symbol, '/');
        assert_eq!(cell.style.fg, Some(Color::Red));

        // Row 0 is not selected — should be dim.
        let cell0 = grid.cell(1, 0);
        assert_eq!(cell0.symbol, '/');
        assert!(cell0.style.dim);
    }

    #[test]
    fn descriptions_align_at_same_column() {
        let mut p = Picker::new();
        p.set_items(items());

        let mut grid = Grid::new(60, 3);
        let area = Rect::new(0, 0, 60, 3);
        let mut slice = grid.slice_mut(area);
        let c = ctx(60, 3);
        p.prepare(area, &c);
        p.draw(area, &mut slice, &c);

        // Longest label is "/resume" (7 chars). With indent=1 + 7 + gap=2,
        // descriptions start at column 10.
        let desc_col = 1 + 7 + 2;
        assert_eq!(grid.cell(desc_col, 0).symbol, 's'); // "start..."
        assert_eq!(grid.cell(desc_col, 1).symbol, 's'); // "start..."
        assert_eq!(grid.cell(desc_col, 2).symbol, 'r'); // "resume..."
    }

    #[test]
    fn empty_shows_no_matches() {
        let p = Picker::new();
        let mut grid = Grid::new(20, 1);
        let area = Rect::new(0, 0, 20, 1);
        let mut slice = grid.slice_mut(area);
        let c = ctx(20, 1);
        p.draw(area, &mut slice, &c);
        assert_eq!(grid.cell(1, 0).symbol, '(');
        assert_eq!(grid.cell(2, 0).symbol, 'n');
    }

    #[test]
    fn handle_key_always_ignored() {
        let mut p = Picker::new();
        p.set_items(items());
        assert_eq!(
            p.handle_key(KeyCode::Down, KeyModifiers::NONE),
            KeyResult::Ignored
        );
        assert_eq!(
            p.handle_key(KeyCode::Enter, KeyModifiers::NONE),
            KeyResult::Ignored
        );
    }

    #[test]
    fn scroll_centers_selection() {
        let mut p = Picker::new();
        let items: Vec<PickerItem> = (0..20)
            .map(|i| PickerItem::new(format!("item-{i}")))
            .collect();
        p.set_items(items);
        p.set_selected(10);

        let area = Rect::new(0, 0, 20, 5);
        let c = ctx(20, 5);
        p.prepare(area, &c);
        // With 5 rows and selection=10, half=2, scroll_top = 10-2 = 8.
        assert_eq!(p.scroll_top, 8);
    }
}
