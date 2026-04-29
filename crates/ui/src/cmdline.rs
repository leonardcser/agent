//! `Cmdline` — focusable compositor float for `:`-style command entry.
//!
//! Sibling to `Picker` and `Notification` in the named-components tier.
//! Single-row, docks at the terminal's bottom row, takes over that row
//! visually when active. Owns its own text buffer, cursor, and history;
//! surfaces `WidgetEvent::SubmitText` / `WidgetEvent::Dismiss` on
//! Enter / Esc so the caller can execute the command and close the
//! float.
//!
//! Completion is **not** part of the component. Callers that want
//! `Tab`-complete register a per-window keymap on the cmdline's WinId
//! and drive the completer externally via `Cmdline::{text, set_text}`.
//! This keeps the component free of feature-specific data lookups
//! (command registries, Lua bindings) and matches nvim's `ext_cmdline`
//! contract: declarative text state + a renderer.

use crate::component::{Component, CursorInfo, DrawContext, KeyResult, WidgetEvent};
use crate::grid::{GridSlice, Style};
use crate::layout::Rect;
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

#[derive(Clone, Debug, Default)]
pub struct CmdlineStyle {
    /// Background fill behind the row.
    pub background: Style,
    /// Prompt char + text.
    pub text: Style,
    /// Inverted cursor cell.
    pub cursor: Style,
}

pub struct Cmdline {
    /// Prompt glyph (`:`, `/`, `?`, …). Rendered before the buffer.
    prompt: char,
    buf: String,
    /// Byte offset into `buf`.
    cursor: usize,
    history: Vec<String>,
    history_idx: Option<usize>,
    stash: String,
    style: CmdlineStyle,
    /// Rect from the last `prepare` — used by `handle_mouse` to map an
    /// absolute click column to a byte offset in `buf`.
    last_area: Rect,
}

impl Default for Cmdline {
    fn default() -> Self {
        Self::new()
    }
}

impl Cmdline {
    pub fn new() -> Self {
        Self {
            prompt: ':',
            buf: String::new(),
            cursor: 0,
            history: Vec::new(),
            history_idx: None,
            stash: String::new(),
            style: CmdlineStyle::default(),
            last_area: Rect::new(0, 0, 0, 0),
        }
    }

    pub fn with_prompt(mut self, ch: char) -> Self {
        self.prompt = ch;
        self
    }

    pub fn with_style(mut self, style: CmdlineStyle) -> Self {
        self.style = style;
        self
    }

    pub fn with_history(mut self, history: Vec<String>) -> Self {
        self.history = history;
        self
    }

    pub fn text(&self) -> &str {
        &self.buf
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn history(&self) -> &[String] {
        &self.history
    }

    /// Replace the buffer and place the cursor at `text.len()`. Used by
    /// external completers that want to inject a completed value.
    pub fn set_text(&mut self, text: impl Into<String>) {
        self.buf = text.into();
        self.cursor = self.buf.len();
        self.reset_history_browse();
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Move `line` onto the history stack (dedup adjacent duplicates)
    /// and return its value. Useful for submit flows where the caller
    /// wants to both display and persist.
    pub fn push_history(&mut self, line: String) {
        if line.is_empty() {
            return;
        }
        if self.history.last().map(|l| l == &line).unwrap_or(false) {
            return;
        }
        self.history.push(line);
    }

    fn reset_history_browse(&mut self) {
        self.history_idx = None;
        self.stash.clear();
    }

    fn insert_char(&mut self, ch: char) {
        self.buf.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
        self.reset_history_browse();
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = self.buf[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
        self.buf.drain(prev..self.cursor);
        self.cursor = prev;
        self.reset_history_browse();
    }

    fn delete(&mut self) {
        if self.cursor >= self.buf.len() {
            return;
        }
        let next = self.buf[self.cursor..]
            .char_indices()
            .nth(1)
            .map(|(i, _)| self.cursor + i)
            .unwrap_or(self.buf.len());
        self.buf.drain(self.cursor..next);
        self.reset_history_browse();
    }

    fn delete_word_back(&mut self) {
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
        self.reset_history_browse();
    }

    fn move_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor = self.buf[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
    }

    fn move_right(&mut self) {
        if self.cursor >= self.buf.len() {
            return;
        }
        self.cursor = self.buf[self.cursor..]
            .char_indices()
            .nth(1)
            .map(|(i, _)| self.cursor + i)
            .unwrap_or(self.buf.len());
    }

    fn move_start(&mut self) {
        self.cursor = 0;
    }

    fn move_end(&mut self) {
        self.cursor = self.buf.len();
    }

    fn clear(&mut self) {
        self.buf.clear();
        self.cursor = 0;
        self.reset_history_browse();
    }

    fn history_up(&mut self) {
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

    fn history_down(&mut self) {
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
}

impl Component for Cmdline {
    fn prepare(&mut self, area: Rect, _ctx: &DrawContext) {
        self.last_area = area;
    }

    fn draw(&self, _area: Rect, slice: &mut GridSlice<'_>, _ctx: &DrawContext) {
        let w = slice.width();
        let h = slice.height();
        if w == 0 || h == 0 {
            return;
        }
        slice.fill(Rect::new(0, 0, w, h), ' ', self.style.background);

        // Prompt glyph.
        if w >= 1 {
            slice.set(0, 0, self.prompt, self.style.text);
        }

        // Buffer content, starting at column 1. Advance per display
        // cell using the same width-aware walker the grid uses.
        let mut col: u16 = 1;
        for ch in self.buf.chars() {
            if col >= w {
                break;
            }
            slice.set(col, 0, ch, self.style.text);
            let cw = unicode_width::UnicodeWidthChar::width(ch)
                .unwrap_or(1)
                .max(1) as u16;
            col = col.saturating_add(cw);
        }
    }

    fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> KeyResult {
        use KeyModifiers as M;
        match (code, mods) {
            (KeyCode::Esc, _) | (KeyCode::Char('c'), M::CONTROL) => {
                KeyResult::Action(WidgetEvent::Dismiss)
            }
            (KeyCode::Enter, _) => KeyResult::Action(WidgetEvent::SubmitText(self.buf.clone())),
            (KeyCode::Backspace, _) => {
                if self.cursor == 0 && self.buf.is_empty() {
                    // Backspace on empty buffer dismisses — matches
                    // current cmdline UX.
                    return KeyResult::Action(WidgetEvent::Dismiss);
                }
                self.backspace();
                KeyResult::Action(WidgetEvent::TextChanged)
            }
            (KeyCode::Delete, _) => {
                self.delete();
                KeyResult::Action(WidgetEvent::TextChanged)
            }
            (KeyCode::Left, _) => {
                self.move_left();
                KeyResult::Consumed
            }
            (KeyCode::Right, _) => {
                self.move_right();
                KeyResult::Consumed
            }
            (KeyCode::Up, _) => {
                self.history_up();
                KeyResult::Consumed
            }
            (KeyCode::Down, _) => {
                self.history_down();
                KeyResult::Consumed
            }
            (KeyCode::Home, _) | (KeyCode::Char('a'), M::CONTROL) => {
                self.move_start();
                KeyResult::Consumed
            }
            (KeyCode::End, _) | (KeyCode::Char('e'), M::CONTROL) => {
                self.move_end();
                KeyResult::Consumed
            }
            (KeyCode::Char('w'), M::CONTROL) => {
                let was_empty = self.buf.is_empty();
                self.delete_word_back();
                if self.buf.is_empty() && !was_empty {
                    KeyResult::Action(WidgetEvent::TextChanged)
                } else if was_empty {
                    KeyResult::Action(WidgetEvent::Dismiss)
                } else {
                    KeyResult::Action(WidgetEvent::TextChanged)
                }
            }
            (KeyCode::Char('u'), M::CONTROL) => {
                self.clear();
                KeyResult::Action(WidgetEvent::TextChanged)
            }
            (KeyCode::Char(ch), M::NONE | M::SHIFT) => {
                self.insert_char(ch);
                KeyResult::Action(WidgetEvent::TextChanged)
            }
            _ => KeyResult::Ignored,
        }
    }

    fn handle_mouse(&mut self, event: MouseEvent) -> KeyResult {
        let MouseEventKind::Down(MouseButton::Left) = event.kind else {
            return KeyResult::Ignored;
        };
        let rect = self.last_area;
        if rect.width == 0 || !rect.contains(event.row, event.column) {
            return KeyResult::Ignored;
        }
        // Column 0 within rect is the prompt glyph; text starts at 1.
        let local = event.column - rect.left;
        if local < 1 {
            self.cursor = 0;
            return KeyResult::Consumed;
        }
        let target_cell = (local - 1) as usize;
        // Walk text chars, summing display width, find the byte offset
        // whose cumulative width exceeds the click cell. Land on the
        // start of that char (so the cursor sits before it).
        let mut acc: usize = 0;
        let mut byte_off: usize = 0;
        for (i, ch) in self.buf.char_indices() {
            let cw = unicode_width::UnicodeWidthChar::width(ch)
                .unwrap_or(1)
                .max(1);
            if acc + cw > target_cell {
                byte_off = i;
                self.cursor = byte_off;
                self.reset_history_browse();
                return KeyResult::Consumed;
            }
            acc += cw;
            byte_off = i + ch.len_utf8();
        }
        // Past end of text: clamp to end.
        self.cursor = byte_off;
        self.reset_history_browse();
        KeyResult::Consumed
    }

    fn cursor(&self) -> Option<CursorInfo> {
        // Cursor column: 1 (prompt) + display-width of text before cursor.
        use unicode_width::UnicodeWidthStr;
        let prefix = &self.buf[..self.cursor];
        let col = 1u16.saturating_add(prefix.width() as u16);
        Some(CursorInfo::hardware(col, 0))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::Grid;

    fn ctx(w: u16, h: u16) -> DrawContext {
        DrawContext {
            terminal_width: w,
            terminal_height: h,
            focused: true,
            selection_style: Default::default(),
        }
    }

    #[test]
    fn renders_prompt_and_text() {
        let mut c = Cmdline::new();
        c.set_text("help");
        let mut grid = Grid::new(20, 1);
        let area = Rect::new(0, 0, 20, 1);
        let mut slice = grid.slice_mut(area);
        c.draw(area, &mut slice, &ctx(20, 1));
        assert_eq!(grid.cell(0, 0).symbol, ':');
        assert_eq!(grid.cell(1, 0).symbol, 'h');
        assert_eq!(grid.cell(2, 0).symbol, 'e');
        assert_eq!(grid.cell(3, 0).symbol, 'l');
        assert_eq!(grid.cell(4, 0).symbol, 'p');
    }

    #[test]
    fn char_insertion_emits_text_changed() {
        let mut c = Cmdline::new();
        assert_eq!(
            c.handle_key(KeyCode::Char('q'), KeyModifiers::NONE),
            KeyResult::Action(WidgetEvent::TextChanged)
        );
        assert_eq!(c.text(), "q");
        assert_eq!(c.cursor(), 1);
    }

    #[test]
    fn enter_emits_submit_text() {
        let mut c = Cmdline::new();
        c.set_text("help");
        assert_eq!(
            c.handle_key(KeyCode::Enter, KeyModifiers::NONE),
            KeyResult::Action(WidgetEvent::SubmitText("help".into()))
        );
    }

    #[test]
    fn esc_emits_dismiss() {
        let mut c = Cmdline::new();
        c.set_text("anything");
        assert_eq!(
            c.handle_key(KeyCode::Esc, KeyModifiers::NONE),
            KeyResult::Action(WidgetEvent::Dismiss)
        );
    }

    #[test]
    fn ctrl_c_dismisses() {
        let mut c = Cmdline::new();
        assert_eq!(
            c.handle_key(KeyCode::Char('c'), KeyModifiers::CONTROL),
            KeyResult::Action(WidgetEvent::Dismiss)
        );
    }

    #[test]
    fn backspace_on_empty_dismisses() {
        let mut c = Cmdline::new();
        assert_eq!(
            c.handle_key(KeyCode::Backspace, KeyModifiers::NONE),
            KeyResult::Action(WidgetEvent::Dismiss)
        );
    }

    #[test]
    fn backspace_deletes_prev_char() {
        let mut c = Cmdline::new();
        c.set_text("help");
        c.handle_key(KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(c.text(), "hel");
        assert_eq!(c.cursor(), 3);
    }

    #[test]
    fn ctrl_u_clears() {
        let mut c = Cmdline::new();
        c.set_text("half-typed");
        c.handle_key(KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert_eq!(c.text(), "");
        assert_eq!(c.cursor(), 0);
    }

    #[test]
    fn ctrl_w_deletes_word_back() {
        let mut c = Cmdline::new();
        c.set_text("cmd foo");
        c.handle_key(KeyCode::Char('w'), KeyModifiers::CONTROL);
        assert_eq!(c.text(), "cmd ");
    }

    #[test]
    fn home_end_move_cursor() {
        let mut c = Cmdline::new();
        c.set_text("abcd");
        c.handle_key(KeyCode::Home, KeyModifiers::NONE);
        assert_eq!(c.cursor(), 0);
        c.handle_key(KeyCode::End, KeyModifiers::NONE);
        assert_eq!(c.cursor(), 4);
    }

    #[test]
    fn history_up_down_cycles() {
        let mut c = Cmdline::new().with_history(vec!["first".into(), "second".into()]);
        c.handle_key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(c.text(), "second");
        c.handle_key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(c.text(), "first");
        c.handle_key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(c.text(), "second");
        c.handle_key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(c.text(), "");
    }

    #[test]
    fn cursor_info_tracks_display_width() {
        let mut c = Cmdline::new();
        c.set_text("ab");
        let ci = c.cursor_info_via_component();
        // Prompt (1) + "ab" (2) = col 3.
        assert_eq!(ci.col, 3);
        assert_eq!(ci.row, 0);
    }

    // Helper: component trait's `cursor` is shadowed by the inherent
    // `cursor(&self) -> usize`. Route through Component to test.
    impl Cmdline {
        fn cursor_info_via_component(&self) -> CursorInfo {
            <Self as Component>::cursor(self).expect("cmdline cursor")
        }
    }
}
