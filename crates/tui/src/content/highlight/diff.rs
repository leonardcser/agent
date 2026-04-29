//! Inline diff rendering: live `print_inline_diff` for tools that
//! produce a fresh diff per render, plus the persisted `CachedInlineDiff`
//! IR that `edit_file` / `edit_notebook` produce once and replay.

use serde::{Deserialize, Serialize};
use similar::{ChangeTag, TextDiff};
use std::path::Path;
use syntect::easy::HighlightLines;

use super::{syntax_theme, SYNTAX_SET};
use crate::content::display::{ColorValue, NamedColor};
use crate::content::layout_out::SpanCollector;
use crate::content::term_width;

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

pub(crate) fn build_inline_diff_cache(
    old: &str,
    new: &str,
    path: &str,
    anchor: &str,
) -> CachedInlineDiff {
    build_inline_diff_cache_ext(old, new, path, anchor, None)
}

pub(crate) fn build_inline_diff_cache_ext(
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
pub(crate) fn print_inline_diff(
    out: &mut SpanCollector,
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

fn print_cached_spans(
    out: &mut SpanCollector,
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

fn split_cached_spans_into_rows(
    out: &mut SpanCollector,
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

pub(crate) fn print_cached_inline_diff(
    out: &mut SpanCollector,
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
                out.print_gutter(indent);
                out.set_fg(ColorValue::Named(NamedColor::DarkGrey));
                out.print_gutter(&format!("{:>w$}", "...", w = 1 + gutter_width));
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
                for (vi, vrow) in visual_rows.iter().enumerate() {
                    out.print_gutter(indent);
                    if let Some((ch, color)) = sign {
                        let bgv = bg.unwrap();
                        out.set_bg(bgv);
                        if vi == 0 {
                            out.set_fg(color);
                            out.print_gutter(&format!(" {:>w$} ", lineno, w = gutter_width));
                            out.set_fg(color);
                            out.print_gutter(&format!("{} ", ch));
                        } else {
                            out.print_gutter(&blank_gutter);
                        }
                        let _content_cols = print_cached_spans(out, vrow, bg);
                        out.fill_line_bg(bgv, right_margin as u16);
                        out.reset_style();
                    } else {
                        if vi == 0 {
                            out.set_fg(ColorValue::Named(NamedColor::DarkGrey));
                            out.print_gutter(&format!(" {:>w$}", lineno, w = gutter_width));
                            out.reset_style();
                            out.print_gutter("   ");
                        } else {
                            out.print_gutter(&blank_gutter);
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
