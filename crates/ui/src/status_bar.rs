use crate::component::{Component, DrawContext, KeyResult};
use crate::grid::{GridSlice, Style};
use crate::layout::Rect;
use crossterm::event::{KeyCode, KeyModifiers};

pub struct StatusSegment {
    pub text: String,
    pub style: Style,
}

impl StatusSegment {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: Style::default(),
        }
    }

    pub fn styled(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }
}

pub struct StatusBar {
    left: Vec<StatusSegment>,
    right: Vec<StatusSegment>,
    bg: Style,
}

impl StatusBar {
    pub fn new() -> Self {
        Self {
            left: Vec::new(),
            right: Vec::new(),
            bg: Style::default(),
        }
    }

    pub fn with_bg(mut self, style: Style) -> Self {
        self.bg = style;
        self
    }

    pub fn set_left(&mut self, segments: Vec<StatusSegment>) {
        self.left = segments;
    }

    pub fn set_right(&mut self, segments: Vec<StatusSegment>) {
        self.right = segments;
    }
}

impl Default for StatusBar {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for StatusBar {
    fn draw(&self, _area: Rect, grid: &mut GridSlice<'_>, _ctx: &DrawContext) {
        use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

        let w = grid.width();
        if w == 0 || grid.height() == 0 {
            return;
        }

        grid.fill(Rect::new(0, 0, w, 1), ' ', self.bg);

        let seg_style = |seg: &StatusSegment| -> Style {
            Style {
                fg: seg.style.fg.or(self.bg.fg),
                bg: seg.style.bg.or(self.bg.bg),
                bold: seg.style.bold || self.bg.bold,
                dim: seg.style.dim || self.bg.dim,
                italic: seg.style.italic,
                ..Style::default()
            }
        };

        let mut col = 0u16;
        for seg in &self.left {
            let style = seg_style(seg);
            for ch in seg.text.chars() {
                let cw = UnicodeWidthChar::width(ch).unwrap_or(1).max(1) as u16;
                if col + cw > w {
                    break;
                }
                grid.set(col, 0, ch, style);
                col += cw;
            }
        }

        let right_width: u16 = self
            .right
            .iter()
            .map(|s| UnicodeWidthStr::width(s.text.as_str()) as u16)
            .sum();
        let mut col = w.saturating_sub(right_width);
        for seg in &self.right {
            let style = seg_style(seg);
            for ch in seg.text.chars() {
                let cw = UnicodeWidthChar::width(ch).unwrap_or(1).max(1) as u16;
                if col + cw > w {
                    break;
                }
                grid.set(col, 0, ch, style);
                col += cw;
            }
        }
    }

    fn handle_key(&mut self, _code: KeyCode, _mods: KeyModifiers) -> KeyResult {
        KeyResult::Ignored
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

    #[test]
    fn renders_left_and_right() {
        let mut bar = StatusBar::new();
        bar.set_left(vec![StatusSegment::plain("LEFT")]);
        bar.set_right(vec![StatusSegment::plain("RIGHT")]);

        let mut grid = Grid::new(20, 1);
        let ctx = DrawContext {
            terminal_width: 20,
            terminal_height: 1,
            focused: false,
        };
        let mut slice = grid.slice_mut(Rect::new(0, 0, 20, 1));
        bar.draw(Rect::new(0, 0, 20, 1), &mut slice, &ctx);

        assert_eq!(grid.cell(0, 0).symbol, 'L');
        assert_eq!(grid.cell(3, 0).symbol, 'T');
        assert_eq!(grid.cell(15, 0).symbol, 'R');
        assert_eq!(grid.cell(19, 0).symbol, 'T');
    }

    #[test]
    fn styled_segments() {
        let mut bar = StatusBar::new();
        bar.set_left(vec![StatusSegment::styled("MODE", Style::fg(Color::Green))]);

        let mut grid = Grid::new(10, 1);
        let ctx = DrawContext {
            terminal_width: 10,
            terminal_height: 1,
            focused: false,
        };
        let mut slice = grid.slice_mut(Rect::new(0, 0, 10, 1));
        bar.draw(Rect::new(0, 0, 10, 1), &mut slice, &ctx);

        assert_eq!(grid.cell(0, 0).style.fg, Some(Color::Green));
    }
}
