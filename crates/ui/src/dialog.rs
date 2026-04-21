//! Dialog: a compositor float built from a vertical stack of panels.
//!
//! A `Dialog` is the single component behind every built-in modal, the
//! completer, the cmdline, and Lua floats. Its visual language is the
//! legacy "docked panel" look: one accent `─` rule at the top, dashed
//! `╌` separators between panels, a `StatusBar` hints row at the
//! bottom, a solid background fill, and no side or bottom edges.
//!
//! Every panel is a real `ui::Window` backed by a `ui::Buffer`. The
//! panel's window holds its cursor, scroll, selection anchor — all the
//! interaction state a terminal window has. Keys and mouse route to
//! the focused panel's window; scrollbar, cursor overlay, and
//! line-decoration-based selection highlight fall out of the buffer
//! and window models. The dialog component itself only draws chrome
//! and orchestrates focus.

use crate::buffer::{Buffer, LineDecoration};
use crate::buffer_view::BufferView;
use crate::component::{Component, CursorInfo, DrawContext, KeyResult};
use crate::grid::{GridSlice, Style};
use crate::layout::Rect;
use crate::status_bar::StatusBar;
use crate::window::{ScrollbarState, Window, WindowViewport};
use crate::BufId;
use crossterm::event::{KeyCode, KeyModifiers};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelKind {
    /// Readonly text or preview. Scrollable, selectable via vim.
    Content,
    /// Selectable rows. Cursor line = current selection. Enter
    /// returns `select:{idx}` from `handle_key`.
    List { multi: bool },
    /// Editable buffer. Enter returns `submit:{text}`.
    Input { multiline: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelHeight {
    /// Exact row count.
    Fixed(u16),
    /// Shrink to content (capped by remaining space).
    Fit,
    /// Consume whatever remains after Fixed/Fit panels are allocated.
    Fill,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeparatorStyle {
    /// `╌` — weak, between-sections.
    Dashed,
    /// `─` — strong (rarely used between panels; reserved for top
    /// rule).
    Solid,
}

/// Description of a panel passed to `Ui::dialog_open`. The dialog
/// instantiates an internal `ui::Window` from this spec; the buffer
/// stays in the Ui registry.
pub struct PanelSpec {
    pub buf: BufId,
    pub kind: PanelKind,
    pub height: PanelHeight,
    pub separator_above: Option<SeparatorStyle>,
    /// Left content padding inside the panel's rect.
    pub pad_left: u16,
    /// Whether this panel participates in focus cycling. Title/summary
    /// panels usually don't.
    pub focusable: bool,
    /// Hide the panel when its buffer has zero non-blank content.
    pub collapse_when_empty: bool,
}

impl PanelSpec {
    pub fn content(buf: BufId, height: PanelHeight) -> Self {
        Self {
            buf,
            kind: PanelKind::Content,
            height,
            separator_above: None,
            pad_left: 1,
            focusable: false,
            collapse_when_empty: false,
        }
    }

    pub fn list(buf: BufId, height: PanelHeight) -> Self {
        Self {
            buf,
            kind: PanelKind::List { multi: false },
            height,
            separator_above: None,
            pad_left: 2,
            focusable: true,
            collapse_when_empty: false,
        }
    }

    pub fn input(buf: BufId, height: PanelHeight, multiline: bool) -> Self {
        Self {
            buf,
            kind: PanelKind::Input { multiline },
            height,
            separator_above: None,
            pad_left: 1,
            focusable: true,
            collapse_when_empty: false,
        }
    }

    pub fn with_separator(mut self, sep: SeparatorStyle) -> Self {
        self.separator_above = Some(sep);
        self
    }

    pub fn with_pad_left(mut self, pad: u16) -> Self {
        self.pad_left = pad;
        self
    }

    pub fn focusable(mut self, focusable: bool) -> Self {
        self.focusable = focusable;
        self
    }
}

#[derive(Default)]
pub struct DialogConfig {
    /// Top rule + accent elements (title-in-body tint).
    pub accent_style: Style,
    /// Dashed separators between panels.
    pub separator_style: Style,
    /// Background fill across the dialog rect.
    pub background_style: Style,
    /// Background color for scrollbar track.
    pub scrollbar_track_style: Style,
    /// Background color for scrollbar thumb.
    pub scrollbar_thumb_style: Style,
    /// Extra keys that dismiss the dialog (beyond Esc).
    pub dismiss_keys: Vec<(KeyCode, KeyModifiers)>,
    /// Hints row content. `None` hides the row.
    pub hints: Option<StatusBar>,
}

/// Internal panel state held by `Dialog`. The `Window` lives here,
/// not in `Ui::wins`: panels are dialog-local interaction, the buffer
/// in `Ui::bufs` is the shared piece.
pub(crate) struct DialogPanel {
    pub buf: BufId,
    pub kind: PanelKind,
    pub height: PanelHeight,
    pub separator_above: Option<SeparatorStyle>,
    pub pad_left: u16,
    pub focusable: bool,
    pub collapse_when_empty: bool,
    pub win: Window,
    /// Cached snapshot of the panel's buffer used by `draw`. Synced
    /// each frame via `Dialog::sync_from_bufs`.
    view: BufferView,
    line_count: usize,
    /// Resolved rect within the dialog, recomputed each frame.
    rect: Rect,
    /// Resolved viewport (rect + scrollbar geometry) recomputed each
    /// frame.
    viewport: Option<WindowViewport>,
}

pub struct Dialog {
    config: DialogConfig,
    panels: Vec<DialogPanel>,
    focused: usize,
}

impl Dialog {
    pub(crate) fn new(config: DialogConfig, mut panels: Vec<DialogPanel>) -> Self {
        let focused = panels.iter().position(|p| p.focusable).unwrap_or(0);
        // Propagate the dialog's bg to each panel's view so glyphs
        // render on the dialog fill instead of terminal defaults.
        for panel in panels.iter_mut() {
            panel.view.set_default_style(config.background_style);
        }
        Self {
            config,
            panels,
            focused,
        }
    }

    pub fn panel_count(&self) -> usize {
        self.panels.len()
    }

    pub fn focused_panel(&self) -> usize {
        self.focused
    }

    pub fn config_mut(&mut self) -> &mut DialogConfig {
        &mut self.config
    }

    /// Copy each panel's buffer content into its internal BufferView.
    /// Called once per frame by `Ui::render` before compositor draw.
    pub(crate) fn sync_from_bufs<'a, F>(&mut self, resolve: F)
    where
        F: Fn(BufId) -> Option<&'a Buffer>,
    {
        let default_style = self.config.background_style;
        for panel in &mut self.panels {
            if let Some(buf) = resolve(panel.buf) {
                panel.line_count = buf.line_count();
                panel.view.sync_from_buffer(buf);
            }
            // Inherit the dialog's background so content cells keep
            // the bg fill after the view paints glyphs.
            panel.view.set_default_style(default_style);
        }
    }

    /// Solve panel rects inside `area` using Fixed / Fit / Fill.
    fn resolve_panel_rects(&mut self, area: Rect) {
        // Reserve top rule row + optional hints block (1 blank + 1 hints).
        let top_rule_rows = 1u16;
        let hints_rows = if self.config.hints.is_some() { 2 } else { 0 };
        let content_top = area.top + top_rule_rows;
        let content_h = area
            .height
            .saturating_sub(top_rule_rows)
            .saturating_sub(hints_rows);

        // Per-panel separator cost (1 row if separator_above is set).
        let sep_cost: Vec<u16> = self
            .panels
            .iter()
            .map(|p| if p.separator_above.is_some() { 1 } else { 0 })
            .collect();

        // Pass 1: resolve Fixed + Fit heights, keep Fill to pass 2.
        let mut heights: Vec<Option<u16>> = vec![None; self.panels.len()];
        let mut used = 0u16;
        let mut fills: Vec<usize> = Vec::new();
        for (i, panel) in self.panels.iter().enumerate() {
            if panel.collapse_when_empty && panel.line_count == 0 {
                heights[i] = Some(0);
                continue;
            }
            let h = match panel.height {
                PanelHeight::Fixed(n) => n,
                PanelHeight::Fit => panel.line_count as u16,
                PanelHeight::Fill => {
                    fills.push(i);
                    continue;
                }
            };
            heights[i] = Some(h);
            used = used.saturating_add(h).saturating_add(sep_cost[i]);
        }
        // Remaining space distributed evenly among Fill panels.
        let sep_remaining: u16 = fills.iter().map(|&i| sep_cost[i]).sum();
        let mut remaining = content_h.saturating_sub(used).saturating_sub(sep_remaining);
        if !fills.is_empty() {
            let per = remaining / fills.len() as u16;
            let extra = remaining % fills.len() as u16;
            for (fi, &idx) in fills.iter().enumerate() {
                let h = per + if (fi as u16) < extra { 1 } else { 0 };
                heights[idx] = Some(h);
                remaining = remaining.saturating_sub(h);
            }
        }

        // If the total is still over budget (Fixed/Fit overflow), clip
        // from the bottom Fit panels last-to-first.
        let sep_fixed_fit: u16 = self
            .panels
            .iter()
            .enumerate()
            .filter(|(i, _)| !fills.contains(i))
            .map(|(i, _)| sep_cost[i])
            .sum();
        let total: u16 =
            heights.iter().filter_map(|h| *h).sum::<u16>() + sep_fixed_fit + sep_remaining;
        if total > content_h {
            let overflow = total - content_h;
            let mut left = overflow;
            for (i, panel) in self.panels.iter().enumerate().rev() {
                if left == 0 {
                    break;
                }
                if matches!(panel.height, PanelHeight::Fit) {
                    if let Some(ref mut h) = heights[i] {
                        let shrink = (*h).min(left);
                        *h -= shrink;
                        left -= shrink;
                    }
                }
            }
        }

        // Pass 2: assign rects top-down.
        let mut y = content_top;
        for (i, panel) in self.panels.iter_mut().enumerate() {
            if panel.separator_above.is_some() {
                y = y.saturating_add(1);
            }
            let h = heights[i].unwrap_or(0);
            let rect = Rect::new(
                y,
                area.left + panel.pad_left,
                area.width.saturating_sub(panel.pad_left),
                h,
            );
            panel.rect = rect;
            let total_lines = panel.line_count as u16;
            let viewport_rows = rect.height;
            let scroll_top = panel
                .win
                .scroll_top
                .min(total_lines.saturating_sub(viewport_rows));
            panel.win.scroll_top = scroll_top;
            let scrollbar = ScrollbarState::new(
                rect.left + rect.width.saturating_sub(1),
                total_lines,
                viewport_rows,
            );
            panel.viewport = Some(WindowViewport::new(
                rect,
                rect.width,
                total_lines,
                scroll_top,
                scrollbar,
            ));
            // BufferView renders starting at its own scroll_offset;
            // keep it in lock-step with the window's scroll_top so
            // scrolling actually moves the visible rows (not just the
            // scrollbar thumb).
            panel.view.set_scroll(scroll_top as usize);
            y = y.saturating_add(h);
        }
    }

    fn draw_top_rule(&self, area: Rect, grid: &mut GridSlice<'_>) {
        if area.height == 0 {
            return;
        }
        for col in 0..area.width {
            grid.set(col, 0, '─', self.config.accent_style);
        }
    }

    fn draw_separator(&self, rel_row: u16, grid: &mut GridSlice<'_>) {
        let w = grid.width();
        for col in 0..w {
            grid.set(col, rel_row, '╌', self.config.separator_style);
        }
    }

    fn draw_panel(
        &self,
        panel: &DialogPanel,
        area: Rect,
        grid: &mut GridSlice<'_>,
        ctx: &DrawContext,
    ) {
        if panel.rect.height == 0 || panel.rect.width == 0 {
            return;
        }
        let rel = Rect::new(
            panel.rect.top.saturating_sub(area.top),
            panel.rect.left.saturating_sub(area.left),
            panel.rect.width,
            panel.rect.height,
        );
        if rel.height == 0 || rel.width == 0 {
            return;
        }

        let mut slice = grid.sub_slice(rel);
        // Scrollbar reserved column (rightmost) if viewport overflows.
        let reserve_scrollbar = panel.viewport.as_ref().and_then(|v| v.scrollbar).is_some();
        let content_rect = if reserve_scrollbar {
            Rect::new(
                panel.rect.top,
                panel.rect.left,
                panel.rect.width.saturating_sub(1),
                panel.rect.height,
            )
        } else {
            panel.rect
        };
        let content_rel = Rect::new(0, 0, content_rect.width, content_rect.height);
        if content_rel.width > 0 && content_rel.height > 0 {
            let mut content_slice = slice.sub_slice(content_rel);
            // Ensure panel bg matches dialog bg.
            content_slice.fill(
                Rect::new(0, 0, content_rel.width, content_rel.height),
                ' ',
                self.config.background_style,
            );
            panel.view.draw(content_rect, &mut content_slice, ctx);

            // List selection: retint the cursor row's fg to accent.
            // Preserves symbols and the dialog bg; no layout shift.
            if let PanelKind::List { .. } = panel.kind {
                self.paint_list_selection(panel, &mut content_slice);
            }
        }

        // Scrollbar.
        if let Some(viewport) = panel.viewport {
            if let Some(bar) = viewport.scrollbar {
                self.draw_scrollbar(panel, viewport, bar, &mut slice);
            }
        }
    }

    fn paint_list_selection(&self, panel: &DialogPanel, slice: &mut GridSlice<'_>) {
        let Some(viewport) = panel.viewport else {
            return;
        };
        let cursor_line = panel.win.cursor_line;
        if cursor_line >= viewport.rect.height {
            return;
        }
        let accent_fg = self.config.accent_style.fg;
        let w = slice.width();
        for col in 0..w {
            let cell = slice.cell(col, cursor_line);
            let style = Style {
                fg: accent_fg.or(cell.style.fg),
                ..cell.style
            };
            slice.set_style(col, cursor_line, style);
        }
    }

    fn draw_scrollbar(
        &self,
        panel: &DialogPanel,
        _viewport: WindowViewport,
        bar: ScrollbarState,
        slice: &mut GridSlice<'_>,
    ) {
        let w = slice.width();
        if w == 0 {
            return;
        }
        let col = w - 1;
        let total = bar.total_rows as usize;
        let viewport_rows = bar.viewport_rows as usize;
        let thumb_size = bar.thumb_size() as usize;
        let max_thumb = bar.max_thumb_top() as usize;
        let max_scroll = bar.max_scroll() as usize;
        let scroll_top = panel.win.scroll_top as usize;
        let thumb_top = (scroll_top * max_thumb + max_scroll / 2)
            .checked_div(max_scroll)
            .unwrap_or(0);
        for row in 0..viewport_rows.min(slice.height() as usize) {
            let is_thumb = row >= thumb_top && row < thumb_top + thumb_size;
            let style = if is_thumb {
                self.config.scrollbar_thumb_style
            } else {
                self.config.scrollbar_track_style
            };
            slice.set(col, row as u16, ' ', style);
        }
        let _ = total;
    }

    fn focus_next(&mut self) {
        if self.panels.is_empty() {
            return;
        }
        for step in 1..=self.panels.len() {
            let idx = (self.focused + step) % self.panels.len();
            if self.panels[idx].focusable {
                self.focused = idx;
                return;
            }
        }
    }

    fn focus_prev(&mut self) {
        if self.panels.is_empty() {
            return;
        }
        for step in 1..=self.panels.len() {
            let idx = (self.focused + self.panels.len() - step) % self.panels.len();
            if self.panels[idx].focusable {
                self.focused = idx;
                return;
            }
        }
    }

    fn scroll_focused(&mut self, delta: isize) {
        let Some(panel) = self.panels.get_mut(self.focused) else {
            return;
        };
        let total = panel.line_count as isize;
        let rows = panel.rect.height as isize;
        let max_scroll = (total - rows).max(0);
        let new = (panel.win.scroll_top as isize + delta).clamp(0, max_scroll);
        panel.win.scroll_top = new as u16;
        // For List panels, cursor_line follows to stay inside the
        // viewport; selection is cursor-line, not a separate index.
        if matches!(panel.kind, PanelKind::List { .. }) {
            let max_line = panel.rect.height.saturating_sub(1);
            panel.win.cursor_line = panel.win.cursor_line.min(max_line);
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let Some(panel) = self.panels.get_mut(self.focused) else {
            return;
        };
        if !matches!(panel.kind, PanelKind::List { .. }) {
            return;
        }
        let total = panel.line_count as isize;
        if total == 0 {
            return;
        }
        let current = panel.win.scroll_top as isize + panel.win.cursor_line as isize;
        let new = (current + delta).clamp(0, total - 1);
        let rows = panel.rect.height as isize;
        let scroll = panel.win.scroll_top as isize;
        if new < scroll {
            panel.win.scroll_top = new as u16;
            panel.win.cursor_line = 0;
        } else if new >= scroll + rows {
            panel.win.scroll_top = (new - rows + 1).max(0) as u16;
            panel.win.cursor_line = (new - panel.win.scroll_top as isize) as u16;
        } else {
            panel.win.cursor_line = (new - scroll) as u16;
        }
    }

    pub fn selected_index(&self) -> Option<usize> {
        let panel = self.panels.get(self.focused)?;
        if !matches!(panel.kind, PanelKind::List { .. }) {
            return None;
        }
        Some(panel.win.scroll_top as usize + panel.win.cursor_line as usize)
    }
}

impl Component for Dialog {
    fn prepare(&mut self, area: Rect, _ctx: &DrawContext) {
        self.resolve_panel_rects(area);
    }

    fn draw(&self, area: Rect, grid: &mut GridSlice<'_>, ctx: &DrawContext) {
        let w = grid.width();
        let h = grid.height();
        if w == 0 || h == 0 {
            return;
        }
        // Fill the entire dialog rect with the background style so
        // chrome and panel fills share the same bg, and panel glyphs
        // (which inherit via view.default_style) stay readable.
        grid.fill(Rect::new(0, 0, w, h), ' ', self.config.background_style);

        self.draw_top_rule(area, grid);

        for panel in &self.panels {
            if let Some(sep) = panel.separator_above {
                let sep_row = panel.rect.top.saturating_sub(area.top).saturating_sub(1);
                if sep_row < h {
                    match sep {
                        SeparatorStyle::Dashed => self.draw_separator(sep_row, grid),
                        SeparatorStyle::Solid => {
                            self.draw_top_rule(Rect::new(sep_row, 0, w, 1), grid)
                        }
                    }
                }
            }
            self.draw_panel(panel, area, grid, ctx);
        }

        if let Some(ref hints) = self.config.hints {
            let hint_y = h.saturating_sub(1);
            let rect = Rect::new(hint_y, 0, w, 1);
            let mut slice = grid.sub_slice(rect);
            hints.draw(
                Rect::new(area.top + hint_y, area.left, w, 1),
                &mut slice,
                ctx,
            );
        }
    }

    fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> KeyResult {
        if matches!(code, KeyCode::Esc) && mods == KeyModifiers::NONE {
            return KeyResult::Action("dismiss".into());
        }
        // Ctrl+C always dismisses a dialog (matches legacy behavior).
        if matches!(code, KeyCode::Char('c')) && mods == KeyModifiers::CONTROL {
            return KeyResult::Action("dismiss".into());
        }
        if self
            .config
            .dismiss_keys
            .iter()
            .any(|&(k, m)| k == code && m == mods)
        {
            return KeyResult::Action("dismiss".into());
        }

        match (code, mods) {
            (KeyCode::Tab, KeyModifiers::NONE) => {
                self.focus_next();
                return KeyResult::Consumed;
            }
            (KeyCode::BackTab, _) => {
                self.focus_prev();
                return KeyResult::Consumed;
            }
            _ => {}
        }

        // Focused-panel-specific routing.
        let Some(panel) = self.panels.get(self.focused) else {
            return KeyResult::Ignored;
        };
        match panel.kind {
            PanelKind::List { .. } => match (code, mods) {
                (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                    self.move_selection(-1);
                    KeyResult::Consumed
                }
                (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                    self.move_selection(1);
                    KeyResult::Consumed
                }
                (KeyCode::PageUp, _) | (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                    let page = panel.rect.height.max(1) as isize / 2;
                    self.move_selection(-page);
                    KeyResult::Consumed
                }
                (KeyCode::PageDown, _) | (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                    let page = panel.rect.height.max(1) as isize / 2;
                    self.move_selection(page);
                    KeyResult::Consumed
                }
                (KeyCode::Home, _) | (KeyCode::Char('g'), KeyModifiers::NONE) => {
                    self.move_selection(isize::MIN / 2);
                    KeyResult::Consumed
                }
                (KeyCode::End, _) => {
                    self.move_selection(isize::MAX / 2);
                    KeyResult::Consumed
                }
                (KeyCode::Enter, _) => {
                    if let Some(idx) = self.selected_index() {
                        KeyResult::Action(format!("select:{idx}"))
                    } else {
                        KeyResult::Ignored
                    }
                }
                (KeyCode::Char(c), KeyModifiers::NONE) if c.is_ascii_digit() => {
                    let idx = (c as u8 - b'1') as usize;
                    KeyResult::Action(format!("select:{idx}"))
                }
                _ => KeyResult::Ignored,
            },
            PanelKind::Content => match (code, mods) {
                (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                    self.scroll_focused(-1);
                    KeyResult::Consumed
                }
                (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                    self.scroll_focused(1);
                    KeyResult::Consumed
                }
                (KeyCode::PageUp, _) | (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                    let page = panel.rect.height.max(1) as isize / 2;
                    self.scroll_focused(-page);
                    KeyResult::Consumed
                }
                (KeyCode::PageDown, _) | (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                    let page = panel.rect.height.max(1) as isize / 2;
                    self.scroll_focused(page);
                    KeyResult::Consumed
                }
                _ => KeyResult::Ignored,
            },
            PanelKind::Input { .. } => {
                // Input panel handling lands in a follow-up commit
                // that wires EditBuffer into the panel's Window.
                KeyResult::Ignored
            }
        }
    }

    fn cursor(&self) -> Option<CursorInfo> {
        // List panels show selection as fg-accent tint on the cursor
        // row (painted inside `draw_panel`), not via a terminal
        // cursor glyph. Input panels will expose a hardware cursor
        // once EditBuffer is wired.
        None
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

pub(crate) fn build_panels(
    specs: Vec<PanelSpec>,
    bufs: &std::collections::HashMap<BufId, Buffer>,
) -> Vec<DialogPanel> {
    use crate::{
        window::{FloatConfig, WinConfig},
        WinId,
    };
    specs
        .into_iter()
        .enumerate()
        .map(|(i, spec)| {
            let line_count = bufs.get(&spec.buf).map(|b| b.line_count()).unwrap_or(0);
            let mut view = BufferView::new();
            if let Some(buf) = bufs.get(&spec.buf) {
                view.sync_from_buffer(buf);
            }
            let win = Window::new(
                WinId(u64::MAX - i as u64),
                spec.buf,
                WinConfig::Float(FloatConfig::default()),
            );
            let _ = LineDecoration::default();
            DialogPanel {
                buf: spec.buf,
                kind: spec.kind,
                height: spec.height,
                separator_above: spec.separator_above,
                pad_left: spec.pad_left,
                focusable: spec.focusable,
                collapse_when_empty: spec.collapse_when_empty,
                win,
                view,
                line_count,
                rect: Rect::new(0, 0, 0, 0),
                viewport: None,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::BufCreateOpts;
    use crate::grid::Grid;
    use crate::BufId;

    fn make_buf(id: u64, lines: &[&str]) -> Buffer {
        let mut buf = Buffer::new(BufId(id), BufCreateOpts::default());
        buf.set_all_lines(lines.iter().map(|s| s.to_string()).collect());
        buf
    }

    fn ctx(w: u16, h: u16) -> DrawContext {
        DrawContext {
            terminal_width: w,
            terminal_height: h,
            focused: true,
        }
    }

    #[test]
    fn resolve_fixed_fit_fill() {
        let mut bufs = std::collections::HashMap::new();
        bufs.insert(BufId(1), make_buf(1, &["title"]));
        bufs.insert(BufId(2), make_buf(2, &["line"; 40]));
        bufs.insert(BufId(3), make_buf(3, &["a", "b", "c"]));
        let panels = build_panels(
            vec![
                PanelSpec::content(BufId(1), PanelHeight::Fixed(1)),
                PanelSpec::content(BufId(2), PanelHeight::Fill),
                PanelSpec::list(BufId(3), PanelHeight::Fit),
            ],
            &bufs,
        );
        let mut dlg = Dialog::new(DialogConfig::default(), panels);
        let area = Rect::new(0, 0, 40, 20);
        dlg.resolve_panel_rects(area);
        // top rule: 1 row; remaining 19 rows split across fixed(1)+fill+fit(3) = 1+15+3
        assert_eq!(dlg.panels[0].rect.height, 1);
        assert_eq!(dlg.panels[1].rect.height, 15);
        assert_eq!(dlg.panels[2].rect.height, 3);
    }

    #[test]
    fn top_rule_is_accent_colored() {
        use crossterm::style::Color;
        let mut bufs = std::collections::HashMap::new();
        bufs.insert(BufId(1), make_buf(1, &["hello"]));
        let panels = build_panels(vec![PanelSpec::content(BufId(1), PanelHeight::Fill)], &bufs);
        let mut dlg = Dialog::new(
            DialogConfig {
                accent_style: Style::fg(Color::Red),
                ..DialogConfig::default()
            },
            panels,
        );
        let mut grid = Grid::new(20, 5);
        let area = Rect::new(0, 0, 20, 5);
        dlg.resolve_panel_rects(area);
        let mut slice = grid.slice_mut(area);
        dlg.draw(area, &mut slice, &ctx(20, 5));
        assert_eq!(grid.cell(0, 0).symbol, '─');
        assert_eq!(grid.cell(0, 0).style.fg, Some(Color::Red));
    }

    #[test]
    fn dashed_separator_between_panels() {
        use crossterm::style::Color;
        let mut bufs = std::collections::HashMap::new();
        bufs.insert(BufId(1), make_buf(1, &["title"]));
        bufs.insert(BufId(2), make_buf(2, &["body"]));
        let panels = build_panels(
            vec![
                PanelSpec::content(BufId(1), PanelHeight::Fixed(1)),
                PanelSpec::content(BufId(2), PanelHeight::Fill)
                    .with_separator(SeparatorStyle::Dashed),
            ],
            &bufs,
        );
        let mut dlg = Dialog::new(
            DialogConfig {
                separator_style: Style::fg(Color::Blue),
                ..DialogConfig::default()
            },
            panels,
        );
        let mut grid = Grid::new(20, 6);
        let area = Rect::new(0, 0, 20, 6);
        dlg.resolve_panel_rects(area);
        let mut slice = grid.slice_mut(area);
        dlg.draw(area, &mut slice, &ctx(20, 6));
        // row 0: top rule ─. row 1: title "title". row 2: dashed ╌.
        assert_eq!(grid.cell(0, 2).symbol, '╌');
        assert_eq!(grid.cell(0, 2).style.fg, Some(Color::Blue));
    }

    #[test]
    fn esc_returns_dismiss() {
        let panels = build_panels(vec![], &std::collections::HashMap::new());
        let mut dlg = Dialog::new(DialogConfig::default(), panels);
        let r = dlg.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(r, KeyResult::Action("dismiss".into()));
    }

    #[test]
    fn list_enter_returns_select() {
        let mut bufs = std::collections::HashMap::new();
        bufs.insert(BufId(1), make_buf(1, &["a", "b", "c"]));
        let panels = build_panels(vec![PanelSpec::list(BufId(1), PanelHeight::Fit)], &bufs);
        let mut dlg = Dialog::new(DialogConfig::default(), panels);
        dlg.resolve_panel_rects(Rect::new(0, 0, 20, 10));
        dlg.move_selection(1);
        let r = dlg.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(r, KeyResult::Action("select:1".into()));
    }

    #[test]
    fn content_panel_renders_buffer_lines() {
        let mut bufs = std::collections::HashMap::new();
        bufs.insert(BufId(1), make_buf(1, &["hello world", "second line"]));
        let panels = build_panels(
            vec![PanelSpec::content(BufId(1), PanelHeight::Fill).with_pad_left(0)],
            &bufs,
        );
        let mut dlg = Dialog::new(DialogConfig::default(), panels);
        let area = Rect::new(0, 0, 20, 5);
        let mut grid = Grid::new(20, 5);
        dlg.resolve_panel_rects(area);
        let mut slice = grid.slice_mut(area);
        dlg.draw(area, &mut slice, &ctx(20, 5));
        // Top rule row 0 is '─'; content starts row 1.
        assert_eq!(grid.cell(0, 1).symbol, 'h');
        assert_eq!(grid.cell(4, 1).symbol, 'o');
        assert_eq!(grid.cell(6, 1).symbol, 'w');
        assert_eq!(grid.cell(0, 2).symbol, 's');
    }

    #[test]
    fn list_panel_renders_items_with_bg() {
        use crossterm::style::Color;
        let mut bufs = std::collections::HashMap::new();
        bufs.insert(BufId(1), make_buf(1, &["apple", "banana", "cherry"]));
        let panels = build_panels(
            vec![PanelSpec::list(BufId(1), PanelHeight::Fill).with_pad_left(0)],
            &bufs,
        );
        let mut dlg = Dialog::new(
            DialogConfig {
                background_style: Style::bg(Color::Black),
                ..DialogConfig::default()
            },
            panels,
        );
        let area = Rect::new(0, 0, 20, 5);
        let mut grid = Grid::new(20, 5);
        dlg.resolve_panel_rects(area);
        let mut slice = grid.slice_mut(area);
        dlg.draw(area, &mut slice, &ctx(20, 5));
        // Content 'apple' should be on row 1.
        assert_eq!(grid.cell(0, 1).symbol, 'a');
        assert_eq!(grid.cell(4, 1).symbol, 'e');
        // And each content cell should keep the dialog's bg fill.
        assert_eq!(grid.cell(0, 1).style.bg, Some(Color::Black));
    }

    #[test]
    fn numeric_digit_selects_row() {
        let mut bufs = std::collections::HashMap::new();
        bufs.insert(BufId(1), make_buf(1, &["a", "b", "c"]));
        let panels = build_panels(vec![PanelSpec::list(BufId(1), PanelHeight::Fit)], &bufs);
        let mut dlg = Dialog::new(DialogConfig::default(), panels);
        dlg.resolve_panel_rects(Rect::new(0, 0, 20, 10));
        let r = dlg.handle_key(KeyCode::Char('2'), KeyModifiers::NONE);
        assert_eq!(r, KeyResult::Action("select:1".into()));
    }
}
