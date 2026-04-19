use super::{draw_soft_cursor, selection, RenderOut};
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
    pub(crate) completion: Option<CompletionState>,
}

pub(crate) struct CompletionState {
    matches: Vec<String>,
    index: usize,
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
    }

    pub fn close(&mut self) {
        self.active = false;
        self.buf.clear();
        self.cursor = 0;
        self.reset_history_browse();
    }

    pub fn submit(&mut self) -> String {
        let line = self.buf.clone();
        self.push_history(line.clone());
        self.close();
        line
    }

    pub fn insert_char(&mut self, ch: char) {
        self.buf.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
        self.completion = None;
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
            self.completion = None;
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
            self.completion = None;
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
        self.completion = None;
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

    /// Tab-complete the current buffer against `commands`. Cycles through
    /// matches on repeated Tab presses; Shift-Tab cycles backwards.
    pub fn complete(&mut self, commands: &[&str], reverse: bool) {
        if let Some(ref mut cs) = self.completion {
            if !cs.matches.is_empty() {
                if reverse {
                    cs.index = if cs.index == 0 {
                        cs.matches.len() - 1
                    } else {
                        cs.index - 1
                    };
                } else {
                    cs.index = (cs.index + 1) % cs.matches.len();
                }
                self.buf = cs.matches[cs.index].clone();
                self.cursor = self.buf.len();
            }
            return;
        }
        let mut matches: Vec<String> = commands
            .iter()
            .filter(|c| c.starts_with(self.buf.as_str()))
            .map(|c| c.to_string())
            .collect();
        matches.sort();
        if matches.is_empty() {
            return;
        }
        let index = 0;
        self.buf = matches[index].clone();
        self.cursor = self.buf.len();
        self.completion = Some(CompletionState { matches, index });
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
