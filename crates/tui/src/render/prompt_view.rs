use crossterm::event::{KeyCode, KeyModifiers};
use ui::component::{Component, DrawContext, KeyResult};
use ui::grid::{GridSlice, Style};
use ui::layout::Rect;

pub(crate) struct StyledSegment {
    pub text: String,
    pub style: Style,
}

pub(crate) struct PromptRow {
    pub segments: Vec<StyledSegment>,
    pub fill: Option<Style>,
    /// Optional scrollbar indicator at a specific column.
    pub scrollbar: Option<(u16, Style)>,
}

impl PromptRow {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            segments: vec![StyledSegment {
                text: text.into(),
                style: Style::default(),
            }],
            fill: None,
            scrollbar: None,
        }
    }

    pub fn styled(segments: Vec<StyledSegment>) -> Self {
        Self {
            segments,
            fill: None,
            scrollbar: None,
        }
    }

    pub fn styled_with_scrollbar(segments: Vec<StyledSegment>, col: u16, style: Style) -> Self {
        Self {
            segments,
            fill: None,
            scrollbar: Some((col, style)),
        }
    }
}

pub(crate) struct PromptView {
    rows: Vec<PromptRow>,
    cursor: Option<(u16, u16)>,
    cursor_style: Option<(Style, char)>,
}

impl PromptView {
    pub fn new() -> Self {
        Self {
            rows: Vec::new(),
            cursor: None,
            cursor_style: None,
        }
    }

    pub fn set_rows(&mut self, rows: Vec<PromptRow>) {
        self.rows = rows;
    }

    pub fn set_cursor(&mut self, pos: Option<(u16, u16)>, style: Option<(Style, char)>) {
        self.cursor = pos;
        self.cursor_style = style;
    }
}

impl Component for PromptView {
    fn draw(&self, _area: Rect, grid: &mut GridSlice<'_>, _ctx: &DrawContext) {
        let h = grid.height();
        let w = grid.width();
        if h == 0 || w == 0 {
            return;
        }

        for (row_idx, row) in self.rows.iter().enumerate() {
            if row_idx as u16 >= h {
                break;
            }
            let y = row_idx as u16;

            if let Some(fill) = row.fill {
                grid.fill(Rect::new(0, y, w, 1), ' ', fill);
            }

            let mut col: u16 = 0;
            for seg in &row.segments {
                for ch in seg.text.chars() {
                    if col >= w {
                        break;
                    }
                    let style = if let Some(fill) = row.fill {
                        Style {
                            fg: seg.style.fg.or(fill.fg),
                            bg: seg.style.bg.or(fill.bg),
                            bold: seg.style.bold,
                            dim: seg.style.dim,
                            italic: seg.style.italic,
                            underline: seg.style.underline,
                            crossedout: seg.style.crossedout,
                        }
                    } else {
                        seg.style
                    };
                    grid.set(col, y, ch, style);
                    col += 1;
                }
            }

            if let Some((sb_col, sb_style)) = row.scrollbar {
                if sb_col < w {
                    grid.set(sb_col, y, ' ', sb_style);
                }
            }
        }

        if let Some((cx, cy)) = self.cursor {
            if let Some((style, glyph)) = self.cursor_style {
                if cx < w && cy < h {
                    grid.set(cx, cy, glyph, style);
                }
            }
        }
    }

    fn handle_key(&mut self, _code: KeyCode, _mods: KeyModifiers) -> KeyResult {
        KeyResult::Ignored
    }

    fn cursor(&self) -> Option<(u16, u16)> {
        self.cursor
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::style::Color;
    use ui::grid::Grid;

    #[test]
    fn renders_plain_rows() {
        let mut view = PromptView::new();
        view.set_rows(vec![PromptRow::plain("hello"), PromptRow::plain("world")]);

        let mut grid = Grid::new(20, 5);
        let ctx = DrawContext {
            terminal_width: 20,
            terminal_height: 5,
            focused: true,
        };
        let mut slice = grid.slice_mut(Rect::new(0, 0, 20, 5));
        view.draw(Rect::new(0, 0, 20, 5), &mut slice, &ctx);

        assert_eq!(grid.cell(0, 0).symbol, 'h');
        assert_eq!(grid.cell(0, 1).symbol, 'w');
    }

    #[test]
    fn cursor_position_reported() {
        let mut view = PromptView::new();
        view.set_cursor(Some((5, 2)), None);
        assert_eq!(view.cursor(), Some((5, 2)));
    }

    #[test]
    fn cursor_style_drawn() {
        let cursor_style = Style {
            fg: Some(Color::Black),
            bg: Some(Color::White),
            ..Style::default()
        };
        let mut view = PromptView::new();
        view.set_rows(vec![PromptRow::plain("abc")]);
        view.set_cursor(Some((1, 0)), Some((cursor_style, 'b')));

        let mut grid = Grid::new(10, 1);
        let ctx = DrawContext {
            terminal_width: 10,
            terminal_height: 1,
            focused: true,
        };
        let mut slice = grid.slice_mut(Rect::new(0, 0, 10, 1));
        view.draw(Rect::new(0, 0, 10, 1), &mut slice, &ctx);

        assert_eq!(grid.cell(1, 0).symbol, 'b');
        assert_eq!(grid.cell(1, 0).style.fg, Some(Color::Black));
        assert_eq!(grid.cell(1, 0).style.bg, Some(Color::White));
    }
}
