use crate::theme;
use crate::utils::format_duration;
use engine::tools::NotebookRenderData;
use std::collections::HashMap;
use std::time::Duration;

use super::display::{ColorRole, ColorValue, DisplayBlock, NamedColor, SpanStyle};
use super::highlight::{
    print_cached_inline_diff, print_inline_diff, print_syntax_file, print_syntax_file_ext,
    render_code_block, render_markdown_table, strip_markdown_markers, BashHighlighter,
};
use super::history::ViewState;
use super::layout_out::{LayoutSink, SpanCollector};
use super::{
    truncate_str, wrap_line, ApprovalScope, Block, ConfirmChoice, LayoutContext, ToolOutput,
    ToolState, ToolStatus,
};

/// Cap on the number of rows a single tool block contributes to the
/// overlay or scrollback. Applied separately to:
///
/// - the bash command summary header (`print_tool_line`), which shows
///   the first N command lines + a "... M below" indicator,
/// - the tool output body (`render_wrapped_output`), which shows the
///   last N output lines + a "... M above" indicator.
///
/// 20 keeps a single tool block visually contained even on small
/// terminals; the overlay tail-crop handles the rare case where two
/// or more capped tools still don't fit.
const MAX_TOOL_BLOCK_ROWS: usize = 20;

/// Default number of lines shown in non-bash tool result previews
/// (`render_default_output`). Joined with " | " separators.
const DEFAULT_PREVIEW_LINES: usize = 3;

/// Layout entry point: produce a `DisplayBlock` for `block` at the given
/// width. Drives the per-variant renderers against a `SpanCollector` and
/// hands the resulting span tree off to the cache / paint stage.
///
/// `state` must be `Some(_)` for `Block::ToolCall` and is unused for every
/// other variant.
pub(super) fn layout_block(
    block: &Block,
    state: Option<&ToolState>,
    ctx: &LayoutContext,
) -> DisplayBlock {
    let width = ctx.width as usize;
    let show_thinking = ctx.show_thinking;
    let mut col = SpanCollector::new(ctx.width);
    render_block(&mut col, block, state, width, show_thinking);
    let mut display = col.finish();
    apply_view_state(&mut display, ctx.view_state);
    display
}

/// Truncate / collapse the laid-out block according to its view state.
/// Runs post-layout so every block variant gets the same treatment.
fn apply_view_state(display: &mut super::display::DisplayBlock, state: ViewState) {
    use super::display::{ColorRole, ColorValue, DisplayLine, DisplaySpan, SpanStyle};
    let total = display.lines.len();
    let ellipsis_line = |text: String| -> DisplayLine {
        DisplayLine {
            spans: vec![DisplaySpan {
                text,
                style: SpanStyle {
                    fg: Some(ColorValue::Role(ColorRole::Muted)),
                    dim: true,
                    ..SpanStyle::default()
                },
                meta: Default::default(),
            }],
            fill_bg: None,
            fill_right_margin: 0,
        }
    };
    match state {
        ViewState::Expanded => {}
        ViewState::Collapsed => {
            if total > 1 {
                let hidden = total - 1;
                display.lines.truncate(1);
                display
                    .lines
                    .push(ellipsis_line(format!("… {hidden} more lines")));
            }
        }
        ViewState::TrimmedHead { keep } => {
            let keep = keep as usize;
            if total > keep {
                let hidden = total - keep;
                display.lines.truncate(keep);
                display
                    .lines
                    .push(ellipsis_line(format!("… {hidden} more lines")));
            }
        }
        ViewState::TrimmedTail { keep } => {
            let keep = keep as usize;
            if total > keep {
                let hidden = total - keep;
                let tail = display.lines.split_off(total - keep);
                display.lines.clear();
                display
                    .lines
                    .push(ellipsis_line(format!("… {hidden} more lines above")));
                display.lines.extend(tail);
            }
        }
    }
}

/// Animated trailing dots for streaming indicators.
pub(super) fn animated_dots() -> &'static str {
    let n = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_millis()
        / 333) as usize
        % 3
        + 1;
    &"..."[..n]
}

/// Concatenate trailing `Block::Thinking` content from the end of a block
/// iterator. The iterator must yield blocks in forward order.
pub(super) fn collect_trailing_thinking<'a, I>(blocks: I) -> String
where
    I: IntoIterator<Item = &'a super::Block>,
    I::IntoIter: DoubleEndedIterator,
{
    let parts: Vec<&str> = blocks
        .into_iter()
        .rev()
        .map_while(|b| match b {
            super::Block::Thinking { content } => Some(content.as_str()),
            _ => None,
        })
        .collect();
    // Parts are in reverse order — join forward.
    let mut out = String::new();
    for part in parts.into_iter().rev() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(part);
    }
    out
}

/// Extract a title and non-empty line count from thinking content.
/// If the first non-empty line is a markdown bold title (`**...**`), use it as the label.
pub(super) fn thinking_summary(content: &str) -> (String, usize) {
    let mut label = None;
    let mut lines = 0usize;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        lines += 1;
        if label.is_none()
            && trimmed.starts_with("**")
            && trimmed.ends_with("**")
            && trimmed.len() > 4
        {
            label = Some(trimmed[2..trimmed.len() - 2].trim().to_string());
        }
    }
    (label.unwrap_or_else(|| "thinking".to_string()), lines)
}

/// Render a single hidden-thinking summary row with optional animated dots.
pub(super) fn render_thinking_summary<S: LayoutSink>(
    out: &mut S,
    width: usize,
    label: &str,
    line_count: usize,
    animated: bool,
) -> u16 {
    let dots = if animated { animated_dots() } else { "" };
    let summary = format!("{label} ({}){dots}", pluralize(line_count, "line", "lines"));
    let max_cols = width.saturating_sub(4).max(1);
    let segs = wrap_line(&summary, max_cols);
    if segs.len() > 1 {
        out.mark_wrapped();
    }
    let mut rows = 0u16;
    for seg in &segs {
        out.set_dim_italic();
        out.print_string(format!(" \u{2502} {}", seg));
        out.reset_style();
        out.newline();
        rows += 1;
    }
    rows
}

/// Element types for spacing calculation.
pub(super) enum Element<'a> {
    Block(&'a Block),
}

/// Number of blank lines to insert between two adjacent elements.
pub(super) fn gap_between(above: &Element, below: &Element) -> u16 {
    match (above, below) {
        // CodeLine→CodeLine: no gap (consecutive lines in same block).
        (Element::Block(Block::CodeLine { .. }), Element::Block(Block::CodeLine { .. })) => {
            return 0
        }
        // Transitions into/out of code lines need a blank line,
        // except after headings (headings have no trailing gap).
        (Element::Block(Block::CodeLine { .. }), _) => return 1,
        (Element::Block(Block::Text { content }), Element::Block(Block::CodeLine { .. })) => {
            let last_line = content.lines().last().unwrap_or("");
            if last_line.trim_start().starts_with('#') {
                return 0;
            }
            return 1;
        }
        (_, Element::Block(Block::CodeLine { .. })) => return 1,
        _ => {}
    }
    match (above, below) {
        (Element::Block(Block::User { .. }), _) => 1,
        (_, Element::Block(Block::User { .. })) => 1,
        (Element::Block(Block::Exec { .. }), _) => 1,
        (_, Element::Block(Block::Exec { .. })) => 1,
        (Element::Block(Block::ToolCall { .. }), Element::Block(Block::ToolCall { .. })) => 1,
        (Element::Block(Block::Text { .. }), Element::Block(Block::ToolCall { .. })) => 1,
        (Element::Block(Block::Thinking { .. }), Element::Block(Block::Thinking { .. })) => 0,
        (_, Element::Block(Block::Thinking { .. })) => 1,
        (Element::Block(Block::Thinking { .. }), _) => 1,
        (Element::Block(Block::ToolCall { .. }), Element::Block(Block::Text { .. })) => 1,
        (Element::Block(Block::Hint { .. }), _) => 1,
        (_, Element::Block(Block::Hint { .. })) => 1,
        (_, Element::Block(Block::Compacted { .. })) => 1,
        (Element::Block(Block::Compacted { .. }), _) => 1,
        (_, Element::Block(Block::AgentMessage { .. })) => 1,
        (Element::Block(Block::AgentMessage { .. }), _) => 1,
        (_, Element::Block(Block::Agent { .. })) => 1,
        (Element::Block(Block::Agent { .. }), _) => 1,
        // Text→Text: 1 gap (paragraph spacing), except when the previous
        // text block ends with a markdown heading — headings do not get a
        // trailing blank line.
        (Element::Block(Block::Text { content }), Element::Block(Block::Text { .. })) => {
            let last_line = content.lines().last().unwrap_or("");
            if last_line.trim_start().starts_with('#') {
                0
            } else {
                1
            }
        }
        _ => 0,
    }
}

pub(super) fn render_block<S: LayoutSink>(
    out: &mut S,
    block: &Block,
    state: Option<&ToolState>,
    width: usize,
    show_thinking: bool,
) -> u16 {
    let _perf = match block {
        Block::User { .. } => crate::perf::begin("render:user"),
        Block::Thinking { .. } => crate::perf::begin("render:thinking"),
        Block::Text { .. } => crate::perf::begin("render:text"),
        Block::CodeLine { .. } => crate::perf::begin("render:code_line"),
        Block::ToolCall { .. } => crate::perf::begin("render:tool_call"),
        Block::Confirm { .. } => crate::perf::begin("render:confirm"),
        Block::Hint { .. } => crate::perf::begin("render:hint"),
        Block::Compacted { .. } => crate::perf::begin("render:compacted"),
        Block::Exec { .. } => crate::perf::begin("render:exec"),
        Block::AgentMessage { .. } => crate::perf::begin("render:agent_msg"),
        Block::Agent { .. } => crate::perf::begin("render:agent"),
    };
    match block {
        Block::User { text, image_labels } => {
            let is_command = crate::completer::Completer::is_command(text.trim());
            // Each rendered row is: " " (1-char prefix) + content + trailing padding.
            // `text_w` is the max content chars per row so the total never reaches
            // the terminal width (which would cause an implicit wrap).
            // Window-level `pad_left` now paints the leading gutter cell;
            // the User block only needs to reserve a trailing cell inside
            // the content rect for its styled background bar.
            let text_w = width.saturating_sub(1).max(1);
            let all_lines: Vec<String> = text.lines().map(|l| l.replace('\t', "    ")).collect();
            // Strip leading/trailing blank lines but preserve internal structure.
            let start = all_lines.iter().position(|l| !l.is_empty()).unwrap_or(0);
            let end = all_lines
                .iter()
                .rposition(|l| !l.is_empty())
                .map_or(0, |i| i + 1);
            let logical_lines: Vec<String> = all_lines[start..end]
                .iter()
                .map(|l| l.trim_end().to_string())
                .collect();
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
            let user_bg = ColorValue::Role(ColorRole::UserBg);
            let mut rows = 0u16;
            for logical_line in &logical_lines {
                if logical_line.is_empty() {
                    let fill = if block_w > 0 { block_w } else { 1 };
                    out.set_bg(user_bg);
                    out.print_string(" ".repeat(fill));
                    out.reset_style();
                    out.newline();
                    rows += 1;
                    continue;
                }
                let chunks = wrap_line(logical_line, text_w);
                if chunks.len() > 1 {
                    out.mark_wrapped();
                }
                for chunk in &chunks {
                    let chunk_len = chunk.chars().count();
                    let trailing = if block_w > 0 {
                        block_w.saturating_sub(chunk_len)
                    } else {
                        1
                    };
                    out.set_bg(user_bg);
                    out.set_bold();
                    print_user_highlights(out, chunk, image_labels, is_command);
                    out.print_string(" ".repeat(trailing));
                    out.reset_style();
                    out.newline();
                    rows += 1;
                }
            }
            rows
        }
        Block::Thinking { content } => {
            if !show_thinking {
                let (label, line_count) = thinking_summary(content);
                return render_thinking_summary(out, width, &label, line_count, false);
            }
            let max_cols = width.saturating_sub(3).max(1); // "│ " prefix + 1 margin
            let mut rows = 0u16;
            for line in content.lines() {
                let segments = wrap_line(line, max_cols);
                if segments.len() > 1 {
                    out.mark_wrapped();
                }
                for seg in &segments {
                    out.set_dim_italic();
                    out.print_string(format!("│ {}", seg));
                    out.reset_style();
                    out.newline();
                    rows += 1;
                }
            }
            rows
        }
        Block::Text { content } => render_markdown_inner(out, content, width, "", false, None),
        Block::CodeLine { content, lang } => {
            render_code_block(out, &[content.as_str()], lang, width, false, None)
        }
        Block::ToolCall {
            call_id,
            name,
            summary,
            args,
        } => {
            let state = state.expect("ToolCall layout requires ToolState");
            render_tool(
                out,
                call_id,
                name,
                summary,
                args,
                state.status,
                state.elapsed,
                state.output.as_deref(),
                state.user_message.as_deref(),
                width,
            )
        }
        Block::Confirm { tool, desc, choice } => {
            render_confirm_result(out, tool, desc, choice.clone(), width)
        }
        Block::Hint { content } => {
            out.push_dim_italic();
            out.print(content);
            out.pop_style();
            out.newline();
            1
        }
        Block::Compacted { summary } => {
            let label = " compacted ";
            let label_len = label.len();
            let remaining = width.saturating_sub(label_len);
            let left = remaining / 2;
            let right = remaining - left;
            out.push_dim();
            out.print_string("─".repeat(left));
            out.print(label);
            out.print_string("─".repeat(right));
            out.pop_style();
            out.newline();
            1 + render_markdown_inner(out, summary, width, "", true, None)
        }
        Block::Exec { command, output } => {
            let char_len = command.chars().count() + 1;
            let pad_width = (char_len + 2).min(width);
            let trailing = pad_width.saturating_sub(char_len + 1);
            out.push_style(SpanStyle {
                bg: Some(ColorValue::Role(ColorRole::UserBg)),
                fg: Some(theme::EXEC.into()),
                bold: true,
                ..Default::default()
            });
            out.print("!");
            out.set_fg(ColorValue::Named(NamedColor::Reset));
            out.print_string(format!("{}{}", command, " ".repeat(trailing)));
            out.pop_style();
            out.newline();
            let mut rows = 1u16;
            if !output.is_empty() {
                rows += render_wrapped_output(out, output, false, width);
            }
            rows
        }
        Block::AgentMessage {
            from_id,
            from_slug: _,
            content,
        } => {
            let header = format!("➜ {from_id}");
            out.push_style(SpanStyle {
                fg: Some(crate::theme::AGENT.into()),
                bold: true,
                ..Default::default()
            });
            out.print(&header);
            out.pop_style();
            out.newline();
            let bctx = super::BoxContext {
                left: "\u{2502} ",
                right: "",
                color: crate::theme::AGENT.into(),
                inner_w: width.saturating_sub(3),
            };
            1 + render_markdown_inner(out, content, width, bctx.left, true, Some(&bctx))
        }
        Block::Agent {
            agent_id,
            slug,
            blocking,
            tool_calls,
            status,
            elapsed,
        } => render_agent_block(
            out,
            agent_id,
            slug.as_deref(),
            *blocking,
            tool_calls,
            *status,
            *elapsed,
            width,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn render_agent_block<S: LayoutSink>(
    out: &mut S,
    agent_id: &str,
    slug: Option<&str>,
    blocking: bool,
    tool_calls: &[crate::app::AgentToolEntry],
    status: super::AgentBlockStatus,
    elapsed: Option<Duration>,
    width: usize,
) -> u16 {
    use super::AgentBlockStatus;
    let mut rows = 0u16;

    // Header: " + agent_id · slug [✓/✗] [elapsed]"
    out.push_style(SpanStyle {
        fg: Some(crate::theme::AGENT.into()),
        bold: true,
        ..Default::default()
    });
    out.print_string(format!("+ {agent_id}"));

    if !blocking {
        out.push_fg(ColorValue::Role(ColorRole::Muted));
        out.print(" started");
        out.pop_style(); // muted fg
        out.pop_style(); // bold+agent fg
        out.newline();
        return rows + 1;
    }

    if let Some(slug) = slug {
        out.push_dim();
        out.print_string(format!(" \u{00b7} {slug}"));
        out.pop_style();
    }

    match status {
        AgentBlockStatus::Done => {
            out.push_fg(crate::theme::SUCCESS.into());
            out.print(" \u{2713}"); // ✓
            out.pop_style();
        }
        AgentBlockStatus::Error => {
            out.push_fg(crate::theme::ERROR.into());
            out.print(" \u{2717}"); // ✗
            out.pop_style();
        }
        AgentBlockStatus::Running => {}
    }

    if let Some(d) = elapsed {
        if d.as_secs_f64() >= 0.1 {
            out.push_style(SpanStyle {
                fg: Some(ColorValue::Role(ColorRole::Muted)),
                dim: true,
                ..Default::default()
            });
            out.print_string(format!("  {}", format_duration(d.as_secs())));
            out.pop_style();
        }
    }

    out.pop_style(); // bold+agent fg
    out.newline();
    rows += 1;

    // Blocking: show last 3 tool calls with left border.
    let visible = tool_calls.iter().rev().take(3).collect::<Vec<_>>();
    for entry in visible.iter().rev() {
        out.push_fg(crate::theme::AGENT.into());
        out.print("\u{2502} "); // │
        out.pop_style();

        out.push_dim();
        out.print(&entry.tool_name);
        out.pop_style();

        // Reserve space for elapsed time so the summary doesn't push it off-screen.
        let time_str = entry
            .elapsed
            .filter(|d| d.as_secs_f64() >= 0.1)
            .map(|d| format!("  {}", format_duration(d.as_secs())));
        let time_w = time_str.as_ref().map_or(0, |s| s.len());
        // 6 = " │ " (3) + space before summary (1) + padding (2)
        let max_summary = width.saturating_sub(6 + entry.tool_name.len() + time_w);
        let summary = truncate_str(&entry.summary, max_summary);
        out.print_string(format!(" {summary}"));

        if let Some(ref ts) = time_str {
            out.push_dim();
            out.print(ts);
            out.pop_style();
        }

        out.newline();
        rows += 1;
    }

    // Bottom border
    let border_w = width.saturating_sub(1);
    out.push_fg(crate::theme::AGENT.into());
    out.print_string(format!("\u{2570}{}", "\u{2500}".repeat(border_w)));
    out.pop_style();
    out.newline();
    rows += 1;

    rows
}

#[allow(clippy::too_many_arguments)]
pub(super) fn render_tool<S: LayoutSink>(
    out: &mut S,
    _call_id: &str,
    name: &str,
    summary: &str,
    args: &HashMap<String, serde_json::Value>,
    status: ToolStatus,
    elapsed: Option<Duration>,
    output: Option<&ToolOutput>,
    user_message: Option<&str>,
    width: usize,
) -> u16 {
    let color: ColorValue = match status {
        ToolStatus::Ok => theme::SUCCESS.into(),
        ToolStatus::Err | ToolStatus::Denied => theme::ERROR.into(),
        ToolStatus::Confirm => ColorValue::Role(ColorRole::Accent),
        ToolStatus::Pending => ColorValue::Role(ColorRole::ToolPending),
    };
    let time = if matches!(
        name,
        "bash" | "web_fetch" | "read_process_output" | "stop_process" | "peek_agent"
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
    let mut rows = print_tool_line(out, name, summary, color, time, tl.as_deref(), width);
    if name == "web_fetch" {
        if let Some(prompt) = args.get("prompt").and_then(|v| v.as_str()) {
            let segs = wrap_line(prompt, width.saturating_sub(3));
            if segs.len() > 1 {
                out.mark_wrapped();
            }
            for seg in &segs {
                print_dim(out, &format!("  {}", seg));
                out.newline();
                rows += 1;
            }
        }
    }
    if let Some(msg) = user_message {
        print_dim(out, &format!("  {msg}"));
        out.newline();
        rows += 1;
    }
    if status != ToolStatus::Denied {
        if let Some(out_data) = output {
            if !out_data.content.is_empty() {
                rows += print_tool_output(out, name, out_data, args, width);
            }
        }
    }
    rows
}

fn render_confirm_result<S: LayoutSink>(
    out: &mut S,
    tool: &str,
    desc: &str,
    choice: Option<ConfirmChoice>,
    width: usize,
) -> u16 {
    let mut rows = 2u16;

    out.push_fg(theme::APPLY.into());
    out.print("  allow? ");
    out.pop_style();
    print_dim(out, tool);
    out.newline();

    let prefix = "  \u{2502} ";
    let prefix_len = prefix.chars().count();
    let segments = wrap_line(desc, width.saturating_sub(prefix_len));
    if segments.len() > 1 {
        out.mark_wrapped();
    }
    for (i, seg) in segments.iter().enumerate() {
        if i == 0 {
            print_dim(out, prefix);
        } else {
            out.print_string(" ".repeat(prefix_len));
        }
        out.print(seg);
        out.newline();
    }
    rows += segments.len().saturating_sub(1) as u16;

    if let Some(c) = choice {
        rows += 1;
        out.print("  ");
        match c {
            ConfirmChoice::Yes | ConfirmChoice::YesAutoApply => {
                print_dim(out, "approved");
            }
            ConfirmChoice::Always(scope) => {
                let suffix = if scope == ApprovalScope::Workspace {
                    " (workspace)"
                } else {
                    ""
                };
                print_dim(out, &format!("always{suffix}"));
            }
            ConfirmChoice::AlwaysPatterns(ref pats, scope) => {
                let suffix = if scope == ApprovalScope::Workspace {
                    " workspace"
                } else {
                    ""
                };
                print_dim(out, &format!("always{suffix} ({})", pats.join(", ")));
            }
            ConfirmChoice::AlwaysDir(ref dir, scope) => {
                let suffix = if scope == ApprovalScope::Workspace {
                    " workspace"
                } else {
                    ""
                };
                print_dim(out, &format!("always{suffix} (dir: {dir})"));
            }
            ConfirmChoice::No => {
                out.push_fg(theme::ERROR.into());
                out.print("denied");
                out.pop_style();
            }
        }
        out.newline();
    }
    rows
}

/// Layout metrics for a tool header line.
struct ToolLineLayout {
    prefix_len: usize,
    max_summary: usize,
}

fn tool_line_layout(name: &str, suffix_len: usize, width: usize) -> ToolLineLayout {
    let prefix_len = 2 + name.len() + 1; // "⏺ " + name + " "
    let max_summary = width.saturating_sub(prefix_len + suffix_len + 1);
    ToolLineLayout {
        prefix_len,
        max_summary,
    }
}

fn print_tool_line<S: LayoutSink>(
    out: &mut S,
    name: &str,
    summary: &str,
    pill_color: ColorValue,
    elapsed: Option<Duration>,
    timeout_label: Option<&str>,
    width: usize,
) -> u16 {
    out.push_fg(pill_color);
    out.print("\u{23fa}");
    out.pop_style();
    let time_str = elapsed
        .filter(|d| d.as_secs_f64() >= 0.1)
        .map(|d| format!("  {}", format_duration(d.as_secs())))
        .unwrap_or_default();
    let timeout_str = timeout_label
        .map(|l| format!(" ({})", l))
        .unwrap_or_default();
    let suffix_len = time_str.len() + timeout_str.len();
    let ly = tool_line_layout(name, suffix_len, width);

    print_dim(out, &format!(" {} ", name));

    if name == "bash" {
        let raw_lines: Vec<&str> = summary.lines().collect();
        let mut wrapped: Vec<String> = Vec::new();
        for line in &raw_lines {
            let segs = wrap_line(line, ly.max_summary.max(1));
            if segs.len() > 1 {
                out.mark_wrapped();
            }
            wrapped.extend(segs);
        }
        let total = wrapped.len();
        let show = total.min(MAX_TOOL_BLOCK_ROWS);
        let mut line_num = 0;
        let mut bh = BashHighlighter::new();

        for (idx, seg) in wrapped[..show].iter().enumerate() {
            if idx > 0 {
                out.print_string(" ".repeat(ly.prefix_len));
            }
            bh.print_line(out, seg);
            if idx == 0 {
                if !time_str.is_empty() {
                    print_dim(out, &time_str);
                }
                if !timeout_str.is_empty() {
                    print_dim(out, &timeout_str);
                }
            }
            out.newline();
            line_num += 1;
        }

        if total > MAX_TOOL_BLOCK_ROWS {
            let skipped = total - MAX_TOOL_BLOCK_ROWS;
            out.print_string(" ".repeat(ly.prefix_len));
            print_dim(
                out,
                &format!("... {} below", pluralize(skipped, "line", "lines")),
            );
            out.newline();
            line_num += 1;
        }

        return line_num as u16;
    }

    let truncated = truncate_str(summary, ly.max_summary);
    if matches!(name, "message_agent" | "stop_agent" | "peek_agent") {
        print_agent_summary(out, &truncated);
    } else {
        out.print(&truncated);
    }
    if !time_str.is_empty() {
        print_dim(out, &time_str);
    }
    if !timeout_str.is_empty() {
        print_dim(out, &timeout_str);
    }
    out.newline();
    1
}

/// Print an agent tool summary: color leading agent name tokens, print the
/// rest as plain text. Agent names are single words (no spaces) optionally
/// separated by ", ". The first token that contains a space or follows a
/// non-comma separator marks the start of the plain-text portion.
fn print_agent_summary<S: LayoutSink>(out: &mut S, summary: &str) {
    // Find where agent names end: consume "word(, word)*" prefix.
    let mut end = 0;
    let mut rest = summary;
    loop {
        let trimmed = rest.trim_start();
        let skipped = rest.len() - trimmed.len();
        let word_end = trimmed.find([' ', ',']).unwrap_or(trimmed.len());
        if word_end == 0 {
            break;
        }
        end += skipped + word_end;
        rest = &trimmed[word_end..];
        // If followed by ", " consume the separator and continue.
        if rest.starts_with(", ") {
            end += 2;
            rest = &rest[2..];
        } else {
            break;
        }
    }
    if end > 0 {
        let names = &summary[..end];
        for (i, name) in names.split(", ").enumerate() {
            if i > 0 {
                out.print(", ");
            }
            out.push_fg(theme::AGENT.into());
            out.print(name.trim());
            out.pop_style();
        }
    }
    let tail = &summary[end..];
    if !tail.is_empty() {
        out.print(tail);
    }
}

fn print_tool_output<S: LayoutSink>(
    out: &mut S,
    name: &str,
    output: &ToolOutput,
    args: &HashMap<String, serde_json::Value>,
    width: usize,
) -> u16 {
    let content = &output.content;
    let is_error = output.is_error;
    match name {
        "web_search" if !is_error => {
            let mut count = 0u16;
            for line in content.lines() {
                if let Some(pos) = line.find(". ") {
                    let prefix = &line[..pos];
                    if !prefix.is_empty() && prefix.chars().all(|c| c.is_ascii_digit()) {
                        let title = &line[pos + 2..];
                        print_dim(out, &format!("   {title}"));
                        out.newline();
                        count += 1;
                    }
                }
            }
            if count == 0 {
                print_dim(out, "   No results found");
                out.newline();
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
        "web_fetch" if !is_error => print_dim_count(out, content.lines().count(), "line", "lines"),
        "edit_file" if !is_error => render_edit_output(out, output, args),
        "write_file" if !is_error => render_write_output(out, args),
        "edit_notebook" if !is_error => render_notebook_output(out, output, width),
        "ask_user_question" if !is_error => render_question_output(out, content, width),
        "exit_plan_mode" if !is_error => render_plan_output(out, args, width),
        "bash" | "read_process_output" | "stop_process" => {
            render_wrapped_output(out, content, is_error, width)
        }
        "peek_agent" if !is_error => render_wrapped_output(out, content, false, width),
        "list_agents" | "message_agent" | "stop_agent" | "spawn_agent" if !is_error => {
            let mut rows = 0u16;
            for line in content.lines() {
                print_dim(out, &format!("   {line}"));
                out.newline();
                rows += 1;
            }
            rows.max(1)
        }
        _ => render_default_output(out, content, is_error, width),
    }
}

fn print_dim<S: LayoutSink>(out: &mut S, text: &str) {
    out.push_dim();
    out.print(text);
    out.pop_style();
}

fn print_dim_count<S: LayoutSink>(out: &mut S, count: usize, singular: &str, plural: &str) -> u16 {
    print_dim(out, &format!("   {}", pluralize(count, singular, plural)));
    out.newline();
    1
}

fn render_edit_output<S: LayoutSink>(
    out: &mut S,
    output: &ToolOutput,
    args: &HashMap<String, serde_json::Value>,
) -> u16 {
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
    } else if let Some(crate::render::ToolOutputRenderCache::InlineDiff(cache)) =
        output.render_cache.as_ref()
    {
        print_cached_inline_diff(out, cache, 0, 0)
    } else {
        print_inline_diff(out, old, new, path, new, 0, 0)
    }
}

fn render_write_output<S: LayoutSink>(
    out: &mut S,
    args: &HashMap<String, serde_json::Value>,
) -> u16 {
    let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
    let path = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
    print_syntax_file(out, content, path, 0, 0)
}

fn render_notebook_output<S: LayoutSink>(out: &mut S, output: &ToolOutput, width: usize) -> u16 {
    let Some(meta) = output.metadata.as_ref() else {
        return render_default_output(out, &output.content, output.is_error, width);
    };
    let Ok(data) = serde_json::from_value::<NotebookRenderData>(meta.clone()) else {
        return render_default_output(out, &output.content, output.is_error, width);
    };

    let mut rows = 0u16;
    print_dim(out, &format!("   {}", data.title()));
    out.newline();
    rows += 1;

    if data.edit_mode == "insert" {
        rows += print_syntax_file_ext(
            out,
            &data.new_source,
            &data.path,
            Some(data.syntax_ext()),
            0,
            0,
        );
    } else if let Some(crate::render::ToolOutputRenderCache::NotebookEdit(ref nb)) =
        output.render_cache
    {
        if let Some(ref cache) = nb.diff {
            rows += print_cached_inline_diff(out, cache, 0, 0);
        } else {
            rows += print_inline_diff(
                out,
                &data.old_source,
                &data.new_source,
                &data.path,
                &data.old_source,
                0,
                0,
            );
        }
    } else {
        rows += print_inline_diff(
            out,
            &data.old_source,
            &data.new_source,
            &data.path,
            &data.old_source,
            0,
            0,
        );
    }
    rows
}

fn render_question_output<S: LayoutSink>(out: &mut S, content: &str, width: usize) -> u16 {
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
            let segs = wrap_line(&combined, max_cols);
            if segs.len() > 1 {
                out.mark_wrapped();
            }
            for seg in &segs {
                print_dim(out, &format!("   {}", seg));
                out.newline();
                rows += 1;
            }
        }
    } else {
        let segs = wrap_line(content, max_cols);
        if segs.len() > 1 {
            out.mark_wrapped();
        }
        for seg in &segs {
            print_dim(out, &format!("   {}", seg));
            out.newline();
            rows += 1;
        }
    }
    rows
}

pub(crate) fn render_markdown_inner<S: LayoutSink>(
    out: &mut S,
    content: &str,
    width: usize,
    indent: &str,
    dim: bool,
    bctx: Option<&super::BoxContext>,
) -> u16 {
    let _perf = crate::perf::begin("render:markdown");
    let max_cols = if let Some(b) = bctx {
        b.inner_w
    } else {
        width.saturating_sub(indent.len() + 1)
    };
    let lines: Vec<&str> = content.lines().collect();
    let mut i = 0;
    let mut rows = 0u16;
    // Track the last non-blank source line for heading gap suppression.
    let mut last_content_line: Option<&str> = None;
    while i < lines.len() {
        if lines[i].trim_start().starts_with("```") {
            // Blank line before code blocks — skip when preceded by a
            // blank line (already provides the gap) or a heading (headings
            // never get a trailing gap).
            let prev_blank = i > 0 && lines[i - 1].trim().is_empty();
            let after_heading = last_content_line.is_some_and(|l| l.trim_start().starts_with('#'));
            if rows > 0 && !prev_blank && !after_heading {
                out.newline();
                rows += 1;
            }
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
            rows += render_code_block(out, code_lines, lang, width, dim, bctx);
            last_content_line = None;
        } else if lines[i].trim_start().starts_with('|') {
            let table_start = i;
            while i < lines.len() && lines[i].trim_start().starts_with('|') {
                i += 1;
            }
            rows +=
                render_markdown_table_from_lines(out, &lines[table_start..i], dim, bctx, indent);
            last_content_line = None;
        } else if is_horizontal_rule(lines[i]) {
            // Blank line before horizontal rule unless preceded by blank or heading.
            let prev_blank = i > 0 && lines[i - 1].trim().is_empty();
            let after_heading = last_content_line.is_some_and(|l| l.trim_start().starts_with('#'));
            if rows > 0 && !prev_blank && !after_heading {
                out.newline();
                rows += 1;
            }
            rows += render_horizontal_rule(out, bctx, indent);
            // Blank line after horizontal rule unless followed by blank or heading.
            let mut next_i = i + 1;
            while next_i < lines.len() && lines[next_i].trim().is_empty() {
                next_i += 1;
            }
            let next_is_heading =
                next_i < lines.len() && lines[next_i].trim_start().starts_with('#');
            if next_i < lines.len() && !next_is_heading && !lines[next_i].trim().is_empty() {
                out.newline();
                rows += 1;
            }
            last_content_line = None;
            i += 1;
        } else {
            if lines[i].trim().is_empty() {
                // Skip blank lines after headings — headings never have
                // a trailing gap.
                let after_heading =
                    last_content_line.is_some_and(|l| l.trim_start().starts_with('#'));
                if after_heading {
                    i += 1;
                    continue;
                }
                // Skip blank lines before list items.
                let mut next_i = i + 1;
                while next_i < lines.len() && lines[next_i].trim().is_empty() {
                    next_i += 1;
                }
                if next_i < lines.len() && is_list_item(lines[next_i]) {
                    i += 1;
                    continue;
                }
            } else {
                last_content_line = Some(lines[i]);
            }
            let segments = wrap_line(lines[i], max_cols);
            if segments.len() > 1 {
                out.mark_wrapped();
            }
            for seg in &segments {
                if let Some(b) = bctx {
                    b.print_left(out);
                    let cols = print_styled_line(out, seg, dim);
                    b.print_right(out, cols);
                } else {
                    out.print(indent);
                    print_styled_line(out, seg, dim);
                }
                out.newline();
            }
            i += 1;
            rows += segments.len() as u16;
        }
    }
    rows
}

/// Render a single line with block-level detection (headings, blockquotes,
/// list markers) then inline styling via the shared `print_inline_styled`.
/// Returns the number of visible columns printed.
fn print_styled_line<S: LayoutSink>(out: &mut S, text: &str, dim: bool) -> usize {
    use super::highlight::print_inline_styled;
    use unicode_width::UnicodeWidthStr;

    let trimmed = text.trim_start();
    if trimmed.starts_with('#') {
        out.push_style(SpanStyle {
            fg: Some(theme::HEADING.into()),
            bold: true,
            dim,
            ..Default::default()
        });
        out.print(trimmed);
        out.pop_style();
        return trimmed.chars().count();
    }

    if trimmed.starts_with('>') {
        let content = trimmed.strip_prefix('>').unwrap().trim_start();
        out.push_style(SpanStyle {
            dim: true,
            italic: true,
            ..Default::default()
        });
        out.print(content);
        out.pop_style();
        return content.chars().count();
    }

    // Split off list-item markers and print them dim.
    let (prefix, body) = split_list_prefix(trimmed);
    let leading_ws = &text[..text.len() - trimmed.len()];
    if !leading_ws.is_empty() {
        out.print(leading_ws);
    }
    if !prefix.is_empty() {
        out.push_dim();
        out.print(prefix);
        out.pop_style();
    }

    print_inline_styled(out, body, dim);
    let visual = strip_markdown_markers(body).width();
    leading_ws.chars().count() + prefix.chars().count() + visual
}

/// Split a list-item prefix (`- `, `* `, `1. `, etc.) from the line content.
/// Returns (prefix, rest). If not a list item, prefix is empty.
fn split_list_prefix(line: &str) -> (&str, &str) {
    // Ordered: "1. ", "12. ", etc.
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i > 0 && i < bytes.len() && bytes[i] == b'.' {
        let end = i + 1;
        if end < bytes.len() && bytes[end] == b' ' {
            return (&line[..end + 1], &line[end + 1..]);
        }
        return (&line[..end], &line[end..]);
    }
    // Unordered: "- " or "* "
    if line.starts_with("- ") || line.starts_with("* ") {
        return (&line[..2], &line[2..]);
    }
    ("", line)
}

/// Check if a line is a list item (ordered or unordered).
fn is_list_item(line: &str) -> bool {
    let trimmed = line.trim_start();
    // Unordered: "- " or "* "
    if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
        return true;
    }
    // Ordered: digits followed by "."
    let bytes = trimmed.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i > 0 && i < bytes.len() && bytes[i] == b'.' {
        return true;
    }
    false
}

/// Check if a line is a horizontal rule (---, ***, ___, etc.).
fn is_horizontal_rule(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    // Count non-space characters - must be at least 3
    let non_space_count = trimmed.chars().filter(|&c| !c.is_whitespace()).count();
    if non_space_count < 3 {
        return false;
    }
    // Check if all non-space characters are the same and one of -, *, or _
    let mut first_char: Option<char> = None;
    for ch in trimmed.chars() {
        if ch == ' ' || ch == '\t' {
            continue;
        }
        if first_char.is_none() {
            first_char = Some(ch);
        } else {
            // All non-space chars must be the same
            if first_char != Some(ch) {
                return false;
            }
        }
        // Must be one of the valid HR characters
        if !matches!(ch, '-' | '*' | '_') {
            return false;
        }
    }
    first_char.is_some()
}

/// Render a horizontal rule line with dim styling (matching list markers).
/// Replaces the HR characters (---, ***, ___) with box-drawing chars (─) but
/// only renders 3 of them to match the visual weight of list markers.
fn render_horizontal_rule<S: LayoutSink>(
    out: &mut S,
    bctx: Option<&super::BoxContext>,
    indent: &str,
) -> u16 {
    // Use box-drawing character, render only 3 chars (like list markers)
    let hr = "─".repeat(3);

    if let Some(b) = bctx {
        b.print_left(out);
    } else if !indent.is_empty() {
        out.print(indent);
    }

    // Always apply dim attribute (same as list markers)
    out.push_dim();
    out.print(&hr);
    out.pop_style();

    if let Some(b) = bctx {
        b.print_right(out, 3);
    }

    out.newline();
    1
}

/// Parse pipe-delimited table lines into rows, then render.
fn render_markdown_table_from_lines<S: LayoutSink>(
    out: &mut S,
    lines: &[&str],
    dim: bool,
    bctx: Option<&super::BoxContext>,
    indent: &str,
) -> u16 {
    let mut table_rows: Vec<Vec<String>> = Vec::new();
    for line in lines {
        if super::is_table_separator(line) {
            continue;
        }
        let trimmed = line.trim().trim_start_matches('|').trim_end_matches('|');
        let cells: Vec<String> = trimmed.split('|').map(|c| c.trim().to_string()).collect();
        table_rows.push(cells);
    }
    render_markdown_table(out, &table_rows, dim, bctx, indent)
}

fn render_plan_output<S: LayoutSink>(
    out: &mut S,
    args: &HashMap<String, serde_json::Value>,
    width: usize,
) -> u16 {
    let body = args
        .get("plan_summary")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if body.is_empty() {
        return 0;
    }

    // Box geometry: "   │ " (5) + content + " │" (2) = 7 overhead
    let inner_w = width.saturating_sub(7);
    let mut rows = 0u16;

    // Top border: "   ┌─ Plan ──...──┐"
    // 3 + 1(┌) + 1(─) + 6(label) + fill + 1(┐) = 5 + inner_w + 2
    let label = " Plan ";
    let fill = inner_w.saturating_sub(label.len()).saturating_add(1);
    out.push_fg(theme::PLAN.into());
    out.print_string(format!(
        "  \u{250c}\u{2500}{label}{}\u{2510}",
        "\u{2500}".repeat(fill)
    ));
    out.pop_style();
    out.newline();
    rows += 1;

    // Body: markdown rendering inside the plan box.
    let bctx = super::BoxContext {
        left: "  \u{2502} ",
        right: " \u{2502}",
        color: theme::PLAN.into(),
        inner_w,
    };
    rows += render_markdown_inner(out, body, width, bctx.left, false, Some(&bctx));

    // Bottom border: "   └──...──┘"
    // 3 + 1(└) + dashes + 1(┘) = 5 + inner_w + 2 → dashes = inner_w + 2
    out.push_fg(theme::PLAN.into());
    out.print_string(format!(
        "  \u{2514}{}\u{2518}",
        "\u{2500}".repeat(inner_w + 2)
    ));
    out.pop_style();
    out.newline();
    rows += 1;

    rows
}

fn render_wrapped_output<S: LayoutSink>(
    out: &mut S,
    content: &str,
    is_error: bool,
    width: usize,
) -> u16 {
    let _perf = crate::perf::begin("render:wrapped_output");
    let max_cols = width.saturating_sub(3); // "  " prefix + 1 margin

    // Pre-wrap all lines so we can count visual rows.
    let wrapped: Vec<String> = content
        .lines()
        .flat_map(|line| {
            let expanded = line.replace('\t', "    ");
            let segs = wrap_line(&expanded, max_cols);
            if segs.len() > 1 {
                out.mark_wrapped();
            }
            segs
        })
        .collect();

    let total = wrapped.len();
    let mut rows = 0u16;
    if total > MAX_TOOL_BLOCK_ROWS {
        let skipped = total - MAX_TOOL_BLOCK_ROWS;
        print_dim(
            out,
            &format!("  ... {} above", pluralize(skipped, "line", "lines")),
        );
        out.newline();
        rows += 1;
    }
    let start = total.saturating_sub(MAX_TOOL_BLOCK_ROWS);
    for seg in &wrapped[start..] {
        if is_error {
            out.push_fg(theme::ERROR.into());
            out.print_string(format!("  {}", seg));
            out.pop_style();
        } else {
            print_dim(out, &format!("  {}", seg));
        }
        out.newline();
        rows += 1;
    }
    rows
}

fn render_default_output<S: LayoutSink>(
    out: &mut S,
    content: &str,
    is_error: bool,
    width: usize,
) -> u16 {
    let preview = result_preview(content, DEFAULT_PREVIEW_LINES);
    let max_cols = width.saturating_sub(3);
    let segs = wrap_line(&preview, max_cols);
    if segs.len() > 1 {
        out.mark_wrapped();
    }
    let mut rows = 0u16;
    for seg in &segs {
        if is_error {
            out.push_fg(theme::ERROR.into());
            out.print_string(format!("  {}", seg));
            out.pop_style();
        } else {
            print_dim(out, &format!("  {}", seg));
        }
        out.newline();
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

fn result_preview(content: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = content.trim_end_matches('\n').lines().collect();
    if lines.len() <= max_lines {
        lines.join(" | ")
    } else {
        format!(
            "{} ... ({})",
            lines[..max_lines].join(" | "),
            pluralize(lines.len(), "line", "lines")
        )
    }
}

/// Print user message text with accent highlighting for valid `@path` refs,
/// `/command` lines, and `[image]` attachment labels.
pub(super) fn print_user_highlights<S: LayoutSink>(
    out: &mut S,
    text: &str,
    image_labels: &[String],
    is_command: bool,
) {
    let accent_role = ColorValue::Role(ColorRole::Accent);

    // Slash commands: accent the entire text, same as the prompt.
    if is_command {
        out.push_fg(accent_role);
        out.print(text);
        out.pop_style();
        return;
    }

    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut i = 0;
    let mut plain = String::new();

    let flush = |out: &mut S, plain: &mut String| {
        if !plain.is_empty() {
            let s = std::mem::take(plain);
            out.print(&s);
        }
    };

    let accent = |out: &mut S, token: String| {
        out.push_fg(accent_role);
        out.print(&token);
        out.pop_style();
    };

    while i < len {
        // Image attachment labels like [screenshot.png].
        if chars[i] == '[' {
            let remaining: String = chars[i..].iter().collect();
            if let Some(label) = image_labels
                .iter()
                .find(|l| remaining.starts_with(l.as_str()))
            {
                flush(out, &mut plain);
                accent(out, label.clone());
                i += label.chars().count();
                continue;
            }
        }

        // @path references validated against the filesystem.
        if let Some((token, end)) = super::try_at_ref(&chars, i) {
            flush(out, &mut plain);
            accent(out, token);
            i = end;
        } else {
            plain.push(chars[i]);
            i += 1;
        }
    }
    flush(out, &mut plain);
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
            image_labels: vec![],
        }
    }

    fn thinking(s: &str) -> Block {
        Block::Thinking {
            content: s.to_string(),
        }
    }

    fn empty_tool_call() -> Block {
        Block::ToolCall {
            call_id: String::new(),
            name: String::new(),
            summary: String::new(),
            args: HashMap::new(),
        }
    }

    fn tool_call() -> Block {
        Block::ToolCall {
            call_id: "call-1".into(),
            name: "bash".into(),
            summary: "ls".into(),
            args: HashMap::new(),
        }
    }

    fn pending_tool_state() -> ToolState {
        ToolState {
            status: ToolStatus::Pending,
            elapsed: None,
            output: None,
            user_message: None,
        }
    }

    fn state_for(block: &Block) -> Option<ToolState> {
        matches!(block, Block::ToolCall { .. }).then(pending_tool_state)
    }

    fn block_rows(block: &Block) -> u16 {
        let mut out = SpanCollector::new(W as u16);
        let st = state_for(block);
        render_block(&mut out, block, st.as_ref(), W, true)
    }

    /// Compute total gap rows between the last history block and an active tool.
    fn tool_gap_for(blocks: &[Block]) -> u16 {
        blocks
            .last()
            .map(|b| gap_between(&Element::Block(b), &Element::Block(&empty_tool_call())))
            .unwrap_or(0)
    }

    /// Simulate the "all-at-once" render path: all blocks are unflushed,
    /// rendered in one pass, then active tool is appended.
    /// Returns (block_rows, tool_gap, total_before_tool).
    fn render_all_at_once(blocks: &[Block]) -> (u16, u16, u16) {
        let mut out = SpanCollector::new(W as u16);
        let mut total = 0u16;
        for i in 0..blocks.len() {
            let gap = if i > 0 {
                gap_between(&Element::Block(&blocks[i - 1]), &Element::Block(&blocks[i]))
            } else {
                0
            };
            let rows = {
                let st = state_for(&blocks[i]);
                render_block(&mut out, &blocks[i], st.as_ref(), W, true)
            };
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
        let mut out = SpanCollector::new(W as u16);
        let mut block_rows_total = 0u16;
        for i in 0..blocks.len() {
            let gap = if i > 0 {
                gap_between(&Element::Block(&blocks[i - 1]), &Element::Block(&blocks[i]))
            } else {
                0
            };
            let rows = {
                let st = state_for(&blocks[i]);
                render_block(&mut out, &blocks[i], st.as_ref(), W, true)
            };
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
        let mut out = SpanCollector::new(W as u16);
        let mut cumulative = 0u16;
        for i in 0..blocks.len() {
            let gap = if i > 0 {
                gap_between(&Element::Block(&blocks[i - 1]), &Element::Block(&blocks[i]))
            } else {
                0
            };
            let rows = {
                let st = state_for(&blocks[i]);
                render_block(&mut out, &blocks[i], st.as_ref(), W, true)
            };
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

        let gap = gap_between(&Element::Block(&text("")), &Element::Block(&empty_tool_call()));
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
        let gap_user_tool = gap_between(&Element::Block(&user("hello")), &Element::Block(&empty_tool_call()));
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
        // Two consecutive text blocks — gap should be 1 (paragraph spacing).
        let gap = gap_between(&Element::Block(&text("a")), &Element::Block(&text("b")));
        assert_eq!(gap, 1, "Text→Text gap = 1");
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
            let mut out = SpanCollector::new(W as u16);
            for i in flushed..end {
                let gap = if i > 0 {
                    gap_between(&Element::Block(&blocks[i - 1]), &Element::Block(&blocks[i]))
                } else {
                    0
                };
                let rows = {
                    let st = state_for(&blocks[i]);
                    render_block(&mut out, &blocks[i], st.as_ref(), W, true)
                };
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

    #[test]
    fn horizontal_rule_detection() {
        // Valid horizontal rules
        assert!(is_horizontal_rule("---"), "basic dashes");
        assert!(is_horizontal_rule("___"), "basic underscores");
        assert!(is_horizontal_rule("***"), "basic asterisks");
        assert!(is_horizontal_rule("------"), "longer dashes");
        assert!(is_horizontal_rule("-----"), "odd length");
        assert!(is_horizontal_rule(" - - - "), "spaced dashes");
        assert!(is_horizontal_rule(" * * * "), "spaced asterisks");
        assert!(is_horizontal_rule(" _ _ _ "), "spaced underscores");
        assert!(is_horizontal_rule("  ---  "), "padded dashes");

        // Invalid horizontal rules
        assert!(!is_horizontal_rule("--"), "too short");
        assert!(!is_horizontal_rule("-"), "single char");
        assert!(!is_horizontal_rule(""), "empty");
        assert!(!is_horizontal_rule("text"), "regular text");
        assert!(!is_horizontal_rule("- -"), "too short with spaces");
        assert!(!is_horizontal_rule("-*-*-*"), "mixed characters");
        assert!(!is_horizontal_rule("---a"), "contains other chars");
        assert!(!is_horizontal_rule("123"), "numbers");
    }
}
