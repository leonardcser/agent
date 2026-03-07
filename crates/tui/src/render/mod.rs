mod blocks;
mod dialogs;
mod highlight;

pub use dialogs::{
    parse_questions, ConfirmDialog, HelpDialog, PsDialog, Question, QuestionDialog, QuestionOption,
    ResumeDialog, RewindDialog,
};

use crate::input::{Attachment, InputSnapshot, InputState, MenuKind, ATTACHMENT_MARKER};
use crate::theme;
use crate::utils::format_duration;
use crossterm::{
    cursor,
    style::{
        Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
    },
    terminal, QueueableCommand,
};
use std::collections::HashMap;
use std::io::{self, Write};
use std::time::{Duration, Instant};

use blocks::{gap_between, render_block, render_tool, Element};

/// Parameters for rendering the prompt section in `draw_frame`.
/// When `None` is passed instead, only content (blocks + active tool) is drawn.
pub struct FramePrompt<'a> {
    pub state: &'a InputState,
    pub mode: protocol::Mode,
    pub queued: &'a [String],
}

/// Output wrapper that selects the line-advance strategy.
///
/// * `row: None` — **scroll mode** (blocks / prompt): `\r\n` pushes content
///   into terminal scrollback, which is the normal way conversation renders.
/// * `row: Some(r)` — **overlay mode** (dialogs): `MoveTo(0, r+1)` repositions
///   the cursor without scrolling, so dialogs never pollute scrollback.
pub(super) struct RenderOut {
    pub out: io::Stdout,
    pub row: Option<u16>,
}

impl RenderOut {
    /// Create a scroll-mode output (for blocks + prompt).
    /// Dialogs switch to overlay mode by setting `out.row = Some(r)`.
    pub fn scroll() -> Self {
        Self {
            out: io::stdout(),
            row: None,
        }
    }
}

impl io::Write for RenderOut {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.out.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

/// Clear remaining characters on the current line and advance to the next.
///
/// In scroll mode (`row: None`) this emits `\r\n`, pushing content into
/// terminal scrollback.  In overlay mode (`row: Some`) it uses `MoveTo` to
/// reposition without scrolling — dialogs use this to avoid polluting
/// scrollback.
#[track_caller]
pub(super) fn crlf(out: &mut RenderOut) {
    let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
    if let Some(ref mut r) = out.row {
        *r += 1;
        let next = *r;
        let _ = out.queue(cursor::MoveTo(0, next));
    } else {
        let caller = std::panic::Location::caller();
        log(&format!(
            "crlf SCROLL at {}:{}",
            caller.file(),
            caller.line()
        ));
        let _ = out.queue(Print("\r\n"));
    }
}

#[track_caller]
fn log(msg: &str) {
    engine::log::entry(engine::log::Level::Debug, msg, &"");
}

const SPINNER_FRAMES: &[&str] = &["✿", "❀", "✾", "❁"];

fn reasoning_color(effort: protocol::ReasoningEffort) -> Color {
    match effort {
        protocol::ReasoningEffort::Off => theme::REASON_OFF,
        protocol::ReasoningEffort::Low => theme::REASON_LOW,
        protocol::ReasoningEffort::Medium => theme::REASON_MED,
        protocol::ReasoningEffort::High => theme::REASON_HIGH,
    }
}

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
    pub user_message: Option<String>,
    pub start_time: Instant,
}

impl ActiveTool {
    fn elapsed(&self) -> Option<Duration> {
        if matches!(
            self.name.as_str(),
            "bash" | "web_fetch" | "read_process_output" | "stop_process"
        ) {
            Some(self.start_time.elapsed())
        } else {
            None
        }
    }
}

#[derive(Clone)]
pub struct ResumeEntry {
    pub id: String,
    pub title: String,
    pub subtitle: Option<String>,
    pub updated_at_ms: u64,
    pub created_at_ms: u64,
    pub cwd: Option<String>,
    pub parent_id: Option<String>,
    /// Nesting depth for display (0 = root, 1 = fork, etc.)
    pub depth: usize,
}

#[derive(Clone)]
pub enum Block {
    User {
        text: String,
    },
    Thinking {
        content: String,
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
        user_message: Option<String>,
    },
    Confirm {
        tool: String,
        desc: String,
        choice: Option<ConfirmChoice>,
    },
    Hint {
        content: String,
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
    /// Approve all future calls matching a specific pattern (e.g. domain).
    AlwaysPattern(String),
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
        Self {
            blocks: Vec::new(),
            flushed: 0,
            last_block_rows: 0,
        }
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
    fn render(&mut self, out: &mut RenderOut, width: usize) -> u16 {
        log(&format!(
            "History::render flushed={} blocks={}",
            self.flushed,
            self.blocks.len()
        ));
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
                crlf(out);
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
    prev_rows: u16,
    /// Where the next frame starts drawing. Updated at the end of every
    /// `draw_frame` call (always fresh). On first frame or after clear,
    /// falls back to `cursor::position()` once.
    anchor_row: Option<u16>,
    /// Computed each frame inside `draw_frame`, exposed via `dialog_row()`
    /// getter for the app loop.
    prev_dialog_row: Option<u16>,
}

impl PromptState {
    fn new() -> Self {
        Self {
            drawn: false,
            dirty: true,
            prev_rows: 0,
            anchor_row: None,
            prev_dialog_row: None,
        }
    }
}

struct WorkingState {
    since: Option<Instant>,
    final_elapsed: Option<Duration>,
    throbber: Option<Throbber>,
    last_spinner_frame: usize,
    retry_deadline: Option<Instant>,
    tps_samples: Vec<f64>,
}

impl WorkingState {
    fn new() -> Self {
        Self {
            since: None,
            final_elapsed: None,
            throbber: None,
            last_spinner_frame: usize::MAX,
            retry_deadline: None,
            tps_samples: Vec::new(),
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
            self.tps_samples.clear();
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

    fn record_tokens_per_sec(&mut self, tps: f64) {
        self.tps_samples.push(tps);
    }

    fn avg_tokens_per_sec(&self) -> Option<f64> {
        if self.tps_samples.is_empty() {
            return None;
        }
        let sum: f64 = self.tps_samples.iter().sum();
        Some(sum / self.tps_samples.len() as f64)
    }

    fn clear(&mut self) {
        self.throbber = None;
        self.since = None;
        self.final_elapsed = None;
        self.tps_samples.clear();
    }

    fn throbber_spans(&self, show_speed: bool) -> Vec<BarSpan> {
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
                        priority: 0,
                    },
                    BarSpan {
                        text: format!(" {}", format_duration(elapsed.as_secs())),
                        color: theme::MUTED,
                        attr: Some(Attribute::Dim),
                        priority: 0,
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
                        priority: 0,
                    },
                    BarSpan {
                        text: format!(" {}", format_duration(elapsed.as_secs())),
                        color: theme::MUTED,
                        attr: Some(Attribute::Dim),
                        priority: 0,
                    },
                ];
                if show_speed {
                    if let Some(avg) = self.avg_tokens_per_sec() {
                        spans.push(BarSpan {
                            text: " · ".into(),
                            color: theme::MUTED,
                            attr: Some(Attribute::Dim),
                            priority: 3, // drop first
                        });
                        spans.push(BarSpan {
                            text: format!("{:.1} tok/s", avg),
                            color: theme::MUTED,
                            attr: Some(Attribute::Dim),
                            priority: 3, // drop first
                        });
                    }
                }
                if let Throbber::Retrying { delay, attempt } = state {
                    let remaining = self
                        .retry_deadline
                        .map(|t| t.saturating_duration_since(Instant::now()))
                        .unwrap_or(delay);
                    spans.push(BarSpan {
                        text: format!(" (retrying in {}s #{})", remaining.as_secs(), attempt),
                        color: theme::MUTED,
                        attr: Some(Attribute::Dim),
                        priority: 0,
                    });
                }
                spans
            }
            Throbber::Done => {
                let secs = self.final_elapsed.map(|d| d.as_secs()).unwrap_or(0);
                let mut spans = vec![BarSpan {
                    text: format!("done {}", format_duration(secs)),
                    color: theme::MUTED,
                    attr: Some(Attribute::Dim),
                    priority: 0,
                }];
                if show_speed {
                    if let Some(avg) = self.avg_tokens_per_sec() {
                        spans.push(BarSpan {
                            text: " · ".into(),
                            color: theme::MUTED,
                            attr: Some(Attribute::Dim),
                            priority: 3,
                        });
                        spans.push(BarSpan {
                            text: format!("{:.1} tok/s", avg),
                            color: theme::MUTED,
                            attr: Some(Attribute::Dim),
                            priority: 3,
                        });
                    }
                }
                spans
            }
            Throbber::Interrupted => {
                vec![BarSpan {
                    text: "interrupted".into(),
                    color: theme::MUTED,
                    attr: Some(Attribute::Dim),
                    priority: 0,
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
    model_label: Option<String>,
    reasoning_effort: protocol::ReasoningEffort,
    /// True once terminal auto-scrolling has pushed content into scrollback.
    pub has_scrollback: bool,
    /// Terminal row where block content starts (top of conversation).
    /// Set once when the first block is rendered; reset on purge/clear.
    content_start_row: Option<u16>,
    /// Skip the next `render_pending_blocks` call.  Set by
    /// `clear_dialog_area` so that `finish_turn` → `flush_blocks` doesn't
    /// render blocks in scroll mode right after a dialog is dismissed (which
    /// causes scrollback pollution on some terminals).  The blocks are
    /// rendered by the next `draw_frame` instead.
    defer_pending_render: bool,
    /// Downgrade the next `redraw(purge=true)` to `redraw(purge=false)`.
    /// Set by `clear_dialog_area` so that spurious resize events in the
    /// same event batch don't purge scrollback (causing pollution on Ghostty).
    defer_redraw: bool,
    /// A permission dialog is waiting for the user to stop typing.
    pending_dialog: bool,
    running_procs: usize,
    show_speed: bool,
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
            model_label: None,
            reasoning_effort: Default::default(),
            has_scrollback: false,
            content_start_row: None,
            defer_pending_render: false,
            defer_redraw: false,
            pending_dialog: false,
            running_procs: 0,
            show_speed: true,
        }
    }

    pub fn set_show_speed(&mut self, show: bool) {
        self.show_speed = show;
        self.prompt.dirty = true;
    }

    pub fn set_running_procs(&mut self, count: usize) {
        if count != self.running_procs {
            self.running_procs = count;
            self.prompt.dirty = true;
        }
    }

    /// Row where a dialog should start rendering (lines up with the prompt bar).
    pub fn dialog_row(&self) -> u16 {
        self.prompt.prev_dialog_row.unwrap_or(0)
    }

    /// Dismiss a dialog overlay.
    ///
    /// Clears from the dialog's anchor row down and lets the prompt redraw
    /// at that position on the next tick.
    pub fn clear_dialog_area(&mut self, dialog_anchor: Option<u16>) {
        let anchor = dialog_anchor.unwrap_or(0);
        let screen_anchor = self.prompt.anchor_row.unwrap_or(anchor);
        let clear_from = anchor.min(screen_anchor);
        log(&format!(
            "clear_dialog_area anchor={anchor} screen_anchor={screen_anchor} clear_from={clear_from}"
        ));

        // Clear each row individually instead of using Clear(FromCursorDown).
        // Some terminals (e.g. Ghostty) push the viewport into scrollback
        // when Clear(FromCursorDown) is issued at row 0.
        let height = terminal::size().map(|(_, h)| h).unwrap_or(24);
        let mut out = RenderOut::scroll();
        for row in clear_from..height {
            let _ = out.queue(cursor::MoveTo(0, row));
            let _ = out.queue(terminal::Clear(terminal::ClearType::CurrentLine));
        }
        let _ = out.flush();
        self.defer_pending_render = true;
        self.defer_redraw = true;
        self.prompt.anchor_row = Some(clear_from);
        self.prompt.drawn = false;
        self.prompt.dirty = true;
        self.prompt.prev_rows = 0;
    }

    /// Move the cursor to the line after the prompt so the shell resumes cleanly.
    pub fn move_cursor_past_prompt(&self) {
        if self.prompt.drawn {
            let anchor = self.prompt.anchor_row.unwrap_or(0);
            let end_row = anchor + self.prompt.prev_rows;
            let height = terminal::size().map(|(_, h)| h).unwrap_or(24);
            let mut out = RenderOut::scroll();
            let _ = out.queue(cursor::MoveTo(0, end_row));
            // At the terminal bottom there's no row below to land on —
            // emit a newline so the shell prompt gets a fresh line.
            if end_row >= height.saturating_sub(1) {
                let _ = out.queue(Print("\n"));
            }
            let _ = out.flush();
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
            user_message: None,
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
            // Reset timer when transitioning from confirm → pending (user approved)
            if tool.status == ToolStatus::Confirm && status == ToolStatus::Pending {
                tool.start_time = Instant::now();
            }
            tool.status = status;
            self.prompt.dirty = true;
        }
    }

    pub fn set_active_user_message(&mut self, msg: String) {
        if let Some(ref mut tool) = self.active_tool {
            tool.user_message = Some(msg);
            self.prompt.dirty = true;
        }
    }

    pub fn finish_tool(&mut self, status: ToolStatus, output: Option<ToolOutput>) {
        if let Some(tool) = self.active_tool.take() {
            let elapsed = tool.elapsed();
            self.history.push(Block::ToolCall {
                name: tool.name,
                summary: tool.summary,
                args: tool.args,
                status,
                elapsed,
                output,
                user_message: tool.user_message,
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

    pub fn set_model_label(&mut self, label: String) {
        self.model_label = Some(label);
        self.prompt.dirty = true;
    }

    pub fn set_reasoning_effort(&mut self, effort: protocol::ReasoningEffort) {
        self.reasoning_effort = effort;
        self.prompt.dirty = true;
    }

    pub fn set_throbber(&mut self, state: Throbber) {
        self.working.set_throbber(state);
        self.prompt.dirty = true;
    }

    pub fn record_tokens_per_sec(&mut self, tps: f64) {
        self.working.record_tokens_per_sec(tps);
        self.prompt.dirty = true;
    }

    pub fn clear_throbber(&mut self) {
        self.working.clear();
        self.prompt.dirty = true;
    }

    pub fn set_pending_dialog(&mut self, pending: bool) {
        self.pending_dialog = pending;
        self.prompt.dirty = true;
    }

    pub fn mark_dirty(&mut self) {
        self.prompt.dirty = true;
    }

    /// Convert active tool to a history block and render any pending blocks.
    pub fn flush_blocks(&mut self) {
        let _perf = crate::perf::begin("flush_blocks");
        self.commit_active_tool();
        self.render_pending_blocks();
    }

    /// Render unflushed blocks and scroll all viewport content into
    /// scrollback so the dialog gets a clean viewport.
    pub fn flush_history_to_scrollback(&mut self) {
        self.render_pending_blocks();

        // Scroll viewport content into scrollback. Content rendered with
        // \r\n only enters scrollback when lines are pushed off the top of
        // the viewport. If the conversation fits on screen, it's still in
        // the viewport — not in scrollback. ScrollUp(n) physically pushes
        // the top n lines into scrollback.
        let rows_to_scroll = self
            .prompt
            .anchor_row
            .or(self.content_start_row)
            .unwrap_or(0);
        if rows_to_scroll > 0 {
            let mut out = RenderOut::scroll();
            let _ = out.queue(terminal::ScrollUp(rows_to_scroll));
            let _ = out.flush();
            self.prompt.anchor_row = Some(0);
            self.prompt.prev_dialog_row = self
                .prompt
                .prev_dialog_row
                .map(|r| r.saturating_sub(rows_to_scroll));
            self.prompt.drawn = false;
            self.prompt.dirty = true;
            self.has_scrollback = true;
        }
    }

    /// Convert active tool to a history block without rendering.
    /// The block remains unflushed so that `draw_frame(None)` will render
    /// it (along with any preceding reasoning blocks) before the dialog
    /// paints on top.
    pub fn commit_active_tool(&mut self) {
        if let Some(tool) = self.active_tool.take() {
            let elapsed = tool.elapsed();
            self.history.push(Block::ToolCall {
                name: tool.name,
                summary: tool.summary,
                args: tool.args,
                status: ToolStatus::Err,
                elapsed,
                output: tool.output,
                user_message: tool.user_message,
            });
        }
    }

    fn render_pending_blocks(&mut self) {
        if self.defer_pending_render {
            self.defer_pending_render = false;
            return;
        }
        if !self.history.has_unflushed() {
            return;
        }
        log("render_pending_blocks");
        let mut out = RenderOut::scroll();
        let _ = out.queue(terminal::BeginSynchronizedUpdate);
        let start_row = if self.prompt.drawn {
            let row = self.prompt.anchor_row.unwrap_or(0);
            let _ = out.queue(cursor::MoveTo(0, row));
            let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));
            self.prompt.drawn = false;
            row
        } else {
            self.prompt
                .anchor_row
                .take()
                .unwrap_or_else(|| cursor::position().map(|(_, y)| y).unwrap_or(0))
        };
        let block_rows = self.history.render(&mut out, term_width());
        self.prompt.anchor_row = Some(start_row + block_rows);
        let _ = out.queue(terminal::EndSynchronizedUpdate);
        let _ = out.flush();
    }

    pub fn erase_prompt(&mut self) {
        if self.prompt.drawn {
            if let Some(anchor) = self.prompt.anchor_row {
                let end = anchor + self.prompt.prev_rows;
                let mut out = RenderOut::scroll();
                let _ = out.queue(terminal::BeginSynchronizedUpdate);
                for r in anchor..=end {
                    let _ = out.queue(cursor::MoveTo(0, r));
                    let _ = out.queue(terminal::Clear(terminal::ClearType::CurrentLine));
                }
                let _ = out.queue(cursor::MoveTo(0, anchor));
                let _ = out.queue(terminal::EndSynchronizedUpdate);
                let _ = out.flush();
            }
            self.prompt.drawn = false;
            self.prompt.dirty = true;
        }
    }

    /// Re-render all blocks. When `purge` is true, clears scrollback and
    /// screen first — necessary after resize or when content has overflowed.
    /// When false, redraws over the current viewport (faster, no flash).
    pub fn redraw(&mut self, purge: bool) {
        let purge = if self.defer_redraw {
            self.defer_redraw = false;
            false
        } else {
            purge
        };
        log(&format!("redraw purge={purge}"));
        let mut out = RenderOut::scroll();
        let _ = out.queue(terminal::BeginSynchronizedUpdate);
        let start = if purge {
            let _ = out.queue(cursor::MoveTo(0, 0));
            let _ = out.queue(terminal::Clear(terminal::ClearType::All));
            let _ = out.queue(terminal::Clear(terminal::ClearType::Purge));
            0
        } else {
            let row = self.content_start_row.unwrap_or(0);
            let _ = out.queue(cursor::MoveTo(0, row));
            row
        };
        self.history.flushed = 0;
        self.history.last_block_rows = 0;
        let block_rows = self.history.render(&mut out, term_width());
        if !purge {
            // Clear remaining rows individually — Clear(FromCursorDown) at
            // low row numbers causes Ghostty to push the viewport into
            // scrollback.
            let cur_row = start + block_rows;
            let height = terminal::size().map(|(_, h)| h).unwrap_or(24);
            for row in cur_row..height {
                let _ = out.queue(cursor::MoveTo(0, row));
                let _ = out.queue(terminal::Clear(terminal::ClearType::CurrentLine));
            }
        }
        let _ = out.queue(terminal::EndSynchronizedUpdate);
        let _ = out.flush();
        self.prompt.drawn = false;
        self.prompt.dirty = true;
        self.prompt.prev_rows = 0;
        if purge {
            self.has_scrollback = false;
            self.content_start_row = Some(0);
            self.prompt.anchor_row = Some(block_rows);
        } else {
            self.prompt.anchor_row = Some(start + block_rows);
        }
    }

    pub fn clear(&mut self) {
        self.history.clear();
        self.active_tool = None;
        self.prompt = PromptState::new();
        self.prompt.anchor_row = Some(0);
        self.working.clear();
        self.context_tokens = None;
        self.has_scrollback = false;
        self.content_start_row = None;
        let mut out = RenderOut::scroll();
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
        self.history
            .blocks
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
        self.redraw(true);
    }

    pub fn draw_prompt(&mut self, state: &InputState, mode: protocol::Mode, width: usize) {
        self.draw_frame(
            width,
            Some(FramePrompt {
                state,
                mode,
                queued: &[],
            }),
        );
    }

    /// Unified rendering entry point. Renders pending blocks + active tool,
    /// then either the prompt (`Some`) or nothing (`None` = dialog covers it).
    /// Returns `true` when content-only mode drew something (caller should
    /// re-dirty any overlay dialog so it repaints on top).
    pub fn draw_frame(&mut self, width: usize, prompt: Option<FramePrompt>) -> bool {
        let _perf = crate::perf::begin("draw_frame");

        if let Some(start) = self.working.since {
            let frame = (start.elapsed().as_millis() / 150) as usize % SPINNER_FRAMES.len();
            if frame != self.working.last_spinner_frame {
                self.working.last_spinner_frame = frame;
                self.prompt.dirty = true;
            }
        }

        let has_new_blocks = self.history.has_unflushed();
        let is_dialog = prompt.is_none();

        // Content-only (dialog overlay): only render when new blocks arrived.
        // The active tool is already on screen from before the dialog opened;
        // re-rendering it every tick would clear+redraw the dialog area and
        // cause visible flicker.
        if is_dialog && !has_new_blocks {
            return false;
        }
        // Full mode: skip if nothing changed.
        if !is_dialog && !has_new_blocks && !self.prompt.dirty {
            return false;
        }

        log(&format!(
            "draw_frame: is_dialog={is_dialog} has_new_blocks={has_new_blocks} dirty={}",
            self.prompt.dirty
        ));

        let mut out = RenderOut::scroll();

        // ── Position cursor ─────────────────────────────────────────────
        let draw_start_row = self
            .prompt
            .anchor_row
            .take()
            .unwrap_or_else(|| cursor::position().map(|(_, y)| y).unwrap_or(0));

        let _ = out.queue(terminal::BeginSynchronizedUpdate);
        let _ = out.queue(cursor::Hide);
        if self.prompt.drawn {
            let _ = out.queue(cursor::MoveTo(0, draw_start_row));
        }
        // In content-only mode (dialog overlay), use overlay rendering so
        // block output uses MoveTo instead of \r\n — preventing scrollback
        // pollution before the dialog paints on top.
        if is_dialog {
            out.row = Some(draw_start_row);
        }

        // ── Render blocks ───────────────────────────────────────────────
        let block_rows = self.history.render(&mut out, width);

        // ── Render active tool ──────────────────────────────────────────
        let mut active_rows: u16 = 0;
        if let Some(ref tool) = self.active_tool {
            let tool_gap = if let Some(last) = self.history.blocks.last() {
                gap_between(&Element::Block(last), &Element::ActiveTool)
            } else {
                0
            };
            for _ in 0..tool_gap {
                crlf(&mut out);
            }
            let rows = render_tool(
                &mut out,
                &tool.name,
                &tool.summary,
                &tool.args,
                tool.status,
                Some(tool.start_time.elapsed()),
                tool.output.as_ref(),
                tool.user_message.as_deref(),
                width,
            );
            active_rows = tool_gap + rows;
        }

        if let Some(p) = prompt {
            // ── Full mode: render prompt ────────────────────────────────
            let gap = if self.active_tool.is_some() {
                gap_between(&Element::ActiveTool, &Element::Prompt)
            } else {
                self.history.blocks.last().map_or(0, |last| {
                    gap_between(&Element::Block(last), &Element::Prompt)
                })
            };
            for _ in 0..gap {
                crlf(&mut out);
            }

            let pre_prompt = block_rows + active_rows + gap;
            let (top_row, new_rows, scrolled) = self.draw_prompt_sections(
                &mut out,
                p.state,
                p.mode,
                width,
                p.queued,
                self.prompt.prev_rows.saturating_sub(pre_prompt),
                draw_start_row,
                pre_prompt,
            );
            if scrolled {
                self.has_scrollback = true;
                self.content_start_row = Some(top_row);
            } else if self.content_start_row.is_none() {
                self.content_start_row = Some(top_row);
            }
            self.prompt.prev_rows = (pre_prompt - block_rows) + new_rows;

            // anchor_row: where the next frame starts drawing (prompt section).
            // When blocks overflow, top_row + block_rows overshoots — compute
            // from the bottom of the viewport instead.
            let prompt_section_rows = active_rows + gap + new_rows;
            if scrolled {
                let height = terminal::size().map(|(_, h)| h).unwrap_or(24);
                self.prompt.anchor_row = Some(height.saturating_sub(prompt_section_rows));
            } else {
                self.prompt.anchor_row = Some(top_row + block_rows);
            }
            // prev_dialog_row: where the prompt bar actually starts (after active
            // tool + gap).  Dialogs render here to line up with the prompt.
            let anchor = self.prompt.anchor_row.unwrap_or(0);
            self.prompt.prev_dialog_row = Some(anchor + active_rows + gap);
            self.prompt.drawn = true;
            self.prompt.dirty = false;

            let _ = out.queue(cursor::Show);
            let _ = out.queue(terminal::EndSynchronizedUpdate);
            let _ = out.flush();
            false
        } else {
            // ── Content-only mode (dialog inline) ───────────────────────
            // Render blocks + active tool, then leave a gap line before
            // the dialog that follows.  The dialog renders inline at
            // `anchor_row`, pushing conversation up via terminal scroll
            // rather than overlaying it.
            let gap: u16 = if block_rows > 0 || active_rows > 0 {
                crlf(&mut out);
                1
            } else {
                0
            };
            let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));

            let content_rows = block_rows + active_rows + gap;
            let height = terminal::size().map(|(_, h)| h).unwrap_or(24);
            let scrolled = draw_start_row + content_rows > height;

            let anchor = if scrolled {
                self.has_scrollback = true;
                height.saturating_sub(active_rows + gap)
            } else {
                draw_start_row + block_rows
            };
            self.prompt.anchor_row = Some(anchor);
            self.prompt.prev_dialog_row = Some(anchor + active_rows + gap);
            self.prompt.prev_rows = active_rows + gap;
            self.prompt.drawn = true;
            self.prompt.dirty = false;

            let _ = out.queue(terminal::EndSynchronizedUpdate);
            let _ = out.flush();
            content_rows > 0
        }
    }

    /// Returns (top_row, total_prompt_rows, scrolled).
    #[allow(clippy::too_many_arguments)]
    fn draw_prompt_sections(
        &self,
        out: &mut RenderOut,
        state: &InputState,
        mode: protocol::Mode,
        width: usize,
        queued: &[String],
        prev_rows: u16,
        draw_start_row: u16,
        pre_prompt_rows: u16,
    ) -> (u16, u16, bool) {
        let usable = width.saturating_sub(2);
        let height = terminal::size()
            .map(|(_, h)| h as usize)
            .unwrap_or(24)
            .saturating_sub(pre_prompt_rows as usize);
        let stash_rows = if state.stash.is_some() { 1 } else { 0 };

        let mut extra_rows = render_stash(out, &state.stash, usable);
        let queued_visual = render_queued(out, queued, usable);
        extra_rows += queued_visual;
        let queued_rows = queued_visual as usize;

        let vi_normal = state.vim_mode() == Some(crate::vim::ViMode::Normal);
        let bar_color = if vi_normal {
            theme::accent()
        } else {
            theme::BAR
        };

        // Build all bar spans with priorities. draw_bar drops highest
        // priority first until everything fits.
        // Priorities: 0 = always, 1 = context tokens, 2 = model, 3 = tok/s
        let mut throbber_spans = self.working.throbber_spans(self.show_speed);

        let mut right_spans = Vec::new();
        if let Some(ref model) = self.model_label {
            right_spans.push(BarSpan {
                text: format!(" {}", model),
                color: theme::MUTED,
                attr: None,
                priority: 2,
            });
            if self.reasoning_effort != protocol::ReasoningEffort::Off {
                let effort = self.reasoning_effort;
                right_spans.push(BarSpan {
                    text: format!(" {}", effort.label()),
                    color: reasoning_color(effort),
                    attr: None,
                    priority: 2,
                });
            }
        }
        if let Some(tokens) = self.context_tokens {
            if !right_spans.is_empty() {
                right_spans.push(BarSpan {
                    text: " ·".into(),
                    color: bar_color,
                    attr: None,
                    priority: 2,
                });
            }
            right_spans.push(BarSpan {
                text: format!(" {}", format_tokens(tokens)),
                color: theme::MUTED,
                attr: None,
                priority: 1,
            });
        }
        if self.running_procs > 0 {
            right_spans.push(BarSpan {
                text: " · ".into(),
                color: bar_color,
                attr: None,
                priority: 0,
            });
            let label = if self.running_procs == 1 {
                "1 proc".to_string()
            } else {
                format!("{} procs", self.running_procs)
            };
            right_spans.push(BarSpan {
                text: label,
                color: theme::accent(),
                attr: None,
                priority: 0,
            });
        }
        if self.pending_dialog {
            if !throbber_spans.is_empty() {
                throbber_spans.push(BarSpan {
                    text: " · ".into(),
                    color: bar_color,
                    attr: None,
                    priority: 0,
                });
            }
            throbber_spans.push(BarSpan {
                text: "permission pending".into(),
                color: theme::accent(),
                attr: Some(Attribute::Bold),
                priority: 0,
            });
        }
        draw_bar(
            out,
            width,
            if throbber_spans.is_empty() {
                None
            } else {
                Some(&throbber_spans)
            },
            if right_spans.is_empty() {
                None
            } else {
                Some(&right_spans)
            },
            bar_color,
        );
        let _ = out.queue(Print("\r\n"));

        let spans = build_display_spans(&state.buf, &state.attachments);
        let display_buf = spans_to_string(&spans);
        let char_kinds = build_char_kinds(&spans);
        let display_cursor = map_cursor(state.cursor_char(), &state.buf, &spans);
        let (visual_lines, cursor_line, cursor_col) =
            wrap_and_locate_cursor(&display_buf, &char_kinds, display_cursor, usable);
        let is_command = crate::completer::Completer::is_command(state.buf.trim());
        let is_exec = matches!(state.buf.as_bytes(), [b'!', c, ..] if !c.is_ascii_whitespace());
        let is_exec_invalid = state.buf == "!";
        let total_content_rows = visual_lines.len();
        let menu_rows = state.menu_rows();
        let comp_total = if menu_rows > 0 {
            menu_rows
        } else {
            state
                .completer
                .as_ref()
                .map(|c| c.results.len().min(5))
                .unwrap_or(0)
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

        for (li, (line, kinds)) in visual_lines
            .iter()
            .skip(scroll_offset)
            .take(content_rows)
            .enumerate()
        {
            let abs_idx = scroll_offset + li;
            let _ = out.queue(Print(" "));
            if is_command {
                let _ = out.queue(SetForegroundColor(theme::accent()));
                let _ = out.queue(Print(line));
                let _ = out.queue(ResetColor);
            } else if (is_exec || is_exec_invalid) && abs_idx == 0 && line.starts_with('!') {
                let _ = out.queue(SetForegroundColor(Color::Red));
                let _ = out.queue(SetAttribute(Attribute::Bold));
                let _ = out.queue(Print("!"));
                let _ = out.queue(SetAttribute(Attribute::Reset));
                let _ = out.queue(ResetColor);
                render_styled_chars(out, &line[1..], &kinds[1..]);
            } else {
                render_styled_chars(out, line, kinds);
            }
            let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
            let _ = out.queue(Print("\r\n"));
        }

        let mode_spans: Vec<BarSpan> = match mode {
            protocol::Mode::Plan => vec![BarSpan {
                text: " plan".into(),
                color: theme::PLAN,
                attr: None,
                priority: 0,
            }],
            protocol::Mode::Apply => vec![BarSpan {
                text: " apply".into(),
                color: theme::APPLY,
                attr: None,
                priority: 0,
            }],
            protocol::Mode::Yolo => vec![BarSpan {
                text: " yolo".into(),
                color: theme::YOLO,
                attr: None,
                priority: 0,
            }],
            protocol::Mode::Normal => vec![],
        };
        draw_bar(
            out,
            width,
            None,
            if mode_spans.is_empty() {
                None
            } else {
                Some(&mode_spans)
            },
            bar_color,
        );

        if comp_rows > 0 {
            let _ = out.queue(Print("\r\n"));
        }
        let comp_rows = if let Some(ref ms) = state.menu {
            draw_menu(out, ms, comp_rows, self.reasoning_effort)
        } else {
            draw_completions(out, state.completer.as_ref(), comp_rows)
        };

        let total_rows = stash_rows + queued_rows + 1 + content_rows + 1 + comp_rows;
        let new_rows = total_rows as u16;

        if prev_rows > new_rows {
            // The \r\n here escapes any "pending wrap" state on the bar line,
            // so Clear operations below won't erase the last bar character.
            let n = prev_rows - new_rows;
            for _ in 0..n {
                let _ = out.queue(Print("\r\n"));
                let _ = out.queue(terminal::Clear(terminal::ClearType::CurrentLine));
            }
            // Clear anything remaining below.
            let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));
        } else if comp_rows > 0 {
            // Completions already moved past the bar; safe to clear below.
            let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));
        }

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
        // When blocks overflow the screen, `top_row + pre_prompt_rows` overshoots
        // because pre_prompt_rows counts scrolled-off block rows. Compute the
        // prompt-section start from the bottom of the viewport instead.
        let prompt_start = if scrolled {
            height.saturating_sub(new_rows + rows_below)
        } else {
            top_row + pre_prompt_rows
        };
        let text_row = prompt_start + 1 + extra_rows + cursor_line_visible as u16;
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

fn render_stash(out: &mut RenderOut, stash: &Option<InputSnapshot>, usable: usize) -> u16 {
    let Some(ref snap) = stash else {
        return 0;
    };
    let full_display = spans_to_string(&build_display_spans(&snap.buf, &snap.attachments));
    let first_line = full_display.lines().next().unwrap_or("");
    let line_count = full_display.lines().count();
    let max_chars = usable.saturating_sub(2);
    let display: String = first_line.chars().take(max_chars).collect();
    let suffix = if display.chars().count() < first_line.chars().count() || line_count > 1 {
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

fn render_queued(out: &mut RenderOut, queued: &[String], usable: usize) -> u16 {
    // Mirrors Block::User rendering (blocks.rs) but with a 2-char indent
    // and no stripping of leading/trailing blank lines.
    let indent = 2usize;
    let text_w = usable.saturating_sub(indent + 1).max(1);
    let mut rows = 0u16;
    for msg in queued {
        let all_lines: Vec<String> = msg.lines().map(|l| l.replace('\t', "    ")).collect();
        let wraps = all_lines.iter().any(|l| l.chars().count() > text_w);
        let multiline = all_lines.len() > 1 || wraps;
        let block_w = if multiline {
            if wraps {
                text_w
            } else {
                all_lines
                    .iter()
                    .map(|l| l.chars().count())
                    .max()
                    .unwrap_or(0)
                    + 1
            }
        } else {
            0
        };
        for line in &all_lines {
            if line.is_empty() {
                let fill = if block_w > 0 { block_w + 1 } else { 2 };
                let _ = out.queue(Print(" ".repeat(indent)));
                let _ = out
                    .queue(SetBackgroundColor(theme::USER_BG))
                    .and_then(|o| o.queue(Print(" ".repeat(fill))))
                    .and_then(|o| o.queue(SetAttribute(Attribute::Reset)))
                    .and_then(|o| o.queue(ResetColor));
                crlf(out);
                rows += 1;
                continue;
            }
            let chunks = chunk_line(line, text_w);
            for chunk in &chunks {
                let chunk_len = chunk.chars().count();
                let trailing = if block_w > 0 {
                    block_w.saturating_sub(chunk_len)
                } else {
                    1
                };
                let _ = out.queue(Print(" ".repeat(indent)));
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

/// Split a string into fixed-width segments by character count.
pub(super) fn chunk_line(line: &str, width: usize) -> Vec<String> {
    let chars: Vec<char> = line.chars().collect();
    if chars.is_empty() {
        return vec![String::new()];
    }
    if width == 0 {
        return vec![line.to_string()];
    }
    chars.chunks(width).map(|c| c.iter().collect()).collect()
}

fn make_relative(path: &str) -> String {
    if let Ok(cwd) = std::env::current_dir() {
        let prefix = cwd.to_string_lossy();
        if let Some(rest) = path.strip_prefix(prefix.as_ref()) {
            let rest = rest.strip_prefix('/').unwrap_or(rest);
            if rest.is_empty() {
                return ".".into();
            }
            return rest.into();
        }
    }
    path.into()
}

pub fn tool_arg_summary(name: &str, args: &HashMap<String, serde_json::Value>) -> String {
    match name {
        "bash" => {
            let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
            cmd.lines().next().unwrap_or("").to_string()
        }
        "read_file" | "write_file" | "edit_file" => {
            let path = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
            make_relative(path)
        }
        "glob" => args
            .get("pattern")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .into(),
        "grep" => {
            let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            match args.get("path").and_then(|v| v.as_str()) {
                Some(p) => format!("{} in {}", pattern, make_relative(p)),
                None => pattern.into(),
            }
        }
        "web_fetch" => args
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .into(),
        "web_search" => args
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .into(),
        "read_process_output" | "stop_process" => {
            args.get("id").and_then(|v| v.as_str()).unwrap_or("").into()
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
    Some(format!("timeout: {}", format_duration(ms / 1000)))
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
    char_kinds: &[SpanKind],
    cursor_char: usize,
    usable: usize,
) -> (Vec<(String, Vec<SpanKind>)>, usize, usize) {
    let mut visual_lines: Vec<(String, Vec<SpanKind>)> = Vec::new();
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
            visual_lines.push((String::new(), Vec::new()));
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
                let kinds = char_kinds
                    .get(line_start..line_start + chunk.len())
                    .unwrap_or_default()
                    .to_vec();
                chars_seen += chunk.len();
                visual_lines.push((chunk.iter().collect(), kinds));
            }
        }
        chars_seen += 1; // for the '\n'
    }
    if visual_lines.is_empty() {
        visual_lines.push((String::new(), Vec::new()));
    }
    (visual_lines, cursor_line, cursor_col)
}

pub(super) struct BarSpan {
    text: String,
    color: Color,
    attr: Option<Attribute>,
    /// Priority for responsive dropping. 0 = always show, higher = drop first.
    priority: u8,
}

pub(super) fn draw_bar(
    out: &mut RenderOut,
    width: usize,
    left: Option<&[BarSpan]>,
    right: Option<&[BarSpan]>,
    bar_color: Color,
) {
    let dash = "\u{2500}";
    let min_dashes = 4;

    // Find the max priority we need to drop to fit.
    let max_priority = {
        let all_priorities: Vec<u8> = left
            .into_iter()
            .chain(right)
            .flat_map(|spans| spans.iter().map(|s| s.priority))
            .collect();
        *all_priorities.iter().max().unwrap_or(&0)
    };

    let mut drop_above = max_priority + 1; // start by showing everything
    loop {
        let left_chars: usize = left
            .map(|spans| {
                let inner: usize = spans
                    .iter()
                    .filter(|s| s.priority < drop_above)
                    .map(|s| s.text.chars().count())
                    .sum();
                if inner > 0 {
                    1 + 1 + inner + 1
                } else {
                    0
                } // dash + space + spans + space
            })
            .unwrap_or(0);
        let right_chars: usize = right
            .map(|spans| {
                let inner: usize = spans
                    .iter()
                    .filter(|s| s.priority < drop_above)
                    .map(|s| s.text.chars().count())
                    .sum();
                if inner > 0 {
                    inner + 2
                } else {
                    0
                } // spans + space + trailing dash
            })
            .unwrap_or(0);
        let total = left_chars + min_dashes + right_chars;
        if total <= width || drop_above == 1 {
            break;
        }
        drop_above -= 1;
    }

    let left_filtered: Vec<&BarSpan> = left
        .map(|spans| spans.iter().filter(|s| s.priority < drop_above).collect())
        .unwrap_or_default();
    let right_filtered: Vec<&BarSpan> = right
        .map(|spans| spans.iter().filter(|s| s.priority < drop_above).collect())
        .unwrap_or_default();

    let left_len: usize = if left_filtered.is_empty() {
        0
    } else {
        1 + 1
            + left_filtered
                .iter()
                .map(|s| s.text.chars().count())
                .sum::<usize>()
            + 1
    };
    let right_len: usize = if right_filtered.is_empty() {
        0
    } else {
        right_filtered
            .iter()
            .map(|s| s.text.chars().count())
            .sum::<usize>()
            + 2
    };
    let bar_len = width.saturating_sub(left_len + right_len);

    if !left_filtered.is_empty() {
        let _ = out.queue(SetForegroundColor(bar_color));
        let _ = out.queue(Print(dash));
        let _ = out.queue(ResetColor);
        let _ = out.queue(Print(" "));
        for span in &left_filtered {
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

    if !right_filtered.is_empty() {
        for span in &right_filtered {
            let _ = out.queue(SetForegroundColor(span.color));
            let _ = out.queue(Print(&span.text));
            let _ = out.queue(ResetColor);
        }
        let _ = out.queue(Print(" "));
        let _ = out.queue(SetForegroundColor(bar_color));
        let _ = out.queue(Print(dash));
        let _ = out.queue(ResetColor);
    }
}

enum Span {
    Plain(String),
    Attachment(String),
    AtRef(String),
}

#[derive(Clone, Copy, PartialEq)]
enum SpanKind {
    Plain,
    Attachment,
    AtRef,
}

fn build_char_kinds(spans: &[Span]) -> Vec<SpanKind> {
    let mut kinds = Vec::new();
    for span in spans {
        let (text, kind) = match span {
            Span::Plain(t) => (t.as_str(), SpanKind::Plain),
            Span::Attachment(t) => (t.as_str(), SpanKind::Attachment),
            Span::AtRef(t) => (t.as_str(), SpanKind::AtRef),
        };
        kinds.extend(std::iter::repeat_n(kind, text.chars().count()));
    }
    kinds
}

fn build_display_spans(buf: &str, attachments: &[Attachment]) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut plain = String::new();
    let mut att_idx = 0;

    let chars: Vec<char> = buf.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == ATTACHMENT_MARKER {
            if !plain.is_empty() {
                spans.push(Span::Plain(std::mem::take(&mut plain)));
            }
            let label = attachments
                .get(att_idx)
                .map(|a| a.display_label())
                .unwrap_or_else(|| "[?]".into());
            spans.push(Span::Attachment(label));
            att_idx += 1;
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
                    let path_str = &token[1..];
                    if std::path::Path::new(path_str).exists() {
                        spans.push(Span::AtRef(token));
                    } else {
                        spans.push(Span::Plain(token));
                    }
                    i = end;
                } else {
                    spans.push(Span::Plain("@".to_string()));
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
            Span::Plain(t) | Span::Attachment(t) | Span::AtRef(t) => s.push_str(t),
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
            Span::Attachment(label) => {
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

/// Render a line using pre-computed per-character span kinds.
fn render_styled_chars(out: &mut RenderOut, line: &str, kinds: &[SpanKind]) {
    let mut current = SpanKind::Plain;
    for (i, ch) in line.chars().enumerate() {
        let kind = kinds.get(i).copied().unwrap_or(SpanKind::Plain);
        if kind != current {
            if current != SpanKind::Plain {
                let _ = out.queue(ResetColor);
            }
            if kind == SpanKind::AtRef || kind == SpanKind::Attachment {
                let _ = out.queue(SetForegroundColor(theme::accent()));
            }
            current = kind;
        }
        let _ = out.queue(Print(ch));
    }
    if current != SpanKind::Plain {
        let _ = out.queue(ResetColor);
    }
}

fn draw_completions(
    out: &mut RenderOut,
    completer: Option<&crate::completer::Completer>,
    max_rows: usize,
) -> usize {
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
            let _ = out.queue(SetForegroundColor(theme::accent()));
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

fn draw_menu(
    out: &mut RenderOut,
    ms: &crate::input::MenuState,
    max_rows: usize,
    reasoning_effort: protocol::ReasoningEffort,
) -> usize {
    if max_rows == 0 {
        return 0;
    }
    let selected = ms.nav.selected;
    match &ms.kind {
        MenuKind::Settings {
            vim_enabled,
            auto_compact,
            show_speed,
        } => {
            let rows: &[(&str, bool)] = &[
                ("vim mode", *vim_enabled),
                ("auto compact", *auto_compact),
                ("show speed", *show_speed),
            ];
            let col = rows.iter().map(|(l, _)| l.len()).max().unwrap_or(0) + 4;
            let mut drawn = 0;
            for (idx, (label, value)) in rows.iter().enumerate() {
                if drawn >= max_rows {
                    break;
                }
                if drawn > 0 {
                    let _ = out.queue(Print("\r\n"));
                }
                draw_menu_row(
                    out,
                    label,
                    if *value { "on" } else { "off" },
                    col,
                    idx == selected,
                );
                drawn += 1;
            }
            drawn
        }
        MenuKind::Theme { presets, .. } => {
            let col = presets
                .iter()
                .map(|(name, _, _)| name.len())
                .max()
                .unwrap_or(0)
                + 4;
            let mut drawn = 0;
            for (idx, (name, detail, ansi)) in presets.iter().enumerate() {
                if drawn >= max_rows {
                    break;
                }
                if drawn > 0 {
                    let _ = out.queue(Print("\r\n"));
                }
                let _ = out.queue(Print("  "));
                if idx == selected {
                    let _ = out.queue(SetForegroundColor(Color::AnsiValue(*ansi)));
                    let _ = out.queue(Print(format!("● {name}")));
                    let _ = out.queue(ResetColor);
                } else {
                    let _ = out.queue(SetAttribute(Attribute::Dim));
                    let _ = out.queue(Print(format!("  {name}")));
                    let _ = out.queue(SetAttribute(Attribute::Reset));
                }
                let label_len = name.len() + 2; // account for "● " or "  "
                let padding = " ".repeat(col.saturating_sub(label_len - 2));
                let _ = out.queue(SetAttribute(Attribute::Dim));
                let _ = out.queue(Print(format!("{padding}{detail}")));
                let _ = out.queue(SetAttribute(Attribute::Reset));
                let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
                drawn += 1;
            }
            drawn
        }
        MenuKind::Stats { lines } => draw_stats(out, lines, max_rows),
        MenuKind::Model { models } => {
            if models.is_empty() {
                return 0;
            }
            let col = models
                .iter()
                .map(|(_, name, _)| name.len())
                .max()
                .unwrap_or(0)
                + 4;
            let mut drawn = 0;
            for (idx, (_, model_name, provider_name)) in models.iter().enumerate() {
                if drawn >= max_rows {
                    break;
                }
                if drawn > 0 {
                    let _ = out.queue(Print("\r\n"));
                }
                draw_menu_row(out, model_name, provider_name, col, idx == selected);
                drawn += 1;
            }
            if drawn > 0 && drawn + 2 <= max_rows {
                let _ = out.queue(Print("\r\n"));
                let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
                drawn += 1;
                let _ = out.queue(Print("\r\n"));
                let _ = out.queue(SetAttribute(Attribute::Dim));
                let _ = out.queue(Print("  thinking: "));
                let _ = out.queue(SetAttribute(Attribute::Reset));
                let _ = out.queue(SetForegroundColor(reasoning_color(reasoning_effort)));
                let _ = out.queue(Print(reasoning_effort.label()));
                let _ = out.queue(ResetColor);
                let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
                drawn += 1;
            }
            drawn
        }
    }
}

fn draw_menu_row(out: &mut RenderOut, label: &str, detail: &str, col: usize, selected: bool) {
    let _ = out.queue(Print("  "));
    if selected {
        let _ = out.queue(SetForegroundColor(theme::accent()));
        let _ = out.queue(Print(label));
        let _ = out.queue(ResetColor);
    } else {
        let _ = out.queue(SetAttribute(Attribute::Dim));
        let _ = out.queue(Print(label));
        let _ = out.queue(SetAttribute(Attribute::Reset));
    }
    let padding = " ".repeat(col.saturating_sub(label.len()));
    let _ = out.queue(SetAttribute(Attribute::Dim));
    let _ = out.queue(Print(format!("{}{}", padding, detail)));
    let _ = out.queue(SetAttribute(Attribute::Reset));
    let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
}

/// Heat intensity colors: dim → accent, 4 levels.
const HEAT_COLORS: [Color; 4] = [
    Color::AnsiValue(238), // very dim
    Color::AnsiValue(103), // muted lavender
    Color::AnsiValue(141), // medium lavender
    Color::AnsiValue(147), // bright accent
];
const HEAT_CHAR: &str = "█";
const HEAT_EMPTY: &str = "·";

fn draw_stats(out: &mut RenderOut, lines: &[crate::metrics::StatsLine], max_rows: usize) -> usize {
    use crate::metrics::StatsLine;

    let mut drawn = 0;
    for line in lines {
        if drawn >= max_rows {
            break;
        }
        if drawn > 0 {
            let _ = out.queue(Print("\r\n"));
        }
        match line {
            StatsLine::Kv { label, value } => {
                let _ = out.queue(Print("  "));
                let _ = out.queue(SetAttribute(Attribute::Dim));
                let _ = out.queue(Print(label));
                let _ = out.queue(SetAttribute(Attribute::Reset));
                let padding = " ".repeat(10usize.saturating_sub(label.len()));
                let _ = out.queue(Print(padding));
                let _ = out.queue(Print(value));
            }
            StatsLine::Sub(text) => {
                let _ = out.queue(Print("    "));
                let _ = out.queue(SetAttribute(Attribute::Dim));
                let _ = out.queue(Print(text));
                let _ = out.queue(SetAttribute(Attribute::Reset));
            }
            StatsLine::Heading(text) => {
                let _ = out.queue(Print("  "));
                let _ = out.queue(SetAttribute(Attribute::Dim));
                let _ = out.queue(Print(text));
                let _ = out.queue(SetAttribute(Attribute::Reset));
            }
            StatsLine::Sparkline { bars, legend } => {
                let _ = out.queue(Print("  "));
                let _ = out.queue(SetForegroundColor(theme::accent()));
                let _ = out.queue(Print(bars));
                let _ = out.queue(ResetColor);
                let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
                drawn += 1;
                if drawn < max_rows {
                    let _ = out.queue(Print("\r\n"));
                    let _ = out.queue(Print("  "));
                    let _ = out.queue(SetAttribute(Attribute::Dim));
                    let _ = out.queue(Print(legend));
                    let _ = out.queue(SetAttribute(Attribute::Reset));
                }
            }
            StatsLine::HeatRow { label, cells } => {
                let _ = out.queue(Print("  "));
                let _ = out.queue(SetAttribute(Attribute::Dim));
                let _ = out.queue(Print(format!("{label} ")));
                let _ = out.queue(SetAttribute(Attribute::Reset));
                for cell in cells {
                    match cell {
                        crate::metrics::HeatCell::Empty => {
                            let _ = out.queue(SetForegroundColor(Color::AnsiValue(238)));
                            let _ = out.queue(Print(format!("{HEAT_EMPTY} ")));
                            let _ = out.queue(ResetColor);
                        }
                        crate::metrics::HeatCell::Level(lvl) => {
                            let color = HEAT_COLORS[(*lvl as usize).min(3)];
                            let _ = out.queue(SetForegroundColor(color));
                            let _ = out.queue(Print(format!("{HEAT_CHAR} ")));
                            let _ = out.queue(ResetColor);
                        }
                    }
                }
            }
            StatsLine::Blank => {
                // just an empty line
            }
        }
        let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
        drawn += 1;
    }
    drawn
}
