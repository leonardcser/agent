use super::display::DisplayLine;
use super::paint_grid::{paint_line_to_grid, GridPaintContext};
use super::scrollbar::Scrollbar;
use crate::theme;
use crate::theme::Theme;
use crossterm::event::{KeyCode, KeyModifiers};
use ui::component::{Component, DrawContext, KeyResult};
use ui::grid::{GridSlice, Style};
use ui::layout::Rect;

pub(crate) struct SoftCursor {
    pub col: u16,
    pub row: u16,
    pub glyph: char,
}

pub(crate) struct TranscriptView {
    lines: Vec<DisplayLine>,
    pad_left: u16,
    scrollbar_col: u16,
    scrollbar: Option<Scrollbar>,
    cursor: Option<SoftCursor>,
    theme: Theme,
    term_width: u16,
}

impl TranscriptView {
    pub fn new(term_width: u16) -> Self {
        Self {
            lines: Vec::new(),
            pad_left: 0,
            scrollbar_col: 0,
            scrollbar: None,
            cursor: None,
            theme: theme::snapshot(),
            term_width,
        }
    }

    pub fn set_lines(&mut self, lines: Vec<DisplayLine>, pad_left: u16) {
        self.lines = lines;
        self.pad_left = pad_left;
    }

    pub fn set_scrollbar(
        &mut self,
        total_rows: usize,
        visible_rows: usize,
        scroll_offset: usize,
        col: u16,
    ) {
        if total_rows > visible_rows && visible_rows > 0 {
            self.scrollbar = Some(Scrollbar::new(total_rows, visible_rows, scroll_offset));
            self.scrollbar_col = col;
        } else {
            self.scrollbar = None;
        }
    }

    pub fn set_cursor(&mut self, cursor: Option<SoftCursor>) {
        self.cursor = cursor;
    }

    pub fn set_term_width(&mut self, w: u16) {
        self.term_width = w;
    }
}

impl Component for TranscriptView {
    fn draw(&self, _area: Rect, grid: &mut GridSlice<'_>, _ctx: &DrawContext) {
        let h = grid.height();
        let w = grid.width();
        if h == 0 || w == 0 {
            return;
        }

        let ctx = GridPaintContext {
            theme: &self.theme,
            term_width: self.term_width,
        };

        for row in 0..h {
            if let Some(line) = self.lines.get(row as usize) {
                paint_line_to_grid(grid, row, line, &ctx, self.pad_left);
            }
        }

        if let Some(ref bar) = self.scrollbar {
            let thumb_bg = Style::bg(theme::scrollbar_thumb());
            let track_bg = Style::bg(theme::scrollbar_track());
            for row in 0..h {
                let style = if bar.is_thumb(row as usize) {
                    thumb_bg
                } else {
                    track_bg
                };
                grid.set(self.scrollbar_col, row, ' ', style);
            }
        }

        if let Some(ref c) = self.cursor {
            if c.row < h && c.col < w {
                let (fg, bg) = if theme::is_light() {
                    (
                        crossterm::style::Color::White,
                        crossterm::style::Color::Black,
                    )
                } else {
                    (
                        crossterm::style::Color::Black,
                        crossterm::style::Color::White,
                    )
                };
                let style = Style {
                    fg: Some(fg),
                    bg: Some(bg),
                    ..Style::default()
                };
                grid.set(c.col, c.row, c.glyph, style);
            }
        }
    }

    fn handle_key(&mut self, _code: KeyCode, _mods: KeyModifiers) -> KeyResult {
        KeyResult::Ignored
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::display::{DisplaySpan, SpanStyle};
    use ui::grid::Grid;

    #[test]
    fn renders_display_lines() {
        let mut view = TranscriptView::new(20);
        view.set_lines(
            vec![
                DisplayLine {
                    spans: vec![DisplaySpan {
                        text: "hello".into(),
                        style: SpanStyle::default(),
                        meta: Default::default(),
                    }],
                    ..Default::default()
                },
                DisplayLine {
                    spans: vec![DisplaySpan {
                        text: "world".into(),
                        style: SpanStyle::default(),
                        meta: Default::default(),
                    }],
                    ..Default::default()
                },
            ],
            0,
        );

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
        assert_eq!(grid.cell(0, 2).symbol, ' ');
    }

    #[test]
    fn renders_soft_cursor() {
        let mut view = TranscriptView::new(20);
        view.set_lines(
            vec![DisplayLine {
                spans: vec![DisplaySpan {
                    text: "abc".into(),
                    style: SpanStyle::default(),
                    meta: Default::default(),
                }],
                ..Default::default()
            }],
            0,
        );
        view.set_cursor(Some(SoftCursor {
            col: 1,
            row: 0,
            glyph: 'b',
        }));

        let mut grid = Grid::new(20, 1);
        let ctx = DrawContext {
            terminal_width: 20,
            terminal_height: 1,
            focused: true,
        };
        let mut slice = grid.slice_mut(Rect::new(0, 0, 20, 1));
        view.draw(Rect::new(0, 0, 20, 1), &mut slice, &ctx);

        assert_eq!(grid.cell(1, 0).symbol, 'b');
        assert!(grid.cell(1, 0).style.fg.is_some());
        assert!(grid.cell(1, 0).style.bg.is_some());
    }

    #[test]
    fn empty_lines_leave_blank() {
        let view = TranscriptView::new(20);
        let mut grid = Grid::new(20, 3);
        let ctx = DrawContext {
            terminal_width: 20,
            terminal_height: 3,
            focused: false,
        };
        let mut slice = grid.slice_mut(Rect::new(0, 0, 20, 3));
        view.draw(Rect::new(0, 0, 20, 3), &mut slice, &ctx);
        assert_eq!(grid.cell(0, 0).symbol, ' ');
    }
}
