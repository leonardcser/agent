//! Low-level buffer editing primitives for `InputState`.
//!
//! These operate directly on `buf`, `cpos`, and `attachment_ids`, and are the
//! implementation details behind the `KeyAction` dispatch in `mod.rs`. They
//! assume any selection handling, undo recording, and completer recomputation
//! has already been set up by the caller (the dispatcher).

use super::{InputState, ATTACHMENT_MARKER, PASTE_LINE_THRESHOLD};
use crate::attachment::AttachmentId;
use crate::vim::ViMode;

impl InputState {
    /// Save undo state before an editing operation.
    /// When vim is in insert mode, skip — the entire insert session is
    /// already covered by the undo entry saved on insert entry.
    pub fn save_undo(&mut self) {
        if let Some(ref vim) = self.buffer.vim {
            if vim.mode() == ViMode::Insert {
                return; // insert session groups all edits into one undo step
            }
        }
        self.buffer.history.save(crate::undo::UndoEntry::snapshot(
            &self.buffer.buf,
            self.buffer.cpos,
            &self.buffer.attachment_ids,
        ));
    }

    pub(super) fn insert_char(&mut self, c: char) {
        self.from_paste = false;
        if self.selection_range().is_some() {
            self.save_undo();
            self.delete_selection();
        }
        self.buffer.buf.insert(self.buffer.cpos, c);
        self.buffer.cpos += c.len_utf8();
        self.recompute_completer();
    }

    pub(super) fn backspace(&mut self) {
        if self.selection_range().is_some() {
            self.save_undo();
            self.delete_selection();
            self.recompute_completer();
            return;
        }
        if self.buffer.cpos == 0 {
            return;
        }
        // If deleting the closing `"` of a `"@path"` token, remove the whole token.
        if let Some(start) = self.quoted_at_ref_start() {
            if start == 0 {
                self.from_paste = false;
            }
            self.buffer.buf.drain(start..self.buffer.cpos);
            self.buffer.cpos = start;
            self.recompute_completer();
            return;
        }
        let prev = self.buffer.buf[..self.buffer.cpos]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
        if prev == 0 {
            self.from_paste = false;
        }
        self.maybe_remove_attachment(prev);
        self.buffer.buf.drain(prev..self.buffer.cpos);
        self.buffer.cpos = prev;
        self.recompute_completer();
    }

    /// If the cursor is right after the closing `"` of a `"@path"` token,
    /// return the byte offset of the opening `"`.
    fn quoted_at_ref_start(&self) -> Option<usize> {
        let before = &self.buffer.buf[..self.buffer.cpos];
        if !before.ends_with('"') {
            return None;
        }
        let inner = &before[..before.len() - 1];
        let at_pos = inner.rfind("@\"")?;
        if at_pos > 0 && !self.buffer.buf[..at_pos].ends_with(char::is_whitespace) {
            return None;
        }
        if inner[at_pos + 2..].contains('"') {
            return None;
        }
        Some(at_pos)
    }

    pub(super) fn delete_word_backward(&mut self) {
        if self.buffer.cpos == 0 {
            return;
        }
        let target = crate::text_utils::word_backward_pos(
            &self.buffer.buf,
            self.buffer.cpos,
            crate::text_utils::CharClass::Word,
        );
        if target == 0 {
            self.from_paste = false;
        }
        self.remove_attachments_in_range(target, self.buffer.cpos);
        self.buffer.buf.drain(target..self.buffer.cpos);
        self.buffer.cpos = target;
        self.recompute_completer();
    }

    pub(super) fn delete_char_forward(&mut self) {
        if self.buffer.cpos >= self.buffer.buf.len() {
            return;
        }
        self.maybe_remove_attachment(self.buffer.cpos);
        let next = self.buffer.buf[self.buffer.cpos..]
            .char_indices()
            .nth(1)
            .map(|(i, _)| self.buffer.cpos + i)
            .unwrap_or(self.buffer.buf.len());
        self.buffer.buf.drain(self.buffer.cpos..next);
        self.recompute_completer();
    }

    pub(super) fn delete_word_forward(&mut self) {
        if self.buffer.cpos >= self.buffer.buf.len() {
            return;
        }
        let target = crate::text_utils::word_forward_pos(
            &self.buffer.buf,
            self.buffer.cpos,
            crate::text_utils::CharClass::Word,
        );
        self.remove_attachments_in_range(self.buffer.cpos, target);
        self.buffer.buf.drain(self.buffer.cpos..target);
        self.recompute_completer();
    }

    pub(super) fn kill_to_end_of_line(&mut self) {
        let end = self.buffer.buf[self.buffer.cpos..]
            .find('\n')
            .map(|i| self.buffer.cpos + i)
            .unwrap_or(self.buffer.buf.len());
        let killed = self.buffer.buf[self.buffer.cpos..end].to_string();
        self.remove_attachments_in_range(self.buffer.cpos, end);
        self.buffer.buf.drain(self.buffer.cpos..end);
        self.kill_and_copy(killed);
        self.recompute_completer();
    }

    pub(super) fn kill_to_start_of_line(&mut self) {
        let start = self.buffer.buf[..self.buffer.cpos]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let killed = self.buffer.buf[start..self.buffer.cpos].to_string();
        self.remove_attachments_in_range(start, self.buffer.cpos);
        self.buffer.buf.drain(start..self.buffer.cpos);
        self.buffer.cpos = start;
        self.kill_and_copy(killed);
        self.recompute_completer();
    }

    pub(super) fn delete_to_start_of_line(&mut self) {
        let start = self.buffer.buf[..self.buffer.cpos]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        self.remove_attachments_in_range(start, self.buffer.cpos);
        self.buffer.buf.drain(start..self.buffer.cpos);
        self.buffer.cpos = start;
        self.recompute_completer();
    }

    pub(super) fn uppercase_word(&mut self) {
        let end = crate::text_utils::word_forward_pos(
            &self.buffer.buf,
            self.buffer.cpos,
            crate::text_utils::CharClass::Word,
        );
        if end == self.buffer.cpos {
            return;
        }
        let upper: String = self.buffer.buf[self.buffer.cpos..end].to_uppercase();
        self.buffer.buf.replace_range(self.buffer.cpos..end, &upper);
        self.buffer.cpos += upper.len();
        self.recompute_completer();
    }

    pub(super) fn lowercase_word(&mut self) {
        let end = crate::text_utils::word_forward_pos(
            &self.buffer.buf,
            self.buffer.cpos,
            crate::text_utils::CharClass::Word,
        );
        if end == self.buffer.cpos {
            return;
        }
        let lower: String = self.buffer.buf[self.buffer.cpos..end].to_lowercase();
        self.buffer.buf.replace_range(self.buffer.cpos..end, &lower);
        self.buffer.cpos += lower.len();
        self.recompute_completer();
    }

    pub(super) fn capitalize_word(&mut self) {
        let end = crate::text_utils::word_forward_pos(
            &self.buffer.buf,
            self.buffer.cpos,
            crate::text_utils::CharClass::Word,
        );
        if end == self.buffer.cpos {
            return;
        }
        let word = &self.buffer.buf[self.buffer.cpos..end];
        let mut cap = String::with_capacity(word.len());
        let mut first = true;
        for c in word.chars() {
            if first && c.is_alphabetic() {
                cap.extend(c.to_uppercase());
                first = false;
            } else {
                cap.push(c);
            }
        }
        self.buffer.buf.replace_range(self.buffer.cpos..end, &cap);
        self.buffer.cpos += cap.len();
        self.recompute_completer();
    }

    pub(super) fn undo(&mut self) {
        let current = crate::undo::UndoEntry::snapshot(&self.buffer.buf, self.buffer.cpos, &self.buffer.attachment_ids);
        if let Some(entry) = self.buffer.history.undo(current) {
            self.buffer.buf = entry.buf;
            self.buffer.cpos = entry.cpos;
            self.buffer.attachment_ids = entry.attachments;
        }
        self.recompute_completer();
    }

    pub(super) fn move_word_forward(&mut self) -> bool {
        if self.buffer.cpos >= self.buffer.buf.len() {
            return false;
        }
        let target = crate::text_utils::word_forward_pos(
            &self.buffer.buf,
            self.buffer.cpos,
            crate::text_utils::CharClass::Word,
        );
        if target != self.buffer.cpos {
            self.buffer.cpos = target;
            self.recompute_completer();
            true
        } else {
            false
        }
    }

    pub(super) fn move_word_backward(&mut self) -> bool {
        if self.buffer.cpos == 0 {
            return false;
        }
        let target = crate::text_utils::word_backward_pos(
            &self.buffer.buf,
            self.buffer.cpos,
            crate::text_utils::CharClass::Word,
        );
        if target != self.buffer.cpos {
            self.buffer.cpos = target;
            self.recompute_completer();
            true
        } else {
            false
        }
    }

    pub(super) fn insert_paste(&mut self, data: String) {
        // Normalize line endings: terminals (especially macOS) send \r for
        // newlines in bracketed paste mode.  Convert \r\n and lone \r to \n
        // so that line counting and display work correctly.
        let data = data.replace("\r\n", "\n").replace('\r', "\n");

        if data.is_empty() {
            return;
        }

        let lines = data.lines().count();
        let char_threshold = PASTE_LINE_THRESHOLD * (crate::render::term_width().saturating_sub(1));
        // Mark as from_paste if inserting at the beginning of the current line.
        // This prevents pasted content starting with '!' from being treated as a shell escape.
        let line_start = self.buffer.buf[..self.buffer.cpos]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        if self.buffer.cpos == line_start {
            self.from_paste = true;
        }
        if lines >= PASTE_LINE_THRESHOLD || data.len() >= char_threshold {
            let id = self.store.insert_paste(data);
            self.insert_attachment_id(id);
        } else {
            self.buffer.buf.insert_str(self.buffer.cpos, &data);
            self.buffer.cpos += data.len();
        }
    }

    pub(super) fn insert_attachment_id(&mut self, id: AttachmentId) {
        let idx = self.buffer.buf[..self.buffer.cpos]
            .chars()
            .filter(|&c| c == ATTACHMENT_MARKER)
            .count();
        self.buffer.attachment_ids.insert(idx, id);
        self.buffer.buf.insert(self.buffer.cpos, ATTACHMENT_MARKER);
        self.buffer.cpos += ATTACHMENT_MARKER.len_utf8();
    }

    /// Remove attachment IDs for any markers in `buf[start..end]`.
    pub(super) fn remove_attachments_in_range(&mut self, start: usize, end: usize) {
        let before = self.buffer.buf[..start]
            .chars()
            .filter(|&c| c == ATTACHMENT_MARKER)
            .count();
        let count = self.buffer.buf[start..end]
            .chars()
            .filter(|&c| c == ATTACHMENT_MARKER)
            .count();
        for i in (0..count).rev() {
            let idx = before + i;
            if idx < self.buffer.attachment_ids.len() {
                self.buffer.attachment_ids.remove(idx);
            }
        }
    }

    pub(super) fn maybe_remove_attachment(&mut self, byte_pos: usize) {
        if self.buffer.buf[byte_pos..].starts_with(ATTACHMENT_MARKER) {
            let idx = self.buffer.buf[..byte_pos]
                .chars()
                .filter(|&c| c == ATTACHMENT_MARKER)
                .count();
            if idx < self.buffer.attachment_ids.len() {
                self.buffer.attachment_ids.remove(idx);
            }
        }
    }

    /// Move cursor to the beginning of the given line number (0-indexed).
    pub(super) fn move_to_line(&mut self, target_line: usize) {
        let mut line = 0;
        let mut pos = 0;
        for (i, c) in self.buffer.buf.char_indices() {
            if line == target_line {
                pos = i;
                break;
            }
            if c == '\n' {
                line += 1;
                if line == target_line {
                    pos = i + 1;
                    break;
                }
            }
        }
        if line < target_line {
            // target beyond end, go to last line start
            pos = self.buffer.buf.rfind('\n').map(|i| i + 1).unwrap_or(0);
        }
        self.buffer.cpos = pos;
        self.recompute_completer();
    }

    /// Kill text into the kill ring and copy to the system clipboard.
    pub(super) fn kill_and_copy(&mut self, text: String) {
        if !text.is_empty() {
            let _ = crate::app::copy_to_clipboard(&text);
        }
        self.buffer.kill_ring.kill(text);
    }
}
