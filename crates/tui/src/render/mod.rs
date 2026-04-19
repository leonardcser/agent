mod blocks;
mod cache;
pub(crate) mod cmdline;
mod completions;
mod context;
mod dialogs;
mod display;
mod highlight;
mod history;
pub(crate) mod layout;
mod layout_out;
mod paint;
mod prompt;
mod region;
mod screen;
mod scrollbar;
mod selection;
mod status;
pub(crate) mod transcript;
mod viewport;
mod working;

pub use cmdline::CmdlineState;
pub(crate) use layout::HitRegion;
pub(crate) use region::{ScrollbarGeom, ViewportHit};
pub use screen::{BtwBlock, ContentVisualKind, ContentVisualRange, Notification, Screen};
pub use transcript::{SnapshotCell, TranscriptSnapshot};
pub use viewport::ViewportGeom;

pub use history::{
    ActiveAgent, ActiveTool, AgentBlockStatus, ApprovalScope, Block, BlockArtifact, BlockId,
    ConfirmChoice, ConfirmRequest, LayoutKey, ResumeEntry, Status, Throbber, ToolOutput,
    ToolOutputRef, ToolState, ToolStatus, ViewState,
};

pub(crate) use selection::{scan_at_token, truncate_str, try_at_ref, wrap_line};

pub use status::StatusPosition;
pub(crate) use status::{draw_bar, BarSpan};

pub use dialogs::{
    parse_questions, AgentSnapshot, AgentsDialog, ConfirmDialog, Dialog, DialogResult,
    ExportDialog, ExportTarget, HelpDialog, PermissionEntry, PermissionsDialog, PsDialog, Question,
    QuestionDialog, QuestionOption, ResumeDialog, RewindDialog, SharedSnapshots,
};

/// Layout placement computed by `draw_frame` for the active dialog.
pub struct DialogPlacement {
    pub row: u16,
    pub granted_rows: u16,
}

use crate::input::InputState;
use crate::theme;
use crate::utils::format_duration;
use crossterm::{
    cursor,
    style::{
        Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
    },
    terminal, QueueableCommand,
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{self, Write};

pub use context::{LayoutContext, PaintContext};
pub use display::DisplayBlock;
pub use highlight::warm_up_syntect;

pub use cache::{
    build_tool_output_render_cache, session_render_hash, CachedNotebookEdit, PersistedLayoutCache,
    RenderCache, ToolOutputRenderCache, LAYOUT_CACHE_VERSION, RENDER_CACHE_VERSION,
};

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

/// Tracked terminal style state for diff-based SGR emission.
#[derive(Clone, Default, PartialEq)]
pub struct StyleState {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub crossedout: bool,
    pub underline: bool,
}

/// RAII guard for a synchronized terminal update frame.
///
/// Creating a `Frame` issues `BeginSynchronizedUpdate`.
/// Dropping it issues `EndSynchronizedUpdate` and flushes the buffer,
/// guaranteeing that the terminal paints everything as a single atomic
/// update — even if the caller forgets to close the frame explicitly.
///
/// Cursor visibility is NOT managed by `Frame` — callers that need to
/// hide/show the cursor should queue those commands explicitly.
pub struct Frame {
    pub out: RenderOut,
}

impl Frame {
    pub fn begin(backend: &dyn TerminalBackend) -> Self {
        let mut out = backend.make_output();
        let _ = out.queue(terminal::BeginSynchronizedUpdate);
        Self { out }
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        let _perf = crate::perf::begin("frame:flush");
        // Neutralize SGR before handing the terminal back. A new `Frame`
        // starts with `current = StyleState::default()`, so the terminal
        // must actually match that — otherwise attributes like dim/italic
        // from the last painted span bleed into the first span of the next
        // frame (e.g. a dialog opened after a thinking block under Ctrl+L).
        // Emitted inside the synchronized-update envelope, so no flicker.
        if self.out.current != StyleState::default() {
            let _ = self.out.queue(SetAttribute(Attribute::Reset));
            let _ = self.out.queue(ResetColor);
            self.out.current = StyleState::default();
        }
        let _ = self.out.queue(terminal::EndSynchronizedUpdate);
        let bytes = self.out.bytes_queued;
        {
            let _p = crate::perf::begin("frame:write_all");
            let _ = self.out.flush();
        }
        self.out.bytes_queued = 0;
        // Record the payload size so the perf summary can show a
        // distribution of how many bytes each frame pushed to the TTY.
        crate::perf::record_value("frame:bytes", bytes as u64);
    }
}

impl std::ops::Deref for Frame {
    type Target = RenderOut;
    fn deref(&self) -> &RenderOut {
        &self.out
    }
}

impl std::ops::DerefMut for Frame {
    fn deref_mut(&mut self) -> &mut RenderOut {
        &mut self.out
    }
}

// 1 MiB is enough for any realistic full redraw payload (~640 KB for a
// 360-block session at 120 cols). `bin/term_bench` confirms there's no
// measurable gain from a larger buffer.
const STDOUT_BUF_CAPACITY: usize = 1 << 20;

thread_local! {
    /// Stashed buffer returned by a dropped `PooledBufWriter`, reused by the
    /// next frame's writer instead of re-allocating 1 MiB every paint.
    static BUFFER_POOL: RefCell<Option<Vec<u8>>> = const { RefCell::new(None) };
}

/// `BufWriter` analogue that recycles its 1 MiB backing buffer across frames
/// via a thread-local pool. Behaviour matches `std::io::BufWriter`: write
/// into the buffer, flush when it's full, pass large writes straight through.
pub struct PooledBufWriter<W: Write> {
    inner: W,
    buf: Vec<u8>,
}

impl<W: Write> PooledBufWriter<W> {
    pub fn new(inner: W) -> Self {
        let buf = BUFFER_POOL
            .with(|p| p.borrow_mut().take())
            .unwrap_or_else(|| Vec::with_capacity(STDOUT_BUF_CAPACITY));
        Self { inner, buf }
    }

    fn flush_buf(&mut self) -> io::Result<()> {
        if !self.buf.is_empty() {
            self.inner.write_all(&self.buf)?;
            self.buf.clear();
        }
        Ok(())
    }
}

impl<W: Write> Write for PooledBufWriter<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.buf.len() + data.len() > self.buf.capacity() {
            self.flush_buf()?;
        }
        if data.len() >= self.buf.capacity() {
            self.inner.write(data)
        } else {
            self.buf.extend_from_slice(data);
            Ok(data.len())
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_buf()?;
        self.inner.flush()
    }
}

impl<W: Write> Drop for PooledBufWriter<W> {
    fn drop(&mut self) {
        let _ = self.flush_buf();
        let mut buf = std::mem::take(&mut self.buf);
        // Only return buffers that match the pool's target capacity — a
        // shrunk or oversized buffer defeats the pool's purpose.
        if buf.capacity() == STDOUT_BUF_CAPACITY {
            buf.clear();
            BUFFER_POOL.with(|p| {
                let mut slot = p.borrow_mut();
                if slot.is_none() {
                    *slot = Some(buf);
                }
            });
        }
    }
}

/// Output wrapper that selects the line-advance strategy (scroll vs overlay).
pub struct RenderOut {
    pub out: Box<dyn Write>,
    pub row: Option<u16>,
    capture: Option<std::sync::Arc<std::sync::Mutex<Vec<u8>>>>,
    /// Current terminal style (what the terminal is actually showing).
    current: StyleState,
    /// Stack of saved styles for push/pop scoping.
    stack: Vec<StyleState>,
    /// Visible columns printed on the current row since the last
    /// newline. Tracked by the `LayoutSink` impl so dialog code that
    /// shares helpers with block renderers can fill rows to the
    /// terminal edge.
    pub(super) line_cols: u16,
    /// Running count of bytes queued since the last `flush()`. Read by
    /// `Frame::drop` for bench instrumentation, then reset on flush.
    pub(super) bytes_queued: usize,
    /// Tracked cursor row. Updated by every cursor-moving operation
    /// (`\r\n`, `MoveTo`, `ScrollUp`, `newline`). Eliminates all derived
    /// cursor-position approximations in `draw_frame` — the row is
    /// always ground truth.
    pub(super) cursor_row: u16,
    /// Terminal height for clamping cursor_row during scroll-mode
    /// `\r\n` (the cursor can't exceed term_h - 1).
    pub(super) term_height: u16,
    /// Cached terminal width; see `newline` for the DECAWM rationale.
    pub(super) term_width: u16,
}

impl RenderOut {
    fn new(
        out: Box<dyn Write>,
        capture: Option<std::sync::Arc<std::sync::Mutex<Vec<u8>>>>,
    ) -> Self {
        Self {
            out,
            row: None,
            capture,
            current: StyleState::default(),
            stack: Vec::new(),
            line_cols: 0,
            bytes_queued: 0,
            cursor_row: 0,
            term_height: 0,
            term_width: 0,
        }
    }

    /// Create a scroll-mode output (for blocks + prompt).
    /// Dialogs switch to overlay mode by setting `out.row = Some(r)`.
    pub fn scroll() -> Self {
        Self::new(Box::new(PooledBufWriter::new(io::stdout())), None)
    }

    /// Create a scroll-mode output writing to a shared buffer (for testing).
    pub fn shared_sink(sink: std::sync::Arc<std::sync::Mutex<Vec<u8>>>) -> Self {
        Self::new(Box::new(SharedWriter(sink)), None)
    }

    /// Create a render output that writes to an in-memory buffer.
    pub fn buffer() -> Self {
        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        Self::new(Box::new(SharedWriter(buf.clone())), Some(buf))
    }

    /// Extract captured bytes (only valid after `buffer()`).
    pub fn into_bytes(self) -> Vec<u8> {
        drop(self.out);
        self.capture
            .and_then(|arc| std::sync::Arc::try_unwrap(arc).ok())
            .and_then(|m| m.into_inner().ok())
            .unwrap_or_default()
    }

    // ── Style stack ──────────────────────────────────────────────────

    /// Push the current style onto the stack and apply a new style on top.
    /// `None` fields in the target inherit from the current style.
    /// Only emits SGR sequences for fields that actually change.
    pub fn push_style(&mut self, target: StyleState) {
        self.stack.push(self.current.clone());
        self.emit_diff(&target);
        self.current = target;
    }

    /// Pop back to the previous style. Only emits SGR for what differs.
    pub fn pop_style(&mut self) {
        if let Some(prev) = self.stack.pop() {
            self.emit_diff(&prev);
            self.current = prev;
        }
    }

    /// Push a scope that only changes foreground color.
    pub fn push_fg(&mut self, color: Color) {
        let mut target = self.current.clone();
        target.fg = Some(color);
        self.push_style(target);
    }

    /// Push a scope that only changes background color.
    pub fn push_bg(&mut self, color: Color) {
        let mut target = self.current.clone();
        target.bg = Some(color);
        self.push_style(target);
    }

    /// Push a scope that adds bold.
    pub fn push_bold(&mut self) {
        let mut target = self.current.clone();
        target.bold = true;
        self.push_style(target);
    }

    /// Push a scope that adds dim.
    pub fn push_dim(&mut self) {
        let mut target = self.current.clone();
        target.dim = true;
        self.push_style(target);
    }

    /// Push a scope that adds italic.
    pub fn push_italic(&mut self) {
        let mut target = self.current.clone();
        target.italic = true;
        self.push_style(target);
    }

    /// Push a scope that adds crossedout.
    pub fn push_crossedout(&mut self) {
        let mut target = self.current.clone();
        target.crossedout = true;
        self.push_style(target);
    }

    /// Push a scope that adds dim + italic.
    pub fn push_dim_italic(&mut self) {
        let mut target = self.current.clone();
        target.dim = true;
        target.italic = true;
        self.push_style(target);
    }

    // ── Direct style setters (no stack, for incremental updates) ─────

    pub fn set_fg(&mut self, color: Color) {
        if self.current.fg != Some(color) {
            self.current.fg = Some(color);
            let _ = self.queue(SetForegroundColor(color));
        }
    }

    pub fn set_bg(&mut self, color: Color) {
        if self.current.bg != Some(color) {
            self.current.bg = Some(color);
            let _ = self.queue(SetBackgroundColor(color));
        }
    }

    /// Update only the background slot — keeping fg and all attributes
    /// untouched — and emit a single SGR command for the change. Used by
    /// the paint stage to set / clear bg around end-of-line padding
    /// without cloning the full `StyleState`.
    pub fn set_bg_only(&mut self, color: Option<Color>) {
        if self.current.bg == color {
            return;
        }
        self.current.bg = color;
        let _ = match color {
            Some(c) => self.queue(SetBackgroundColor(c)),
            None => self.queue(SetBackgroundColor(Color::Reset)),
        };
    }

    pub fn set_bold(&mut self) {
        if !self.current.bold {
            self.current.bold = true;
            let _ = self.queue(SetAttribute(Attribute::Bold));
        }
    }

    pub fn set_dim(&mut self) {
        if !self.current.dim {
            self.current.dim = true;
            let _ = self.queue(SetAttribute(Attribute::Dim));
        }
    }

    pub fn set_italic(&mut self) {
        if !self.current.italic {
            self.current.italic = true;
            let _ = self.queue(SetAttribute(Attribute::Italic));
        }
    }

    pub fn set_dim_italic(&mut self) {
        self.set_dim();
        self.set_italic();
    }

    /// Reset all style to terminal defaults.
    pub fn reset_style(&mut self) {
        let clean = StyleState::default();
        if self.current != clean {
            let _ = self.queue(SetAttribute(Attribute::Reset));
            let _ = self.queue(ResetColor);
            self.current = clean;
        }
    }

    // ── Cursor-tracking helpers ───────────────────────────────────

    /// Initialize cursor tracking at the start of a frame.
    pub(super) fn init_cursor(&mut self, row: u16, term_width: u16, term_height: u16) {
        self.cursor_row = row;
        self.term_width = term_width;
        self.term_height = term_height;
    }

    /// Queue a MoveTo and update the tracked cursor row.
    pub(super) fn move_to(&mut self, col: u16, row: u16) {
        let _ = self.queue(cursor::MoveTo(col, row));
        self.cursor_row = row;
        self.line_cols = col;
    }

    /// Emit visible text and keep `line_cols` in sync. This is the sole
    /// supported way to write printable content through a `RenderOut` —
    /// raw `queue(Print(...))` calls bypass column tracking and can leave
    /// `newline`'s last-column guard stale. Escape sequences (SGR, cursor
    /// moves, clears) should continue to go through `queue`.
    pub fn print(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.line_cols = self
            .line_cols
            .saturating_add(crate::render::layout_out::display_width(text) as u16);
        let _ = self.queue(Print(text));
    }

    /// Formatted variant of [`print`] for `Display` values that aren't
    /// already `&str`. Allocates; prefer `print(&str)` when possible.
    pub fn print_fmt<D: std::fmt::Display>(&mut self, text: D) {
        self.print(&text.to_string());
    }

    // ── Newline helpers ─────────────────────────────────────────────

    /// Advance to the next line, clearing trailing residue on the
    /// current line.
    ///
    /// - **Overlay mode** (`row` is `Some`): emits `MoveTo(0, row+1)` so
    ///   position stays exact without relying on terminal linefeed behaviour.
    ///   This is the only interactive path.
    /// - **Scroll mode** (`row` is `None`): emits `\r\n`, retained for
    ///   headless/test callers that write into a buffer without cursor state.
    pub fn newline(&mut self) {
        // Skip `Clear::UntilNewLine` when the row was painted to the full
        // terminal width. Issuing `CSI K` with the cursor sitting on the
        // last-painted column erases that glyph on some terminals
        // (Terminal.app, iTerm2, some Alacritty builds) — there is nothing
        // to clear anyway, so we just drop the sequence. Otherwise EL runs
        // to clear trailing residue from the previous frame.
        if self.term_width == 0 || self.line_cols < self.term_width {
            let _ = self.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
        }
        if let Some(r) = &mut self.row {
            *r += 1;
            let next = *r;
            let _ = self.queue(cursor::MoveTo(0, next));
            self.cursor_row = next;
        } else {
            let _ = self.queue(Print("\r\n"));
            if self.term_height > 0 {
                self.cursor_row = self.cursor_row.saturating_add(1).min(self.term_height - 1);
            }
        }
        self.line_cols = 0;
    }

    /// Diff to `target` and replace `current` without growing the
    /// push/pop stack. Used by the paint stage which emits flat
    /// sequences rather than nested style scopes.
    pub fn set_state(&mut self, target: StyleState) {
        if self.current == target {
            return;
        }
        self.emit_diff(&target);
        self.current = target;
    }

    // ── Internal diff engine ─────────────────────────────────────────

    /// Emit the minimal SGR sequences to transition from `self.current` to `target`.
    fn emit_diff(&mut self, target: &StyleState) {
        // Attributes being turned OFF require special handling.
        // Bold/dim share NormalIntensity (SGR 22) — turning off either
        // turns off both, so we may need to re-enable the other.
        let need_unbold = self.current.bold && !target.bold;
        let need_undim = self.current.dim && !target.dim;
        let need_unitalic = self.current.italic && !target.italic;
        let need_uncrossed = self.current.crossedout && !target.crossedout;
        let need_ununderline = self.current.underline && !target.underline;

        // Check if a full reset is cheaper than individual unsets.
        // A reset is 1 sequence vs potentially many unsets + re-sets.
        let unsets = need_unbold as u8
            + need_undim as u8
            + need_unitalic as u8
            + need_uncrossed as u8
            + need_ununderline as u8;
        let fg_change = self.current.fg != target.fg;
        let bg_change = self.current.bg != target.bg;

        // NormalIntensity (SGR 22) kills both bold AND dim. If we need to
        // turn off bold but keep dim (or vice versa), we'd need to re-emit
        // the one we want to keep. Count that cost.
        let intensity_conflict = (need_unbold && target.dim) || (need_undim && target.bold);

        if unsets >= 2 || intensity_conflict {
            // Full reset is simpler.
            let _ = self.queue(SetAttribute(Attribute::Reset));
            let _ = self.queue(ResetColor);

            // Re-apply everything the target wants.
            if let Some(fg) = target.fg {
                let _ = self.queue(SetForegroundColor(fg));
            }
            if let Some(bg) = target.bg {
                let _ = self.queue(SetBackgroundColor(bg));
            }
            if target.bold {
                let _ = self.queue(SetAttribute(Attribute::Bold));
            }
            if target.dim {
                let _ = self.queue(SetAttribute(Attribute::Dim));
            }
            if target.italic {
                let _ = self.queue(SetAttribute(Attribute::Italic));
            }
            if target.crossedout {
                let _ = self.queue(SetAttribute(Attribute::CrossedOut));
            }
            if target.underline {
                let _ = self.queue(SetAttribute(Attribute::Underlined));
            }
            return;
        }

        // Individual transitions — only emit what changed.

        // Bold/dim: NormalIntensity unsets both.
        if need_unbold || need_undim {
            let _ = self.queue(SetAttribute(Attribute::NormalIntensity));
            // Re-enable the one we want to keep (if any).
            if need_unbold && target.dim {
                let _ = self.queue(SetAttribute(Attribute::Dim));
            }
            if need_undim && target.bold {
                let _ = self.queue(SetAttribute(Attribute::Bold));
            }
        }
        if need_unitalic {
            let _ = self.queue(SetAttribute(Attribute::NoItalic));
        }
        if need_uncrossed {
            let _ = self.queue(SetAttribute(Attribute::NotCrossedOut));
        }
        if need_ununderline {
            let _ = self.queue(SetAttribute(Attribute::NoUnderline));
        }

        // Attributes being turned ON.
        if !self.current.bold && target.bold {
            let _ = self.queue(SetAttribute(Attribute::Bold));
        }
        if !self.current.dim && target.dim {
            let _ = self.queue(SetAttribute(Attribute::Dim));
        }
        if !self.current.italic && target.italic {
            let _ = self.queue(SetAttribute(Attribute::Italic));
        }
        if !self.current.crossedout && target.crossedout {
            let _ = self.queue(SetAttribute(Attribute::CrossedOut));
        }
        if !self.current.underline && target.underline {
            let _ = self.queue(SetAttribute(Attribute::Underlined));
        }

        // Colors.
        if fg_change {
            if let Some(fg) = target.fg {
                let _ = self.queue(SetForegroundColor(fg));
            } else {
                let _ = self.queue(SetForegroundColor(Color::Reset));
            }
        }
        if bg_change {
            if let Some(bg) = target.bg {
                let _ = self.queue(SetBackgroundColor(bg));
            } else {
                let _ = self.queue(SetBackgroundColor(Color::Reset));
            }
        }
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
        let n = self.out.write(buf)?;
        if crate::perf::enabled() {
            self.bytes_queued = self.bytes_queued.saturating_add(n);
        }
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

/// Colors for the software block cursor, adapted to light/dark theme.
pub(super) fn cursor_colors() -> (Color, Color) {
    if theme::is_light() {
        (Color::White, Color::Black)
    } else {
        (Color::Black, Color::White)
    }
}

/// Draw a software cursor at the given position. Any character passed
/// as `under` is re-rendered with the cursor fg/bg inverted so the
/// glyph under the cursor stays visible (as in the prompt). Pass `" "`
/// for an empty cell.
pub(super) fn draw_soft_cursor(out: &mut RenderOut, col: u16, row: u16, under: &str) {
    let (fg, bg) = cursor_colors();
    let _ = out.queue(cursor::MoveTo(col, row));
    // Reset first so any lingering DIM / italic / underline style from
    // the surrounding paint doesn't wash out the cursor — we want a
    // hard-contrast black-on-white (or white-on-black) cell.
    out.reset_style();
    out.set_fg(fg);
    out.set_bg(bg);
    let glyph = if under.is_empty() { " " } else { under };
    out.print(glyph);
    out.reset_style();
}

/// Resolve a `display::ColorRole` against the live theme atomics.
/// Used by `RenderOut`'s `LayoutSink` impl (dialogs and overlay paints
/// that don't go through a `Theme` snapshot).
pub(super) fn resolve_role_live(role: display::ColorRole) -> Color {
    use display::ColorRole as R;
    match role {
        R::Accent => theme::accent(),
        R::Slug => theme::slug_color(),
        R::UserBg => theme::user_bg(),
        R::CodeBlockBg => theme::code_block_bg(),
        R::Bar => theme::bar(),
        R::ToolPending => theme::tool_pending(),
        R::ReasonOff => theme::reason_off(),
        R::Muted => theme::muted(),
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
    pub color: display::ColorValue,
    /// Inner content width (between left and right borders).
    pub inner_w: usize,
}

impl BoxContext {
    /// Print the left border with color.
    pub fn print_left<S: layout_out::LayoutSink>(&self, out: &mut S) {
        out.push_fg(self.color);
        out.print(self.left);
        out.pop_style();
    }

    /// Print right-side padding and border for a line that used `cols` content columns.
    pub fn print_right<S: layout_out::LayoutSink>(&self, out: &mut S, cols: usize) {
        let pad = self.inner_w.saturating_sub(cols);
        if pad > 0 {
            out.print_string(" ".repeat(pad));
        }
        out.push_fg(self.color);
        out.print(self.right);
        out.pop_style();
    }
}

/// Emit `n` blank rows via the sink's native `newline()`.
pub(super) fn emit_newlines<S: layout_out::LayoutSink>(out: &mut S, n: u16) {
    for _ in 0..n {
        out.newline();
    }
}

pub(super) fn reasoning_color(effort: protocol::ReasoningEffort) -> Color {
    match effort {
        protocol::ReasoningEffort::Off => theme::reason_off(),
        protocol::ReasoningEffort::Low => theme::REASON_LOW,
        protocol::ReasoningEffort::Medium => theme::REASON_MED,
        protocol::ReasoningEffort::High => theme::REASON_HIGH,
        protocol::ReasoningEffort::Max => theme::REASON_MAX,
    }
}

pub fn term_width() -> usize {
    terminal::size().map(|(w, _)| w as usize).unwrap_or(80)
}

pub fn term_height() -> usize {
    terminal::size().map(|(_, h)| h as usize).unwrap_or(24)
}

pub use engine::tools::tool_arg_summary;

pub fn tool_timeout_label(args: &HashMap<String, serde_json::Value>) -> Option<String> {
    let ms = args.get("timeout_ms").and_then(|v| v.as_u64())?;
    Some(format!("timeout: {}", format_duration(ms / 1000)))
}

pub(super) fn format_tokens(n: u32) -> String {
    if n >= 1_000_000 {
        format!("{:.1}m", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{}", n)
    }
}
