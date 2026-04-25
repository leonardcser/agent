//! `Notification` — a non-focusable ephemeral toast `Component`.
//!
//! Sibling to `Picker`: another pre-styled, non-focusable compositor
//! float. Shows a single-row message with a leading level label
//! (info/error). Caller controls lifecycle (open → close via
//! `win_close`); the toast stays put until replaced.
//!
//! Mouse drag-selects the message body the same way `TextInput` does:
//! `Down` anchors a char index, `Drag` extends, `Up` emits
//! `Action(Yank(text))` so the host can copy to the clipboard.

use crate::component::{Component, CursorInfo, DrawContext, KeyResult, WidgetEvent};
use crate::grid::{GridSlice, Style};
use crate::layout::Rect;
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotificationLevel {
    Info,
    Error,
}

#[derive(Clone, Debug)]
pub struct NotificationStyle {
    /// Style for the "info" / "error" leading label.
    pub info_label: Style,
    pub error_label: Style,
    /// Style for the message body.
    pub message: Style,
    /// Background fill behind the row.
    pub background: Style,
}

impl Default for NotificationStyle {
    fn default() -> Self {
        Self {
            info_label: Style {
                bold: true,
                ..Style::default()
            },
            error_label: Style {
                bold: true,
                ..Style::default()
            },
            message: Style::dim(),
            background: Style::default(),
        }
    }
}

pub struct Notification {
    message: String,
    level: NotificationLevel,
    style: NotificationStyle,
    /// Char-index anchor (set on mouse Down inside the message body)
    /// and cursor (extended by Drag). `None` outside an active drag.
    anchor: Option<usize>,
    cursor_col: usize,
    /// Last drawn rect — handle_mouse needs the absolute origin to
    /// translate `event.column` into a char index.
    last_area: Rect,
}

impl Notification {
    pub fn new(message: impl Into<String>, level: NotificationLevel) -> Self {
        Self {
            message: message.into(),
            level,
            style: NotificationStyle::default(),
            anchor: None,
            cursor_col: 0,
            last_area: Rect::new(0, 0, 0, 0),
        }
    }

    pub fn with_style(mut self, style: NotificationStyle) -> Self {
        self.style = style;
        self
    }

    pub fn set_message(&mut self, message: impl Into<String>) {
        self.message = message.into();
        self.anchor = None;
        self.cursor_col = 0;
    }

    pub fn set_level(&mut self, level: NotificationLevel) {
        self.level = level;
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn level(&self) -> NotificationLevel {
        self.level
    }

    /// `(start, end)` char indices of the active selection in
    /// `message`, normalized so `start <= end`. `None` when no drag
    /// is active or anchor == cursor.
    fn selection_range(&self) -> Option<(usize, usize)> {
        let anchor = self.anchor?;
        if anchor == self.cursor_col {
            return None;
        }
        Some(if anchor < self.cursor_col {
            (anchor, self.cursor_col)
        } else {
            (self.cursor_col, anchor)
        })
    }

    /// First column where the message body starts (after indent +
    /// level label + gap). Mirrors the layout used by `draw`.
    fn text_start_col(&self) -> u16 {
        let indent: u16 = 1;
        let gap: u16 = 2;
        let label_len = match self.level {
            NotificationLevel::Info => 4,
            NotificationLevel::Error => 5,
        };
        indent + label_len + gap
    }

    /// Translate a column relative to `last_area` into a char index in
    /// `message`, clamped to the message length. Columns left of the
    /// message body clamp to 0; columns past the end clamp to the
    /// last index.
    fn char_index_at_local_col(&self, local_col: u16) -> usize {
        let text_start = self.text_start_col();
        if local_col < text_start {
            return 0;
        }
        let offset = (local_col - text_start) as usize;
        offset.min(self.message.chars().count())
    }
}

impl Component for Notification {
    fn prepare(&mut self, area: Rect, _ctx: &DrawContext) {
        self.last_area = area;
    }

    fn draw(&self, _area: Rect, slice: &mut GridSlice<'_>, ctx: &DrawContext) {
        let w = slice.width();
        let h = slice.height();
        if w == 0 || h == 0 {
            return;
        }
        slice.fill(Rect::new(0, 0, w, h), ' ', self.style.background);

        let (label, label_style) = match self.level {
            NotificationLevel::Info => ("info", self.style.info_label),
            NotificationLevel::Error => ("error", self.style.error_label),
        };

        let indent: u16 = 1;
        let gap: u16 = 2;

        let mut col: u16 = indent;
        for ch in label.chars() {
            if col >= w {
                break;
            }
            slice.set(col, 0, ch, label_style);
            col = col.saturating_add(1);
        }
        col = col.saturating_add(gap);

        let selection = self.selection_range();
        let budget = w.saturating_sub(col) as usize;
        for (i, ch) in self.message.chars().enumerate().take(budget) {
            if col >= w {
                break;
            }
            let is_selected = selection.is_some_and(|(s, e)| i >= s && i < e);
            let style = if is_selected {
                Style {
                    bg: ctx.selection_style.bg,
                    ..self.style.message
                }
            } else {
                self.style.message
            };
            slice.set(col, 0, ch, style);
            col = col.saturating_add(1);
        }
    }

    fn handle_key(&mut self, _code: KeyCode, _mods: KeyModifiers) -> KeyResult {
        KeyResult::Ignored
    }

    fn handle_mouse(&mut self, event: MouseEvent) -> KeyResult {
        let rect = self.last_area;
        if rect.width == 0 || !rect.contains(event.row, event.column) {
            return KeyResult::Ignored;
        }
        let local_col = event.column - rect.left;
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let idx = self.char_index_at_local_col(local_col);
                self.anchor = Some(idx);
                self.cursor_col = idx;
                // Capture so subsequent Drag/Up route here even when
                // the pointer leaves the toast row.
                KeyResult::Capture
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if self.anchor.is_some() {
                    self.cursor_col = self.char_index_at_local_col(local_col);
                    KeyResult::Consumed
                } else {
                    KeyResult::Ignored
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let yanked = self
                    .selection_range()
                    .map(|(s, e)| self.message.chars().skip(s).take(e - s).collect::<String>())
                    .filter(|s| !s.is_empty());
                // Selection survives the release so the user can see
                // what they grabbed; cleared on the next Down.
                match yanked {
                    Some(text) => KeyResult::Action(WidgetEvent::Yank(text)),
                    None => KeyResult::Consumed,
                }
            }
            _ => KeyResult::Ignored,
        }
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
    use crossterm::style::Color;

    fn ctx(w: u16, h: u16) -> DrawContext {
        DrawContext {
            terminal_width: w,
            terminal_height: h,
            focused: false,
            selection_style: Default::default(),
        }
    }

    #[test]
    fn renders_info_label_then_message() {
        let n = Notification::new("saved", NotificationLevel::Info);
        let mut grid = Grid::new(30, 1);
        let area = Rect::new(0, 0, 30, 1);
        let mut slice = grid.slice_mut(area);
        n.draw(area, &mut slice, &ctx(30, 1));

        assert_eq!(grid.cell(1, 0).symbol, 'i');
        assert_eq!(grid.cell(2, 0).symbol, 'n');
        assert_eq!(grid.cell(3, 0).symbol, 'f');
        assert_eq!(grid.cell(4, 0).symbol, 'o');
        assert_eq!(grid.cell(7, 0).symbol, 's');
        assert_eq!(grid.cell(8, 0).symbol, 'a');
    }

    #[test]
    fn error_level_uses_error_label_style() {
        let n = Notification::new("boom", NotificationLevel::Error).with_style(NotificationStyle {
            info_label: Style::bold(),
            error_label: Style::fg(Color::Red),
            message: Style::dim(),
            background: Style::default(),
        });
        let mut grid = Grid::new(30, 1);
        let area = Rect::new(0, 0, 30, 1);
        let mut slice = grid.slice_mut(area);
        n.draw(area, &mut slice, &ctx(30, 1));

        assert_eq!(grid.cell(1, 0).symbol, 'e');
        assert_eq!(grid.cell(1, 0).style.fg, Some(Color::Red));
    }

    #[test]
    fn handle_key_always_ignored() {
        let mut n = Notification::new("x", NotificationLevel::Info);
        assert_eq!(
            n.handle_key(KeyCode::Esc, KeyModifiers::NONE),
            KeyResult::Ignored
        );
    }

    #[test]
    fn message_truncates_to_width() {
        let n = Notification::new("aaaaaaaaaaaaaaaaaaaaaaa", NotificationLevel::Info);
        let mut grid = Grid::new(12, 1);
        let area = Rect::new(0, 0, 12, 1);
        let mut slice = grid.slice_mut(area);
        n.draw(area, &mut slice, &ctx(12, 1));
        // indent(1) + "info"(4) + gap(2) = col 7. Width 12 → budget 5.
        for col in 7..12 {
            assert_eq!(grid.cell(col, 0).symbol, 'a');
        }
    }
}
