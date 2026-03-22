use crate::keymap::{hints, nav_lookup, NavAction};
use crate::render::{crlf, draw_bar};
use crate::theme;
use crossterm::event::{KeyCode, KeyModifiers};
use crossterm::style::{Attribute, Print, SetAttribute, SetForegroundColor};
use crossterm::{terminal, QueueableCommand};

use super::{end_dialog_draw, DialogResult, ListState};

pub struct HelpDialog {
    list: ListState,
    sections: Vec<(&'static str, Vec<(&'static str, &'static str)>)>,
    total_rows: usize,
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
            list: ListState::new(0, None, 4),
            sections,
            total_rows,
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
        self.list.handle_resize();
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

    fn draw(&mut self, start_row: u16, sync_started: bool) {
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

        let Some((mut out, w, _)) = self.list.begin_draw(start_row, total_content, sync_started)
        else {
            return;
        };
        let max_visible = self.list.max_visible;

        // Clamp scroll
        let max_scroll = total_content.saturating_sub(max_visible);
        self.list.scroll_offset = self.list.scroll_offset.min(max_scroll);

        draw_bar(&mut out, w, None, None, theme::accent());
        crlf(&mut out);

        let _ = out.queue(SetAttribute(Attribute::Dim));
        let _ = out.queue(Print(" help"));
        let _ = out.queue(SetAttribute(Attribute::Reset));
        crlf(&mut out);
        crlf(&mut out);

        for &(label, detail) in content_lines
            .iter()
            .skip(self.list.scroll_offset)
            .take(max_visible)
        {
            if label.is_empty() && detail.is_empty() {
                crlf(&mut out);
            } else {
                let _ = out.queue(Print("  "));
                let _ = out.queue(SetForegroundColor(theme::MUTED));
                let _ = out.queue(Print(label));
                let _ = out.queue(crossterm::style::ResetColor);
                let padding = " ".repeat(label_col.saturating_sub(label.len()));
                let _ = out.queue(SetAttribute(Attribute::Dim));
                let _ = out.queue(Print(format!("{padding}{detail}")));
                let _ = out.queue(SetAttribute(Attribute::Reset));
                let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
                crlf(&mut out);
            }
        }

        crlf(&mut out);
        let _ = out.queue(SetAttribute(Attribute::Dim));
        let _ = out.queue(Print(&hints::join(&[
            hints::CLOSE,
            hints::NAV,
            hints::SCROLL,
        ])));
        let _ = out.queue(SetAttribute(Attribute::Reset));
        let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));

        end_dialog_draw(&mut out);
    }
}
