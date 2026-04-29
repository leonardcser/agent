//! Per-window cursor state shared by the prompt and transcript windows
//! and by vim's motion code. This is the nvim analogue of
//! `window.selection + window.curswant` — one struct every vertical /
//! horizontal motion and every selection path consults, so there is
//! exactly one source of truth per window.
//!
//! - `anchor` — the shift-selection anchor. Vim Visual's `v`/`V` set
//!   this too (via `set_anchor`), so paint/copy read one range.
//! - `curswant` — preferred display column for vertical motion. Set by
//!   the first vertical motion after a horizontal one; preserved across
//!   subsequent vertical motions so the cursor returns to the wanted
//!   column on longer lines. Measured in terminal cells, so wide glyphs
//!   (`⏺`, CJK) don't throw the column off.
//!
//! Both windows own a `WindowCursor`. Vim borrows it via `VimContext`
//! so its j / k / visual-j / visual-k motions use the same `curswant`
//! as the keymap's shift+arrow path — one code path, one state.

use crate::text;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WindowCursor {
    anchor: Option<usize>,
    curswant: Option<usize>,
}

impl WindowCursor {
    pub const fn new() -> Self {
        Self {
            anchor: None,
            curswant: None,
        }
    }

    // ── Selection anchor ────────────────────────────────────────────────

    /// Latch the anchor at `cpos` if none is set. Called before a
    /// shift-movement so the first extension anchors where the cursor
    /// was before the key.
    pub fn extend(&mut self, cpos: usize) {
        if self.anchor.is_none() {
            self.anchor = Some(cpos);
        }
    }

    pub fn clear_anchor(&mut self) {
        self.anchor = None;
    }

    pub fn set_anchor(&mut self, anchor: Option<usize>) {
        self.anchor = anchor;
    }

    pub fn anchor(&self) -> Option<usize> {
        self.anchor
    }

    /// Current selection as a `(start, end)` byte pair. Returns `None`
    /// when no anchor is set or the anchor equals `cpos`.
    pub fn range(&self, cpos: usize) -> Option<(usize, usize)> {
        let a = self.anchor?;
        let (lo, hi) = if a <= cpos { (a, cpos) } else { (cpos, a) };
        (lo != hi).then_some((lo, hi))
    }

    // ── curswant (preferred vertical-motion column) ─────────────────────

    pub fn curswant(&self) -> Option<usize> {
        self.curswant
    }

    pub fn set_curswant(&mut self, c: Option<usize>) {
        self.curswant = c;
    }

    pub fn clear_curswant(&mut self) {
        self.curswant = None;
    }

    /// Single vertical-motion entry point. Every caller (vim j/k, vim
    /// visual j/k, keymap up/down, shift+arrow, mouse wheel lines)
    /// routes through here so the preferred column survives short
    /// lines identically regardless of input source. Returns the new
    /// cpos; internally updates `curswant`.
    pub fn move_vertical(&mut self, buf: &str, cpos: usize, delta: isize) -> usize {
        let (new_cpos, new_want) = text::vertical_move(buf, cpos, delta, self.curswant);
        self.curswant = Some(new_want);
        new_cpos
    }
}
