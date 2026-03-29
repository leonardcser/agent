mod blocks;
mod dialogs;
mod highlight;
mod prompt;
mod working;

use prompt::PromptState;
use working::WorkingState;

pub use dialogs::{
    parse_questions, AgentSnapshot, AgentsDialog, ConfirmDialog, Dialog, DialogResult, HelpDialog,
    PermissionEntry, PermissionsDialog, PsDialog, Question, QuestionDialog, QuestionOption,
    ResumeDialog, RewindDialog, SharedSnapshots,
};

use crate::attachment::{AttachmentId, AttachmentStore};
use crate::input::{InputSnapshot, InputState, MenuKind, ATTACHMENT_MARKER};
use crate::keymap::hints;
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
use std::io::{self, BufWriter, Write};
use std::time::{Duration, Instant};

use blocks::{gap_between, render_block, render_tool, Element};

/// Maximum number of lines to re-render during a full redraw (e.g. purge).
/// Older blocks beyond this limit are dropped to avoid flooding the terminal.
const MAX_REDRAW_LINES: u16 = 2000;

/// Parameters for rendering the prompt section in `draw_frame`.
/// When `None` is passed instead, only content (blocks + active tool) is drawn.
pub struct FramePrompt<'a> {
    pub state: &'a InputState,
    pub mode: protocol::Mode,
    pub queued: &'a [String],
    pub prediction: Option<&'a str>,
}

/// Output wrapper that selects the line-advance strategy.
///
/// * `row: None` — **scroll mode** (blocks / prompt): `\r\n` pushes content
///   into terminal scrollback, which is the normal way conversation renders.
/// * `row: Some(r)` — **overlay mode** (dialogs): `MoveTo(0, r+1)` repositions
///   the cursor without scrolling, so dialogs never pollute scrollback.
pub(super) struct RenderOut {
    pub out: Box<dyn Write>,
    pub row: Option<u16>,
    capture: Option<std::sync::Arc<std::sync::Mutex<Vec<u8>>>>,
}

impl RenderOut {
    /// Create a scroll-mode output (for blocks + prompt).
    /// Dialogs switch to overlay mode by setting `out.row = Some(r)`.
    pub fn scroll() -> Self {
        Self {
            out: Box::new(BufWriter::with_capacity(1 << 16, io::stdout())),
            row: None,
            capture: None,
        }
    }

    /// Create a render output that writes to an in-memory buffer.
    pub fn buffer() -> Self {
        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        Self {
            out: Box::new(SharedWriter(buf.clone())),
            row: None,
            capture: Some(buf),
        }
    }

    /// Extract captured bytes (only valid after `buffer()`).
    pub fn into_bytes(self) -> Vec<u8> {
        drop(self.out);
        self.capture
            .and_then(|arc| std::sync::Arc::try_unwrap(arc).ok())
            .and_then(|m| m.into_inner().ok())
            .unwrap_or_default()
    }
}

struct SharedWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
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
///
/// In overlay mode, Clear is issued on the *current* row (after the
/// content just printed) and then the cursor advances to the next row
/// *without* clearing it.  The next row's stale content is overwritten
/// by the subsequent `Print`.  This avoids a visible blank→content
/// flash on terminals that don't fully support synchronized updates.
pub(super) fn crlf(out: &mut RenderOut) {
    if out.row.is_some() {
        let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
        if let Some(r) = &mut out.row {
            *r += 1;
            let next = *r;
            let _ = out.queue(cursor::MoveTo(0, next));
        }
    } else {
        let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
        let _ = out.queue(Print("\r\n"));
    }
}

pub(super) const SPINNER_FRAMES: &[&str] = &["✿", "❀", "✾", "❁"];

/// A markdown table separator line (e.g. `|---|---|`).
pub(super) fn is_table_separator(line: &str) -> bool {
    let t = line.trim();
    !t.is_empty()
        && t.chars()
            .all(|c| c == '-' || c == '|' || c == ':' || c == ' ')
}

/// Context for rendering content inside a bordered box.
/// When passed to `render_markdown` and its sub-renderers, each output line
/// gets a colored left border prefix and a right border suffix with padding.
pub(super) struct BoxContext {
    /// Left border string printed before each line (e.g. "   │ ").
    pub left: &'static str,
    /// Right border string printed after padding (e.g. " │").
    pub right: &'static str,
    /// Color for the border characters.
    pub color: Color,
    /// Inner content width (between left and right borders).
    pub inner_w: usize,
}

impl BoxContext {
    /// Print the left border with color.
    pub fn print_left(&self, out: &mut RenderOut) {
        let _ = out.queue(SetForegroundColor(self.color));
        let _ = out.queue(Print(self.left));
        let _ = out.queue(ResetColor);
    }

    /// Print right-side padding and border for a line that used `cols` content columns.
    pub fn print_right(&self, out: &mut RenderOut, cols: usize) {
        let pad = self.inner_w.saturating_sub(cols);
        if pad > 0 {
            let _ = out.queue(Print(" ".repeat(pad)));
        }
        let _ = out.queue(SetForegroundColor(self.color));
        let _ = out.queue(Print(self.right));
        let _ = out.queue(ResetColor);
    }
}

fn reasoning_color(effort: protocol::ReasoningEffort) -> Color {
    match effort {
        protocol::ReasoningEffort::Off => theme::reason_off(),
        protocol::ReasoningEffort::Low => theme::REASON_LOW,
        protocol::ReasoningEffort::Medium => theme::REASON_MED,
        protocol::ReasoningEffort::High => theme::REASON_HIGH,
        protocol::ReasoningEffort::Max => theme::REASON_MAX,
    }
}

/// All data needed to show a confirm dialog. Flows unchanged from
/// `EngineEvent::RequestPermission` through `SessionControl`, `DeferredDialog`,
/// `ConfirmContext`, and `ConfirmDialog::new`.
pub struct ConfirmRequest {
    pub call_id: String,
    pub tool_name: String,
    pub desc: String,
    pub args: std::collections::HashMap<String, serde_json::Value>,
    pub approval_patterns: Vec<String>,
    /// Set during dispatch when paths outside the workspace are detected.
    pub outside_dir: Option<std::path::PathBuf>,
    pub summary: Option<String>,
    pub request_id: u64,
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
    pub metadata: Option<serde_json::Value>,
}

pub struct ActiveExec {
    pub command: String,
    pub output: String,
    pub start_time: Instant,
    pub finished: bool,
    pub exit_code: Option<i32>,
}

/// A blocking agent rendered in the dynamic section (like an active tool).
pub struct ActiveAgent {
    pub agent_id: String,
    pub slug: Option<String>,
    pub tool_calls: Vec<crate::app::AgentToolEntry>,
    pub status: AgentBlockStatus,
    pub start_time: Instant,
    /// Frozen elapsed time once the agent finishes.
    pub final_elapsed: Option<Duration>,
}

pub struct ActiveTool {
    pub call_id: String,
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
            "bash" | "web_fetch" | "read_process_output" | "stop_process" | "peek_agent"
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
        /// Bracketed labels for image attachments (e.g. `[screenshot.png]`).
        /// Used to accent-highlight them in the rendered message.
        image_labels: Vec<String>,
    },
    Thinking {
        content: String,
    },
    Text {
        content: String,
    },
    /// A single line of code from a streaming code block.
    CodeLine {
        content: String,
        lang: String,
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
    Exec {
        command: String,
        output: String,
    },
    Compacted {
        summary: String,
    },
    AgentMessage {
        from_id: String,
        from_slug: String,
        content: String,
    },
    /// Inline agent block — shows a spawned subagent's progress.
    Agent {
        agent_id: String,
        slug: Option<String>,
        blocking: bool,
        tool_calls: Vec<crate::app::AgentToolEntry>,
        status: AgentBlockStatus,
        elapsed: Option<Duration>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AgentBlockStatus {
    Running,
    Done,
    Error,
}

#[derive(Clone, Copy, PartialEq)]
pub enum ApprovalScope {
    Session,
    Workspace,
}

#[derive(Clone, PartialEq)]
pub enum ConfirmChoice {
    Yes,
    YesAutoApply,
    No,
    Always(ApprovalScope),
    AlwaysPatterns(Vec<String>, ApprovalScope),
    AlwaysDir(String, ApprovalScope),
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
    /// Cached row count for each block (from its last render).
    row_counts: Vec<u16>,
    flushed: usize,
    last_block_rows: u16,
}

impl BlockHistory {
    fn new() -> Self {
        Self {
            blocks: Vec::new(),
            row_counts: Vec::new(),
            flushed: 0,
            last_block_rows: 0,
        }
    }

    fn push(&mut self, block: Block) {
        self.blocks.push(block);
        self.row_counts.push(0);
    }

    fn has_unflushed(&self) -> bool {
        self.flushed < self.blocks.len()
    }

    fn clear(&mut self) {
        self.blocks.clear();
        self.row_counts.clear();
        self.flushed = 0;
        self.last_block_rows = 0;
    }

    /// Gap (in rows) before the block at `i`, based on adjacency rules.
    fn block_gap(&self, i: usize) -> u16 {
        if i > 0 {
            gap_between(
                &Element::Block(&self.blocks[i - 1]),
                &Element::Block(&self.blocks[i]),
            )
        } else {
            0
        }
    }

    /// Find the earliest block index such that rendering from that index to
    /// the end stays within `max_lines`, using cached row counts.
    fn redraw_start(&self, max_lines: u16) -> usize {
        let mut budget = max_lines;
        let mut start = self.blocks.len();
        for i in (0..self.blocks.len()).rev() {
            let total = self.block_gap(i) + self.row_counts[i];
            if total > budget {
                break;
            }
            budget -= total;
            start = i;
        }
        start
    }

    fn truncate(&mut self, idx: usize) {
        self.blocks.truncate(idx);
        self.row_counts.truncate(idx);
        self.flushed = self.flushed.min(idx);
    }

    /// Render unflushed blocks. Returns total rows printed.
    fn render(&mut self, out: &mut RenderOut, width: usize) -> u16 {
        if !self.has_unflushed() {
            return 0;
        }
        let mut total = 0u16;
        let last_idx = self.blocks.len().saturating_sub(1);
        for i in self.flushed..self.blocks.len() {
            let gap = self.block_gap(i);
            for _ in 0..gap {
                crlf(out);
            }
            let rows = render_block(out, &self.blocks[i], width);
            self.row_counts[i] = rows;
            total += gap + rows;
            if i == last_idx {
                self.last_block_rows = rows + gap;
            }
        }
        self.flushed = self.blocks.len();
        total
    }
}

/// Streaming state for incremental thinking output.
/// Completed lines are committed to block history immediately.
/// Only the current incomplete line lives in the overlay.
struct ActiveThinking {
    current_line: String,
    paragraph: String,
}

/// Streaming state for incremental LLM text output.
/// Completed lines are committed to block history immediately.
/// Only the current incomplete line lives in the overlay.
struct ActiveText {
    current_line: String,
    paragraph: String,
    in_code_block: Option<String>,
    /// Table rows accumulated silently during streaming.
    table_rows: Vec<String>,
    /// Cached count of non-separator data rows (avoids recomputing per frame).
    table_data_rows: usize,
}

pub struct Screen {
    history: BlockHistory,
    active_thinking: Option<ActiveThinking>,
    active_text: Option<ActiveText>,
    active_tools: Vec<ActiveTool>,
    active_agent: Option<ActiveAgent>,
    active_exec: Option<ActiveExec>,
    prompt: PromptState,
    working: WorkingState,
    context_tokens: Option<u32>,
    session_cost_usd: f64,
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
    /// Set when `draw_frame` issues `BeginSynchronizedUpdate` in content-only
    /// mode.  The dialog that follows skips its own `BeginSync`, ensuring a
    /// single atomic sync block covers both the tool overlay and the dialog.
    sync_started: bool,
    running_procs: usize,
    running_agents: usize,
    show_tps: bool,
    show_tokens: bool,
    show_cost: bool,
    show_slug: bool,
    /// Whether to render the active tool above the dialog in content-only
    /// mode.  Set when tool + dialog fit on screen; cleared on dialog close.
    show_tool_in_dialog: bool,
    /// Ephemeral btw side-question state, rendered above the prompt.
    btw: Option<BtwBlock>,
    /// Ephemeral notification shown above the prompt, dismissed on any key.
    notification: Option<Notification>,
    /// Short task label (slug) shown on the status bar after the throbber.
    task_label: Option<String>,
}

/// A short ephemeral notification rendered above the prompt bar.
pub struct Notification {
    pub message: String,
    pub is_error: bool,
}

/// State for an in-flight `/btw` side question.
pub struct BtwBlock {
    pub question: String,
    pub image_labels: Vec<String>,
    pub response: Option<String>,
    /// Cached wrapped lines for scrolling.
    wrapped: Vec<String>,
    scroll_offset: usize,
    /// Terminal width when lines were last wrapped.
    wrap_width: usize,
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
            active_thinking: None,
            active_text: None,
            active_tools: Vec::new(),
            active_agent: None,
            active_exec: None,
            prompt: PromptState::new(),
            working: WorkingState::new(),
            context_tokens: None,
            session_cost_usd: 0.0,
            model_label: None,
            reasoning_effort: Default::default(),
            has_scrollback: false,
            content_start_row: None,
            defer_pending_render: false,
            defer_redraw: false,
            pending_dialog: false,
            sync_started: false,
            running_procs: 0,
            running_agents: 0,
            show_tps: true,
            show_tokens: true,
            show_cost: true,
            show_slug: true,
            show_tool_in_dialog: false,
            btw: None,
            notification: None,
            task_label: None,
        }
    }

    pub fn set_btw(&mut self, question: String, image_labels: Vec<String>) {
        self.btw = Some(BtwBlock {
            question,
            image_labels,
            response: None,
            wrapped: Vec::new(),
            scroll_offset: 0,
            wrap_width: 0,
        });
        self.prompt.dirty = true;
    }

    pub fn set_btw_response(&mut self, content: String) {
        if let Some(ref mut btw) = self.btw {
            btw.response = Some(content);
            btw.wrapped.clear();
            btw.scroll_offset = 0;
            btw.wrap_width = 0;
            self.prompt.dirty = true;
        }
    }

    pub fn dismiss_btw(&mut self) {
        if self.btw.is_some() {
            self.btw = None;
            self.prompt.dirty = true;
        }
    }

    pub fn has_btw(&self) -> bool {
        self.btw.is_some()
    }

    /// Scroll the btw block. Returns true if state changed.
    pub fn btw_scroll(&mut self, delta: isize) -> bool {
        let Some(ref mut btw) = self.btw else {
            return false;
        };
        if btw.wrapped.is_empty() {
            return false;
        }
        let term_h = terminal::size().map(|(_, h)| h).unwrap_or(24) as usize;
        let max_lines = (term_h / 2).saturating_sub(4).max(1);
        let max = btw.wrapped.len().saturating_sub(max_lines);
        let old = btw.scroll_offset;
        if delta < 0 {
            btw.scroll_offset = btw.scroll_offset.saturating_sub((-delta) as usize);
        } else {
            btw.scroll_offset = (btw.scroll_offset + delta as usize).min(max);
        }
        if btw.scroll_offset != old {
            self.prompt.dirty = true;
            true
        } else {
            false
        }
    }

    pub fn notify(&mut self, message: String) {
        self.notification = Some(Notification {
            message,
            is_error: false,
        });
        self.prompt.dirty = true;
    }

    pub fn notify_error(&mut self, message: String) {
        self.notification = Some(Notification {
            message,
            is_error: true,
        });
        self.prompt.dirty = true;
    }

    pub fn dismiss_notification(&mut self) {
        if self.notification.is_some() {
            self.notification = None;
            self.prompt.dirty = true;
        }
    }

    pub fn has_notification(&self) -> bool {
        self.notification.is_some()
    }

    pub fn set_show_tps(&mut self, show: bool) {
        self.show_tps = show;
        self.prompt.dirty = true;
    }

    pub fn set_show_tokens(&mut self, show: bool) {
        self.show_tokens = show;
        self.prompt.dirty = true;
    }

    pub fn set_show_cost(&mut self, show: bool) {
        self.show_cost = show;
        self.prompt.dirty = true;
    }

    pub fn set_show_slug(&mut self, show: bool) {
        self.show_slug = show;
        self.prompt.dirty = true;
    }

    pub fn set_running_procs(&mut self, count: usize) {
        if count != self.running_procs {
            self.running_procs = count;
            self.prompt.dirty = true;
        }
    }

    pub fn set_agent_count(&mut self, count: usize) {
        if count != self.running_agents {
            self.running_agents = count;
            self.prompt.dirty = true;
        }
    }

    /// Start tracking a blocking agent in the dynamic section.
    pub fn start_active_agent(&mut self, agent_id: String) {
        self.active_agent = Some(ActiveAgent {
            agent_id,
            slug: None,
            tool_calls: Vec::new(),
            status: AgentBlockStatus::Running,
            start_time: Instant::now(),
            final_elapsed: None,
        });
        self.prompt.dirty = true;
    }

    /// Update the active blocking agent's state.
    pub fn update_active_agent(
        &mut self,
        slug: Option<&str>,
        tool_calls: &[crate::app::AgentToolEntry],
        status: AgentBlockStatus,
    ) {
        if let Some(ref mut agent) = self.active_agent {
            agent.slug = slug.map(str::to_string);
            agent.tool_calls = tool_calls.to_vec();
            if status != AgentBlockStatus::Running && agent.status == AgentBlockStatus::Running {
                // Freeze the timer on completion.
                agent.final_elapsed = Some(agent.start_time.elapsed());
            }
            agent.status = status;
            self.prompt.dirty = true;
        }
    }

    /// Mark the active agent as cancelled/error (before flush commits it).
    pub fn cancel_active_agent(&mut self) {
        if let Some(ref mut agent) = self.active_agent {
            agent.status = AgentBlockStatus::Error;
            agent.final_elapsed = Some(agent.start_time.elapsed());
        }
    }

    /// Commit the active agent to history and clear the dynamic slot.
    pub fn finish_active_agent(&mut self) {
        if let Some(mut agent) = self.active_agent.take() {
            // If still marked Running, the tool returned successfully —
            // the subagent's TurnComplete may not have been drained yet.
            if agent.status == AgentBlockStatus::Running {
                agent.status = AgentBlockStatus::Done;
                agent.final_elapsed = Some(agent.start_time.elapsed());
            }
            let elapsed = agent
                .final_elapsed
                .unwrap_or_else(|| agent.start_time.elapsed());
            self.history.push(Block::Agent {
                agent_id: agent.agent_id,
                slug: agent.slug,
                blocking: true,
                tool_calls: agent.tool_calls,
                status: agent.status,
                elapsed: Some(elapsed),
            });
            self.prompt.dirty = true;
        }
    }

    /// Whether to show the active tool above a dialog overlay.
    pub fn set_show_tool_in_dialog(&mut self, show: bool) {
        self.show_tool_in_dialog = show;
        self.prompt.dirty = true;
    }

    /// Row where a dialog should start rendering (lines up with the prompt bar).
    pub fn dialog_row(&self) -> u16 {
        self.prompt.prev_dialog_row.unwrap_or(0)
    }

    /// Returns true and resets the flag if `draw_frame` already issued
    /// `BeginSynchronizedUpdate` for this frame (content-only mode).
    pub fn take_sync_started(&mut self) -> bool {
        std::mem::take(&mut self.sync_started)
    }

    /// After a dialog draws (and potentially ScrollUp's), reconcile the
    /// screen's anchor with the dialog's actual position so subsequent
    /// `draw_frame` calls render the active tool at the correct row.
    pub fn sync_dialog_anchor(&mut self, actual: Option<u16>) {
        let Some(actual) = actual else { return };
        let expected = self.prompt.prev_dialog_row.unwrap_or(actual);
        if actual >= expected {
            return;
        }
        let deficit = expected - actual;
        if let Some(ref mut a) = self.prompt.anchor_row {
            *a = a.saturating_sub(deficit);
        }
        self.prompt.prev_dialog_row = Some(actual);
    }

    /// Dismiss a dialog overlay.
    ///
    /// Clears from the dialog's anchor row down and lets the prompt redraw
    /// at that position on the next tick.
    pub fn clear_dialog_area(&mut self, dialog_anchor: Option<u16>) {
        let anchor = dialog_anchor.unwrap_or(0);
        let screen_anchor = self.prompt.anchor_row.unwrap_or(anchor);

        // Account for lines the dialog's ScrollUp pushed content upward.
        // `prev_dialog_row` is where the dialog was *expected* to start;
        // `anchor` is where it *actually* rendered (post-scroll).  The
        // difference is the number of rows everything was shifted up.
        let expected = self.prompt.prev_dialog_row.unwrap_or(anchor);
        let scroll_deficit = expected.saturating_sub(anchor);
        let adjusted_anchor = screen_anchor.saturating_sub(scroll_deficit);

        let clear_from = anchor.min(adjusted_anchor);

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
        self.show_tool_in_dialog = false;
        if scroll_deficit > 0 {
            if let Some(ref mut cs) = self.content_start_row {
                *cs = cs.saturating_sub(scroll_deficit);
            }
        }
        self.prompt.anchor_row = Some(clear_from);
        self.prompt.drawn = false;
        self.prompt.dirty = true;
        self.prompt.prev_rows = 0;
        self.prompt.prev_dialog_row = None;
    }

    /// Move the cursor to the line after the prompt so the shell resumes cleanly.
    /// When `clear_below` is true, clears remaining rows (completions).
    pub fn move_cursor_past_prompt(&self, clear_below: bool) {
        if !self.prompt.drawn {
            return;
        }
        let anchor = self.prompt.anchor_row.unwrap_or(0);
        let end_row = anchor + self.prompt.prev_rows;
        let height = terminal::size().map(|(_, h)| h).unwrap_or(24);
        let mut out = RenderOut::scroll();
        let _ = out.queue(cursor::MoveTo(0, end_row.min(height.saturating_sub(1))));
        let _ = out.queue(Print("\r\n\r\n"));
        if clear_below {
            let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));
        }
        let _ = out.flush();
    }

    pub fn begin_turn(&mut self) {
        self.history.last_block_rows = 0;
        self.active_tools.clear();
    }

    pub fn push(&mut self, block: Block) {
        let block = match block {
            Block::Text { content } => {
                let t = content.trim();
                if t.is_empty() {
                    return;
                }
                Block::Text {
                    content: t.to_string(),
                }
            }
            Block::AgentMessage {
                from_id,
                from_slug,
                content,
            } => {
                let t = content.trim();
                if t.is_empty() {
                    return;
                }
                Block::AgentMessage {
                    from_id,
                    from_slug,
                    content: t.to_string(),
                }
            }
            Block::Thinking { content } => {
                let t = content.trim();
                if t.is_empty() {
                    return;
                }
                Block::Thinking {
                    content: t.to_string(),
                }
            }
            Block::Compacted { summary } => {
                let t = summary.trim();
                if t.is_empty() {
                    return;
                }
                Block::Compacted {
                    summary: t.to_string(),
                }
            }
            other => other,
        };
        self.history.push(block);
        self.prompt.dirty = true;
    }

    // ── Streaming thinking ────────────────────────────────────────────

    pub fn append_streaming_thinking(&mut self, delta: &str) {
        let at = self.active_thinking.get_or_insert_with(|| ActiveThinking {
            current_line: String::new(),
            paragraph: String::new(),
        });

        for ch in delta.chars() {
            if ch == '\r' {
                continue;
            }
            if ch == '\n' {
                let line = std::mem::take(&mut at.current_line);
                if line.trim().is_empty() && !at.paragraph.is_empty() {
                    // Blank line — commit the paragraph.
                    // Include the trailing newline so it renders as visual spacing.
                    at.paragraph.push('\n');
                    let para = std::mem::take(&mut at.paragraph);
                    self.history.push(Block::Thinking { content: para });
                } else {
                    if !at.paragraph.is_empty() {
                        at.paragraph.push('\n');
                    }
                    at.paragraph.push_str(&line);
                }
            } else {
                at.current_line.push(ch);
            }
        }
        self.prompt.dirty = true;
    }

    /// Flush remaining thinking content.
    pub fn flush_streaming_thinking(&mut self) {
        if let Some(mut at) = self.active_thinking.take() {
            // Commit any remaining content (paragraph + current line).
            if !at.current_line.is_empty() {
                if !at.paragraph.is_empty() {
                    at.paragraph.push('\n');
                }
                at.paragraph.push_str(&at.current_line);
            }
            let trimmed = at.paragraph.trim();
            if !trimmed.is_empty() {
                self.history.push(Block::Thinking {
                    content: trimmed.to_string(),
                });
            }
            self.prompt.dirty = true;
        }
    }

    // ── Streaming text ─────────────────────────────────────────────────

    pub fn append_streaming_text(&mut self, delta: &str) {
        // Text starting means thinking is done — commit remaining thinking.
        if self.active_thinking.is_some() {
            self.flush_streaming_thinking();
        }

        let at = self.active_text.get_or_insert_with(|| ActiveText {
            current_line: String::new(),
            paragraph: String::new(),
            in_code_block: None,
            table_rows: Vec::new(),
            table_data_rows: 0,
        });

        for ch in delta.chars() {
            if ch == '\r' {
                continue; // Strip \r (CRLF → LF)
            }
            if ch == '\n' {
                let line = std::mem::take(&mut at.current_line);
                Self::process_text_line(&mut self.history, at, &line);
            } else {
                at.current_line.push(ch);
            }
        }
        self.prompt.dirty = true;
    }

    /// Process a completed line of streaming text.
    fn process_text_line(history: &mut BlockHistory, at: &mut ActiveText, line: &str) {
        let trimmed = line.trim_start();

        // ── Code fence detection ────────────────────────────────────────
        if trimmed.starts_with("```") {
            if at.in_code_block.is_some() {
                // Closing fence — individual code lines were already committed.
                at.in_code_block = None;
                return;
            } else {
                // Opening fence — commit pending text/table.
                Self::commit_paragraph(history, at);
                Self::commit_table(history, at);
                let lang = trimmed.trim_start_matches('`').trim().to_string();
                at.in_code_block = Some(lang);
                return;
            }
        }

        // ── Inside a code block ─────────────────────────────────────────
        if let Some(ref lang) = at.in_code_block {
            history.push(Block::CodeLine {
                content: line.to_string(),
                lang: lang.clone(),
            });
            return;
        }

        // ── Table row — accumulate silently ────────────────────────────
        if trimmed.starts_with('|') {
            Self::commit_paragraph(history, at);
            if !is_table_separator(line) {
                at.table_data_rows += 1;
            }
            at.table_rows.push(line.to_string());
            return;
        }

        // ── Blank line ───────────────────────────────────────────────────
        if line.trim().is_empty() {
            if !at.table_rows.is_empty() {
                return; // Skip blank lines inside tables.
            }
            if !at.paragraph.is_empty() {
                Self::commit_paragraph(history, at);
            }
            return;
        }

        // ── Non-table line after table — commit the table ────────────────
        Self::commit_table(history, at);

        // ── Regular text line ───────────────────────────────────────────
        if !at.paragraph.is_empty() {
            at.paragraph.push('\n');
        }
        at.paragraph.push_str(line);
    }

    fn commit_table(history: &mut BlockHistory, at: &mut ActiveText) {
        if !at.table_rows.is_empty() {
            let content = std::mem::take(&mut at.table_rows).join("\n");
            history.push(Block::Text { content });
            at.table_data_rows = 0;
        }
    }

    fn commit_paragraph(history: &mut BlockHistory, at: &mut ActiveText) {
        let para = std::mem::take(&mut at.paragraph);
        let trimmed = para.trim();
        if !trimmed.is_empty() {
            history.push(Block::Text {
                content: trimmed.to_string(),
            });
        }
    }

    /// Flush remaining streaming text.
    pub fn flush_streaming_text(&mut self) {
        self.flush_streaming_thinking();
        if let Some(mut at) = self.active_text.take() {
            // If inside an unclosed code block, commit current_line as a code line.
            if let Some(ref lang) = at.in_code_block {
                if !at.current_line.is_empty() {
                    self.history.push(Block::CodeLine {
                        content: std::mem::take(&mut at.current_line),
                        lang: lang.clone(),
                    });
                }
                at.in_code_block = None;
            }
            // If current_line is a table row, add it to the table.
            if !at.current_line.is_empty() && at.current_line.trim_start().starts_with('|') {
                at.table_rows.push(std::mem::take(&mut at.current_line));
            }
            Self::commit_table(&mut self.history, &mut at);
            // Commit remaining paragraph + current line.
            if !at.current_line.is_empty() {
                if !at.paragraph.is_empty() {
                    at.paragraph.push('\n');
                }
                at.paragraph.push_str(&at.current_line);
            }
            Self::commit_paragraph(&mut self.history, &mut at);
            self.prompt.dirty = true;
        }
    }

    pub fn start_tool(
        &mut self,
        call_id: String,
        name: String,
        summary: String,
        args: HashMap<String, serde_json::Value>,
    ) {
        self.active_tools.push(ActiveTool {
            call_id,
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

    pub fn start_exec(&mut self, command: String) {
        self.active_exec = Some(ActiveExec {
            command,
            output: String::new(),
            start_time: Instant::now(),
            finished: false,
            exit_code: None,
        });
        self.prompt.dirty = true;
    }

    pub fn append_exec_output(&mut self, chunk: &str) {
        if let Some(ref mut exec) = self.active_exec {
            if !exec.output.is_empty() && !exec.output.ends_with('\n') {
                exec.output.push('\n');
            }
            exec.output.push_str(chunk);
            self.prompt.dirty = true;
        }
    }

    pub fn finish_exec(&mut self, exit_code: Option<i32>) {
        if let Some(ref mut exec) = self.active_exec {
            exec.finished = true;
            exec.exit_code = exit_code;
            self.prompt.dirty = true;
        }
    }

    /// Commit the active exec to block history.
    pub fn commit_exec(&mut self) {
        if let Some(exec) = self.active_exec.take() {
            let mut output = exec.output;
            output.truncate(output.trim_end().len());
            self.history.push(Block::Exec {
                command: exec.command,
                output,
            });
            self.prompt.dirty = true;
        }
    }

    pub fn has_active_exec(&self) -> bool {
        self.active_exec.is_some()
    }

    /// Index of an active tool by call_id. Empty call_id (e.g.
    /// ask_user_question) falls back to the last active tool.
    fn active_tool_index(&self, call_id: &str) -> Option<usize> {
        if call_id.is_empty() {
            self.active_tools.len().checked_sub(1)
        } else {
            self.active_tools.iter().position(|t| t.call_id == call_id)
        }
    }

    fn active_tool_mut(&mut self, call_id: &str) -> Option<&mut ActiveTool> {
        let idx = self.active_tool_index(call_id)?;
        Some(&mut self.active_tools[idx])
    }

    pub fn append_active_output(&mut self, call_id: &str, chunk: &str) {
        if let Some(tool) = self.active_tool_mut(call_id) {
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
                        metadata: None,
                    });
                }
            }
            self.prompt.dirty = true;
        } else if let Some(Block::ToolCall { ref mut output, .. }) = self.last_tool_block_mut() {
            match output {
                Some(ref mut out) => {
                    if !out.content.is_empty() {
                        out.content.push('\n');
                    }
                    out.content.push_str(chunk);
                }
                None => {
                    *output = Some(ToolOutput {
                        content: chunk.to_string(),
                        is_error: false,
                        metadata: None,
                    });
                }
            }
        }
    }

    pub fn set_active_status(&mut self, call_id: &str, status: ToolStatus) {
        if let Some(tool) = self.active_tool_mut(call_id) {
            // Reset timer when transitioning from confirm → pending (user approved)
            if tool.status == ToolStatus::Confirm && status == ToolStatus::Pending {
                tool.start_time = Instant::now();
            }
            tool.status = status;
            self.prompt.dirty = true;
        } else if let Some(Block::ToolCall {
            status: ref mut s, ..
        }) = self.last_tool_block_mut()
        {
            *s = status;
        }
    }

    pub fn set_active_user_message(&mut self, call_id: &str, msg: String) {
        if let Some(tool) = self.active_tool_mut(call_id) {
            tool.user_message = Some(msg);
            self.prompt.dirty = true;
        } else if let Some(Block::ToolCall {
            ref mut user_message,
            ..
        }) = self.last_tool_block_mut()
        {
            *user_message = Some(msg);
        }
    }

    pub fn finish_tool(
        &mut self,
        call_id: &str,
        status: ToolStatus,
        output: Option<ToolOutput>,
        engine_elapsed: Option<Duration>,
    ) {
        if let Some(idx) = self.active_tool_index(call_id) {
            let tool = self.active_tools.remove(idx);
            let elapsed = if status == ToolStatus::Denied {
                None
            } else {
                engine_elapsed.or_else(|| tool.elapsed())
            };
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
        } else if let Some(Block::ToolCall {
            status: ref mut s,
            output: ref mut o,
            ..
        }) = self.last_tool_block_mut()
        {
            *s = status;
            *o = output;
        }
    }

    pub fn set_context_tokens(&mut self, tokens: u32) {
        self.context_tokens = Some(tokens);
        self.prompt.dirty = true;
    }

    /// Rows the active tools would occupy if rendered (including gaps).
    pub fn active_tool_rows(&self) -> u16 {
        if self.active_tools.is_empty() {
            return 0;
        }
        let gap = if let Some(last) = self.history.blocks.last() {
            blocks::gap_between(&blocks::Element::Block(last), &blocks::Element::ActiveTool)
        } else {
            0
        };
        let w = crossterm::terminal::size()
            .map(|(w, _)| w as usize)
            .unwrap_or(80);
        let inter_tool_gap =
            blocks::gap_between(&blocks::Element::ActiveTool, &blocks::Element::ActiveTool);
        let mut total = gap;
        for (i, tool) in self.active_tools.iter().enumerate() {
            if i > 0 {
                total += inter_tool_gap;
            }
            // At confirm time there's no output yet, so tool rows = 1 + optional web_fetch prompt
            let mut rows = 1u16;
            if tool.name == "web_fetch" {
                if let Some(prompt) = tool.args.get("prompt").and_then(|v| v.as_str()) {
                    rows += wrap_line(prompt, w.saturating_sub(4)).len() as u16;
                }
            }
            total += rows;
        }
        total
    }

    pub fn clear_context_tokens(&mut self) {
        self.context_tokens = None;
        self.prompt.dirty = true;
    }

    pub fn context_tokens(&self) -> Option<u32> {
        self.context_tokens
    }

    pub fn set_session_cost(&mut self, usd: f64) {
        self.session_cost_usd = usd;
        self.prompt.dirty = true;
    }

    pub fn set_model_label(&mut self, label: String) {
        self.model_label = Some(label);
        self.prompt.dirty = true;
    }

    pub fn set_task_label(&mut self, label: String) {
        if label.trim().is_empty() {
            self.task_label = None;
        } else {
            self.task_label = Some(label);
        }
        self.prompt.dirty = true;
    }

    pub fn clear_task_label(&mut self) {
        self.task_label = None;
        self.prompt.dirty = true;
    }

    pub fn set_reasoning_effort(&mut self, effort: protocol::ReasoningEffort) {
        self.reasoning_effort = effort;
        self.prompt.dirty = true;
    }

    pub fn working_throbber(&self) -> Option<Throbber> {
        self.working.throbber
    }

    pub fn set_throbber(&mut self, state: Throbber) {
        self.working.set_throbber(state);
        self.prompt.dirty = true;
    }

    pub fn record_tokens_per_sec(&mut self, tps: f64) {
        self.working.record_tokens_per_sec(tps);
        self.prompt.dirty = true;
    }

    pub fn turn_meta(&self) -> Option<protocol::TurnMeta> {
        self.working.turn_meta()
    }

    pub fn restore_from_turn_meta(&mut self, meta: &protocol::TurnMeta) {
        self.working.restore_from_turn_meta(meta);
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

    /// Center the input viewport on the cursor (vim `zz`).
    pub fn center_input_scroll(&mut self) {
        // The actual centering happens in draw_prompt_sections using a
        // sentinel value. We set input_scroll to usize::MAX so the
        // scroll logic knows to center instead of preserving position.
        self.prompt.input_scroll = usize::MAX;
        self.prompt.dirty = true;
    }

    /// Convert active tools to history blocks and render any pending blocks.
    pub fn flush_blocks(&mut self) {
        let _perf = crate::perf::begin("flush_blocks");
        self.commit_active_tools();
        self.render_pending_blocks();
    }

    /// Convert all active tools to history blocks without rendering.
    /// The blocks remain unflushed so that `draw_frame(None)` will render
    /// them (along with any preceding reasoning blocks) before the dialog
    /// paints on top.
    pub fn commit_active_tools(&mut self) {
        self.commit_active_tools_as(ToolStatus::Err);
    }

    pub fn commit_active_tools_as(&mut self, status: ToolStatus) {
        self.finish_active_agent();
        for tool in self.active_tools.drain(..) {
            let elapsed = if status == ToolStatus::Denied {
                None
            } else {
                tool.elapsed()
            };
            self.history.push(Block::ToolCall {
                name: tool.name,
                summary: tool.summary,
                args: tool.args,
                status,
                elapsed,
                output: tool.output,
                user_message: tool.user_message,
            });
        }
    }

    /// Get a mutable reference to the last history block if it's a ToolCall.
    /// Updates data only — does NOT change flushed/anchor_row so there is no
    /// risk of duplicate scroll-mode renders.
    fn last_tool_block_mut(&mut self) -> Option<&mut Block> {
        let idx = self.history.blocks.len().checked_sub(1)?;
        if matches!(self.history.blocks[idx], Block::ToolCall { .. }) {
            Some(&mut self.history.blocks[idx])
        } else {
            None
        }
    }

    /// Whether any content (blocks, active tool, active exec) exists above
    /// the prompt.  Used to decide whether to emit a 1-line gap before the
    /// prompt bar.
    fn has_content(&self) -> bool {
        !self.history.blocks.is_empty()
            || self.active_thinking.is_some()
            || self.active_text.is_some()
            || !self.active_tools.is_empty()
            || self.active_agent.is_some()
            || self.active_exec.is_some()
    }

    pub fn render_pending_blocks(&mut self) {
        self.render_pending_blocks_inner(true);
    }

    /// Render pending blocks but leave the synchronized update open so
    /// that subsequent rendering (tool overlay + dialog) is part of the
    /// same atomic frame.  The caller is responsible for eventually
    /// issuing `EndSynchronizedUpdate`.
    pub fn render_pending_blocks_for_dialog(&mut self) {
        self.render_pending_blocks_inner(false);
    }

    fn render_pending_blocks_inner(&mut self, close_sync: bool) {
        if self.defer_pending_render {
            self.defer_pending_render = false;
            return;
        }
        if !self.history.has_unflushed() {
            return;
        }
        let mut out = RenderOut::scroll();
        let _ = out.queue(terminal::BeginSynchronizedUpdate);
        let _ = out.queue(cursor::Hide);
        let start_row = if self.prompt.drawn {
            let row = self.prompt.anchor_row.unwrap_or(0);
            let _ = out.queue(cursor::MoveTo(0, row));
            let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));
            self.prompt.drawn = false;
            self.prompt.prev_rows = 0;
            row
        } else {
            self.prompt
                .anchor_row
                .take()
                .unwrap_or_else(|| cursor::position().map(|(_, y)| y).unwrap_or(0))
        };
        let (w, h) = terminal::size().unwrap_or((80, 24));
        let block_rows = self.history.render(&mut out, w as usize);
        // Cap anchor at the last terminal row — scroll-mode rendering may
        // have pushed past the bottom, making start_row + block_rows overshoot.
        self.prompt.anchor_row = Some((start_row + block_rows).min(h.saturating_sub(1)));
        if close_sync {
            let _ = out.queue(terminal::EndSynchronizedUpdate);
        } else {
            self.sync_started = true;
        }
        let _ = out.flush();
    }

    /// Mark the prompt as needing a full redraw.  Does NOT perform any
    /// terminal I/O — the next `draw_frame` will clear stale rows and
    /// repaint atomically within a single synchronized-update frame,
    /// preventing the flash that occurred when erasure was flushed as a
    /// separate frame.
    pub fn erase_prompt(&mut self) {
        if self.prompt.drawn {
            self.prompt.drawn = false;
            self.prompt.dirty = true;
        }
    }

    /// Erase the prompt area without issuing its own sync frame.
    /// Used when a sync is already open (e.g. from
    /// `render_pending_blocks_for_dialog`) and the caller needs the
    /// terminal lines cleared immediately within that frame.
    pub fn erase_prompt_nosync(&mut self) {
        if self.prompt.drawn {
            if let Some(anchor) = self.prompt.anchor_row {
                let height = terminal::size().map(|(_, h)| h).unwrap_or(24);
                let end = (anchor + self.prompt.prev_rows).min(height);
                let mut out = RenderOut::scroll();
                let _ = out.queue(cursor::Hide);
                for r in anchor..end {
                    let _ = out.queue(cursor::MoveTo(0, r));
                    let _ = out.queue(terminal::Clear(terminal::ClearType::CurrentLine));
                }
                let _ = out.queue(cursor::MoveTo(0, anchor));
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
        let mut out = RenderOut::scroll();
        let _ = out.queue(terminal::BeginSynchronizedUpdate);
        let _ = out.queue(cursor::Hide);
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
        let (w, height) = terminal::size().unwrap_or((80, 24));
        let start_idx = self.history.redraw_start(MAX_REDRAW_LINES);
        self.history.flushed = start_idx;
        self.history.last_block_rows = 0;
        let block_rows = self.history.render(&mut out, w as usize);
        if !purge {
            // Clear remaining rows individually — Clear(FromCursorDown) at
            // low row numbers causes Ghostty to push the viewport into
            // scrollback.
            let cur_row = start + block_rows;
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
            self.prompt.anchor_row = Some(block_rows.min(height.saturating_sub(1)));
        } else {
            self.prompt.anchor_row = Some((start + block_rows).min(height.saturating_sub(1)));
        }
    }

    pub fn clear(&mut self) {
        self.history.clear();
        self.active_thinking = None;
        self.active_text = None;
        self.active_tools.clear();
        self.active_agent = None;
        self.active_exec = None;
        self.prompt = PromptState::new();
        self.prompt.anchor_row = Some(0);
        self.working.clear();
        self.context_tokens = None;
        self.session_cost_usd = 0.0;
        self.task_label = None;
        self.has_scrollback = false;
        self.content_start_row = None;
        let mut out = RenderOut::scroll();
        let _ = out.queue(terminal::BeginSynchronizedUpdate);
        let _ = out.queue(cursor::Hide);
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
                if let Block::User { text, .. } = b {
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
        self.active_tools.clear();
        self.active_agent = None;
        self.redraw(true);
    }

    pub fn draw_prompt(&mut self, state: &InputState, mode: protocol::Mode, width: usize) {
        self.draw_frame(
            width,
            Some(FramePrompt {
                state,
                mode,
                queued: &[],
                prediction: None,
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

        // Content-only (dialog overlay): only render when new blocks arrived
        // or when the active tool should be shown and has changes.
        if is_dialog && !has_new_blocks && !(self.show_tool_in_dialog && self.prompt.dirty) {
            return false;
        }
        // Full mode: skip if nothing changed.
        if !is_dialog && !has_new_blocks && !self.prompt.dirty {
            return false;
        }

        let mut out = RenderOut::scroll();

        // ── Position cursor ─────────────────────────────────────────────
        let explicit_anchor = self.prompt.anchor_row.take();
        let draw_start_row =
            explicit_anchor.unwrap_or_else(|| cursor::position().map(|(_, y)| y).unwrap_or(0));

        // In content-only mode the sync frame may already be open (from
        // render_pending_blocks_for_dialog).  Only issue BeginSync when
        // one hasn't been started yet.  The dialog that follows will
        // close the frame with EndSync, so blocks + tool + dialog are
        // painted as a single atomic update.
        if !self.sync_started {
            let _ = out.queue(terminal::BeginSynchronizedUpdate);
        }
        if is_dialog {
            self.sync_started = true;
        }
        let _ = out.queue(cursor::Hide);
        // Reposition when the prompt was previously drawn (incremental
        // update) OR when an explicit anchor was set (e.g. after
        // redraw/clear/rewind where the cursor may not match the anchor).
        if self.prompt.drawn || explicit_anchor.is_some() {
            let _ = out.queue(cursor::MoveTo(0, draw_start_row));
        }
        if is_dialog {
            out.row = Some(draw_start_row);
        }

        // ── Render blocks ───────────────────────────────────────────────
        let block_rows = self.history.render(&mut out, width);

        // ── Clear stale volatile area ────────────────────────────────────
        // When new blocks are committed (block_rows > 0), the overlay
        // shrinks and previous frame's streaming/prompt content lingers.
        // Clear everything below the new block content so the overlay and
        // prompt render into clean space.  With synchronized updates this
        // is invisible.
        if block_rows > 0 && self.prompt.prev_rows > 0 {
            let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));
            // Area below is now clean — reset prev_rows so draw_prompt_sections
            // doesn't try to clear already-cleared rows (which would add extra
            // blank lines).
            self.prompt.prev_rows = 0;
        }

        // ── Render streaming overlay ─────────────────────────────────────
        // Only the current incomplete line lives here (tables and completed
        // lines are committed to block history immediately).
        let mut streaming_rows: u16 = 0;

        let mut overlay_blocks: Vec<Block> = Vec::new();

        // Current thinking line (incomplete — no \n yet).
        if let Some(ref at) = self.active_thinking {
            let text = match (at.paragraph.is_empty(), at.current_line.is_empty()) {
                (true, true) => String::new(),
                (true, false) => at.current_line.clone(),
                (false, true) => at.paragraph.clone(),
                (false, false) => format!("{}\n{}", at.paragraph, at.current_line),
            };
            if !text.trim().is_empty() {
                overlay_blocks.push(Block::Thinking { content: text });
            }
        }

        // Current text overlay.
        if let Some(ref at) = self.active_text {
            let in_table =
                !at.table_rows.is_empty() || at.current_line.trim_start().starts_with('|');

            if in_table {
                let n = at.table_data_rows;
                let dot_count = (std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .subsec_millis()
                    / 333) as usize
                    % 3
                    + 1;
                let dots = &"..."[..dot_count];
                overlay_blocks.push(Block::Hint {
                    content: format!(" building table ({n} rows){dots}"),
                });
            } else if at.in_code_block.is_some() && !at.current_line.is_empty() {
                let lang = at.in_code_block.as_deref().unwrap_or("").to_string();
                overlay_blocks.push(Block::CodeLine {
                    content: at.current_line.clone(),
                    lang,
                });
            } else {
                let mut overlay_content = String::new();
                if !at.paragraph.is_empty() {
                    overlay_content.push_str(&at.paragraph);
                }
                if !at.current_line.is_empty() {
                    if !overlay_content.is_empty() {
                        overlay_content.push('\n');
                    }
                    overlay_content.push_str(&at.current_line);
                }
                if !overlay_content.trim().is_empty() {
                    overlay_blocks.push(Block::Text {
                        content: overlay_content,
                    });
                }
            }
        }

        // Render overlay blocks with correct gaps.
        for (i, block) in overlay_blocks.iter().enumerate() {
            let gap = if i == 0 {
                if let Some(last) = self.history.blocks.last() {
                    gap_between(&Element::Block(last), &Element::Block(block))
                } else {
                    0
                }
            } else {
                gap_between(
                    &Element::Block(&overlay_blocks[i - 1]),
                    &Element::Block(block),
                )
            };
            for _ in 0..gap {
                crlf(&mut out);
            }
            let rows = blocks::render_block(&mut out, block, width);
            streaming_rows += gap + rows;
        }

        // ── Render active tools ─────────────────────────────────────────
        let mut active_rows: u16 = 0;
        let show_active = !is_dialog || self.show_tool_in_dialog;
        if show_active {
            for (i, tool) in self.active_tools.iter().enumerate() {
                let tool_gap = if i == 0 {
                    if streaming_rows > 0 {
                        // Streaming text is above — always 1 gap.
                        1
                    } else if let Some(last) = self.history.blocks.last() {
                        gap_between(&Element::Block(last), &Element::ActiveTool)
                    } else {
                        0
                    }
                } else {
                    gap_between(&Element::ActiveTool, &Element::ActiveTool)
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
                active_rows += tool_gap + rows;
            }
        }

        // ── Render active blocking agent ───────────────────────────
        if show_active {
            if let Some(ref agent) = self.active_agent {
                let agent_gap = if !self.active_tools.is_empty() {
                    1
                } else if let Some(last) = self.history.blocks.last() {
                    gap_between(&Element::Block(last), &Element::ActiveTool)
                } else {
                    0
                };
                for _ in 0..agent_gap {
                    crlf(&mut out);
                }
                let elapsed = agent
                    .final_elapsed
                    .unwrap_or_else(|| agent.start_time.elapsed());
                let rows = blocks::render_block(
                    &mut out,
                    &Block::Agent {
                        agent_id: agent.agent_id.clone(),
                        slug: agent.slug.clone(),
                        blocking: true,
                        tool_calls: agent.tool_calls.clone(),
                        status: agent.status,
                        elapsed: Some(elapsed),
                    },
                    width,
                );
                active_rows += agent_gap + rows;
            }
        }

        // ── Render active exec ──────────────────────────────────────
        if show_active {
            if let Some(ref exec) = self.active_exec {
                let exec_gap = if self.active_agent.is_some() || !self.active_tools.is_empty() {
                    1
                } else if let Some(last) = self.history.blocks.last() {
                    gap_between(&Element::Block(last), &Element::ActiveExec)
                } else {
                    0
                };
                for _ in 0..exec_gap {
                    crlf(&mut out);
                }
                let rows = blocks::render_active_exec(&mut out, exec, width);
                active_rows += exec_gap + rows;
            }
        }

        if let Some(p) = prompt {
            // ── Full mode: render prompt ────────────────────────────────
            // Emit a single blank-line gap when there is any content above
            // the prompt.  anchor_row always points to the end of content
            // (never includes the gap), so we emit it unconditionally here.
            let gap: u16 = if self.has_content() { 1 } else { 0 };
            for _ in 0..gap {
                crlf(&mut out);
            }

            let pre_prompt = block_rows + streaming_rows + active_rows + gap;
            let (top_row, new_rows, scrolled) = self.draw_prompt_sections(
                &mut out,
                p.state,
                p.mode,
                width,
                p.queued,
                p.prediction,
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

            // anchor_row: where the next frame starts drawing.  Points to
            // the end of flushed block content — the gap is always emitted
            // fresh by draw_frame, never baked into anchor_row.
            let prompt_section_rows = streaming_rows + active_rows + gap + new_rows;
            if scrolled {
                let height = terminal::size().map(|(_, h)| h).unwrap_or(24);
                self.prompt.anchor_row = Some(height.saturating_sub(prompt_section_rows));
            } else {
                self.prompt.anchor_row = Some(top_row + block_rows);
            }
            // prev_dialog_row: where the prompt bar actually starts (after active
            // tool + gap).  Dialogs render here to line up with the prompt.
            let anchor = self.prompt.anchor_row.unwrap_or(0);
            self.prompt.prev_dialog_row = Some(anchor + streaming_rows + active_rows + gap);
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
            let gap: u16 = if block_rows > 0 || streaming_rows > 0 || active_rows > 0 {
                // Clear the gap row (stale prompt content may linger) and
                // advance past it.  crlf no longer clears the next row, so
                // we handle it explicitly here.  The dialog bar row (after
                // the gap) is left untouched — the dialog overwrites it.
                if out.row.is_some() {
                    let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
                    *out.row.as_mut().unwrap() += 1;
                }
                1
            } else {
                0
            };

            let content_rows = block_rows + streaming_rows + active_rows + gap;
            let height = terminal::size().map(|(_, h)| h).unwrap_or(24);
            let scrolled = draw_start_row + content_rows > height;

            let anchor = if scrolled {
                self.has_scrollback = true;
                height.saturating_sub(streaming_rows + active_rows + gap)
            } else {
                draw_start_row + block_rows
            };
            self.prompt.anchor_row = Some(anchor);
            self.prompt.prev_dialog_row = Some(anchor + streaming_rows + active_rows + gap);
            self.prompt.prev_rows = streaming_rows + active_rows + gap;
            self.prompt.drawn = true;
            self.prompt.dirty = false;

            // The BeginSync issued at the top of draw_frame stays open.
            // The dialog's EndSync + flush closes it, so the terminal
            // paints tool overlay + dialog as one atomic frame.
            content_rows > 0
        }
    }

    /// Returns (top_row, total_prompt_rows, scrolled).
    #[allow(clippy::too_many_arguments)]
    fn draw_prompt_sections(
        &mut self,
        out: &mut RenderOut,
        state: &InputState,
        mode: protocol::Mode,
        width: usize,
        queued: &[String],
        prediction: Option<&str>,
        prev_rows: u16,
        draw_start_row: u16,
        pre_prompt_rows: u16,
    ) -> (u16, u16, bool) {
        let _perf = crate::perf::begin("draw_prompt");
        let usable = width.saturating_sub(2);
        let height = terminal::size()
            .map(|(_, h)| h as usize)
            .unwrap_or(24)
            .saturating_sub(pre_prompt_rows as usize);
        let stash_rows = if state.stash.is_some() { 1 } else { 0 };

        let mut extra_rows = render_stash(out, &state.stash, usable, &state.store);
        let queued_visual = render_queued(out, queued, usable);
        extra_rows += queued_visual;
        let queued_rows = queued_visual as usize;
        let btw_visual = if let Some(ref mut btw) = self.btw {
            let term_h = terminal::size().map(|(_, h)| h).unwrap_or(24) as usize;
            // Cap btw to half the terminal height, minus overhead for bar+input.
            let max_btw = (term_h / 2).saturating_sub(4);
            let rows = render_btw(out, btw, usable, max_btw, state.vim_enabled());
            extra_rows += rows;
            rows as usize
        } else {
            0
        };
        let bar_color = theme::bar();

        // Build all bar spans with priorities. draw_bar drops highest
        // priority first until everything fits.
        // Priorities: 0 = always, 1 = context tokens, 2 = model, 3 = tok/s
        let mut throbber_spans = self.working.throbber_spans(self.show_tps);

        if self.show_slug {
            if let Some(ref label) = self.task_label {
                let is_compacting = self.working.throbber == Some(Throbber::Compacting);
                let slug_text = if let Some(spinner) = self.working.spinner_char() {
                    if !throbber_spans.is_empty() {
                        throbber_spans.remove(0);
                    }
                    // Keep "compacting" visible after the tag.
                    if is_compacting {
                        throbber_spans.insert(
                            0,
                            BarSpan {
                                text: " compacting".into(),
                                color: Color::Reset,
                                bg: None,
                                attr: None,
                                priority: 1,
                            },
                        );
                    }
                    format!(" {} {} ", spinner, label)
                } else {
                    format!(" {} ", label)
                };
                throbber_spans.insert(
                    0,
                    BarSpan {
                        text: slug_text,
                        color: Color::Black,
                        bg: Some(theme::slug_color()),
                        attr: None,
                        priority: 1,
                    },
                );
            }
        }

        let mut right_spans = Vec::new();
        if let Some(ref model) = self.model_label {
            right_spans.push(BarSpan {
                text: format!(" {}", model),
                color: theme::muted(),
                bg: None,
                attr: None,
                priority: 2,
            });
            if self.reasoning_effort != protocol::ReasoningEffort::Off {
                let effort = self.reasoning_effort;
                right_spans.push(BarSpan {
                    text: format!(" {}", effort.label()),
                    color: reasoning_color(effort),
                    bg: None,
                    attr: None,
                    priority: 2,
                });
            }
        }
        if self.show_tokens {
            if let Some(tokens) = self.context_tokens {
                if !right_spans.is_empty() {
                    right_spans.push(BarSpan {
                        text: " ·".into(),
                        color: bar_color,
                        bg: None,
                        attr: None,
                        priority: 2,
                    });
                }
                right_spans.push(BarSpan {
                    text: format!(" {}", format_tokens(tokens)),
                    color: theme::muted(),
                    bg: None,
                    attr: None,
                    priority: 1,
                });
            }
        }
        if self.show_cost && self.session_cost_usd > 0.0 {
            if !right_spans.is_empty() {
                right_spans.push(BarSpan {
                    text: " ·".into(),
                    color: bar_color,
                    bg: None,
                    attr: None,
                    priority: 2,
                });
            }
            right_spans.push(BarSpan {
                text: format!(" {}", crate::metrics::format_cost(self.session_cost_usd)),
                color: theme::muted(),
                bg: None,
                attr: None,
                priority: 1,
            });
        }
        if self.pending_dialog {
            if !throbber_spans.is_empty() {
                throbber_spans.push(BarSpan {
                    text: " · ".into(),
                    color: bar_color,
                    bg: None,
                    attr: None,
                    priority: 0,
                });
            }
            throbber_spans.push(BarSpan {
                text: "permission pending".into(),
                color: theme::accent(),
                bg: None,
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

        let spans = build_display_spans(&state.buf, &state.attachment_ids, &state.store);
        let display_buf = spans_to_string(&spans);
        let char_kinds = build_char_kinds(&spans);
        let display_cursor = map_cursor(state.cursor_char(), &state.buf, &spans);
        // Map selection range from raw byte offsets to display character offsets.
        let display_selection = state.selection_range().map(|(start, end)| {
            let raw_start_char = crate::input::char_pos(&state.buf, start);
            let raw_end_char = crate::input::char_pos(&state.buf, end);
            let ds = map_cursor(raw_start_char, &state.buf, &spans);
            let de = map_cursor(raw_end_char, &state.buf, &spans);
            (ds, de)
        });
        let (visual_lines, cursor_line, cursor_col) =
            wrap_and_locate_cursor(&display_buf, &char_kinds, display_cursor, usable);
        let cmd_hint =
            crate::completer::Completer::command_hint(&state.buf, &state.command_arg_sources);
        let has_arg_space = cmd_hint.is_some()
            && state.buf.len() > cmd_hint.as_ref().unwrap().0.len()
            && state.buf.as_bytes()[cmd_hint.as_ref().unwrap().0.len()] == b' ';
        let is_command =
            cmd_hint.is_some() || crate::completer::Completer::is_command(state.buf.trim());
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

        // Reserve space for the status line (always shown when no completions/menus).
        let status_line_reserve: usize = if comp_total == 0 { 1 } else { 0 };

        let fixed_base = stash_rows + queued_rows + 2 + status_line_reserve;
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
        let scroll_offset = if total_content_rows > content_rows {
            // Vim-style viewport: persist scroll across frames, only adjust
            // when the cursor moves outside the visible range.
            let mut off = self.prompt.input_scroll;
            // Sentinel: center viewport on cursor (zz).
            if off == usize::MAX {
                off = cursor_line.saturating_sub(content_rows / 2);
            }
            // Cursor below viewport → scroll down just enough.
            if cursor_line >= off + content_rows {
                off = cursor_line + 1 - content_rows;
            }
            // Cursor above viewport → scroll up just enough.
            if cursor_line < off {
                off = cursor_line;
            }
            // Clamp to valid range.
            let max_off = total_content_rows.saturating_sub(content_rows);
            off = off.min(max_off);
            self.prompt.input_scroll = off;
            off
        } else {
            self.prompt.input_scroll = 0;
            0
        };
        let cursor_line_visible = cursor_line
            .saturating_sub(scroll_offset)
            .min(content_rows.saturating_sub(1));

        // If notification is active and input is empty, render it in the input area.
        let show_notif = self.notification.is_some() && state.buf.is_empty();
        let show_prediction = !show_notif && prediction.is_some() && state.buf.is_empty();
        if show_notif {
            let notif = self.notification.as_ref().unwrap();
            let _ = out.queue(Print(" "));
            if notif.is_error {
                let _ = out.queue(SetForegroundColor(theme::ERROR));
            } else {
                let _ = out.queue(SetAttribute(Attribute::Dim));
            }
            let msg: String = notif
                .message
                .chars()
                .take(usable.saturating_sub(1))
                .collect();
            let _ = out.queue(Print(&msg));
            if notif.is_error {
                let _ = out.queue(ResetColor);
            } else {
                let _ = out.queue(SetAttribute(Attribute::Reset));
            }
            let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
            let _ = out.queue(Print("\r\n"));
        } else if show_prediction {
            let pred = prediction.unwrap();
            let first_line = pred.lines().next().unwrap_or(pred);
            let _ = out.queue(Print(" "));
            let _ = out.queue(SetAttribute(Attribute::Dim));
            let msg: String = first_line.chars().take(usable.saturating_sub(1)).collect();
            let _ = out.queue(Print(&msg));
            let _ = out.queue(SetAttribute(Attribute::Reset));
            let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
            let _ = out.queue(Print("\r\n"));
        }

        // Compute cumulative display-char offset for each visual line.
        // Must match the counting logic in wrap_and_locate_cursor: each
        // visual line contributes its char count, and each '\n' in the
        // display buffer contributes 1 additional char between logical lines.
        let line_char_offsets = compute_visual_line_offsets(&display_buf, &visual_lines);

        for (li, (line, kinds)) in visual_lines
            .iter()
            .skip(scroll_offset)
            .take(if show_notif || show_prediction {
                0
            } else {
                content_rows
            })
            .enumerate()
        {
            let abs_idx = scroll_offset + li;
            // Compute per-line selection range (in char offsets within this line).
            let line_sel = display_selection.and_then(|(sel_start, sel_end)| {
                let line_start = line_char_offsets[abs_idx];
                let line_len = line.chars().count();
                let line_end = line_start + line_len;
                if line_len == 0 && sel_start <= line_start && sel_end > line_start {
                    // Empty line within selection — highlight a phantom space.
                    Some((0, 1))
                } else if sel_end <= line_start || sel_start >= line_end {
                    None
                } else {
                    let s = sel_start.saturating_sub(line_start);
                    let e = sel_end.min(line_end) - line_start;
                    Some((s, e))
                }
            });
            let _ = out.queue(Print(" "));
            if has_arg_space && abs_idx == 0 {
                // Command prefix in accent, argument text in normal style.
                let (prefix, hint) = cmd_hint.as_ref().unwrap();
                let prefix_len = prefix.len();
                let _ = out.queue(SetForegroundColor(theme::accent()));
                if line.len() >= prefix_len {
                    let _ = out.queue(Print(&line[..prefix_len]));
                    let _ = out.queue(ResetColor);
                    let rest = &line[prefix_len..];
                    if rest.trim().is_empty() && state.buf.ends_with(' ') {
                        // Show hint when only "/cmd " typed with no argument yet.
                        // Truncate with ellipsis if it would overflow the line.
                        let max = usable.saturating_sub(prefix_len + 2); // +2 for spaces
                        let truncated: String = if hint.chars().count() > max {
                            let mut s: String = hint.chars().take(max.saturating_sub(1)).collect();
                            s.push('…');
                            s
                        } else {
                            hint.clone()
                        };
                        let _ = out.queue(Print(" "));
                        let _ = out.queue(SetAttribute(Attribute::Dim));
                        let _ = out.queue(Print(&truncated));
                        let _ = out.queue(SetAttribute(Attribute::Reset));
                    } else {
                        let _ = out.queue(Print(rest));
                    }
                } else {
                    let _ = out.queue(Print(line));
                    let _ = out.queue(ResetColor);
                }
            } else if has_arg_space {
                render_styled_chars(out, line, kinds, line_sel);
            } else if is_command {
                // All chars are accent-colored; reuse AtRef kind for accent rendering.
                let accent_kinds = vec![SpanKind::AtRef; line.chars().count()];
                render_styled_chars(out, line, &accent_kinds, line_sel);
            } else if (is_exec || is_exec_invalid) && abs_idx == 0 && line.starts_with('!') {
                // Render the `!` prefix with its own style (possibly selected).
                let bang_selected = line_sel.is_some_and(|(s, _)| s == 0);
                if bang_selected {
                    let _ = out.queue(SetBackgroundColor(theme::selection_bg()));
                }
                let _ = out.queue(SetForegroundColor(Color::Red));
                let _ = out.queue(SetAttribute(Attribute::Bold));
                let _ = out.queue(Print("!"));
                let _ = out.queue(SetAttribute(Attribute::Reset));
                let _ = out.queue(ResetColor);
                // Shift selection range by 1 for the remaining text.
                let rest_sel = line_sel.and_then(|(s, e)| {
                    let s2 = if s == 0 { 0 } else { s - 1 };
                    let e2 = e.saturating_sub(1);
                    if s2 < e2 {
                        Some((s2, e2))
                    } else {
                        None
                    }
                });
                render_styled_chars(out, &line[1..], &kinds[1..], rest_sel);
            } else {
                render_styled_chars(out, line, kinds, line_sel);
            }
            let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
            let _ = out.queue(Print("\r\n"));
        }

        let mode_spans: Vec<BarSpan> = match mode {
            protocol::Mode::Plan => vec![BarSpan {
                text: " plan".into(),
                color: theme::PLAN,
                bg: None,
                attr: None,
                priority: 0,
            }],
            protocol::Mode::Apply => vec![BarSpan {
                text: " apply".into(),
                color: theme::APPLY,
                bg: None,
                attr: None,
                priority: 0,
            }],
            protocol::Mode::Yolo => vec![BarSpan {
                text: " yolo".into(),
                color: theme::YOLO,
                bg: None,
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

        // Status line below the prompt: vim mode + procs + agents.
        // Always shown unless completions or menus are visible.
        let status_line_rows = if comp_rows == 0 {
            let vim_label = vim_mode_label(state.vim_mode());

            let _ = out.queue(Print("\r\n"));
            let _ = out.queue(Print(" "));
            if let Some(label) = vim_label {
                let _ = out.queue(SetAttribute(Attribute::Dim));
                let _ = out.queue(Print(label));
                let _ = out.queue(SetAttribute(Attribute::Reset));
            }
            if self.running_procs > 0 {
                if vim_label.is_some() {
                    let _ = out.queue(Print("  "));
                }
                let _ = out.queue(SetForegroundColor(theme::accent()));
                let label = if self.running_procs == 1 {
                    "1 proc".to_string()
                } else {
                    format!("{} procs", self.running_procs)
                };
                let _ = out.queue(Print(&label));
                let _ = out.queue(ResetColor);
            }
            if self.running_agents > 0 {
                if self.running_procs > 0 {
                    let _ = out.queue(SetAttribute(Attribute::Dim));
                    let _ = out.queue(Print(" · "));
                    let _ = out.queue(SetAttribute(Attribute::NormalIntensity));
                } else if vim_label.is_some() {
                    let _ = out.queue(Print("  "));
                }
                let _ = out.queue(SetForegroundColor(theme::AGENT));
                let label = if self.running_agents == 1 {
                    "1 agent".to_string()
                } else {
                    format!("{} agents", self.running_agents)
                };
                let _ = out.queue(Print(&label));
                let _ = out.queue(ResetColor);
            }
            let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
            1
        } else {
            0
        };

        if comp_rows > 0 {
            let _ = out.queue(Print("\r\n"));
        }
        let comp_rows = if let Some(ref ms) = state.menu {
            draw_menu(out, ms, comp_rows, self.reasoning_effort)
        } else {
            draw_completions(out, state.completer.as_ref(), comp_rows)
        };

        let total_rows = stash_rows
            + queued_rows
            + btw_visual
            + 1
            + content_rows
            + 1
            + status_line_rows
            + comp_rows;
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
        // When the prompt section overflows the viewport, some leading rows
        // (stash/queued/btw) have scrolled off the top. Reduce extra_rows by
        // the overflow so the cursor lands on the correct input row.
        let overflow = if scrolled {
            (new_rows + rows_below).saturating_sub(height)
        } else {
            0
        };
        let visible_extra = extra_rows.saturating_sub(overflow);
        let text_row = prompt_start + 1 + visible_extra + cursor_line_visible as u16;
        let text_col = 1 + cursor_col as u16;
        let _ = out.queue(cursor::MoveTo(text_col, text_row));

        (top_row, total_rows as u16, scrolled)
    }
}

fn vim_mode_label(mode: Option<crate::vim::ViMode>) -> Option<&'static str> {
    match mode {
        Some(crate::vim::ViMode::Insert) => Some("-- INSERT --"),
        Some(crate::vim::ViMode::Visual) => Some("-- VISUAL --"),
        Some(crate::vim::ViMode::VisualLine) => Some("-- VISUAL LINE --"),
        _ => None,
    }
}

fn render_stash(
    out: &mut RenderOut,
    stash: &Option<InputSnapshot>,
    usable: usize,
    store: &AttachmentStore,
) -> u16 {
    let Some(ref snap) = stash else {
        return 0;
    };
    let full_display =
        spans_to_string(&build_display_spans(&snap.buf, &snap.attachment_ids, store));
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
    let _ = out.queue(SetForegroundColor(theme::muted()));
    let _ = out.queue(Print(format!("{}{}", display, suffix)));
    let _ = out.queue(SetAttribute(Attribute::Reset));
    let _ = out.queue(ResetColor);
    let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
    let _ = out.queue(Print("\r\n"));
    1
}

fn render_queued(out: &mut RenderOut, queued: &[String], usable: usize) -> u16 {
    // Mirrors Block::User rendering (blocks.rs) but with a 1-char indent
    // and no stripping of leading/trailing blank lines.
    let indent = 1usize;
    let text_w = usable.saturating_sub(indent + 1).max(1);
    let mut rows = 0u16;
    for msg in queued {
        let is_command = crate::completer::Completer::is_command(msg.trim());
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
                let _ = out.queue(SetBackgroundColor(theme::user_bg()));
                let _ = out.queue(Print(" ".repeat(fill)));
                let _ = out.queue(SetAttribute(Attribute::Reset));
                let _ = out.queue(ResetColor);
                crlf(out);
                rows += 1;
                continue;
            }
            let chunks = wrap_line(line, text_w);
            for chunk in &chunks {
                let chunk_len = chunk.chars().count();
                let trailing = if block_w > 0 {
                    block_w.saturating_sub(chunk_len)
                } else {
                    1
                };
                let _ = out.queue(Print(" ".repeat(indent)));
                let _ = out.queue(SetBackgroundColor(theme::user_bg()));
                let _ = out.queue(SetAttribute(Attribute::Bold));
                let _ = out.queue(Print(" "));
                blocks::print_user_highlights(out, chunk, &[], is_command);
                let _ = out.queue(Print(" ".repeat(trailing)));
                let _ = out.queue(SetAttribute(Attribute::Reset));
                let _ = out.queue(ResetColor);
                crlf(out);
                rows += 1;
            }
        }
    }
    rows
}

fn render_btw(
    out: &mut RenderOut,
    btw: &mut BtwBlock,
    usable: usize,
    max_content_lines: usize,
    vim_enabled: bool,
) -> u16 {
    let max_lines = max_content_lines.max(1);
    let mut rows = 0u16;

    // Header: "/btw" in accent, question with @path and image highlighting.
    let _ = out.queue(Print(" "));
    let _ = out.queue(SetForegroundColor(theme::accent()));
    let _ = out.queue(Print("/btw"));
    let _ = out.queue(ResetColor);
    let _ = out.queue(Print(" "));
    let max_q = usable.saturating_sub(6); // " /btw " = 6 chars
    let q: String = btw.question.chars().take(max_q).collect();
    blocks::print_user_highlights(out, &q, &btw.image_labels, false);
    let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
    let _ = out.queue(Print("\r\n"));
    rows += 1;

    // Body: response or spinner.
    match btw.response {
        Some(ref text) => {
            let render_w = usable;

            // Rebuild rendered line cache on width change or first render.
            if btw.wrapped.is_empty() || btw.wrap_width != render_w {
                btw.wrapped.clear();
                let mut buf = RenderOut::buffer();
                blocks::render_markdown_inner(&mut buf, text, render_w, "   ", true, None);
                let _ = std::io::Write::flush(&mut buf);
                let bytes = buf.into_bytes();
                let rendered = String::from_utf8_lossy(&bytes);
                for line in rendered.split("\r\n") {
                    btw.wrapped.push(line.to_string());
                }
                // Remove trailing empty from split.
                if btw.wrapped.last().is_some_and(|l| l.is_empty()) {
                    btw.wrapped.pop();
                }
                if btw.wrapped.is_empty() {
                    btw.wrapped.push(String::new());
                }
                btw.wrap_width = render_w;
                // Clamp scroll.
                let max = btw.wrapped.len().saturating_sub(max_lines);
                btw.scroll_offset = btw.scroll_offset.min(max);
            }

            let total = btw.wrapped.len();
            let visible = total.min(max_lines);
            let can_scroll = total > max_lines;

            for line in btw.wrapped.iter().skip(btw.scroll_offset).take(visible) {
                let _ = out.queue(Print(line));
                let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
                let _ = out.queue(Print("\r\n"));
                rows += 1;
            }

            // Blank line before hint.
            let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
            let _ = out.queue(Print("\r\n"));
            rows += 1;

            // Scroll hint or dismiss hint.
            let _ = out.queue(SetForegroundColor(theme::muted()));
            if can_scroll {
                let end = (btw.scroll_offset + visible).min(total);
                let _ = out.queue(Print(format!(
                    "   [{end}/{total}]  {}  {}  esc: close",
                    hints::nav(vim_enabled),
                    hints::scroll(vim_enabled),
                )));
            } else {
                let _ = out.queue(Print("   esc: close"));
            }
            let _ = out.queue(ResetColor);
            let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
            let _ = out.queue(Print("\r\n"));
            rows += 1;
        }
        None => {
            let frame = (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                / 150) as usize
                % SPINNER_FRAMES.len();
            let _ = out.queue(SetForegroundColor(theme::muted()));
            let _ = out.queue(Print(format!("   {} thinking", SPINNER_FRAMES[frame])));
            let _ = out.queue(ResetColor);
            let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
            let _ = out.queue(Print("\r\n"));
            rows += 1;
        }
    }

    // Blank separator line before the bar.
    let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
    let _ = out.queue(Print("\r\n"));
    rows += 1;

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

/// Wrap a line to fit within `width` display columns, breaking at word boundaries.
/// Words longer than `width` are broken character-by-character.
pub(super) fn wrap_line(line: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![line.to_string()];
    }
    let mut chunks: Vec<String> = Vec::new();

    // Handle embedded newlines: split into logical lines first, then wrap each.
    for logical_line in line.split('\n') {
        let mut current = String::new();
        let mut col = 0;

        for word in logical_line.split_inclusive(' ') {
            let wlen = word.chars().count();
            if col + wlen > width && col > 0 {
                chunks.push(current);
                current = String::new();
                col = 0;
            }
            if wlen > width {
                // Word is longer than the line — hard-wrap it character by character.
                for ch in word.chars() {
                    if col >= width {
                        chunks.push(current);
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
        // Always emit at least one chunk per logical line (preserves blank lines).
        chunks.push(current);
    }
    chunks
}

pub use engine::tools::tool_arg_summary;

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
    let _perf = crate::perf::begin("wrap_cursor");
    let mut visual_lines: Vec<(String, Vec<SpanKind>)> = Vec::new();
    let mut cursor_line = 0;
    let mut cursor_col = 0;
    let mut chars_seen = 0usize;
    let mut cursor_set = false;
    let max_col = usable.max(1);

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
            // Build word-boundary chunks.
            let mut chunks: Vec<&[char]> = Vec::new();
            let mut line_start = 0;
            let mut col = 0;

            // Find word boundaries (split on spaces, keeping space with preceding word).
            let mut i = 0;
            while i < chars.len() {
                // Find end of current word (including trailing spaces).
                let word_start = i;
                // Non-space characters.
                while i < chars.len() && chars[i] != ' ' {
                    i += 1;
                }
                // Trailing spaces.
                while i < chars.len() && chars[i] == ' ' {
                    i += 1;
                }
                let word_len = i - word_start;

                if word_len > max_col {
                    // Word is longer than the line — must hard-wrap it character by character.
                    let mut wi = word_start;
                    while wi < i {
                        let take = (max_col - col).min(i - wi);
                        if take == 0 {
                            chunks.push(&chars[line_start..wi]);
                            line_start = wi;
                            col = 0;
                            continue;
                        }
                        col += take;
                        wi += take;
                        if col >= max_col && wi < chars.len() {
                            chunks.push(&chars[line_start..wi]);
                            line_start = wi;
                            col = 0;
                        }
                    }
                } else if col + word_len > max_col && col > 0 {
                    // Wrap before this word.
                    chunks.push(&chars[line_start..word_start]);
                    line_start = word_start;
                    col = word_len;
                } else {
                    col += word_len;
                }
            }
            // Remaining text on the last visual line.
            if line_start <= chars.len() {
                chunks.push(&chars[line_start..]);
            }

            for (ci, chunk) in chunks.iter().enumerate() {
                let chunk_start = chars_seen;
                let is_last_chunk = ci == chunks.len() - 1;
                if !cursor_set
                    && cursor_char >= chunk_start
                    && (cursor_char < chunk_start + chunk.len()
                        || (is_last_chunk && cursor_char == chunk_start + chunk.len()))
                {
                    cursor_line = visual_lines.len();
                    cursor_col = cursor_char - chunk_start;
                    cursor_set = true;
                }
                let kinds = char_kinds
                    .get(chunk_start..chunk_start + chunk.len())
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

/// Compute the display-char offset of each visual line.
///
/// The display buffer is the concatenation of spans (with attachments
/// expanded to their labels).  `wrap_and_locate_cursor` splits on `\n`
/// and then further wraps each logical line into visual lines.  The
/// char offsets it uses include +1 for every `\n` consumed by `split`.
/// We replicate that counting here by re-splitting the display buffer
/// and mapping each logical line's visual chunks to offsets.
fn compute_visual_line_offsets(
    display_buf: &str,
    visual_lines: &[(String, Vec<SpanKind>)],
) -> Vec<usize> {
    let mut offsets = Vec::with_capacity(visual_lines.len());
    let mut chars_seen: usize = 0;
    let mut vl_idx = 0;
    let newline_count = display_buf.chars().filter(|&c| c == '\n').count();

    for (li, text_line) in display_buf.split('\n').enumerate() {
        let line_chars = text_line.chars().count();
        if line_chars == 0 {
            if vl_idx < visual_lines.len() {
                offsets.push(chars_seen);
                vl_idx += 1;
            }
        } else {
            let mut consumed = 0;
            while vl_idx < visual_lines.len() && consumed < line_chars {
                offsets.push(chars_seen + consumed);
                consumed += visual_lines[vl_idx].0.chars().count();
                vl_idx += 1;
            }
        }
        chars_seen += line_chars;
        if li < newline_count {
            chars_seen += 1;
        }
    }
    while offsets.len() < visual_lines.len() {
        offsets.push(chars_seen);
    }
    offsets
}

pub(super) struct BarSpan {
    text: String,
    color: Color,
    bg: Option<Color>,
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
    let _perf = crate::perf::begin("draw_bar");
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
                    1 + inner + 1
                } else {
                    0
                } // dash + spans + space
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
        1 + left_filtered
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
        for span in &left_filtered {
            if let Some(attr) = span.attr {
                let _ = out.queue(SetAttribute(attr));
            }
            if let Some(bg) = span.bg {
                let _ = out.queue(SetBackgroundColor(bg));
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
            if let Some(bg) = span.bg {
                let _ = out.queue(SetBackgroundColor(bg));
            }
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

/// Scan an `@path` or `@"path with spaces"` token starting at position `i`.
/// Returns `(token_string, path_str, end_index)`.
pub(crate) fn scan_at_token(chars: &[char], i: usize) -> Option<(String, String, usize)> {
    if chars[i] != '@' {
        return None;
    }
    if i > 0 && !chars[i - 1].is_whitespace() && chars[i - 1] != '(' {
        return None;
    }

    let quoted = i + 1 < chars.len() && chars[i + 1] == '"';
    let end = if quoted {
        let mut e = i + 2;
        while e < chars.len() && chars[e] != '"' {
            e += 1;
        }
        if e >= chars.len() || e == i + 2 {
            return None;
        }
        e + 1
    } else {
        let mut e = i + 1;
        while e < chars.len() && !chars[e].is_whitespace() {
            e += 1;
        }
        if e <= i + 1 {
            return None;
        }
        e
    };

    let token: String = chars[i..end].iter().collect();
    let path = if quoted {
        token[2..token.len() - 1].to_string()
    } else {
        token[1..].to_string()
    };
    Some((token, path, end))
}

/// Check if position `i` in `chars` starts a valid `@path` reference.
/// Returns `Some((token, end_index))` if the path after `@` exists on disk.
pub(super) fn try_at_ref(chars: &[char], i: usize) -> Option<(String, usize)> {
    let (token, path, end) = scan_at_token(chars, i)?;
    if std::path::Path::new(&path).exists() {
        return Some((token, end));
    }
    // Strip trailing punctuation and retry
    let trimmed = path.trim_end_matches([',', '.', ')', ';', ':', '!', '?']);
    if trimmed.len() < path.len() && !trimmed.is_empty() && std::path::Path::new(trimmed).exists() {
        let stripped = path.len() - trimmed.len();
        let short_token = token[..token.len() - stripped].to_string();
        Some((short_token, end - stripped))
    } else {
        None
    }
}

fn build_display_spans(buf: &str, att_ids: &[AttachmentId], store: &AttachmentStore) -> Vec<Span> {
    let _perf = crate::perf::begin("display_spans");
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
            let label = att_ids
                .get(att_idx)
                .map(|&id| store.display_label(id))
                .unwrap_or_else(|| "[?]".into());
            spans.push(Span::Attachment(label));
            att_idx += 1;
            i += 1;
        } else if let Some((token, end)) = try_at_ref(&chars, i) {
            if !plain.is_empty() {
                spans.push(Span::Plain(std::mem::take(&mut plain)));
            }
            spans.push(Span::AtRef(token));
            i = end;
        } else if let Some((token, _, end)) = scan_at_token(&chars, i) {
            if !plain.is_empty() {
                spans.push(Span::Plain(std::mem::take(&mut plain)));
            }
            spans.push(Span::Plain(token));
            i = end;
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
/// `selection` is an optional (start_char, end_char) range within this line.
fn render_styled_chars(
    out: &mut RenderOut,
    line: &str,
    kinds: &[SpanKind],
    selection: Option<(usize, usize)>,
) {
    let mut current = SpanKind::Plain;
    let mut in_sel = false;
    let char_count = line.chars().count();
    for (i, ch) in line.chars().enumerate() {
        let kind = kinds.get(i).copied().unwrap_or(SpanKind::Plain);
        let want_sel = selection.is_some_and(|(s, e)| i >= s && i < e);

        if kind != current || want_sel != in_sel {
            // Reset previous styling before applying new.
            if in_sel || current != SpanKind::Plain {
                let _ = out.queue(ResetColor);
            }
            if want_sel {
                let _ = out.queue(SetBackgroundColor(theme::selection_bg()));
            }
            if kind == SpanKind::AtRef || kind == SpanKind::Attachment {
                let _ = out.queue(SetForegroundColor(theme::accent()));
            }
            current = kind;
            in_sel = want_sel;
        }
        let _ = out.queue(Print(ch));
    }
    // Render a highlighted space for empty lines within a selection.
    if let Some((s, e)) = selection {
        if e > char_count && s <= char_count {
            if !in_sel {
                let _ = out.queue(SetBackgroundColor(theme::selection_bg()));
            }
            let _ = out.queue(Print(' '));
            let _ = out.queue(ResetColor);
            return;
        }
    }
    if in_sel || current != SpanKind::Plain {
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
        crate::completer::CompleterKind::CommandArg => "",
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
            show_tps,
            show_tokens,
            show_cost,
            show_prediction,
            show_slug,
            restrict_to_workspace,
        } => {
            let rows: &[(&str, bool)] = &[
                ("vim mode", *vim_enabled),
                ("auto compact", *auto_compact),
                ("show tok/s", *show_tps),
                ("show tokens", *show_tokens),
                ("show cost", *show_cost),
                ("input prediction", *show_prediction),
                ("task slug", *show_slug),
                ("restrict to workspace", *restrict_to_workspace),
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
        MenuKind::Theme { presets, .. } | MenuKind::Color { presets, .. } => {
            draw_color_presets(out, presets, selected, max_rows)
        }
        MenuKind::Stats { left, right } => draw_stats(out, left, right, max_rows),
        MenuKind::Cost { lines } => draw_stats_sequential(out, lines, 0, max_rows),
        MenuKind::Model { models, query, .. } => {
            let mut drawn = 0;

            // Show filter query if active.
            if !query.is_empty() && drawn < max_rows {
                let _ = out.queue(SetAttribute(Attribute::Dim));
                let _ = out.queue(Print("  filter: "));
                let _ = out.queue(SetAttribute(Attribute::Reset));
                let _ = out.queue(SetForegroundColor(theme::accent()));
                let _ = out.queue(Print(query));
                let _ = out.queue(ResetColor);
                let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
                drawn += 1;
            }

            if models.is_empty() {
                if drawn > 0 && drawn < max_rows {
                    let _ = out.queue(Print("\r\n"));
                    let _ = out.queue(SetAttribute(Attribute::Dim));
                    let _ = out.queue(Print("  no matches"));
                    let _ = out.queue(SetAttribute(Attribute::Reset));
                    let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
                    drawn += 1;
                }
            } else {
                let max_visible = 5.min(max_rows.saturating_sub(drawn));
                let total = models.len();
                let col = models
                    .iter()
                    .map(|(_, name, _)| name.len())
                    .max()
                    .unwrap_or(0)
                    + 4;

                // Scroll window around selected item.
                let mut start = 0;
                if total > max_visible {
                    let half = max_visible / 2;
                    start = selected.saturating_sub(half);
                    if start + max_visible > total {
                        start = total - max_visible;
                    }
                }
                let end = (start + max_visible).min(total);

                for (idx, (_, model_name, provider_name)) in
                    models.iter().enumerate().skip(start).take(end - start)
                {
                    if drawn > 0 {
                        let _ = out.queue(Print("\r\n"));
                    }
                    draw_menu_row(out, model_name, provider_name, col, idx == selected);
                    drawn += 1;
                }
            }

            // Always show thinking line.
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

fn draw_color_presets(
    out: &mut RenderOut,
    presets: &[(&str, &str, u8)],
    selected: usize,
    max_rows: usize,
) -> usize {
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
        let label_len = name.len() + 2;
        let padding = " ".repeat(col.saturating_sub(label_len - 2));
        let _ = out.queue(SetAttribute(Attribute::Dim));
        let _ = out.queue(Print(format!("{padding}{detail}")));
        let _ = out.queue(SetAttribute(Attribute::Reset));
        let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
        drawn += 1;
    }
    drawn
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

use crate::metrics::{label_col_width, stats_line_visual_width as stats_line_width};

fn draw_stats_line(out: &mut RenderOut, line: &crate::metrics::StatsLine, label_col: usize) {
    use crate::metrics::StatsLine;
    match line {
        StatsLine::Kv { label, value } => {
            let _ = out.queue(SetAttribute(Attribute::Dim));
            let _ = out.queue(Print(label));
            let _ = out.queue(SetAttribute(Attribute::Reset));
            let col = label_col.max(label.len() + 2);
            let padding = " ".repeat(col.saturating_sub(label.len()));
            let _ = out.queue(Print(padding));
            let _ = out.queue(Print(value));
        }
        StatsLine::Heading(text) | StatsLine::SparklineLegend(text) => {
            let _ = out.queue(SetAttribute(Attribute::Dim));
            let _ = out.queue(Print(text));
            let _ = out.queue(SetAttribute(Attribute::Reset));
        }
        StatsLine::SparklineBars(bars) => {
            let _ = out.queue(SetForegroundColor(theme::accent()));
            let _ = out.queue(Print(bars));
            let _ = out.queue(ResetColor);
        }
        StatsLine::HeatRow { label, cells } => {
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
        StatsLine::Blank => {}
    }
}

fn draw_stats_sequential(
    out: &mut RenderOut,
    lines: &[crate::metrics::StatsLine],
    already_drawn: usize,
    max_rows: usize,
) -> usize {
    let lc = label_col_width(lines);
    let mut count = 0;
    for line in lines {
        if already_drawn + count >= max_rows {
            break;
        }
        if already_drawn + count > 0 {
            let _ = out.queue(Print("\r\n"));
        }
        let _ = out.queue(Print("  "));
        draw_stats_line(out, line, lc);
        let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
        count += 1;
    }
    count
}

fn draw_stats(
    out: &mut RenderOut,
    left: &[crate::metrics::StatsLine],
    right: &[crate::metrics::StatsLine],
    max_rows: usize,
) -> usize {
    let left_lc = label_col_width(left);
    let right_lc = label_col_width(right);

    let left_col_width = left
        .iter()
        .map(|l| 2 + stats_line_width(l, left_lc))
        .max()
        .unwrap_or(0);

    let right_width: usize = right
        .iter()
        .map(|l| stats_line_width(l, right_lc))
        .max()
        .unwrap_or(0);
    let term_width = terminal::size().map(|(w, _)| w as usize).unwrap_or(80);
    let gap = 5;
    let side_by_side = !right.is_empty() && left_col_width + gap + right_width + 2 <= term_width;

    if !side_by_side {
        let mut drawn = draw_stats_sequential(out, left, 0, max_rows);
        if !right.is_empty() && drawn < max_rows {
            let _ = out.queue(Print("\r\n"));
            let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
            drawn += 1;
            drawn += draw_stats_sequential(out, right, drawn, max_rows);
        }
        return drawn;
    }

    let total = left.len().max(right.len());
    let right_col = left_col_width + gap;
    let mut drawn = 0;

    for i in 0..total {
        if drawn >= max_rows {
            break;
        }
        if drawn > 0 {
            let _ = out.queue(Print("\r\n"));
        }

        let lw = if i < left.len() {
            let _ = out.queue(Print("  "));
            draw_stats_line(out, &left[i], left_lc);
            2 + stats_line_width(&left[i], left_lc)
        } else {
            0
        };

        if i < right.len() {
            let pad = right_col.saturating_sub(lw);
            let _ = out.queue(Print(" ".repeat(pad)));
            draw_stats_line(out, &right[i], right_lc);
        }

        let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
        drawn += 1;
    }
    drawn
}

#[cfg(test)]
mod selection_tests {
    use super::*;

    fn vlines(strs: &[&str]) -> Vec<(String, Vec<SpanKind>)> {
        strs.iter()
            .map(|s| (s.to_string(), vec![SpanKind::Plain; s.chars().count()]))
            .collect()
    }

    #[test]
    fn offsets_single_line() {
        let offsets = compute_visual_line_offsets("hello", &vlines(&["hello"]));
        assert_eq!(offsets, vec![0]);
    }

    #[test]
    fn offsets_two_logical_lines() {
        let offsets = compute_visual_line_offsets("aaa\nbbb", &vlines(&["aaa", "bbb"]));
        assert_eq!(offsets, vec![0, 4]);
    }

    #[test]
    fn offsets_three_logical_lines() {
        let offsets = compute_visual_line_offsets("aaa\nbbb\nccc", &vlines(&["aaa", "bbb", "ccc"]));
        assert_eq!(offsets, vec![0, 4, 8]);
    }

    #[test]
    fn offsets_empty_line() {
        let offsets = compute_visual_line_offsets("aaa\n\nccc", &vlines(&["aaa", "", "ccc"]));
        assert_eq!(offsets, vec![0, 4, 5]);
    }

    #[test]
    fn offsets_wrapped_line() {
        let offsets = compute_visual_line_offsets("abcdef", &vlines(&["abc", "def"]));
        assert_eq!(offsets, vec![0, 3]);
    }

    #[test]
    fn offsets_wrapped_multiline() {
        let offsets = compute_visual_line_offsets("abcdef\nxy", &vlines(&["abc", "def", "xy"]));
        assert_eq!(offsets, vec![0, 3, 7]);
    }

    #[test]
    fn offsets_selection_across_wrapped() {
        let offsets = compute_visual_line_offsets("abcdef", &vlines(&["abc", "def"]));
        // Selection chars 1..5 should map to line0:(1,3), line1:(0,2).
        let sel = (1usize, 5usize);
        let l0_s = sel.0.saturating_sub(offsets[0]);
        let l0_e = sel.1.min(offsets[0] + 3) - offsets[0];
        assert_eq!((l0_s, l0_e), (1, 3));
        let l1_s = sel.0.saturating_sub(offsets[1]);
        let l1_e = sel.1.min(offsets[1] + 3) - offsets[1];
        assert_eq!((l1_s, l1_e), (0, 2));
    }

    #[test]
    fn offsets_selection_across_logical() {
        let offsets = compute_visual_line_offsets("aaa\nbbb", &vlines(&["aaa", "bbb"]));
        // Select display chars 1..6 = "aa\nbb".
        let sel = (1usize, 6usize);
        let l0_s = sel.0.saturating_sub(offsets[0]);
        let l0_e = sel.1.min(offsets[0] + 3) - offsets[0];
        assert_eq!((l0_s, l0_e), (1, 3));
        let l1_s = sel.0.saturating_sub(offsets[1]);
        let l1_e = sel.1.min(offsets[1] + 3) - offsets[1];
        assert_eq!((l1_s, l1_e), (0, 2));
    }
}
