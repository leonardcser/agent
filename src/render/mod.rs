mod blocks;
mod dialogs;
mod highlight;

pub use dialogs::{
    parse_questions, show_ask_question, show_confirm, show_rewind, show_resume, Question,
    QuestionOption,
};

use crate::input::{InputState, SettingsMenu, PASTE_MARKER};
use crate::theme;
use crossterm::{
    cursor,
    style::{Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor},
    terminal, QueueableCommand,
};
use std::collections::HashMap;
use std::io::{self, Write};
use std::time::{Duration, Instant};

use blocks::{gap_between, render_block, render_tool, Element};

/// Clear remaining characters on the current line and advance to the next.
/// Using Clear(UntilNewLine) before \r\n ensures old content doesn't leak
/// through when overwriting in place (flicker-free rendering).
pub(super) fn crlf(out: &mut io::Stdout) {
    let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
    let _ = out.queue(Print("\r\n"));
}

const SPINNER_FRAMES: &[&str] = &["✿", "❀", "✾", "❁"];

#[derive(Clone, Copy, PartialEq)]
pub enum ToolStatus {
    Pending,
    Confirm,
    Ok,
    Err,
    Denied,
}

#[derive(Clone)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

pub struct ActiveTool {
    pub name: String,
    pub summary: String,
    pub args: HashMap<String, serde_json::Value>,
    pub status: ToolStatus,
    pub output: Option<ToolOutput>,
    pub start_time: Instant,
}

impl ActiveTool {
    fn elapsed(&self) -> Option<Duration> {
        if self.name == "bash" { Some(self.start_time.elapsed()) } else { None }
    }
}

#[derive(Clone)]
pub struct ResumeEntry {
    pub id: String,
    pub title: String,
    pub subtitle: Option<String>,
    pub updated_at_ms: u64,
    pub created_at_ms: u64,
}

#[derive(Clone)]
pub enum Block {
    User {
        text: String,
    },
    Text {
        content: String,
    },
    ToolCall {
        name: String,
        summary: String,
        args: HashMap<String, serde_json::Value>,
        status: ToolStatus,
        elapsed: Option<Duration>,
        output: Option<ToolOutput>,
    },
    Confirm {
        tool: String,
        desc: String,
        choice: Option<ConfirmChoice>,
    },
    Error {
        message: String,
    },
    Exec {
        command: String,
        output: String,
    },
}

#[derive(Clone, PartialEq)]
pub enum ConfirmChoice {
    Yes,
    No,
    Always,
    /// User approved and typed additional instructions.
    YesWithMessage(String),
}

#[derive(Clone, Copy, PartialEq)]
pub enum Throbber {
    Working,
    Retrying { delay: Duration, attempt: u32 },
    Compacting,
    Done,
    Interrupted,
}

struct BlockHistory {
    blocks: Vec<Block>,
    flushed: usize,
    last_block_rows: u16,
}

impl BlockHistory {
    fn new() -> Self {
        Self { blocks: Vec::new(), flushed: 0, last_block_rows: 0 }
    }

    fn push(&mut self, block: Block) {
        self.blocks.push(block);
    }

    fn has_unflushed(&self) -> bool {
        self.flushed < self.blocks.len()
    }

    fn clear(&mut self) {
        self.blocks.clear();
        self.flushed = 0;
        self.last_block_rows = 0;
    }

    fn truncate(&mut self, idx: usize) {
        self.blocks.truncate(idx);
        self.flushed = self.flushed.min(idx);
    }

    /// Render unflushed blocks. Returns total rows printed.
    fn render(&mut self, out: &mut io::Stdout, width: usize) -> u16 {
        if !self.has_unflushed() {
            return 0;
        }
        let mut total = 0u16;
        let last_idx = self.blocks.len().saturating_sub(1);
        for i in self.flushed..self.blocks.len() {
            let gap = if i > 0 {
                gap_between(
                    &Element::Block(&self.blocks[i - 1]),
                    &Element::Block(&self.blocks[i]),
                )
            } else {
                0
            };
            for _ in 0..gap {
                let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
                let _ = out.queue(Print("\r\n"));
            }
            let rows = render_block(out, &self.blocks[i], width);
            total += gap + rows;
            if i == last_idx {
                self.last_block_rows = rows + gap;
            }
        }
        self.flushed = self.blocks.len();
        total
    }
}

struct PromptState {
    drawn: bool,
    dirty: bool,
    redraw_row: u16,
    prev_rows: u16,
    /// Cursor row to use when `drawn` is false, avoiding racy cursor::position() queries.
    fallback_row: Option<u16>,
}

impl PromptState {
    fn new() -> Self {
        Self { drawn: false, dirty: true, redraw_row: 0, prev_rows: 0, fallback_row: None }
    }
}

struct WorkingState {
    since: Option<Instant>,
    final_elapsed: Option<Duration>,
    throbber: Option<Throbber>,
    last_spinner_frame: usize,
    retry_deadline: Option<Instant>,
}

impl WorkingState {
    fn new() -> Self {
        Self {
            since: None,
            final_elapsed: None,
            throbber: None,
            last_spinner_frame: usize::MAX,
            retry_deadline: None,
        }
    }

    fn set_throbber(&mut self, state: Throbber) {
        let is_active = matches!(
            state,
            Throbber::Working | Throbber::Retrying { .. } | Throbber::Compacting
        );
        if is_active && self.since.is_none() {
            self.since = Some(Instant::now());
            self.final_elapsed = None;
        }
        if !is_active {
            self.final_elapsed = self.since.map(|s| s.elapsed());
            self.since = None;
        }
        self.retry_deadline = match state {
            Throbber::Retrying { delay, .. } => Some(Instant::now() + delay),
            _ => None,
        };
        self.throbber = Some(state);
    }

    fn clear(&mut self) {
        self.throbber = None;
        self.since = None;
        self.final_elapsed = None;
    }

    fn throbber_spans(&self) -> Vec<BarSpan> {
        let Some(state) = self.throbber else {
            return vec![];
        };
        match state {
            Throbber::Compacting => {
                let Some(start) = self.since else {
                    return vec![];
                };
                let elapsed = start.elapsed();
                let idx = (elapsed.as_millis() / 150) as usize % SPINNER_FRAMES.len();
                vec![
                    BarSpan {
                        text: format!("{} compacting", SPINNER_FRAMES[idx]),
                        color: Color::Reset,
                        attr: Some(Attribute::Bold),
                    },
                    BarSpan {
                        text: format!(" {}s", elapsed.as_secs()),
                        color: theme::MUTED,
                        attr: Some(Attribute::Dim),
                    },
                ]
            }
            Throbber::Working | Throbber::Retrying { .. } => {
                let Some(start) = self.since else {
                    return vec![];
                };
                let elapsed = start.elapsed();
                let idx = (elapsed.as_millis() / 150) as usize % SPINNER_FRAMES.len();
                let spinner_color = if matches!(state, Throbber::Retrying { .. }) {
                    theme::MUTED
                } else {
                    Color::Reset
                };
                let mut spans = vec![
                    BarSpan {
                        text: format!("{} working", SPINNER_FRAMES[idx]),
                        color: spinner_color,
                        attr: Some(Attribute::Bold),
                    },
                    BarSpan {
                        text: format!(" {}s", elapsed.as_secs()),
                        color: theme::MUTED,
                        attr: Some(Attribute::Dim),
                    },
                ];
                if let Throbber::Retrying { delay, attempt } = state {
                    let remaining = self
                        .retry_deadline
                        .map(|t| t.saturating_duration_since(Instant::now()))
                        .unwrap_or(delay);
                    spans.push(BarSpan {
                        text: format!(
                            " (retrying in {}s #{})",
                            remaining.as_secs(),
                            attempt
                        ),
                        color: theme::MUTED,
                        attr: Some(Attribute::Dim),
                    });
                }
                spans
            }
            Throbber::Done => {
                let secs = self.final_elapsed.map(|d| d.as_secs()).unwrap_or(0);
                vec![BarSpan {
                    text: format!("done {}s", secs),
                    color: theme::MUTED,
                    attr: Some(Attribute::Dim),
                }]
            }
            Throbber::Interrupted => {
                vec![BarSpan {
                    text: "interrupted".into(),
                    color: theme::MUTED,
                    attr: Some(Attribute::Dim),
                }]
            }
        }
    }
}

pub struct Screen {
    history: BlockHistory,
    active_tool: Option<ActiveTool>,
    prompt: PromptState,
    working: WorkingState,
    context_tokens: Option<u32>,
    /// True once terminal auto-scrolling has pushed content into scrollback.
    has_scrollback: bool,
    /// Terminal row where block content starts (top of conversation).
    /// Set once when the first block is rendered; reset on purge/clear.
    content_start_row: Option<u16>,
}

impl Default for Screen {
    fn default() -> Self {
        Self::new()
    }
}

impl Screen {
    pub fn new() -> Self {
        Self {
            history: BlockHistory::new(),
            active_tool: None,
            prompt: PromptState::new(),
            working: WorkingState::new(),
            context_tokens: None,
            has_scrollback: false,
            content_start_row: None,
        }
    }

    pub fn begin_turn(&mut self) {
        self.history.last_block_rows = 0;
        self.active_tool = None;
    }

    pub fn push(&mut self, block: Block) {
        self.history.push(block);
        self.prompt.dirty = true;
    }

    pub fn start_tool(
        &mut self,
        name: String,
        summary: String,
        args: HashMap<String, serde_json::Value>,
    ) {
        self.active_tool = Some(ActiveTool {
            name,
            summary,
            args,
            status: ToolStatus::Pending,
            output: None,
            start_time: Instant::now(),
        });
        self.prompt.dirty = true;
    }

    pub fn append_active_output(&mut self, chunk: &str) {
        if let Some(ref mut tool) = self.active_tool {
            match tool.output {
                Some(ref mut out) => {
                    if !out.content.is_empty() {
                        out.content.push('\n');
                    }
                    out.content.push_str(chunk);
                }
                None => {
                    tool.output = Some(ToolOutput {
                        content: chunk.to_string(),
                        is_error: false,
                    });
                }
            }
            self.prompt.dirty = true;
        }
    }

    pub fn set_active_status(&mut self, status: ToolStatus) {
        if let Some(ref mut tool) = self.active_tool {
            tool.status = status;
            if status == ToolStatus::Pending {
                tool.start_time = Instant::now();
            }
            self.prompt.dirty = true;
        }
    }

    pub fn finish_tool(
        &mut self,
        status: ToolStatus,
        output: Option<ToolOutput>,
    ) {
        if let Some(tool) = self.active_tool.take() {
            let elapsed = tool.elapsed();
            self.history.push(Block::ToolCall {
                name: tool.name,
                summary: tool.summary,
                args: tool.args,
                status,
                elapsed,
                output,
            });
            self.prompt.dirty = true;
        }
    }

    pub fn set_context_tokens(&mut self, tokens: u32) {
        self.context_tokens = Some(tokens);
        self.prompt.dirty = true;
    }

    pub fn clear_context_tokens(&mut self) {
        self.context_tokens = None;
        self.prompt.dirty = true;
    }

    pub fn context_tokens(&self) -> Option<u32> {
        self.context_tokens
    }

    pub fn set_throbber(&mut self, state: Throbber) {
        self.working.set_throbber(state);
        self.prompt.dirty = true;
    }

    pub fn clear_throbber(&mut self) {
        self.working.clear();
        self.prompt.dirty = true;
    }

    pub fn mark_dirty(&mut self) {
        self.prompt.dirty = true;
    }

    pub fn flush_blocks(&mut self) {
        let _perf = crate::perf::begin("flush_blocks");
        if let Some(tool) = self.active_tool.take() {
            let elapsed = tool.elapsed();
            self.history.push(Block::ToolCall {
                name: tool.name,
                summary: tool.summary,
                args: tool.args,
                status: ToolStatus::Err,
                elapsed,
                output: tool.output,
            });
        }
        let mut out = io::stdout();
        let _ = out.queue(terminal::BeginSynchronizedUpdate);
        let start_row = if self.prompt.drawn {
            let row = self.prompt.redraw_row;
            let _ = out.queue(cursor::MoveTo(0, row));
            let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));
            self.prompt.drawn = false;
            row
        } else {
            self.prompt.fallback_row.take().unwrap_or_else(|| {
                cursor::position().map(|(_, y)| y).unwrap_or(0)
            })
        };
        let block_rows = self.history.render(&mut out, term_width());
        self.prompt.fallback_row = Some(start_row + block_rows);
        let _ = out.queue(terminal::EndSynchronizedUpdate);
        let _ = out.flush();
    }

    pub fn erase_prompt(&mut self) {
        if self.prompt.drawn {
            erase_prompt_at(self.prompt.redraw_row);
            self.prompt.fallback_row = Some(self.prompt.redraw_row);
            self.prompt.drawn = false;
        }
    }

    /// Re-render all history blocks after dialogs, resize, etc.
    /// Only purges scrollback when content has actually overflowed.
    pub fn redraw_in_place(&mut self) {
        self.full_redraw(self.has_scrollback);
    }

    /// Re-render all blocks. When `purge` is true, clears scrollback and
    /// screen first — necessary when content has overflowed the viewport.
    /// When false, redraws over the current viewport (faster, no flash).
    fn full_redraw(&mut self, purge: bool) {
        let mut out = io::stdout();
        let _ = out.queue(terminal::BeginSynchronizedUpdate);
        if purge {
            let _ = out.queue(cursor::MoveTo(0, 0));
            let _ = out.queue(terminal::Clear(terminal::ClearType::All));
            let _ = out.queue(terminal::Clear(terminal::ClearType::Purge));
        } else {
            let _ = out.queue(cursor::MoveTo(0, self.content_start_row.unwrap_or(0)));
        }
        self.history.flushed = 0;
        self.history.last_block_rows = 0;
        let block_rows = self.history.render(&mut out, term_width());
        if !purge {
            let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));
        }
        let _ = out.queue(terminal::EndSynchronizedUpdate);
        let _ = out.flush();
        self.prompt.drawn = false;
        self.prompt.dirty = true;
        self.prompt.prev_rows = 0;
        if purge {
            self.has_scrollback = false;
            self.content_start_row = None;
            self.prompt.fallback_row = Some(block_rows);
        } else {
            let start = self.content_start_row.unwrap_or(0);
            self.prompt.fallback_row = Some(start + block_rows);
        }
    }

    pub fn clear(&mut self) {
        self.history.clear();
        self.active_tool = None;
        self.prompt = PromptState::new();
        self.prompt.fallback_row = Some(0);
        self.working.clear();
        self.context_tokens = None;
        self.has_scrollback = false;
        self.content_start_row = None;
        let mut out = io::stdout();
        let _ = out.queue(terminal::BeginSynchronizedUpdate);
        let _ = out.queue(cursor::MoveTo(0, 0));
        let _ = out.queue(terminal::Clear(terminal::ClearType::All));
        let _ = out.queue(terminal::Clear(terminal::ClearType::Purge));
        let _ = out.queue(terminal::EndSynchronizedUpdate);
        let _ = out.flush();
    }

    pub fn has_history(&self) -> bool {
        !self.history.blocks.is_empty()
    }

    /// Returns (block_index, full_text) for each User block.
    pub fn user_turns(&self) -> Vec<(usize, String)> {
        self.history.blocks
            .iter()
            .enumerate()
            .filter_map(|(i, b)| {
                if let Block::User { text } = b {
                    Some((i, text.clone()))
                } else {
                    None
                }
            })
            .collect()
    }

    /// Truncate blocks so that only blocks before `block_idx` remain.
    pub fn truncate_to(&mut self, block_idx: usize) {
        self.history.truncate(block_idx);
        self.active_tool = None;
        self.full_redraw(self.has_scrollback);
    }

    pub fn draw_prompt(&mut self, state: &InputState, mode: super::input::Mode, width: usize) {
        self.prompt.dirty = true;
        self.draw_prompt_with_queued(state, mode, width, &[]);
    }

    pub fn draw_prompt_with_queued(
        &mut self,
        state: &InputState,
        mode: super::input::Mode,
        width: usize,
        queued: &[String],
    ) {
        let _perf = crate::perf::begin("draw_prompt");

        if let Some(start) = self.working.since {
            let frame = (start.elapsed().as_millis() / 150) as usize % SPINNER_FRAMES.len();
            if frame != self.working.last_spinner_frame {
                self.working.last_spinner_frame = frame;
                self.prompt.dirty = true;
            }
        }

        let has_new_blocks = self.history.has_unflushed();
        if !has_new_blocks && !self.prompt.dirty {
            return;
        }

        let mut out = io::stdout();

        let draw_start_row = if self.prompt.drawn {
            let _ = out.queue(terminal::BeginSynchronizedUpdate);
            let _ = out.queue(cursor::Hide);
            let _ = out.queue(cursor::MoveTo(0, self.prompt.redraw_row));
            self.prompt.redraw_row
        } else {
            // Use tracked row when available to avoid cursor::position() which
            // races with pending keystrokes in stdin and can return wrong values.
            let row = self.prompt.fallback_row.take().unwrap_or_else(|| {
                cursor::position().map(|(_, y)| y).unwrap_or(0)
            });
            let _ = out.queue(terminal::BeginSynchronizedUpdate);
            let _ = out.queue(cursor::Hide);
            row
        };

        let block_rows = self.history.render(&mut out, term_width());

        let mut active_rows: u16 = 0;
        if let Some(ref tool) = self.active_tool {
            let tool_gap = if let Some(last) = self.history.blocks.last() {
                gap_between(&Element::Block(last), &Element::ActiveTool)
            } else {
                0
            };
            for _ in 0..tool_gap {
                let _ = out.queue(terminal::Clear(terminal::ClearType::CurrentLine));
                let _ = out.queue(Print("\r\n"));
            }
            let rows = render_tool(
                &mut out,
                &tool.name,
                &tool.summary,
                &tool.args,
                tool.status,
                Some(tool.start_time.elapsed()),
                tool.output.as_ref(),
                width,
            );
            active_rows = tool_gap + rows;
        }

        let gap = if self.active_tool.is_some() {
            gap_between(&Element::ActiveTool, &Element::Prompt)
        } else {
            self.history.blocks.last().map_or(0, |last| {
                gap_between(&Element::Block(last), &Element::Prompt)
            })
        };
        for _ in 0..gap {
            let _ = out.queue(terminal::Clear(terminal::ClearType::CurrentLine));
            let _ = out.queue(Print("\r\n"));
        }

        let pre_prompt = block_rows + active_rows + gap;
        let (top_row, new_rows, scrolled) = self.draw_prompt_sections(
            &mut out,
            state,
            mode,
            width,
            queued,
            self.prompt.prev_rows.saturating_sub(pre_prompt),
            draw_start_row,
            pre_prompt,
        );
        if scrolled {
            self.has_scrollback = true;
        }
        self.prompt.prev_rows = (pre_prompt - block_rows) + new_rows;

        if self.content_start_row.is_none() {
            self.content_start_row = Some(top_row);
        }
        self.prompt.redraw_row = top_row + block_rows;
        self.prompt.drawn = true;
        self.prompt.dirty = false;

        let _ = out.queue(cursor::Show);
        let _ = out.queue(terminal::EndSynchronizedUpdate);
        let _ = out.flush();
    }

    /// Returns (top_row, total_prompt_rows, scrolled).
    #[allow(clippy::too_many_arguments)]
    fn draw_prompt_sections(
        &self,
        out: &mut io::Stdout,
        state: &InputState,
        mode: super::input::Mode,
        width: usize,
        queued: &[String],
        prev_rows: u16,
        draw_start_row: u16,
        pre_prompt_rows: u16,
    ) -> (u16, u16, bool) {
        let usable = width.saturating_sub(1);
        let height = terminal::size().map(|(_, h)| h as usize).unwrap_or(24);
        let stash_rows = if state.stash.is_some() { 1 } else { 0 };
        let queued_rows = queued.len();

        let mut extra_rows = render_stash(out, &state.stash, usable);
        extra_rows += render_queued(out, queued, usable);

        let vi_normal = state.vim_mode() == Some(crate::vim::ViMode::Normal);
        let bar_color = if vi_normal { theme::ACCENT } else { theme::BAR };

        let tokens_label = self.context_tokens.map(format_tokens);
        let throbber_spans = self.working.throbber_spans();
        draw_bar(
            out,
            width,
            if throbber_spans.is_empty() { None } else { Some(&throbber_spans) },
            tokens_label.as_deref().map(|l| (l, theme::MUTED)),
            bar_color,
        );
        let _ = out.queue(Print("\r\n"));

        let spans = build_display_spans(&state.buf, &state.pastes);
        let display_buf = spans_to_string(&spans);
        let display_cursor = map_cursor(state.cursor_char(), &state.buf, &spans);
        let (visual_lines, cursor_line, cursor_col) =
            wrap_and_locate_cursor(&display_buf, display_cursor, usable);
        let is_command = crate::completer::Completer::is_command(state.buf.trim());
        let is_exec = matches!(state.buf.as_bytes(), [b'!', c, ..] if !c.is_ascii_whitespace());
        let is_exec_invalid = state.buf == "!";
        let total_content_rows = visual_lines.len();
        let comp_total = if state.settings.is_some() {
            2
        } else {
            state.completer.as_ref().map(|c| c.results.len().min(5)).unwrap_or(0)
        };
        let mut comp_rows = comp_total;

        let fixed_base = stash_rows + queued_rows + 2;
        let mut fixed = fixed_base + comp_rows;
        let mut max_content_rows = height.saturating_sub(fixed);
        if max_content_rows == 0 {
            let available_for_comp = height.saturating_sub(fixed_base + 1);
            if available_for_comp == 0 {
                comp_rows = 0;
            } else {
                comp_rows = comp_rows.min(available_for_comp);
            }
            fixed = fixed_base + comp_rows;
            max_content_rows = height.saturating_sub(fixed);
            if max_content_rows == 0 {
                max_content_rows = 1;
            }
        }

        let content_rows = total_content_rows.min(max_content_rows);
        let mut scroll_offset = 0usize;
        if total_content_rows > content_rows {
            if cursor_line + 1 > content_rows {
                scroll_offset = cursor_line + 1 - content_rows;
            }
            if scroll_offset + content_rows > total_content_rows {
                scroll_offset = total_content_rows - content_rows;
            }
        }
        let cursor_line_visible = cursor_line
            .saturating_sub(scroll_offset)
            .min(content_rows.saturating_sub(1));

        for (li, line) in visual_lines
            .iter()
            .skip(scroll_offset)
            .take(content_rows)
            .enumerate()
        {
            let abs_idx = scroll_offset + li;
            let _ = out.queue(Print(" "));
            if is_command {
                let _ = out.queue(SetForegroundColor(theme::ACCENT));
                let _ = out.queue(Print(line));
                let _ = out.queue(ResetColor);
            } else if (is_exec || is_exec_invalid) && abs_idx == 0 && line.starts_with('!') {
                let _ = out.queue(SetForegroundColor(Color::Red));
                let _ = out.queue(SetAttribute(Attribute::Bold));
                let _ = out.queue(Print("!"));
                let _ = out.queue(SetAttribute(Attribute::Reset));
                let _ = out.queue(ResetColor);
                render_line_spans(out, &line[1..]);
            } else {
                render_line_spans(out, line);
            }
            let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
            let _ = out.queue(Print("\r\n"));
        }

        let mode_label = match mode {
            super::input::Mode::Plan => Some(("plan", theme::PLAN)),
            super::input::Mode::Apply => Some(("apply", theme::APPLY)),
            super::input::Mode::Normal => None,
        };
        draw_bar(out, width, None, mode_label, bar_color);

        if comp_rows > 0 {
            let _ = out.queue(Print("\r\n"));
        }
        let comp_rows = if state.settings.is_some() {
            draw_settings(out, state.settings.as_ref(), comp_rows)
        } else {
            draw_completions(out, state.completer.as_ref(), comp_rows)
        };

        let total_rows = stash_rows + queued_rows + 1 + content_rows + 1 + comp_rows;
        let new_rows = total_rows as u16;

        if prev_rows > new_rows {
            let n = prev_rows - new_rows;
            for _ in 0..n {
                let _ = out.queue(Print("\r\n"));
                let _ = out.queue(terminal::Clear(terminal::ClearType::CurrentLine));
            }
        }
        // Clear anything remaining below — catches edge cases where the previous
        // frame was taller due to pre-prompt section changes (active tool, blocks).
        let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));

        let rows_below: u16 = prev_rows.saturating_sub(new_rows);
        let total_drawn = pre_prompt_rows + new_rows + rows_below;
        let height = terminal::size().map(|(_, h)| h).unwrap_or(24);
        // If content would extend past terminal bottom, the terminal scrolls up
        let scrolled = draw_start_row + total_drawn > height;
        let top_row = if scrolled {
            height.saturating_sub(total_drawn)
        } else {
            draw_start_row
        };
        let text_row = top_row + pre_prompt_rows + 1 + extra_rows + cursor_line_visible as u16;
        let text_col = 1 + cursor_col as u16;
        let _ = out.queue(cursor::MoveTo(text_col, text_row));

        #[cfg(debug_assertions)]
        {
            let _ = out.flush();
            if let Ok((_, actual_row)) = cursor::position() {
                debug_assert_eq!(
                    actual_row, text_row,
                    "cursor row drift: calculated={text_row} actual={actual_row} \
                     top={top_row} pre_prompt={pre_prompt_rows} draw_start={draw_start_row}"
                );
            }
        }

        (top_row, total_rows as u16, scrolled)
    }

}

fn render_stash(out: &mut io::Stdout, stash: &Option<(String, usize, Vec<String>)>, usable: usize) -> u16 {
    let Some((ref stash_buf, _, _)) = stash else {
        return 0;
    };
    let first_line = stash_buf.lines().next().unwrap_or("");
    let line_count = stash_buf.lines().count();
    let max_chars = usable.saturating_sub(2);
    let display: String = first_line.chars().take(max_chars).collect();
    let suffix = if display.chars().count() < first_line.chars().count()
        || line_count > 1
    {
        "\u{2026}"
    } else {
        ""
    };
    let _ = out.queue(Print("  "));
    let _ = out.queue(SetAttribute(Attribute::Dim));
    let _ = out.queue(SetForegroundColor(theme::MUTED));
    let _ = out.queue(Print(format!("{}{}", display, suffix)));
    let _ = out.queue(SetAttribute(Attribute::Reset));
    let _ = out.queue(ResetColor);
    let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
    let _ = out.queue(Print("\r\n"));
    1
}

fn render_queued(out: &mut io::Stdout, queued: &[String], usable: usize) -> u16 {
    let mut rows = 0u16;
    for msg in queued {
        let indent = 2usize;
        let display: String = msg.chars().take(usable.saturating_sub(indent)).collect();
        let _ = out.queue(Print(" ".repeat(indent)));
        let _ = out.queue(SetBackgroundColor(theme::USER_BG));
        let _ = out.queue(SetAttribute(Attribute::Bold));
        let _ = out.queue(Print(format!(" {} ", display)));
        let _ = out.queue(SetAttribute(Attribute::Reset));
        let _ = out.queue(ResetColor);
        let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
        let _ = out.queue(Print("\r\n"));
        rows += 1;
    }
    rows
}

pub fn term_width() -> usize {
    terminal::size().map(|(w, _)| w as usize).unwrap_or(80)
}

pub fn term_height() -> usize {
    terminal::size().map(|(_, h)| h as usize).unwrap_or(24)
}

pub(super) fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut truncated: String = s.chars().take(max.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}

pub fn erase_prompt_at(row: u16) {
    let mut out = io::stdout();
    let _ = out.queue(terminal::BeginSynchronizedUpdate);
    let _ = out.queue(cursor::MoveTo(0, row));
    let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));
    let _ = out.queue(terminal::EndSynchronizedUpdate);
    let _ = out.flush();
}

pub fn tool_arg_summary(name: &str, args: &HashMap<String, serde_json::Value>) -> String {
    match name {
        "bash" => {
            let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
            cmd.lines().next().unwrap_or("").to_string()
        }
        "read_file" | "write_file" | "edit_file" => args
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .into(),
        "glob" => args
            .get("pattern")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .into(),
        "grep" => {
            let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            match args.get("path").and_then(|v| v.as_str()) {
                Some(p) => format!("{} in {}", pattern, p),
                None => pattern.into(),
            }
        }
        "ask_user_question" => {
            let count = args
                .get("questions")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            format!("{} question{}", count, if count == 1 { "" } else { "s" })
        }
        "exit_plan_mode" => "plan ready".into(),
        _ => String::new(),
    }
}

pub fn tool_timeout_label(args: &HashMap<String, serde_json::Value>) -> Option<String> {
    let ms = args.get("timeout_ms").and_then(|v| v.as_u64())?;
    let secs = ms as f64 / 1000.0;
    if secs >= 60.0 {
        Some(format!(
            "timeout: {}m{:.0}s",
            (secs / 60.0) as u64,
            secs % 60.0
        ))
    } else {
        Some(format!("timeout: {:.0}s", secs))
    }
}

fn format_tokens(n: u32) -> String {
    if n >= 1_000_000 {
        format!("{:.1}m", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{}", n)
    }
}

fn wrap_and_locate_cursor(
    buf: &str,
    cursor_char: usize,
    usable: usize,
) -> (Vec<String>, usize, usize) {
    let mut visual_lines: Vec<String> = Vec::new();
    let mut cursor_line = 0;
    let mut cursor_col = 0;
    let mut chars_seen = 0usize;
    let mut cursor_set = false;

    for text_line in buf.split('\n') {
        let chars: Vec<char> = text_line.chars().collect();
        if chars.is_empty() {
            if !cursor_set && chars_seen == cursor_char {
                cursor_line = visual_lines.len();
                cursor_col = 0;
                cursor_set = true;
            }
            visual_lines.push(String::new());
        } else {
            let chunks: Vec<_> = chars.chunks(usable.max(1)).collect();
            for (ci, chunk) in chunks.iter().enumerate() {
                let line_start = chars_seen;
                let is_last_chunk = ci == chunks.len() - 1;
                if !cursor_set
                    && cursor_char >= line_start
                    && (cursor_char < line_start + chunk.len()
                        || (is_last_chunk && cursor_char == line_start + chunk.len()))
                {
                    cursor_line = visual_lines.len();
                    cursor_col = cursor_char - line_start;
                    cursor_set = true;
                }
                chars_seen += chunk.len();
                visual_lines.push(chunk.iter().collect());
            }
        }
        chars_seen += 1;
    }
    if visual_lines.is_empty() {
        visual_lines.push(String::new());
    }
    (visual_lines, cursor_line, cursor_col)
}

pub(super) struct BarSpan {
    text: String,
    color: Color,
    attr: Option<Attribute>,
}

pub(super) fn draw_bar(
    out: &mut io::Stdout,
    width: usize,
    left: Option<&[BarSpan]>,
    right: Option<(&str, Color)>,
    bar_color: Color,
) {
    let dash = "\u{2500}";

    let left_len: usize = left
        .map(|spans| 1 + 1 + spans.iter().map(|s| s.text.chars().count()).sum::<usize>() + 1)
        .unwrap_or(0);
    let right_len: usize = right
        .map(|(text, _)| 1 + text.chars().count() + 1 + 1)
        .unwrap_or(0);
    let bar_len = width.saturating_sub(left_len + right_len);

    if let Some(spans) = left {
        let _ = out.queue(SetForegroundColor(bar_color));
        let _ = out.queue(Print(dash));
        let _ = out.queue(ResetColor);
        let _ = out.queue(Print(" "));
        for span in spans {
            if let Some(attr) = span.attr {
                let _ = out.queue(SetAttribute(attr));
            }
            let _ = out.queue(SetForegroundColor(span.color));
            let _ = out.queue(Print(&span.text));
            let _ = out.queue(ResetColor);
            if span.attr.is_some() {
                let _ = out.queue(SetAttribute(Attribute::Reset));
            }
        }
        let _ = out.queue(Print(" "));
    }

    let _ = out.queue(SetForegroundColor(bar_color));
    let _ = out.queue(Print(dash.repeat(bar_len)));
    let _ = out.queue(ResetColor);

    if let Some((text, color)) = right {
        let _ = out.queue(SetForegroundColor(color));
        let _ = out.queue(Print(format!(" {} ", text)));
        let _ = out.queue(ResetColor);
        let _ = out.queue(SetForegroundColor(bar_color));
        let _ = out.queue(Print(dash));
        let _ = out.queue(ResetColor);
    }
}

enum Span {
    Plain(String),
    Paste(String),
    AtRef(String),
}

fn build_display_spans(buf: &str, pastes: &[String]) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut plain = String::new();
    let mut paste_idx = 0;

    let chars: Vec<char> = buf.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == PASTE_MARKER {
            if !plain.is_empty() {
                spans.push(Span::Plain(std::mem::take(&mut plain)));
            }
            let lines = pastes
                .get(paste_idx)
                .map(|p| p.lines().count().max(1))
                .unwrap_or(1);
            spans.push(Span::Paste(format!("[pasted {} lines]", lines)));
            paste_idx += 1;
            i += 1;
        } else if chars[i] == '@' {
            let at_start = i == 0 || chars[i - 1].is_whitespace();
            if at_start {
                if !plain.is_empty() {
                    spans.push(Span::Plain(std::mem::take(&mut plain)));
                }
                let mut end = i + 1;
                while end < chars.len() && !chars[end].is_whitespace() {
                    end += 1;
                }
                if end > i + 1 {
                    let token: String = chars[i..end].iter().collect();
                    spans.push(Span::AtRef(token));
                    i = end;
                } else {
                    spans.push(Span::AtRef("@".to_string()));
                    i += 1;
                }
            } else {
                plain.push(chars[i]);
                i += 1;
            }
        } else {
            plain.push(chars[i]);
            i += 1;
        }
    }
    if !plain.is_empty() {
        spans.push(Span::Plain(plain));
    }
    spans
}

fn spans_to_string(spans: &[Span]) -> String {
    let mut s = String::new();
    for span in spans {
        match span {
            Span::Plain(t) | Span::Paste(t) | Span::AtRef(t) => s.push_str(t),
        }
    }
    s
}

fn map_cursor(raw_cursor: usize, raw_buf: &str, spans: &[Span]) -> usize {
    let mut raw_pos = 0;
    let mut display_pos = 0;
    for span in spans {
        match span {
            Span::Plain(t) => {
                let chars = t.chars().count();
                if raw_cursor >= raw_pos && raw_cursor < raw_pos + chars {
                    return display_pos + (raw_cursor - raw_pos);
                }
                raw_pos += chars;
                display_pos += chars;
            }
            Span::Paste(label) => {
                if raw_cursor == raw_pos {
                    return display_pos;
                }
                raw_pos += 1;
                display_pos += label.chars().count();
            }
            Span::AtRef(token) => {
                let chars = token.chars().count();
                if raw_cursor >= raw_pos && raw_cursor < raw_pos + chars {
                    return display_pos + (raw_cursor - raw_pos);
                }
                raw_pos += chars;
                display_pos += chars;
            }
        }
    }
    let _ = raw_buf;
    display_pos
}

fn render_line_spans(out: &mut io::Stdout, line: &str) {
    let mut rest = line;
    while !rest.is_empty() {
        let paste_pos = rest.find("[pasted ");
        let at_pos = rest.find('@');

        let next = match (paste_pos, at_pos) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };

        let Some(pos) = next else {
            let _ = out.queue(Print(rest));
            break;
        };

        if pos > 0 {
            let _ = out.queue(Print(&rest[..pos]));
        }

        if paste_pos == Some(pos) {
            if let Some(end) = rest[pos..].find(']') {
                let label = &rest[pos..pos + end + 1];
                if label.ends_with(" lines]") {
                    let _ = out.queue(SetForegroundColor(theme::ACCENT));
                    let _ = out.queue(Print(label));
                    let _ = out.queue(ResetColor);
                    rest = &rest[pos + end + 1..];
                    continue;
                }
            }
            let _ = out.queue(Print(&rest[pos..pos + 1]));
            rest = &rest[pos + 1..];
        } else if at_pos == Some(pos) {
            let after = &rest[pos + 1..];
            let tok_end = after.find(char::is_whitespace).unwrap_or(after.len());
            let token = &rest[pos..pos + 1 + tok_end];
            let path_str = &token[1..];
            if !path_str.is_empty() && std::path::Path::new(path_str).exists() {
                let _ = out.queue(SetForegroundColor(theme::ACCENT));
                let _ = out.queue(Print(token));
                let _ = out.queue(ResetColor);
            } else {
                let _ = out.queue(Print(token));
            }
            rest = &rest[pos + 1 + tok_end..];
        } else {
            let _ = out.queue(Print(&rest[pos..pos + 1]));
            rest = &rest[pos + 1..];
        }
    }
}

fn draw_completions(out: &mut io::Stdout, completer: Option<&crate::completer::Completer>, max_rows: usize) -> usize {
    let Some(comp) = completer else {
        return 0;
    };
    if comp.results.is_empty() || max_rows == 0 {
        return 0;
    }
    let total = comp.results.len();
    let max_rows = max_rows.min(total);
    let mut start = 0;
    if total > max_rows {
        let half = max_rows / 2;
        start = comp.selected.saturating_sub(half);
        if start + max_rows > total {
            start = total - max_rows;
        }
    }
    let end = start + max_rows;
    let last = max_rows - 1;
    let prefix = match comp.kind {
        crate::completer::CompleterKind::Command => "/",
        crate::completer::CompleterKind::File => "./",
        crate::completer::CompleterKind::History => "",
    };
    let max_label = comp
        .results
        .iter()
        .map(|i| prefix.len() + i.label.len())
        .max()
        .unwrap_or(0);
    let avail = term_width().saturating_sub(2);
    for (i, item) in comp.results[start..end].iter().enumerate() {
        let idx = start + i;
        let _ = out.queue(Print("  "));
        let raw = format!("{}{}", prefix, item.label);
        let label: String = raw.chars().take(avail).collect();
        if idx == comp.selected {
            let _ = out.queue(SetForegroundColor(theme::ACCENT));
            let _ = out.queue(Print(&label));
            if let Some(ref desc) = item.description {
                let pad = max_label - label.len() + 2;
                let _ = out.queue(ResetColor);
                let _ = out.queue(SetAttribute(Attribute::Dim));
                let _ = out.queue(Print(format!("{:>width$}{}", "", desc, width = pad)));
                let _ = out.queue(SetAttribute(Attribute::Reset));
            }
            let _ = out.queue(ResetColor);
        } else {
            let _ = out.queue(SetAttribute(Attribute::Dim));
            let _ = out.queue(Print(&label));
            if let Some(ref desc) = item.description {
                let pad = max_label - label.len() + 2;
                let _ = out.queue(Print(format!("{:>width$}{}", "", desc, width = pad)));
            }
            let _ = out.queue(SetAttribute(Attribute::Reset));
        }
        let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
        if i < last {
            let _ = out.queue(Print("\r\n"));
        }
    }
    max_rows
}

fn draw_settings(out: &mut io::Stdout, settings: Option<&SettingsMenu>, max_rows: usize) -> usize {
    let Some(s) = settings else {
        return 0;
    };
    if max_rows == 0 {
        return 0;
    }
    let rows = [
        ("vim mode", s.vim_enabled, 0usize),
        ("auto compact", s.auto_compact, 1usize),
    ];
    let col = rows.iter().map(|(l, _, _)| l.len()).max().unwrap_or(0) + 4;
    let mut drawn = 0;
    for (label, value, idx) in &rows {
        if drawn >= max_rows {
            break;
        }
        if drawn > 0 {
            let _ = out.queue(Print("\r\n"));
        }
        let _ = out.queue(Print("  "));
        if *idx == s.selected {
            let _ = out.queue(SetForegroundColor(theme::ACCENT));
            let _ = out.queue(Print(label));
            let _ = out.queue(ResetColor);
        } else {
            let _ = out.queue(SetAttribute(Attribute::Dim));
            let _ = out.queue(Print(label));
            let _ = out.queue(SetAttribute(Attribute::Reset));
        }
        let padding = " ".repeat(col - label.len());
        let _ = out.queue(SetAttribute(Attribute::Dim));
        let _ = out.queue(Print(format!(
            "{}{}",
            padding,
            if *value { "on" } else { "off" }
        )));
        let _ = out.queue(SetAttribute(Attribute::Reset));
        let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
        drawn += 1;
    }
    drawn
}
