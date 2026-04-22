use crate::buffer::Buffer;
use crate::buffer_view::BufferView;
use crate::component::{Component, CursorInfo, DrawContext, KeyResult, WidgetEvent};
use crate::grid::{GridSlice, Style};
use crate::layout::{Border, Rect};
use crate::list_select::{ListItem, ListSelect};
use crate::text_input::TextInput;
use crossterm::event::{KeyCode, KeyModifiers};

pub struct FloatDialogConfig {
    pub title: Option<String>,
    pub border: Border,
    pub border_style: Style,
    pub background_style: Style,
    pub max_height: Option<u16>,
    pub hint_left: Option<String>,
    pub hint_right: Option<String>,
    pub hint_style: Style,
    pub accent_style: Style,
    pub footer_height: Option<u16>,
    pub dismiss_keys: Vec<(KeyCode, KeyModifiers)>,
}

impl Default for FloatDialogConfig {
    fn default() -> Self {
        Self {
            title: None,
            border: Border::Rounded,
            border_style: Style::default(),
            background_style: Style::default(),
            max_height: None,
            hint_left: None,
            hint_right: None,
            hint_style: Style {
                dim: true,
                ..Style::default()
            },
            accent_style: Style::default(),
            footer_height: None,
            dismiss_keys: Vec::new(),
        }
    }
}

enum Focus {
    Content,
    Footer,
    Input,
}

pub struct FloatDialog {
    content: BufferView,
    footer: Option<ListSelect>,
    input: Option<TextInput>,
    config: FloatDialogConfig,
    focus: Focus,
}

impl FloatDialog {
    pub fn new(config: FloatDialogConfig) -> Self {
        let mut content = BufferView::new();
        content.set_border(Border::None);
        Self {
            content,
            footer: None,
            input: None,
            focus: Focus::Content,
            config,
        }
    }

    pub fn content_only(config: FloatDialogConfig) -> Self {
        Self::new(config)
    }

    pub fn with_footer(mut self, items: Vec<ListItem>) -> Self {
        self.footer = Some(ListSelect::new(items));
        self.focus = Focus::Footer;
        self
    }

    pub fn with_input(mut self, placeholder: impl Into<String>) -> Self {
        self.input = Some(TextInput::new().with_placeholder(placeholder));
        self.focus = Focus::Input;
        self
    }

    pub fn content_mut(&mut self) -> &mut BufferView {
        &mut self.content
    }

    pub fn footer_mut(&mut self) -> Option<&mut ListSelect> {
        self.footer.as_mut()
    }

    pub fn input_mut(&mut self) -> Option<&mut TextInput> {
        self.input.as_mut()
    }

    pub fn set_content_lines(&mut self, lines: Vec<String>) {
        self.content.set_lines(lines);
    }

    pub fn set_footer_items(&mut self, items: Vec<ListItem>) {
        if let Some(ref mut footer) = self.footer {
            footer.set_items(items);
        } else {
            self.footer = Some(ListSelect::new(items));
            if matches!(self.focus, Focus::Content) {
                self.focus = Focus::Footer;
            }
        }
    }

    pub fn sync_content_from_buffer(&mut self, buf: &Buffer) {
        self.content.sync_from_buffer(buf);
    }

    pub fn selected(&self) -> Option<usize> {
        self.footer.as_ref().map(|f| f.selected())
    }

    pub fn selected_text(&self) -> Option<&str> {
        self.footer.as_ref().and_then(|f| f.selected_text())
    }

    pub fn input_text(&self) -> Option<&str> {
        self.input.as_ref().map(|i| i.text())
    }

    pub fn config(&self) -> &FloatDialogConfig {
        &self.config
    }

    pub fn config_mut(&mut self) -> &mut FloatDialogConfig {
        &mut self.config
    }

    fn layout_regions(&self, area: Rect) -> DialogLayout {
        let has_border = self.config.border != Border::None;
        let chrome = if has_border { 2u16 } else { 0 };

        let inner_w = area.width.saturating_sub(chrome);
        let inner_h = area.height.saturating_sub(chrome);
        let offset_x = if has_border { 1 } else { 0 };
        let offset_y = if has_border { 1 } else { 0 };

        let has_hints = self.config.hint_left.is_some() || self.config.hint_right.is_some();
        let hint_rows = if has_hints { 1u16 } else { 0 };
        let input_rows = if self.input.is_some() { 1u16 } else { 0 };

        let footer_rows = if let Some(footer_height) = self.config.footer_height {
            footer_height
        } else if let Some(ref list) = self.footer {
            let items_len = list.len() as u16;
            items_len.min(inner_h.saturating_sub(hint_rows + input_rows + 1))
        } else {
            0
        };

        let separator_rows = if footer_rows > 0 || input_rows > 0 {
            1u16
        } else {
            0
        };
        let content_h = inner_h
            .saturating_sub(footer_rows)
            .saturating_sub(separator_rows)
            .saturating_sub(input_rows)
            .saturating_sub(hint_rows);

        let mut y = offset_y;

        let content_rect = Rect::new(area.top + y, area.left + offset_x, inner_w, content_h);
        y += content_h;

        let separator_y = if separator_rows > 0 {
            let sy = area.top + y;
            y += separator_rows;
            Some(sy)
        } else {
            None
        };

        let footer_rect = if footer_rows > 0 {
            let r = Rect::new(area.top + y, area.left + offset_x, inner_w, footer_rows);
            y += footer_rows;
            Some(r)
        } else {
            None
        };

        let input_rect = if input_rows > 0 {
            let r = Rect::new(area.top + y, area.left + offset_x, inner_w, input_rows);
            y += input_rows;
            Some(r)
        } else {
            None
        };

        let hint_rect = if has_hints {
            Some(Rect::new(area.top + y, area.left + offset_x, inner_w, 1))
        } else {
            None
        };

        DialogLayout {
            content: content_rect,
            separator_y,
            footer: footer_rect,
            input: input_rect,
            hints: hint_rect,
        }
    }

    fn draw_border(&self, grid: &mut GridSlice<'_>) {
        let border = self.config.border;
        let (h, v, tl, tr, bl, br) = match border {
            Border::None => return,
            Border::Single => ('─', '│', '┌', '┐', '└', '┘'),
            Border::Double => ('═', '║', '╔', '╗', '╚', '╝'),
            Border::Rounded => ('─', '│', '╭', '╮', '╰', '╯'),
        };
        let w = grid.width();
        let h_total = grid.height();
        if w < 2 || h_total < 2 {
            return;
        }

        let style = self.config.border_style;

        grid.set(0, 0, tl, style);
        if let Some(ref title) = self.config.title {
            grid.set(1, 0, h, style);
            let max_title = (w as usize).saturating_sub(4);
            let title_style = Style {
                bold: true,
                ..self.config.accent_style
            };
            for (i, ch) in title.chars().take(max_title).enumerate() {
                grid.set(2 + i as u16, 0, ch, title_style);
            }
            let title_len = title.chars().take(max_title).count();
            grid.set(2 + title_len as u16, 0, h, style);
            for col in (3 + title_len as u16)..w.saturating_sub(1) {
                grid.set(col, 0, h, style);
            }
        } else {
            for col in 1..w.saturating_sub(1) {
                grid.set(col, 0, h, style);
            }
        }
        grid.set(w - 1, 0, tr, style);

        for row in 1..h_total.saturating_sub(1) {
            grid.set(0, row, v, style);
            grid.set(w - 1, row, v, style);
        }

        grid.set(0, h_total - 1, bl, style);
        for col in 1..w.saturating_sub(1) {
            grid.set(col, h_total - 1, h, style);
        }
        grid.set(w - 1, h_total - 1, br, style);
    }

    fn draw_separator(&self, grid: &mut GridSlice<'_>, y: u16) {
        let w = grid.width();
        let has_border = self.config.border != Border::None;
        let style = self.config.border_style;

        if has_border {
            let (left_t, right_t, h_char) = match self.config.border {
                Border::Single => ('├', '┤', '─'),
                Border::Double => ('╠', '╣', '═'),
                Border::Rounded => ('├', '┤', '─'),
                Border::None => return,
            };
            grid.set(0, y, left_t, style);
            for col in 1..w.saturating_sub(1) {
                grid.set(col, y, h_char, style);
            }
            grid.set(w - 1, y, right_t, style);
        } else {
            for col in 0..w {
                grid.set(col, y, '─', style);
            }
        }
    }

    fn draw_hints(&self, grid: &mut GridSlice<'_>, rect: Rect, area: Rect) {
        let y = rect.top.saturating_sub(area.top);
        let x = rect.left.saturating_sub(area.left);
        let w = rect.width;
        let style = self.config.hint_style;

        if let Some(ref hint_left) = self.config.hint_left {
            for (i, ch) in hint_left.chars().enumerate() {
                if i as u16 >= w {
                    break;
                }
                grid.set(x + i as u16, y, ch, style);
            }
        }

        if let Some(ref hint_right) = self.config.hint_right {
            let len = hint_right.chars().count() as u16;
            let start = w.saturating_sub(len);
            for (i, ch) in hint_right.chars().enumerate() {
                let col = x + start + i as u16;
                if col >= x + w {
                    break;
                }
                grid.set(col, y, ch, style);
            }
        }
    }

    fn scroll_content_up(&mut self) {
        let offset = self.content.scroll_offset();
        if offset > 0 {
            self.content.set_scroll(offset - 1);
        }
    }

    fn scroll_content_down(&mut self) {
        let offset = self.content.scroll_offset();
        self.content.set_scroll(offset + 1);
    }

    fn scroll_content_half_page(&mut self, up: bool, viewport_h: u16) {
        let half = (viewport_h / 2).max(1) as usize;
        let offset = self.content.scroll_offset();
        if up {
            self.content.set_scroll(offset.saturating_sub(half));
        } else {
            self.content.set_scroll(offset + half);
        }
    }
}

impl Component for FloatDialog {
    fn draw(&self, area: Rect, grid: &mut GridSlice<'_>, ctx: &DrawContext) {
        let w = grid.width();
        let h = grid.height();
        if w > 0 && h > 0 {
            grid.fill(Rect::new(0, 0, w, h), ' ', self.config.background_style);
        }

        self.draw_border(grid);

        let layout = self.layout_regions(area);

        // Draw content
        {
            let rel = Rect::new(
                layout.content.top.saturating_sub(area.top),
                layout.content.left.saturating_sub(area.left),
                layout.content.width,
                layout.content.height,
            );
            if rel.width > 0 && rel.height > 0 {
                let mut slice = grid.sub_slice(rel);
                self.content.draw(layout.content, &mut slice, ctx);
            }
        }

        // Draw separator
        if let Some(sep_y) = layout.separator_y {
            self.draw_separator(grid, sep_y.saturating_sub(area.top));
        }

        // Draw footer
        if let (Some(ref footer), Some(footer_rect)) = (&self.footer, layout.footer) {
            let rel = Rect::new(
                footer_rect.top.saturating_sub(area.top),
                footer_rect.left.saturating_sub(area.left),
                footer_rect.width,
                footer_rect.height,
            );
            if rel.width > 0 && rel.height > 0 {
                let mut slice = grid.sub_slice(rel);
                footer.draw(footer_rect, &mut slice, ctx);
            }
        }

        // Draw input
        if let (Some(ref input), Some(input_rect)) = (&self.input, layout.input) {
            let rel = Rect::new(
                input_rect.top.saturating_sub(area.top),
                input_rect.left.saturating_sub(area.left),
                input_rect.width,
                input_rect.height,
            );
            if rel.width > 0 && rel.height > 0 {
                let mut slice = grid.sub_slice(rel);
                input.draw(input_rect, &mut slice, ctx);
            }
        }

        // Draw hints
        if let Some(hint_rect) = layout.hints {
            self.draw_hints(grid, hint_rect, area);
        }
    }

    fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> KeyResult {
        // Global dismiss
        if matches!(code, KeyCode::Esc) && mods == KeyModifiers::NONE {
            return KeyResult::Action(WidgetEvent::Dismiss);
        }
        if self
            .config
            .dismiss_keys
            .iter()
            .any(|&(k, m)| k == code && m == mods)
        {
            return KeyResult::Action(WidgetEvent::Dismiss);
        }

        // Route to focused sub-component first
        match self.focus {
            Focus::Input => {
                if let Some(ref mut input) = self.input {
                    let result = input.handle_key(code, mods);
                    match &result {
                        KeyResult::Action(WidgetEvent::Submit) => {
                            let text = input.text().to_string();
                            return KeyResult::Action(WidgetEvent::SubmitText(text));
                        }
                        KeyResult::Action(WidgetEvent::Cancel) => {
                            return KeyResult::Action(WidgetEvent::Dismiss);
                        }
                        KeyResult::Consumed => {
                            return KeyResult::Consumed;
                        }
                        _ => {}
                    }
                }
            }
            Focus::Footer => {
                if let Some(ref mut footer) = self.footer {
                    let result = footer.handle_key(code, mods);
                    match &result {
                        KeyResult::Action(WidgetEvent::SelectDefault) => {
                            let idx = footer.selected();
                            return KeyResult::Action(WidgetEvent::Select(idx));
                        }
                        KeyResult::Action(WidgetEvent::Dismiss) => {
                            return KeyResult::Action(WidgetEvent::Dismiss);
                        }
                        KeyResult::Consumed => {
                            return KeyResult::Consumed;
                        }
                        _ => {}
                    }
                }
            }
            Focus::Content => {}
        }

        // Content scroll keys (available regardless of focus)
        match (code, mods) {
            (KeyCode::Up, KeyModifiers::NONE) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                if matches!(self.focus, Focus::Content) {
                    self.scroll_content_up();
                    return KeyResult::Consumed;
                }
            }
            (KeyCode::Down, KeyModifiers::NONE) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                if matches!(self.focus, Focus::Content) {
                    self.scroll_content_down();
                    return KeyResult::Consumed;
                }
            }
            (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                self.scroll_content_half_page(true, 10);
                return KeyResult::Consumed;
            }
            (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                self.scroll_content_half_page(false, 10);
                return KeyResult::Consumed;
            }
            (KeyCode::Home, _) | (KeyCode::Char('g'), KeyModifiers::NONE) => {
                if matches!(self.focus, Focus::Content) {
                    self.content.set_scroll(0);
                    return KeyResult::Consumed;
                }
            }
            (KeyCode::End, _) | (KeyCode::Char('G'), KeyModifiers::SHIFT) => {
                if matches!(self.focus, Focus::Content) {
                    let lines = self.content.line_count();
                    if lines > 0 {
                        self.content.set_scroll(lines.saturating_sub(1));
                    }
                    return KeyResult::Consumed;
                }
            }
            (KeyCode::Tab, KeyModifiers::NONE) => {
                self.cycle_focus_forward();
                return KeyResult::Consumed;
            }
            (KeyCode::BackTab, KeyModifiers::SHIFT) => {
                self.cycle_focus_backward();
                return KeyResult::Consumed;
            }
            _ => {}
        }

        KeyResult::Ignored
    }

    fn cursor(&self) -> Option<CursorInfo> {
        if let Focus::Input = self.focus {
            self.input.as_ref().and_then(|i| i.cursor())
        } else {
            None
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

impl FloatDialog {
    fn cycle_focus_forward(&mut self) {
        self.focus = match self.focus {
            Focus::Content => {
                if self.footer.is_some() {
                    Focus::Footer
                } else if self.input.is_some() {
                    Focus::Input
                } else {
                    Focus::Content
                }
            }
            Focus::Footer => {
                if self.input.is_some() {
                    Focus::Input
                } else {
                    Focus::Content
                }
            }
            Focus::Input => Focus::Content,
        };
    }

    fn cycle_focus_backward(&mut self) {
        self.focus = match self.focus {
            Focus::Content => {
                if self.input.is_some() {
                    Focus::Input
                } else if self.footer.is_some() {
                    Focus::Footer
                } else {
                    Focus::Content
                }
            }
            Focus::Footer => Focus::Content,
            Focus::Input => {
                if self.footer.is_some() {
                    Focus::Footer
                } else {
                    Focus::Content
                }
            }
        };
    }
}

struct DialogLayout {
    content: Rect,
    separator_y: Option<u16>,
    footer: Option<Rect>,
    input: Option<Rect>,
    hints: Option<Rect>,
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
        }
    }

    #[test]
    fn background_fills_dialog_rect() {
        use crossterm::style::Color;
        let bg = Style {
            bg: Some(Color::Blue),
            ..Default::default()
        };
        let dialog = FloatDialog::new(FloatDialogConfig {
            border: Border::None,
            background_style: bg,
            ..Default::default()
        });

        let mut grid = Grid::new(10, 4);
        let area = Rect::new(0, 0, 10, 4);
        let c = ctx(10, 4);
        let mut slice = grid.slice_mut(area);
        dialog.draw(area, &mut slice, &c);

        for y in 0..4 {
            for x in 0..10 {
                assert_eq!(
                    grid.cell(x, y).style.bg,
                    Some(Color::Blue),
                    "cell ({x},{y}) missing bg"
                );
            }
        }
    }

    #[test]
    fn content_only_dialog() {
        let mut dialog = FloatDialog::new(FloatDialogConfig {
            title: Some("Help".into()),
            border: Border::Rounded,
            ..Default::default()
        });
        dialog.set_content_lines(vec!["line 1".into(), "line 2".into(), "line 3".into()]);

        let mut grid = Grid::new(30, 7);
        let area = Rect::new(0, 0, 30, 7);
        let c = ctx(30, 7);
        let mut slice = grid.slice_mut(area);
        dialog.draw(area, &mut slice, &c);

        assert_eq!(grid.cell(0, 0).symbol, '╭');
        assert_eq!(grid.cell(29, 0).symbol, '╮');
        assert_eq!(grid.cell(0, 6).symbol, '╰');
        assert_eq!(grid.cell(2, 0).symbol, 'H');
        assert_eq!(grid.cell(1, 1).symbol, 'l');
    }

    #[test]
    fn dialog_with_footer() {
        let dialog = FloatDialog::new(FloatDialogConfig {
            title: Some("Export".into()),
            border: Border::Rounded,
            footer_height: Some(2),
            ..Default::default()
        })
        .with_footer(vec![ListItem::plain("Clipboard"), ListItem::plain("File")]);

        let mut grid = Grid::new(30, 8);
        let area = Rect::new(0, 0, 30, 8);
        let c = ctx(30, 8);
        let mut slice = grid.slice_mut(area);
        dialog.draw(area, &mut slice, &c);

        assert_eq!(grid.cell(0, 0).symbol, '╭');
        // Footer items should be rendered
        let layout = dialog.layout_regions(area);
        assert!(layout.footer.is_some());
        assert!(layout.separator_y.is_some());
    }

    #[test]
    fn dialog_with_input() {
        let dialog = FloatDialog::new(FloatDialogConfig {
            title: Some("Search".into()),
            border: Border::Rounded,
            ..Default::default()
        })
        .with_input("type to search...");

        let mut grid = Grid::new(30, 6);
        let area = Rect::new(0, 0, 30, 6);
        let c = ctx(30, 6);
        let mut slice = grid.slice_mut(area);
        dialog.draw(area, &mut slice, &c);

        assert!(dialog.input.is_some());
        let layout = dialog.layout_regions(area);
        assert!(layout.input.is_some());
    }

    #[test]
    fn esc_dismisses() {
        let mut dialog = FloatDialog::new(FloatDialogConfig::default());
        let result = dialog.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(result, KeyResult::Action(WidgetEvent::Dismiss));
    }

    #[test]
    fn footer_select_returns_index() {
        let mut dialog = FloatDialog::new(FloatDialogConfig {
            footer_height: Some(3),
            ..Default::default()
        })
        .with_footer(vec![
            ListItem::plain("A"),
            ListItem::plain("B"),
            ListItem::plain("C"),
        ]);

        // Move down then enter
        dialog.handle_key(KeyCode::Down, KeyModifiers::NONE);
        let result = dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(result, KeyResult::Action(WidgetEvent::Select(1)));
    }

    #[test]
    fn input_submit_returns_text() {
        let mut dialog = FloatDialog::new(FloatDialogConfig::default()).with_input("message...");

        dialog.handle_key(KeyCode::Char('h'), KeyModifiers::NONE);
        dialog.handle_key(KeyCode::Char('i'), KeyModifiers::NONE);
        let result = dialog.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            result,
            KeyResult::Action(WidgetEvent::SubmitText("hi".into()))
        );
    }

    #[test]
    fn content_scroll() {
        let mut dialog = FloatDialog::new(FloatDialogConfig::default());
        dialog.set_content_lines((0..20).map(|i| format!("line {i}")).collect());
        // Focus is on content by default (no footer/input)
        assert_eq!(dialog.content.scroll_offset(), 0);

        dialog.handle_key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(dialog.content.scroll_offset(), 1);

        dialog.handle_key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(dialog.content.scroll_offset(), 0);
    }

    #[test]
    fn tab_cycles_focus() {
        let mut dialog = FloatDialog::new(FloatDialogConfig {
            footer_height: Some(2),
            ..Default::default()
        })
        .with_footer(vec![ListItem::plain("A")])
        .with_input("msg");

        // Initial focus is on input (last with_* call)
        assert!(matches!(dialog.focus, Focus::Input));

        dialog.handle_key(KeyCode::Tab, KeyModifiers::NONE);
        assert!(matches!(dialog.focus, Focus::Content));

        dialog.handle_key(KeyCode::Tab, KeyModifiers::NONE);
        assert!(matches!(dialog.focus, Focus::Footer));

        dialog.handle_key(KeyCode::Tab, KeyModifiers::NONE);
        assert!(matches!(dialog.focus, Focus::Input));
    }

    #[test]
    fn numeric_footer_select() {
        let mut dialog = FloatDialog::new(FloatDialogConfig {
            footer_height: Some(3),
            ..Default::default()
        })
        .with_footer(vec![
            ListItem::plain("A"),
            ListItem::plain("B"),
            ListItem::plain("C"),
        ]);

        let result = dialog.handle_key(KeyCode::Char('2'), KeyModifiers::NONE);
        assert_eq!(result, KeyResult::Action(WidgetEvent::Select(1)));
    }

    #[test]
    fn half_page_scroll() {
        let mut dialog = FloatDialog::new(FloatDialogConfig::default());
        dialog.set_content_lines((0..50).map(|i| format!("line {i}")).collect());

        dialog.handle_key(KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert!(dialog.content.scroll_offset() > 0);

        let offset = dialog.content.scroll_offset();
        dialog.handle_key(KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert!(dialog.content.scroll_offset() < offset);
    }

    #[test]
    fn separator_drawn_with_footer() {
        let dialog = FloatDialog::new(FloatDialogConfig {
            border: Border::Rounded,
            footer_height: Some(2),
            ..Default::default()
        })
        .with_footer(vec![ListItem::plain("A"), ListItem::plain("B")]);

        let mut grid = Grid::new(20, 8);
        let area = Rect::new(0, 0, 20, 8);
        let c = ctx(20, 8);
        let mut slice = grid.slice_mut(area);
        dialog.draw(area, &mut slice, &c);

        let layout = dialog.layout_regions(area);
        let sep_y = layout.separator_y.unwrap();
        let rel_y = sep_y - area.top;
        assert_eq!(grid.cell(0, rel_y).symbol, '├');
        assert_eq!(grid.cell(19, rel_y).symbol, '┤');
        assert_eq!(grid.cell(1, rel_y).symbol, '─');
    }

    #[test]
    fn hints_rendered() {
        let mut dialog = FloatDialog::new(FloatDialogConfig {
            border: Border::Rounded,
            hint_left: Some("ESC close".into()),
            hint_right: Some("Enter select".into()),
            ..Default::default()
        });
        dialog.set_content_lines(vec!["hello".into()]);

        let mut grid = Grid::new(40, 5);
        let area = Rect::new(0, 0, 40, 5);
        let c = ctx(40, 5);
        let mut slice = grid.slice_mut(area);
        dialog.draw(area, &mut slice, &c);

        let layout = dialog.layout_regions(area);
        if let Some(hint_rect) = layout.hints {
            let hy = hint_rect.top - area.top;
            let hx = hint_rect.left - area.left;
            assert_eq!(grid.cell(hx, hy).symbol, 'E');
        }
    }
}
