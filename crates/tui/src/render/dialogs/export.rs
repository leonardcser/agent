use crate::keymap::{hints, nav_lookup, NavAction};
use crate::render::draw_bar;
use crate::theme;
use crossterm::event::{KeyCode, KeyModifiers};

use super::{end_dialog_draw, DialogResult, ListState, RenderOut};

#[derive(Clone, Copy, Debug)]
pub enum ExportTarget {
    Clipboard,
    File,
}

const OPTIONS: &[(ExportTarget, &str, &str)] = &[
    (
        ExportTarget::Clipboard,
        "Copy to clipboard",
        "markdown of the conversation",
    ),
    (
        ExportTarget::File,
        "Write to file",
        "save in the current working directory",
    ),
];

pub struct ExportDialog {
    list: ListState,
}

impl ExportDialog {
    pub fn new() -> Self {
        Self {
            list: ListState::new(OPTIONS.len()),
        }
    }

    fn selected_target(&self) -> ExportTarget {
        OPTIONS[self.list.selected].0
    }
}

impl Default for ExportDialog {
    fn default() -> Self {
        Self::new()
    }
}

impl super::Dialog for ExportDialog {
    fn height(&self) -> u16 {
        // bar + header + options + blank + hint
        self.list.height(OPTIONS.len(), 4)
    }

    fn mark_dirty(&mut self) {
        self.list.dirty = true;
    }

    fn handle_resize(&mut self) {
        self.list.handle_resize();
    }

    fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> Option<DialogResult> {
        let n = OPTIONS.len();
        // Digit shortcut: 1/2 picks and confirms immediately.
        if let KeyCode::Char(c) = code {
            if mods.is_empty() || mods == KeyModifiers::SHIFT {
                if let Some(d) = c.to_digit(10) {
                    let idx = d as usize;
                    if idx >= 1 && idx <= n {
                        return Some(DialogResult::Export {
                            target: Some(OPTIONS[idx - 1].0),
                        });
                    }
                }
            }
        }

        match nav_lookup(code, mods) {
            Some(NavAction::Confirm) => Some(DialogResult::Export {
                target: Some(self.selected_target()),
            }),
            Some(NavAction::Dismiss) => Some(DialogResult::Export { target: None }),
            Some(nav) => {
                self.list.handle_nav(nav, n);
                None
            }
            None => None,
        }
    }

    fn draw(&mut self, out: &mut RenderOut, start_row: u16, width: u16, granted_rows: u16) {
        let n = OPTIONS.len();
        let Some(w) = self
            .list
            .begin_draw(out, start_row, n, width, granted_rows, 4)
        else {
            return;
        };

        draw_bar(out, w, None, None, theme::accent());
        out.newline();

        out.push_dim();
        out.print(" Export:");
        out.pop_style();
        out.newline();

        for (i, (_, label, desc)) in OPTIONS.iter().enumerate() {
            let highlighted = i == self.list.selected;
            out.print("  ");
            out.push_dim();
            out.print(&format!("{}.", i + 1));
            out.pop_style();
            out.print(" ");
            if highlighted {
                out.push_fg(theme::accent());
                out.print(label);
                out.pop_style();
                let used = 2 + 2 + 1 + label.chars().count() + 2;
                let remaining = w.saturating_sub(used);
                if remaining > 3 {
                    let d: String = desc.chars().take(remaining).collect();
                    out.push_dim();
                    out.print(&format!("  {d}"));
                    out.pop_style();
                }
            } else {
                out.print(label);
            }
            out.newline();
        }

        out.newline();
        out.push_dim();
        out.print(&hints::join(&[hints::SELECT, hints::CANCEL]));
        out.pop_style();
        end_dialog_draw(out);
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
