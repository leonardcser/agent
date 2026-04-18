//! Pane abstraction — UI containers that own a `Buffer` + viewport
//! state and handle their own focus/mouse/scroll.
//!
//! The TUI currently has two panes:
//!
//! - **Prompt** — writable buffer; lives inside `InputState` which
//!   layers the completer/menu/history around it.
//! - **Content** — readonly buffer that mirrors the rendered
//!   transcript; drives vim motions, visual selection, yank.
//!
//! This module holds `ContentPane`, the state + behaviour formerly
//! spread across `App.content_buffer` + `history_cursor_line/col` +
//! `history_scroll_offset`. The prompt pane is kept as `InputState`
//! (plus its completer/menu side-car) so the rich edit stack there
//! doesn't have to be wrapped in a trait object right now.

use crate::buffer::TextBuffer;
use crossterm::event::KeyEvent;

/// A declarative change to a pane's text buffer. Going through
/// `Pane::apply` instead of writing `buf` / `cpos` directly ensures
/// side-effects (undo snapshot, completer recompute, selection
/// clear) run uniformly — no matter which caller produced the edit.
#[derive(Debug, Clone)]
pub enum Mutation {
    /// Replace the entire buffer. Cursor lands at the end by default
    /// unless `cursor` is supplied.
    Replace { text: String, cursor: Option<usize> },
    /// Insert `text` at the current cursor; cursor moves to the end
    /// of the inserted span. Any active selection is deleted first.
    InsertText(String),
    /// Move the cursor without touching the text. Does NOT snapshot
    /// undo — cursor motion is never an edit.
    SetCursor(usize),
    /// Clear the buffer (and cursor, attachments, selection).
    Clear,
    /// Append `text` at the end of the buffer; cursor jumps to the
    /// end. Used for multi-message unqueue paths.
    Append(String),
}

/// Common pane interface. Writing to the underlying buffer goes
/// through `apply()` so undo + completer invariants are uniform; reads
/// go through `text()`. The richer prompt-specific behaviours
/// (completer, menu, stash, ghost text) stay on `InputState`.
pub trait Pane {
    fn text(&self) -> &TextBuffer;
    fn text_mut(&mut self) -> &mut TextBuffer;
    fn selection(&self) -> Option<(usize, usize)>;
    fn apply(&mut self, mutation: Mutation);
}

/// Readonly pane showing the scrollback / transcript. Owns its
/// window-level state (cursor, vim, selection, kill ring, scroll)
/// and carries a readonly `TextBuffer` that holds the joined
/// transcript text — the buffer is refreshed from the rendered
/// transcript on every key dispatch (the transcript is rebuilt
/// each frame).
pub struct ContentPane {
    /// Underlying content-only buffer.
    pub buffer: TextBuffer,
    /// Cursor byte offset into `buffer.buf` — window-level.
    pub cpos: usize,
    /// Per-window vim state (mode, visual_anchor, curswant).
    pub vim: Option<crate::vim::Vim>,
    /// Non-vim selection anchor (unused on content pane right now;
    /// vim visual mode is the selection mechanism).
    pub selection_anchor: Option<usize>,
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

impl ContentPane {
    pub fn new() -> Self {
        Self {
            buffer: TextBuffer::readonly(),
            cpos: 0,
            vim: Some(crate::vim::Vim::new()),
            selection_anchor: None,
            kill_ring: crate::input::KillRing::new(),
            scroll_offset: 0,
            cursor_line: 0,
            cursor_col: 0,
            pinned_last_total: None,
        }
    }

    /// Current selection range (vim visual takes priority over
    /// shift-selection anchor). Returns byte offsets in `buffer.buf`.
    pub fn selection_range(&self) -> Option<(usize, usize)> {
        if let Some(ref vim) = self.vim {
            if let Some(range) = vim.visual_range(&self.buffer.buf, self.cpos) {
                return Some(range);
            }
        }
        let anchor = self.selection_anchor?;
        let (a, b) = if anchor <= self.cpos {
            (anchor, self.cpos)
        } else {
            (self.cpos, anchor)
        };
        (a != b).then_some((a, b))
    }

    /// Select the word at `cpos` and enter vim Visual anchored at its
    /// start. Used by double-click.
    pub fn select_word_at(&mut self, cpos: usize) -> Option<(usize, usize)> {
        let (start, end) = self.buffer.word_range_at(cpos)?;
        self.cpos = end.saturating_sub(1).max(start);
        if let Some(vim) = self.vim.as_mut() {
            vim.begin_visual(crate::vim::ViMode::Visual, start);
        } else {
            self.selection_anchor = Some(start);
        }
        Some((start, end))
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
        let line = &rows[line_idx];
        let col_byte = line
            .char_indices()
            .nth(self.cursor_col as usize)
            .map(|(b, _)| b)
            .unwrap_or(line.len());
        offsets[line_idx] + col_byte
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
        let col = self.cpos - offsets[line_idx];
        let line_from_bottom = ((total - 1).saturating_sub(line_idx)) as u16;
        self.cursor_col = col as u16;
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
        !matches!(action, crate::vim::Action::Passthrough)
    }

    /// Move the content cursor by `delta` lines (positive = down).
    /// Direct scroll — avoids synthesizing vim `j`/`k`, which can
    /// return Passthrough in some states and prevent motion. We
    /// compute the new line directly from the current `cpos` line +
    /// delta, clamp, and reposition.
    pub fn scroll_by_lines(&mut self, delta: isize, rows: &[String], viewport_rows: u16) {
        if rows.is_empty() {
            return;
        }
        let offsets = Self::line_start_offsets(rows);
        self.buffer.buf = rows.join("\n");

        // Find current line via cpos (if valid) or derive from viewport state.
        let total = rows.len();
        let current_line = if self.cpos > 0 && self.cpos <= self.buffer.buf.len() {
            match offsets.binary_search(&self.cpos) {
                Ok(i) => i,
                Err(i) => i.saturating_sub(1),
            }
        } else {
            // Stale or zero cpos — use viewport state to locate.
            let from_bottom = self.cursor_line as usize + self.scroll_offset as usize;
            (total - 1).saturating_sub(from_bottom).min(total - 1)
        };

        let target_line = if delta >= 0 {
            (current_line + delta as usize).min(total - 1)
        } else {
            current_line.saturating_sub((-delta) as usize)
        };

        // Keep horizontal column stable (like vim's curswant).
        let col = if let Some(line) = rows.get(current_line) {
            let mut off = self.cpos.saturating_sub(offsets[current_line]).min(line.len());
            while off > 0 && !line.is_char_boundary(off) {
                off -= 1;
            }
            line[..off].chars().count()
        } else {
            0
        };
        let target_row = &rows[target_line];
        let clamped_col = col.min(target_row.chars().count());
        let byte_off = target_row
            .char_indices()
            .nth(clamped_col)
            .map(|(b, _)| b)
            .unwrap_or(target_row.len());
        self.cpos = offsets[target_line] + byte_off;
        self.sync_from_cpos(rows, &offsets, viewport_rows);
    }

    /// Jump the cursor to the transcript `(line_idx, col)` position and
    /// pull the viewport to keep it onscreen.
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
        let line = &rows[line_idx];
        let clamped_col = col.min(line.chars().count());
        let byte_off = line
            .char_indices()
            .nth(clamped_col)
            .map(|(b, _)| b)
            .unwrap_or(line.len());
        self.cpos = offsets[line_idx] + byte_off;
        self.sync_from_cpos(rows, &offsets, viewport_rows);
    }
}

impl Default for ContentPane {
    fn default() -> Self {
        Self::new()
    }
}

impl Pane for ContentPane {
    fn text(&self) -> &TextBuffer {
        &self.buffer
    }
    fn text_mut(&mut self) -> &mut TextBuffer {
        &mut self.buffer
    }
    fn selection(&self) -> Option<(usize, usize)> {
        ContentPane::selection_range(self)
    }
    /// The content pane is readonly: edit-producing mutations are
    /// ignored. Cursor motion still applies — it's just viewport
    /// navigation, not an edit.
    fn apply(&mut self, mutation: Mutation) {
        match mutation {
            Mutation::SetCursor(pos) => {
                self.cpos = pos.min(self.buffer.buf.len());
            }
            Mutation::Replace { .. }
            | Mutation::InsertText(_)
            | Mutation::Clear
            | Mutation::Append(_) => {}
        }
    }
}

/// Identifier for which pane currently holds focus. `AppFocus` in
/// `app/mod.rs` mirrors these values — both will be unified once the
/// prompt side migrates to a `PromptPane` wrapper.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneId {
    Prompt,
    Content,
}
