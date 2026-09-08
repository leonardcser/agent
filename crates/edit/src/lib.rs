//! `smelt-edit` - editor layer over `smelt-term`.
//!
//! `Ui` ties `Window`s (per-buffer view + viewport + scrollbar + gutter)
//! to layout and routes events. Renderer primitives come from `smelt-term`.

pub mod callback;
pub(crate) mod event;
pub mod gutter;
pub(crate) mod modal;
pub(crate) mod motions;
pub mod named;
pub(crate) mod overlay;
mod resize;
use resize::{ResolvedDecoration, ResolvedUiLayout};
pub mod row;
pub mod text;
pub(crate) mod text_objects;
pub mod vim;
pub(crate) mod window;

pub use named::NamedSlots;

pub use smelt_buffer::attachment::AttachmentId;
pub use smelt_buffer::buffer::{
    BufCreateOpts, BufId, Buffer, BufferCopy, BufferParser, CopyOutput, ExtmarkOpts,
    ExtmarkPayload, RangeLayer, SelectionRange, SpanAction, SpanMeta, SpanStyle, LUA_BUF_ID_BASE,
};
pub use smelt_buffer::clipboard::Clipboard;
pub use smelt_buffer::undo::{UndoEntry, UndoHistory};

pub use smelt_term::{
    flush_diff, paint_layout_tree, Align, Border, Cell, CellUpdate, Color, Compositor, Constraint,
    ContainerId, Corner, Grid, GridSlice, Gutters, HitRegistry, LayoutTree, Line, Natural,
    NaturalRef, PaintDispatch, PaintId, Rect, SnapshotFrame, Span, StaticNatural, Style, Theme,
};
pub use smelt_term::{grid, layout};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WinId(pub u64);

impl WinId {
    pub fn raw(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for WinId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "win:{}", self.0)
    }
}

/// Each `WinId` doubles as the `PaintId` for its layout-tree leaf.
impl From<WinId> for PaintId {
    fn from(w: WinId) -> Self {
        PaintId(w.0)
    }
}

/// Routes `Callback::Lua` handles out to the host's Lua runtime.
pub type LuaInvoke<'a> = dyn FnMut(callback::LuaHandle, WinId, &callback::Payload) + 'a;

pub use callback::{
    Callback, CallbackCtx, CallbackResult, KeyBind, LuaHandle, MouseButton, Payload, WinEvent,
};
use callback::{Callbacks, KeymapScope};
pub use event::{Event, Status};
pub use modal::{ModalId, ModalOwner};
pub use overlay::{
    BodyDrag, ChromeAction, ChromeOwner, Decoration, DecorationId, DragConfig, HitTarget, Overlay,
    OverlayId, ResizeConfig, ResizeEdges,
};
pub use row::{
    add_signed_row, display_row_matches, doc_range_for_match, row_match_is_selectable,
    row_to_usize, BufferDocument, DisplayAction, DisplayDocument, DisplayRow, DisplayRows,
    DisplaySnapshot, DocPosition, DocRange, DocumentHandle, MaterializeRequest, MaterializedRows,
    PreparedWindowRequest, RowBreak, RowIndex, StaticRowsDocument, TextRange,
};
pub use vim::VimMode;
pub use window::{
    clamp_scroll, materialized_row_range, resolve_document_command, scroll_to_show,
    CursorScreenRowSelection, CursorShape, DocumentCommand, DocumentCopy, DocumentKeyResult,
    DocumentTextObject, DocumentViewExecutor, DocumentViewScreenRowRestore, DocumentViewState,
    DrawContext, EventCtx, MouseCtx, RowHighlight, RowHighlightMode, RowHighlightRows,
    RowHighlightWidth, RowYankFlash, ScrollbarState, SplitConfig, VerticalScroll, ViewportMetrics,
    Window, WindowSurface, WindowViewport,
};

/// Byte offsets of hard `\n` line breaks in `text`.
pub fn hard_breaks_for_text(text: &str) -> Vec<usize> {
    text.match_indices('\n').map(|(idx, _)| idx).collect()
}

/// Hard break offsets for `lines.join("\n")` without allocating the joined text.
pub fn hard_breaks_for_lines(lines: &[String]) -> Vec<usize> {
    let mut hard = Vec::with_capacity(lines.len().saturating_sub(1));
    let mut pos = 0usize;
    for (i, line) in lines.iter().enumerate() {
        pos += line.len();
        if i + 1 < lines.len() {
            hard.push(pos);
            pos += 1;
        }
    }
    hard
}

/// Byte ranges in `line` that are selectable/searchable after non-selectable
/// display spans are removed.
pub fn selectable_byte_ranges_for_line(
    line: &str,
    spans: &[smelt_buffer::buffer::Span],
) -> Vec<std::ops::Range<usize>> {
    if line.is_empty() {
        return Vec::new();
    }
    let line_cells = text::byte_to_cell(line, line.len());
    let mut blocked: Vec<(usize, usize)> = spans
        .iter()
        .filter(|span| !span.meta.selectable)
        .map(|span| {
            (
                (span.col_start as usize).min(line_cells),
                (span.col_end as usize).min(line_cells),
            )
        })
        .filter(|(start, end)| start < end)
        .collect();
    if blocked.is_empty() {
        return std::iter::once(0..line.len()).collect();
    }
    blocked.sort_unstable();

    let mut ranges = Vec::new();
    let mut cell = 0usize;
    for (start, end) in blocked {
        if start > cell {
            let byte_start = text::cell_to_byte(line, cell);
            let byte_end = text::cell_to_byte(line, start);
            if byte_start < byte_end {
                ranges.push(byte_start..byte_end);
            }
        }
        cell = cell.max(end);
    }
    if cell < line_cells {
        let byte_start = text::cell_to_byte(line, cell);
        if byte_start < line.len() {
            ranges.push(byte_start..line.len());
        }
    }
    ranges
}

/// Cell ranges in `line` that trigger span actions.
pub fn display_actions_for_spans(spans: &[smelt_buffer::buffer::Span]) -> Vec<DisplayAction> {
    spans
        .iter()
        .filter_map(|span| {
            let action = span.meta.action.clone()?;
            let cell_start = span.col_start as usize;
            let cell_end = span.col_end as usize;
            (cell_start < cell_end).then_some(DisplayAction {
                cell_start,
                cell_end,
                action,
            })
        })
        .collect()
}

use std::collections::HashMap;

/// One host-painted leaf from a prepared frame.
pub struct PreparedPaintLeaf {
    pub id: PaintId,
    pub rect: Rect,
    pub context: DrawContext,
}

/// Resolved frame geometry and painter-order operations.
///
/// Hosts may render [`Self::paint_leaves`] off-screen, then pass this plan back
/// to [`Ui::paint_prepared_frame_with_paints`] without resolving layout again.
pub struct PreparedFrame {
    commands: Vec<PreparedFrameCommand>,
    paint_leaves: Vec<PreparedPaintLeaf>,
    theme: std::sync::Arc<Theme>,
}

impl PreparedFrame {
    pub fn paint_leaves(&self) -> &[PreparedPaintLeaf] {
        &self.paint_leaves
    }
}

enum PreparedFrameCommand {
    Clear(Rect),
    Divider {
        divider: layout::SplitDivider,
        active: bool,
    },
    Chrome {
        area: Rect,
        chrome: layout::Chrome,
        context: layout::ChromePaintCtx,
    },
    Window {
        win: WinId,
        rect: Rect,
        context: DrawContext,
    },
    Paint(usize),
}

pub struct Ui {
    bufs: HashMap<BufId, Buffer>,
    wins: HashMap<WinId, Window>,
    next_buf_id: u64,
    next_win_id: u64,
    surface: smelt_term::Surface,
    callbacks: Callbacks,
    /// Insertion order is the secondary z-order sort key; see [`Self::overlays_in_z_order`].
    overlays: Vec<(OverlayId, Overlay)>,
    next_overlay_id: u32,
    /// Modal focus scopes in opening order. Presentation order decides which owns input.
    modals: Vec<(ModalId, modal::Modal)>,
    next_modal_id: u32,
    /// Host-owned containers embedded in the root layout.
    docked_surfaces: Vec<(ContainerId, DockedSurface)>,
    next_docked_surface_id: u64,
    /// Window-owned decorations paint with their owner pane, below later split
    /// leaves and below global overlays.
    decorations: Vec<(DecorationId, Decoration)>,
    next_decoration_id: u32,
    /// Stable-name ↔ resource-id maps for the hot-reload path. Plugins
    /// that pass `opts.name = "foo"` to `buf_create` / `win_open_*` /
    /// `overlay_open` get the same id back on every (re-)load, so
    /// `/reload` can mutate the resource in place instead of tearing
    /// it down and losing scroll/cursor/size. Anonymous resources
    /// (`opts.name == nil`) skip these maps and are reaped on reload.
    named_bufs: NamedSlots<BufId>,
    named_wins: NamedSlots<WinId>,
    named_overlays: NamedSlots<OverlayId>,
    lua_generation_names: Option<LuaGenerationNames>,
    /// `set_focus` pushes the outgoing focus here; overlay-close walks it back.
    focus_history: Vec<WinId>,
    focus: Option<WinId>,
    last_emitted_focus: Option<WinId>,
    /// Gesture target that bypasses hit-testing for the duration of a drag.
    /// Auto-clears when the owning split or overlay disappears.
    capture: Option<HitTarget>,
    /// `(time, row, col, count)` - tracks successive Down events for click-count.
    last_click: Option<(std::time::Instant, u16, u16, u8)>,
    /// Global cursor shape; only the focused window honours it.
    cursor_shape: CursorShape,
    /// Timestamp when edge-drag autoscroll last engaged; drives host tick-rate ramp.
    drag_autoscroll_since: Option<std::time::Instant>,
    /// In-flight content drag position used for edge autoscroll. The pointer,
    /// not the selection endpoint, is the source of truth for whether a drag
    /// wants to keep scrolling.
    drag_autoscroll: Option<DragAutoscroll>,
    /// In-flight chrome drag/resize gesture; `None` when idle.
    chrome_drag: Option<ChromeDrag>,
    split_interaction: layout::SplitInteraction,
    /// Frozen scrollbar geometry for the active pointer gesture.
    scrollbar_drag: Option<ScrollbarDrag>,
    /// Groups of windows whose `scroll_top` is mirrored. Each group tracks the
    /// last value observed per member; the side that moved this frame becomes
    /// the leader and its value is copied to all others. Synced once per
    /// frame via [`Ui::sync_scroll_links`] before paint.
    scroll_groups: Vec<ScrollGroup>,
}

/// Sizing and interaction policy for a root-docked surface.
#[derive(Clone, Copy, Debug)]
pub struct DockedSurfaceConfig {
    pub height: Constraint,
    pub min_height: Option<Constraint>,
    pub max_height: Option<Constraint>,
    pub resize: ResizeConfig,
    /// Rows kept available to surrounding content when `height` is `Fit`.
    pub fit_reserved_rows: u16,
    pub blocks_agent: bool,
}

/// UI-owned root container with stable identity and modal association.
#[derive(Clone, Debug)]
pub struct DockedSurface {
    layout: LayoutTree,
    height: Constraint,
    min_height: Option<Constraint>,
    max_height: Option<Constraint>,
    height_override: Option<u16>,
    resize: ResizeConfig,
    fit_reserved_rows: u16,
    expanded: bool,
    modal: ModalId,
    resolved_rect: Option<Rect>,
}

impl DockedSurface {
    pub fn modal(&self) -> ModalId {
        self.modal
    }

    pub fn expanded(&self) -> bool {
        self.expanded
    }

    pub fn resize_config(&self) -> ResizeConfig {
        self.resize
    }

    pub fn resolved_rect(&self) -> Option<Rect> {
        self.resolved_rect
    }
}

/// One scroll-mirror group. `members` lists the participating window ids;
/// `last` is the post-sync `(scroll_top, scroll_left)` of each member captured
/// the previous frame, used to detect which side moved on which axis this
/// frame. Each axis is leader-detected independently - a horizontal pan on
/// one member mirrors only horizontally, leaving vertical positions untouched.
#[derive(Clone)]
struct ScrollGroup {
    members: Vec<WinId>,
    last: Vec<(RowIndex, u16)>,
}

#[derive(Clone, Default)]
struct LuaGenerationNames {
    bufs: std::collections::HashSet<String>,
    wins: std::collections::HashSet<String>,
    overlays: std::collections::HashSet<String>,
}

#[derive(Clone, Copy, Debug)]
struct DragAutoscroll {
    owner: WinId,
    row: u16,
    edge: Option<DragAutoscrollEdge>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DragAutoscrollEdge {
    Top,
    Bottom,
}

#[derive(Clone, Copy, Debug)]
struct ChromeDrag {
    owner: overlay::ChromeOwner,
    action: overlay::ChromeAction,
    start_rect: Rect,
    origin_row: u16,
    origin_col: u16,
}

#[derive(Clone, Copy, Debug)]
struct ScrollbarDrag {
    owner: WinId,
    rect_top: u16,
    bar: window::ScrollbarState,
    thumb_grab_row: u16,
}

impl Ui {
    pub fn new() -> Self {
        Self {
            bufs: HashMap::new(),
            wins: HashMap::new(),
            next_buf_id: 1,
            next_win_id: 0,
            surface: smelt_term::Surface::new(80, 24),
            callbacks: Callbacks::new(),
            overlays: Vec::new(),
            next_overlay_id: 1,
            modals: Vec::new(),
            next_modal_id: 1,
            docked_surfaces: Vec::new(),
            next_docked_surface_id: 1,
            decorations: Vec::new(),
            next_decoration_id: 1,
            named_bufs: NamedSlots::new(),
            named_wins: NamedSlots::new(),
            named_overlays: NamedSlots::new(),
            lua_generation_names: None,
            focus_history: Vec::new(),
            focus: None,
            last_emitted_focus: None,
            capture: None,
            last_click: None,
            cursor_shape: CursorShape::Hidden,
            drag_autoscroll_since: None,
            drag_autoscroll: None,
            chrome_drag: None,
            split_interaction: layout::SplitInteraction::default(),
            scrollbar_drag: None,
            scroll_groups: Vec::new(),
        }
    }

    /// Clone value and view state for a candidate Lua generation without
    /// copying callbacks. Rust callbacks cannot be cloned, and callbacks from
    /// the committed Lua generation must remain live until the candidate
    /// commits. They are merged explicitly at the commit boundary.
    pub fn fork_for_lua_generation(&self) -> Self {
        Self {
            bufs: self.bufs.clone(),
            wins: self.wins.clone(),
            next_buf_id: self.next_buf_id,
            next_win_id: self.next_win_id,
            surface: self.surface.clone(),
            callbacks: Callbacks::new(),
            overlays: self.overlays.clone(),
            next_overlay_id: self.next_overlay_id,
            modals: self.modals.clone(),
            next_modal_id: self.next_modal_id,
            docked_surfaces: self.docked_surfaces.clone(),
            next_docked_surface_id: self.next_docked_surface_id,
            decorations: self.decorations.clone(),
            next_decoration_id: self.next_decoration_id,
            named_bufs: self.named_bufs.clone(),
            named_wins: self.named_wins.clone(),
            named_overlays: self.named_overlays.clone(),
            lua_generation_names: Some(LuaGenerationNames::default()),
            focus_history: self.focus_history.clone(),
            focus: self.focus,
            last_emitted_focus: None,
            capture: self.capture,
            last_click: self.last_click,
            cursor_shape: self.cursor_shape,
            drag_autoscroll_since: self.drag_autoscroll_since,
            drag_autoscroll: self.drag_autoscroll,
            chrome_drag: self.chrome_drag,
            split_interaction: self.split_interaction,
            scrollbar_drag: self.scrollbar_drag,
            scroll_groups: self.scroll_groups.clone(),
        }
    }

    /// Preserve callbacks installed by Rust when replacing the committed Lua
    /// generation. All Lua callbacks in `retired` are intentionally dropped.
    pub fn merge_rust_callbacks_from(&mut self, retired: &mut Self) {
        let callbacks = retired.callbacks.take_rust_callbacks();
        self.callbacks.merge_rust_callbacks(callbacks);
    }

    /// Link `wins` into a single scroll-mirror group. Any existing groups that
    /// overlap with `wins` are merged into one. Duplicates and unknown ids are
    /// dropped silently. A group with fewer than two live members is discarded.
    pub fn link_scroll(&mut self, wins: &[WinId]) {
        let mut next: Vec<WinId> = Vec::new();
        for w in wins {
            if self.wins.contains_key(w) && !next.contains(w) {
                next.push(*w);
            }
        }
        if next.len() < 2 {
            return;
        }
        // Merge any existing group whose members intersect `next`.
        let mut keep: Vec<ScrollGroup> = Vec::with_capacity(self.scroll_groups.len());
        for g in std::mem::take(&mut self.scroll_groups) {
            if g.members.iter().any(|m| next.contains(m)) {
                for m in g.members {
                    if !next.contains(&m) {
                        next.push(m);
                    }
                }
            } else {
                keep.push(g);
            }
        }
        let last = next
            .iter()
            .map(|w| {
                self.wins
                    .get(w)
                    .map(|win| (win.scroll_top(), win.scroll_left))
                    .unwrap_or((0, 0))
            })
            .collect();
        keep.push(ScrollGroup {
            members: next,
            last,
        });
        self.scroll_groups = keep;
    }

    /// Mirror `(scroll_top, scroll_left)` across every group, axis-by-axis.
    /// Pruning rules:
    /// - drop members whose window no longer exists;
    /// - drop the whole group once it has fewer than two live members.
    ///
    /// Each axis is leader-detected independently: the first member whose
    /// current value drifted from `last` wins on that axis. The leader's
    /// value is written to every other member and to each `last` entry.
    /// Ties (everyone equal on an axis) are a no-op on that axis.
    pub fn sync_scroll_links(&mut self) {
        if self.scroll_groups.is_empty() {
            return;
        }
        let mut groups = std::mem::take(&mut self.scroll_groups);
        groups.retain_mut(|g| {
            // Prune dead members and keep `last` aligned.
            let mut i = 0;
            while i < g.members.len() {
                if self.wins.contains_key(&g.members[i]) {
                    i += 1;
                } else {
                    g.members.remove(i);
                    g.last.remove(i);
                }
            }
            if g.members.len() < 2 {
                return false;
            }
            let now: Vec<(RowIndex, u16)> = g
                .members
                .iter()
                .map(|w| {
                    self.wins
                        .get(w)
                        .map(|win| (win.scroll_top(), win.scroll_left))
                        .unwrap_or((0, 0))
                })
                .collect();
            // Vertical axis.
            let v_leader = now.iter().zip(g.last.iter()).position(|(n, l)| n.0 != l.0);
            let v_target = v_leader.map(|i| now[i].0);
            // Horizontal axis.
            let h_leader = now.iter().zip(g.last.iter()).position(|(n, l)| n.1 != l.1);
            let h_target = h_leader.map(|i| now[i].1);
            for (i, wid) in g.members.iter().enumerate() {
                if let Some(w) = self.wins.get_mut(wid) {
                    if let Some(t) = v_target {
                        if now[i].0 != t {
                            w.pin_scroll(t);
                        }
                    }
                    if let Some(t) = h_target {
                        if now[i].1 != t {
                            w.scroll_left = t;
                        }
                    }
                }
                let row = v_target.unwrap_or(now[i].0);
                let col = h_target.unwrap_or(now[i].1);
                g.last[i] = (row, col);
            }
            true
        });
        self.scroll_groups = groups;
    }

    /// Returns 1/2/3 for successive Downs on the same cell within 400ms; wraps at 4.
    fn record_click(&mut self, row: u16, col: u16, now: std::time::Instant) -> u8 {
        use std::time::Duration;
        let count = match self.last_click {
            Some((t, r, c, n))
                if now.duration_since(t) < Duration::from_millis(400)
                    && r == row
                    && c == col
                    && n < 3 =>
            {
                n + 1
            }
            _ => 1,
        };
        self.last_click = Some((now, row, col, count));
        count
    }

    fn update_drag_autoscroll_pointer(&mut self, target: HitTarget, row: u16) {
        let HitTarget::Window(owner) = target else {
            self.drag_autoscroll = None;
            self.drag_autoscroll_since = None;
            return;
        };
        let edge = self.drag_autoscroll_edge_for(owner, row);
        if self
            .drag_autoscroll
            .is_some_and(|drag| drag.owner == owner && drag.edge != edge)
        {
            self.drag_autoscroll_since = None;
        }
        self.drag_autoscroll = Some(DragAutoscroll { owner, row, edge });
    }

    fn drag_autoscroll_edge_for(&self, owner: WinId, row: u16) -> Option<DragAutoscrollEdge> {
        let win = self.wins.get(&owner)?;
        let viewport = win.viewport?;
        if viewport.rect.height == 0 {
            return None;
        }
        let top = viewport.rect.top;
        let bottom = viewport
            .rect
            .top
            .saturating_add(viewport.rect.height.saturating_sub(1));
        if row <= top {
            Some(DragAutoscrollEdge::Top)
        } else if row >= bottom {
            Some(DragAutoscrollEdge::Bottom)
        } else {
            None
        }
    }

    /// Hit-test a primary-button Down/Drag/Up against any content leaf (splits or overlay).
    /// Latches capture on Down; returns `(target, click_count)` where `click_count`
    /// is 0 for Drag/Up. Up clears capture. Call only when `dispatch_event`
    /// returned `Ignored` - that method owns scrollbar drag and modal blocking.
    /// Overlay leaves participate so custom paint leaves and selectable dialog bodies
    /// can receive captured press / drag / release events.
    pub fn resolve_split_mouse(
        &mut self,
        me: crossterm::event::MouseEvent,
        now: std::time::Instant,
    ) -> Option<(HitTarget, u8)> {
        use crossterm::event::{MouseButton, MouseEventKind};
        match me.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let target = match self.hit_test(me.row, me.column, None)? {
                    HitTarget::Window(w) => HitTarget::Window(w),
                    HitTarget::Paint(p) => HitTarget::Paint(p),
                    _ => return None,
                };
                self.set_capture(target);
                self.update_drag_autoscroll_pointer(target, me.row);
                let count = self.record_click(me.row, me.column, now);
                Some((target, count))
            }
            MouseEventKind::Drag(MouseButton::Left) => match self.capture {
                Some(target @ (HitTarget::Window(_) | HitTarget::Paint(_))) => {
                    self.update_drag_autoscroll_pointer(target, me.row);
                    Some((target, 0))
                }
                _ => None,
            },
            MouseEventKind::Up(MouseButton::Left) => match self.capture {
                Some(target @ (HitTarget::Window(_) | HitTarget::Paint(_))) => {
                    self.clear_capture();
                    Some((target, 0))
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// Replace the splits layout. Clears focus and capture if their targets
    /// are no longer reachable, then restores focus to the active modal.
    pub fn set_layout(&mut self, tree: LayoutTree) {
        self.surface.set_layout(tree);
        self.refresh_docked_surface_rects();
        if let Some(focus) = self.focus {
            if !self.splits().contains_leaf(focus)
                && self.overlay_for_leaf(focus).is_none()
                && self.decoration_for_leaf(focus).is_none()
            {
                self.focus = None;
            }
        }
        let active_modal = self.active_modal();
        if active_modal.is_some() && self.focused_modal() != active_modal {
            self.focus_active_modal();
        }
        if let Some(cap) = self.capture {
            if !self.capture_target_alive(cap) {
                self.clear_capture();
            }
        }
    }

    fn splits(&self) -> &LayoutTree {
        self.surface.layout()
    }

    /// Registered presentation trees: root first, then decorations and overlays.
    /// Docked surfaces appear through their mounted subtree in the root layout.
    /// Unlike resolved geometry, this includes zero-sized and hidden decorations.
    pub fn layout_trees(&self) -> impl Iterator<Item = &LayoutTree> {
        std::iter::once(self.splits())
            .chain(self.decorations.iter().map(|(_, value)| &value.layout))
            .chain(self.overlays.iter().map(|(_, value)| &value.layout))
    }

    fn resolve_splits(&self) -> HashMap<WinId, Rect> {
        let sizer = UiLeafSizer {
            wins: &self.wins,
            bufs: &self.bufs,
        };
        self.splits()
            .resolve(self.surface.area(), &sizer)
            .leaves()
            .map(|(p, r)| (WinId(p.0), r))
            .collect()
    }

    fn resolved_docked_surface_rects(&self) -> HashMap<ContainerId, Rect> {
        let sizer = UiLeafSizer {
            wins: &self.wins,
            bufs: &self.bufs,
        };
        self.splits()
            .resolve(self.surface.area(), &sizer)
            .containers()
            .collect()
    }

    fn refresh_docked_surface_rects(&mut self) {
        let rects = self.resolved_docked_surface_rects();
        for (id, surface) in &mut self.docked_surfaces {
            surface.resolved_rect = rects.get(id).copied();
        }
    }

    pub fn split_rect(&self, win: WinId) -> Option<Rect> {
        self.resolve_splits().get(&win).copied()
    }

    pub fn win_content_width(&self, win: WinId) -> Option<u16> {
        if let Some(rect) = self.paint_rect(PaintId::from(win)) {
            return self.win_content_width_at(win, rect.width);
        }
        self.wins
            .get(&win)?
            .viewport
            .map(|viewport| viewport.content_width)
    }

    fn win_content_width_at(&self, win: WinId, width: u16) -> Option<u16> {
        let window = self.wins.get(&win)?;
        let gutter_width = self
            .bufs
            .get(&window.buf)
            .map(|buf| window.gutter_width(buf))
            .unwrap_or(0)
            .min(width);
        Some(
            window
                .config
                .gutters
                .content_width_with_gutter(width, gutter_width),
        )
    }

    /// Resolve mounted windows' content widths and heights in one layout pass.
    /// The snapshot is valid until layout, content measurements, or gutters change.
    pub fn resolved_win_sizes(&self) -> HashMap<WinId, (u16, u16)> {
        self.resolve_scene(None)
            .layouts()
            .flat_map(|layout| layout.leaves())
            .filter_map(|(id, rect)| {
                let win = WinId(id.0);
                self.win_content_width_at(win, rect.width)
                    .map(|width| (win, (width, rect.height)))
            })
            .collect()
    }

    pub fn buf_create(&mut self, opts: BufCreateOpts) -> BufId {
        let id = BufId(self.next_buf_id);
        self.next_buf_id += 1;
        let buf = Buffer::new(id, opts);
        self.bufs.insert(id, buf);
        id
    }

    /// Create a buffer at an explicit id. Returns `Err` if the id is already occupied.
    /// Lua-minted ids live above `LUA_BUF_ID_BASE`; advancing `next_buf_id` past that
    /// boundary would cause Rust and Lua allocators to collide.
    pub fn buf_create_with_id(&mut self, id: BufId, opts: BufCreateOpts) -> Result<BufId, BufId> {
        if self.bufs.contains_key(&id) {
            return Err(id);
        }
        let buf = Buffer::new(id, opts);
        self.bufs.insert(id, buf);
        // Only advance when the id is in Rust's range - Lua ids have their own counter.
        if id.0 < LUA_BUF_ID_BASE && id.0 >= self.next_buf_id {
            self.next_buf_id = id.0 + 1;
        }
        Ok(id)
    }

    pub fn buf(&self, id: BufId) -> Option<&Buffer> {
        self.bufs.get(&id)
    }

    pub fn buf_mut(&mut self, id: BufId) -> Option<&mut Buffer> {
        self.bufs.get_mut(&id)
    }

    pub fn buffers_mut(&mut self) -> impl Iterator<Item = &mut Buffer> {
        self.bufs.values_mut()
    }

    pub fn buf_destroy(&mut self, id: BufId) -> Option<Buffer> {
        // Native views own their projection and retain their original backing
        // until detachment. Neither resource can be destroyed out from under one.
        if self.wins.values().any(|win| {
            win.native_scratch().is_some() && win.retained_buffers().any(|buf| buf == id)
        }) {
            return None;
        }
        self.named_bufs.unbind_by_id(id);
        self.bufs.remove(&id)
    }

    /// Attach a native document using projection storage owned solely by this
    /// window. Detaching restores the backing buffer and its prior view state.
    pub fn set_window_document(
        &mut self,
        id: WinId,
        source: Option<std::sync::Arc<dyn smelt_buffer::document::RowSource>>,
    ) -> bool {
        let Some(win) = self.wins.get(&id) else {
            return false;
        };
        if let Some(source) = source {
            let scratch = win
                .native_scratch()
                .unwrap_or_else(|| self.buf_create(BufCreateOpts::default()));
            self.bufs.get_mut(&scratch).unwrap().readonly = true;
            self.wins
                .get_mut(&id)
                .unwrap()
                .attach_row_source(source, scratch);
        } else if let Some(scratch) = self.wins.get_mut(&id).unwrap().detach_row_source() {
            self.buf_destroy(scratch);
        }
        true
    }

    pub fn set_window_host_document(&mut self, id: WinId, handle: Option<DocumentHandle>) {
        self.set_window_document(id, None);
        if let Some(win) = self.wins.get_mut(&id) {
            win.set_document_handle(handle);
        }
    }

    fn is_native_scratch(&self, id: BufId) -> bool {
        self.wins
            .values()
            .any(|win| win.native_scratch() == Some(id))
    }

    fn remove_window(&mut self, id: WinId) {
        self.named_wins.unbind_by_id(id);
        if let Some(win) = self.wins.remove(&id) {
            if let Some(scratch) = win.native_scratch() {
                self.buf_destroy(scratch);
            }
        }
    }

    // ── Named resources (hot-reload-survivable handles) ──────────────

    pub fn named_buf(&self, name: &str) -> Option<BufId> {
        self.named_bufs.lookup(name)
    }

    pub fn name_buf(&mut self, name: impl Into<String>, id: BufId) {
        let name = name.into();
        if let Some(names) = &mut self.lua_generation_names {
            names.bufs.insert(name.clone());
        }
        self.named_bufs.bind(name, id);
    }

    pub fn named_win(&self, name: &str) -> Option<WinId> {
        self.named_wins.lookup(name)
    }

    pub fn touch_named_win(&mut self, name: &str) -> Option<WinId> {
        if let Some(names) = &mut self.lua_generation_names {
            names.wins.insert(name.to_string());
        }
        self.named_wins.lookup(name)
    }

    pub fn name_win(&mut self, name: impl Into<String>, id: WinId) {
        let name = name.into();
        if let Some(names) = &mut self.lua_generation_names {
            names.wins.insert(name.clone());
        }
        self.named_wins.bind(name, id);
    }

    pub fn named_overlay(&self, name: &str) -> Option<OverlayId> {
        self.named_overlays.lookup(name)
    }

    pub fn name_overlay(&mut self, name: impl Into<String>, id: OverlayId) {
        let name = name.into();
        if let Some(names) = &mut self.lua_generation_names {
            names.overlays.insert(name.clone());
        }
        self.named_overlays.bind(name, id);
    }

    /// Counts of bound names per registry: `(bufs, wins, overlays)`.
    /// Used by fuzz post-checks to assert named resources survive
    /// `/reload` - anonymous slots get reaped but named ones must
    /// keep their bindings.
    pub fn named_counts(&self) -> (usize, usize, usize) {
        (
            self.named_bufs.names().count(),
            self.named_wins.names().count(),
            self.named_overlays.names().count(),
        )
    }

    // ── Named-refresh shortcuts ──────────────────────────────────────
    //
    // Each `lookup_named_X_mut` fuses the two-step `named_X` →
    // `X_mut` Option chain that every Lua refresh path needs. Callers
    // get both the stable id (to return to Lua) and a mutable reference
    // (to apply opts) in one go.

    pub fn lookup_named_buf_mut(&mut self, name: &str) -> Option<(BufId, &mut Buffer)> {
        if let Some(names) = &mut self.lua_generation_names {
            names.bufs.insert(name.to_string());
        }
        let bid = self.named_bufs.lookup(name)?;
        let buf = self.bufs.get_mut(&bid)?;
        Some((bid, buf))
    }

    pub fn lookup_named_overlay_mut(&mut self, name: &str) -> Option<(OverlayId, &mut Overlay)> {
        if let Some(names) = &mut self.lua_generation_names {
            names.overlays.insert(name.to_string());
        }
        let id = self.named_overlays.lookup(name)?;
        let ov = self
            .overlays
            .iter_mut()
            .find_map(|(oid, ov)| (*oid == id).then_some(ov))?;
        Some((id, ov))
    }

    /// Retire stable Lua resources that the candidate generation did not
    /// re-declare. Re-declared resources retain their ids and view state.
    pub fn finish_lua_generation(&mut self, lua_buf_threshold: u64) -> Vec<u64> {
        let Some(names) = self.lua_generation_names.take() else {
            return Vec::new();
        };
        let stale_overlays: Vec<_> = self
            .named_overlays
            .bindings()
            .into_iter()
            .filter_map(|(name, id)| (!names.overlays.contains(&name)).then_some(id))
            .collect();
        let stale_wins: Vec<_> = self
            .named_wins
            .bindings()
            .into_iter()
            .filter_map(|(name, id)| (!names.wins.contains(&name)).then_some(id))
            .collect();
        let stale_bufs: Vec<_> = self
            .named_bufs
            .bindings()
            .into_iter()
            .filter_map(|(name, id)| (!names.bufs.contains(&name)).then_some(id))
            .collect();

        let mut callback_ids = Vec::new();
        for id in stale_overlays {
            callback_ids.extend(self.overlay_close_tree(id));
        }
        for id in stale_wins {
            callback_ids.extend(self.win_close(id));
        }

        let referenced: std::collections::HashSet<_> = self
            .wins
            .values()
            .flat_map(Window::retained_buffers)
            .collect();
        for id in stale_bufs {
            if id.0 >= lua_buf_threshold && !referenced.contains(&id) {
                self.buf_destroy(id);
            } else {
                self.named_bufs.unbind_by_id(id);
            }
        }
        callback_ids
    }

    /// Close every overlay whose id isn't in `named_overlays` and remove
    /// every Lua-created buffer (id ≥ `lua_buf_threshold`) that no
    /// surviving window references. Used by `/reload` so anonymous
    /// dialogs/pickers from the previous cycle don't linger as ghost
    /// overlays. Named resources are left untouched - plugins recover
    /// them by passing the same `opts.name` on re-open. Returns the
    /// union of Lua callback ids the caller must release.
    pub fn reap_anonymous(&mut self, lua_buf_threshold: u64) -> Vec<u64> {
        let keep_overlays = self.named_overlays.ids_set();
        let doomed: Vec<WinId> = self
            .overlays
            .iter()
            .filter(|(id, _)| !keep_overlays.contains(id))
            .filter_map(|(_, ov)| ov.layout.leaves_in_order().into_iter().next())
            .map(|p| WinId(p.0))
            .collect();
        let doomed_decorations: Vec<DecorationId> =
            self.decorations.iter().map(|(id, _)| *id).collect();
        let mut ids = Vec::new();
        for leaf in doomed {
            ids.extend(self.win_close(leaf));
        }
        for decoration in doomed_decorations {
            ids.extend(self.decoration_close_tree(decoration));
        }
        let referenced: std::collections::HashSet<BufId> = self
            .wins
            .values()
            .flat_map(Window::retained_buffers)
            .collect();
        let named_bufs = self.named_bufs.ids_set();
        let drop_bufs: Vec<BufId> = self
            .bufs
            .keys()
            .copied()
            .filter(|id| id.0 >= lua_buf_threshold)
            .filter(|id| !referenced.contains(id) && !named_bufs.contains(id))
            .collect();
        for id in drop_bufs {
            self.bufs.remove(&id);
        }
        ids
    }

    pub fn win_buf_mut(&mut self, win: WinId) -> Option<&mut Buffer> {
        let id = self.wins.get(&win)?.buf;
        self.bufs.get_mut(&id)
    }

    /// Borrow both a window and a buffer mutably at once. Safe because they live
    /// in disjoint collections inside `Ui`.
    pub fn win_and_buf_mut(
        &mut self,
        win: WinId,
        buf: BufId,
    ) -> (Option<&mut Window>, Option<&mut Buffer>) {
        let win_ref = self.wins.get_mut(&win);
        let buf_ref = self.bufs.get_mut(&buf);
        (win_ref, buf_ref)
    }

    // ── Docked surfaces and modals ───────────────────────────────────

    /// Register a root-docked surface and its modal focus scope.
    pub fn docked_surface_open(
        &mut self,
        layout: LayoutTree,
        leaves: Vec<WinId>,
        config: DockedSurfaceConfig,
    ) -> (ContainerId, ModalId) {
        let id = ContainerId(self.next_docked_surface_id);
        self.next_docked_surface_id = self.next_docked_surface_id.wrapping_add(1);
        let modal = self.open_modal(leaves, config.blocks_agent, ModalOwner::Docked(id));
        self.docked_surfaces.push((
            id,
            DockedSurface {
                layout,
                height: config.height,
                min_height: config.min_height,
                max_height: config.max_height,
                height_override: None,
                resize: config.resize,
                fit_reserved_rows: config.fit_reserved_rows,
                expanded: false,
                modal,
                resolved_rect: None,
            },
        ));
        self.focus_active_modal();
        (id, modal)
    }

    pub fn docked_surface(&self, id: ContainerId) -> Option<&DockedSurface> {
        self.docked_surfaces
            .iter()
            .find_map(|(surface_id, surface)| (*surface_id == id).then_some(surface))
    }

    pub fn active_docked_surface(&self) -> Option<ContainerId> {
        self.docked_surfaces.last().map(|(id, _)| *id)
    }

    fn docked_surface_mut(&mut self, id: ContainerId) -> Option<&mut DockedSurface> {
        self.docked_surfaces
            .iter_mut()
            .find_map(|(surface_id, surface)| (*surface_id == id).then_some(surface))
    }

    /// Clone the canonical surface subtree with its stable container marker attached.
    pub fn docked_surface_layout(&self, id: ContainerId) -> Option<LayoutTree> {
        self.docked_surface(id)
            .map(|surface| surface.layout.clone().with_container(id))
    }

    /// Return the buffer revisions that can affect a docked surface's natural size.
    /// Root layout retention uses this payload-independent key to remeasure a fit
    /// dialog only after one of its mounted leaves changes.
    pub fn docked_surface_buffer_revisions(&self, id: ContainerId) -> Option<Vec<(BufId, u64)>> {
        let surface = self.docked_surface(id)?;
        let leaves = self.modal_leaves(surface.modal)?;
        Some(
            leaves
                .iter()
                .filter_map(|leaf| {
                    let buf_id = self.win(*leaf)?.buf;
                    let revision = self.buf(buf_id)?.changedtick();
                    Some((buf_id, revision))
                })
                .collect(),
        )
    }

    /// Resolve the surface's current height policy against the terminal and its content.
    pub fn docked_surface_height(&mut self, id: ContainerId) -> Option<Constraint> {
        let surface = self.docked_surface(id)?.clone();
        if surface.expanded {
            return Some(Constraint::Fill);
        }
        let term = self.surface.terminal_size();
        let leaves = self.modal_leaves(surface.modal)?.to_vec();
        for leaf in leaves {
            let width = self.win_content_width(leaf).unwrap_or(term.0);
            if let Some(buf) = self.win(leaf).map(|window| window.buf) {
                if let Some(buf) = self.buf_mut(buf) {
                    buf.ensure_rendered_at(width);
                }
            }
        }
        let sizer = UiLeafSizer {
            wins: &self.wins,
            bufs: &self.bufs,
        };
        let natural = surface.layout.natural_size_with(term, &sizer).1;
        let mut height = surface
            .height_override
            .unwrap_or_else(|| resolve_constraint(surface.height, term.1, natural));
        if surface.height_override.is_none() && surface.height == Constraint::Fit {
            height = height.min(term.1.saturating_sub(surface.fit_reserved_rows).max(1));
        }
        if let Some(max_height) = surface.max_height {
            height = height.min(resolve_constraint(max_height, term.1, natural));
        }
        if let Some(min_height) = surface.min_height {
            height = height.max(resolve_constraint(min_height, term.1, natural));
        }
        Some(Constraint::Length(height.clamp(1, term.1.max(1))))
    }

    pub fn docked_surface_toggle_expanded(&mut self, id: ContainerId) -> bool {
        let Some(surface) = self.docked_surface_mut(id) else {
            return false;
        };
        surface.expanded = !surface.expanded;
        true
    }

    /// Remove a docked surface. The caller closes the associated modal after
    /// removing the surface from the root composition.
    pub fn docked_surface_remove(&mut self, id: ContainerId) -> Option<DockedSurface> {
        let index = self
            .docked_surfaces
            .iter()
            .position(|(surface_id, _)| *surface_id == id)?;
        if self
            .chrome_drag
            .is_some_and(|drag| drag.owner == overlay::ChromeOwner::Container(id))
        {
            self.chrome_drag = None;
            self.capture = None;
        }
        Some(self.docked_surfaces.remove(index).1)
    }

    fn open_modal(&mut self, leaves: Vec<WinId>, blocks_agent: bool, owner: ModalOwner) -> ModalId {
        let id = ModalId(self.next_modal_id);
        self.next_modal_id += 1;
        self.modals.push((
            id,
            modal::Modal {
                leaves,
                blocks_agent,
                owner,
            },
        ));
        id
    }

    fn modal_focus_target(&self, leaves: &[WinId]) -> Option<WinId> {
        leaves
            .iter()
            .copied()
            .find(|win| {
                self.wins
                    .get(win)
                    .is_some_and(|window| window.accepts_focus())
            })
            .or_else(|| leaves.first().copied())
    }

    /// Close a modal focus scope and return Lua callback handles owned by it.
    #[must_use]
    pub fn modal_close(&mut self, id: ModalId) -> Vec<u64> {
        let Some(pos) = self.modals.iter().position(|(mid, _)| *mid == id) else {
            return Vec::new();
        };
        self.modals.remove(pos);
        self.callbacks.clear_scope(KeymapScope::Modal(id))
    }

    pub(crate) fn modal(&self, id: ModalId) -> Option<&modal::Modal> {
        self.modals
            .iter()
            .find_map(|(mid, modal)| (*mid == id).then_some(modal))
    }

    fn modal_mut(&mut self, id: ModalId) -> Option<&mut modal::Modal> {
        self.modals
            .iter_mut()
            .find_map(|(mid, modal)| (*mid == id).then_some(modal))
    }

    /// Modal presentation currently above the others. Floating modal overlays
    /// paint above root-docked modals and retain their normal overlay z-order;
    /// root-docked modals fall back to opening order.
    pub fn active_modal(&self) -> Option<ModalId> {
        self.overlays_in_z_order()
            .into_iter()
            .rev()
            .find_map(|(overlay, _)| self.modal_for_overlay(overlay))
            .or_else(|| {
                self.modals.iter().rev().find_map(|(id, modal)| {
                    matches!(modal.owner, ModalOwner::Docked(_)).then_some(*id)
                })
            })
    }

    pub fn focused_modal(&self) -> Option<ModalId> {
        let focus = self.focus?;
        self.modals
            .iter()
            .rev()
            .find_map(|(id, modal)| modal.contains(focus).then_some(*id))
    }

    pub fn active_modal_blocks_agent(&self) -> bool {
        self.active_modal()
            .and_then(|id| self.modal(id))
            .is_some_and(|modal| modal.blocks_agent)
    }

    pub fn active_modal_owner(&self) -> Option<ModalOwner> {
        self.active_modal()
            .and_then(|id| self.modal(id))
            .map(|modal| modal.owner)
    }

    pub fn modal_leaves(&self, id: ModalId) -> Option<&[WinId]> {
        self.modal(id).map(|modal| modal.leaves.as_slice())
    }

    fn modal_for_overlay(&self, overlay: OverlayId) -> Option<ModalId> {
        self.modals
            .iter()
            .find_map(|(id, modal)| (modal.owner == ModalOwner::Overlay(overlay)).then_some(*id))
    }

    pub fn sync_overlay_modal(&mut self, overlay: OverlayId) {
        let Some((is_modal, blocks_agent, leaves)) = self.overlay(overlay).map(|value| {
            (
                value.modal,
                value.blocks_agent,
                value
                    .layout
                    .leaves_in_order()
                    .into_iter()
                    .map(|leaf| WinId(leaf.0))
                    .collect::<Vec<_>>(),
            )
        }) else {
            return;
        };
        let existing = self.modal_for_overlay(overlay);
        match (is_modal, existing) {
            (true, Some(id)) => {
                if let Some(modal) = self.modal_mut(id) {
                    modal.leaves = leaves;
                    modal.blocks_agent = blocks_agent;
                }
            }
            (true, None) => {
                if !leaves.is_empty() {
                    self.open_modal(leaves, blocks_agent, ModalOwner::Overlay(overlay));
                }
            }
            (false, Some(id)) => {
                let _ = self.modal_close(id);
            }
            (false, None) => {}
        }
        // Recomposition preserves a still-mounted focus, including a list that
        // was explicitly focused despite being excluded from automatic focus.
        let retained = self
            .active_modal()
            .and_then(|id| self.modal(id))
            .is_some_and(|modal| self.focus.is_some_and(|win| modal.leaves.contains(&win)));
        if !retained {
            self.focus_active_modal();
        }
    }

    pub fn focus_active_modal(&mut self) -> bool {
        let Some(target) = self
            .active_modal()
            .and_then(|id| self.modal(id))
            .and_then(|modal| self.modal_focus_target(&modal.leaves))
        else {
            return false;
        };
        self.set_focus(target)
    }

    // ── Overlay ──────────────────────────────────────────────────────

    /// Register an overlay and return its `OverlayId`. Modal overlays register
    /// a separate focus scope over the overlay's leaves.
    pub fn overlay_open(&mut self, overlay: Overlay) -> OverlayId {
        let id = OverlayId(self.next_overlay_id);
        self.next_overlay_id += 1;
        let modal = overlay.modal;
        let blocks_agent = overlay.blocks_agent;
        let leaves = overlay
            .layout
            .leaves_in_order()
            .into_iter()
            .map(|leaf| WinId(leaf.0))
            .collect::<Vec<_>>();
        self.overlays.push((id, overlay));
        if modal && !leaves.is_empty() {
            self.open_modal(leaves, blocks_agent, ModalOwner::Overlay(id));
            self.focus_active_modal();
        }
        id
    }

    /// Close an overlay. Returns the removed `Overlay`. Restores focus to the most
    /// recent still-focusable entry in `focus_history`, or clears focus if history
    /// is exhausted. Focus outside the closed overlay is left untouched.
    pub fn overlay_close(&mut self, id: OverlayId) -> Option<Overlay> {
        let pos = self.overlays.iter().position(|(oid, _)| *oid == id)?;
        let (_, removed) = self.overlays.remove(pos);
        self.named_overlays.unbind_by_id(id);
        if let Some(modal) = self.modal_for_overlay(id) {
            let _ = self.modal_close(modal);
        }
        if let Some(cap) = self.capture {
            let owned = match cap {
                HitTarget::Chrome {
                    owner: ChromeOwner::Split(_),
                    ..
                } => !self.capture_target_alive(cap),
                HitTarget::Chrome { owner, .. } => owner == overlay::ChromeOwner::Overlay(id),
                HitTarget::Window(w) | HitTarget::Scrollbar { owner: w } => {
                    removed.layout.contains_leaf(w)
                }
                HitTarget::Paint(p) => removed.layout.contains_leaf(p),
            };
            if owned {
                self.clear_capture();
            }
        }

        if let Some(focused) = self.focus {
            if removed.layout.contains_leaf(focused) {
                self.focus = None;
                while let Some(prior) = self.focus_history.pop() {
                    if self.overlay_for_leaf(prior).is_some()
                        || self.decoration_for_leaf(prior).is_some()
                    {
                        self.focus = Some(prior);
                        break;
                    }
                    if self.splits().contains_leaf(prior)
                        && self.wins.get(&prior).is_some_and(|w| w.accepts_focus())
                    {
                        self.focus = Some(prior);
                        break;
                    }
                }
            }
        }
        if self.active_modal().is_some() {
            self.focus_active_modal();
        }
        Some(removed)
    }

    pub fn overlay(&self, id: OverlayId) -> Option<&Overlay> {
        self.overlays
            .iter()
            .find_map(|(oid, ov)| (*oid == id).then_some(ov))
    }

    pub fn overlay_mut(&mut self, id: OverlayId) -> Option<&mut Overlay> {
        self.overlays
            .iter_mut()
            .find_map(|(oid, ov)| (*oid == id).then_some(ov))
    }

    pub fn overlay_count(&self) -> usize {
        self.overlays.len()
    }

    /// Register a window-owned decoration. Decorations paint inside `owner`'s
    /// rect with owner-local z ordering instead of in the global overlay plane.
    pub fn decoration_open(&mut self, decoration: Decoration) -> DecorationId {
        let id = DecorationId(self.next_decoration_id);
        self.next_decoration_id += 1;
        self.decorations.push((id, decoration));
        id
    }

    /// Close a decoration and every window callback registered inside it.
    /// Window leaves are removed; paint leaves only lose their leaf-scoped callbacks.
    #[must_use]
    pub fn decoration_close_tree(&mut self, id: DecorationId) -> Vec<u64> {
        let Some(pos) = self.decorations.iter().position(|(did, _)| *did == id) else {
            return Vec::new();
        };
        let (_, removed) = self.decorations.remove(pos);
        if let Some(cap) = self.capture {
            let owned = match cap {
                HitTarget::Chrome { .. } => false,
                HitTarget::Window(w) | HitTarget::Scrollbar { owner: w } => {
                    removed.layout.contains_leaf(w)
                }
                HitTarget::Paint(p) => removed.layout.contains_leaf(p),
            };
            if owned {
                self.clear_capture();
            }
        }
        if let Some(focused) = self.focus {
            if removed.layout.contains_leaf(focused) {
                self.focus = None;
                while let Some(prior) = self.focus_history.pop() {
                    if self.overlay_for_leaf(prior).is_some()
                        || self.decoration_for_leaf(prior).is_some()
                    {
                        self.focus = Some(prior);
                        break;
                    }
                    if self.splits().contains_leaf(prior)
                        && self.wins.get(&prior).is_some_and(|w| w.accepts_focus())
                    {
                        self.focus = Some(prior);
                        break;
                    }
                }
            }
        }
        let mut all_ids = Vec::new();
        for leaf in removed.layout.leaves_in_order() {
            let win = WinId(leaf.0);
            self.remove_window(win);
            all_ids.extend(self.close_decorations_owned_by(win));
            all_ids.extend(self.callbacks.clear_all(win));
        }
        all_ids
    }

    pub fn decoration(&self, id: DecorationId) -> Option<&Decoration> {
        self.decorations
            .iter()
            .find_map(|(did, dec)| (*did == id).then_some(dec))
    }

    pub fn decoration_for_leaf(&self, win: WinId) -> Option<DecorationId> {
        for (id, dec) in &self.decorations {
            if dec.layout.contains_leaf(win) {
                return Some(*id);
            }
        }
        None
    }

    pub fn decoration_for_paint(&self, paint: PaintId) -> Option<DecorationId> {
        for (id, dec) in &self.decorations {
            if dec.layout.contains_leaf(paint) {
                return Some(*id);
            }
        }
        None
    }

    fn decoration_ids_owned_by(&self, owner: WinId) -> Vec<DecorationId> {
        self.decorations
            .iter()
            .filter_map(|(id, dec)| (dec.owner == owner).then_some(*id))
            .collect()
    }

    fn close_decorations_owned_by(&mut self, owner: WinId) -> Vec<u64> {
        let ids = self.decoration_ids_owned_by(owner);
        let mut callbacks = Vec::new();
        for id in ids {
            callbacks.extend(self.decoration_close_tree(id));
        }
        callbacks
    }

    fn decorations_in_z_order(&self) -> Vec<(DecorationId, &Decoration)> {
        let mut entries: Vec<(DecorationId, &Decoration)> = self
            .decorations
            .iter()
            .map(|(id, dec)| (*id, dec))
            .collect();
        entries.sort_by_key(|(_, dec)| dec.z);
        entries
    }

    fn overlays_in_z_order(&self) -> Vec<(OverlayId, &Overlay)> {
        let mut entries: Vec<(OverlayId, &Overlay)> =
            self.overlays.iter().map(|(id, o)| (*id, o)).collect();
        entries.sort_by_key(|(_, o)| o.z);
        entries
    }

    /// Overlay backing the active modal, if the modal is floating rather than
    /// mounted in the root layout.
    pub fn active_modal_overlay(&self) -> Option<OverlayId> {
        match self.active_modal_owner() {
            Some(ModalOwner::Overlay(overlay)) => Some(overlay),
            Some(ModalOwner::Docked(_)) | None => None,
        }
    }

    /// Overlay whose layout contains the currently-focused window, if any.
    pub fn focused_overlay(&self) -> Option<OverlayId> {
        let focused = self.focus()?;
        self.overlays
            .iter()
            .find_map(|(id, ov)| ov.layout.contains_leaf(focused).then_some(*id))
    }

    /// Pan the wheel-scrollable leaf at `(row, col)` by `delta` visual rows.
    /// Called from both the coalesced-wheel flush in the host and the
    /// `MouseEventKind::Scroll{Up,Down}` arm of `dispatch_event` (which fires
    /// when wheel events bypass coalescing, e.g. an overlay is focused).
    /// Returns `true` when something was actually panned.
    pub fn scroll_at(&mut self, row: u16, col: u16, delta: isize) -> bool {
        if delta == 0 {
            return false;
        }
        let target_win = match self.hit_test(row, col, None) {
            Some(HitTarget::Window(w)) => Some(w),
            Some(HitTarget::Scrollbar { owner }) => Some(owner),
            _ => None,
        };
        let Some(w) = target_win else {
            return false;
        };
        let leaf_info = self.wins.get(&w).and_then(|win| {
            win.supports_wheel_scroll().then(|| {
                win.viewport
                    .map(|vp| (win.buf, vp.rect.height, vp.content_width))
            })?
        });
        let Some((buf_id, vp_height, vp_width)) = leaf_info else {
            return false;
        };
        if let Some(buf) = self.bufs.get_mut(&buf_id) {
            buf.ensure_rendered_at(vp_width);
        }
        let (win, buf) = self.win_and_buf_mut(w, buf_id);
        if let (Some(win), Some(buf)) = (win, buf) {
            win.ensure_layout(buf, vp_width);
            win.pan_by_lines(buf, delta, vp_height);
            true
        } else {
            false
        }
    }

    /// Horizontal twin of [`Self::scroll_at`]. Routes a column-delta to the
    /// hit window's `pan_by_columns`. Returns whether anything panned.
    pub fn scroll_at_horizontal(&mut self, row: u16, col: u16, delta: isize) -> bool {
        if delta == 0 {
            return false;
        }
        let target_win = match self.hit_test(row, col, None) {
            Some(HitTarget::Window(w)) => Some(w),
            Some(HitTarget::Scrollbar { owner }) => Some(owner),
            _ => None,
        };
        let Some(w) = target_win else {
            return false;
        };
        let vp_width = self.wins.get(&w).and_then(|win| {
            win.supports_wheel_scroll()
                .then(|| win.viewport.map(|vp| vp.content_width))?
        });
        let Some(vp_width) = vp_width else {
            return false;
        };
        if let Some(win) = self.wins.get_mut(&w) {
            win.pan_by_columns(delta, vp_width);
            true
        } else {
            false
        }
    }

    /// Hit-test a screen position. Checks overlays (topmost-z first, modal-aware)
    /// then splits leaves. Scrollbar column returns `HitTarget::Scrollbar`.
    pub fn hit_test(&self, row: u16, col: u16, cursor: Option<(u16, u16)>) -> Option<HitTarget> {
        self.hit_test_in(&self.resolve_scene(cursor), row, col)
    }

    fn hit_test_in(&self, scene: &ResolvedUiLayout, row: u16, col: u16) -> Option<HitTarget> {
        if let Some((_, target)) = self.overlay_hit_test_in(scene, row, col) {
            return Some(target);
        }
        for decoration in scene.decorations.iter().rev() {
            if let Some(target) = self.layout_hit_test(&decoration.layout, row, col) {
                return Some(target);
            }
        }
        self.docked_surface_chrome_hit_test(&scene.root, row, col)
            .or_else(|| self.layout_hit_test(&scene.root, row, col))
    }

    fn docked_surface_chrome_hit_test(
        &self,
        layout: &layout::ResolvedLayout,
        row: u16,
        col: u16,
    ) -> Option<HitTarget> {
        layout.containers().rev().find_map(|(id, rect)| {
            let surface = self.docked_surface(id)?;
            if !rect.contains(row, col) {
                return None;
            }
            let action = resize_chrome_action(rect, surface.resize, row, col);
            (action != overlay::ChromeAction::None).then_some(HitTarget::Chrome {
                owner: overlay::ChromeOwner::Container(id),
                action,
            })
        })
    }

    /// Hit-test against overlays only. The active modal is opaque at its rect for
    /// lower-z overlays; higher-z overlays still receive clicks on the parts that
    /// cover the modal.
    fn overlay_hit_test_in(
        &self,
        scene: &ResolvedUiLayout,
        row: u16,
        col: u16,
    ) -> Option<(OverlayId, HitTarget)> {
        let modal = self
            .active_modal_overlay()
            .and_then(|id| scene.overlays.iter().find(|overlay| overlay.id == id));
        for resolved in scene.overlays.iter().rev() {
            let (id, rect) = (resolved.id, resolved.rect);
            if !rect.contains(row, col) {
                continue;
            }
            // The active modal blocks lower-z overlays at its rectangle.
            if modal.is_some_and(|modal| resolved.z < modal.z && modal.rect.contains(row, col)) {
                continue;
            }
            if let Some(target) = self.layout_hit_test(&resolved.layout, row, col) {
                return Some((id, target));
            }
            let overlay = self.overlay(id)?;
            return Some((
                id,
                HitTarget::Chrome {
                    owner: ChromeOwner::Overlay(id),
                    action: chrome_action(rect, overlay, row, col),
                },
            ));
        }
        None
    }

    #[cfg(test)]
    fn overlay_hit_test(
        &self,
        row: u16,
        col: u16,
        cursor: Option<(u16, u16)>,
    ) -> Option<(OverlayId, HitTarget)> {
        self.overlay_hit_test_in(&self.resolve_scene(cursor), row, col)
    }

    /// Returns z-ordered (lowest first) overlay rects. Overlays whose anchor
    /// cannot resolve (missing cursor / missing Win target) are silently skipped.
    fn resolve_overlays(&self, cursor: Option<(u16, u16)>) -> Vec<(OverlayId, Rect, &Overlay)> {
        self.resolve_overlays_with_rects(cursor, &self.resolve_splits())
    }

    fn resolve_overlays_with_rects(
        &self,
        cursor: Option<(u16, u16)>,
        split_rects: &HashMap<WinId, Rect>,
    ) -> Vec<(OverlayId, Rect, &Overlay)> {
        let (term_w, term_h) = self.surface.terminal_size();
        let ctx = overlay::AnchorContext {
            term_width: term_w,
            term_height: term_h,
            cursor,
            win_rects: split_rects,
        };
        let mut out = Vec::with_capacity(self.overlays.len());
        for (id, ov) in self.overlays_in_z_order() {
            let size = ov
                .size_override
                .unwrap_or_else(|| resolve_overlay_size(ov, (term_w, term_h), self));
            if let Some(rect) = overlay::resolve_anchor(&ov.anchor, size, &ctx) {
                out.push((id, rect, ov));
            }
        }
        out
    }

    /// Returns owner-local z-ordered decoration rects. Decorations are skipped
    /// when their owner is not present in the main split layout.
    fn resolve_decorations(&self) -> Vec<(DecorationId, WinId, Rect, &Decoration)> {
        self.resolve_decorations_with_rects(&self.resolve_splits())
    }

    fn resolve_decorations_with_rects(
        &self,
        split_rects: &HashMap<WinId, Rect>,
    ) -> Vec<(DecorationId, WinId, Rect, &Decoration)> {
        let sizer = UiLeafSizer {
            wins: &self.wins,
            bufs: &self.bufs,
        };
        let mut out = Vec::with_capacity(self.decorations.len());
        for (id, dec) in self.decorations_in_z_order() {
            let Some(owner_rect) = split_rects.get(&dec.owner).copied() else {
                continue;
            };
            let size = resolve_decoration_size(dec, owner_rect, &sizer);
            let rect = overlay::resolve_owner_anchor(
                owner_rect,
                size,
                dec.align,
                dec.row_offset,
                dec.col_offset,
            );
            out.push((id, dec.owner, rect, dec));
        }
        out
    }

    pub fn win_open_split(&mut self, buf: BufId, config: SplitConfig) -> Option<WinId> {
        if !self.bufs.contains_key(&buf) || self.is_native_scratch(buf) {
            return None;
        }
        while self.wins.contains_key(&WinId(self.next_win_id)) {
            self.next_win_id += 1;
        }
        let id = WinId(self.next_win_id);
        self.next_win_id += 1;
        let win = Window::new(id, buf, config);
        self.wins.insert(id, win);
        Some(id)
    }

    /// Open a window at an explicit `WinId`. Returns `false` if the id is occupied
    /// or the buffer doesn't exist. Use when callers need a stable id for Lua callbacks.
    pub fn win_open_split_at(&mut self, id: WinId, buf: BufId, config: SplitConfig) -> bool {
        if self.wins.contains_key(&id)
            || !self.bufs.contains_key(&buf)
            || self.is_native_scratch(buf)
        {
            return false;
        }
        let win = Window::new(id, buf, config);
        self.wins.insert(id, win);
        true
    }

    /// Close an overlay and every leaf callback registered inside it.
    /// Window leaves are removed; paint leaves only lose their leaf-scoped callbacks.
    #[must_use]
    pub fn overlay_close_tree(&mut self, id: OverlayId) -> Vec<u64> {
        let mut all_ids = Vec::new();
        if let Some(removed) = self.overlay_close(id) {
            for leaf in removed.layout.leaves_in_order() {
                let win = WinId(leaf.0);
                self.remove_window(win);
                all_ids.extend(self.close_decorations_owned_by(win));
                all_ids.extend(self.callbacks.clear_all(win));
            }
        }
        all_ids.extend(self.callbacks.clear_scope(KeymapScope::Overlay(id)));
        all_ids
    }

    /// Close a window. Returns Lua callback ids for the caller to drop from the Lua registry.
    /// When `id` is an overlay or decoration leaf, closes the whole owner tree.
    #[must_use]
    pub fn win_close(&mut self, id: WinId) -> Vec<u64> {
        if let Some(overlay_id) = self.overlay_for_leaf(id) {
            return self.overlay_close_tree(overlay_id);
        }
        if let Some(decoration_id) = self.decoration_for_leaf(id) {
            return self.decoration_close_tree(decoration_id);
        }
        self.remove_window(id);
        if self.focus == Some(id) {
            self.focus = None;
        }
        let mut all_ids = self.close_decorations_owned_by(id);
        all_ids.extend(self.callbacks.clear_all(id));
        all_ids
    }

    // ── Callbacks ────────────────────────────────────────────────────

    /// Bind a key to a callback. Returns the displaced callback, if any.
    #[must_use]
    pub fn win_set_keymap(&mut self, win: WinId, key: KeyBind, cb: Callback) -> Option<Callback> {
        self.callbacks.set_keymap(win, key, cb)
    }

    #[must_use]
    pub fn win_clear_keymap(&mut self, win: WinId, key: KeyBind) -> Option<Callback> {
        self.callbacks.clear_keymap(win, key)
    }

    /// Catch-all key handler; runs after specific keymaps miss. Returns the displaced callback.
    #[must_use]
    pub fn win_set_key_fallback(&mut self, win: WinId, cb: Callback) -> Option<Callback> {
        self.callbacks.set_key_fallback(win, cb)
    }

    /// Bind a key on an overlay. Fires when any leaf of the overlay holds focus,
    /// after a per-window keymap miss but before global Lua keymaps.
    /// Returns the displaced callback, if any.
    #[must_use]
    pub fn overlay_set_keymap(
        &mut self,
        overlay: OverlayId,
        key: KeyBind,
        cb: Callback,
    ) -> Option<Callback> {
        self.callbacks
            .set_scoped_keymap(KeymapScope::Overlay(overlay), key, cb)
    }

    #[must_use]
    pub fn overlay_clear_keymap(&mut self, overlay: OverlayId, key: KeyBind) -> Option<Callback> {
        self.callbacks
            .clear_scoped_keymap(KeymapScope::Overlay(overlay), key)
    }

    /// Remove every overlay-scoped binding. Returns Lua handle ids for caller cleanup.
    #[must_use]
    pub fn overlay_clear_callbacks(&mut self, overlay: OverlayId) -> Vec<u64> {
        self.callbacks.clear_scope(KeymapScope::Overlay(overlay))
    }

    /// Bind a key across every leaf in a modal focus scope.
    #[must_use]
    pub fn modal_set_keymap(
        &mut self,
        modal: ModalId,
        key: KeyBind,
        cb: Callback,
    ) -> Option<Callback> {
        self.callbacks
            .set_scoped_keymap(KeymapScope::Modal(modal), key, cb)
    }

    #[must_use]
    pub fn modal_clear_keymap(&mut self, modal: ModalId, key: KeyBind) -> Option<Callback> {
        self.callbacks
            .clear_scoped_keymap(KeymapScope::Modal(modal), key)
    }

    /// Register an event callback. Multiple callbacks per (win, event) fire in registration order.
    pub fn win_on_event(&mut self, win: WinId, ev: WinEvent, cb: Callback) {
        self.callbacks.on_event(win, ev, cb);
    }

    pub fn has_win_event(&self, win: WinId, ev: WinEvent) -> bool {
        self.callbacks.has_event(win, ev)
    }

    /// Remove a specific event callback by Lua handle id.
    #[must_use]
    pub fn win_clear_event_by_id(&mut self, win: WinId, ev: WinEvent, id: u64) -> Option<Callback> {
        self.callbacks.clear_event_by_id(win, ev, id)
    }

    /// Register an event callback on a generic layout leaf. Window leaves use
    /// their normal `WinId`; paint leaves share the same raw layout id space.
    pub fn leaf_on_event(&mut self, leaf: PaintId, ev: WinEvent, cb: Callback) {
        self.callbacks.on_event(WinId(leaf.0), ev, cb);
    }

    /// Remove a specific generic-leaf event callback by Lua handle id.
    #[must_use]
    pub fn leaf_clear_event_by_id(
        &mut self,
        leaf: PaintId,
        ev: WinEvent,
        id: u64,
    ) -> Option<Callback> {
        self.callbacks.clear_event_by_id(WinId(leaf.0), ev, id)
    }

    /// Remove every callback associated with a generic layout leaf.
    #[must_use]
    pub fn leaf_clear_callbacks(&mut self, leaf: PaintId) -> Vec<u64> {
        self.callbacks.clear_all(WinId(leaf.0))
    }

    /// Fire a `WinEvent` on `win`. Callbacks registered on `win` for `ev` fire in
    /// registration order. The event does not bubble to other leaves - consumers that
    /// need to catch events from multiple panels (e.g. `dialog.lua`) register on each
    /// relevant leaf.
    pub fn fire_win_event(
        &mut self,
        win: WinId,
        ev: WinEvent,
        payload: Payload,
        lua_invoke: &mut LuaInvoke,
    ) {
        let Some(mut cbs) = self.callbacks.take_event(win, ev) else {
            return;
        };
        for cb in cbs.iter_mut() {
            match cb {
                Callback::Rust(inner) => {
                    let mut ctx = CallbackCtx {
                        ui: self,
                        win,
                        payload: payload.clone(),
                    };
                    let _ = inner(&mut ctx);
                }
                Callback::Lua(handle) => {
                    lua_invoke(*handle, win, &payload);
                }
            }
        }
        self.callbacks.restore_event(win, ev, cbs);
    }

    pub fn win(&self, id: WinId) -> Option<&Window> {
        self.wins.get(&id)
    }

    pub fn win_mut(&mut self, id: WinId) -> Option<&mut Window> {
        self.wins.get_mut(&id)
    }

    /// Read-only iterator over every live `(WinId, &Window)` pair. Order
    /// is unspecified (backed by `HashMap`), so callers must not rely on it.
    pub fn iter_wins(&self) -> impl Iterator<Item = (WinId, &Window)> {
        self.wins.iter().map(|(id, w)| (*id, w))
    }

    pub fn yank_flash_active(&self, now: std::time::Instant) -> bool {
        self.wins
            .values()
            .any(|win| win.yank_flash_until().is_some_and(|until| until > now))
    }

    /// Read-only iterator over every live `(BufId, &Buffer)` pair. Order
    /// is unspecified (backed by `HashMap`), so callers must not rely on it.
    pub fn iter_bufs(&self) -> impl Iterator<Item = (BufId, &Buffer)> {
        self.bufs.iter().map(|(id, b)| (*id, b))
    }

    pub fn set_terminal_size(&mut self, w: u16, h: u16) {
        let old = self.surface.terminal_size();
        let new = (w, h);
        if old != new {
            self.reflow_overlay_size_overrides(old, new);
        }
        self.surface.set_terminal_size(w, h);
        self.refresh_docked_surface_rects();
    }

    fn reflow_overlay_size_overrides(&mut self, old_term: (u16, u16), new_term: (u16, u16)) {
        let updates: Vec<(OverlayId, (u16, u16))> = {
            let sizer = UiLeafSizer {
                wins: &self.wins,
                bufs: &self.bufs,
            };
            self.overlays
                .iter()
                .filter_map(|(id, ov)| {
                    let (override_w, override_h) = ov.size_override?;
                    let old_declared = resolve_overlay_size(ov, old_term, &sizer);
                    let new_declared = resolve_overlay_size(ov, new_term, &sizer);
                    let next_w = if override_w == old_declared.0 {
                        new_declared.0
                    } else {
                        override_w.min(new_term.0)
                    };
                    let next_h = if override_h == old_declared.1 {
                        new_declared.1
                    } else {
                        override_h.min(new_term.1)
                    };
                    let next = (next_w, next_h);
                    (next != (override_w, override_h)).then_some((*id, next))
                })
                .collect()
        };

        for (id, size) in updates {
            if let Some((_, ov)) = self.overlays.iter_mut().find(|(oid, _)| *oid == id) {
                ov.size_override = Some(size);
            }
        }
    }

    pub fn terminal_size(&self) -> (u16, u16) {
        self.surface.terminal_size()
    }

    // ── Focus ──────────────────────────────────────────────────────

    pub fn focus(&self) -> Option<WinId> {
        self.focus
    }

    pub fn focused_window(&self) -> Option<&Window> {
        self.wins.get(&self.focus()?)
    }

    /// Leaf that currently drives the single visible terminal cursor. During a mouse
    /// drag-select the cursor temporarily moves to the dragging leaf (so the user sees
    /// feedback at the drag end even on a non-focusable leaf like a notification);
    /// otherwise it follows keyboard focus. The renderer queries this once per frame
    /// and routes `cursor_shape` accordingly so only one leaf paints a block.
    pub fn active_cursor_leaf(&self) -> Option<WinId> {
        if let Some(HitTarget::Window(w)) = self.capture {
            if self.wins.get(&w).is_some_and(|win| win.drag_active()) {
                return Some(w);
            }
        }
        self.focus
    }

    /// `true` if any window currently has an active drag (mid-drag `drag_endpoint`).
    /// `render_loop` reads this so the global `cursor_shape` flips to `Block` for the
    /// duration of the drag, even when keyboard focus is on a leaf that normally has
    /// no visible cursor.
    pub fn any_drag_active(&self) -> bool {
        if let Some(HitTarget::Window(w)) = self.capture {
            return self.wins.get(&w).is_some_and(|win| win.drag_active());
        }
        false
    }

    /// Focus `win`. Returns `false` unless it belongs to the active modal, when
    /// one exists, or is otherwise a focusable split, decoration, or overlay
    /// leaf. Re-focusing the current window is a no-op (no history push).
    pub fn set_focus(&mut self, win: WinId) -> bool {
        let active_modal = self.active_modal();
        let is_active_modal_leaf = active_modal
            .and_then(|modal| self.modal(modal))
            .is_some_and(|modal| modal.contains(win));
        if active_modal.is_some() && !is_active_modal_leaf {
            return false;
        }

        let prior = self.focus;
        if prior == Some(win) {
            return true;
        }
        let is_split_leaf = self.splits().contains_leaf(win)
            && self.wins.get(&win).is_some_and(|w| w.accepts_focus());
        let is_overlay_leaf = self.overlay_for_leaf(win).is_some();
        let is_decoration_leaf = self.decoration_for_leaf(win).is_some()
            && self.wins.get(&win).is_some_and(|w| w.accepts_focus());
        let is_active_modal_window = is_active_modal_leaf && self.wins.contains_key(&win);
        if !is_split_leaf && !is_overlay_leaf && !is_decoration_leaf && !is_active_modal_window {
            return false;
        }
        if let Some(p) = prior {
            self.focus_history.push(p);
        }
        self.focus = Some(win);
        true
    }

    /// Returns the `OverlayId` of the open overlay whose layout contains `win`, if any.
    pub fn overlay_for_leaf(&self, win: WinId) -> Option<OverlayId> {
        for (id, ov) in &self.overlays {
            if ov.layout.contains_leaf(win) {
                return Some(*id);
            }
        }
        None
    }

    /// Returns the `OverlayId` of the open overlay whose layout contains `paint`, if any.
    pub fn overlay_for_paint(&self, paint: PaintId) -> Option<OverlayId> {
        for (id, ov) in &self.overlays {
            if ov.layout.contains_leaf(paint) {
                return Some(*id);
            }
        }
        None
    }

    #[cfg(test)]
    fn focus_history(&self) -> &[WinId] {
        &self.focus_history
    }

    #[cfg(test)]
    fn focus_next(&mut self) -> bool {
        self.focus_step(1)
    }

    #[cfg(test)]
    fn focus_prev(&mut self) -> bool {
        self.focus_step(-1)
    }

    #[cfg(test)]
    fn focus_step(&mut self, dir: i32) -> bool {
        let Some(modal_id) = self.active_modal() else {
            return false;
        };
        let Some(modal) = self.modal(modal_id) else {
            return false;
        };
        let leaves: Vec<WinId> = modal
            .leaves
            .iter()
            .copied()
            .filter(|win| self.wins.contains_key(win))
            .collect();
        if leaves.is_empty() {
            return false;
        }
        let current = self.focus();
        let current_idx = current
            .and_then(|w| leaves.iter().position(|x| *x == w))
            .map(|i| i as i32)
            .unwrap_or(-1);
        let len = leaves.len() as i32;
        let next_idx = (current_idx + dir).rem_euclid(len) as usize;
        let target = leaves[next_idx];
        if Some(target) == current {
            return false;
        }
        self.set_focus(target)
    }

    // ── Capture ──────────────────────────────────────────────────

    pub fn capture(&self) -> Option<HitTarget> {
        self.capture
    }

    /// Window id when a left-button drag is currently captured to that window
    /// (i.e. a content drag is in progress on it). `None` for scrollbar /
    /// overlay-chrome drags and when no drag is active.
    pub fn drag_capture_window(&self) -> Option<WinId> {
        match self.capture {
            Some(HitTarget::Window(w)) => Some(w),
            _ => None,
        }
    }

    fn set_capture(&mut self, target: HitTarget) {
        if !matches!(target, HitTarget::Scrollbar { .. }) {
            self.scrollbar_drag = None;
        }
        if !matches!(target, HitTarget::Window(_)) {
            self.drag_autoscroll = None;
            self.drag_autoscroll_since = None;
        }
        self.capture = Some(target);
    }

    fn clear_capture(&mut self) {
        self.capture = None;
        self.chrome_drag = None;
        self.split_interaction.cancel();
        self.drag_autoscroll_since = None;
        self.drag_autoscroll = None;
        self.scrollbar_drag = None;
    }

    /// Cancel any in-flight pointer gesture across the UI. This does not move
    /// persistent window cursors; it only drops transient capture/drag state.
    pub fn cancel_pointer_interaction(&mut self) {
        self.clear_capture();
        for win in self.wins.values_mut() {
            win.clear_mouse_state();
        }
    }

    /// Finish pointer state before keyboard routing. A pending caret click is
    /// committed to `cpos`; all transient mouse capture/drag state is cleared.
    pub fn finish_pointer_interaction_for_keyboard(&mut self) {
        let captured = self.drag_capture_window();
        if let Some(win_id) = captured {
            if let Some(buf_id) = self.wins.get(&win_id).map(|win| win.buf) {
                if let (Some(win), Some(buf)) = (self.wins.get_mut(&win_id), self.bufs.get(&buf_id))
                {
                    win.commit_pending_caret_click(buf);
                }
            }
        }
        self.clear_capture();
        for (win_id, win) in self.wins.iter_mut() {
            if Some(*win_id) != captured {
                win.clear_mouse_state();
            }
        }
    }

    pub fn cursor_shape(&self) -> CursorShape {
        self.cursor_shape
    }

    pub fn set_cursor_shape(&mut self, shape: CursorShape) {
        self.cursor_shape = shape;
    }

    /// Sleep duration until the next edge-drag autoscroll tick, ramping from
    /// `AUTOSCROLL_START_MS` down to `AUTOSCROLL_MIN_MS` over
    /// `AUTOSCROLL_RAMP_DIVISOR_MS` of edge dwell. `None` when the drag pointer
    /// is not parked at a viewport edge (host should fall back to its normal frame
    /// interval). Does not mutate; safe to call while computing the next
    /// `tokio::select` sleep.
    pub fn drag_autoscroll_interval(&self) -> Option<std::time::Duration> {
        self.edge_drag_delta().is_some().then(|| {
            let held = self
                .drag_autoscroll_since
                .map(|t| t.elapsed().as_millis() as u64)
                .unwrap_or(0);
            let ms = AUTOSCROLL_START_MS
                .saturating_sub(held / AUTOSCROLL_RAMP_DIVISOR_MS)
                .max(AUTOSCROLL_MIN_MS);
            std::time::Duration::from_millis(ms)
        })
    }

    /// Current edge-drag autoscroll owner and row delta, if an in-flight drag
    /// is requesting a tick. Hosts can inspect this before calling
    /// [`Self::tick_drag_autoscroll`] to preserve semantic scroll intent.
    pub fn drag_autoscroll_delta(&self) -> Option<(WinId, isize)> {
        self.edge_drag_delta()
    }

    /// Begin an edge-drag autoscroll tick without moving any window rows.
    /// Hosts with document-owned viewport projection can use this to keep the
    /// autoscroll ramp timing while applying the movement semantically.
    pub fn begin_drag_autoscroll_tick(&mut self) -> Option<(WinId, isize)> {
        let Some(delta) = self.edge_drag_delta() else {
            self.drag_autoscroll_since = None;
            return None;
        };
        self.drag_autoscroll_since
            .get_or_insert_with(std::time::Instant::now);
        Some(delta)
    }

    /// Per-frame autoscroll step: when a left-button drag's pointer is parked
    /// at the top or bottom of the captured window's viewport, pan one row in
    /// that direction and move the selection endpoint to the new leading edge.
    /// The pointer edge intent, not the current endpoint projection, is the
    /// trigger so sparse row-document materialization cannot stall the gesture.
    /// Returns `true` if anything panned.
    pub fn tick_drag_autoscroll(&mut self) -> bool {
        let Some((win_id, delta)) = self.begin_drag_autoscroll_tick() else {
            return false;
        };
        let win = self.wins.get(&win_id).expect("edge_drag_delta validated");
        let viewport_h = win.viewport.expect("edge_drag_delta validated").rect.height;
        let buf_id = win.buf;
        let (win, buf) = self.win_and_buf_mut(win_id, buf_id);
        let w = win.expect("captured window");
        w.drag_autoscroll_step(buf.expect("captured buffer"), viewport_h, delta)
    }

    /// `(win, delta)` when an in-flight drag's stored pointer sits at the top
    /// or bottom of the captured window's viewport. Shared trigger check for
    /// [`drag_autoscroll_interval`] and [`tick_drag_autoscroll`].
    fn edge_drag_delta(&self) -> Option<(WinId, isize)> {
        let win_id = match self.capture? {
            HitTarget::Window(w) => w,
            _ => return None,
        };
        let drag = self.drag_autoscroll?;
        if drag.owner != win_id {
            return None;
        }
        let win = self.wins.get(&win_id)?;
        if win.viewport?.rect.height == 0 || !win.drag_active() {
            return None;
        }
        let edge = self
            .drag_autoscroll_edge_for(drag.owner, drag.row)
            .or(drag.edge)?;
        let delta = match edge {
            DragAutoscrollEdge::Top => -1,
            DragAutoscrollEdge::Bottom => 1,
        };
        Some((win_id, delta))
    }

    fn capture_target_alive(&self, target: HitTarget) -> bool {
        match target {
            HitTarget::Window(w) | HitTarget::Scrollbar { owner: w } => {
                self.splits().contains_leaf(w)
                    || self.overlay_for_leaf(w).is_some()
                    || self.decoration_for_leaf(w).is_some()
            }
            HitTarget::Paint(p) => {
                self.splits().contains_leaf(p)
                    || self.overlay_for_paint(p).is_some()
                    || self.decoration_for_paint(p).is_some()
            }
            HitTarget::Chrome { owner, .. } => match owner {
                overlay::ChromeOwner::Overlay(owner) => {
                    self.overlays.iter().any(|(id, _)| *id == owner)
                }
                overlay::ChromeOwner::Container(owner) => self.docked_surface(owner).is_some(),
                overlay::ChromeOwner::Split(id) => self.resolved_split(id).is_some(),
            },
        }
    }

    // ── Renderer ─────────────────────────────────────────────────

    pub fn render<W: std::io::Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        self.render_with_paints(w, |_, _, _| {})
    }

    /// Resolve overlay leaf geometry into `Window::viewport` before the next paint.
    ///
    /// Overlay layout is independent from the main split tree, so a newly opened
    /// overlay otherwise has no viewport until its first frame is painted. Priming
    /// here lets Lua resize callbacks and tail-follow calculations see the real
    /// overlay rect before anything is drawn.
    pub fn prime_overlay_viewports(&mut self) {
        for overlay in &self.resolve_scene(None).overlays {
            self.prepare_layout_windows(&overlay.layout, &mut |_, _| {});
        }
    }

    pub fn prime_decoration_viewports(&mut self) {
        for decoration in &self.resolve_scene(None).decorations {
            self.prepare_layout_windows(&decoration.layout, &mut |_, _| {});
        }
    }

    fn prepare_layout_windows<P>(
        &mut self,
        layout: &layout::ResolvedLayout,
        prepare: &mut P,
    ) -> Vec<PreparedWindowRequest>
    where
        P: FnMut(&mut Ui, MaterializeRequest),
    {
        layout
            .leaves()
            .filter_map(|(id, rect)| self.prepare_window_for_render(WinId(id.0), rect, prepare))
            .collect()
    }

    fn prepare_window_for_render<P>(
        &mut self,
        win_id: WinId,
        rect: Rect,
        prepare: &mut P,
    ) -> Option<PreparedWindowRequest>
    where
        P: FnMut(&mut Ui, MaterializeRequest),
    {
        let follow_tail = self.should_follow_tail(win_id);
        let win = self.wins.get_mut(&win_id)?;
        win.begin_viewport_prepare();
        let buf_id = win.buf;
        let document_handle = win.document_handle();
        let gutter_width = self
            .bufs
            .get(&buf_id)
            .map(|buf| win.gutter_width(buf))
            .unwrap_or(0)
            .min(rect.width);
        let content_width = win
            .config
            .gutters
            .content_width_with_gutter(rect.width, gutter_width);
        let request = MaterializeRequest {
            win: win_id,
            buf: buf_id,
            document_handle,
            rect,
            gutter_width,
            content_width,
            scroll_top: win.scroll_top(),
            follow_tail,
        };
        prepare(self, request);
        let theme = self.theme().clone();
        if let (Some(win), Some(buf)) = (self.wins.get_mut(&win_id), self.bufs.get_mut(&buf_id)) {
            win.prepare_row_source(buf, content_width, rect.height, follow_tail, &theme);
        }

        let prepared_width = content_width;
        let win = self.wins.get(&win_id)?;
        let buf_id = win.buf;
        let document_handle = win.document_handle();
        let gutter_width = self
            .bufs
            .get(&buf_id)
            .map(|buf| win.gutter_width(buf))
            .unwrap_or(0)
            .min(rect.width);
        let content_width = win
            .config
            .gutters
            .content_width_with_gutter(rect.width, gutter_width);
        // Materialized metadata can establish or change the gutter. Format native
        // rows at the resulting content width before exposing this viewport.
        if content_width != prepared_width {
            if let (Some(win), Some(buf)) = (self.wins.get_mut(&win_id), self.bufs.get_mut(&buf_id))
            {
                win.prepare_row_source(buf, content_width, rect.height, follow_tail, &theme);
            }
        }
        if let Some(buf) = self.bufs.get_mut(&buf_id) {
            buf.ensure_rendered_at(content_width);
        }
        if let (Some(buf), Some(win)) = (self.bufs.get(&buf_id), self.wins.get_mut(&win_id)) {
            win.ensure_layout(buf, content_width);
            win.refresh_document_view_position_from_buffer(buf);
        }
        let total_rows = match (self.bufs.get(&buf_id), self.wins.get(&win_id)) {
            (Some(buf), Some(win)) => win.scroll_row_total(buf),
            _ => 0,
        };
        if let Some(win) = self.wins.get_mut(&win_id) {
            let viewport_height = win
                .prepared_viewport_height()
                .unwrap_or(rect.height)
                .min(rect.height);
            let viewport_rect = Rect {
                height: viewport_height,
                ..rect
            };
            if win.pending_scroll_to_cursor && viewport_height > 0 {
                if let Some(buf) = self.bufs.get(&buf_id) {
                    win.keep_cursor_visible(buf, total_rows, viewport_height, content_width);
                }
                win.pending_scroll_to_cursor = false;
            }
            let scrollbar = if win.config.gutters.scrollbar && rect.width > 0 {
                let bar_col = rect.left + rect.width.saturating_sub(1);
                window::ScrollbarState::new(bar_col, total_rows, viewport_height)
            } else {
                None
            };
            win.viewport = Some(
                window::WindowViewport::new(
                    viewport_rect,
                    content_width,
                    total_rows,
                    win.scroll_top(),
                    scrollbar,
                )
                .with_gutter_width(gutter_width),
            );
        }
        let viewport_rect = self.wins.get(&win_id)?.viewport?.rect;
        Some(PreparedWindowRequest {
            win: win_id,
            buf: buf_id,
            document_handle,
            rect,
            viewport_rect,
            gutter_width,
            content_width,
        })
    }

    fn prepared_window_matches_split(
        &self,
        request: PreparedWindowRequest,
        win_id: WinId,
        rect: Rect,
    ) -> bool {
        let Some(win) = self.wins.get(&win_id) else {
            return false;
        };
        let gutter_width = self
            .bufs
            .get(&win.buf)
            .map(|buf| win.gutter_width(buf))
            .unwrap_or(0)
            .min(rect.width);
        let content_width = win
            .config
            .gutters
            .content_width_with_gutter(rect.width, gutter_width);
        request.win == win_id
            && request.buf == win.buf
            && request.document_handle == win.document_handle()
            && request.rect == rect
            && win
                .viewport
                .is_some_and(|viewport| viewport.rect == request.viewport_rect)
            && request.gutter_width == gutter_width
            && request.content_width == content_width
    }

    /// Prepare one split window without painting a frame. Hosts with committed
    /// view observers use this to materialize row-backed documents before
    /// notifying observers, then pass the returned request to
    /// `render_with_prepared_splits_and_paints` so the split is not prepared twice.
    pub fn prepare_split_window_with<P>(
        &mut self,
        win: WinId,
        mut prepare: P,
    ) -> Option<PreparedWindowRequest>
    where
        P: FnMut(&mut Ui, MaterializeRequest),
    {
        let rect = self.split_rect(win)?;
        let request = self.prepare_window_for_render(win, rect, &mut prepare)?;
        self.resolve_tail_scrolls();
        Some(request)
    }

    /// Render one frame, delegating non-Window `Paint(id)` leaves to `paint(id, slice, ctx)`.
    pub fn render_with_paints<W, F>(&mut self, w: &mut W, paint: F) -> std::io::Result<()>
    where
        W: std::io::Write,
        F: FnMut(PaintId, &mut GridSlice<'_>, &DrawContext),
    {
        self.render_with_paints_prepared(w, |_, _| {}, paint)
    }

    /// Render one frame with a host preparation hook for row-backed windows.
    ///
    /// `prepare` runs after split/overlay geometry is known and before the backing
    /// buffer is rendered/layouted for paint. Row-backed sources can materialize a
    /// bounded row slice into `request.buf`, apply row metadata to `request.win`,
    /// and then let the normal `Window::render` path handle painting.
    pub fn render_with_paints_prepared<W, P, F>(
        &mut self,
        w: &mut W,
        prepare: P,
        paint: F,
    ) -> std::io::Result<()>
    where
        W: std::io::Write,
        P: FnMut(&mut Ui, MaterializeRequest),
        F: FnMut(PaintId, &mut GridSlice<'_>, &DrawContext),
    {
        self.render_with_paints_prepared_and_after_layout(w, prepare, |_, _| {}, paint)
    }

    /// Render one frame with host preparation and an after-layout hook for windows.
    ///
    /// `prepare` runs before the backing buffer is rendered/layouted so row-backed
    /// sources can materialize content. `after_layout` runs after each prepared
    /// window has current buffer content, layout, and viewport metadata, but before paint.
    pub fn render_with_paints_prepared_and_after_layout<W, P, D, F>(
        &mut self,
        w: &mut W,
        prepare: P,
        after_layout: D,
        paint: F,
    ) -> std::io::Result<()>
    where
        W: std::io::Write,
        P: FnMut(&mut Ui, MaterializeRequest),
        D: FnMut(&mut Ui, PreparedWindowRequest),
        F: FnMut(PaintId, &mut GridSlice<'_>, &DrawContext),
    {
        self.render_with_prepared_splits_and_paints(
            w,
            std::iter::empty(),
            prepare,
            after_layout,
            paint,
        )
    }

    /// Paint and flush a frame while reusing split windows prepared by an earlier host phase.
    pub fn render_with_prepared_splits_and_paints<W, I, P, D, F>(
        &mut self,
        w: &mut W,
        prepared_splits: I,
        prepare: P,
        after_layout: D,
        paint: F,
    ) -> std::io::Result<()>
    where
        W: std::io::Write,
        I: IntoIterator<Item = PreparedWindowRequest>,
        P: FnMut(&mut Ui, MaterializeRequest),
        D: FnMut(&mut Ui, PreparedWindowRequest),
        F: FnMut(PaintId, &mut GridSlice<'_>, &DrawContext),
    {
        let frame = self.paint_frame_with_prepared_splits_and_paints(
            prepared_splits,
            prepare,
            after_layout,
            paint,
        );
        self.flush_prepared_frame(w, frame)
    }

    /// Paint a frame into an owned grid while reusing previously prepared splits.
    ///
    /// Reused requests are still passed to `after_layout`; overlays, decorations,
    /// and all remaining splits are prepared from their current state. The owned
    /// grid can be amended after this method releases its UI borrow, then passed to
    /// [`Self::flush_prepared_frame`].
    pub fn paint_frame_with_prepared_splits_and_paints<I, P, D, F>(
        &mut self,
        prepared_splits: I,
        prepare: P,
        after_layout: D,
        mut paint: F,
    ) -> Grid
    where
        I: IntoIterator<Item = PreparedWindowRequest>,
        P: FnMut(&mut Ui, MaterializeRequest),
        D: FnMut(&mut Ui, PreparedWindowRequest),
        F: FnMut(PaintId, &mut GridSlice<'_>, &DrawContext),
    {
        let prepared =
            self.prepare_frame_with_prepared_splits(prepared_splits, prepare, after_layout);
        self.paint_prepared_frame_with_paints(prepared, |_, id, slice, context| {
            paint(id, slice, context);
        })
    }

    /// Materialize windows and resolve one immutable frame plan.
    pub fn prepare_frame_with_prepared_splits<I, P, D>(
        &mut self,
        prepared_splits: I,
        mut prepare: P,
        mut after_layout: D,
    ) -> PreparedFrame
    where
        I: IntoIterator<Item = PreparedWindowRequest>,
        P: FnMut(&mut Ui, MaterializeRequest),
        D: FnMut(&mut Ui, PreparedWindowRequest),
    {
        let mut prepared_splits: HashMap<WinId, PreparedWindowRequest> = prepared_splits
            .into_iter()
            .map(|request| (request.win, request))
            .collect();
        let scene = self.resolve_scene(None);
        for window in self.wins.values().filter(|win| win.row_source().is_some()) {
            if !scene
                .layouts()
                .any(|layout| layout.leaf_rect(window.id().into()).is_some())
            {
                window.suspend_row_source();
            }
        }
        let mut prepared_windows = Vec::new();
        for layout in scene.overlays.iter().map(|overlay| &overlay.layout).chain(
            scene
                .decorations
                .iter()
                .map(|decoration| &decoration.layout),
        ) {
            prepared_windows.extend(self.prepare_layout_windows(layout, &mut prepare));
        }
        for (paint_id, rect) in scene.root.leaves() {
            let win_id = WinId(paint_id.0);
            let prepared = prepared_splits
                .remove(&win_id)
                .filter(|request| self.prepared_window_matches_split(*request, win_id, rect));
            if let Some(request) =
                prepared.or_else(|| self.prepare_window_for_render(win_id, rect, &mut prepare))
            {
                prepared_windows.push(request);
            }
        }
        self.resolve_tail_scrolls();
        for request in prepared_windows {
            after_layout(self, request);
        }
        let containers: HashMap<_, _> = scene.root.containers().collect();
        for (id, surface) in &mut self.docked_surfaces {
            surface.resolved_rect = containers.get(id).copied();
        }

        let term_size = self.surface.terminal_size();
        let theme = std::sync::Arc::clone(self.surface.theme());
        let active_resize = self.chrome_drag.and_then(|drag| match drag.action {
            overlay::ChromeAction::Resize(edges) => Some((drag.owner, resize_chrome_ctx(edges))),
            overlay::ChromeAction::None | overlay::ChromeAction::Move => None,
        });
        let mut builder = PreparedFrameBuilder {
            commands: Vec::new(),
            paint_leaves: Vec::new(),
            wins: &self.wins,
            bufs: &self.bufs,
            focus: self.focus,
            active_cursor: self.active_cursor_leaf(),
            active_split: self.split_interaction.active_id(),
            cursor_shape: self.cursor_shape,
            term_size,
            theme: &theme,
        };
        collect_root_frame_commands(&scene.root, &scene.decorations, active_resize, &mut builder);
        for overlay in &scene.overlays {
            builder
                .commands
                .push(PreparedFrameCommand::Clear(overlay.rect));
            let root_chrome = active_resize
                .and_then(|(owner, context)| {
                    (owner == overlay::ChromeOwner::Overlay(overlay.id)).then_some(context)
                })
                .unwrap_or_default();
            collect_layout_frame_commands(&overlay.layout, root_chrome, &mut builder);
        }
        PreparedFrame {
            commands: builder.commands,
            paint_leaves: builder.paint_leaves,
            theme,
        }
    }

    /// Paint a previously resolved frame without consulting layout again.
    pub fn paint_prepared_frame_with_paints<F>(
        &mut self,
        prepared: PreparedFrame,
        mut paint: F,
    ) -> Grid
    where
        F: FnMut(usize, PaintId, &mut GridSlice<'_>, &DrawContext),
    {
        let PreparedFrame {
            commands,
            paint_leaves,
            theme,
        } = prepared;
        let wins = &self.wins;
        let bufs = &self.bufs;
        let compositor_theme = std::sync::Arc::clone(&theme);
        self.surface
            .compositor_mut()
            .paint_frame(&compositor_theme, move |grid, _| {
                for command in commands {
                    match command {
                        PreparedFrameCommand::Clear(area) => grid.clear(area),
                        PreparedFrameCommand::Divider { divider, active } => {
                            let mut normal = theme.get("Comment");
                            let mut dragging = theme.get("SmeltResizeHandle");
                            normal.bg = None;
                            dragging.bg = None;
                            let styles = layout::DividerStyles {
                                normal,
                                active: dragging,
                            };
                            divider.paint(grid, divider.style(styles, active));
                        }
                        PreparedFrameCommand::Chrome {
                            area,
                            chrome,
                            context,
                        } => layout::paint_chrome_with(grid, area, &chrome, &theme, context),
                        PreparedFrameCommand::Window { win, rect, context } => {
                            let Some(window) = wins.get(&win) else {
                                continue;
                            };
                            let Some(buffer) = bufs.get(&window.buf) else {
                                continue;
                            };
                            window.render(buffer, &mut grid.slice_mut(rect), &context);
                        }
                        PreparedFrameCommand::Paint(slot) => {
                            let leaf = &paint_leaves[slot];
                            paint(slot, leaf.id, &mut grid.slice_mut(leaf.rect), &leaf.context);
                        }
                    }
                }
            })
    }

    /// Flush an owned frame returned by [`Self::paint_frame_with_prepared_splits_and_paints`].
    pub fn flush_prepared_frame<W: std::io::Write>(
        &mut self,
        w: &mut W,
        frame: Grid,
    ) -> std::io::Result<()> {
        self.surface.compositor_mut().flush_frame(w, frame)
    }

    /// Paint directly into the compositor's `Grid`, bypassing layout and overlay machinery.
    pub fn render_raw<W, F>(&mut self, w: &mut W, paint: F) -> std::io::Result<()>
    where
        W: std::io::Write,
        F: FnOnce(&mut Grid, &Theme),
    {
        self.surface.render_raw(w, paint)
    }

    /// Resolved screen rect for a `PaintId` leaf across splits, overlays, and decorations.
    pub fn paint_rect(&self, id: PaintId) -> Option<Rect> {
        let (term_w, term_h) = self.surface.terminal_size();
        let area = Rect::new(0, 0, term_w, term_h);
        let sizer = UiLeafSizer {
            wins: &self.wins,
            bufs: &self.bufs,
        };
        if let Some(rect) = layout::resolve_layout_with(self.splits(), area, &sizer).get(&id) {
            return Some(*rect);
        }
        for (_oid, ov_rect, ov) in self.resolve_overlays(None) {
            if let Some(rect) = layout::resolve_layout_with(&ov.layout, ov_rect, &sizer).get(&id) {
                return Some(*rect);
            }
        }
        for (_did, _owner, decoration_rect, decoration) in self.resolve_decorations() {
            if let Some(rect) =
                layout::resolve_layout_with(&decoration.layout, decoration_rect, &sizer).get(&id)
            {
                return Some(*rect);
            }
        }
        None
    }

    /// Render to a sink and return the resulting grid snapshot. Used by the storybook harness.
    pub fn snapshot(&mut self) -> SnapshotFrame {
        let mut sink = std::io::sink();
        self.render(&mut sink).expect("snapshot render to sink");
        self.flushed_snapshot()
    }

    /// Return the most recently flushed frame without composing another one.
    ///
    /// Callers that render through a custom paint pipeline must use this method so
    /// capturing the frame does not replace callback-painted cells with a plain UI render.
    pub fn flushed_snapshot(&self) -> SnapshotFrame {
        SnapshotFrame::from_grid(self.surface.compositor().previous())
    }

    pub fn theme(&self) -> &std::sync::Arc<Theme> {
        self.surface.theme()
    }

    pub fn theme_mut(&mut self) -> &mut Theme {
        self.surface.theme_mut()
    }

    /// Own the pointer-capture lifecycle for a virtualized scrollbar without
    /// mutating its committed window scroll. The host resolves the pointer to
    /// semantic content and applies the eventual exact viewport atomically.
    pub fn dispatch_virtual_scrollbar_event(
        &mut self,
        event: crossterm::event::MouseEvent,
    ) -> Status {
        use crossterm::event::{MouseButton, MouseEventKind};

        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let Some(HitTarget::Scrollbar { owner }) =
                    self.hit_test(event.row, event.column, None)
                else {
                    return Status::Ignored;
                };
                self.set_capture(HitTarget::Scrollbar { owner });
                self.start_scrollbar_drag(owner, event.row);
                Status::Consumed
            }
            MouseEventKind::Drag(MouseButton::Left)
                if matches!(self.capture, Some(HitTarget::Scrollbar { .. })) =>
            {
                Status::Consumed
            }
            MouseEventKind::Up(MouseButton::Left)
                if matches!(self.capture, Some(HitTarget::Scrollbar { .. })) =>
            {
                self.clear_capture();
                Status::Consumed
            }
            _ => Status::Ignored,
        }
    }

    /// Route a terminal event. Key: fires keymaps; bare Esc/Ctrl-C on a modal dismisses it.
    /// Resize: updates terminal size. Mouse: owns scrollbar drag and chrome drag; absorbs
    /// wheel on focused overlays. Clicks outside a modal pass through to the host so
    /// drag-select on background splits still works - the host is expected to skip
    /// `app_focus` promotion while a modal is active, leaving the modal focused.
    /// Returns `Ignored` for everything else so the host can continue routing.
    pub fn dispatch_event(&mut self, ev: Event, lua_invoke: &mut LuaInvoke) -> Status {
        use crossterm::event::{KeyEvent, MouseButton, MouseEventKind};
        match ev {
            Event::Key(KeyEvent {
                code, modifiers, ..
            }) => {
                if let Status::Consumed = self.dispatch_key(code, modifiers, lua_invoke) {
                    return Status::Consumed;
                }
                if let Status::Consumed = self.dispatch_key_fallback(code, modifiers, lua_invoke) {
                    return Status::Consumed;
                }
                self.try_dismiss_modal_for_chord(code, modifiers, lua_invoke)
            }
            Event::Resize(w, h) => {
                self.set_terminal_size(w, h);
                Status::Consumed
            }
            Event::Mouse(me) => {
                match me.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        let scene = self.resolve_scene(None);
                        let hit = self.hit_test_in(&scene, me.row, me.column);
                        if let Some(HitTarget::Scrollbar { owner }) = hit {
                            self.set_capture(HitTarget::Scrollbar { owner });
                            self.start_scrollbar_drag(owner, me.row);
                            self.apply_scrollbar_drag(owner, me.row);
                            return Status::Consumed;
                        }
                        let raise = match hit {
                            Some(HitTarget::Chrome {
                                owner: overlay::ChromeOwner::Overlay(owner),
                                ..
                            }) => Some(owner),
                            Some(HitTarget::Chrome {
                                owner: ChromeOwner::Split(_),
                                ..
                            }) => self
                                .overlay_hit_test_in(&scene, me.row, me.column)
                                .map(|(id, _)| id),
                            Some(HitTarget::Window(w)) => self.overlay_for_leaf(w),
                            Some(HitTarget::Paint(p)) => self.overlay_for_paint(p),
                            _ => None,
                        };
                        if let Some(owner) = raise {
                            self.raise_overlay_to_front(owner);
                        }
                        // Inert leaf bodies can opt into the same move action as chrome.
                        // Selectable leaves opt out: dragging inside them starts text selection.
                        let drag_target: Option<(overlay::ChromeOwner, overlay::ChromeAction)> =
                            match hit {
                                Some(HitTarget::Chrome { owner, action }) => Some((owner, action)),
                                Some(HitTarget::Window(w)) => {
                                    let leaf = self.wins.get(&w);
                                    let leaf_focusable =
                                        leaf.is_some_and(|win| win.accepts_focus());
                                    let leaf_selectable =
                                        leaf.is_some_and(|win| win.supports_text_selection());
                                    self.overlay_for_leaf(w).and_then(|owner| {
                                        let body_drag = self
                                            .overlay(owner)
                                            .map(|o| o.draggable.body)
                                            .unwrap_or(overlay::BodyDrag::Never);
                                        let can_drag = match body_drag {
                                            overlay::BodyDrag::Never => false,
                                            overlay::BodyDrag::Always => true,
                                            overlay::BodyDrag::Inert => {
                                                !leaf_focusable && !leaf_selectable
                                            }
                                        };
                                        can_drag.then_some((
                                            overlay::ChromeOwner::Overlay(owner),
                                            overlay::ChromeAction::Move,
                                        ))
                                    })
                                }
                                Some(HitTarget::Paint(p)) => {
                                    self.overlay_for_paint(p).and_then(|owner| {
                                        let body_drag = self
                                            .overlay(owner)
                                            .map(|o| o.draggable.body)
                                            .unwrap_or(overlay::BodyDrag::Never);
                                        let can_drag = match body_drag {
                                            overlay::BodyDrag::Never => false,
                                            overlay::BodyDrag::Always
                                            | overlay::BodyDrag::Inert => true,
                                        };
                                        can_drag.then_some((
                                            overlay::ChromeOwner::Overlay(owner),
                                            overlay::ChromeAction::Move,
                                        ))
                                    })
                                }
                                _ => None,
                            };
                        if let Some((owner, action)) = drag_target {
                            if let ChromeOwner::Split(id) = owner {
                                if let Some(resolved) = scene.split(id) {
                                    if self
                                        .split_interaction
                                        .begin(resolved, me.row, me.column)
                                        .is_some()
                                    {
                                        self.set_capture(HitTarget::Chrome { owner, action });
                                    }
                                }
                                return Status::Consumed;
                            }
                            if action != overlay::ChromeAction::None {
                                if let Some(rect) = self.chrome_owner_rect(owner) {
                                    self.set_capture(HitTarget::Chrome { owner, action });
                                    self.chrome_drag = Some(ChromeDrag {
                                        owner,
                                        action,
                                        start_rect: rect,
                                        origin_row: me.row,
                                        origin_col: me.column,
                                    });
                                    return Status::Consumed;
                                }
                            }
                            // Non-drag chrome click still consumes so it doesn't fall through.
                            if matches!(hit, Some(HitTarget::Chrome { .. })) {
                                return Status::Consumed;
                            }
                        }
                    }
                    MouseEventKind::Drag(MouseButton::Left) => {
                        if let Some(HitTarget::Scrollbar { owner }) = self.capture {
                            self.apply_scrollbar_drag(owner, me.row);
                            return Status::Consumed;
                        }
                        if let Some(id) = self.split_interaction.active_id() {
                            let resolved = self.resolved_split(id);
                            self.split_interaction
                                .update(resolved.as_ref(), me.row, me.column);
                            if self.split_interaction.active_id().is_none() {
                                self.clear_capture();
                            }
                            return Status::Consumed;
                        }
                        if let Some(drag) = self.chrome_drag {
                            self.apply_chrome_drag(drag, me.row, me.column);
                            return Status::Consumed;
                        }
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        if matches!(self.capture, Some(HitTarget::Scrollbar { .. })) {
                            self.clear_capture();
                            return Status::Consumed;
                        }
                        if let Some(id) = self.split_interaction.active_id() {
                            let resolved = self.resolved_split(id);
                            self.split_interaction
                                .release(resolved.as_ref(), me.row, me.column);
                            self.clear_capture();
                            return Status::Consumed;
                        }
                        if self.chrome_drag.is_some() {
                            self.clear_capture();
                            return Status::Consumed;
                        }
                    }
                    _ => {}
                }
                let is_scroll = matches!(
                    me.kind,
                    MouseEventKind::ScrollUp
                        | MouseEventKind::ScrollDown
                        | MouseEventKind::ScrollLeft
                        | MouseEventKind::ScrollRight
                );
                if is_scroll {
                    // Route the wheel direction to the appropriate axis on the
                    // leaf under the cursor (when it's wheel-scrollable). When
                    // an overlay is focused, claim the event regardless so it
                    // can't bleed through to splits underneath.
                    let (vdelta, hdelta) = match me.kind {
                        MouseEventKind::ScrollUp => (-3_isize, 0),
                        MouseEventKind::ScrollDown => (3, 0),
                        MouseEventKind::ScrollLeft => (0, -3),
                        MouseEventKind::ScrollRight => (0, 3),
                        _ => (0, 0),
                    };
                    let mut consumed = false;
                    if vdelta != 0 {
                        consumed |= self.scroll_at(me.row, me.column, vdelta);
                    }
                    if hdelta != 0 {
                        consumed |= self.scroll_at_horizontal(me.row, me.column, hdelta);
                    }
                    if consumed || self.focused_overlay().is_some() {
                        return Status::Consumed;
                    }
                }
                // A modal grabs keyboard focus and Esc/Ctrl-C, but mouse events
                // outside its rect still flow to the splits underneath so the
                // user can drag-select transcript content. The caller is
                // responsible for not promoting `app_focus` while a modal is
                // active - keeping the modal as the focused overlay produces
                // the natural snap-back-after-drag behavior.
                Status::Ignored
            }
            Event::FocusGained | Event::FocusLost | Event::Paste(_) => Status::Ignored,
        }
    }

    /// Raise `id` above all other overlays' `z`. Called on any Down inside an overlay.
    fn raise_overlay_to_front(&mut self, id: OverlayId) {
        let max_z = self.overlays.iter().map(|(_, o)| o.z).max().unwrap_or(0);
        if let Some((_, overlay)) = self.overlays.iter_mut().find(|(oid, _)| *oid == id) {
            overlay.z = max_z.saturating_add(1);
        }
        if self.modal_for_overlay(id).is_some() && self.focused_modal() != self.active_modal() {
            self.focus_active_modal();
        }
    }

    fn resolved_overlay_rect(&self, id: OverlayId) -> Option<Rect> {
        self.resolve_overlays(None)
            .into_iter()
            .find_map(|(oid, rect, _)| if oid == id { Some(rect) } else { None })
    }

    fn chrome_owner_rect(&self, owner: overlay::ChromeOwner) -> Option<Rect> {
        match owner {
            overlay::ChromeOwner::Overlay(id) => self.resolved_overlay_rect(id),
            overlay::ChromeOwner::Container(id) => self
                .docked_surface(id)
                .and_then(DockedSurface::resolved_rect),
            overlay::ChromeOwner::Split(id) => self
                .resolved_split(id)
                .map(|resolved| resolved.geometry.panes[0]),
        }
    }

    /// Apply a chrome-drag delta to its overlay or root container owner.
    fn apply_chrome_drag(&mut self, drag: ChromeDrag, row: u16, col: u16) {
        let dy = row as i32 - drag.origin_row as i32;
        let dx = col as i32 - drag.origin_col as i32;
        match drag.action {
            ChromeAction::Resize(edges) => {
                self.resize_chrome(drag.owner, drag.start_rect, edges, dx, dy)
            }
            ChromeAction::Move => {
                if let ChromeOwner::Overlay(id) = drag.owner {
                    if let Some((_, overlay)) = self.overlays.iter_mut().find(|(oid, _)| *oid == id)
                    {
                        overlay.anchor = layout::Anchor::ScreenAt {
                            row: i32::from(drag.start_rect.top) + dy,
                            col: i32::from(drag.start_rect.left) + dx,
                            corner: Corner::NW,
                        };
                    }
                }
            }
            ChromeAction::None => {}
        }
    }

    fn docked_surface_resize_bounds(&self, id: ContainerId) -> Option<(u16, u16, u16, u16)> {
        let surface = self.docked_surface(id)?;
        let term = self.surface.terminal_size();
        let sizer = UiLeafSizer {
            wins: &self.wins,
            bufs: &self.bufs,
        };
        let natural = surface.layout.natural_size_with(term, &sizer).1;
        let min_height = surface
            .min_height
            .map(|constraint| resolve_constraint(constraint, term.1, natural))
            .unwrap_or(1)
            .max(1)
            .min(term.1.max(1));
        let max_height = surface
            .max_height
            .map(|constraint| resolve_constraint(constraint, term.1, natural))
            .unwrap_or(term.1)
            .max(min_height)
            .min(term.1.max(1));
        Some((1, term.0.max(1), min_height, max_height))
    }

    fn start_scrollbar_drag(&mut self, owner: WinId, row: u16) {
        let Some(win) = self.wins.get(&owner) else {
            self.scrollbar_drag = None;
            return;
        };
        let Some(vp) = win.viewport else {
            self.scrollbar_drag = None;
            return;
        };
        let Some(bar) = vp.scrollbar else {
            self.scrollbar_drag = None;
            return;
        };
        let rel_row = row.saturating_sub(vp.rect.top);
        let metrics = bar.display_metrics(win.scroll_top());
        let thumb_grab_row = if metrics.is_thumb_at(rel_row) {
            rel_row.saturating_sub(metrics.thumb_top)
        } else {
            metrics.thumb_size / 2
        };
        self.scrollbar_drag = Some(ScrollbarDrag {
            owner,
            rect_top: vp.rect.top,
            bar,
            thumb_grab_row,
        });
    }

    /// Resolve a pointer row against the scrollbar geometry frozen at pointer-down.
    ///
    /// Returns the thumb-position numerator and denominator, total rows, and viewport
    /// rows. Without an active drag, the current window geometry is used.
    pub fn scrollbar_pointer_fraction(
        &self,
        owner: WinId,
        row: u16,
    ) -> Option<(u64, u64, RowIndex, u16)> {
        let (rect_top, bar, thumb_grab_row) =
            if let Some(drag) = self.scrollbar_drag.filter(|drag| drag.owner == owner) {
                (drag.rect_top, drag.bar, drag.thumb_grab_row)
            } else {
                let win = self.wins.get(&owner)?;
                let viewport = win.viewport?;
                let bar = viewport.scrollbar?;
                let rel_row = row.saturating_sub(viewport.rect.top);
                let metrics = bar.display_metrics(win.scroll_top());
                let thumb_grab_row = if metrics.is_thumb_at(rel_row) {
                    rel_row.saturating_sub(metrics.thumb_top)
                } else {
                    metrics.thumb_size / 2
                };
                (viewport.rect.top, bar, thumb_grab_row)
            };
        let rel_row = row.saturating_sub(rect_top);
        let metrics = bar.metrics(0);
        let thumb_top = rel_row
            .saturating_sub(thumb_grab_row)
            .min(metrics.max_thumb_top);
        Some((
            u64::from(thumb_top),
            u64::from(metrics.max_thumb_top.max(1)),
            bar.total_rows,
            bar.viewport_rows,
        ))
    }

    fn apply_scrollbar_drag(&mut self, owner: WinId, row: u16) {
        let drag = self.scrollbar_drag.filter(|drag| drag.owner == owner);
        let from_top = if let Some(drag) = drag {
            let rel_row = row.saturating_sub(drag.rect_top);
            let metrics = drag.bar.metrics(0);
            let thumb_top = rel_row
                .saturating_sub(drag.thumb_grab_row)
                .min(metrics.max_thumb_top);
            metrics.scroll_from_thumb_top(thumb_top)
        } else {
            let Some(win) = self.wins.get(&owner) else {
                return;
            };
            let Some(vp) = win.viewport else {
                return;
            };
            let Some(bar) = vp.scrollbar else {
                return;
            };
            let rel_row = row.saturating_sub(vp.rect.top);
            let metrics = bar.metrics(0);
            let thumb_top = metrics.thumb_top_for_click(rel_row);
            metrics.scroll_from_thumb_top(thumb_top)
        };
        let Some(win) = self.wins.get(&owner) else {
            return;
        };
        let buf_id = win.buf;
        let viewport_rows = self
            .scrollbar_drag
            .filter(|drag| drag.owner == owner)
            .map(|drag| drag.bar.viewport_rows)
            .or_else(|| win.viewport.map(|vp| vp.rect.height))
            .unwrap_or(0);
        let (win, buf) = self.win_and_buf_mut(owner, buf_id);
        if let (Some(win), Some(buf)) = (win, buf) {
            win.scroll_to_preserving_cursor_screen_row(from_top, buf, viewport_rows);
        }
    }

    pub fn dispatch_key(
        &mut self,
        code: crossterm::event::KeyCode,
        mods: crossterm::event::KeyModifiers,
        lua_invoke: &mut LuaInvoke,
    ) -> Status {
        // Tier 1 of the key cascade: per-window specific keymaps. Returns
        // `Status::Ignored` when no exact chord match exists so the caller
        // can run `dispatch_overlay_key` (tier 1b), then global Lua keymaps,
        // then `dispatch_key_fallback`, then `try_dismiss_modal_for_chord`
        // as the final resort.
        let key = KeyBind::new(code, mods);
        self.run_key_callback(
            code,
            mods,
            lua_invoke,
            |s, win| s.callbacks.take_keymap(win, key),
            |s, win, cb| s.callbacks.restore_keymap(win, key, cb),
        )
    }

    /// Dispatch a keymap shared by every leaf in the focused modal.
    pub fn dispatch_modal_key(
        &mut self,
        code: crossterm::event::KeyCode,
        mods: crossterm::event::KeyModifiers,
        lua_invoke: &mut LuaInvoke,
    ) -> Status {
        let Some(modal) = self.focused_modal() else {
            return Status::Ignored;
        };
        let key = KeyBind::new(code, mods);
        self.run_key_callback(
            code,
            mods,
            lua_invoke,
            |ui, _| {
                ui.callbacks
                    .take_scoped_keymap(KeymapScope::Modal(modal), key)
            },
            |ui, _, callback| {
                ui.callbacks
                    .restore_scoped_keymap(KeymapScope::Modal(modal), key, callback)
            },
        )
    }

    /// Overlay-scoped keymaps fire when the focused window belongs to an
    /// overlay and the overlay has a binding for the chord.
    pub fn dispatch_overlay_key(
        &mut self,
        code: crossterm::event::KeyCode,
        mods: crossterm::event::KeyModifiers,
        lua_invoke: &mut LuaInvoke,
    ) -> Status {
        let Some(win) = self.focus() else {
            return Status::Ignored;
        };
        let Some(overlay) = self.overlay_for_leaf(win) else {
            return Status::Ignored;
        };
        let key = KeyBind::new(code, mods);
        self.run_key_callback(
            code,
            mods,
            lua_invoke,
            |ui, _| {
                ui.callbacks
                    .take_scoped_keymap(KeymapScope::Overlay(overlay), key)
            },
            |ui, _, callback| {
                ui.callbacks
                    .restore_scoped_keymap(KeymapScope::Overlay(overlay), key, callback);
            },
        )
    }

    /// Tier 3 of the key cascade: per-window catch-all fallback (the "text
    /// input" tier - e.g. a dialog input that inserts any printable char).
    /// Runs only after `dispatch_event(Event::Key)` returned `Ignored` AND
    /// global Lua keymaps declined, so site-wide chords like `?` → /help take
    /// precedence over a leaf's blanket printable-char capture.
    pub fn dispatch_key_fallback(
        &mut self,
        code: crossterm::event::KeyCode,
        mods: crossterm::event::KeyModifiers,
        lua_invoke: &mut LuaInvoke,
    ) -> Status {
        self.run_key_callback(
            code,
            mods,
            lua_invoke,
            |s, win| s.callbacks.take_key_fallback(win),
            |s, win, cb| s.callbacks.restore_key_fallback(win, cb),
        )
    }

    pub fn dispatch_paste_fallback(
        &mut self,
        content: String,
        lua_invoke: &mut LuaInvoke,
    ) -> Status {
        self.run_focused_callback(
            Payload::Paste { content },
            lua_invoke,
            |s, win| s.callbacks.take_key_fallback(win),
            |s, win, cb| s.callbacks.restore_key_fallback(win, cb),
        )
    }

    /// Shared shell for focused callback dispatch: look up the callback on
    /// the focused window with `take`, invoke it (Rust or Lua), hand it back
    /// via `restore`, and fan out any `CallbackResult::Event` follow-up.
    fn run_focused_callback(
        &mut self,
        payload: Payload,
        lua_invoke: &mut LuaInvoke,
        take: impl FnOnce(&mut Self, WinId) -> Option<Callback>,
        restore: impl FnOnce(&mut Self, WinId, Callback),
    ) -> Status {
        let Some(win) = self.focus() else {
            return Status::Ignored;
        };
        let Some(mut cb) = take(self, win) else {
            return Status::Ignored;
        };

        let mut follow_up: Option<(WinEvent, Payload)> = None;
        let result = match &mut cb {
            Callback::Rust(inner) => {
                let mut ctx = CallbackCtx {
                    ui: self,
                    win,
                    payload,
                };
                match inner(&mut ctx) {
                    CallbackResult::Consumed => Status::Consumed,
                    CallbackResult::Pass => Status::Ignored,
                    CallbackResult::Event(ev, payload) => {
                        follow_up = Some((ev, payload));
                        Status::Consumed
                    }
                }
            }
            Callback::Lua(handle) => {
                lua_invoke(*handle, win, &payload);
                Status::Consumed
            }
        };
        restore(self, win, cb);

        if let Some((ev, payload)) = follow_up {
            if let Some(win) = self.focus() {
                self.fire_win_event(win, ev, payload, lua_invoke);
            }
        }

        result
    }

    /// Shared shell for the tier-1 and tier-3 key dispatchers: look up the
    /// callback on the focused window with `take`, invoke it (Rust or Lua),
    /// hand it back via `restore`, and fan out any `CallbackResult::Event`
    /// follow-up. Both tiers differ only in how they fetch the callback.
    fn run_key_callback(
        &mut self,
        code: crossterm::event::KeyCode,
        mods: crossterm::event::KeyModifiers,
        lua_invoke: &mut LuaInvoke,
        take: impl FnOnce(&mut Self, WinId) -> Option<Callback>,
        restore: impl FnOnce(&mut Self, WinId, Callback),
    ) -> Status {
        self.run_focused_callback(Payload::Key { code, mods }, lua_invoke, take, restore)
    }

    /// Final tier of the key cascade: dismiss the active modal on a bare
    /// `Esc` or `Ctrl-C`. Returns `Status::Consumed` only when a modal was
    /// actually dismissed. Caller runs this AFTER specific keymaps, global
    /// Lua keymaps, leaf fallback, and any overlay-viewer handler - the
    /// modal should close only when no one else claimed the chord.
    pub fn try_dismiss_modal_for_chord(
        &mut self,
        code: crossterm::event::KeyCode,
        mods: crossterm::event::KeyModifiers,
        lua_invoke: &mut LuaInvoke,
    ) -> Status {
        let is_dismiss_chord = matches!(code, crossterm::event::KeyCode::Esc)
            && mods == crossterm::event::KeyModifiers::NONE
            || matches!(code, crossterm::event::KeyCode::Char('c'))
                && mods == crossterm::event::KeyModifiers::CONTROL;
        if !is_dismiss_chord {
            return Status::Ignored;
        }
        let Some(modal_id) = self.active_modal() else {
            return Status::Ignored;
        };
        let Some(modal) = self.modal(modal_id).cloned() else {
            return Status::Ignored;
        };
        let Some(root) = modal.leaves.first().copied() else {
            return Status::Ignored;
        };
        self.fire_win_event(root, WinEvent::Dismiss, Payload::None, lua_invoke);
        // Floating overlays retain their historical close-on-unhandled-dismiss
        // behavior. Root dialogs close through their Lua lifecycle callback.
        if let ModalOwner::Overlay(overlay) = modal.owner {
            if self.modal_for_overlay(overlay).is_some() {
                let _ = self.overlay_close(overlay);
            }
        }
        Status::Consumed
    }

    /// Whether `win`'s tail-follow should fire this frame: tail mode is active,
    /// there is no active selection, and no pointer gesture owns the same window.
    pub fn should_follow_tail(&self, win: WinId) -> bool {
        let Some(w) = self.wins.get(&win) else {
            return false;
        };
        if !w.is_following_tail() || w.selection_active() {
            return false;
        }
        !matches!(
            self.capture,
            Some(HitTarget::Window(d) | HitTarget::Scrollbar { owner: d }) if d == win
        )
    }

    /// Resolve tail-follow windows against their current row count. This is a
    /// viewport operation only: it never rewrites cursor or selection endpoints.
    pub fn resolve_tail_scrolls(&mut self) {
        let ids: Vec<WinId> = self.wins.keys().copied().collect();
        for id in ids {
            if !self.should_follow_tail(id) {
                continue;
            }
            let (buf_id, viewport_rows) = {
                let win = self.wins.get(&id).expect("win exists");
                (win.buf, win.viewport.map(|v| v.rect.height).unwrap_or(0))
            };
            let total_rows = match (self.bufs.get(&buf_id), self.wins.get(&id)) {
                (Some(buf), Some(win)) => win.scroll_row_total(buf),
                _ => 0,
            };
            let max_scroll = total_rows.saturating_sub(viewport_rows as RowIndex);
            if let Some(w) = self.wins.get_mut(&id) {
                w.resolve_tail_scroll(max_scroll);
            }
        }
    }

    pub fn dispatch_tick(&mut self, lua_invoke: &mut LuaInvoke) {
        let wins: Vec<WinId> = self.callbacks.wins_with_event(WinEvent::Tick);
        for win in wins {
            self.fire_win_event(win, WinEvent::Tick, Payload::None, lua_invoke);
        }
    }

    /// Publish the committed focus transition before other viewport observers.
    /// Covers keyboard, mouse, overlay closure and reload without requiring
    /// each caller of `set_focus` to dispatch callbacks itself.
    pub fn dispatch_focus_events(&mut self, lua_invoke: &mut LuaInvoke) {
        let focus = self.focus();
        if self.last_emitted_focus == focus {
            return;
        }
        let previous = std::mem::replace(&mut self.last_emitted_focus, focus);
        if let Some(win) = previous {
            self.fire_win_event(win, WinEvent::FocusLost, Payload::None, lua_invoke);
        }
        if let Some(win) = focus {
            self.fire_win_event(win, WinEvent::FocusGained, Payload::None, lua_invoke);
        }
    }

    /// Fire `WinEvent::Scrolled` on every subscribed window whose
    /// `(scroll_top, tail-follow)` changed since the last emission.
    pub fn dispatch_scroll_events(&mut self, lua_invoke: &mut LuaInvoke) {
        let wins: Vec<WinId> = self.callbacks.wins_with_event(WinEvent::Scrolled);
        for win in wins {
            let Some(w) = self.wins.get_mut(&win) else {
                continue;
            };
            let cur = (w.scroll_top(), w.is_following_tail());
            if w.last_emitted_scroll == Some(cur) {
                continue;
            }
            w.last_emitted_scroll = Some(cur);
            let payload = Payload::Scroll {
                top: cur.0,
                follow: cur.1,
            };
            self.fire_win_event(win, WinEvent::Scrolled, payload, lua_invoke);
        }
    }

    /// Fire `WinEvent::Resized` on every subscribed window whose viewport
    /// `(rect, content_width)` changed since the last emission. Fires the
    /// first time the leaf gets a viewport, then again on terminal resize
    /// or layout reflow.
    pub fn dispatch_resize_events(&mut self, lua_invoke: &mut LuaInvoke) {
        let wins: Vec<WinId> = self.callbacks.wins_with_event(WinEvent::Resized);
        for win in wins {
            let Some(w) = self.wins.get_mut(&win) else {
                continue;
            };
            let Some(vp) = w.viewport else {
                continue;
            };
            let cur = (vp.rect, vp.content_width);
            if w.last_emitted_resize == Some(cur) {
                continue;
            }
            w.last_emitted_resize = Some(cur);
            let payload = Payload::Rect {
                row: vp.rect.top,
                col: vp.rect.left,
                width: vp.rect.width,
                height: vp.rect.height,
                content_width: vp.content_width,
            };
            self.fire_win_event(win, WinEvent::Resized, payload, lua_invoke);
        }
    }

    pub fn force_redraw(&mut self) {
        self.surface.force_redraw();
    }
}

impl Default for Ui {
    fn default() -> Self {
        Self::new()
    }
}

/// Compositor-bearing surface exposed by frontends. `TuiApp` implements this; `HeadlessApp`
/// does not - UiHost-only Lua bindings raise a runtime error in headless context.
/// `Ui` also impls it so tests can exercise compositor code without a full frontend.
pub trait UiHost {
    fn ui(&mut self) -> &mut Ui;
    fn set_focus(&mut self, win: WinId) -> bool;
    fn buf_create(&mut self, opts: BufCreateOpts) -> BufId;
    fn buf_mut(&mut self, id: BufId) -> Option<&mut Buffer>;
    fn win_open_split(&mut self, buf: BufId, config: SplitConfig) -> Option<WinId>;
    #[must_use]
    fn win_close(&mut self, id: WinId) -> Vec<u64>;
    fn win_mut(&mut self, id: WinId) -> Option<&mut Window>;
    fn overlay_open(&mut self, overlay: Overlay) -> OverlayId;

    /// Last-painted viewport for `win`. Hosts override when they project geometry differently.
    fn viewport_for(&self, win: WinId) -> Option<WindowViewport>;

    /// Last-painted visible row range for `win`.
    fn visible_range(&self, win: WinId) -> Option<std::ops::Range<RowIndex>> {
        let viewport = self.viewport_for(win)?;
        Some(
            viewport.scroll_top
                ..viewport
                    .scroll_top
                    .saturating_add(viewport.rect.height as RowIndex),
        )
    }
}

fn display_rows_for_ui_range(
    ui: &Ui,
    win: WinId,
    start: RowIndex,
    count: RowIndex,
) -> Option<DisplayRows> {
    let (buf_id, materialized) = {
        let win = ui.win(win)?;
        (win.buf, win.materialized_rows())
    };
    let buf = ui.buf(buf_id)?;
    let rows = buf.lines();
    let (start_idx, end) = if let Some(materialized) = materialized {
        let requested = start..start.saturating_add(count);
        let available = materialized.materialized_range();
        let clipped_start = requested.start.max(available.start);
        let clipped_end = requested.end.min(available.end);
        if clipped_start >= clipped_end {
            return Some(DisplayRows::empty());
        }
        (
            row_to_usize(materialized.local_row(clipped_start)).min(rows.len()),
            row_to_usize(materialized.local_row(clipped_end)).min(rows.len()),
        )
    } else {
        (
            row_to_usize(start).min(rows.len()),
            row_to_usize(start.saturating_add(count)).min(rows.len()),
        )
    };
    let text_rows = rows[start_idx..end].to_vec();
    let display_rows: Vec<DisplayRow> = text_rows
        .iter()
        .enumerate()
        .map(|(offset, row)| {
            let spans = buf.highlights_at(start_idx + offset);
            let display_row =
                DisplayRow::new(row.clone(), selectable_byte_ranges_for_line(row, &spans))
                    .with_actions(display_actions_for_spans(&spans));
            if offset == 0 {
                display_row
            } else {
                display_row.with_break_before(RowBreak::Hard)
            }
        })
        .collect();
    Some(DisplayRows { rows: display_rows })
}

pub struct BufferDisplayDocument<'a> {
    ui: &'a mut Ui,
    win: WinId,
}

impl<'a> BufferDisplayDocument<'a> {
    pub fn new(ui: &'a mut Ui, win: WinId) -> Self {
        Self { ui, win }
    }

    fn native(&self) -> Option<crate::row::RowSourceDocument<'_>> {
        let window = self.ui.win(self.win)?;
        Some(crate::row::RowSourceDocument {
            source: window.row_source()?.as_ref(),
            theme: self.ui.theme(),
            width: window
                .viewport
                .map(|viewport| viewport.content_width)
                .or_else(|| self.ui.win_content_width(self.win))
                .unwrap_or(0),
        })
    }
}

impl DisplayDocument for BufferDisplayDocument<'_> {
    fn snapshot(&mut self) -> DisplaySnapshot {
        if let Some(mut document) = self.native() {
            return document.snapshot();
        }
        let total_rows = self
            .ui
            .win(self.win)
            .and_then(|win| {
                win.materialized_rows()
                    .map(|rows| rows.total_rows)
                    .or_else(|| self.ui.buf(win.buf).map(|buf| buf.line_count() as RowIndex))
            })
            .unwrap_or(0);
        DisplaySnapshot {
            generation: 0,
            total_rows,
        }
    }

    fn materialize(&mut self, range: std::ops::Range<RowIndex>) -> DisplayRows {
        if let Some(mut document) = self.native() {
            return document.materialize(range);
        }
        let count = range.end.saturating_sub(range.start);
        display_rows_for_ui_range(self.ui, self.win, range.start, count)
            .unwrap_or_else(DisplayRows::empty)
    }

    fn copy_range(&mut self, range: TextRange) -> Option<CopyOutput> {
        if let Some(mut document) = self.native() {
            return document.copy_range(range);
        }
        let range = match range {
            TextRange::Rows(range) => range,
            TextRange::Bytes(_) => return None,
        };
        let win = self.ui.win(self.win)?;
        let buf = self.ui.buf(win.buf)?;
        let range = if let Some(materialized) = win.materialized_rows() {
            DocRange {
                start: DocPosition {
                    row: materialized.local_row(range.start.row),
                    byte_col: range.start.byte_col,
                },
                end: DocPosition {
                    row: materialized.local_row(range.end.row),
                    byte_col: range.end.byte_col,
                },
            }
        } else {
            range
        };
        crate::row::copy_buffer_doc_range(buf, range)
    }

    fn search_matches(
        &mut self,
        query: &str,
        origin: DocPosition,
        _forward: bool,
        chunk_rows: RowIndex,
    ) -> Vec<DocRange> {
        let Some(win) = self.ui.win(self.win) else {
            return Vec::new();
        };
        let row_document = win.row_cursor().is_some() || win.materialized_rows().is_some();
        let total_rows = self.snapshot().total_rows;
        if row_document {
            crate::row::scan_document_row_window(self, query, origin, total_rows, chunk_rows)
        } else {
            crate::row::scan_document_rows(self, query, 0, total_rows, chunk_rows)
        }
    }
}

impl UiHost for Ui {
    fn ui(&mut self) -> &mut Ui {
        self
    }
    fn set_focus(&mut self, win: WinId) -> bool {
        Ui::set_focus(self, win)
    }
    fn buf_create(&mut self, opts: BufCreateOpts) -> BufId {
        Ui::buf_create(self, opts)
    }
    fn buf_mut(&mut self, id: BufId) -> Option<&mut Buffer> {
        Ui::buf_mut(self, id)
    }
    fn win_open_split(&mut self, buf: BufId, config: SplitConfig) -> Option<WinId> {
        Ui::win_open_split(self, buf, config)
    }
    fn win_close(&mut self, id: WinId) -> Vec<u64> {
        Ui::win_close(self, id)
    }
    fn win_mut(&mut self, id: WinId) -> Option<&mut Window> {
        Ui::win_mut(self, id)
    }
    fn overlay_open(&mut self, overlay: Overlay) -> OverlayId {
        Ui::overlay_open(self, overlay)
    }
    fn viewport_for(&self, win: WinId) -> Option<WindowViewport> {
        Ui::win(self, win).and_then(|w| w.viewport)
    }
}

/// Minimum `(width, height)` a resize gesture can shrink an overlay to.
const MIN_OVERLAY_SIZE: (u16, u16) = (8, 3);

/// Drag-autoscroll ramp: first tick after the endpoint parks at the edge fires
/// after `AUTOSCROLL_START_MS`; the interval shortens by 1 ms per
/// `AUTOSCROLL_RAMP_DIVISOR_MS` of dwell, floored at `AUTOSCROLL_MIN_MS`
/// (≈30 → 200 lines/sec over ~3 s).
const AUTOSCROLL_START_MS: u64 = 30;
const AUTOSCROLL_MIN_MS: u64 = 5;
const AUTOSCROLL_RAMP_DIVISOR_MS: u64 = 120;

fn resize_chrome_ctx(edges: overlay::ResizeEdges) -> layout::ChromePaintCtx {
    let hl = smelt_buffer::theme::intern("SmeltResizeHandle");
    layout::ChromePaintCtx {
        top: edges.north.then_some(hl),
        right: edges.east.then_some(hl),
        bottom: edges.south.then_some(hl),
        left: edges.west.then_some(hl),
    }
}

/// Resolve the concrete action for a chrome cell. Resize handles win over drag
/// handles, but top-row drag remains intact unless top resizing is explicitly enabled.
fn chrome_action(rect: Rect, ov: &Overlay, row: u16, col: u16) -> overlay::ChromeAction {
    let action = resize_chrome_action(rect, ov.resizable, row, col);
    if action != overlay::ChromeAction::None {
        return action;
    }
    if ov.draggable.title {
        overlay::ChromeAction::Move
    } else {
        overlay::ChromeAction::None
    }
}

fn resize_chrome_action(
    rect: Rect,
    resize: overlay::ResizeConfig,
    row: u16,
    col: u16,
) -> overlay::ChromeAction {
    let top = row == rect.top;
    let bottom = row + 1 == rect.bottom();
    let left = col == rect.left;
    let right = col + 1 == rect.right();

    if resize.corners {
        if top && left && resize.top && resize.left {
            return overlay::ChromeAction::Resize(overlay::ResizeEdges::corner(
                true, false, false, true,
            ));
        }
        if top && right && resize.top && resize.right {
            return overlay::ChromeAction::Resize(overlay::ResizeEdges::corner(
                true, true, false, false,
            ));
        }
        if bottom && left && resize.bottom && resize.left {
            return overlay::ChromeAction::Resize(overlay::ResizeEdges::corner(
                false, false, true, true,
            ));
        }
        if bottom && right && resize.bottom && resize.right {
            return overlay::ChromeAction::Resize(overlay::ResizeEdges::corner(
                false, true, true, false,
            ));
        }
    }
    if top && resize.top {
        return overlay::ChromeAction::Resize(overlay::ResizeEdges::north());
    }
    if bottom && resize.bottom {
        return overlay::ChromeAction::Resize(overlay::ResizeEdges::south());
    }
    if left && resize.left && !top {
        return overlay::ChromeAction::Resize(overlay::ResizeEdges::west());
    }
    if right && resize.right && !top {
        return overlay::ChromeAction::Resize(overlay::ResizeEdges::east());
    }
    overlay::ChromeAction::None
}

fn resize_chrome_geometry(
    start: Rect,
    edges: overlay::ResizeEdges,
    dx: i32,
    dy: i32,
    bounds: (u16, u16, u16, u16),
) -> (i32, i32, u16, u16) {
    let (min_w, max_w, min_h, max_h) = bounds;
    let mut top = start.top as i32;
    let mut left = start.left as i32;
    let mut width = start.width as i32;
    let mut height = start.height as i32;

    if edges.east {
        width += dx;
    }
    if edges.south {
        height += dy;
    }
    if edges.west {
        width -= dx;
    }
    if edges.north {
        height -= dy;
    }

    let new_w = width.clamp(min_w as i32, max_w as i32) as u16;
    let new_h = height.clamp(min_h as i32, max_h as i32) as u16;
    if edges.west {
        left = start.left as i32 + (start.width as i32 - new_w as i32);
    }
    if edges.north {
        top = start.top as i32 + (start.height as i32 - new_h as i32);
    }
    (top, left, new_w, new_h)
}

fn anchor_after_resize(
    anchor: layout::Anchor,
    edges: overlay::ResizeEdges,
    top: i32,
    left: i32,
) -> layout::Anchor {
    match anchor {
        layout::Anchor::ScreenBottom { .. }
            if edges.north && !edges.east && !edges.south && !edges.west =>
        {
            anchor
        }
        _ => layout::Anchor::ScreenAt {
            row: top,
            col: left,
            corner: Corner::NW,
        },
    }
}

fn overlay_resize_bounds(
    overlay: &Overlay,
    term: (u16, u16),
    sizer: &dyn layout::LeafSizer,
) -> (u16, u16, u16, u16) {
    let natural = overlay.layout.natural_size_with(term, sizer);
    let min_w = overlay
        .min_width
        .map(|c| resolve_constraint(c, term.0, natural.0))
        .unwrap_or(MIN_OVERLAY_SIZE.0)
        .max(MIN_OVERLAY_SIZE.0)
        .min(term.0);
    let min_h = overlay
        .min_height
        .map(|c| resolve_constraint(c, term.1, natural.1))
        .unwrap_or(MIN_OVERLAY_SIZE.1)
        .max(MIN_OVERLAY_SIZE.1)
        .min(term.1);
    let max_w = overlay
        .max_width
        .map(|c| resolve_constraint(c, term.0, natural.0))
        .unwrap_or(term.0)
        .max(min_w)
        .min(term.0);
    let max_h = overlay
        .max_height
        .map(|c| resolve_constraint(c, term.1, natural.1))
        .unwrap_or(term.1)
        .max(min_h)
        .min(term.1);
    (min_w, max_w, min_h, max_h)
}

pub fn resolve_constraint(
    constraint: layout::Constraint,
    term_axis: u16,
    natural_axis: u16,
) -> u16 {
    use layout::Constraint::*;
    match constraint {
        Length(n) => n.min(term_axis),
        Percentage(p) => ((term_axis as u32 * p as u32) / 100) as u16,
        Ratio(num, denom) if denom != 0 => {
            ((term_axis as u32 * num as u32) / denom as u32).min(term_axis as u32) as u16
        }
        Max(n) => natural_axis.min(n).min(term_axis),
        Min(n) => natural_axis.max(n).min(term_axis),
        Fill => term_axis,
        Fit | Ratio(_, _) => natural_axis.min(term_axis),
    }
}

/// Resolve an overlay's rect size from its `width`/`height` `Constraint`
/// against the terminal extent. `Fit` reads the layout's natural size on
/// that axis; explicit sizing modes evaluate independently per axis.
/// Then [`Overlay::max_width`] / [`Overlay::max_height`] cap the result if
/// set - they're resolved with the same rules and act as upper bounds.
fn resolve_overlay_size(
    overlay: &Overlay,
    term: (u16, u16),
    sizer: &dyn layout::LeafSizer,
) -> (u16, u16) {
    use layout::Constraint::*;
    let needs_natural = matches!(
        (overlay.width, overlay.height),
        (Fit, _) | (_, Fit) | (Max(_), _) | (_, Max(_)) | (Min(_), _) | (_, Min(_)),
    );
    let natural = if needs_natural {
        overlay.layout.natural_size_with(term, sizer)
    } else {
        (0, 0)
    };
    let mut w = resolve_constraint(overlay.width, term.0, natural.0);
    let mut h = resolve_constraint(overlay.height, term.1, natural.1);
    if let Some(cap) = overlay.max_width {
        w = w.min(resolve_constraint(cap, term.0, natural.0));
    }
    if let Some(cap) = overlay.max_height {
        h = h.min(resolve_constraint(cap, term.1, natural.1));
    }
    if let Some(floor) = overlay.min_width {
        w = w.max(resolve_constraint(floor, term.0, natural.0));
    }
    if let Some(floor) = overlay.min_height {
        h = h.max(resolve_constraint(floor, term.1, natural.1));
    }
    // Floors can't push past the terminal extent.
    w = w.min(term.0);
    h = h.min(term.1);
    (w, h)
}

fn resolve_decoration_size(
    decoration: &Decoration,
    owner: Rect,
    sizer: &dyn layout::LeafSizer,
) -> (u16, u16) {
    use layout::Constraint::*;
    let cap = (owner.width, owner.height);
    let needs_natural = matches!(
        (decoration.width, decoration.height),
        (Fit, _) | (_, Fit) | (Max(_), _) | (_, Max(_)) | (Min(_), _) | (_, Min(_)),
    );
    let natural = if needs_natural {
        decoration.layout.natural_size_with(cap, sizer)
    } else {
        (0, 0)
    };
    let mut w = resolve_constraint(decoration.width, cap.0, natural.0);
    let mut h = resolve_constraint(decoration.height, cap.1, natural.1);
    if let Some(cap_w) = decoration.max_width {
        w = w.min(resolve_constraint(cap_w, cap.0, natural.0));
    }
    if let Some(cap_h) = decoration.max_height {
        h = h.min(resolve_constraint(cap_h, cap.1, natural.1));
    }
    if let Some(floor_w) = decoration.min_width {
        w = w.max(resolve_constraint(floor_w, cap.0, natural.0));
    }
    if let Some(floor_h) = decoration.min_height {
        h = h.max(resolve_constraint(floor_h, cap.1, natural.1));
    }
    (w.min(cap.0), h.min(cap.1))
}

struct PreparedFrameBuilder<'a> {
    commands: Vec<PreparedFrameCommand>,
    paint_leaves: Vec<PreparedPaintLeaf>,
    wins: &'a HashMap<WinId, Window>,
    bufs: &'a HashMap<BufId, Buffer>,
    focus: Option<WinId>,
    active_cursor: Option<WinId>,
    active_split: Option<layout::SplitId>,
    cursor_shape: CursorShape,
    term_size: (u16, u16),
    theme: &'a std::sync::Arc<Theme>,
}

impl PreparedFrameBuilder<'_> {
    fn push_divider(&mut self, divider: layout::SplitDivider) {
        self.commands.push(PreparedFrameCommand::Divider {
            active: self.active_split == Some(divider.id),
            divider,
        });
    }

    fn push_leaf(&mut self, id: PaintId, rect: Rect) {
        let win_id = WinId(id.0);
        if let Some(win) = self
            .wins
            .get(&win_id)
            .filter(|win| self.bufs.contains_key(&win.buf))
        {
            let owns_cursor = self.active_cursor == Some(win_id);
            self.commands.push(PreparedFrameCommand::Window {
                win: win_id,
                rect,
                context: DrawContext {
                    terminal_width: self.term_size.0,
                    terminal_height: self.term_size.1,
                    focused: self.focus == Some(win_id),
                    cursor_shape: if owns_cursor && !win.hide_cursor {
                        self.cursor_shape
                    } else {
                        CursorShape::Hidden
                    },
                    theme: std::sync::Arc::clone(self.theme),
                    vim_mode: win.vim_mode(),
                },
            });
            return;
        }

        let slot = self.paint_leaves.len();
        self.paint_leaves.push(PreparedPaintLeaf {
            id,
            rect,
            context: DrawContext {
                terminal_width: self.term_size.0,
                terminal_height: self.term_size.1,
                focused: false,
                cursor_shape: CursorShape::Hidden,
                theme: std::sync::Arc::clone(self.theme),
                vim_mode: VimMode::default(),
            },
        });
        self.commands.push(PreparedFrameCommand::Paint(slot));
    }
}

fn collect_layout_frame_commands(
    layout: &layout::ResolvedLayout,
    root_chrome: layout::ChromePaintCtx,
    builder: &mut PreparedFrameBuilder<'_>,
) {
    for operation in layout.operations().iter().cloned() {
        match operation {
            smelt_term::LayoutPaintOp::Chrome { area, chrome, root } => {
                builder.commands.push(PreparedFrameCommand::Chrome {
                    area,
                    chrome,
                    context: if root {
                        root_chrome
                    } else {
                        layout::ChromePaintCtx::empty()
                    },
                });
            }
            smelt_term::LayoutPaintOp::Leaf { id, rect } => builder.push_leaf(id, rect),
            smelt_term::LayoutPaintOp::Divider(divider) => builder.push_divider(divider),
        }
    }
}

fn collect_root_frame_commands(
    layout: &layout::ResolvedLayout,
    decorations: &[ResolvedDecoration],
    active_resize: Option<(overlay::ChromeOwner, layout::ChromePaintCtx)>,
    builder: &mut PreparedFrameBuilder<'_>,
) {
    for operation in layout.operations().iter().cloned() {
        match operation {
            smelt_term::LayoutPaintOp::Divider(divider) => builder.push_divider(divider),
            smelt_term::LayoutPaintOp::Chrome { area, chrome, .. } => {
                let context = active_resize
                    .and_then(|(owner, context)| {
                        chrome
                            .container
                            .is_some_and(|id| owner == overlay::ChromeOwner::Container(id))
                            .then_some(context)
                    })
                    .unwrap_or_default();
                builder.commands.push(PreparedFrameCommand::Chrome {
                    area,
                    chrome,
                    context,
                });
            }
            smelt_term::LayoutPaintOp::Leaf { id, rect } => {
                builder.push_leaf(id, rect);
                for decoration in decorations
                    .iter()
                    .filter(|decoration| decoration.owner == WinId(id.0))
                {
                    builder
                        .commands
                        .push(PreparedFrameCommand::Clear(decoration.rect));
                    collect_layout_frame_commands(
                        &decoration.layout,
                        layout::ChromePaintCtx::empty(),
                        builder,
                    );
                }
            }
        }
    }
}

/// Looks up each window leaf's natural size from its buffer's current
/// (wrapped) line count. Paint regions and unknown ids report `(0, 0)`.
/// Callers that want the line count to reflect a freshly wrapped width must
/// run `Buffer::ensure_rendered_at` first; this sizer reads only.
pub(crate) struct UiLeafSizer<'a> {
    pub wins: &'a HashMap<WinId, Window>,
    pub bufs: &'a HashMap<BufId, Buffer>,
}

impl<'a> layout::LeafSizer for UiLeafSizer<'a> {
    fn leaf_natural_size(&self, id: PaintId, cap: (u16, u16)) -> (u16, u16) {
        let win_id = WinId(id.0);
        let Some(win) = self.wins.get(&win_id) else {
            return (0, 0);
        };
        let Some(buf) = self.bufs.get(&win.buf) else {
            return (0, 0);
        };
        let chrome = win
            .config
            .gutters
            .pad_left
            .saturating_add(win.config.gutters.pad_right)
            .saturating_add(win.config.gutters.scrollbar_width())
            .saturating_add(win.gutter_width(buf));
        let h = if win.wrap {
            let width = cap.0.saturating_sub(chrome);
            let rows = smelt_buffer::wrap_layout::WrappedLayout::from_buffer(buf, width, true)
                .visual_count();
            rows.min(u16::MAX as usize) as u16
        } else {
            (buf.lines().len() as u32).min(u16::MAX as u32) as u16
        };
        // Wrapped content has no intrinsic width - its layout depends on the
        // wrap column, which is whatever the parent slot resolves to. Defer
        // to the cap. For non-wrapping content we can compute the actual
        // longest-line width + chrome, so `width = "fit"` overlays shrink
        // around their content instead of defaulting to the terminal.
        let w = if win.wrap {
            cap.0
        } else {
            let longest = buf
                .lines()
                .iter()
                .map(|l| smelt_buffer::cell_width::text_width_u16(l.as_str()))
                .max()
                .unwrap_or(0);
            longest.saturating_add(chrome).min(cap.0)
        };
        (w, h.min(cap.1))
    }
}

impl layout::LeafSizer for Ui {
    fn leaf_natural_size(&self, id: PaintId, cap: (u16, u16)) -> (u16, u16) {
        UiLeafSizer {
            wins: &self.wins,
            bufs: &self.bufs,
        }
        .leaf_natural_size(id, cap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ui() -> Ui {
        let mut ui = Ui::new();
        ui.set_terminal_size(80, 24);
        ui
    }

    fn dispatch_key(
        ui: &mut Ui,
        code: crossterm::event::KeyCode,
        mods: crossterm::event::KeyModifiers,
    ) -> Status {
        ui.dispatch_event(
            Event::Key(crossterm::event::KeyEvent::new(code, mods)),
            &mut |_, _, _| {},
        )
    }

    fn make_split(ui: &mut Ui, win_id: WinId) {
        let buf = ui.buf_create(BufCreateOpts::default());
        assert!(ui.win_open_split_at(
            win_id,
            buf,
            SplitConfig {
                region: format!("test:{}", win_id.0),
                gutters: layout::Gutters::default(),
            },
        ));
        let mut leaves: Vec<(Constraint, LayoutTree)> = ui
            .splits()
            .leaves_in_order()
            .into_iter()
            .map(|w| (Constraint::Fill, LayoutTree::leaf(w)))
            .collect();
        leaves.push((Constraint::Fill, LayoutTree::leaf(win_id)));
        ui.set_layout(LayoutTree::vbox(leaves));
    }

    fn register_window(ui: &mut Ui, win_id: WinId) {
        let buf = ui.buf_create(BufCreateOpts::default());
        assert!(ui.win_open_split_at(
            win_id,
            buf,
            SplitConfig {
                region: format!("test:{}", win_id.0),
                gutters: layout::Gutters::default(),
            },
        ));
    }

    #[test]
    fn host_display_document_surfaces_buffer_actions() {
        let mut ui = make_ui();
        let win = WinId(42);
        register_window(&mut ui, win);
        let buf_id = ui.win(win).unwrap().buf;
        let action = smelt_buffer::buffer::SpanAction::OpenUrl("https://example.test".into());
        {
            let buf = ui.buf_mut(buf_id).unwrap();
            buf.set_all_lines(vec!["open link".into()]);
            buf.add_highlight_group_with_meta(
                0,
                5,
                9,
                smelt_buffer::theme::intern("SmeltLink"),
                smelt_buffer::buffer::SpanMeta::action(action.clone()),
            );
        }

        let mut doc = BufferDisplayDocument::new(&mut ui, win);

        assert_eq!(
            DisplayDocument::action_at(
                &mut doc,
                DocPosition {
                    row: 0,
                    byte_col: 6
                }
            ),
            Some(action)
        );
        assert_eq!(
            DisplayDocument::action_at(
                &mut doc,
                DocPosition {
                    row: 0,
                    byte_col: 4
                }
            ),
            None
        );
    }

    #[test]
    fn host_display_document_maps_materialized_rows_to_buffer_actions() {
        let mut ui = make_ui();
        let win = WinId(42);
        register_window(&mut ui, win);
        let buf_id = ui.win(win).unwrap().buf;
        let action = smelt_buffer::buffer::SpanAction::OpenUrl("https://example.test".into());
        {
            let buf = ui.buf_mut(buf_id).unwrap();
            buf.set_all_lines(vec!["zero".into(), "open link".into()]);
            buf.add_highlight_group_with_meta(
                1,
                5,
                9,
                smelt_buffer::theme::intern("SmeltLink"),
                smelt_buffer::buffer::SpanMeta::action(action.clone()),
            );
        }
        ui.win_mut(win)
            .unwrap()
            .apply_materialized_rows(MaterializedRows {
                clamped_scroll: 40,
                row_base: 40,
                total_rows: 100,
                materialized_rows: 2,
            });

        let mut doc = BufferDisplayDocument::new(&mut ui, win);

        assert_eq!(doc.snapshot().total_rows, 100);
        assert_eq!(
            DisplayDocument::action_at(
                &mut doc,
                DocPosition {
                    row: 39,
                    byte_col: 6
                }
            ),
            None
        );
        assert_eq!(
            DisplayDocument::action_at(
                &mut doc,
                DocPosition {
                    row: 41,
                    byte_col: 6
                }
            ),
            Some(action)
        );
    }

    #[test]
    fn replacing_buffer_lines_detaches_stale_materialized_windows() {
        let mut ui = make_ui();
        let first = WinId(42);
        let second = WinId(43);
        make_split(&mut ui, first);
        let buffer = ui.win(first).unwrap().buf;
        assert!(ui.win_open_split_at(
            second,
            buffer,
            SplitConfig {
                region: "test:43".into(),
                gutters: layout::Gutters::default(),
            },
        ));
        let lines_tick = ui.buf(buffer).unwrap().lines_tick();
        for window in [first, second] {
            ui.win_mut(window).unwrap().apply_materialized_rows_at_tick(
                MaterializedRows {
                    clamped_scroll: 10,
                    row_base: 8,
                    total_rows: 100,
                    materialized_rows: 20,
                },
                lines_tick,
            );
        }

        ui.buf_mut(buffer)
            .unwrap()
            .set_all_lines(vec!["ordinary buffer".into()]);
        for window in [first, second] {
            let (window, buffer) = ui.win_and_buf_mut(window, buffer);
            window.unwrap().ensure_layout(buffer.unwrap(), 40);
        }

        assert!(!ui.win(first).unwrap().has_materialized_rows());
        assert!(!ui.win(second).unwrap().has_materialized_rows());
    }

    #[test]
    fn render_prepare_hook_materializes_before_window_layout() {
        let mut ui = make_ui();
        let win = WinId(42);
        make_split(&mut ui, win);
        let buf_id = ui.win(win).unwrap().buf;
        let mut requests = Vec::new();
        let mut out = Vec::new();

        ui.render_with_paints_prepared(
            &mut out,
            |ui, request| {
                requests.push(request);
                ui.buf_mut(request.buf)
                    .unwrap()
                    .set_all_lines(vec!["prepared".into()]);
                ui.win_mut(request.win)
                    .unwrap()
                    .apply_materialized_rows(MaterializedRows {
                        clamped_scroll: request.scroll_top,
                        row_base: 4,
                        total_rows: 20,
                        materialized_rows: 1,
                    });
            },
            |_, _, _| {},
        )
        .unwrap();

        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].win, win);
        assert_eq!(requests[0].buf, buf_id);
        assert_eq!(requests[0].rect.height, 24);
        assert_eq!(ui.buf(buf_id).unwrap().lines(), &["prepared".to_string()]);
        let viewport = ui.win(win).unwrap().viewport.expect("render sets viewport");
        assert_eq!(viewport.total_rows, 20);
        assert_eq!(
            ui.win(win)
                .unwrap()
                .scroll_row_total(ui.buf(buf_id).unwrap()),
            20
        );
    }

    #[test]
    fn staged_paint_jobs_can_amend_owned_frame_before_flush() {
        let mut ui = make_ui();
        let paint_id = PaintId(1u64 << 32);
        ui.overlay_open(
            Overlay::new(LayoutTree::leaf(paint_id), layout::Anchor::ScreenCenter)
                .with_size((4, 2)),
        );
        let mut jobs = Vec::new();
        let mut frame = ui.paint_frame_with_prepared_splits_and_paints(
            std::iter::empty(),
            |_, _| {},
            |_, _| {},
            |id, slice, _| jobs.push((id, slice.grid_rect())),
        );

        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].0, paint_id);
        frame.slice_mut(jobs[0].1).set(0, 0, 'X', Style::default());
        ui.flush_prepared_frame(&mut Vec::new(), frame).unwrap();

        assert!(ui.flushed_snapshot().text().contains('X'));
    }

    #[test]
    fn prepared_frame_freezes_paint_geometry_and_context() {
        let mut ui = make_ui();
        let paint_id = PaintId(1u64 << 32);
        let overlay_id = ui.overlay_open(
            Overlay::new(LayoutTree::leaf(paint_id), layout::Anchor::ScreenCenter)
                .with_size((4, 2)),
        );
        let prepared =
            ui.prepare_frame_with_prepared_splits(std::iter::empty(), |_, _| {}, |_, _| {});
        let leaf = prepared
            .paint_leaves()
            .first()
            .expect("prepared paint leaf");
        let planned_rect = leaf.rect;
        assert_eq!((planned_rect.width, planned_rect.height), (4, 2));
        assert_eq!(
            (leaf.context.terminal_width, leaf.context.terminal_height),
            ui.terminal_size()
        );
        assert!(std::sync::Arc::ptr_eq(&leaf.context.theme, ui.theme()));

        let overlay = ui.overlay_mut(overlay_id).expect("overlay");
        overlay.width = Constraint::Length(20);
        overlay.height = Constraint::Length(10);
        let mut painted_rects = Vec::new();
        let _frame = ui.paint_prepared_frame_with_paints(prepared, |slot, id, slice, _context| {
            assert_eq!(slot, 0);
            assert_eq!(id, paint_id);
            painted_rects.push(slice.grid_rect());
        });

        assert_eq!(painted_rects, [planned_rect]);
    }

    #[test]
    fn prepared_split_is_not_prepared_again_during_paint() {
        let mut ui = make_ui();
        let win = WinId(42);
        make_split(&mut ui, win);
        let mut early_prepare_calls = 0;
        let prepared = ui
            .prepare_split_window_with(win, |_, request| {
                assert_eq!(request.win, win);
                early_prepare_calls += 1;
            })
            .expect("visible split should prepare");
        let mut late_prepare_calls = 0;
        let mut after_layout_calls = 0;
        let mut out = Vec::new();

        ui.render_with_prepared_splits_and_paints(
            &mut out,
            Some(prepared),
            |_, _| late_prepare_calls += 1,
            |_, request| {
                assert_eq!(request.win, win);
                after_layout_calls += 1;
            },
            |_, _, _| {},
        )
        .unwrap();

        assert_eq!(early_prepare_calls, 1);
        assert_eq!(late_prepare_calls, 0);
        assert_eq!(after_layout_calls, 1);
    }

    #[test]
    fn buf_create_with_id_lua_range_does_not_advance_rust_allocator() {
        let mut ui = make_ui();
        let rust_first = ui.buf_create(BufCreateOpts::default());
        ui.buf_create_with_id(BufId(LUA_BUF_ID_BASE), BufCreateOpts::default())
            .unwrap();
        let rust_second = ui.buf_create(BufCreateOpts::default());
        assert_eq!(rust_second.0, rust_first.0 + 1);
        assert!(rust_second.0 < LUA_BUF_ID_BASE);
    }

    // ── Overlay API ──────────────────────────────────────────────────

    fn stub_overlay() -> Overlay {
        let layout = LayoutTree::vbox(vec![(Constraint::Fill, LayoutTree::leaf(WinId(99)))]);
        Overlay::new(layout, layout::Anchor::ScreenCenter)
    }

    #[test]
    fn overlay_open_returns_unique_ids() {
        let mut ui = make_ui();
        let a = ui.overlay_open(stub_overlay());
        let b = ui.overlay_open(stub_overlay());
        assert_ne!(a, b);
        assert!(ui.overlay(a).is_some());
        assert!(ui.overlay(b).is_some());
    }

    #[test]
    fn overlay_close_removes_overlay() {
        let mut ui = make_ui();
        let id = ui.overlay_open(stub_overlay());
        let removed = ui.overlay_close(id);
        assert!(removed.is_some());
        assert!(ui.overlay(id).is_none());
        assert!(ui.overlay_close(id).is_none());
    }

    #[test]
    fn closing_split_owner_closes_owned_decorations() {
        let mut ui = make_ui();
        let owner = WinId(10);
        let child = WinId(11);
        let grandchild = WinId(12);
        make_split(&mut ui, owner);
        register_window(&mut ui, child);
        register_window(&mut ui, grandchild);

        let first = ui.decoration_open(Decoration::new(owner, LayoutTree::leaf(child)));
        let nested = ui.decoration_open(Decoration::new(child, LayoutTree::leaf(grandchild)));
        assert!(ui.set_focus(owner));
        assert!(ui.set_focus(child));

        let _ = ui.win_close(owner);

        assert!(ui.decoration(first).is_none());
        assert!(ui.decoration(nested).is_none());
        assert!(ui.focus().is_none());
        assert!(ui.win(owner).is_none());
        assert!(ui.win(child).is_none());
        assert!(ui.win(grandchild).is_none());
    }

    #[test]
    fn closing_overlay_owner_closes_owned_decorations() {
        let mut ui = make_ui();
        let owner = WinId(20);
        let child = WinId(21);
        register_window(&mut ui, owner);
        register_window(&mut ui, child);
        ui.overlay_open(Overlay::new(
            LayoutTree::leaf(owner),
            layout::Anchor::ScreenCenter,
        ));
        let decoration = ui.decoration_open(Decoration::new(owner, LayoutTree::leaf(child)));

        let _ = ui.win_close(owner);

        assert!(ui.decoration(decoration).is_none());
        assert!(ui.win(owner).is_none());
        assert!(ui.win(child).is_none());
    }

    #[test]
    fn overlay_mut_allows_anchor_drag() {
        let mut ui = make_ui();
        let id = ui.overlay_open(stub_overlay());
        ui.overlay_mut(id).unwrap().anchor = layout::Anchor::ScreenAt {
            row: 5,
            col: 10,
            corner: Corner::NW,
        };
        assert!(matches!(
            ui.overlay(id).unwrap().anchor,
            layout::Anchor::ScreenAt {
                row: 5,
                col: 10,
                ..
            }
        ));
    }

    #[test]
    fn overlays_in_z_order_sorts_by_z_then_id() {
        let mut ui = make_ui();
        let high = ui.overlay_open(stub_overlay().with_z(100));
        let mid = ui.overlay_open(stub_overlay().with_z(50));
        let low_a = ui.overlay_open(stub_overlay().with_z(10));
        let low_b = ui.overlay_open(stub_overlay().with_z(10));
        let order: Vec<OverlayId> = ui
            .overlays_in_z_order()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        // Lowest z first; same z falls back to insertion order (id).
        assert_eq!(order, vec![low_a, low_b, mid, high]);
    }

    fn sized_overlay(width: u16, height: u16, anchor: layout::Anchor) -> Overlay {
        // Single-leaf box wrapped in an Hbox of fixed width holding a
        // Vbox of fixed height - exercises both axes' natural-size
        // composition.
        let layout = LayoutTree::hbox(vec![(
            Constraint::Length(width),
            LayoutTree::vbox(vec![(
                Constraint::Length(height),
                LayoutTree::leaf(WinId(99)),
            )]),
        )]);
        Overlay::new(layout, anchor)
    }

    fn bordered_overlay(width: u16, height: u16, anchor: layout::Anchor) -> Overlay {
        let inner_w = width.saturating_sub(2);
        let inner_h = height.saturating_sub(2);
        let layout = LayoutTree::hbox(vec![(
            Constraint::Length(inner_w),
            LayoutTree::vbox(vec![(
                Constraint::Length(inner_h),
                LayoutTree::leaf(WinId(99)),
            )]),
        )])
        .with_border(layout::Border::SINGLE);
        Overlay::new(layout, anchor)
    }

    #[test]
    fn fit_sized_split_geometry_matches_painted_viewport() {
        let mut ui = make_ui();
        let buf = ui.buf_create(BufCreateOpts::default());
        ui.buf_mut(buf)
            .expect("buffer")
            .set_all_lines((0..6).map(|row| format!("row {row}")).collect());
        let win = WinId(200);
        assert!(ui.win_open_split_at(
            win,
            buf,
            SplitConfig {
                region: "fit-list".into(),
                gutters: layout::Gutters::default(),
            },
        ));
        ui.set_layout(LayoutTree::vbox(vec![
            (Constraint::Fill, LayoutTree::vbox(Vec::new())),
            (Constraint::Fit, LayoutTree::leaf(win)),
        ]));

        let expected = Rect::new(18, 0, 80, 6);
        assert_eq!(ui.split_rect(win), Some(expected));
        assert_eq!(ui.paint_rect(PaintId::from(win)), Some(expected));

        ui.render(&mut std::io::sink()).expect("render fit split");
        assert_eq!(
            ui.win(win).and_then(|win| win.viewport).map(|vp| vp.rect),
            Some(expected)
        );
    }

    #[test]
    fn resolved_win_sizes_match_painted_content_across_surfaces() {
        let mut ui = make_ui();
        let root = WinId(10);
        let overlay = WinId(99);
        let decoration = WinId(12);
        let hidden = WinId(13);
        make_split(&mut ui, root);
        for win in [overlay, decoration, hidden] {
            register_window(&mut ui, win);
        }
        ui.overlay_open(sized_overlay(40, 10, layout::Anchor::ScreenCenter));
        ui.decoration_open(Decoration::new(root, LayoutTree::leaf(decoration)));
        for win in [root, overlay, decoration] {
            let window = ui.win_mut(win).unwrap();
            window.config.gutters.pad_left = 2;
            window.config.gutters.pad_right = 1;
        }

        for (width, height) in [(80, 24), (30, 12), (2, 4)] {
            ui.set_terminal_size(width, height);
            let sizes = ui.resolved_win_sizes();
            assert!(!sizes.contains_key(&hidden));
            ui.render(&mut std::io::sink()).unwrap();
            for win in [root, overlay, decoration] {
                let viewport = ui.win(win).unwrap().viewport.unwrap();
                assert_eq!(
                    sizes.get(&win),
                    Some(&(viewport.content_width, viewport.rect.height)),
                    "{win:?} at {width}x{height}"
                );
                assert_eq!(ui.win_content_width(win), Some(viewport.content_width));
            }
        }
    }

    #[test]
    fn resolve_overlays_centers_screen_center_anchor() {
        let mut ui = make_ui();
        let id = ui.overlay_open(sized_overlay(40, 10, layout::Anchor::ScreenCenter));
        let resolved = ui.resolve_overlays(None);
        assert_eq!(resolved.len(), 1);
        let (got_id, rect, _) = &resolved[0];
        assert_eq!(*got_id, id);
        // Centered: term 80x24, overlay 40x10 → top=7, left=20.
        assert_eq!(*rect, Rect::new(7, 20, 40, 10));
    }

    #[test]
    fn resolve_overlays_skips_cursor_anchor_when_cursor_missing() {
        let mut ui = make_ui();
        ui.overlay_open(sized_overlay(
            10,
            5,
            layout::Anchor::Cursor {
                corner: Corner::NW,
                row_offset: 0,
                col_offset: 0,
            },
        ));
        // No cursor supplied → overlay drops out of the resolved set.
        assert!(ui.resolve_overlays(None).is_empty());
        // With a cursor, it resolves.
        let resolved = ui.resolve_overlays(Some((4, 6)));
        assert_eq!(resolved.len(), 1);
    }

    #[test]
    fn resolve_overlays_skips_win_anchor_when_target_missing() {
        let mut ui = make_ui();
        ui.overlay_open(sized_overlay(
            10,
            5,
            layout::Anchor::Win {
                target: WinId(999).into(),
                attach: layout::Align::NW,
                row_offset: 0,
                col_offset: 0,
            },
        ));
        assert!(ui.resolve_overlays(None).is_empty());
        // Once the target lands as a splits leaf with a known rect,
        // the overlay resolves anchored to it. Build a tree that
        // produces rect (top=5, left=10, width=30, height=8) on an
        // 80x24 terminal.
        let target = WinId(999);
        let buf = ui.buf_create(BufCreateOpts::default());
        assert!(ui.win_open_split_at(
            target,
            buf,
            SplitConfig {
                region: "anchor".into(),
                gutters: layout::Gutters::default(),
            },
        ));
        // vbox: 5 rows blank + 8-row hbox (10 cols blank + 30-col leaf
        // + fill) + fill below.
        let tree = LayoutTree::vbox(vec![
            (Constraint::Length(5), LayoutTree::vbox(Vec::new())),
            (
                Constraint::Length(8),
                LayoutTree::hbox(vec![
                    (Constraint::Length(10), LayoutTree::vbox(Vec::new())),
                    (Constraint::Length(30), LayoutTree::leaf(target)),
                    (Constraint::Fill, LayoutTree::vbox(Vec::new())),
                ]),
            ),
            (Constraint::Fill, LayoutTree::vbox(Vec::new())),
        ]);
        ui.set_layout(tree);
        assert_eq!(ui.split_rect(target), Some(Rect::new(5, 10, 30, 8)));
        let resolved = ui.resolve_overlays(None);
        assert_eq!(resolved.len(), 1);
        let (_, rect, _) = &resolved[0];
        assert_eq!(*rect, Rect::new(5, 10, 10, 5));
    }

    #[test]
    fn active_modal_empty_returns_none() {
        let ui = make_ui();
        assert_eq!(ui.active_modal(), None);
    }

    #[test]
    fn active_modal_skips_non_modal_overlays() {
        let mut ui = make_ui();
        ui.overlay_open(stub_overlay()); // non-modal
        ui.overlay_open(stub_overlay().with_z(100)); // non-modal, higher z
        assert_eq!(ui.active_modal(), None);
    }

    #[test]
    fn focused_window_returns_window_for_split_with_inserted_win() {
        let mut ui = make_ui();
        let win = WinId(7);
        make_split(&mut ui, win);
        ui.set_focus(win);
        assert_eq!(ui.focused_window().map(|w| w.id), Some(win));
    }

    #[test]
    fn overlay_close_with_focus_inside_pops_to_prior() {
        let mut ui = make_ui();
        let outside = WinId(7);
        make_split(&mut ui, outside);
        // Inside-the-overlay leaf id (stub_overlay uses WinId(99)).
        let inside = WinId(99);
        make_split(&mut ui, inside);
        let id = ui.overlay_open(stub_overlay());

        ui.set_focus(outside);
        ui.set_focus(inside);
        assert_eq!(ui.focus(), Some(inside));
        assert_eq!(ui.focus_history(), &[outside]);

        ui.overlay_close(id);
        // Pop walked back to `outside`; history drained.
        assert_eq!(ui.focus(), Some(outside));
        assert!(ui.focus_history().is_empty());
    }

    #[test]
    fn overlay_close_with_focus_outside_leaves_focus_alone() {
        let mut ui = make_ui();
        let outside = WinId(50);
        make_split(&mut ui, outside);
        let id = ui.overlay_open(stub_overlay());
        ui.set_focus(outside);
        ui.overlay_close(id);
        assert_eq!(ui.focus(), Some(outside));
    }

    #[test]
    fn overlay_close_with_exhausted_history_clears_focus() {
        let mut ui = make_ui();
        let inside = WinId(99); // stub_overlay's leaf
        make_split(&mut ui, inside);
        let id = ui.overlay_open(stub_overlay());
        ui.set_focus(inside);
        // No prior focus - history empty.
        assert!(ui.focus_history().is_empty());
        ui.overlay_close(id);
        assert_eq!(ui.focus(), None);
    }

    #[test]
    fn focused_overlay_returns_none_when_no_focus() {
        let mut ui = make_ui();
        ui.overlay_open(stub_overlay());
        assert_eq!(ui.focused_overlay(), None);
    }

    #[test]
    fn focused_overlay_returns_overlay_containing_focused_leaf() {
        let mut ui = make_ui();
        let win = WinId(99);
        make_split(&mut ui, win);
        let id = ui.overlay_open(stub_overlay()); // stub uses Leaf(WinId(99))
        ui.set_focus(win);
        assert_eq!(ui.focused_overlay(), Some(id));
    }

    #[test]
    fn focused_overlay_returns_none_when_focus_on_unrelated_split() {
        let mut ui = make_ui();
        let other = WinId(50);
        make_split(&mut ui, other);
        ui.overlay_open(stub_overlay());
        ui.set_focus(other);
        assert_eq!(ui.focused_overlay(), None);
    }

    #[test]
    fn active_modal_overlay_follows_visual_z_order() {
        let mut ui = make_ui();
        let high_win = WinId(98);
        let low_win = WinId(99);
        make_split(&mut ui, high_win);
        make_split(&mut ui, low_win);
        let high = ui.overlay_open(
            Overlay::new(LayoutTree::leaf(high_win), layout::Anchor::ScreenCenter)
                .with_z(100)
                .modal(true),
        );
        let low = ui.overlay_open(
            Overlay::new(LayoutTree::leaf(low_win), layout::Anchor::ScreenCenter)
                .with_z(10)
                .modal(true),
        );

        assert_eq!(ui.active_modal_overlay(), Some(high));
        assert_eq!(ui.focus(), Some(high_win));

        ui.raise_overlay_to_front(low);
        assert_eq!(ui.active_modal_overlay(), Some(low));
        assert_eq!(ui.focus(), Some(low_win));

        ui.overlay_close(low);
        assert_eq!(ui.active_modal_overlay(), Some(high));
        assert_eq!(ui.focus(), Some(high_win));
    }

    #[test]
    fn raising_active_modal_overlay_preserves_its_focused_leaf() {
        for focusable in [false, true] {
            let mut ui = make_ui();
            let [first, second, third] = [WinId(100), WinId(101), WinId(102)];
            for win in [first, second, third] {
                register_window(&mut ui, win);
            }
            ui.win_mut(second)
                .unwrap()
                .set_surface(WindowSurface::list(focusable));
            let overlay = ui.overlay_open(modal_overlay_with_leaves(first, second, third));
            assert_eq!(ui.focus(), Some(first));
            assert!(ui.set_focus(second));
            let history = ui.focus_history().to_vec();
            for _ in 0..3 {
                ui.raise_overlay_to_front(overlay);
                assert_eq!(ui.active_modal_overlay(), Some(overlay));
                assert_eq!(ui.focus(), Some(second));
                assert_eq!(ui.focus_history(), history);
            }
            ui.focus = None;
            ui.raise_overlay_to_front(overlay);
            assert_eq!(ui.focus(), Some(first));
        }
    }

    #[test]
    fn docked_modal_focuses_its_leaf_and_contains_programmatic_focus() {
        let mut ui = make_ui();
        let background = WinId(7);
        let dialog = WinId(100);
        make_split(&mut ui, background);
        register_window(&mut ui, dialog);
        ui.win_mut(dialog)
            .expect("dialog window")
            .set_surface(WindowSurface::selectable_text());
        assert!(ui.set_focus(background));

        let (_, modal) = ui.docked_surface_open(
            LayoutTree::leaf(dialog),
            vec![dialog],
            DockedSurfaceConfig {
                height: Constraint::Length(1),
                min_height: None,
                max_height: None,
                resize: ResizeConfig::none(),
                fit_reserved_rows: 0,
                blocks_agent: false,
            },
        );

        assert_eq!(ui.active_modal(), Some(modal));
        assert_eq!(ui.focused_modal(), Some(modal));
        assert_eq!(ui.focus(), Some(dialog));
        assert!(!ui.set_focus(background));
        assert_eq!(ui.focus(), Some(dialog));

        let _ = ui.modal_close(modal);
        assert!(ui.set_focus(background));
        assert_eq!(ui.focus(), Some(background));
    }

    #[test]
    fn active_modal_overlay_contains_focus_but_allows_owned_leaves() {
        let mut ui = make_ui();
        let background = WinId(7);
        let first = WinId(100);
        let second = WinId(101);
        let third = WinId(102);
        make_split(&mut ui, background);
        for win in [first, second, third] {
            register_window(&mut ui, win);
        }
        assert!(ui.set_focus(background));

        let overlay = ui.overlay_open(modal_overlay_with_leaves(first, second, third));

        assert_eq!(ui.focus(), Some(first));
        assert!(!ui.set_focus(background));
        assert!(ui.set_focus(second));
        assert_eq!(ui.focus(), Some(second));

        ui.overlay_close(overlay);
        assert_eq!(ui.focus(), Some(background));
    }

    #[test]
    fn modal_overlay_stays_active_above_later_docked_modal() {
        let mut ui = make_ui();
        let docked_leaf = WinId(100);
        register_window(&mut ui, docked_leaf);
        let overlay = ui.overlay_open(stub_overlay().modal(true));
        let (_, docked) = ui.docked_surface_open(
            LayoutTree::leaf(docked_leaf),
            vec![docked_leaf],
            DockedSurfaceConfig {
                height: Constraint::Length(1),
                min_height: None,
                max_height: None,
                resize: ResizeConfig::none(),
                fit_reserved_rows: 0,
                blocks_agent: false,
            },
        );

        assert_eq!(ui.active_modal_overlay(), Some(overlay));
        assert_eq!(ui.focus(), Some(WinId(99)));
        assert!(!ui.set_focus(docked_leaf));
        assert_eq!(ui.focus(), Some(WinId(99)));

        ui.overlay_close(overlay);
        assert_eq!(ui.active_modal(), Some(docked));
        assert_eq!(ui.focus(), Some(docked_leaf));
        assert_eq!(
            ui.active_modal_owner(),
            Some(ModalOwner::Docked(ContainerId(1)))
        );
    }

    #[test]
    fn set_layout_restores_focus_to_active_modal() {
        let mut ui = make_ui();
        let background = WinId(7);
        let dialog = WinId(100);
        make_split(&mut ui, background);
        register_window(&mut ui, dialog);
        let (_, modal) = ui.docked_surface_open(
            LayoutTree::leaf(dialog),
            vec![dialog],
            DockedSurfaceConfig {
                height: Constraint::Length(1),
                min_height: None,
                max_height: None,
                resize: ResizeConfig::none(),
                fit_reserved_rows: 0,
                blocks_agent: false,
            },
        );

        ui.set_layout(LayoutTree::leaf(background));

        assert_eq!(ui.active_modal(), Some(modal));
        assert_eq!(ui.focus(), Some(dialog));
    }

    #[test]
    fn removing_overlay_modality_preserves_focus() {
        let mut ui = make_ui();
        let overlay = ui.overlay_open(stub_overlay().modal(true));
        assert_eq!(ui.focus(), Some(WinId(99)));

        ui.overlay_mut(overlay).expect("overlay").modal = false;
        ui.sync_overlay_modal(overlay);

        assert_eq!(ui.active_modal(), None);
        assert_eq!(ui.focus(), Some(WinId(99)));
    }

    #[test]
    fn focus_returns_none_on_fresh_ui() {
        let ui = make_ui();
        assert_eq!(ui.focus(), None);
        assert!(ui.focus_history().is_empty());
    }

    #[test]
    fn set_focus_unknown_win_returns_false() {
        let mut ui = make_ui();
        assert!(!ui.set_focus(WinId(999)));
        assert_eq!(ui.focus(), None);
    }

    #[test]
    fn set_focus_on_registered_split_focuses_the_win() {
        let mut ui = make_ui();
        let win = WinId(7);
        make_split(&mut ui, win);
        assert!(ui.set_focus(win));
        assert_eq!(ui.focus(), Some(win));
        assert!(ui.focus_history().is_empty());
    }

    #[test]
    fn set_focus_pushes_prior_focus_to_history() {
        let mut ui = make_ui();
        let a = WinId(7);
        let b = WinId(8);
        make_split(&mut ui, a);
        make_split(&mut ui, b);
        ui.set_focus(a);
        ui.set_focus(b);
        assert_eq!(ui.focus(), Some(b));
        assert_eq!(ui.focus_history(), &[a]);
    }

    #[test]
    fn set_focus_same_win_is_noop() {
        let mut ui = make_ui();
        let win = WinId(7);
        make_split(&mut ui, win);
        ui.set_focus(win);
        assert!(ui.set_focus(win));
        assert!(ui.focus_history().is_empty());
    }

    #[test]
    fn set_focus_chain_builds_history_in_order() {
        let mut ui = make_ui();
        for n in 1..=4 {
            make_split(&mut ui, WinId(n));
        }
        ui.set_focus(WinId(1));
        ui.set_focus(WinId(2));
        ui.set_focus(WinId(3));
        ui.set_focus(WinId(4));
        assert_eq!(ui.focus(), Some(WinId(4)));
        assert_eq!(ui.focus_history(), &[WinId(1), WinId(2), WinId(3)]);
    }

    #[test]
    fn overlay_hit_test_returns_none_when_empty() {
        let ui = make_ui();
        assert_eq!(ui.overlay_hit_test(10, 30, None), None);
    }

    #[test]
    fn overlay_hit_test_window_inside_leaf() {
        let mut ui = make_ui();
        register_window(&mut ui, WinId(99));
        // 40x10 overlay centered at (7, 20)..(17, 60); single Leaf.
        let id = ui.overlay_open(sized_overlay(40, 10, layout::Anchor::ScreenCenter));
        let hit = ui.overlay_hit_test(10, 30, None).unwrap();
        assert_eq!(hit.0, id);
        assert!(matches!(hit.1, HitTarget::Window(WinId(99))));
    }

    #[test]
    fn overlay_hit_test_chrome_when_inside_overlay_outside_leaves() {
        let mut ui = make_ui();
        register_window(&mut ui, WinId(99));
        // Outer Vbox with single-border + inner Hbox of fixed width
        // gives the overlay a concrete (42, 10) natural size centered
        // at (7, 19). Border consumes the top/bottom row + left/right
        // col; leaf occupies rows 8..16, cols 20..60.
        let id = ui.overlay_open(bordered_overlay(42, 10, layout::Anchor::ScreenCenter));
        // Inside overlay rect (row 7 = top border), outside the leaf.
        let hit = ui.overlay_hit_test(7, 30, None).unwrap();
        assert_eq!(hit.0, id);
        assert_eq!(
            hit.1,
            HitTarget::Chrome {
                owner: ChromeOwner::Overlay(id),
                action: ChromeAction::None
            }
        );
        // Inside the leaf → Window.
        let hit = ui.overlay_hit_test(10, 30, None).unwrap();
        assert!(matches!(hit.1, HitTarget::Window(WinId(99))));
    }

    #[test]
    fn resizable_true_keeps_title_drag_on_top_chrome() {
        let mut ui = make_ui();
        let id = ui.overlay_open(
            bordered_overlay(42, 10, layout::Anchor::ScreenCenter)
                .draggable(true)
                .resizable(true),
        );
        let down = mouse_event(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            7,
            30,
        );
        assert_eq!(ui.dispatch_event(down, &mut |_, _, _| {}), Status::Consumed);
        let drag = ui.chrome_drag.expect("title row should start drag");
        assert_eq!(drag.owner, overlay::ChromeOwner::Overlay(id));
        assert_eq!(drag.action, overlay::ChromeAction::Move);
    }

    #[test]
    fn resizable_true_bottom_right_corner_resizes_southeast() {
        let mut ui = make_ui();
        let id = ui.overlay_open(
            bordered_overlay(42, 10, layout::Anchor::ScreenCenter)
                .draggable(true)
                .resizable(true),
        );
        let down = mouse_event(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            16,
            60,
        );
        assert_eq!(ui.dispatch_event(down, &mut |_, _, _| {}), Status::Consumed);
        assert_eq!(
            ui.chrome_drag.unwrap().action,
            overlay::ChromeAction::Resize(overlay::ResizeEdges::corner(false, true, true, false))
        );

        let drag = mouse_event(
            crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            18,
            63,
        );
        assert_eq!(ui.dispatch_event(drag, &mut |_, _, _| {}), Status::Consumed);
        assert_eq!(ui.overlay(id).unwrap().size_override, Some((45, 12)));
    }

    #[test]
    fn active_overlay_resize_highlights_border() {
        let mut ui = make_ui();
        register_window(&mut ui, WinId(99));
        ui.theme_mut().set(
            "SmeltResizeHandle",
            Style {
                fg: Some(Color::Red),
                ..Style::default()
            },
        );
        ui.overlay_open(
            bordered_overlay(42, 10, layout::Anchor::ScreenCenter)
                .draggable(true)
                .resizable(true),
        );
        let down = mouse_event(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            16,
            60,
        );
        assert_eq!(ui.dispatch_event(down, &mut |_, _, _| {}), Status::Consumed);

        let frame = ui.snapshot();
        assert_eq!(frame.styles[10][60].fg, Some(Color::Red));
        assert_eq!(frame.styles[16][30].fg, Some(Color::Red));
        assert_eq!(frame.styles[7][30].fg, None);
        assert_eq!(frame.styles[10][19].fg, None);
    }

    #[test]
    fn top_resize_config_makes_top_chrome_resize_north() {
        let mut ui = make_ui();
        let id = ui.overlay_open(
            bordered_overlay(42, 10, layout::Anchor::ScreenCenter).resize_config(
                overlay::ResizeConfig {
                    top: true,
                    right: false,
                    bottom: false,
                    left: false,
                    corners: false,
                },
            ),
        );
        let down = mouse_event(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            7,
            30,
        );
        assert_eq!(ui.dispatch_event(down, &mut |_, _, _| {}), Status::Consumed);
        let drag = ui.chrome_drag.expect("top row should start resize");
        assert_eq!(drag.owner, overlay::ChromeOwner::Overlay(id));
        assert_eq!(
            drag.action,
            overlay::ChromeAction::Resize(overlay::ResizeEdges::north())
        );
    }

    #[test]
    fn top_resizing_bottom_docked_overlay_preserves_bottom_anchor() {
        let mut ui = make_ui();
        let id = ui.overlay_open(
            sized_overlay(20, 6, layout::Anchor::ScreenBottom { above_rows: 0 }).resize_config(
                overlay::ResizeConfig {
                    top: true,
                    right: false,
                    bottom: false,
                    left: false,
                    corners: false,
                },
            ),
        );
        let start_rect = ui.resolved_overlay_rect(id).unwrap();
        assert_eq!(start_rect, Rect::new(18, 30, 20, 6));

        ui.apply_chrome_drag(
            ChromeDrag {
                owner: overlay::ChromeOwner::Overlay(id),
                action: overlay::ChromeAction::Resize(overlay::ResizeEdges::north()),
                start_rect,
                origin_row: 18,
                origin_col: 30,
            },
            15,
            30,
        );

        let ov = ui.overlay(id).unwrap();
        assert_eq!(ov.size_override, Some((20, 9)));
        assert!(matches!(
            ov.anchor,
            layout::Anchor::ScreenBottom { above_rows: 0 }
        ));
        assert_eq!(
            ui.resolved_overlay_rect(id).unwrap(),
            Rect::new(15, 30, 20, 9)
        );
    }

    #[test]
    fn terminal_resize_reflows_unmodified_axis_of_size_override() {
        let mut ui = make_ui();
        let id = ui.overlay_open(
            Overlay::new(
                LayoutTree::leaf(PaintId(900)),
                layout::Anchor::ScreenBottom { above_rows: 0 },
            )
            .with_width(Constraint::Percentage(100))
            .with_height(Constraint::Length(6))
            .resize_config(overlay::ResizeConfig {
                top: true,
                right: false,
                bottom: false,
                left: false,
                corners: false,
            }),
        );
        let start_rect = ui.resolved_overlay_rect(id).unwrap();
        assert_eq!(start_rect, Rect::new(18, 0, 80, 6));

        ui.apply_chrome_drag(
            ChromeDrag {
                owner: overlay::ChromeOwner::Overlay(id),
                action: overlay::ChromeAction::Resize(overlay::ResizeEdges::north()),
                start_rect,
                origin_row: 18,
                origin_col: 0,
            },
            15,
            0,
        );
        assert_eq!(ui.overlay(id).unwrap().size_override, Some((80, 9)));

        ui.set_terminal_size(100, 24);
        assert_eq!(ui.overlay(id).unwrap().size_override, Some((100, 9)));
        assert_eq!(
            ui.resolved_overlay_rect(id).unwrap(),
            Rect::new(15, 0, 100, 9)
        );
    }

    #[test]
    fn resize_from_left_keeps_right_edge_fixed() {
        let mut ui = make_ui();
        let id = ui.overlay_open(sized_overlay(
            20,
            6,
            layout::Anchor::ScreenAt {
                row: 5,
                col: 10,
                corner: Corner::NW,
            },
        ));
        let start_rect = ui.resolved_overlay_rect(id).unwrap();
        ui.apply_chrome_drag(
            ChromeDrag {
                owner: overlay::ChromeOwner::Overlay(id),
                action: overlay::ChromeAction::Resize(overlay::ResizeEdges::west()),
                start_rect,
                origin_row: 5,
                origin_col: 10,
            },
            5,
            14,
        );

        let ov = ui.overlay(id).unwrap();
        assert_eq!(ov.size_override, Some((16, 6)));
        assert!(matches!(
            ov.anchor,
            layout::Anchor::ScreenAt {
                row: 5,
                col: 14,
                corner: Corner::NW,
            }
        ));
    }

    #[test]
    fn manual_resize_respects_min_and_max_constraints() {
        let mut ui = make_ui();
        let id = ui.overlay_open(
            sized_overlay(
                20,
                6,
                layout::Anchor::ScreenAt {
                    row: 5,
                    col: 10,
                    corner: Corner::NW,
                },
            )
            .with_min_width(Some(Constraint::Length(15)))
            .with_max_height(Some(Constraint::Length(7))),
        );
        let start_rect = ui.resolved_overlay_rect(id).unwrap();
        ui.apply_chrome_drag(
            ChromeDrag {
                owner: overlay::ChromeOwner::Overlay(id),
                action: overlay::ChromeAction::Resize(overlay::ResizeEdges::east()),
                start_rect,
                origin_row: 5,
                origin_col: 29,
            },
            5,
            0,
        );
        assert_eq!(ui.overlay(id).unwrap().size_override, Some((15, 6)));

        ui.apply_chrome_drag(
            ChromeDrag {
                owner: overlay::ChromeOwner::Overlay(id),
                action: overlay::ChromeAction::Resize(overlay::ResizeEdges::south()),
                start_rect,
                origin_row: 10,
                origin_col: 29,
            },
            20,
            29,
        );
        assert_eq!(ui.overlay(id).unwrap().size_override, Some((20, 7)));
    }

    #[test]
    fn overlay_hit_test_returns_none_outside_overlay_rect() {
        let mut ui = make_ui();
        ui.overlay_open(sized_overlay(40, 10, layout::Anchor::ScreenCenter));
        // (0, 0) is outside the overlay's centered rect.
        assert_eq!(ui.overlay_hit_test(0, 0, None), None);
    }

    #[test]
    fn body_click_drags_non_focusable_draggable_overlay_no_modal() {
        let mut ui = make_ui();
        let buf = ui.buf_create(BufCreateOpts::default());
        let leaf = WinId(123);
        assert!(ui.win_open_split_at(
            leaf,
            buf,
            SplitConfig {
                region: "test".into(),
                gutters: Default::default(),
            }
        ));
        ui.win_mut(leaf)
            .unwrap()
            .set_surface(WindowSurface::inert());

        let perf_layout = LayoutTree::hbox(vec![(
            Constraint::Length(10),
            LayoutTree::vbox(vec![(Constraint::Length(4), LayoutTree::leaf(leaf))]),
        )])
        .with_border(layout::Border::SINGLE)
        .with_title("perf");
        let perf = ui.overlay_open(
            Overlay::new(
                perf_layout,
                layout::Anchor::ScreenAt {
                    row: 0,
                    col: 60,
                    corner: layout::Corner::NW,
                },
            )
            .draggable(true),
        );
        // Click on the body (inside the leaf, not the border).
        let down = crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 65,
            row: 2,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        let status = ui.dispatch_event(Event::Mouse(down), &mut |_, _, _| {});
        assert_eq!(status, Status::Consumed);
        assert!(
            ui.chrome_drag.is_some(),
            "body click on inert leaf of a draggable overlay should latch drag (no modal)"
        );
        assert_eq!(
            ui.chrome_drag.unwrap().owner,
            overlay::ChromeOwner::Overlay(perf)
        );
    }

    #[test]
    fn body_click_drags_non_focusable_draggable_overlay_through_modal() {
        // Regression: an inert leaf inside a draggable overlay
        // (perf panel - pure HUD) treats body
        // clicks the same as chrome Title/Body for drag purposes,
        // *and* this works while a modal dialog is open below.
        let mut ui = make_ui();
        let buf = ui.buf_create(BufCreateOpts::default());
        let leaf = WinId(123);
        assert!(ui.win_open_split_at(
            leaf,
            buf,
            SplitConfig {
                region: "test".into(),
                gutters: Default::default(),
            }
        ));
        // Mark the leaf inert (matches HUD-style Lua surfaces).
        ui.win_mut(leaf)
            .unwrap()
            .set_surface(WindowSurface::inert());

        let perf_layout = LayoutTree::hbox(vec![(
            Constraint::Length(10),
            LayoutTree::vbox(vec![(Constraint::Length(4), LayoutTree::leaf(leaf))]),
        )])
        .with_border(layout::Border::SINGLE)
        .with_title("perf");
        let perf = ui.overlay_open(
            Overlay::new(
                perf_layout,
                layout::Anchor::ScreenAt {
                    row: 0,
                    col: 60,
                    corner: layout::Corner::NW,
                },
            )
            .draggable(true),
        );
        ui.overlay_open(
            sized_overlay(80, 8, layout::Anchor::ScreenBottom { above_rows: 1 }).modal(true),
        );

        // Click on the body (inside the leaf, not the border).
        let down = crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 65,
            row: 2,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        let status = ui.dispatch_event(Event::Mouse(down), &mut |_, _, _| {});
        assert_eq!(status, Status::Consumed);
        assert!(
            ui.chrome_drag.is_some(),
            "body click on non-focusable leaf of a draggable overlay should latch drag"
        );
        assert_eq!(
            ui.chrome_drag.unwrap().owner,
            overlay::ChromeOwner::Overlay(perf)
        );
    }

    #[test]
    fn chrome_drag_latches_on_non_modal_overlay_with_modal_open() {
        // Regression: a draggable non-modal overlay (perf panel) and a
        // modal dialog (messages) both at default z=50 should not
        // block each other's chrome drag - if the click lands on the
        // perf panel's chrome, the drag must latch.
        let mut ui = make_ui();
        // Perf panel: bordered, top-right corner, draggable. Outer
        // rect 10×4 with single border → top row is Title chrome.
        let perf_layout = LayoutTree::hbox(vec![(
            Constraint::Length(10),
            LayoutTree::vbox(vec![(Constraint::Length(4), LayoutTree::leaf(WinId(99)))]),
        )])
        .with_border(layout::Border::SINGLE)
        .with_title("perf");
        let perf = ui.overlay_open(
            Overlay::new(
                perf_layout,
                layout::Anchor::ScreenAt {
                    row: 0,
                    col: 60,
                    corner: layout::Corner::NW,
                },
            )
            .draggable(true),
        );
        // Messages dialog: bottom-docked modal.
        ui.overlay_open(
            sized_overlay(80, 8, layout::Anchor::ScreenBottom { above_rows: 1 }).modal(true),
        );
        // Click on perf's title row (row 0, somewhere on the border).
        let down = crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 65,
            row: 0,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        let hit = ui.hit_test(0, 65, None);
        let modal = ui.active_modal();
        let status = ui.dispatch_event(Event::Mouse(down), &mut |_, _, _| {});
        assert!(
            ui.chrome_drag.is_some(),
            "chrome drag should latch on perf panel even with modal open: hit={hit:?} modal={modal:?} status={status:?}"
        );
        let drag = ui.chrome_drag.unwrap();
        assert_eq!(drag.owner, overlay::ChromeOwner::Overlay(perf));
    }

    #[test]
    fn overlay_hit_test_modal_blocks_only_inside_its_rect() {
        let mut ui = make_ui();
        // Lower-z overlay covering (7,20)..(17,60).
        let under = ui.overlay_open(sized_overlay(40, 10, layout::Anchor::ScreenCenter).with_z(10));
        // Higher-z modal at same anchor, smaller (10x4 → centered (10,35)..(14,45)).
        let modal_id = ui.overlay_open(
            sized_overlay(10, 4, layout::Anchor::ScreenCenter)
                .with_z(100)
                .modal(true),
        );
        // Hit inside the modal → returns the modal (modal is opaque on
        // its own rect).
        let hit = ui.overlay_hit_test(11, 36, None).unwrap();
        assert_eq!(hit.0, modal_id);
        // Hit inside the under overlay but outside the modal - the
        // visible part of the under overlay still receives clicks. The
        // modal only blocks at its own rect, not globally.
        let outside = ui.overlay_hit_test(8, 22, None).unwrap();
        assert_eq!(outside.0, under);
    }

    fn modal_overlay_with_leaves(a: WinId, b: WinId, c: WinId) -> Overlay {
        let layout = LayoutTree::vbox(vec![
            (Constraint::Length(3), LayoutTree::leaf(a)),
            (
                Constraint::Length(3),
                LayoutTree::hbox(vec![
                    (Constraint::Length(20), LayoutTree::leaf(b)),
                    (Constraint::Length(20), LayoutTree::leaf(c)),
                ]),
            ),
        ]);
        Overlay::new(layout, layout::Anchor::ScreenCenter).modal(true)
    }

    #[test]
    fn focus_next_returns_false_outside_modal() {
        let mut ui = make_ui();
        let win = WinId(50);
        make_split(&mut ui, win);
        ui.set_focus(win);
        // No modal open → focus cycling is a no-op.
        assert!(!ui.focus_next());
        assert_eq!(ui.focus(), Some(win));
    }

    #[test]
    fn focus_next_cycles_modal_leaves() {
        let mut ui = make_ui();
        let a = WinId(100);
        let b = WinId(101);
        let c = WinId(102);
        for w in [a, b, c] {
            make_split(&mut ui, w);
        }
        ui.overlay_open(modal_overlay_with_leaves(a, b, c));
        ui.set_focus(a);
        assert!(ui.focus_next());
        assert_eq!(ui.focus(), Some(b));
        assert!(ui.focus_next());
        assert_eq!(ui.focus(), Some(c));
        // Wrap.
        assert!(ui.focus_next());
        assert_eq!(ui.focus(), Some(a));
    }

    #[test]
    fn focus_prev_walks_backwards_with_wrap() {
        let mut ui = make_ui();
        let a = WinId(100);
        let b = WinId(101);
        let c = WinId(102);
        for w in [a, b, c] {
            make_split(&mut ui, w);
        }
        ui.overlay_open(modal_overlay_with_leaves(a, b, c));
        ui.set_focus(a);
        assert!(ui.focus_prev());
        assert_eq!(ui.focus(), Some(c));
        assert!(ui.focus_prev());
        assert_eq!(ui.focus(), Some(b));
    }

    #[test]
    fn focus_next_skips_unregistered_leaves() {
        let mut ui = make_ui();
        let a = WinId(100);
        let c = WinId(102);
        // b (101) intentionally not registered.
        make_split(&mut ui, a);
        make_split(&mut ui, c);
        ui.overlay_open(modal_overlay_with_leaves(a, WinId(101), c));
        ui.set_focus(a);
        assert!(ui.focus_next());
        assert_eq!(ui.focus(), Some(c));
    }

    #[test]
    fn hit_test_returns_overlay_window_when_overlay_covers_point() {
        let mut ui = make_ui();
        register_window(&mut ui, WinId(99));
        ui.overlay_open(sized_overlay(40, 10, layout::Anchor::ScreenCenter));
        // Centered (7,20)..(17,60); (10,30) lands on the leaf.
        let hit = ui.hit_test(10, 30, None).unwrap();
        assert!(matches!(hit, HitTarget::Window(WinId(99))));
    }

    #[test]
    fn hit_test_returns_overlay_paint_when_paint_leaf_covers_point() {
        let mut ui = make_ui();
        let paint = PaintId(1u64 << 32);
        ui.overlay_open(
            Overlay::new(LayoutTree::leaf(paint), layout::Anchor::ScreenCenter).with_size((40, 10)),
        );
        let hit = ui.hit_test(10, 30, None).unwrap();
        assert_eq!(hit, HitTarget::Paint(paint));
    }

    #[test]
    fn hit_test_returns_chrome_with_overlay_owner() {
        let mut ui = make_ui();
        let id = ui.overlay_open(Overlay::new(
            LayoutTree::vbox(vec![(
                Constraint::Length(8),
                LayoutTree::hbox(vec![(Constraint::Length(40), LayoutTree::leaf(WinId(99)))]),
            )])
            .with_border(layout::Border::SINGLE),
            layout::Anchor::ScreenCenter,
        ));
        let hit = ui.hit_test(7, 30, None).unwrap();
        assert_eq!(
            hit,
            HitTarget::Chrome {
                owner: overlay::ChromeOwner::Overlay(id),
                action: overlay::ChromeAction::None
            }
        );
    }

    #[test]
    fn hit_test_returns_none_when_nothing_covers_point() {
        let ui = make_ui();
        assert_eq!(ui.hit_test(0, 0, None), None);
    }

    #[test]
    fn overlay_hit_test_topmost_wins_when_no_modal() {
        let mut ui = make_ui();
        let _bottom =
            ui.overlay_open(sized_overlay(40, 10, layout::Anchor::ScreenCenter).with_z(10));
        let top = ui.overlay_open(sized_overlay(40, 10, layout::Anchor::ScreenCenter).with_z(50));
        let hit = ui.overlay_hit_test(10, 30, None).unwrap();
        assert_eq!(hit.0, top);
    }

    #[test]
    fn resolve_overlays_returns_z_ordered_resolved_set() {
        let mut ui = make_ui();
        let high = ui.overlay_open(sized_overlay(20, 5, layout::Anchor::ScreenCenter).with_z(100));
        let low = ui.overlay_open(sized_overlay(10, 4, layout::Anchor::ScreenCenter).with_z(10));
        let resolved = ui.resolve_overlays(None);
        let ids: Vec<OverlayId> = resolved.iter().map(|(id, _, _)| *id).collect();
        assert_eq!(ids, vec![low, high]);
    }

    #[test]
    fn overlay_open_modal_focuses_first_leaf() {
        let mut ui = make_ui();
        let buf = ui.buf_create(BufCreateOpts::default());
        let win = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "t".into(),
                    gutters: Gutters::default(),
                },
            )
            .unwrap();
        let layout = LayoutTree::vbox(vec![(Constraint::Length(3), LayoutTree::leaf(win))]);
        ui.overlay_open(Overlay::new(layout, layout::Anchor::ScreenCenter).modal(true));
        assert_eq!(ui.focus(), Some(win));
    }

    #[test]
    fn set_focus_accepts_overlay_leaf() {
        let mut ui = make_ui();
        let buf = ui.buf_create(BufCreateOpts::default());
        let win = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "t".into(),
                    gutters: Gutters::default(),
                },
            )
            .unwrap();
        let layout = LayoutTree::vbox(vec![(Constraint::Length(3), LayoutTree::leaf(win))]);
        ui.overlay_open(Overlay::new(layout, layout::Anchor::ScreenCenter)); // not modal
        assert!(ui.set_focus(win));
        assert_eq!(ui.focus(), Some(win));
    }

    #[test]
    fn set_focus_accepts_focusable_splits_leaf() {
        let mut ui = make_ui();
        let buf = ui.buf_create(BufCreateOpts::default());
        let win = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "p".into(),
                    gutters: Gutters::default(),
                },
            )
            .unwrap();
        ui.set_layout(LayoutTree::vbox(vec![(
            Constraint::Fill,
            LayoutTree::leaf(win),
        )]));
        assert!(ui.set_focus(win));
        assert_eq!(ui.focus(), Some(win));
    }

    #[test]
    fn set_focus_rejects_non_focusable_splits_leaf() {
        let mut ui = make_ui();
        let buf = ui.buf_create(BufCreateOpts::default());
        let win = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "p".into(),
                    gutters: Gutters::default(),
                },
            )
            .unwrap();
        ui.win_mut(win).unwrap().set_surface(WindowSurface::inert());
        ui.set_layout(LayoutTree::vbox(vec![(
            Constraint::Fill,
            LayoutTree::leaf(win),
        )]));
        assert!(!ui.set_focus(win));
        assert_eq!(ui.focus(), None);
    }

    #[test]
    fn set_layout_drops_focus_when_focused_leaf_disappears() {
        let mut ui = make_ui();
        let buf = ui.buf_create(BufCreateOpts::default());
        let win = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "p".into(),
                    gutters: Gutters::default(),
                },
            )
            .unwrap();
        ui.set_layout(LayoutTree::vbox(vec![(
            Constraint::Fill,
            LayoutTree::leaf(win),
        )]));
        ui.set_focus(win);
        assert_eq!(ui.focus(), Some(win));
        // New tree omits the focused leaf - focus clears.
        ui.set_layout(LayoutTree::vbox(Vec::new()));
        assert_eq!(ui.focus(), None);
    }

    #[test]
    fn capture_starts_unset() {
        let ui = make_ui();
        assert_eq!(ui.capture(), None);
    }

    #[test]
    fn set_capture_then_clear_capture() {
        let mut ui = make_ui();
        let target = HitTarget::Scrollbar { owner: WinId(7) };
        ui.set_capture(target);
        assert_eq!(ui.capture(), Some(target));
        ui.clear_capture();
        assert_eq!(ui.capture(), None);
    }

    #[test]
    fn set_layout_clears_capture_when_split_owner_disappears() {
        let mut ui = make_ui();
        let buf = ui.buf_create(BufCreateOpts::default());
        let win = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "p".into(),
                    gutters: Gutters::default(),
                },
            )
            .unwrap();
        ui.set_layout(LayoutTree::vbox(vec![(
            Constraint::Fill,
            LayoutTree::leaf(win),
        )]));
        ui.set_capture(HitTarget::Scrollbar { owner: win });
        // Replacement tree omits `win` - capture must clear.
        ui.set_layout(LayoutTree::vbox(Vec::new()));
        assert_eq!(ui.capture(), None);
    }

    #[test]
    fn set_layout_keeps_capture_when_split_owner_persists() {
        let mut ui = make_ui();
        let buf = ui.buf_create(BufCreateOpts::default());
        let win = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "p".into(),
                    gutters: Gutters::default(),
                },
            )
            .unwrap();
        let tree = LayoutTree::vbox(vec![(Constraint::Fill, LayoutTree::leaf(win))]);
        ui.set_layout(tree.clone());
        let target = HitTarget::Scrollbar { owner: win };
        ui.set_capture(target);
        ui.set_layout(tree);
        assert_eq!(ui.capture(), Some(target));
    }

    #[test]
    fn overlay_close_clears_capture_for_overlay_chrome() {
        let mut ui = make_ui();
        let id = ui.overlay_open(stub_overlay());
        ui.set_capture(HitTarget::Chrome {
            owner: overlay::ChromeOwner::Overlay(id),
            action: overlay::ChromeAction::Move,
        });
        ui.overlay_close(id);
        assert_eq!(ui.capture(), None);
    }

    #[test]
    fn overlay_close_clears_capture_for_overlay_leaf() {
        let mut ui = make_ui();
        let id = ui.overlay_open(stub_overlay());
        ui.set_capture(HitTarget::Window(WinId(99)));
        ui.overlay_close(id);
        // The gesture that captured the leaf ends with the overlay
        // it lived in.
        assert_eq!(ui.capture(), None);
    }

    #[test]
    fn overlay_close_keeps_capture_for_unrelated_target() {
        let mut ui = make_ui();
        let id = ui.overlay_open(stub_overlay());
        let other = WinId(50);
        make_split(&mut ui, other);
        ui.set_capture(HitTarget::Scrollbar { owner: other });
        ui.overlay_close(id);
        assert_eq!(ui.capture(), Some(HitTarget::Scrollbar { owner: other }));
    }

    #[test]
    fn handle_key_routes_to_overlay_leaf_callback() {
        let mut ui = make_ui();
        let buf = ui.buf_create(BufCreateOpts::default());
        let win = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "t".into(),
                    gutters: Gutters::default(),
                },
            )
            .unwrap();
        let layout = LayoutTree::vbox(vec![(Constraint::Length(3), LayoutTree::leaf(win))]);
        let oid = ui.overlay_open(Overlay::new(layout, layout::Anchor::ScreenCenter).modal(true));
        let cb: Callback = Callback::Rust(Box::new(move |ctx| {
            if let Some(o) = ctx.ui.overlay_for_leaf(ctx.win) {
                let _ = ctx.ui.overlay_close(o);
            }
            CallbackResult::Consumed
        }));
        let _ = ui.win_set_keymap(
            win,
            KeyBind::new(
                crossterm::event::KeyCode::Char('q'),
                crossterm::event::KeyModifiers::NONE,
            ),
            cb,
        );
        let result = dispatch_key(
            &mut ui,
            crossterm::event::KeyCode::Char('q'),
            crossterm::event::KeyModifiers::NONE,
        );
        assert_eq!(result, Status::Consumed);
        assert!(ui.overlay(oid).is_none());
    }

    #[test]
    fn callback_result_event_dispatches_winevent_after_keymap() {
        // A built-in keymap callback (e.g. a list's Enter binding)
        // returns `CallbackResult::Event(Submit, payload)`. The
        // dispatcher must follow up with `fire_win_event` so any
        // registered `on_event(win, "submit", ...)` handler fires.
        let mut ui = make_ui();
        let buf = ui.buf_create(BufCreateOpts::default());
        let win = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "list".into(),
                    gutters: Gutters::default(),
                },
            )
            .unwrap();
        // Wrap the win in a modal overlay so it becomes a focusable
        // leaf reachable via `set_focus`.
        let layout = LayoutTree::vbox(vec![(Constraint::Length(3), LayoutTree::leaf(win))]);
        ui.overlay_open(Overlay::new(layout, layout::Anchor::ScreenCenter).modal(true));

        let submit_cb: Callback = Callback::Rust(Box::new(|_| {
            CallbackResult::Event(WinEvent::Submit, Payload::Selection { index: 7 })
        }));
        let _ = ui.win_set_keymap(
            win,
            KeyBind::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::NONE,
            ),
            submit_cb,
        );

        let observed = std::sync::Arc::new(std::sync::Mutex::new(Vec::<usize>::new()));
        let observed_cb = observed.clone();
        ui.win_on_event(
            win,
            WinEvent::Submit,
            Callback::Rust(Box::new(move |ctx| {
                if let Payload::Selection { index } = ctx.payload {
                    observed_cb.lock().unwrap().push(index);
                }
                CallbackResult::Consumed
            })),
        );

        let result = dispatch_key(
            &mut ui,
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        );
        assert_eq!(result, Status::Consumed);
        assert_eq!(*observed.lock().unwrap(), vec![7]);
    }

    #[test]
    fn dispatch_event_esc_closes_active_modal() {
        let mut ui = make_ui();
        let id = ui.overlay_open(modal_overlay_with_leaves(WinId(50), WinId(51), WinId(52)));
        assert_eq!(ui.active_modal_overlay(), Some(id));
        let result = dispatch_key(
            &mut ui,
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        );
        assert_eq!(result, Status::Consumed);
        assert_eq!(ui.active_modal(), None);
    }

    #[test]
    fn dispatch_event_esc_with_modifiers_does_not_dismiss_modal() {
        let mut ui = make_ui();
        let id = ui.overlay_open(modal_overlay_with_leaves(WinId(50), WinId(51), WinId(52)));
        // Esc + Shift falls through to normal dispatch - built-in
        // dismiss is bare Esc only.
        let _ = dispatch_key(
            &mut ui,
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::SHIFT,
        );
        assert_eq!(ui.active_modal_overlay(), Some(id));
    }

    #[test]
    fn modal_esc_fires_dismiss_once_on_overlay_root() {
        // Multi-panel overlay: dialog.lua registers
        // `on_event("dismiss", …)` on the dialog's root WinId (the
        // first leaf in declaration order, returned from `_open`).
        // Esc must fire Dismiss exactly once on the root - not
        // once per leaf - so dialog.lua's single handler runs once
        // and the parked task resumes once. Non-root leaves with
        // their own Dismiss callbacks are addressed via root
        // redirect inside `fire_win_event`.
        let mut ui = make_ui();
        let a = WinId(60);
        let b = WinId(61);
        let c = WinId(62);
        let id = ui.overlay_open(modal_overlay_with_leaves(a, b, c));
        let count = std::sync::Arc::new(std::sync::Mutex::new(0u32));
        // Only the root (a) gets a callback - like dialog.lua does.
        let count_cb = count.clone();
        ui.win_on_event(
            a,
            WinEvent::Dismiss,
            Callback::Rust(Box::new(move |_| {
                *count_cb.lock().unwrap() += 1;
                CallbackResult::Consumed
            })),
        );
        let result = dispatch_key(
            &mut ui,
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        );
        assert_eq!(result, Status::Consumed);
        assert_eq!(*count.lock().unwrap(), 1);
        assert!(ui.overlay(id).is_none());
    }

    #[test]
    fn dispatch_event_ctrl_c_closes_active_modal() {
        let mut ui = make_ui();
        let id = ui.overlay_open(modal_overlay_with_leaves(WinId(50), WinId(51), WinId(52)));
        assert_eq!(ui.active_modal_overlay(), Some(id));
        let result = dispatch_key(
            &mut ui,
            crossterm::event::KeyCode::Char('c'),
            crossterm::event::KeyModifiers::CONTROL,
        );
        assert_eq!(result, Status::Consumed);
        assert_eq!(ui.active_modal(), None);
    }

    #[test]
    fn modal_ctrl_c_fires_dismiss_once_on_overlay_root() {
        let mut ui = make_ui();
        let a = WinId(60);
        let b = WinId(61);
        let c = WinId(62);
        let id = ui.overlay_open(modal_overlay_with_leaves(a, b, c));
        let count = std::sync::Arc::new(std::sync::Mutex::new(0u32));
        let count_cb = count.clone();
        ui.win_on_event(
            a,
            WinEvent::Dismiss,
            Callback::Rust(Box::new(move |_| {
                *count_cb.lock().unwrap() += 1;
                CallbackResult::Consumed
            })),
        );
        let result = dispatch_key(
            &mut ui,
            crossterm::event::KeyCode::Char('c'),
            crossterm::event::KeyModifiers::CONTROL,
        );
        assert_eq!(result, Status::Consumed);
        assert_eq!(*count.lock().unwrap(), 1);
        assert!(ui.overlay(id).is_none());
    }

    #[test]
    fn modal_esc_consumed_by_focused_leaf_does_not_dismiss() {
        // Esc chain: the focused window gets first dibs. If its
        // keymap consumes Esc, the modal stays open.
        let mut ui = make_ui();
        let a = WinId(60);
        let b = WinId(61);
        let id = ui.overlay_open(modal_overlay_with_leaves(a, b, WinId(62)));
        ui.set_focus(a);

        let esc_consumed = std::sync::Arc::new(std::sync::Mutex::new(false));
        let esc_cb = esc_consumed.clone();
        let cb: Callback = Callback::Rust(Box::new(move |_| {
            *esc_cb.lock().unwrap() = true;
            CallbackResult::Consumed
        }));
        let _ = ui.win_set_keymap(
            a,
            KeyBind::new(
                crossterm::event::KeyCode::Esc,
                crossterm::event::KeyModifiers::NONE,
            ),
            cb,
        );

        let result = dispatch_key(
            &mut ui,
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        );
        assert_eq!(result, Status::Consumed);
        assert!(*esc_consumed.lock().unwrap());
        // Modal stays open because the leaf consumed Esc.
        assert!(ui.overlay(id).is_some());
    }

    #[test]
    fn dispatch_overlay_key_fires_when_focus_in_overlay() {
        // Overlay-scoped keymap fires when the focused leaf belongs to that
        // overlay, even though the leaf itself has no specific keymap.
        let mut ui = make_ui();
        let a = WinId(70);
        let id = ui.overlay_open(modal_overlay_with_leaves(a, WinId(71), WinId(72)));
        ui.set_focus(a);

        let fired = std::sync::Arc::new(std::sync::Mutex::new(false));
        let fired_cb = fired.clone();
        let cb: Callback = Callback::Rust(Box::new(move |_| {
            *fired_cb.lock().unwrap() = true;
            CallbackResult::Consumed
        }));
        let _ = ui.overlay_set_keymap(id, KeyBind::plain(crossterm::event::KeyCode::Tab), cb);

        let result = ui.dispatch_overlay_key(
            crossterm::event::KeyCode::Tab,
            crossterm::event::KeyModifiers::NONE,
            &mut |_, _, _| {},
        );
        assert_eq!(result, Status::Consumed);
        assert!(*fired.lock().unwrap());
    }

    #[test]
    fn dispatch_overlay_key_ignored_when_focus_outside_overlay() {
        // A non-modal overlay keymap stays inert if the focused leaf is in a
        // different overlay (or none): the cascade routes through
        // `overlay_for_leaf`.
        let mut ui = make_ui();
        let outside = WinId(80);
        make_split(&mut ui, outside);
        ui.set_focus(outside);
        let overlay_only_leaf = WinId(81);
        let id = ui.overlay_open(
            modal_overlay_with_leaves(overlay_only_leaf, WinId(82), WinId(83)).modal(false),
        );
        ui.set_focus(outside);

        let fired = std::sync::Arc::new(std::sync::Mutex::new(false));
        let fired_cb = fired.clone();
        let cb: Callback = Callback::Rust(Box::new(move |_| {
            *fired_cb.lock().unwrap() = true;
            CallbackResult::Consumed
        }));
        let _ = ui.overlay_set_keymap(id, KeyBind::plain(crossterm::event::KeyCode::Tab), cb);

        let result = ui.dispatch_overlay_key(
            crossterm::event::KeyCode::Tab,
            crossterm::event::KeyModifiers::NONE,
            &mut |_, _, _| {},
        );
        assert_eq!(result, Status::Ignored);
        assert!(!*fired.lock().unwrap());
    }

    #[test]
    fn overlay_keymaps_reaped_on_overlay_close() {
        // Closing an overlay must clear its overlay-scoped keymaps so a
        // subsequent overlay reusing the same id (or a stale dispatch) can't
        // hit a freed Lua callback.
        let mut ui = make_ui();
        let a = WinId(90);
        let id = ui.overlay_open(modal_overlay_with_leaves(a, WinId(91), WinId(92)));
        ui.set_focus(a);

        let _ = ui.overlay_set_keymap(
            id,
            KeyBind::plain(crossterm::event::KeyCode::Tab),
            Callback::Lua(LuaHandle(999)),
        );

        // Closing the focused leaf cascades to overlay close + cleanup.
        let lua_ids = ui.win_close(a);
        assert!(
            lua_ids.contains(&999),
            "expected reaped lua ids: {lua_ids:?}"
        );
    }

    #[test]
    fn fire_win_event_fires_on_leaf_not_root() {
        // Events fire on the leaf they're directed at. Consumers that need to catch
        // events from multiple panels register on each leaf themselves.
        let mut ui = make_ui();
        let a = WinId(70);
        let b = WinId(71);
        let _id = ui.overlay_open(modal_overlay_with_leaves(a, b, WinId(72)));
        let saw_a = std::sync::Arc::new(std::sync::Mutex::new(false));
        let saw_b = std::sync::Arc::new(std::sync::Mutex::new(false));
        {
            let saw_cb = saw_a.clone();
            ui.win_on_event(
                a,
                WinEvent::Submit,
                Callback::Rust(Box::new(move |_| {
                    *saw_cb.lock().unwrap() = true;
                    CallbackResult::Consumed
                })),
            );
        }
        {
            let saw_cb = saw_b.clone();
            ui.win_on_event(
                b,
                WinEvent::Submit,
                Callback::Rust(Box::new(move |_| {
                    *saw_cb.lock().unwrap() = true;
                    CallbackResult::Consumed
                })),
            );
        }
        ui.fire_win_event(b, WinEvent::Submit, Payload::None, &mut |_, _, _| {});
        assert!(!*saw_a.lock().unwrap(), "root handler must not fire");
        assert!(*saw_b.lock().unwrap(), "leaf handler must fire");
    }

    #[test]
    fn win_close_on_overlay_leaf_closes_overlay_and_clears_all_leaves() {
        // Lua flow: `smelt.win.close(win_id)` is the canonical way for
        // a dialog to dismiss itself. When `win_id` is a leaf of an
        // open overlay the call must close the whole overlay (not just
        // detach one panel) and clear callbacks for every leaf so the
        // Lua-side registry drops them in lockstep.
        let mut ui = make_ui();
        let buf = ui.buf_create(BufCreateOpts::default());
        let win_a = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "a".into(),
                    gutters: Gutters::default(),
                },
            )
            .unwrap();
        let win_b = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "b".into(),
                    gutters: Gutters::default(),
                },
            )
            .unwrap();
        let layout = LayoutTree::vbox(vec![
            (Constraint::Length(3), LayoutTree::leaf(win_a)),
            (Constraint::Length(3), LayoutTree::leaf(win_b)),
        ]);
        let oid = ui.overlay_open(Overlay::new(layout, layout::Anchor::ScreenCenter).modal(true));
        let cb_noop: Callback = Callback::Rust(Box::new(|_| CallbackResult::Consumed));
        ui.win_on_event(win_a, WinEvent::Dismiss, cb_noop);
        let cb_noop2: Callback = Callback::Rust(Box::new(|_| CallbackResult::Consumed));
        ui.win_on_event(win_b, WinEvent::Dismiss, cb_noop2);

        let _ = ui.win_close(win_a);

        assert!(ui.overlay(oid).is_none());
        // Both leaves' Window entries gone from the registry.
        assert!(ui.win(win_a).is_none());
        assert!(ui.win(win_b).is_none());
        // Closing again is a no-op - overlay is already gone.
        assert_eq!(ui.win_close(win_a), Vec::<u64>::new());
    }

    #[test]
    fn close_drops_named_bindings_for_win_buf_and_overlay() {
        // Regression: when a Lua plugin closes a named overlay (or its
        // win, or buf), the name slot must be dropped so the next
        // `smelt.win.new({name=...})` allocates a fresh id instead of
        // returning a stale one that no longer maps to anything.
        let mut ui = make_ui();
        let buf = ui.buf_create(BufCreateOpts::default());
        ui.name_buf("plug.buf", buf);
        let win = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "r".into(),
                    gutters: Gutters::default(),
                },
            )
            .unwrap();
        ui.name_win("plug.win", win);
        let layout = LayoutTree::leaf(win);
        let oid = ui.overlay_open(Overlay::new(layout, layout::Anchor::ScreenCenter));
        ui.name_overlay("plug.overlay", oid);

        let _ = ui.win_close(win);

        assert_eq!(ui.named_win("plug.win"), None);
        assert_eq!(ui.named_overlay("plug.overlay"), None);
        // Named buf survives close (buf lifetime is independent of its windows).
        assert_eq!(ui.named_buf("plug.buf"), Some(buf));

        ui.buf_destroy(buf);
        assert_eq!(ui.named_buf("plug.buf"), None);
    }

    #[test]
    fn render_drives_ensure_rendered_at_for_each_overlay_leaf() {
        // Plain / markdown / diff parsers populate the buffer's lines
        // lazily on `ensure_rendered_at(width)`. The overlay paint walk
        // takes immutable references and can't drive the parser, so
        // `Ui::render` must do a pre-pass that calls
        // `ensure_rendered_at` for each leaf at the leaf's resolved
        // width before paint.
        use std::sync::{Arc, Mutex};
        struct WidthRecorder {
            calls: Arc<Mutex<Vec<u16>>>,
        }
        impl BufferParser for WidthRecorder {
            fn parse(&self, buf: &mut Buffer, _source: &str, width: u16) {
                self.calls.lock().unwrap().push(width);
                buf.set_all_lines(vec![format!("rendered@{width}")]);
            }
        }
        let mut ui = make_ui();
        let buf = ui.buf_create(BufCreateOpts::default());
        let calls = Arc::new(Mutex::new(Vec::<u16>::new()));
        if let Some(b) = ui.buf_mut(buf) {
            b.set_parser(Arc::new(WidthRecorder {
                calls: calls.clone(),
            }));
            b.set_source("seed".into());
        }
        let win = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "test".into(),
                    gutters: Gutters::default(),
                },
            )
            .unwrap();
        let layout = LayoutTree::vbox(vec![(
            Constraint::Length(3),
            LayoutTree::hbox(vec![(Constraint::Length(40), LayoutTree::leaf(win))]),
        )])
        .with_border(Border::SINGLE);
        ui.overlay_open(Overlay::new(layout, layout::Anchor::ScreenCenter));
        let mut out = Vec::new();
        ui.render(&mut out).unwrap();
        // Outer overlay: 42×5 (40 leaf width + 2 border, 3 leaf height + 2
        // border). Leaf rect width = 40, minus the 1-col scrollbar reservation
        // from `Gutters::default()` ⇒ parser called with 39.
        let widths = calls.lock().unwrap().clone();
        assert!(
            widths.contains(&39),
            "parser must be invoked at the leaf's content width; got {widths:?}"
        );
    }

    #[test]
    fn overlay_leaf_with_overflowing_content_auto_attaches_scrollbar() {
        // Verifies the post-render auto-attach path: an overlay leaf opened with
        // the default `Gutters::default()` (scrollbar = true) and a buffer whose
        // line count exceeds the leaf height must end up with a populated
        // `viewport.scrollbar`. This is the contract the wheel-routing and
        // paint_scrollbar paths rely on for /stats, /help, /messages.
        let mut ui = make_ui(); // 80x24 terminal
        let buf = ui.buf_create(BufCreateOpts::default());
        if let Some(b) = ui.buf_mut(buf) {
            let lines: Vec<String> = (0..60).map(|i| format!("line {i}")).collect();
            b.set_all_lines(lines);
        }
        let win = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "test".into(),
                    gutters: Gutters::default(),
                },
            )
            .unwrap();
        // A 40-wide × 10-tall content area inside a single border ⇒ 42×12 overlay.
        let layout = LayoutTree::vbox(vec![(
            Constraint::Length(10),
            LayoutTree::hbox(vec![(Constraint::Length(40), LayoutTree::leaf(win))]),
        )])
        .with_border(Border::SINGLE);
        ui.overlay_open(Overlay::new(layout, layout::Anchor::ScreenCenter));
        let mut out = Vec::new();
        ui.render(&mut out).unwrap();
        let vp = ui
            .wins
            .get(&win)
            .and_then(|w| w.viewport)
            .expect("viewport populated by pre-pass");
        assert_eq!(vp.total_rows, 60, "total_rows must equal buf.line_count()");
        assert_eq!(vp.rect.height, 10, "leaf height = layout-resolved height");
        let bar = vp.scrollbar.expect(
            "scrollbar must auto-attach: gutters.scrollbar=true and total_rows>viewport_rows",
        );
        assert_eq!(bar.total_rows, 60);
        assert_eq!(bar.viewport_rows, 10);
        // Bar column = leaf_rect.right - 1. Overlay centered: term 80, ov w=42
        // ⇒ left=19, leaf left=20, leaf width=40 ⇒ bar_col=59.
        assert_eq!(bar.col, 59, "scrollbar lives at rightmost column of leaf");
    }

    #[test]
    fn scrollbar_pointer_fraction_uses_frozen_drag_geometry() {
        let mut ui = make_ui();
        let win = WinId(42);
        register_window(&mut ui, win);
        let original_bar = ScrollbarState::new(39, 100, 20).expect("overflowing scrollbar");
        ui.win_mut(win).unwrap().viewport = Some(WindowViewport::new(
            Rect::new(10, 0, 40, 20),
            39,
            100,
            0,
            Some(original_bar),
        ));

        ui.start_scrollbar_drag(win, 10);
        let refined_bar = ScrollbarState::new(39, 1_000, 8).expect("overflowing scrollbar");
        ui.win_mut(win).unwrap().viewport = Some(WindowViewport::new(
            Rect::new(2, 0, 40, 8),
            39,
            1_000,
            0,
            Some(refined_bar),
        ));

        assert_eq!(
            ui.scrollbar_pointer_fraction(win, 18),
            Some((8, 16, 100, 20)),
            "an active drag must retain its pointer-down track and extent",
        );
    }

    #[test]
    fn virtual_scrollbar_drag_captures_without_mutating_committed_scroll() {
        let mut ui = make_ui();
        let win = WinId(42);
        register_window(&mut ui, win);
        let buf = ui.win(win).unwrap().buf;
        ui.buf_mut(buf)
            .unwrap()
            .set_all_lines((0..1_000).map(|row| format!("row {row}")).collect());
        ui.set_layout(LayoutTree::leaf(win));
        ui.render(&mut Vec::new()).unwrap();
        ui.win_mut(win).unwrap().set_resolved_scroll(200);
        let viewport = ui.win(win).unwrap().viewport.expect("window viewport");
        let bar = viewport.scrollbar.expect("overflowing scrollbar");
        let start_row = viewport.rect.top.saturating_add(bar.metrics(200).thumb_top);
        let drag_row = viewport.rect.top.saturating_add(15);

        let event = |kind, row| crossterm::event::MouseEvent {
            kind,
            row,
            column: bar.col,
            modifiers: crossterm::event::KeyModifiers::empty(),
        };
        assert_eq!(
            ui.dispatch_virtual_scrollbar_event(event(
                crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left,),
                start_row,
            )),
            Status::Consumed
        );
        assert_eq!(
            ui.dispatch_virtual_scrollbar_event(event(
                crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left,),
                drag_row,
            )),
            Status::Consumed
        );
        assert_eq!(ui.win(win).unwrap().scroll_top(), 200);
        assert!(matches!(
            ui.capture(),
            Some(HitTarget::Scrollbar { owner }) if owner == win
        ));
        assert_eq!(
            ui.dispatch_virtual_scrollbar_event(event(
                crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
                drag_row,
            )),
            Status::Consumed
        );
        assert!(ui.capture().is_none());
    }

    #[test]
    fn wheel_over_overflowing_overlay_leaf_pans_scroll_top() {
        // Real-world regression for /stats, /help, /messages: a mouse wheel
        // event over a focused overlay leaf with overflowing content must
        // update `scroll_top` (so wheel-scroll feels live).
        let mut ui = make_ui();
        let buf = ui.buf_create(BufCreateOpts::default());
        if let Some(b) = ui.buf_mut(buf) {
            let lines: Vec<String> = (0..60).map(|i| format!("line {i}")).collect();
            b.set_all_lines(lines);
        }
        let win = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "test".into(),
                    gutters: Gutters::default(),
                },
            )
            .unwrap();
        if let Some(w) = ui.win_mut(win) {
            w.set_surface(WindowSurface::editable_text());
        }
        let layout = LayoutTree::vbox(vec![(
            Constraint::Length(10),
            LayoutTree::hbox(vec![(Constraint::Length(40), LayoutTree::leaf(win))]),
        )])
        .with_border(Border::SINGLE);
        let ov = Overlay::new(layout, layout::Anchor::ScreenCenter);
        let ov = Overlay { modal: true, ..ov };
        ui.overlay_open(ov);
        // Render once so the pre-pass populates the viewport before wheel routing
        // hit-tests against it.
        let mut out = Vec::new();
        ui.render(&mut out).unwrap();
        assert_eq!(ui.focus(), Some(win), "modal focused first leaf");
        assert_eq!(ui.win(win).unwrap().scroll_top(), 0, "starts at top");
        // Wheel-down over a cell inside the leaf rect (centered overlay puts
        // the leaf at left=20..60, top=7..17 on a 80x24 terminal).
        let scroll = mouse_event(crossterm::event::MouseEventKind::ScrollDown, 10, 30);
        let status = ui.dispatch_event(scroll, &mut |_, _, _| {});
        assert_eq!(status, Status::Consumed);
        assert!(
            ui.win(win).unwrap().scroll_top() > 0,
            "wheel must advance scroll_top via pan_by_lines"
        );
    }

    #[test]
    fn hit_test_returns_scrollbar_for_overlay_bar_column() {
        // Regression: previously hit_test short-circuited to HitTarget::Window for any
        // overlay position, making the auto-attached overlay scrollbar undraggable.
        // After the fix, the rightmost column of an overflowing overlay leaf must
        // return HitTarget::Scrollbar so the drag-capture path fires.
        let mut ui = make_ui();
        let buf = ui.buf_create(BufCreateOpts::default());
        if let Some(b) = ui.buf_mut(buf) {
            let lines: Vec<String> = (0..60).map(|i| format!("line {i}")).collect();
            b.set_all_lines(lines);
        }
        let win = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "test".into(),
                    gutters: Gutters::default(),
                },
            )
            .unwrap();
        let layout = LayoutTree::vbox(vec![(
            Constraint::Length(10),
            LayoutTree::hbox(vec![(Constraint::Length(40), LayoutTree::leaf(win))]),
        )])
        .with_border(Border::SINGLE);
        ui.overlay_open(Overlay::new(layout, layout::Anchor::ScreenCenter));
        let mut out = Vec::new();
        ui.render(&mut out).unwrap();
        // bar_col = 59 (see overlay_leaf_with_overflowing_content_auto_attaches_scrollbar).
        let hit = ui
            .hit_test(10, 59, None)
            .expect("rightmost col hits something");
        assert!(
            matches!(hit, HitTarget::Scrollbar { owner } if owner == win),
            "rightmost column of overflowing overlay leaf must be Scrollbar, got {hit:?}",
        );
        // Adjacent column to the left is still Window.
        let hit = ui.hit_test(10, 58, None).expect("interior col hits window");
        assert!(
            matches!(hit, HitTarget::Window(w) if w == win),
            "interior column must still be Window, got {hit:?}",
        );
    }

    #[test]
    fn prime_overlay_viewports_sets_leaf_geometry_before_render() {
        let mut ui = make_ui();
        let buf = ui.buf_create(BufCreateOpts::default());
        if let Some(b) = ui.buf_mut(buf) {
            b.set_all_lines(vec!["overlay-text".into()]);
        }
        let win = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "test".into(),
                    gutters: Gutters::default(),
                },
            )
            .unwrap();
        let layout = LayoutTree::vbox(vec![(
            Constraint::Length(10),
            LayoutTree::hbox(vec![(Constraint::Length(40), LayoutTree::leaf(win))]),
        )])
        .with_border(Border::SINGLE);
        ui.overlay_open(Overlay::new(layout, layout::Anchor::ScreenCenter));

        assert!(ui.wins.get(&win).and_then(|w| w.viewport).is_none());
        ui.prime_overlay_viewports();

        let vp = ui
            .wins
            .get(&win)
            .and_then(|w| w.viewport)
            .expect("viewport populated before render");
        assert_eq!(vp.rect, Rect::new(7, 20, 40, 10));
        assert_eq!(vp.content_width, 39);
    }

    #[test]
    fn overlay_leaf_without_overflow_has_no_scrollbar() {
        // Companion to the above: when content fits, ScrollbarState::new returns None.
        let mut ui = make_ui();
        let buf = ui.buf_create(BufCreateOpts::default());
        if let Some(b) = ui.buf_mut(buf) {
            b.set_all_lines(vec!["short".into()]);
        }
        let win = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "test".into(),
                    gutters: Gutters::default(),
                },
            )
            .unwrap();
        let layout = LayoutTree::vbox(vec![(
            Constraint::Length(10),
            LayoutTree::hbox(vec![(Constraint::Length(40), LayoutTree::leaf(win))]),
        )])
        .with_border(Border::SINGLE);
        ui.overlay_open(Overlay::new(layout, layout::Anchor::ScreenCenter));
        let mut out = Vec::new();
        ui.render(&mut out).unwrap();
        let vp = ui
            .wins
            .get(&win)
            .and_then(|w| w.viewport)
            .expect("viewport populated");
        assert!(
            vp.scrollbar.is_none(),
            "no overflow → ScrollbarState::new returns None"
        );
    }

    #[test]
    fn render_paints_overlay_leaf_buffer() {
        let mut ui = make_ui();
        let buf = ui.buf_create(BufCreateOpts::default());
        if let Some(b) = ui.buf_mut(buf) {
            b.set_all_lines(vec!["overlay-text".into()]);
        }
        let win = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "test".into(),
                    gutters: Gutters::default(),
                },
            )
            .unwrap();
        let layout = LayoutTree::vbox(vec![(
            Constraint::Length(3),
            LayoutTree::hbox(vec![(Constraint::Length(40), LayoutTree::leaf(win))]),
        )])
        .with_border(Border::SINGLE)
        .with_title("title");
        ui.overlay_open(Overlay::new(layout, layout::Anchor::ScreenCenter));
        let mut out = Vec::new();
        ui.render(&mut out).unwrap();
        // Borrow Compositor's previous grid (post-flush swap) for assertions.
        let frame = ui.surface.compositor().previous();
        // Centered (term 80x24, overlay natural 42 wide × 5 tall →
        // top=9 left=19). Title sits in the top border row at col=20.
        assert_eq!(frame.cell(19, 9).symbol, '┌');
        assert_eq!(frame.cell(20, 9).symbol, 't');
        assert_eq!(frame.cell(24, 9).symbol, 'e');
        // Leaf paints inside the border at (top+1, left+1) = (10, 20).
        assert_eq!(frame.cell(20, 10).symbol, 'o');
        assert_eq!(frame.cell(31, 10).symbol, 't');
    }

    // ── UiHost trait dispatch ────────────────────────────────────────

    #[test]
    fn ui_host_dispatch_round_trips_through_dyn() {
        // Drive every UiHost method through `&mut dyn UiHost` so the
        // trait shape is exercised end-to-end (not just the inherent
        // path). Mirrors how Lua bindings reach the compositor - by
        // trait, not by direct field access.
        fn drive(host: &mut dyn UiHost) -> (BufId, WinId, OverlayId) {
            let buf = host.buf_create(BufCreateOpts::default());
            host.buf_mut(buf)
                .unwrap()
                .set_all_lines(vec!["uihost".into()]);
            let win = host
                .win_open_split(
                    buf,
                    SplitConfig {
                        region: "uihost-test".into(),
                        gutters: Gutters::default(),
                    },
                )
                .unwrap();
            {
                let w = host.win_mut(win).unwrap();
                w.text_state_mut().cursor_row = 0;
                w.text_state_mut().cursor_col = 3;
            }
            // Hosting `win` in a modal overlay both makes it focusable
            // (overlay leaf) and exercises `overlay_open`. The modal
            // also auto-focuses the first leaf - re-asserting via the
            // explicit `set_focus` keeps that method on the trait path.
            let layout = LayoutTree::vbox(vec![(Constraint::Fill, LayoutTree::leaf(win))]);
            let oid =
                host.overlay_open(Overlay::new(layout, layout::Anchor::ScreenCenter).modal(true));
            assert!(host.set_focus(win));
            // `ui()` must yield the same compositor every other method
            // mutates; assert the focused window matches what we just set.
            assert_eq!(host.ui().focus(), Some(win));
            (buf, win, oid)
        }

        let mut ui = make_ui();
        let (buf, win, oid) = drive(&mut ui);

        // Fire-event path through the trait. The callback observes the
        // payload that the trait dispatch threaded through.
        let saw = std::sync::Arc::new(std::sync::Mutex::new(false));
        let saw_cb = saw.clone();
        ui.win_on_event(
            win,
            WinEvent::TextChanged,
            Callback::Rust(Box::new(move |_| {
                *saw_cb.lock().unwrap() = true;
                CallbackResult::Consumed
            })),
        );
        ui.fire_win_event(
            win,
            WinEvent::TextChanged,
            Payload::Text {
                content: "uihost".into(),
            },
            &mut |_, _, _| {},
        );
        assert!(*saw.lock().unwrap());

        // Close paths through the trait clean up the structures the
        // open paths created.
        let removed = ui.overlay_close(oid);
        assert!(removed.is_some());
        let cb_ids = UiHost::win_close(&mut ui, win);
        assert!(cb_ids.is_empty());
        assert!(ui.buf(buf).is_some());
    }

    // ── UiHost per-pane data accessors ───────────────────────────────

    #[test]
    fn hard_break_helpers_return_newline_byte_offsets() {
        assert_eq!(hard_breaks_for_text("first\nsecond\nthird"), vec![5, 12]);
        assert_eq!(hard_breaks_for_text("é\nz"), vec![2]);
        assert_eq!(
            hard_breaks_for_lines(&["first".into(), "second".into(), "third".into()]),
            vec![5, 12]
        );
    }

    #[test]
    fn ui_host_per_pane_data_default_impl() {
        // Ui's default display-row / rows / breaks / viewport accessors cover
        // any window the host hasn't overridden - buffer lines as rows,
        // join positions as hard breaks, no soft wraps. Drives them through
        // `&mut dyn UiHost` so the trait shape is exercised end-to-end.
        let mut ui = make_ui();
        let buf = ui.buf_create(BufCreateOpts::default());
        ui.buf_mut(buf)
            .unwrap()
            .set_all_lines(vec!["hello".into(), "world!".into(), "ok".into()]);
        let win = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "per-pane-default".into(),
                    gutters: Gutters::default(),
                },
            )
            .unwrap();
        let rect = layout::Rect::new(0, 0, 20, 10);
        ui.win_mut(win).unwrap().viewport = Some(window::WindowViewport::new(rect, 20, 0, 0, None));

        fn assert_default_shape(ui: &Ui, win: WinId) {
            let vp = UiHost::viewport_for(ui, win).unwrap();
            assert_eq!(vp.rect.width, 20);
            let display_rows = display_rows_for_ui_range(ui, win, 1, 2).unwrap();
            let display_text: Vec<_> = display_rows
                .rows
                .iter()
                .map(|row| row.text.as_str())
                .collect();
            assert_eq!(display_text, vec!["world!", "ok"]);
            assert!(display_rows.soft_breaks().is_empty());
            assert_eq!(display_rows.hard_breaks(), vec![6]);
            // "world!\nok" - the join between the two ranged rows lives at
            // byte 6 and is a hard break for an unwrapped buffer.
        }
        assert_default_shape(&ui, win);

        // Unknown window → `None` for viewport and bounded display access.
        let stranger = WinId(9999);
        assert!(UiHost::viewport_for(&ui, stranger).is_none());
        assert!(display_rows_for_ui_range(&ui, stranger, 0, 1).is_none());
    }

    #[test]
    fn record_click_caps_at_three_then_wraps() {
        let mut ui = make_ui();
        let t0 = std::time::Instant::now();
        // Same cell, no time gap → climbs to 3, then wraps.
        assert_eq!(ui.record_click(5, 7, t0), 1);
        assert_eq!(ui.record_click(5, 7, t0), 2);
        assert_eq!(ui.record_click(5, 7, t0), 3);
        assert_eq!(ui.record_click(5, 7, t0), 1);
        // Different cell resets the count.
        assert_eq!(ui.record_click(5, 7, t0), 2);
        assert_eq!(ui.record_click(8, 7, t0), 1);
    }

    /// Two clicks on the same cell more than 400ms apart count as fresh
    /// singles, not a double-click. Exercising the `now` parameter directly
    /// proves the gap check reads the host clock rather than `Instant::now`.
    #[test]
    fn record_click_resets_after_400ms_gap() {
        let mut ui = make_ui();
        let t0 = std::time::Instant::now();
        assert_eq!(ui.record_click(5, 7, t0), 1);
        let later = t0 + std::time::Duration::from_millis(401);
        assert_eq!(ui.record_click(5, 7, later), 1);
        // A follow-up within 400ms of `later` still pairs with it.
        let still_near = later + std::time::Duration::from_millis(100);
        assert_eq!(ui.record_click(5, 7, still_near), 2);
    }

    /// Set up a single splits leaf at `(0, 0, 20, 10)` with a painted
    /// scrollbar at column 19 covering 100 rows of content. Returns
    /// the leaf's `WinId` so callers can latch capture / hit-test
    /// against it.
    fn make_scrollbar_split(ui: &mut Ui) -> WinId {
        let buf = ui.buf_create(BufCreateOpts::default());
        if let Some(b) = ui.buf_mut(buf) {
            let lines: Vec<String> = (0..100).map(|i| format!("line {i}")).collect();
            b.set_all_lines(lines);
        }
        let win = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "p".into(),
                    gutters: Gutters::default(),
                },
            )
            .unwrap();
        ui.set_layout(LayoutTree::vbox(vec![(
            Constraint::Fill,
            LayoutTree::leaf(win),
        )]));
        let rect = layout::Rect::new(0, 0, 20, 10);
        let bar = window::ScrollbarState::new(19, 100, 10).unwrap();
        let w = ui.win_mut(win).unwrap();
        w.viewport = Some(window::WindowViewport::new(rect, 19, 100, 0, Some(bar)));
        win
    }

    fn mouse_event(kind: crossterm::event::MouseEventKind, row: u16, col: u16) -> Event {
        Event::Mouse(crossterm::event::MouseEvent {
            kind,
            row,
            column: col,
            modifiers: crossterm::event::KeyModifiers::NONE,
        })
    }

    #[test]
    fn hit_test_returns_scrollbar_for_splits_leaf_with_painted_bar() {
        let mut ui = make_ui();
        let win = make_scrollbar_split(&mut ui);
        // Column 19 is the scrollbar column; rows 0..10 are inside the
        // viewport.
        assert_eq!(
            ui.hit_test(3, 19, None),
            Some(HitTarget::Scrollbar { owner: win })
        );
        // Same row, column 18 → content, not scrollbar.
        assert_eq!(ui.hit_test(3, 18, None), Some(HitTarget::Window(win)));
    }

    #[test]
    fn dispatch_mouse_left_down_on_scrollbar_latches_capture_and_snaps_scroll() {
        let mut ui = make_ui();
        let win = make_scrollbar_split(&mut ui);
        let down = mouse_event(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            9,
            19,
        );
        let status = ui.dispatch_event(down, &mut |_, _, _| {});
        assert_eq!(status, Status::Consumed);
        assert_eq!(ui.capture(), Some(HitTarget::Scrollbar { owner: win }));
        // Bottom row click → bar snaps to max scroll (90 = total - viewport).
        assert_eq!(ui.win(win).unwrap().scroll_top(), 90);
    }

    #[test]
    fn dispatch_mouse_left_drag_with_scrollbar_capture_resnaps_scroll() {
        let mut ui = make_ui();
        let win = make_scrollbar_split(&mut ui);
        ui.set_capture(HitTarget::Scrollbar { owner: win });
        let drag = mouse_event(
            crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            5,
            5,
        );
        let status = ui.dispatch_event(drag, &mut |_, _, _| {});
        assert_eq!(status, Status::Consumed);
        // Capture survives the drag.
        assert_eq!(ui.capture(), Some(HitTarget::Scrollbar { owner: win }));
        // Mid-track drag advances scroll past zero.
        assert!(ui.win(win).unwrap().scroll_top() > 0);
    }

    #[test]
    fn scrollbar_drag_uses_frozen_geometry_until_release() {
        let mut ui = make_ui();
        let win = make_scrollbar_split(&mut ui);
        let down = mouse_event(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            5,
            19,
        );
        assert_eq!(ui.dispatch_event(down, &mut |_, _, _| {}), Status::Consumed);
        let initial_scroll = ui.win(win).unwrap().scroll_top();

        let rect = layout::Rect::new(0, 0, 20, 10);
        let changed_bar = window::ScrollbarState::new(19, 1000, 10).unwrap();
        ui.win_mut(win).unwrap().viewport = Some(window::WindowViewport::new(
            rect,
            19,
            1000,
            initial_scroll,
            Some(changed_bar),
        ));

        let drag = mouse_event(
            crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            5,
            19,
        );
        assert_eq!(ui.dispatch_event(drag, &mut |_, _, _| {}), Status::Consumed);
        assert_eq!(ui.win(win).unwrap().scroll_top(), initial_scroll);
    }

    #[test]
    fn dispatch_mouse_left_up_with_scrollbar_capture_clears_it() {
        let mut ui = make_ui();
        let win = make_scrollbar_split(&mut ui);
        ui.set_capture(HitTarget::Scrollbar { owner: win });
        let up = mouse_event(
            crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
            0,
            0,
        );
        let status = ui.dispatch_event(up, &mut |_, _, _| {});
        assert_eq!(status, Status::Consumed);
        assert_eq!(ui.capture(), None);
    }

    #[test]
    fn dispatch_mouse_left_down_off_scrollbar_returns_ignored() {
        let mut ui = make_ui();
        let win = make_scrollbar_split(&mut ui);
        let down = mouse_event(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            3,
            5,
        );
        let status = ui.dispatch_event(down, &mut |_, _, _| {});
        // Splits leaf without scrollbar capture is host-routed.
        assert_eq!(status, Status::Ignored);
        assert_eq!(ui.capture(), None);
        let _ = win;
    }

    fn raw_mouse_event(
        kind: crossterm::event::MouseEventKind,
        row: u16,
        col: u16,
    ) -> crossterm::event::MouseEvent {
        crossterm::event::MouseEvent {
            kind,
            row,
            column: col,
            modifiers: crossterm::event::KeyModifiers::NONE,
        }
    }

    #[test]
    fn resolve_split_mouse_down_latches_window_capture_and_records_click() {
        let mut ui = make_ui();
        let win = make_scrollbar_split(&mut ui);
        // Click on content (col 5, row 3) - not on the scrollbar.
        let me = raw_mouse_event(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            3,
            5,
        );
        let resolved = ui.resolve_split_mouse(me, std::time::Instant::now());
        assert_eq!(resolved, Some((HitTarget::Window(win), 1)));
        assert_eq!(ui.capture(), Some(HitTarget::Window(win)));
        // A second Down on the same cell increments the click count.
        let resolved = ui.resolve_split_mouse(me, std::time::Instant::now());
        assert_eq!(resolved, Some((HitTarget::Window(win), 2)));
    }

    #[test]
    fn resolve_split_mouse_drag_routes_to_captured_window_off_rect() {
        let mut ui = make_ui();
        let win = make_scrollbar_split(&mut ui);
        ui.set_capture(HitTarget::Window(win));
        // Drag at (50, 50) - well outside the leaf rect - still routes
        // to `win` because capture is latched.
        let drag = raw_mouse_event(
            crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            50,
            50,
        );
        let resolved = ui.resolve_split_mouse(drag, std::time::Instant::now());
        assert_eq!(resolved, Some((HitTarget::Window(win), 0)));
        assert_eq!(ui.capture(), Some(HitTarget::Window(win)));
    }

    #[test]
    fn resolve_split_mouse_up_clears_window_capture() {
        let mut ui = make_ui();
        let win = make_scrollbar_split(&mut ui);
        ui.set_capture(HitTarget::Window(win));
        let up = raw_mouse_event(
            crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
            0,
            0,
        );
        let resolved = ui.resolve_split_mouse(up, std::time::Instant::now());
        assert_eq!(resolved, Some((HitTarget::Window(win), 0)));
        assert_eq!(ui.capture(), None);
    }

    #[test]
    fn resolve_split_mouse_down_on_scrollbar_returns_none() {
        let mut ui = make_ui();
        let _win = make_scrollbar_split(&mut ui);
        // Click on the scrollbar column - Ui::dispatch_event handles
        // that gesture; resolve_split_mouse declines.
        let me = raw_mouse_event(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            3,
            19,
        );
        assert_eq!(ui.resolve_split_mouse(me, std::time::Instant::now()), None);
        assert_eq!(ui.capture(), None);
    }

    #[test]
    fn resolve_split_mouse_orphan_drag_returns_none() {
        let mut ui = make_ui();
        let _win = make_scrollbar_split(&mut ui);
        let drag = raw_mouse_event(
            crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            3,
            5,
        );
        assert_eq!(
            ui.resolve_split_mouse(drag, std::time::Instant::now()),
            None
        );
    }

    #[test]
    fn resolve_split_mouse_non_left_returns_none() {
        let mut ui = make_ui();
        let _win = make_scrollbar_split(&mut ui);
        let me = raw_mouse_event(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Right),
            3,
            5,
        );
        assert_eq!(ui.resolve_split_mouse(me, std::time::Instant::now()), None);
        assert_eq!(ui.capture(), None);
    }

    /// Place the captured window's drag endpoint at the start of visual row
    /// `vrow` so the window has an active drag selection.
    fn park_drag_endpoint_at(ui: &mut Ui, win: WinId, vrow: usize) {
        let buf_id = ui.win(win).unwrap().buf;
        let buf = ui.buf(buf_id).unwrap();
        let byte = buf.byte_at_display_pos(vrow, 0);
        ui.win_mut(win).unwrap().text_state_mut().drag_endpoint = Some(byte);
    }

    fn park_drag_pointer_at(ui: &mut Ui, win: WinId, row: u16) {
        ui.update_drag_autoscroll_pointer(HitTarget::Window(win), row);
    }

    #[test]
    fn drag_autoscroll_returns_none_without_window_capture() {
        let ui = make_ui();
        assert!(ui.drag_autoscroll_interval().is_none());
    }

    #[test]
    fn drag_autoscroll_fires_at_top_edge() {
        let mut ui = make_ui();
        let win = make_scrollbar_split(&mut ui);
        ui.set_capture(HitTarget::Window(win));
        park_drag_endpoint_at(&mut ui, win, 0);
        park_drag_pointer_at(&mut ui, win, 0);
        assert!(ui.drag_autoscroll_interval().is_some());
        // Already at top of buffer - can't scroll further up, but the
        // trigger still fires; the tick just no-ops the pan.
        assert!(!ui.tick_drag_autoscroll());

        // Scroll into the buffer. The endpoint no longer has to be exactly on
        // the edge; the stored pointer intent drives the gesture.
        ui.win_mut(win).unwrap().pin_scroll(5);
        park_drag_endpoint_at(&mut ui, win, 6);
        park_drag_pointer_at(&mut ui, win, 0);
        assert!(ui.tick_drag_autoscroll());
        assert_eq!(ui.win(win).unwrap().scroll_top(), 4);
    }

    #[test]
    fn drag_autoscroll_fires_at_bottom_edge() {
        let mut ui = make_ui();
        let win = make_scrollbar_split(&mut ui);
        ui.set_capture(HitTarget::Window(win));
        // Viewport height is 10; row 9 is the last visible row.
        park_drag_endpoint_at(&mut ui, win, 5);
        park_drag_pointer_at(&mut ui, win, 9);
        assert!(ui.drag_autoscroll_interval().is_some());
        assert!(ui.tick_drag_autoscroll());
        assert_eq!(ui.win(win).unwrap().scroll_top(), 1);
    }

    #[test]
    fn drag_autoscroll_idle_when_pointer_leaves_edge() {
        let mut ui = make_ui();
        let win = make_scrollbar_split(&mut ui);
        ui.set_capture(HitTarget::Window(win));
        park_drag_endpoint_at(&mut ui, win, 9);
        park_drag_pointer_at(&mut ui, win, 5);
        assert!(ui.drag_autoscroll_interval().is_none());
        assert!(!ui.tick_drag_autoscroll());
    }

    #[test]
    fn drag_autoscroll_clears_latch_when_capture_releases() {
        let mut ui = make_ui();
        let win = make_scrollbar_split(&mut ui);
        ui.set_capture(HitTarget::Window(win));
        park_drag_endpoint_at(&mut ui, win, 9);
        park_drag_pointer_at(&mut ui, win, 9);
        assert!(ui.tick_drag_autoscroll());
        ui.clear_capture();
        assert!(ui.drag_autoscroll_interval().is_none());
        assert!(!ui.tick_drag_autoscroll());
    }

    #[test]
    fn drag_autoscroll_ignores_scrollbar_capture() {
        let mut ui = make_ui();
        let win = make_scrollbar_split(&mut ui);
        ui.set_capture(HitTarget::Scrollbar { owner: win });
        park_drag_endpoint_at(&mut ui, win, 0);
        park_drag_pointer_at(&mut ui, win, 0);
        assert!(ui.drag_autoscroll_interval().is_none());
        assert!(!ui.tick_drag_autoscroll());
    }

    #[test]
    fn drag_autoscroll_row_document_fires_at_bottom_edge() {
        let mut ui = make_ui();
        let buf = ui.buf_create(BufCreateOpts::default());
        if let Some(b) = ui.buf_mut(buf) {
            b.set_all_lines((0..20).map(|i| format!("line {i}")).collect());
        }
        let win = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "p".into(),
                    gutters: Gutters::default(),
                },
            )
            .unwrap();
        ui.set_layout(LayoutTree::vbox(vec![(
            Constraint::Fill,
            LayoutTree::leaf(win),
        )]));
        let rect = Rect::new(0, 0, 20, 10);
        ui.wins.get_mut(&win).unwrap().viewport = Some(WindowViewport {
            rect,
            content_width: 20,
            total_rows: 21,
            scroll_top: 0,
            scrollbar: None,
            gutter_width: 0,
        });
        ui.wins
            .get_mut(&win)
            .unwrap()
            .set_materialized_rows(0, 20, 21);

        ui.set_capture(HitTarget::Window(win));
        // Park row-document drag endpoint at bottom edge (row 9).
        {
            let state = ui.wins.get_mut(&win).unwrap().document_view_state_mut();
            state.cursor = DocPosition {
                row: 9,
                byte_col: 0,
            };
            state.drag_endpoint = Some(DocPosition {
                row: 9,
                byte_col: 0,
            });
        }
        park_drag_pointer_at(&mut ui, win, 9);
        // Sync local cursor to match the row state (row 9 is local 9 when row_base=0).
        ui.wins.get_mut(&win).unwrap().text_state_mut().cursor_row = 9;

        assert!(ui.drag_autoscroll_interval().is_some());
        assert!(ui.tick_drag_autoscroll());
        assert_eq!(ui.win(win).unwrap().scroll_top(), 1);
        let state = *ui.win(win).unwrap().document_view_state_ref();
        assert_eq!(state.cursor.row, 10);
        assert_eq!(
            state.drag_endpoint,
            Some(DocPosition {
                row: 10,
                byte_col: 0
            })
        );
    }

    #[test]
    fn drag_autoscroll_row_document_uses_pointer_when_endpoint_unmaterialized() {
        let mut ui = make_ui();
        let buf = ui.buf_create(BufCreateOpts::default());
        if let Some(b) = ui.buf_mut(buf) {
            b.set_all_lines((0..20).map(|i| format!("line {i}")).collect());
        }
        let win = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "p".into(),
                    gutters: Gutters::default(),
                },
            )
            .unwrap();
        ui.set_layout(LayoutTree::vbox(vec![(
            Constraint::Fill,
            LayoutTree::leaf(win),
        )]));
        ui.wins.get_mut(&win).unwrap().viewport = Some(WindowViewport {
            rect: Rect::new(0, 0, 20, 10),
            content_width: 20,
            total_rows: 100,
            scroll_top: 55,
            scrollbar: None,
            gutter_width: 0,
        });
        ui.wins
            .get_mut(&win)
            .unwrap()
            .set_materialized_rows(50, 20, 100);
        ui.win_mut(win).unwrap().pin_scroll(55);
        {
            let state = ui.wins.get_mut(&win).unwrap().document_view_state_mut();
            state.cursor = DocPosition {
                row: 56,
                byte_col: 0,
            };
            state.drag_endpoint = Some(DocPosition {
                row: 56,
                byte_col: 0,
            });
            state.preferred_cell_col = Some(0);
        }
        ui.set_capture(HitTarget::Window(win));
        park_drag_pointer_at(&mut ui, win, 0);

        assert!(ui.tick_drag_autoscroll());
        assert_eq!(ui.win(win).unwrap().scroll_top(), 54);
        assert_eq!(ui.win(win).unwrap().document_view_state().cursor.row, 54);

        ui.wins
            .get_mut(&win)
            .unwrap()
            .set_materialized_rows(70, 20, 100);
        assert!(ui.tick_drag_autoscroll());
        assert_eq!(ui.win(win).unwrap().scroll_top(), 53);
        let state = ui.win(win).unwrap().document_view_state();
        assert_eq!(state.cursor.row, 53);
        assert_eq!(state.drag_endpoint.unwrap().row, 53);
    }

    #[test]
    fn render_reanchors_tail_when_viewport_height_shrinks() {
        let mut ui = make_ui();
        ui.set_terminal_size(20, 12);
        let transcript_buf = ui.buf_create(BufCreateOpts::default());
        if let Some(buf) = ui.buf_mut(transcript_buf) {
            buf.set_all_lines((0..50).map(|i| format!("line {i}")).collect());
        }
        let prompt_buf = ui.buf_create(BufCreateOpts::default());
        let transcript = ui
            .win_open_split(
                transcript_buf,
                SplitConfig {
                    region: "transcript".into(),
                    gutters: Gutters {
                        scrollbar: false,
                        ..Default::default()
                    },
                },
            )
            .unwrap();
        let prompt = ui
            .win_open_split(
                prompt_buf,
                SplitConfig {
                    region: "prompt".into(),
                    gutters: Gutters {
                        scrollbar: false,
                        ..Default::default()
                    },
                },
            )
            .unwrap();
        ui.set_layout(LayoutTree::vbox(vec![
            (Constraint::Fill, LayoutTree::leaf(transcript)),
            (Constraint::Length(2), LayoutTree::leaf(prompt)),
        ]));

        let mut out = Vec::new();
        ui.render(&mut out).unwrap();
        ui.win_mut(transcript).unwrap().follow_tail();
        ui.resolve_tail_scrolls();
        assert_eq!(ui.win(transcript).unwrap().scroll_top(), 40);

        ui.set_layout(LayoutTree::vbox(vec![
            (Constraint::Fill, LayoutTree::leaf(transcript)),
            (Constraint::Length(5), LayoutTree::leaf(prompt)),
        ]));
        out.clear();
        ui.render(&mut out).unwrap();

        let win = ui.win(transcript).unwrap();
        let viewport = win.viewport.expect("render populates viewport");
        assert_eq!(viewport.rect.height, 7);
        assert_eq!(win.scroll_top(), 43);
        assert_eq!(viewport.scroll_top, 43);
        assert!(win.is_following_tail());
    }

    #[test]
    fn resolve_tail_scrolls_respects_frozen() {
        let mut ui = make_ui();
        ui.set_terminal_size(20, 10);
        let buf = ui.buf_create(BufCreateOpts::default());
        if let Some(b) = ui.buf_mut(buf) {
            b.set_all_lines((0..50).map(|i| format!("line {i}")).collect());
        }
        let win = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "p".into(),
                    gutters: Gutters::default(),
                },
            )
            .unwrap();
        ui.set_layout(LayoutTree::vbox(vec![(
            Constraint::Fill,
            LayoutTree::leaf(win),
        )]));
        let rect = layout::Rect::new(0, 0, 20, 10);
        let bar = window::ScrollbarState::new(19, 50, 10).unwrap();
        ui.win_mut(win).unwrap().viewport =
            Some(window::WindowViewport::new(rect, 19, 50, 0, Some(bar)));
        // Start at top in tail-follow mode.
        ui.win_mut(win).unwrap().pin_scroll(0);
        ui.win_mut(win).unwrap().follow_tail();
        ui.resolve_tail_scrolls();
        assert_eq!(
            ui.win(win).unwrap().scroll_top(),
            40,
            "tail-follow snaps to bottom"
        );

        // Move back to top and pin with a selection anchor.
        {
            let w = ui.win_mut(win).unwrap();
            w.pin_scroll(0);
            w.text_state_mut().selection_anchor = Some(5);
        }
        ui.resolve_tail_scrolls();
        assert_eq!(
            ui.win(win).unwrap().scroll_top(),
            0,
            "selection pins scroll in place"
        );

        // Clearing the selection leaves the window pinned until tail is requested again.
        ui.win_mut(win).unwrap().text_state_mut().selection_anchor = None;
        ui.resolve_tail_scrolls();
        assert_eq!(
            ui.win(win).unwrap().scroll_top(),
            0,
            "clearing selection does not implicitly re-tail"
        );

        ui.win_mut(win).unwrap().follow_tail();
        ui.resolve_tail_scrolls();
        assert_eq!(ui.win(win).unwrap().scroll_top(), 40);
    }

    #[test]
    fn resolve_tail_scrolls_skips_scrollbar_capture() {
        let mut ui = make_ui();
        let buf = ui.buf_create(BufCreateOpts::default());
        if let Some(b) = ui.buf_mut(buf) {
            b.set_all_lines((0..50).map(|i| format!("line {i}")).collect());
        }
        let win = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "p".into(),
                    gutters: Gutters::default(),
                },
            )
            .unwrap();
        let rect = layout::Rect::new(0, 0, 20, 10);
        let bar = window::ScrollbarState::new(19, 50, 10).unwrap();
        ui.win_mut(win).unwrap().viewport =
            Some(window::WindowViewport::new(rect, 19, 50, 0, Some(bar)));
        ui.win_mut(win).unwrap().pin_scroll(0);
        ui.win_mut(win).unwrap().follow_tail();
        ui.set_capture(HitTarget::Scrollbar { owner: win });

        ui.resolve_tail_scrolls();

        assert_eq!(ui.win(win).unwrap().scroll_top(), 0);
        ui.clear_capture();
    }

    #[test]
    fn render_prepare_uses_effective_follow_tail() {
        let mut ui = make_ui();
        ui.set_terminal_size(20, 10);
        let buf = ui.buf_create(BufCreateOpts::default());
        if let Some(b) = ui.buf_mut(buf) {
            b.set_all_lines((0..50).map(|i| format!("line {i}")).collect());
        }
        let win = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "p".into(),
                    gutters: Gutters::default(),
                },
            )
            .unwrap();
        ui.set_layout(LayoutTree::vbox(vec![(
            Constraint::Fill,
            LayoutTree::leaf(win),
        )]));

        ui.win_mut(win).unwrap().pin_scroll(0);
        ui.win_mut(win).unwrap().follow_tail();
        let mut request_follow_tail = None;
        let mut out = Vec::new();
        ui.render_with_paints_prepared(
            &mut out,
            |_, request| {
                if request.win == win {
                    request_follow_tail = Some(request.follow_tail);
                }
            },
            |_, _, _| {},
        )
        .unwrap();
        assert_eq!(request_follow_tail, Some(true));

        ui.win_mut(win).unwrap().pin_scroll(0);
        ui.win_mut(win).unwrap().follow_tail();
        ui.set_capture(HitTarget::Scrollbar { owner: win });
        let mut request_follow_tail = None;
        let mut out = Vec::new();
        ui.render_with_paints_prepared(
            &mut out,
            |_, request| {
                if request.win == win {
                    request_follow_tail = Some(request.follow_tail);
                }
            },
            |_, _, _| {},
        )
        .unwrap();
        assert_eq!(request_follow_tail, Some(false));
        ui.clear_capture();
    }

    #[test]
    fn scrollbar_drag_maps_thumb_to_scroll_top() {
        let mut ui = make_ui();
        let win = make_scrollbar_split(&mut ui);
        // 100 rows, 10-row viewport → max_scroll = 90.
        // Click at row 0 (top of scrollbar) → scroll_top = 0.
        ui.apply_scrollbar_drag(win, 0);
        assert_eq!(ui.win(win).unwrap().scroll_top(), 0);

        // Click at row 9 (bottom of scrollbar) → scroll_top = max_scroll = 90.
        ui.apply_scrollbar_drag(win, 9);
        assert_eq!(ui.win(win).unwrap().scroll_top(), 90);

        // Click in the middle (row 4) → somewhere near middle of scroll range.
        ui.apply_scrollbar_drag(win, 4);
        let scroll = ui.win(win).unwrap().scroll_top();
        assert!(
            scroll > 0 && scroll < 90,
            "mid-thumb maps to mid-scroll: got {scroll}"
        );
    }

    #[test]
    fn scroll_anchor_restored_after_terminal_resize() {
        let mut ui = make_ui();
        let buf = ui.buf_create(BufCreateOpts::default());
        if let Some(b) = ui.buf_mut(buf) {
            b.set_all_lines((0..100).map(|i| format!("line {i}")).collect());
        }
        let win = ui
            .win_open_split(
                buf,
                SplitConfig {
                    region: "p".into(),
                    gutters: Gutters {
                        scrollbar: false,
                        ..Default::default()
                    },
                },
            )
            .unwrap();
        ui.set_layout(LayoutTree::vbox(vec![(
            Constraint::Fill,
            LayoutTree::leaf(win),
        )]));
        let w = ui.win_mut(win).unwrap();
        w.viewport = Some(window::WindowViewport::new(
            layout::Rect::new(0, 0, 80, 24),
            80,
            100,
            50,
            None,
        ));
        w.wrap = false;
        // Build layout first so set_scroll can stamp an anchor.
        {
            let (w, b) = ui.win_and_buf_mut(win, buf);
            let ww = w.unwrap();
            let bref = b.unwrap();
            ww.ensure_layout(bref, 80);
            ww.set_scroll(50, bref);
        }
        assert!(
            ui.win(win).unwrap().scroll_anchor.is_some(),
            "anchor stamped"
        );

        // Simulate terminal resize: narrower width forces layout rebuild.
        // ensure_layout already calls restore_scroll_from_anchor internally
        // when there is no cursor screen row to preserve.
        ui.set_terminal_size(40, 24);
        {
            let (w, b) = ui.win_and_buf_mut(win, buf);
            w.unwrap().ensure_layout(b.unwrap(), 40);
        }
        // At width 40 each line still fits on one row ("line N" < 40 chars),
        // so visual row should still be 50.
        assert_eq!(
            ui.win(win).unwrap().scroll_top(),
            50,
            "anchor restored same logical row after resize"
        );
    }
}
