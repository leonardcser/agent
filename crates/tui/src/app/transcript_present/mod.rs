//! Projection: `Block` + `ToolState` + `LayoutContext` → `DisplayBlock`.
//!
//! Pattern-matches on the domain `Block` enum and emits presentation-
//! layer spans through `SpanCollector`. The only consumer is
//! `BlockHistory::ensure_rows`, which caches the result per
//! `LayoutKey`. Render paints the resulting `DisplayBlock`; it has no
//! further knowledge of `Block` variants.

use super::transcript_model::{
    AgentBlockStatus, ApprovalScope, Block, ConfirmChoice, ToolOutput, ToolState, ToolStatus,
    ViewState,
};
use crate::content::display::{
    ColorRole, ColorValue, DisplayBlock, NamedColor, SpanMeta, SpanStyle,
};
use crate::content::highlight::{
    print_cached_inline_diff, print_inline_diff, print_syntax_file, print_syntax_file_ext,
    render_code_block, render_markdown_table, BashHighlighter,
};
use crate::content::layout_out::{display_width, SpanCollector};
use crate::content::{truncate_str, wrap_line, LayoutContext};
use crate::theme;
use crate::utils::format_duration;
use engine::tools::NotebookRenderData;
use std::collections::HashMap;

mod agent;
mod markdown;
mod tool_previews;
mod tools;

use agent::render_agent_block;
#[cfg(test)]
use markdown::is_horizontal_rule;
pub(crate) use markdown::render_markdown_inner;
use tools::{pluralize, render_confirm_result, render_tool, render_wrapped_output};

use std::time::Duration;

/// Preprocessed user message layout: tab-expanded, blank-trimmed lines
/// with a computed `block_w` for multiline bubble rendering.
pub(crate) struct UserBlockGeometry {
    pub lines: Vec<String>,
    pub block_w: usize,
}

impl UserBlockGeometry {
    pub fn new(text: &str, text_w: usize) -> Self {
        let all_lines: Vec<String> = text.lines().map(|l| l.replace('\t', "    ")).collect();
        let start = all_lines.iter().position(|l| !l.is_empty()).unwrap_or(0);
        let end = all_lines
            .iter()
            .rposition(|l| !l.is_empty())
            .map_or(0, |i| i + 1);
        let lines: Vec<String> = all_lines[start..end]
            .iter()
            .map(|l| l.trim_end().to_string())
            .collect();
        let wraps = lines.iter().any(|l| display_width(l) > text_w);
        let multiline = lines.len() > 1 || wraps;
        let block_w = if multiline {
            if wraps {
                text_w + 1
            } else {
                lines.iter().map(|l| display_width(l)).max().unwrap_or(0) + 1
            }
        } else {
            0
        };
        Self { lines, block_w }
    }
}

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
pub(crate) fn layout_block(
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
fn apply_view_state(display: &mut crate::content::display::DisplayBlock, state: ViewState) {
    use crate::content::display::{ColorRole, ColorValue, DisplayLine, DisplaySpan, SpanStyle};
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
            gutter_bg: None,
            fill_bg: None,
            fill_right_margin: 0,
            soft_wrapped: false,
            source_text: None,
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

/// Extract a title and non-empty line count from thinking content.
/// If the first non-empty line is a markdown bold title (`**...**`), use it as the label.
pub(crate) fn thinking_summary(content: &str) -> (String, usize) {
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
pub(crate) fn render_thinking_summary(
    out: &mut SpanCollector,
    width: usize,
    label: &str,
    line_count: usize,
    animated: bool,
) -> u16 {
    let dots = if animated { animated_dots() } else { "" };
    let summary = format!("{label} ({}){dots}", pluralize(line_count, "line", "lines"));
    let max_cols = width.saturating_sub(3).max(1);
    let segs = wrap_line(&summary, max_cols);
    if segs.len() > 1 {
        out.mark_wrapped();
    }
    let mut rows = 0u16;
    for seg in &segs {
        out.set_dim_italic();
        out.print_gutter("\u{2502} ");
        out.print(seg);
        out.reset_style();
        out.newline();
        rows += 1;
    }
    rows
}

/// Element types for spacing calculation.
pub(crate) enum Element<'a> {
    Block(&'a Block),
}

/// Number of blank lines to insert between two adjacent elements.
pub(crate) fn gap_between(above: &Element, below: &Element) -> u16 {
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

pub(super) fn render_block(
    out: &mut SpanCollector,
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
            let text_w = width.saturating_sub(1).max(1);
            let geom = UserBlockGeometry::new(text, text_w);
            let user_bg = ColorValue::Role(ColorRole::UserBg);
            let mut rows = 0u16;
            let pad_meta = SpanMeta {
                selectable: false,
                copy_as: None,
            };
            for logical_line in &geom.lines {
                if logical_line.is_empty() {
                    let fill = if geom.block_w > 0 { geom.block_w } else { 1 };
                    out.set_bg(user_bg);
                    out.print_with_meta(&" ".repeat(fill), pad_meta.clone());
                    out.reset_style();
                    out.set_gutter_bg(user_bg);
                    out.newline();
                    rows += 1;
                    continue;
                }
                let chunks = wrap_line(logical_line, text_w);
                if chunks.len() > 1 {
                    out.mark_wrapped();
                }
                for chunk in &chunks {
                    let chunk_w = display_width(chunk);
                    let trailing = if geom.block_w > 0 {
                        geom.block_w.saturating_sub(chunk_w)
                    } else {
                        1
                    };
                    out.set_bg(user_bg);
                    out.set_bold();
                    print_user_highlights(out, chunk, image_labels, is_command);
                    out.print_with_meta(&" ".repeat(trailing), pad_meta.clone());
                    out.reset_style();
                    out.set_gutter_bg(user_bg);
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
                    out.print_gutter("│ ");
                    out.print(seg);
                    out.reset_style();
                    out.newline();
                    rows += 1;
                }
            }
            rows
        }
        Block::Text { content } => render_markdown_inner(out, content, width, "", false, None),
        Block::CodeLine { content, lang } => {
            render_code_block(out, &[content.as_str()], lang, width, false, None, false)
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
            out.print_gutter(&"─".repeat(left));
            out.print_gutter(label);
            out.print_gutter(&"─".repeat(right));
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
            let bctx = crate::content::BoxContext {
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
/// Print user message text with accent highlighting for valid `@path` refs,
/// `/command` lines, and `[image]` attachment labels.
pub(super) fn print_user_highlights(
    out: &mut SpanCollector,
    text: &str,
    image_labels: &[String],
    is_command: bool,
) {
    let accent_role = ColorValue::Role(ColorRole::Accent);

    // Commands: accent the entire text, same as the prompt.
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

    let flush = |out: &mut SpanCollector, plain: &mut String| {
        if !plain.is_empty() {
            let s = std::mem::take(plain);
            out.print(&s);
        }
    };

    let accent = |out: &mut SpanCollector, token: String| {
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
        if let Some((token, end)) = crate::content::try_at_ref(&chars, i) {
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

    fn render_split(blocks: &[Block]) -> (u16, u16, u16) {
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

        let gap = gap_between(
            &Element::Block(&text("")),
            &Element::Block(&empty_tool_call()),
        );
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
        let gap_user_tool = gap_between(
            &Element::Block(&user("hello")),
            &Element::Block(&empty_tool_call()),
        );
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

    #[test]
    fn thinking_summary_extracts_bold_title() {
        let (label, lines) =
            thinking_summary("**Analyzing the bug**\nLet me check...\n\nMore notes");
        assert_eq!(label, "Analyzing the bug");
        assert_eq!(lines, 3);
    }

    #[test]
    fn thinking_summary_falls_back_to_default() {
        let (label, lines) = thinking_summary("Let me think about this.\nLine two.");
        assert_eq!(label, "thinking");
        assert_eq!(lines, 2);
    }

    #[test]
    fn thinking_summary_skips_blank_lines() {
        let (_, lines) = thinking_summary("\n\nfirst\n\nsecond\n\n");
        assert_eq!(lines, 2);
    }

    #[test]
    fn thinking_summary_empty() {
        let (label, lines) = thinking_summary("");
        assert_eq!(label, "thinking");
        assert_eq!(lines, 0);
    }

    #[test]
    fn thinking_summary_bold_must_have_content() {
        // "****" is 4 chars — the `len() > 4` check rejects empty bold
        let (label, _) = thinking_summary("****");
        assert_eq!(label, "thinking");
    }

    #[test]
    fn bash_tool_layout_sets_source_text_and_soft_wrap() {
        // A bash command that wraps at width 30 should produce:
        // - Row 0: source_text = full command, soft_wrapped = false
        // - Row 1+: source_text = None, soft_wrapped = true
        let block = Block::ToolCall {
            call_id: "c1".into(),
            name: "bash".into(),
            summary: "echo hello && echo world && echo done".into(),
            args: HashMap::new(),
        };
        let state = ToolState {
            status: ToolStatus::Ok,
            elapsed: Some(std::time::Duration::from_secs(1)),
            output: None,
            user_message: None,
        };
        let ctx = LayoutContext {
            width: 30,
            show_thinking: true,
            view_state: ViewState::Expanded,
        };
        let display = layout_block(&block, Some(&state), &ctx);

        assert!(
            display.lines.len() >= 2,
            "command should wrap at width 30, got {} lines",
            display.lines.len()
        );
        assert_eq!(
            display.lines[0].source_text.as_deref(),
            Some("echo hello && echo world && echo done"),
        );
        assert!(!display.lines[0].soft_wrapped);
        for line in &display.lines[1..] {
            assert!(
                line.source_text.is_none(),
                "continuation rows should not have source_text"
            );
            assert!(
                line.soft_wrapped,
                "continuation rows should be soft_wrapped"
            );
        }
    }

    #[test]
    fn bash_tool_multiline_command_only_wraps_mark_soft() {
        // A multi-line command (real newlines) should NOT mark line 2
        // as soft-wrapped — only segments within a wrapped line should.
        let block = Block::ToolCall {
            call_id: "c2".into(),
            name: "bash".into(),
            summary: "echo one\necho two".into(),
            args: HashMap::new(),
        };
        let state = ToolState {
            status: ToolStatus::Ok,
            elapsed: None,
            output: None,
            user_message: None,
        };
        let ctx = LayoutContext {
            width: 80,
            show_thinking: true,
            view_state: ViewState::Expanded,
        };
        let display = layout_block(&block, Some(&state), &ctx);

        assert!(display.lines.len() >= 2);
        assert!(!display.lines[0].soft_wrapped);
        assert!(
            !display.lines[1].soft_wrapped,
            "second real line should NOT be soft-wrapped"
        );
    }

    #[test]
    fn bash_tool_time_suffix_is_non_selectable() {
        let block = Block::ToolCall {
            call_id: "c3".into(),
            name: "bash".into(),
            summary: "ls".into(),
            args: HashMap::new(),
        };
        let state = ToolState {
            status: ToolStatus::Ok,
            elapsed: Some(std::time::Duration::from_secs(3)),
            output: None,
            user_message: None,
        };
        let ctx = LayoutContext {
            width: 80,
            show_thinking: true,
            view_state: ViewState::Expanded,
        };
        let display = layout_block(&block, Some(&state), &ctx);
        let first_line = &display.lines[0];

        // The time suffix "  3s" should be in a non-selectable span
        let has_non_selectable_time = first_line
            .spans
            .iter()
            .any(|span| !span.meta.selectable && span.text.contains("3s"));
        assert!(
            has_non_selectable_time,
            "elapsed time should be in a non-selectable span"
        );
    }
}
