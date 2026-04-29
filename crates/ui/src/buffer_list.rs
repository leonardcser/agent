//! `BufferList` — a `PanelWidget` for selectable lists backed by a
//! `Buffer`.
//!
//! Where `OptionList` builds its rows from in-memory `OptionItem`s,
//! `BufferList` mirrors a `Buffer`'s line content directly, with all the
//! decorations and per-line styling that come with it (formatter spans,
//! gutter/fill backgrounds, virtual text). It owns a `BufferView`
//! synced from the source `Buffer` each frame, an absolute selection
//! anchor decoupled from the viewport scroll, and its own scrollbar.

use crate::buffer::Buffer;
use crate::buffer_view::BufferView;
use crate::component::{Component, DrawContext, KeyResult, WidgetEvent};
use crate::dialog::{ListWidget, PanelWidget};
use crate::grid::{GridSlice, Style};
use crate::layout::Rect;
use crate::window::{ScrollbarState, WindowViewport};
use crate::BufId;
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

pub struct BufferList {
    buf: BufId,
    view: BufferView,
    /// Absolute row of the current selection. Decoupled from scroll
    /// (wheel / scrollbar drag does NOT move it) so the selected item
    /// stays put even when scrolled out of view.
    selection_abs: u16,
    /// Top of the viewport in line coordinates.
    scroll_top: u16,
    /// Total rows (refreshed in `sync_from_buffer`).
    line_count: u16,
    /// Rect from the last `prepare`. Used by `handle_mouse` to map
    /// absolute click rows into viewport-relative offsets.
    last_area: Rect,
    /// Resolved viewport (rect + scrollbar geometry) recomputed in
    /// `prepare` so callers (App scrollbar drag, Dialog query) can hit
    /// test against the same geometry the widget renders with.
    viewport: Option<WindowViewport>,
    cursor_style: Style,
    bg_style: Style,
    scrollbar_track_style: Style,
    scrollbar_thumb_style: Style,
}

impl BufferList {
    pub fn new(buf: BufId) -> Self {
        Self {
            buf,
            view: BufferView::new(),
            selection_abs: 0,
            scroll_top: 0,
            line_count: 0,
            last_area: Rect::new(0, 0, 0, 0),
            viewport: None,
            cursor_style: Style::default(),
            bg_style: Style::default(),
            scrollbar_track_style: Style::default(),
            scrollbar_thumb_style: Style::default(),
        }
    }

    pub fn with_cursor_style(mut self, style: Style) -> Self {
        self.cursor_style = style;
        self
    }

    pub fn with_bg_style(mut self, style: Style) -> Self {
        self.bg_style = style;
        self.view.set_default_style(style);
        self
    }

    pub fn with_scrollbar_styles(mut self, track: Style, thumb: Style) -> Self {
        self.scrollbar_track_style = track;
        self.scrollbar_thumb_style = thumb;
        self
    }

    pub fn buf(&self) -> BufId {
        self.buf
    }

    pub fn move_selection(&mut self, delta: isize) {
        let total = self.line_count as isize;
        if total == 0 {
            return;
        }
        let new = (self.selection_abs as isize + delta).clamp(0, total - 1);
        self.selection_abs = new as u16;
        self.ensure_visible();
    }

    fn ensure_visible(&mut self) {
        let rows = self.last_area.height;
        if rows == 0 {
            return;
        }
        let abs = self.selection_abs;
        if abs < self.scroll_top {
            self.scroll_top = abs;
        } else if abs >= self.scroll_top + rows {
            self.scroll_top = abs.saturating_sub(rows.saturating_sub(1));
        }
    }

    fn page_step(&self) -> isize {
        (self.last_area.height.max(1) as isize) / 2
    }

    fn paint_cursor(&self, slice: &mut GridSlice<'_>, content_w: u16) {
        let rows = self.last_area.height;
        if rows == 0 || self.selection_abs < self.scroll_top {
            return;
        }
        let rel = self.selection_abs - self.scroll_top;
        if rel >= rows {
            return;
        }
        let accent_fg = self.cursor_style.fg;
        for col in 0..content_w {
            let cell = slice.cell(col, rel);
            let style = Style {
                fg: accent_fg.or(cell.style.fg),
                ..cell.style
            };
            slice.set_style(col, rel, style);
        }
    }

    fn draw_scrollbar(&self, slice: &mut GridSlice<'_>, bar: ScrollbarState, scrollbar_col: u16) {
        let viewport_rows = bar.viewport_rows as usize;
        let thumb_size = bar.thumb_size() as usize;
        let max_thumb = bar.max_thumb_top() as usize;
        let max_scroll = bar.max_scroll() as usize;
        let scroll_top = self.scroll_top as usize;
        let thumb_top = (scroll_top * max_thumb + max_scroll / 2)
            .checked_div(max_scroll)
            .unwrap_or(0);
        let h = slice.height() as usize;
        for row in 0..viewport_rows.min(h) {
            let is_thumb = row >= thumb_top && row < thumb_top + thumb_size;
            let style = if is_thumb {
                self.scrollbar_thumb_style
            } else {
                self.scrollbar_track_style
            };
            slice.set(scrollbar_col, row as u16, ' ', style);
        }
    }
}

impl Component for BufferList {
    fn prepare(&mut self, area: Rect, _ctx: &DrawContext) {
        self.last_area = area;
        let viewport_rows = area.height;
        let total = self.line_count;
        let scroll_top = self.scroll_top.min(total.saturating_sub(viewport_rows));
        self.scroll_top = scroll_top;
        let scrollbar_col = area.left + area.width.saturating_sub(1);
        let scrollbar = ScrollbarState::new(scrollbar_col, total, viewport_rows);
        self.viewport = Some(WindowViewport::new(
            area, area.width, total, scroll_top, scrollbar,
        ));
        self.view.set_scroll(scroll_top as usize);
    }

    fn draw(&self, area: Rect, slice: &mut GridSlice<'_>, ctx: &DrawContext) {
        let w = slice.width();
        let h = slice.height();
        if w == 0 || h == 0 {
            return;
        }
        slice.fill(Rect::new(0, 0, w, h), ' ', self.bg_style);
        let scrollbar = self.viewport.and_then(|v| v.scrollbar);
        let content_w = if scrollbar.is_some() {
            w.saturating_sub(1)
        } else {
            w
        };
        if content_w > 0 {
            let mut content_slice = slice.sub_slice(Rect::new(0, 0, content_w, h));
            self.view.draw(area, &mut content_slice, ctx);
            self.paint_cursor(&mut content_slice, content_w);
        }
        if let Some(bar) = scrollbar {
            self.draw_scrollbar(slice, bar, w - 1);
        }
    }

    fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> KeyResult {
        match (code, mods) {
            (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                self.move_selection(-1);
                KeyResult::Consumed
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                self.move_selection(1);
                KeyResult::Consumed
            }
            (KeyCode::PageUp, _) | (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                let p = self.page_step();
                self.move_selection(-p);
                KeyResult::Consumed
            }
            (KeyCode::PageDown, _) | (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                let p = self.page_step();
                self.move_selection(p);
                KeyResult::Consumed
            }
            (KeyCode::Home, _) | (KeyCode::Char('g'), KeyModifiers::NONE) => {
                self.move_selection(isize::MIN / 2);
                KeyResult::Consumed
            }
            (KeyCode::End, _) | (KeyCode::Char('G'), KeyModifiers::SHIFT) => {
                self.move_selection(isize::MAX / 2);
                KeyResult::Consumed
            }
            (KeyCode::Enter, _) => {
                if self.line_count == 0 {
                    KeyResult::Ignored
                } else {
                    KeyResult::Action(WidgetEvent::Select(self.selection_abs as usize))
                }
            }
            _ => KeyResult::Ignored,
        }
    }

    fn handle_mouse(&mut self, event: MouseEvent) -> KeyResult {
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let rect = self.last_area;
                if rect.width == 0 || !rect.contains(event.row, event.column) {
                    return KeyResult::Ignored;
                }
                let rel = event.row - rect.top;
                if rel >= rect.height {
                    return KeyResult::Ignored;
                }
                let target = self.scroll_top + rel;
                if (target as usize) < self.line_count as usize {
                    self.selection_abs = target;
                }
                KeyResult::Consumed
            }
            MouseEventKind::ScrollUp => {
                self.scroll_top = self.scroll_top.saturating_sub(3);
                KeyResult::Consumed
            }
            MouseEventKind::ScrollDown => {
                let rows = self.last_area.height;
                let max_scroll = self.line_count.saturating_sub(rows);
                self.scroll_top = (self.scroll_top + 3).min(max_scroll);
                KeyResult::Consumed
            }
            _ => KeyResult::Ignored,
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

impl PanelWidget for BufferList {
    fn content_rows(&self) -> usize {
        self.line_count as usize
    }
    fn selected_index(&self) -> Option<usize> {
        if self.line_count == 0 {
            None
        } else {
            Some(self.selection_abs as usize)
        }
    }
    fn as_list_widget(&mut self) -> Option<&mut dyn ListWidget> {
        Some(self)
    }
    fn viewport(&self) -> Option<WindowViewport> {
        self.viewport
    }
    fn apply_scrollbar_drag(&mut self, thumb_top: u16) -> bool {
        let Some(viewport) = self.viewport else {
            return false;
        };
        let Some(bar) = viewport.scrollbar else {
            return false;
        };
        let thumb_top = thumb_top.min(bar.max_thumb_top());
        let from_top = bar.scroll_from_top_for_thumb(thumb_top);
        self.scroll_top = from_top;
        self.view.set_scroll(from_top as usize);
        true
    }
    fn scroll_by(&mut self, delta: isize) -> isize {
        let rows = self.last_area.height as isize;
        let total = self.line_count as isize;
        let max_scroll = (total - rows).max(0);
        let cur = self.scroll_top as isize;
        let new = (cur + delta).clamp(0, max_scroll);
        if new == cur {
            return 0;
        }
        self.scroll_top = new as u16;
        self.view.set_scroll(new as usize);
        new - cur
    }
}

impl ListWidget for BufferList {
    fn row_count(&self) -> usize {
        self.line_count as usize
    }
    fn selected(&self) -> Option<usize> {
        if self.line_count == 0 {
            None
        } else {
            Some(self.selection_abs as usize)
        }
    }
    fn set_selected(&mut self, idx: usize) {
        if self.line_count == 0 {
            return;
        }
        self.selection_abs = (idx as u16).min(self.line_count - 1);
        self.ensure_visible();
    }
    fn scroll_top(&self) -> usize {
        self.scroll_top as usize
    }
    fn set_scroll_top(&mut self, top: usize) {
        let rows = self.last_area.height;
        let max_scroll = self.line_count.saturating_sub(rows);
        self.scroll_top = (top as u16).min(max_scroll);
    }
    fn row_at(&self, rel_row: u16) -> Option<usize> {
        let target = self.scroll_top + rel_row;
        ((target as usize) < self.line_count as usize).then_some(target as usize)
    }
    fn buf_id(&self) -> Option<BufId> {
        Some(self.buf)
    }
    fn sync_from_buffer(&mut self, buf: &Buffer) {
        self.line_count = buf.line_count() as u16;
        self.view.sync_from_buffer(buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::BufCreateOpts;
    use crate::grid::Grid;
    use crossterm::event::{KeyEventKind, KeyEventState, MouseEvent};

    fn make_buf(lines: &[&str]) -> Buffer {
        let mut buf = Buffer::new(BufId(1), BufCreateOpts::default());
        buf.set_all_lines(lines.iter().map(|s| s.to_string()).collect());
        buf
    }

    fn ctx(w: u16, h: u16) -> DrawContext {
        DrawContext {
            terminal_width: w,
            terminal_height: h,
            focused: true,
            selection_style: Default::default(),
        }
    }

    fn mouse(kind: MouseEventKind, row: u16, col: u16) -> MouseEvent {
        MouseEvent {
            kind,
            row,
            column: col,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn renders_buffer_lines_with_cursor_tint() {
        use crossterm::style::Color;
        let buf = make_buf(&["alpha", "beta", "gamma"]);
        let mut list = BufferList::new(BufId(1)).with_cursor_style(Style::fg(Color::Red));
        list.sync_from_buffer(&buf);
        list.prepare(Rect::new(0, 0, 10, 3), &ctx(10, 3));

        let mut grid = Grid::new(10, 3);
        let mut slice = grid.slice_mut(Rect::new(0, 0, 10, 3));
        list.draw(Rect::new(0, 0, 10, 3), &mut slice, &ctx(10, 3));
        assert_eq!(grid.cell(0, 0).symbol, 'a');
        assert_eq!(grid.cell(0, 0).style.fg, Some(Color::Red));
        assert_eq!(grid.cell(0, 1).symbol, 'b');
        assert_eq!(grid.cell(0, 1).style.fg, None);
    }

    #[test]
    fn arrow_keys_move_selection() {
        let buf = make_buf(&["a", "b", "c"]);
        let mut list = BufferList::new(BufId(1));
        list.sync_from_buffer(&buf);
        list.prepare(Rect::new(0, 0, 10, 3), &ctx(10, 3));

        let _ = KeyEventKind::Press;
        let _ = KeyEventState::NONE;
        assert_eq!(list.selection_abs, 0);
        list.handle_key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(list.selection_abs, 1);
        list.handle_key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(list.selection_abs, 2);
        list.handle_key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(list.selection_abs, 2, "clamped at last");
        list.handle_key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(list.selection_abs, 1);
    }

    #[test]
    fn enter_emits_select_with_index() {
        let buf = make_buf(&["a", "b", "c"]);
        let mut list = BufferList::new(BufId(1));
        list.sync_from_buffer(&buf);
        list.prepare(Rect::new(0, 0, 10, 3), &ctx(10, 3));
        list.set_selected(2);
        let r = list.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(r, KeyResult::Action(WidgetEvent::Select(2)));
    }

    #[test]
    fn click_sets_selection_via_rel_row() {
        let buf = make_buf(&["a", "b", "c", "d"]);
        let mut list = BufferList::new(BufId(1));
        list.sync_from_buffer(&buf);
        list.prepare(Rect::new(2, 0, 10, 3), &ctx(10, 5));
        let r = list.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 4, 3));
        assert_eq!(r, KeyResult::Consumed);
        assert_eq!(list.selection_abs, 2, "row 4 - top 2 = rel 2 → idx 2");
    }

    #[test]
    fn scroll_wheel_does_not_move_selection() {
        let buf = make_buf(
            &(0..20)
                .map(|i| format!("l{i}"))
                .collect::<Vec<_>>()
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>(),
        );
        let mut list = BufferList::new(BufId(1));
        list.sync_from_buffer(&buf);
        list.prepare(Rect::new(0, 0, 10, 5), &ctx(10, 5));
        list.set_selected(0);
        let r = list.handle_mouse(mouse(MouseEventKind::ScrollDown, 2, 3));
        assert_eq!(r, KeyResult::Consumed);
        assert_eq!(list.scroll_top, 3);
        assert_eq!(list.selection_abs, 0, "selection stays put on wheel");
    }

    #[test]
    fn scrollbar_drag_snaps_scroll() {
        let buf = make_buf(
            &(0..20)
                .map(|i| format!("l{i}"))
                .collect::<Vec<_>>()
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>(),
        );
        let mut list = BufferList::new(BufId(1));
        list.sync_from_buffer(&buf);
        list.prepare(Rect::new(0, 0, 10, 5), &ctx(10, 5));
        let bar = list.viewport.unwrap().scrollbar.unwrap();
        assert!(list.apply_scrollbar_drag(bar.max_thumb_top()));
        assert_eq!(list.scroll_top, bar.max_scroll());
    }
}
