//! `Buffer` — pure content. Holds the text, the undo stack, and
//! attachment markers. Nothing else.
//!
//! Cursor position, selection, vim state, and the kill ring live on
//! the **window** that's displaying the buffer (nvim model). A buffer
//! shown in two windows has two independent cursors; the buffer
//! itself has none. Display coordinates (which screen row/col the
//! cursor paints at) are *derived* on render from
//! `(window.cursor, window.scroll, snapshot)` — never stored.
//!
//! Readonly is a property of the buffer (e.g. the transcript buffer
//! is readonly); the owning window checks it before applying edits.

use crate::AttachmentId;
use crate::undo::UndoHistory;

/// Pure-content buffer. The owning window provides cursor / vim /
/// selection state when operating on it.
pub struct Buffer {
    /// Raw UTF-8 text content.
    pub buf: String,
    /// Attachment markers inside `buf`.
    pub attachment_ids: Vec<AttachmentId>,
    /// Undo/redo stack. Readonly buffers pass `None` capacity to
    /// disable.
    pub history: UndoHistory,
    /// Whether this buffer can be edited. Windows check this before
    /// running any edit-producing operation.
    pub readonly: bool,
}

impl Buffer {
    /// A new empty writable buffer with a default-sized undo stack.
    pub fn new() -> Self {
        Self {
            buf: String::new(),
            attachment_ids: Vec::new(),
            history: UndoHistory::new(Some(100)),
            readonly: false,
        }
    }

    /// A new empty readonly buffer (undo disabled).
    pub fn readonly() -> Self {
        Self {
            buf: String::new(),
            attachment_ids: Vec::new(),
            history: UndoHistory::new(None),
            readonly: true,
        }
    }

    /// Find word boundaries around the given byte offset inside `buf`.
    /// A word is a contiguous run of alphanumeric characters plus `_`.
    /// Returns `(start, end)` byte offsets, or `None` if the position
    /// is in whitespace / out of bounds.
    pub fn word_range_at(&self, pos: usize) -> Option<(usize, usize)> {
        let is_word = |c: char| c.is_alphanumeric() || c == '_';
        let bytes = self.buf.as_bytes();
        if pos >= bytes.len() {
            return None;
        }
        let mut start = pos;
        while start > 0 {
            let prev = self.buf[..start]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            let c = self.buf[prev..].chars().next()?;
            if !is_word(c) {
                break;
            }
            start = prev;
        }
        let mut end = pos;
        while end < self.buf.len() {
            let c = self.buf[end..].chars().next()?;
            if !is_word(c) {
                break;
            }
            end += c.len_utf8();
        }
        if start == end {
            None
        } else {
            Some((start, end))
        }
    }
}

impl Default for Buffer {
    fn default() -> Self {
        Self::new()
    }
}
