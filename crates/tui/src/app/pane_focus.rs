//! Shared `Ctrl-W` pane navigation and resizing chords.

use crate::app::{EventOutcome, TuiApp};
use crossterm::event::{Event, KeyCode};
use std::time::Duration;

/// Max inter-key gap between `Ctrl-W` and its follow-up key.
const PANE_CHORD_WINDOW: Duration = Duration::from_millis(750);

impl TuiApp {
    /// Focus a window and keep the logical app pane in sync when the target is
    /// one of the two primary panes.
    pub(crate) fn focus_window(&mut self, win: crate::smelt_edit::WinId) -> bool {
        if !self.ui.set_focus(win) {
            return false;
        }
        match win {
            crate::app::PROMPT_WIN => self.app_focus = crate::app::AppFocus::Prompt,
            crate::app::TRANSCRIPT_WIN => self.app_focus = crate::app::AppFocus::Content,
            _ => {}
        }
        true
    }

    pub(crate) fn handle_pane_chord(&mut self, ev: &Event) -> Option<EventOutcome> {
        use crossterm::event::KeyModifiers as M;
        let Event::Key(k) = ev else { return None };

        let now = self.core.clock.instant_now();
        if let Some(started) = self.timers.pending_pane_chord {
            if now.duration_since(started) < PANE_CHORD_WINDOW {
                self.timers.pending_pane_chord = None;
                if matches!(k.modifiers, M::NONE | M::SHIFT) {
                    use crate::smelt_edit::layout::Axis;
                    let resize = match k.code {
                        KeyCode::Char('<') => Some((Axis::Horizontal, -4)),
                        KeyCode::Char('>') => Some((Axis::Horizontal, 4)),
                        KeyCode::Char('-') => Some((Axis::Vertical, -1)),
                        KeyCode::Char('+') => Some((Axis::Vertical, 1)),
                        _ => None,
                    };
                    if let Some(win) = self.ui.focus() {
                        if let Some((axis, delta)) = resize {
                            self.ui.resize_window(win, axis, delta);
                            self.refresh_main_layout();
                            return Some(EventOutcome::Redraw);
                        }
                        if k.code == KeyCode::Char('=') {
                            self.ui.equalize_window(win);
                            return Some(EventOutcome::Redraw);
                        }
                    }
                }
                let navigated = matches!(
                    (k.code, k.modifiers),
                    (KeyCode::Char('w'), _) | (KeyCode::Char('j' | 'k' | 'h' | 'l' | 'p'), M::NONE)
                );
                if navigated {
                    self.toggle_pane_focus();
                    return Some(EventOutcome::Redraw);
                }
                return None;
            }
            self.timers.pending_pane_chord = None;
        }

        if k.code == KeyCode::Char('w') && k.modifiers.contains(M::CONTROL) {
            self.timers.pending_pane_chord = Some(now);
            return Some(EventOutcome::Noop);
        }
        None
    }

    pub(crate) fn toggle_pane_focus(&mut self) {
        let target = match self.app_focus {
            crate::app::AppFocus::Prompt => crate::app::AppFocus::Content,
            crate::app::AppFocus::Content => crate::app::AppFocus::Prompt,
        };
        if target == crate::app::AppFocus::Content && !self.has_transcript_content() {
            return;
        }
        self.app_focus = target;
        if self.app_focus == crate::app::AppFocus::Content {
            self.refocus_content();
        }
    }

    /// Warm up the content pane on focus switch: clamp cpos and sync cursor state.
    /// Without this, a resumed session has stale state and the first key is a no-op.
    fn refocus_content(&mut self) {
        let viewport = self.viewport_rows_estimate();
        let win_id = self.well_known.transcript;
        let buf_id = self.transcript_win().buf;
        let (win, buf) = self.ui.win_and_buf_mut(win_id, buf_id);
        win.expect("transcript window")
            .refocus(buf.expect("transcript buffer"), viewport);
        self.snap_transcript_cursor();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::AppFocus;
    use crossterm::event::{KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    #[test]
    fn ctrl_w_starts_pane_chord_without_toggling_focus() {
        let mut app = crate::app::test_harness::TestApp::builder().build().app;

        let outcome = app.handle_pane_chord(&key(KeyCode::Char('w'), KeyModifiers::CONTROL));

        assert!(matches!(outcome, Some(EventOutcome::Noop)));
        assert_eq!(app.app_focus, AppFocus::Prompt);
        assert!(app.timers.pending_pane_chord.is_some());
    }

    #[test]
    fn pane_chord_toggles_to_content_only_when_transcript_has_content() {
        let mut app = crate::app::test_harness::TestApp::builder().build().app;
        app.handle_pane_chord(&key(KeyCode::Char('w'), KeyModifiers::CONTROL));
        let outcome = app.handle_pane_chord(&key(KeyCode::Char('j'), KeyModifiers::NONE));
        assert!(matches!(outcome, Some(EventOutcome::Redraw)));
        assert_eq!(app.app_focus, AppFocus::Prompt);

        app.push_block(smelt_core::Block::Text {
            content: "content".into(),
        });
        app.handle_pane_chord(&key(KeyCode::Char('w'), KeyModifiers::CONTROL));
        let outcome = app.handle_pane_chord(&key(KeyCode::Char('j'), KeyModifiers::NONE));
        assert!(matches!(outcome, Some(EventOutcome::Redraw)));
        assert_eq!(app.app_focus, AppFocus::Content);
        assert!(app.timers.pending_pane_chord.is_none());
    }

    #[test]
    fn non_navigation_key_clears_pending_pane_chord() {
        let mut app = crate::app::test_harness::TestApp::builder().build().app;
        app.handle_pane_chord(&key(KeyCode::Char('w'), KeyModifiers::CONTROL));

        let outcome = app.handle_pane_chord(&key(KeyCode::Char('x'), KeyModifiers::NONE));

        assert!(outcome.is_none());
        assert_eq!(app.app_focus, AppFocus::Prompt);
        assert!(app.timers.pending_pane_chord.is_none());
    }
}
