//! `EditBuffer` — pure content. Holds the text, the undo stack, and
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

use crate::undo::UndoHistory;
use crate::AttachmentId;

/// Pure-content edit buffer. The owning window provides cursor / vim /
/// selection state when operating on it.
pub struct EditBuffer {
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

impl EditBuffer {
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
        word_range_at(&self.buf, pos)
    }

    /// Like `word_range_at` but treats byte positions listed in
    /// `transparent` as if they were word characters during the walk.
    /// Used to cross soft-wrap `\n` boundaries so a word split by a
    /// display wrap still selects as one unit. `transparent` must be
    /// sorted ascending. Leading/trailing transparent bytes are
    /// trimmed from the returned range.
    pub fn word_range_at_transparent(
        &self,
        pos: usize,
        transparent: &[usize],
    ) -> Option<(usize, usize)> {
        word_range_at_transparent(&self.buf, pos, transparent)
    }

    /// Vim "WORD" (capital W) variant of [`Self::word_range_at_transparent`]:
    /// a token is any contiguous run of non-whitespace characters,
    /// punctuation included. Used by the transcript double-click
    /// selector so e.g. `foo.bar(baz)` selects as one unit.
    pub fn big_word_range_at_transparent(
        &self,
        pos: usize,
        transparent: &[usize],
    ) -> Option<(usize, usize)> {
        big_word_range_at_transparent(&self.buf, pos, transparent)
    }

    /// Source-line range at `pos`. `hard_breaks` lists byte positions of
    /// `\n` characters that are "real" line breaks (i.e. not soft-wrap
    /// continuations). Returns the span bounded by the previous hard
    /// break (exclusive) and the next hard break (exclusive), or the
    /// buffer start/end. The returned range does not include the
    /// trailing `\n`.
    pub fn line_range_at(&self, pos: usize, hard_breaks: &[usize]) -> Option<(usize, usize)> {
        line_range_at(&self.buf, pos, hard_breaks)
    }
}

/// Standalone version of [`EditBuffer::word_range_at`] that operates on
/// any `&str`. Window's mouse helpers use this against
/// `rows.join("\n")` so they don't need `self.edit_buf.buf` to match —
/// the prompt's `edit_buf.buf` is the source buffer (≠ wrapped display
/// rows), so a `self`-bound method couldn't be reused there.
pub fn word_range_at(buf: &str, pos: usize) -> Option<(usize, usize)> {
    word_range_at_transparent(buf, pos, &[])
}

/// Like [`word_range_at`] but treats byte positions listed in
/// `transparent` as if they were word characters during the walk.
pub fn word_range_at_transparent(
    buf: &str,
    pos: usize,
    transparent: &[usize],
) -> Option<(usize, usize)> {
    token_range_at_transparent(buf, pos, transparent, |c| c.is_alphanumeric() || c == '_')
}

/// Vim "WORD" range: any contiguous run of non-whitespace.
pub fn big_word_range_at_transparent(
    buf: &str,
    pos: usize,
    transparent: &[usize],
) -> Option<(usize, usize)> {
    token_range_at_transparent(buf, pos, transparent, |c| !c.is_whitespace())
}

fn token_range_at_transparent<F>(
    buf: &str,
    pos: usize,
    transparent: &[usize],
    is_word: F,
) -> Option<(usize, usize)>
where
    F: Fn(char) -> bool,
{
    let is_trans = |p: usize| transparent.binary_search(&p).is_ok();
    if pos >= buf.len() {
        return None;
    }
    let first = buf[pos..].chars().next()?;
    if !is_word(first) && !is_trans(pos) {
        return None;
    }
    let mut start = pos;
    while start > 0 {
        let prev = buf[..start]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
        let c = buf[prev..].chars().next()?;
        if !is_word(c) && !is_trans(prev) {
            break;
        }
        start = prev;
    }
    let mut end = pos;
    while end < buf.len() {
        let c = buf[end..].chars().next()?;
        if !is_word(c) && !is_trans(end) {
            break;
        }
        end += c.len_utf8();
    }
    while start < end {
        let c = buf[start..].chars().next()?;
        if is_trans(start) && !is_word(c) {
            start += c.len_utf8();
        } else {
            break;
        }
    }
    while end > start {
        let prev = buf[..end].char_indices().next_back().map(|(i, _)| i)?;
        let c = buf[prev..].chars().next()?;
        if is_trans(prev) && !is_word(c) {
            end = prev;
        } else {
            break;
        }
    }
    if start == end {
        return None;
    }
    if !buf[start..end].chars().any(is_word) {
        return None;
    }
    Some((start, end))
}

/// Standalone version of [`EditBuffer::line_range_at`].
pub fn line_range_at(buf: &str, pos: usize, hard_breaks: &[usize]) -> Option<(usize, usize)> {
    if buf.is_empty() {
        return None;
    }
    let pos = pos.min(buf.len());
    let start = match hard_breaks.binary_search(&pos) {
        Ok(_) => pos + 1,
        Err(idx) => {
            if idx == 0 {
                0
            } else {
                hard_breaks[idx - 1] + 1
            }
        }
    };
    let end = match hard_breaks.binary_search(&pos) {
        Ok(_) => pos,
        Err(idx) => {
            if idx < hard_breaks.len() {
                hard_breaks[idx]
            } else {
                buf.len()
            }
        }
    };
    if end <= start {
        return None;
    }
    Some((start, end))
}

impl Default for EditBuffer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf(s: &str) -> EditBuffer {
        let mut b = EditBuffer::new();
        b.buf = s.into();
        b
    }

    #[test]
    fn word_range_at_plain() {
        let b = buf("hello world");
        assert_eq!(b.word_range_at(0), Some((0, 5)));
        assert_eq!(b.word_range_at(4), Some((0, 5)));
        assert_eq!(b.word_range_at(6), Some((6, 11)));
        assert_eq!(b.word_range_at(5), None); // on the space
    }

    #[test]
    fn word_range_at_treats_newline_as_non_word() {
        // Baseline: walk stops at '\n', so clicking on "world" only
        // selects "world", not "hello\nworld".
        let b = buf("hello\nworld");
        assert_eq!(b.word_range_at(6), Some((6, 11)));
    }

    #[test]
    fn word_range_at_transparent_crosses_soft_wrap() {
        // "verylong" was soft-wrapped as "very\nlong". The \n at byte 4
        // is a soft-wrap — treat as transparent → whole "verylong"
        // selects regardless of which side was clicked.
        let b = buf("very\nlong");
        let transparent = [4usize];
        assert_eq!(b.word_range_at_transparent(0, &transparent), Some((0, 9)));
        assert_eq!(b.word_range_at_transparent(5, &transparent), Some((0, 9)));
        // Click on the transparent \n itself → still selects the word.
        assert_eq!(b.word_range_at_transparent(4, &transparent), Some((0, 9)));
    }

    #[test]
    fn word_range_at_transparent_trims_trailing_wrap() {
        // "end\n\nrest" with the first \n soft and second hard. Click
        // on "end" should return [0, 3) — the trailing transparent \n
        // is trimmed because nothing word-like followed it in the
        // extended walk (the hard \n stops forward walk before "rest").
        let b = buf("end\n\nrest");
        let transparent = [3usize]; // first \n is soft
        assert_eq!(b.word_range_at_transparent(0, &transparent), Some((0, 3)));
    }

    #[test]
    fn word_range_at_transparent_returns_none_on_punctuation() {
        let b = buf("a, b");
        assert_eq!(b.word_range_at_transparent(1, &[]), None);
    }

    #[test]
    fn line_range_at_single_line() {
        let b = buf("just one line");
        assert_eq!(b.line_range_at(0, &[]), Some((0, 13)));
        assert_eq!(b.line_range_at(6, &[]), Some((0, 13)));
    }

    #[test]
    fn line_range_at_multiple_lines() {
        let b = buf("first\nsecond\nthird");
        let hard = [5usize, 12];
        assert_eq!(b.line_range_at(2, &hard), Some((0, 5)));
        assert_eq!(b.line_range_at(6, &hard), Some((6, 12)));
        assert_eq!(b.line_range_at(13, &hard), Some((13, 18)));
    }

    #[test]
    fn line_range_at_joins_soft_wrapped_rows() {
        // "one long paragraph\nnext" with the first \n being a soft
        // wrap (not in hard_breaks) and the second a real line break.
        // Clicking in the wrapped part should return the full
        // paragraph span.
        let b = buf("one long\nparagraph\nnext");
        let hard = [18usize]; // only the second \n is hard
        assert_eq!(b.line_range_at(4, &hard), Some((0, 18)));
        assert_eq!(b.line_range_at(10, &hard), Some((0, 18)));
        assert_eq!(b.line_range_at(19, &hard), Some((19, 23)));
    }

    #[test]
    fn line_range_at_on_hard_break_returns_empty_line() {
        let b = buf("a\n\nb");
        let hard = [1usize, 2];
        // Pos on the first hard break → empty line after it.
        assert_eq!(b.line_range_at(1, &hard), None);
        // Pos on the second hard break → empty line between.
        assert_eq!(b.line_range_at(2, &hard), None);
    }
}
