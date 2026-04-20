use crate::component::{Component, DrawContext, KeyResult};
use crate::grid::{GridSlice, Style};
use crate::layout::Rect;
use crossterm::event::{KeyCode, KeyModifiers};

pub struct ListItem {
    pub text: String,
    pub style: Style,
}

impl ListItem {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: Style::default(),
        }
    }

    pub fn styled(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }
}

pub struct ListSelect {
    items: Vec<ListItem>,
    selected: usize,
    scroll_offset: usize,
    indicator: &'static str,
    selected_style: Style,
}

impl ListSelect {
    pub fn new(items: Vec<ListItem>) -> Self {
        Self {
            items,
            selected: 0,
            scroll_offset: 0,
            indicator: "▸ ",
            selected_style: Style {
                bold: true,
                ..Style::default()
            },
        }
    }

    pub fn with_indicator(mut self, indicator: &'static str) -> Self {
        self.indicator = indicator;
        self
    }

    pub fn with_selected_style(mut self, style: Style) -> Self {
        self.selected_style = style;
        self
    }

    pub fn set_items(&mut self, items: Vec<ListItem>) {
        self.items = items;
        self.selected = 0;
        self.scroll_offset = 0;
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn selected_text(&self) -> Option<&str> {
        self.items.get(self.selected).map(|i| i.text.as_str())
    }

    pub fn select(&mut self, idx: usize) {
        if idx < self.items.len() {
            self.selected = idx;
        }
    }

    pub fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    pub fn move_down(&mut self) {
        if self.selected + 1 < self.items.len() {
            self.selected += 1;
        }
    }

    pub fn ensure_visible(&mut self, viewport_height: u16) {
        let vh = viewport_height as usize;
        if vh == 0 {
            return;
        }
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        } else if self.selected >= self.scroll_offset + vh {
            self.scroll_offset = self.selected - vh + 1;
        }
    }
}

impl Component for ListSelect {
    fn draw(&self, _area: Rect, grid: &mut GridSlice<'_>, _ctx: &DrawContext) {
        let h = grid.height();
        let w = grid.width();
        let indicator_len = self.indicator.chars().count() as u16;

        for row in 0..h {
            let idx = self.scroll_offset + row as usize;
            if idx >= self.items.len() {
                break;
            }
            let item = &self.items[idx];
            let is_selected = idx == self.selected;

            if is_selected {
                grid.put_str(0, row, self.indicator, self.selected_style);
                let style = Style {
                    fg: self.selected_style.fg.or(item.style.fg),
                    bg: self.selected_style.bg.or(item.style.bg),
                    bold: self.selected_style.bold || item.style.bold,
                    dim: item.style.dim,
                    italic: item.style.italic,
                    ..Style::default()
                };
                let max_text = (w.saturating_sub(indicator_len)) as usize;
                let truncated: String = item.text.chars().take(max_text).collect();
                grid.put_str(indicator_len, row, &truncated, style);
            } else {
                let padding: String = " ".repeat(indicator_len as usize);
                grid.put_str(0, row, &padding, Style::default());
                let max_text = (w.saturating_sub(indicator_len)) as usize;
                let truncated: String = item.text.chars().take(max_text).collect();
                grid.put_str(indicator_len, row, &truncated, item.style);
            }
        }
    }

    fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> KeyResult {
        match (code, mods) {
            (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                self.move_up();
                KeyResult::Consumed
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                self.move_down();
                KeyResult::Consumed
            }
            (KeyCode::Home, _) | (KeyCode::Char('g'), KeyModifiers::NONE) => {
                self.select(0);
                KeyResult::Consumed
            }
            (KeyCode::End, _) | (KeyCode::Char('G'), KeyModifiers::SHIFT) => {
                if !self.items.is_empty() {
                    self.select(self.items.len() - 1);
                }
                KeyResult::Consumed
            }
            (KeyCode::Enter, _) => KeyResult::Action("select".into()),
            (KeyCode::Esc, _) | (KeyCode::Char('q'), KeyModifiers::NONE) => {
                KeyResult::Action("dismiss".into())
            }
            (KeyCode::Char(c), KeyModifiers::NONE) if c.is_ascii_digit() => {
                let idx = c as usize - '0' as usize;
                if idx > 0 && idx <= self.items.len() {
                    self.select(idx - 1);
                    KeyResult::Action("select".into())
                } else {
                    KeyResult::Ignored
                }
            }
            _ => KeyResult::Ignored,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::Grid;

    fn make_list(items: &[&str]) -> ListSelect {
        let items = items.iter().map(|t| ListItem::plain(*t)).collect();
        ListSelect::new(items)
    }

    #[test]
    fn renders_items_with_indicator() {
        let list = make_list(&["alpha", "beta", "gamma"]);
        let mut grid = Grid::new(20, 3);
        let ctx = DrawContext {
            terminal_width: 20,
            terminal_height: 3,
            focused: true,
        };
        let mut slice = grid.slice_mut(Rect::new(0, 0, 20, 3));
        list.draw(Rect::new(0, 0, 20, 3), &mut slice, &ctx);
        assert_eq!(grid.cell(0, 0).symbol, '▸');
        assert_eq!(grid.cell(2, 0).symbol, 'a');
        assert_eq!(grid.cell(2, 1).symbol, 'b');
    }

    #[test]
    fn navigation_up_down() {
        let mut list = make_list(&["a", "b", "c"]);
        assert_eq!(list.selected(), 0);
        list.move_down();
        assert_eq!(list.selected(), 1);
        list.move_down();
        assert_eq!(list.selected(), 2);
        list.move_down();
        assert_eq!(list.selected(), 2);
        list.move_up();
        assert_eq!(list.selected(), 1);
    }

    #[test]
    fn enter_returns_select_action() {
        let mut list = make_list(&["a", "b"]);
        list.move_down();
        let result = list.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(result, KeyResult::Action("select".into()));
        assert_eq!(list.selected(), 1);
    }

    #[test]
    fn esc_returns_dismiss_action() {
        let mut list = make_list(&["a"]);
        let result = list.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(result, KeyResult::Action("dismiss".into()));
    }

    #[test]
    fn numeric_selection() {
        let mut list = make_list(&["a", "b", "c"]);
        let result = list.handle_key(KeyCode::Char('2'), KeyModifiers::NONE);
        assert_eq!(result, KeyResult::Action("select".into()));
        assert_eq!(list.selected(), 1);
    }
}
