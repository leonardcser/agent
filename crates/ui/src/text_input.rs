use crate::component::{Component, DrawContext, KeyResult};
use crate::grid::{GridSlice, Style};
use crate::layout::Rect;
use crossterm::event::{KeyCode, KeyModifiers};

pub struct TextInput {
    text: String,
    cursor_col: usize,
    scroll_offset: usize,
    placeholder: Option<String>,
    placeholder_style: Style,
    text_style: Style,
}

impl TextInput {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            cursor_col: 0,
            scroll_offset: 0,
            placeholder: None,
            placeholder_style: Style {
                dim: true,
                ..Style::default()
            },
            text_style: Style::default(),
        }
    }

    pub fn with_placeholder(mut self, text: impl Into<String>) -> Self {
        self.placeholder = Some(text.into());
        self
    }

    pub fn with_text_style(mut self, style: Style) -> Self {
        self.text_style = style;
        self
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor_col = self.text.chars().count();
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor_col = 0;
        self.scroll_offset = 0;
    }

    pub fn cursor_col(&self) -> usize {
        self.cursor_col
    }

    fn char_count(&self) -> usize {
        self.text.chars().count()
    }

    fn insert_char(&mut self, ch: char) {
        let byte_pos = self
            .text
            .char_indices()
            .nth(self.cursor_col)
            .map(|(i, _)| i)
            .unwrap_or(self.text.len());
        self.text.insert(byte_pos, ch);
        self.cursor_col += 1;
    }

    fn delete_back(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
            let byte_pos = self
                .text
                .char_indices()
                .nth(self.cursor_col)
                .map(|(i, _)| i)
                .unwrap_or(self.text.len());
            let next = self.text[byte_pos..]
                .chars()
                .next()
                .map(|c| c.len_utf8())
                .unwrap_or(0);
            self.text.drain(byte_pos..byte_pos + next);
        }
    }

    fn delete_forward(&mut self) {
        if self.cursor_col < self.char_count() {
            let byte_pos = self
                .text
                .char_indices()
                .nth(self.cursor_col)
                .map(|(i, _)| i)
                .unwrap_or(self.text.len());
            let next = self.text[byte_pos..]
                .chars()
                .next()
                .map(|c| c.len_utf8())
                .unwrap_or(0);
            self.text.drain(byte_pos..byte_pos + next);
        }
    }

    fn move_left(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        }
    }

    fn move_right(&mut self) {
        if self.cursor_col < self.char_count() {
            self.cursor_col += 1;
        }
    }

    fn move_home(&mut self) {
        if self.cursor_col != 0 {
            self.cursor_col = 0;
        }
    }

    fn move_end(&mut self) {
        let count = self.char_count();
        if self.cursor_col != count {
            self.cursor_col = count;
        }
    }

    fn delete_word_back(&mut self) {
        if self.cursor_col == 0 {
            return;
        }
        let chars: Vec<char> = self.text.chars().collect();
        let mut pos = self.cursor_col;
        while pos > 0 && chars[pos - 1].is_whitespace() {
            pos -= 1;
        }
        while pos > 0 && !chars[pos - 1].is_whitespace() {
            pos -= 1;
        }
        let start_byte: usize = chars[..pos].iter().map(|c| c.len_utf8()).sum();
        let end_byte: usize = chars[..self.cursor_col].iter().map(|c| c.len_utf8()).sum();
        self.text.drain(start_byte..end_byte);
        self.cursor_col = pos;
    }

    pub fn ensure_visible(&mut self, width: u16) {
        let w = width as usize;
        if w == 0 {
            return;
        }
        if self.cursor_col < self.scroll_offset {
            self.scroll_offset = self.cursor_col;
        } else if self.cursor_col >= self.scroll_offset + w {
            self.scroll_offset = self.cursor_col - w + 1;
        }
    }
}

impl Default for TextInput {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for TextInput {
    fn draw(&self, _area: Rect, grid: &mut GridSlice<'_>, _ctx: &DrawContext) {
        let w = grid.width();
        if w == 0 || grid.height() == 0 {
            return;
        }

        if self.text.is_empty() {
            if let Some(ref ph) = self.placeholder {
                grid.put_str(0, 0, ph, self.placeholder_style);
            }
            return;
        }

        let chars: Vec<char> = self.text.chars().collect();
        let visible_start = self.scroll_offset;
        let visible_end = (visible_start + w as usize).min(chars.len());
        for (col, &ch) in chars[visible_start..visible_end].iter().enumerate() {
            grid.set(col as u16, 0, ch, self.text_style);
        }
    }

    fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> KeyResult {
        match (code, mods) {
            (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                self.insert_char(c);
                KeyResult::Consumed
            }
            (KeyCode::Backspace, _) => {
                self.delete_back();
                KeyResult::Consumed
            }
            (KeyCode::Delete, _) => {
                self.delete_forward();
                KeyResult::Consumed
            }
            (KeyCode::Left, KeyModifiers::NONE) => {
                self.move_left();
                KeyResult::Consumed
            }
            (KeyCode::Right, KeyModifiers::NONE) => {
                self.move_right();
                KeyResult::Consumed
            }
            (KeyCode::Home, _) | (KeyCode::Char('a'), KeyModifiers::CONTROL) => {
                self.move_home();
                KeyResult::Consumed
            }
            (KeyCode::End, _) | (KeyCode::Char('e'), KeyModifiers::CONTROL) => {
                self.move_end();
                KeyResult::Consumed
            }
            (KeyCode::Char('w'), KeyModifiers::CONTROL) => {
                self.delete_word_back();
                KeyResult::Consumed
            }
            (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                self.clear();
                KeyResult::Consumed
            }
            (KeyCode::Enter, _) => KeyResult::Action("submit".into()),
            (KeyCode::Esc, _) => KeyResult::Action("cancel".into()),
            _ => KeyResult::Ignored,
        }
    }

    fn cursor(&self) -> Option<(u16, u16)> {
        let visible_col = self.cursor_col.saturating_sub(self.scroll_offset);
        Some((visible_col as u16, 0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> TextInput {
        TextInput::new()
    }

    #[test]
    fn type_and_read() {
        let mut ti = input();
        ti.insert_char('h');
        ti.insert_char('i');
        assert_eq!(ti.text(), "hi");
        assert_eq!(ti.cursor_col(), 2);
    }

    #[test]
    fn backspace() {
        let mut ti = input();
        ti.set_text("hello");
        ti.delete_back();
        assert_eq!(ti.text(), "hell");
    }

    #[test]
    fn delete_forward() {
        let mut ti = input();
        ti.set_text("hello");
        ti.move_home();
        ti.delete_forward();
        assert_eq!(ti.text(), "ello");
    }

    #[test]
    fn movement() {
        let mut ti = input();
        ti.set_text("abc");
        assert_eq!(ti.cursor_col(), 3);
        ti.move_left();
        assert_eq!(ti.cursor_col(), 2);
        ti.move_home();
        assert_eq!(ti.cursor_col(), 0);
        ti.move_end();
        assert_eq!(ti.cursor_col(), 3);
    }

    #[test]
    fn delete_word_back() {
        let mut ti = input();
        ti.set_text("hello world");
        ti.delete_word_back();
        assert_eq!(ti.text(), "hello ");
    }

    #[test]
    fn ctrl_u_clears() {
        let mut ti = input();
        ti.set_text("some text");
        let result = ti.handle_key(KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert_eq!(result, KeyResult::Consumed);
        assert_eq!(ti.text(), "");
    }

    #[test]
    fn enter_submits() {
        let mut ti = input();
        let result = ti.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(result, KeyResult::Action("submit".into()));
    }

    #[test]
    fn cursor_position() {
        let mut ti = input();
        ti.set_text("abc");
        ti.move_left();
        assert_eq!(ti.cursor(), Some((2, 0)));
    }

    #[test]
    fn renders_text() {
        let mut ti = input();
        ti.set_text("hello");
        let mut grid = crate::grid::Grid::new(10, 1);
        let ctx = DrawContext {
            terminal_width: 10,
            terminal_height: 1,
            focused: true,
        };
        let mut slice = grid.slice_mut(Rect::new(0, 0, 10, 1));
        ti.draw(Rect::new(0, 0, 10, 1), &mut slice, &ctx);
        assert_eq!(grid.cell(0, 0).symbol, 'h');
        assert_eq!(grid.cell(4, 0).symbol, 'o');
    }

    #[test]
    fn renders_placeholder_when_empty() {
        let ti = TextInput::new().with_placeholder("type here...");
        let mut grid = crate::grid::Grid::new(20, 1);
        let ctx = DrawContext {
            terminal_width: 20,
            terminal_height: 1,
            focused: true,
        };
        let mut slice = grid.slice_mut(Rect::new(0, 0, 20, 1));
        ti.draw(Rect::new(0, 0, 20, 1), &mut slice, &ctx);
        assert_eq!(grid.cell(0, 0).symbol, 't');
        assert!(grid.cell(0, 0).style.dim);
    }
}
