//! Low-level buffer editing primitives for `PromptState`.
//!
//! These operate directly on `buf`, `cpos`, and `attachment_ids`, and are the
//! implementation details behind the `KeyAction` dispatch in `mod.rs`. They
//! assume any selection handling, undo recording, and completer recomputation
//! has already been set up by the caller (the dispatcher).

use super::{PromptState, ATTACHMENT_MARKER, PASTE_LINE_THRESHOLD};
use crate::attachment::AttachmentId;
use crate::vim::ViMode;

impl PromptState {
    /// Save undo state before an editing operation.
    /// When vim is in insert mode, skip — the entire insert session is
    /// already covered by the undo entry saved on insert entry.
    pub fn save_undo(&mut self) {
        if let Some(ref vim) = self.win.vim {
            if vim.mode() == ViMode::Insert {
                return; // insert session groups all edits into one undo step
            }
        }
        self.win.edit_buf.history.save(ui::UndoEntry::snapshot(
            &self.win.edit_buf.buf,
            self.win.cpos,
            &self.win.edit_buf.attachment_ids,
        ));
    }

    pub(super) fn insert_char(&mut self, c: char) {
        self.from_paste = false;
        if self.selection_range().is_some() {
            self.save_undo();
            self.delete_selection();
        }
        self.win.edit_buf.buf.insert(self.win.cpos, c);
        self.win.cpos += c.len_utf8();
        self.recompute_completer();
    }

    pub(super) fn backspace(&mut self) {
        if self.selection_range().is_some() {
            self.save_undo();
            self.delete_selection();
            self.recompute_completer();
            return;
        }
        if self.win.cpos == 0 {
            return;
        }
        // If deleting the closing `"` of a `"@path"` token, remove the whole token.
        if let Some(start) = self.quoted_at_ref_start() {
            if start == 0 {
                self.from_paste = false;
            }
            self.win.edit_buf.buf.drain(start..self.win.cpos);
            self.win.cpos = start;
            self.recompute_completer();
            return;
        }
        let prev = self.win.edit_buf.buf[..self.win.cpos]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
        if prev == 0 {
            self.from_paste = false;
        }
        self.maybe_remove_attachment(prev);
        self.win.edit_buf.buf.drain(prev..self.win.cpos);
        self.win.cpos = prev;
        self.recompute_completer();
    }

    /// If the cursor is right after the closing `"` of a `"@path"` token,
    /// return the byte offset of the opening `"`.
    fn quoted_at_ref_start(&self) -> Option<usize> {
        let before = &self.win.edit_buf.buf[..self.win.cpos];
        if !before.ends_with('"') {
            return None;
        }
        let inner = &before[..before.len() - 1];
        let at_pos = inner.rfind("@\"")?;
        if at_pos > 0 && !self.win.edit_buf.buf[..at_pos].ends_with(char::is_whitespace) {
            return None;
        }
        if inner[at_pos + 2..].contains('"') {
            return None;
        }
        Some(at_pos)
    }

    pub(super) fn delete_word_backward(&mut self) {
        if self.win.cpos == 0 {
            return;
        }
        let target = ui::text::word_backward_pos(
            &self.win.edit_buf.buf,
            self.win.cpos,
            ui::text::CharClass::Word,
        );
        if target == 0 {
            self.from_paste = false;
        }
        self.remove_attachments_in_range(target, self.win.cpos);
        self.win.edit_buf.buf.drain(target..self.win.cpos);
        self.win.cpos = target;
        self.recompute_completer();
    }

    pub(super) fn delete_char_forward(&mut self) {
        if self.win.cpos >= self.win.edit_buf.buf.len() {
            return;
        }
        self.maybe_remove_attachment(self.win.cpos);
        let next = self.win.edit_buf.buf[self.win.cpos..]
            .char_indices()
            .nth(1)
            .map(|(i, _)| self.win.cpos + i)
            .unwrap_or(self.win.edit_buf.buf.len());
        self.win.edit_buf.buf.drain(self.win.cpos..next);
        self.recompute_completer();
    }

    pub(super) fn delete_word_forward(&mut self) {
        if self.win.cpos >= self.win.edit_buf.buf.len() {
            return;
        }
        let target = ui::text::word_forward_pos(
            &self.win.edit_buf.buf,
            self.win.cpos,
            ui::text::CharClass::Word,
        );
        self.remove_attachments_in_range(self.win.cpos, target);
        self.win.edit_buf.buf.drain(self.win.cpos..target);
        self.recompute_completer();
    }

    pub(super) fn kill_to_end_of_line(&mut self) {
        let end = self.win.edit_buf.buf[self.win.cpos..]
            .find('\n')
            .map(|i| self.win.cpos + i)
            .unwrap_or(self.win.edit_buf.buf.len());
        let killed = self.win.edit_buf.buf[self.win.cpos..end].to_string();
        self.remove_attachments_in_range(self.win.cpos, end);
        self.win.edit_buf.buf.drain(self.win.cpos..end);
        self.kill_and_copy(killed);
        self.recompute_completer();
    }

    pub(super) fn kill_to_start_of_line(&mut self) {
        let start = self.win.edit_buf.buf[..self.win.cpos]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let killed = self.win.edit_buf.buf[start..self.win.cpos].to_string();
        self.remove_attachments_in_range(start, self.win.cpos);
        self.win.edit_buf.buf.drain(start..self.win.cpos);
        self.win.cpos = start;
        self.kill_and_copy(killed);
        self.recompute_completer();
    }

    pub(super) fn delete_to_start_of_line(&mut self) {
        let start = self.win.edit_buf.buf[..self.win.cpos]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        self.remove_attachments_in_range(start, self.win.cpos);
        self.win.edit_buf.buf.drain(start..self.win.cpos);
        self.win.cpos = start;
        self.recompute_completer();
    }

    pub(super) fn uppercase_word(&mut self) {
        let end = ui::text::word_forward_pos(
            &self.win.edit_buf.buf,
            self.win.cpos,
            ui::text::CharClass::Word,
        );
        if end == self.win.cpos {
            return;
        }
        let upper: String = self.win.edit_buf.buf[self.win.cpos..end].to_uppercase();
        self.win
            .edit_buf
            .buf
            .replace_range(self.win.cpos..end, &upper);
        self.win.cpos += upper.len();
        self.recompute_completer();
    }

    pub(super) fn lowercase_word(&mut self) {
        let end = ui::text::word_forward_pos(
            &self.win.edit_buf.buf,
            self.win.cpos,
            ui::text::CharClass::Word,
        );
        if end == self.win.cpos {
            return;
        }
        let lower: String = self.win.edit_buf.buf[self.win.cpos..end].to_lowercase();
        self.win
            .edit_buf
            .buf
            .replace_range(self.win.cpos..end, &lower);
        self.win.cpos += lower.len();
        self.recompute_completer();
    }

    pub(super) fn capitalize_word(&mut self) {
        let end = ui::text::word_forward_pos(
            &self.win.edit_buf.buf,
            self.win.cpos,
            ui::text::CharClass::Word,
        );
        if end == self.win.cpos {
            return;
        }
        let word = &self.win.edit_buf.buf[self.win.cpos..end];
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
        self.win
            .edit_buf
            .buf
            .replace_range(self.win.cpos..end, &cap);
        self.win.cpos += cap.len();
        self.recompute_completer();
    }

    pub(super) fn undo(&mut self) {
        let current = ui::UndoEntry::snapshot(
            &self.win.edit_buf.buf,
            self.win.cpos,
            &self.win.edit_buf.attachment_ids,
        );
        if let Some(entry) = self.win.edit_buf.history.undo(current) {
            self.win.edit_buf.buf = entry.buf;
            self.win.cpos = entry.cpos;
            self.win.edit_buf.attachment_ids = entry.attachments;
        }
        self.recompute_completer();
    }

    pub(super) fn move_word_forward(&mut self) -> bool {
        if self.win.cpos >= self.win.edit_buf.buf.len() {
            return false;
        }
        let target = ui::text::word_forward_pos(
            &self.win.edit_buf.buf,
            self.win.cpos,
            ui::text::CharClass::Word,
        );
        if target != self.win.cpos {
            self.win.cpos = target;
            self.recompute_completer();
            true
        } else {
            false
        }
    }

    pub(super) fn move_word_backward(&mut self) -> bool {
        if self.win.cpos == 0 {
            return false;
        }
        let target = ui::text::word_backward_pos(
            &self.win.edit_buf.buf,
            self.win.cpos,
            ui::text::CharClass::Word,
        );
        if target != self.win.cpos {
            self.win.cpos = target;
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
        let char_threshold =
            PASTE_LINE_THRESHOLD * (crate::content::term_width().saturating_sub(1));
        // Mark as from_paste if inserting at the beginning of the current line.
        // This prevents pasted content starting with '!' from being treated as a shell escape.
        let line_start = self.win.edit_buf.buf[..self.win.cpos]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        if self.win.cpos == line_start {
            self.from_paste = true;
        }
        if lines >= PASTE_LINE_THRESHOLD || data.len() >= char_threshold {
            let id = self.store.insert_paste(data);
            self.insert_attachment_id(id);
        } else {
            self.win.edit_buf.buf.insert_str(self.win.cpos, &data);
            self.win.cpos += data.len();
        }
    }

    pub(super) fn insert_attachment_id(&mut self, id: AttachmentId) {
        let idx = self.win.edit_buf.buf[..self.win.cpos]
            .chars()
            .filter(|&c| c == ATTACHMENT_MARKER)
            .count();
        self.win.edit_buf.attachment_ids.insert(idx, id);
        self.win
            .edit_buf
            .buf
            .insert(self.win.cpos, ATTACHMENT_MARKER);
        self.win.cpos += ATTACHMENT_MARKER.len_utf8();
    }

    /// Remove attachment IDs for any markers in `buf[start..end]`.
    pub(super) fn remove_attachments_in_range(&mut self, start: usize, end: usize) {
        let before = self.win.edit_buf.buf[..start]
            .chars()
            .filter(|&c| c == ATTACHMENT_MARKER)
            .count();
        let count = self.win.edit_buf.buf[start..end]
            .chars()
            .filter(|&c| c == ATTACHMENT_MARKER)
            .count();
        for i in (0..count).rev() {
            let idx = before + i;
            if idx < self.win.edit_buf.attachment_ids.len() {
                self.win.edit_buf.attachment_ids.remove(idx);
            }
        }
    }

    pub(super) fn maybe_remove_attachment(&mut self, byte_pos: usize) {
        if self.win.edit_buf.buf[byte_pos..].starts_with(ATTACHMENT_MARKER) {
            let idx = self.win.edit_buf.buf[..byte_pos]
                .chars()
                .filter(|&c| c == ATTACHMENT_MARKER)
                .count();
            if idx < self.win.edit_buf.attachment_ids.len() {
                self.win.edit_buf.attachment_ids.remove(idx);
            }
        }
    }

    /// Move cursor to the beginning of the given line number (0-indexed).
    pub(super) fn move_to_line(&mut self, target_line: usize) {
        let mut line = 0;
        let mut pos = 0;
        for (i, c) in self.win.edit_buf.buf.char_indices() {
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
            pos = self
                .win
                .edit_buf
                .buf
                .rfind('\n')
                .map(|i| i + 1)
                .unwrap_or(0);
        }
        self.win.cpos = pos;
        self.recompute_completer();
    }

    /// Kill text into the kill ring and copy to the system clipboard.
    /// Records the clipboard write on the kill ring so subsequent
    /// pastes know this is *our* latest push (distinguished from an
    /// externally-updated clipboard).
    pub(super) fn kill_and_copy(&mut self, text: String) {
        if !text.is_empty() && crate::app::copy_to_clipboard(&text).is_ok() {
            self.win.kill_ring.record_clipboard_write(text.clone());
        }
        self.win.kill_ring.kill(text);
    }
}
