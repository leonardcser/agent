use super::display::{DisplayLine, SpanStyle};
use super::paint::resolve;
use crate::theme::Theme;
use ui::grid::{GridSlice, Style};
use unicode_width::UnicodeWidthChar;

#[allow(dead_code)]
pub(crate) struct GridPaintContext<'a> {
    pub theme: &'a Theme,
    pub term_width: u16,
}

#[allow(dead_code)]
pub(crate) fn paint_line_to_grid(
    grid: &mut GridSlice<'_>,
    row: u16,
    line: &DisplayLine,
    ctx: &GridPaintContext<'_>,
    pad_left: u16,
) {
    if row >= grid.height() {
        return;
    }

    let mut col: u16 = 0;

    if pad_left > 0 {
        let gutter_style = match line.gutter_bg {
            Some(bg) => Style::bg(resolve(bg, ctx.theme, true)),
            None => Style::default(),
        };
        for _ in 0..pad_left {
            if col < grid.width() {
                grid.set(col, row, ' ', gutter_style);
                col += 1;
            }
        }
    }

    for span in &line.spans {
        let style = resolve_span_style(&span.style, ctx.theme);
        for ch in span.text.chars() {
            let w = ch.width().unwrap_or(0) as u16;
            if w == 0 {
                continue;
            }
            if col + w > grid.width() {
                break;
            }
            grid.set(col, row, ch, style);
            if w == 2 {
                grid.set(col + 1, row, ' ', style);
            }
            col += w;
        }
    }

    if let Some(fill) = line.fill_bg {
        let visible_cols = col;
        let pad = ctx
            .term_width
            .saturating_sub(visible_cols)
            .saturating_sub(line.fill_right_margin);
        if pad > 0 {
            let fill_style = Style::bg(resolve(fill, ctx.theme, true));
            for _ in 0..pad {
                if col >= grid.width() {
                    break;
                }
                grid.set(col, row, ' ', fill_style);
                col += 1;
            }
        }
    }
}

#[allow(dead_code)]
fn resolve_span_style(span: &SpanStyle, theme: &Theme) -> Style {
    Style {
        fg: span.fg.map(|c| resolve(c, theme, false)),
        bg: span.bg.map(|c| resolve(c, theme, true)),
        bold: span.bold,
        dim: span.dim,
        italic: span.italic,
        underline: span.underline,
        crossedout: span.crossedout,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::display::{DisplayLine, DisplaySpan, SpanStyle};
    use ui::grid::Grid;
    use ui::layout::Rect;

    fn test_theme() -> Theme {
        crate::theme::snapshot()
    }

    #[test]
    fn paint_simple_line() {
        let line = DisplayLine {
            spans: vec![DisplaySpan {
                text: "hello".into(),
                style: SpanStyle::default(),
                meta: Default::default(),
            }],
            ..Default::default()
        };
        let theme = test_theme();
        let ctx = GridPaintContext {
            theme: &theme,
            term_width: 20,
        };
        let mut grid = Grid::new(20, 1);
        let mut slice = grid.slice_mut(Rect::new(0, 0, 20, 1));
        paint_line_to_grid(&mut slice, 0, &line, &ctx, 0);
        assert_eq!(grid.cell(0, 0).symbol, 'h');
        assert_eq!(grid.cell(4, 0).symbol, 'o');
        assert_eq!(grid.cell(5, 0).symbol, ' ');
    }

    #[test]
    fn paint_with_pad_left() {
        let line = DisplayLine {
            spans: vec![DisplaySpan {
                text: "hi".into(),
                style: SpanStyle::default(),
                meta: Default::default(),
            }],
            ..Default::default()
        };
        let theme = test_theme();
        let ctx = GridPaintContext {
            theme: &theme,
            term_width: 20,
        };
        let mut grid = Grid::new(20, 1);
        let mut slice = grid.slice_mut(Rect::new(0, 0, 20, 1));
        paint_line_to_grid(&mut slice, 0, &line, &ctx, 3);
        assert_eq!(grid.cell(0, 0).symbol, ' ');
        assert_eq!(grid.cell(2, 0).symbol, ' ');
        assert_eq!(grid.cell(3, 0).symbol, 'h');
        assert_eq!(grid.cell(4, 0).symbol, 'i');
    }

    #[test]
    fn paint_styled_span() {
        use crate::render::display::ColorValue;
        let line = DisplayLine {
            spans: vec![DisplaySpan {
                text: "bold".into(),
                style: SpanStyle {
                    bold: true,
                    fg: Some(ColorValue::Rgb(255, 0, 0)),
                    ..Default::default()
                },
                meta: Default::default(),
            }],
            ..Default::default()
        };
        let theme = test_theme();
        let ctx = GridPaintContext {
            theme: &theme,
            term_width: 20,
        };
        let mut grid = Grid::new(20, 1);
        let mut slice = grid.slice_mut(Rect::new(0, 0, 20, 1));
        paint_line_to_grid(&mut slice, 0, &line, &ctx, 0);
        assert!(grid.cell(0, 0).style.bold);
        assert_eq!(
            grid.cell(0, 0).style.fg,
            Some(crossterm::style::Color::Rgb { r: 255, g: 0, b: 0 })
        );
    }

    #[test]
    fn paint_fill_bg() {
        use crate::render::display::ColorValue;
        let line = DisplayLine {
            spans: vec![DisplaySpan {
                text: "ab".into(),
                style: SpanStyle::default(),
                meta: Default::default(),
            }],
            fill_bg: Some(ColorValue::Rgb(0, 0, 128)),
            fill_right_margin: 0,
            ..Default::default()
        };
        let theme = test_theme();
        let ctx = GridPaintContext {
            theme: &theme,
            term_width: 10,
        };
        let mut grid = Grid::new(10, 1);
        let mut slice = grid.slice_mut(Rect::new(0, 0, 10, 1));
        paint_line_to_grid(&mut slice, 0, &line, &ctx, 0);
        assert_eq!(grid.cell(0, 0).symbol, 'a');
        assert_eq!(grid.cell(1, 0).symbol, 'b');
        assert_eq!(
            grid.cell(2, 0).style.bg,
            Some(crossterm::style::Color::Rgb { r: 0, g: 0, b: 128 })
        );
        assert_eq!(
            grid.cell(9, 0).style.bg,
            Some(crossterm::style::Color::Rgb { r: 0, g: 0, b: 128 })
        );
    }

    #[test]
    fn paint_clips_at_grid_width() {
        let line = DisplayLine {
            spans: vec![DisplaySpan {
                text: "hello world this is long".into(),
                style: SpanStyle::default(),
                meta: Default::default(),
            }],
            ..Default::default()
        };
        let theme = test_theme();
        let ctx = GridPaintContext {
            theme: &theme,
            term_width: 5,
        };
        let mut grid = Grid::new(5, 1);
        let mut slice = grid.slice_mut(Rect::new(0, 0, 5, 1));
        paint_line_to_grid(&mut slice, 0, &line, &ctx, 0);
        assert_eq!(grid.cell(4, 0).symbol, 'o');
    }

    #[test]
    fn paint_row_out_of_bounds_is_noop() {
        let line = DisplayLine {
            spans: vec![DisplaySpan {
                text: "hi".into(),
                style: SpanStyle::default(),
                meta: Default::default(),
            }],
            ..Default::default()
        };
        let theme = test_theme();
        let ctx = GridPaintContext {
            theme: &theme,
            term_width: 20,
        };
        let mut grid = Grid::new(20, 1);
        let mut slice = grid.slice_mut(Rect::new(0, 0, 20, 1));
        paint_line_to_grid(&mut slice, 5, &line, &ctx, 0);
        assert_eq!(grid.cell(0, 0).symbol, ' ');
    }
}
