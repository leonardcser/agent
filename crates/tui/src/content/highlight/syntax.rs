//! Syntax-highlighted code blocks and source files plus the shared
//! `BashHighlighter` used in confirm dialogs and the transcript.

use std::path::Path;
use syntect::easy::HighlightLines;
use syntect::highlighting::Style;

use super::{syntax_theme, SYNTAX_SET};
use crate::content::display::{ColorRole, ColorValue, NamedColor};
use crate::content::layout_out::SpanCollector;
use crate::content::term_width;

/// Render a code block. When `fence` is true, the rendered output
/// stays unchanged but each code line's `source_text` carries its raw
/// content, with the opening ```` ```{lang} ```` prepended to the first
/// row and the closing ```` ``` ```` appended to the last row. This
/// lets partial selections (vim visual / click-drag) over fenced blocks
/// round-trip back to raw markdown — the visible code body is what the
/// user sees, the fences re-attach if the first/last row is covered.
pub(crate) fn render_code_block(
    out: &mut SpanCollector,
    lines: &[&str],
    lang: &str,
    width: usize,
    dim: bool,
    bctx: Option<&super::super::BoxContext>,
    fence: bool,
) -> u16 {
    let _perf = crate::perf::begin("render:code_block");
    let ext = match lang {
        "" => "txt",
        "js" | "javascript" => "js",
        "ts" | "typescript" => "ts",
        "py" | "python" => "py",
        "rb" | "ruby" => "rb",
        "rs" | "rust" => "rs",
        "sh" | "bash" | "zsh" | "shell" => "sh",
        "yml" => "yaml",
        other => other,
    };
    let syntax = SYNTAX_SET
        .find_syntax_by_extension(ext)
        .or_else(|| SYNTAX_SET.find_syntax_by_name(lang))
        .unwrap_or_else(|| SYNTAX_SET.find_syntax_plain_text());
    let theme = syntax_theme();
    let content_width = if let Some(b) = bctx { b.inner_w } else { width };
    let text_w = content_width.max(1);
    let expanded: Vec<String> = lines.iter().map(|l| l.replace('\t', "    ")).collect();
    let mut rows = 0u16;
    let mut h = HighlightLines::new(syntax, theme);

    if dim {
        out.set_dim();
    }

    let bg = ColorValue::Role(ColorRole::CodeBlockBg);
    let last_idx = expanded.len().saturating_sub(1);
    for (line_idx, line) in expanded.iter().enumerate() {
        let line_with_nl = format!("{}\n", line);
        let regions = h
            .highlight_line(&line_with_nl, &SYNTAX_SET)
            .unwrap_or_default();
        let visual_rows = split_regions_into_rows(out, &regions, text_w);
        if visual_rows.len() > 1 {
            out.mark_wrapped();
        }
        for (vi, vrow) in visual_rows.iter().enumerate() {
            if vi == 0 {
                let mut src = String::new();
                if fence && line_idx == 0 {
                    src.push_str("```");
                    src.push_str(lang);
                    src.push('\n');
                }
                src.push_str(line);
                if fence && line_idx == last_idx {
                    src.push_str("\n```");
                }
                out.set_source_text(&src);
            } else {
                out.mark_soft_wrap_continuation();
            }
            if let Some(b) = bctx {
                if dim {
                    out.reset_style();
                }
                b.print_left(out);
                if dim {
                    out.set_dim();
                }
            }
            let cols = print_split_regions(out, vrow, Some(bg));
            let pad = content_width.saturating_sub(cols);
            if pad > 0 {
                out.set_bg(bg);
                out.print_string(" ".repeat(pad));
            }
            if let Some(b) = bctx {
                if dim {
                    out.reset_style();
                }
                out.set_fg(b.color);
                out.print(b.right);
            }
            out.reset_style();
            out.newline();
        }
        rows += visual_rows.len() as u16;
    }

    if dim {
        out.reset_style();
    }
    rows
}

pub(super) fn render_highlighted(
    out: &mut SpanCollector,
    lines: &[&str],
    syntax: &syntect::parsing::SyntaxReference,
    skip: u16,
    max_rows: u16,
) -> u16 {
    let _perf = crate::perf::begin("render:highlighted");
    let indent = "  ";
    let theme = syntax_theme();
    let gutter_width = format!("{}", lines.len()).len();
    let prefix_len = indent.len() + 1 + gutter_width + 3;
    let max_content = term_width().saturating_sub(prefix_len + 1).max(1);
    let limit = lines.len();

    let blank_gutter = " ".repeat(1 + gutter_width + 3);
    let mut total_rows = 0u16;
    let mut emitted = 0u16;
    let emit_limit = if max_rows == 0 { u16::MAX } else { max_rows };
    let mut h = HighlightLines::new(syntax, theme);
    for (i, line) in lines[..limit].iter().enumerate() {
        if emitted >= emit_limit {
            break;
        }
        let line_with_nl = format!("{}\n", line);
        let regions = h
            .highlight_line(&line_with_nl, &SYNTAX_SET)
            .unwrap_or_default();
        let visual_rows = split_regions_into_rows(out, &regions, max_content);
        for (vi, vrow) in visual_rows.iter().enumerate() {
            if total_rows >= skip && emitted < emit_limit {
                out.print_gutter(indent);
                if vi == 0 {
                    out.set_fg(ColorValue::Named(NamedColor::DarkGrey));
                    out.print_gutter(&format!(" {:>w$}", i + 1, w = gutter_width));
                    out.reset_style();
                    out.print_gutter("   ");
                } else {
                    out.print_gutter(&blank_gutter);
                }
                print_split_regions(out, vrow, None);
                out.newline();
                emitted += 1;
            }
            total_rows += 1;
        }
    }
    emitted
}

pub(crate) fn print_syntax_file(
    out: &mut SpanCollector,
    content: &str,
    path: &str,
    skip: u16,
    max_rows: u16,
) -> u16 {
    print_syntax_file_ext(out, content, path, None, skip, max_rows)
}

pub(crate) fn print_syntax_file_ext(
    out: &mut SpanCollector,
    content: &str,
    path: &str,
    syntax_ext: Option<&str>,
    skip: u16,
    max_rows: u16,
) -> u16 {
    let _perf = crate::perf::begin("render:syntax_file");
    let ext = syntax_ext.unwrap_or_else(|| {
        Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("txt")
    });
    let syntax = SYNTAX_SET
        .find_syntax_by_extension(ext)
        .unwrap_or_else(|| SYNTAX_SET.find_syntax_plain_text());
    let lines: Vec<&str> = content.lines().collect();
    render_highlighted(out, &lines, syntax, skip, max_rows)
}
/// Split syntax regions into visual rows that each fit within `max_width` columns.
fn split_regions_into_rows(
    out: &mut SpanCollector,
    regions: &[(Style, &str)],
    max_width: usize,
) -> Vec<Vec<(Style, String)>> {
    let max_width = max_width.max(1);
    let mut rows: Vec<Vec<(Style, String)>> = Vec::new();
    let mut current_row: Vec<(Style, String)> = Vec::new();
    let mut col = 0;

    for (style, text) in regions {
        let text = text.trim_end_matches('\n').trim_end_matches('\r');
        if text.is_empty() {
            continue;
        }
        let mut chars = text.chars().peekable();
        while chars.peek().is_some() {
            let remaining = max_width.saturating_sub(col);
            if remaining == 0 {
                rows.push(std::mem::take(&mut current_row));
                col = 0;
                continue;
            }
            let chunk: String = chars.by_ref().take(remaining).collect();
            col += chunk.chars().count();
            current_row.push((*style, chunk));
        }
    }
    if !current_row.is_empty() {
        rows.push(current_row);
    }
    if rows.is_empty() {
        rows.push(Vec::new());
    }
    if rows.len() > 1 {
        out.mark_wrapped();
    }
    rows
}

/// Stateful bash/shell syntax highlighter that preserves state across lines.
pub(crate) struct BashHighlighter<'a> {
    h: HighlightLines<'a>,
}

impl<'a> BashHighlighter<'a> {
    pub fn new() -> Self {
        let syntax = SYNTAX_SET
            .find_syntax_by_extension("sh")
            .unwrap_or_else(|| SYNTAX_SET.find_syntax_plain_text());
        let theme = syntax_theme();
        Self {
            h: HighlightLines::new(syntax, theme),
        }
    }

    /// Advance the highlighter state without emitting output.
    pub fn advance(&mut self, line: &str) {
        let line_with_nl = format!("{}\n", line);
        let _ = self.h.highlight_line(&line_with_nl, &SYNTAX_SET);
    }

    /// Print a single line with syntax highlighting.
    /// Does not emit a newline — the caller controls line breaks.
    pub fn print_line(&mut self, out: &mut SpanCollector, line: &str) {
        let line_with_nl = format!("{}\n", line);
        let regions = self
            .h
            .highlight_line(&line_with_nl, &SYNTAX_SET)
            .unwrap_or_default();
        for (style, text) in &regions {
            let text = text.trim_end_matches('\n').trim_end_matches('\r');
            if text.is_empty() {
                continue;
            }
            let fg = ColorValue::Rgb(style.foreground.r, style.foreground.g, style.foreground.b);
            out.set_fg(fg);
            out.print(text);
        }
        out.reset_style();
    }
}

/// Print pre-split owned regions. Returns columns printed.
fn print_split_regions(
    out: &mut SpanCollector,
    regions: &[(Style, String)],
    bg: Option<ColorValue>,
) -> usize {
    let mut col = 0;
    for (style, text) in regions {
        if text.is_empty() {
            continue;
        }
        if let Some(bg_color) = bg {
            out.set_bg(bg_color);
        }
        let fg = ColorValue::Rgb(style.foreground.r, style.foreground.g, style.foreground.b);
        out.set_fg(fg);
        out.print(text);
        col += text.chars().count();
    }
    out.reset_style();
    col
}
