//! Projection: turn `DisplayBlock` output (produced by `SpanCollector`)
//! into a `ui::Buffer` so content rendered by `print_inline_diff`,
//! `print_syntax_file`, `render_markdown_inner`, etc. flows through the
//! normal buffer → view → grid path and inherits scrollbar, selection,
//! and vim motions.

use super::display::{ColorRole, ColorValue, DisplayLine, SpanStyle as DisplaySpanStyle};
use super::layout_out::SpanCollector;
use crate::theme::Theme;
use crossterm::style::Color;
use ui::buffer::{Buffer, LineDecoration, SpanMeta, SpanStyle};

/// Resolve a `ColorValue` against a theme snapshot. The `Theme` is
/// captured once per render so cached layouts survive theme changes
/// without invalidation.
#[inline]
fn resolve(c: ColorValue, theme: &Theme) -> Color {
    match c {
        ColorValue::Rgb(r, g, b) => Color::Rgb { r, g, b },
        ColorValue::Ansi(v) => Color::AnsiValue(v),
        ColorValue::Named(n) => Color::from(n),
        ColorValue::Role(role) => match role {
            ColorRole::Accent => theme.accent,
            ColorRole::Slug => theme.slug,
            ColorRole::UserBg => theme.user_bg,
            ColorRole::CodeBlockBg => theme.code_block_bg,
            ColorRole::Bar => theme.bar,
            ColorRole::ToolPending => theme.tool_pending,
            ColorRole::ReasonOff => theme.reason_off,
            ColorRole::Muted => theme.muted,
        },
    }
}

/// Run any span-emitting renderer (inline diff, syntax highlighter,
/// markdown, etc.) against a fresh `SpanCollector` and project the
/// captured `DisplayBlock` into `buf`. Renderers write into
/// `&mut SpanCollector`; their styled output lands as `SpanStyle`
/// highlights on `buf`, gaining scrollbar / selection / vim motions
/// for free.
pub(crate) fn render_into_buffer(
    buf: &mut Buffer,
    width: u16,
    theme: &Theme,
    fill: impl FnOnce(&mut SpanCollector),
) {
    let mut collector = SpanCollector::new(width);
    fill(&mut collector);
    let block = collector.finish();
    let lines: Vec<ProjectedLine> = block
        .lines
        .iter()
        .map(|l| project_display_line(l, theme))
        .collect();
    apply_to_buffer(buf, &lines);
}

#[derive(Default)]
pub(crate) struct ProjectedLine {
    pub text: String,
    pub highlights: Vec<(u16, u16, SpanStyle, SpanMeta)>,
    pub decoration: LineDecoration,
}

pub(crate) fn project_display_line(dline: &DisplayLine, theme: &Theme) -> ProjectedLine {
    let mut text = String::new();
    let mut highlights = Vec::new();
    let mut char_offset: u16 = 0;

    for span in &dline.spans {
        let col_start = char_offset;
        text.push_str(&span.text);
        let span_chars = span.text.chars().count() as u16;
        char_offset += span_chars;
        let col_end = char_offset;

        let style = resolve_span_style(&span.style, theme);
        let has_style = !style_is_default(&style);
        let has_meta = !span.meta.selectable || span.meta.copy_as.is_some();
        if has_style || has_meta {
            highlights.push((
                col_start,
                col_end,
                style,
                SpanMeta {
                    selectable: span.meta.selectable,
                    copy_as: span.meta.copy_as.clone(),
                },
            ));
        }
    }

    let decoration = LineDecoration {
        gutter_bg: dline.gutter_bg.map(|c| resolve(c, theme)),
        fill_bg: dline.fill_bg.map(|c| resolve(c, theme)),
        fill_right_margin: dline.fill_right_margin,
        soft_wrapped: dline.soft_wrapped,
        source_text: dline.source_text.clone(),
    };

    ProjectedLine {
        text,
        highlights,
        decoration,
    }
}

fn resolve_span_style(span: &DisplaySpanStyle, theme: &Theme) -> SpanStyle {
    SpanStyle {
        fg: span.fg.map(|c| resolve(c, theme)),
        bg: span.bg.map(|c| resolve(c, theme)),
        bold: span.bold,
        dim: span.dim,
        italic: span.italic,
    }
}

fn style_is_default(s: &SpanStyle) -> bool {
    s.fg.is_none() && s.bg.is_none() && !s.bold && !s.dim && !s.italic
}

pub(crate) fn apply_to_buffer(buf: &mut Buffer, lines: &[ProjectedLine]) {
    let text_lines: Vec<String> = lines.iter().map(|l| l.text.clone()).collect();
    buf.set_all_lines(text_lines);

    for (i, pline) in lines.iter().enumerate() {
        for (col_start, col_end, style, meta) in &pline.highlights {
            buf.add_highlight_with_meta(i, *col_start, *col_end, style.clone(), meta.clone());
        }

        let dec = &pline.decoration;
        if dec.gutter_bg.is_some()
            || dec.fill_bg.is_some()
            || dec.fill_right_margin != 0
            || dec.soft_wrapped
            || dec.source_text.is_some()
        {
            buf.set_decoration(i, dec.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::display::{ColorValue, DisplaySpan, SpanStyle as DSpanStyle};
    use crossterm::style::Color;
    use ui::buffer::BufCreateOpts;
    use ui::BufId;

    fn test_theme() -> Theme {
        crate::theme::snapshot()
    }

    fn make_buf() -> Buffer {
        Buffer::new(
            BufId(99),
            BufCreateOpts {
                modifiable: true,
                ..BufCreateOpts::default()
            },
        )
    }

    #[test]
    fn projects_styled_spans() {
        let theme = test_theme();
        let dline = DisplayLine {
            spans: vec![
                DisplaySpan {
                    text: "red".into(),
                    style: DSpanStyle {
                        fg: Some(ColorValue::Rgb(255, 0, 0)),
                        ..Default::default()
                    },
                    meta: Default::default(),
                },
                DisplaySpan {
                    text: " normal".into(),
                    style: DSpanStyle::default(),
                    meta: Default::default(),
                },
            ],
            ..Default::default()
        };
        let projected = project_display_line(&dline, &theme);
        assert_eq!(projected.text, "red normal");
        assert_eq!(projected.highlights.len(), 1);
        assert_eq!(projected.highlights[0].0, 0);
        assert_eq!(projected.highlights[0].1, 3);
        assert_eq!(
            projected.highlights[0].2.fg,
            Some(Color::Rgb { r: 255, g: 0, b: 0 })
        );
    }

    #[test]
    fn render_into_buffer_captures_inline_diff_output() {
        use super::super::highlight::print_inline_diff;
        let mut buf = make_buf();
        let theme = test_theme();
        render_into_buffer(&mut buf, 40, &theme, |sink| {
            print_inline_diff(
                sink,
                "old\nline\n",
                "new\nline\n",
                "/tmp/x.txt",
                "old\nline\n",
                0,
                10,
            );
        });
        // The diff renderer emits at least one styled line per changed
        // line; projecting must produce non-empty buffer content.
        assert!(buf.line_count() > 0);
        // At least one line should have a highlight (diff bg / fg).
        let any_highlight = (0..buf.line_count()).any(|i| !buf.highlights_at(i).is_empty());
        assert!(any_highlight, "expected at least one styled span");
    }

    #[test]
    fn apply_writes_to_buffer() {
        let mut buf = make_buf();
        let lines = vec![
            ProjectedLine {
                text: "line one".into(),
                highlights: vec![(0, 4, SpanStyle::bold(), SpanMeta::default())],
                decoration: LineDecoration::default(),
            },
            ProjectedLine {
                text: "line two".into(),
                highlights: vec![],
                decoration: LineDecoration {
                    fill_bg: Some(Color::Blue),
                    ..LineDecoration::default()
                },
            },
        ];
        apply_to_buffer(&mut buf, &lines);

        assert_eq!(buf.line_count(), 2);
        assert_eq!(buf.get_line(0), Some("line one"));
        assert_eq!(buf.get_line(1), Some("line two"));
        assert_eq!(buf.highlights_at(0).len(), 1);
        assert!(buf.highlights_at(0)[0].style.bold);
        assert_eq!(buf.decoration_at(1).fill_bg, Some(Color::Blue));
    }
}
