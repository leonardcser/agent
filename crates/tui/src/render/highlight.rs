use crate::theme;
use serde::{Deserialize, Serialize};
use similar::{ChangeTag, TextDiff};
use std::path::Path;
use std::sync::LazyLock;
use syntect::easy::HighlightLines;
use syntect::highlighting::Style;
use syntect::parsing::SyntaxSet;
use unicode_width::UnicodeWidthStr;

use super::display::{ColorRole, ColorValue, NamedColor};
use super::layout_out::LayoutSink;
use super::term_width;

pub(super) static SYNTAX_SET: LazyLock<SyntaxSet> =
    LazyLock::new(SyntaxSet::load_defaults_newlines);
pub(super) static THEME_SET: LazyLock<two_face::theme::EmbeddedLazyThemeSet> =
    LazyLock::new(two_face::theme::extra);

/// Force eager initialization of the syntect syntax and theme sets. Call
/// once at startup from a background thread so the first tool render
/// doesn't pay the ~30ms deserialization cost mid-frame.
pub fn warm_up_syntect() {
    LazyLock::force(&SYNTAX_SET);
    LazyLock::force(&THEME_SET);
}

fn syntax_theme() -> &'static syntect::highlighting::Theme {
    if theme::is_light() {
        &THEME_SET[two_face::theme::EmbeddedThemeName::MonokaiExtendedLight]
    } else {
        &THEME_SET[two_face::theme::EmbeddedThemeName::MonokaiExtended]
    }
}

pub(crate) fn render_code_block<S: LayoutSink>(
    out: &mut S,
    lines: &[&str],
    lang: &str,
    width: usize,
    dim: bool,
    bctx: Option<&super::BoxContext>,
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
    for line in &expanded {
        let line_with_nl = format!("{}\n", line);
        let regions = h
            .highlight_line(&line_with_nl, &SYNTAX_SET)
            .unwrap_or_default();
        let visual_rows = split_regions_into_rows(out, &regions, text_w);
        for vrow in &visual_rows {
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

pub(super) fn render_highlighted<S: LayoutSink>(
    out: &mut S,
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
                let gutter_meta = super::display::SpanMeta {
                    selectable: false,
                    copy_as: None,
                };
                out.print_with_meta(indent, gutter_meta.clone());
                if vi == 0 {
                    out.set_fg(ColorValue::Named(NamedColor::DarkGrey));
                    out.print_with_meta(
                        &format!(" {:>w$}", i + 1, w = gutter_width),
                        gutter_meta.clone(),
                    );
                    out.reset_style();
                    out.print_with_meta("   ", gutter_meta);
                } else {
                    out.print_with_meta(&blank_gutter, gutter_meta);
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

pub(super) fn print_syntax_file<S: LayoutSink>(
    out: &mut S,
    content: &str,
    path: &str,
    skip: u16,
    max_rows: u16,
) -> u16 {
    print_syntax_file_ext(out, content, path, None, skip, max_rows)
}

pub(super) fn print_syntax_file_ext<S: LayoutSink>(
    out: &mut S,
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

struct DiffChange {
    tag: ChangeTag,
    value: String,
}

struct DiffViewData {
    file_content: String,
    start_line: usize,
    first_mod: usize,
    view_start: usize,
    view_end: usize,
    max_display_lineno: usize,
    changes: Vec<DiffChange>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedInlineDiff {
    pub max_display_lineno: usize,
    pub lines: Vec<CachedDiffLine>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedSpan {
    pub text: String,
    pub fg: (u8, u8, u8),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CachedDiffLine {
    Context {
        lineno: usize,
        text: String,
        spans: Vec<CachedSpan>,
    },
    Delete {
        lineno: usize,
        text: String,
        spans: Vec<CachedSpan>,
    },
    Insert {
        lineno: usize,
        text: String,
        spans: Vec<CachedSpan>,
    },
    Ellipsis,
}

fn cached_spans_for_line(h: &mut HighlightLines, line: &str) -> Vec<CachedSpan> {
    let line_with_nl = format!("{}\n", line);
    h.highlight_line(&line_with_nl, &SYNTAX_SET)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(style, text)| {
            let text = text.trim_end_matches('\n').trim_end_matches('\r');
            if text.is_empty() {
                None
            } else {
                Some(CachedSpan {
                    text: text.to_string(),
                    fg: (style.foreground.r, style.foreground.g, style.foreground.b),
                })
            }
        })
        .collect()
}

pub(super) fn build_inline_diff_cache(
    old: &str,
    new: &str,
    path: &str,
    anchor: &str,
) -> CachedInlineDiff {
    build_inline_diff_cache_ext(old, new, path, anchor, None)
}

pub(super) fn build_inline_diff_cache_ext(
    old: &str,
    new: &str,
    path: &str,
    anchor: &str,
    syntax_ext: Option<&str>,
) -> CachedInlineDiff {
    let _perf = crate::perf::begin("render:build_diff_cache");
    let dv = compute_diff_view(old, new, path, anchor);
    let expanded_lines: Vec<String> = dv
        .file_content
        .lines()
        .map(|l| l.replace('\t', "    "))
        .collect();
    let file_lines: Vec<&str> = expanded_lines.iter().map(|s| s.as_str()).collect();
    let lookup = if !anchor.is_empty() { anchor } else { old };
    let lookup_indent = lookup
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take_while(|c| c.is_whitespace())
        .count();
    let file_indent = file_lines
        .get(dv.start_line)
        .unwrap_or(&"")
        .chars()
        .take_while(|c| c.is_whitespace())
        .count();
    let extra_indent = " ".repeat(file_indent.saturating_sub(lookup_indent));

    let ext = syntax_ext.unwrap_or_else(|| {
        Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("txt")
    });
    let syntax = SYNTAX_SET
        .find_syntax_by_extension(ext)
        .unwrap_or_else(|| SYNTAX_SET.find_syntax_plain_text());
    let theme = syntax_theme();
    let mut h = HighlightLines::new(syntax, theme);

    let ctx = 3usize;
    let visible = compute_change_visibility(&dv.changes, ctx);
    let mut lines = Vec::new();

    let ctx_before_end = dv.start_line.min(dv.first_mod);
    let ctx_before_start = dv.view_start.min(ctx_before_end);
    for (idx, line) in file_lines[ctx_before_start..ctx_before_end]
        .iter()
        .enumerate()
    {
        lines.push(CachedDiffLine::Context {
            lineno: ctx_before_start + idx + 1,
            text: (*line).to_string(),
            spans: cached_spans_for_line(&mut h, line),
        });
    }

    let mut old_lineno = dv.start_line;
    let mut new_lineno = dv.start_line;
    let mut pending_ellipsis = false;
    let mut emitted_any = !lines.is_empty();
    for (ci, change) in dv.changes.iter().enumerate() {
        let raw = format!(
            "{}{}",
            extra_indent,
            change.value.trim_end_matches('\n').replace('\t', "    ")
        );
        let spans = cached_spans_for_line(&mut h, &raw);
        match change.tag {
            ChangeTag::Equal => {
                if visible[ci] {
                    if pending_ellipsis {
                        pending_ellipsis = false;
                        lines.push(CachedDiffLine::Ellipsis);
                    }
                    if new_lineno >= dv.view_start && new_lineno < dv.view_end {
                        lines.push(CachedDiffLine::Context {
                            lineno: new_lineno + 1,
                            text: raw,
                            spans,
                        });
                        emitted_any = true;
                    }
                } else if emitted_any {
                    pending_ellipsis = true;
                }
                old_lineno += 1;
                new_lineno += 1;
            }
            ChangeTag::Delete => {
                if pending_ellipsis {
                    pending_ellipsis = false;
                    lines.push(CachedDiffLine::Ellipsis);
                }
                lines.push(CachedDiffLine::Delete {
                    lineno: old_lineno + 1,
                    text: raw,
                    spans,
                });
                old_lineno += 1;
            }
            ChangeTag::Insert => {
                if pending_ellipsis {
                    pending_ellipsis = false;
                    lines.push(CachedDiffLine::Ellipsis);
                }
                lines.push(CachedDiffLine::Insert {
                    lineno: new_lineno + 1,
                    text: raw,
                    spans,
                });
                new_lineno += 1;
            }
        }
    }

    let anchor_lines = anchor.lines().count();
    let after_start = dv.start_line + anchor_lines;
    let after_end = dv.view_end.min(file_lines.len());
    for (idx, line) in file_lines
        .iter()
        .take(after_end)
        .skip(after_start)
        .enumerate()
    {
        lines.push(CachedDiffLine::Context {
            lineno: after_start + idx + 1,
            text: (*line).to_string(),
            spans: cached_spans_for_line(&mut h, line),
        });
    }

    CachedInlineDiff {
        max_display_lineno: dv.max_display_lineno,
        lines,
    }
}

fn compute_diff_view(old: &str, new: &str, path: &str, anchor: &str) -> DiffViewData {
    let file_content = std::fs::read_to_string(path).unwrap_or_default();
    let file_lines_count = file_content.lines().count();
    let lookup = if !anchor.is_empty() {
        anchor
    } else if !old.is_empty() {
        old
    } else {
        new
    };
    let start_line = if lookup.is_empty() {
        0
    } else {
        file_content
            .find(lookup)
            .map(|pos| file_content[..pos].bytes().filter(|&b| b == b'\n').count())
            .unwrap_or(0)
    };

    let diff = TextDiff::from_lines(old, new);
    let changes: Vec<DiffChange> = diff
        .iter_all_changes()
        .map(|c| DiffChange {
            tag: c.tag(),
            value: c.value().to_string(),
        })
        .collect();
    let ctx = 3usize;
    let mut first_mod: Option<usize> = None;
    let mut last_mod: Option<usize> = None;
    let mut new_line = start_line;
    let mut old_line = start_line;
    for c in &changes {
        match c.tag {
            ChangeTag::Equal => {
                new_line += 1;
                old_line += 1;
            }
            ChangeTag::Delete => {
                if first_mod.is_none() {
                    first_mod = Some(new_line);
                }
                last_mod = Some(new_line);
                old_line += 1;
            }
            ChangeTag::Insert => {
                if first_mod.is_none() {
                    first_mod = Some(new_line);
                }
                last_mod = Some(new_line);
                new_line += 1;
            }
        }
    }
    let first_mod = first_mod.unwrap_or(start_line);
    let last_mod = last_mod.unwrap_or(start_line);
    let view_start = first_mod.saturating_sub(ctx);
    let view_end = (last_mod + 1 + ctx).min(file_lines_count);
    let max_display_lineno = view_end.max(old_line).max(new_line);

    DiffViewData {
        file_content,
        start_line,
        first_mod,
        view_start,
        view_end,
        max_display_lineno,
        changes,
    }
}

/// For each change, decide whether it should be shown or collapsed.
/// Equal lines within `ctx` of a non-Equal change are visible; the rest are collapsed.
fn compute_change_visibility(changes: &[DiffChange], ctx: usize) -> Vec<bool> {
    let n = changes.len();
    // Forward pass: set visible based on distance from previous non-Equal.
    let mut visible = vec![false; n];
    let mut d = usize::MAX;
    for i in 0..n {
        if changes[i].tag != ChangeTag::Equal {
            d = 0;
            visible[i] = true;
        } else {
            visible[i] = d <= ctx;
        }
        d = d.saturating_add(1);
    }
    // Backward pass: also mark Equal lines near a following non-Equal.
    d = usize::MAX;
    for i in (0..n).rev() {
        if changes[i].tag != ChangeTag::Equal {
            d = 0;
        } else if d <= ctx {
            visible[i] = true;
        }
        d = d.saturating_add(1);
    }
    visible
}

/// Render a syntax-highlighted inline diff.
/// `skip` rows are computed but not emitted; up to `max_rows` visible rows
/// are written to `out`.
pub(super) fn print_inline_diff<S: LayoutSink>(
    out: &mut S,
    old: &str,
    new: &str,
    path: &str,
    anchor: &str,
    skip: u16,
    max_rows: u16,
) -> u16 {
    let _perf = crate::perf::begin("render:inline_diff_cold");
    let cache = build_inline_diff_cache(old, new, path, anchor);
    print_cached_inline_diff(out, &cache, skip, max_rows)
}

fn print_cached_spans<S: LayoutSink>(
    out: &mut S,
    spans: &[CachedSpan],
    bg: Option<ColorValue>,
) -> usize {
    let mut col = 0;
    for span in spans {
        if span.text.is_empty() {
            continue;
        }
        if let Some(bg_color) = bg {
            out.set_bg(bg_color);
        }
        out.set_fg(ColorValue::Rgb(span.fg.0, span.fg.1, span.fg.2));
        out.print(&span.text);
        col += span.text.chars().count();
    }
    out.reset_style();
    col
}

fn split_cached_spans_into_rows<S: LayoutSink>(
    out: &mut S,
    spans: &[CachedSpan],
    max_width: usize,
) -> Vec<Vec<CachedSpan>> {
    let _ = out; // wrap-marking is owned by the diff caller
    let max_width = max_width.max(1);
    let mut rows: Vec<Vec<CachedSpan>> = Vec::new();
    let mut current_row: Vec<CachedSpan> = Vec::new();
    let mut col = 0;

    for span in spans {
        if span.text.is_empty() {
            continue;
        }
        let mut chars = span.text.chars().peekable();
        while chars.peek().is_some() {
            let remaining = max_width.saturating_sub(col);
            if remaining == 0 {
                rows.push(std::mem::take(&mut current_row));
                col = 0;
                continue;
            }
            let chunk: String = chars.by_ref().take(remaining).collect();
            col += chunk.chars().count();
            current_row.push(CachedSpan {
                text: chunk,
                fg: span.fg,
            });
        }
    }
    if !current_row.is_empty() {
        rows.push(current_row);
    }
    if rows.is_empty() {
        rows.push(Vec::new());
    }
    rows
}

pub(super) fn print_cached_inline_diff<S: LayoutSink>(
    out: &mut S,
    cache: &CachedInlineDiff,
    skip: u16,
    max_rows: u16,
) -> u16 {
    let _perf = crate::perf::begin("render:inline_diff_cached");

    let indent = "  ";
    let gutter_width = format!("{}", cache.max_display_lineno).len();
    let prefix_len = indent.len() + 1 + gutter_width + 3;
    let right_margin = indent.len();
    let tw = term_width();
    let max_content = tw.saturating_sub(prefix_len + right_margin).max(1);
    // Diff lines re-wrap content per row using `term_width()`-derived
    // bounds, so the layout cannot be replayed at a different width.
    out.mark_wrapped();
    let emit_limit = if max_rows == 0 { u16::MAX } else { max_rows };
    let bg_del = ColorValue::Rgb(60, 20, 20);
    let bg_add = ColorValue::Rgb(20, 50, 20);
    let blank_gutter = " ".repeat(1 + gutter_width + 3);

    let mut emitted = 0u16;
    for (row_idx, line) in cache.lines.iter().enumerate() {
        if row_idx < skip as usize {
            continue;
        }
        if emitted >= emit_limit {
            break;
        }
        match line {
            CachedDiffLine::Ellipsis => {
                let gutter_meta = super::display::SpanMeta {
                    selectable: false,
                    copy_as: None,
                };
                out.print_with_meta(indent, gutter_meta.clone());
                out.set_fg(ColorValue::Named(NamedColor::DarkGrey));
                out.print_with_meta(&format!("{:>w$}", "...", w = 1 + gutter_width), gutter_meta);
                out.reset_style();
                out.newline();
            }
            CachedDiffLine::Context { lineno, spans, .. }
            | CachedDiffLine::Delete { lineno, spans, .. }
            | CachedDiffLine::Insert { lineno, spans, .. } => {
                let visual_rows = split_cached_spans_into_rows(out, spans, max_content);
                let (sign, bg) = match line {
                    CachedDiffLine::Context { .. } => (None, None),
                    CachedDiffLine::Delete { .. } => (
                        Some(('-', ColorValue::Named(NamedColor::Red))),
                        Some(bg_del),
                    ),
                    CachedDiffLine::Insert { .. } => (
                        Some(('+', ColorValue::Named(NamedColor::Green))),
                        Some(bg_add),
                    ),
                    CachedDiffLine::Ellipsis => unreachable!(),
                };
                let gutter_meta = super::display::SpanMeta {
                    selectable: false,
                    copy_as: None,
                };
                for (vi, vrow) in visual_rows.iter().enumerate() {
                    out.print_with_meta(indent, gutter_meta.clone());
                    if let Some((ch, color)) = sign {
                        let bgv = bg.unwrap();
                        out.set_bg(bgv);
                        if vi == 0 {
                            out.set_fg(color);
                            out.print_with_meta(
                                &format!(" {:>w$} ", lineno, w = gutter_width),
                                gutter_meta.clone(),
                            );
                            out.set_fg(color);
                            out.print_with_meta(&format!("{} ", ch), gutter_meta.clone());
                        } else {
                            out.print_with_meta(&blank_gutter, gutter_meta.clone());
                        }
                        let _content_cols = print_cached_spans(out, vrow, bg);
                        out.fill_line_bg(bgv, right_margin as u16);
                        out.reset_style();
                    } else {
                        if vi == 0 {
                            out.set_fg(ColorValue::Named(NamedColor::DarkGrey));
                            out.print_with_meta(
                                &format!(" {:>w$}", lineno, w = gutter_width),
                                gutter_meta.clone(),
                            );
                            out.reset_style();
                            out.print_with_meta("   ", gutter_meta.clone());
                        } else {
                            out.print_with_meta(&blank_gutter, gutter_meta.clone());
                        }
                        print_cached_spans(out, vrow, None);
                    }
                    out.newline();
                }
            }
        }
        emitted += 1;
    }
    emitted
}

/// Count rows an inline diff would take without rendering.
pub(super) fn count_inline_diff_rows(old: &str, new: &str, path: &str, anchor: &str) -> u16 {
    let cache = build_inline_diff_cache(old, new, path, anchor);
    count_cached_inline_diff_rows(&cache)
}

pub(super) fn count_cached_inline_diff_rows(cache: &CachedInlineDiff) -> u16 {
    let indent = "  ";
    let gutter_width = format!("{}", cache.max_display_lineno).len();
    let prefix_len = indent.len() + 1 + gutter_width + 3;
    let right_margin = indent.len();
    let max_content = term_width().saturating_sub(prefix_len + right_margin);

    let visual_rows_for = |line: &str| -> usize {
        let chars = line.replace('\t', "    ").chars().count();
        if max_content == 0 {
            1
        } else {
            chars.div_ceil(max_content)
        }
        .max(1)
    };

    cache
        .lines
        .iter()
        .map(|line| match line {
            CachedDiffLine::Ellipsis => 1,
            CachedDiffLine::Context { text, .. }
            | CachedDiffLine::Delete { text, .. }
            | CachedDiffLine::Insert { text, .. } => visual_rows_for(text),
        })
        .sum::<usize>() as u16
}

/// Split syntax regions into visual rows that each fit within `max_width` columns.
fn split_regions_into_rows<S: LayoutSink>(
    out: &mut S,
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
    pub fn print_line<S: LayoutSink>(&mut self, out: &mut S, line: &str) {
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
fn print_split_regions<S: LayoutSink>(
    out: &mut S,
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

/// Strip inline markdown markers (`**`, `*`, `__`, `_`, `` ` ``, `~~`) and
/// return the visible text content. Used for measuring visual width.
/// Recurses into nested spans so nested emphasis/code is also stripped,
/// keeping this consistent with `print_inline_styled`'s actual output.
pub(crate) fn strip_markdown_markers(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    strip_range(&chars, 0, chars.len())
}

fn strip_range(chars: &[char], start: usize, end: usize) -> String {
    let mut out = String::new();
    let mut i = start;
    while i < end {
        if let Some((content_start, content_end, after)) = skip_inline_span_range(chars, i, end) {
            // Code spans are literal; emphasis/strike recurse for nesting.
            if chars[i] == '`' {
                out.extend(chars[content_start..content_end].iter());
            } else {
                out.push_str(&strip_range(chars, content_start, content_end));
            }
            i = after;
            continue;
        }
        // When an emphasis delimiter run didn't open a span, consume
        // the whole run at once. Otherwise a stray `*` inside e.g.
        // `**text*` could be re-interpreted by the next iteration as
        // an italic opener, producing a stripped string that doesn't
        // match what `print_inline_styled` actually emits.
        if chars[i] == '*' || chars[i] == '_' {
            let run = run_length(chars, i, end, chars[i]);
            for _ in 0..run {
                out.push(chars[i]);
            }
            i += run;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Identify character positions in `text` where line-breaking is allowed.
/// Returns a bool vec parallel to `text.chars()` — `true` at spaces outside
/// inline markdown spans (delimiters are not breakable).
fn breakable_positions(text: &str) -> Vec<bool> {
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut breakable = vec![false; len];
    let mut i = 0;
    while i < len {
        if let Some((_, _, after)) = skip_inline_span_range(&chars, i, len) {
            // Jump past the entire span (delimiters + content) — no breaks inside.
            i = after;
            continue;
        }
        if chars[i] == ' ' {
            breakable[i] = true;
        }
        i += 1;
    }
    breakable
}

/// Try to match an inline markdown span at position `i` within the open
/// range `[0..end)`. Returns `Some((content_start, content_end, after))`
/// if a complete span is found. Uses strict delimiter-run matching so
/// that e.g. `**text*` does not collapse to `*` + italic("text").
fn skip_inline_span_range(chars: &[char], i: usize, end: usize) -> Option<(usize, usize, usize)> {
    if i >= end {
        return None;
    }

    // `code`: highest precedence.
    if chars[i] == '`' {
        if let Some(close) = find_code_close(chars, i + 1, end) {
            return Some((i + 1, close, close + 1));
        }
    }

    // ~~strikethrough~~
    if i + 1 < end && chars[i] == '~' && chars[i + 1] == '~' {
        if let Some(close) = find_strike_close(chars, i + 2, end) {
            return Some((i + 2, close, close + 2));
        }
    }

    // Emphasis: *italic*, **bold**, ***both*** (and `_` variants).
    if chars[i] == '*' || chars[i] == '_' {
        let marker = chars[i];
        let run = run_length(chars, i, end, marker);
        if (1..=3).contains(&run) && can_open_emphasis(chars, i, run, end, marker) {
            if let Some(close) = find_closing_run(chars, i + run, end, marker, run) {
                return Some((i + run, close, close + run));
            }
        }
    }

    None
}

pub(crate) fn render_markdown_table<S: LayoutSink>(
    out: &mut S,
    rows: &[Vec<String>],
    dim: bool,
    bctx: Option<&super::BoxContext>,
    indent: &str,
) -> u16 {
    if rows.is_empty() {
        return 0;
    }

    let num_cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if num_cols == 0 {
        return 0;
    }

    // Calculate column widths based on visual (stripped) content.
    let max_table = if let Some(b) = bctx {
        b.inner_w.saturating_sub(1)
    } else {
        term_width().saturating_sub(2)
    };
    let mut col_widths = vec![0usize; num_cols];
    for row in rows {
        for (c, cell) in row.iter().enumerate() {
            let visual = strip_markdown_markers(cell).width();
            col_widths[c] = col_widths[c].max(visual);
        }
    }

    // Borders: "┃" + (" col ┃") * num_cols → 3 * num_cols + 1.
    let overhead = 3 * num_cols + 1;

    // Minimum column widths: the longest unwrappable segment per column.
    let mut min_widths = vec![0usize; num_cols];
    for row in rows {
        for (c, cell) in row.iter().enumerate() {
            min_widths[c] = min_widths[c].max(min_visual_width(cell));
        }
    }

    // Shrink columns by wrapping until the table fits, or we hit minimums.
    let total: usize = col_widths.iter().sum::<usize>() + overhead;
    if total > max_table {
        let avail = max_table.saturating_sub(overhead);
        let min_total: usize = min_widths.iter().sum();

        if min_total > avail {
            // Can't fit even at minimum widths — switch to stacked layout.
            return render_table_stacked(out, rows, dim);
        }

        // Shrink proportionally but clamp to min_widths.
        let content_total: usize = col_widths.iter().sum();
        if content_total > 0 {
            // First pass: proportional shrink.
            let mut new_widths: Vec<usize> = col_widths
                .iter()
                .zip(min_widths.iter())
                .map(|(&w, &min)| ((w * avail) / content_total).max(min))
                .collect();

            // Redistribute any excess from clamped columns.
            loop {
                let used: usize = new_widths.iter().sum();
                if used <= avail {
                    break;
                }
                let excess = used - avail;
                // Find columns that can still shrink.
                let shrinkable: Vec<usize> = (0..num_cols)
                    .filter(|&c| new_widths[c] > min_widths[c])
                    .collect();
                if shrinkable.is_empty() {
                    break;
                }
                let per_col = (excess / shrinkable.len()).max(1);
                for &c in &shrinkable {
                    let reduce = per_col.min(new_widths[c] - min_widths[c]);
                    new_widths[c] -= reduce;
                }
            }
            col_widths = new_widths;
        }
    }

    let mut total_rows = 0u16;

    let bar = |out: &mut S, dim: bool| {
        out.set_fg(ColorValue::Role(ColorRole::Bar));
        if dim {
            out.set_dim();
        }
    };
    let reset = |out: &mut S, _dim: bool| {
        out.reset_style();
    };

    let render_table_row = |out: &mut S, row: &[String], widths: &[usize], dim: bool| -> u16 {
        let wrapped: Vec<Vec<String>> = row
            .iter()
            .enumerate()
            .map(|(c, cell)| {
                let w = widths.get(c).copied().unwrap_or(0);
                wrap_cell_words(out, cell, w)
            })
            .collect();
        let height = wrapped.iter().map(|w| w.len()).max().unwrap_or(1);

        for vline in 0..height {
            if let Some(b) = bctx {
                b.print_left(out);
            } else if !indent.is_empty() {
                out.print(indent);
            }
            bar(out, dim);
            out.print("┃");
            reset(out, dim);
            let mut line_cols = 1; // "┃"
            for (c, width) in widths.iter().enumerate() {
                let text = wrapped
                    .get(c)
                    .and_then(|w| w.get(vline))
                    .map(|s| s.as_str())
                    .unwrap_or("");
                let visual_len = strip_markdown_markers(text).width();
                out.print(" ");
                print_inline_styled(out, text, dim);
                let pad = width.saturating_sub(visual_len);
                if pad > 0 {
                    out.print_string(" ".repeat(pad));
                }
                out.print(" ");
                bar(out, dim);
                out.print("┃");
                reset(out, dim);
                line_cols += width + 3; // " content pad ┃"
            }
            if let Some(b) = bctx {
                b.print_right(out, line_cols);
            }
            out.newline();
        }
        height as u16
    };

    // left, horizontal, junction, right
    let render_border =
        |out: &mut S, widths: &[usize], dim: bool, l: &str, j: &str, r: &str| -> u16 {
            if let Some(b) = bctx {
                b.print_left(out);
            } else if !indent.is_empty() {
                out.print(indent);
            }
            bar(out, dim);
            out.print(l);
            let mut line_cols = 1; // "l"
            for (c, width) in widths.iter().enumerate() {
                let seg = width + 2;
                out.print_string("━".repeat(seg));
                line_cols += seg;
                if c + 1 < widths.len() {
                    out.print(j);
                    line_cols += 1;
                }
            }
            out.print(r);
            line_cols += 1;
            reset(out, dim);
            if let Some(b) = bctx {
                b.print_right(out, line_cols);
            }
            out.newline();
            1
        };

    // Top border
    total_rows += render_border(out, &col_widths, dim, "┏", "┳", "┓");

    // Header
    if let Some(header) = rows.first() {
        total_rows += render_table_row(out, header, &col_widths, dim);
        total_rows += render_border(out, &col_widths, dim, "┣", "╋", "┫");
    }

    // Data rows
    for row in rows.iter().skip(1) {
        total_rows += render_table_row(out, row, &col_widths, dim);
    }

    // Bottom border
    total_rows += render_border(out, &col_widths, dim, "┗", "┻", "┛");

    total_rows
}

/// Stacked layout for tables too wide for the terminal.
/// Each data row becomes a block of "Header: value" lines, separated by blank lines.
fn render_table_stacked<S: LayoutSink>(out: &mut S, rows: &[Vec<String>], dim: bool) -> u16 {
    let header = match rows.first() {
        Some(h) => h,
        None => return 0,
    };

    let label_width = header
        .iter()
        .map(|h| strip_markdown_markers(h).width())
        .max()
        .unwrap_or(0);

    // "  label  value" → indent for continuation lines is 2 + label_width + 2
    let value_indent = 2 + label_width + 2;
    let value_width = term_width().saturating_sub(value_indent);

    let mut total_rows = 0u16;
    for (ri, row) in rows.iter().skip(1).enumerate() {
        if ri > 0 {
            out.newline();
            total_rows += 1;
        }
        for (c, cell) in row.iter().enumerate() {
            let label = header.get(c).map(|s| s.as_str()).unwrap_or("");
            let label_visual = strip_markdown_markers(label).width();
            let pad = label_width.saturating_sub(label_visual);

            let wrapped = wrap_cell_words(out, cell, value_width);
            for (li, line) in wrapped.iter().enumerate() {
                if li == 0 {
                    out.print("  ");
                    out.set_fg(ColorValue::Named(NamedColor::DarkGrey));
                    if dim {
                        out.set_dim();
                    }
                    print_inline_styled(out, label, dim);
                    if pad > 0 {
                        out.print_string(" ".repeat(pad));
                    }
                    out.reset_style();
                    out.print("  ");
                } else {
                    out.print_string(" ".repeat(value_indent));
                }
                print_inline_styled(out, line, dim);
                out.newline();
                total_rows += 1;
            }
        }
    }
    total_rows
}

/// Word-wrap cell text so each line's visual width (after stripping markers) fits within `max_width`.
/// Only breaks at spaces that are outside inline markdown spans.
fn wrap_cell_words<S: LayoutSink>(out: &mut S, text: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![text.to_string()];
    }

    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let breakable = breakable_positions(text);

    let mut lines = Vec::new();
    let mut line_start = 0usize;
    let mut last_break = None::<usize>;
    for ci in 0..len {
        if breakable[ci] {
            last_break = Some(ci);
        }
        let visual_width =
            strip_markdown_markers(&chars[line_start..=ci].iter().collect::<String>()).width();

        if visual_width > max_width {
            if let Some(bp) = last_break {
                let line: String = chars[line_start..bp].iter().collect();
                lines.push(line);
                line_start = bp + 1;
                last_break = None;
            }
        }
    }
    if line_start < len {
        let line: String = chars[line_start..].iter().collect();
        lines.push(line);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    if lines.len() > 1 {
        out.mark_wrapped();
    }
    lines
}

/// Find the visual width of the longest unwrappable segment in text.
/// Used to compute minimum column widths.
fn min_visual_width(text: &str) -> usize {
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let breakable = breakable_positions(text);

    let mut max_w = 0usize;
    let mut seg_start = 0;
    for ci in 0..len {
        if breakable[ci] {
            if ci > seg_start {
                let seg: String = chars[seg_start..ci].iter().collect();
                max_w = max_w.max(strip_markdown_markers(&seg).width());
            }
            seg_start = ci + 1;
        }
    }
    if seg_start < len {
        let seg: String = chars[seg_start..].iter().collect();
        max_w = max_w.max(strip_markdown_markers(&seg).width());
    }
    max_w
}

/// Render inline markdown spans: `**bold**`, `__bold__`, `*italic*`, `_italic_`,
/// `***bold+italic***`, `` `code` ``, `~~strikethrough~~`.
/// Everything else passes through literally.
pub(crate) fn print_inline_styled<S: LayoutSink>(out: &mut S, text: &str, dim: bool) {
    if dim {
        out.push_dim();
    }
    let chars: Vec<char> = text.chars().collect();
    let nodes = parse_inline(&chars, 0, chars.len());
    emit_inline_nodes(out, &nodes);
    if dim {
        out.pop_style();
    }
}

// ── Inline markdown AST + parser ─────────────────────────────────────────
//
// `print_inline_styled` parses its input into a small `InlineNode` tree
// and then walks the tree to emit spans. The tree approach is what lets
// nested spans (bold containing italic, code inside italic, …) render
// correctly: each inner node pushes a style on top of the outer one
// instead of flatly resetting between spans.
//
// Delimiter matching is **strict** on count: an opener of length N can
// only match a closer of length N. That prevents the "inverted" case
// where e.g. `**text*` used to flip an unclosed bold into an italic by
// letting a single `*` close a double `**`. Runs that don't match
// anything are emitted as literal text *as a whole run*, so the trailing
// `*` of `**text*` never gets re-scanned as a new italic opener.

enum InlineNode {
    Text(String),
    Code(String),
    Strike(Vec<InlineNode>),
    Bold(Vec<InlineNode>),
    Italic(Vec<InlineNode>),
    BoldItalic(Vec<InlineNode>),
}

/// Length of the run of consecutive `marker` chars starting at `i`.
fn run_length(chars: &[char], i: usize, end: usize, marker: char) -> usize {
    let mut j = i;
    while j < end && chars[j] == marker {
        j += 1;
    }
    j - i
}

/// Can a delimiter run of `count` `marker` chars at position `i` open
/// emphasis? Rules (simplified CommonMark left-flanking):
/// - The character after the run must exist and not be whitespace.
/// - For `_`: the character before the run must not be alphanumeric.
///   Prevents intraword emphasis like `snake_case` or URLs containing
///   underscores.
fn can_open_emphasis(chars: &[char], i: usize, count: usize, end: usize, marker: char) -> bool {
    let after = i + count;
    if after >= end || chars[after].is_whitespace() {
        return false;
    }
    if marker == '_' && i > 0 && chars[i - 1].is_alphanumeric() {
        return false;
    }
    true
}

/// Find a closing delimiter run of **exactly** `count` consecutive
/// `marker` chars in `[start..end)`. Rules:
/// - The character before the run must not be whitespace
///   (right-flanking).
/// - For `_`: the character after the run must not be alphanumeric.
/// - Run length must equal `count` exactly — a run of 1 cannot close an
///   opener of 2, and vice versa.
fn find_closing_run(
    chars: &[char],
    start: usize,
    end: usize,
    marker: char,
    count: usize,
) -> Option<usize> {
    let mut j = start;
    while j < end {
        if chars[j] == marker {
            let run = run_length(chars, j, end, marker);
            if run == count && j > 0 && !chars[j - 1].is_whitespace() {
                let after = j + run;
                if marker == '*' || after >= end || !chars[after].is_alphanumeric() {
                    return Some(j);
                }
            }
            j += run;
        } else {
            j += 1;
        }
    }
    None
}

/// Find the closing backtick of a code span starting at `start`.
fn find_code_close(chars: &[char], start: usize, end: usize) -> Option<usize> {
    let mut j = start;
    while j < end {
        if chars[j] == '`' {
            return Some(j);
        }
        j += 1;
    }
    None
}

/// Find the closing `~~` of a strikethrough span.
fn find_strike_close(chars: &[char], start: usize, end: usize) -> Option<usize> {
    let mut j = start;
    while j + 1 < end {
        if chars[j] == '~' && chars[j + 1] == '~' {
            return Some(j);
        }
        j += 1;
    }
    None
}

/// Parse the slice `chars[start..end]` into a flat list of `InlineNode`s.
/// Recurses into emphasis/strikethrough content so nesting works, but
/// treats code-span content as literal.
fn parse_inline(chars: &[char], start: usize, end: usize) -> Vec<InlineNode> {
    let mut nodes: Vec<InlineNode> = Vec::new();
    let mut plain = String::new();
    let mut i = start;

    macro_rules! flush_plain {
        () => {
            if !plain.is_empty() {
                nodes.push(InlineNode::Text(std::mem::take(&mut plain)));
            }
        };
    }

    while i < end {
        // Code span (precedence over emphasis: CommonMark §6.1).
        if chars[i] == '`' {
            if let Some(close) = find_code_close(chars, i + 1, end) {
                flush_plain!();
                let content: String = chars[i + 1..close].iter().collect();
                nodes.push(InlineNode::Code(content));
                i = close + 1;
                continue;
            }
        }

        // Strikethrough `~~text~~`.
        if i + 1 < end && chars[i] == '~' && chars[i + 1] == '~' {
            if let Some(close) = find_strike_close(chars, i + 2, end) {
                flush_plain!();
                let inner = parse_inline(chars, i + 2, close);
                nodes.push(InlineNode::Strike(inner));
                i = close + 2;
                continue;
            }
        }

        // Emphasis: `*italic*`, `**bold**`, `***both***`.
        if chars[i] == '*' || chars[i] == '_' {
            let marker = chars[i];
            let open_run = run_length(chars, i, end, marker);

            if (1..=3).contains(&open_run) && can_open_emphasis(chars, i, open_run, end, marker) {
                if let Some(close) = find_closing_run(chars, i + open_run, end, marker, open_run) {
                    flush_plain!();
                    let inner = parse_inline(chars, i + open_run, close);
                    let node = match open_run {
                        1 => InlineNode::Italic(inner),
                        2 => InlineNode::Bold(inner),
                        3 => InlineNode::BoldItalic(inner),
                        _ => unreachable!("run length checked by contains()"),
                    };
                    nodes.push(node);
                    i = close + open_run;
                    continue;
                }
            }

            // No match — emit the ENTIRE run as literal and skip past it.
            // Emitting char-by-char would let the tail of the run re-enter
            // the parser as a new opener (the "inverted emphasis" bug).
            for _ in 0..open_run {
                plain.push(marker);
            }
            i += open_run;
            continue;
        }

        plain.push(chars[i]);
        i += 1;
    }

    flush_plain!();
    nodes
}

/// Walk an `InlineNode` tree and emit its spans to the sink. Uses
/// `push_style`/`pop_style` so inner nodes inherit the outer style —
/// e.g. italic inside bold becomes a single span with both attributes.
fn emit_inline_nodes<S: LayoutSink>(out: &mut S, nodes: &[InlineNode]) {
    for node in nodes {
        match node {
            InlineNode::Text(s) => out.print(s),
            InlineNode::Code(s) => {
                out.push_fg(ColorValue::Role(ColorRole::Accent));
                out.print(s);
                out.pop_style();
            }
            InlineNode::Strike(children) => {
                out.push_crossedout();
                emit_inline_nodes(out, children);
                out.pop_style();
            }
            InlineNode::Bold(children) => {
                out.push_bold();
                emit_inline_nodes(out, children);
                out.pop_style();
            }
            InlineNode::Italic(children) => {
                out.push_italic();
                emit_inline_nodes(out, children);
                out.pop_style();
            }
            InlineNode::BoldItalic(children) => {
                let mut style = out.snapshot_style();
                style.bold = true;
                style.italic = true;
                out.push_style(style);
                emit_inline_nodes(out, children);
                out.pop_style();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::display::{ColorRole, ColorValue, SpanStyle};
    use super::super::layout_out::SpanCollector;
    use super::*;

    /// Render `text` through `print_inline_styled` (dim=false) and return
    /// a compact `Vec<(tag, text)>` representation of the span tree.
    /// Tags: "plain", "bold", "italic", "bi" (bold+italic), "code",
    /// "strike". Adjacent spans with the same style are merged by the
    /// sink, so you get one entry per visible style run.
    fn parse(text: &str) -> Vec<(&'static str, String)> {
        let mut sink = SpanCollector::new(200);
        print_inline_styled(&mut sink, text, false);
        let block = sink.finish();
        let line = match block.lines.into_iter().next() {
            Some(l) => l,
            None => return Vec::new(),
        };
        line.spans
            .into_iter()
            .filter(|s| !s.text.is_empty())
            .map(|s| (tag_for(&s.style), s.text))
            .collect()
    }

    fn tag_for(style: &SpanStyle) -> &'static str {
        // Code spans carry an accent foreground; they can also inherit
        // bold/italic when nested inside emphasis, in which case the
        // rendered span shows both attributes at once.
        let is_code = matches!(style.fg, Some(ColorValue::Role(ColorRole::Accent)));
        match (style.bold, style.italic, style.crossedout, is_code) {
            (false, false, false, false) => "plain",
            (true, false, false, false) => "bold",
            (false, true, false, false) => "italic",
            (true, true, false, false) => "bi",
            (false, false, true, false) => "strike",
            (false, false, false, true) => "code",
            (true, false, false, true) => "bold+code",
            (false, true, false, true) => "italic+code",
            (true, true, false, true) => "bi+code",
            _ => "mixed",
        }
    }

    // Tag shorthands.
    fn p(s: &str) -> (&'static str, String) {
        ("plain", s.into())
    }
    fn b(s: &str) -> (&'static str, String) {
        ("bold", s.into())
    }
    fn i(s: &str) -> (&'static str, String) {
        ("italic", s.into())
    }
    fn bi(s: &str) -> (&'static str, String) {
        ("bi", s.into())
    }
    fn c(s: &str) -> (&'static str, String) {
        ("code", s.into())
    }
    fn s(s: &str) -> (&'static str, String) {
        ("strike", s.into())
    }

    // ── Plain ──────────────────────────────────────────────────────────

    #[test]
    fn plain_text() {
        assert_eq!(parse("hello world"), vec![p("hello world")]);
    }

    #[test]
    fn empty_string() {
        assert_eq!(parse(""), vec![]);
    }

    // ── Bold ───────────────────────────────────────────────────────────

    #[test]
    fn bold_star() {
        assert_eq!(parse("**hello**"), vec![b("hello")]);
    }

    #[test]
    fn bold_underscore() {
        assert_eq!(parse("__hello__"), vec![b("hello")]);
    }

    #[test]
    fn bold_within_text() {
        assert_eq!(parse("a **bold** c"), vec![p("a "), b("bold"), p(" c")]);
    }

    // ── Italic ─────────────────────────────────────────────────────────

    #[test]
    fn italic_star() {
        assert_eq!(parse("*hello*"), vec![i("hello")]);
    }

    #[test]
    fn italic_underscore() {
        assert_eq!(parse("_hello_"), vec![i("hello")]);
    }

    #[test]
    fn italic_within_text() {
        assert_eq!(parse("a *word* b"), vec![p("a "), i("word"), p(" b")]);
    }

    // ── Bold + italic (triple delimiters) ──────────────────────────────

    #[test]
    fn bold_italic_star() {
        assert_eq!(parse("***both***"), vec![bi("both")]);
    }

    #[test]
    fn bold_italic_underscore() {
        assert_eq!(parse("___both___"), vec![bi("both")]);
    }

    // ── Inline code ────────────────────────────────────────────────────

    #[test]
    fn inline_code() {
        assert_eq!(parse("`foo`"), vec![c("foo")]);
    }

    #[test]
    fn inline_code_with_stars_inside() {
        // Stars inside backticks are literal.
        assert_eq!(parse("`*not bold*`"), vec![c("*not bold*")]);
    }

    #[test]
    fn inline_code_with_underscores_inside() {
        assert_eq!(parse("`_not italic_`"), vec![c("_not italic_")]);
    }

    #[test]
    fn inline_code_around_text() {
        assert_eq!(
            parse("call `foo()` please"),
            vec![p("call "), c("foo()"), p(" please")]
        );
    }

    // ── Strikethrough ──────────────────────────────────────────────────

    #[test]
    fn strikethrough_basic() {
        assert_eq!(parse("~~gone~~"), vec![s("gone")]);
    }

    // ── Intraword underscores (CommonMark: NOT emphasis) ──────────────

    #[test]
    fn intraword_underscore_identifier() {
        // `snake_case_variable` — underscores are part of the identifier.
        assert_eq!(parse("snake_case_variable"), vec![p("snake_case_variable")]);
    }

    #[test]
    fn intraword_underscore_in_url() {
        assert_eq!(
            parse("https://example.com/foo_bar_baz"),
            vec![p("https://example.com/foo_bar_baz")]
        );
    }

    #[test]
    fn intraword_underscore_between_letters() {
        assert_eq!(parse("foo_bar"), vec![p("foo_bar")]);
    }

    // ── Unclosed delimiters (should stay literal) ─────────────────────

    #[test]
    fn unclosed_bold_stays_literal() {
        assert_eq!(parse("**text"), vec![p("**text")]);
    }

    #[test]
    fn unclosed_italic_stays_literal() {
        assert_eq!(parse("*text"), vec![p("*text")]);
    }

    #[test]
    fn unclosed_code_stays_literal() {
        assert_eq!(parse("`unclosed"), vec![p("`unclosed")]);
    }

    /// Regression: `**text*` (3 stars) is an unclosed bold, NOT an
    /// opened bold that collapses to italic. Previously the parser
    /// dropped the leading `*` and produced an italic, giving the user
    /// an "inverted" result (italic instead of bold).
    #[test]
    fn odd_star_count_does_not_invert_emphasis() {
        assert_eq!(parse("**text*"), vec![p("**text*")]);
    }

    #[test]
    fn odd_star_count_trailing_double() {
        assert_eq!(parse("*text**"), vec![p("*text**")]);
    }

    // ── Nested emphasis (CommonMark supports this) ────────────────────

    #[test]
    fn bold_containing_italic() {
        // `**bold *italic* bold**` — inner italic must render inside bold.
        assert_eq!(
            parse("**bold *it* bold**"),
            vec![b("bold "), bi("it"), b(" bold")]
        );
    }

    #[test]
    fn italic_containing_bold() {
        assert_eq!(
            parse("*it **bold** it*"),
            vec![i("it "), bi("bold"), i(" it")]
        );
    }

    #[test]
    fn bold_containing_code() {
        // Code span nested inside bold inherits the outer bold, so the
        // inner span carries both attributes at once.
        assert_eq!(
            parse("**call `foo()` now**"),
            vec![b("call "), ("bold+code", "foo()".into()), b(" now")]
        );
    }

    // ── Precedence: code > emphasis ───────────────────────────────────

    #[test]
    fn code_inside_italic() {
        // `*a `code` b*` — italic wrapping, code inside. The inner code
        // span inherits italic, so it's italic+code.
        assert_eq!(
            parse("*a `code` b*"),
            vec![i("a "), ("italic+code", "code".into()), i(" b")]
        );
    }

    #[test]
    fn code_containing_italic_stars() {
        // The `*` inside a code span is literal.
        assert_eq!(
            parse("before `*x*` after"),
            vec![p("before "), c("*x*"), p(" after")]
        );
    }

    // ── Multiple runs on one line ─────────────────────────────────────

    #[test]
    fn bold_then_italic() {
        assert_eq!(parse("**a** and *b*"), vec![b("a"), p(" and "), i("b")]);
    }

    #[test]
    fn adjacent_bolds() {
        assert_eq!(parse("**a** **b**"), vec![b("a"), p(" "), b("b")]);
    }

    // ── Asterisk as literal ───────────────────────────────────────────

    #[test]
    fn asterisk_as_multiplication() {
        // `a * b` — stars with whitespace on both sides, not emphasis.
        assert_eq!(parse("a * b = c"), vec![p("a * b = c")]);
    }

    #[test]
    fn trailing_lone_star() {
        assert_eq!(parse("note*"), vec![p("note*")]);
    }

    #[test]
    fn star_right_after_word() {
        assert_eq!(parse("footnote*"), vec![p("footnote*")]);
    }

    // ── Stress: edge cases the spec cares about ──────────────────────

    #[test]
    fn space_before_closing_delim_rejects_emphasis() {
        // `**text **` — close preceded by space is NOT right-flanking.
        assert_eq!(parse("**text **"), vec![p("**text **")]);
    }

    #[test]
    fn space_after_opening_delim_rejects_emphasis() {
        // `** text**` — open followed by space is NOT left-flanking.
        assert_eq!(parse("** text**"), vec![p("** text**")]);
    }

    #[test]
    fn four_star_run_is_literal() {
        // Runs of 4+ delimiters have no standard meaning; keep them literal.
        assert_eq!(parse("****text****"), vec![p("****text****")]);
    }

    #[test]
    fn deeply_nested_bold_italic_code() {
        // `**outer *inner `code` inner* outer**`
        assert_eq!(
            parse("**a *b `c` d* e**"),
            vec![
                b("a "),
                bi("b "),
                ("bi+code", "c".into()),
                bi(" d"),
                b(" e"),
            ]
        );
    }

    #[test]
    fn bold_italic_containing_plain_text() {
        assert_eq!(parse("***a b c***"), vec![bi("a b c")]);
    }

    #[test]
    fn two_italic_runs_separated_by_text() {
        assert_eq!(
            parse("start *a* mid *b* end"),
            vec![p("start "), i("a"), p(" mid "), i("b"), p(" end"),]
        );
    }

    #[test]
    fn mixed_underscore_and_star_dont_match() {
        // `*foo_` — `*` opener, `_` is just a literal char, not a closer.
        assert_eq!(parse("*foo_"), vec![p("*foo_")]);
    }

    #[test]
    fn underscore_surrounded_by_non_alnum_can_italic() {
        // `(_foo_)` — `_` is not intraword here because `(` and `)` are
        // not alphanumeric. CommonMark permits this as italic.
        assert_eq!(parse("(_foo_)"), vec![p("("), i("foo"), p(")")]);
    }

    #[test]
    fn star_can_italic_intraword() {
        // Unlike `_`, `*` does not have the intraword restriction.
        assert_eq!(parse("foo*bar*baz"), vec![p("foo"), i("bar"), p("baz")]);
    }

    #[test]
    fn code_with_backtick_literal() {
        // A backtick inside a code span closes it — our single-backtick
        // parser can't represent literal backticks inside a code span.
        // `` `a`b` `` → code("a") + plain("b`").
        assert_eq!(parse("`a`b`"), vec![c("a"), p("b`")]);
    }

    #[test]
    fn strip_markers_matches_parse_for_nested() {
        // The visible width used by wrapping code must match the text
        // that the parser actually emits.
        let text = "**bold *it* bold**";
        let stripped = strip_markdown_markers(text);
        assert_eq!(stripped, "bold it bold");
        // And matches what print_inline_styled would emit:
        let emitted: String = parse(text).into_iter().map(|(_, t)| t).collect();
        assert_eq!(emitted, stripped);
    }

    #[test]
    fn strip_markers_handles_intraword_underscore() {
        // Must not strip `_` that are intraword — they're part of the
        // identifier, not emphasis markers.
        assert_eq!(
            strip_markdown_markers("call foo_bar_baz() now"),
            "call foo_bar_baz() now"
        );
    }

    #[test]
    fn strip_markers_matches_parse_for_unclosed_bold() {
        // The old parser produced `*` + italic("text") for `**text*`,
        // giving width=4 after stripping. The new parser keeps the run
        // literal, so stripping should return the whole thing.
        assert_eq!(strip_markdown_markers("**text*"), "**text*");
    }
}
