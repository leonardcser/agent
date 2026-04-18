//! Shared non-vim shift-selection state — the anchor side of the
//! classic "hold shift while moving to extend a selection" pattern.
//!
//! Both the prompt and transcript windows keep one of these alongside
//! their cursor. Motion code calls `clear()` on plain movement and
//! `extend(cpos)` on shift-movement, then asks `range(cpos)` for the
//! current byte span. Vim Visual mode is a separate mechanism layered
//! on top — when vim is active the window consults vim first, falling
//! back to this type for editors that never enter vim.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ShiftSelection {
    anchor: Option<usize>,
}

impl ShiftSelection {
    pub const fn new() -> Self {
        Self { anchor: None }
    }

    /// Latch the anchor at `cpos` if none is set. Called before a
    /// shift-movement so the first extension anchors where the cursor
    /// was before the key.
    pub fn extend(&mut self, cpos: usize) {
        if self.anchor.is_none() {
            self.anchor = Some(cpos);
        }
    }

    /// Drop the anchor; subsequent motions start fresh.
    pub fn clear(&mut self) {
        self.anchor = None;
    }

    pub fn set(&mut self, anchor: Option<usize>) {
        self.anchor = anchor;
    }

    pub fn anchor(&self) -> Option<usize> {
        self.anchor
    }

    /// Current selection as a `(start, end)` byte pair relative to the
    /// buffer the caller used for `cpos`. Returns `None` when there is
    /// no anchor or the anchor equals `cpos`.
    pub fn range(&self, cpos: usize) -> Option<(usize, usize)> {
        let a = self.anchor?;
        let (lo, hi) = if a <= cpos { (a, cpos) } else { (cpos, a) };
        (lo != hi).then_some((lo, hi))
    }
}
