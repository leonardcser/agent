//! Wiring between the input buffer and the `Completer` popup.
//!
//! Inline completers (`Command`/`File`/`CommandArg`) are driven by the
//! buffer contents (`/cmd`, `@file`, `/cmd arg`). Each owns a single
//! `CompleterSession` on `PromptState.completer`, cleaned up
//! deterministically on close.

use super::{cursor_in_at_zone, find_slash_anchor, Action, PromptState};
use crate::completer::{Completer, CompleterKind};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

impl PromptState {
    /// Try to handle the event as a completer navigation. Returns Some if consumed.
    pub(super) fn handle_completer_event(&mut self, ev: &Event) -> Option<Action> {
        let _kind = self.completer.as_ref().map(|c| c.kind)?;

        match ev {
            Event::Key(KeyEvent {
                code: KeyCode::Enter,
                modifiers,
                ..
            }) if !modifiers.contains(KeyModifiers::SHIFT) => {
                let session = self.completer.take().unwrap();
                if let Some(win) = session.picker_win {
                    self.pending_picker_close.push(win);
                }
                let comp = session.completer;
                let kind = comp.kind;
                self.accept_completion(&comp);
                if kind == CompleterKind::Command {
                    let display = self.message_display_text();
                    let content = self.build_content();
                    self.clear();
                    Some(Action::Submit { content, display })
                } else {
                    Some(Action::Redraw)
                }
            }
            Event::Key(KeyEvent {
                code: KeyCode::Esc, ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }) => {
                self.close_completer();
                Some(Action::Redraw)
            }
            // Inline completers only cycle when the list has multiple
            // entries — a single-option match falls through to normal
            // arrow-key behaviour (cursor navigation in the prompt).
            Event::Key(KeyEvent {
                code: KeyCode::Up, ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Char('k' | 'p'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }) => {
                let comp = self.completer.as_mut().unwrap();
                if comp.results.len() <= 1 {
                    return None;
                }
                // Completer pickers dock *above* the prompt and paint
                // reversed — logical index 0 (best match) sits on the
                // bottom visual row. Up moves toward higher indices
                // (worse matches, higher on screen).
                comp.move_down();
                Some(Action::Redraw)
            }
            Event::Key(KeyEvent {
                code: KeyCode::Down,
                ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Char('j' | 'n'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }) => {
                let comp = self.completer.as_mut().unwrap();
                if comp.results.len() <= 1 {
                    return None;
                }
                comp.move_up();
                Some(Action::Redraw)
            }
            Event::Key(KeyEvent {
                code: KeyCode::Tab, ..
            }) => {
                let session = self.completer.take().unwrap();
                let picker_win = session.picker_win;
                let comp = session.completer;
                let was_command = comp.kind == CompleterKind::Command;
                self.accept_completion(&comp);
                if was_command {
                    // `accept_completion` wrote `/theme ` (trailing
                    // space). Re-sync so the CommandArg picker takes
                    // over — if the command declared `args`, we land
                    // straight in its args picker.
                    self.sync_completer();
                }
                if let Some(win) = picker_win {
                    self.pending_picker_close.push(win);
                }
                Some(Action::Redraw)
            }
            _ => None,
        }
    }

    fn accept_completion(&mut self, comp: &Completer) {
        if let Some(label) = comp.accept() {
            let end = self.win.cpos;
            let start = comp.anchor;
            if comp.kind == CompleterKind::CommandArg {
                // Replace just the argument portion after the command prefix.
                self.win.edit_buf.buf.replace_range(start..end, label);
                self.win.cpos = start + label.len();
            } else {
                let trigger = &self.win.edit_buf.buf[start..start + 1];
                let replacement = if trigger == "/" {
                    format!("/{} ", label)
                } else if label.contains(' ') {
                    format!("@\"{}\" ", label)
                } else {
                    format!("@{} ", label)
                };
                self.win
                    .edit_buf
                    .buf
                    .replace_range(start..end, &replacement);
                self.win.cpos = start + replacement.len();
            }
        }
    }

    /// Activate completer if the buffer looks like a command or file ref.
    pub(super) fn sync_completer(&mut self) {
        // Slash commands are single-line by design — once the user has
        // broken into multiple lines, hide the command picker.
        let single_line = !self.win.edit_buf.buf.contains('\n');
        if single_line {
            if let Some((src_idx, arg_anchor)) = self.find_command_arg_zone() {
                let items = self.command_arg_sources[src_idx].1.clone();
                let query = self.arg_query(arg_anchor);
                self.set_or_update_completer(
                    CompleterKind::CommandArg,
                    || Completer::command_args(arg_anchor, &items),
                    query,
                );
                return;
            }
            if find_slash_anchor(&self.win.edit_buf.buf, self.win.cpos).is_some() {
                let query = self.win.edit_buf.buf[1..self.win.cpos].to_string();
                self.set_or_update_completer(
                    CompleterKind::Command,
                    || Completer::commands(0),
                    query,
                );
                return;
            }
        }
        self.close_completer();
    }

    /// Recompute the completer based on where the cursor currently sits.
    /// Shows the file or command picker if the cursor is inside an @/slash
    /// zone, hides it otherwise.
    pub(super) fn recompute_completer(&mut self) {
        if let Some(at_pos) = cursor_in_at_zone(&self.win.edit_buf.buf, self.win.cpos) {
            let query = if self.win.cpos > at_pos + 1 {
                self.win.edit_buf.buf[at_pos + 1..self.win.cpos].to_string()
            } else {
                String::new()
            };
            if self
                .completer
                .as_ref()
                .is_some_and(|c| c.kind == CompleterKind::File && c.anchor == at_pos)
            {
                self.completer.as_mut().unwrap().update_query(query);
            } else {
                let mut comp = Completer::files(at_pos);
                comp.update_query(query);
                self.set_completer(comp);
            }
            return;
        }
        // Slash commands are single-line by design — once the user has
        // broken into multiple lines, hide the command picker.
        let single_line = !self.win.edit_buf.buf.contains('\n');
        if single_line {
            if let Some((src_idx, arg_anchor)) = self.find_command_arg_zone() {
                let items = self.command_arg_sources[src_idx].1.clone();
                let query = self.arg_query(arg_anchor);
                self.set_or_update_completer(
                    CompleterKind::CommandArg,
                    || Completer::command_args(arg_anchor, &items),
                    query,
                );
                return;
            }
            if find_slash_anchor(&self.win.edit_buf.buf, self.win.cpos).is_some()
                || (self.win.cpos == 0 && self.win.edit_buf.buf.starts_with('/'))
            {
                let end = self.win.cpos.max(1);
                let query = self.win.edit_buf.buf[1..end].to_string();
                self.set_or_update_completer(
                    CompleterKind::Command,
                    || Completer::commands(0),
                    query,
                );
                return;
            }
        }
        self.close_completer();
    }

    /// Reuse the current completer if it matches `kind`, otherwise create a new
    /// one via `make`. Either way, update the query.
    fn set_or_update_completer(
        &mut self,
        kind: CompleterKind,
        make: impl FnOnce() -> Completer,
        query: String,
    ) {
        if self.completer.as_ref().is_some_and(|c| c.kind == kind) {
            self.completer.as_mut().unwrap().update_query(query);
        } else {
            let mut comp = make();
            comp.update_query(query);
            self.set_completer(comp);
        }
    }

    fn arg_query(&self, anchor: usize) -> String {
        if self.win.cpos > anchor {
            self.win.edit_buf.buf[anchor..self.win.cpos].to_string()
        } else {
            String::new()
        }
    }

    /// Check if the cursor is inside a command argument zone (e.g. `/model foo`).
    /// Returns `(source_index, arg_anchor)` where source_index indexes into
    /// `command_arg_sources` and arg_anchor is the byte offset after the space.
    fn find_command_arg_zone(&self) -> Option<(usize, usize)> {
        for (i, (cmd, _)) in self.command_arg_sources.iter().enumerate() {
            let anchor = cmd.len() + 1; // "/cmd" + space
            if self.win.edit_buf.buf.len() >= anchor
                && self.win.edit_buf.buf.starts_with(cmd.as_str())
                && self.win.edit_buf.buf.as_bytes()[cmd.len()] == b' '
                && self.win.cpos >= anchor
            {
                return Some((i, anchor));
            }
        }
        None
    }
}
