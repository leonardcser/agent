use super::{begin_dialog_draw, end_dialog_draw, wrap_line, DialogResult, ListState, RenderOut};
use crate::keymap::{hints, nav_lookup, NavAction};
use crate::render::draw_bar;
use crate::theme;
use crossterm::event::{KeyCode, KeyModifiers};
use crossterm::{terminal, QueueableCommand};

pub struct FloatDialog {
    pub id: u64,
    title: String,
    lines: Vec<String>,
    loading: bool,
    footer_items: Vec<String>,
    accent: crossterm::style::Color,
    list: ListState,
    wrapped: Vec<String>,
    wrap_width: usize,
    content_scroll: usize,
    vim_enabled: bool,
}

impl FloatDialog {
    pub fn new(
        id: u64,
        title: String,
        lines: Vec<String>,
        loading: bool,
        footer_items: Vec<String>,
        accent: Option<crossterm::style::Color>,
        vim_enabled: bool,
    ) -> Self {
        let item_count = footer_items.len();
        Self {
            id,
            title,
            lines,
            loading,
            footer_items,
            accent: accent.unwrap_or_else(theme::accent),
            list: ListState::new(item_count),
            wrapped: Vec::new(),
            wrap_width: 0,
            content_scroll: 0,
            vim_enabled,
        }
    }

    pub fn set_lines(&mut self, lines: Vec<String>) {
        self.lines = lines;
        self.loading = false;
        self.wrapped.clear();
        self.wrap_width = 0;
        self.content_scroll = 0;
        self.list.dirty = true;
    }

    pub fn set_title(&mut self, title: String) {
        self.title = title;
        self.list.dirty = true;
    }

    pub fn set_loading(&mut self, loading: bool) {
        self.loading = loading;
        self.list.dirty = true;
    }

    fn ensure_wrapped(&mut self, width: usize) {
        if self.wrap_width == width || width < 4 {
            return;
        }
        self.wrap_width = width;
        self.wrapped.clear();
        let usable = width.saturating_sub(4);
        for line in &self.lines {
            if line.is_empty() {
                self.wrapped.push(String::new());
            } else {
                for wl in wrap_line(line, usable) {
                    self.wrapped.push(wl);
                }
            }
        }
    }

    fn chrome_rows(&self) -> u16 {
        let footer = if self.footer_items.is_empty() {
            0
        } else {
            self.footer_items.len() as u16 + 1
        };
        4 + footer
    }

    fn content_budget(&self, granted_rows: u16) -> usize {
        (granted_rows as usize).saturating_sub(self.chrome_rows() as usize)
    }
}

impl super::Dialog for FloatDialog {
    fn height(&self) -> u16 {
        let content = if self.loading || self.lines.is_empty() {
            1
        } else {
            self.wrapped.len().max(1) as u16
        };
        self.chrome_rows() + content
    }

    fn constrain_height(&self) -> bool {
        true
    }

    fn mark_dirty(&mut self) {
        self.list.dirty = true;
    }

    fn handle_resize(&mut self) {
        self.wrapped.clear();
        self.wrap_width = 0;
        self.list.handle_resize();
    }

    fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> Option<DialogResult> {
        if let Some(action) = nav_lookup(code, mods) {
            match action {
                NavAction::Dismiss => {
                    return Some(DialogResult::FloatDismiss { id: self.id });
                }
                NavAction::Confirm if !self.footer_items.is_empty() => {
                    return Some(DialogResult::FloatSelect {
                        id: self.id,
                        index: self.list.selected,
                    });
                }
                NavAction::Confirm => {
                    return Some(DialogResult::FloatDismiss { id: self.id });
                }
                _ => {}
            }

            if self.footer_items.is_empty() {
                let total = self.wrapped.len();
                let budget = self.content_budget(40);
                let max = total.saturating_sub(budget);
                match action {
                    NavAction::Up if self.content_scroll > 0 => {
                        self.content_scroll -= 1;
                        self.list.dirty = true;
                    }
                    NavAction::Down if self.content_scroll < max => {
                        self.content_scroll += 1;
                        self.list.dirty = true;
                    }
                    NavAction::PageUp => {
                        self.content_scroll = self.content_scroll.saturating_sub(10);
                        self.list.dirty = true;
                    }
                    NavAction::PageDown => {
                        self.content_scroll = (self.content_scroll + 10).min(max);
                        self.list.dirty = true;
                    }
                    _ => {}
                }
            } else if self.list.handle_nav(action, self.footer_items.len()) {
                // selection handled
            }
        }

        if let KeyCode::Char(c) = code {
            if let Some(d) = c.to_digit(10) {
                let idx = d as usize;
                if idx >= 1 && idx <= self.footer_items.len() {
                    return Some(DialogResult::FloatSelect {
                        id: self.id,
                        index: idx - 1,
                    });
                }
            }
        }

        None
    }

    fn draw(&mut self, out: &mut RenderOut, start_row: u16, width: u16, granted_rows: u16) {
        if !self.list.dirty {
            return;
        }
        self.list.dirty = false;

        let w = width as usize;
        self.ensure_wrapped(w);

        begin_dialog_draw(out, start_row);

        draw_bar(out, w, None, None, self.accent);
        out.newline();

        out.push_dim();
        out.print(&format!(" {}", self.title));
        out.pop_style();
        let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
        out.newline();
        out.newline();

        let budget = self.content_budget(granted_rows);
        if self.loading {
            out.push_dim();
            out.print("  thinking…");
            out.pop_style();
            let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
            out.newline();
        } else if !self.wrapped.is_empty() {
            let max_scroll = self.wrapped.len().saturating_sub(budget);
            self.content_scroll = self.content_scroll.min(max_scroll);
            for line in self.wrapped.iter().skip(self.content_scroll).take(budget) {
                out.print("  ");
                out.print(line);
                let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
                out.newline();
            }
        }

        if !self.footer_items.is_empty() {
            out.newline();
            for (i, item) in self.footer_items.iter().enumerate() {
                let selected = i == self.list.selected;
                out.print("  ");
                if selected {
                    out.push_fg(self.accent);
                    out.print("▸ ");
                } else {
                    out.print("  ");
                }
                out.push_dim();
                out.print(&format!("{}. ", i + 1));
                out.pop_style();
                out.print(item);
                if selected {
                    out.pop_style();
                }
                let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
                out.newline();
            }
        }

        out.newline();
        out.push_dim();
        if self.footer_items.is_empty() {
            out.print(&hints::join(&[
                hints::CLOSE,
                hints::scroll(self.vim_enabled),
            ]));
        } else {
            out.print(&hints::join(&[hints::CLOSE, hints::nav(self.vim_enabled)]));
        }
        out.pop_style();
        let _ = out.queue(terminal::Clear(terminal::ClearType::UntilNewLine));
        end_dialog_draw(out);
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
