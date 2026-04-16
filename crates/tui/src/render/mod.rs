mod blocks;
mod cache;
mod context;
mod dialogs;
mod display;
mod highlight;
mod layout_out;
mod paint;
mod prompt;
mod working;

use prompt::PromptState;
use working::WorkingState;

pub use dialogs::{
    parse_questions, AgentSnapshot, AgentsDialog, ConfirmDialog, Dialog, DialogResult, HelpDialog,
    PermissionEntry, PermissionsDialog, PsDialog, Question, QuestionDialog, QuestionOption,
    ResumeDialog, RewindDialog, SharedSnapshots,
};

/// Layout placement computed by `draw_frame` for the active dialog.
pub struct DialogPlacement {
    pub row: u16,
    pub granted_rows: u16,
}

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
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthChar;

use blocks::{
    collect_trailing_thinking, gap_between, layout_block, render_active_exec, render_block,
    render_thinking_summary, render_tool, thinking_summary, Element,
};

pub use context::{LayoutContext, PaintContext};
pub use display::DisplayBlock;
pub use highlight::warm_up_syntect;
use layout_out::LayoutSink;
use layout_out::SpanCollector;
use paint::{paint_block, paint_line};

pub use cache::{
    build_tool_output_render_cache, session_render_hash, CachedNotebookEdit, PersistedLayoutCache,
    RenderCache, ToolOutputRenderCache, LAYOUT_CACHE_VERSION, RENDER_CACHE_VERSION,
};

/// Cap on rows painted by a single redraw, regardless of history length.
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

    /// Queue a ScrollUp. Does not move the cursor — only shifts
    /// content up, affecting the logical position of previously-drawn
    /// content.
    pub(super) fn scroll_up(&mut self, n: u16) {
        let _ = self.queue(terminal::ScrollUp(n));
    }

    // ── Newline helpers ─────────────────────────────────────────────

    /// Advance to the next line, clearing trailing content on the
    /// current line.  Works in both scroll and overlay mode:
    ///
    /// - **Scroll mode** (`row` is `None`): emits `\r\n` which commits
    ///   the line to scrollback.  The cursor advances by 1 row, capped
    ///   at the last terminal row (where the terminal scrolls instead).
    /// - **Overlay mode** (`row` is `Some`): emits `MoveTo(0, row+1)`
    ///   so the position stays exact without relying on the terminal's
    ///   linefeed behaviour.
    ///
    /// When the caller knows which mode is active, prefer
    /// [`scroll_newline`] or [`overlay_newline`] for compile-time
    /// documentation and a debug-mode assertion.
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

    /// Newline in scroll mode — emits `\r\n` that commits to scrollback.
    /// Panics in debug builds if `row` is `Some` (overlay mode).
    pub fn scroll_newline(&mut self) {
        debug_assert!(
            self.row.is_none(),
            "scroll_newline called in overlay mode (row={:?})",
            self.row
        );
        self.newline();
    }

    /// Newline in overlay mode — emits `MoveTo` to the next row.
    /// Panics in debug builds if `row` is `None` (scroll mode).
    pub fn overlay_newline(&mut self) {
        debug_assert!(self.row.is_some(), "overlay_newline called in scroll mode");
        self.newline();
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
fn cursor_colors() -> (Color, Color) {
    if theme::is_light() {
        (Color::White, Color::Black)
    } else {
        (Color::Black, Color::White)
    }
}

/// Draw a software cursor (block) at the given position.
/// The native terminal cursor stays hidden to avoid flickering.
pub(super) fn draw_soft_cursor(out: &mut RenderOut, col: u16, row: u16) {
    let (fg, bg) = cursor_colors();
    let _ = out.queue(cursor::MoveTo(col, row));
    out.set_fg(fg);
    out.set_bg(bg);
    out.print(" ");
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
fn emit_newlines<S: layout_out::LayoutSink>(out: &mut S, n: u16) {
    for _ in 0..n {
        out.newline();
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

/// Mutable sidecar for a committed `Block::ToolCall`, keyed by `call_id` on
/// `BlockHistory::tool_states`. Holds every field of a tool block that can
/// change after the block has been pushed (status flip, streamed output,
/// finalized elapsed, etc.). Splitting this out keeps `Block::ToolCall`
/// immutable so its layout can be cached permanently once terminal.
#[derive(Clone)]
pub struct ToolState {
    pub status: ToolStatus,
    pub elapsed: Option<Duration>,
    pub output: Option<ToolOutputRef>,
    pub user_message: Option<String>,
}

impl ToolState {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status,
            ToolStatus::Ok | ToolStatus::Err | ToolStatus::Denied
        )
    }
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
    /// Cached text-content size in bytes (None if unknown).
    pub size_bytes: Option<u64>,
}

#[derive(Clone, serde::Serialize)]
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
    /// Immutable handle to a committed tool call. The mutable result
    /// (status, elapsed, output, user_message) lives in `BlockHistory::tool_states`
    /// keyed by `call_id`; look it up with `Screen::tool_state`.
    ToolCall {
        call_id: String,
        name: String,
        summary: String,
        args: HashMap<String, serde_json::Value>,
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

impl Block {
    /// Stable content hash of this block. Two blocks with the same id
    /// produce identical `DisplayBlock`s for the same `LayoutKey` and
    /// `ToolState`. For `ToolCall`, `ToolState` (status / output / elapsed)
    /// is deliberately *not* hashed — mutable tool state lives separately
    /// and is invalidated via `BlockHistory::invalidate_block_layout`.
    ///
    /// Implementation: serialize through `serde_json::Value` first (whose
    /// `Map` is a `BTreeMap` without the `preserve_order` feature) so the
    /// `HashMap<String, Value>` arg fields are emitted in sorted-key
    /// order, then hash the resulting bytes. Without the intermediate
    /// `to_value` step, two blocks with identical content but different
    /// HashMap insertion orders would produce different ids.
    pub fn id(&self) -> BlockId {
        let value = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
        let bytes = serde_json::to_vec(&value).unwrap_or_default();
        BlockId(seahash::hash(&bytes))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum AgentBlockStatus {
    Running,
    Done,
    Error,
}

#[derive(Clone, Copy, PartialEq, serde::Serialize)]
pub enum ApprovalScope {
    Session,
    Workspace,
}

#[derive(Clone, PartialEq, serde::Serialize)]
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

/// Stable content hash of a `Block`. Computed once at construction; two
/// blocks with the same id are guaranteed to lay out identically given the
/// same `LayoutKey` and (for tool blocks) `ToolState`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct BlockId(pub u64);

/// Cache key for a single `DisplayBlock` layout — the inputs to
/// `layout_block` that affect the laid-out output for a given block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct LayoutKey {
    pub width: u16,
    pub show_thinking: bool,
}

/// Per-block cached artifacts. Keeps a bounded LRU of the most recent
/// `LayoutKey → DisplayBlock` pairs so that resize cycles (e.g. 100→80→100)
/// can hit the cache on every step.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct BlockArtifact {
    /// `(LayoutKey, DisplayBlock)` entries ordered most-recently-used first.
    pub layouts: Vec<(LayoutKey, DisplayBlock)>,
}

impl BlockArtifact {
    const MAX_LAYOUTS: usize = 4;

    pub fn get(&self, key: LayoutKey) -> Option<&DisplayBlock> {
        self.layouts.iter().find(|(k, _)| *k == key).map(|(_, b)| b)
    }

    pub fn insert(&mut self, key: LayoutKey, block: DisplayBlock) {
        self.layouts.retain(|(k, _)| *k != key);
        self.layouts.insert(0, (key, block));
        self.layouts.truncate(Self::MAX_LAYOUTS);
    }

    pub fn clear(&mut self) {
        self.layouts.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.layouts.is_empty()
    }

    /// Drop cached layouts whose replay would be stale at `new_width`.
    /// Layouts whose `layout_width` equals `new_width` or whose source
    /// didn't wrap and still fits are preserved.
    pub fn invalidate_for_width(&mut self, new_width: u16) {
        self.layouts
            .retain(|(k, b)| k.width == new_width || b.is_valid_at(new_width));
    }
}

struct BlockHistory {
    /// Append-only sequence of `BlockId`s. The "ith block" is
    /// `blocks[&order[i]]`. Duplicate ids are allowed: two identical blocks
    /// at different positions resolve to the same entry in `blocks` /
    /// `artifacts`.
    order: Vec<BlockId>,
    /// Content-addressed block store.
    blocks: HashMap<BlockId, Block>,
    /// Content-addressed per-block layout cache.
    artifacts: HashMap<BlockId, BlockArtifact>,
    /// Mutable sidecar state for `Block::ToolCall` entries, keyed by `call_id`.
    tool_states: HashMap<String, ToolState>,
    /// Terminal width when artifacts were last width-pruned.
    cache_width: usize,
    /// True iff the layout cache has changed since the last persisted save.
    /// When false, `save_session` skips writing the layout cache file.
    cache_dirty: bool,
    flushed: usize,
    last_block_rows: u16,
    /// When true, the leading gap of the next unflushed block is suppressed.
    /// Set after a dialog dismiss where ScrollUp pushed the gap into scrollback.
    suppress_leading_gap: bool,
    /// Rows to skip from the top of the next-rendered first block.
    /// Set by `redraw` when a single block exceeds the redraw budget;
    /// consumed by the next `render` call and reset to 0 afterwards.
    pending_head_skip: u16,
}

impl BlockHistory {
    fn new() -> Self {
        Self {
            order: Vec::new(),
            blocks: HashMap::new(),
            artifacts: HashMap::new(),
            tool_states: HashMap::new(),
            cache_width: 0,
            cache_dirty: false,
            flushed: 0,
            last_block_rows: 0,
            suppress_leading_gap: false,
            pending_head_skip: 0,
        }
    }

    fn len(&self) -> usize {
        self.order.len()
    }

    fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    fn block_at(&self, i: usize) -> &Block {
        &self.blocks[&self.order[i]]
    }

    fn last_block(&self) -> Option<&Block> {
        self.order.last().and_then(|id| self.blocks.get(id))
    }

    /// Append `block` and return its `BlockId`. Duplicate content merges
    /// into the existing entry in `blocks`/`artifacts`, so two identical
    /// blocks share their cached layouts.
    fn push(&mut self, block: Block) -> BlockId {
        let id = block.id();
        self.order.push(id);
        self.blocks.entry(id).or_insert(block);
        self.artifacts.entry(id).or_default();
        self.cache_dirty = true;
        id
    }

    /// Push a `Block::ToolCall` alongside its initial `ToolState`.
    fn push_with_state(&mut self, block: Block, call_id: String, state: ToolState) -> BlockId {
        self.tool_states.insert(call_id, state);
        self.push(block)
    }

    /// `BlockId` of the most recent `Block::ToolCall` whose `call_id` matches.
    fn tool_block_id(&self, call_id: &str) -> Option<BlockId> {
        self.order.iter().rev().copied().find(|id| {
            matches!(
                self.blocks.get(id),
                Some(Block::ToolCall { call_id: c, .. }) if c == call_id
            )
        })
    }

    /// Drop every cached layout for a single block id.
    fn invalidate_block_layout(&mut self, id: BlockId) {
        if let Some(artifact) = self.artifacts.get_mut(&id) {
            if !artifact.is_empty() {
                artifact.clear();
                self.cache_dirty = true;
            }
        }
    }

    fn has_unflushed(&self) -> bool {
        self.flushed < self.order.len()
    }

    fn clear(&mut self) {
        self.order.clear();
        self.blocks.clear();
        self.artifacts.clear();
        self.tool_states.clear();
        self.flushed = 0;
        self.last_block_rows = 0;
        self.cache_dirty = true;
    }

    /// Width-aware invalidation: prune cached layouts that are no longer
    /// replayable at `new_width`. The bounded LRU in each artifact means
    /// layouts from previous widths survive and can be reused after a
    /// resize cycle.
    fn invalidate_for_width(&mut self, new_width: usize) {
        let _perf = crate::perf::begin("history:invalidate_for_width");
        let nw = new_width as u16;
        let mut dirty = false;
        for artifact in self.artifacts.values_mut() {
            let before = artifact.layouts.len();
            artifact.invalidate_for_width(nw);
            if artifact.layouts.len() != before {
                dirty = true;
            }
        }
        self.cache_width = new_width;
        if dirty {
            self.cache_dirty = true;
        }
    }

    /// Gap (in rows) before the block at `i`, based on adjacency rules.
    fn block_gap(&self, i: usize) -> u16 {
        if i > 0 {
            gap_between(
                &Element::Block(self.block_at(i - 1)),
                &Element::Block(self.block_at(i)),
            )
        } else {
            0
        }
    }

    /// Rows the block at `i` would occupy under `key`. Lays the block out
    /// if no cached layout exists, so that the caller's subsequent render
    /// pass gets a cache hit.
    fn ensure_rows(&mut self, i: usize, key: LayoutKey) -> u16 {
        let id = self.order[i];
        if let Some(rows) = self
            .artifacts
            .get(&id)
            .and_then(|a| a.get(key))
            .map(|d| d.rows())
        {
            return rows;
        }
        let block = &self.blocks[&id];
        let tool_state = if let Block::ToolCall { call_id, .. } = block {
            self.tool_states.get(call_id)
        } else {
            None
        };
        let lctx = LayoutContext {
            width: key.width,
            show_thinking: key.show_thinking,
        };
        let display = layout_block(block, tool_state, &lctx);
        let rows = display.rows();
        let artifact = self.artifacts.get_mut(&id).unwrap();
        artifact.insert(key, display);
        self.cache_dirty = true;
        rows
    }

    /// Earliest block index such that `[start_idx..len()]` fits in
    /// `max_lines` rows. Walks backwards and stops early — O(tail),
    /// not O(history).
    ///
    /// Returns `(start_idx, head_skip)`. When a single block at the
    /// tail is taller than the entire budget, that block is still
    /// included and `head_skip` is the number of rows to drop from
    /// its top so the painted slice fits the budget. This keeps very
    /// long messages visible (tail-cropped, tmux-style) instead of
    /// disappearing entirely.
    fn redraw_start(&mut self, max_lines: u16, key: LayoutKey) -> (usize, u16) {
        let _perf = crate::perf::begin("history:redraw_start");
        let mut budget = max_lines;
        let mut start = self.order.len();
        for i in (0..self.order.len()).rev() {
            let rows = self.ensure_rows(i, key);
            let total = self.block_gap(i) + rows;
            if total > budget {
                if start == self.order.len() {
                    // Even the last block is bigger than the budget —
                    // include it and crop its head to fit. The gap is
                    // dropped by `render` when `head_skip > 0`, so the
                    // budget here is purely block rows, not gap+rows.
                    let head_skip = rows.saturating_sub(budget);
                    return (i, head_skip);
                }
                break;
            }
            budget -= total;
            start = i;
        }
        (start, 0)
    }

    fn truncate(&mut self, idx: usize) {
        if idx >= self.order.len() {
            return;
        }
        let removed: Vec<BlockId> = self.order.drain(idx..).collect();
        let live: HashSet<BlockId> = self.order.iter().copied().collect();
        for id in removed {
            if !live.contains(&id) {
                self.blocks.remove(&id);
                self.artifacts.remove(&id);
            }
        }
        self.flushed = self.flushed.min(self.order.len());
        self.cache_dirty = true;
        self.gc_tool_states();
    }

    /// Drop tool states whose owning `Block::ToolCall` no longer appears in
    /// `order`.
    fn gc_tool_states(&mut self) {
        let live: HashSet<String> = self
            .order
            .iter()
            .filter_map(|id| self.blocks.get(id))
            .filter_map(|b| {
                if let Block::ToolCall { call_id, .. } = b {
                    Some(call_id.clone())
                } else {
                    None
                }
            })
            .collect();
        self.tool_states.retain(|cid, _| live.contains(cid));
    }

    /// Render unflushed blocks. Returns total rows printed.
    fn render(&mut self, out: &mut RenderOut, width: usize, show_thinking: bool) -> u16 {
        if !self.has_unflushed() {
            return 0;
        }
        let _perf = crate::perf::begin("history:render");
        let use_cache = out.row.is_none();

        if use_cache && width != self.cache_width {
            self.invalidate_for_width(width);
        }

        let theme = crate::theme::snapshot();
        let pctx = PaintContext {
            theme: &theme,
            term_width: width as u16,
        };
        let key = LayoutKey {
            width: width as u16,
            show_thinking,
        };

        let mut total = 0u16;
        let last_idx = self.order.len().saturating_sub(1);
        let mut first = true;
        // Consume any pending head-skip set by the redraw path. Skip
        // the leading gap on the first block too, since we're starting
        // mid-block visually.
        let head_skip = std::mem::take(&mut self.pending_head_skip);
        for i in self.flushed..self.order.len() {
            let head_skip_block = if first { head_skip } else { 0 };
            let gap = if first && (self.suppress_leading_gap || head_skip > 0) {
                0
            } else {
                self.block_gap(i)
            };
            first = false;
            for _ in 0..gap {
                out.scroll_newline();
            }

            let id = self.order[i];
            let block = &self.blocks[&id];
            let tool_state = if let Block::ToolCall { call_id, .. } = block {
                self.tool_states.get(call_id)
            } else {
                None
            };

            let rows = if use_cache {
                if let Some(cached) = self.artifacts.get(&id).and_then(|a| a.get(key)) {
                    let _p = crate::perf::begin("history:cache_hit");
                    paint_block(out, cached, &pctx, head_skip_block as usize);
                    cached.rows().saturating_sub(head_skip_block)
                } else {
                    let _p = crate::perf::begin("history:cache_miss");
                    let lctx = LayoutContext {
                        width: width as u16,
                        show_thinking,
                    };
                    let display = layout_block(block, tool_state, &lctx);
                    paint_block(out, &display, &pctx, head_skip_block as usize);
                    let rows = display.rows().saturating_sub(head_skip_block);
                    let artifact = self.artifacts.get_mut(&id).unwrap();
                    artifact.insert(key, display);
                    self.cache_dirty = true;
                    rows
                }
            } else {
                let _p = crate::perf::begin("history:overlay_render");
                let lctx = LayoutContext {
                    width: width as u16,
                    show_thinking,
                };
                let display = layout_block(block, tool_state, &lctx);
                paint_block(out, &display, &pctx, head_skip_block as usize);
                display.rows().saturating_sub(head_skip_block)
            };

            total += gap + rows;
            if i == last_idx {
                self.last_block_rows = rows + gap;
            }
        }
        self.suppress_leading_gap = false;
        self.flushed = self.order.len();
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
    /// A permission dialog is waiting for the user to stop typing.
    pending_dialog: bool,
    /// A dialog is currently open (confirm, rewind, etc.).
    dialog_open: bool,
    /// Whether the active dialog's height should be constrained to
    /// `max(h/2, natural_space)` to limit scroll-up.
    constrain_dialog: bool,
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
    /// Ephemeral btw side-question state, rendered above the prompt.
    btw: Option<BtwBlock>,
    /// Ephemeral notification shown above the prompt, dismissed on any key.
    notification: Option<Notification>,
    /// Short task label (slug) shown on the status bar after the throbber.
    task_label: Option<String>,

    /// Terminal I/O backend (real terminal or test buffer).
    backend: Box<dyn TerminalBackend>,
    focused: bool,
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
            pending_dialog: false,
            dialog_open: false,
            constrain_dialog: false,
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
            btw: None,
            notification: None,
            task_label: None,
            backend,
            focused: true,
        }
    }

    pub fn size(&self) -> (u16, u16) {
        self.backend.size()
    }

    fn cursor_y(&self) -> u16 {
        self.prompt
            .anchor_row
            .unwrap_or_else(|| self.backend.cursor_y())
    }

    /// Expose the backend for dialogs that need output + size.
    pub fn backend(&self) -> &dyn TerminalBackend {
        &*self.backend
    }

    pub fn set_focused(&mut self, focused: bool) {
        if self.focused == focused {
            return;
        }
        self.focused = focused;
        self.prompt.dirty = true;
    }

    /// Set the prompt anchor row explicitly (used by test harness).
    pub fn set_anchor_row(&mut self, row: u16) {
        self.prompt.anchor_row = Some(row);
    }

    /// Number of committed blocks in history.
    pub fn block_count(&self) -> usize {
        self.history.len()
    }

    /// Cloned snapshot of all blocks in history, in order.
    pub fn blocks(&self) -> Vec<Block> {
        self.history
            .order
            .iter()
            .map(|id| self.history.blocks[id].clone())
            .collect()
    }

    /// Cloned snapshot of every committed tool's `ToolState`. Pairs with
    /// `blocks()` to fully reconstruct history (used by the test harness).
    pub fn tool_states_snapshot(&self) -> HashMap<String, ToolState> {
        self.history.tool_states.clone()
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
        let max_lines = btw_max_body_rows(term_h);
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

    /// Row where a dialog should start rendering (lines up with the prompt bar).
    pub fn dialog_row(&self) -> u16 {
        self.prompt.prev_dialog_row.unwrap_or(0)
    }

    /// Emit a blank gap line after the dialog, then clear any stale
    /// rows between the dialog and the status bar.  Called globally
    /// from `render_frame` so every dialog gets the same gap without
    /// each one needing to emit it.
    ///
    /// Only emits when `out` is in overlay mode (`out.row` is `Some`).
    /// On early-exit frames where the dialog didn't redraw, `out.row`
    /// stays `None` (scroll mode) and a newline would push a `\r\n`
    /// into scrollback, polluting the scroll buffer and shifting the
    /// visible dialog.
    pub fn queue_dialog_gap(&self, out: &mut RenderOut) {
        if out.row.is_none() {
            return;
        }
        out.overlay_newline();
        let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));
    }

    /// Render the status line at the very last row of the terminal into
    /// the given output buffer.  Used as a callback inside dialog sync frames
    /// so the status bar is painted atomically with dialog content.
    pub fn queue_status_line(&self, out: &mut RenderOut) {
        let (_, h) = self.size();
        if h == 0 {
            return;
        }
        let _ = out.queue(cursor::SavePosition);
        let _ = out.queue(cursor::MoveTo(0, h - 1));
        out.reset_style();
        self.render_status_line(out);
        let _ = out.queue(cursor::RestorePosition);
    }

    /// Render the status line content at the current cursor position.
    /// Responsively drops/truncates elements when the terminal is too narrow.
    fn render_status_line(&self, out: &mut RenderOut) {
        let (w, _) = self.size();
        let width = w as usize;
        let status_bg = Color::AnsiValue(233);

        // ── Build all status spans ──
        let mut spans: Vec<StatusSpan> = Vec::with_capacity(16);

        // Slug pill: spinner (always visible) + label (truncatable).
        let is_compacting = self.working.throbber == Some(Throbber::Compacting);
        let spinner = self.working.spinner_char();
        let pill_bg = if is_compacting {
            Color::White
        } else {
            theme::slug_color()
        };
        let pill_style = StyleState {
            fg: Some(Color::Black),
            bg: Some(pill_bg),
            ..StyleState::default()
        };

        if let Some(sp) = spinner {
            spans.push(StatusSpan {
                text: format!(" {sp} "),
                style: pill_style.clone(),
                priority: 0,
                group: false,
                truncatable: false,
            });
            let label = if is_compacting {
                "compacting ".into()
            } else if self.show_slug {
                self.task_label
                    .as_ref()
                    .map(|l| format!("{l} "))
                    .unwrap_or_else(|| "working ".into())
            } else {
                "working ".into()
            };
            spans.push(StatusSpan {
                text: label,
                style: pill_style,
                priority: 5,
                group: false,
                truncatable: true,
            });
        } else if self.show_slug {
            if let Some(ref label) = self.task_label {
                spans.push(StatusSpan {
                    text: format!(" {label} "),
                    style: pill_style,
                    priority: 5,
                    group: false,
                    truncatable: true,
                });
            }
        }

        // Vim mode.
        if self.last_vim_enabled {
            let vim_label = vim_mode_label(self.last_vim_mode).unwrap_or("NORMAL");
            let vim_fg = match self.last_vim_mode {
                Some(crate::vim::ViMode::Insert) => Color::AnsiValue(78),
                Some(crate::vim::ViMode::Visual) | Some(crate::vim::ViMode::VisualLine) => {
                    Color::AnsiValue(176)
                }
                _ => Color::AnsiValue(74),
            };
            spans.push(StatusSpan {
                text: format!(" {vim_label} "),
                style: StyleState {
                    fg: Some(vim_fg),
                    bg: Some(Color::AnsiValue(236)),
                    ..StyleState::default()
                },
                priority: 3,
                group: false,
                truncatable: false,
            });
        }

        // Mode indicator.
        let (mode_icon, mode_name, mode_fg) = match self.last_mode {
            protocol::Mode::Plan => ("◇ ", "plan", theme::PLAN),
            protocol::Mode::Apply => ("→ ", "apply", theme::APPLY),
            protocol::Mode::Yolo => ("⚡", "yolo", theme::YOLO),
            protocol::Mode::Normal => ("○ ", "normal", theme::muted()),
        };
        spans.push(StatusSpan {
            text: format!(" {mode_icon}{mode_name} "),
            style: StyleState {
                fg: Some(mode_fg),
                bg: Some(Color::AnsiValue(234)),
                ..StyleState::default()
            },
            priority: 1,
            group: false,
            truncatable: false,
        });

        // Throbber status (timer, tok/s, retry, done, interrupted).
        // Skip the first span for active states — it duplicates the pill.
        let throbber_spans = self.working.throbber_spans(self.show_tps);
        let is_active = matches!(
            self.working.throbber,
            Some(Throbber::Working) | Some(Throbber::Compacting) | Some(Throbber::Retrying { .. })
        );
        let status_bar_spans: &[BarSpan] = if is_active && !throbber_spans.is_empty() {
            &throbber_spans[1..]
        } else {
            &throbber_spans
        };
        for bar_span in status_bar_spans {
            // Map BarSpan priorities: timer (0) → 4, tok/s (3) → 6.
            let priority = match bar_span.priority {
                0 => 4,
                3 => 6,
                p => p,
            };
            spans.push(StatusSpan {
                text: bar_span.text.clone(),
                style: StyleState {
                    fg: Some(bar_span.color),
                    bg: Some(status_bg),
                    bold: bar_span.bold,
                    dim: bar_span.dim,
                    ..StyleState::default()
                },
                priority,
                group: false,
                truncatable: false,
            });
        }

        // Permission pending.
        if self.pending_dialog && !self.dialog_open {
            spans.push(StatusSpan {
                text: "permission pending".into(),
                style: StyleState {
                    fg: Some(theme::accent()),
                    bg: Some(status_bg),
                    bold: true,
                    ..StyleState::default()
                },
                priority: 2,
                group: true,
                truncatable: false,
            });
        }

        // Running procs.
        if self.running_procs > 0 {
            let label = if self.running_procs == 1 {
                "1 proc".into()
            } else {
                format!("{} procs", self.running_procs)
            };
            spans.push(StatusSpan {
                text: label,
                style: StyleState {
                    fg: Some(theme::accent()),
                    bg: Some(status_bg),
                    ..StyleState::default()
                },
                priority: 2,
                group: true,
                truncatable: false,
            });
        }

        // Running agents.
        if self.running_agents > 0 {
            let label = if self.running_agents == 1 {
                "1 agent".into()
            } else {
                format!("{} agents", self.running_agents)
            };
            spans.push(StatusSpan {
                text: label,
                style: StyleState {
                    fg: Some(theme::AGENT),
                    bg: Some(status_bg),
                    ..StyleState::default()
                },
                priority: 2,
                group: true,
                truncatable: false,
            });
        }

        // ── Responsive layout ──
        render_status_spans(out, &mut spans, width, status_bg);
    }

    /// Dismiss a dialog overlay.
    ///
    /// Clears from the screen anchor (or the dialog row, whichever is
    /// higher) down, so any ephemeral overlay shifted upward by
    /// `ScrollUp` is wiped along with the dialog bar itself.
    pub fn clear_dialog_area(&mut self) {
        let dialog_row = self.prompt.prev_dialog_row.unwrap_or(0);
        let screen_anchor = self.prompt.anchor_row.unwrap_or(dialog_row);
        let clear_from = screen_anchor.min(dialog_row);

        let height = self.size().1;
        let mut frame = Frame::begin(&*self.backend);
        for row in clear_from..height {
            let _ = frame.queue(cursor::MoveTo(0, row));
            let _ = frame.queue(terminal::Clear(terminal::ClearType::CurrentLine));
        }
        // When the dialog scrolled enough to push the anchor to row 0,
        // the prompt gap that was between the last block and the prompt
        // got pushed into scrollback.  The next block render would emit
        // gap_between() again, creating a double blank line.  Suppress
        // the leading gap so only the scrollback copy remains.
        let scrolled_by_dialog = screen_anchor == 0 && self.has_scrollback;
        if scrolled_by_dialog {
            self.history.suppress_leading_gap = true;
        }
        self.defer_pending_render = true;
        // Only reset anchor/prev_rows when the dialog caused ScrollUp
        // (prompt was physically moved). For non-scrolled dialogs the
        // prompt is still in its original position — just mark dirty so
        // it redraws in place.
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
        let last_row = anchor + self.prompt.prev_rows.saturating_sub(1);
        let height = self.size().1;
        let mut out = self.backend.make_output();
        // Erase the software block cursor so it doesn't linger on exit.
        if let Some((col, row)) = self.prompt.soft_cursor {
            let _ = out.queue(cursor::MoveTo(col, row));
            let _ = out.queue(ResetColor);
            out.print(" ");
        }
        let _ = out.queue(cursor::MoveTo(0, last_row.min(height.saturating_sub(1))));
        let _ = out.queue(Print("\r\n\r\n"));
        out.line_cols = 0;
        if clear_below {
            let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));
        }
        let _ = out.flush();
    }

    pub fn begin_turn(&mut self) {
        self.history.last_block_rows = 0;
        self.active_tools.clear();
    }

    /// Push a `Block::ToolCall` along with its `ToolState`. Use this on
    /// the resume path where the protocol message already carries a
    /// finished tool result.
    pub fn push_tool_call(&mut self, block: Block, state: ToolState) {
        debug_assert!(matches!(block, Block::ToolCall { .. }));
        let call_id = match &block {
            Block::ToolCall { call_id, .. } => call_id.clone(),
            _ => return,
        };
        self.history.push_with_state(block, call_id, state);
        self.prompt.dirty = true;
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
            .order
            .iter()
            .rev()
            .filter_map(|id| self.history.blocks.get(id))
            .find(|b| !matches!(b, Block::Thinking { .. }))
        {
            gap_between(
                &Element::Block(last),
                &Element::Block(&Block::Thinking {
                    content: String::new(),
                }),
            )
        } else if self.history.is_empty() {
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
        } else if let Some(cid) = self.last_tool_call_id() {
            self.update_tool_state(&cid, |state| match state.output {
                Some(ref mut out) => {
                    if !out.content.is_empty() {
                        out.content.push('\n');
                    }
                    out.content.push_str(chunk);
                }
                None => {
                    state.output = Some(Box::new(ToolOutput {
                        content: chunk.to_string(),
                        is_error: false,
                        metadata: None,
                        render_cache: None,
                    }));
                }
            });
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
        } else if let Some(cid) = self.last_tool_call_id() {
            self.update_tool_state(&cid, |state| state.status = status);
        }
    }

    pub fn set_active_user_message(&mut self, call_id: &str, msg: String) {
        if let Some(tool) = self.active_tool_mut(call_id) {
            tool.user_message = Some(msg);
            self.prompt.dirty = true;
        } else if let Some(cid) = self.last_tool_call_id() {
            self.update_tool_state(&cid, |state| state.user_message = Some(msg));
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
            let cid = if call_id.is_empty() {
                tool.call_id.clone()
            } else {
                call_id.to_string()
            };
            let state = ToolState {
                status,
                elapsed,
                output,
                user_message: tool.user_message,
            };
            self.history.push_with_state(
                Block::ToolCall {
                    call_id: cid.clone(),
                    name: tool.name,
                    summary: tool.summary,
                    args: tool.args,
                },
                cid,
                state,
            );
            self.prompt.dirty = true;
        } else if let Some(cid) = self.last_tool_call_id() {
            self.update_tool_state(&cid, |state| {
                state.status = status;
                state.output = output;
            });
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
        // Spinner pause/resume is handled by the caller based on
        // whether the dialog blocks the agent — non-blocking dialogs
        // keep the spinner animating.
        self.prompt.dirty = true;
    }

    pub fn set_constrain_dialog(&mut self, constrain: bool) {
        self.constrain_dialog = constrain;
    }

    /// Pause the working spinner. Used when a blocking dialog (confirm,
    /// question) opens and the agent is suspended.
    pub fn pause_spinner(&mut self) {
        self.working.pause();
    }

    /// Resume the working spinner after a blocking dialog closes.
    pub fn resume_spinner(&mut self) {
        self.working.resume();
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
        let _perf = crate::perf::begin("render:flush_blocks");
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
            let state = ToolState {
                status,
                elapsed,
                output: tool.output,
                user_message: tool.user_message,
            };
            self.history.push_with_state(
                Block::ToolCall {
                    call_id: tool.call_id.clone(),
                    name: tool.name,
                    summary: tool.summary,
                    args: tool.args,
                },
                tool.call_id,
                state,
            );
        }
    }

    /// `call_id` of the most recent committed `Block::ToolCall`, if any.
    fn last_tool_call_id(&self) -> Option<String> {
        self.history
            .order
            .iter()
            .rev()
            .find_map(|id| match self.history.blocks.get(id) {
                Some(Block::ToolCall { call_id, .. }) => Some(call_id.clone()),
                _ => None,
            })
    }

    /// Read-only view of a committed tool's mutable state.
    pub fn tool_state(&self, call_id: &str) -> Option<&ToolState> {
        self.history.tool_states.get(call_id)
    }

    /// Mutate a committed tool's state and invalidate its layout cache so
    /// the next paint reflects the change. Returns true if `call_id` was
    /// found in history.
    pub fn update_tool_state(
        &mut self,
        call_id: &str,
        mutator: impl FnOnce(&mut ToolState),
    ) -> bool {
        let Some(state) = self.history.tool_states.get_mut(call_id) else {
            return false;
        };
        mutator(state);
        if let Some(id) = self.history.tool_block_id(call_id) {
            self.history.invalidate_block_layout(id);
        }
        self.prompt.dirty = true;
        true
    }

    /// Insert or replace tool state for a call_id without touching blocks.
    /// Used by resume to attach state to freshly reconstructed blocks.
    pub fn set_tool_state(&mut self, call_id: String, state: ToolState) {
        self.history.tool_states.insert(call_id, state);
    }

    /// Whether any content (blocks, active tool, active exec) exists above
    /// the prompt.  Used to decide whether to emit a 1-line gap before the
    /// prompt bar.
    fn has_content(&self) -> bool {
        !self.history.is_empty() || self.has_ephemeral()
    }

    pub fn render_pending_blocks(&mut self) {
        if self.defer_pending_render {
            self.defer_pending_render = false;
            return;
        }
        if !self.history.has_unflushed() {
            return;
        }
        let mut frame = Frame::begin(&*self.backend);
        let start_row = if self.prompt.drawn {
            let row = self.prompt.anchor_row.unwrap_or(0);
            let _ = frame.queue(cursor::MoveTo(0, row));
            let _ = frame.queue(terminal::Clear(terminal::ClearType::FromCursorDown));
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
            .render(&mut frame, w as usize, self.show_thinking);
        // Cap anchor at the last terminal row — scroll-mode rendering may
        // have pushed past the bottom, making start_row + block_rows overshoot.
        self.prompt.anchor_row = Some((start_row + block_rows).min(h.saturating_sub(1)));
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

    /// Clear screen + scrollback and repaint the last
    /// `MAX_REDRAW_LINES` rows of committed history in scroll mode.
    /// Rows past the viewport scroll into the fresh scrollback so the
    /// user can scroll back through recent history. Purging scrollback
    /// is what keeps a resize from leaving duplicate rows across the
    /// viewport and scrollback.
    pub fn redraw(&mut self) {
        let _perf = crate::perf::begin("redraw");
        let (w, height) = self.size();
        let mut frame = Frame::begin(&*self.backend);
        let _ = frame.queue(cursor::MoveTo(0, 0));
        let _ = frame.queue(terminal::Clear(terminal::ClearType::All));
        let _ = frame.queue(terminal::Clear(terminal::ClearType::Purge));
        // Drop stale layouts from a previous width before laying out
        // anything new.
        if w as usize != self.history.cache_width {
            let _p = crate::perf::begin("redraw:invalidate");
            self.history.invalidate_for_width(w as usize);
        }
        let key = LayoutKey {
            width: w,
            show_thinking: self.show_thinking,
        };
        let (start_idx, head_skip) = {
            let _p = crate::perf::begin("redraw:start_idx");
            self.history.redraw_start(MAX_REDRAW_LINES, key)
        };
        self.history.flushed = start_idx;
        self.history.last_block_rows = 0;
        self.history.pending_head_skip = head_skip;
        let block_rows = {
            let _p = crate::perf::begin("redraw:render_blocks");
            self.history
                .render(&mut frame, w as usize, self.show_thinking)
        };
        self.prompt.drawn = false;
        self.prompt.dirty = true;
        self.prompt.prev_rows = 0;
        self.content_start_row = Some(0);
        self.has_scrollback = false;
        self.prompt.anchor_row = Some(block_rows.min(height.saturating_sub(1)));
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
        let mut frame = Frame::begin(&*self.backend);
        let _ = frame.queue(cursor::MoveTo(0, 0));
        let _ = frame.queue(terminal::Clear(terminal::ClearType::All));
        let _ = frame.queue(terminal::Clear(terminal::ClearType::Purge));
    }

    pub fn has_history(&self) -> bool {
        !self.history.is_empty()
    }

    /// Snapshot the per-tool intermediate representations stored on
    /// committed `Block::ToolCall` blocks. The IR is width-independent and
    /// expensive to rebuild (it contains the LCS diff and syntect tokens),
    /// so we persist it alongside the session and reattach on resume.
    /// Returns `None` if no IR has been built yet.
    pub fn export_render_cache(&self) -> Option<RenderCache> {
        let mut cache = RenderCache::new(String::new());
        for id in &self.history.order {
            if let Some(Block::ToolCall { call_id, .. }) = self.history.blocks.get(id) {
                if let Some(state) = self.history.tool_states.get(call_id) {
                    if let Some(out) = state.output.as_deref() {
                        if let Some(ir) = &out.render_cache {
                            cache.insert_tool_output(call_id.clone(), ir.clone());
                        }
                    }
                }
            }
        }
        if cache.tool_outputs.is_empty() {
            None
        } else {
            Some(cache)
        }
    }

    /// Whether the layout cache has changed since the last
    /// `export_layout_cache`. Used by `save_session` to skip writing the
    /// cache file when nothing would change on disk.
    pub fn layout_cache_dirty(&self) -> bool {
        self.history.cache_dirty
    }

    /// Export a content-addressed snapshot of every cached block artifact
    /// that is safe to persist. Tool blocks whose `ToolState` is not yet
    /// terminal are skipped — their layout captures transient state.
    pub fn export_layout_cache(&mut self) -> Option<PersistedLayoutCache> {
        if self.history.is_empty() {
            return None;
        }
        let mut cache = PersistedLayoutCache::new(crate::theme::is_light());
        // Walk `order` so we only export artifacts for blocks currently in
        // history (and so we can inspect the `ToolState` of each tool block
        // exactly once — duplicates in `order` resolve to the same entry).
        for id in &self.history.order {
            if cache.blocks.contains_key(id) {
                continue;
            }
            let Some(block) = self.history.blocks.get(id) else {
                continue;
            };
            let persist = match block {
                Block::ToolCall { call_id, .. } => self
                    .history
                    .tool_states
                    .get(call_id)
                    .map(|s| s.is_terminal())
                    .unwrap_or(false),
                _ => true,
            };
            if !persist {
                continue;
            }
            if let Some(artifact) = self.history.artifacts.get(id) {
                if !artifact.is_empty() {
                    cache.blocks.insert(*id, artifact.clone());
                }
            }
        }
        self.history.cache_dirty = false;
        if cache.blocks.is_empty() {
            return None;
        }
        crate::perf::record_value("layout_cache:artifacts", cache.blocks.len() as u64);
        let total_layouts: usize = cache.blocks.values().map(|a| a.layouts.len()).sum();
        crate::perf::record_value("layout_cache:layouts", total_layouts as u64);
        Some(cache)
    }

    /// Install a previously persisted layout cache. Entries for block ids
    /// not currently in history are ignored; missing ids just become cache
    /// misses on the next render. Tool blocks in a non-terminal state
    /// still skip cache adoption so the next render rebuilds their layout.
    pub fn import_layout_cache(&mut self, cache: PersistedLayoutCache) {
        if !cache.is_compatible(crate::theme::is_light()) {
            return;
        }
        let nw = self.size().0;
        let live: HashSet<BlockId> = self.history.order.iter().copied().collect();
        for (id, mut artifact) in cache.blocks {
            if !live.contains(&id) {
                continue;
            }
            let Some(block) = self.history.blocks.get(&id) else {
                continue;
            };
            let allow = match block {
                Block::ToolCall { call_id, .. } => self
                    .history
                    .tool_states
                    .get(call_id)
                    .map(|s| s.is_terminal())
                    .unwrap_or(false),
                _ => true,
            };
            if !allow {
                continue;
            }
            // Drop any layouts that would not paint correctly at the
            // current terminal width.
            artifact
                .layouts
                .retain(|(k, b)| k.width == nw || b.is_valid_at(nw));
            if artifact.is_empty() {
                continue;
            }
            self.history
                .artifacts
                .entry(id)
                .and_modify(|a| {
                    for (k, b) in &artifact.layouts {
                        a.insert(*k, b.clone());
                    }
                })
                .or_insert(artifact);
        }
        self.history.cache_width = nw as usize;
        self.history.cache_dirty = false;
    }

    /// Returns (block_index, full_text) for each User block. The index is
    /// the position in the ordered history and is the value expected by
    /// `truncate_to`.
    pub fn user_turns(&self) -> Vec<(usize, String)> {
        self.history
            .order
            .iter()
            .enumerate()
            .filter_map(|(i, id)| match self.history.blocks.get(id) {
                Some(Block::User { text, .. }) => Some((i, text.clone())),
                _ => None,
            })
            .collect()
    }

    /// Truncate blocks so that only blocks before `block_idx` remain.
    pub fn truncate_to(&mut self, block_idx: usize) {
        self.history.truncate(block_idx);
        self.active_tools.clear();
        self.active_agents.clear();
        self.redraw();
    }

    pub fn draw_prompt(&mut self, state: &InputState, mode: protocol::Mode, width: usize) {
        let mut frame = Frame::begin(&*self.backend);
        self.draw_frame(
            &mut frame,
            width,
            Some(FramePrompt {
                state,
                mode,
                queued: &[],
                prediction: None,
            }),
            None,
        );
    }

    /// Update spinner animation state. Call before rendering.
    pub fn update_spinner(&mut self) {
        if let Some(elapsed) = self.working.elapsed() {
            let frame = (elapsed.as_millis() / 150) as usize % SPINNER_FRAMES.len();
            if frame != self.working.last_spinner_frame {
                self.working.last_spinner_frame = frame;
                self.prompt.dirty = true;
            }
        }
    }

    /// Returns true when there is content or prompt work to render.
    pub fn needs_draw(&self, is_dialog: bool) -> bool {
        let has_new_blocks = self.history.has_unflushed();
        if is_dialog {
            has_new_blocks || (self.has_ephemeral() && self.prompt.dirty)
        } else {
            has_new_blocks || self.prompt.dirty
        }
    }

    /// Whether any streaming overlay element is active.
    fn has_ephemeral(&self) -> bool {
        self.active_thinking.is_some()
            || self.active_text.is_some()
            || !self.active_tools.is_empty()
            || !self.active_agents.is_empty()
            || self.active_exec.is_some()
    }

    /// Write every ephemeral element into `out` with `newline()` for
    /// inter-item gaps. The caller feeds a `SpanCollector` and then
    /// tail-crops the flat line stream — content-agnostic cropping,
    /// so streaming bash output, a partial markdown table, and a
    /// multi-line tool command are all dropped oldest-first together.
    fn render_ephemeral_into<S: LayoutSink>(&self, out: &mut S, width: usize) {
        // `last_committed` is borrowed — no clone. As streaming items
        // are emitted they are also stored in `prev_synth` so the next
        // item's gap is computed against the most recently emitted
        // element rather than the last committed one.
        let last_committed: Option<&Block> = self.history.last_block();
        let mut prev_synth: Option<Block> = None;
        let mut had_streaming = false;

        // ── Active thinking ─────────────────────────────────────────
        if let Some(ref at) = self.active_thinking {
            if self.show_thinking {
                let content = match (at.paragraph.is_empty(), at.current_line.is_empty()) {
                    (true, true) => None,
                    (true, false) => Some(at.current_line.clone()),
                    (false, true) => Some(at.paragraph.clone()),
                    (false, false) => Some(format!("{}\n{}", at.paragraph, at.current_line)),
                };
                if let Some(content) = content.filter(|t| !t.trim().is_empty()) {
                    let block = Block::Thinking { content };
                    let gap = prev_synth
                        .as_ref()
                        .or(last_committed)
                        .map(|p| gap_between(&Element::Block(p), &Element::Block(&block)))
                        .unwrap_or(0);
                    emit_newlines(out, gap);
                    render_block(out, &block, None, width, self.show_thinking);
                    prev_synth = Some(block);
                    had_streaming = true;
                }
            } else {
                // Animated summary: aggregate committed Thinking blocks
                // with the in-flight text so the count keeps ticking
                // even right after a paragraph commit.
                let mut combined = collect_trailing_thinking(
                    self.history.order.iter().map(|id| &self.history.blocks[id]),
                );
                if !at.paragraph.is_empty() {
                    if !combined.is_empty() {
                        combined.push('\n');
                    }
                    combined.push_str(&at.paragraph);
                }
                if !at.current_line.is_empty() {
                    if !combined.is_empty() {
                        combined.push('\n');
                    }
                    combined.push_str(&at.current_line);
                }
                if !combined.is_empty() {
                    let (label, line_count) = thinking_summary(&combined);
                    emit_newlines(out, self.thinking_summary_gap());
                    render_thinking_summary(out, width, &label, line_count, true);
                    had_streaming = true;
                }
            }
        }

        // ── Active text (paragraph, code line, or partial table) ───
        if let Some(ref at) = self.active_text {
            let in_table =
                !at.table_rows.is_empty() || at.current_line.trim_start().starts_with('|');

            let block_opt: Option<Block> = if in_table {
                // Partial markdown table rendered live. Paint cropping
                // handles overflow; commits normally when complete.
                let mut total = at.table_rows.iter().map(|r| r.len() + 1).sum::<usize>();
                let cur_trim = at.current_line.trim_start();
                let append_cur = cur_trim.starts_with('|');
                if append_cur {
                    total += at.current_line.len() + 1;
                }
                if total == 0 {
                    None
                } else {
                    let mut content = String::with_capacity(total);
                    for row in &at.table_rows {
                        if !content.is_empty() {
                            content.push('\n');
                        }
                        content.push_str(row);
                    }
                    if append_cur {
                        if !content.is_empty() {
                            content.push('\n');
                        }
                        content.push_str(&at.current_line);
                    }
                    Some(Block::Text { content })
                }
            } else if at.in_code_block.is_some() && !at.current_line.is_empty() {
                Some(Block::CodeLine {
                    content: at.current_line.clone(),
                    lang: at.in_code_block.clone().unwrap_or_default(),
                })
            } else if !at.paragraph.is_empty() || !at.current_line.trim().is_empty() {
                let mut content =
                    String::with_capacity(at.paragraph.len() + at.current_line.len() + 1);
                content.push_str(&at.paragraph);
                if !at.current_line.is_empty() {
                    if !content.is_empty() {
                        content.push('\n');
                    }
                    content.push_str(&at.current_line);
                }
                (!content.trim().is_empty()).then_some(Block::Text { content })
            } else {
                None
            };

            if let Some(block) = block_opt {
                let gap = prev_synth
                    .as_ref()
                    .or(last_committed)
                    .map(|p| gap_between(&Element::Block(p), &Element::Block(&block)))
                    .unwrap_or(0);
                emit_newlines(out, gap);
                render_block(out, &block, None, width, self.show_thinking);
                prev_synth = Some(block);
                had_streaming = true;
            }
        }

        // ── Active tools ───────────────────────────────────────────
        let mut tool_count = 0usize;
        for tool in self.active_tools.iter() {
            let tool_gap = if tool_count == 0 {
                if had_streaming {
                    1
                } else if let Some(p) = prev_synth.as_ref().or(last_committed) {
                    gap_between(&Element::Block(p), &Element::ActiveTool)
                } else {
                    0
                }
            } else {
                gap_between(&Element::ActiveTool, &Element::ActiveTool)
            };
            emit_newlines(out, tool_gap);
            render_tool(
                out,
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
            tool_count += 1;
        }

        // ── Active blocking agents ─────────────────────────────────
        for (i, agent) in self.active_agents.iter().enumerate() {
            let agent_gap = if i > 0 || tool_count > 0 {
                1
            } else if let Some(p) = prev_synth.as_ref().or(last_committed) {
                gap_between(&Element::Block(p), &Element::ActiveTool)
            } else {
                0
            };
            emit_newlines(out, agent_gap);
            let elapsed = agent
                .final_elapsed
                .unwrap_or_else(|| agent.start_time.elapsed());
            let agent_block = Block::Agent {
                agent_id: agent.agent_id.clone(),
                slug: agent.slug.clone(),
                blocking: true,
                tool_calls: agent.tool_calls.clone(),
                status: agent.status,
                elapsed: Some(elapsed),
            };
            render_block(out, &agent_block, None, width, self.show_thinking);
        }

        // ── Active exec ────────────────────────────────────────────
        if let Some(ref exec) = self.active_exec {
            let exec_gap = if !self.active_agents.is_empty() || tool_count > 0 {
                1
            } else if let Some(p) = prev_synth.as_ref().or(last_committed) {
                gap_between(&Element::Block(p), &Element::ActiveExec)
            } else {
                0
            };
            emit_newlines(out, exec_gap);
            render_active_exec(out, exec, width);
        }
    }

    /// Unified rendering entry point. Renders pending blocks + active
    /// overlay, then either the prompt (`Some`) or nothing (`None` =
    /// dialog covers it). `dialog_height` is the height of the active
    /// dialog in dialog mode — used to reserve space so the overlay
    /// tail-crops above it instead of fighting the dialog's own layout.
    ///
    /// The caller owns the `Frame` (sync lifecycle). This method only
    /// queues draw commands into the provided output buffer.
    ///
    /// Returns `true` when content-only mode drew something (caller
    /// should re-dirty any overlay dialog so it repaints on top).
    pub fn draw_frame(
        &mut self,
        out: &mut RenderOut,
        width: usize,
        prompt: Option<FramePrompt>,
        dialog_height: Option<u16>,
    ) -> (bool, Option<DialogPlacement>) {
        let _perf = crate::perf::begin("render:frame");

        self.update_spinner();

        let has_new_blocks = self.history.has_unflushed();
        let is_dialog = prompt.is_none();
        let has_ephemeral = self.has_ephemeral();

        // Seed `term_width` before the dialog-only fast path below,
        // which returns without reaching the main `init_cursor` call.
        let (seed_term_w, seed_term_h) = self.size();
        out.init_cursor(out.cursor_row, seed_term_w, seed_term_h);

        // Dialog mode: only repaint the content region when new blocks
        // land, the overlay has changed, or the dialog height changed
        // (needs full layout recomputation).  But ALWAYS return a
        // valid placement so the dialog itself can redraw (e.g. after
        // the user navigated within it).
        let dialog_height_changed =
            is_dialog && dialog_height.unwrap_or(0) != self.prompt.prev_dialog_height;
        if is_dialog
            && self.prompt.drawn
            && !has_new_blocks
            && !dialog_height_changed
            && !(has_ephemeral && self.prompt.dirty)
        {
            // Content hasn't changed — skip the expensive repaint but
            // return the last placement so the dialog can still draw.
            let placement = self.prompt.prev_dialog_row.map(|row| {
                let mut dh = dialog_height.unwrap_or(0);
                if self.constrain_dialog {
                    let (_, th) = self.size();
                    let half_h = th / 2;
                    // Use the stored anchor and prev_rows (overlay+gap)
                    // from the previous frame as approximation.
                    let anchor = self.prompt.anchor_row.unwrap_or(0);
                    let overhead = self.prompt.prev_rows + 2;
                    let natural = th.saturating_sub(anchor + overhead);
                    dh = dh.min(half_h.max(natural));
                }
                let max_avail = self.size().1.saturating_sub(2 + row);
                DialogPlacement {
                    row,
                    granted_rows: dh.min(max_avail),
                }
            });
            return (false, placement);
        }
        // Full mode: skip if nothing changed.
        if !is_dialog && !has_new_blocks && !self.prompt.dirty {
            return (false, None);
        }

        // ── Position cursor ─────────────────────────────────────────────
        let (term_w, term_h) = self.size();
        let explicit_anchor = self.prompt.anchor_row.take();
        let draw_start_row = explicit_anchor.unwrap_or_else(|| self.cursor_y());

        // Initialize cursor tracking for this frame.
        out.init_cursor(draw_start_row, term_w, term_h);
        // Reposition when the prompt was previously drawn (incremental
        // update) OR when an explicit anchor was set (e.g. after
        // redraw/clear/rewind where the cursor may not match the anchor).
        if self.prompt.drawn || explicit_anchor.is_some() {
            out.move_to(0, draw_start_row);
        }
        // NOTE: out.row stays None during history.render so blocks use
        // scroll-mode newline (\r\n → scrollback), not overlay-mode
        // newline (MoveTo).  Set it to Some only after blocks are done.

        // ── Render blocks (scroll mode — commits to scrollback) ─────
        let block_rows = self.history.render(out, width, self.show_thinking);

        // `cursor_row` is ground truth after scroll-mode rendering.
        let base_anchor = out.cursor_row;

        // Switch to overlay positioning for the ephemeral content and
        // dialog that follow.
        if is_dialog {
            out.row = Some(base_anchor);
        }

        // ── Lay out ephemeral overlay (measure only) ────────────────
        let (overlay_flat, overlay_rows) = if has_ephemeral {
            let mut col = SpanCollector::new(width as u16);
            self.render_ephemeral_into(&mut col, width);
            let flat = col.finish();
            let rows = flat.lines.len() as u16;
            (Some(flat), rows)
        } else {
            (None, 0)
        };

        // ── Measure total mutable region ────────────────────────────
        let prompt_gap: u16 = if self.has_content() { 1 } else { 0 };
        // For constrained dialogs, cap the effective height to
        // max(h/2, natural_space) so the dialog doesn't scroll the
        // viewport more than half the terminal.  Unconstrained dialogs
        // (confirm, question) use their full requested height.
        let raw_dialog_height = dialog_height;
        let dialog_height = if is_dialog && self.constrain_dialog {
            dialog_height.map(|dh| {
                let half_h = term_h / 2;
                let overhead = overlay_rows + prompt_gap + 2;
                let natural = term_h.saturating_sub(base_anchor + overhead);
                dh.min(half_h.max(natural))
            })
        } else {
            dialog_height
        };

        let prompt_height: u16 = if let Some(ref p) = prompt {
            self.measure_prompt_height(p.state, width, p.queued, p.prediction)
        } else {
            // Reserve dialog + bottom gap + status bar.
            let dh = dialog_height.unwrap_or(self.prompt.prev_prompt_ui_rows.max(1));
            // Constrained dialogs leave room for the ephemeral overlay
            // so it doesn't get tail-cropped.  Unconstrained dialogs
            // (confirm, question) take priority over the overlay.
            let cap = if self.constrain_dialog {
                term_h.saturating_sub(overlay_rows)
            } else {
                term_h
            };
            (dh + 2).min(cap)
        };
        // Only count overlay rows that will actually be visible. When
        // an unconstrained dialog fills the terminal, the overlay is
        // fully cropped — including it in total_mutable would scroll
        // extra blank lines into scrollback.
        let viewport_for_overlay = term_h.saturating_sub(prompt_gap + prompt_height);
        let effective_overlay = overlay_rows.min(viewport_for_overlay);
        let total_mutable = effective_overlay + prompt_gap + prompt_height;

        // ── ScrollUp if mutable region overflows viewport ────────
        let _ = out.queue(cursor::MoveTo(0, base_anchor));
        let available = term_h.saturating_sub(base_anchor);
        let scroll_amount = total_mutable.saturating_sub(available);

        // Clear upfront only when scrolling. Without scrolling, each
        // painted line already clears its own trailing residue via
        // newline, and the dialog/prompt cleans up below itself — so
        // skipping the bulk clear avoids a visible blank→repaint flash
        // on terminals with imperfect synchronized-update support.
        let needs_scroll = scroll_amount > 0;
        if needs_scroll {
            let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));
            out.scroll_up(scroll_amount);
            self.has_scrollback = true;
        }
        let final_anchor = base_anchor.saturating_sub(scroll_amount);

        // Switch to absolute positioning. From here on, everything
        // uses MoveTo via `out.row` — position is always exact.
        out.row = Some(final_anchor);
        let _ = out.queue(cursor::MoveTo(0, final_anchor));

        // ── Paint ephemeral overlay ─────────────────────────────────
        let ephemeral_rows: u16 = if let Some(flat) = overlay_flat {
            let theme = crate::theme::snapshot();
            let pctx = PaintContext {
                theme: &theme,
                term_width: width as u16,
            };
            // Tail-crop: if overlay itself exceeds viewport above
            // prompt, drop lines from the head.
            let viewport_for_overlay = term_h.saturating_sub(prompt_gap + prompt_height);
            let crop_head =
                overlay_rows.saturating_sub(viewport_for_overlay.saturating_sub(final_anchor));
            for line in &flat.lines[crop_head as usize..] {
                paint_line(out, line, &pctx);
            }
            overlay_rows.saturating_sub(crop_head)
        } else {
            0
        };

        // ── Render prompt or dialog ─────────────────────────────────
        if let Some(p) = prompt {
            // Gap between content and prompt.
            for _ in 0..prompt_gap {
                out.overlay_newline();
            }

            // `out.row` is set, so all newline calls inside
            // draw_prompt_sections use MoveTo — position is exact.
            let prompt_start_row = out.row.unwrap();
            let available_height = term_h.saturating_sub(prompt_start_row) as usize;
            let new_rows = self.draw_prompt_sections(
                out,
                p.state,
                p.mode,
                width,
                p.queued,
                p.prediction,
                available_height,
            );

            self.prompt.prev_rows = new_rows;
            self.prompt.prev_prompt_ui_rows = new_rows;

            self.prompt.anchor_row = Some(final_anchor);
            self.prompt.prev_dialog_row = Some(final_anchor + ephemeral_rows + prompt_gap);
            self.prompt.drawn = true;
            self.prompt.dirty = false;
            if scroll_amount > 0 {
                self.content_start_row = Some(
                    term_h.saturating_sub(ephemeral_rows + prompt_gap + new_rows + block_rows),
                );
            } else if self.content_start_row.is_none() {
                self.content_start_row = Some(draw_start_row);
            }

            // When the upfront Clear::FromCursorDown was skipped
            // (deferred_clear), erase stale rows that linger below the
            // freshly painted content. SavePosition / RestorePosition
            // preserve the input cursor that draw_prompt_sections placed.
            if !needs_scroll {
                let cleanup = prompt_start_row + new_rows;
                if cleanup < term_h {
                    let _ = out.queue(cursor::SavePosition);
                    let _ = out.queue(cursor::MoveTo(0, cleanup));
                    let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));
                    let _ = out.queue(cursor::RestorePosition);
                }
            }

            (false, None)
        } else {
            // ── Dialog mode ─────────────────────────────────────────
            // Gap between chat content and dialog top — mirrors the
            // prompt_gap above, but only when there IS content above
            // AND the dialog doesn't fill the full available space
            // (otherwise the gap wastes a valuable row).
            let has_content_above = block_rows > 0 || ephemeral_rows > 0 || self.has_content();
            let overlay_end = final_anchor + ephemeral_rows;
            let dh = dialog_height.unwrap_or(0);
            let max_no_gap = term_h.saturating_sub(1 + overlay_end);
            let gap: u16 = if has_content_above && dh < max_no_gap {
                out.overlay_newline();
                1
            } else {
                0
            };

            let content_rows = block_rows + ephemeral_rows + gap;
            let dialog_row = overlay_end + gap;
            // Reserve 1 row for the status bar and 1 row for the gap
            // between dialog and status bar (always present).
            let max_available = term_h.saturating_sub(2 + dialog_row);
            let granted_rows = dh.min(max_available);

            self.prompt.anchor_row = Some(final_anchor);
            self.prompt.prev_dialog_row = Some(dialog_row);
            self.prompt.prev_dialog_height = raw_dialog_height.unwrap_or(0);
            self.prompt.prev_rows = ephemeral_rows + gap;
            self.prompt.drawn = true;
            self.prompt.dirty = false;

            let placement = if granted_rows > 0 {
                Some(DialogPlacement {
                    row: dialog_row,
                    granted_rows,
                })
            } else {
                None
            };
            (content_rows > 0, placement)
        }
    }

    /// Measure prompt height without painting. Used by `draw_frame` to
    /// compute ScrollUp before entering overlay mode.
    fn measure_prompt_height(
        &self,
        state: &InputState,
        width: usize,
        queued: &[String],
        prediction: Option<&str>,
    ) -> u16 {
        let usable = width.saturating_sub(2);
        let text_w = usable.saturating_sub(2).max(1);

        // Extra rows: notification + queued + stash + btw.
        let notification: u16 = if self.notification.is_some() { 1 } else { 0 };
        let stash: u16 = if state.stash.is_some() { 1 } else { 0 };

        let mut queued_rows = 0u16;
        for msg in queued {
            for line in queued_logical_lines(msg) {
                let chars = line.chars().count();
                queued_rows += if chars == 0 {
                    1
                } else {
                    chars.div_ceil(text_w) as u16
                };
            }
        }

        let btw_rows: u16 = if let Some(ref btw) = self.btw {
            let term_h = self.size().1 as usize;
            let max_lines = btw_max_body_rows(term_h).max(1);
            let body = match btw.response {
                Some(_) => {
                    let visible = btw.wrapped.len().min(max_lines) as u16;
                    visible + 2 // body lines + blank + hint
                }
                None => 1, // spinner
            };
            1 + body + 1 // header + body + separator
        } else {
            0
        };

        // Input rows.
        let show_prediction = prediction.is_some() && state.buf.is_empty();
        let input_rows: u16 = if show_prediction {
            1
        } else {
            let (visual_lines, _, _, _) = wrap_and_locate_cursor(&state.buf, &[], 0, usable);
            visual_lines.len() as u16
        };

        // Completions / status.
        let menu_rows = state.menu_rows();
        let comp_rows: u16 = if menu_rows > 0 {
            menu_rows as u16
        } else {
            completion_reserved_rows(state.completer.as_ref()) as u16
        };
        let status_rows: u16 = if comp_rows == 0 { 1 } else { 0 };

        notification
            + queued_rows
            + stash
            + btw_rows
            + 1 // top bar
            + input_rows
            + 1 // bottom bar
            + status_rows
            + comp_rows
    }

    /// Render the prompt section. `out.row` MUST be set (overlay mode)
    /// so all line advances use MoveTo. Returns the total rows painted.
    #[allow(clippy::too_many_arguments)]
    fn draw_prompt_sections(
        &mut self,
        out: &mut RenderOut,
        state: &InputState,
        mode: protocol::Mode,
        width: usize,
        queued: &[String],
        prediction: Option<&str>,
        height: usize,
    ) -> u16 {
        let _perf = crate::perf::begin("render:prompt");
        // Cache state for dialog-mode status line rendering.
        self.last_vim_enabled = state.vim_enabled();
        self.last_vim_mode = state.vim_mode();
        self.last_mode = mode;
        self.prompt.soft_cursor = None;
        let usable = width.saturating_sub(2);
        // Neutralize any styling carried over from the preceding
        // history/overlay paint in this same frame before the prompt
        // sections start printing plain text.
        out.reset_style();
        let notification_rows = render_notification(out, self.notification.as_ref(), usable);
        let queued_visual = render_queued(out, queued, usable);
        let queued_rows = queued_visual as usize;
        let stash_rows = render_stash(out, &state.stash, usable);
        let term_h = self.size().1 as usize;
        let btw_visual = if let Some(ref mut btw) = self.btw {
            let max_btw = btw_max_body_rows(term_h);
            render_btw(out, btw, usable, max_btw, state.vim_enabled()) as usize
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
                bold: false,
                dim: false,
                priority: 2,
            });
            if self.reasoning_effort != protocol::ReasoningEffort::Off {
                let effort = self.reasoning_effort;
                right_spans.push(BarSpan {
                    text: format!(" {}", effort.label()),
                    color: reasoning_color(effort),
                    bg: None,
                    bold: false,
                    dim: false,
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
                        bold: false,
                        dim: false,
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
                    bold: false,
                    dim: false,
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
                    bold: false,
                    dim: false,
                    priority: 2,
                });
            }
            right_spans.push(BarSpan {
                text: format!(" {}", crate::metrics::format_cost(self.session_cost_usd)),
                color: theme::muted(),
                bg: None,
                bold: false,
                dim: false,
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
        out.newline();

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
        let (visual_lines, cursor_line, _, cursor_char_in_line) =
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

        // 2 = top bar (above input) + bottom bar (below input).
        const PROMPT_BARS: usize = 2;
        let fixed_base = notification_rows as usize
            + stash_rows as usize
            + queued_rows
            + btw_visual
            + PROMPT_BARS
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
        let show_prediction = prediction.is_some() && state.buf.is_empty();
        if show_prediction {
            let pred = prediction.unwrap();
            let first_line = pred.lines().next().unwrap_or(pred);
            // Draw the cursor at column 1 (after the leading space).
            let cursor_row = out.row.unwrap_or(0);
            draw_soft_cursor(out, 1, cursor_row);
            let _ = out.queue(cursor::MoveTo(2, cursor_row));
            out.push_dim();
            let msg: String = first_line.chars().take(usable.saturating_sub(2)).collect();
            out.print(&msg);
            out.pop_style();
            let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
            out.newline();
        }

        // Compute cumulative display-char offset for each visual line.
        // Must match the counting logic in wrap_and_locate_cursor: each
        // visual line contributes its char count, and each '\n' in the
        // display buffer contributes 1 additional char between logical lines.
        let line_char_offsets = compute_visual_line_offsets(&display_buf, &visual_lines);

        let has_scrollbar = total_content_rows > content_rows && content_rows > 0;
        let (thumb_start, thumb_end) = if has_scrollbar {
            let thumb_size = (content_rows * content_rows / total_content_rows).max(1);
            let max_scroll = total_content_rows - content_rows;
            let thumb_pos = scroll_offset
                .checked_mul(content_rows.saturating_sub(thumb_size))
                .and_then(|v| v.checked_div(max_scroll))
                .unwrap_or(0);
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
            let line_cursor = if abs_idx == cursor_line && self.focused {
                Some(cursor_char_in_line)
            } else {
                None
            };
            out.print(" ");
            if has_arg_space && abs_idx == 0 {
                // Command prefix in accent, argument text in normal style.
                let (prefix, hint) = cmd_hint.as_ref().unwrap();
                let prefix_len = prefix.chars().count();
                let line_chars = line.chars().count();
                // Build kinds: accent for the prefix chars, plain for the rest.
                let mut cmd_kinds = vec![SpanKind::AtRef; prefix_len.min(line_chars)];
                cmd_kinds.resize(line_chars, SpanKind::Plain);
                render_styled_chars(out, line, &cmd_kinds, line_sel, line_cursor);
                // Show hint only when exactly one trailing space follows /cmd —
                // additional spaces mean the user started typing, so the hint
                // should disappear rather than shift right.
                if line_chars >= prefix_len && state.buf == format!("{prefix} ") {
                    let max = usable.saturating_sub(prefix_len + 2);
                    let truncated: String = if hint.chars().count() > max {
                        let mut s: String = hint.chars().take(max.saturating_sub(1)).collect();
                        s.push('…');
                        s
                    } else {
                        hint.clone()
                    };
                    out.push_dim();
                    out.print(&truncated);
                    out.pop_style();
                }
            } else if has_arg_space {
                render_styled_chars(out, line, kinds, line_sel, line_cursor);
            } else if is_command {
                // All chars are accent-colored; reuse AtRef kind for accent rendering.
                let accent_kinds = vec![SpanKind::AtRef; line.chars().count()];
                render_styled_chars(out, line, &accent_kinds, line_sel, line_cursor);
            } else if (is_exec || is_exec_invalid) && abs_idx == 0 && line.starts_with('!') {
                // Render the `!` prefix with its own style (possibly selected).
                let bang_cursor = line_cursor == Some(0);
                let bang_selected = line_sel.is_some_and(|(s, _)| s == 0);
                if bang_cursor {
                    let (fg, bg) = cursor_colors();
                    out.set_fg(fg);
                    out.set_bg(bg);
                    out.print("!");
                    out.reset_style();
                } else {
                    out.push_style(StyleState {
                        fg: Some(Color::Red),
                        bg: if bang_selected {
                            Some(theme::selection_bg())
                        } else {
                            None
                        },
                        bold: true,
                        ..StyleState::default()
                    });
                    out.print("!");
                    out.pop_style();
                }
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
                let rest_cursor = line_cursor.and_then(|c| c.checked_sub(1));
                render_styled_chars(out, &line[1..], &kinds[1..], rest_sel, rest_cursor);
            } else {
                render_styled_chars(out, line, kinds, line_sel, line_cursor);
            }
            let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
            if line_cursor.is_some() {
                self.prompt.soft_cursor = Some((1 + cursor_char_in_line as u16, out.cursor_row));
            }
            if has_scrollbar {
                let bg = if li >= thumb_start && li < thumb_end {
                    theme::scrollbar_thumb()
                } else {
                    theme::scrollbar_track()
                };
                let _ = out.queue(cursor::MoveToColumn((width as u16).saturating_sub(1)));
                out.push_bg(bg);
                out.print(" ");
                out.pop_style();
            }
            out.newline();
        }

        draw_bar(out, width, None, None, bar_color);

        // Status line below the prompt:
        // pill(spinner+slug) mode vim_mode · status time · speed · procs · agents
        let status_line_rows = if comp_rows == 0 {
            out.newline();
            self.render_status_line(out);
            1
        } else {
            0
        };

        if comp_rows > 0 {
            out.newline();
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

        // Mirror of `fixed_base`'s structure: extras, top bar, input
        // content, bottom bar, status line / completions.
        (notification_rows as usize
            + stash_rows as usize
            + queued_rows
            + btw_visual
            + 1 // top bar (above input)
            + content_rows
            + 1 // bottom bar (below input)
            + status_line_rows
            + comp_rows) as u16
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

    out.print(" ");
    out.push_style(StyleState {
        fg: if notification.is_error {
            Some(theme::ERROR)
        } else {
            None
        },
        bold: true,
        ..StyleState::default()
    });
    out.print(label);
    out.pop_style();
    out.print("  ");

    let msg: String = notification.message.chars().take(max_msg).collect();
    out.push_dim();
    out.print(&msg);
    out.pop_style();
    out.overlay_newline();
    1
}

fn render_stash(out: &mut RenderOut, stash: &Option<InputSnapshot>, usable: usize) -> u16 {
    let Some(_) = stash else {
        return 0;
    };
    let text = "› Stashed (ctrl+s to unstash)";
    let display: String = text.chars().take(usable).collect();
    out.print("  ");
    out.push_style(StyleState {
        fg: Some(theme::muted()),
        dim: true,
        ..StyleState::default()
    });
    out.print(&display);
    out.pop_style();
    out.overlay_newline();
    1
}

/// Mirror Block::User's line preprocessing for a queued message:
/// expand tabs, strip leading/trailing blank lines, trim trailing
/// whitespace on each remaining line.
fn queued_logical_lines(msg: &str) -> Vec<String> {
    let all_lines: Vec<String> = msg.lines().map(|l| l.replace('\t', "    ")).collect();
    let start = all_lines.iter().position(|l| !l.is_empty()).unwrap_or(0);
    let end = all_lines
        .iter()
        .rposition(|l| !l.is_empty())
        .map_or(0, |i| i + 1);
    all_lines[start..end]
        .iter()
        .map(|l| l.trim_end().to_string())
        .collect()
}

fn render_queued(out: &mut RenderOut, queued: &[String], usable: usize) -> u16 {
    // Mirrors Block::User rendering (blocks.rs) with a 1-char indent.
    let indent = 1usize;
    let text_w = usable.saturating_sub(indent + 1).max(1);
    let mut rows = 0u16;
    for msg in queued {
        let is_command = crate::completer::Completer::is_command(msg.trim());
        let logical_lines = queued_logical_lines(msg);
        let wraps = logical_lines.iter().any(|l| l.chars().count() > text_w);
        let multiline = logical_lines.len() > 1 || wraps;
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
        for line in &logical_lines {
            if line.is_empty() {
                let fill = if block_w > 0 { block_w + 1 } else { 2 };
                out.print(&" ".repeat(indent));
                out.push_bg(theme::user_bg());
                out.print(&" ".repeat(fill));
                out.pop_style();
                out.overlay_newline();
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
                out.print(&" ".repeat(indent));
                out.push_style(StyleState {
                    bg: Some(theme::user_bg()),
                    bold: true,
                    ..StyleState::default()
                });
                out.print(" ");
                blocks::print_user_highlights(out, chunk, &[], is_command);
                out.print(&" ".repeat(trailing));
                out.pop_style();
                out.overlay_newline();
                rows += 1;
            }
        }
    }
    rows
}

/// Chrome rows the BTW block reserves around its body content (header
/// row + bar row + input rows etc., before the body fills the rest).
const BTW_CHROME_ROWS: usize = 4;

/// Maximum body lines the BTW block displays at the given terminal
/// height. Capped at half the terminal so the BTW never dominates the
/// screen, with `BTW_CHROME_ROWS` taken out for header/input chrome.
fn btw_max_body_rows(term_h: usize) -> usize {
    (term_h / 2).saturating_sub(BTW_CHROME_ROWS).max(1)
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
    out.print(" ");
    out.push_fg(theme::accent());
    out.print("/btw");
    out.pop_style();
    out.print(" ");
    let max_q = usable.saturating_sub(6); // " /btw " = 6 chars
    let q: String = btw.question.chars().take(max_q).collect();
    blocks::print_user_highlights(out, &q, &btw.image_labels, false);
    out.overlay_newline();
    rows += 1;

    // Body: response or spinner.
    match btw.response {
        Some(ref text) => {
            let render_w = usable;

            // Rebuild rendered line cache on width change or first render.
            if btw.wrapped.is_empty() || btw.wrap_width != render_w {
                btw.wrapped.clear();
                let mut buf = RenderOut::buffer();
                blocks::render_markdown_inner(&mut buf, text, render_w, "   ", false, None);
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
                out.print(line);
                out.overlay_newline();
                rows += 1;
            }

            // Blank line before hint.
            out.overlay_newline();
            rows += 1;

            // Scroll hint or dismiss hint.
            out.push_fg(theme::muted());
            if can_scroll {
                let end = (btw.scroll_offset + visible).min(total);
                out.print(&format!(
                    "   [{end}/{total}]  {}  {}  esc: close",
                    hints::nav(vim_enabled),
                    hints::scroll(vim_enabled),
                ));
            } else {
                out.print("   esc: close");
            }
            out.pop_style();
            out.overlay_newline();
            rows += 1;
        }
        None => {
            let frame = (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                / 150) as usize
                % SPINNER_FRAMES.len();
            out.push_fg(theme::muted());
            out.print(&format!("   {} thinking", SPINNER_FRAMES[frame]));
            out.pop_style();
            out.overlay_newline();
            rows += 1;
        }
    }

    // Blank separator line before the bar.
    out.overlay_newline();
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
) -> (Vec<(String, Vec<SpanKind>)>, usize, usize, usize) {
    let _perf = crate::perf::begin("render:wrap_cursor");
    let mut visual_lines: Vec<(String, Vec<SpanKind>)> = Vec::new();
    let mut cursor_line = 0;
    let mut cursor_col = 0;
    let mut cursor_char_in_line = 0usize;
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
                &mut cursor_char_in_line,
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
                        &mut cursor_char_in_line,
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
                        &mut cursor_char_in_line,
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
            &mut cursor_char_in_line,
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
    (visual_lines, cursor_line, cursor_col, cursor_char_in_line)
}

#[allow(clippy::too_many_arguments)]
fn push_visual_line(
    visual_lines: &mut Vec<(String, Vec<SpanKind>)>,
    cursor_line: &mut usize,
    cursor_col: &mut usize,
    cursor_char_in_line: &mut usize,
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
        *cursor_char_in_line = cursor_char - start_char;
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
    bold: bool,
    dim: bool,
    /// Priority for responsive dropping. 0 = always show, higher = drop first.
    priority: u8,
}

/// A span in the status line with responsive priority support.
struct StatusSpan {
    text: String,
    style: StyleState,
    /// Priority for responsive dropping. 0 = always show, higher = drop first.
    priority: u8,
    /// If true, a " · " separator is inserted before this span during rendering.
    group: bool,
    /// If true, the text can be truncated with "…" before being fully dropped.
    truncatable: bool,
}

/// Separator inserted between groups in the status line.
const STATUS_SEP: &str = " \u{00b7} "; // " · "
const STATUS_SEP_LEN: usize = 3;

/// Render status spans with responsive dropping and truncation.
///
/// Algorithm:
/// 1. Calculate total width of all visible spans (including group separators).
/// 2. While total > available width, find the highest-priority span and either
///    truncate it (if truncatable) or remove it entirely.
/// 3. Render the surviving spans left-to-right with group separators.
fn render_status_spans(
    out: &mut RenderOut,
    spans: &mut Vec<StatusSpan>,
    width: usize,
    fill_bg: Color,
) {
    // Calculate total char width including separators.
    let total_width = |spans: &[StatusSpan]| -> usize {
        let mut w = 0;
        for span in spans {
            if span.group && w > 0 {
                w += STATUS_SEP_LEN;
            }
            w += span.text.chars().count();
        }
        w
    };

    // Iteratively drop or truncate until it fits.
    while total_width(spans) > width && !spans.is_empty() {
        // Find the span with the highest priority (drop first).
        let max_pri = spans.iter().map(|s| s.priority).max().unwrap_or(0);
        if max_pri == 0 {
            break; // only priority-0 spans left, nothing more to drop
        }
        // Try truncating first: find the last truncatable span at this priority.
        let trunc_idx = spans
            .iter()
            .rposition(|s| s.priority == max_pri && s.truncatable);
        if let Some(idx) = trunc_idx {
            let available =
                width.saturating_sub(total_width(spans) - spans[idx].text.chars().count());
            if available >= 2 {
                spans[idx].text = truncate_str(&spans[idx].text, available);
                continue;
            }
        }
        // Drop ALL spans at this priority level at once (avoids orphaned separators).
        spans.retain(|s| s.priority != max_pri);
    }

    // Render.
    let sep_style = StyleState {
        fg: Some(crate::theme::muted()),
        bg: Some(fill_bg),
        dim: true,
        ..StyleState::default()
    };
    let mut col = 0;
    for span in spans.iter() {
        if span.group && col > 0 {
            out.push_style(sep_style.clone());
            out.print(STATUS_SEP);
            out.pop_style();
            col += STATUS_SEP_LEN;
        }
        out.push_style(span.style.clone());
        out.print(&span.text);
        out.pop_style();
        col += span.text.chars().count();
    }

    // Fill the rest of the line with the dark bg.
    if col < width {
        out.push_style(StyleState {
            bg: Some(fill_bg),
            ..StyleState::default()
        });
        let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
        out.pop_style();
    }
    out.reset_style();
}

pub(super) fn draw_bar(
    out: &mut RenderOut,
    width: usize,
    left: Option<&[BarSpan]>,
    right: Option<&[BarSpan]>,
    bar_color: Color,
) {
    let _perf = crate::perf::begin("render:bar");
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
            out.push_style(StyleState {
                fg: Some(span.color),
                bg: span.bg,
                bold: span.bold,
                dim: span.dim,
                ..StyleState::default()
            });
            out.print(&span.text);
            out.pop_style();
        }
        out.print(" ");
    }

    out.push_fg(bar_color);
    out.print(&dash.repeat(bar_len));
    out.pop_style();

    if !right_filtered.is_empty() {
        for span in &right_filtered {
            out.push_style(StyleState {
                fg: Some(span.color),
                bg: span.bg,
                bold: span.bold,
                dim: span.dim,
                ..StyleState::default()
            });
            out.print(&span.text);
            out.pop_style();
        }
        out.print(" ");
        out.push_fg(bar_color);
        out.print(dash);
        out.pop_style();
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
    let _perf = crate::perf::begin("render:display_spans");
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
/// `cursor_pos` is an optional char index within this line to render as a
/// software block cursor (white bg, black fg).
fn render_styled_chars(
    out: &mut RenderOut,
    line: &str,
    kinds: &[SpanKind],
    selection: Option<(usize, usize)>,
    cursor_pos: Option<usize>,
) {
    let mut current = SpanKind::Plain;
    let mut in_sel = false;
    let mut in_cursor = false;
    let char_count = line.chars().count();
    for (i, ch) in line.chars().enumerate() {
        let kind = kinds.get(i).copied().unwrap_or(SpanKind::Plain);
        let want_sel = selection.is_some_and(|(s, e)| i >= s && i < e);
        let want_cursor = cursor_pos == Some(i);

        if kind != current || want_sel != in_sel || want_cursor != in_cursor {
            // Reset previous styling before applying new.
            if in_sel || in_cursor || current != SpanKind::Plain {
                out.reset_style();
            }
            if want_cursor {
                let (fg, bg) = cursor_colors();
                out.set_fg(fg);
                out.set_bg(bg);
            } else {
                if want_sel {
                    out.set_bg(theme::selection_bg());
                }
                if kind == SpanKind::AtRef || kind == SpanKind::Attachment {
                    out.set_fg(theme::accent());
                }
            }
            current = kind;
            in_sel = want_sel;
            in_cursor = want_cursor;
        }
        out.print(ch.encode_utf8(&mut [0u8; 4]));
    }
    // Render a cursor block past the end of the line.
    if cursor_pos == Some(char_count) {
        if in_sel || in_cursor || current != SpanKind::Plain {
            out.reset_style();
        }
        let (fg, bg) = cursor_colors();
        out.set_fg(fg);
        out.set_bg(bg);
        out.print(" ");
        out.reset_style();
        return;
    }
    // Render a highlighted space for empty lines within a selection.
    if let Some((s, e)) = selection {
        if e > char_count && s <= char_count {
            if !in_sel {
                out.set_bg(theme::selection_bg());
            }
            out.print(" ");
            out.reset_style();
            return;
        }
    }
    if in_sel || in_cursor || current != SpanKind::Plain {
        out.reset_style();
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
    if comp.results.is_empty() && !comp.is_picker() {
        return 0;
    }
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
            out.push_dim();
            out.print("  no results");
            out.pop_style();
            let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
            if show_hints && max_rows > hint_rows + empty_gap {
                out.newline();
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
            out.print("  ");
            if selected {
                let ansi = item.ansi_color.unwrap_or(theme::accent_value());
                out.push_fg(Color::AnsiValue(ansi));
                out.print(&format!("● {}", label));
                out.pop_style();
            } else {
                out.push_dim();
                out.print(&format!("  {}", label));
                out.pop_style();
            }
            if let Some(ref desc) = item.description {
                let pad = (max_label + 2).saturating_sub(label.len());
                out.push_dim();
                out.print(&format!("{:>width$}{}", "", desc, width = pad));
                out.pop_style();
            }
        } else {
            out.print("  ");
            if selected {
                out.push_fg(theme::accent());
                out.print(&label);
                if let Some(ref desc) = item.description {
                    let pad = max_label - label.len() + 2;
                    out.pop_style();
                    out.push_dim();
                    out.print(&format!("{:>width$}{}", "", desc, width = pad));
                    out.pop_style();
                } else {
                    out.pop_style();
                }
            } else {
                out.push_dim();
                out.print(&label);
                if let Some(ref desc) = item.description {
                    let pad = max_label - label.len() + 2;
                    out.print(&format!("{:>width$}{}", "", desc, width = pad));
                }
                out.pop_style();
            }
        }

        drawn += 1;
        if i < last || show_hints {
            out.newline();
        } else {
            let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
        }
    }

    if show_hints && drawn < max_rows {
        draw_settings_hints(out, vim_enabled);
        drawn += 2;
    }

    drawn
}

fn draw_settings_hints(out: &mut RenderOut, vim_enabled: bool) {
    out.newline();
    out.push_dim();
    out.print(&crate::keymap::hints::join(&[
        crate::keymap::hints::picker_nav(vim_enabled),
        "enter/space: toggle",
        crate::keymap::hints::CANCEL,
    ]));
    out.pop_style();
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
            out.push_dim();
            out.print(label);
            out.pop_style();
            let col = label_col.max(label.len() + 2);
            let padding = " ".repeat(col.saturating_sub(label.len()));
            out.print(&padding);
            out.print(value);
        }
        StatsLine::Heading(text) | StatsLine::SparklineLegend(text) => {
            out.push_dim();
            out.print(text);
            out.pop_style();
        }
        StatsLine::SparklineBars(bars) => {
            out.push_fg(theme::accent());
            out.print(bars);
            out.pop_style();
        }
        StatsLine::HeatRow { label, cells } => {
            out.push_dim();
            out.print(&format!("{label} "));
            out.pop_style();
            for cell in cells {
                match cell {
                    crate::metrics::HeatCell::Empty => {
                        out.push_fg(Color::AnsiValue(238));
                        out.print(&format!("{HEAT_EMPTY} "));
                        out.pop_style();
                    }
                    crate::metrics::HeatCell::Level(lvl) => {
                        let color = HEAT_COLORS[(*lvl as usize).min(3)];
                        out.push_fg(color);
                        out.print(&format!("{HEAT_CHAR} "));
                        out.pop_style();
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
            out.newline();
        }
        out.print("  ");
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
            out.newline();
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
            out.newline();
        }

        let lw = if i < left.len() {
            out.print("  ");
            draw_stats_line(out, &left[i], left_lc);
            2 + stats_line_width(&left[i], left_lc)
        } else {
            0
        };

        if i < right.len() {
            let pad = right_col.saturating_sub(lw);
            out.print(&" ".repeat(pad));
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
        let (_, cursor_line, cursor_col, _) = wrap_and_locate_cursor("a\tb", &kinds, 3, 80);
        assert_eq!(cursor_line, 0);
        assert_eq!(cursor_col, 8);
    }

    #[test]
    fn prompt_cursor_tracks_multiple_tabs_from_prompt_column() {
        let kinds = vec![SpanKind::Plain; "\t\tb".chars().count()];
        let (_, cursor_line, cursor_col, _) = wrap_and_locate_cursor("\t\tb", &kinds, 3, 80);
        assert_eq!(cursor_line, 0);
        assert_eq!(cursor_col, 16);
    }

    #[test]
    fn prompt_cursor_uses_wide_char_display_width() {
        let kinds = vec![SpanKind::Plain; "a界b".chars().count()];
        let (_, cursor_line, cursor_col, _) = wrap_and_locate_cursor("a界b", &kinds, 3, 80);
        assert_eq!(cursor_line, 0);
        assert_eq!(cursor_col, 4);
    }

    #[test]
    fn prompt_tabs_respect_prompt_column_without_forced_wrap() {
        let kinds = vec![SpanKind::Plain; "a\tb".chars().count()];
        let (lines, cursor_line, cursor_col, _) = wrap_and_locate_cursor("a\tb", &kinds, 3, 8);
        assert_eq!(
            lines.iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>(),
            vec!["a\tb"]
        );
        assert_eq!(cursor_line, 0);
        assert_eq!(cursor_col, 8);
    }

    #[test]
    fn dialog_render_starts_with_cursor_position() {
        let mut out = RenderOut::buffer();
        let mut dialog = crate::render::HelpDialog::new(false);
        dialog.draw(&mut out, 0, 40, 12);
        let rendered = String::from_utf8(out.into_bytes()).unwrap();
        // begin_dialog_draw positions the cursor at (0, start_row).
        assert!(
            rendered.starts_with("\u{1b}[1;1H"),
            "dialog should start with cursor position, got: {rendered:?}"
        );
    }

    #[test]
    fn status_line_renders_content() {
        let mut screen = Screen::with_backend(Box::new(StdioBackend));
        screen.set_running_procs(1);
        let mut out = RenderOut::buffer();
        screen.queue_status_line(&mut out);
        let rendered = String::from_utf8(out.into_bytes()).unwrap();
        // Status line should contain the "proc" indicator.
        assert!(
            rendered.contains("proc"),
            "status line missing proc indicator: {rendered:?}"
        );
    }

    #[test]
    fn prompt_sections_reset_terminal_style_before_rendering() {
        let mut screen = Screen::with_backend(Box::new(StdioBackend));
        screen.set_anchor_row(0);
        let input = crate::input::InputState::default();
        let mut out = RenderOut::buffer();
        screen.draw_frame(
            &mut out,
            40,
            Some(FramePrompt {
                state: &input,
                mode: protocol::Mode::Normal,
                queued: &[],
                prediction: None,
            }),
            None,
        );
        let rendered = String::from_utf8(out.into_bytes()).unwrap();
        assert!(
            rendered.contains("\u{1b}[0m\u{1b}[0m"),
            "rendered: {rendered:?}"
        );
    }

    #[test]
    fn file_completer_with_no_results_reserves_no_rows() {
        let mut comp = crate::completer::Completer::files(0);
        comp.update_query("zzzzzz".into());

        assert!(comp.results.is_empty());
        assert!(!comp.is_picker());
        assert_eq!(completion_reserved_rows(Some(&comp)), 0);
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
            redact_secrets: true,
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
            redact_secrets: true,
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
        // Gap row is either "\r\n\r\n" (new form, since scroll-mode
        // `scroll_newline` drops the redundant Clear::UntilNewLine) or
        // "\r\n\x1b[K\r\n" (old form), depending on history path.
        assert!(
            rendered.contains("\r\n\r\n") || rendered.contains("\r\n\u{1b}[K\r\n"),
            "rendered: {rendered:?}"
        );
    }

    #[test]
    fn block_artifact_bounded_lru_roundtrip() {
        // Resize cycle 100 → 80 → 100 → 80 must hit cache on every repeat
        // step, because the bounded LRU keeps both widths resident.
        let mut history = BlockHistory::new();
        let id = history.push(Block::Text {
            content: "the quick brown fox jumps over the lazy dog".into(),
        });

        let mut sink = RenderOut::buffer();
        history.render(&mut sink, 100, true);
        history.flushed = 0;
        history.render(&mut sink, 80, true);
        history.flushed = 0;
        history.render(&mut sink, 100, true);
        history.flushed = 0;
        history.render(&mut sink, 80, true);

        let keys: Vec<LayoutKey> = history
            .artifacts
            .get(&id)
            .unwrap()
            .layouts
            .iter()
            .map(|(k, _)| *k)
            .collect();
        let k100 = LayoutKey {
            width: 100,
            show_thinking: true,
        };
        let k80 = LayoutKey {
            width: 80,
            show_thinking: true,
        };
        assert!(keys.contains(&k100), "expected width=100 cached: {keys:?}");
        assert!(keys.contains(&k80), "expected width=80 cached: {keys:?}");
        assert!(keys.len() <= BlockArtifact::MAX_LAYOUTS);
    }

    #[test]
    fn duplicate_block_ids_share_artifact() {
        // Two identical blocks at different positions should resolve to the
        // same `BlockId` and share a single entry in `blocks` / `artifacts`.
        let mut history = BlockHistory::new();
        let a = history.push(Block::Text {
            content: "same".into(),
        });
        let b = history.push(Block::Text {
            content: "same".into(),
        });
        assert_eq!(a, b);
        assert_eq!(history.order.len(), 2);
        assert_eq!(history.blocks.len(), 1);
        assert_eq!(history.artifacts.len(), 1);
    }

    #[test]
    fn export_render_cache_skips_blocks_without_ir() {
        let mut screen = Screen::new();
        screen.push(Block::Thinking {
            content: "alpha\nbeta".into(),
        });
        screen.history.render(&mut RenderOut::buffer(), 80, true);
        // Thinking blocks don't carry tool-output IR, so the cache is empty.
        assert!(screen.export_render_cache().is_none());
    }
}
