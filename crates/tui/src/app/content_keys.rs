//! Content-pane key dispatch: vim/novim key handlers over the readonly
//! transcript buffer. Routes arrow keys, page keys, and vim motions;
//! Enter runs a block-scoped keymap; Ctrl-C returns focus to the prompt.

use super::*;
use crossterm::event::Event;

impl App {
    pub(super) fn handle_event_app_history(&mut self, ev: &Event) -> EventOutcome {
        let k = match ev {
            Event::Key(k) => *k,
            _ => return EventOutcome::Noop,
        };
        use crossterm::event::KeyModifiers as M;

        // Ctrl-C from a non-prompt pane returns focus to the prompt.
        if k.modifiers.contains(M::CONTROL) && matches!(k.code, KeyCode::Char('c')) {
            self.app_focus = crate::app::AppFocus::Prompt;
            return EventOutcome::Redraw;
        }

        // Readonly-buffer scrolling keybinds: Ctrl-U / Ctrl-D (half-page),
        // Ctrl-B / Ctrl-F (full-page), Ctrl-Y / Ctrl-E (one line). These
        // mirror Vim's scroll commands. Since Vim in the prompt reuses
        // PromptState for these, we implement them here by driving the
        // content cursor directly — which in turn pulls the viewport via
        // the normal scroll-follows-cursor logic.
        if k.modifiers.contains(M::CONTROL) {
            let half = (self.viewport_rows_estimate() / 2).max(1) as isize;
            let full = (self.viewport_rows_estimate() as isize).max(1);
            let delta: Option<isize> = match k.code {
                KeyCode::Char('u') => Some(-half),
                KeyCode::Char('d') => Some(half),
                KeyCode::Char('b') => Some(-full),
                KeyCode::Char('f') => Some(full),
                KeyCode::Char('y') => Some(-1),
                KeyCode::Char('e') => Some(1),
                _ => None,
            };
            if let Some(dn) = delta {
                self.move_content_cursor_by_lines(dn);
                return EventOutcome::Redraw;
            }
        }

        // Shift+arrow / Shift+Home/End extends selection via the shared
        // keymap regardless of vim mode — the anchor logic lives in
        // one place (`ShiftSelection`). Vim's own v/V remain for users
        // who prefer them.
        if k.modifiers.contains(M::SHIFT)
            && matches!(
                k.code,
                KeyCode::Left
                    | KeyCode::Right
                    | KeyCode::Up
                    | KeyCode::Down
                    | KeyCode::Home
                    | KeyCode::End
            )
        {
            return self.handle_content_novim_key(k);
        }
        // Block-scoped bindings: the focused block gets first crack at
        // the key before buffer/window keymaps (nvim-style layering).
        if let Some(outcome) = self.dispatch_block_key(k) {
            return outcome;
        }

        if self.transcript_window.vim_enabled() {
            if self.handle_content_vim_key(k) {
                return EventOutcome::Redraw;
            }
            match (k.code, k.modifiers) {
                (KeyCode::Char('q'), M::NONE) => EventOutcome::Quit,
                _ => EventOutcome::Noop,
            }
        } else {
            self.handle_content_novim_key(k)
        }
    }

    /// Content-pane key handler when vim is disabled. Drives the same
    /// selection mechanism as the prompt: shift+movement extends via
    /// `ShiftSelection`; plain movement clears it; Ctrl-C / ⌘C copies.
    fn handle_content_novim_key(&mut self, k: KeyEvent) -> EventOutcome {
        use crate::keymap::{lookup, KeyAction, KeyContext};
        use crossterm::event::KeyModifiers as M;
        // Pull in the latest nav-only text (selectable chars) so cpos
        // stays valid across streaming updates.

        let rows = self.full_transcript_display_text(self.settings.show_thinking);
        let viewport = self.viewport_rows_estimate();
        self.transcript_window.resync(&rows, viewport);
        let ctx = KeyContext {
            buf_empty: self.transcript_window.edit_buf.buf.is_empty(),
            vim_non_insert: false,
            vim_enabled: false,
            agent_running: false,
            ghost_text_visible: false,
        };
        if let Some(action) = lookup(k.code, k.modifiers, &ctx) {
            let extending = matches!(
                action,
                KeyAction::SelectLeft
                    | KeyAction::SelectRight
                    | KeyAction::SelectUp
                    | KeyAction::SelectDown
                    | KeyAction::SelectWordForward
                    | KeyAction::SelectWordBackward
                    | KeyAction::SelectStartOfLine
                    | KeyAction::SelectEndOfLine
            );
            match action {
                KeyAction::MoveLeft
                | KeyAction::MoveRight
                | KeyAction::MoveUp
                | KeyAction::MoveDown
                | KeyAction::MoveStartOfLine
                | KeyAction::MoveEndOfLine
                | KeyAction::MoveWordForward
                | KeyAction::MoveWordBackward => {
                    self.transcript_window.win_cursor.clear_anchor();
                }
                _ if extending => {
                    self.transcript_window
                        .win_cursor
                        .extend(self.transcript_window.cpos);
                }
                _ => {}
            }
            let delta: Option<isize> = match action {
                KeyAction::MoveUp | KeyAction::SelectUp => Some(-1),
                KeyAction::MoveDown | KeyAction::SelectDown => Some(1),
                _ => None,
            };
            if let Some(d) = delta {
                self.move_content_cursor_by_lines(d);
                return EventOutcome::Redraw;
            }
            let buf = self.transcript_window.edit_buf.buf.clone();
            let mv: Option<usize> = match action {
                KeyAction::MoveLeft | KeyAction::SelectLeft => Some(ui::text::prev_char_boundary(
                    &buf,
                    self.transcript_window.cpos,
                )),
                KeyAction::MoveRight | KeyAction::SelectRight => Some(
                    ui::text::next_char_boundary(&buf, self.transcript_window.cpos),
                ),
                KeyAction::MoveStartOfLine | KeyAction::SelectStartOfLine => {
                    Some(ui::text::line_start(&buf, self.transcript_window.cpos))
                }
                KeyAction::MoveEndOfLine | KeyAction::SelectEndOfLine => {
                    Some(ui::text::line_end(&buf, self.transcript_window.cpos))
                }
                KeyAction::MoveWordForward | KeyAction::SelectWordForward => {
                    Some(ui::text::word_forward_pos(
                        &buf,
                        self.transcript_window.cpos,
                        ui::text::CharClass::Word,
                    ))
                }
                KeyAction::MoveWordBackward | KeyAction::SelectWordBackward => {
                    Some(ui::text::word_backward_pos(
                        &buf,
                        self.transcript_window.cpos,
                        ui::text::CharClass::Word,
                    ))
                }
                KeyAction::CopySelection => {
                    if let Some((s, e)) = self.transcript_window.selection_range(&rows) {
                        let s = ui::text::snap(&buf, s);
                        let e = ui::text::snap(&buf, e);
                        if s < e {
                            let copy = self.copy_display_range(s, e, self.settings.show_thinking);
                            let _ = crate::app::commands::copy_to_clipboard(&copy);
                        }
                    }
                    return EventOutcome::Redraw;
                }
                _ => None,
            };
            if let Some(new_cpos) = mv {
                self.transcript_window.cpos = new_cpos;
                self.snap_transcript_cursor();

                let rows = self.full_transcript_display_text(self.settings.show_thinking);
                let viewport = self.viewport_rows_estimate();
                self.transcript_window.resync(&rows, viewport);
                return EventOutcome::Redraw;
            }
        }
        match (k.code, k.modifiers) {
            (KeyCode::Char('q'), M::NONE) => EventOutcome::Quit,
            _ => EventOutcome::Noop,
        }
    }

    /// Move the content-pane cursor by `delta` lines. Delegates to
    /// `TranscriptWindow::scroll_by_lines`, which reuses vim `j`/`k` so
    /// vertical motion shares one code path (with `curswant`) across
    /// mouse wheel, Ctrl-U/D, arrows and j/k.
    pub(super) fn move_content_cursor_by_lines(&mut self, delta: isize) {
        let rows = self.full_transcript_display_text(self.settings.show_thinking);
        let viewport = self.viewport_rows_estimate();
        self.transcript_window
            .scroll_by_lines(delta, &rows, viewport);
        self.snap_transcript_cursor();
    }

    /// Build the transcript buffer, run `key` through the content-pane
    /// `Vim` instance, and mirror the resulting cursor / visual / yank
    /// state back onto our scroll + cursor. Returns `true` when vim
    /// consumed the key (caller should return `Redraw`).
    fn handle_content_vim_key(&mut self, k: KeyEvent) -> bool {
        let rows = self.full_transcript_display_text(self.settings.show_thinking);
        let viewport = self.viewport_rows_estimate();
        match self.transcript_window.handle_key(k, &rows, viewport) {
            None => false,
            Some(yanked) => {
                if let Some(raw) = yanked {
                    let copy = if let Some((s, e)) = self.transcript_window.kill_ring.source_range()
                    {
                        self.copy_display_range(s, e, self.settings.show_thinking)
                    } else {
                        raw
                    };
                    if crate::app::commands::copy_to_clipboard(&copy).is_ok() {
                        self.transcript_window
                            .kill_ring
                            .record_clipboard_write(copy);
                    }
                }
                self.snap_transcript_cursor();
                true
            }
        }
    }
}
