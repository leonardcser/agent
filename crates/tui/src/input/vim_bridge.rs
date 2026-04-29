//! Bridge between `PromptState` and the vim state machine.
//!
//! Vim owns its own mode/count state but operates on the input's live
//! `buf`/`cpos`/`attachment_ids`. After Part B of the refactor, vim no longer
//! keeps a private register or undo history either — it borrows the kill ring
//! and the single `UndoHistory` owned by `PromptState` through `VimContext`,
//! so no post-key sync is needed.

use super::{Action, History, PromptState};
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

impl PromptState {
    pub(super) fn dispatch_vim(
        &mut self,
        ev: &Event,
        history: &mut Option<&mut History>,
    ) -> VimBridgeResult {
        if self.win.vim.is_none() {
            return VimBridgeResult::NotAKey;
        }
        let Event::Key(key_ev) = ev else {
            return VimBridgeResult::NotAKey;
        };
        let key_ev: KeyEvent = *key_ev;

        // Seed vim's curswant from the window cursor so a prior
        // shift+arrow-driven vertical motion's preferred column is
        // preserved when the user next hits j/k, and write it back out
        // after — one source of truth, one `curswant`.
        let seed = self.win.win_cursor.curswant();
        let vim = self.win.vim.as_mut().unwrap();
        vim.set_curswant(seed);
        let result = {
            let mut clipboard = crate::app::commands::SystemClipboard;
            let mut ctx = VimContext {
                buf: &mut self.win.edit_buf.buf,
                cpos: &mut self.win.cpos,
                attachments: &mut self.win.edit_buf.attachment_ids,
                kill_ring: &mut self.win.kill_ring,
                history: &mut self.win.edit_buf.history,
                clipboard: &mut clipboard,
            };
            vim.handle_key(key_ev, &mut ctx)
        };
        let post = self.win.vim.as_ref().unwrap().curswant();
        self.win.win_cursor.set_curswant(post);

        match result {
            vim::Action::Consumed => {
                // Clear shift+key selection on any vim-consumed key
                // (e.g. Esc in insert mode, Esc in visual mode).
                self.clear_selection();
                self.recompute_completer();
                VimBridgeResult::Handled(Action::Redraw)
            }
            vim::Action::Submit => {
                if self.win.edit_buf.buf.is_empty() && self.win.edit_buf.attachment_ids.is_empty() {
                    VimBridgeResult::Handled(Action::SubmitEmpty)
                } else {
                    let display = self.message_display_text();
                    let content = self.build_content();
                    self.clear();
                    VimBridgeResult::Handled(Action::Submit { content, display })
                }
            }
            vim::Action::HistoryPrev => {
                if let Some(entry) = history
                    .as_deref_mut()
                    .and_then(|h| h.up(&self.win.edit_buf.buf))
                {
                    self.win.edit_buf.buf = entry.to_string();
                    self.win.cpos = 0;
                    self.sync_completer();
                }
                VimBridgeResult::Handled(Action::Redraw)
            }
            vim::Action::HistoryNext => {
                if let Some(entry) = history.as_deref_mut().and_then(|h| h.down()) {
                    self.win.edit_buf.buf = entry.to_string();
                    self.win.cpos = self.win.edit_buf.buf.len();
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
