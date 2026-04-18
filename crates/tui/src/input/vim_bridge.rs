//! Bridge between `InputState` and the vim state machine.
//!
//! Vim owns its own mode/count state but operates on the input's live
//! `buf`/`cpos`/`attachment_ids`. After Part B of the refactor, vim no longer
//! keeps a private register or undo history either — it borrows the kill ring
//! and the single `UndoHistory` owned by `InputState` through `VimContext`,
//! so no post-key sync is needed.

use super::{Action, History, InputState};
use crate::vim::{self, VimContext};
use crossterm::event::{Event, KeyEvent};

/// Outcome of the vim bridge for a single key event.
pub(super) enum VimBridgeResult {
    /// Vim consumed the key (possibly generating an Action for the caller).
    Handled(Action),
    /// Vim passed the key through; caller should continue to keymap lookup.
    Passthrough,
    /// Not a key event or vim disabled — caller handles as paste/resize/etc.
    NotAKey,
}

impl InputState {
    pub(super) fn dispatch_vim(
        &mut self,
        ev: &Event,
        history: &mut Option<&mut History>,
    ) -> VimBridgeResult {
        if self.buffer.vim.is_none() {
            return VimBridgeResult::NotAKey;
        }
        let Event::Key(key_ev) = ev else {
            return VimBridgeResult::NotAKey;
        };
        let key_ev: KeyEvent = *key_ev;

        let vim = self.buffer.vim.as_mut().unwrap();
        let result = {
            let mut ctx = VimContext {
                buf: &mut self.buffer.buf,
                cpos: &mut self.buffer.cpos,
                attachments: &mut self.buffer.attachment_ids,
                kill_ring: &mut self.buffer.kill_ring,
                history: &mut self.buffer.history,
            };
            vim.handle_key(key_ev, &mut ctx)
        };

        match result {
            vim::Action::Consumed => {
                // Clear shift+key selection on any vim-consumed key
                // (e.g. Esc in insert mode, Esc in visual mode).
                self.clear_selection();
                self.recompute_completer();
                VimBridgeResult::Handled(Action::Redraw)
            }
            vim::Action::Submit => {
                if self.buffer.buf.is_empty() && self.buffer.attachment_ids.is_empty() {
                    VimBridgeResult::Handled(Action::SubmitEmpty)
                } else {
                    let display = self.message_display_text();
                    let content = self.build_content();
                    self.clear();
                    VimBridgeResult::Handled(Action::Submit { content, display })
                }
            }
            vim::Action::HistoryPrev => {
                if let Some(entry) = history.as_deref_mut().and_then(|h| h.up(&self.buffer.buf)) {
                    self.buffer.buf = entry.to_string();
                    self.buffer.cpos = 0;
                    self.sync_completer();
                }
                VimBridgeResult::Handled(Action::Redraw)
            }
            vim::Action::HistoryNext => {
                if let Some(entry) = history.as_deref_mut().and_then(|h| h.down()) {
                    self.buffer.buf = entry.to_string();
                    self.buffer.cpos = self.buffer.buf.len();
                    self.sync_completer();
                }
                VimBridgeResult::Handled(Action::Redraw)
            }
            vim::Action::EditInEditor => VimBridgeResult::Handled(Action::EditInEditor),
            vim::Action::CenterScroll => VimBridgeResult::Handled(Action::CenterScroll),
            vim::Action::Passthrough => VimBridgeResult::Passthrough,
        }
    }
}
