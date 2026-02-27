use crate::theme;
use comfy_table::{presets::UTF8_BORDERS_ONLY, ContentArrangement, Table};
use crossterm::{
    style::{Color, Print, ResetColor, SetAttribute, SetForegroundColor, SetBackgroundColor, Attribute},
    QueueableCommand,
};
use similar::{ChangeTag, TextDiff};
use std::io;
use std::path::Path;
use std::sync::LazyLock;
use syntect::easy::HighlightLines;
use syntect::highlighting::Style;
use syntect::parsing::SyntaxSet;

use super::{crlf, term_width};

pub(super) static SYNTAX_SET: LazyLock<SyntaxSet> =
    LazyLock::new(SyntaxSet::load_defaults_newlines);
pub(super) static THEME_SET: LazyLock<two_face::theme::EmbeddedLazyThemeSet> =
    LazyLock::new(two_face::theme::extra);

struct DiffLayout {
    indent: &'static str,
    gutter_width: usize,
    max_content: usize,
}

pub(super) fn render_code_block(out: &mut io::Stdout, lines: &[&str], lang: &str) -> u16 {
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
    render_highlighted(out, lines, syntax, 0)
}

pub(super) fn render_highlighted(
    out: &mut io::Stdout,
    lines: &[&str],
    syntax: &syntect::parsing::SyntaxReference,
    max_rows: u16,
) -> u16 {
    let indent = "   ";
    let theme = &THEME_SET[two_face::theme::EmbeddedThemeName::MonokaiExtended];
    let gutter_width = format!("{}", lines.len()).len().max(2);
    let prefix_len = indent.len() + 1 + gutter_width + 3;
    let max_content = term_width().saturating_sub(prefix_len + 1);
    let limit = if max_rows == 0 {
        lines.len()
    } else {
        (max_rows as usize).min(lines.len())
    };

    let mut h = HighlightLines::new(syntax, theme);
    for (i, line) in lines[..limit].iter().enumerate() {
        let line_with_nl = format!("{}\n", line);
        let regions = h
            .highlight_line(&line_with_nl, &SYNTAX_SET)
            .unwrap_or_default();
        let _ = out.queue(Print(indent));
        let _ = out.queue(SetForegroundColor(Color::DarkGrey));
        let _ = out.queue(Print(format!(" {:>w$}", i + 1, w = gutter_width)));
        let _ = out.queue(ResetColor);
        let _ = out.queue(Print("   "));
        print_syntect_regions(out, &regions, max_content, None);
        crlf(out);
    }
    if limit < lines.len() {
        print_truncation(out, limit as u16, max_rows);
    }
    limit as u16
}

pub(super) fn print_syntax_file(out: &mut io::Stdout, content: &str, path: &str, max_rows: u16) -> u16 {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("txt");
    let syntax = SYNTAX_SET
        .find_syntax_by_extension(ext)
        .unwrap_or_else(|| SYNTAX_SET.find_syntax_plain_text());
    let lines: Vec<&str> = content.lines().collect();
    render_highlighted(out, &lines, syntax, max_rows)
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
        .map(|c| DiffChange { tag: c.tag(), value: c.value().to_string() })
        .collect();
    let ctx = 3usize;
    let mut first_mod: Option<usize> = None;
    let mut last_mod: Option<usize> = None;
    let mut new_line = start_line;
    for c in &changes {
        match c.tag {
            ChangeTag::Equal => { new_line += 1; }
            ChangeTag::Delete => {
                if first_mod.is_none() { first_mod = Some(new_line); }
                last_mod = Some(new_line);
            }
            ChangeTag::Insert => {
                if first_mod.is_none() { first_mod = Some(new_line); }
                last_mod = Some(new_line);
                new_line += 1;
            }
        }
    }
    let first_mod = first_mod.unwrap_or(start_line);
    let last_mod = last_mod.unwrap_or(start_line);
    let view_start = first_mod.saturating_sub(ctx);
    let view_end = (last_mod + 1 + ctx).min(file_lines_count);

    DiffViewData { file_content, start_line, first_mod, view_start, view_end, changes }
}

/// Render a syntax-highlighted inline diff.
pub(super) fn print_inline_diff(out: &mut io::Stdout, old: &str, new: &str, path: &str, anchor: &str, max_rows: u16) -> u16 {
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
    let file_lines: Vec<&str> = dv.file_content.lines().collect();
    let changes = &dv.changes;

    let max_lineno = dv.view_end;
    let gutter_width = format!("{}", max_lineno).len().max(2);
    let prefix_len = indent.len() + 1 + gutter_width + 3;
    let right_margin = indent.len();
    let max_content = term_width().saturating_sub(prefix_len + right_margin);

    let bg_del = Color::Rgb { r: 60, g: 20, b: 20 };
    let bg_add = Color::Rgb { r: 20, g: 50, b: 20 };

    let layout = DiffLayout { indent, gutter_width, max_content };
    let limit = if max_rows == 0 { u16::MAX } else { max_rows };

    let mut h_old = HighlightLines::new(syntax, theme);
    let mut h_new = HighlightLines::new(syntax, theme);
    for i in 0..dv.view_start {
        if i < file_lines.len() {
            let line = format!("{}\n", file_lines[i]);
            let _ = h_old.highlight_line(&line, &SYNTAX_SET);
            let _ = h_new.highlight_line(&line, &SYNTAX_SET);
        }
    }

    let ctx_before_end = dv.start_line.min(dv.first_mod);
    let ctx_before_start = dv.view_start.min(ctx_before_end);
    let mut rows = print_diff_lines(
        out,
        &mut h_new,
        &file_lines[ctx_before_start..ctx_before_end],
        ctx_before_start,
        None,
        None,
        &layout,
    );
    for line in &file_lines[ctx_before_start..ctx_before_end] {
        let _ = h_old.highlight_line(&format!("{}\n", line), &SYNTAX_SET);
    }

    if rows >= limit {
        print_truncation(out, rows, limit);
        return limit;
    }

    let mut old_lineno = dv.start_line;
    let mut new_lineno = dv.start_line;
    for change in changes {
        if rows >= limit {
            print_truncation(out, rows, limit);
            return limit;
        }
        let text = change.value.trim_end_matches('\n');
        match change.tag {
            ChangeTag::Equal => {
                if new_lineno >= dv.view_start && new_lineno < dv.view_end {
                    print_diff_lines(out, &mut h_new, &[text], new_lineno, None, None, &layout);
                    rows += 1;
                }
                let _ = h_old.highlight_line(&format!("{}\n", text), &SYNTAX_SET);
                new_lineno += 1;
            }
            ChangeTag::Delete => {
                print_diff_lines(
                    out,
                    &mut h_old,
                    &[text],
                    old_lineno,
                    Some(('-', Color::Red)),
                    Some(bg_del),
                    &layout,
                );
                old_lineno += 1;
                rows += 1;
            }
            ChangeTag::Insert => {
                print_diff_lines(
                    out,
                    &mut h_new,
                    &[text],
                    new_lineno,
                    Some(('+', Color::Green)),
                    Some(bg_add),
                    &layout,
                );
                new_lineno += 1;
                rows += 1;
            }
        }
    }

    if rows >= limit {
        print_truncation(out, rows, limit);
        return limit;
    }

    let anchor_lines = anchor.lines().count();
    let after_start = dv.start_line + anchor_lines;
    let after_end = dv.view_end.min(file_lines.len());
    if after_start < after_end {
        let remaining = (limit - rows) as usize;
        let ctx_slice = &file_lines[after_start..after_end];
        let ctx_slice = if ctx_slice.len() > remaining {
            &ctx_slice[..remaining]
        } else {
            ctx_slice
        };
        rows += print_diff_lines(out, &mut h_new, ctx_slice, after_start, None, None, &layout);
    }
    rows
}

/// Count rows an inline diff would take without rendering.
pub(super) fn count_inline_diff_rows(old: &str, new: &str, path: &str, anchor: &str) -> u16 {
    let dv = compute_diff_view(old, new, path, anchor);

    let ctx_before_end = dv.start_line.min(dv.first_mod);
    let ctx_before_start = dv.view_start.min(ctx_before_end);
    let mut rows: usize = ctx_before_end.saturating_sub(ctx_before_start);

    let mut new_lineno = dv.start_line;
    for change in &dv.changes {
        match change.tag {
            ChangeTag::Equal => {
                if new_lineno >= dv.view_start && new_lineno < dv.view_end {
                    rows += 1;
                }
                new_lineno += 1;
            }
            ChangeTag::Delete => { rows += 1; }
            ChangeTag::Insert => { rows += 1; new_lineno += 1; }
        }
    }

    let anchor_lines = anchor.lines().count();
    let after_start = dv.start_line + anchor_lines;
    if after_start < dv.view_end {
        rows += dv.view_end - after_start;
    }
    rows as u16
}

fn print_truncation(out: &mut io::Stdout, _rows: u16, _limit: u16) {
    let _ = out.queue(SetAttribute(Attribute::Dim));
    let _ = out.queue(Print("   ..."));
    let _ = out.queue(SetAttribute(Attribute::Reset));
    crlf(out);
}

fn print_diff_lines(
    out: &mut io::Stdout,
    h: &mut HighlightLines,
    lines: &[&str],
    start_line: usize,
    sign: Option<(char, Color)>,
    bg: Option<Color>,
    layout: &DiffLayout,
) -> u16 {
    let DiffLayout { indent, gutter_width, max_content } = *layout;
    for (i, line) in lines.iter().enumerate() {
        let lineno = start_line + i + 1;
        let line_with_nl = format!("{}\n", line);
        let regions = h
            .highlight_line(&line_with_nl, &SYNTAX_SET)
            .unwrap_or_default();
        let _ = out.queue(Print(indent));
        if let Some((ch, color)) = sign {
            let _ = out.queue(SetBackgroundColor(bg.unwrap()));
            let _ = out.queue(SetForegroundColor(Color::DarkGrey));
            let _ = out.queue(Print(format!(" {:>w$} ", lineno, w = gutter_width)));
            let _ = out.queue(SetForegroundColor(color));
            let _ = out.queue(Print(format!("{} ", ch)));
            let content_cols = print_syntect_regions(out, &regions, max_content, bg);
            let prefix_cols = indent.len() + 1 + gutter_width + 3;
            let right_margin = indent.len();
            let pad = term_width().saturating_sub(prefix_cols + content_cols + right_margin);
            if pad > 0 {
                if let Some(bg_color) = bg {
                    let _ = out.queue(SetBackgroundColor(bg_color));
                }
                let _ = out.queue(Print(" ".repeat(pad)));
            }
            let _ = out.queue(ResetColor);
        } else {
            let _ = out.queue(SetForegroundColor(Color::DarkGrey));
            let _ = out.queue(Print(format!(" {:>w$}", lineno, w = gutter_width)));
            let _ = out.queue(ResetColor);
            let _ = out.queue(Print("   "));
            print_syntect_regions(out, &regions, max_content, None);
        }
        crlf(out);
    }
    lines.len() as u16
}

/// Print syntax-highlighted regions, respecting max_width in display columns.
/// Returns the number of display columns actually printed.
pub(super) fn print_syntect_regions(
    out: &mut io::Stdout,
    regions: &[(Style, &str)],
    max_width: usize,
    bg: Option<Color>,
) -> usize {
    let mut col = 0;
    for (style, text) in regions {
        let text = text.trim_end_matches('\n').trim_end_matches('\r');
        if text.is_empty() {
            continue;
        }
        let remaining = max_width.saturating_sub(col);
        if remaining == 0 {
            break;
        }
        let char_count = text.chars().count();
        let display: String = if char_count <= remaining {
            text.to_string()
        } else {
            text.chars().take(remaining).collect()
        };
        let display_cols = display.chars().count();
        if let Some(bg_color) = bg {
            let _ = out.queue(SetBackgroundColor(bg_color));
        }
        let fg = Color::Rgb {
            r: style.foreground.r,
            g: style.foreground.g,
            b: style.foreground.b,
        };
        let _ = out.queue(SetForegroundColor(fg));
        let _ = out.queue(Print(&display));
        col += display_cols;
    }
    let _ = out.queue(ResetColor);
    col
}

pub(super) fn render_markdown_table(out: &mut io::Stdout, lines: &[&str]) -> u16 {
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
        let mut in_border = false;
        for ch in line.chars() {
            let is_border =
                ('\u{2500}'..='\u{257F}').contains(&ch) || ('\u{2580}'..='\u{259F}').contains(&ch);
            if is_border && !in_border {
                let _ = out.queue(SetForegroundColor(theme::BAR));
                in_border = true;
            } else if !is_border && in_border {
                let _ = out.queue(ResetColor);
                in_border = false;
            }
            let _ = out.queue(Print(ch.to_string()));
        }
        if in_border {
            let _ = out.queue(ResetColor);
        }
        crlf(out);
    }
    rendered.lines().count() as u16
}
