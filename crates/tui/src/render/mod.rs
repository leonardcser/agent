mod blocks;
mod cache;
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
use crate::input::{InputSnapshot, InputState, ATTACHMENT_MARKER};
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
use unicode_width::UnicodeWidthChar;

use blocks::{
    animated_dots, collect_trailing_thinking, gap_between, render_block, render_thinking_summary,
    render_tool, thinking_summary, Element,
};

pub use cache::{
    build_tool_output_render_cache, session_render_hash, CachedNotebookEdit, RenderCache,
    ToolOutputRenderCache,
};

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

/// Abstracts terminal I/O so rendering can target either a real
/// terminal (stdout + crossterm queries) or an in-memory buffer.
pub trait TerminalBackend {
    /// Terminal dimensions `(cols, rows)`.
    fn size(&self) -> (u16, u16);
    /// Current cursor row. Used as fallback when `anchor_row` is unset.
    fn cursor_y(&self) -> u16;
    /// Build a `RenderOut` that writes to this backend's output.
    fn make_output(&self) -> RenderOut;
}

/// Production backend writing to stdout and querying the real terminal.
pub struct StdioBackend;

impl TerminalBackend for StdioBackend {
    fn size(&self) -> (u16, u16) {
        terminal::size().unwrap_or((80, 24))
    }
    fn cursor_y(&self) -> u16 {
        cursor::position().map(|(_, y)| y).unwrap_or(0)
    }
    fn make_output(&self) -> RenderOut {
        RenderOut::scroll()
    }
}

/// Output wrapper that selects the line-advance strategy.
///
/// * `row: None` — **scroll mode** (blocks / prompt): `\r\n` pushes content
///   into terminal scrollback, which is the normal way conversation renders.
/// * `row: Some(r)` — **overlay mode** (dialogs): `MoveTo(0, r+1)` repositions
///   the cursor without scrolling, so dialogs never pollute scrollback.
pub struct RenderOut {
    pub out: Box<dyn Write>,
    pub row: Option<u16>,
    capture: Option<std::sync::Arc<std::sync::Mutex<Vec<u8>>>>,
    /// Tracked foreground color (for skipping redundant style commands).
    cur_fg: Option<Color>,
    /// Tracked background color.
    cur_bg: Option<Color>,
    /// Tracked bold attribute.
    cur_bold: bool,
    /// Tracked dim attribute.
    cur_dim: bool,
    /// Tracked italic attribute.
    cur_italic: bool,
}

impl RenderOut {
    /// Create a scroll-mode output (for blocks + prompt).
    /// Dialogs switch to overlay mode by setting `out.row = Some(r)`.
    pub fn scroll() -> Self {
        Self {
            out: Box::new(BufWriter::with_capacity(1 << 20, io::stdout())),
            row: None,
            capture: None,
            cur_fg: None,
            cur_bg: None,
            cur_bold: false,
            cur_dim: false,
            cur_italic: false,
        }
    }

    /// Create a scroll-mode output writing to a shared buffer (for testing).
    pub fn shared_sink(sink: std::sync::Arc<std::sync::Mutex<Vec<u8>>>) -> Self {
        Self {
            out: Box::new(SharedWriter(sink)),
            row: None,
            capture: None,
            cur_fg: None,
            cur_bg: None,
            cur_bold: false,
            cur_dim: false,
            cur_italic: false,
        }
    }

    /// Create a render output that writes to an in-memory buffer.
    pub fn buffer() -> Self {
        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        Self {
            out: Box::new(SharedWriter(buf.clone())),
            row: None,
            capture: Some(buf),
            cur_fg: None,
            cur_bg: None,
            cur_bold: false,
            cur_dim: false,
            cur_italic: false,
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

    pub fn set_fg(&mut self, color: Color) {
        if self.cur_fg != Some(color) {
            self.cur_fg = Some(color);
            let _ = self.queue(SetForegroundColor(color));
        }
    }

    pub fn set_bg(&mut self, color: Color) {
        if self.cur_bg != Some(color) {
            self.cur_bg = Some(color);
            let _ = self.queue(SetBackgroundColor(color));
        }
    }

    pub fn set_bold(&mut self) {
        if !self.cur_bold {
            self.cur_bold = true;
            let _ = self.queue(SetAttribute(Attribute::Bold));
        }
    }

    pub fn set_dim(&mut self) {
        if !self.cur_dim {
            self.cur_dim = true;
            let _ = self.queue(SetAttribute(Attribute::Dim));
        }
    }

    pub fn set_italic(&mut self) {
        if !self.cur_italic {
            self.cur_italic = true;
            let _ = self.queue(SetAttribute(Attribute::Italic));
        }
    }

    /// Set dim+italic, skipping if both are already active.
    pub fn set_dim_italic(&mut self) {
        if self.cur_dim && self.cur_italic {
            return;
        }
        if !self.cur_dim {
            self.cur_dim = true;
            let _ = self.queue(SetAttribute(Attribute::Dim));
        }
        if !self.cur_italic {
            self.cur_italic = true;
            let _ = self.queue(SetAttribute(Attribute::Italic));
        }
    }

    pub fn reset_style(&mut self) {
        if self.cur_fg.is_some()
            || self.cur_bg.is_some()
            || self.cur_bold
            || self.cur_dim
            || self.cur_italic
        {
            let _ = self.queue(SetAttribute(Attribute::Reset));
            let _ = self.queue(ResetColor);
            self.cur_fg = None;
            self.cur_bg = None;
            self.cur_bold = false;
            self.cur_dim = false;
            self.cur_italic = false;
        }
    }

    pub fn invalidate_style(&mut self) {
        self.cur_fg = None;
        self.cur_bg = None;
        self.cur_bold = false;
        self.cur_dim = false;
        self.cur_italic = false;
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

#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
    pub render_cache: Option<ToolOutputRenderCache>,
}

pub type ToolOutputRef = Box<ToolOutput>;

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
    pub output: Option<ToolOutputRef>,
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
        call_id: String,
        name: String,
        summary: String,
        args: HashMap<String, serde_json::Value>,
        status: ToolStatus,
        elapsed: Option<Duration>,
        output: Option<ToolOutputRef>,
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
    /// Cached row count for each block in thinking-visible mode.
    row_counts_visible: Vec<u16>,
    /// Cached row count for each block in thinking-hidden mode.
    row_counts_hidden: Vec<u16>,
    /// Cached rendered bytes for each block (scroll-mode ANSI output) in
    /// thinking-visible mode.
    cached_bytes_visible: Vec<Option<Vec<u8>>>,
    /// Cached rendered bytes for each block (scroll-mode ANSI output) in
    /// thinking-hidden mode.
    cached_bytes_hidden: Vec<Option<Vec<u8>>>,
    /// Terminal width when caches were built. Mismatch invalidates all caches.
    cache_width: usize,
    flushed: usize,
    last_block_rows: u16,
    /// When true, the leading gap of the next unflushed block is suppressed.
    /// Set after a dialog dismiss where ScrollUp pushed the gap into scrollback.
    suppress_leading_gap: bool,
}

impl BlockHistory {
    fn new() -> Self {
        Self {
            blocks: Vec::new(),
            row_counts_visible: Vec::new(),
            row_counts_hidden: Vec::new(),
            cached_bytes_visible: Vec::new(),
            cached_bytes_hidden: Vec::new(),
            cache_width: 0,
            flushed: 0,
            last_block_rows: 0,
            suppress_leading_gap: false,
        }
    }

    fn push(&mut self, block: Block) {
        self.blocks.push(block);
        self.row_counts_visible.push(0);
        self.row_counts_hidden.push(0);
        self.cached_bytes_visible.push(None);
        self.cached_bytes_hidden.push(None);
    }

    fn has_unflushed(&self) -> bool {
        self.flushed < self.blocks.len()
    }

    fn clear(&mut self) {
        self.blocks.clear();
        self.row_counts_visible.clear();
        self.row_counts_hidden.clear();
        self.cached_bytes_visible.clear();
        self.cached_bytes_hidden.clear();
        self.flushed = 0;
        self.last_block_rows = 0;
    }

    fn invalidate_caches(&mut self) {
        for c in &mut self.cached_bytes_visible {
            *c = None;
        }
        for c in &mut self.cached_bytes_hidden {
            *c = None;
        }
    }

    fn row_counts(&self, show_thinking: bool) -> &[u16] {
        if show_thinking {
            &self.row_counts_visible
        } else {
            &self.row_counts_hidden
        }
    }

    fn row_counts_mut(&mut self, show_thinking: bool) -> &mut Vec<u16> {
        if show_thinking {
            &mut self.row_counts_visible
        } else {
            &mut self.row_counts_hidden
        }
    }

    fn cached_bytes_mut(&mut self, show_thinking: bool) -> &mut Vec<Option<Vec<u8>>> {
        if show_thinking {
            &mut self.cached_bytes_visible
        } else {
            &mut self.cached_bytes_hidden
        }
    }

    fn cached_bytes(&self, show_thinking: bool) -> &[Option<Vec<u8>>] {
        if show_thinking {
            &self.cached_bytes_visible
        } else {
            &self.cached_bytes_hidden
        }
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
    fn redraw_start(&self, max_lines: u16, show_thinking: bool) -> usize {
        let row_counts = self.row_counts(show_thinking);
        let mut budget = max_lines;
        let mut start = self.blocks.len();
        for i in (0..self.blocks.len()).rev() {
            let total = self.block_gap(i) + row_counts[i];
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
        self.row_counts_visible.truncate(idx);
        self.row_counts_hidden.truncate(idx);
        self.cached_bytes_visible.truncate(idx);
        self.cached_bytes_hidden.truncate(idx);
        self.flushed = self.flushed.min(idx);
    }

    /// Render unflushed blocks. Returns total rows printed.
    ///
    /// In scroll mode (`out.row` is `None`), rendered bytes are cached per
    /// block and replayed on subsequent redraws instead of re-rendering.
    fn render(&mut self, out: &mut RenderOut, width: usize, show_thinking: bool) -> u16 {
        if !self.has_unflushed() {
            return 0;
        }
        let use_cache = out.row.is_none();

        // Invalidate caches when terminal width changes.
        if use_cache && width != self.cache_width {
            self.invalidate_caches();
            self.cache_width = width;
        }

        let mut total = 0u16;
        let last_idx = self.blocks.len().saturating_sub(1);
        let mut first = true;
        for i in self.flushed..self.blocks.len() {
            let gap = if first && self.suppress_leading_gap {
                0
            } else {
                self.block_gap(i)
            };
            first = false;
            for _ in 0..gap {
                crlf(out);
            }

            if use_cache {
                let cached = self.cached_bytes(show_thinking)[i].clone();
                if let Some(cached) = cached {
                    let _ = out.write_all(&cached);
                    out.invalidate_style();
                } else {
                    let mut buf = RenderOut::buffer();
                    let rows = render_block(&mut buf, &self.blocks[i], width, show_thinking);
                    let bytes = buf.into_bytes();
                    let _ = out.write_all(&bytes);
                    out.invalidate_style();
                    self.cached_bytes_mut(show_thinking)[i] = Some(bytes);
                    self.row_counts_mut(show_thinking)[i] = rows;
                }
            } else {
                let rows = render_block(out, &self.blocks[i], width, show_thinking);
                self.row_counts_mut(show_thinking)[i] = rows;
            }

            let rows = self.row_counts(show_thinking)[i];
            total += gap + rows;
            if i == last_idx {
                self.last_block_rows = rows + gap;
            }
        }
        self.suppress_leading_gap = false;
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
    active_agents: Vec<ActiveAgent>,
    active_exec: Option<ActiveExec>,
    prompt: PromptState,
    working: WorkingState,
    context_tokens: Option<u32>,
    context_window: Option<u32>,
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
    /// A dialog is currently open (confirm, rewind, etc.).
    dialog_open: bool,
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
    show_thinking: bool,
    /// Cached state for rendering the status line during dialogs.
    last_vim_enabled: bool,
    last_vim_mode: Option<crate::vim::ViMode>,
    last_mode: protocol::Mode,
    /// Whether to render the active tool above the dialog in content-only
    /// mode.  Set when tool + dialog fit on screen; cleared on dialog close.
    show_tool_in_dialog: bool,
    /// Ephemeral btw side-question state, rendered above the prompt.
    btw: Option<BtwBlock>,
    /// Ephemeral notification shown above the prompt, dismissed on any key.
    notification: Option<Notification>,
    /// Short task label (slug) shown on the status bar after the throbber.
    task_label: Option<String>,

    /// Terminal I/O backend (real terminal or test buffer).
    backend: Box<dyn TerminalBackend>,
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
        Self::with_backend(Box::new(StdioBackend))
    }

    pub fn with_backend(backend: Box<dyn TerminalBackend>) -> Self {
        Self {
            history: BlockHistory::new(),
            active_thinking: None,
            active_text: None,
            active_tools: Vec::new(),
            active_agents: Vec::new(),
            active_exec: None,
            prompt: PromptState::new(),
            working: WorkingState::new(),
            context_tokens: None,
            context_window: None,
            session_cost_usd: 0.0,
            model_label: None,
            reasoning_effort: Default::default(),
            has_scrollback: false,
            content_start_row: None,
            defer_pending_render: false,
            defer_redraw: false,
            pending_dialog: false,
            dialog_open: false,
            sync_started: false,
            running_procs: 0,
            running_agents: 0,
            show_tps: true,
            show_tokens: true,
            show_cost: true,
            show_slug: true,
            show_thinking: true,
            last_vim_enabled: false,
            last_vim_mode: None,
            last_mode: protocol::Mode::Normal,
            show_tool_in_dialog: false,
            btw: None,
            notification: None,
            task_label: None,
            backend,
        }
    }

    fn size(&self) -> (u16, u16) {
        self.backend.size()
    }

    fn scroll_output(&self) -> RenderOut {
        self.backend.make_output()
    }

    fn cursor_y(&self) -> u16 {
        self.prompt
            .anchor_row
            .unwrap_or_else(|| self.backend.cursor_y())
    }

    /// Build a `RenderOut` from this screen's backend.
    pub fn backend_make_output(&self) -> RenderOut {
        self.backend.make_output()
    }

    /// Expose the backend for dialogs that need output + size.
    pub fn backend(&self) -> &dyn TerminalBackend {
        &*self.backend
    }

    /// Set the prompt anchor row explicitly (used by test harness).
    pub fn set_anchor_row(&mut self, row: u16) {
        self.prompt.anchor_row = Some(row);
    }

    /// Number of committed blocks in history.
    pub fn block_count(&self) -> usize {
        self.history.blocks.len()
    }

    /// Cloned snapshot of all blocks in history.
    pub fn blocks(&self) -> Vec<Block> {
        self.history.blocks.clone()
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
        let term_h = self.size().1 as usize;
        let Some(ref mut btw) = self.btw else {
            return false;
        };
        if btw.wrapped.is_empty() {
            return false;
        }
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

    /// Apply all toggle settings from a resolved settings snapshot.
    pub fn apply_settings(&mut self, s: &crate::state::ResolvedSettings) {
        self.show_tps = s.show_tps;
        self.show_tokens = s.show_tokens;
        self.show_cost = s.show_cost;
        self.show_slug = s.show_slug;
        self.show_thinking = s.show_thinking;
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
        self.active_agents.push(ActiveAgent {
            agent_id,
            slug: None,
            tool_calls: Vec::new(),
            status: AgentBlockStatus::Running,
            start_time: Instant::now(),
            final_elapsed: None,
        });
        self.prompt.dirty = true;
    }

    /// Update a specific active blocking agent's state.
    pub fn update_active_agent(
        &mut self,
        agent_id: &str,
        slug: Option<&str>,
        tool_calls: &[crate::app::AgentToolEntry],
        status: AgentBlockStatus,
    ) {
        if let Some(agent) = self
            .active_agents
            .iter_mut()
            .find(|a| a.agent_id == agent_id)
        {
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

    /// Mark all active agents as cancelled/error (before flush commits them).
    pub fn cancel_active_agents(&mut self) {
        for agent in &mut self.active_agents {
            agent.status = AgentBlockStatus::Error;
            agent.final_elapsed = Some(agent.start_time.elapsed());
        }
    }

    /// Commit a specific active agent to history and remove it from the live set.
    pub fn finish_active_agent(&mut self, agent_id: &str) {
        if let Some(idx) = self
            .active_agents
            .iter()
            .position(|a| a.agent_id == agent_id)
        {
            let mut agent = self.active_agents.remove(idx);
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

    /// Commit all active agents to history and clear the live set.
    pub fn finish_all_active_agents(&mut self) {
        let agents: Vec<ActiveAgent> = self.active_agents.drain(..).collect();
        for mut agent in agents {
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
        }
        self.prompt.dirty = true;
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
        self.has_scrollback = true;
        self.prompt.prev_dialog_row = Some(actual);
    }

    /// Render the status line at the very last row of the terminal.
    /// Called after a dialog is drawn so the status bar stays visible.
    pub fn draw_dialog_status_line(&mut self) {
        let (_, h) = self.size();
        let mut out = self.scroll_output();
        let _ = out.queue(cursor::SavePosition);
        let _ = out.queue(cursor::MoveTo(0, h - 1));
        self.render_status_line(&mut out);
        let _ = out.queue(cursor::RestorePosition);
        let _ = out.flush();
    }

    /// Render the status line content at the current cursor position.
    fn render_status_line(&self, out: &mut RenderOut) {
        let status_bg = Color::AnsiValue(233);

        // ── Slug pill (far left) ──
        let is_compacting = self.working.throbber == Some(Throbber::Compacting);
        let spinner = self.working.spinner_char();
        let has_slug = self.show_slug && self.task_label.is_some();

        let pill_label: Option<String> = if is_compacting {
            let sp = spinner.unwrap_or(SPINNER_FRAMES[0]);
            Some(format!(" {} compacting ", sp))
        } else if has_slug {
            let label = self.task_label.as_ref().unwrap();
            Some(if let Some(sp) = spinner {
                format!(" {} {} ", sp, label)
            } else {
                format!(" {} ", label)
            })
        } else {
            spinner.map(|sp| format!(" {} working ", sp))
        };

        if let Some(ref pill_text) = pill_label {
            let pill_bg = if is_compacting {
                Color::White
            } else {
                theme::slug_color()
            };
            let _ = out.queue(SetBackgroundColor(pill_bg));
            let _ = out.queue(SetForegroundColor(Color::Black));
            let _ = out.queue(Print(pill_text));
            let _ = out.queue(ResetColor);
        }

        // ── Dark bg for the middle section ──
        let _ = out.queue(SetBackgroundColor(status_bg));

        // ── Vim mode ──
        if self.last_vim_enabled {
            let vim_label = vim_mode_label(self.last_vim_mode).unwrap_or("NORMAL");
            let vim_fg = match self.last_vim_mode {
                Some(crate::vim::ViMode::Insert) => Color::AnsiValue(78),
                Some(crate::vim::ViMode::Visual) | Some(crate::vim::ViMode::VisualLine) => {
                    Color::AnsiValue(176)
                }
                _ => Color::AnsiValue(74),
            };
            let _ = out.queue(SetBackgroundColor(Color::AnsiValue(236)));
            let _ = out.queue(SetForegroundColor(vim_fg));
            let _ = out.queue(Print(format!(" {vim_label} ")));
        }

        // ── Mode indicator ──
        let (mode_icon, mode_name, mode_fg) = match self.last_mode {
            protocol::Mode::Plan => ("◇ ", "plan", theme::PLAN),
            protocol::Mode::Apply => ("→ ", "apply", theme::APPLY),
            protocol::Mode::Yolo => ("⚡", "yolo", theme::YOLO),
            protocol::Mode::Normal => ("○ ", "normal", theme::muted()),
        };
        let _ = out.queue(SetBackgroundColor(Color::AnsiValue(234)));
        let _ = out.queue(SetForegroundColor(mode_fg));
        let _ = out.queue(Print(format!(" {mode_icon}{mode_name} ")));
        let _ = out.queue(SetBackgroundColor(status_bg));
        let mut has_prev = true;

        // ── Throbber status ──
        let throbber_spans = self.working.throbber_spans(self.show_tps);
        // The first span for active states (Working/Compacting/Retrying)
        // contains the spinner + label which is already shown in the pill.
        let is_active = matches!(
            self.working.throbber,
            Some(Throbber::Working) | Some(Throbber::Compacting) | Some(Throbber::Retrying { .. })
        );
        let status_spans: &[BarSpan] = if is_active && !throbber_spans.is_empty() {
            &throbber_spans[1..]
        } else {
            &throbber_spans
        };
        if !status_spans.is_empty() {
            for span in status_spans {
                if let Some(attr) = span.attr {
                    let _ = out.queue(SetAttribute(attr));
                }
                let _ = out.queue(SetForegroundColor(span.color));
                let _ = out.queue(Print(&span.text));
                if span.attr.is_some() {
                    let _ = out.queue(SetAttribute(Attribute::Reset));
                    let _ = out.queue(SetBackgroundColor(status_bg));
                }
            }
            has_prev = true;
        }

        // ── Permission pending (only when no dialog is open) ──
        if self.pending_dialog && !self.dialog_open {
            if has_prev {
                let _ = out.queue(SetForegroundColor(theme::muted()));
                let _ = out.queue(Print(" · "));
            }
            let _ = out.queue(SetForegroundColor(theme::accent()));
            let _ = out.queue(SetAttribute(Attribute::Bold));
            let _ = out.queue(Print("permission pending"));
            let _ = out.queue(SetAttribute(Attribute::Reset));
            let _ = out.queue(SetBackgroundColor(status_bg));
            has_prev = true;
        }

        // ── Running procs ──
        if self.running_procs > 0 {
            if has_prev {
                let _ = out.queue(SetForegroundColor(theme::muted()));
                let _ = out.queue(Print(" · "));
            }
            let _ = out.queue(SetForegroundColor(theme::accent()));
            let label = if self.running_procs == 1 {
                "1 proc".to_string()
            } else {
                format!("{} procs", self.running_procs)
            };
            let _ = out.queue(Print(&label));
            has_prev = true;
        }

        // ── Running agents ──
        if self.running_agents > 0 {
            if has_prev {
                let _ = out.queue(SetForegroundColor(theme::muted()));
                let _ = out.queue(Print(" · "));
            }
            let _ = out.queue(SetForegroundColor(theme::AGENT));
            let label = if self.running_agents == 1 {
                "1 agent".to_string()
            } else {
                format!("{} agents", self.running_agents)
            };
            let _ = out.queue(Print(&label));
        }

        // Fill the rest of the line with the dark bg
        let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
        let _ = out.queue(ResetColor);
    }

    /// Dismiss a dialog overlay.
    ///
    /// Clears from the dialog's anchor row down and lets the prompt redraw
    /// at that position on the next tick.
    pub fn clear_dialog_area(&mut self, dialog_anchor: Option<u16>) {
        let dialog_row = dialog_anchor.unwrap_or(0);

        // When the tool overlay was shown above the dialog and the dialog's
        // begin_dialog_draw used ScrollUp, the overlay was shifted upward
        // and now sits between the screen anchor and the dialog bar.
        // Extend the clear range to wipe the ghost.
        let clear_from = if self.show_tool_in_dialog {
            let screen_anchor = self.prompt.anchor_row.unwrap_or(dialog_row);
            screen_anchor.min(dialog_row)
        } else {
            dialog_row
        };

        let height = self.size().1;
        let mut out = self.scroll_output();
        for row in clear_from..height {
            let _ = out.queue(cursor::MoveTo(0, row));
            let _ = out.queue(terminal::Clear(terminal::ClearType::CurrentLine));
        }
        let _ = out.flush();
        // When the dialog used ScrollUp, the prompt gap that was between
        // the last block and the prompt was pushed into scrollback.  The
        // next block render would emit gap_between() again, creating a
        // double blank line (one in scrollback, one in visible).  Suppress
        // the leading gap when the anchor was pushed to row 0 — meaning
        // the previous block's trailing gap is now in scrollback.
        let screen_anchor = self.prompt.anchor_row.unwrap_or(dialog_row);
        if screen_anchor == 0 && self.has_scrollback {
            self.history.suppress_leading_gap = true;
        }
        self.defer_pending_render = true;
        self.defer_redraw = true;
        self.show_tool_in_dialog = false;
        // Only reset anchor/prev_rows when the dialog caused ScrollUp
        // (prompt was physically moved). For non-scrolled dialogs the
        // prompt is still in its original position — just mark dirty so
        // it redraws in place.
        let scrolled_by_dialog = screen_anchor == 0 && self.has_scrollback;
        if scrolled_by_dialog || self.prompt.anchor_row.is_none() {
            self.prompt.anchor_row = Some(clear_from);
            self.prompt.prev_rows = 0;
        }
        self.prompt.drawn = false;
        self.prompt.dirty = true;
        self.prompt.prev_dialog_row = None;
    }

    /// Move the cursor to the line after the prompt so the shell resumes cleanly.
    /// When `clear_below` is true, clears remaining rows (completions).
    pub fn move_cursor_past_prompt(&self, clear_below: bool) {
        if !self.prompt.drawn {
            return;
        }
        let anchor = self.prompt.anchor_row.unwrap_or(0);
        // prev_rows is the count of rows drawn, so the last drawn row is
        // anchor + prev_rows - 1.  Move there, then \r\n lands on the first
        // line after the prompt with no extra gap.
        let last_row = anchor + self.prompt.prev_rows.saturating_sub(1);
        let height = self.size().1;
        let mut out = self.scroll_output();
        let _ = out.queue(cursor::MoveTo(0, last_row.min(height.saturating_sub(1))));
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

    /// Gap before a thinking summary overlay, skipping over hidden thinking blocks.
    fn thinking_summary_gap(&self) -> u16 {
        if let Some(last) = self
            .history
            .blocks
            .iter()
            .rev()
            .find(|b| !matches!(b, Block::Thinking { .. }))
        {
            gap_between(
                &Element::Block(last),
                &Element::Block(&Block::Thinking {
                    content: String::new(),
                }),
            )
        } else if self.history.blocks.is_empty() {
            0
        } else {
            1
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
            // If inside an unclosed code block, check whether current_line
            // is the closing fence before committing it as a code line.
            if at.in_code_block.is_some() {
                if at.current_line.trim_start().starts_with("```") {
                    // Closing fence — just close the block, don't render it.
                    at.current_line.clear();
                } else if !at.current_line.is_empty() {
                    let lang = at.in_code_block.as_ref().unwrap().clone();
                    self.history.push(Block::CodeLine {
                        content: std::mem::take(&mut at.current_line),
                        lang,
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
                    tool.output = Some(Box::new(ToolOutput {
                        content: chunk.to_string(),
                        is_error: false,
                        metadata: None,
                        render_cache: None,
                    }));
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
                    *output = Some(Box::new(ToolOutput {
                        content: chunk.to_string(),
                        is_error: false,
                        metadata: None,
                        render_cache: None,
                    }));
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
        output: Option<ToolOutputRef>,
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
                call_id: call_id.to_string(),
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

    pub fn set_context_window(&mut self, window: u32) {
        self.context_window = Some(window);
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
        let w = self.size().0 as usize;
        let inter_tool_gap =
            blocks::gap_between(&blocks::Element::ActiveTool, &blocks::Element::ActiveTool);
        let mut total = gap;
        for (i, tool) in self.active_tools.iter().enumerate() {
            if i > 0 {
                total += inter_tool_gap;
            }
            let mut rows = blocks::tool_line_rows(&tool.name, &tool.summary, w);
            if tool.name == "web_fetch" {
                if let Some(prompt) = tool.args.get("prompt").and_then(|v| v.as_str()) {
                    rows += wrap_line(prompt, w.saturating_sub(4)).len() as u16;
                }
            }
            total += rows;
        }
        total
    }

    /// Returns whether the active tool overlay fits on screen above a
    /// dialog of the given height.
    ///
    /// The check is purely about physical space: can the tool overlay and
    /// dialog both fit within the terminal height? If yes, the content-only
    /// frame shows the tool and lets the dialog's `ScrollUp` handle
    /// positioning. If no (dialog is nearly full-screen), the tool is
    /// hidden to avoid it being pushed into scrollback as a ghost.
    pub fn tool_overlay_fits_with_dialog(&self, dialog_height: u16) -> bool {
        let (_width, height) = self.size();
        let active_rows = self.active_tool_rows();
        if active_rows == 0 {
            return true;
        }
        let gap: u16 = 1;
        active_rows + gap + dialog_height <= height
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

    pub fn set_dialog_open(&mut self, open: bool) {
        if open == self.dialog_open {
            return;
        }
        self.dialog_open = open;
        if open {
            self.working.pause();
        } else {
            self.working.resume();
        }
        self.prompt.dirty = true;
    }

    pub fn mark_dirty(&mut self) {
        self.prompt.dirty = true;
    }

    pub fn is_dirty(&self) -> bool {
        self.prompt.dirty || self.history.has_unflushed()
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
        self.finish_all_active_agents();
        for tool in self.active_tools.drain(..) {
            let elapsed = if status == ToolStatus::Denied {
                None
            } else {
                tool.elapsed()
            };
            self.history.push(Block::ToolCall {
                call_id: tool.call_id,
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
            || !self.active_agents.is_empty()
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
        let mut out = self.scroll_output();
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
                .unwrap_or_else(|| self.cursor_y())
        };
        let (w, h) = self.size();
        let block_rows = self
            .history
            .render(&mut out, w as usize, self.show_thinking);
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
    /// terminal lines cleared immediately within that frame. Avoid flushing
    /// here so the terminal can present the erase together with the
    /// subsequent dialog draw as a single synchronized update.
    pub fn erase_prompt_nosync(&mut self) {
        if self.prompt.drawn {
            if let Some(anchor) = self.prompt.anchor_row {
                let height = self.size().1;
                let end = (anchor + self.prompt.prev_rows).min(height);
                let mut out = self.scroll_output();
                let _ = out.queue(cursor::Hide);
                for r in anchor..end {
                    let _ = out.queue(cursor::MoveTo(0, r));
                    let _ = out.queue(terminal::Clear(terminal::ClearType::CurrentLine));
                }
                let _ = out.queue(cursor::MoveTo(0, anchor));
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
        let mut out = self.scroll_output();
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
        let (w, height) = self.size();
        let start_idx = self
            .history
            .redraw_start(MAX_REDRAW_LINES, self.show_thinking);
        self.history.flushed = start_idx;
        self.history.last_block_rows = 0;
        let block_rows = self
            .history
            .render(&mut out, w as usize, self.show_thinking);
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
        self.active_agents.clear();
        self.active_exec = None;
        self.prompt = PromptState::new();
        self.prompt.anchor_row = Some(0);
        self.working.clear();
        self.context_tokens = None;
        self.session_cost_usd = 0.0;
        self.task_label = None;
        self.has_scrollback = false;
        self.content_start_row = None;
        let mut out = self.scroll_output();
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

    pub fn export_render_cache(&self) -> Option<RenderCache> {
        None
    }

    pub fn import_render_cache(&mut self, _cache: RenderCache) {}

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
        self.active_agents.clear();
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

        if let Some(elapsed) = self.working.elapsed() {
            let frame = (elapsed.as_millis() / 150) as usize % SPINNER_FRAMES.len();
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

        let mut out = self.scroll_output();

        // ── Position cursor ─────────────────────────────────────────────
        let explicit_anchor = self.prompt.anchor_row.take();
        let draw_start_row = explicit_anchor.unwrap_or_else(|| self.cursor_y());

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
        let block_rows = self.history.render(&mut out, width, self.show_thinking);

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

        // Current thinking overlay.
        if let Some(ref at) = self.active_thinking {
            let text = match (at.paragraph.is_empty(), at.current_line.is_empty()) {
                (true, true) => String::new(),
                (true, false) => at.current_line.clone(),
                (false, true) => at.paragraph.clone(),
                (false, false) => format!("{}\n{}", at.paragraph, at.current_line),
            };
            if self.show_thinking {
                if !text.trim().is_empty() {
                    overlay_blocks.push(Block::Thinking { content: text });
                }
            } else {
                // Animated summary while streaming. Always render from
                // committed blocks even when active text is momentarily
                // empty (right after a paragraph commit) to avoid flicker.
                let mut combined = collect_trailing_thinking(&self.history.blocks);
                if !text.trim().is_empty() {
                    if !combined.is_empty() {
                        combined.push('\n');
                    }
                    combined.push_str(&text);
                }
                if !combined.is_empty() {
                    let (label, line_count) = thinking_summary(&combined);
                    let gap = self.thinking_summary_gap();
                    for _ in 0..gap {
                        crlf(&mut out);
                    }
                    let rows = render_thinking_summary(&mut out, width, &label, line_count, true);
                    streaming_rows += gap + rows;
                }
            }
        }

        // Current text overlay.
        if let Some(ref at) = self.active_text {
            let in_table =
                !at.table_rows.is_empty() || at.current_line.trim_start().starts_with('|');

            if in_table {
                let n = at.table_data_rows;
                let dots = animated_dots();
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
            let rows = blocks::render_block(&mut out, block, width, self.show_thinking);
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
                    &tool.call_id,
                    &tool.name,
                    &tool.summary,
                    &tool.args,
                    tool.status,
                    Some(tool.start_time.elapsed()),
                    tool.output.as_deref(),
                    tool.user_message.as_deref(),
                    width,
                );
                active_rows += tool_gap + rows;
            }
        }

        // ── Render active blocking agents ──────────────────────────
        if show_active {
            for (i, agent) in self.active_agents.iter().enumerate() {
                let agent_gap = if i > 0 || !self.active_tools.is_empty() {
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
                    self.show_thinking,
                );
                active_rows += agent_gap + rows;
            }
        }

        // ── Render active exec ──────────────────────────────────────
        if show_active {
            if let Some(ref exec) = self.active_exec {
                let exec_gap = if !self.active_agents.is_empty() || !self.active_tools.is_empty() {
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
                let height = self.size().1;
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
            let height = self.size().1;
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
        // Cache state for dialog-mode status line rendering.
        self.last_vim_enabled = state.vim_enabled();
        self.last_vim_mode = state.vim_mode();
        self.last_mode = mode;
        let usable = width.saturating_sub(2);
        let height = (self.size().1 as usize).saturating_sub(pre_prompt_rows as usize);
        let mut extra_rows = 0u16;
        let notification_rows = render_notification(out, self.notification.as_ref(), usable);
        extra_rows += notification_rows;
        let queued_visual = render_queued(out, queued, usable);
        extra_rows += queued_visual;
        let queued_rows = queued_visual as usize;
        let stash_rows = render_stash(out, &state.stash, usable);
        extra_rows += stash_rows;
        let term_h = self.size().1 as usize;
        let btw_visual = if let Some(ref mut btw) = self.btw {
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
                let token_text = if let Some(window) = self.context_window {
                    if window > 0 {
                        let pct = (tokens as f64 / window as f64 * 100.0) as u32;
                        format!(" {} ({}%)", format_tokens(tokens), pct)
                    } else {
                        format!(" {}", format_tokens(tokens))
                    }
                } else {
                    format!(" {}", format_tokens(tokens))
                };
                right_spans.push(BarSpan {
                    text: token_text,
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
        draw_bar(
            out,
            width,
            None,
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
            completion_reserved_rows(state.completer.as_ref())
        };
        let mut comp_rows = comp_total;

        // Reserve space for the status line (always shown when no completions/menus).
        let status_line_reserve: usize = if comp_total == 0 { 1 } else { 0 };

        let fixed_base = notification_rows as usize
            + stash_rows as usize
            + queued_rows
            + 2
            + status_line_reserve;
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

        let show_prediction = prediction.is_some() && state.buf.is_empty();
        if show_prediction {
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

        let has_scrollbar = total_content_rows > content_rows;
        let (thumb_start, thumb_end) = if has_scrollbar {
            let thumb_size = (content_rows * content_rows / total_content_rows).max(1);
            let max_scroll = total_content_rows - content_rows;
            let thumb_pos = if max_scroll > 0 {
                scroll_offset * (content_rows - thumb_size) / max_scroll
            } else {
                0
            };
            (thumb_pos, thumb_pos + thumb_size)
        } else {
            (0, 0)
        };

        for (li, (line, kinds)) in visual_lines
            .iter()
            .skip(scroll_offset)
            .take(if show_prediction { 0 } else { content_rows })
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
            if has_scrollbar {
                let bg = if li >= thumb_start && li < thumb_end {
                    theme::scrollbar_thumb()
                } else {
                    theme::scrollbar_track()
                };
                let _ = out.queue(cursor::MoveToColumn(width as u16 - 1));
                let _ = out.queue(SetBackgroundColor(bg));
                let _ = out.queue(Print(" "));
                let _ = out.queue(ResetColor);
            }
            let _ = out.queue(Print("\r\n"));
        }

        draw_bar(out, width, None, None, bar_color);

        // Status line below the prompt:
        // pill(spinner+slug) mode vim_mode · status time · speed · procs · agents
        let status_line_rows = if comp_rows == 0 {
            let _ = out.queue(Print("\r\n"));
            self.render_status_line(out);
            1
        } else {
            0
        };

        if comp_rows > 0 {
            let _ = out.queue(Print("\r\n"));
        }
        let comp_rows = if let Some(ref ms) = state.menu {
            draw_menu(out, ms, comp_rows)
        } else {
            draw_completions(
                out,
                state.completer.as_ref(),
                comp_rows,
                state.vim_enabled(),
            )
        };

        let total_rows = notification_rows as usize
            + stash_rows as usize
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
        let height = self.size().1;
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
        Some(crate::vim::ViMode::Insert) => Some("INSERT"),
        Some(crate::vim::ViMode::Visual) => Some("VISUAL"),
        Some(crate::vim::ViMode::VisualLine) => Some("VISUAL LINE"),
        _ => None,
    }
}

fn render_notification(
    out: &mut RenderOut,
    notification: Option<&Notification>,
    usable: usize,
) -> u16 {
    let Some(notification) = notification else {
        return 0;
    };

    let label = if notification.is_error {
        "error"
    } else {
        "info"
    };
    let max_msg = usable.saturating_sub(label.len() + 3);

    let _ = out.queue(Print(" "));
    if notification.is_error {
        let _ = out.queue(SetForegroundColor(theme::ERROR));
    }
    let _ = out.queue(SetAttribute(Attribute::Bold));
    let _ = out.queue(Print(label));
    let _ = out.queue(SetAttribute(Attribute::Reset));
    let _ = out.queue(ResetColor);
    let _ = out.queue(Print("  "));

    let msg: String = notification.message.chars().take(max_msg).collect();
    let _ = out.queue(SetAttribute(Attribute::Dim));
    let _ = out.queue(Print(&msg));
    let _ = out.queue(SetAttribute(Attribute::Reset));
    let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
    let _ = out.queue(Print("\r\n"));
    1
}

fn render_stash(out: &mut RenderOut, stash: &Option<InputSnapshot>, usable: usize) -> u16 {
    let Some(_) = stash else {
        return 0;
    };
    let text = "› Stashed (ctrl+s to unstash)";
    let display: String = text.chars().take(usable).collect();
    let _ = out.queue(Print("  "));
    let _ = out.queue(SetAttribute(Attribute::Dim));
    let _ = out.queue(SetForegroundColor(theme::muted()));
    let _ = out.queue(Print(display));
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
    let prompt_col = 1usize;

    for text_line in buf.split('\n') {
        let chars: Vec<char> = text_line.chars().collect();
        if chars.is_empty() {
            push_visual_line(
                &mut visual_lines,
                &mut cursor_line,
                &mut cursor_col,
                &mut cursor_set,
                chars_seen,
                &[],
                &[],
                cursor_char,
                true,
                prompt_col,
            );
            chars_seen += 1;
            continue;
        }

        let mut line_chars: Vec<char> = Vec::new();
        let mut line_kinds: Vec<SpanKind> = Vec::new();
        let mut line_width = 0usize;
        let mut line_start = chars_seen;
        let mut last_break: Option<usize> = None;
        let mut i = 0usize;

        while i < chars.len() {
            let ch = chars[i];
            let kind = char_kinds
                .get(chars_seen + i)
                .copied()
                .unwrap_or(SpanKind::Plain);
            let ch_width = display_char_width(ch, prompt_col + line_width);

            if !line_chars.is_empty() && line_width + ch_width > max_col {
                if let Some(break_idx) = last_break {
                    let carry_chars = line_chars.split_off(break_idx);
                    let carry_kinds = line_kinds.split_off(break_idx);
                    push_visual_line(
                        &mut visual_lines,
                        &mut cursor_line,
                        &mut cursor_col,
                        &mut cursor_set,
                        line_start,
                        &line_chars,
                        &line_kinds,
                        cursor_char,
                        false,
                        prompt_col,
                    );
                    line_start += break_idx;
                    line_chars = carry_chars;
                    line_kinds = carry_kinds;
                    line_width = display_width(&line_chars, prompt_col);
                    last_break = line_chars
                        .iter()
                        .rposition(|&c| c == ' ')
                        .map(|idx| idx + 1);
                } else {
                    push_visual_line(
                        &mut visual_lines,
                        &mut cursor_line,
                        &mut cursor_col,
                        &mut cursor_set,
                        line_start,
                        &line_chars,
                        &line_kinds,
                        cursor_char,
                        false,
                        prompt_col,
                    );
                    line_start += line_chars.len();
                    line_chars.clear();
                    line_kinds.clear();
                    line_width = 0;
                    last_break = None;
                }
                continue;
            }

            line_chars.push(ch);
            line_kinds.push(kind);
            line_width += ch_width;
            if ch == ' ' {
                last_break = Some(line_chars.len());
            }
            i += 1;
        }

        push_visual_line(
            &mut visual_lines,
            &mut cursor_line,
            &mut cursor_col,
            &mut cursor_set,
            line_start,
            &line_chars,
            &line_kinds,
            cursor_char,
            true,
            prompt_col,
        );
        chars_seen += chars.len() + 1;
    }
    if visual_lines.is_empty() {
        visual_lines.push((String::new(), Vec::new()));
    }
    (visual_lines, cursor_line, cursor_col)
}

#[allow(clippy::too_many_arguments)]
fn push_visual_line(
    visual_lines: &mut Vec<(String, Vec<SpanKind>)>,
    cursor_line: &mut usize,
    cursor_col: &mut usize,
    cursor_set: &mut bool,
    start_char: usize,
    line_chars: &[char],
    line_kinds: &[SpanKind],
    cursor_char: usize,
    is_last_chunk: bool,
    prompt_col: usize,
) {
    let end_char = start_char + line_chars.len();
    if !*cursor_set
        && cursor_char >= start_char
        && (cursor_char < end_char || (is_last_chunk && cursor_char == end_char))
    {
        *cursor_line = visual_lines.len();
        *cursor_col = display_width(&line_chars[..cursor_char - start_char], prompt_col);
        *cursor_set = true;
    }
    visual_lines.push((line_chars.iter().collect(), line_kinds.to_vec()));
}

fn display_width(chars: &[char], start_col: usize) -> usize {
    let mut col = start_col;
    for &ch in chars {
        col += display_char_width(ch, col);
    }
    col.saturating_sub(start_col)
}

fn display_char_width(ch: char, col: usize) -> usize {
    if ch == '\t' {
        let tab_stop = 8usize;
        tab_stop - (col % tab_stop)
    } else {
        UnicodeWidthChar::width(ch).unwrap_or(0)
    }
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
                    inner + 1
                } else {
                    0
                } // spans + trailing space
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
        left_filtered
            .iter()
            .map(|s| s.text.chars().count())
            .sum::<usize>()
            + 1 // trailing space
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

fn completion_layout(completer: &crate::completer::Completer) -> (usize, usize, usize) {
    let show_hints = completer.kind == crate::completer::CompleterKind::Settings;
    let hint_rows = usize::from(show_hints) * 2;
    let empty_gap = usize::from(show_hints);
    let list_rows = completer.max_visible_rows();
    (list_rows, hint_rows, empty_gap)
}

fn completion_reserved_rows(completer: Option<&crate::completer::Completer>) -> usize {
    let Some(comp) = completer else {
        return 0;
    };
    let (list_rows, hint_rows, empty_gap) = completion_layout(comp);
    if list_rows == 0 {
        return 0;
    }
    list_rows + hint_rows + empty_gap
}

fn draw_completions(
    out: &mut RenderOut,
    completer: Option<&crate::completer::Completer>,
    max_rows: usize,
    vim_enabled: bool,
) -> usize {
    use crate::completer::CompleterKind;

    let Some(comp) = completer else {
        return 0;
    };
    if max_rows == 0 {
        return 0;
    }

    let (_, hint_rows, empty_gap) = completion_layout(comp);
    let show_hints = hint_rows > 0;
    let list_rows = max_rows.saturating_sub(hint_rows + empty_gap);

    if comp.results.is_empty() {
        if comp.is_picker() {
            let _ = out.queue(SetAttribute(Attribute::Dim));
            let _ = out.queue(Print("  no results"));
            let _ = out.queue(SetAttribute(Attribute::Reset));
            let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
            if show_hints && max_rows > hint_rows + empty_gap {
                let _ = out.queue(Print("\r\n"));
                draw_settings_hints(out, vim_enabled);
                return 1 + empty_gap + hint_rows;
            }
            return 1;
        }
        return 0;
    }
    if list_rows == 0 {
        return 0;
    }
    let total = comp.results.len();
    let visible_rows = list_rows.min(total);
    let mut start = 0;
    if total > visible_rows {
        let half = visible_rows / 2;
        start = comp.selected.saturating_sub(half);
        if start + visible_rows > total {
            start = total - visible_rows;
        }
    }
    let end = start + visible_rows;
    let last = visible_rows - 1;

    let is_color_picker = matches!(comp.kind, CompleterKind::Theme | CompleterKind::Color);

    let prefix = match comp.kind {
        CompleterKind::Command => "/",
        CompleterKind::File => "./",
        _ => "",
    };
    let max_label = comp
        .results
        .iter()
        .map(|i| prefix.len() + i.label.len())
        .max()
        .unwrap_or(0);
    let avail = term_width().saturating_sub(4);

    let mut drawn = 0;
    for (i, item) in comp.results[start..end].iter().enumerate() {
        let idx = start + i;
        let selected = idx == comp.selected;
        let raw = format!("{}{}", prefix, item.label);
        let label: String = raw.chars().take(avail).collect();

        if is_color_picker {
            let _ = out.queue(Print("  "));
            if selected {
                let ansi = item.ansi_color.unwrap_or(theme::accent_value());
                let _ = out.queue(SetForegroundColor(Color::AnsiValue(ansi)));
                let _ = out.queue(Print(format!("● {}", label)));
                let _ = out.queue(ResetColor);
            } else {
                let _ = out.queue(SetAttribute(Attribute::Dim));
                let _ = out.queue(Print(format!("  {}", label)));
                let _ = out.queue(SetAttribute(Attribute::Reset));
            }
            if let Some(ref desc) = item.description {
                let pad = (max_label + 2).saturating_sub(label.len());
                let _ = out.queue(SetAttribute(Attribute::Dim));
                let _ = out.queue(Print(format!("{:>width$}{}", "", desc, width = pad)));
                let _ = out.queue(SetAttribute(Attribute::Reset));
            }
        } else {
            let _ = out.queue(Print("  "));
            if selected {
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
        }

        drawn += 1;
        let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
        if i < last || show_hints {
            let _ = out.queue(Print("\r\n"));
        }
    }

    if show_hints && drawn < max_rows {
        draw_settings_hints(out, vim_enabled);
        drawn += 2;
    }

    drawn
}

fn draw_settings_hints(out: &mut RenderOut, vim_enabled: bool) {
    let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
    let _ = out.queue(Print("\r\n"));
    let _ = out.queue(SetAttribute(Attribute::Dim));
    let _ = out.queue(Print(crate::keymap::hints::join(&[
        crate::keymap::hints::picker_nav(vim_enabled),
        "enter/space: toggle",
        crate::keymap::hints::CANCEL,
    ])));
    let _ = out.queue(SetAttribute(Attribute::Reset));
    let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
}

fn draw_menu(out: &mut RenderOut, ms: &crate::input::MenuState, max_rows: usize) -> usize {
    if max_rows == 0 {
        return 0;
    }
    if let crate::input::MenuKind::Stats { left, right } = &ms.kind {
        return draw_stats(out, left, right, max_rows);
    }
    if let crate::input::MenuKind::Cost { lines } = &ms.kind {
        return draw_stats_sequential(out, lines, 0, max_rows);
    }
    0
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
    fn prompt_cursor_uses_tab_display_width() {
        let kinds = vec![SpanKind::Plain; "a\tb".chars().count()];
        let (_, cursor_line, cursor_col) = wrap_and_locate_cursor("a\tb", &kinds, 3, 80);
        assert_eq!(cursor_line, 0);
        assert_eq!(cursor_col, 8);
    }

    #[test]
    fn prompt_cursor_tracks_multiple_tabs_from_prompt_column() {
        let kinds = vec![SpanKind::Plain; "\t\tb".chars().count()];
        let (_, cursor_line, cursor_col) = wrap_and_locate_cursor("\t\tb", &kinds, 3, 80);
        assert_eq!(cursor_line, 0);
        assert_eq!(cursor_col, 16);
    }

    #[test]
    fn prompt_cursor_uses_wide_char_display_width() {
        let kinds = vec![SpanKind::Plain; "a界b".chars().count()];
        let (_, cursor_line, cursor_col) = wrap_and_locate_cursor("a界b", &kinds, 3, 80);
        assert_eq!(cursor_line, 0);
        assert_eq!(cursor_col, 4);
    }

    #[test]
    fn prompt_tabs_respect_prompt_column_without_forced_wrap() {
        let kinds = vec![SpanKind::Plain; "a\tb".chars().count()];
        let (lines, cursor_line, cursor_col) = wrap_and_locate_cursor("a\tb", &kinds, 3, 8);
        assert_eq!(
            lines.iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>(),
            vec!["a\tb"]
        );
        assert_eq!(cursor_line, 0);
        assert_eq!(cursor_col, 8);
    }

    #[test]
    fn settings_empty_results_leave_blank_line_before_hints() {
        let state = crate::input::SettingsState {
            vim: true,
            auto_compact: false,
            show_tps: true,
            show_tokens: true,
            show_cost: true,
            show_prediction: true,
            show_slug: true,
            show_thinking: true,
            restrict_to_workspace: false,
        };
        let mut comp = crate::completer::Completer::settings(&state);
        comp.update_query("zzzzzz".into());
        let mut out = RenderOut::buffer();
        let rows = draw_completions(
            &mut out,
            Some(&comp),
            completion_reserved_rows(Some(&comp)),
            true,
        );
        let rendered = String::from_utf8(out.into_bytes()).unwrap();
        assert!(rows >= 4);
        assert!(
            rendered.contains("no results")
                && rendered.contains("\r\n\u{1b}[K\r\n")
                && rendered.contains("ctrl+j/k: navigate"),
            "rendered: {rendered:?}"
        );
    }

    #[test]
    fn completion_reserved_rows_is_stable_for_settings_filtering() {
        let state = crate::input::SettingsState {
            vim: true,
            auto_compact: false,
            show_tps: true,
            show_tokens: true,
            show_cost: true,
            show_prediction: true,
            show_slug: true,
            show_thinking: true,
            restrict_to_workspace: false,
        };
        let mut comp = crate::completer::Completer::settings(&state);
        let rows_before = completion_reserved_rows(Some(&comp));
        comp.update_query("thi".into());
        let rows_after = completion_reserved_rows(Some(&comp));
        assert_eq!(rows_before, rows_after);
    }

    #[test]
    fn hidden_thinking_keeps_gap_above_summary() {
        let mut history = BlockHistory::new();
        history.push(Block::Text {
            content: "hello".into(),
        });
        history.push(Block::Thinking {
            content: "alpha\nbeta".into(),
        });

        let mut out = RenderOut::buffer();
        history.render(&mut out, 80, false);
        let rendered = String::from_utf8(out.into_bytes()).unwrap();
        assert!(rendered.contains("hello"));
        assert!(rendered.contains("thinking (2 lines)"));
        assert!(
            rendered.contains("\r\n\u{1b}[K\r\n") || rendered.contains("\n\n"),
            "rendered: {rendered:?}"
        );
    }

    #[test]
    fn export_render_cache_is_empty() {
        let mut screen = Screen::new();
        screen.push(Block::Thinking {
            content: "alpha\nbeta".into(),
        });
        screen.history.render(&mut RenderOut::buffer(), 80, true);
        assert!(screen.export_render_cache().is_none());
    }
}
