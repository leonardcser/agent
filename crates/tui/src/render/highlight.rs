use crate::render::blocks::print_styled_dim;
use crate::theme;
use comfy_table::{presets::UTF8_BORDERS_ONLY, ContentArrangement, Table};
use crossterm::{
    style::{
        Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
    },
    QueueableCommand,
};
use similar::{ChangeTag, TextDiff};
use std::path::Path;
use std::sync::LazyLock;
use syntect::easy::HighlightLines;
use syntect::highlighting::Style;
use syntect::parsing::SyntaxSet;

use super::{crlf, term_width, RenderOut};

pub(super) static SYNTAX_SET: LazyLock<SyntaxSet> =
    LazyLock::new(SyntaxSet::load_defaults_newlines);
pub(super) static THEME_SET: LazyLock<two_face::theme::EmbeddedLazyThemeSet> =
    LazyLock::new(two_face::theme::extra);

struct DiffLayout {
    indent: &'static str,
    gutter_width: usize,
    max_content: usize,
}

pub(super) fn render_code_block(
    out: &mut RenderOut,
    lines: &[&str],
    lang: &str,
    width: usize,
    dim: bool,
) -> u16 {
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
    if dim {
        let _ = out.queue(SetAttribute(Attribute::Dim));
    }
    let theme = &THEME_SET[two_face::theme::EmbeddedThemeName::MonokaiExtended];
    let text_w = width.saturating_sub(1).max(1);
    let expanded: Vec<String> = lines.iter().map(|l| l.replace('\t', "    ")).collect();
    let max_len = expanded
        .iter()
        .map(|l| l.chars().count())
        .max()
        .unwrap_or(0);
    let wraps = max_len > text_w;
    let block_w = if wraps { text_w + 1 } else { max_len + 1 };
    let mut rows = 0u16;
    let mut h = HighlightLines::new(syntax, theme);
    // Top dashed border
    let _ = out.queue(SetForegroundColor(theme::BAR));
    let _ = out.queue(Print("\u{254c}".repeat(block_w)));
    let _ = out.queue(ResetColor);
    crlf(out);
    rows += 1;
    for line in &expanded {
        let line_with_nl = format!("{}\n", line);
        let regions = h
            .highlight_line(&line_with_nl, &SYNTAX_SET)
            .unwrap_or_default();
        let visual_rows = split_regions_into_rows(&regions, text_w);
        for vrow in &visual_rows {
            print_split_regions(out, vrow, None);
            crlf(out);
        }
        rows += visual_rows.len() as u16;
    }
    // Bottom dashed border
    let _ = out.queue(SetForegroundColor(theme::BAR));
    let _ = out.queue(Print("\u{254c}".repeat(block_w)));
    let _ = out.queue(ResetColor);
    crlf(out);
    rows += 1;
    if dim {
        let _ = out.queue(SetAttribute(Attribute::NormalIntensity));
    }
    rows
}

pub(super) fn render_highlighted(
    out: &mut RenderOut,
    lines: &[&str],
    syntax: &syntect::parsing::SyntaxReference,
    skip: u16,
    max_rows: u16,
) -> u16 {
    let indent = "   ";
    let theme = &THEME_SET[two_face::theme::EmbeddedThemeName::MonokaiExtended];
    let gutter_width = format!("{}", lines.len()).len().max(2);
    let prefix_len = indent.len() + 1 + gutter_width + 3;
    let max_content = term_width().saturating_sub(prefix_len + 1);
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
        let visual_rows = split_regions_into_rows(&regions, max_content);
        for (vi, vrow) in visual_rows.iter().enumerate() {
            if total_rows >= skip && emitted < emit_limit {
                let _ = out.queue(Print(indent));
                if vi == 0 {
                    let _ = out.queue(SetForegroundColor(Color::DarkGrey));
                    let _ = out.queue(Print(format!(" {:>w$}", i + 1, w = gutter_width)));
                    let _ = out.queue(ResetColor);
                    let _ = out.queue(Print("   "));
                } else {
                    let _ = out.queue(Print(&blank_gutter));
                }
                print_split_regions(out, vrow, None);
                crlf(out);
                emitted += 1;
            }
            total_rows += 1;
        }
    }
    emitted
}

pub(super) fn print_syntax_file(
    out: &mut RenderOut,
    content: &str,
    path: &str,
    skip: u16,
    max_rows: u16,
) -> u16 {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("txt");
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
    changes: Vec<DiffChange>,
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
            .map(|pos| file_content[..pos].lines().count())
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
    for c in &changes {
        match c.tag {
            ChangeTag::Equal => {
                new_line += 1;
            }
            ChangeTag::Delete => {
                if first_mod.is_none() {
                    first_mod = Some(new_line);
                }
                last_mod = Some(new_line);
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

    DiffViewData {
        file_content,
        start_line,
        first_mod,
        view_start,
        view_end,
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
pub(super) fn print_inline_diff(
    out: &mut RenderOut,
    old: &str,
    new: &str,
    path: &str,
    anchor: &str,
    skip: u16,
    max_rows: u16,
) -> u16 {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("txt");
    let syntax = SYNTAX_SET
        .find_syntax_by_extension(ext)
        .unwrap_or_else(|| SYNTAX_SET.find_syntax_plain_text());
    let theme = &THEME_SET[two_face::theme::EmbeddedThemeName::MonokaiExtended];

    let indent = "   ";
    let dv = compute_diff_view(old, new, path, anchor);
    let expanded_lines: Vec<String> = dv
        .file_content
        .lines()
        .map(|l| l.replace('\t', "    "))
        .collect();
    let file_lines: Vec<&str> = expanded_lines.iter().map(|s| s.as_str()).collect();
    let changes = &dv.changes;

    let max_lineno = dv.view_end;
    let gutter_width = format!("{}", max_lineno).len().max(2);
    let prefix_len = indent.len() + 1 + gutter_width + 3;
    let right_margin = indent.len();
    let max_content = term_width().saturating_sub(prefix_len + right_margin);

    let bg_del = Color::Rgb {
        r: 60,
        g: 20,
        b: 20,
    };
    let bg_add = Color::Rgb {
        r: 20,
        g: 50,
        b: 20,
    };

    let layout = DiffLayout {
        indent,
        gutter_width,
        max_content,
    };
    let emit_limit = if max_rows == 0 { u16::MAX } else { max_rows };

    let mut h_old = HighlightLines::new(syntax, theme);
    let mut h_new = HighlightLines::new(syntax, theme);
    for i in 0..dv.view_start {
        if i < file_lines.len() {
            let line = format!("{}\n", file_lines[i]);
            let _ = h_old.highlight_line(&line, &SYNTAX_SET);
            let _ = h_new.highlight_line(&line, &SYNTAX_SET);
        }
    }

    let mut total: u16 = 0;
    let mut emitted: u16 = 0;

    let ctx_before_end = dv.start_line.min(dv.first_mod);
    let ctx_before_start = dv.view_start.min(ctx_before_end);
    let before_rows = print_diff_lines_skip(
        out,
        &mut h_new,
        &file_lines[ctx_before_start..ctx_before_end],
        ctx_before_start,
        None,
        None,
        &layout,
        skip,
        emit_limit,
        total,
    );
    let count_before = (ctx_before_end - ctx_before_start) as u16;
    emitted += before_rows;
    total += count_before;
    for line in &file_lines[ctx_before_start..ctx_before_end] {
        let _ = h_old.highlight_line(&format!("{}\n", line), &SYNTAX_SET);
    }

    if emitted >= emit_limit {
        return emitted;
    }

    let ctx = 3usize;
    let visible = compute_change_visibility(changes, ctx);
    let mut old_lineno = dv.start_line;
    let mut new_lineno = dv.start_line;
    let mut pending_ellipsis = false;
    let mut emitted_any = total > 0;
    for (ci, change) in changes.iter().enumerate() {
        if emitted >= emit_limit {
            break;
        }
        let raw = change.value.trim_end_matches('\n').replace('\t', "    ");
        let text = raw.as_str();
        match change.tag {
            ChangeTag::Equal => {
                if visible[ci] {
                    if pending_ellipsis {
                        pending_ellipsis = false;
                        if total >= skip && emitted < emit_limit {
                            let _ = out.queue(Print(indent));
                            let _ = out.queue(SetForegroundColor(Color::DarkGrey));
                            let _ =
                                out.queue(Print(format!("{:>w$}", "...", w = 1 + gutter_width)));
                            let _ = out.queue(ResetColor);
                            crlf(out);
                            emitted += 1;
                        }
                        total += 1;
                    }
                    if new_lineno >= dv.view_start && new_lineno < dv.view_end {
                        if total >= skip && emitted < emit_limit {
                            print_diff_lines(
                                out,
                                &mut h_new,
                                &[text],
                                new_lineno,
                                None,
                                None,
                                &layout,
                            );
                            emitted += 1;
                        } else {
                            // Advance highlighter without emitting
                            let _ = h_new.highlight_line(&format!("{}\n", text), &SYNTAX_SET);
                        }
                        total += 1;
                        emitted_any = true;
                    }
                } else if emitted_any {
                    pending_ellipsis = true;
                }
                let _ = h_old.highlight_line(&format!("{}\n", text), &SYNTAX_SET);
                if !visible[ci] {
                    let _ = h_new.highlight_line(&format!("{}\n", text), &SYNTAX_SET);
                }
                new_lineno += 1;
            }
            ChangeTag::Delete => {
                if pending_ellipsis {
                    pending_ellipsis = false;
                    if total >= skip && emitted < emit_limit {
                        let _ = out.queue(Print(indent));
                        let _ = out.queue(SetForegroundColor(Color::DarkGrey));
                        let _ = out.queue(Print(format!("{:>w$}", "...", w = 1 + gutter_width)));
                        let _ = out.queue(ResetColor);
                        crlf(out);
                        emitted += 1;
                    }
                    total += 1;
                }
                if total >= skip && emitted < emit_limit {
                    print_diff_lines(
                        out,
                        &mut h_old,
                        &[text],
                        old_lineno,
                        Some(('-', Color::Red)),
                        Some(bg_del),
                        &layout,
                    );
                    emitted += 1;
                } else {
                    let _ = h_old.highlight_line(&format!("{}\n", text), &SYNTAX_SET);
                }
                old_lineno += 1;
                total += 1;
            }
            ChangeTag::Insert => {
                if pending_ellipsis {
                    pending_ellipsis = false;
                    if total >= skip && emitted < emit_limit {
                        let _ = out.queue(Print(indent));
                        let _ = out.queue(SetForegroundColor(Color::DarkGrey));
                        let _ = out.queue(Print(format!("{:>w$}", "...", w = 1 + gutter_width)));
                        let _ = out.queue(ResetColor);
                        crlf(out);
                        emitted += 1;
                    }
                    total += 1;
                }
                if total >= skip && emitted < emit_limit {
                    print_diff_lines(
                        out,
                        &mut h_new,
                        &[text],
                        new_lineno,
                        Some(('+', Color::Green)),
                        Some(bg_add),
                        &layout,
                    );
                    emitted += 1;
                } else {
                    let _ = h_new.highlight_line(&format!("{}\n", text), &SYNTAX_SET);
                }
                new_lineno += 1;
                total += 1;
            }
        }
    }

    if emitted >= emit_limit {
        return emitted;
    }

    let anchor_lines = anchor.lines().count();
    let after_start = dv.start_line + anchor_lines;
    let after_end = dv.view_end.min(file_lines.len());
    if after_start < after_end {
        let ctx_slice = &file_lines[after_start..after_end];
        emitted += print_diff_lines_skip(
            out,
            &mut h_new,
            ctx_slice,
            after_start,
            None,
            None,
            &layout,
            skip,
            emit_limit - emitted,
            total,
        );
    }
    emitted
}

/// Count rows an inline diff would take without rendering.
pub(super) fn count_inline_diff_rows(old: &str, new: &str, path: &str, anchor: &str) -> u16 {
    let dv = compute_diff_view(old, new, path, anchor);

    let indent = "   ";
    let max_lineno = dv.view_end;
    let gutter_width = format!("{}", max_lineno).len().max(2);
    let prefix_len = indent.len() + 1 + gutter_width + 3;
    let right_margin = indent.len();
    let max_content = term_width().saturating_sub(prefix_len + right_margin);

    let expanded_lines: Vec<String> = dv
        .file_content
        .lines()
        .map(|l| l.replace('\t', "    "))
        .collect();
    let file_lines: Vec<&str> = expanded_lines.iter().map(|s| s.as_str()).collect();

    let visual_rows_for = |line: &str| -> usize {
        let chars = line.replace('\t', "    ").chars().count();
        if max_content == 0 {
            1
        } else {
            chars.div_ceil(max_content)
        }
        .max(1)
    };

    let ctx_before_end = dv.start_line.min(dv.first_mod);
    let ctx_before_start = dv.view_start.min(ctx_before_end);
    let mut rows: usize = 0;
    for i in ctx_before_start..ctx_before_end {
        if i < file_lines.len() {
            rows += visual_rows_for(file_lines[i]);
        }
    }

    let ctx = 3usize;
    let visible = compute_change_visibility(&dv.changes, ctx);
    let mut new_lineno = dv.start_line;
    let mut pending_ellipsis = false;
    let mut emitted_any = rows > 0;
    for (ci, change) in dv.changes.iter().enumerate() {
        let line = change.value.trim_end_matches('\n');
        match change.tag {
            ChangeTag::Equal => {
                if visible[ci] {
                    if pending_ellipsis {
                        pending_ellipsis = false;
                        rows += 1; // the "..." line
                    }
                    if new_lineno >= dv.view_start && new_lineno < dv.view_end {
                        rows += visual_rows_for(line);
                        emitted_any = true;
                    }
                } else if emitted_any {
                    pending_ellipsis = true;
                }
                new_lineno += 1;
            }
            ChangeTag::Delete => {
                if pending_ellipsis {
                    pending_ellipsis = false;
                    rows += 1;
                }
                rows += visual_rows_for(line);
            }
            ChangeTag::Insert => {
                if pending_ellipsis {
                    pending_ellipsis = false;
                    rows += 1;
                }
                rows += visual_rows_for(line);
                new_lineno += 1;
            }
        }
    }

    let anchor_lines = anchor.lines().count();
    let after_start = dv.start_line + anchor_lines;
    let after_end = dv.view_end.min(file_lines.len());
    for line in file_lines.iter().take(after_end).skip(after_start) {
        rows += visual_rows_for(line);
    }
    rows as u16
}

fn print_diff_lines(
    out: &mut RenderOut,
    h: &mut HighlightLines,
    lines: &[&str],
    start_line: usize,
    sign: Option<(char, Color)>,
    bg: Option<Color>,
    layout: &DiffLayout,
) -> u16 {
    let DiffLayout {
        indent,
        gutter_width,
        max_content,
    } = *layout;
    let prefix_cols = indent.len() + 1 + gutter_width + 3;
    let right_margin = indent.len();
    let blank_gutter = " ".repeat(1 + gutter_width + 3);
    let mut total_rows = 0u16;
    for (i, line) in lines.iter().enumerate() {
        let lineno = start_line + i + 1;
        let line_with_nl = format!("{}\n", line);
        let regions = h
            .highlight_line(&line_with_nl, &SYNTAX_SET)
            .unwrap_or_default();
        let visual_rows = split_regions_into_rows(&regions, max_content);
        for (vi, vrow) in visual_rows.iter().enumerate() {
            let _ = out.queue(Print(indent));
            if let Some((ch, color)) = sign {
                let _ = out.queue(SetBackgroundColor(bg.unwrap()));
                if vi == 0 {
                    let _ = out.queue(SetForegroundColor(Color::DarkGrey));
                    let _ = out.queue(Print(format!(" {:>w$} ", lineno, w = gutter_width)));
                    let _ = out.queue(SetForegroundColor(color));
                    let _ = out.queue(Print(format!("{} ", ch)));
                } else {
                    let _ = out.queue(Print(&blank_gutter));
                }
                let content_cols = print_split_regions(out, vrow, bg);
                let pad = term_width().saturating_sub(prefix_cols + content_cols + right_margin);
                if pad > 0 {
                    if let Some(bg_color) = bg {
                        let _ = out.queue(SetBackgroundColor(bg_color));
                    }
                    let _ = out.queue(Print(" ".repeat(pad)));
                }
                let _ = out.queue(ResetColor);
            } else {
                if vi == 0 {
                    let _ = out.queue(SetForegroundColor(Color::DarkGrey));
                    let _ = out.queue(Print(format!(" {:>w$}", lineno, w = gutter_width)));
                    let _ = out.queue(ResetColor);
                    let _ = out.queue(Print("   "));
                } else {
                    let _ = out.queue(Print(&blank_gutter));
                }
                print_split_regions(out, vrow, None);
            }
            crlf(out);
        }
        total_rows += visual_rows.len() as u16;
    }
    total_rows
}

/// Like `print_diff_lines` but respects a global skip offset and emit limit.
/// `global_total` is the row counter before this call; rows with index < `skip`
/// are suppressed. Returns the number of rows actually emitted.
#[allow(clippy::too_many_arguments)]
fn print_diff_lines_skip(
    out: &mut RenderOut,
    h: &mut HighlightLines,
    lines: &[&str],
    start_line: usize,
    sign: Option<(char, Color)>,
    bg: Option<Color>,
    layout: &DiffLayout,
    skip: u16,
    emit_limit: u16,
    global_total: u16,
) -> u16 {
    let DiffLayout {
        indent,
        gutter_width,
        max_content,
    } = *layout;
    let prefix_cols = indent.len() + 1 + gutter_width + 3;
    let right_margin = indent.len();
    let blank_gutter = " ".repeat(1 + gutter_width + 3);
    let mut row_idx = global_total;
    let mut emitted = 0u16;
    for (i, line) in lines.iter().enumerate() {
        if emitted >= emit_limit {
            // Still advance highlighter for remaining lines
            let _ = h.highlight_line(&format!("{}\n", line), &SYNTAX_SET);
            continue;
        }
        let lineno = start_line + i + 1;
        let line_with_nl = format!("{}\n", line);
        let regions = h
            .highlight_line(&line_with_nl, &SYNTAX_SET)
            .unwrap_or_default();
        let visual_rows = split_regions_into_rows(&regions, max_content);
        for (vi, vrow) in visual_rows.iter().enumerate() {
            if row_idx >= skip && emitted < emit_limit {
                let _ = out.queue(Print(indent));
                if let Some((ch, color)) = sign {
                    let _ = out.queue(SetBackgroundColor(bg.unwrap()));
                    if vi == 0 {
                        let _ = out.queue(SetForegroundColor(Color::DarkGrey));
                        let _ = out.queue(Print(format!(" {:>w$} ", lineno, w = gutter_width)));
                        let _ = out.queue(SetForegroundColor(color));
                        let _ = out.queue(Print(format!("{} ", ch)));
                    } else {
                        let _ = out.queue(Print(&blank_gutter));
                    }
                    let content_cols = print_split_regions(out, vrow, bg);
                    let pad =
                        term_width().saturating_sub(prefix_cols + content_cols + right_margin);
                    if pad > 0 {
                        if let Some(bg_color) = bg {
                            let _ = out.queue(SetBackgroundColor(bg_color));
                        }
                        let _ = out.queue(Print(" ".repeat(pad)));
                    }
                    let _ = out.queue(ResetColor);
                } else {
                    if vi == 0 {
                        let _ = out.queue(SetForegroundColor(Color::DarkGrey));
                        let _ = out.queue(Print(format!(" {:>w$}", lineno, w = gutter_width)));
                        let _ = out.queue(ResetColor);
                        let _ = out.queue(Print("   "));
                    } else {
                        let _ = out.queue(Print(&blank_gutter));
                    }
                    print_split_regions(out, vrow, None);
                }
                crlf(out);
                emitted += 1;
            }
            row_idx += 1;
        }
    }
    emitted
}

/// Split syntax regions into visual rows that each fit within `max_width` columns.
fn split_regions_into_rows(
    regions: &[(Style, &str)],
    max_width: usize,
) -> Vec<Vec<(Style, String)>> {
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
    rows
}

/// Print pre-split owned regions. Returns columns printed.
fn print_split_regions(
    out: &mut RenderOut,
    regions: &[(Style, String)],
    bg: Option<Color>,
) -> usize {
    let mut col = 0;
    for (style, text) in regions {
        if text.is_empty() {
            continue;
        }
        if let Some(bg_color) = bg {
            let _ = out.queue(SetBackgroundColor(bg_color));
        }
        let fg = Color::Rgb {
            r: style.foreground.r,
            g: style.foreground.g,
            b: style.foreground.b,
        };
        let _ = out.queue(SetForegroundColor(fg));
        let _ = out.queue(Print(text));
        col += text.chars().count();
    }
    let _ = out.queue(ResetColor);
    col
}

pub(super) fn render_markdown_table(out: &mut RenderOut, lines: &[&str], dim: bool) -> u16 {
    let mut rows: Vec<Vec<String>> = Vec::new();
    for line in lines {
        let trimmed = line.trim().trim_start_matches('|').trim_end_matches('|');
        if trimmed
            .chars()
            .all(|c| c == '-' || c == '|' || c == ':' || c == ' ')
        {
            continue;
        }
        let cells: Vec<String> = trimmed.split('|').map(|c| c.trim().to_string()).collect();
        rows.push(cells);
    }

    if rows.is_empty() {
        return 0;
    }

    let mut table = Table::new();
    table
        .load_preset(UTF8_BORDERS_ONLY)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_width((term_width().saturating_sub(2)) as u16);

    if let Some(header) = rows.first() {
        table.set_header(header);
    }
    for row in rows.iter().skip(1) {
        table.add_row(row);
    }

    let rendered = table.to_string();
    for line in rendered.lines() {
        let _ = out.queue(Print(" "));
        // Split the line into border and content segments, render content with
        // inline markdown styling (bold, italic, inline code).
        let mut seg = String::new();
        let mut in_border = false;
        for ch in line.chars() {
            let is_border =
                ('\u{2500}'..='\u{257F}').contains(&ch) || ('\u{2580}'..='\u{259F}').contains(&ch);
            if is_border {
                if !seg.is_empty() {
                    let pad = seg.len() - strip_markdown_markers(&seg).len();
                    print_styled_dim(out, &seg, dim);
                    if pad > 0 {
                        let _ = out.queue(Print(" ".repeat(pad)));
                    }
                    seg.clear();
                }
                if !in_border {
                    let _ = out.queue(SetForegroundColor(theme::BAR));
                    if dim {
                        let _ = out.queue(SetAttribute(Attribute::Dim));
                    }
                    in_border = true;
                }
                let _ = out.queue(Print(ch.to_string()));
            } else {
                if in_border {
                    let _ = out.queue(ResetColor);
                    if dim {
                        let _ = out.queue(SetAttribute(Attribute::Reset));
                    }
                    in_border = false;
                }
                seg.push(ch);
            }
        }
        if !seg.is_empty() {
            let pad = seg.len() - strip_markdown_markers(&seg).len();
            print_styled_dim(out, &seg, dim);
            if pad > 0 {
                let _ = out.queue(Print(" ".repeat(pad)));
            }
        }
        if in_border {
            let _ = out.queue(ResetColor);
        }
        crlf(out);
    }
    rendered.lines().count() as u16
}

fn strip_markdown_markers(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut out = String::with_capacity(len);
    let mut i = 0;
    while i < len {
        if i + 1 < len && chars[i] == '*' && chars[i + 1] == '*' {
            let mut j = i + 2;
            while j + 1 < len && !(chars[j] == '*' && chars[j + 1] == '*') {
                j += 1;
            }
            if j + 1 < len {
                out.extend(&chars[i + 2..j]);
                i = j + 2;
                continue;
            }
        }
        if chars[i] == '*' && i + 1 < len && chars[i + 1] != '*' {
            let mut j = i + 1;
            while j < len && chars[j] != '*' {
                j += 1;
            }
            if j < len {
                out.extend(&chars[i + 1..j]);
                i = j + 1;
                continue;
            }
        }
        if chars[i] == '`' {
            let mut j = i + 1;
            while j < len && chars[j] != '`' {
                j += 1;
            }
            if j < len {
                out.extend(&chars[i + 1..j]);
                i = j + 1;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}
