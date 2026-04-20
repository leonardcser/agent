use crate::keymap::{hints, nav_lookup, NavAction};
use crate::render::draw_bar;
use crate::theme;
use crossterm::event::{KeyCode, KeyModifiers};
use crossterm::{terminal, QueueableCommand};

use super::{end_dialog_draw, DialogResult, ListState, RenderOut};

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
            list: ListState::new(0),
            sections,
            total_rows,
            vim_enabled,
        }
    }
}

impl super::Dialog for HelpDialog {
    fn height(&self) -> u16 {
        self.list.height(self.total_rows, 5)
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

    fn draw(&mut self, out: &mut RenderOut, start_row: u16, width: u16, granted_rows: u16) {
        let label_col = self
            .sections
            .iter()
            .flat_map(|(_, entries)| entries.iter().map(|(k, _)| k.len()))
            .max()
            .unwrap_or(0)
            + 4;

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

        let Some(w) = self
            .list
            .begin_draw(out, start_row, total_content, width, granted_rows, 5)
        else {
            return;
        };
        let max_visible = self.list.max_visible;

        // Clamp scroll
        let max_scroll = total_content.saturating_sub(max_visible);
        self.list.scroll_offset = self.list.scroll_offset.min(max_scroll);

        draw_bar(out, w, None, None, theme::accent());
        out.newline();

        out.push_dim();
        out.print(" help");
        out.pop_style();
        out.newline();
        out.newline();

        for &(label, detail) in content_lines
            .iter()
            .skip(self.list.scroll_offset)
            .take(max_visible)
        {
            if label.is_empty() && detail.is_empty() {
                out.newline();
            } else {
                out.print("  ");
                out.push_fg(theme::muted());
                out.print(label);
                out.pop_style();
                let padding = " ".repeat(label_col.saturating_sub(label.len()));
                out.push_dim();
                out.print(&format!("{padding}{detail}"));
                out.pop_style();
                let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
                out.newline();
            }
        }

        out.newline();
        out.push_dim();
        out.print(&hints::join(&[
            hints::CLOSE,
            hints::nav(self.vim_enabled),
            hints::scroll(self.vim_enabled),
        ]));
        out.pop_style();
        let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
        end_dialog_draw(out);
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::Dialog;

    #[test]
    fn dialog_render_starts_with_cursor_position() {
        let mut out = RenderOut::buffer();
        let mut dialog = HelpDialog::new(false);
        dialog.draw(&mut out, 0, 40, 12);
        let rendered = String::from_utf8(out.into_bytes()).unwrap();
        // begin_dialog_draw positions the cursor at (0, start_row).
        assert!(
            rendered.starts_with("\u{1b}[1;1H"),
            "dialog should start with cursor position, got: {rendered:?}"
        );
    }
}
