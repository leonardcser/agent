use super::{draw_soft_cursor, selection, RenderOut};
use crate::completer::{CompletionItem, Completer};
use crossterm::style::Color;

/// Nvim-style `:` command line rendered inside the status bar row.
#[derive(Default)]
pub struct CmdlineState {
    pub active: bool,
    pub buf: String,
    pub cursor: usize,
    history: Vec<String>,
    history_idx: Option<usize>,
    stash: String,
    pub(crate) completer: Option<Completer>,
}

impl CmdlineState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn open(&mut self) {
        self.active = true;
        self.buf.clear();
        self.cursor = 0;
        self.reset_history_browse();
        self.completer = None;
    }

    pub fn close(&mut self) {
        self.active = false;
        self.buf.clear();
        self.cursor = 0;
        self.reset_history_browse();
        self.completer = None;
    }

    pub fn submit(&mut self) -> String {
        let accepted = if let Some(ref comp) = self.completer {
            comp.selected_item().map(|i| i.label.clone())
        } else {
            None
        };
        let line = accepted.unwrap_or_else(|| self.buf.clone());
        self.push_history(line.clone());
        self.close();
        line
    }

    pub fn insert_char(&mut self, ch: char) {
        self.buf.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            let prev = self.buf[..self.cursor]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.buf.drain(prev..self.cursor);
            self.cursor = prev;
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.buf.len() {
            let next = self.buf[self.cursor..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| self.cursor + i)
                .unwrap_or(self.buf.len());
            self.buf.drain(self.cursor..next);
        }
    }

    pub fn move_left(&mut self) {
        if self.cursor > 0 {
            self.cursor = self.buf[..self.cursor]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
        }
    }

    pub fn move_right(&mut self) {
        if self.cursor < self.buf.len() {
            self.cursor = self.buf[self.cursor..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| self.cursor + i)
                .unwrap_or(self.buf.len());
        }
    }

    pub fn delete_word_back(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let before = &self.buf[..self.cursor];
        let end = before.len();
        let trimmed = before.trim_end();
        let start = trimmed
            .rfind(|c: char| !c.is_alphanumeric() && c != '_')
            .map(|i| i + trimmed[i..].chars().next().unwrap().len_utf8())
            .unwrap_or(0);
        self.buf.drain(start..end);
        self.cursor = start;
    }

    pub fn move_start(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.buf.len();
    }

    pub fn push_history(&mut self, line: String) {
        if line.is_empty() {
            return;
        }
        if self.history.last().map(|l| l == &line).unwrap_or(false) {
            return;
        }
        self.history.push(line);
    }

    pub fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let idx = match self.history_idx {
            None => {
                self.stash = self.buf.clone();
                self.history.len() - 1
            }
            Some(0) => return,
            Some(i) => i - 1,
        };
        self.history_idx = Some(idx);
        self.buf = self.history[idx].clone();
        self.cursor = self.buf.len();
    }

    pub fn history_down(&mut self) {
        let Some(idx) = self.history_idx else {
            return;
        };
        if idx + 1 >= self.history.len() {
            self.history_idx = None;
            self.buf = std::mem::take(&mut self.stash);
        } else {
            self.history_idx = Some(idx + 1);
            self.buf = self.history[idx + 1].clone();
        }
        self.cursor = self.buf.len();
    }

    pub fn reset_history_browse(&mut self) {
        self.history_idx = None;
        self.stash.clear();
    }

    /// Rebuild the completer from `Completer::commands` plus any
    /// Lua-registered names. Called after every edit so the dropdown
    /// stays visible and up-to-date.
    pub fn update_completer(&mut self, lua_commands: &[String]) {
        let mut comp = Completer::commands(0);
        if !lua_commands.is_empty() {
            let mut items: Vec<CompletionItem> = comp
                .all_items()
                .to_vec();
            for name in lua_commands {
                if !items.iter().any(|i| i.label == *name) {
                    items.push(CompletionItem {
                        label: name.clone(),
                        description: Some("(lua)".into()),
                        ..Default::default()
                    });
                }
            }
            comp.refresh_items(items);
        }
        comp.update_query(self.buf.clone());
        self.completer = Some(comp);
    }

    pub fn render(&self, out: &mut RenderOut, width: u16, row: u16) {
        let w = width as usize;
        let bg = Color::AnsiValue(233);
        out.push_bg(bg);
        out.push_fg(Color::White);
        out.print(":");
        let visible_width = w.saturating_sub(1);
        let display = selection::truncate_str(&self.buf, visible_width);
        out.print(&display);
        let used = 1 + display.chars().count();
        if used < w {
            out.print(&" ".repeat(w - used));
        }
        out.pop_style();
        out.pop_style();
        let cursor_col = (1 + self.buf[..self.cursor].chars().count()) as u16;
        let under = self.buf[self.cursor..]
            .chars()
            .next()
            .map(|c| c.to_string())
            .unwrap_or_else(|| " ".to_string());
        draw_soft_cursor(out, cursor_col, row, &under);
    }
}
