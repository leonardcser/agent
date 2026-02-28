use crate::theme;
use crossterm::{
    style::{
        Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
    },
    QueueableCommand,
};
use std::collections::HashMap;
use std::io;
use std::time::Duration;

use super::highlight::{
    print_inline_diff, print_syntax_file, render_code_block, render_markdown_table,
};
use super::{crlf, truncate_str, Block, ConfirmChoice, ToolOutput, ToolStatus};

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

pub(super) fn render_block(out: &mut io::Stdout, block: &Block, width: usize) -> u16 {
    let _perf = crate::perf::begin("render_block");
    match block {
        Block::User { text } => {
            let w = width;
            let content_w = w.saturating_sub(1).max(1);
            let logical_lines: Vec<String> =
                text.trim().lines().map(|l| l.trim().to_string()).collect();
            let multiline = logical_lines.len() > 1
                || logical_lines
                    .first()
                    .is_some_and(|l| l.chars().count() > content_w);
            // For multi-line messages, pad all lines to the same width.
            let block_w = if multiline {
                logical_lines
                    .iter()
                    .map(|l| l.chars().count().min(content_w))
                    .max()
                    .unwrap_or(0)
            } else {
                0
            };
            let mut rows = 0u16;
            for logical_line in &logical_lines {
                if logical_line.is_empty() {
                    let fill = if multiline {
                        (block_w + 2).min(content_w + 1)
                    } else {
                        2
                    };
                    let _ = out
                        .queue(SetBackgroundColor(theme::USER_BG))
                        .and_then(|o| o.queue(Print(" ".repeat(fill))))
                        .and_then(|o| o.queue(SetAttribute(Attribute::Reset)))
                        .and_then(|o| o.queue(ResetColor));
                    crlf(out);
                    rows += 1;
                    continue;
                }
                let chars: Vec<char> = logical_line.chars().collect();
                let mut start = 0;
                loop {
                    let chunk: String = chars[start..(start + content_w).min(chars.len())]
                        .iter()
                        .collect();
                    let chunk_len = chunk.chars().count();
                    let full_width = chunk_len >= content_w;
                    let trailing = if full_width {
                        0
                    } else if multiline {
                        block_w.saturating_sub(chunk_len) + 1
                    } else {
                        1
                    };
                    let _ = out
                        .queue(SetBackgroundColor(theme::USER_BG))
                        .and_then(|o| o.queue(SetAttribute(Attribute::Bold)))
                        .and_then(|o| o.queue(Print(format!(" {}{}", chunk, " ".repeat(trailing)))))
                        .and_then(|o| o.queue(SetAttribute(Attribute::Reset)))
                        .and_then(|o| o.queue(ResetColor));
                    let wraps = full_width && start + content_w < chars.len();
                    if !wraps {
                        crlf(out);
                    }
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
                    rows += render_code_block(out, code_lines, lang);
                } else if lines[i].trim_start().starts_with('|') {
                    let table_start = i;
                    while i < lines.len() && lines[i].trim_start().starts_with('|') {
                        i += 1;
                    }
                    rows += render_markdown_table(out, &lines[table_start..i]);
                } else {
                    let max_cols = width.saturating_sub(1);
                    let segments = wrap_line(lines[i], max_cols);
                    for seg in &segments {
                        let _ = out.queue(Print(" "));
                        print_styled(out, seg);
                        crlf(out);
                    }
                    i += 1;
                    rows += segments.len() as u16;
                }
            }
            rows
        }
        Block::ToolCall {
            name,
            summary,
            status,
            elapsed,
            output,
            args,
        } => render_tool(
            out,
            name,
            summary,
            args,
            *status,
            *elapsed,
            output.as_ref(),
            width,
        ),
        Block::Confirm { tool, desc, choice } => {
            render_confirm_result(out, tool, desc, choice.clone())
        }
        Block::Error { message } => {
            print_error(out, message);
            1
        }
        Block::Exec { command, output } => {
            let w = width;
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
            crlf(out);
            let mut rows = 1u16;
            if !output.is_empty() {
                for line in output.lines() {
                    let _ = out.queue(SetForegroundColor(theme::MUTED));
                    let _ = out.queue(Print(format!("  {}", line)));
                    let _ = out.queue(ResetColor);
                    crlf(out);
                    rows += 1;
                }
            }
            rows
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn render_tool(
    out: &mut io::Stdout,
    name: &str,
    summary: &str,
    args: &HashMap<String, serde_json::Value>,
    status: ToolStatus,
    elapsed: Option<Duration>,
    output: Option<&ToolOutput>,
    width: usize,
) -> u16 {
    let color = match status {
        ToolStatus::Ok => theme::TOOL_OK,
        ToolStatus::Err | ToolStatus::Denied => theme::TOOL_ERR,
        ToolStatus::Confirm => theme::ACCENT,
        ToolStatus::Pending => theme::TOOL_PENDING,
    };
    let time = if name == "bash" && !matches!(status, ToolStatus::Confirm) {
        elapsed
    } else {
        None
    };
    let tl = if name == "bash" && status == ToolStatus::Pending {
        let ms = args
            .get("timeout_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(120_000);
        let secs = ms / 1000;
        Some(if secs.is_multiple_of(60) {
            format!("timeout {}m", secs / 60)
        } else if secs >= 60 {
            format!("timeout {}m{}s", secs / 60, secs % 60)
        } else {
            format!("timeout {}s", secs)
        })
    } else {
        None
    };
    print_tool_line(out, name, summary, color, time, tl.as_deref(), width);
    let mut rows = 1u16;
    if status != ToolStatus::Denied {
        if let Some(out_data) = output {
            rows += print_tool_output(out, name, &out_data.content, out_data.is_error, args);
        }
    }
    rows
}

fn render_confirm_result(
    out: &mut io::Stdout,
    tool: &str,
    desc: &str,
    choice: Option<ConfirmChoice>,
) -> u16 {
    let mut rows = 2u16;

    let _ = out.queue(SetForegroundColor(theme::APPLY));
    let _ = out.queue(Print("   allow? "));
    let _ = out.queue(ResetColor);
    print_dim(out, tool);
    crlf(out);

    print_dim(out, "   \u{2502} ");
    let _ = out.queue(Print(desc));
    crlf(out);

    if let Some(c) = choice {
        rows += 1;
        let _ = out.queue(Print("   "));
        match c {
            ConfirmChoice::Yes | ConfirmChoice::YesWithMessage(_) => {
                print_dim(out, "approved");
            }
            ConfirmChoice::Always => {
                print_dim(out, "always");
            }
            ConfirmChoice::AlwaysPattern(ref pat) => {
                print_dim(out, &format!("always ({})", pat));
            }
            ConfirmChoice::No => {
                let _ = out.queue(SetForegroundColor(theme::TOOL_ERR));
                let _ = out.queue(Print("denied"));
                let _ = out.queue(ResetColor);
            }
        }
        crlf(out);
    }
    rows
}

fn print_tool_line(
    out: &mut io::Stdout,
    name: &str,
    summary: &str,
    pill_color: Color,
    elapsed: Option<Duration>,
    timeout_label: Option<&str>,
    width: usize,
) {
    let _ = out.queue(Print(" "));
    let _ = out.queue(SetForegroundColor(pill_color));
    let _ = out.queue(Print("\u{23fa}"));
    let _ = out.queue(ResetColor);
    let time_str = elapsed
        .filter(|d| d.as_secs_f64() >= 0.1)
        .map(|d| format!("  {:.1}s", d.as_secs_f64()))
        .unwrap_or_default();
    let timeout_str = timeout_label
        .map(|l| format!(" ({})", l))
        .unwrap_or_default();
    let suffix_len = time_str.len() + timeout_str.len();
    let prefix_len = 3 + name.len() + 1;
    let max_summary = width.saturating_sub(prefix_len + suffix_len + 1);
    let truncated = truncate_str(summary, max_summary);
    print_dim(out, &format!(" {}", name));
    let _ = out.queue(Print(format!(" {}", truncated)));
    if !time_str.is_empty() {
        print_dim(out, &time_str);
    }
    if !timeout_str.is_empty() {
        print_dim(out, &timeout_str);
    }
    crlf(out);
}

fn print_tool_output(
    out: &mut io::Stdout,
    name: &str,
    content: &str,
    is_error: bool,
    args: &HashMap<String, serde_json::Value>,
) -> u16 {
    match name {
        "read_file" | "glob" | "grep" | "web_fetch" | "web_search" if !is_error => {
            let (s, p) = match name {
                "glob" => ("file", "files"),
                "grep" => ("match", "matches"),
                _ => ("line", "lines"),
            };
            print_dim_count(out, content.lines().count(), s, p)
        }
        "edit_file" if !is_error => render_edit_output(out, args),
        "write_file" if !is_error => render_write_output(out, args),
        "ask_user_question" if !is_error => render_question_output(out, content),
        "bash" if content.is_empty() => 0,
        "bash" => render_bash_output(out, content, is_error),
        _ => render_default_output(out, content, is_error),
    }
}

fn print_dim(out: &mut io::Stdout, text: &str) {
    let _ = out.queue(SetAttribute(Attribute::Dim));
    let _ = out.queue(Print(text));
    let _ = out.queue(SetAttribute(Attribute::Reset));
}

fn print_dim_count(out: &mut io::Stdout, count: usize, singular: &str, plural: &str) -> u16 {
    print_dim(out, &format!("   {}", pluralize(count, singular, plural)));
    crlf(out);
    1
}

fn render_edit_output(out: &mut io::Stdout, args: &HashMap<String, serde_json::Value>) -> u16 {
    let old = args
        .get("old_string")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let new = args
        .get("new_string")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let path = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
    if new.is_empty() {
        print_dim_count(out, old.lines().count(), "line deleted", "lines deleted")
    } else {
        print_inline_diff(out, old, new, path, new, 0)
    }
}

fn render_write_output(out: &mut io::Stdout, args: &HashMap<String, serde_json::Value>) -> u16 {
    let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
    let path = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
    print_syntax_file(out, content, path, 0)
}

fn render_question_output(out: &mut io::Stdout, content: &str) -> u16 {
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
            print_dim(out, &format!("   {} ", question));
            let _ = out.queue(Print(&answer_str));
            crlf(out);
            rows += 1;
        }
    } else {
        print_dim(out, &format!("   {}", content));
        crlf(out);
        rows += 1;
    }
    rows
}

fn render_bash_output(out: &mut io::Stdout, content: &str, is_error: bool) -> u16 {
    const MAX_LINES: usize = 20;
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    let mut rows = 0u16;
    if total > MAX_LINES {
        let skipped = total - MAX_LINES;
        print_dim(out, &format!("   ... {} lines above", skipped));
        crlf(out);
        rows += 1;
    }
    let visible = if total > MAX_LINES {
        &lines[total - MAX_LINES..]
    } else {
        &lines[..]
    };
    for line in visible {
        if is_error {
            let _ = out.queue(SetForegroundColor(theme::TOOL_ERR));
            let _ = out.queue(Print(format!("   {}", line)));
            let _ = out.queue(ResetColor);
        } else {
            print_dim(out, &format!("   {}", line));
        }
        crlf(out);
        rows += 1;
    }
    rows
}

fn render_default_output(out: &mut io::Stdout, content: &str, is_error: bool) -> u16 {
    let preview = result_preview(content, 3);
    if is_error {
        let _ = out.queue(SetForegroundColor(theme::TOOL_ERR));
        let _ = out.queue(Print(format!("   {}", preview)));
        let _ = out.queue(ResetColor);
    } else {
        print_dim(out, &format!("   {}", preview));
    }
    crlf(out);
    preview.lines().count() as u16
}

fn pluralize(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("1 {}", singular)
    } else {
        format!("{} {}", count, plural)
    }
}

fn print_error(out: &mut io::Stdout, msg: &str) {
    let _ = out.queue(SetForegroundColor(theme::TOOL_ERR));
    let _ = out.queue(Print(format!(" error: {}", msg)));
    let _ = out.queue(ResetColor);
    crlf(out);
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

pub(super) fn print_styled(out: &mut io::Stdout, text: &str) {
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
