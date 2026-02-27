use crate::theme;
use crossterm::{
    style::{
        Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
    },
    QueueableCommand,
};
use std::collections::HashMap;
use std::io::{self};
use std::time::Duration;

use super::highlight::{
    print_inline_diff, print_syntax_file, render_code_block, render_markdown_table,
};
use super::{Block, ConfirmChoice, ToolOutput, ToolStatus, term_width, truncate_str};

/// Element types for spacing calculation.
pub(super) enum Element<'a> {
    Block(&'a Block),
    ActiveTool,
    Prompt,
}

/// Number of blank lines to insert between two adjacent elements.
pub(super) fn gap_between(above: &Element, below: &Element) -> u16 {
    match (above, below) {
        (Element::Block(Block::User { .. }), _) => 1,
        (_, Element::Block(Block::User { .. })) => 1,
        (Element::Block(Block::Exec { .. }), _) => 1,
        (_, Element::Block(Block::Exec { .. })) => 1,
        (Element::Block(Block::ToolCall { .. }), Element::Block(Block::ToolCall { .. })) => 1,
        (Element::Block(Block::ToolCall { .. }), Element::ActiveTool) => 1,
        (Element::Block(Block::Text { .. }), Element::Block(Block::ToolCall { .. })) => 1,
        (Element::Block(Block::Text { .. }), Element::ActiveTool) => 1,
        (Element::Block(Block::ToolCall { .. }), Element::Block(Block::Text { .. })) => 1,
        (Element::Block(_), Element::Prompt) => 1,
        (Element::ActiveTool, Element::Prompt) => 1,
        _ => 0,
    }
}

pub(super) fn render_block(block: &Block, _width: usize) -> u16 {
    match block {
        Block::User { text } => {
            let mut out = io::stdout();
            let w = term_width();
            let content_w = w.saturating_sub(1).max(1);
            let logical_lines: Vec<String> = text
                .trim_start()
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect();
            let mut rows = 0u16;
            for logical_line in &logical_lines {
                let chars: Vec<char> = logical_line.chars().collect();
                let single_line = chars.len() <= content_w;
                let mut start = 0;
                loop {
                    let chunk: String =
                        chars[start..(start + content_w).min(chars.len())].iter().collect();
                    let chunk_len = chunk.chars().count();
                    let trailing = if single_line {
                        1
                    } else {
                        content_w.saturating_sub(chunk_len)
                    };
                    let _ = out
                        .queue(SetBackgroundColor(theme::USER_BG))
                        .and_then(|o| o.queue(SetAttribute(Attribute::Bold)))
                        .and_then(|o| {
                            o.queue(Print(format!(" {}{}", chunk, " ".repeat(trailing))))
                        })
                        .and_then(|o| o.queue(SetAttribute(Attribute::Reset)))
                        .and_then(|o| o.queue(ResetColor));
                    let _ = out.queue(Print("\r\n"));
                    rows += 1;
                    start += content_w;
                    if start >= chars.len() {
                        break;
                    }
                }
            }
            rows
        }
        Block::Text { content } => {
            let mut out = io::stdout();
            let lines: Vec<&str> = content.lines().collect();
            let mut i = 0;
            let mut rows = 0u16;
            while i < lines.len() {
                if lines[i].trim_start().starts_with("```") {
                    let lang = lines[i].trim_start().trim_start_matches('`').trim();
                    i += 1;
                    let code_start = i;
                    while i < lines.len() && !lines[i].trim_start().starts_with("```") {
                        i += 1;
                    }
                    let code_lines = &lines[code_start..i];
                    if i < lines.len() {
                        i += 1;
                    }
                    rows += render_code_block(code_lines, lang);
                } else if lines[i].trim_start().starts_with('|') {
                    let table_start = i;
                    while i < lines.len() && lines[i].trim_start().starts_with('|') {
                        i += 1;
                    }
                    rows += render_markdown_table(&lines[table_start..i]);
                } else {
                    let max_cols = term_width().saturating_sub(1);
                    let segments = wrap_line(lines[i], max_cols);
                    for seg in &segments {
                        let _ = out.queue(Print(" "));
                        print_styled(seg);
                        let _ = out.queue(Print("\r\n"));
                    }
                    i += 1;
                    rows += segments.len() as u16;
                }
            }
            rows
        }
        Block::ToolCall { name, summary, status, elapsed, output, args } => {
            render_tool(name, summary, args, *status, *elapsed, output.as_ref())
        }
        Block::Confirm { tool, desc, choice } => {
            render_confirm_result(tool, desc, choice.clone())
        }
        Block::Error { message } => {
            print_error(message);
            1
        }
        Block::Exec { command, output } => {
            let mut out = io::stdout();
            let w = term_width();
            let display = format!("!{}", command);
            let char_len = display.chars().count();
            let pad_width = (char_len + 2).min(w);
            let trailing = pad_width.saturating_sub(char_len + 1);
            let _ = out.queue(SetBackgroundColor(theme::USER_BG));
            let _ = out.queue(SetForegroundColor(theme::EXEC));
            let _ = out.queue(SetAttribute(Attribute::Bold));
            let _ = out.queue(Print(" !"));
            let _ = out.queue(SetForegroundColor(Color::Reset));
            let _ = out.queue(Print(format!("{}{}", command, " ".repeat(trailing))));
            let _ = out.queue(SetAttribute(Attribute::Reset));
            let _ = out.queue(ResetColor);
            let _ = out.queue(Print("\r\n"));
            let mut rows = 1u16;
            if !output.is_empty() {
                for line in output.lines() {
                    let _ = out
                        .queue(SetForegroundColor(theme::MUTED))
                        .and_then(|o| o.queue(Print(format!("  {}\r\n", line))))
                        .and_then(|o| o.queue(ResetColor));
                    rows += 1;
                }
            }
            rows
        }
    }
}

pub(super) fn render_tool(
    name: &str,
    summary: &str,
    args: &HashMap<String, serde_json::Value>,
    status: ToolStatus,
    elapsed: Option<Duration>,
    output: Option<&ToolOutput>,
) -> u16 {
    let color = match status {
        ToolStatus::Ok => theme::TOOL_OK,
        ToolStatus::Err | ToolStatus::Denied => theme::TOOL_ERR,
        ToolStatus::Confirm => theme::ACCENT,
        ToolStatus::Pending => theme::TOOL_PENDING,
    };
    let time = if name == "bash" && !matches!(status, ToolStatus::Pending | ToolStatus::Confirm) {
        elapsed
    } else {
        None
    };
    let tl = super::tool_timeout_label(args);
    print_tool_line(name, summary, color, time, tl.as_deref());
    let mut rows = 1u16;
    if status != ToolStatus::Denied {
        if let Some(out_data) = output {
            rows += print_tool_output(name, &out_data.content, out_data.is_error, args);
        }
    }
    rows
}

fn render_confirm_result(tool: &str, desc: &str, choice: Option<ConfirmChoice>) -> u16 {
    let mut out = io::stdout();
    let mut rows = 2u16;

    let _ = out
        .queue(SetForegroundColor(theme::APPLY))
        .and_then(|o| o.queue(Print("   allow? ")))
        .and_then(|o| o.queue(ResetColor))
        .and_then(|o| o.queue(SetAttribute(Attribute::Dim)))
        .and_then(|o| o.queue(Print(tool)))
        .and_then(|o| o.queue(SetAttribute(Attribute::Reset)))
        .and_then(|o| o.queue(Print("\r\n")));

    let _ = out
        .queue(SetAttribute(Attribute::Dim))
        .and_then(|o| o.queue(Print("   \u{2502} ")))
        .and_then(|o| o.queue(SetAttribute(Attribute::Reset)))
        .and_then(|o| o.queue(Print(desc)))
        .and_then(|o| o.queue(Print("\r\n")));

    if let Some(c) = choice {
        rows += 1;
        let _ = out.queue(Print("   "));
        match c {
            ConfirmChoice::Yes | ConfirmChoice::YesWithMessage(_) => {
                let _ = out
                    .queue(SetAttribute(Attribute::Dim))
                    .and_then(|o| o.queue(Print("approved\r\n")))
                    .and_then(|o| o.queue(SetAttribute(Attribute::Reset)));
            }
            ConfirmChoice::Always => {
                let _ = out
                    .queue(SetAttribute(Attribute::Dim))
                    .and_then(|o| o.queue(Print("always\r\n")))
                    .and_then(|o| o.queue(SetAttribute(Attribute::Reset)));
            }
            ConfirmChoice::No => {
                let _ = out
                    .queue(SetForegroundColor(theme::TOOL_ERR))
                    .and_then(|o| o.queue(Print("denied\r\n")))
                    .and_then(|o| o.queue(ResetColor));
            }
        }
    }
    rows
}

fn print_tool_line(
    name: &str,
    summary: &str,
    pill_color: Color,
    elapsed: Option<Duration>,
    timeout_label: Option<&str>,
) {
    let mut out = io::stdout();
    let width = term_width();
    let _ = out.queue(Print(" "));
    let _ = out.queue(SetForegroundColor(pill_color));
    let _ = out.queue(Print("\u{23fa}"));
    let _ = out.queue(ResetColor);
    let time_str = elapsed
        .filter(|d| d.as_secs_f64() >= 0.1)
        .map(|d| format!("  {:.1}s", d.as_secs_f64()))
        .unwrap_or_default();
    let timeout_str = timeout_label
        .map(|l| format!("  ({})", l))
        .unwrap_or_default();
    let suffix_len = time_str.len() + timeout_str.len();
    let prefix_len = 3 + name.len() + 1;
    let max_summary = width.saturating_sub(prefix_len + suffix_len + 1);
    let truncated = truncate_str(summary, max_summary);
    let _ = out.queue(SetAttribute(Attribute::Dim));
    let _ = out.queue(Print(format!(" {}", name)));
    let _ = out.queue(SetAttribute(Attribute::Reset));
    let _ = out.queue(Print(format!(" {}", truncated)));
    if !time_str.is_empty() {
        let _ = out.queue(SetAttribute(Attribute::Dim));
        let _ = out.queue(Print(&time_str));
        let _ = out.queue(SetAttribute(Attribute::Reset));
    }
    if !timeout_str.is_empty() {
        let _ = out.queue(SetAttribute(Attribute::Dim));
        let _ = out.queue(Print(&timeout_str));
        let _ = out.queue(SetAttribute(Attribute::Reset));
    }
    let _ = out.queue(Print("\r\n"));
}

fn print_tool_output(
    name: &str,
    content: &str,
    is_error: bool,
    args: &HashMap<String, serde_json::Value>,
) -> u16 {
    let mut out = io::stdout();
    match name {
        "read_file" if !is_error => {
            let line_count = content.lines().count();
            let _ = out
                .queue(SetAttribute(Attribute::Dim))
                .and_then(|o| {
                    o.queue(Print(format!(
                        "   {}\r\n",
                        pluralize(line_count, "line", "lines")
                    )))
                })
                .and_then(|o| o.queue(SetAttribute(Attribute::Reset)));
            1
        }
        "edit_file" if !is_error => {
            let old = args.get("old_string").and_then(|v| v.as_str()).unwrap_or("");
            let new = args.get("new_string").and_then(|v| v.as_str()).unwrap_or("");
            let path = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
            if new.is_empty() {
                let n = old.lines().count();
                let _ = out
                    .queue(SetAttribute(Attribute::Dim))
                    .and_then(|o| {
                        o.queue(Print(format!(
                            "   {}\r\n",
                            pluralize(n, "line deleted", "lines deleted")
                        )))
                    })
                    .and_then(|o| o.queue(SetAttribute(Attribute::Reset)));
                1
            } else {
                print_inline_diff(old, new, path, new, 0)
            }
        }
        "write_file" if !is_error => {
            let file_content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let path = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
            print_syntax_file(file_content, path, 0)
        }
        "ask_user_question" if !is_error => {
            let mut out = io::stdout();
            let mut rows = 0u16;
            if let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(content) {
                for (question, answer) in &map {
                    let answer_str = match answer {
                        serde_json::Value::String(s) => s.clone(),
                        serde_json::Value::Array(arr) => arr
                            .iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join(", "),
                        other => other.to_string(),
                    };
                    let _ = out
                        .queue(SetAttribute(Attribute::Dim))
                        .and_then(|o| o.queue(Print(format!("   {} ", question))))
                        .and_then(|o| o.queue(SetAttribute(Attribute::Reset)))
                        .and_then(|o| o.queue(Print(format!("{}\r\n", answer_str))));
                    rows += 1;
                }
            } else {
                let _ = out
                    .queue(SetAttribute(Attribute::Dim))
                    .and_then(|o| o.queue(Print(format!("   {}\r\n", content))))
                    .and_then(|o| o.queue(SetAttribute(Attribute::Reset)));
                rows += 1;
            }
            rows
        }
        "bash" if content.is_empty() => 0,
        "bash" => {
            const MAX_LINES: usize = 20;
            let lines: Vec<&str> = content.lines().collect();
            let total = lines.len();
            let mut rows = 0u16;
            if total > MAX_LINES {
                let skipped = total - MAX_LINES;
                let _ = out.queue(SetAttribute(Attribute::Dim));
                let _ = out.queue(Print(format!("   ... {} lines above\r\n", skipped)));
                let _ = out.queue(SetAttribute(Attribute::Reset));
                rows += 1;
            }
            let visible = if total > MAX_LINES { &lines[total - MAX_LINES..] } else { &lines[..] };
            for line in visible {
                if is_error {
                    let _ = out.queue(SetForegroundColor(theme::TOOL_ERR));
                    let _ = out.queue(Print(format!("   {}\r\n", line)));
                    let _ = out.queue(ResetColor);
                } else {
                    let _ = out.queue(SetAttribute(Attribute::Dim));
                    let _ = out.queue(Print(format!("   {}\r\n", line)));
                    let _ = out.queue(SetAttribute(Attribute::Reset));
                }
                rows += 1;
            }
            rows
        }
        "grep" if !is_error => {
            let count = content.lines().count();
            let _ = out
                .queue(SetAttribute(Attribute::Dim))
                .and_then(|o| {
                    o.queue(Print(format!(
                        "   {}\r\n",
                        pluralize(count, "match", "matches")
                    )))
                })
                .and_then(|o| o.queue(SetAttribute(Attribute::Reset)));
            1
        }
        "glob" if !is_error => {
            let count = content.lines().count();
            let _ = out
                .queue(SetAttribute(Attribute::Dim))
                .and_then(|o| {
                    o.queue(Print(format!(
                        "   {}\r\n",
                        pluralize(count, "file", "files")
                    )))
                })
                .and_then(|o| o.queue(SetAttribute(Attribute::Reset)));
            1
        }
        _ => {
            let preview = result_preview(content, 3);
            if is_error {
                let _ = out
                    .queue(SetForegroundColor(theme::TOOL_ERR))
                    .and_then(|o| o.queue(Print(format!("   {}\r\n", preview))))
                    .and_then(|o| o.queue(ResetColor));
            } else {
                let _ = out
                    .queue(SetAttribute(Attribute::Dim))
                    .and_then(|o| o.queue(Print(format!("   {}\r\n", preview))))
                    .and_then(|o| o.queue(SetAttribute(Attribute::Reset)));
            }
            preview.lines().count() as u16
        }
    }
}

fn pluralize(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("1 {}", singular)
    } else {
        format!("{} {}", count, plural)
    }
}

fn print_error(msg: &str) {
    let mut out = io::stdout();
    let _ = out
        .queue(SetForegroundColor(theme::TOOL_ERR))
        .and_then(|o| o.queue(Print(format!(" error: {}\r\n", msg))))
        .and_then(|o| o.queue(ResetColor));
}

fn result_preview(content: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = content.trim_end_matches('\n').lines().collect();
    if lines.len() <= max_lines {
        lines.join(" | ")
    } else {
        format!(
            "{} ... ({} lines)",
            lines[..max_lines].join(" | "),
            lines.len()
        )
    }
}

/// Wrap a line to fit within `max_cols` display columns, breaking at word boundaries.
pub(super) fn wrap_line(line: &str, max_cols: usize) -> Vec<String> {
    if max_cols == 0 {
        return vec![line.to_string()];
    }
    let mut segments: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut col = 0;

    for word in line.split_inclusive(' ') {
        let wlen = word.chars().count();
        if col + wlen > max_cols && col > 0 {
            segments.push(current);
            current = String::new();
            col = 0;
        }
        if wlen > max_cols {
            for ch in word.chars() {
                if col >= max_cols {
                    segments.push(current);
                    current = String::new();
                    col = 0;
                }
                current.push(ch);
                col += 1;
            }
        } else {
            current.push_str(word);
            col += wlen;
        }
    }
    segments.push(current);
    segments
}

pub(super) fn print_styled(text: &str) {
    let mut out = io::stdout();

    let trimmed = text.trim_start();
    if trimmed.starts_with('#') {
        let _ = out.queue(SetForegroundColor(theme::HEADING));
        let _ = out.queue(SetAttribute(Attribute::Bold));
        let _ = out.queue(Print(trimmed));
        let _ = out.queue(SetAttribute(Attribute::Reset));
        let _ = out.queue(ResetColor);
        return;
    }

    if trimmed.starts_with('>') {
        let content = trimmed.strip_prefix('>').unwrap().trim_start();
        let _ = out.queue(SetAttribute(Attribute::Dim));
        let _ = out.queue(SetAttribute(Attribute::Italic));
        let _ = out.queue(Print(content));
        let _ = out.queue(SetAttribute(Attribute::Reset));
        return;
    }

    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut i = 0;
    let mut plain = String::new();

    while i < len {
        if i + 1 < len && chars[i] == '*' && chars[i + 1] == '*' {
            if !plain.is_empty() {
                let _ = out.queue(Print(&plain));
                plain.clear();
            }
            i += 2;
            let start = i;
            while i + 1 < len && !(chars[i] == '*' && chars[i + 1] == '*') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let _ = out.queue(SetAttribute(Attribute::Bold));
            let _ = out.queue(SetAttribute(Attribute::Dim));
            let _ = out.queue(Print(&word));
            let _ = out.queue(SetAttribute(Attribute::Reset));
            if i + 1 < len {
                i += 2;
            }
            continue;
        }

        if chars[i] == '*' && i + 1 < len && chars[i + 1] != '*' {
            if !plain.is_empty() {
                let _ = out.queue(Print(&plain));
                plain.clear();
            }
            i += 1;
            let start = i;
            while i < len && chars[i] != '*' {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let _ = out.queue(SetAttribute(Attribute::Italic));
            let _ = out.queue(SetAttribute(Attribute::Dim));
            let _ = out.queue(Print(&word));
            let _ = out.queue(SetAttribute(Attribute::Reset));
            if i < len {
                i += 1;
            }
            continue;
        }

        if chars[i] == '`' {
            if !plain.is_empty() {
                let _ = out.queue(Print(&plain));
                plain.clear();
            }
            i += 1;
            let start = i;
            while i < len && chars[i] != '`' {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let _ = out.queue(SetForegroundColor(theme::ACCENT));
            let _ = out.queue(Print(&word));
            let _ = out.queue(ResetColor);
            if i < len {
                i += 1;
            }
            continue;
        }

        plain.push(chars[i]);
        i += 1;
    }
    if !plain.is_empty() {
        let _ = out.queue(Print(&plain));
    }
}
