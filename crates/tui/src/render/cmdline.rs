use super::{draw_soft_cursor, selection, RenderOut};
use crossterm::style::Color;

/// Nvim-style `:` command line rendered inside the status bar row.
#[derive(Default)]
pub struct CmdlineState {
    pub buf: String,
    pub cursor: usize,
}

impl CmdlineState {
    pub fn new() -> Self {
        Self::default()
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

    pub fn move_start(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.buf.len();
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
