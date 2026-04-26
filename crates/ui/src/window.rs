use crate::edit_buffer::EditBuffer;
use crate::kill_ring::KillRing;
use crate::layout::{Border, Constraint, Gutters, Placement, Rect};
use crate::text::{byte_to_cell, cell_to_byte};
use crate::vim::{Action, ViMode, Vim, VimContext};
use crate::window_cursor::WindowCursor;
use crate::{BufId, WinId};
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewportHit {
    Scrollbar,
    Content { row: u16, col: u16 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScrollbarState {
    pub col: u16,
    pub total_rows: u16,
    pub viewport_rows: u16,
}

impl ScrollbarState {
    pub fn new(col: u16, total_rows: u16, viewport_rows: u16) -> Option<Self> {
        (viewport_rows > 0 && total_rows > viewport_rows).then_some(Self {
            col,
            total_rows,
            viewport_rows,
        })
    }

    pub fn max_scroll(&self) -> u16 {
        self.total_rows.saturating_sub(self.viewport_rows)
    }

    pub fn thumb_size(&self) -> u16 {
        let rows = self.viewport_rows as usize;
        let total = self.total_rows as usize;
        ((rows * rows) / total).max(1) as u16
    }

    pub fn max_thumb_top(&self) -> u16 {
        self.viewport_rows.saturating_sub(self.thumb_size())
    }

    /// Convert a click row (relative to viewport top) into the thumb
    /// top such that the thumb is centered on the click — the row the
    /// pointer is on lands on the middle of the thumb, not its first
    /// cell. Clamped to `[0, max_thumb_top()]`. Used by both the
    /// jump-scroll click and the in-flight drag tick so the thumb
    /// stays under the pointer.
    pub fn thumb_top_for_click(&self, rel_row: u16) -> u16 {
        let half = self.thumb_size() / 2;
        rel_row.saturating_sub(half).min(self.max_thumb_top())
    }

    pub fn scroll_from_top_for_thumb(&self, thumb_top: u16) -> u16 {
        let max_thumb = self.max_thumb_top();
        let max_scroll = self.max_scroll();
        if max_thumb == 0 || max_scroll == 0 {
            return 0;
        }
        let thumb_top = thumb_top.min(max_thumb);
        let from_top =
            (thumb_top as u32 * max_scroll as u32 + max_thumb as u32 / 2) / max_thumb as u32;
        from_top.min(u16::MAX as u32) as u16
    }

    pub fn contains(&self, rect: Rect, row: u16, col: u16) -> bool {
        col == self.col && row >= rect.top && row < rect.bottom()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowViewport {
    pub rect: Rect,
    pub content_width: u16,
    pub total_rows: u16,
    pub scroll_top: u16,
    pub scrollbar: Option<ScrollbarState>,
}

impl WindowViewport {
    pub fn new(
        rect: Rect,
        content_width: u16,
        total_rows: u16,
        scroll_top: u16,
        scrollbar: Option<ScrollbarState>,
    ) -> Self {
        Self {
            rect,
            content_width,
            total_rows,
            scroll_top,
            scrollbar,
        }
    }

    pub fn contains(&self, row: u16, col: u16) -> bool {
        self.rect.contains(row, col)
    }

    pub fn hit(&self, row: u16, col: u16) -> Option<ViewportHit> {
        if !self.contains(row, col) {
            return None;
        }
        if let Some(bar) = self.scrollbar {
            if bar.contains(self.rect, row, col) {
                return Some(ViewportHit::Scrollbar);
            }
        }
        let rel_row = row - self.rect.top;
        let rel_col = col.saturating_sub(self.rect.left);
        let max_col = self.content_width.saturating_sub(1);
        Some(ViewportHit::Content {
            row: rel_row,
            col: rel_col.min(max_col),
        })
    }
}

/// Result of a `Window::handle_mouse` call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MouseAction {
    /// Window handled the event and there's no host follow-up.
    Consumed,
    /// Window had nothing to do with the event (out-of-rect Down,
    /// non-left button, etc.). Host may treat as fall-through.
    Ignored,
    /// Window took the gesture and asks the host to route subsequent
    /// `Drag` / `Up` events back here even if the pointer leaves the
    /// rect. Returned from `Down(Left)` whenever the click landed on
    /// content.
    Capture,
    /// Window finalised a selection; host should copy `text` to the
    /// clipboard (and typically push it onto its kill ring).
    Yank(String),
}

/// Per-call context for [`Window::handle_mouse`]. The window itself
/// does not store row layout or viewport geometry — they're recomputed
/// each frame and supplied here so a single `Window` primitive can
/// drive heterogeneous backings (transcript display projection,
/// dialog buffer panel, plain split window).
pub struct MouseCtx<'a> {
    /// Display rows, one per visual line. For buffers with no soft
    /// wrap, this is the buffer's `lines()`. For projected views
    /// (transcript) it's the post-projection rows.
    pub rows: &'a [String],
    /// Byte positions in `rows.join("\n")` of soft-wrap boundaries —
    /// the word selector treats these as transparent so a word split
    /// across two display rows still selects as one unit. Empty when
    /// the rows are not soft-wrapped.
    pub soft_breaks: &'a [usize],
    /// Byte positions in `rows.join("\n")` of *hard* line breaks —
    /// real `\n` characters in the source. Used by triple-click line
    /// selection to grow the range to the full source line.
    pub hard_breaks: &'a [usize],
    /// Painted viewport rect + content width + scrollbar geometry.
    pub viewport: WindowViewport,
    /// Click sequence number (1, 2, 3, …). Hosts that don't track
    /// click cadence can pass `1` and lose word/line click — that's
    /// fine for read-only views.
    pub click_count: u8,
}

/// Slice `[s, e)` out of `rows.join("\n")` without allocating the
/// full join. Used by `mouse_down`'s yank path.
fn slice_rows(rows: &[String], s: usize, e: usize) -> String {
    let joined = rows.join("\n");
    let s = s.min(joined.len());
    let e = e.min(joined.len());
    if e <= s {
        return String::new();
    }
    let s = crate::text::snap(&joined, s);
    let e = crate::text::snap(&joined, e);
    joined[s..e].to_string()
}

#[derive(Clone, Debug)]
pub struct SplitConfig {
    pub region: String,
    pub gutters: Gutters,
}

#[derive(Clone, Debug)]
pub struct FloatConfig {
    pub placement: Placement,
    pub border: Border,
    pub title: Option<String>,
    pub zindex: u16,
    /// Whether `<C-w>` window cycling and programmatic focus can land
    /// here. `false` for popups (completer, notification) that should
    /// never steal cursor. Modeled on Neovim's `WinConfig.focusable`.
    pub focusable: bool,
    /// Whether the host should pause engine-event drain while this
    /// float is focused. True for modal permission prompts (Confirm,
    /// Question, Lua dialogs gating a parked task); false for
    /// read-only viewers (Help, Ps, Resume) that can coexist with a
    /// running turn.
    pub blocks_agent: bool,
}

impl Default for FloatConfig {
    fn default() -> Self {
        Self {
            placement: Placement::Centered {
                width: Constraint::Pct(80),
                height: Constraint::Pct(50),
            },
            border: Border::Single,
            title: None,
            zindex: 50,
            focusable: true,
            blocks_agent: false,
        }
    }
}

#[derive(Clone, Debug)]
pub enum WinConfig {
    Split(SplitConfig),
    Float(FloatConfig),
}

pub struct Window {
    pub(crate) id: WinId,
    pub buf: BufId,
    pub config: WinConfig,
    pub focusable: bool,

    pub edit_buf: EditBuffer,
    pub cpos: usize,
    pub vim: Option<Vim>,
    pub win_cursor: WindowCursor,
    pub kill_ring: KillRing,
    pub scroll_top: u16,
    pub cursor_line: u16,
    pub cursor_col: u16,
    /// Autoscroll flag for append-only buffers. `true` keeps the
    /// viewport snapped to the newest row — new content flows in
    /// below and the viewport follows. Flipped to `false` as soon as
    /// the user scrolls away from the bottom; back to `true` when
    /// they scroll back to it.
    pub follow_tail: bool,
    /// One-shot "center cursor on next render" request, set by vim `zz`.
    /// Honored and cleared by the next paint that knows the viewport
    /// dimensions. Independent of `follow_tail` (which is sticky).
    pub pending_recenter: bool,
    /// Last `cpos` observed by the renderer. Compared against current
    /// `cpos` each frame to distinguish "cursor moved → ensure visible"
    /// from "wheel/scrollbar panned → leave scroll alone." `None` on
    /// first render forces an initial ensure-visible.
    pub last_render_cpos: Option<usize>,
    pub cursor_positioned: bool,
    /// Active drag-anchor span set by a double-click word-select
    /// (`[start, end)` byte range in the joined buffer). When set,
    /// drag extension grows in word units and keeps the original word
    /// inside the selection regardless of drag direction. Cleared on
    /// mouse-up or any non-mouse cursor motion.
    pub drag_anchor_word: Option<(usize, usize)>,
    /// Same as `drag_anchor_word` but for triple-click line-select.
    pub drag_anchor_line: Option<(usize, usize)>,
}

impl Window {
    pub fn new(id: WinId, buf: BufId, config: WinConfig) -> Self {
        Self {
            id,
            buf,
            config,
            focusable: true,
            edit_buf: EditBuffer::readonly(),
            cpos: 0,
            vim: None,
            win_cursor: WindowCursor::new(),
            kill_ring: KillRing::new(),
            scroll_top: 0,
            cursor_line: 0,
            cursor_col: 0,
            follow_tail: true,
            pending_recenter: false,
            last_render_cpos: None,
            cursor_positioned: false,
            drag_anchor_word: None,
            drag_anchor_line: None,
        }
    }

    pub fn id(&self) -> WinId {
        self.id
    }

    pub fn is_float(&self) -> bool {
        matches!(self.config, WinConfig::Float(_))
    }

    pub fn is_split(&self) -> bool {
        matches!(self.config, WinConfig::Split(_))
    }

    pub fn zindex(&self) -> u16 {
        match &self.config {
            WinConfig::Float(f) => f.zindex,
            WinConfig::Split(_) => 0,
        }
    }

    // ── Vim ────────────────────────────────────────────────────────────

    pub fn set_vim_enabled(&mut self, enabled: bool) {
        if enabled {
            if self.vim.is_none() {
                self.vim = Some(Vim::new());
            }
        } else {
            self.vim = None;
            self.win_cursor.clear_anchor();
        }
    }

    pub fn vim_enabled(&self) -> bool {
        self.vim.is_some()
    }

    // ── Cursor ─────────────────────────────────────────────────────────

    pub fn cursor_abs_row(&self) -> usize {
        self.scroll_top as usize + self.cursor_line as usize
    }

    pub fn selection_range(&self, rows: &[String]) -> Option<(usize, usize)> {
        let cpos = self.compute_cpos(rows);
        if let Some(ref vim) = self.vim {
            if let Some(range) = vim.visual_range(&rows.join("\n"), cpos) {
                return Some(range);
            }
        }
        self.win_cursor.range(cpos)
    }

    /// Select the word at `cpos` (vim "w" semantics: alphanumeric +
    /// underscore). `transparent` byte positions are crossed by the
    /// boundary walk as if they were word chars (used for soft-wrap
    /// `\n` so a word broken across display rows selects as one unit);
    /// must be sorted ascending. Cursor lands at the last char of the
    /// selection so the visual-range render covers the whole word.
    pub fn select_word_at_transparent(
        &mut self,
        cpos: usize,
        transparent: &[usize],
        rows: &[String],
        buf: &str,
        viewport_rows: u16,
    ) -> Option<(usize, usize)> {
        let (start, end) = crate::edit_buffer::word_range_at_transparent(buf, cpos, transparent)?;
        self.finish_range_select(start, end, rows, viewport_rows);
        Some((start, end))
    }

    /// Vim "WORD" (capital W) variant of [`Self::select_word_at_transparent`]:
    /// the token is any whitespace-delimited run, punctuation included.
    pub fn select_big_word_at_transparent(
        &mut self,
        cpos: usize,
        transparent: &[usize],
        rows: &[String],
        buf: &str,
        viewport_rows: u16,
    ) -> Option<(usize, usize)> {
        let (start, end) =
            crate::edit_buffer::big_word_range_at_transparent(buf, cpos, transparent)?;
        self.finish_range_select(start, end, rows, viewport_rows);
        Some((start, end))
    }

    /// Select the source line containing `cpos` and enter Visual mode
    /// anchored at the line's start. `hard_breaks` lists byte positions
    /// of `\n` characters that are real line breaks (not soft-wrap
    /// continuations), sorted ascending. We use plain `Visual` rather
    /// than `VisualLine` because `VisualLine` snaps to display rows
    /// (every `\n`), which would collapse selection to a single
    /// wrapped row for soft-wrapped content. Returns the byte range of
    /// the selected source line.
    pub fn select_line_at(
        &mut self,
        cpos: usize,
        hard_breaks: &[usize],
        rows: &[String],
        buf: &str,
        viewport_rows: u16,
    ) -> Option<(usize, usize)> {
        let (start, end) = crate::edit_buffer::line_range_at(buf, cpos, hard_breaks)?;
        self.finish_range_select(start, end, rows, viewport_rows);
        Some((start, end))
    }

    /// Park the cursor at the last char of `[start, end)` and anchor a
    /// Visual selection at `start`. Re-syncs `cursor_line`/`cursor_col`
    /// from the new `cpos` so the selection-highlight pass (which
    /// derives cpos via `compute_cpos`) sees the moved cursor — without
    /// this, the highlight extends only to the original click position.
    fn finish_range_select(
        &mut self,
        start: usize,
        end: usize,
        rows: &[String],
        viewport_rows: u16,
    ) {
        self.cpos = end.saturating_sub(1).max(start);
        let offsets = Self::line_start_offsets(rows);
        self.sync_from_cpos(rows, &offsets, viewport_rows);
        if let Some(vim) = self.vim.as_mut() {
            vim.begin_visual(ViMode::Visual, start);
        } else {
            self.win_cursor.set_anchor(Some(start));
        }
    }

    pub fn resync(&mut self, rows: &[String], viewport_rows: u16) {
        if rows.is_empty() {
            return;
        }
        let offsets = Self::line_start_offsets(rows);
        self.edit_buf.buf = rows.join("\n");
        self.sync_from_cpos(rows, &offsets, viewport_rows);
    }

    pub fn refocus(&mut self, rows: &[String], viewport_rows: u16) {
        if rows.is_empty() {
            self.edit_buf.buf.clear();
            self.cpos = 0;
            self.cursor_line = 0;
            self.cursor_col = 0;
            self.cursor_positioned = false;
            return;
        }
        if let Some(vim) = self.vim.as_mut() {
            if vim.mode() != ViMode::Normal {
                vim.set_mode(ViMode::Normal);
            }
        }
        if !self.cursor_positioned {
            let total = rows.len();
            let last_line = total.saturating_sub(1);
            let offsets = Self::line_start_offsets(rows);
            self.edit_buf.buf = rows.join("\n");
            self.cpos = offsets[last_line];
            self.sync_from_cpos(rows, &offsets, viewport_rows);
            self.cursor_positioned = true;
        } else {
            let offsets = self.mount(rows);
            self.sync_from_cpos(rows, &offsets, viewport_rows);
        }
        if self.win_cursor.curswant().is_none() {
            self.win_cursor.set_curswant(Some(self.cursor_col as usize));
        }
    }

    pub fn reanchor_to_visible_row(&mut self, rows: &[String], viewport_rows: u16) {
        if rows.is_empty() {
            return;
        }
        let offsets = Self::line_start_offsets(rows);
        self.edit_buf.buf = rows.join("\n");
        let total = rows.len() as u16;
        let max = total.saturating_sub(viewport_rows);
        self.scroll_top = self.scroll_top.min(max);
        let cursor_line = self.cursor_line.min(viewport_rows.saturating_sub(1));
        let target_line = (self.scroll_top + cursor_line) as usize;
        let target_line = target_line.min(rows.len() - 1);
        let line = &rows[target_line];
        let want = self
            .win_cursor
            .curswant()
            .unwrap_or(self.cursor_col as usize);
        let col_bytes = cell_to_byte(line, want);
        self.cpos = offsets[target_line] + col_bytes;
        self.cursor_col = byte_to_cell(line, col_bytes) as u16;
        self.cursor_line = cursor_line;
    }

    // ── Follow-tail ────────────────────────────────────────────────────

    /// Snap to the bottom of the buffer and re-enable autoscroll.
    /// `scroll_top = u16::MAX` is the "go to bottom" sentinel; the
    /// per-frame clamp in the render loop resolves it to `max_scroll`.
    pub fn scroll_to_bottom(&mut self) {
        self.scroll_top = u16::MAX;
        self.follow_tail = true;
    }

    // ── Navigation ─────────────────────────────────────────────────────

    pub fn compute_cpos(&self, rows: &[String]) -> usize {
        let offsets = Self::line_start_offsets(rows);
        self.visible_cpos(rows, &offsets)
    }

    fn line_start_offsets(rows: &[String]) -> Vec<usize> {
        let mut v = Vec::with_capacity(rows.len());
        let mut acc = 0usize;
        for r in rows {
            v.push(acc);
            acc += r.len() + 1;
        }
        v
    }

    fn visible_cpos(&self, rows: &[String], offsets: &[usize]) -> usize {
        let total = rows.len();
        if total == 0 {
            return 0;
        }
        let line_idx = (self.scroll_top as usize + self.cursor_line as usize).min(total - 1);
        offsets[line_idx] + cell_to_byte(&rows[line_idx], self.cursor_col as usize)
    }

    fn sync_from_cpos(&mut self, rows: &[String], offsets: &[usize], viewport_rows: u16) {
        let total = rows.len();
        if total == 0 {
            return;
        }
        let tail_byte = *offsets.last().unwrap() + rows.last().map_or(0, |r| r.len());
        self.cpos = self.cpos.min(tail_byte);
        let line_idx = match offsets.binary_search(&self.cpos) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        };
        let line = &rows[line_idx];
        let byte_col = self.cpos.saturating_sub(offsets[line_idx]);
        self.cursor_col = byte_to_cell(line, byte_col) as u16;
        let line_idx = line_idx as u16;
        let viewport_bottom = self
            .scroll_top
            .saturating_add(viewport_rows.saturating_sub(1));
        if line_idx > viewport_bottom {
            self.scroll_top = line_idx.saturating_sub(viewport_rows.saturating_sub(1));
        } else if line_idx < self.scroll_top {
            self.scroll_top = line_idx;
        }
        self.cursor_line = line_idx.saturating_sub(self.scroll_top);
    }

    fn mount(&mut self, rows: &[String]) -> Vec<usize> {
        let offsets = Self::line_start_offsets(rows);
        self.edit_buf.buf = rows.join("\n");
        self.cpos = self.visible_cpos(rows, &offsets);
        offsets
    }

    // ── Mouse dispatch ─────────────────────────────────────────────────

    /// Handle a single mouse event using the supplied `MouseCtx`
    /// (rows, soft/hard line breaks, viewport, click count).
    /// Encapsulates the cursor and Visual selection logic that the
    /// transcript pane has used for ages: click-to-position cursor,
    /// double-click word-select-and-yank, triple-click line-select-
    /// and-yank, drag extension anchored to the original word/line
    /// when applicable, and mouse-up yank-on-release.
    ///
    /// The caller owns clipboard side effects: a returned [`MouseAction::Yank`]
    /// asks the host to copy `text` and (typically) record it on the
    /// kill ring. The window's `drag_anchor_*` fields are managed
    /// internally so successive `Drag` events extend by the right unit.
    pub fn handle_mouse(&mut self, event: MouseEvent, ctx: MouseCtx) -> MouseAction {
        // Build the joined buffer once and pass it down. Mouse helpers
        // operate on this `&str` instead of `self.edit_buf.buf`, which
        // lets surfaces whose `edit_buf.buf` is *not* `rows.join("\n")`
        // (the prompt — source buffer ≠ wrapped display rows) reuse
        // `Window::handle_mouse` directly. The transcript and dialog
        // buffer panels still keep `edit_buf.buf == rows.join("\n")`
        // via their existing sync paths; the buffer arg just doesn't
        // need it to be true.
        let buf = ctx.rows.join("\n");
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => self.mouse_down(event, ctx, &buf),
            MouseEventKind::Drag(MouseButton::Left) => self.mouse_drag(event, ctx, &buf),
            MouseEventKind::Up(MouseButton::Left) => self.mouse_up(ctx, &buf),
            _ => MouseAction::Ignored,
        }
    }

    fn mouse_down(&mut self, event: MouseEvent, ctx: MouseCtx, buf: &str) -> MouseAction {
        // Hit-test against the painted viewport: anything that lands
        // on the scrollbar or outside the rect is the host's problem
        // (scrollbar drag latching, focus shift, …).
        let Some(hit) = ctx.viewport.hit(event.row, event.column) else {
            return MouseAction::Ignored;
        };
        let ViewportHit::Content {
            row: rel_row,
            col: rel_col,
        } = hit
        else {
            return MouseAction::Ignored;
        };
        if ctx.rows.is_empty() {
            return MouseAction::Consumed;
        }

        let viewport_rows = ctx.viewport.rect.height;
        let line_idx = (self.scroll_top as usize + rel_row as usize).min(ctx.rows.len() - 1);
        self.jump_to_line_col(ctx.rows, line_idx, rel_col as usize, viewport_rows);
        let cpos = self.cpos;

        match ctx.click_count {
            2 => {
                // Vim "WORD" (whitespace-delimited, punctuation in)
                // matches what users expect from a double-click in
                // a code preview: `foo.bar(baz)` selects whole.
                if let Some((s, e)) = self.select_big_word_at_transparent(
                    cpos,
                    ctx.soft_breaks,
                    ctx.rows,
                    buf,
                    viewport_rows,
                ) {
                    self.drag_anchor_word = Some((s, e));
                    self.drag_anchor_line = None;
                    return MouseAction::Yank(slice_rows(ctx.rows, s, e));
                }
                MouseAction::Capture
            }
            3 => {
                if let Some((s, e)) =
                    self.select_line_at(cpos, ctx.hard_breaks, ctx.rows, buf, viewport_rows)
                {
                    self.drag_anchor_line = Some((s, e));
                    self.drag_anchor_word = None;
                    return MouseAction::Yank(slice_rows(ctx.rows, s, e));
                }
                MouseAction::Capture
            }
            _ => {
                // Single click: anchor a Visual selection at the click
                // so a subsequent drag grows from this point. Vim and
                // non-vim paths anchor differently (vim's Visual range
                // reads cpos directly; non-vim uses `WindowCursor`).
                self.drag_anchor_word = None;
                self.drag_anchor_line = None;
                if let Some(vim) = self.vim.as_mut() {
                    vim.begin_visual(ViMode::Visual, cpos);
                } else {
                    self.win_cursor.set_anchor(Some(cpos));
                }
                MouseAction::Capture
            }
        }
    }

    fn mouse_drag(&mut self, event: MouseEvent, ctx: MouseCtx, buf: &str) -> MouseAction {
        // Drag past the rect edges still extends — clamp the cell to
        // the viewport's content area so the cursor lands on the
        // nearest visible position. Host handles edge-autoscroll on
        // a separate timer.
        let viewport_rows = ctx.viewport.rect.height;
        if viewport_rows == 0 || ctx.rows.is_empty() {
            return MouseAction::Consumed;
        }
        let rel_row = event
            .row
            .saturating_sub(ctx.viewport.rect.top)
            .min(viewport_rows.saturating_sub(1));
        let rel_col = event
            .column
            .saturating_sub(ctx.viewport.rect.left)
            .min(ctx.viewport.content_width.saturating_sub(1));
        let line_idx = (self.scroll_top as usize + rel_row as usize).min(ctx.rows.len() - 1);
        self.jump_to_line_col(ctx.rows, line_idx, rel_col as usize, viewport_rows);

        if self.drag_anchor_word.is_some() {
            self.extend_word_anchored_drag(ctx, buf);
        } else if self.drag_anchor_line.is_some() {
            self.extend_line_anchored_drag(ctx, buf);
        } else if self.vim.is_none() {
            self.win_cursor.extend(self.cpos);
        }
        MouseAction::Consumed
    }

    fn mouse_up(&mut self, ctx: MouseCtx, buf: &str) -> MouseAction {
        // Compute the final selection from rows; yank if non-empty.
        // A bare click (no drag, no double/triple-click span) leaves
        // `anchor == cpos`. Vim Visual reports an inclusive single-char
        // range in that case, so we skip the yank when no actual
        // selection was made — matches the transcript's pre-7a
        // App-level guard, but applied here so every consumer gets it.
        let cpos = self.compute_cpos(ctx.rows);
        let has_drag_span = self.drag_anchor_word.is_some() || self.drag_anchor_line.is_some();
        let has_selection = if let Some(vim) = self.vim.as_ref() {
            match vim.mode() {
                ViMode::Visual | ViMode::VisualLine => vim.visual_anchor() != Some(cpos),
                _ => false,
            }
        } else {
            self.win_cursor.anchor().is_some_and(|a| a != cpos)
        };
        let range = if has_drag_span || has_selection {
            if let Some(vim) = self.vim.as_ref() {
                vim.visual_range(buf, cpos)
            } else {
                self.win_cursor.range(cpos)
            }
        } else {
            None
        };
        let action = match range {
            Some((s, e)) if e > s => {
                let s = crate::text::snap(buf, s);
                let e = crate::text::snap(buf, e);
                if e > s {
                    MouseAction::Yank(buf[s..e].to_string())
                } else {
                    MouseAction::Consumed
                }
            }
            _ => MouseAction::Consumed,
        };
        // The user's gesture is over: clear all selection state so a
        // fresh click starts a fresh selection. Owning this here means
        // every consumer (transcript, prompt, dialog buffer) gets the
        // same lifecycle for free — no bespoke clear-anchor code in
        // the host adapters.
        if let Some(vim) = self.vim.as_mut() {
            if matches!(vim.mode(), ViMode::Visual | ViMode::VisualLine) {
                vim.set_mode(ViMode::Normal);
            }
        }
        self.win_cursor.clear_anchor();
        self.drag_anchor_word = None;
        self.drag_anchor_line = None;
        action
    }

    /// Word-anchored drag extension: keep the originally-double-clicked
    /// word inside the selection while the drag grows by full WORD
    /// units, flipping the visual anchor as the drag crosses back over
    /// the original word.
    fn extend_word_anchored_drag(&mut self, ctx: MouseCtx, buf: &str) {
        let Some((ws, we)) = self.drag_anchor_word else {
            return;
        };
        let p = self.compute_cpos(ctx.rows);
        let (new_cpos, new_anchor) = if p >= we {
            let far = crate::edit_buffer::word_range_at_transparent(buf, p, ctx.soft_breaks)
                .map(|(_, e)| e.saturating_sub(1).max(ws))
                .unwrap_or(p.max(we.saturating_sub(1)));
            (far, ws)
        } else if p < ws {
            let near = crate::edit_buffer::word_range_at_transparent(buf, p, ctx.soft_breaks)
                .map(|(s, _)| s)
                .unwrap_or(p);
            (near, we.saturating_sub(1).max(ws))
        } else {
            (we.saturating_sub(1).max(ws), ws)
        };
        self.cpos = new_cpos;
        if let Some(vim) = self.vim.as_mut() {
            vim.begin_visual(ViMode::Visual, new_anchor);
        } else {
            self.win_cursor.set_anchor(Some(new_anchor));
        }
    }

    fn extend_line_anchored_drag(&mut self, ctx: MouseCtx, buf: &str) {
        let Some((ls, le)) = self.drag_anchor_line else {
            return;
        };
        let p = self.compute_cpos(ctx.rows);
        let (new_cpos, new_anchor) = if p >= le {
            let far = crate::edit_buffer::line_range_at(buf, p, ctx.hard_breaks)
                .map(|(_, e)| e.saturating_sub(1).max(ls))
                .unwrap_or(p.max(le.saturating_sub(1)));
            (far, ls)
        } else if p < ls {
            let near = crate::edit_buffer::line_range_at(buf, p, ctx.hard_breaks)
                .map(|(s, _)| s)
                .unwrap_or(p);
            (near, le.saturating_sub(1).max(ls))
        } else {
            (le.saturating_sub(1).max(ls), ls)
        };
        self.cpos = new_cpos;
        if let Some(vim) = self.vim.as_mut() {
            vim.begin_visual(ViMode::Visual, new_anchor);
        } else {
            self.win_cursor.set_anchor(Some(new_anchor));
        }
    }

    // ── Key dispatch ───────────────────────────────────────────────────

    /// Esc handler shared by every buffer surface. Clears selection
    /// state without dismissing higher-level UI; returns `true` if the
    /// key was consumed (i.e. there was something to clear). Callers
    /// (Dialog, App) chain this *before* their own Esc semantics:
    /// dialog dismiss only fires when the focused window had nothing
    /// to clear.
    pub fn handle_escape(&mut self) -> bool {
        if let Some(vim) = self.vim.as_mut() {
            if matches!(vim.mode(), ViMode::Visual | ViMode::VisualLine) {
                vim.set_mode(ViMode::Normal);
                self.win_cursor.clear_anchor();
                self.drag_anchor_word = None;
                self.drag_anchor_line = None;
                return true;
            }
        }
        if self.win_cursor.anchor().is_some() {
            self.win_cursor.clear_anchor();
            self.drag_anchor_word = None;
            self.drag_anchor_line = None;
            return true;
        }
        false
    }

    pub fn handle_key(
        &mut self,
        k: KeyEvent,
        rows: &[String],
        viewport_rows: u16,
    ) -> Option<Option<String>> {
        if rows.is_empty() {
            return None;
        }
        let offsets = self.mount(rows);
        if !self.dispatch_vim_key(k) {
            return None;
        }
        if let Some(vim) = self.vim.as_mut() {
            if vim.mode() == ViMode::Insert {
                vim.set_mode(ViMode::Normal);
            }
        }
        let yanked = self.kill_ring.current().to_string();
        let yanked = if yanked.is_empty() {
            None
        } else {
            self.kill_ring.set_with_linewise(String::new(), false);
            Some(yanked)
        };
        self.sync_from_cpos(rows, &offsets, viewport_rows);
        Some(yanked)
    }

    fn dispatch_vim_key(&mut self, key: KeyEvent) -> bool {
        let Some(vim) = self.vim.as_mut() else {
            return false;
        };
        let key = match key.code {
            KeyCode::Up => KeyEvent {
                code: KeyCode::Char('k'),
                ..key
            },
            KeyCode::Down => KeyEvent {
                code: KeyCode::Char('j'),
                ..key
            },
            KeyCode::Left => KeyEvent {
                code: KeyCode::Char('h'),
                ..key
            },
            KeyCode::Right => KeyEvent {
                code: KeyCode::Char('l'),
                ..key
            },
            _ => key,
        };
        vim.set_curswant(self.win_cursor.curswant());
        let mut cpos = self.cpos;
        // The transcript pushes yanked text to the clipboard at the
        // caller (`content_keys::handle_content_vim_key`) because it
        // needs `copy_display_range` to convert rendered text back to
        // raw markdown. Using `NullClipboard` here avoids a redundant
        // raw-text push from `yank_range`.
        let mut clipboard = crate::clipboard::NullClipboard;
        let mut ctx = VimContext {
            buf: &mut self.edit_buf.buf,
            cpos: &mut cpos,
            attachments: &mut self.edit_buf.attachment_ids,
            kill_ring: &mut self.kill_ring,
            history: &mut self.edit_buf.history,
            clipboard: &mut clipboard,
        };
        let action = vim.handle_key(key, &mut ctx);
        self.cpos = cpos;
        self.win_cursor.set_curswant(vim.curswant());
        !matches!(action, Action::Passthrough)
    }

    /// Shift `scroll_top` by `delta` rows, clamped to
    /// `[0, total_lines - viewport_rows]`. Intentionally does **not**
    /// touch `cpos`, `cursor_line`, or `win_cursor` — wheel / scrollbar
    /// scrolling moves the viewport only, letting the cursor scroll out
    /// of view until the next keyboard motion or click re-anchors it.
    /// Matches tmux copy-mode semantics: "wheel pans the buffer,
    /// keyboard moves the cursor."
    pub fn scroll_view_by(&mut self, delta: isize, total_lines: usize, viewport_rows: u16) {
        if delta == 0 {
            return;
        }
        let max_scroll = total_lines.saturating_sub(viewport_rows as usize) as isize;
        let new = ((self.scroll_top as isize) + delta).clamp(0, max_scroll);
        self.scroll_top = new as u16;
        self.follow_tail = new >= max_scroll;
    }

    /// Adjust `scroll_top` so the cursor's visual row (`cursor_line`)
    /// sits inside the viewport. Top edge → scroll up to align cursor;
    /// bottom edge → scroll down by one row past the cursor. No-op if
    /// already visible. Called by every cpos-mutating site (key insert,
    /// vim motion, paste, click-to-position) — the universal "keep the
    /// cursor visible" policy shared by transcript, prompt, and dialog
    /// buffer panels.
    pub fn ensure_cursor_visible(&mut self, cursor_line: usize, viewport_rows: u16) {
        let viewport = viewport_rows as usize;
        if viewport == 0 {
            return;
        }
        let scroll_top = self.scroll_top as usize;
        if cursor_line < scroll_top {
            self.scroll_top = cursor_line as u16;
        } else if cursor_line >= scroll_top + viewport {
            self.scroll_top = (cursor_line + 1 - viewport) as u16;
        }
    }

    /// If `follow_tail` is set, snap `scroll_top` to the last viewport
    /// of `total_rows`. Generalizes the transcript's tail-follow auto-
    /// snap so any append-only buffer surface can opt in via
    /// `follow_tail = true`.
    pub fn snap_to_bottom_if_following(&mut self, total_rows: usize, viewport_rows: u16) {
        if !self.follow_tail {
            return;
        }
        let viewport = viewport_rows as usize;
        self.scroll_top = total_rows.saturating_sub(viewport) as u16;
    }

    pub fn scroll_by_lines(&mut self, delta: isize, rows: &[String], viewport_rows: u16) {
        if rows.is_empty() || delta == 0 {
            return;
        }
        let offsets = self.mount(rows);
        let new_cpos = self
            .win_cursor
            .move_vertical(&self.edit_buf.buf, self.cpos, delta);
        self.cpos = new_cpos;
        if let Some(vim) = self.vim.as_mut() {
            if vim.mode() == ViMode::Insert {
                vim.set_mode(ViMode::Normal);
            }
        }
        self.sync_from_cpos(rows, &offsets, viewport_rows);
        let max_scroll = (rows.len() as u16).saturating_sub(viewport_rows);
        self.follow_tail = self.scroll_top >= max_scroll;
    }

    pub fn jump_to_line_col(
        &mut self,
        rows: &[String],
        line_idx: usize,
        col: usize,
        viewport_rows: u16,
    ) {
        if rows.is_empty() {
            return;
        }
        let line_idx = line_idx.min(rows.len() - 1);
        let offsets = Self::line_start_offsets(rows);
        // Intentionally do NOT write `self.edit_buf.buf` here. Mouse
        // helpers are pure over `(rows, &str buf)` so the prompt — whose
        // `edit_buf.buf` is the source buffer, not the wrapped display
        // rows — can run through `Window::handle_mouse` without losing
        // its source content.
        let line = &rows[line_idx];
        let col_bytes = cell_to_byte(line, col);
        self.cpos = offsets[line_idx] + col_bytes;
        let landed_col = byte_to_cell(line, col_bytes);
        self.win_cursor.set_curswant(Some(landed_col));
        self.sync_from_cpos(rows, &offsets, viewport_rows);
        let max_scroll = (rows.len() as u16).saturating_sub(viewport_rows);
        self.follow_tail = self.scroll_top >= max_scroll;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BufId;

    fn make_win() -> Window {
        Window::new(
            WinId(1),
            BufId(1),
            WinConfig::Split(SplitConfig {
                region: "test".into(),
                gutters: Gutters::default(),
            }),
        )
    }

    fn sample_rows(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("line {i}")).collect()
    }

    #[test]
    fn scroll_view_by_unsticks_from_bottom() {
        let mut w = make_win();
        w.scroll_top = 80;
        w.follow_tail = true;
        w.scroll_view_by(-3, 100, 20);
        assert_eq!(w.scroll_top, 77);
        assert!(!w.follow_tail);
    }

    #[test]
    fn scroll_view_by_restickes_at_bottom() {
        let mut w = make_win();
        w.scroll_top = 70;
        w.follow_tail = false;
        w.scroll_view_by(10, 100, 20);
        assert_eq!(w.scroll_top, 80);
        assert!(w.follow_tail);
    }

    #[test]
    fn scroll_to_bottom_sets_follow_tail() {
        let mut w = make_win();
        w.follow_tail = false;
        w.scroll_top = 10;
        w.scroll_to_bottom();
        assert_eq!(w.scroll_top, u16::MAX);
        assert!(w.follow_tail);
    }

    #[test]
    fn scroll_by_lines_moves_cursor_down() {
        let mut w = make_win();
        w.set_vim_enabled(true);
        let rows = sample_rows(30);
        let viewport = 10;
        w.jump_to_line_col(&rows, 0, 0, viewport);
        assert_eq!(w.cursor_line, 0);
        assert_eq!(w.scroll_top, 0);
        w.scroll_by_lines(1, &rows, viewport);
        assert_eq!(w.cursor_line, 1);
        assert_eq!(w.scroll_top, 0);
    }

    #[test]
    fn refocus_on_empty_resets_cursor() {
        let mut w = make_win();
        w.cursor_line = 5;
        w.cursor_col = 3;
        w.refocus(&[], 20);
        assert_eq!(w.cursor_line, 0);
        assert_eq!(w.cursor_col, 0);
    }

    #[test]
    fn jump_to_last_line_scrolls_to_bottom() {
        let mut w = make_win();
        let rows = sample_rows(50);
        let viewport = 10;
        w.jump_to_line_col(&rows, 49, 0, viewport);
        assert_eq!(w.scroll_top, 40);
        assert_eq!(w.cursor_line, 9);
    }

    #[test]
    fn cursor_abs_row_top_relative() {
        let mut w = make_win();
        w.scroll_top = 10;
        w.cursor_line = 5;
        assert_eq!(w.cursor_abs_row(), 15);
    }

    fn click_event(kind: MouseEventKind, row: u16, col: u16) -> MouseEvent {
        use crossterm::event::KeyModifiers;
        MouseEvent {
            kind,
            row,
            column: col,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn viewport_for(rows: &[String], rect: Rect) -> WindowViewport {
        WindowViewport::new(rect, rect.width, rows.len() as u16, 0, None)
    }

    fn hard_breaks(rows: &[String]) -> Vec<usize> {
        let mut out = Vec::new();
        let mut acc = 0usize;
        for (i, r) in rows.iter().enumerate() {
            if i + 1 < rows.len() {
                acc += r.len();
                out.push(acc);
                acc += 1;
            }
        }
        out
    }

    #[test]
    fn click_positions_cursor_and_captures() {
        let mut w = make_win();
        let rows: Vec<String> = vec!["hello world".into(), "second line".into()];
        let rect = Rect::new(0, 0, 20, 5);
        let ctx = MouseCtx {
            rows: &rows,
            soft_breaks: &[],
            hard_breaks: &hard_breaks(&rows),
            viewport: viewport_for(&rows, rect),
            click_count: 1,
        };
        let r = w.handle_mouse(
            click_event(MouseEventKind::Down(MouseButton::Left), 1, 7),
            ctx,
        );
        assert_eq!(r, MouseAction::Capture);
        assert_eq!(w.cursor_line, 1);
        assert_eq!(w.cursor_col, 7);
        assert!(w.win_cursor.anchor().is_some());
    }

    #[test]
    fn double_click_yanks_word() {
        let mut w = make_win();
        let rows: Vec<String> = vec!["hello world".into()];
        let rect = Rect::new(0, 0, 20, 5);
        let ctx = MouseCtx {
            rows: &rows,
            soft_breaks: &[],
            hard_breaks: &hard_breaks(&rows),
            viewport: viewport_for(&rows, rect),
            click_count: 2,
        };
        let r = w.handle_mouse(
            click_event(MouseEventKind::Down(MouseButton::Left), 0, 8),
            ctx,
        );
        match r {
            MouseAction::Yank(t) => assert_eq!(t, "world"),
            other => panic!("expected Yank, got {other:?}"),
        }
        assert!(w.drag_anchor_word.is_some());
    }

    #[test]
    fn triple_click_yanks_line() {
        let mut w = make_win();
        let rows: Vec<String> = vec!["alpha".into(), "beta gamma".into()];
        let rect = Rect::new(0, 0, 20, 5);
        let ctx = MouseCtx {
            rows: &rows,
            soft_breaks: &[],
            hard_breaks: &hard_breaks(&rows),
            viewport: viewport_for(&rows, rect),
            click_count: 3,
        };
        let r = w.handle_mouse(
            click_event(MouseEventKind::Down(MouseButton::Left), 1, 0),
            ctx,
        );
        match r {
            MouseAction::Yank(t) => assert_eq!(t, "beta gamma"),
            other => panic!("expected Yank, got {other:?}"),
        }
        assert!(w.drag_anchor_line.is_some());
    }

    #[test]
    fn drag_extends_then_release_yanks() {
        let mut w = make_win();
        let rows: Vec<String> = vec!["hello world".into()];
        let rect = Rect::new(0, 0, 20, 5);
        let breaks = hard_breaks(&rows);
        let viewport = viewport_for(&rows, rect);
        let mk_ctx = || MouseCtx {
            rows: &rows,
            soft_breaks: &[],
            hard_breaks: &breaks,
            viewport,
            click_count: 1,
        };
        // Click at col 0, drag to col 5, release.
        w.handle_mouse(
            click_event(MouseEventKind::Down(MouseButton::Left), 0, 0),
            mk_ctx(),
        );
        w.handle_mouse(
            click_event(MouseEventKind::Drag(MouseButton::Left), 0, 5),
            mk_ctx(),
        );
        let r = w.handle_mouse(
            click_event(MouseEventKind::Up(MouseButton::Left), 0, 5),
            mk_ctx(),
        );
        match r {
            MouseAction::Yank(t) => assert_eq!(t, "hello"),
            other => panic!("expected Yank, got {other:?}"),
        }
        assert!(w.drag_anchor_word.is_none());
        assert!(w.drag_anchor_line.is_none());
    }

    #[test]
    fn follow_tail_default_true() {
        let w = make_win();
        assert!(w.follow_tail);
    }
}
