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
use super::{chunk_line, crlf, truncate_str, Block, ConfirmChoice, ToolOutput, ToolStatus};

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
        (_, Element::Block(Block::Thinking { .. })) => 1,
        (Element::Block(Block::Thinking { .. }), _) => 1,
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
            // Each rendered row is: " " (1-char prefix) + content + trailing padding.
            // `text_w` is the max content chars per row so the total never reaches
            // the terminal width (which would cause an implicit wrap).
            let text_w = width.saturating_sub(2).max(1);
            let all_lines: Vec<String> = text.lines().map(|l| l.replace('\t', "    ")).collect();
            // Strip leading/trailing blank lines but preserve internal structure.
            let start = all_lines.iter().position(|l| !l.is_empty()).unwrap_or(0);
            let end = all_lines
                .iter()
                .rposition(|l| !l.is_empty())
                .map_or(0, |i| i + 1);
            let logical_lines: Vec<String> = all_lines[start..end].to_vec();
            let wraps = logical_lines.iter().any(|l| l.chars().count() > text_w);
            let multiline = logical_lines.len() > 1 || wraps;
            // For multi-line messages, pad all rows to the same width.
            // If any line wraps, that means the longest line is text_w.
            let block_w = if multiline {
                if wraps {
                    text_w
                } else {
                    logical_lines
                        .iter()
                        .map(|l| l.chars().count())
                        .max()
                        .unwrap_or(0)
                        + 1
                }
            } else {
                0
            };
            let mut rows = 0u16;
            for logical_line in &logical_lines {
                if logical_line.is_empty() {
                    let fill = if block_w > 0 { block_w + 1 } else { 2 };
                    let _ = out
                        .queue(SetBackgroundColor(theme::USER_BG))
                        .and_then(|o| o.queue(Print(" ".repeat(fill))))
                        .and_then(|o| o.queue(SetAttribute(Attribute::Reset)))
                        .and_then(|o| o.queue(ResetColor));
                    crlf(out);
                    rows += 1;
                    continue;
                }
                let chunks = chunk_line(logical_line, text_w);
                for chunk in &chunks {
                    let chunk_len = chunk.chars().count();
                    let trailing = if block_w > 0 {
                        block_w.saturating_sub(chunk_len)
                    } else {
                        1
                    };
                    let _ = out
                        .queue(SetBackgroundColor(theme::USER_BG))
                        .and_then(|o| o.queue(SetAttribute(Attribute::Bold)))
                        .and_then(|o| o.queue(Print(format!(" {}{}", chunk, " ".repeat(trailing)))))
                        .and_then(|o| o.queue(SetAttribute(Attribute::Reset)))
                        .and_then(|o| o.queue(ResetColor));
                    crlf(out);
                    rows += 1;
                }
            }
            rows
        }
        Block::Thinking { content } => {
            let max_cols = width.saturating_sub(4).max(1); // "│ " prefix + 1 margin
            let mut rows = 0u16;
            for line in content.lines() {
                let segments = wrap_line(line, max_cols);
                for seg in &segments {
                    let _ = out.queue(SetAttribute(Attribute::Dim));
                    let _ = out.queue(SetAttribute(Attribute::Italic));
                    let _ = out.queue(Print(format!(" \u{2502} {}", seg)));
                    let _ = out.queue(SetAttribute(Attribute::Reset));
                    crlf(out);
                    rows += 1;
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
            user_message,
        } => render_tool(
            out,
            name,
            summary,
            args,
            *status,
            *elapsed,
            output.as_ref(),
            user_message.as_deref(),
            width,
        ),
        Block::Confirm { tool, desc, choice } => {
            render_confirm_result(out, tool, desc, choice.clone(), width)
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
    user_message: Option<&str>,
    width: usize,
) -> u16 {
    let color = match status {
        ToolStatus::Ok => theme::TOOL_OK,
        ToolStatus::Err | ToolStatus::Denied => theme::TOOL_ERR,
        ToolStatus::Confirm => theme::ACCENT,
        ToolStatus::Pending => theme::TOOL_PENDING,
    };
    let time = if name == "bash" && status != ToolStatus::Confirm {
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
    if let Some(msg) = user_message {
        print_dim(out, &format!("   {msg}"));
        crlf(out);
        rows += 1;
    }
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
    width: usize,
) -> u16 {
    let mut rows = 2u16;

    let _ = out.queue(SetForegroundColor(theme::APPLY));
    let _ = out.queue(Print("   allow? "));
    let _ = out.queue(ResetColor);
    print_dim(out, tool);
    crlf(out);

    let prefix = "   \u{2502} ";
    let prefix_len = prefix.chars().count();
    let segments = wrap_line(desc, width.saturating_sub(prefix_len));
    for (i, seg) in segments.iter().enumerate() {
        if i == 0 {
            print_dim(out, prefix);
        } else {
            let _ = out.queue(Print(" ".repeat(prefix_len)));
        }
        let _ = out.queue(Print(seg));
        crlf(out);
    }
    rows += segments.len().saturating_sub(1) as u16;

    if let Some(c) = choice {
        rows += 1;
        let _ = out.queue(Print("   "));
        match c {
            ConfirmChoice::Yes => {
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
        "web_search" if !is_error => {
            let mut count = 0u16;
            for line in content.lines() {
                if let Some(pos) = line.find(". ") {
                    let prefix = &line[..pos];
                    if !prefix.is_empty() && prefix.chars().all(|c| c.is_ascii_digit()) {
                        let title = &line[pos + 2..];
                        print_dim(out, &format!("   {title}"));
                        crlf(out);
                        count += 1;
                    }
                }
            }
            if count == 0 {
                print_dim(out, "   No results found");
                crlf(out);
                return 1;
            }
            count
        }
        "read_file" | "glob" | "grep" | "web_fetch" if !is_error => {
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
