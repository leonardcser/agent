//! `Notification` — a non-focusable ephemeral toast `Component`.
//!
//! Sibling to `Picker`: another pre-styled, non-focusable compositor
//! float. Shows a single-row message with a leading level label
//! (info/error). Caller controls lifecycle (open → dismiss on any key,
//! via `win_close`).

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
}

impl Notification {
    pub fn new(message: impl Into<String>, level: NotificationLevel) -> Self {
        Self {
            message: message.into(),
            level,
            style: NotificationStyle::default(),
        }
    }

    pub fn with_style(mut self, style: NotificationStyle) -> Self {
        self.style = style;
        self
    }

    pub fn set_message(&mut self, message: impl Into<String>) {
        self.message = message.into();
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
}

impl Component for Notification {
    fn draw(&self, _area: Rect, slice: &mut GridSlice<'_>, _ctx: &DrawContext) {
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

        let budget = w.saturating_sub(col) as usize;
        for ch in self.message.chars().take(budget) {
            if col >= w {
                break;
            }
            slice.set(col, 0, ch, self.style.message);
            col = col.saturating_add(1);
        }
    }

    fn handle_key(&mut self, _code: KeyCode, _mods: KeyModifiers) -> KeyResult {
        KeyResult::Ignored
    }

    fn handle_mouse(&mut self, event: MouseEvent) -> KeyResult {
        // Click-to-dismiss: any primary click on the toast resolves
        // its lifecycle (caller closes the float on `Dismiss`).
        if let MouseEventKind::Down(MouseButton::Left) = event.kind {
            return KeyResult::Action(WidgetEvent::Dismiss);
        }
        KeyResult::Ignored
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
