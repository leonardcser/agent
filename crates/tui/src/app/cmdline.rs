//! Cmdline open/close, Enter/Esc/Tab/Ctrl+C pre-interception before
//! compositor dispatch, Tab-completer cycling.

use super::*;
use crossterm::event::KeyEvent;

impl App {
    pub fn cmdline_is_focused(&self) -> bool {
        self.cmdline_win.is_some() && self.ui.focused_float() == self.cmdline_win
    }

    pub fn open_cmdline(&mut self) {
        if self.cmdline_win.is_some() {
            return;
        }
        let status_bg = crossterm::style::Color::AnsiValue(233);
        let style = ui::CmdlineStyle {
            background: ui::grid::Style {
                bg: Some(status_bg),
                fg: Some(crossterm::style::Color::White),
                ..Default::default()
            },
            text: ui::grid::Style {
                bg: Some(status_bg),
                fg: Some(crossterm::style::Color::White),
                ..Default::default()
            },
            cursor: ui::grid::Style {
                bg: Some(crossterm::style::Color::White),
                fg: Some(status_bg),
                ..Default::default()
            },
        };
        let cmdline = ui::Cmdline::new()
            .with_history(self.cmdline_history.clone())
            .with_style(style);
        // Take over the bottom row (same row as the status bar at
        // zindex 500). Higher zindex paints on top of it. `above_rows
        // = 0` so the rect is `term_h - 1`, not `term_h - 2` (which is
        // what the `dock_bottom_full_width` helper reserves).
        let config = ui::FloatConfig {
            border: ui::Border::None,
            placement: ui::Placement::DockBottom {
                above_rows: 0,
                full_width: true,
                max_width: ui::Constraint::Fill,
                max_height: ui::Constraint::Fixed(1),
            },
            zindex: 600,
            focusable: true,
            blocks_agent: false,
            ..Default::default()
        };
        if let Some(win_id) = self.ui.cmdline_open(config, cmdline) {
            self.cmdline_win = Some(win_id);
            self.cmdline_completer = None;
        }
    }

    fn close_cmdline(&mut self) {
        if let Some(win) = self.cmdline_win.take() {
            // Merge the component's history (which may have had a
            // successful submit appended on our behalf) back into the
            // app-level persistent list before the component is dropped.
            if let Some(c) = self.ui.cmdline(win) {
                self.cmdline_history = c.history().to_vec();
            }
            self.close_float(win);
        }
        self.cmdline_completer = None;
    }

    /// Pre-compositor hook for the focused cmdline float. Intercepts
    /// Enter (run the command, propagate Quit), Esc/Ctrl+C (dismiss),
    /// and Tab / Shift+Tab (drive the shared completer). Everything
    /// else returns `None` so the compositor routes the key to the
    /// `ui::Cmdline` component for text editing / history / cursor
    /// motion.
    ///
    /// Return value mirrors `dispatch_terminal_event`:
    /// - `None` → key wasn't ours; let compositor handle it.
    /// - `Some(true)` → we handled it AND it's a Quit. Propagate up.
    /// - `Some(false)` → we handled it; no quit.
    pub(super) fn cmdline_preintercept(&mut self, k: KeyEvent) -> Option<bool> {
        use crossterm::event::KeyModifiers as M;
        match (k.code, k.modifiers) {
            (KeyCode::Esc, _) | (KeyCode::Char('c'), M::CONTROL) => {
                self.close_cmdline();
                Some(false)
            }
            (KeyCode::Backspace, M::NONE) | (KeyCode::Char('w'), M::CONTROL) => {
                let empty = self
                    .cmdline_win
                    .and_then(|w| self.ui.cmdline(w))
                    .is_some_and(|c| c.text().is_empty());
                if empty {
                    self.close_cmdline();
                    Some(false)
                } else {
                    None
                }
            }
            (KeyCode::Enter, _) => {
                let line = self
                    .cmdline_win
                    .and_then(|w| self.ui.cmdline(w))
                    .map(|c| c.text().to_string())
                    .unwrap_or_default();
                // Persist into the shared history before close wipes
                // the component.
                if let Some(w) = self.cmdline_win {
                    if let Some(c) = self.ui.cmdline_mut(w) {
                        c.push_history(line.clone());
                    }
                }
                self.close_cmdline();
                if line.is_empty() {
                    return Some(false);
                }
                let action = super::commands::run_command(self, &format!(":{line}"));
                match action {
                    CommandAction::Quit => Some(true),
                    CommandAction::CancelAndClear => {
                        self.reset_session();
                        self.agent = None;
                        Some(false)
                    }
                    CommandAction::Compact { instructions } => {
                        if self.history.is_empty() {
                            self.notify_error("nothing to compact".into());
                        } else {
                            self.compact_history(instructions);
                        }
                        Some(false)
                    }
                    CommandAction::Exec(rx, kill) => {
                        self.exec_rx = Some(rx);
                        self.exec_kill = Some(kill);
                        Some(false)
                    }
                    CommandAction::Continue => Some(false),
                }
            }
            (KeyCode::Tab, _)
            | (KeyCode::Char('j'), M::CONTROL)
            | (KeyCode::Char('n'), M::CONTROL) => {
                self.cmdline_cycle_completer(true);
                Some(false)
            }
            (KeyCode::BackTab, _)
            | (KeyCode::Char('k'), M::CONTROL)
            | (KeyCode::Char('p'), M::CONTROL) => {
                self.cmdline_cycle_completer(false);
                Some(false)
            }
            _ => None,
        }
    }

    /// Build / advance the completer based on the cmdline's current
    /// text, then apply the selected label. No-op when the cmdline
    /// isn't open.
    fn cmdline_cycle_completer(&mut self, next: bool) {
        use crate::completer::{Completer, CompletionItem};
        let Some(win) = self.cmdline_win else {
            return;
        };
        let current = self
            .ui
            .cmdline(win)
            .map(|c| c.text().to_string())
            .unwrap_or_default();
        if self.cmdline_completer.is_none() {
            let mut comp = Completer::commands(0);
            let lua_cmds = self.lua.command_names();
            if !lua_cmds.is_empty() {
                let mut items: Vec<CompletionItem> = comp.all_items().to_vec();
                for name in lua_cmds {
                    if !items.iter().any(|i| i.label == name) {
                        items.push(CompletionItem {
                            label: name,
                            description: Some("(lua)".into()),
                            ..Default::default()
                        });
                    }
                }
                comp.refresh_items(items);
            }
            comp.update_query(current);
            self.cmdline_completer = Some(comp);
        } else if let Some(ref mut comp) = self.cmdline_completer {
            // Reversed picker style: `next` advances upward toward the
            // best match near the prompt.
            if next {
                comp.move_up();
            } else {
                comp.move_down();
            }
        }
        if let Some(ref comp) = self.cmdline_completer {
            if let Some(item) = comp.selected_item() {
                let label = item.label.clone();
                if let Some(c) = self.ui.cmdline_mut(win) {
                    c.set_text(label);
                }
            }
        }
    }
}
