use crate::keymap::{hints, nav_lookup, NavAction};
use crate::render::{crlf, draw_bar};
use crate::theme;
use crossterm::event::{KeyCode, KeyModifiers};
use crossterm::style::Print;
use crossterm::{terminal, QueueableCommand};

use super::{end_dialog_draw, DialogResult, ListState, RenderOut};

/// Chrome rows around the item list: bar + "help" header + blank
/// separator below header + blank separator above hints + hints
/// footer. One row taller than the other dialogs because help has an
/// extra blank line between its header and content.
const LIST_OVERHEAD: u16 = 5;

pub struct HelpDialog {
    list: ListState,
    sections: Vec<(&'static str, Vec<(&'static str, &'static str)>)>,
    total_rows: usize,
    vim_enabled: bool,
}

impl HelpDialog {
    pub fn new(vim_enabled: bool) -> Self {
        let sections = hints::help_sections(vim_enabled);
        let total_rows = sections
            .iter()
            .enumerate()
            .map(|(i, (_, entries))| entries.len() + if i + 1 < sections.len() { 1 } else { 0 })
            .sum();
        Self {
            list: ListState::new(0, None, LIST_OVERHEAD),
            sections,
            total_rows,
            vim_enabled,
        }
    }
}

impl super::Dialog for HelpDialog {
    fn height(&self) -> u16 {
        self.list.height(self.total_rows)
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
        // Help-specific close keys (q, ?).
        if matches!(
            (code, mods),
            (KeyCode::Char('q'), _) | (KeyCode::Char('?'), _)
        ) {
            return Some(DialogResult::Dismissed);
        }

        let max_scroll = self.total_rows.saturating_sub(self.list.max_visible);
        let page = self.list.max_visible.max(1);
        match nav_lookup(code, mods) {
            Some(NavAction::Dismiss | NavAction::Confirm) => Some(DialogResult::Dismissed),
            Some(NavAction::Up) => {
                if self.list.scroll_offset > 0 {
                    self.list.scroll_offset -= 1;
                    self.list.dirty = true;
                }
                None
            }
            Some(NavAction::Down) => {
                if self.list.scroll_offset < max_scroll {
                    self.list.scroll_offset += 1;
                    self.list.dirty = true;
                }
                None
            }
            Some(NavAction::PageUp) => {
                self.list.scroll_offset = self.list.scroll_offset.saturating_sub(page);
                self.list.dirty = true;
                None
            }
            Some(NavAction::PageDown) => {
                self.list.scroll_offset = (self.list.scroll_offset + page).min(max_scroll);
                self.list.dirty = true;
                None
            }
            _ => None,
        }
    }

    fn draw(&mut self, out: &mut RenderOut, start_row: u16, width: u16, height: u16) {
        let label_col = self
            .sections
            .iter()
            .flat_map(|(_, entries)| entries.iter().map(|(k, _)| k.len()))
            .max()
            .unwrap_or(0)
            + 4;

        // Collect content lines for scrolling.
        let mut content_lines: Vec<(&str, &str)> = Vec::new();
        for (si, (_, entries)) in self.sections.iter().enumerate() {
            for &(label, detail) in entries {
                content_lines.push((label, detail));
            }
            if si + 1 < self.sections.len() {
                content_lines.push(("", "")); // blank separator
            }
        }
        let total_content = content_lines.len();

        let Some((w, _)) = self
            .list
            .begin_draw(out, start_row, total_content, width, height)
        else {
            return;
        };
        let max_visible = self.list.max_visible;

        // Clamp scroll
        let max_scroll = total_content.saturating_sub(max_visible);
        self.list.scroll_offset = self.list.scroll_offset.min(max_scroll);

        draw_bar(out, w, None, None, theme::accent());
        crlf(out);

        out.push_dim();
        let _ = out.queue(Print(" help"));
        out.pop_style();
        crlf(out);
        crlf(out);

        for &(label, detail) in content_lines
            .iter()
            .skip(self.list.scroll_offset)
            .take(max_visible)
        {
            if label.is_empty() && detail.is_empty() {
                crlf(out);
            } else {
                let _ = out.queue(Print("  "));
                out.push_fg(theme::muted());
                let _ = out.queue(Print(label));
                out.pop_style();
                let padding = " ".repeat(label_col.saturating_sub(label.len()));
                out.push_dim();
                let _ = out.queue(Print(format!("{padding}{detail}")));
                out.pop_style();
                let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
                crlf(out);
            }
        }

        crlf(out);
        out.push_dim();
        let _ = out.queue(Print(&hints::join(&[
            hints::CLOSE,
            hints::nav(self.vim_enabled),
            hints::scroll(self.vim_enabled),
        ])));
        out.pop_style();
        let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));

        end_dialog_draw(out);
    }
}
