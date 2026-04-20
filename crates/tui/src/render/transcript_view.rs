use super::scrollbar::Scrollbar;
use crate::theme;
use crossterm::event::{KeyCode, KeyModifiers};
use ui::buffer::Buffer;
use ui::buffer_view::BufferView;
use ui::component::{Component, DrawContext, KeyResult};
use ui::grid::{GridSlice, Style};
use ui::layout::Rect;

pub(crate) struct SoftCursor {
    pub col: u16,
    pub row: u16,
    pub glyph: char,
}

pub(crate) struct TranscriptView {
    view: BufferView,
    pad_left: u16,
    scrollbar_col: u16,
    scrollbar: Option<Scrollbar>,
    cursor: Option<SoftCursor>,
}

impl TranscriptView {
    pub fn new(_term_width: u16) -> Self {
        Self {
            view: BufferView::new(),
            pad_left: 0,
            scrollbar_col: 0,
            scrollbar: None,
            cursor: None,
        }
    }

    pub fn sync_from_buffer(&mut self, buf: &Buffer, scroll_offset: usize, pad_left: u16) {
        self.view.sync_from_buffer(buf);
        self.view.set_scroll(scroll_offset);
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
}

impl Component for TranscriptView {
    fn draw(&self, area: Rect, grid: &mut GridSlice<'_>, ctx: &DrawContext) {
        let h = grid.height();
        let w = grid.width();
        if h == 0 || w == 0 {
            return;
        }

        self.view.draw(area, grid, ctx);

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
    use crossterm::style::Color;
    use ui::buffer::{BufCreateOpts, SpanStyle};
    use ui::grid::Grid;
    use ui::BufId;

    fn make_buf(lines: &[&str]) -> Buffer {
        let mut buf = Buffer::new(BufId(1), BufCreateOpts::default());
        buf.set_all_lines(lines.iter().map(|s| String::from(*s)).collect());
        buf
    }

    #[test]
    fn renders_buffer_lines() {
        let buf = make_buf(&["hello", "world"]);
        let mut view = TranscriptView::new(20);
        view.sync_from_buffer(&buf, 0, 0);

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
    fn renders_with_scroll_offset() {
        let buf = make_buf(&["line0", "line1", "line2", "line3"]);
        let mut view = TranscriptView::new(20);
        view.sync_from_buffer(&buf, 2, 0);

        let mut grid = Grid::new(20, 2);
        let ctx = DrawContext {
            terminal_width: 20,
            terminal_height: 2,
            focused: true,
        };
        let mut slice = grid.slice_mut(Rect::new(0, 0, 20, 2));
        view.draw(Rect::new(0, 0, 20, 2), &mut slice, &ctx);

        assert_eq!(grid.cell(4, 0).symbol, '2');
        assert_eq!(grid.cell(4, 1).symbol, '3');
    }

    #[test]
    fn renders_soft_cursor() {
        let buf = make_buf(&["abc"]);
        let mut view = TranscriptView::new(20);
        view.sync_from_buffer(&buf, 0, 0);
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
    fn renders_highlighted_buffer() {
        let mut buf = Buffer::new(BufId(1), BufCreateOpts::default());
        buf.set_all_lines(vec!["colored text".into()]);
        buf.add_highlight(0, 0, 7, SpanStyle::fg(Color::Red));

        let mut view = TranscriptView::new(20);
        view.sync_from_buffer(&buf, 0, 0);

        let mut grid = Grid::new(20, 1);
        let ctx = DrawContext {
            terminal_width: 20,
            terminal_height: 1,
            focused: true,
        };
        let mut slice = grid.slice_mut(Rect::new(0, 0, 20, 1));
        view.draw(Rect::new(0, 0, 20, 1), &mut slice, &ctx);

        assert_eq!(grid.cell(0, 0).style.fg, Some(Color::Red));
        assert_eq!(grid.cell(7, 0).style.fg, None);
    }

    #[test]
    fn empty_buffer_leaves_blank() {
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
