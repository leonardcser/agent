use crate::keymap::{hints, nav_lookup, NavAction};
use crate::render::draw_bar;
use crate::theme;
use crossterm::event::{KeyCode, KeyModifiers};

use super::{end_dialog_draw, truncate_str, DialogResult, ListState, RenderOut};

pub struct RewindDialog {
    turns: Vec<(usize, String)>,
    list: ListState,
    restore_vim_insert: bool,
}

impl RewindDialog {
    pub fn new(turns: Vec<(usize, String)>, restore_vim_insert: bool) -> Self {
        // +1 for the "(current)" sentinel entry at the end.
        let total = turns.len() + 1;
        let mut list = ListState::new(total);
        list.selected = total.saturating_sub(1);
        list.scroll_offset = total.saturating_sub(list.max_visible);
        Self {
            turns,
            list,
            restore_vim_insert,
        }
    }

    fn total_items(&self) -> usize {
        self.turns.len() + 1
    }

    fn is_current_selected(&self) -> bool {
        self.list.selected == self.turns.len()
    }
}

impl super::Dialog for RewindDialog {
    fn height(&self) -> u16 {
        self.list.height(self.total_items(), 4)
    }

    fn constrain_height(&self) -> bool {
        true
    }

    fn mark_dirty(&mut self) {
        self.list.dirty = true;
    }

    fn handle_resize(&mut self) {
        self.list.handle_resize();
    }

    fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> Option<DialogResult> {
        let n = self.total_items();
        match nav_lookup(code, mods) {
            Some(NavAction::Confirm) => {
                if self.is_current_selected() {
                    // "(current)" — just dismiss, no rewind.
                    Some(DialogResult::Rewind {
                        block_idx: None,
                        restore_vim_insert: self.restore_vim_insert,
                    })
                } else {
                    Some(DialogResult::Rewind {
                        block_idx: Some(self.turns[self.list.selected].0),
                        restore_vim_insert: self.restore_vim_insert,
                    })
                }
            }
            Some(NavAction::Dismiss) => Some(DialogResult::Rewind {
                block_idx: None,
                restore_vim_insert: self.restore_vim_insert,
            }),
            Some(nav) => {
                self.list.handle_nav(nav, n);
                None
            }
            None => None,
        }
    }

    fn draw(&mut self, out: &mut RenderOut, start_row: u16, width: u16, granted_rows: u16) {
        let n = self.total_items();
        let Some(w) = self
            .list
            .begin_draw(out, start_row, n, width, granted_rows, 4)
        else {
            return;
        };

        draw_bar(out, w, None, None, theme::accent());
        out.newline();

        out.push_dim();
        out.print(" Rewind to:");
        out.pop_style();
        out.newline();

        let num_width = n.to_string().len();
        let range = self.list.visible_range(n);
        for i in range.start..range.end {
            let is_current = i == self.turns.len();
            let label = if is_current {
                "(current)"
            } else {
                self.turns[i].1.lines().next().unwrap_or("")
            };
            let num = i + 1;
            let pad = num_width + 4;
            let max_label = w.saturating_sub(pad);
            let truncated = truncate_str(label, max_label);
            out.push_dim();
            out.print(&format!("  {:>width$}.", num, width = num_width));
            out.pop_style();
            if i == self.list.selected {
                out.print(" ");
                out.push_fg(theme::accent());
                out.print(&truncated);
                out.pop_style();
            } else {
                out.print(" ");
                out.print(&truncated);
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
