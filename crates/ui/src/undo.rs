//! Shared undo/redo history for the input buffer and vim motions.
//!
//! A single `UndoEntry` captures the triple (buf, cpos, attachments) that both
//! the plain input and vim need to save. `UndoHistory` owns the undo and redo
//! stacks and enforces an optional size cap. Callers drive it with the current
//! buffer state on each operation; the history only stores states and never
//! mutates the live buffer directly.

use crate::AttachmentId;

pub struct UndoEntry {
    pub buf: String,
    pub cpos: usize,
    pub attachments: Vec<AttachmentId>,
}

impl UndoEntry {
    pub fn snapshot(buf: &str, cpos: usize, attachments: &[AttachmentId]) -> Self {
        Self {
            buf: buf.to_string(),
            cpos,
            attachments: attachments.to_vec(),
        }
    }
}

pub struct UndoHistory {
    undo: Vec<UndoEntry>,
    redo: Vec<UndoEntry>,
    cap: Option<usize>,
}

impl UndoHistory {
    pub fn new(cap: Option<usize>) -> Self {
        Self {
            undo: Vec::new(),
            redo: Vec::new(),
            cap,
        }
    }

    /// Record a pre-edit snapshot, clearing any pending redo.
    pub fn save(&mut self, entry: UndoEntry) {
        self.redo.clear();
        self.undo.push(entry);
        if let Some(cap) = self.cap {
            while self.undo.len() > cap {
                self.undo.remove(0);
            }
        }
    }

    /// Pop the most recent snapshot, stashing `current` onto the redo stack.
    pub fn undo(&mut self, current: UndoEntry) -> Option<UndoEntry> {
        let entry = self.undo.pop()?;
        self.redo.push(current);
        Some(entry)
    }

    /// Pop the most recent redo, stashing `current` back onto the undo stack.
    pub fn redo(&mut self, current: UndoEntry) -> Option<UndoEntry> {
        let entry = self.redo.pop()?;
        self.undo.push(current);
        Some(entry)
    }
}
