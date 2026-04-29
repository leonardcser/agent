//! Pane focus + block-scoped key dispatch. `Ctrl-W` chord toggles
//! focus between prompt and transcript; when a block is focused,
//! `dispatch_block_key` gets first crack at a key before buffer /
//! window keymaps (nvim-style layering).

use super::*;
use crossterm::event::Event;
use std::time::Duration;

/// Max inter-key gap between `Ctrl-W` and its follow-up key.
const PANE_CHORD_WINDOW: Duration = Duration::from_millis(750);

impl App {
    pub(super) fn handle_pane_chord(&mut self, ev: &Event, t: &mut Timers) -> Option<EventOutcome> {
        use crossterm::event::KeyModifiers as M;
        let Event::Key(k) = ev else { return None };

        // In-flight chord: consume the follow-up key.
        if let Some(started) = t.pending_pane_chord {
            if started.elapsed() < PANE_CHORD_WINDOW {
                let navigated = matches!(
                    (k.code, k.modifiers),
                    (KeyCode::Char('w'), _) | (KeyCode::Char('j' | 'k' | 'h' | 'l' | 'p'), M::NONE)
                );
                t.pending_pane_chord = None;
                if navigated {
                    self.toggle_pane_focus();
                    return Some(EventOutcome::Redraw);
                }
                // Non-navigation follow-up — fall through so the key is
                // processed normally.
                return None;
            }
            t.pending_pane_chord = None;
        }

        // Prime the chord.
        if k.code == KeyCode::Char('w') && k.modifiers.contains(M::CONTROL) {
            t.pending_pane_chord = Some(Instant::now());
            return Some(EventOutcome::Noop);
        }
        None
    }

    fn toggle_pane_focus(&mut self) {
        let target = match self.app_focus {
            crate::app::AppFocus::Prompt => crate::app::AppFocus::Content,
            crate::app::AppFocus::Content => crate::app::AppFocus::Prompt,
        };
        if target == crate::app::AppFocus::Content
            && !self.has_transcript_content(self.settings.show_thinking)
        {
            return;
        }
        self.app_focus = target;
        if self.app_focus == crate::app::AppFocus::Content {
            self.refocus_content();
        }
    }

    /// Warm up the content pane on focus switch: mount the transcript,
    /// clamp cpos into range, sync cursor line/col. Without this, a
    /// resumed session has stale/zero state and the first key press
    /// is a no-op until the user triggers a click-to-position.
    fn refocus_content(&mut self) {
        let rows = self.full_transcript_display_text(self.settings.show_thinking);
        let viewport = self.viewport_rows_estimate();
        self.transcript_window.refocus(&rows, viewport);
        self.snap_transcript_cursor();
    }

    /// Determine which block the content cursor is currently on, if any.
    /// Derives the absolute row from `cpos` (byte offset in the display
    /// buffer), then looks up the snapshot's `block_of_row`.
    fn focused_block_id(&mut self) -> Option<BlockId> {
        let tw = self.transcript_width() as u16;
        let snap = self.transcript.snapshot(tw, self.settings.show_thinking);
        if snap.rows.is_empty() {
            return None;
        }
        let row = self.transcript_window.cursor_abs_row();
        snap.block_of_row.get(row).copied().flatten()
    }

    /// Try to handle a key as a block-scoped binding. Returns `Some` if
    /// the key was consumed, `None` to fall through to buffer/window
    /// keymaps.
    pub(super) fn dispatch_block_key(&mut self, k: KeyEvent) -> Option<EventOutcome> {
        use crossterm::event::KeyModifiers as M;
        if k.modifiers != M::NONE {
            return None;
        }
        let block_id = self.focused_block_id()?;
        let is_tool = matches!(
            self.transcript.block(block_id),
            Some(Block::ToolCall { .. })
        );
        if !is_tool {
            return None;
        }
        match k.code {
            KeyCode::Char('e') => {
                let vs = self.block_view_state(block_id);
                let next = match vs {
                    ViewState::Expanded => ViewState::Collapsed,
                    _ => ViewState::Expanded,
                };
                self.set_block_view_state(block_id, next);
                Some(EventOutcome::Redraw)
            }
            _ => None,
        }
    }
}
