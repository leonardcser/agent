use super::display::{DisplayLine, SpanStyle as DisplaySpanStyle};
use super::history::{BlockHistory, LayoutKey, ViewState};
use super::paint::resolve;
use crate::theme::Theme;
use ui::buffer::{Buffer, LineDecoration, SpanMeta, SpanStyle};

pub(crate) struct TranscriptProjection {
    buf: Buffer,
    display_lines: Vec<DisplayLine>,
    generation: u64,
    width: u16,
    show_thinking: bool,
}

impl TranscriptProjection {
    pub(crate) fn new(buf: Buffer) -> Self {
        Self {
            buf,
            display_lines: Vec::new(),
            generation: u64::MAX,
            width: 0,
            show_thinking: false,
        }
    }

    pub(crate) fn buf(&self) -> &Buffer {
        &self.buf
    }

    pub(crate) fn total_lines(&self) -> usize {
        self.buf.line_count()
    }

    pub(super) fn viewport_display_lines(
        &self,
        scroll: u16,
        viewport_rows: u16,
    ) -> Vec<DisplayLine> {
        let start = scroll as usize;
        let end = (start + viewport_rows as usize).min(self.display_lines.len());
        self.display_lines[start..end].to_vec()
    }

    pub(super) fn project(
        &mut self,
        history: &mut BlockHistory,
        width: u16,
        show_thinking: bool,
        theme: &Theme,
        ephemeral_lines: &[DisplayLine],
    ) {
        let gen = history.generation();
        if gen == self.generation && width == self.width && show_thinking == self.show_thinking {
            return;
        }

        if width as usize != history.cache_width {
            history.invalidate_for_width(width as usize);
        }

        let key = LayoutKey {
            view_state: ViewState::Expanded,
            width,
            show_thinking,
            content_hash: 0,
        };

        let mut lines: Vec<ProjectedLine> = Vec::new();
        let mut raw_lines: Vec<DisplayLine> = Vec::new();

        for i in 0..history.len() {
            let rows = history.ensure_rows(i, key);
            let gap = if rows == 0 { 0 } else { history.block_gap(i) };

            for _ in 0..gap {
                lines.push(ProjectedLine::default());
                raw_lines.push(DisplayLine::default());
            }

            let id = history.order[i];
            let bkey = history.resolve_key(id, key);
            if let Some(display) = history.artifacts.get(&id).and_then(|a| a.get(bkey)) {
                for dline in &display.lines {
                    lines.push(project_display_line(dline, theme));
                    raw_lines.push(dline.clone());
                }
            }
        }

        for dline in ephemeral_lines {
            lines.push(project_display_line(dline, theme));
            raw_lines.push(dline.clone());
        }

        apply_to_buffer(&mut self.buf, &lines);
        self.display_lines = raw_lines;

        self.generation = gen;
        self.width = width;
        self.show_thinking = show_thinking;
    }
}

#[derive(Default)]
struct ProjectedLine {
    text: String,
    highlights: Vec<(u16, u16, SpanStyle, SpanMeta)>,
    decoration: LineDecoration,
}

fn project_display_line(dline: &DisplayLine, theme: &Theme) -> ProjectedLine {
    let mut text = String::new();
    let mut highlights = Vec::new();

    for span in &dline.spans {
        let col_start = text.len() as u16;
        text.push_str(&span.text);
        let col_end = text.len() as u16;

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
        gutter_bg: dline.gutter_bg.map(|c| resolve(c, theme, true)),
        fill_bg: dline.fill_bg.map(|c| resolve(c, theme, true)),
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
        fg: span.fg.map(|c| resolve(c, theme, false)),
        bg: span.bg.map(|c| resolve(c, theme, true)),
        bold: span.bold,
        dim: span.dim,
        italic: span.italic,
    }
}

fn style_is_default(s: &SpanStyle) -> bool {
    s.fg.is_none() && s.bg.is_none() && !s.bold && !s.dim && !s.italic
}

fn apply_to_buffer(buf: &mut Buffer, lines: &[ProjectedLine]) {
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
    use crate::render::display::{ColorValue, DisplaySpan, SpanStyle as DSpanStyle};
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
    fn projects_plain_line() {
        let theme = test_theme();
        let dline = DisplayLine {
            spans: vec![DisplaySpan {
                text: "hello world".into(),
                style: DSpanStyle::default(),
                meta: Default::default(),
            }],
            ..Default::default()
        };
        let projected = project_display_line(&dline, &theme);
        assert_eq!(projected.text, "hello world");
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
        assert_eq!(projected.highlights[0].0, 0); // col_start
        assert_eq!(projected.highlights[0].1, 3); // col_end
        assert_eq!(
            projected.highlights[0].2.fg,
            Some(Color::Rgb { r: 255, g: 0, b: 0 })
        );
    }

    #[test]
    fn projects_fill_bg_decoration() {
        let theme = test_theme();
        let dline = DisplayLine {
            spans: vec![DisplaySpan {
                text: "code".into(),
                style: DSpanStyle::default(),
                meta: Default::default(),
            }],
            fill_bg: Some(ColorValue::Rgb(30, 30, 30)),
            fill_right_margin: 2,
            ..Default::default()
        };
        let projected = project_display_line(&dline, &theme);
        assert_eq!(
            projected.decoration.fill_bg,
            Some(Color::Rgb {
                r: 30,
                g: 30,
                b: 30
            })
        );
        assert_eq!(projected.decoration.fill_right_margin, 2);
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
