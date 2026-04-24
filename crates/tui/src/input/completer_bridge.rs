//! Wiring between the input buffer and the `Completer` popup.
//!
//! Three completer kinds flow through here:
//!   * `Command`/`File`/`CommandArg` — inline completers driven by the
//!     buffer contents (`/cmd`, `@file`, `/cmd arg`).
//!   * `ArgPicker` — Lua-driven picker (theme, model, color, …). While
//!     open it *owns* the prompt: typed characters filter instead of
//!     spawning a new mode, Tab inserts the selection, Enter accepts +
//!     resolves the parked Lua task, Esc dismisses.
//!
//! Every kind shares the same ownership model: one `CompleterSession`
//! on `PromptState.completer`, cleaned up deterministically on close.

use super::{cursor_in_at_zone, find_slash_anchor, Action, ArgPickerEvent, PromptState};
use crate::completer::{Completer, CompleterKind};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

impl PromptState {
    /// Try to handle the event as a completer navigation. Returns Some if consumed.
    pub(super) fn handle_completer_event(&mut self, ev: &Event) -> Option<Action> {
        let kind = self.completer.as_ref().map(|c| c.kind)?;
        let is_arg_picker = kind == CompleterKind::ArgPicker;

        match ev {
            Event::Key(KeyEvent {
                code: KeyCode::Enter,
                modifiers,
                ..
            }) if !modifiers.contains(KeyModifiers::SHIFT) => {
                if is_arg_picker {
                    return Some(self.resolve_arg_picker("enter"));
                }
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
                // close_completer handles both inline completers and
                // ArgPicker cleanup (pushes a Dismiss event for the
                // latter so the parked Lua task resumes with `nil`).
                self.close_completer();
                Some(Action::Redraw)
            }
            // ArgPicker owns input unconditionally: Up/Down always
            // cycle even when there's one result (so the hint is
            // consistent and never falls through to cursor motion).
            //
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
                if !is_arg_picker && comp.results.len() <= 1 {
                    return None;
                }
                // Completer pickers dock *above* the prompt and paint
                // reversed — logical index 0 (best match) sits on the
                // bottom visual row. Up moves toward higher indices
                // (worse matches, higher on screen).
                comp.move_down();
                self.fire_arg_preview();
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
                if !is_arg_picker && comp.results.len() <= 1 {
                    return None;
                }
                comp.move_up();
                self.fire_arg_preview();
                Some(Action::Redraw)
            }
            Event::Key(KeyEvent {
                code: KeyCode::Tab, ..
            }) => {
                if is_arg_picker {
                    return Some(self.resolve_arg_picker("tab"));
                }
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

    /// Fire a `Preview` ArgPicker event with the current selection,
    /// so the Lua `on_select` callback (if any) can live-preview.
    fn fire_arg_preview(&mut self) {
        let Some(session) = self.completer.as_ref() else {
            return;
        };
        let Some(handles) = session.arg_picker.as_ref() else {
            return;
        };
        let Some(k) = handles.on_select else {
            return;
        };
        let index = session.selected + 1; // 1-based for Lua
        self.pending_arg_events.push(ArgPickerEvent::Preview {
            callback_id: k.0,
            index,
        });
    }

    /// Tear down the active ArgPicker session with an accept action.
    /// `action` is `"tab"` (Rust also inserts the label into the
    /// buffer) or `"enter"` (caller runs the command via the resolved
    /// task value). Returns `Action::Redraw`.
    fn resolve_arg_picker(&mut self, action: &'static str) -> Action {
        let session = self.completer.take().unwrap();
        if let Some(win) = session.picker_win {
            self.pending_picker_close.push(win);
        }
        let comp = session.completer;
        let handles = session
            .arg_picker
            .expect("resolve_arg_picker called on non-ArgPicker session");

        // Grab the accepted label BEFORE we stop borrowing `comp`.
        let accepted_label = comp.accept().map(|s| s.to_string());

        if action == "tab" {
            if let Some(ref label) = accepted_label {
                let end = self.win.cpos;
                let start = comp.anchor;
                self.win.edit_buf.buf.replace_range(start..end, label);
                self.win.cpos = start + label.len();
            }
        }

        let index = comp.selected + 1; // 1-based
        let release_ids: Vec<u64> = handles
            .on_select
            .into_iter()
            .chain(handles.on_accept)
            .map(|k| k.0)
            .collect();
        self.pending_arg_events.push(ArgPickerEvent::Accept {
            task_id: handles.task_id,
            index,
            action,
            release_ids,
        });
        Action::Redraw
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
        // An ArgPicker owns the prompt — never replace it from
        // buffer-driven re-sync. Only explicit user action
        // (Enter/Tab/Esc) or `close_completer` can end the session.
        if self
            .completer
            .as_ref()
            .is_some_and(|c| c.kind == CompleterKind::ArgPicker)
        {
            let query = self.win.edit_buf.buf.clone();
            self.completer.as_mut().unwrap().update_query(query);
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
        // ArgPicker reverse-filter: the entire buffer is the query.
        // Typed `/`, `!`, `:`, whatever — all flow into filtering, not
        // into spawning new modes. This mirrors main's behaviour for
        // Theme/Model/Color/Settings kinds.
        if self
            .completer
            .as_ref()
            .is_some_and(|c| c.kind == CompleterKind::ArgPicker)
        {
            let query = self.win.edit_buf.buf.clone();
            self.completer.as_mut().unwrap().update_query(query);
            self.fire_arg_preview();
            return;
        }
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
