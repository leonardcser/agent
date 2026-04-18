//! Wiring between the input buffer and the `Completer` picker.
//!
//! The completer pops up over the input for command/file/picker-style flows
//! (`/`, `@`, `/model`, `/theme`, `/color`, settings, history). This file
//! owns:
//!   * event interception (`handle_completer_event`) — navigation, accept, Esc
//!   * re-computing the completer as the buffer changes (`recompute_completer`)
//!   * writing accepted completions back into the buffer (`accept_completion`)
//!   * settings toggle and picker acceptance logic
//!
//! All functions are methods on `InputState` (separate `impl` block).

use super::{cursor_in_at_zone, find_slash_anchor, Action, InputState, MenuResult, SettingsState};
use crate::completer::{Completer, CompleterKind};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

impl InputState {
    /// Accept the selected item from a Model/Theme/Color picker, clear the buffer,
    /// apply side effects, and return the appropriate action.
    pub(super) fn accept_picker(&mut self, comp: Completer) -> Action {
        self.history_saved_buf = None;
        self.buffer.buf.clear();
        self.buffer.cpos = 0;
        match comp.kind {
            CompleterKind::Model => match comp.accept_extra().map(|s| s.to_string()) {
                Some(k) => Action::MenuResult(MenuResult::ModelSelect(k)),
                None => Action::Redraw,
            },
            CompleterKind::Theme => match comp.selected_item().and_then(|i| i.ansi_color) {
                Some(v) => {
                    crate::theme::set_accent(v);
                    Action::MenuResult(MenuResult::ThemeSelect(v))
                }
                None => Action::Redraw,
            },
            CompleterKind::Color => match comp.selected_item().and_then(|i| i.ansi_color) {
                Some(v) => {
                    crate::theme::set_slug_color(v);
                    Action::MenuResult(MenuResult::ColorSelect(v))
                }
                None => Action::Redraw,
            },
            _ => Action::Redraw,
        }
    }

    pub(super) fn toggle_selected_setting(&self, comp: &Completer) -> Action {
        let Some(key) = comp.accept_extra() else {
            return Action::Redraw;
        };
        let s = |k: &str| Self::self_setting_bool(comp, k);
        let mut state = SettingsState {
            vim: s("vim"),
            auto_compact: s("auto_compact"),
            show_tps: s("show_tps"),
            show_tokens: s("show_tokens"),
            show_cost: s("show_cost"),
            show_prediction: s("show_prediction"),
            show_slug: s("show_slug"),
            show_thinking: s("show_thinking"),
            restrict_to_workspace: s("restrict_to_workspace"),
            redact_secrets: s("redact_secrets"),
        };
        match key {
            "vim" => state.vim ^= true,
            "auto_compact" => state.auto_compact ^= true,
            "show_tps" => state.show_tps ^= true,
            "show_tokens" => state.show_tokens ^= true,
            "show_cost" => state.show_cost ^= true,
            "show_prediction" => state.show_prediction ^= true,
            "show_slug" => state.show_slug ^= true,
            "show_thinking" => state.show_thinking ^= true,
            "restrict_to_workspace" => state.restrict_to_workspace ^= true,
            "redact_secrets" => state.redact_secrets ^= true,
            _ => return Action::Redraw,
        }
        Action::MenuResult(MenuResult::Settings(state))
    }

    /// Try to handle the event as a completer navigation. Returns Some if consumed.
    pub(super) fn handle_completer_event(&mut self, ev: &Event) -> Option<Action> {
        let kind = self.completer.as_ref().map(|c| c.kind)?;
        let is_picker = matches!(
            kind,
            CompleterKind::History
                | CompleterKind::Model
                | CompleterKind::Theme
                | CompleterKind::Color
                | CompleterKind::Settings
        );

        match ev {
            Event::Key(KeyEvent {
                code: KeyCode::Enter,
                modifiers,
                ..
            }) if !modifiers.contains(KeyModifiers::SHIFT) => {
                if kind == CompleterKind::Settings {
                    let comp = self.completer.as_ref().unwrap();
                    return Some(self.toggle_selected_setting(comp));
                }
                let comp = self.completer.take().unwrap();
                match comp.kind {
                    CompleterKind::History => {
                        if let Some(label) = comp.accept() {
                            self.buffer.buf = label.to_string();
                            self.buffer.cpos = self.buffer.buf.len();
                        }
                        self.history_saved_buf = None;
                        Some(Action::Redraw)
                    }
                    CompleterKind::Model | CompleterKind::Theme | CompleterKind::Color => {
                        Some(self.accept_picker(comp))
                    }
                    _ => {
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
                }
            }
            Event::Key(KeyEvent {
                code: KeyCode::Char(' '),
                modifiers: KeyModifiers::NONE,
                ..
            }) if kind == CompleterKind::Settings => {
                let comp = self.completer.as_ref().unwrap();
                Some(self.toggle_selected_setting(comp))
            }
            Event::Key(KeyEvent {
                code: KeyCode::Esc, ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }) => {
                let comp = self.completer.take().unwrap();
                // Restore original theme/color on dismiss.
                match comp.kind {
                    CompleterKind::Theme => {
                        if let Some(orig) = comp.original_value {
                            crate::theme::set_accent(orig);
                        }
                    }
                    CompleterKind::Color => {
                        if let Some(orig) = comp.original_value {
                            crate::theme::set_slug_color(orig);
                        }
                    }
                    _ => {}
                }
                // Restore saved buffer for all picker types.
                if is_picker {
                    if let Some((buf, cpos)) = self.history_saved_buf.take() {
                        self.buffer.buf = buf;
                        self.buffer.cpos = cpos;
                    }
                }
                Some(Action::Redraw)
            }
            // Ctrl+R cycles forward through history matches.
            Event::Key(KeyEvent {
                code: KeyCode::Char('r'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }) if kind == CompleterKind::History => {
                let comp = self.completer.as_mut().unwrap();
                comp.move_down();
                Some(Action::Redraw)
            }
            Event::Key(KeyEvent {
                code: KeyCode::Up, ..
            })
            | Event::Key(KeyEvent {
                code: KeyCode::Char('k' | 'p'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }) => {
                let comp = self.completer.as_mut().unwrap();
                if comp.results.len() <= 1 && !is_picker {
                    return None;
                }
                comp.move_up();
                self.live_preview_picker();
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
                if comp.results.len() <= 1 && !is_picker {
                    return None;
                }
                comp.move_down();
                self.live_preview_picker();
                Some(Action::Redraw)
            }
            Event::Key(KeyEvent {
                code: KeyCode::Tab, ..
            }) => {
                let comp = self.completer.take().unwrap();
                match comp.kind {
                    CompleterKind::History => {
                        if let Some(label) = comp.accept() {
                            self.buffer.buf = label.to_string();
                            self.buffer.cpos = self.buffer.buf.len();
                        }
                        self.history_saved_buf = None;
                    }
                    CompleterKind::Settings => {
                        // Put it back — settings toggle doesn't close the picker.
                        self.completer = Some(comp);
                        let c = self.completer.as_ref().unwrap();
                        return Some(self.toggle_selected_setting(c));
                    }
                    CompleterKind::Model | CompleterKind::Theme | CompleterKind::Color => {
                        return Some(self.accept_picker(comp));
                    }
                    _ => {
                        let was_command = comp.kind == CompleterKind::Command;
                        self.accept_completion(&comp);
                        if was_command {
                            self.sync_completer();
                        }
                    }
                }
                Some(Action::Redraw)
            }
            _ => None,
        }
    }

    fn self_setting_bool(comp: &Completer, key: &str) -> bool {
        comp.all_items()
            .iter()
            .find(|item| item.extra.as_deref() == Some(key))
            .is_some_and(|item| item.description.as_deref() == Some("on"))
    }

    fn accept_completion(&mut self, comp: &Completer) {
        if let Some(label) = comp.accept() {
            let end = self.buffer.cpos;
            let start = comp.anchor;
            if comp.kind == CompleterKind::CommandArg {
                // Replace just the argument portion after the command prefix.
                self.buffer.buf.replace_range(start..end, label);
                self.buffer.cpos = start + label.len();
            } else {
                let trigger = &self.buffer.buf[start..start + 1];
                let replacement = if trigger == "/" {
                    format!("/{} ", label)
                } else if label.contains(' ') {
                    format!("@\"{}\" ", label)
                } else {
                    format!("@{} ", label)
                };
                self.buffer.buf.replace_range(start..end, &replacement);
                self.buffer.cpos = start + replacement.len();
            }
        }
    }

    /// Activate completer if the buffer looks like a command or file ref.
    pub(super) fn sync_completer(&mut self) {
        if let Some((src_idx, arg_anchor)) = self.find_command_arg_zone() {
            let items = self.command_arg_sources[src_idx].1.clone();
            let query = self.arg_query(arg_anchor);
            self.set_or_update_completer(
                CompleterKind::CommandArg,
                || Completer::command_args(arg_anchor, &items),
                query,
            );
        } else if find_slash_anchor(&self.buffer.buf, self.buffer.cpos).is_some() {
            let query = self.buffer.buf[1..self.buffer.cpos].to_string();
            self.set_or_update_completer(CompleterKind::Command, || Completer::commands(0), query);
        } else {
            self.completer = None;
        }
    }

    /// Live-preview theme/color while navigating in a picker completer.
    pub(super) fn live_preview_picker(&self) {
        if let Some(comp) = &self.completer {
            if let Some(item) = comp.results.get(comp.selected) {
                match comp.kind {
                    CompleterKind::Theme => {
                        if let Some(c) = item.ansi_color {
                            crate::theme::set_accent(c);
                        }
                    }
                    CompleterKind::Color => {
                        if let Some(c) = item.ansi_color {
                            crate::theme::set_slug_color(c);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    /// Recompute the completer based on where the cursor currently sits.
    /// Shows the file or command picker if the cursor is inside an @/slash zone,
    /// hides it otherwise. Never touches a history/picker completer.
    pub(super) fn recompute_completer(&mut self) {
        if self.completer.as_ref().is_some_and(|c| {
            matches!(
                c.kind,
                CompleterKind::History
                    | CompleterKind::Model
                    | CompleterKind::Theme
                    | CompleterKind::Color
                    | CompleterKind::Settings
            )
        }) {
            let query = self.buffer.buf.clone();
            self.completer.as_mut().unwrap().update_query(query);
            self.live_preview_picker();
            return;
        }
        if let Some(at_pos) = cursor_in_at_zone(&self.buffer.buf, self.buffer.cpos) {
            let query = if self.buffer.cpos > at_pos + 1 {
                self.buffer.buf[at_pos + 1..self.buffer.cpos].to_string()
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
                self.completer = Some(comp);
            }
        } else if let Some((src_idx, arg_anchor)) = self.find_command_arg_zone() {
            let items = self.command_arg_sources[src_idx].1.clone();
            let query = self.arg_query(arg_anchor);
            self.set_or_update_completer(
                CompleterKind::CommandArg,
                || Completer::command_args(arg_anchor, &items),
                query,
            );
        } else if find_slash_anchor(&self.buffer.buf, self.buffer.cpos).is_some()
            || (self.buffer.cpos == 0 && self.buffer.buf.starts_with('/'))
        {
            let end = self.buffer.cpos.max(1);
            let query = self.buffer.buf[1..end].to_string();
            self.set_or_update_completer(CompleterKind::Command, || Completer::commands(0), query);
        } else {
            self.completer = None;
        }
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
            self.completer = Some(comp);
        }
    }

    fn arg_query(&self, anchor: usize) -> String {
        if self.buffer.cpos > anchor {
            self.buffer.buf[anchor..self.buffer.cpos].to_string()
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
            if self.buffer.buf.len() >= anchor
                && self.buffer.buf.starts_with(cmd.as_str())
                && self.buffer.buf.as_bytes()[cmd.len()] == b' '
                && self.buffer.cpos >= anchor
            {
                return Some((i, anchor));
            }
        }
        None
    }
}
