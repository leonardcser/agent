use crate::theme;
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
use unicode_width::UnicodeWidthStr;

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

pub(crate) fn render_code_block(
    out: &mut RenderOut,
    lines: &[&str],
    lang: &str,
    width: usize,
    dim: bool,
    bctx: Option<&super::BoxContext>,
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
    let theme = &THEME_SET[two_face::theme::EmbeddedThemeName::MonokaiExtended];
    let content_width = if let Some(b) = bctx { b.inner_w } else { width };
    let text_w = content_width.saturating_sub(1).max(1);
    let expanded: Vec<String> = lines.iter().map(|l| l.replace('\t', "    ")).collect();
    let mut rows = 0u16;
    let mut h = HighlightLines::new(syntax, theme);

    if dim {
        let _ = out.queue(SetAttribute(Attribute::Dim));
    }

    for line in &expanded {
        let line_with_nl = format!("{}\n", line);
        let regions = h
            .highlight_line(&line_with_nl, &SYNTAX_SET)
            .unwrap_or_default();
        let visual_rows = split_regions_into_rows(&regions, text_w);
        for vrow in &visual_rows {
            if let Some(b) = bctx {
                b.print_left(out);
            }
            let cols = print_split_regions(out, vrow, Some(theme::CODE_BLOCK_BG));
            let pad = text_w.saturating_sub(cols);
            if pad > 0 {
                let _ = out.queue(SetBackgroundColor(theme::CODE_BLOCK_BG));
                let _ = out.queue(Print(" ".repeat(pad)));
            }
            let _ = out.queue(ResetColor);
            if let Some(b) = bctx {
                b.print_right(out, text_w);
            }
            crlf(out);
        }
        rows += visual_rows.len() as u16;
    }

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
    let gutter_width = format!("{}", lines.len()).len();
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
    max_display_lineno: usize,
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

    // Compute the leading whitespace the old/new strings are missing compared
    // to the actual file indentation so we can re-add it to diff lines.
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

    let max_lineno = dv.max_display_lineno;
    let gutter_width = format!("{}", max_lineno).len();
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
        let raw = format!(
            "{}{}",
            extra_indent,
            change.value.trim_end_matches('\n').replace('\t', "    ")
        );
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
                old_lineno += 1;
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
    let max_lineno = dv.max_display_lineno;
    let gutter_width = format!("{}", max_lineno).len();
    let prefix_len = indent.len() + 1 + gutter_width + 3;
    let right_margin = indent.len();
    let max_content = term_width().saturating_sub(prefix_len + right_margin);

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
    let file_indent_n = file_lines
        .get(dv.start_line)
        .unwrap_or(&"")
        .chars()
        .take_while(|c| c.is_whitespace())
        .count();
    let extra_indent_n = file_indent_n.saturating_sub(lookup_indent);

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
        let padded = format!(
            "{}{}",
            " ".repeat(extra_indent_n),
            change.value.trim_end_matches('\n')
        );
        let line = padded.as_str();
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
                    let _ = out.queue(SetForegroundColor(color));
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
                        let _ = out.queue(SetForegroundColor(color));
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

/// Stateful bash/shell syntax highlighter that preserves state across lines.
pub(crate) struct BashHighlighter<'a> {
    h: HighlightLines<'a>,
}

impl<'a> BashHighlighter<'a> {
    pub fn new() -> Self {
        let syntax = SYNTAX_SET
            .find_syntax_by_extension("sh")
            .unwrap_or_else(|| SYNTAX_SET.find_syntax_plain_text());
        let theme = &THEME_SET[two_face::theme::EmbeddedThemeName::MonokaiExtended];
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
    /// Does not emit `crlf` — the caller controls line breaks.
    pub fn print_line(&mut self, out: &mut RenderOut, line: &str) {
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
            let fg = Color::Rgb {
                r: style.foreground.r,
                g: style.foreground.g,
                b: style.foreground.b,
            };
            let _ = out.queue(SetForegroundColor(fg));
            let _ = out.queue(Print(text));
        }
        let _ = out.queue(ResetColor);
    }
}

/// Print a single line of bash/shell code with syntax highlighting.
/// Does not emit `crlf` — the caller controls line breaks.
pub(crate) fn print_highlighted_bash_line(out: &mut RenderOut, line: &str) {
    BashHighlighter::new().print_line(out, line);
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

/// Strip inline markdown markers (`**`, `*`, `__`, `_`, `` ` ``, `~~`) and
/// return the visible text content. Used for measuring visual width.
pub(crate) fn strip_markdown_markers(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut out = String::with_capacity(len);
    let mut i = 0;
    while i < len {
        if let Some(skip) = skip_inline_span(&chars, i) {
            // Append the content between the opening and closing delimiters.
            let (content_start, content_end, after) = skip;
            out.extend(&chars[content_start..content_end]);
            i = after;
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
        if let Some(skip) = skip_inline_span(&chars, i) {
            // Jump past the entire span (delimiters + content) — no breaks inside.
            i = skip.2;
            continue;
        }
        if chars[i] == ' ' {
            breakable[i] = true;
        }
        i += 1;
    }
    breakable
}

/// Try to match an inline markdown span starting at position `i`.
/// Returns `Some((content_start, content_end, after))` if a complete span is
/// found, where `content_start..content_end` is the inner text and `after` is
/// the index past the closing delimiter. Returns `None` if no span matches.
fn skip_inline_span(chars: &[char], i: usize) -> Option<(usize, usize, usize)> {
    let len = chars.len();

    // ~~strikethrough~~
    if i + 1 < len && chars[i] == '~' && chars[i + 1] == '~' {
        if let Some(end) = find_closing_pair(chars, i + 2, '~', '~') {
            return Some((i + 2, end, end + 2));
        }
    }

    // ***bold+italic*** or ___bold+italic___
    if i + 2 < len
        && chars[i] == chars[i + 1]
        && chars[i] == chars[i + 2]
        && (chars[i] == '*' || chars[i] == '_')
    {
        if let Some(end) = find_closing_triple(chars, i + 3, chars[i]) {
            return Some((i + 3, end, end + 3));
        }
    }

    // **bold** or __bold__
    if i + 1 < len
        && chars[i] == chars[i + 1]
        && (chars[i] == '*' || chars[i] == '_')
        && i + 2 < len
        && !chars[i + 2].is_whitespace()
    {
        if let Some(end) = find_closing_pair(chars, i + 2, chars[i], chars[i]) {
            if !chars[end - 1].is_whitespace() {
                return Some((i + 2, end, end + 2));
            }
        }
    }

    // *italic* or _italic_
    if (chars[i] == '*' || chars[i] == '_')
        && i + 1 < len
        && chars[i + 1] != chars[i]
        && !chars[i + 1].is_whitespace()
    {
        if let Some(end) = find_closing_single(chars, i + 1, chars[i]) {
            if !chars[end - 1].is_whitespace() {
                return Some((i + 1, end, end + 1));
            }
        }
    }

    // `code`
    if chars[i] == '`' {
        if let Some(end) = find_closing_single(chars, i + 1, '`') {
            return Some((i + 1, end, end + 1));
        }
    }

    None
}

pub(crate) fn render_markdown_table(
    out: &mut RenderOut,
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

    let bar = |out: &mut RenderOut, dim: bool| {
        let _ = out.queue(SetForegroundColor(theme::BAR));
        if dim {
            let _ = out.queue(SetAttribute(Attribute::Dim));
        }
    };
    let reset = |out: &mut RenderOut, dim: bool| {
        let _ = out.queue(ResetColor);
        if dim {
            let _ = out.queue(SetAttribute(Attribute::Reset));
        }
    };

    let render_table_row =
        |out: &mut RenderOut, row: &[String], widths: &[usize], dim: bool| -> u16 {
            let wrapped: Vec<Vec<String>> = row
                .iter()
                .enumerate()
                .map(|(c, cell)| {
                    let w = widths.get(c).copied().unwrap_or(0);
                    wrap_cell_words(cell, w)
                })
                .collect();
            let height = wrapped.iter().map(|w| w.len()).max().unwrap_or(1);

            for vline in 0..height {
                if let Some(b) = bctx {
                    b.print_left(out);
                } else if !indent.is_empty() {
                    let _ = out.queue(Print(indent));
                }
                bar(out, dim);
                let _ = out.queue(Print("┃"));
                reset(out, dim);
                let mut line_cols = 1; // "┃"
                for (c, width) in widths.iter().enumerate() {
                    let text = wrapped
                        .get(c)
                        .and_then(|w| w.get(vline))
                        .map(|s| s.as_str())
                        .unwrap_or("");
                    let visual_len = strip_markdown_markers(text).width();
                    let _ = out.queue(Print(" "));
                    print_inline_styled(out, text, dim);
                    let pad = width.saturating_sub(visual_len);
                    if pad > 0 {
                        let _ = out.queue(Print(" ".repeat(pad)));
                    }
                    let _ = out.queue(Print(" "));
                    bar(out, dim);
                    let _ = out.queue(Print("┃"));
                    reset(out, dim);
                    line_cols += width + 3; // " content pad ┃"
                }
                if let Some(b) = bctx {
                    b.print_right(out, line_cols);
                }
                crlf(out);
            }
            height as u16
        };

    // left, horizontal, junction, right
    let render_border =
        |out: &mut RenderOut, widths: &[usize], dim: bool, l: &str, j: &str, r: &str| -> u16 {
            if let Some(b) = bctx {
                b.print_left(out);
            } else if !indent.is_empty() {
                let _ = out.queue(Print(indent));
            }
            bar(out, dim);
            let _ = out.queue(Print(l));
            let mut line_cols = 1; // "l"
            for (c, width) in widths.iter().enumerate() {
                let seg = width + 2;
                let _ = out.queue(Print("━".repeat(seg)));
                line_cols += seg;
                if c + 1 < widths.len() {
                    let _ = out.queue(Print(j));
                    line_cols += 1;
                }
            }
            let _ = out.queue(Print(r));
            line_cols += 1;
            reset(out, dim);
            if let Some(b) = bctx {
                b.print_right(out, line_cols);
            }
            crlf(out);
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
fn render_table_stacked(out: &mut RenderOut, rows: &[Vec<String>], dim: bool) -> u16 {
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
            crlf(out);
            total_rows += 1;
        }
        for (c, cell) in row.iter().enumerate() {
            let label = header.get(c).map(|s| s.as_str()).unwrap_or("");
            let label_visual = strip_markdown_markers(label).width();
            let pad = label_width.saturating_sub(label_visual);

            let wrapped = wrap_cell_words(cell, value_width);
            for (li, line) in wrapped.iter().enumerate() {
                if li == 0 {
                    let _ = out.queue(Print("  "));
                    let _ = out.queue(SetForegroundColor(Color::DarkGrey));
                    if dim {
                        let _ = out.queue(SetAttribute(Attribute::Dim));
                    }
                    print_inline_styled(out, label, dim);
                    if pad > 0 {
                        let _ = out.queue(Print(" ".repeat(pad)));
                    }
                    let _ = out.queue(ResetColor);
                    if dim {
                        let _ = out.queue(SetAttribute(Attribute::Reset));
                    }
                    let _ = out.queue(Print("  "));
                } else {
                    let _ = out.queue(Print(" ".repeat(value_indent)));
                }
                print_inline_styled(out, line, dim);
                crlf(out);
                total_rows += 1;
            }
        }
    }
    total_rows
}

/// Word-wrap cell text so each line's visual width (after stripping markers) fits within `max_width`.
/// Only breaks at spaces that are outside inline markdown spans.
fn wrap_cell_words(text: &str, max_width: usize) -> Vec<String> {
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
pub(crate) fn print_inline_styled(out: &mut super::RenderOut, text: &str, dim: bool) {
    macro_rules! reset {
        () => {
            let _ = out.queue(SetAttribute(Attribute::Reset));
            let _ = out.queue(ResetColor);
            if dim {
                let _ = out.queue(SetAttribute(Attribute::Dim));
            }
        };
    }

    if dim {
        let _ = out.queue(SetAttribute(Attribute::Dim));
    }

    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut i = 0;
    let mut plain = String::new();

    macro_rules! flush_plain {
        () => {
            if !plain.is_empty() {
                let _ = out.queue(Print(&plain));
                plain.clear();
            }
        };
    }

    while i < len {
        // ~~strikethrough~~
        if i + 1 < len && chars[i] == '~' && chars[i + 1] == '~' {
            if let Some(end) = find_closing_pair(&chars, i + 2, '~', '~') {
                flush_plain!();
                let word: String = chars[i + 2..end].iter().collect();
                let _ = out.queue(SetAttribute(Attribute::CrossedOut));
                let _ = out.queue(Print(&word));
                reset!();
                i = end + 2;
                continue;
            }
        }

        // ***bold+italic*** or ___bold+italic___
        if i + 2 < len
            && chars[i] == chars[i + 1]
            && chars[i] == chars[i + 2]
            && (chars[i] == '*' || chars[i] == '_')
        {
            let marker = chars[i];
            if let Some(end) = find_closing_triple(&chars, i + 3, marker) {
                flush_plain!();
                let word: String = chars[i + 3..end].iter().collect();
                let _ = out.queue(SetAttribute(Attribute::Bold));
                let _ = out.queue(SetAttribute(Attribute::Italic));
                let _ = out.queue(Print(&word));
                reset!();
                i = end + 3;
                continue;
            }
        }

        // **bold** or __bold__
        if i + 1 < len
            && chars[i] == chars[i + 1]
            && (chars[i] == '*' || chars[i] == '_')
            && i + 2 < len
            && !chars[i + 2].is_whitespace()
        {
            let marker = chars[i];
            if let Some(end) = find_closing_pair(&chars, i + 2, marker, marker) {
                if !chars[end - 1].is_whitespace() {
                    flush_plain!();
                    let word: String = chars[i + 2..end].iter().collect();
                    let _ = out.queue(SetAttribute(Attribute::Bold));
                    let _ = out.queue(Print(&word));
                    reset!();
                    i = end + 2;
                    continue;
                }
            }
        }

        // *italic* or _italic_
        if (chars[i] == '*' || chars[i] == '_')
            && i + 1 < len
            && chars[i + 1] != chars[i]
            && !chars[i + 1].is_whitespace()
        {
            let marker = chars[i];
            if let Some(end) = find_closing_single(&chars, i + 1, marker) {
                if !chars[end - 1].is_whitespace() {
                    flush_plain!();
                    let word: String = chars[i + 1..end].iter().collect();
                    let _ = out.queue(SetAttribute(Attribute::Italic));
                    let _ = out.queue(Print(&word));
                    reset!();
                    i = end + 1;
                    continue;
                }
            }
        }

        // `code`
        if chars[i] == '`' {
            if let Some(end) = find_closing_single(&chars, i + 1, '`') {
                flush_plain!();
                let word: String = chars[i + 1..end].iter().collect();
                let _ = out.queue(SetForegroundColor(theme::accent()));
                if dim {
                    let _ = out.queue(SetAttribute(Attribute::Dim));
                }
                let _ = out.queue(Print(&word));
                reset!();
                i = end + 1;
                continue;
            }
        }

        plain.push(chars[i]);
        i += 1;
    }
    flush_plain!();

    if dim {
        let _ = out.queue(SetAttribute(Attribute::Reset));
        let _ = out.queue(ResetColor);
    }
}

/// Find position of a single closing `marker` starting from `start`.
fn find_closing_single(chars: &[char], start: usize, marker: char) -> Option<usize> {
    let mut j = start;
    while j < chars.len() {
        if chars[j] == marker {
            return Some(j);
        }
        j += 1;
    }
    None
}

/// Find position of a closing double-marker (`m1 m2`) starting from `start`.
fn find_closing_pair(chars: &[char], start: usize, m1: char, m2: char) -> Option<usize> {
    let mut j = start;
    while j + 1 < chars.len() {
        if chars[j] == m1 && chars[j + 1] == m2 {
            return Some(j);
        }
        j += 1;
    }
    None
}

/// Find position of a closing triple-marker starting from `start`.
fn find_closing_triple(chars: &[char], start: usize, marker: char) -> Option<usize> {
    let mut j = start;
    while j + 2 < chars.len() {
        if chars[j] == marker && chars[j + 1] == marker && chars[j + 2] == marker {
            return Some(j);
        }
        j += 1;
    }
    None
}
