use crate::component::{Component, CursorInfo, DrawContext, KeyResult, WidgetEvent};
use crate::dialog::PanelWidget;
use crate::grid::{GridSlice, Style};
use crate::layout::Rect;
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

pub struct TextInput {
    text: String,
    cursor_col: usize,
    scroll_offset: usize,
    placeholder: Option<String>,
    placeholder_style: Style,
    text_style: Style,
    /// Rect from the last `prepare` — needed by `handle_mouse` to
    /// translate absolute click columns into a text offset.
    last_area: Rect,
    /// Selection anchor (char index) for click-drag text selection.
    /// `None` outside a drag. Set on first `Drag` after `Down`; cleared
    /// on `Up` or any keyboard edit.
    anchor: Option<usize>,
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
            last_area: Rect::new(0, 0, 0, 0),
            anchor: None,
        }
    }

    /// Resolved (start, end) char indices when a drag-select is active,
    /// `None` otherwise. `start <= end` regardless of drag direction.
    pub fn selection_range(&self) -> Option<(usize, usize)> {
        let anchor = self.anchor?;
        if anchor == self.cursor_col {
            return None;
        }
        let (s, e) = if anchor < self.cursor_col {
            (anchor, self.cursor_col)
        } else {
            (self.cursor_col, anchor)
        };
        Some((s, e))
    }

    fn clear_selection(&mut self) {
        self.anchor = None;
    }

    /// Translate a click column relative to the widget's rect into a
    /// char index (clamped). Used by `handle_mouse` for both initial
    /// `Down` and subsequent `Drag` extension.
    fn char_index_at_local_col(&self, local_col: u16) -> usize {
        let target = self.scroll_offset + local_col as usize;
        target.min(self.char_count())
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
        self.clear_selection();
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
        self.clear_selection();
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
        self.clear_selection();
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
        self.clear_selection();
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
    fn prepare(&mut self, area: Rect, _ctx: &DrawContext) {
        self.last_area = area;
    }

    fn draw(&self, _area: Rect, grid: &mut GridSlice<'_>, ctx: &DrawContext) {
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
        let selection = self.selection_range();
        for (col, &ch) in chars[visible_start..visible_end].iter().enumerate() {
            let abs_idx = visible_start + col;
            let in_selection = selection
                .map(|(s, e)| abs_idx >= s && abs_idx < e)
                .unwrap_or(false);
            let style = if in_selection {
                Style {
                    bg: ctx.selection_style.bg,
                    ..self.text_style
                }
            } else {
                self.text_style
            };
            grid.set(col as u16, 0, ch, style);
        }
    }

    fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> KeyResult {
        // Edits return `text_changed` so callers bound to
        // `WinEvent::TextChanged` can refresh on every keystroke.
        // Cursor/navigation keys stay `Consumed` since the buffer
        // didn't change.
        match (code, mods) {
            (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                self.insert_char(c);
                KeyResult::Action(WidgetEvent::TextChanged)
            }
            (KeyCode::Backspace, _) => {
                self.delete_back();
                KeyResult::Action(WidgetEvent::TextChanged)
            }
            (KeyCode::Delete, _) => {
                self.delete_forward();
                KeyResult::Action(WidgetEvent::TextChanged)
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
                KeyResult::Action(WidgetEvent::TextChanged)
            }
            (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                self.clear();
                KeyResult::Action(WidgetEvent::TextChanged)
            }
            (KeyCode::Enter, _) => KeyResult::Action(WidgetEvent::Submit),
            (KeyCode::Esc, _) => KeyResult::Action(WidgetEvent::Cancel),
            _ => KeyResult::Ignored,
        }
    }

    fn handle_mouse(&mut self, event: MouseEvent) -> KeyResult {
        let rect = self.last_area;
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if rect.width == 0 || !rect.contains(event.row, event.column) {
                    return KeyResult::Ignored;
                }
                let local_col = event.column - rect.left;
                self.cursor_col = self.char_index_at_local_col(local_col);
                self.clear_selection();
                // Capture so subsequent Drag/Up route here even if the
                // pointer slides off the rect.
                KeyResult::Capture
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if rect.width == 0 {
                    return KeyResult::Ignored;
                }
                // First drag tick: pin the anchor at the cursor's
                // current position (== where Down landed). Clamp
                // pointer column to the rect so a drag past the right
                // edge selects to end-of-text instead of nothing.
                if self.anchor.is_none() {
                    self.anchor = Some(self.cursor_col);
                }
                let clamped_col = event
                    .column
                    .clamp(rect.left, rect.left + rect.width.saturating_sub(1));
                let local_col = clamped_col - rect.left;
                self.cursor_col = self.char_index_at_local_col(local_col);
                KeyResult::Consumed
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let yank = self
                    .selection_range()
                    .map(|(s, e)| self.text.chars().skip(s).take(e - s).collect::<String>());
                self.clear_selection();
                match yank {
                    Some(text) if !text.is_empty() => KeyResult::Action(WidgetEvent::Yank(text)),
                    _ => KeyResult::Consumed,
                }
            }
            _ => KeyResult::Ignored,
        }
    }

    fn cursor(&self) -> Option<CursorInfo> {
        let visible_col = self.cursor_col.saturating_sub(self.scroll_offset);
        Some(CursorInfo::hardware(visible_col as u16, 0))
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

impl PanelWidget for TextInput {
    fn content_rows(&self) -> usize {
        1
    }
    fn text_value(&self) -> Option<String> {
        Some(self.text.clone())
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
        let result = Component::handle_key(&mut ti, KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert_eq!(result, KeyResult::Action(WidgetEvent::TextChanged));
        assert_eq!(ti.text(), "");
    }

    #[test]
    fn enter_submits() {
        let mut ti = input();
        let result = Component::handle_key(&mut ti, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(result, KeyResult::Action(WidgetEvent::Submit));
    }

    #[test]
    fn cursor_position() {
        let mut ti = input();
        ti.set_text("abc");
        ti.move_left();
        let ci = Component::cursor(&ti).unwrap();
        assert_eq!((ci.col, ci.row), (2, 0));
        assert!(ci.style.is_none());
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
            selection_style: Default::default(),
        };
        let mut slice = grid.slice_mut(Rect::new(0, 0, 10, 1));
        Component::draw(&ti, Rect::new(0, 0, 10, 1), &mut slice, &ctx);
        assert_eq!(grid.cell(0, 0).symbol, 'h');
        assert_eq!(grid.cell(4, 0).symbol, 'o');
    }

    fn mouse_event(kind: MouseEventKind, row: u16, col: u16) -> MouseEvent {
        MouseEvent {
            kind,
            row,
            column: col,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn click_positions_cursor_and_requests_capture() {
        let mut ti = TextInput::new();
        ti.set_text("hello world");
        ti.last_area = Rect::new(2, 5, 20, 1);
        let r = ti.handle_mouse(mouse_event(MouseEventKind::Down(MouseButton::Left), 2, 8));
        assert_eq!(r, KeyResult::Capture);
        assert_eq!(ti.cursor_col(), 3);
    }

    #[test]
    fn drag_extends_selection_and_release_yanks() {
        let mut ti = TextInput::new();
        ti.set_text("hello world");
        ti.last_area = Rect::new(2, 5, 20, 1);
        ti.handle_mouse(mouse_event(MouseEventKind::Down(MouseButton::Left), 2, 5));
        // Drag to col 10 → cursor at char 5; anchor pinned at 0.
        ti.handle_mouse(mouse_event(MouseEventKind::Drag(MouseButton::Left), 2, 10));
        assert_eq!(ti.selection_range(), Some((0, 5)));
        let r = ti.handle_mouse(mouse_event(MouseEventKind::Up(MouseButton::Left), 2, 10));
        assert_eq!(r, KeyResult::Action(WidgetEvent::Yank("hello".into())));
        // Selection clears on release.
        assert_eq!(ti.selection_range(), None);
    }

    #[test]
    fn typing_clears_selection() {
        let mut ti = TextInput::new();
        ti.set_text("hello");
        ti.anchor = Some(0);
        ti.cursor_col = 3;
        assert!(ti.selection_range().is_some());
        ti.handle_key(KeyCode::Char('x'), KeyModifiers::NONE);
        assert_eq!(ti.selection_range(), None);
    }

    #[test]
    fn renders_placeholder_when_empty() {
        let ti = TextInput::new().with_placeholder("type here...");
        let mut grid = crate::grid::Grid::new(20, 1);
        let ctx = DrawContext {
            terminal_width: 20,
            terminal_height: 1,
            focused: true,
            selection_style: Default::default(),
        };
        let mut slice = grid.slice_mut(Rect::new(0, 0, 20, 1));
        Component::draw(&ti, Rect::new(0, 0, 20, 1), &mut slice, &ctx);
        assert_eq!(grid.cell(0, 0).symbol, 't');
        assert!(grid.cell(0, 0).style.dim);
    }
}
