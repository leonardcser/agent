use crate::buffer::{Buffer, LineDecoration, Span, SpanMeta};
use crate::component::{Component, DrawContext, KeyResult};
use crate::grid::{GridSlice, Style};
use crate::layout::{Border, Rect};
use crossterm::event::{KeyCode, KeyModifiers};
use std::sync::Arc;

pub struct HighlightSpan {
    pub col_start: u16,
    pub col_end: u16,
    pub style: Style,
    pub meta: SpanMeta,
}

pub struct BufferView {
    /// Lines/decorations/buf_highlights are `Arc`-shared with the source
    /// `Buffer`: sync is a refcount bump, not a deep copy.
    lines: Arc<Vec<String>>,
    /// Spans imported from the source `Buffer`.
    buf_highlights: Arc<Vec<Vec<Span>>>,
    /// Transient overlays pushed by `add_highlight` each frame
    /// (selection, search); cleared on `sync_from_buffer`.
    transient_highlights: Vec<Vec<HighlightSpan>>,
    decorations: Arc<Vec<LineDecoration>>,
    scroll_offset: usize,
    border: Border,
    border_style: Style,
    /// Fallback style for cells that have no per-span highlight and
    /// no per-line decoration override. Used by containers that pre-
    /// fill a background (e.g. Dialog panels) so the content keeps
    /// the container's bg instead of reverting to terminal defaults.
    default_style: Style,
}

impl BufferView {
    pub fn new() -> Self {
        Self {
            lines: Arc::new(Vec::new()),
            buf_highlights: Arc::new(Vec::new()),
            transient_highlights: Vec::new(),
            decorations: Arc::new(Vec::new()),
            scroll_offset: 0,
            border: Border::None,
            border_style: Style::default(),
            default_style: Style::default(),
        }
    }

    pub fn set_default_style(&mut self, style: Style) {
        self.default_style = style;
    }

    pub fn with_border(mut self, border: Border) -> Self {
        self.border = border;
        self
    }

    pub fn set_lines(&mut self, lines: Vec<String>) {
        self.lines = Arc::new(lines);
        self.buf_highlights = Arc::new(Vec::new());
        self.transient_highlights.clear();
        self.decorations = Arc::new(Vec::new());
    }

    pub fn set_scroll(&mut self, offset: usize) {
        if self.scroll_offset != offset {
            self.scroll_offset = offset;
        }
    }

    pub fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }

    pub fn set_border(&mut self, border: Border) {
        self.border = border;
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub fn sync_from_buffer(&mut self, buf: &Buffer) {
        self.lines = Arc::clone(buf.lines_arc());
        self.buf_highlights = Arc::clone(buf.highlights_arc());
        self.decorations = Arc::clone(buf.decorations_arc());
        self.transient_highlights.clear();
    }

    pub fn add_highlight(&mut self, line: usize, col_start: u16, col_end: u16, style: Style) {
        while self.transient_highlights.len() <= line {
            self.transient_highlights.push(Vec::new());
        }
        self.transient_highlights[line].push(HighlightSpan {
            col_start,
            col_end,
            style,
            meta: SpanMeta::default(),
        });
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

        let style = self.border_style;

        grid.set(0, 0, tl, style);
        for col in 1..w.saturating_sub(1) {
            grid.set(col, 0, h, style);
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

    fn draw_content(&self, grid: &mut GridSlice<'_>, ctx: &DrawContext) {
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
            let decoration = self.decorations.get(line_idx);
            let buf_spans = self
                .buf_highlights
                .get(line_idx)
                .map(|v| v.as_slice())
                .unwrap_or(&[]);
            let transient_spans = self
                .transient_highlights
                .get(line_idx)
                .map(|v| v.as_slice())
                .unwrap_or(&[]);

            let bg_override = decoration.and_then(|d| d.gutter_bg);
            let chars: Vec<char> = line.chars().collect();
            let mut col = 0u16;

            let fallback_style = match bg_override {
                Some(bg) => Style {
                    bg: Some(bg),
                    ..self.default_style
                },
                None => self.default_style,
            };
            if buf_spans.is_empty() && transient_spans.is_empty() {
                for &ch in &chars {
                    if col >= content_w {
                        break;
                    }
                    grid.set(offset_x + col, offset_y + row, ch, fallback_style);
                    col += 1;
                }
            } else {
                // Layered merge: `buf_spans` paint first (from the source
                // buffer), then `transient_spans` overlay (selection /
                // search highlights re-applied each frame). Within each
                // layer, later spans override earlier non-None fg/bg so
                // a selection-bg tint preserves the underlying fg.
                while col < content_w && (col as usize) < chars.len() {
                    let mut style = fallback_style;
                    for span in buf_spans {
                        if col >= span.col_start && col < span.col_end {
                            if span.style.fg.is_some() {
                                style.fg = span.style.fg;
                            }
                            if span.style.bg.is_some() {
                                style.bg = span.style.bg;
                            }
                            style.bold |= span.style.bold;
                            style.dim |= span.style.dim;
                            style.italic |= span.style.italic;
                        }
                    }
                    for span in transient_spans {
                        if col >= span.col_start && col < span.col_end {
                            if span.style.fg.is_some() {
                                style.fg = span.style.fg;
                            }
                            if span.style.bg.is_some() {
                                style.bg = span.style.bg;
                            }
                            style.bold |= span.style.bold;
                            style.dim |= span.style.dim;
                            style.italic |= span.style.italic;
                            style.underline |= span.style.underline;
                            style.crossedout |= span.style.crossedout;
                        }
                    }
                    grid.set(offset_x + col, offset_y + row, chars[col as usize], style);
                    col += 1;
                }
            }

            if let Some(dec) = decoration {
                if let Some(fill_bg) = dec.fill_bg {
                    let fill_style = Style::bg(fill_bg);
                    let fill_end = ctx
                        .terminal_width
                        .saturating_sub(dec.fill_right_margin)
                        .saturating_sub(offset_x);
                    for c in col..fill_end.min(content_w) {
                        grid.set(offset_x + c, offset_y + row, ' ', fill_style);
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
    fn draw(&self, _area: Rect, grid: &mut GridSlice<'_>, ctx: &DrawContext) {
        self.draw_border(grid);
        self.draw_content(grid, ctx);
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
            selection_style: Default::default(),
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
            selection_style: Default::default(),
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
            selection_style: Default::default(),
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
    fn renders_highlighted_text() {
        let mut view = make_view(vec!["hello world"]);
        view.add_highlight(0, 0, 5, Style::fg(Color::Red));
        let mut grid = Grid::new(20, 1);
        let ctx = DrawContext {
            terminal_width: 20,
            terminal_height: 1,
            focused: false,
            selection_style: Default::default(),
        };
        let mut slice = grid.slice_mut(Rect::new(0, 0, 20, 1));
        view.draw(Rect::new(0, 0, 20, 1), &mut slice, &ctx);
        assert_eq!(grid.cell(0, 0).style.fg, Some(Color::Red));
        assert_eq!(grid.cell(4, 0).style.fg, Some(Color::Red));
        assert_eq!(grid.cell(5, 0).style.fg, None);
    }

    #[test]
    fn renders_fill_bg_decoration() {
        let mut buf = Buffer::new(BufId(1), BufCreateOpts::default());
        buf.set_all_lines(vec!["hi".into()]);
        buf.set_decoration(
            0,
            LineDecoration {
                fill_bg: Some(Color::Blue),
                ..LineDecoration::default()
            },
        );
        let mut view = BufferView::new();
        view.sync_from_buffer(&buf);
        let mut grid = Grid::new(10, 1);
        let ctx = DrawContext {
            terminal_width: 10,
            terminal_height: 1,
            focused: false,
            selection_style: Default::default(),
        };
        let mut slice = grid.slice_mut(Rect::new(0, 0, 10, 1));
        view.draw(Rect::new(0, 0, 10, 1), &mut slice, &ctx);
        assert_eq!(grid.cell(0, 0).symbol, 'h');
        assert_eq!(grid.cell(2, 0).symbol, ' ');
        assert_eq!(grid.cell(2, 0).style.bg, Some(Color::Blue));
        assert_eq!(grid.cell(9, 0).style.bg, Some(Color::Blue));
    }

    #[test]
    fn renders_gutter_bg_decoration() {
        let mut buf = Buffer::new(BufId(1), BufCreateOpts::default());
        buf.set_all_lines(vec!["ab".into()]);
        buf.set_decoration(
            0,
            LineDecoration {
                gutter_bg: Some(Color::Yellow),
                ..LineDecoration::default()
            },
        );
        let mut view = BufferView::new();
        view.sync_from_buffer(&buf);
        let mut grid = Grid::new(10, 1);
        let ctx = DrawContext {
            terminal_width: 10,
            terminal_height: 1,
            focused: false,
            selection_style: Default::default(),
        };
        let mut slice = grid.slice_mut(Rect::new(0, 0, 10, 1));
        view.draw(Rect::new(0, 0, 10, 1), &mut slice, &ctx);
        assert_eq!(grid.cell(0, 0).symbol, 'a');
        assert_eq!(grid.cell(0, 0).style.bg, Some(Color::Yellow));
        assert_eq!(grid.cell(1, 0).style.bg, Some(Color::Yellow));
        assert_eq!(grid.cell(2, 0).style.bg, None);
    }

    #[test]
    fn sync_from_buffer_copies_decorations() {
        let mut buf = Buffer::new(BufId(1), BufCreateOpts::default());
        buf.set_all_lines(vec!["line".into()]);
        buf.set_decoration(
            0,
            LineDecoration {
                fill_bg: Some(Color::Red),
                ..LineDecoration::default()
            },
        );

        let mut view = BufferView::new();
        view.sync_from_buffer(&buf);
        assert_eq!(view.decorations.len(), 1);
        assert_eq!(view.decorations[0].fill_bg, Some(Color::Red));
    }

    #[test]
    fn sync_from_buffer_copies_span_meta() {
        let mut buf = Buffer::new(BufId(1), BufCreateOpts::default());
        buf.set_all_lines(vec!["test".into()]);
        buf.add_highlight_with_meta(
            0,
            0,
            4,
            crate::buffer::SpanStyle::fg(Color::Red),
            SpanMeta {
                selectable: true,
                copy_as: Some("copied".into()),
            },
        );

        let mut view = BufferView::new();
        view.sync_from_buffer(&buf);
        assert!(view.buf_highlights[0][0].meta.selectable);
        assert_eq!(
            view.buf_highlights[0][0].meta.copy_as.as_deref(),
            Some("copied")
        );
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
            selection_style: Default::default(),
        };
        let mut slice = grid.slice_mut(Rect::new(0, 0, 20, 1));
        view.draw(Rect::new(0, 0, 20, 1), &mut slice, &ctx);
        assert_eq!(grid.cell(0, 0).symbol, 't');
        assert_eq!(grid.cell(0, 0).style.fg, Some(Color::Green));
        assert_eq!(grid.cell(4, 0).style.fg, None);
    }
}
