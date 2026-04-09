use crate::keymap::{hints, nav_lookup, NavAction};
use crate::render::{crlf, draw_bar};
use crate::theme;
use crossterm::event::{KeyCode, KeyModifiers};
use crossterm::style::Print;
use crossterm::{terminal, QueueableCommand};

use super::{end_dialog_draw, truncate_str, DialogResult, ListState, RenderOut};

/// Chrome rows around the item list: bar + "Rewind to:" header +
/// blank separator + hints footer.
const LIST_OVERHEAD: u16 = 4;

pub struct RewindDialog {
    turns: Vec<(usize, String)>,
    list: ListState,
    restore_vim_insert: bool,
}

impl RewindDialog {
    pub fn new(
        turns: Vec<(usize, String)>,
        restore_vim_insert: bool,
        max_height: Option<u16>,
    ) -> Self {
        // +1 for the "(current)" sentinel entry at the end.
        let total = turns.len() + 1;
        let mut list = ListState::new(total, max_height, LIST_OVERHEAD);
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
        self.list.height(self.total_items())
    }

    fn mark_dirty(&mut self) {
        self.list.dirty = true;
    }

    fn anchor_row(&self) -> Option<u16> {
        self.list.anchor_row
    }

    fn handle_resize(&mut self) {
        self.list
            .handle_resize(terminal::size().ok().map(|(_, h)| h));
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
            Some(NavAction::Up) => {
                self.list.select_prev(n);
                None
            }
            Some(NavAction::Down) => {
                self.list.select_next(n);
                None
            }
            Some(NavAction::PageUp) => {
                self.list.page_up();
                None
            }
            Some(NavAction::PageDown) => {
                self.list.page_down(n);
                None
            }
            _ => None,
        }
    }

    fn draw(&mut self, out: &mut RenderOut, start_row: u16, width: u16, height: u16) {
        let n = self.total_items();
        let Some((w, _)) = self.list.begin_draw(out, start_row, n, width, height) else {
            return;
        };

        draw_bar(out, w, None, None, theme::accent());
        crlf(out);

        out.push_dim();
        let _ = out.queue(Print(" Rewind to:"));
        out.pop_style();
        crlf(out);

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
            let _ = out.queue(Print(format!("  {:>width$}.", num, width = num_width)));
            out.pop_style();
            if i == self.list.selected {
                let _ = out.queue(Print(" "));
                out.push_fg(theme::accent());
                let _ = out.queue(Print(&truncated));
                out.pop_style();
            } else {
                let _ = out.queue(Print(" "));
                let _ = out.queue(Print(&truncated));
            }
            crlf(out);
        }

        crlf(out);
        out.push_dim();
        let _ = out.queue(Print(&hints::join(&[hints::SELECT, hints::CANCEL])));
        out.pop_style();
        end_dialog_draw(out);
    }
}
