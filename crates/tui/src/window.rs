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

/// Side of a window a gutter column sits on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GutterSide {
    Left,
    Right,
}

/// Per-window gutter reservations. Content-rect width is
/// `window.rect.width - pad_left - pad_right`. Cursor columns live in
/// content-rect coords — clicks in gutters route to the gutter widget
/// (scrollbar) or snap into the content rect.
///
/// Extension points: `numbercol_width`, `signcol_width`, `foldcol_width`
/// slot into the same computation without call-site changes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WindowGutters {
    pub pad_left: u16,
    pub pad_right: u16,
    /// `None` = no scrollbar column reserved; `Some(side)` = one column
    /// inside the matching pad is dedicated to the scrollbar track.
    pub scrollbar: Option<GutterSide>,
}

impl WindowGutters {
    /// Total horizontal reservation in cells.
    pub fn total(&self) -> u16 {
        self.pad_left + self.pad_right
    }

    /// Content width after subtracting gutter reservations. Saturating
    /// so a narrow terminal never underflows to `u16::MAX`.
    pub fn content_width(&self, window_width: u16) -> u16 {
        window_width.saturating_sub(self.total())
    }
}

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
    /// Top-of-viewport offset in rows (top-relative: 0 = first line
    /// visible, `max_scroll` = stuck to bottom).
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
    /// Top-relative scroll: index of the first visible content line.
    /// `0` = top of transcript visible. `max_scroll` = stuck to bottom.
    pub scroll_top: u16,
    /// Cursor row relative to the viewport top (0 = top-most visible
    /// row).
    pub cursor_line: u16,
    /// Cursor column (visual char index within the line).
    pub cursor_col: u16,
    /// When `Some`, the viewport is pinned: `scroll_top` is held
    /// constant while new content grows below. Each frame, the delta
    /// in total rows is tracked so we know when content shrinks too.
    pub pinned_last_total: Option<u16>,
    /// Selection anchor in (row, col) space. Survives transcript
    /// mutations during streaming because it's in row/col coordinates
    /// rather than byte offsets.
    pub selection_anchor: Option<(usize, usize)>,
    /// True once the user has explicitly positioned the cursor in the
    /// content pane (any key/mouse movement). When false, `refocus`
    /// places the cursor at the bottom of the viewport since the
    /// transcript always shows the tail.
    cursor_positioned: bool,
}

impl TranscriptWindow {
    pub fn new() -> Self {
        Self {
            buffer: Buffer::readonly(),
            cpos: 0,
            vim: None,
            cursor: crate::cursor::WindowCursor::new(),
            kill_ring: crate::input::KillRing::new(),
            scroll_top: 0,
            cursor_line: 0,
            cursor_col: 0,
            pinned_last_total: None,
            selection_anchor: None,
            cursor_positioned: false,
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
            self.selection_anchor = None;
        }
    }

    pub fn vim_enabled(&self) -> bool {
        self.vim.is_some()
    }

    /// Absolute row index (from top) of the cursor in the transcript.
    pub fn cursor_abs_row(&self, _total_rows: usize) -> usize {
        self.scroll_top as usize + self.cursor_line as usize
    }

    /// Current selection range (vim visual takes priority over
    /// shift-selection anchor). Returns byte offsets in the nav buffer.
    pub fn selection_range(&self, rows: &[String]) -> Option<(usize, usize)> {
        let cpos = self.compute_cpos(rows);
        if let Some(ref vim) = self.vim {
            if let Some(range) = vim.visual_range(&rows.join("\n"), cpos) {
                return Some(range);
            }
        }
        let (ar, ac) = self.selection_anchor?;
        let offsets = Self::line_start_offsets(rows);
        let anchor_row = ar.min(rows.len().saturating_sub(1));
        let anchor_byte = offsets.get(anchor_row).copied().unwrap_or(0)
            + cell_to_byte(rows.get(anchor_row).map(|s| s.as_str()).unwrap_or(""), ac);
        let (lo, hi) = if anchor_byte <= cpos {
            (anchor_byte, cpos)
        } else {
            (cpos, anchor_byte)
        };
        (lo != hi).then_some((lo, hi))
    }

    /// Select the word at `cpos` and enter vim Visual anchored at its
    /// start. Used by double-click.
    pub fn select_word_at(&mut self, rows: &[String], cpos: usize) -> Option<(usize, usize)> {
        let (start, end) = self.buffer.word_range_at(cpos)?;
        if let Some(vim) = self.vim.as_mut() {
            self.cpos = end.saturating_sub(1).max(start);
            vim.begin_visual(crate::vim::ViMode::Visual, start);
        } else {
            self.cpos = end;
            let offsets = Self::line_start_offsets(rows);
            let anchor_line = match offsets.binary_search(&start) {
                Ok(i) => i,
                Err(i) => i.saturating_sub(1),
            };
            let byte_col = start.saturating_sub(offsets[anchor_line]);
            let col = byte_to_cell(&rows[anchor_line], byte_col);
            self.selection_anchor = Some((anchor_line, col));
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
            self.cursor_positioned = false;
            return;
        }
        if let Some(vim) = self.vim.as_mut() {
            if vim.mode() != crate::vim::ViMode::Normal {
                vim.set_mode(crate::vim::ViMode::Normal);
            }
        }
        if !self.cursor_positioned {
            let total = rows.len();
            let last_line = total.saturating_sub(1);
            let offsets = Self::line_start_offsets(rows);
            self.buffer.buf = rows.join("\n");
            self.cpos = offsets[last_line];
            self.sync_from_cpos(rows, &offsets, viewport_rows);
            self.cursor_positioned = true;
        } else {
            let offsets = self.mount(rows);
            self.sync_from_cpos(rows, &offsets, viewport_rows);
        }
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
        let geom = crate::render::ViewportGeom::new(total, viewport_rows, self.scroll_top);
        self.scroll_top = geom.clamped_scroll();
        let cursor_line = self.cursor_line.min(viewport_rows.saturating_sub(1));
        let target_line = (self.scroll_top + cursor_line) as usize;
        let target_line = target_line.min(rows.len() - 1);
        let line = &rows[target_line];
        let want = self.cursor.curswant().unwrap_or(self.cursor_col as usize);
        let col_bytes = cell_to_byte(line, want);
        self.cpos = offsets[target_line] + col_bytes;
        self.cursor_col = byte_to_cell(line, col_bytes) as u16;
        self.cursor_line = cursor_line;
    }

    /// Enter pin mode. `scroll_top` is held constant while new
    /// content arrives below.
    pub fn pin(&mut self, total_rows: u16, _viewport_rows: u16) {
        self.pinned_last_total = Some(total_rows);
    }

    /// Release pin mode.
    pub fn unpin(&mut self) {
        self.pinned_last_total = None;
    }

    /// Apply the pin: with top-relative scroll, pinning means holding
    /// `scroll_top` constant (the same content stays visible as new
    /// rows grow below). When content *shrinks*, clamp `scroll_top`
    /// to the new max so we don't point past the end.
    pub fn apply_pin(&mut self, total_rows: u16, viewport_rows: u16) {
        if self.pinned_last_total.is_none() {
            return;
        }
        let max = total_rows.saturating_sub(viewport_rows);
        self.scroll_top = self.scroll_top.min(max);
        self.pinned_last_total = Some(total_rows);
    }

    /// Is the pin currently active?
    pub fn is_pinned(&self) -> bool {
        self.pinned_last_total.is_some()
    }

    /// Compute the byte offset into the joined nav buffer from the
    /// current `(scroll_top + cursor_line, cursor_col)` position.
    /// Use this instead of reading `cpos` directly when the buffer
    /// may have changed since the last `mount()`.
    pub fn compute_cpos(&self, rows: &[String]) -> usize {
        let offsets = Self::line_start_offsets(rows);
        self.visible_cpos(rows, &offsets)
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
    /// currently shown as the cursor (uses `scroll_top` + `cursor_line`
    /// + `cursor_col`).
    fn visible_cpos(&self, rows: &[String], offsets: &[usize]) -> usize {
        let total = rows.len();
        if total == 0 {
            return 0;
        }
        let line_idx = (self.scroll_top as usize + self.cursor_line as usize).min(total - 1);
        offsets[line_idx] + cell_to_byte(&rows[line_idx], self.cursor_col as usize)
    }

    /// Reconcile pane state from the underlying buffer's `cpos`. Given
    /// the transcript rows + viewport height, this repositions
    /// `cursor_line`, `cursor_col`, and `scroll_top` so the cursor
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
        if let Some(ref vim) = self.vim {
            return vim.visual_range(&self.buffer.buf, self.cpos);
        }
        None
    }
    fn clear_selection(&mut self) {
        self.selection_anchor = None;
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
        self.scroll_top
    }
    fn set_scroll_top(&mut self, row: u16) {
        self.scroll_top = row;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tw() -> TranscriptWindow {
        TranscriptWindow::new()
    }

    #[test]
    fn apply_pin_holds_scroll_top_on_growth() {
        let mut tw = make_tw();
        tw.scroll_top = 5;
        tw.pin(100, 20);
        tw.apply_pin(103, 20);
        // scroll_top stays at 5 — new content grows below
        assert_eq!(tw.scroll_top, 5);
        assert_eq!(tw.pinned_last_total, Some(103));
    }

    #[test]
    fn apply_pin_clamps_on_shrinkage() {
        let mut tw = make_tw();
        tw.scroll_top = 85; // near max for total=100, viewport=20 → max=80
        tw.pin(100, 20);
        tw.apply_pin(97, 20); // new max = 77
        assert_eq!(tw.scroll_top, 77);
        assert_eq!(tw.pinned_last_total, Some(97));
    }

    #[test]
    fn apply_pin_clamps_to_zero() {
        let mut tw = make_tw();
        tw.scroll_top = 2;
        tw.pin(100, 20);
        tw.apply_pin(15, 20); // new max = 0 (total < viewport)
        assert_eq!(tw.scroll_top, 0);
    }

    #[test]
    fn apply_pin_noop_when_unpinned() {
        let mut tw = make_tw();
        tw.scroll_top = 5;
        tw.apply_pin(200, 20);
        assert_eq!(tw.scroll_top, 5);
    }

    #[test]
    fn apply_pin_consecutive_growth_and_shrinkage() {
        let mut tw = make_tw();
        tw.scroll_top = 10;
        tw.pin(100, 20);
        tw.apply_pin(105, 20); // growth → scroll_top stays
        assert_eq!(tw.scroll_top, 10);
        tw.apply_pin(102, 20); // shrinkage, max=82 → still fine
        assert_eq!(tw.scroll_top, 10);
        tw.apply_pin(25, 20); // shrinkage, max=5 → clamp
        assert_eq!(tw.scroll_top, 5);
    }

    fn sample_rows(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("line {i}")).collect()
    }

    #[test]
    fn scroll_by_lines_j_moves_cursor_down() {
        let mut tw = make_tw();
        tw.set_vim_enabled(true);
        let rows = sample_rows(30);
        let viewport = 10;
        tw.jump_to_line_col(&rows, 0, 0, viewport);
        assert_eq!(tw.cursor_line, 0);
        assert_eq!(tw.scroll_top, 0);
        tw.scroll_by_lines(1, &rows, viewport);
        assert_eq!(tw.cursor_line, 1);
        assert_eq!(tw.scroll_top, 0);
    }

    #[test]
    fn scroll_by_lines_k_moves_cursor_up() {
        let mut tw = make_tw();
        tw.set_vim_enabled(true);
        let rows = sample_rows(30);
        let viewport = 10;
        tw.scroll_top = 20;
        tw.cursor_line = 5;
        tw.refocus(&rows, viewport);
        let prev_line = tw.cursor_line;
        tw.scroll_by_lines(-1, &rows, viewport);
        assert!(tw.cursor_line < prev_line || tw.scroll_top < 20);
    }

    #[test]
    fn refocus_on_empty_transcript_resets_cursor() {
        let mut tw = make_tw();
        tw.cursor_line = 5;
        tw.cursor_col = 3;
        tw.refocus(&[], 20);
        assert_eq!(tw.cursor_line, 0);
        assert_eq!(tw.cursor_col, 0);
    }

    #[test]
    fn jump_to_last_line_scrolls_to_bottom() {
        let mut tw = make_tw();
        let rows = sample_rows(50);
        let viewport = 10;
        tw.jump_to_line_col(&rows, 49, 0, viewport);
        assert_eq!(tw.scroll_top, 40);
        assert_eq!(tw.cursor_line, 9);
    }

    #[test]
    fn cursor_abs_row_top_relative() {
        let mut tw = make_tw();
        tw.scroll_top = 10;
        tw.cursor_line = 5;
        assert_eq!(tw.cursor_abs_row(100), 15);
    }

    #[test]
    fn unpin_stops_tracking() {
        let mut tw = make_tw();
        tw.pin(100, 20);
        assert!(tw.is_pinned());
        tw.unpin();
        assert!(!tw.is_pinned());
        tw.scroll_top = 5;
        tw.apply_pin(200, 20);
        assert_eq!(tw.scroll_top, 5);
    }
}
