//! Window abstraction — UI containers that own a `Buffer` + viewport
//! state and handle their own focus/mouse/scroll.
//!
//! The TUI currently has two panes:
//!
//! - **Prompt** — writable buffer; lives inside `InputState` which
//!   layers the completer/menu/history around it.
//! - **Content** — readonly buffer that mirrors the rendered
//!   transcript; drives vim motions, visual selection, yank.
//!
//! This module holds `TranscriptWindow`, the state + behaviour formerly
//! spread across `App.content_buffer` + `history_cursor_line/col` +
//! `history_scroll_offset`. The prompt pane is kept as `InputState`
//! (plus its completer/menu side-car) so the rich edit stack there
//! doesn't have to be wrapped in a trait object right now.

use crate::buffer::Buffer;
use crate::text_utils::{byte_to_cell, cell_to_byte};
use crossterm::event::{KeyCode, KeyEvent};

/// Common window interface — the shared shape every pane (prompt,
/// transcript, future floats) exposes. Typed mutations live on
/// `crate::api::{buf, win}`; this trait is the minimum every caller
/// reads from.
///
/// Mirrors nvim's split between **buffer** (text) and **window**
/// (cursor + selection + scroll). `text()` returns the buffer handle;
/// `cursor`, `selection`, `scroll_top` live on the window.
pub trait Window {
    fn text(&self) -> &Buffer;
    fn text_mut(&mut self) -> &mut Buffer;
    fn cursor(&self) -> usize;
    fn set_cursor(&mut self, pos: usize);
    fn selection(&self) -> Option<(usize, usize)>;
    fn clear_selection(&mut self);
    /// Top-of-viewport offset in rows. Interpretation is window-role
    /// dependent (prompt = top-relative; transcript = bottom-relative
    /// until Stage 7 lands the unified top-relative model).
    fn scroll_top(&self) -> u16;
    fn set_scroll_top(&mut self, row: u16);
}

/// Readonly pane showing the scrollback / transcript. Owns its
/// window-level state (cursor, vim, selection, kill ring, scroll)
/// and carries a readonly `Buffer` that holds the joined
/// transcript text — the buffer is refreshed from the rendered
/// transcript on every key dispatch (the transcript is rebuilt
/// each frame).
pub struct TranscriptWindow {
    /// Underlying content-only buffer.
    pub buffer: Buffer,
    /// Cursor byte offset into `buffer.buf` — window-level.
    pub cpos: usize,
    /// Per-window vim state (mode, visual_anchor, curswant).
    pub vim: Option<crate::vim::Vim>,
    /// Shared per-window cursor state — selection anchor + curswant.
    /// Vim Visual mode drives the same anchor; every vertical motion
    /// (j/k, shift+arrow, wheel) goes through `cursor.move_vertical`.
    pub cursor: crate::cursor::WindowCursor,
    /// Per-window kill ring — yanking from the transcript copies
    /// here; `handle_key` lifts it to the system clipboard.
    pub kill_ring: crate::input::KillRing,
    /// Rows scrolled away from the bottom of the transcript. 0 = the
    /// most recent row is visible at the bottom of the viewport.
    pub scroll_offset: u16,
    /// Cursor row relative to the viewport bottom (0 = bottom-most
    /// row, `viewport_rows - 1` = top row).
    pub cursor_line: u16,
    /// Cursor column (visual char index within the line).
    pub cursor_col: u16,
    /// When `Some`, the viewport is in "pinned" mode: new content
    /// arriving at the bottom pushes into scrollback instead of
    /// shifting the visible rows. Pinning is *delta-based*, not
    /// absolute-based — we remember the last-observed transcript
    /// row count and each frame add its growth to `scroll_offset`.
    /// This way the user can still scroll freely (wheel, j/k motion
    /// past viewport edge) while pinned; user scroll updates
    /// `scroll_offset` directly, and the pin only reacts to content
    /// growing.
    pub pinned_last_total: Option<u16>,
}

impl TranscriptWindow {
    pub fn new() -> Self {
        Self {
            buffer: Buffer::readonly(),
            cpos: 0,
            vim: None,
            cursor: crate::cursor::WindowCursor::new(),
            kill_ring: crate::input::KillRing::new(),
            scroll_offset: 0,
            cursor_line: 0,
            cursor_col: 0,
            pinned_last_total: None,
        }
    }

    /// Toggle whether this pane routes motions / selection through a
    /// vim state machine. Mirrors `InputState::set_vim_enabled` so the
    /// whole app flips together.
    pub fn set_vim_enabled(&mut self, enabled: bool) {
        if enabled {
            if self.vim.is_none() {
                self.vim = Some(crate::vim::Vim::new());
            }
        } else {
            self.vim = None;
            self.cursor.clear_anchor();
        }
    }

    pub fn vim_enabled(&self) -> bool {
        self.vim.is_some()
    }

    /// Current selection range (vim visual takes priority over
    /// shift-selection anchor). Returns byte offsets in `buffer.buf`.
    pub fn selection_range(&self) -> Option<(usize, usize)> {
        if let Some(ref vim) = self.vim {
            if let Some(range) = vim.visual_range(&self.buffer.buf, self.cpos) {
                return Some(range);
            }
        }
        self.cursor.range(self.cpos)
    }

    /// Select the word at `cpos` and enter vim Visual anchored at its
    /// start. Used by double-click.
    pub fn select_word_at(&mut self, cpos: usize) -> Option<(usize, usize)> {
        let (start, end) = self.buffer.word_range_at(cpos)?;
        if let Some(vim) = self.vim.as_mut() {
            // Vim Visual is char-inclusive at the cursor.
            self.cpos = end.saturating_sub(1).max(start);
            vim.begin_visual(crate::vim::ViMode::Visual, start);
        } else {
            // ShiftSelection is half-open [anchor, cpos); place the
            // cursor past the last char so the whole word is in range.
            self.cpos = end;
            self.cursor.set_anchor(Some(start));
        }
        Some((start, end))
    }

    /// Public entry point for callers that set `cpos` directly (non-vim
    /// keymap, click-to-position) and need the scroll/cursor display
    /// state re-derived from it.
    pub fn resync(&mut self, rows: &[String], viewport_rows: u16) {
        if rows.is_empty() {
            return;
        }
        let offsets = Self::line_start_offsets(rows);
        self.buffer.buf = rows.join("\n");
        self.sync_from_cpos(rows, &offsets, viewport_rows);
    }

    /// Ensure the pane state is consistent with the current
    /// transcript. Called when focus switches to the content window:
    /// mounts the buffer, clamps cpos to the visible tail if it's
    /// stale, and syncs cursor_line/col. Safe to call on an empty
    /// transcript (no-op). This is what makes the first key press
    /// after a focus switch Just Work — the window is "warmed up"
    /// with valid coordinates instead of relying on lazy mount from
    /// the first key.
    pub fn refocus(&mut self, rows: &[String], viewport_rows: u16) {
        if rows.is_empty() {
            self.buffer.buf.clear();
            self.cpos = 0;
            self.cursor_line = 0;
            self.cursor_col = 0;
            return;
        }
        let offsets = self.mount(rows);
        self.sync_from_cpos(rows, &offsets, viewport_rows);
        // Seed `curswant` from the current display column so the very
        // first `j`/`k` after focus preserves the column. Without this
        // the first vertical motion collapses to column 0 because
        // `curswant` is `None`.
        if self.cursor.curswant().is_none() {
            self.cursor.set_curswant(Some(self.cursor_col as usize));
        }
    }

    /// Reanchor the cursor after an external scroll change (e.g.
    /// scrollbar drag) that moved the viewport without touching
    /// `cpos`. Keeps the cursor at the same *screen* row and rebuilds
    /// `cpos` + `cursor_col` from whichever transcript line is now at
    /// that row, using `curswant` for the column (so the cursor keeps
    /// its visual column across lines of different lengths — vim
    /// `curswant` semantics).
    pub fn reanchor_to_visible_row(&mut self, rows: &[String], viewport_rows: u16) {
        if rows.is_empty() {
            return;
        }
        let offsets = Self::line_start_offsets(rows);
        self.buffer.buf = rows.join("\n");
        let total = rows.len() as u16;
        let max_scroll = total.saturating_sub(viewport_rows);
        let scroll = self.scroll_offset.min(max_scroll);
        self.scroll_offset = scroll;
        let cursor_line = self.cursor_line.min(viewport_rows.saturating_sub(1));
        let line_from_bottom = scroll.saturating_add(cursor_line);
        let target_line = (total.saturating_sub(1).saturating_sub(line_from_bottom)) as usize;
        let target_line = target_line.min(rows.len() - 1);
        let line = &rows[target_line];
        let want = self.cursor.curswant().unwrap_or(self.cursor_col as usize);
        let col_bytes = cell_to_byte(line, want);
        self.cpos = offsets[target_line] + col_bytes;
        self.cursor_col = byte_to_cell(line, col_bytes) as u16;
        self.cursor_line = cursor_line;
    }

    /// Enter pin mode. Call this when starting a selection / visual
    /// drag. `total_rows` is the transcript row count right now. From
    /// this frame on, `apply_pin` will detect any growth and push it
    /// into `scroll_offset` so the visible rows don't shift. The
    /// user can still scroll freely with wheel / j / k — their scroll
    /// changes `scroll_offset` directly, and pin just adds any new
    /// growth on top of that.
    pub fn pin(&mut self, total_rows: u16, _viewport_rows: u16) {
        self.pinned_last_total = Some(total_rows);
    }

    /// Release pin mode. The next frame resumes normal scroll behavior
    /// (stuck-to-bottom if `scroll_offset == 0`).
    pub fn unpin(&mut self) {
        self.pinned_last_total = None;
    }

    /// Apply the pin to `scroll_offset`: any growth in `total_rows`
    /// since the last frame is added to `scroll_offset` so the user's
    /// view stays stable. User-initiated scroll (wheel / motion) is
    /// preserved — we only add deltas, never snap.
    pub fn apply_pin(&mut self, total_rows: u16, _viewport_rows: u16) {
        let Some(last) = self.pinned_last_total else {
            return;
        };
        let delta = total_rows.saturating_sub(last);
        if delta > 0 {
            self.scroll_offset = self.scroll_offset.saturating_add(delta);
        }
        self.pinned_last_total = Some(total_rows);
    }

    /// Is the pin currently active?
    pub fn is_pinned(&self) -> bool {
        self.pinned_last_total.is_some()
    }

    /// Per-line byte offsets into the joined transcript buffer.
    fn line_start_offsets(rows: &[String]) -> Vec<usize> {
        let mut v = Vec::with_capacity(rows.len());
        let mut acc = 0usize;
        for r in rows {
            v.push(acc);
            acc += r.len() + 1;
        }
        v
    }

    /// Absolute byte offset inside the joined transcript for the cell
    /// currently shown as the cursor (uses `cursor_line` +
    /// `scroll_offset` + `cursor_col`).
    fn visible_cpos(&self, rows: &[String], offsets: &[usize]) -> usize {
        let total = rows.len();
        if total == 0 {
            return 0;
        }
        let from_bottom = self.cursor_line as usize + self.scroll_offset as usize;
        let line_idx = (total - 1).saturating_sub(from_bottom).min(total - 1);
        offsets[line_idx] + cell_to_byte(&rows[line_idx], self.cursor_col as usize)
    }

    /// Reconcile pane state from the underlying buffer's `cpos`. Given
    /// the transcript rows + viewport height, this repositions
    /// `cursor_line`, `cursor_col`, and `scroll_offset` so the cursor
    /// stays onscreen.
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
        // Cursor column must be in terminal cells, not bytes — multibyte
        // glyphs like `⏺` or `─` are 3 bytes but only 1 cell.
        let line = &rows[line_idx];
        let byte_col = self.cpos.saturating_sub(offsets[line_idx]);
        self.cursor_col = byte_to_cell(line, byte_col) as u16;
        let line_from_bottom = ((total - 1).saturating_sub(line_idx)) as u16;
        let top_lfb = self
            .scroll_offset
            .saturating_add(viewport_rows.saturating_sub(1));
        if line_from_bottom > top_lfb {
            self.scroll_offset = line_from_bottom.saturating_sub(viewport_rows.saturating_sub(1));
        } else if line_from_bottom < self.scroll_offset {
            self.scroll_offset = line_from_bottom;
        }
        self.cursor_line = line_from_bottom.saturating_sub(self.scroll_offset);
    }

    /// Sync the underlying buffer's `buf` + `cpos` from the current
    /// view (visible cursor line/col + transcript rows). Returns the
    /// per-line offsets cache so repeated operations within one frame
    /// can reuse them without rejoining the text.
    fn mount(&mut self, rows: &[String]) -> Vec<usize> {
        let offsets = Self::line_start_offsets(rows);
        self.buffer.buf = rows.join("\n");
        self.cpos = self.visible_cpos(rows, &offsets);
        offsets
    }

    /// Dispatch a key through the buffer's vim instance. Returns
    /// `Some(yanked)` when vim consumed the key and there is new
    /// content in the kill ring (caller should copy to the system
    /// clipboard). Returns `None` if the key was passed through.
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
        // Readonly enforcement — a motion like `i` / `a` would flip
        // vim into Insert mode where the next keystroke would edit
        // the mounted transcript. Snap it back to Normal immediately
        // so the transcript stays intact.
        if let Some(vim) = self.vim.as_mut() {
            if vim.mode() == crate::vim::ViMode::Insert {
                vim.set_mode(crate::vim::ViMode::Normal);
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

    /// Build a `VimContext` from window-owned cursor / kill ring +
    /// buffer-owned text / attachments / undo, and dispatch the key
    /// through the window's `Vim` instance. Returns `true` if vim
    /// consumed the key.
    fn dispatch_vim_key(&mut self, key: KeyEvent) -> bool {
        let Some(vim) = self.vim.as_mut() else {
            return false;
        };
        // Map arrow keys to j/k/h/l so vertical motion on the readonly
        // transcript always takes the curswant-preserving path.
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
        // Seed vim's curswant from the window cursor so a prior
        // shift+arrow vertical motion's preferred column is preserved,
        // and write it back afterwards — single source of truth lives
        // on `self.cursor`.
        vim.set_curswant(self.cursor.curswant());
        let mut cpos = self.cpos;
        let mut ctx = crate::vim::VimContext {
            buf: &mut self.buffer.buf,
            cpos: &mut cpos,
            attachments: &mut self.buffer.attachment_ids,
            kill_ring: &mut self.kill_ring,
            history: &mut self.buffer.history,
        };
        let action = vim.handle_key(key, &mut ctx);
        self.cpos = cpos;
        self.cursor.set_curswant(vim.curswant());
        !matches!(action, crate::vim::Action::Passthrough)
    }

    /// Move the content cursor by `delta` lines (positive = down). This
    /// is the shared code path for vim `j`/`k`, arrow keys, mouse wheel,
    /// and half/full-page scrolls — each computes a line delta and calls
    /// here so `curswant` (preferred column) and viewport-follows-cursor
    /// behave identically regardless of input source.
    pub fn scroll_by_lines(&mut self, delta: isize, rows: &[String], viewport_rows: u16) {
        if rows.is_empty() || delta == 0 {
            return;
        }
        let offsets = self.mount(rows);
        // One path for vertical motion regardless of vim state: ask the
        // shared `WindowCursor` to step the cpos by `delta` lines,
        // updating its `curswant` internally. Vim's Visual mode — if
        // active — gets the same column preservation because the next
        // dispatch syncs `cursor.curswant` → `vim.curswant` via
        // `dispatch_vim_key`.
        let new_cpos = self
            .cursor
            .move_vertical(&self.buffer.buf, self.cpos, delta);
        self.cpos = new_cpos;
        // A readonly transcript should never land in Insert.
        if let Some(vim) = self.vim.as_mut() {
            if vim.mode() == crate::vim::ViMode::Insert {
                vim.set_mode(crate::vim::ViMode::Normal);
            }
        }
        self.sync_from_cpos(rows, &offsets, viewport_rows);
    }

    /// Jump the cursor to the transcript `(line_idx, col)` position and
    /// pull the viewport to keep it onscreen. Seeds `curswant` from the
    /// clicked column so subsequent vertical motion (mouse wheel, j/k)
    /// preserves the clicked visual column.
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
        self.buffer.buf = rows.join("\n");
        let line = &rows[line_idx];
        let col_bytes = cell_to_byte(line, col);
        self.cpos = offsets[line_idx] + col_bytes;
        let landed_col = byte_to_cell(line, col_bytes);
        self.cursor.set_curswant(Some(landed_col));
        self.sync_from_cpos(rows, &offsets, viewport_rows);
    }
}

impl Default for TranscriptWindow {
    fn default() -> Self {
        Self::new()
    }
}

impl Window for TranscriptWindow {
    fn text(&self) -> &Buffer {
        &self.buffer
    }
    fn text_mut(&mut self) -> &mut Buffer {
        &mut self.buffer
    }
    fn cursor(&self) -> usize {
        self.cpos
    }
    fn set_cursor(&mut self, pos: usize) {
        self.cpos = pos.min(self.buffer.buf.len());
    }
    fn selection(&self) -> Option<(usize, usize)> {
        TranscriptWindow::selection_range(self)
    }
    fn clear_selection(&mut self) {
        self.cursor.clear_anchor();
        if let Some(vim) = self.vim.as_mut() {
            if matches!(
                vim.mode(),
                crate::vim::ViMode::Visual | crate::vim::ViMode::VisualLine
            ) {
                vim.set_mode(crate::vim::ViMode::Normal);
            }
        }
    }
    fn scroll_top(&self) -> u16 {
        self.scroll_offset
    }
    fn set_scroll_top(&mut self, row: u16) {
        self.scroll_offset = row;
    }
}

/// Identifier for which pane currently holds focus. `AppFocus` in
/// `app/mod.rs` mirrors these values — both will be unified once the
/// prompt side migrates to a `PromptWindow` wrapper.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WinId {
    Prompt,
    Content,
}
