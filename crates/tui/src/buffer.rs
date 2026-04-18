//! Unified buffer abstraction shared by the prompt (writable) and the
//! content pane (readonly). A `Buffer` owns text + cursor + selection +
//! vim + kill-ring + undo + viewport state, and exposes a single set of
//! methods for key handling, mouse clicks, and scroll — so both panes
//! route through the same code path regardless of whether they can be
//! edited.
//!
//! The content pane refreshes `buf`/`cpos` each time from the rendered
//! transcript before dispatching, since the transcript is recomputed
//! every frame. Writable buffers persist their content across calls.

use crate::attachment::AttachmentId;
use crate::input::KillRing;
use crate::undo::UndoHistory;
use crate::vim::{self, Vim, VimContext};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

/// Text buffer with editor state shared between the prompt and the
/// content pane. `readonly = true` disables edit-producing actions —
/// vim `Insert` is snapped back to `Normal`, and the buffer content
/// is expected to be fed from outside on every call.
pub struct Buffer {
    /// Raw UTF-8 text content. For readonly buffers this is refreshed
    /// from the renderer's view-text each dispatch.
    pub buf: String,
    /// Byte offset of the cursor inside `buf`.
    pub cpos: usize,
    /// If set, buf/cpos edits are rejected and vim is forced out of
    /// insert mode on each key.
    pub readonly: bool,
    /// Vim state (motions, visual modes, curswant, pending op). `None`
    /// means vim is disabled on this buffer.
    pub vim: Option<Vim>,
    /// Non-vim selection anchor (shift+arrow). Vim visual mode takes
    /// priority over this.
    pub selection_anchor: Option<usize>,
    /// Per-buffer kill ring for yank/paste.
    pub kill_ring: KillRing,
    /// Attachment markers inside `buf`. Readonly buffers keep this
    /// empty. Named to match `InputState.attachment_ids` so the field
    /// path is identical whether the buffer is writable or readonly.
    pub attachment_ids: Vec<AttachmentId>,
    /// Undo/redo stack.
    pub history: UndoHistory,
    /// Rows scrolled away from the bottom edge of the viewport (0 =
    /// stuck to bottom). Only used by panes whose content is larger
    /// than the visible area.
    pub scroll_offset: u16,
    /// Viewport-relative cursor row, 0 = bottom visible row,
    /// viewport_rows - 1 = top.
    pub cursor_line: u16,
    /// Visual column of the cursor.
    pub cursor_col: u16,
}

impl Buffer {
    /// A new empty writable buffer.
    pub fn writable() -> Self {
        Self {
            buf: String::new(),
            cpos: 0,
            readonly: false,
            vim: None,
            selection_anchor: None,
            kill_ring: KillRing::new(),
            attachment_ids: Vec::new(),
            history: UndoHistory::new(Some(100)),
            scroll_offset: 0,
            cursor_line: 0,
            cursor_col: 0,
        }
    }

    /// A new empty readonly buffer with its own vim instance for
    /// motions and visual/yank.
    pub fn readonly() -> Self {
        Self {
            buf: String::new(),
            cpos: 0,
            readonly: true,
            vim: Some(Vim::new()),
            selection_anchor: None,
            kill_ring: KillRing::new(),
            attachment_ids: Vec::new(),
            history: UndoHistory::new(None),
            scroll_offset: 0,
            cursor_line: 0,
            cursor_col: 0,
        }
    }

    /// Current selection range (vim visual takes priority over
    /// shift-selection anchor). Returns byte offsets in `buf`.
    pub fn selection_range(&self) -> Option<(usize, usize)> {
        if let Some(ref vim) = self.vim {
            if let Some(range) = vim.visual_range(&self.buf, self.cpos) {
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

    /// Hand `key` to the buffer's vim instance (if any). Returns
    /// `true` if vim consumed the key. Readonly buffers auto-snap back
    /// to Normal mode.
    pub fn handle_vim_key(&mut self, key: KeyEvent) -> bool {
        let Some(vim) = self.vim.as_mut() else {
            return false;
        };
        let mut cpos = self.cpos;
        let mut ctx = VimContext {
            buf: &mut self.buf,
            cpos: &mut cpos,
            attachments: &mut self.attachment_ids,
            kill_ring: &mut self.kill_ring,
            history: &mut self.history,
        };
        let action = vim.handle_key(key, &mut ctx);
        if self.readonly && vim.mode() == vim::ViMode::Insert {
            vim.set_mode(vim::ViMode::Normal);
        }
        self.cpos = cpos;
        !matches!(action, vim::Action::Passthrough)
    }

    /// Synthesize `count` presses of the given key code with no
    /// modifiers and dispatch them through the vim path. Used by
    /// mouse wheel scroll (which feeds `j` / `k`) so vertical motion
    /// reuses the exact same code path — including `curswant` (desired
    /// column) — as arrow keys or `j`/`k`.
    pub fn press_n(&mut self, code: KeyCode, count: usize) {
        let k = KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        };
        for _ in 0..count {
            if !self.handle_vim_key(k) {
                break;
            }
        }
    }
}
