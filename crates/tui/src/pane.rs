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

use crate::buffer::Buffer;
use crossterm::event::{KeyCode, KeyEvent};

/// Readonly pane showing the scrollback / transcript. Owns its
/// `Buffer` (vim instance + kill ring + undo + text cache) and the
/// viewport scroll / cursor row+col.
pub struct ContentPane {
    /// Underlying readonly buffer — vim motions run against its
    /// `buf`, which the caller refreshes from the rendered transcript
    /// before every key dispatch (the transcript is rebuilt each frame).
    pub buffer: Buffer,
    /// Rows scrolled away from the bottom of the transcript. 0 = the
    /// most recent row is visible at the bottom of the viewport.
    pub scroll_offset: u16,
    /// Cursor row relative to the viewport bottom (0 = bottom-most
    /// row, `viewport_rows - 1` = top row).
    pub cursor_line: u16,
    /// Cursor column (visual char index within the line).
    pub cursor_col: u16,
}

impl ContentPane {
    pub fn new() -> Self {
        Self {
            buffer: Buffer::readonly(),
            scroll_offset: 0,
            cursor_line: 0,
            cursor_col: 0,
        }
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
        self.buffer.cpos = self.buffer.cpos.min(tail_byte);
        let line_idx = match offsets.binary_search(&self.buffer.cpos) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        };
        let col = self.buffer.cpos - offsets[line_idx];
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
        let offsets = Self::line_start_offsets(rows);
        self.buffer.buf = rows.join("\n");
        self.buffer.cpos = self.visible_cpos(rows, &offsets);
        if !self.buffer.handle_vim_key(k) {
            return None;
        }
        let yanked = self.buffer.kill_ring.current().to_string();
        let yanked = if yanked.is_empty() {
            None
        } else {
            self.buffer
                .kill_ring
                .set_with_linewise(String::new(), false);
            Some(yanked)
        };
        self.sync_from_cpos(rows, &offsets, viewport_rows);
        Some(yanked)
    }

    /// Move the content cursor by `delta` lines (positive = down). Uses
    /// vim `j` / `k` via `handle_key` so vertical motion shares the
    /// same path — including `curswant` — as real keypresses.
    pub fn scroll_by_lines(&mut self, delta: isize, rows: &[String], viewport_rows: u16) {
        let (code, count) = if delta >= 0 {
            (KeyCode::Char('j'), delta as usize)
        } else {
            (KeyCode::Char('k'), (-delta) as usize)
        };
        for _ in 0..count {
            if self
                .handle_key(synth_key(code), rows, viewport_rows)
                .is_none()
            {
                break;
            }
        }
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
        self.buffer.cpos = offsets[line_idx] + byte_off;
        self.sync_from_cpos(rows, &offsets, viewport_rows);
    }
}

fn synth_key(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: crossterm::event::KeyModifiers::NONE,
        kind: crossterm::event::KeyEventKind::Press,
        state: crossterm::event::KeyEventState::empty(),
    }
}

impl Default for ContentPane {
    fn default() -> Self {
        Self::new()
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
