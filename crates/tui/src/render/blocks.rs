use crate::theme;
use crate::utils::format_duration;
use crossterm::{
    style::{
        Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
    },
    QueueableCommand,
};
use std::collections::HashMap;
use std::time::Duration;

use super::highlight::{
    print_highlighted_bash_line, print_inline_diff, print_syntax_file, render_code_block,
    render_markdown_table,
};
use super::{
    crlf, truncate_str, wrap_line, Block, ConfirmChoice, RenderOut, ToolOutput, ToolStatus,
};

/// Element types for spacing calculation.
pub(super) enum Element<'a> {
    Block(&'a Block),
    ActiveTool,
    Prompt,
}

/// Number of blank lines to insert between two adjacent elements.
pub(super) fn gap_between(above: &Element, below: &Element) -> u16 {
    // Code block borders already provide visual separation, so skip gaps
    // when a Text block leads or trails with a code fence.
    if let Element::Block(Block::Text { content }) = below {
        if content.trim_start().starts_with("```") {
            return 0;
        }
    }
    if let Element::Block(Block::Text { content }) = above {
        if content.trim_end().ends_with("```") {
            return 0;
        }
    }
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
        (Element::Block(Block::Hint { .. }), _) => 1,
        (_, Element::Block(Block::Hint { .. })) => 1,
        (Element::Block(Block::Error { .. }), _) => 1,
        (_, Element::Block(Block::Error { .. })) => 1,
        (Element::Block(_), Element::Prompt) => 1,
        (Element::ActiveTool, Element::Prompt) => 1,
        _ => 0,
    }
}

pub(super) fn render_block(out: &mut RenderOut, block: &Block, width: usize) -> u16 {
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
                    text_w + 1
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
                let chunks = wrap_line(logical_line, text_w);
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
        Block::Text { content } => render_markdown(out, content.trim_end(), width, " ", false),
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
        Block::Hint { content } => {
            let _ = out.queue(SetAttribute(Attribute::Dim));
            let _ = out.queue(Print(content));
            let _ = out.queue(SetAttribute(Attribute::Reset));
            crlf(out);
            1
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
                let max_cols = w.saturating_sub(3);
                for line in output.lines() {
                    for seg in &wrap_line(line, max_cols) {
                        let _ = out.queue(SetForegroundColor(theme::MUTED));
                        let _ = out.queue(Print(format!("  {}", seg)));
                        let _ = out.queue(ResetColor);
                        crlf(out);
                        rows += 1;
                    }
                }
            }
            rows
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn render_tool(
    out: &mut RenderOut,
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
        ToolStatus::Confirm => theme::accent(),
        ToolStatus::Pending => theme::TOOL_PENDING,
    };
    let time = if matches!(
        name,
        "bash" | "web_fetch" | "read_process_output" | "stop_process"
    ) && status != ToolStatus::Confirm
    {
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
        Some(format!("timeout: {}", format_duration(secs)))
    } else {
        None
    };
    print_tool_line(out, name, summary, color, time, tl.as_deref(), width);
    let mut rows = 1u16;
    if name == "web_fetch" {
        if let Some(prompt) = args.get("prompt").and_then(|v| v.as_str()) {
            for seg in &wrap_line(prompt, width.saturating_sub(4)) {
                print_dim(out, &format!("   {}", seg));
                crlf(out);
                rows += 1;
            }
        }
    }
    if let Some(msg) = user_message {
        print_dim(out, &format!("   {msg}"));
        crlf(out);
        rows += 1;
    }
    if status != ToolStatus::Denied {
        if let Some(out_data) = output {
            rows += print_tool_output(out, name, &out_data.content, out_data.is_error, args, width);
        }
    }
    rows
}

fn render_confirm_result(
    out: &mut RenderOut,
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
    out: &mut RenderOut,
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
        .map(|d| format!("  {}", format_duration(d.as_secs())))
        .unwrap_or_default();
    let timeout_str = timeout_label
        .map(|l| format!(" ({})", l))
        .unwrap_or_default();
    let suffix_len = time_str.len() + timeout_str.len();
    let prefix_len = 3 + name.len() + 1;
    let max_summary = width.saturating_sub(prefix_len + suffix_len + 1);
    let truncated = truncate_str(summary, max_summary);
    print_dim(out, &format!(" {}", name));
    let _ = out.queue(Print(" "));
    if name == "bash" {
        print_highlighted_bash_line(out, &truncated);
    } else {
        let _ = out.queue(Print(&truncated));
    }
    if !time_str.is_empty() {
        print_dim(out, &time_str);
    }
    if !timeout_str.is_empty() {
        print_dim(out, &timeout_str);
    }
    crlf(out);
}

fn print_tool_output(
    out: &mut RenderOut,
    name: &str,
    content: &str,
    is_error: bool,
    args: &HashMap<String, serde_json::Value>,
    width: usize,
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
        "read_file" | "glob" | "grep" if !is_error => {
            let (s, p) = match name {
                "glob" => ("file", "files"),
                "grep" => ("match", "matches"),
                _ => ("line", "lines"),
            };
            print_dim_count(out, content.lines().count(), s, p)
        }
        "web_fetch" if !is_error => render_web_fetch_output(out, content, width),
        "edit_file" if !is_error => render_edit_output(out, args),
        "write_file" if !is_error => render_write_output(out, args),
        "ask_user_question" if !is_error => render_question_output(out, content, width),
        "exit_plan_mode" if !is_error => render_plan_output(out, content, width),
        "bash" | "read_process_output" | "stop_process" => {
            render_bash_output(out, content, is_error, width)
        }
        _ => render_default_output(out, content, is_error, width),
    }
}

fn print_dim(out: &mut RenderOut, text: &str) {
    let _ = out.queue(SetAttribute(Attribute::Dim));
    let _ = out.queue(Print(text));
    let _ = out.queue(SetAttribute(Attribute::Reset));
}

fn print_dim_count(out: &mut RenderOut, count: usize, singular: &str, plural: &str) -> u16 {
    print_dim(out, &format!("   {}", pluralize(count, singular, plural)));
    crlf(out);
    1
}

fn render_edit_output(out: &mut RenderOut, args: &HashMap<String, serde_json::Value>) -> u16 {
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
        print_inline_diff(out, old, new, path, new, 0, 0)
    }
}

fn render_write_output(out: &mut RenderOut, args: &HashMap<String, serde_json::Value>) -> u16 {
    let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
    let path = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
    print_syntax_file(out, content, path, 0, 0)
}

fn render_question_output(out: &mut RenderOut, content: &str, width: usize) -> u16 {
    let max_cols = width.saturating_sub(4);
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
            let combined = format!("{} {}", question, answer_str);
            for seg in &wrap_line(&combined, max_cols) {
                print_dim(out, &format!("   {}", seg));
                crlf(out);
                rows += 1;
            }
        }
    } else {
        for seg in &wrap_line(content, max_cols) {
            print_dim(out, &format!("   {}", seg));
            crlf(out);
            rows += 1;
        }
    }
    rows
}

fn render_web_fetch_output(out: &mut RenderOut, content: &str, width: usize) -> u16 {
    render_markdown(out, content.trim(), width, "   ", true)
}

fn render_markdown(
    out: &mut RenderOut,
    content: &str,
    width: usize,
    indent: &str,
    dim: bool,
) -> u16 {
    let max_cols = width.saturating_sub(indent.len() + 1);
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
            rows += render_code_block(out, code_lines, lang, width, dim);
            // Skip blank lines after code block — the border provides spacing.
            while i < lines.len() && lines[i].trim().is_empty() {
                i += 1;
            }
        } else if lines[i].trim_start().starts_with('|') {
            let table_start = i;
            while i < lines.len() && lines[i].trim_start().starts_with('|') {
                i += 1;
            }
            rows += render_markdown_table(out, &lines[table_start..i], dim);
        } else {
            // Skip blank lines before a code fence — the border provides spacing.
            let is_blank = lines[i].trim().is_empty();
            if is_blank {
                let mut j = i + 1;
                while j < lines.len() && lines[j].trim().is_empty() {
                    j += 1;
                }
                if j < lines.len() && lines[j].trim_start().starts_with("```") {
                    i = j;
                    continue;
                }
            }
            let segments = wrap_line(lines[i], max_cols);
            for seg in &segments {
                let _ = out.queue(Print(indent));
                print_styled_dim(out, seg, dim);
                crlf(out);
            }
            i += 1;
            rows += segments.len() as u16;
        }
    }
    rows
}

fn render_plan_output(out: &mut RenderOut, content: &str, width: usize) -> u16 {
    // Skip the first line ("Plan saved to ...") for the bordered display.
    let body = if let Some(pos) = content.find("\n\n") {
        &content[pos + 2..]
    } else {
        content
    };

    if body.is_empty() {
        return 0;
    }

    let inner_w = width.saturating_sub(6); // "   │ " prefix + " │" suffix area
    let mut rows = 0u16;

    // Top border: "   ┌─ Plan ─────┐"
    let label = " Plan ";
    let border_right = inner_w.saturating_sub(label.len() + 1);
    let _ = out.queue(SetForegroundColor(theme::accent()));
    let _ = out.queue(Print(format!(
        "   \u{250c}\u{2500}{label}{}",
        "\u{2500}".repeat(border_right)
    )));
    let _ = out.queue(ResetColor);
    crlf(out);
    rows += 1;

    // Body lines
    for line in body.lines() {
        let segments = wrap_line(line, inner_w);
        for seg in &segments {
            let char_count = seg.chars().count();
            let padding = inner_w.saturating_sub(char_count);
            let _ = out.queue(SetForegroundColor(theme::accent()));
            let _ = out.queue(Print("   \u{2502} "));
            let _ = out.queue(ResetColor);
            let _ = out.queue(SetAttribute(Attribute::Dim));
            let _ = out.queue(Print(seg));
            let _ = out.queue(SetAttribute(Attribute::Reset));
            let _ = out.queue(Print(" ".repeat(padding)));
            crlf(out);
            rows += 1;
        }
    }

    // Bottom border: "   └─────────────┘"
    let _ = out.queue(SetForegroundColor(theme::accent()));
    let _ = out.queue(Print(format!(
        "   \u{2514}{}",
        "\u{2500}".repeat(inner_w + 1)
    )));
    let _ = out.queue(ResetColor);
    crlf(out);
    rows += 1;

    rows
}

fn render_bash_output(out: &mut RenderOut, content: &str, is_error: bool, width: usize) -> u16 {
    const MAX_LINES: usize = 20;
    let max_cols = width.saturating_sub(4); // "   " prefix + 1 margin
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
        let line = &line.replace('\t', "    ");
        let segments = wrap_line(line, max_cols);
        for seg in &segments {
            if is_error {
                let _ = out.queue(SetForegroundColor(theme::TOOL_ERR));
                let _ = out.queue(Print(format!("   {}", seg)));
                let _ = out.queue(ResetColor);
            } else {
                print_dim(out, &format!("   {}", seg));
            }
            crlf(out);
            rows += 1;
        }
    }
    rows
}

fn render_default_output(out: &mut RenderOut, content: &str, is_error: bool, width: usize) -> u16 {
    let preview = result_preview(content, 3);
    let max_cols = width.saturating_sub(4);
    let mut rows = 0u16;
    for seg in &wrap_line(&preview, max_cols) {
        if is_error {
            let _ = out.queue(SetForegroundColor(theme::TOOL_ERR));
            let _ = out.queue(Print(format!("   {}", seg)));
            let _ = out.queue(ResetColor);
        } else {
            print_dim(out, &format!("   {}", seg));
        }
        crlf(out);
        rows += 1;
    }
    rows
}

fn pluralize(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("1 {}", singular)
    } else {
        format!("{} {}", count, plural)
    }
}

fn print_error(out: &mut RenderOut, msg: &str) {
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

pub(crate) fn print_styled_dim(out: &mut RenderOut, text: &str, dim: bool) {
    macro_rules! reset {
        () => {
            let _ = out.queue(SetAttribute(Attribute::Reset));
            let _ = out.queue(ResetColor);
            if dim {
                let _ = out.queue(SetAttribute(Attribute::Dim));
            }
        };
    }

    let trimmed = text.trim_start();
    if trimmed.starts_with('#') {
        let _ = out.queue(SetForegroundColor(theme::HEADING));
        let _ = out.queue(SetAttribute(Attribute::Bold));
        let _ = out.queue(Print(trimmed));
        reset!();
        return;
    }

    if trimmed.starts_with('>') {
        let content = trimmed.strip_prefix('>').unwrap().trim_start();
        let _ = out.queue(SetAttribute(Attribute::Dim));
        let _ = out.queue(SetAttribute(Attribute::Italic));
        let _ = out.queue(Print(content));
        reset!();
        return;
    }

    if dim {
        let _ = out.queue(SetAttribute(Attribute::Dim));
    }

    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut i = 0;
    let mut plain = String::new();

    while i < len {
        if i + 1 < len
            && chars[i] == '*'
            && chars[i + 1] == '*'
            && i + 2 < len
            && !chars[i + 2].is_whitespace()
            && (i == 0 || !chars[i - 1].is_alphanumeric())
        {
            // Look ahead to find closing **
            let mut j = i + 2;
            while j + 1 < len && !(chars[j] == '*' && chars[j + 1] == '*') {
                j += 1;
            }
            if j + 1 < len && !chars[j - 1].is_whitespace() {
                if !plain.is_empty() {
                    let _ = out.queue(Print(&plain));
                    plain.clear();
                }
                let word: String = chars[i + 2..j].iter().collect();
                let _ = out.queue(SetAttribute(Attribute::Bold));
                let _ = out.queue(Print(&word));
                reset!();
                i = j + 2;
                continue;
            }
        }

        if chars[i] == '*'
            && i + 1 < len
            && chars[i + 1] != '*'
            && !chars[i + 1].is_whitespace()
            && (i == 0 || !chars[i - 1].is_alphanumeric())
        {
            // Look ahead to find closing *
            let mut j = i + 1;
            while j < len && chars[j] != '*' {
                j += 1;
            }
            if j < len && !chars[j - 1].is_whitespace() {
                if !plain.is_empty() {
                    let _ = out.queue(Print(&plain));
                    plain.clear();
                }
                let word: String = chars[i + 1..j].iter().collect();
                let _ = out.queue(SetAttribute(Attribute::Italic));
                let _ = out.queue(Print(&word));
                reset!();
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
                if !plain.is_empty() {
                    let _ = out.queue(Print(&plain));
                    plain.clear();
                }
                let word: String = chars[i + 1..j].iter().collect();
                let _ = out.queue(SetForegroundColor(theme::accent()));
                if dim {
                    let _ = out.queue(SetAttribute(Attribute::Dim));
                }
                let _ = out.queue(Print(&word));
                reset!();
                i = j + 1;
                continue;
            }
        }

        plain.push(chars[i]);
        i += 1;
    }
    if !plain.is_empty() {
        let _ = out.queue(Print(&plain));
    }
    if dim {
        let _ = out.queue(SetAttribute(Attribute::Reset));
        let _ = out.queue(ResetColor);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: usize = 80;

    fn text(s: &str) -> Block {
        Block::Text {
            content: s.to_string(),
        }
    }

    fn user(s: &str) -> Block {
        Block::User {
            text: s.to_string(),
        }
    }

    fn thinking(s: &str) -> Block {
        Block::Thinking {
            content: s.to_string(),
        }
    }

    fn tool_call() -> Block {
        Block::ToolCall {
            name: "bash".into(),
            summary: "ls".into(),
            args: HashMap::new(),
            status: ToolStatus::Pending,
            elapsed: None,
            output: None,
            user_message: None,
        }
    }

    fn block_rows(block: &Block) -> u16 {
        let mut out = RenderOut::buffer();
        render_block(&mut out, block, W)
    }

    /// Compute total gap rows between the last history block and an active tool.
    fn tool_gap_for(blocks: &[Block]) -> u16 {
        blocks
            .last()
            .map(|b| gap_between(&Element::Block(b), &Element::ActiveTool))
            .unwrap_or(0)
    }

    /// Simulate the "all-at-once" render path: all blocks are unflushed,
    /// rendered in one pass, then active tool is appended.
    /// Returns (block_rows, tool_gap, total_before_tool).
    fn render_all_at_once(blocks: &[Block]) -> (u16, u16, u16) {
        let mut out = RenderOut::buffer();
        let mut total = 0u16;
        for i in 0..blocks.len() {
            let gap = if i > 0 {
                gap_between(&Element::Block(&blocks[i - 1]), &Element::Block(&blocks[i]))
            } else {
                0
            };
            let rows = render_block(&mut out, &blocks[i], W);
            total += gap + rows;
        }
        let tg = tool_gap_for(blocks);
        (total, tg, total + tg)
    }

    /// Simulate the "split" render path: blocks are flushed in one pass
    /// (render_pending_blocks), then the dialog frame renders the active
    /// tool separately. anchor_row = start + block_rows.
    /// In draw_frame(None), block_rows = 0 (all flushed), tool_gap is
    /// computed from last block. Returns (block_rows, tool_gap, total_before_tool).
    fn render_split(blocks: &[Block]) -> (u16, u16, u16) {
        // Phase 1: render_pending_blocks
        let mut out = RenderOut::buffer();
        let mut block_rows_total = 0u16;
        for i in 0..blocks.len() {
            let gap = if i > 0 {
                gap_between(&Element::Block(&blocks[i - 1]), &Element::Block(&blocks[i]))
            } else {
                0
            };
            let rows = render_block(&mut out, &blocks[i], W);
            block_rows_total += gap + rows;
        }
        // anchor_row = start_row + block_rows_total

        // Phase 2: draw_frame(None) — dialog mode
        // block_rows = 0 (all flushed)
        let tg = tool_gap_for(blocks);
        // Active tool rendered at anchor_row + tg
        // Total rows from start to tool = block_rows_total + tg
        (block_rows_total, tg, block_rows_total + tg)
    }

    /// Simulate a third path: blocks flushed across multiple draw_frame calls
    /// (each event gets its own tick), then dialog frame renders tool.
    /// Key difference: anchor_row is set by the LAST draw_frame(prompt) call,
    /// which uses anchor_row = top_row + block_rows. When blocks were flushed
    /// in a previous frame, block_rows = 0, so anchor_row = top_row.
    fn render_incremental(blocks: &[Block]) -> (u16, u16, u16) {
        // Each block arrives in a separate frame.
        // Frame N renders block N, prompt after it.
        // anchor_row = top_row + block_rows_in_this_frame.
        // For the LAST frame (that rendered the last block), anchor_row =
        // draw_start + (gap + rows of that block).
        // But draw_start for that frame = anchor from previous frame.
        //
        // Net effect: final anchor = sum of all block rows + gaps.
        // This is the same as render_split.
        let mut out = RenderOut::buffer();
        let mut cumulative = 0u16;
        for i in 0..blocks.len() {
            let gap = if i > 0 {
                gap_between(&Element::Block(&blocks[i - 1]), &Element::Block(&blocks[i]))
            } else {
                0
            };
            let rows = render_block(&mut out, &blocks[i], W);
            cumulative += gap + rows;
        }
        let tg = tool_gap_for(blocks);
        (cumulative, tg, cumulative + tg)
    }

    // ── The actual tests ────────────────────────────────────────────────

    #[test]
    fn text_then_tool_all_at_once() {
        let blocks = vec![user("hello"), text("I'll check that.")];
        let (_, tg, _) = render_all_at_once(&blocks);
        assert_eq!(tg, 1, "exactly 1 gap row between Text and ActiveTool");
    }

    #[test]
    fn text_then_tool_split() {
        let blocks = vec![user("hello"), text("I'll check that.")];
        let (_, tg, _) = render_split(&blocks);
        assert_eq!(
            tg, 1,
            "exactly 1 gap row between Text and ActiveTool (split)"
        );
    }

    #[test]
    fn all_paths_produce_same_total() {
        let blocks = vec![user("hello"), text("I'll check that.")];
        let a = render_all_at_once(&blocks);
        let b = render_split(&blocks);
        let c = render_incremental(&blocks);
        assert_eq!(a.2, b.2, "all-at-once vs split total must match");
        assert_eq!(b.2, c.2, "split vs incremental total must match");
    }

    #[test]
    fn thinking_text_tool_all_paths_match() {
        let blocks = vec![
            user("fix the bug"),
            thinking("Let me analyze..."),
            text("I'll fix it now."),
        ];
        let a = render_all_at_once(&blocks);
        let b = render_split(&blocks);
        let c = render_incremental(&blocks);
        assert_eq!(a.2, b.2, "all-at-once vs split");
        assert_eq!(b.2, c.2, "split vs incremental");
        assert_eq!(a.1, 1, "tool gap = 1");
    }

    #[test]
    fn empty_thinking_text_tool() {
        // Empty thinking block renders 0 rows but still exists in history.
        let blocks = vec![user("fix it"), thinking(""), text("Here's the fix.")];
        let a = render_all_at_once(&blocks);
        let b = render_split(&blocks);

        // The empty thinking block renders 0 rows.
        let thinking_rows = block_rows(&thinking(""));
        assert_eq!(thinking_rows, 0);

        // But gap_between still counts gaps around it:
        // User→Thinking = 1, Thinking→Text = 1
        // So there are 2 blank lines between User and Text.
        let user_thinking_gap = gap_between(
            &Element::Block(&user("fix it")),
            &Element::Block(&thinking("")),
        );
        let thinking_text_gap = gap_between(
            &Element::Block(&thinking("")),
            &Element::Block(&text("Here's the fix.")),
        );
        assert_eq!(user_thinking_gap, 1);
        assert_eq!(thinking_text_gap, 1);

        // But the gap from Text→ActiveTool should still be 1.
        assert_eq!(a.1, 1, "tool gap after text = 1");
        assert_eq!(a.2, b.2, "paths match with empty thinking");
    }

    #[test]
    fn text_with_trailing_newlines() {
        // LLM often sends trailing \n\n — trim_end handles it.
        let blocks = vec![user("hello"), text("response\n\n")];
        let trimmed_rows = block_rows(&text("response\n\n"));
        let clean_rows = block_rows(&text("response"));
        assert_eq!(
            trimmed_rows, clean_rows,
            "trailing newlines don't add rows (trim_end)"
        );

        let a = render_all_at_once(&blocks);
        let b = render_split(&blocks);
        assert_eq!(a.1, 1, "tool gap = 1");
        assert_eq!(a.2, b.2);
    }

    #[test]
    fn text_with_internal_blank_line() {
        // Text with internal blank line: "para1\n\npara2"
        let blocks = vec![user("hello"), text("para1\n\npara2")];
        let rows = block_rows(&text("para1\n\npara2"));
        assert_eq!(rows, 3, "3 rows: para1, blank, para2");

        let a = render_all_at_once(&blocks);
        let b = render_split(&blocks);
        assert_eq!(a.1, 1, "tool gap still 1");
        assert_eq!(a.2, b.2);
    }

    #[test]
    fn tool_call_then_text_then_tool() {
        // Multi-tool turn: first tool finished, then new text + new tool.
        let blocks = vec![
            user("do two things"),
            text("First task:"),
            tool_call(),
            text("Second task:"),
        ];
        let a = render_all_at_once(&blocks);
        let b = render_split(&blocks);
        assert_eq!(a.1, 1);
        assert_eq!(a.2, b.2);
    }

    #[test]
    fn empty_text_before_tool() {
        // What if the LLM sends empty text content?
        let blocks = vec![user("hello"), text("")];
        let rows = block_rows(&text(""));
        assert_eq!(rows, 0, "empty text renders 0 rows");

        let gap = gap_between(&Element::Block(&text("")), &Element::ActiveTool);
        assert_eq!(gap, 1, "gap is still 1 for empty text block");

        // This means: User(1 row) + gap(1) + Text(0 rows) + gap(1) = tool at offset 3
        // But visually the empty text is invisible, so it looks like 2 blank lines.
        // This could be the bug source!
        let a = render_all_at_once(&blocks);
        let b = render_split(&blocks);
        assert_eq!(a.2, b.2, "both paths match (even if wrong)");

        // Compare with blocks that DON'T have the empty text:
        let blocks_no_empty = vec![user("hello")];
        let c = render_all_at_once(&blocks_no_empty);
        // User→ActiveTool gap:
        let gap_user_tool = gap_between(&Element::Block(&user("hello")), &Element::ActiveTool);
        assert_eq!(gap_user_tool, 1, "User→ActiveTool = 1");

        // With empty text:  total = user_rows + 1(User→Text gap=0, Text→Text=0? no, User→Text)
        // Let me compute manually:
        let user_text_gap =
            gap_between(&Element::Block(&user("hello")), &Element::Block(&text("")));
        // User→anything = 1
        assert_eq!(user_text_gap, 1, "User→Text = 1");
        // text("")→ActiveTool = 1
        // So total: user_rows + 1(gap) + 0(empty text) + 1(gap) = user_rows + 2
        // vs without empty text: user_rows + 1(gap)
        // That's ONE EXTRA blank line when there's an empty text block!

        let diff = a.2 as i32 - c.2 as i32;
        // diff should be 1 if there's an extra gap from the empty text
        assert_eq!(diff, 1, "empty text block adds 1 extra gap row (the bug!)");
    }

    #[test]
    fn adjacent_text_blocks_gap() {
        // Two consecutive text blocks — gap should be 0 (Text→Text = _ => 0).
        let gap = gap_between(&Element::Block(&text("a")), &Element::Block(&text("b")));
        assert_eq!(gap, 0, "Text→Text gap = 0");
    }

    /// Simulate draw_frame anchor tracking across multiple frames.
    /// Returns the row offset where the active tool starts, relative to
    /// where the first block started rendering.
    ///
    /// `flushed_at` is the set of frame boundaries: blocks[0..flushed_at[0]]
    /// are rendered in frame 0, blocks[flushed_at[0]..flushed_at[1]] in
    /// frame 1, etc. The active tool renders in the final frame.
    fn tool_start_row(blocks: &[Block], flushed_at: &[usize]) -> u16 {
        let mut anchor: u16 = 0; // start of rendering
        let mut flushed: usize = 0;

        for &end in flushed_at {
            // This frame renders blocks[flushed..end]
            let mut frame_block_rows = 0u16;
            let mut out = RenderOut::buffer();
            for i in flushed..end {
                let gap = if i > 0 {
                    gap_between(&Element::Block(&blocks[i - 1]), &Element::Block(&blocks[i]))
                } else {
                    0
                };
                let rows = render_block(&mut out, &blocks[i], W);
                frame_block_rows += gap + rows;
            }
            // In non-dialog draw_frame: anchor_row = top_row + block_rows
            // where top_row = draw_start_row = previous anchor
            // So new anchor = anchor + frame_block_rows
            anchor += frame_block_rows;
            flushed = end;
        }

        // Final frame: dialog mode. All blocks flushed.
        // draw_start_row = anchor (from last frame)
        // block_rows = 0 (all flushed)
        // tool_gap = gap_between(last block, ActiveTool)
        let tg = tool_gap_for(blocks);
        // Tool renders at anchor + tg
        anchor + tg
    }

    #[test]
    fn anchor_tracking_single_frame() {
        // All blocks arrive together, single frame before dialog.
        let blocks = vec![user("hello"), text("response")];
        let row = tool_start_row(&blocks, &[2]);

        let user_rows = block_rows(&user("hello"));
        let text_rows = block_rows(&text("response"));
        let expected = user_rows + 1 /* User→Text */ + text_rows + 1 /* Text→Tool */;
        assert_eq!(row, expected);
    }

    #[test]
    fn anchor_tracking_split_frames() {
        // User flushed in frame 0, Text in frame 1, then dialog.
        let blocks = vec![user("hello"), text("response")];
        let row = tool_start_row(&blocks, &[1, 2]);

        let user_rows = block_rows(&user("hello"));
        let text_rows = block_rows(&text("response"));
        let expected = user_rows + 1 /* User→Text */ + text_rows + 1 /* Text→Tool */;
        assert_eq!(row, expected);
    }

    #[test]
    fn anchor_tracking_each_block_separate() {
        // Each block flushed in its own frame.
        let blocks = vec![user("hello"), text("response")];
        let row = tool_start_row(&blocks, &[1, 2]);

        // Same as single frame — the math should be identical.
        let single = tool_start_row(&blocks, &[2]);
        assert_eq!(row, single, "split and single-frame anchors must match");
    }

    #[test]
    fn anchor_tracking_with_empty_thinking() {
        let blocks = vec![user("hi"), thinking(""), text("fix")];

        let single = tool_start_row(&blocks, &[3]);
        let split = tool_start_row(&blocks, &[1, 2, 3]);
        assert_eq!(single, split, "empty thinking: single vs split must match");

        // Without the empty thinking:
        let blocks_no_thinking = vec![user("hi"), text("fix")];
        let no_thinking = tool_start_row(&blocks_no_thinking, &[2]);

        // The empty thinking adds 1 extra row (its gap before text).
        assert_eq!(
            single - no_thinking,
            1,
            "empty thinking adds exactly 1 extra row"
        );
    }

    #[test]
    fn anchor_tracking_with_thinking() {
        let blocks = vec![user("hi"), thinking("let me think"), text("fix")];

        let single = tool_start_row(&blocks, &[3]);
        let split_2 = tool_start_row(&blocks, &[1, 3]);
        let split_3 = tool_start_row(&blocks, &[1, 2, 3]);
        assert_eq!(single, split_2, "single vs 2-split");
        assert_eq!(single, split_3, "single vs 3-split");
    }

    #[test]
    fn empty_thinking_adds_extra_gap() {
        // Empty thinking between user and text adds 2 gaps for 0 visible rows.
        let with_empty_thinking = vec![user("hi"), thinking(""), text("response")];
        let without_thinking = vec![user("hi"), text("response")];

        let a = render_all_at_once(&with_empty_thinking);
        let b = render_all_at_once(&without_thinking);

        // Gap accounting:
        // With: User(N) + 1(User→Thinking) + 0(empty) + 1(Thinking→Text) + M(Text) = N+M+2
        // Without: User(N) + 1(User→Text) + M(Text) = N+M+1
        let diff = a.2 as i32 - b.2 as i32;
        assert_eq!(
            diff, 1,
            "empty thinking adds 1 extra gap row before text content"
        );
    }
}
