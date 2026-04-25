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
use crate::component::{Component, CursorInfo, DrawContext, KeyResult, WidgetEvent};
use crate::grid::{GridSlice, Style};
use crate::layout::Rect;
use crate::status_bar::StatusBar;
use crate::window::{ScrollbarState, Window, WindowViewport};
use crate::BufId;
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

/// Mutable buffer lookup shim used by `Dialog::sync_from_bufs_mut` so
/// dialogs can drive formatter-backed buffers without inlining the
/// HashMap of the host `Ui`. `FnMut` can't express "returns a borrow
/// tied to self" — this trait can.
pub(crate) trait BufferResolver {
    fn get(&mut self, id: BufId) -> Option<&mut Buffer>;
}

impl<S: std::hash::BuildHasher> BufferResolver for std::collections::HashMap<BufId, Buffer, S> {
    fn get(&mut self, id: BufId) -> Option<&mut Buffer> {
        self.get_mut(&id)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelKind {
    /// Readonly text or preview. Scrollable, selectable via vim.
    Content,
    /// Selectable rows. Cursor line = current selection. Enter
    /// returns `select:{idx}` from `handle_key`.
    List { multi: bool },
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

/// A self-contained panel renderer with its own interaction state.
/// Widgets let dialogs embed composite components (multi-line editors,
/// multi-select lists with "Other" fields, tab bars, previews) that go
/// beyond what a raw buffer-backed panel can express.
///
/// A `PanelWidget` is a `Component` (rendering, key handling, cursor,
/// prepare, downcast) plus panel-specific helpers (`content_rows` for
/// `PanelHeight::Fit`, `tick` for live-refresh). Widgets should be
/// `Send` so they can move between threads during event dispatch.
pub trait PanelWidget: Component + Send {
    /// Rows the widget would like to occupy. Used for `PanelHeight::Fit`.
    fn content_rows(&self) -> usize {
        0
    }
    /// Called once per event-loop tick on the focused float. Default
    /// no-op.
    fn tick(&mut self) {}
    /// 0-based selection index for list-like widgets (`OptionList`,
    /// `ListSelect`). `None` for non-selectable widgets.
    fn selected_index(&self) -> Option<usize> {
        None
    }
    /// Current text for input-like widgets (`TextInput`). `None` for
    /// widgets without a text concept.
    fn text_value(&self) -> Option<String> {
        None
    }
    /// Cast to `&mut dyn ListWidget` if this widget is list-shaped.
    /// Default returns `None`. Implementors override with `Some(self)`.
    fn as_list_widget(&mut self) -> Option<&mut dyn ListWidget> {
        None
    }
    /// Resolved viewport (rect + scrollbar geometry) for widgets with
    /// scrollable content. Returned in absolute coords so App-side
    /// scrollbar hit-tests use the same geometry the widget renders
    /// with. `None` for widgets without a scrollable viewport.
    fn viewport(&self) -> Option<WindowViewport> {
        None
    }
    /// Snap scroll so the scrollbar thumb top lands at `thumb_top` rows
    /// from the viewport top. Returns `true` if the widget consumed the
    /// drag (i.e. it has a draggable scrollbar).
    fn apply_scrollbar_drag(&mut self, _thumb_top: u16) -> bool {
        false
    }
}

/// Selectable-list contract that buffer-backed lists (future
/// `BufferList`) and item-backed lists (`OptionList`) both satisfy.
/// Lets callers route uniform list operations — click-to-select,
/// keyboard nav, scroll — without knowing the backing store. Widgets
/// expose themselves through `PanelWidget::as_list_widget`.
pub trait ListWidget {
    fn row_count(&self) -> usize;
    fn selected(&self) -> Option<usize>;
    fn set_selected(&mut self, idx: usize);
    fn scroll_top(&self) -> usize;
    fn set_scroll_top(&mut self, top: usize);
    /// Resolve the row index at `rel_row` rows below the widget's draw
    /// area top. Returns `None` if the row is past the last item.
    fn row_at(&self, rel_row: u16) -> Option<usize>;
    /// Backing buffer when the list mirrors a `Buffer` (`BufferList`).
    /// `OptionList` and other in-memory lists return `None`. The dialog
    /// uses this to resolve a `Buffer` from its registry and feed it to
    /// `sync_from_buffer` each frame.
    fn buf_id(&self) -> Option<BufId> {
        None
    }
    /// Mirror `buf` into the list's internal view. No-op for lists that
    /// don't track a `Buffer`.
    fn sync_from_buffer(&mut self, _buf: &Buffer) {}
}

/// Live snapshot of a dialog panel's widget state. Built on-demand by
/// [`crate::Ui::snapshot_dialog_panels`] at keymap / event / tick
/// dispatch so Lua callbacks can pull-read the current selection and
/// input text without a bidirectional channel back into Ui.
#[derive(Clone, Debug)]
pub struct PanelSnapshot {
    pub kind: PanelKind,
    /// 0-based cursor / selection index. `None` for panels without a
    /// selection (`Content`, `Input`, plain `Content`-widget panels).
    pub selected: Option<usize>,
    /// Current text for `Input` panels and `TextInput` widgets. Empty
    /// for others — callers inspect `kind` to disambiguate.
    pub text: String,
}

/// What a panel renders: a buffer in `Ui::bufs`, or a self-contained
/// widget.
pub enum PanelContent {
    Buffer(BufId),
    Widget(Box<dyn PanelWidget>),
}

/// Description of a panel passed to `Ui::dialog_open`. The dialog
/// instantiates an internal `ui::Window` from this spec; the buffer
/// stays in the Ui registry.
pub struct PanelSpec {
    pub content: PanelContent,
    pub kind: PanelKind,
    pub height: PanelHeight,
    pub separator_above: Option<SeparatorStyle>,
    /// Left content padding inside the panel's rect.
    pub pad_left: u16,
    /// Whether this panel participates in focus cycling. Title/summary
    /// panels usually don't.
    pub focusable: bool,
    /// Take initial focus on dialog open. When no panel opts in, the
    /// dialog focuses the first focusable panel.
    pub focus_initial: bool,
    /// Hide the panel when its buffer has zero non-blank content.
    pub collapse_when_empty: bool,
}

impl PanelSpec {
    pub fn content(buf: BufId, height: PanelHeight) -> Self {
        Self {
            content: PanelContent::Buffer(buf),
            kind: PanelKind::Content,
            height,
            separator_above: None,
            pad_left: 1,
            focusable: false,
            focus_initial: false,
            collapse_when_empty: false,
        }
    }

    pub fn list(buf: BufId, height: PanelHeight) -> Self {
        Self {
            content: PanelContent::Buffer(buf),
            kind: PanelKind::List { multi: false },
            height,
            separator_above: None,
            pad_left: 2,
            focusable: true,
            focus_initial: false,
            collapse_when_empty: false,
        }
    }

    /// Widget-backed panel. The widget owns its own draw + key
    /// handling; the dialog only places it in the panel layout.
    pub fn widget(widget: Box<dyn PanelWidget>, height: PanelHeight) -> Self {
        Self {
            content: PanelContent::Widget(widget),
            kind: PanelKind::Content,
            height,
            separator_above: None,
            pad_left: 1,
            focusable: true,
            focus_initial: false,
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

    pub fn with_initial_focus(mut self, focus: bool) -> Self {
        self.focus_initial = focus;
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

/// Internal panel state held by `Dialog`. For buffer-backed panels
/// the dialog owns a `Window` (cursor, scroll, selection anchor) and
/// a `BufferView` snapshot synced each frame. Widget-backed panels
/// manage their own state.
pub(crate) struct DialogPanel {
    pub kind: PanelKind,
    pub height: PanelHeight,
    pub separator_above: Option<SeparatorStyle>,
    pub pad_left: u16,
    pub focusable: bool,
    pub focus_initial: bool,
    pub collapse_when_empty: bool,
    pub content: DialogPanelContent,
    /// Rows the content wants to render. For buffers, `buf.line_count`
    /// at last sync. For widgets, `widget.content_rows()`.
    pub line_count: usize,
    /// Absolute selection row for `PanelKind::List`. Decoupled from the
    /// viewport: wheel / scrollbar drag scrolls the buffer but leaves
    /// this alone, so the selected item stays put even when scrolled
    /// out of view. `win.cursor_line` is re-derived from this on each
    /// scroll or nav (it's always the viewport-relative render row, or
    /// a sentinel > viewport_rows when the selection is off-screen).
    pub selection_abs: u16,
    /// Resolved rect within the dialog, recomputed each frame.
    rect: Rect,
    /// Resolved viewport (rect + scrollbar geometry) recomputed each
    /// frame. `None` for widget panels.
    viewport: Option<WindowViewport>,
}

#[allow(clippy::large_enum_variant)]
pub(crate) enum DialogPanelContent {
    Buffer {
        buf: BufId,
        view: BufferView,
        win: Window,
    },
    Widget(Box<dyn PanelWidget>),
}

impl DialogPanel {
    pub(crate) fn win_mut(&mut self) -> Option<&mut Window> {
        match &mut self.content {
            DialogPanelContent::Buffer { win, .. } => Some(win),
            DialogPanelContent::Widget(_) => None,
        }
    }

    pub(crate) fn win(&self) -> Option<&Window> {
        match &self.content {
            DialogPanelContent::Buffer { win, .. } => Some(win),
            DialogPanelContent::Widget(_) => None,
        }
    }
}

pub struct Dialog {
    config: DialogConfig,
    panels: Vec<DialogPanel>,
    focused: usize,
    /// Layer rect from the last `prepare`/`draw`, used to translate
    /// panel-relative cursor positions back to coords relative to the
    /// dialog (what the compositor expects from `Component::cursor`).
    area: Rect,
}

impl Dialog {
    pub(crate) fn new(config: DialogConfig, mut panels: Vec<DialogPanel>) -> Self {
        let focused = panels
            .iter()
            .position(|p| p.focusable && p.focus_initial)
            .or_else(|| panels.iter().position(|p| p.focusable))
            .unwrap_or(0);
        // Propagate the dialog's bg to each buffer panel's view so
        // glyphs render on the dialog fill instead of terminal
        // defaults. Widgets handle their own styling.
        for panel in panels.iter_mut() {
            if let DialogPanelContent::Buffer { view, .. } = &mut panel.content {
                view.set_default_style(config.background_style);
            }
        }
        Self {
            config,
            panels,
            focused,
            area: Rect::new(0, 0, 0, 0),
        }
    }

    pub fn panel_count(&self) -> usize {
        self.panels.len()
    }

    /// Natural dialog height: sum of each panel's desired rows (from
    /// the latest `sync_from_bufs`) plus chrome (top rule + optional
    /// hints block + per-panel separator rows). Consumed by
    /// `Placement::FitContent` to size the float to its contents.
    /// `Fill` panels count by `line_count` here so the cap behaviour
    /// stays consistent — under FitContent the dialog is "as tall as
    /// content, up to cap", not "stretch to fill".
    pub fn natural_height(&self) -> u16 {
        let top_rule_rows = 1u16;
        let hints_rows = if self.config.hints.is_some() { 2 } else { 0 };
        let sep_rows: u16 = self
            .panels
            .iter()
            .map(|p| if p.separator_above.is_some() { 1 } else { 0 })
            .sum();
        let content_rows: u16 = self
            .panels
            .iter()
            .filter(|p| !(p.collapse_when_empty && p.line_count == 0))
            .map(|p| match p.height {
                PanelHeight::Fixed(n) => n,
                PanelHeight::Fit | PanelHeight::Fill => p.line_count as u16,
            })
            .sum();
        top_rule_rows
            .saturating_add(hints_rows)
            .saturating_add(sep_rows)
            .saturating_add(content_rows)
    }

    pub fn focused_panel(&self) -> usize {
        self.focused
    }

    /// Panel index whose resolved rect contains `(row, col)`. Rects are
    /// recomputed each `prepare`/`draw`, so this reflects the last
    /// rendered frame.
    pub fn panel_at(&self, row: u16, col: u16) -> Option<usize> {
        self.panels.iter().position(|p| p.rect.contains(row, col))
    }

    /// Resolved viewport (rect + scrollbar geometry) for a panel.
    /// Buffer-backed panels keep their viewport on `DialogPanel`;
    /// widget panels delegate to `PanelWidget::viewport` so widgets
    /// like `BufferList` can expose their own scrollable geometry.
    pub fn panel_viewport(&self, panel_idx: usize) -> Option<WindowViewport> {
        let panel = self.panels.get(panel_idx)?;
        if let DialogPanelContent::Widget(w) = &panel.content {
            return w.viewport();
        }
        panel.viewport
    }

    /// Snap the buffer-backed panel's scroll so its scrollbar thumb
    /// lands with its top at `thumb_top` rows from the viewport top.
    /// Caller is responsible for clamping to `max_thumb_top()` if it
    /// cares; this method clamps internally too. Returns `true` when
    /// the panel is buffer-backed with a visible scrollbar.
    pub fn apply_panel_scrollbar_drag(&mut self, panel_idx: usize, thumb_top: u16) -> bool {
        let Some(panel) = self.panels.get_mut(panel_idx) else {
            return false;
        };
        if let DialogPanelContent::Widget(w) = &mut panel.content {
            return w.apply_scrollbar_drag(thumb_top);
        }
        let Some(viewport) = panel.viewport else {
            return false;
        };
        let Some(bar) = viewport.scrollbar else {
            return false;
        };
        let DialogPanelContent::Buffer { win, view, .. } = &mut panel.content else {
            return false;
        };
        let thumb_top = thumb_top.min(bar.max_thumb_top());
        let from_top = bar.scroll_from_top_for_thumb(thumb_top);
        win.scroll_top = from_top;
        view.set_scroll(from_top as usize);
        if matches!(panel.kind, PanelKind::List { .. }) {
            Self::sync_cursor_line(panel);
        }
        true
    }

    /// Set focus to `panel_idx`. No-op if the index is out of range or
    /// the panel is not focusable.
    pub fn focus_panel(&mut self, panel_idx: usize) {
        if self.panels.get(panel_idx).is_some_and(|p| p.focusable) {
            self.focused = panel_idx;
        }
    }

    pub fn config_mut(&mut self) -> &mut DialogConfig {
        &mut self.config
    }

    /// Downcast the widget in `panel_idx` to a concrete widget type
    /// `T`. Returns `None` if the panel is buffer-backed or the widget
    /// is not of type `T`.
    pub fn panel_widget_mut<T: PanelWidget + 'static>(
        &mut self,
        panel_idx: usize,
    ) -> Option<&mut T> {
        let panel = self.panels.get_mut(panel_idx)?;
        let DialogPanelContent::Widget(widget) = &mut panel.content else {
            return None;
        };
        widget.as_any_mut().downcast_mut::<T>()
    }

    /// Drive any formatter-backed buffers at the current content width,
    /// then copy each panel's buffer content into its internal
    /// `BufferView`. Called once per frame by `Ui::render` before
    /// compositor draw.
    ///
    /// `content_width` is the resolved float width (from
    /// `resolve_float_rect`), minus the scrollbar column reservation.
    /// The dialog passes this through to
    /// [`Buffer::ensure_rendered_at`] so markdown / wrap / syntax
    /// formatters reflow when the terminal resizes or the source
    /// changes. `bufs` is a mutable resolver (trait, not `FnMut`,
    /// because the returned borrow lives longer than any single call)
    /// so formatters can write the regenerated lines + decorations
    /// directly into the buffer.
    pub(crate) fn sync_from_bufs_mut(&mut self, content_width: u16, bufs: &mut dyn BufferResolver) {
        let default_style = self.config.background_style;
        for panel in &mut self.panels {
            match &mut panel.content {
                DialogPanelContent::Buffer { buf, view, .. } => {
                    if let Some(b) = bufs.get(*buf) {
                        b.ensure_rendered_at(content_width);
                        panel.line_count = b.line_count();
                        view.sync_from_buffer(b);
                    }
                    view.set_default_style(default_style);
                }
                DialogPanelContent::Widget(widget) => {
                    // List-shaped widgets that mirror a `Buffer`
                    // (`BufferList`) need their internal view re-synced
                    // when the source buffer changes — the `as_list_widget`
                    // hop is how a Widget panel exposes its `BufId`.
                    let buf_id = widget.as_list_widget().and_then(|lw| lw.buf_id());
                    if let Some(buf_id) = buf_id {
                        if let Some(b) = bufs.get(buf_id) {
                            b.ensure_rendered_at(content_width);
                            if let Some(lw) = widget.as_list_widget() {
                                lw.sync_from_buffer(b);
                            }
                        }
                    }
                    panel.line_count = widget.content_rows();
                }
            }
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
            match &mut panel.content {
                DialogPanelContent::Buffer { view, win, .. } => {
                    let scroll_top = win
                        .scroll_top
                        .min(total_lines.saturating_sub(viewport_rows));
                    win.scroll_top = scroll_top;
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
                    // scrolling actually moves the visible rows (not just
                    // the scrollbar thumb).
                    view.set_scroll(scroll_top as usize);
                }
                DialogPanelContent::Widget(_) => {
                    panel.viewport = None;
                }
            }
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
            match &panel.content {
                DialogPanelContent::Buffer { view, .. } => {
                    view.draw(content_rect, &mut content_slice, ctx);
                    if let PanelKind::List { .. } = panel.kind {
                        self.paint_list_selection(panel, &mut content_slice);
                    }
                }
                DialogPanelContent::Widget(widget) => {
                    widget.draw(content_rect, &mut content_slice, ctx);
                }
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
        let Some(win) = panel.win() else { return };
        let cursor_line = win.cursor_line;
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
        let scroll_top = panel.win().map(|w| w.scroll_top).unwrap_or(0) as usize;
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
        self.panel_scroll_by(self.focused, delta);
    }

    fn move_selection(&mut self, delta: isize) {
        self.move_selection_at(self.focused, delta);
    }

    /// Move the selection of the List panel at `panel_idx` by `delta`
    /// rows. Updates `selection_abs` (the source of truth) and scrolls
    /// the viewport only as much as needed to keep the selection
    /// visible — the tmux/less "keyboard re-anchors the viewport"
    /// half of the split wheel/keyboard model.
    fn move_selection_at(&mut self, panel_idx: usize, delta: isize) {
        let Some(panel) = self.panels.get_mut(panel_idx) else {
            return;
        };
        if !matches!(panel.kind, PanelKind::List { .. }) {
            return;
        }
        let total = panel.line_count as isize;
        if total == 0 {
            return;
        }
        let new = ((panel.selection_abs as isize) + delta).clamp(0, total - 1);
        panel.selection_abs = new as u16;
        Self::ensure_selection_visible(panel);
    }

    /// Re-anchor a list panel's viewport and render cursor to its
    /// current `selection_abs`. Called after selection changes
    /// (`move_selection_at`, `set_selected_index`) and after pure
    /// viewport scrolls (`panel_scroll_by`, scrollbar drag) so
    /// `win.cursor_line` is always either the correct viewport-relative
    /// row or `u16::MAX` (render path treats this as "off-screen, skip
    /// the highlight").
    fn ensure_selection_visible(panel: &mut DialogPanel) {
        let rows = panel.rect.height as isize;
        let abs = panel.selection_abs as isize;
        let Some(win) = panel.win_mut() else { return };
        // Move scroll_top just far enough to include the selection.
        if rows > 0 {
            let scroll = win.scroll_top as isize;
            if abs < scroll {
                win.scroll_top = abs as u16;
            } else if abs >= scroll + rows {
                win.scroll_top = (abs - rows + 1).max(0) as u16;
            }
        }
        Self::sync_cursor_line(panel);
    }

    /// Derive `win.cursor_line` from `selection_abs` and the current
    /// `scroll_top`. Called after any mutation that might change either.
    /// `u16::MAX` signals off-screen — the render path in `draw_panel`
    /// already short-circuits on `cursor_line >= viewport.rect.height`,
    /// so painting the cursor highlight is skipped automatically.
    fn sync_cursor_line(panel: &mut DialogPanel) {
        let rows = panel.rect.height as i64;
        let abs = panel.selection_abs as i64;
        let Some(win) = panel.win_mut() else { return };
        let rel = abs - win.scroll_top as i64;
        win.cursor_line = if rel < 0 || rel >= rows {
            u16::MAX
        } else {
            rel as u16
        };
    }

    /// Scroll a buffer-backed panel's viewport by `delta` rows without
    /// moving `selection_abs`. Used for mouse-wheel and scrollbar drag.
    /// If the selection would fall outside the new viewport, the
    /// highlight is hidden (`cursor_line = u16::MAX`); `Enter` still
    /// submits the same item.
    pub fn panel_scroll_by(&mut self, panel_idx: usize, delta: isize) -> isize {
        let Some(panel) = self.panels.get_mut(panel_idx) else {
            return 0;
        };
        let total = panel.line_count as isize;
        let rows = panel.rect.height as isize;
        let max_scroll = (total - rows).max(0);
        let Some(win) = panel.win_mut() else { return 0 };
        let cur = win.scroll_top as isize;
        let new = (cur + delta).clamp(0, max_scroll);
        if new == cur {
            return 0;
        }
        win.scroll_top = new as u16;
        if matches!(panel.kind, PanelKind::List { .. }) {
            Self::sync_cursor_line(panel);
        }
        new - cur
    }

    /// Handle a navigation key against the List panel at `panel_idx`.
    /// Mirrors the keys the `PanelKind::List` arm accepts in
    /// `handle_key`, but scoped to an arbitrary panel — used to route
    /// Up/Down/Enter through to a list when the focused widget panel
    /// (TextInput) ignores them.
    fn list_nav_on(&mut self, panel_idx: usize, code: KeyCode, mods: KeyModifiers) -> KeyResult {
        let height = self
            .panels
            .get(panel_idx)
            .map(|p| p.rect.height.max(1) as isize)
            .unwrap_or(1);
        match (code, mods) {
            (KeyCode::Up, _) => {
                self.move_selection_at(panel_idx, -1);
                KeyResult::Consumed
            }
            (KeyCode::Down, _) => {
                self.move_selection_at(panel_idx, 1);
                KeyResult::Consumed
            }
            (KeyCode::PageUp, _) => {
                self.move_selection_at(panel_idx, -(height / 2));
                KeyResult::Consumed
            }
            (KeyCode::PageDown, _) => {
                self.move_selection_at(panel_idx, height / 2);
                KeyResult::Consumed
            }
            (KeyCode::Home, _) => {
                self.move_selection_at(panel_idx, isize::MIN / 2);
                KeyResult::Consumed
            }
            (KeyCode::End, _) => {
                self.move_selection_at(panel_idx, isize::MAX / 2);
                KeyResult::Consumed
            }
            (KeyCode::Enter, _) => self
                .selected_index_at(panel_idx)
                .map(|idx| KeyResult::Action(WidgetEvent::Select(idx)))
                .unwrap_or(KeyResult::Ignored),
            _ => KeyResult::Ignored,
        }
    }

    pub fn selected_index_at(&self, panel_idx: usize) -> Option<usize> {
        let panel = self.panels.get(panel_idx)?;
        if !matches!(panel.kind, PanelKind::List { .. }) {
            return None;
        }
        Some(panel.selection_abs as usize)
    }

    pub fn panel_kind_at(&self, panel_idx: usize) -> Option<PanelKind> {
        self.panels.get(panel_idx).map(|p| p.kind)
    }

    /// 0-based selection index for the widget in `panel_idx`, if the
    /// widget exposes one (e.g. `OptionList`). `None` for buffer-backed
    /// panels or widgets without a selection concept.
    pub fn panel_widget_selected(&self, panel_idx: usize) -> Option<usize> {
        let panel = self.panels.get(panel_idx)?;
        match &panel.content {
            DialogPanelContent::Widget(w) => w.selected_index(),
            DialogPanelContent::Buffer { .. } => None,
        }
    }

    /// Current text for the widget in `panel_idx`, if the widget
    /// exposes one (e.g. `TextInput`). `None` for buffer-backed panels
    /// or widgets without a text concept.
    pub fn panel_widget_text(&self, panel_idx: usize) -> Option<String> {
        let panel = self.panels.get(panel_idx)?;
        match &panel.content {
            DialogPanelContent::Widget(w) => w.text_value(),
            DialogPanelContent::Buffer { .. } => None,
        }
    }

    /// Height of `panel_idx`'s rect from the last layout pass. Callers
    /// use this to compute a half-page scroll delta.
    pub fn panel_rect_height(&self, panel_idx: usize) -> u16 {
        self.panels
            .get(panel_idx)
            .map(|p| p.rect.height)
            .unwrap_or(0)
    }

    /// Move the List selection to an absolute index. Out-of-range
    /// values clamp to the last valid row.
    pub fn set_selected_index(&mut self, idx: usize) {
        let Some(panel) = self.panels.get_mut(self.focused) else {
            return;
        };
        if !matches!(panel.kind, PanelKind::List { .. }) {
            return;
        }
        let total = panel.line_count;
        if total == 0 {
            return;
        }
        let clamped = idx.min(total - 1) as u16;
        panel.selection_abs = clamped;
        Self::ensure_selection_visible(panel);
    }

    pub fn selected_index(&self) -> Option<usize> {
        // Prefer the focused panel's selection. If the focused panel
        // isn't a list (e.g. a search-style input above a results
        // list), fall back to the first list panel's selection so the
        // user's "what would Enter pick" intent is preserved across
        // focus.
        if let Some(idx) = self.selected_index_at(self.focused) {
            return Some(idx);
        }
        let list_idx = self
            .panels
            .iter()
            .position(|p| matches!(p.kind, PanelKind::List { .. }))?;
        self.selected_index_at(list_idx)
    }
}

impl Component for Dialog {
    fn prepare(&mut self, area: Rect, ctx: &DrawContext) {
        self.area = area;
        self.resolve_panel_rects(area);
        for panel in &mut self.panels {
            if let DialogPanelContent::Widget(widget) = &mut panel.content {
                widget.prepare(panel.rect, ctx);
            }
        }
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
            return KeyResult::Action(WidgetEvent::Dismiss);
        }
        // Ctrl+C always dismisses a dialog (matches legacy behavior).
        if matches!(code, KeyCode::Char('c')) && mods == KeyModifiers::CONTROL {
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

        // Widget panels: route directly to widget. If the widget
        // doesn't handle it (e.g. TextInput ignores Up/Down), fall
        // through to list-panel nav below so the list behaves like a
        // passive picker under the input — fzf-style.
        if let Some(panel) = self.panels.get_mut(self.focused) {
            if let DialogPanelContent::Widget(widget) = &mut panel.content {
                let r = widget.handle_key(code, mods);
                let list_idx = self
                    .panels
                    .iter()
                    .position(|p| matches!(p.kind, PanelKind::List { .. }));
                // Rewrite a widget `Submit` into `Select(list_row)` so
                // Enter on an input panel picks the currently-selected
                // list row instead of falling back to option 0.
                if let (KeyResult::Action(WidgetEvent::Submit), Some(list_idx)) = (&r, list_idx) {
                    if let Some(idx) = self.selected_index_at(list_idx) {
                        return KeyResult::Action(WidgetEvent::Select(idx));
                    }
                }
                if !matches!(r, KeyResult::Ignored) {
                    return r;
                }
                if let Some(list_idx) = list_idx {
                    return self.list_nav_on(list_idx, code, mods);
                }
                return r;
            }
        }

        // Focused-panel-specific routing.
        let (focused_kind, focused_height) = {
            let Some(panel) = self.panels.get(self.focused) else {
                return KeyResult::Ignored;
            };
            (panel.kind, panel.rect.height)
        };
        match focused_kind {
            PanelKind::List { .. } => {
                let nav = match (code, mods) {
                    (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                        self.move_selection(-1);
                        Some(KeyResult::Consumed)
                    }
                    (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                        self.move_selection(1);
                        Some(KeyResult::Consumed)
                    }
                    (KeyCode::PageUp, _) | (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                        let page = focused_height.max(1) as isize / 2;
                        self.move_selection(-page);
                        Some(KeyResult::Consumed)
                    }
                    (KeyCode::PageDown, _) | (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                        let page = focused_height.max(1) as isize / 2;
                        self.move_selection(page);
                        Some(KeyResult::Consumed)
                    }
                    (KeyCode::Home, _) | (KeyCode::Char('g'), KeyModifiers::NONE) => {
                        self.move_selection(isize::MIN / 2);
                        Some(KeyResult::Consumed)
                    }
                    (KeyCode::End, _) => {
                        self.move_selection(isize::MAX / 2);
                        Some(KeyResult::Consumed)
                    }
                    (KeyCode::Enter, _) => Some(
                        self.selected_index()
                            .map(|idx| KeyResult::Action(WidgetEvent::Select(idx)))
                            .unwrap_or(KeyResult::Ignored),
                    ),
                    _ => None,
                };
                if let Some(r) = nav {
                    return r;
                }
                // Picker-style fall-through: any key the list didn't
                // claim gets forwarded to the first sibling Widget
                // panel — for a dialog with `TextInput` above the
                // list, this lets typing update a live filter while
                // the list keeps nav focus (fzf UX).
                let widget_idx = self
                    .panels
                    .iter()
                    .position(|p| matches!(p.content, DialogPanelContent::Widget(_)));
                if let Some(idx) = widget_idx {
                    if let Some(panel) = self.panels.get_mut(idx) {
                        if let DialogPanelContent::Widget(w) = &mut panel.content {
                            let r = w.handle_key(code, mods);
                            if !matches!(r, KeyResult::Ignored) {
                                return r;
                            }
                        }
                    }
                }
                // No typing sink: preserve the digit-shortcut for
                // options-style list dialogs.
                if let (KeyCode::Char(c), KeyModifiers::NONE) = (code, mods) {
                    if c.is_ascii_digit() {
                        let idx = (c as u8 - b'1') as usize;
                        return KeyResult::Action(WidgetEvent::Select(idx));
                    }
                }
                KeyResult::Ignored
            }
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
                    let page = focused_height.max(1) as isize / 2;
                    self.scroll_focused(-page);
                    KeyResult::Consumed
                }
                (KeyCode::PageDown, _) | (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                    let page = focused_height.max(1) as isize / 2;
                    self.scroll_focused(page);
                    KeyResult::Consumed
                }
                _ => KeyResult::Ignored,
            },
        }
    }

    fn handle_mouse(&mut self, event: MouseEvent) -> KeyResult {
        let row = event.row;
        let col = event.column;

        match event.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let delta: isize = match event.kind {
                    MouseEventKind::ScrollUp => -3,
                    MouseEventKind::ScrollDown => 3,
                    _ => 0,
                };
                let panel_idx = self.panel_at(row, col).unwrap_or(self.focused);
                self.panel_scroll_by(panel_idx, delta);
                KeyResult::Consumed
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let Some(panel_idx) = self.panel_at(row, col) else {
                    // Click landed on dialog chrome (top rule, hints).
                    // Absorb so it doesn't fall through to layers
                    // beneath, but do nothing.
                    return KeyResult::Consumed;
                };
                let kind = self.panels[panel_idx].kind;
                let is_widget = matches!(
                    self.panels[panel_idx].content,
                    DialogPanelContent::Widget(_)
                );

                match (is_widget, kind) {
                    (true, _) => {
                        self.focus_panel(panel_idx);
                        if let Some(panel) = self.panels.get_mut(panel_idx) {
                            if let DialogPanelContent::Widget(w) = &mut panel.content {
                                let r = w.handle_mouse(event);
                                if !matches!(r, KeyResult::Ignored) {
                                    return r;
                                }
                            }
                        }
                        KeyResult::Consumed
                    }
                    (false, PanelKind::List { .. }) => {
                        // fzf-style: clicking a list row updates the
                        // selection but does NOT change keyboard focus
                        // — keymaps stay on the input above (if any).
                        let panel = &self.panels[panel_idx];
                        let scroll_top = panel.win().map(|w| w.scroll_top).unwrap_or(0) as isize;
                        let rel = (row as isize) - (panel.rect.top as isize);
                        if rel < 0 || rel >= panel.rect.height as isize {
                            return KeyResult::Consumed;
                        }
                        let target = scroll_top + rel;
                        let cur = panel.selection_abs as isize;
                        let delta = target - cur;
                        if delta != 0 {
                            self.move_selection_at(panel_idx, delta);
                        }
                        KeyResult::Consumed
                    }
                    (false, PanelKind::Content) => {
                        // Buffer-backed content panel: click focuses it
                        // (no-op for non-focusable title/preview panels)
                        // so subsequent j/k scrolls the right panel.
                        self.focus_panel(panel_idx);
                        KeyResult::Consumed
                    }
                }
            }
            _ => KeyResult::Ignored,
        }
    }

    fn cursor(&self) -> Option<CursorInfo> {
        // List panels show selection as fg-accent tint on the cursor
        // row (painted inside `draw_panel`), not via a terminal
        // cursor glyph. Widgets may expose a hardware cursor.
        let panel = self.panels.get(self.focused)?;
        if let DialogPanelContent::Widget(widget) = &panel.content {
            let ci = widget.cursor()?;
            // Widget returns coords relative to its own draw area
            // (panel.rect). Translate to dialog-relative so the
            // compositor can add this layer's absolute rect.
            let rel_col = panel
                .rect
                .left
                .saturating_sub(self.area.left)
                .saturating_add(ci.col);
            let rel_row = panel
                .rect
                .top
                .saturating_sub(self.area.top)
                .saturating_add(ci.row);
            return Some(CursorInfo {
                col: rel_col,
                row: rel_row,
                style: ci.style,
            });
        }
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
            let (content, line_count) = match spec.content {
                PanelContent::Buffer(buf_id) => {
                    let line_count = bufs.get(&buf_id).map(|b| b.line_count()).unwrap_or(0);
                    let mut view = BufferView::new();
                    if let Some(buf) = bufs.get(&buf_id) {
                        view.sync_from_buffer(buf);
                    }
                    let win = Window::new(
                        WinId(u64::MAX - i as u64),
                        buf_id,
                        WinConfig::Float(FloatConfig::default()),
                    );
                    (
                        DialogPanelContent::Buffer {
                            buf: buf_id,
                            view,
                            win,
                        },
                        line_count,
                    )
                }
                PanelContent::Widget(widget) => {
                    let rows = widget.content_rows();
                    (DialogPanelContent::Widget(widget), rows)
                }
            };
            let _ = LineDecoration::default();
            DialogPanel {
                kind: spec.kind,
                height: spec.height,
                separator_above: spec.separator_above,
                pad_left: spec.pad_left,
                focusable: spec.focusable,
                focus_initial: spec.focus_initial,
                collapse_when_empty: spec.collapse_when_empty,
                content,
                line_count,
                selection_abs: 0,
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
        assert_eq!(r, KeyResult::Action(WidgetEvent::Dismiss));
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
        assert_eq!(r, KeyResult::Action(WidgetEvent::Select(1)));
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
        assert_eq!(r, KeyResult::Action(WidgetEvent::Select(1)));
    }

    #[test]
    fn widget_panel_receives_keys_and_draws() {
        use crate::text_input::TextInput;
        let panels = build_panels(
            vec![PanelSpec::widget(
                Box::new(TextInput::new()),
                PanelHeight::Fixed(1),
            )],
            &std::collections::HashMap::new(),
        );
        let mut dlg = Dialog::new(DialogConfig::default(), panels);
        let area = Rect::new(0, 0, 20, 3);
        dlg.resolve_panel_rects(area);
        // Type "hi" into the widget via the dialog's key routing.
        // TextInput emits a `text_changed` action per edit so callers
        // can subscribe to `WinEvent::TextChanged`.
        assert_eq!(
            dlg.handle_key(KeyCode::Char('h'), KeyModifiers::NONE),
            KeyResult::Action(WidgetEvent::TextChanged)
        );
        assert_eq!(
            dlg.handle_key(KeyCode::Char('i'), KeyModifiers::NONE),
            KeyResult::Action(WidgetEvent::TextChanged)
        );
        // Widget draws the typed text.
        let mut grid = Grid::new(20, 3);
        let mut slice = grid.slice_mut(area);
        dlg.draw(area, &mut slice, &ctx(20, 3));
        assert_eq!(grid.cell(1, 1).symbol, 'h');
        assert_eq!(grid.cell(2, 1).symbol, 'i');
        // Cursor is translated to dialog-relative coords.
        let ci = dlg.cursor().expect("widget cursor");
        assert_eq!((ci.col, ci.row), (3, 1));
    }

    #[test]
    fn panel_at_returns_hit_panel_index() {
        let mut bufs = std::collections::HashMap::new();
        bufs.insert(BufId(1), make_buf(1, &["title"]));
        bufs.insert(BufId(2), make_buf(2, &["body-line"; 30]));
        let panels = build_panels(
            vec![
                PanelSpec::content(BufId(1), PanelHeight::Fixed(1)),
                PanelSpec::content(BufId(2), PanelHeight::Fill),
            ],
            &bufs,
        );
        let mut dlg = Dialog::new(DialogConfig::default(), panels);
        dlg.resolve_panel_rects(Rect::new(0, 0, 20, 10));
        // Row 1 is the Fixed(1) title panel, row 2+ is the Fill body.
        assert_eq!(dlg.panel_at(1, 5), Some(0));
        assert_eq!(dlg.panel_at(5, 5), Some(1));
        // Top rule (row 0) is chrome — no panel.
        assert_eq!(dlg.panel_at(0, 5), None);
    }

    #[test]
    fn apply_panel_scrollbar_drag_moves_scroll_top() {
        let mut bufs = std::collections::HashMap::new();
        bufs.insert(BufId(1), make_buf(1, &["row"; 40]));
        let panels = build_panels(vec![PanelSpec::content(BufId(1), PanelHeight::Fill)], &bufs);
        let mut dlg = Dialog::new(DialogConfig::default(), panels);
        dlg.resolve_panel_rects(Rect::new(0, 0, 20, 10));
        let vp = dlg.panel_viewport(0).expect("panel has viewport");
        let bar = vp.scrollbar.expect("scrollbar visible");
        // Dragging the thumb to the max position snaps scroll_top to
        // max_scroll (total - viewport = 40 - 9 = 31).
        assert!(dlg.apply_panel_scrollbar_drag(0, bar.max_thumb_top()));
        let scroll = match &dlg.panels[0].content {
            DialogPanelContent::Buffer { win, .. } => win.scroll_top,
            _ => unreachable!(),
        };
        assert_eq!(scroll, bar.max_scroll());
    }

    #[test]
    fn apply_panel_scrollbar_drag_ignores_widget_panel() {
        use crate::text_input::TextInput;
        let panels = build_panels(
            vec![PanelSpec::widget(
                Box::new(TextInput::new()),
                PanelHeight::Fixed(1),
            )],
            &std::collections::HashMap::new(),
        );
        let mut dlg = Dialog::new(DialogConfig::default(), panels);
        dlg.resolve_panel_rects(Rect::new(0, 0, 20, 3));
        assert!(!dlg.apply_panel_scrollbar_drag(0, 5));
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
    fn click_on_list_row_updates_selection_without_focus_change() {
        use crate::text_input::TextInput;
        let mut bufs = std::collections::HashMap::new();
        bufs.insert(BufId(1), make_buf(1, &["a", "b", "c", "d"]));
        // fzf-style: input on top is focused, list below is the
        // passive picker. Click on a list row should move selection
        // but keep focus on the input so typing keeps filtering.
        let panels = build_panels(
            vec![
                PanelSpec::widget(Box::new(TextInput::new()), PanelHeight::Fixed(1))
                    .with_initial_focus(true),
                PanelSpec::list(BufId(1), PanelHeight::Fill).with_pad_left(0),
            ],
            &bufs,
        );
        let mut dlg = Dialog::new(DialogConfig::default(), panels);
        let area = Rect::new(0, 0, 20, 10);
        let ctx = ctx(20, 10);
        dlg.prepare(area, &ctx);
        let initial_focus = dlg.focused_panel();
        // Top rule row 0; input row 1; list rows 2..end. Click row 4
        // (list row index 2 = item "c").
        let r = dlg.handle_mouse(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            4,
            5,
        ));
        assert_eq!(r, KeyResult::Consumed);
        assert_eq!(dlg.selected_index_at(1), Some(2));
        // Focus stays on the input panel (fzf-style).
        assert_eq!(dlg.focused_panel(), initial_focus);
    }

    #[test]
    fn click_on_widget_panel_focuses_it_and_forwards() {
        use crate::text_input::TextInput;
        let mut bufs = std::collections::HashMap::new();
        bufs.insert(BufId(1), make_buf(1, &["a", "b", "c"]));
        let mut ti = TextInput::new();
        ti.set_text("hello");
        let panels = build_panels(
            vec![
                PanelSpec::list(BufId(1), PanelHeight::Fixed(3))
                    .with_pad_left(0)
                    .with_initial_focus(true),
                PanelSpec::widget(Box::new(ti), PanelHeight::Fixed(1)),
            ],
            &bufs,
        );
        let mut dlg = Dialog::new(DialogConfig::default(), panels);
        let area = Rect::new(0, 0, 20, 10);
        let ctx = ctx(20, 10);
        dlg.prepare(area, &ctx);
        // Top rule row 0; list rows 1..3; widget row 4. Widget panel
        // has the default pad_left=1, so click at column 4 lands on
        // char index 3 ("hello"[3] = 'l').
        dlg.handle_mouse(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            4,
            4,
        ));
        assert_eq!(dlg.focused_panel(), 1);
        let widget = dlg
            .panel_widget_mut::<TextInput>(1)
            .expect("widget panel");
        assert_eq!(widget.cursor_col(), 3);
    }

    #[test]
    fn wheel_scrolls_panel_under_cursor() {
        let mut bufs = std::collections::HashMap::new();
        bufs.insert(BufId(1), make_buf(1, &["row"; 40]));
        let panels = build_panels(
            vec![PanelSpec::content(BufId(1), PanelHeight::Fill).with_pad_left(0)],
            &bufs,
        );
        let mut dlg = Dialog::new(DialogConfig::default(), panels);
        let area = Rect::new(0, 0, 20, 10);
        let ctx = ctx(20, 10);
        dlg.prepare(area, &ctx);
        let r = dlg.handle_mouse(mouse_event(MouseEventKind::ScrollDown, 5, 5));
        assert_eq!(r, KeyResult::Consumed);
        let scroll = match &dlg.panels[0].content {
            DialogPanelContent::Buffer { win, .. } => win.scroll_top,
            _ => unreachable!(),
        };
        assert!(scroll > 0, "wheel should advance scroll_top");
    }
}
