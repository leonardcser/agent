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

pub struct Screen {
    blocks: Vec<Block>,
    flushed: usize,
    last_block_rows: u16,
    active_tool: Option<ActiveTool>,
    prompt_drawn: bool,
    prompt_dirty: bool,
    prompt_top_row: u16,
    working_since: Option<Instant>,
    final_elapsed: Option<Duration>,
    context_tokens: Option<u32>,
    throbber: Option<Throbber>,
    last_spinner_frame: usize,
    prev_prompt_rows: u16,
    retry_deadline: Option<Instant>,
}

impl Screen {
    pub fn new() -> Self {
        Self {
            blocks: Vec::new(),
            flushed: 0,
            last_block_rows: 0,
            active_tool: None,
            prompt_drawn: false,
            prompt_dirty: true,
            prompt_top_row: 0,
            working_since: None,
            final_elapsed: None,
            context_tokens: None,
            throbber: None,
            last_spinner_frame: usize::MAX,
            prev_prompt_rows: 0,
            retry_deadline: None,
        }
    }

    pub fn begin_turn(&mut self) {
        self.last_block_rows = 0;
        self.active_tool = None;
    }

    pub fn push(&mut self, block: Block) {
        self.blocks.push(block);
        self.prompt_dirty = true;
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
        });
        self.prompt_dirty = true;
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
            self.prompt_dirty = true;
        }
    }

    pub fn set_active_status(&mut self, status: ToolStatus) {
        if let Some(ref mut tool) = self.active_tool {
            tool.status = status;
            self.prompt_dirty = true;
        }
    }

    pub fn finish_tool(
        &mut self,
        status: ToolStatus,
        output: Option<ToolOutput>,
        elapsed: Option<Duration>,
    ) {
        if let Some(tool) = self.active_tool.take() {
            self.blocks.push(Block::ToolCall {
                name: tool.name,
                summary: tool.summary,
                args: tool.args,
                status,
                elapsed,
                output,
            });
            self.prompt_dirty = true;
        }
    }

    pub fn set_context_tokens(&mut self, tokens: u32) {
        self.context_tokens = Some(tokens);
        self.prompt_dirty = true;
    }

    pub fn clear_context_tokens(&mut self) {
        self.context_tokens = None;
        self.prompt_dirty = true;
    }

    pub fn context_tokens(&self) -> Option<u32> {
        self.context_tokens
    }

    pub fn set_throbber(&mut self, state: Throbber) {
        let is_active = matches!(
            state,
            Throbber::Working | Throbber::Retrying { .. } | Throbber::Compacting
        );
        if is_active && self.working_since.is_none() {
            self.working_since = Some(Instant::now());
            self.final_elapsed = None;
        }
        if !is_active {
            self.final_elapsed = self.working_since.map(|s| s.elapsed());
            self.working_since = None;
        }
        self.retry_deadline = match state {
            Throbber::Retrying { delay, .. } => Some(Instant::now() + delay),
            _ => None,
        };
        self.throbber = Some(state);
        self.prompt_dirty = true;
    }

    pub fn clear_throbber(&mut self) {
        self.throbber = None;
        self.working_since = None;
        self.final_elapsed = None;
        self.prompt_dirty = true;
    }

    pub fn mark_dirty(&mut self) {
        self.prompt_dirty = true;
    }

    pub fn flush_blocks(&mut self) {
        if let Some(tool) = self.active_tool.take() {
            self.blocks.push(Block::ToolCall {
                name: tool.name,
                summary: tool.summary,
                args: tool.args,
                status: tool.status,
                elapsed: None,
                output: tool.output,
            });
        }
        let mut out = io::stdout();
        if self.prompt_drawn {
            let _ = out.queue(cursor::MoveTo(0, self.prompt_top_row));
            let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));
            self.prompt_drawn = false;
        }
        self.render_blocks();
        let _ = out.flush();
    }

    pub fn erase_prompt(&mut self) {
        if self.prompt_drawn {
            erase_prompt_at(self.prompt_top_row);
            self.prompt_drawn = false;
        }
    }

    pub fn redraw_all(&mut self) {
        let mut out = io::stdout();
        let _ = out.queue(cursor::MoveTo(0, 0));
        let _ = out.queue(terminal::Clear(terminal::ClearType::All));
        let _ = out.queue(terminal::Clear(terminal::ClearType::Purge));
        let _ = out.flush();
        self.flushed = 0;
        self.last_block_rows = 0;
        self.prompt_drawn = false;
        self.prompt_dirty = true;
        self.prev_prompt_rows = 0;
    }

    pub fn clear(&mut self) {
        self.blocks.clear();
        self.flushed = 0;
        self.last_block_rows = 0;
        self.active_tool = None;
        self.prompt_drawn = false;
        self.prompt_dirty = true;
        self.prev_prompt_rows = 0;
        self.throbber = None;
        self.working_since = None;
        self.final_elapsed = None;
        self.context_tokens = None;
        self.retry_deadline = None;
        let mut out = io::stdout();
        let _ = out.queue(cursor::MoveTo(0, 0));
        let _ = out.queue(terminal::Clear(terminal::ClearType::All));
        let _ = out.queue(terminal::Clear(terminal::ClearType::Purge));
        let _ = out.flush();
    }

    pub fn has_history(&self) -> bool {
        !self.blocks.is_empty()
    }

    /// Returns (block_index, full_text) for each User block.
    pub fn user_turns(&self) -> Vec<(usize, String)> {
        self.blocks
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
        self.blocks.truncate(block_idx);
        self.flushed = self.flushed.min(block_idx);
        self.active_tool = None;
        self.prompt_dirty = true;
        self.redraw_all();
    }

    fn render_blocks(&mut self) {
        let has_new = self.flushed < self.blocks.len();
        if !has_new {
            return;
        }

        let mut out = io::stdout();
        let w = term_width();
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
                let _ = out.queue(Print("\r\n"));
            }
            let rows = render_block(&self.blocks[i], w);
            if i == last_idx {
                self.last_block_rows = rows + gap;
            }
        }
        self.flushed = self.blocks.len();
        self.prompt_drawn = false;
    }

    pub fn draw_prompt(&mut self, state: &InputState, mode: super::input::Mode, width: usize) {
        self.prompt_dirty = true;
        self.draw_prompt_with_queued(state, mode, width, &[]);
    }

    pub fn draw_prompt_with_queued(
        &mut self,
        state: &InputState,
        mode: super::input::Mode,
        width: usize,
        queued: &[String],
    ) {
        if let Some(start) = self.working_since {
            let frame = (start.elapsed().as_millis() / 150) as usize % SPINNER_FRAMES.len();
            if frame != self.last_spinner_frame {
                self.last_spinner_frame = frame;
                self.prompt_dirty = true;
            }
        }

        let has_new_blocks = self.flushed < self.blocks.len();
        if !has_new_blocks && !self.prompt_dirty {
            return;
        }

        let mut out = io::stdout();
        let _ = out.queue(Print("\x1b[?2026h"));

        if self.prompt_drawn {
            let _ = out.queue(cursor::MoveTo(0, self.prompt_top_row));
            let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));
        }

        self.render_blocks();

        let mut active_rows: u16 = 0;
        if let Some(ref tool) = self.active_tool {
            let tool_gap = if let Some(last) = self.blocks.last() {
                gap_between(&Element::Block(last), &Element::ActiveTool)
            } else {
                0
            };
            for _ in 0..tool_gap {
                let _ = out.queue(terminal::Clear(terminal::ClearType::CurrentLine));
                let _ = out.queue(Print("\r\n"));
            }
            let rows = render_tool(
                &tool.name,
                &tool.summary,
                &tool.args,
                tool.status,
                None,
                tool.output.as_ref(),
            );
            active_rows = tool_gap + rows;
        }

        let gap = if self.active_tool.is_some() {
            gap_between(&Element::ActiveTool, &Element::Prompt)
        } else {
            self.blocks.last().map_or(0, |last| {
                gap_between(&Element::Block(last), &Element::Prompt)
            })
        };
        for _ in 0..gap {
            let _ = out.queue(terminal::Clear(terminal::ClearType::CurrentLine));
            let _ = out.queue(Print("\r\n"));
        }

        let pre_prompt = active_rows + gap;
        let (top_row, new_rows) = self.draw_prompt_sections(
            state,
            mode,
            width,
            queued,
            self.prev_prompt_rows.saturating_sub(pre_prompt),
        );
        self.prev_prompt_rows = pre_prompt + new_rows;

        self.prompt_top_row = top_row.saturating_sub(pre_prompt);
        self.prompt_drawn = true;
        self.prompt_dirty = false;

        let _ = out.queue(Print("\x1b[?2026l"));
        let _ = out.flush();
    }

    /// Returns (top_row, total_prompt_rows).
    fn draw_prompt_sections(
        &self,
        state: &InputState,
        mode: super::input::Mode,
        width: usize,
        queued: &[String],
        prev_rows: u16,
    ) -> (u16, u16) {
        let mut out = io::stdout();
        let usable = width.saturating_sub(1);
        let height = terminal::size().map(|(_, h)| h as usize).unwrap_or(24);
        let mut extra_rows: u16 = 0;
        let stash_rows = if state.stash.is_some() { 1 } else { 0 };
        let queued_rows = queued.len();

        if let Some((ref stash_buf, _, _)) = state.stash {
            let first_line = stash_buf.lines().next().unwrap_or("");
            let line_count = stash_buf.lines().count();
            let max_chars = usable.saturating_sub(2);
            let display: String = first_line.chars().take(max_chars).collect();
            let suffix = if display.chars().count() < first_line.chars().count() {
                "\u{2026}" // ellipsis
            } else if line_count > 1 {
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
            extra_rows += 1;
        }

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
            extra_rows += 1;
        }

        let vi_normal = state.vim_mode() == Some(crate::vim::ViMode::Normal);
        let bar_color = if vi_normal { theme::ACCENT } else { theme::BAR };

        let tokens_label = self.context_tokens.map(format_tokens);
        let throbber_spans = self.throbber_spans();
        draw_bar(
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
        let is_exec = state.buf.starts_with('!');
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
            } else if is_exec && abs_idx == 0 && line.starts_with('!') {
                let _ = out.queue(SetForegroundColor(theme::EXEC));
                let _ = out.queue(SetAttribute(Attribute::Bold));
                let _ = out.queue(Print("!"));
                let _ = out.queue(SetAttribute(Attribute::Reset));
                let _ = out.queue(ResetColor);
                render_line_spans(&line[1..]);
            } else {
                render_line_spans(line);
            }
            let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
            let _ = out.queue(Print("\r\n"));
        }

        let mode_label = match mode {
            super::input::Mode::Plan => Some(("plan", theme::PLAN)),
            super::input::Mode::Apply => Some(("apply", theme::APPLY)),
            super::input::Mode::Normal => None,
        };
        draw_bar(width, None, mode_label, bar_color);

        if comp_rows > 0 {
            let _ = out.queue(Print("\r\n"));
        }
        let comp_rows = if state.settings.is_some() {
            draw_settings(state.settings.as_ref(), comp_rows)
        } else {
            draw_completions(state.completer.as_ref(), comp_rows)
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

        let _ = out.flush();
        let final_row = cursor::position().map(|(_, y)| y).unwrap_or(0);
        let rows_below = if prev_rows > new_rows { prev_rows - new_rows } else { 0 };
        let top_row = final_row.saturating_sub(new_rows + rows_below - 1);
        let text_row = top_row + 1 + extra_rows as u16 + cursor_line_visible as u16;
        let text_col = 1 + cursor_col as u16;
        let _ = out.queue(cursor::MoveTo(text_col, text_row));

        (top_row, total_rows as u16)
    }

    fn throbber_spans(&self) -> Vec<BarSpan> {
        let Some(state) = self.throbber else {
            return vec![];
        };
        match state {
            Throbber::Compacting => {
                let Some(start) = self.working_since else {
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
                let Some(start) = self.working_since else {
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

pub fn erase_prompt_at(top_row: u16) {
    let mut out = io::stdout();
    let _ = out.queue(cursor::MoveTo(0, top_row));
    let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));
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
    width: usize,
    left: Option<&[BarSpan]>,
    right: Option<(&str, Color)>,
    bar_color: Color,
) {
    let mut out = io::stdout();
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

fn render_line_spans(line: &str) {
    let mut out = io::stdout();
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

fn draw_completions(completer: Option<&crate::completer::Completer>, max_rows: usize) -> usize {
    let Some(comp) = completer else {
        return 0;
    };
    if comp.results.is_empty() || max_rows == 0 {
        return 0;
    }
    let mut out = io::stdout();
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

fn draw_settings(settings: Option<&SettingsMenu>, max_rows: usize) -> usize {
    let Some(s) = settings else {
        return 0;
    };
    if max_rows == 0 {
        return 0;
    }
    let mut out = io::stdout();
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
