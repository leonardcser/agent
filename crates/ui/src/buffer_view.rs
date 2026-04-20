use crate::buffer::Buffer;
use crate::component::{Component, DrawContext, KeyResult};
use crate::grid::{GridSlice, Style};
use crate::layout::{Border, Rect};
use crossterm::event::{KeyCode, KeyModifiers};

pub struct BufferView {
    lines: Vec<String>,
    highlights: Vec<Vec<(u16, u16, Style)>>,
    scroll_offset: usize,
    border: Border,
    title: Option<String>,
    title_style: Style,
}

impl BufferView {
    pub fn new() -> Self {
        Self {
            lines: Vec::new(),
            highlights: Vec::new(),
            scroll_offset: 0,
            border: Border::None,
            title: None,
            title_style: Style::default(),
        }
    }

    pub fn with_border(mut self, border: Border) -> Self {
        self.border = border;
        self
    }

    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    pub fn with_title_style(mut self, style: Style) -> Self {
        self.title_style = style;
        self
    }

    pub fn set_lines(&mut self, lines: Vec<String>) {
        self.lines = lines;
        self.highlights.clear();
    }

    pub fn set_scroll(&mut self, offset: usize) {
        if self.scroll_offset != offset {
            self.scroll_offset = offset;
        }
    }

    pub fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }

    pub fn set_title(&mut self, title: Option<String>) {
        self.title = title;
    }

    pub fn set_border(&mut self, border: Border) {
        self.border = border;
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub fn sync_from_buffer(&mut self, buf: &Buffer) {
        self.lines = buf.lines().to_vec();
        self.highlights.clear();
        for i in 0..buf.line_count() {
            let spans = buf.highlights_at(i);
            if spans.is_empty() {
                self.highlights.push(Vec::new());
            } else {
                let converted: Vec<(u16, u16, Style)> = spans
                    .iter()
                    .map(|s| {
                        let style = Style {
                            fg: s.style.fg,
                            bg: s.style.bg,
                            bold: s.style.bold,
                            dim: s.style.dim,
                            italic: s.style.italic,
                            ..Style::default()
                        };
                        (s.col_start, s.col_end, style)
                    })
                    .collect();
                self.highlights.push(converted);
            }
        }
    }

    pub fn add_highlight(&mut self, line: usize, col_start: u16, col_end: u16, style: Style) {
        while self.highlights.len() <= line {
            self.highlights.push(Vec::new());
        }
        self.highlights[line].push((col_start, col_end, style));
    }

    pub fn content_height(&self, area_height: u16) -> u16 {
        let chrome = if self.border != Border::None { 2 } else { 0 };
        area_height.saturating_sub(chrome)
    }

    fn draw_border(&self, grid: &mut GridSlice<'_>) {
        let (h, v, tl, tr, bl, br) = match self.border {
            Border::None => return,
            Border::Single => ('─', '│', '┌', '┐', '└', '┘'),
            Border::Double => ('═', '║', '╔', '╗', '╚', '╝'),
            Border::Rounded => ('─', '│', '╭', '╮', '╰', '╯'),
        };
        let w = grid.width();
        let h_total = grid.height();
        if w < 2 || h_total < 2 {
            return;
        }

        let style = self.title_style;

        grid.set(0, 0, tl, style);
        if let Some(ref title) = self.title {
            grid.set(1, 0, h, style);
            let max_title = (w as usize).saturating_sub(4);
            for (i, ch) in title.chars().take(max_title).enumerate() {
                grid.set(2 + i as u16, 0, ch, style);
            }
            let title_len = title.chars().take(max_title).count();
            grid.set(2 + title_len as u16, 0, h, style);
            for col in (3 + title_len as u16)..w.saturating_sub(1) {
                grid.set(col, 0, h, style);
            }
        } else {
            for col in 1..w.saturating_sub(1) {
                grid.set(col, 0, h, style);
            }
        }
        grid.set(w - 1, 0, tr, style);

        for row in 1..h_total.saturating_sub(1) {
            grid.set(0, row, v, style);
            grid.set(w - 1, row, v, style);
        }

        grid.set(0, h_total - 1, bl, style);
        for col in 1..w.saturating_sub(1) {
            grid.set(col, h_total - 1, h, style);
        }
        grid.set(w - 1, h_total - 1, br, style);
    }

    fn draw_content(&self, grid: &mut GridSlice<'_>) {
        let has_border = self.border != Border::None;
        let (offset_x, offset_y, content_w, content_h) = if has_border {
            (
                1u16,
                1u16,
                grid.width().saturating_sub(2),
                grid.height().saturating_sub(2),
            )
        } else {
            (0, 0, grid.width(), grid.height())
        };

        for row in 0..content_h {
            let line_idx = self.scroll_offset + row as usize;
            if line_idx >= self.lines.len() {
                break;
            }
            let line = &self.lines[line_idx];
            let spans = self
                .highlights
                .get(line_idx)
                .map(|v| v.as_slice())
                .unwrap_or(&[]);

            if spans.is_empty() {
                for (col, ch) in line.chars().enumerate() {
                    let col = col as u16;
                    if col >= content_w {
                        break;
                    }
                    grid.set(offset_x + col, offset_y + row, ch, Style::default());
                }
            } else {
                let chars: Vec<char> = line.chars().collect();
                let mut col = 0u16;
                while col < content_w && (col as usize) < chars.len() {
                    let active = spans
                        .iter()
                        .find(|(start, end, _)| col >= *start && col < *end);
                    if let Some((_, end, style)) = active {
                        let end = (*end).min(content_w).min(chars.len() as u16);
                        for c in col..end {
                            grid.set(offset_x + c, offset_y + row, chars[c as usize], *style);
                        }
                        col = end;
                    } else {
                        grid.set(
                            offset_x + col,
                            offset_y + row,
                            chars[col as usize],
                            Style::default(),
                        );
                        col += 1;
                    }
                }
            }
        }
    }
}

impl Default for BufferView {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for BufferView {
    fn draw(&self, _area: Rect, grid: &mut GridSlice<'_>, _ctx: &DrawContext) {
        self.draw_border(grid);
        self.draw_content(grid);
    }

    fn handle_key(&mut self, _code: KeyCode, _mods: KeyModifiers) -> KeyResult {
        KeyResult::Ignored
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::BufCreateOpts;
    use crate::grid::Grid;
    use crate::BufId;
    use crossterm::style::Color;

    fn make_view(lines: Vec<&str>) -> BufferView {
        let mut view = BufferView::new();
        view.set_lines(lines.into_iter().map(String::from).collect());
        view
    }

    #[test]
    fn renders_plain_text() {
        let view = make_view(vec!["hello", "world"]);
        let mut grid = Grid::new(10, 3);
        let ctx = DrawContext {
            terminal_width: 10,
            terminal_height: 3,
            focused: false,
        };
        let mut slice = grid.slice_mut(Rect::new(0, 0, 10, 3));
        view.draw(Rect::new(0, 0, 10, 3), &mut slice, &ctx);
        assert_eq!(grid.cell(0, 0).symbol, 'h');
        assert_eq!(grid.cell(4, 0).symbol, 'o');
        assert_eq!(grid.cell(0, 1).symbol, 'w');
    }

    #[test]
    fn renders_with_scroll() {
        let mut view = make_view(vec!["line0", "line1", "line2", "line3"]);
        view.set_scroll(2);
        let mut grid = Grid::new(10, 2);
        let ctx = DrawContext {
            terminal_width: 10,
            terminal_height: 2,
            focused: false,
        };
        let mut slice = grid.slice_mut(Rect::new(0, 0, 10, 2));
        view.draw(Rect::new(0, 0, 10, 2), &mut slice, &ctx);
        assert_eq!(grid.cell(0, 0).symbol, 'l');
        assert_eq!(grid.cell(4, 0).symbol, '2');
        assert_eq!(grid.cell(4, 1).symbol, '3');
    }

    #[test]
    fn renders_with_border() {
        let mut view = make_view(vec!["hello"]);
        view.set_border(Border::Single);
        let mut grid = Grid::new(12, 3);
        let ctx = DrawContext {
            terminal_width: 12,
            terminal_height: 3,
            focused: false,
        };
        let mut slice = grid.slice_mut(Rect::new(0, 0, 12, 3));
        view.draw(Rect::new(0, 0, 12, 3), &mut slice, &ctx);
        assert_eq!(grid.cell(0, 0).symbol, '┌');
        assert_eq!(grid.cell(11, 0).symbol, '┐');
        assert_eq!(grid.cell(0, 2).symbol, '└');
        assert_eq!(grid.cell(1, 1).symbol, 'h');
        assert_eq!(grid.cell(5, 1).symbol, 'o');
    }

    #[test]
    fn renders_with_title() {
        let view = BufferView::new()
            .with_border(Border::Rounded)
            .with_title("test");
        let mut grid = Grid::new(20, 3);
        let ctx = DrawContext {
            terminal_width: 20,
            terminal_height: 3,
            focused: false,
        };
        let mut slice = grid.slice_mut(Rect::new(0, 0, 20, 3));
        view.draw(Rect::new(0, 0, 20, 3), &mut slice, &ctx);
        assert_eq!(grid.cell(0, 0).symbol, '╭');
        assert_eq!(grid.cell(2, 0).symbol, 't');
        assert_eq!(grid.cell(5, 0).symbol, 't');
    }

    #[test]
    fn renders_highlighted_text() {
        let mut view = make_view(vec!["hello world"]);
        view.add_highlight(0, 0, 5, Style::fg(Color::Red));
        let mut grid = Grid::new(20, 1);
        let ctx = DrawContext {
            terminal_width: 20,
            terminal_height: 1,
            focused: false,
        };
        let mut slice = grid.slice_mut(Rect::new(0, 0, 20, 1));
        view.draw(Rect::new(0, 0, 20, 1), &mut slice, &ctx);
        assert_eq!(grid.cell(0, 0).style.fg, Some(Color::Red));
        assert_eq!(grid.cell(4, 0).style.fg, Some(Color::Red));
        assert_eq!(grid.cell(5, 0).style.fg, None);
    }

    #[test]
    fn sync_from_buffer() {
        let mut buf = Buffer::new(BufId(1), BufCreateOpts::default());
        buf.set_all_lines(vec!["test line".into()]);
        buf.add_highlight(0, 0, 4, crate::buffer::SpanStyle::fg(Color::Green));

        let mut view = BufferView::new();
        view.sync_from_buffer(&buf);

        let mut grid = Grid::new(20, 1);
        let ctx = DrawContext {
            terminal_width: 20,
            terminal_height: 1,
            focused: false,
        };
        let mut slice = grid.slice_mut(Rect::new(0, 0, 20, 1));
        view.draw(Rect::new(0, 0, 20, 1), &mut slice, &ctx);
        assert_eq!(grid.cell(0, 0).symbol, 't');
        assert_eq!(grid.cell(0, 0).style.fg, Some(Color::Green));
        assert_eq!(grid.cell(4, 0).style.fg, None);
    }
}
