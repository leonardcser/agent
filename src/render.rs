use crate::input::{InputState, PASTE_MARKER};
use crate::theme;
use comfy_table::{presets::UTF8_BORDERS_ONLY, ContentArrangement, Table};
use crossterm::{
    cursor,
    style::{
        Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
    },
    terminal, ExecutableCommand,
};
use std::collections::HashMap;
use std::io::{self, Write};
use std::path::Path;
use std::time::{Duration, Instant};
use syntect::easy::HighlightLines;
use syntect::highlighting::Style;
use syntect::parsing::SyntaxSet;

use std::sync::LazyLock;

static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static THEME_SET: LazyLock<two_face::theme::EmbeddedLazyThemeSet> =
    LazyLock::new(two_face::theme::extra);

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
}

#[derive(Clone, Copy, PartialEq)]
pub enum ConfirmChoice {
    Yes,
    No,
    Always,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Throbber {
    /// Agent is running. Stores the start instant for elapsed time.
    Working,
    Done,
    Interrupted,
}

pub struct Screen {
    blocks: Vec<Block>,
    flushed: usize,
    last_block_rows: u16,
    rerender: bool,
    prompt_drawn: bool,
    prompt_top_row: u16,
    working_since: Option<Instant>,
    final_elapsed: Option<Duration>,
    context_tokens: Option<u32>,
    throbber: Option<Throbber>,
}

impl Screen {
    pub fn new() -> Self {
        Self {
            blocks: Vec::new(),
            flushed: 0,
            last_block_rows: 0,
            rerender: false,
            prompt_drawn: false,
            prompt_top_row: 0,
            working_since: None,
            final_elapsed: None,
            context_tokens: None,
            throbber: None,
        }
    }

    pub fn begin_turn(&mut self) {
        self.last_block_rows = 0;
        self.rerender = false;
    }

    pub fn push(&mut self, block: Block) {
        self.blocks.push(block);
    }

    pub fn update_last_tool(&mut self, status: ToolStatus, output: Option<ToolOutput>, elapsed: Option<Duration>) {
        let idx = self.blocks.iter().rposition(|b| matches!(b, Block::ToolCall { .. }));
        if let Some(idx) = idx {
            if let Block::ToolCall { status: ref mut s, elapsed: ref mut e, output: ref mut o, .. } = self.blocks[idx] {
                *s = status;
                *e = elapsed;
                *o = output;
            }
            if idx < self.flushed {
                self.flushed = idx;
                self.rerender = true;
            }
        }
    }

    pub fn append_tool_output(&mut self, chunk: &str) {
        let idx = self.blocks.iter().rposition(|b| matches!(b, Block::ToolCall { .. }));
        if let Some(idx) = idx {
            if let Block::ToolCall { output: ref mut o, .. } = self.blocks[idx] {
                match o {
                    Some(ref mut out) => {
                        if !out.content.is_empty() { out.content.push('\n'); }
                        out.content.push_str(chunk);
                    }
                    None => {
                        *o = Some(ToolOutput { content: chunk.to_string(), is_error: false });
                    }
                }
            }
            if idx < self.flushed {
                self.flushed = idx;
                self.rerender = true;
            }
        }
    }

    pub fn set_last_tool_status(&mut self, status: ToolStatus) {
        let idx = self.blocks.iter().rposition(|b| matches!(b, Block::ToolCall { .. }));
        if let Some(idx) = idx {
            if let Block::ToolCall { status: ref mut s, .. } = self.blocks[idx] {
                *s = status;
            }
            if idx < self.flushed {
                self.flushed = idx;
                self.rerender = true;
            }
        }
    }

    pub fn set_context_tokens(&mut self, tokens: u32) {
        self.context_tokens = Some(tokens);
    }

    pub fn set_throbber(&mut self, state: Throbber) {
        if state == Throbber::Working && self.working_since.is_none() {
            self.working_since = Some(Instant::now());
            self.final_elapsed = None;
        }
        if state != Throbber::Working {
            self.final_elapsed = self.working_since.map(|s| s.elapsed());
            self.working_since = None;
        }
        self.throbber = Some(state);
    }

    pub fn clear_throbber(&mut self) {
        self.throbber = None;
        self.working_since = None;
        self.final_elapsed = None;
    }

    pub fn flush_blocks(&mut self) {
        self.render_blocks();
    }

    pub fn erase_prompt(&mut self) {
        if self.prompt_drawn {
            erase_prompt_at(self.prompt_top_row);
            self.prompt_drawn = false;
        }
    }

    pub fn redraw_all(&mut self) {
        let mut out = io::stdout();
        let _ = out.execute(cursor::MoveTo(0, 0));
        let _ = out.execute(terminal::Clear(terminal::ClearType::All));
        let _ = out.execute(terminal::Clear(terminal::ClearType::Purge));
        let _ = out.execute(Print("\r\n"));
        let _ = out.flush();
        self.flushed = 0;
        self.last_block_rows = 0;
        self.rerender = false;
        self.prompt_drawn = false;
    }

    pub fn clear(&mut self) {
        self.blocks.clear();
        self.flushed = 0;
        self.last_block_rows = 0;
        self.rerender = false;
        self.prompt_drawn = false;
        self.throbber = None;
        self.working_since = None;
        self.final_elapsed = None;
        let mut out = io::stdout();
        let _ = out.execute(cursor::MoveTo(0, 0));
        let _ = out.execute(terminal::Clear(terminal::ClearType::All));
        let _ = out.execute(terminal::Clear(terminal::ClearType::Purge));
        let _ = out.flush();
    }

    pub fn has_history(&self) -> bool {
        !self.blocks.is_empty()
    }

    // ── Block rendering ──────────────────────────────────────────────────

    fn render_blocks(&mut self) {
        let has_new = self.flushed < self.blocks.len();
        if !has_new && !self.rerender {
            if self.prompt_drawn {
                erase_prompt_at(self.prompt_top_row);
            }
            self.prompt_drawn = false;
            return;
        }

        let mut out = io::stdout();
        if self.rerender {
            let erase_from = if self.prompt_drawn {
                self.prompt_top_row.saturating_sub(self.last_block_rows)
            } else {
                let _ = out.flush();
                cursor::position().map(|(_, y)| y).unwrap_or(0).saturating_sub(self.last_block_rows)
            };
            let _ = out.execute(cursor::MoveTo(0, erase_from));
            let _ = out.execute(terminal::Clear(terminal::ClearType::FromCursorDown));
            let _ = out.flush();
            self.rerender = false;
        } else if self.prompt_drawn {
            erase_prompt_at(self.prompt_top_row);
        } else {
            let _ = out.flush();
            let pos = cursor::position().map(|(_, y)| y).unwrap_or(0);
            let _ = out.execute(cursor::MoveTo(0, pos));
            let _ = out.execute(terminal::Clear(terminal::ClearType::FromCursorDown));
            let _ = out.flush();
        }

        let w = term_width();
        let last_idx = self.blocks.len().saturating_sub(1);
        for i in self.flushed..self.blocks.len() {
            let gap = if i > 0 {
                gap_between(&Element::Block(&self.blocks[i - 1]), &Element::Block(&self.blocks[i]))
            } else {
                0
            };
            for _ in 0..gap { let _ = out.execute(Print("\r\n")); }
            let rows = render_block(&self.blocks[i], w);
            if i == last_idx {
                self.last_block_rows = rows + gap;
            }
        }
        self.flushed = self.blocks.len();
        self.prompt_drawn = false;
    }

    // ── Prompt drawing (broken into sections) ────────────────────────────

    /// Main entry point: flush blocks, then draw the full prompt box.
    pub fn draw_prompt(&mut self, state: &InputState, mode: super::input::Mode, width: usize) {
        self.draw_prompt_with_queued(state, mode, width, &[]);
    }

    pub fn draw_prompt_with_queued(&mut self, state: &InputState, mode: super::input::Mode, width: usize, queued: &[String]) {
        self.render_blocks();

        let gap = self.blocks.last().map_or(0, |last| {
            gap_between(&Element::Block(last), &Element::Throbber)
        });
        for _ in 0..gap { let _ = io::stdout().execute(Print("\r\n")); }

        self.prompt_top_row = self.draw_prompt_sections(state, mode, width, queued);
        if gap > 0 {
            self.prompt_top_row = self.prompt_top_row.saturating_sub(gap);
        }
        self.prompt_drawn = true;
    }

    fn draw_prompt_sections(
        &self,
        state: &InputState,
        mode: super::input::Mode,
        width: usize,
        queued: &[String],
    ) -> u16 {
        let mut out = io::stdout();
        let usable = width.saturating_sub(1);
        let mut extra_rows: u16 = 0;

        // 1. Throbber
        extra_rows += self.draw_throbber();

        // 2. Queued messages
        for msg in queued {
            let display: String = msg.chars().take(usable).collect();
            let _ = out.execute(SetBackgroundColor(theme::USER_BG));
            let _ = out.execute(SetAttribute(Attribute::Bold));
            let _ = out.execute(Print(format!(" {} ", display)));
            let _ = out.execute(SetAttribute(Attribute::Reset));
            let _ = out.execute(ResetColor);
            let _ = out.execute(Print("\r\n"));
            extra_rows += 1;
        }

        // Vim normal mode colors both bars with accent.
        let vi_normal = state.vim_mode() == Some(crate::vim::ViMode::Normal);
        let bar_color = if vi_normal { theme::ACCENT } else { theme::BAR };

        // 3. Top bar
        let tokens_label = self.context_tokens.map(format_tokens);
        draw_bar(width, tokens_label.as_deref().map(|l| (l, bar_color)), bar_color);
        let _ = out.execute(Print("\r\n"));

        // 4. Content with structured spans
        let spans = build_display_spans(&state.buf, &state.pastes);
        let display_buf = spans_to_string(&spans);
        let display_cursor = map_cursor(state.cursor_char(), &state.buf, &spans);
        let (visual_lines, cursor_line, cursor_col) = wrap_and_locate_cursor(&display_buf, display_cursor, usable);
        let is_command = matches!(state.buf.trim(), "/clear" | "/new" | "/exit" | "/quit" | "/vim");
        for line in &visual_lines {
            let _ = out.execute(Print(" "));
            if is_command {
                let _ = out.execute(SetForegroundColor(theme::ACCENT));
                let _ = out.execute(Print(line));
                let _ = out.execute(ResetColor);
            } else {
                render_line_spans(line);
            }
            let _ = out.execute(Print("\r\n"));
        }

        // 5. Bottom bar
        if mode == super::input::Mode::Apply {
            draw_bar(width, Some(("apply", theme::APPLY)), bar_color);
        } else {
            draw_bar(width, None, bar_color);
        }

        // 6. Completion list (below the bar)
        let has_comp = state.completer.as_ref().is_some_and(|c| !c.results.is_empty());
        if has_comp {
            let _ = out.execute(Print("\r\n"));
        }
        let comp_rows = draw_completions(state.completer.as_ref());

        let _ = out.flush();

        // Position cursor.
        // Rows from top_row to final_row (inclusive):
        //   top_bar, content lines, bottom_bar, [completion lines if any]
        let total_rows = extra_rows as usize + 1 + visual_lines.len() + 1 + comp_rows;
        let final_row = cursor::position().map(|(_, y)| y).unwrap_or(0);
        let top_row = final_row.saturating_sub(total_rows as u16 - 1);
        let text_row = top_row + 1 + extra_rows + cursor_line as u16;
        let text_col = 1 + cursor_col as u16;
        let _ = out.execute(cursor::MoveTo(text_col, text_row));
        let _ = out.flush();

        top_row
    }

    fn draw_throbber(&self) -> u16 {
        let Some(state) = self.throbber else { return 0 };
        let mut out = io::stdout();
        let _ = out.execute(Print(" "));
        match state {
            Throbber::Working => {
                if let Some(start) = self.working_since {
                    let elapsed = start.elapsed();
                    let idx = (elapsed.as_millis() / 150) as usize % SPINNER_FRAMES.len();
                    let _ = out.execute(SetForegroundColor(theme::PRIMARY));
                    let _ = out.execute(Print(format!("{} working...", SPINNER_FRAMES[idx])));
                    let _ = out.execute(ResetColor);
                    let _ = out.execute(SetAttribute(Attribute::Dim));
                    let _ = out.execute(Print(format!(" ({})", format_elapsed(elapsed))));
                    let _ = out.execute(SetAttribute(Attribute::Reset));
                }
            }
            Throbber::Done => {
                let _ = out.execute(SetAttribute(Attribute::Dim));
                let _ = out.execute(Print("done"));
                if let Some(d) = self.final_elapsed {
                    let _ = out.execute(Print(format!(" ({})", format_elapsed(d))));
                }
                let _ = out.execute(SetAttribute(Attribute::Reset));
            }
            Throbber::Interrupted => {
                let _ = out.execute(SetAttribute(Attribute::Dim));
                let _ = out.execute(Print("interrupted"));
                if let Some(d) = self.final_elapsed {
                    let _ = out.execute(Print(format!(" ({})", format_elapsed(d))));
                }
                let _ = out.execute(SetAttribute(Attribute::Reset));
            }
        }
        let _ = out.execute(Print("\r\n"));
        1
    }

}

/// Element types for spacing calculation. Throbber represents the
/// spinner/prompt area below all blocks.
enum Element<'a> {
    Block(&'a Block),
    Throbber,
}

/// Number of blank lines to insert between two adjacent elements.
/// Single source of truth for all vertical spacing.
fn gap_between(above: &Element, below: &Element) -> u16 {
    match (above, below) {
        // 1 blank line between user message and anything below
        (Element::Block(Block::User { .. }), _) => 1,
        // 1 blank line before a user message (new turn)
        (_, Element::Block(Block::User { .. })) => 1,
        // 1 blank line between tools
        (Element::Block(Block::ToolCall { .. }), Element::Block(Block::ToolCall { .. })) => 1,
        // 1 blank line between text and tool
        (Element::Block(Block::Text { .. }), Element::Block(Block::ToolCall { .. })) => 1,
        (Element::Block(Block::ToolCall { .. }), Element::Block(Block::Text { .. })) => 1,
        // 1 blank line between last block and throbber/prompt
        (Element::Block(_), Element::Throbber) => 1,
        _ => 0,
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

fn format_elapsed(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}s", secs)
    }
}

pub fn term_width() -> usize {
    terminal::size().map(|(w, _)| w as usize).unwrap_or(80)
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut truncated: String = s.chars().take(max.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}

fn render_block(block: &Block, _width: usize) -> u16 {
    match block {
        Block::User { text } => {
            let mut out = io::stdout();
            let lines: Vec<&str> = text.lines().collect();
            let max_len = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
            let pad_width = max_len + 2; // 1 space padding on each side
            for line in &lines {
                let char_len = line.chars().count();
                let trailing = pad_width - char_len - 1;
                let _ = out
                    .execute(SetBackgroundColor(theme::USER_BG))
                    .and_then(|o| o.execute(SetAttribute(Attribute::Bold)))
                    .and_then(|o| {
                        o.execute(Print(format!(" {}{}", line, " ".repeat(trailing))))
                    })
                    .and_then(|o| o.execute(SetAttribute(Attribute::Reset)))
                    .and_then(|o| o.execute(ResetColor));
                let _ = out.execute(Print("\r\n"));
            }
            lines.len() as u16
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
                    } // skip closing ```
                    rows += render_code_block(code_lines, lang);
                } else if lines[i].trim_start().starts_with('|') {
                    let table_start = i;
                    while i < lines.len() && lines[i].trim_start().starts_with('|') {
                        i += 1;
                    }
                    rows += render_markdown_table(&lines[table_start..i]);
                } else if lines[i].trim() == "---" {
                    let w = term_width();
                    let bar_len = w.saturating_sub(2);
                    let _ = out.execute(SetForegroundColor(theme::BAR));
                    let _ = out.execute(Print(format!(" {}\r\n", "\u{00B7}".repeat(bar_len))));
                    let _ = out.execute(ResetColor);
                    i += 1;
                    rows += 1;
                } else {
                    let _ = out.execute(Print(" "));
                    print_styled(lines[i]);
                    let _ = out.execute(Print("\r\n"));
                    i += 1;
                    rows += 1;
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
        } => {
            let color = match status {
                ToolStatus::Ok => theme::TOOL_OK,
                ToolStatus::Err | ToolStatus::Denied => theme::TOOL_ERR,
                ToolStatus::Confirm => theme::ACCENT,
                ToolStatus::Pending => theme::TOOL_PENDING,
            };
            let time = if name == "bash" && !matches!(status, ToolStatus::Pending | ToolStatus::Confirm) {
                *elapsed
            } else {
                None
            };
            let tl = tool_timeout_label(args);
            print_tool_line(name, summary, color, time, tl.as_deref());
            let mut rows = 1u16;

            if *status != ToolStatus::Denied {
                if let Some(ref out_data) = output {
                    rows += print_tool_output(name, &out_data.content, out_data.is_error, args);
                }
            }
            rows
        }
        Block::Confirm { tool, desc, choice } => {
            render_confirm_result(tool, desc, *choice)
        }
        Block::Error { message } => {
            print_error(message);
            1
        }
    }
}

fn render_confirm_result(tool: &str, desc: &str, choice: Option<ConfirmChoice>) -> u16 {
    let mut out = io::stdout();
    let mut rows = 2u16; // allow line + desc line

    let _ = out
        .execute(SetForegroundColor(theme::APPLY))
        .and_then(|o| o.execute(Print("   allow? ")))
        .and_then(|o| o.execute(ResetColor))
        .and_then(|o| o.execute(SetAttribute(Attribute::Dim)))
        .and_then(|o| o.execute(Print(tool)))
        .and_then(|o| o.execute(SetAttribute(Attribute::Reset)))
        .and_then(|o| o.execute(Print("\r\n")));

    let _ = out
        .execute(SetAttribute(Attribute::Dim))
        .and_then(|o| o.execute(Print("   \u{2502} ")))
        .and_then(|o| o.execute(SetAttribute(Attribute::Reset)))
        .and_then(|o| o.execute(Print(desc)))
        .and_then(|o| o.execute(Print("\r\n")));

    if let Some(c) = choice {
        rows += 1;
        let _ = out.execute(Print("   "));
        match c {
            ConfirmChoice::Yes => {
                let _ = out
                    .execute(SetAttribute(Attribute::Dim))
                    .and_then(|o| o.execute(Print("approved\r\n")))
                    .and_then(|o| o.execute(SetAttribute(Attribute::Reset)));
            }
            ConfirmChoice::Always => {
                let _ = out
                    .execute(SetAttribute(Attribute::Dim))
                    .and_then(|o| o.execute(Print("always\r\n")))
                    .and_then(|o| o.execute(SetAttribute(Attribute::Reset)));
            }
            ConfirmChoice::No => {
                let _ = out
                    .execute(SetForegroundColor(theme::TOOL_ERR))
                    .and_then(|o| o.execute(Print("denied\r\n")))
                    .and_then(|o| o.execute(ResetColor));
            }
        }
    }
    rows
}

fn print_styled(text: &str) {
    let mut out = io::stdout();

    // Markdown headings: lines starting with #
    let trimmed = text.trim_start();
    if trimmed.starts_with('#') {
        let _ = out.execute(SetForegroundColor(theme::HEADING));
        let _ = out.execute(SetAttribute(Attribute::Bold));
        let _ = out.execute(Print(trimmed));
        let _ = out.execute(SetAttribute(Attribute::Reset));
        let _ = out.execute(ResetColor);
        return;
    }

    // Quote blocks: lines starting with >
    if trimmed.starts_with('>') {
        let content = trimmed.strip_prefix('>').unwrap().trim_start();
        let _ = out.execute(SetAttribute(Attribute::Dim));
        let _ = out.execute(SetAttribute(Attribute::Italic));
        let _ = out.execute(Print(content));
        let _ = out.execute(SetAttribute(Attribute::Reset));
        return;
    }

    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut i = 0;
    let mut plain = String::new();

    while i < len {
        // **bold** — rendered bold + dim
        if i + 1 < len && chars[i] == '*' && chars[i + 1] == '*' {
            if !plain.is_empty() {
                let _ = out.execute(Print(&plain));
                plain.clear();
            }
            i += 2;
            let start = i;
            while i + 1 < len && !(chars[i] == '*' && chars[i + 1] == '*') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let _ = out.execute(SetAttribute(Attribute::Bold));
            let _ = out.execute(SetAttribute(Attribute::Dim));
            let _ = out.execute(Print(&word));
            let _ = out.execute(SetAttribute(Attribute::Reset));
            if i + 1 < len {
                i += 2;
            }
            continue;
        }

        // *italic* — rendered italic + dim
        if chars[i] == '*' && i + 1 < len && chars[i + 1] != '*' {
            if !plain.is_empty() {
                let _ = out.execute(Print(&plain));
                plain.clear();
            }
            i += 1;
            let start = i;
            while i < len && chars[i] != '*' {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let _ = out.execute(SetAttribute(Attribute::Italic));
            let _ = out.execute(SetAttribute(Attribute::Dim));
            let _ = out.execute(Print(&word));
            let _ = out.execute(SetAttribute(Attribute::Reset));
            if i < len {
                i += 1;
            }
            continue;
        }

        if chars[i] == '`' {
            if !plain.is_empty() {
                let _ = out.execute(Print(&plain));
                plain.clear();
            }
            i += 1;
            let start = i;
            while i < len && chars[i] != '`' {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let _ = out.execute(SetForegroundColor(theme::ACCENT));
            let _ = out.execute(Print(&word));
            let _ = out.execute(ResetColor);
            if i < len {
                i += 1;
            }
            continue;
        }

        plain.push(chars[i]);
        i += 1;
    }
    if !plain.is_empty() {
        let _ = out.execute(Print(&plain));
    }
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
    let _ = out.execute(Print(" "));
    let _ = out.execute(SetForegroundColor(pill_color));
    let _ = out.execute(Print("\u{23fa}"));
    let _ = out.execute(ResetColor);
    // Truncate summary to fit on one line: " ⏺ name summary  1.2s  (timeout: 30s)"
    let time_str = elapsed
        .filter(|d| d.as_secs_f64() >= 0.1)
        .map(|d| format!("  {:.1}s", d.as_secs_f64()))
        .unwrap_or_default();
    let timeout_str = timeout_label
        .map(|l| format!("  ({})", l))
        .unwrap_or_default();
    let suffix_len = time_str.len() + timeout_str.len();
    let prefix_len = 3 + name.len() + 1; // " ⏺ " + name + " "
    let max_summary = width.saturating_sub(prefix_len + suffix_len + 1);
    let truncated = truncate_str(summary, max_summary);
    let _ = out.execute(SetAttribute(Attribute::Dim));
    let _ = out.execute(Print(format!(" {}", name)));
    let _ = out.execute(SetAttribute(Attribute::Reset));
    let _ = out.execute(Print(format!(" {}", truncated)));
    if !time_str.is_empty() {
        let _ = out.execute(SetAttribute(Attribute::Dim));
        let _ = out.execute(Print(&time_str));
        let _ = out.execute(SetAttribute(Attribute::Reset));
    }
    if !timeout_str.is_empty() {
        let _ = out.execute(SetAttribute(Attribute::Dim));
        let _ = out.execute(Print(&timeout_str));
        let _ = out.execute(SetAttribute(Attribute::Reset));
    }
    let _ = out.execute(Print("\r\n"));
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
                .execute(SetAttribute(Attribute::Dim))
                .and_then(|o| o.execute(Print(format!("   {} lines\r\n", line_count))))
                .and_then(|o| o.execute(SetAttribute(Attribute::Reset)));
            1
        }
        "edit_file" if !is_error => {
            let old = args.get("old_string").and_then(|v| v.as_str()).unwrap_or("");
            let new = args.get("new_string").and_then(|v| v.as_str()).unwrap_or("");
            let path = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
            print_syntax_diff(old, new, path)
        }
        "write_file" if !is_error => {
            let file_content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let path = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
            print_syntax_file(file_content, path)
        }
        "bash" if content.is_empty() => 0,
        "bash" => {
            let count = content.lines().count();
            for line in content.lines() {
                if is_error {
                    let _ = out.execute(SetForegroundColor(theme::TOOL_ERR));
                    let _ = out.execute(Print(format!("   {}\r\n", line)));
                    let _ = out.execute(ResetColor);
                } else {
                    let _ = out.execute(SetAttribute(Attribute::Dim));
                    let _ = out.execute(Print(format!("   {}\r\n", line)));
                    let _ = out.execute(SetAttribute(Attribute::Reset));
                }
            }
            count as u16
        }
        _ => {
            let preview = result_preview(content, 3);
            if is_error {
                let _ = out
                    .execute(SetForegroundColor(theme::TOOL_ERR))
                    .and_then(|o| o.execute(Print(format!("   {}\r\n", preview))))
                    .and_then(|o| o.execute(ResetColor));
            } else {
                let _ = out
                    .execute(SetAttribute(Attribute::Dim))
                    .and_then(|o| o.execute(Print(format!("   {}\r\n", preview))))
                    .and_then(|o| o.execute(SetAttribute(Attribute::Reset)));
            }
            preview.lines().count() as u16
        }
    }
}

fn print_syntax_diff(old: &str, new: &str, path: &str) -> u16 {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("txt");
    let syntax = SYNTAX_SET
        .find_syntax_by_extension(ext)
        .unwrap_or_else(|| SYNTAX_SET.find_syntax_plain_text());
    let theme = &THEME_SET[two_face::theme::EmbeddedThemeName::MonokaiExtended];

    let indent = "   "; // align with tool output level

    // The file on disk already contains new_text (edit already applied).
    // Find the position of new_text to determine line numbers, then use
    // the surrounding (unchanged) lines as context.
    let file_content = std::fs::read_to_string(path).unwrap_or_default();
    let file_lines: Vec<&str> = file_content.lines().collect();
    let start_line = file_content
        .find(new)
        .map(|pos| file_content[..pos].lines().count())
        .unwrap_or(0);

    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();

    // Context: 3 lines before and after
    let ctx = 3;
    let ctx_start = start_line.saturating_sub(ctx);
    let ctx_end = (start_line + new_lines.len() + ctx).min(file_lines.len());
    let max_lineno = ctx_end;
    let gutter_width = format!("{}", max_lineno).len().max(2);
    // Layout: indent + " " + lineno + " " + sign + " " + code
    // Context: indent + " " + lineno + "   " + code  (sign replaced by space)
    // Diff:    indent + " " + lineno + " " + "-" + " " + code
    let prefix_len = indent.len() + 1 + gutter_width + 3; // " " + gutter + " - "
    let max_content = term_width().saturating_sub(prefix_len + 1);

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

    let layout = DiffLayout { indent, gutter_width, max_content };

    let mut h = HighlightLines::new(syntax, theme);
    // Prime highlighter up to context start
    for i in 0..ctx_start {
        if i < file_lines.len() {
            let _ = h.highlight_line(&format!("{}\n", file_lines[i]), &SYNTAX_SET);
        }
    }

    // Context before
    let mut rows = print_diff_lines(&mut h, &file_lines[ctx_start..start_line], ctx_start, None, None, &layout);
    // Deleted lines
    rows += print_diff_lines(&mut h, &old_lines, start_line, Some(('-', Color::Red)), Some(bg_del), &layout);
    // Re-highlight from start_line for added lines + context after
    let mut h2 = HighlightLines::new(syntax, theme);
    for i in 0..start_line {
        if i < file_lines.len() {
            let _ = h2.highlight_line(&format!("{}\n", file_lines[i]), &SYNTAX_SET);
        }
    }
    // Added lines
    rows += print_diff_lines(&mut h2, &new_lines, start_line, Some(('+', Color::Green)), Some(bg_add), &layout);
    // Context after
    let after_start = start_line + new_lines.len();
    let after_end = (after_start + ctx).min(file_lines.len());
    rows += print_diff_lines(&mut h2, &file_lines[after_start..after_end], after_start, None, None, &layout);
    rows
}

struct DiffLayout {
    indent: &'static str,
    gutter_width: usize,
    max_content: usize,
}

fn print_diff_lines(
    h: &mut HighlightLines,
    lines: &[&str],
    start_line: usize,
    sign: Option<(char, Color)>,
    bg: Option<Color>,
    layout: &DiffLayout,
) -> u16 {
    let DiffLayout { indent, gutter_width, max_content } = *layout;
    let mut out = io::stdout();
    for (i, line) in lines.iter().enumerate() {
        let lineno = start_line + i + 1;
        let line_with_nl = format!("{}\n", line);
        let regions = h
            .highlight_line(&line_with_nl, &SYNTAX_SET)
            .unwrap_or_default();
        let _ = out.execute(Print(indent));
        if let Some((ch, color)) = sign {
            let _ = out.execute(SetBackgroundColor(bg.unwrap()));
            let _ = out.execute(SetForegroundColor(Color::DarkGrey));
            let _ = out.execute(Print(format!(" {:>w$} ", lineno, w = gutter_width)));
            let _ = out.execute(SetForegroundColor(color));
            let _ = out.execute(Print(format!("{} ", ch)));
            print_syntect_regions(&regions, max_content, bg);
            let used = indent.len() + 1 + gutter_width + 3 + visible_len(&regions);
            let pad = term_width().saturating_sub(used);
            if pad > 0 {
                if let Some(bg_color) = bg {
                    let _ = out.execute(SetBackgroundColor(bg_color));
                }
                let _ = out.execute(Print(" ".repeat(pad)));
            }
            let _ = out.execute(ResetColor);
        } else {
            let _ = out.execute(SetForegroundColor(Color::DarkGrey));
            let _ = out.execute(Print(format!(" {:>w$}", lineno, w = gutter_width)));
            let _ = out.execute(ResetColor);
            let _ = out.execute(Print("   "));
            print_syntect_regions(&regions, max_content, None);
        }
        let _ = out.execute(Print("\r\n"));
    }
    lines.len() as u16
}

fn print_syntax_file(content: &str, path: &str) -> u16 {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("txt");
    let syntax = SYNTAX_SET
        .find_syntax_by_extension(ext)
        .unwrap_or_else(|| SYNTAX_SET.find_syntax_plain_text());
    let lines: Vec<&str> = content.lines().collect();
    render_highlighted(&lines, syntax)
}

fn visible_len(regions: &[(Style, &str)]) -> usize {
    regions
        .iter()
        .map(|(_, t)| t.trim_end_matches('\n').trim_end_matches('\r').len())
        .sum()
}

fn print_syntect_regions(regions: &[(Style, &str)], max_width: usize, bg: Option<Color>) {
    let mut out = io::stdout();
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
        let display = truncate_str(text, remaining);
        if let Some(bg_color) = bg {
            let _ = out.execute(SetBackgroundColor(bg_color));
        }
        let fg = Color::Rgb {
            r: style.foreground.r,
            g: style.foreground.g,
            b: style.foreground.b,
        };
        let _ = out.execute(SetForegroundColor(fg));
        let _ = out.execute(Print(&display));
        col += display.len();
    }
    let _ = out.execute(ResetColor);
}

fn render_code_block(lines: &[&str], lang: &str) -> u16 {
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
    render_highlighted(lines, syntax)
}

fn render_highlighted(lines: &[&str], syntax: &syntect::parsing::SyntaxReference) -> u16 {
    let mut out = io::stdout();
    let indent = "   ";
    let theme = &THEME_SET[two_face::theme::EmbeddedThemeName::MonokaiExtended];
    let gutter_width = format!("{}", lines.len()).len().max(2);
    let prefix_len = indent.len() + 1 + gutter_width + 3;
    let max_content = term_width().saturating_sub(prefix_len + 1);

    let mut h = HighlightLines::new(syntax, theme);
    for (i, line) in lines.iter().enumerate() {
        let line_with_nl = format!("{}\n", line);
        let regions = h
            .highlight_line(&line_with_nl, &SYNTAX_SET)
            .unwrap_or_default();
        let _ = out.execute(Print(indent));
        let _ = out.execute(SetForegroundColor(Color::DarkGrey));
        let _ = out.execute(Print(format!(" {:>w$}", i + 1, w = gutter_width)));
        let _ = out.execute(ResetColor);
        let _ = out.execute(Print("   "));
        print_syntect_regions(&regions, max_content, None);
        let _ = out.execute(Print("\r\n"));
    }
    lines.len() as u16
}

fn render_markdown_table(lines: &[&str]) -> u16 {
    let mut out = io::stdout();

    // Parse markdown table: skip separator rows (containing ---)
    let mut rows: Vec<Vec<String>> = Vec::new();
    for line in lines {
        let trimmed = line.trim().trim_start_matches('|').trim_end_matches('|');
        // Skip separator rows like |---|---|
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

    // First row is header
    if let Some(header) = rows.first() {
        table.set_header(header);
    }
    for row in rows.iter().skip(1) {
        table.add_row(row);
    }

    let rendered = table.to_string();
    for line in rendered.lines() {
        let _ = out.execute(Print(" "));
        // Render border chars gray, content normal
        let mut in_border = false;
        for ch in line.chars() {
            let is_border =
                ('\u{2500}'..='\u{257F}').contains(&ch) || ('\u{2580}'..='\u{259F}').contains(&ch);
            if is_border && !in_border {
                let _ = out.execute(SetForegroundColor(theme::BAR));
                in_border = true;
            } else if !is_border && in_border {
                let _ = out.execute(ResetColor);
                in_border = false;
            }
            let _ = out.execute(Print(ch.to_string()));
        }
        if in_border {
            let _ = out.execute(ResetColor);
        }
        let _ = out.execute(Print("\r\n"));
    }
    rendered.lines().count() as u16
}

fn print_error(msg: &str) {
    let mut out = io::stdout();
    let _ = out
        .execute(SetForegroundColor(theme::TOOL_ERR))
        .and_then(|o| o.execute(Print(format!(" error: {}\r\n", msg))))
        .and_then(|o| o.execute(ResetColor));
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

pub fn tool_arg_summary(name: &str, args: &HashMap<String, serde_json::Value>) -> String {
    let base = match name {
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
        _ => String::new(),
    };
    base
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

/// Word-wrap text and locate cursor position in visual-line space.
/// Returns (visual_lines, cursor_line, cursor_col).
fn wrap_and_locate_cursor(buf: &str, cursor_char: usize, usable: usize) -> (Vec<String>, usize, usize) {
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
        chars_seen += 1; // account for the '\n'
    }
    if visual_lines.is_empty() {
        visual_lines.push(String::new());
    }
    (visual_lines, cursor_line, cursor_col)
}

/// Draw a horizontal bar, optionally with a right-aligned label.
/// `bar_color` controls the color of the `─` line segments.
fn draw_bar(width: usize, label: Option<(&str, Color)>, bar_color: Color) {
    let mut out = io::stdout();
    if let Some((text, color)) = label {
        let tail = format!(" {} \u{2500}", text);
        let bar_len = width.saturating_sub(tail.chars().count());
        let _ = out.execute(SetForegroundColor(bar_color));
        let _ = out.execute(Print("\u{2500}".repeat(bar_len)));
        let _ = out.execute(ResetColor);
        let _ = out.execute(SetForegroundColor(color));
        let _ = out.execute(Print(format!(" {} ", text)));
        let _ = out.execute(ResetColor);
        let _ = out.execute(SetForegroundColor(bar_color));
        let _ = out.execute(Print("\u{2500}"));
        let _ = out.execute(ResetColor);
    } else {
        let _ = out.execute(SetForegroundColor(bar_color));
        let _ = out.execute(Print("\u{2500}".repeat(width)));
        let _ = out.execute(ResetColor);
    }
}

// ── Structured spans for prompt content ──────────────────────────────────────

enum Span {
    Plain(String),
    Paste(String),  // "[pasted N lines]"
    AtRef(String),  // "@path"
}

/// Build display spans from the raw buffer + paste storage.
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
            let lines = pastes.get(paste_idx).map(|p| p.lines().count().max(1)).unwrap_or(1);
            spans.push(Span::Paste(format!("[pasted {} lines]", lines)));
            paste_idx += 1;
            i += 1;
        } else if chars[i] == '@' {
            // Check: preceded by whitespace or start of string
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
                    // Bare '@'
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

/// Map a cursor position from raw-buffer char space to display-string char space.
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
                // A paste marker is 1 char in the raw buffer
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
    // Cursor at end
    let _ = raw_buf;
    display_pos
}

/// Render a visual line with span-aware highlighting.
/// We re-parse the wrapped line to find @ tokens and paste labels.
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
            let _ = out.execute(Print(rest));
            break;
        };

        if pos > 0 { let _ = out.execute(Print(&rest[..pos])); }

        if paste_pos == Some(pos) {
            if let Some(end) = rest[pos..].find(']') {
                let label = &rest[pos..pos + end + 1];
                if label.ends_with(" lines]") {
                    let _ = out.execute(SetForegroundColor(theme::ACCENT));
                    let _ = out.execute(SetAttribute(Attribute::Dim));
                    let _ = out.execute(Print(label));
                    let _ = out.execute(SetAttribute(Attribute::Reset));
                    let _ = out.execute(ResetColor);
                    rest = &rest[pos + end + 1..];
                    continue;
                }
            }
            let _ = out.execute(Print(&rest[pos..pos + 1]));
            rest = &rest[pos + 1..];
        } else if at_pos == Some(pos) {
            let after = &rest[pos + 1..];
            let tok_end = after.find(char::is_whitespace).unwrap_or(after.len());
            let token = &rest[pos..pos + 1 + tok_end];
            let _ = out.execute(SetForegroundColor(theme::ACCENT));
            let _ = out.execute(Print(token));
            let _ = out.execute(ResetColor);
            rest = &rest[pos + 1 + tok_end..];
        } else {
            let _ = out.execute(Print(&rest[pos..pos + 1]));
            rest = &rest[pos + 1..];
        }
    }
}

fn draw_completions(completer: Option<&crate::completer::Completer>) -> usize {
    let Some(comp) = completer else { return 0 };
    if comp.results.is_empty() { return 0; }
    let mut out = io::stdout();
    let last = comp.results.len() - 1;
    let prefix = match comp.kind {
        crate::completer::CompleterKind::Command => "/",
        crate::completer::CompleterKind::File => "./",
    };
    // Find the longest label to align descriptions.
    let max_label = comp.results.iter().map(|i| prefix.len() + i.label.len()).max().unwrap_or(0);
    for (i, item) in comp.results.iter().enumerate() {
        let _ = out.execute(Print("  "));
        let label = format!("{}{}", prefix, item.label);
        if i == comp.selected {
            let _ = out.execute(SetForegroundColor(theme::ACCENT));
            let _ = out.execute(Print(&label));
            if let Some(ref desc) = item.description {
                let pad = max_label - label.len() + 2;
                let _ = out.execute(ResetColor);
                let _ = out.execute(SetAttribute(Attribute::Dim));
                let _ = out.execute(Print(format!("{:>width$}{}", "", desc, width = pad)));
                let _ = out.execute(SetAttribute(Attribute::Reset));
            }
            let _ = out.execute(ResetColor);
        } else {
            let _ = out.execute(SetAttribute(Attribute::Dim));
            let _ = out.execute(Print(&label));
            if let Some(ref desc) = item.description {
                let pad = max_label - label.len() + 2;
                let _ = out.execute(Print(format!("{:>width$}{}", "", desc, width = pad)));
            }
            let _ = out.execute(SetAttribute(Attribute::Reset));
        }
        if i < last {
            let _ = out.execute(Print("\r\n"));
        }
    }
    comp.results.len()
}

pub fn erase_prompt_at(top_row: u16) {
    let mut out = io::stdout();
    let _ = out.execute(cursor::MoveTo(0, top_row));
    let _ = out.execute(terminal::Clear(terminal::ClearType::FromCursorDown));
    let _ = out.flush();
}

/// Show confirm prompt as a bottom bar overlay. Returns the user's choice.
pub fn show_confirm(tool_name: &str, desc: &str) -> ConfirmChoice {
    let mut out = io::stdout();
    let (width, height) = terminal::size().unwrap_or((80, 24));
    let w = width as usize;
    let bar_row = height.saturating_sub(2);

    // Save cursor position
    let _ = out.flush();
    let saved_pos = cursor::position().unwrap_or((0, 0));

    // Draw bar + content at absolute bottom
    let _ = out.execute(cursor::MoveTo(0, bar_row));
    let _ = out.execute(terminal::Clear(terminal::ClearType::FromCursorDown));
    draw_bar(w, Some(("allow? (y)es (n)o (a)lways", theme::ACCENT)), theme::ACCENT);
    let _ = out.execute(Print("\r\n"));

    // Content line: " tool_name: desc"
    let _ = out.execute(Print(" "));
    let _ = out.execute(SetForegroundColor(theme::ACCENT));
    let _ = out.execute(Print(tool_name));
    let _ = out.execute(ResetColor);
    let _ = out.execute(SetAttribute(Attribute::Dim));
    let _ = out.execute(Print(format!(": {}", desc)));
    let _ = out.execute(SetAttribute(Attribute::Reset));

    let _ = out.execute(cursor::Show);
    let _ = out.flush();

    use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};

    let choice = loop {
        if let Ok(Event::Key(KeyEvent { code, modifiers, .. })) = event::read() {
            match (code, modifiers) {
                (KeyCode::Enter, _) | (KeyCode::Char('y' | 'Y'), _) => {
                    break ConfirmChoice::Yes;
                }
                (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                    break ConfirmChoice::No;
                }
                (KeyCode::Esc, _) | (KeyCode::Char('n' | 'N'), _) => {
                    break ConfirmChoice::No;
                }
                (KeyCode::Char('a' | 'A'), _) => {
                    break ConfirmChoice::Always;
                }
                _ => {}
            }
        }
    };

    // Erase the overlay
    let _ = out.execute(cursor::Hide);
    let _ = out.execute(cursor::MoveTo(0, bar_row));
    let _ = out.execute(terminal::Clear(terminal::ClearType::FromCursorDown));
    let _ = out.execute(cursor::MoveTo(saved_pos.0, saved_pos.1));
    let _ = out.flush();

    choice
}
