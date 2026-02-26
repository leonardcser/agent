use crate::completer::FileCompleter;
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
    /// Blocks rendered to terminal and stable (next index to render).
    flushed: usize,
    /// Rows occupied by the last rendered block (for erase on mutation).
    last_block_rows: u16,
    /// A previously-rendered block was mutated and needs re-rendering.
    rerender: bool,
    /// Whether a prompt is currently drawn on screen.
    prompt_drawn: bool,
    prompt_top_row: u16,
    working_since: Option<Instant>,
    /// Snapshot of elapsed time when the throbber leaves Working state.
    final_elapsed: Option<Duration>,
    /// Latest context token count from the LLM.
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

    /// Mark the start of a new turn. Previous blocks are kept for resize redraw.
    pub fn begin_turn(&mut self) {
        self.last_block_rows = 0;
        self.rerender = false;
    }

    /// Add a block. Rendering is deferred until render().
    pub fn push(&mut self, block: Block) {
        self.blocks.push(block);
    }

    /// Update the most recent ToolCall block's status, elapsed, and output.
    pub fn update_last_tool(
        &mut self,
        status: ToolStatus,
        output: Option<ToolOutput>,
        elapsed: Option<Duration>,
    ) {
        let tool_idx = self
            .blocks
            .iter()
            .rposition(|b| matches!(b, Block::ToolCall { .. }));
        if let Some(idx) = tool_idx {
            if let Block::ToolCall {
                status: ref mut s,
                elapsed: ref mut e,
                output: ref mut o,
                ..
            } = self.blocks[idx]
            {
                *s = status;
                *e = elapsed;
                *o = output;
            }
            // Back up flushed so this block gets re-rendered.
            if idx < self.flushed {
                self.flushed = idx;
                self.rerender = true;
            }
        }
    }

    pub fn append_tool_output(&mut self, chunk: &str) {
        let tool_idx = self
            .blocks
            .iter()
            .rposition(|b| matches!(b, Block::ToolCall { .. }));
        if let Some(idx) = tool_idx {
            if let Block::ToolCall { output: ref mut o, .. } = self.blocks[idx] {
                match o {
                    Some(ref mut out) => {
                        if !out.content.is_empty() {
                            out.content.push('\n');
                        }
                        out.content.push_str(chunk);
                    }
                    None => {
                        *o = Some(ToolOutput {
                            content: chunk.to_string(),
                            is_error: false,
                        });
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
        let tool_idx = self
            .blocks
            .iter()
            .rposition(|b| matches!(b, Block::ToolCall { .. }));
        if let Some(idx) = tool_idx {
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

    /// Render blocks incrementally. New blocks are appended without erasing.
    /// Mutated blocks (via update_last_tool) erase only the affected tail.
    fn render_blocks(&mut self) {
        let has_new = self.flushed < self.blocks.len();
        if !has_new && !self.rerender {
            // Nothing changed — just erase prompt for redraw.
            if self.prompt_drawn {
                erase_prompt_at(self.prompt_top_row);
            }
            self.prompt_drawn = false;
            return;
        }

        let mut out = io::stdout();

        if self.rerender {
            // A rendered block was mutated — erase from its start.
            let erase_from = if self.prompt_drawn {
                self.prompt_top_row.saturating_sub(self.last_block_rows)
            } else {
                let _ = out.flush();
                cursor::position()
                    .map(|(_, y)| y)
                    .unwrap_or(0)
                    .saturating_sub(self.last_block_rows)
            };
            let _ = out.execute(cursor::MoveTo(0, erase_from));
            let _ = out.execute(terminal::Clear(terminal::ClearType::FromCursorDown));
            let _ = out.flush();
            self.rerender = false;
        } else if self.prompt_drawn {
            // Append mode — just erase prompt to make room for new blocks.
            erase_prompt_at(self.prompt_top_row);
        } else {
            // First render of this turn — clear from cursor down.
            let _ = out.flush();
            let pos = cursor::position().map(|(_, y)| y).unwrap_or(0);
            let _ = out.execute(cursor::MoveTo(0, pos));
            let _ = out.execute(terminal::Clear(terminal::ClearType::FromCursorDown));
            let _ = out.flush();
        }

        // Render blocks from flushed onwards. Track the last block's row count
        // so we can erase just that block if it gets mutated later.
        let w = term_width();
        let render_end = self.blocks.len();
        let last_idx = render_end.saturating_sub(1);

        for i in self.flushed..render_end {
            let gap = if i > 0 {
                gap_between(&Element::Block(&self.blocks[i - 1]), &Element::Block(&self.blocks[i]))
            } else {
                0
            };
            for _ in 0..gap {
                let _ = out.execute(Print("\r\n"));
            }
            let rows = render_block(&self.blocks[i], w);
            if i == last_idx {
                self.last_block_rows = rows + gap as u16;
            }
        }

        self.flushed = self.blocks.len();
        self.prompt_drawn = false;
    }

    /// Single render entry point. Re-renders blocks if dirty, then always redraws the prompt.
    pub fn render(
        &mut self,
        buf: &str,
        cursor_char: usize,
        mode: super::input::Mode,
        width: usize,
        queued: &[String],
        pastes: &[String],
    ) {
        self.render_with_completer(buf, cursor_char, mode, width, queued, pastes, None);
    }

    pub fn render_with_completer(
        &mut self,
        buf: &str,
        cursor_char: usize,
        mode: super::input::Mode,
        width: usize,
        queued: &[String],
        pastes: &[String],
        completer: Option<&FileCompleter>,
    ) {
        self.render_blocks();

        let throbber_info = self.throbber.map(|t| (t, self.working_since, self.final_elapsed));

        let gap = self.blocks.last().map_or(0, |last| {
            gap_between(&Element::Block(last), &Element::Throbber)
        });
        for _ in 0..gap {
            let _ = io::stdout().execute(Print("\r\n"));
        }
        self.prompt_top_row =
            draw_prompt_box(buf, cursor_char, mode, width, queued, throbber_info, self.context_tokens, pastes, completer);
        if gap > 0 {
            self.prompt_top_row = self.prompt_top_row.saturating_sub(gap as u16);
        }
        self.prompt_drawn = true;
    }

    /// Flush dirty blocks to the terminal without drawing a prompt.
    pub fn flush_blocks(&mut self) {
        self.render_blocks();
    }

    pub fn erase_prompt(&mut self) {
        if self.prompt_drawn {
            erase_prompt_at(self.prompt_top_row);
            self.prompt_drawn = false;
        }
    }

    /// Terminal resized — clear everything and re-render all blocks.
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
fn draw_bar(width: usize, label: Option<(&str, Color)>) {
    let mut out = io::stdout();
    if let Some((text, color)) = label {
        let tail = format!(" {} \u{2500}", text);
        let bar_len = width.saturating_sub(tail.chars().count());
        let _ = out.execute(SetForegroundColor(theme::BAR));
        let _ = out.execute(Print("\u{2500}".repeat(bar_len)));
        let _ = out.execute(ResetColor);
        let _ = out.execute(SetForegroundColor(color));
        let _ = out.execute(Print(format!(" {} ", text)));
        let _ = out.execute(ResetColor);
        let _ = out.execute(SetForegroundColor(theme::BAR));
        let _ = out.execute(Print("\u{2500}"));
        let _ = out.execute(ResetColor);
    } else {
        let _ = out.execute(SetForegroundColor(theme::BAR));
        let _ = out.execute(Print("\u{2500}".repeat(width)));
        let _ = out.execute(ResetColor);
    }
}

/// Build a display string where each PASTE_MARKER is replaced with a label.
/// Returns (display_string, display_cursor_char).
fn paste_display(buf: &str, cursor_char: usize, pastes: &[String]) -> (String, usize) {
    use super::input::PASTE_MARKER;
    let mut display = String::new();
    let mut display_cursor = 0;
    let mut paste_idx = 0;
    for (i, c) in buf.chars().enumerate() {
        if i == cursor_char {
            display_cursor = display.chars().count();
        }
        if c == PASTE_MARKER {
            let lines = pastes.get(paste_idx).map(|p| p.lines().count().max(1)).unwrap_or(1);
            let label = format!("[pasted {} lines]", lines);
            display.push_str(&label);
            paste_idx += 1;
        } else {
            display.push(c);
        }
    }
    if cursor_char >= buf.chars().count() {
        display_cursor = display.chars().count();
    }
    (display, display_cursor)
}

/// Print a line, highlighting `[pasted N lines]` labels and `@path` tokens.
fn print_styled_line(line: &str, has_pastes: bool) {
    let mut out = io::stdout();
    let mut rest = line;
    while !rest.is_empty() {
        // Find the next special token
        let paste_pos = if has_pastes { rest.find("[pasted ") } else { None };
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

        // Print text before the token
        if pos > 0 {
            let _ = out.execute(Print(&rest[..pos]));
        }

        // Check what we found
        if has_pastes && paste_pos == Some(pos) {
            if let Some(rel_end) = rest[pos..].find(']') {
                let label = &rest[pos..pos + rel_end + 1];
                if label.ends_with(" lines]") {
                    let _ = out.execute(SetForegroundColor(theme::ACCENT));
                    let _ = out.execute(SetAttribute(Attribute::Dim));
                    let _ = out.execute(Print(label));
                    let _ = out.execute(SetAttribute(Attribute::Reset));
                    let _ = out.execute(ResetColor);
                    rest = &rest[pos + rel_end + 1..];
                    continue;
                }
            }
            let _ = out.execute(Print(&rest[pos..pos + 1]));
            rest = &rest[pos + 1..];
        } else if at_pos == Some(pos) {
            // @ token: find the end (next whitespace or end of string)
            let after_at = &rest[pos + 1..];
            let token_end = after_at.find(char::is_whitespace).unwrap_or(after_at.len());
            if token_end > 0 {
                let token = &rest[pos..pos + 1 + token_end];
                let _ = out.execute(SetForegroundColor(theme::ACCENT));
                let _ = out.execute(Print(token));
                let _ = out.execute(ResetColor);
                rest = &rest[pos + 1 + token_end..];
            } else {
                // Bare '@' with nothing after it
                let _ = out.execute(SetForegroundColor(theme::ACCENT));
                let _ = out.execute(Print("@"));
                let _ = out.execute(ResetColor);
                rest = &rest[pos + 1..];
            }
        } else {
            let _ = out.execute(Print(&rest[pos..pos + 1]));
            rest = &rest[pos + 1..];
        }
    }
}

pub fn draw_prompt_box(
    buf: &str,
    cursor_char: usize,
    mode: super::input::Mode,
    width: usize,
    queued: &[String],
    throbber: Option<(Throbber, Option<Instant>, Option<Duration>)>,
    context_tokens: Option<u32>,
    pastes: &[String],
    completer: Option<&FileCompleter>,
) -> u16 {
    let mut out = io::stdout();
    let usable = width.saturating_sub(1);
    let has_pastes = !pastes.is_empty();

    // Throbber line: directly above the prompt, no gap between them.
    let throbber_count: usize = if throbber.is_some() { 1 } else { 0 };
    if let Some((state, working_since, final_elapsed)) = throbber {
        let _ = out.execute(Print(" "));
        match state {
            Throbber::Working => {
                if let Some(start) = working_since {
                    let elapsed = start.elapsed();
                    let frame_idx = (elapsed.as_millis() / 150) as usize % SPINNER_FRAMES.len();
                    let spinner = SPINNER_FRAMES[frame_idx];
                    let time_str = format_elapsed(elapsed);

                    let _ = out.execute(SetForegroundColor(theme::PRIMARY));
                    let _ = out.execute(Print(format!("{} working...", spinner)));
                    let _ = out.execute(ResetColor);
                    let _ = out.execute(SetAttribute(Attribute::Dim));
                    let _ = out.execute(Print(format!(" ({})", time_str)));
                    let _ = out.execute(SetAttribute(Attribute::Reset));
                }
            }
            Throbber::Done => {
                let _ = out.execute(SetAttribute(Attribute::Dim));
                let _ = out.execute(Print("done"));
                if let Some(d) = final_elapsed {
                    let _ = out.execute(Print(format!(" ({})", format_elapsed(d))));
                }
                let _ = out.execute(SetAttribute(Attribute::Reset));
            }
            Throbber::Interrupted => {
                let _ = out.execute(SetAttribute(Attribute::Dim));
                let _ = out.execute(Print("interrupted"));
                if let Some(d) = final_elapsed {
                    let _ = out.execute(Print(format!(" ({})", format_elapsed(d))));
                }
                let _ = out.execute(SetAttribute(Attribute::Reset));
            }
        }
        let _ = out.execute(Print("\r\n"));
    }

    // Queued messages above the prompt
    let queued_count = queued.len();
    for msg in queued {
        let display: String = msg.chars().take(usable).collect();
        let _ = out.execute(SetBackgroundColor(theme::USER_BG));
        let _ = out.execute(SetAttribute(Attribute::Bold));
        let _ = out.execute(Print(format!(" {} ", display)));
        let _ = out.execute(SetAttribute(Attribute::Reset));
        let _ = out.execute(ResetColor);
        let _ = out.execute(Print("\r\n"));
    }

    // Top bar with token count in the right
    let tokens_label = context_tokens.map(format_tokens);
    if let Some(ref label) = tokens_label {
        draw_bar(width, Some((label, theme::BAR)));
    } else {
        draw_bar(width, None);
    }
    let _ = out.execute(Print("\r\n"));

    // Content lines
    let (display_buf, display_cursor) = if has_pastes {
        paste_display(buf, cursor_char, pastes)
    } else {
        (buf.to_string(), cursor_char)
    };
    let (visual_lines, cursor_line, cursor_col) = wrap_and_locate_cursor(&display_buf, display_cursor, usable);
    let is_command = matches!(buf.trim(), "/clear" | "/new" | "/exit" | "/quit");
    for line in &visual_lines {
        let _ = out.execute(Print(" "));
        if is_command {
            let _ = out.execute(SetForegroundColor(theme::ACCENT));
            let _ = out.execute(Print(line));
            let _ = out.execute(ResetColor);
        } else {
            print_styled_line(line, has_pastes);
        }
        let _ = out.execute(Print("\r\n"));
    }

    // Completion list (between content and bottom bar)
    let comp_count = if let Some(comp) = completer {
        let results = &comp.results;
        let sel = comp.selected;
        for (i, path) in results.iter().enumerate() {
            let _ = out.execute(Print("  "));
            if i == sel {
                let _ = out.execute(SetForegroundColor(theme::ACCENT));
                let _ = out.execute(Print(format!("  {}", path)));
                let _ = out.execute(ResetColor);
            } else {
                let _ = out.execute(SetAttribute(Attribute::Dim));
                let _ = out.execute(Print(format!("  {}", path)));
                let _ = out.execute(SetAttribute(Attribute::Reset));
            }
            let _ = out.execute(Print("\r\n"));
        }
        results.len()
    } else {
        0
    };

    // Bottom bar
    if mode == super::input::Mode::Apply {
        draw_bar(width, Some(("apply", theme::APPLY)));
    } else {
        draw_bar(width, None);
    }

    let _ = out.flush();

    let total_content = throbber_count + queued_count + visual_lines.len() + comp_count;
    let final_row = cursor::position().map(|(_, y)| y).unwrap_or(0);
    let top_row = final_row.saturating_sub(total_content as u16 + 1);

    let text_row = top_row + 1 + throbber_count as u16 + queued_count as u16 + cursor_line as u16;
    let text_col = 1 + cursor_col as u16;
    let _ = out.execute(cursor::MoveTo(text_col, text_row));
    let _ = out.flush();

    top_row
}

pub fn erase_prompt_at(top_row: u16) {
    let mut out = io::stdout();
    let _ = out.execute(cursor::MoveTo(0, top_row));
    let _ = out.execute(terminal::Clear(terminal::ClearType::FromCursorDown));
    let _ = out.flush();
}

/// Show confirm prompt inline (tool line already visible above). Returns the user's choice.
pub fn show_confirm(_: &str, _: &str) -> ConfirmChoice {
    let mut out = io::stdout();

    // Record row so we can erase after
    let _ = out.flush();
    let confirm_row = cursor::position().map(|(_, y)| y).unwrap_or(0);

    let _ = out.execute(SetAttribute(Attribute::Dim));
    let _ = out.execute(Print("   \u{2192} allow? (y/n/a)"));
    let _ = out.execute(SetAttribute(Attribute::Reset));

    let _ = out.execute(cursor::Show);
    let _ = out.flush();

    use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};

    let choice = loop {
        if let Ok(Event::Key(KeyEvent {
            code, modifiers, ..
        })) = event::read()
        {
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

    let _ = out.execute(cursor::Hide);

    // Erase the confirm line
    let _ = out.execute(cursor::MoveTo(0, confirm_row));
    let _ = out.execute(terminal::Clear(terminal::ClearType::CurrentLine));

    choice
}
