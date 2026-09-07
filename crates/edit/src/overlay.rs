//! Z-stacked overlay window groups positioned via an `Anchor` over a `LayoutTree`.

use super::WinId;
use crate::layout::{Align, Anchor, Constraint, ContainerId, Corner, LayoutTree, PaintId, Rect};
use std::collections::HashMap;

/// Stable handle for an overlay. Distinct from `WinId` to avoid chrome/content hit collision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OverlayId(pub u32);

/// Stable handle for a window-owned decoration. Decorations are rendered inside
/// their owner pane instead of in the global overlay plane.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DecorationId(pub u32);

/// Edges affected by a resize drag.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResizeEdges {
    pub north: bool,
    pub east: bool,
    pub south: bool,
    pub west: bool,
}

impl ResizeEdges {
    pub const fn north() -> Self {
        Self {
            north: true,
            east: false,
            south: false,
            west: false,
        }
    }

    pub const fn east() -> Self {
        Self {
            north: false,
            east: true,
            south: false,
            west: false,
        }
    }

    pub const fn south() -> Self {
        Self {
            north: false,
            east: false,
            south: true,
            west: false,
        }
    }

    pub const fn west() -> Self {
        Self {
            north: false,
            east: false,
            south: false,
            west: true,
        }
    }

    pub const fn corner(north: bool, east: bool, south: bool, west: bool) -> Self {
        Self {
            north,
            east,
            south,
            west,
        }
    }
}

/// Body-drag policy for an overlay's leaf area.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyDrag {
    Never,
    /// Drag only when the hit leaf is inert/non-focusable and non-selectable.
    Inert,
    Always,
}

/// Regions that can move an overlay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DragConfig {
    pub title: bool,
    pub body: BodyDrag,
}

impl DragConfig {
    pub const fn none() -> Self {
        Self {
            title: false,
            body: BodyDrag::Never,
        }
    }

    pub const fn floating() -> Self {
        Self {
            title: true,
            body: BodyDrag::Inert,
        }
    }
}

/// Chrome edges that can resize an overlay. `corners` upgrades cells where two
/// enabled edges meet into diagonal handles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResizeConfig {
    pub top: bool,
    pub right: bool,
    pub bottom: bool,
    pub left: bool,
    pub corners: bool,
}

impl ResizeConfig {
    pub const fn none() -> Self {
        Self {
            top: false,
            right: false,
            bottom: false,
            left: false,
            corners: false,
        }
    }

    pub const fn floating() -> Self {
        Self {
            top: false,
            right: true,
            bottom: true,
            left: true,
            corners: true,
        }
    }
}

/// Concrete action assigned to an overlay chrome cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChromeAction {
    None,
    Move,
    Resize(ResizeEdges),
}

/// Stable owner of interactive container chrome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChromeOwner {
    Overlay(OverlayId),
    Container(ContainerId),
    Split(crate::layout::SplitId),
}

/// Global mouse hit-test result covering both overlays and splits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HitTarget {
    Window(super::WinId),
    Paint(PaintId),
    Scrollbar {
        owner: super::WinId,
    },
    Chrome {
        owner: ChromeOwner,
        action: ChromeAction,
    },
}

#[derive(Clone, Debug)]
pub struct Decoration {
    pub owner: WinId,
    pub layout: LayoutTree,
    /// Alignment point inside the owner rect. `Center` centers the decoration;
    /// edge/corner values pin the matching edge/corner inside the owner.
    pub align: Align,
    pub row_offset: i32,
    pub col_offset: i32,
    /// Width/height constraints are resolved against the owner rect, not the
    /// terminal. `Fit` reads the decoration layout's natural size.
    pub width: Constraint,
    pub height: Constraint,
    pub max_width: Option<Constraint>,
    pub max_height: Option<Constraint>,
    pub min_width: Option<Constraint>,
    pub min_height: Option<Constraint>,
    /// Stacking order relative to other decorations on the same owner.
    pub z: u16,
}

impl Decoration {
    pub fn new(owner: WinId, layout: LayoutTree) -> Self {
        Self {
            owner,
            layout,
            align: Align::Center,
            row_offset: 0,
            col_offset: 0,
            width: Constraint::Fit,
            height: Constraint::Fit,
            max_width: None,
            max_height: None,
            min_width: None,
            min_height: None,
            z: 0,
        }
    }

    pub fn with_align(mut self, align: Align) -> Self {
        self.align = align;
        self
    }

    pub fn with_offset(mut self, row: i32, col: i32) -> Self {
        self.row_offset = row;
        self.col_offset = col;
        self
    }

    pub fn with_z(mut self, z: u16) -> Self {
        self.z = z;
        self
    }

    pub fn with_width(mut self, width: Constraint) -> Self {
        self.width = width;
        self
    }

    pub fn with_height(mut self, height: Constraint) -> Self {
        self.height = height;
        self
    }

    pub fn with_max_width(mut self, max_width: Option<Constraint>) -> Self {
        self.max_width = max_width;
        self
    }

    pub fn with_max_height(mut self, max_height: Option<Constraint>) -> Self {
        self.max_height = max_height;
        self
    }

    pub fn with_min_width(mut self, min_width: Option<Constraint>) -> Self {
        self.min_width = min_width;
        self
    }

    pub fn with_min_height(mut self, min_height: Option<Constraint>) -> Self {
        self.min_height = min_height;
        self
    }
}

#[derive(Clone, Debug)]
pub struct Overlay {
    pub layout: LayoutTree,
    pub anchor: Anchor,
    /// How wide the overlay rect is along the horizontal axis. Resolved
    /// against the terminal width every frame. Default `Fit` reads the
    /// layout's natural width - back-compat with the original
    /// `natural_size_with` path.
    pub width: Constraint,
    /// Vertical-axis twin of [`Self::width`].
    pub height: Constraint,
    /// Optional cap applied after [`Self::width`] resolves. Lets a caller
    /// say "fit to content, but never exceed 50% of the terminal" by
    /// pairing `width = Fit` with `max_width = Percentage(50)`. Resolved
    /// the same way as `width` but used only as an upper bound.
    pub max_width: Option<Constraint>,
    /// Vertical-axis twin of [`Self::max_width`].
    pub max_height: Option<Constraint>,
    /// Optional floor applied after [`Self::width`] resolves. Pairs with
    /// `width = Fit` to express "shrink to content, but never smaller
    /// than 20 cells". Resolved the same way as `width` but used only as
    /// a lower bound.
    pub min_width: Option<Constraint>,
    /// Vertical-axis twin of [`Self::min_width`].
    pub min_height: Option<Constraint>,
    /// Stacking order. Higher draws on top; same `z` breaks by insertion order.
    pub z: u16,
    /// When true, focus + Tab cycling stay inside this overlay; Esc/Ctrl-C fires Dismiss.
    pub modal: bool,
    /// When true, the host pauses engine-event drain while focus is here.
    pub blocks_agent: bool,
    /// Regions that can move this overlay.
    pub draggable: DragConfig,
    /// Chrome edges that can resize this overlay.
    pub resizable: ResizeConfig,
    /// Explicit size override; set by resize gesture, preserved across frames.
    /// Wins over [`Self::width`]/[`Self::height`] when set.
    pub size_override: Option<(u16, u16)>,
}

impl Overlay {
    pub fn new(layout: LayoutTree, anchor: Anchor) -> Self {
        Self {
            layout,
            anchor,
            width: Constraint::Fit,
            height: Constraint::Fit,
            max_width: None,
            max_height: None,
            min_width: None,
            min_height: None,
            z: 50,
            modal: false,
            blocks_agent: false,
            draggable: DragConfig::none(),
            resizable: ResizeConfig::none(),
            size_override: None,
        }
    }

    pub fn with_z(mut self, z: u16) -> Self {
        self.z = z;
        self
    }

    pub fn modal(mut self, modal: bool) -> Self {
        self.modal = modal;
        self
    }

    pub fn blocks_agent(mut self, b: bool) -> Self {
        self.blocks_agent = b;
        self
    }

    pub fn draggable(mut self, b: bool) -> Self {
        self.draggable = if b {
            DragConfig::floating()
        } else {
            DragConfig::none()
        };
        self
    }

    pub fn drag_config(mut self, config: DragConfig) -> Self {
        self.draggable = config;
        self
    }

    pub fn resizable(mut self, b: bool) -> Self {
        self.resizable = if b {
            ResizeConfig::floating()
        } else {
            ResizeConfig::none()
        };
        self
    }

    pub fn resize_config(mut self, config: ResizeConfig) -> Self {
        self.resizable = config;
        self
    }

    pub fn with_size(mut self, size: (u16, u16)) -> Self {
        self.size_override = Some(size);
        self
    }

    pub fn with_width(mut self, w: Constraint) -> Self {
        self.width = w;
        self
    }

    pub fn with_height(mut self, h: Constraint) -> Self {
        self.height = h;
        self
    }

    pub fn with_max_width(mut self, w: Option<Constraint>) -> Self {
        self.max_width = w;
        self
    }

    pub fn with_max_height(mut self, h: Option<Constraint>) -> Self {
        self.max_height = h;
        self
    }

    pub fn with_min_width(mut self, w: Option<Constraint>) -> Self {
        self.min_width = w;
        self
    }

    pub fn with_min_height(mut self, h: Option<Constraint>) -> Self {
        self.min_height = h;
        self
    }
}

/// Inputs for the anchor resolver.
pub struct AnchorContext<'a> {
    pub term_width: u16,
    pub term_height: u16,
    pub cursor: Option<(u16, u16)>,
    pub win_rects: &'a HashMap<WinId, Rect>,
}

/// Resolve an anchor + overlay size to a clamped screen rect.
/// Returns `None` for `Cursor` anchors without a cursor or `Win` anchors with no target rect.
pub fn resolve_anchor(anchor: &Anchor, size: (u16, u16), ctx: &AnchorContext<'_>) -> Option<Rect> {
    let (w, h) = size;
    let term_w = ctx.term_width;
    let term_h = ctx.term_height;
    let w = w.min(term_w);
    let h = h.min(term_h);
    let (top, left) = match anchor {
        Anchor::ScreenCenter => (term_h.saturating_sub(h) / 2, term_w.saturating_sub(w) / 2),
        Anchor::ScreenAt { row, col, corner } => {
            let (r, c) = corner_to_topleft(*corner, *row, *col, w, h);
            (clamp_axis(r, term_h, h), clamp_axis(c, term_w, w))
        }
        Anchor::Cursor {
            corner,
            row_offset,
            col_offset,
        } => {
            let (cy, cx) = ctx.cursor?;
            let r = cy as i32 + row_offset;
            let c = cx as i32 + col_offset;
            let (r, c) = corner_to_topleft(*corner, r, c, w, h);
            // Flip to opposite corner if overflow.
            let r = if r + h as i32 > term_h as i32 || r < 0 {
                let opposite = flip_vert(*corner);
                let (r2, _) = corner_to_topleft(
                    opposite,
                    cy as i32 + row_offset,
                    cx as i32 + col_offset,
                    w,
                    h,
                );
                r2
            } else {
                r
            };
            let c = if c + w as i32 > term_w as i32 || c < 0 {
                let opposite = flip_horiz(*corner);
                let (_, c2) = corner_to_topleft(
                    opposite,
                    cy as i32 + row_offset,
                    cx as i32 + col_offset,
                    w,
                    h,
                );
                c2
            } else {
                c
            };
            (clamp_axis(r, term_h, h), clamp_axis(c, term_w, w))
        }
        Anchor::Win {
            target,
            attach,
            row_offset,
            col_offset,
        } => {
            let target_rect = ctx.win_rects.get(&WinId(target.0))?;
            // The alignment picks the same anchor point on the target and
            // the overlay; subtracting `align_offset(attach, overlay)` from
            // the target's anchor point yields the overlay's top-left.
            let target_x = target_rect.left as i32 + align_x(*attach, target_rect.width);
            let target_y = target_rect.top as i32 + align_y(*attach, target_rect.height);
            let r = target_y - align_y(*attach, h) + row_offset;
            let c = target_x - align_x(*attach, w) + col_offset;
            (clamp_axis(r, term_h, h), clamp_axis(c, term_w, w))
        }
        Anchor::ScreenBottom { above_rows } => {
            let avail_h = term_h.saturating_sub(*above_rows);
            let h = h.min(avail_h);
            let top = avail_h.saturating_sub(h);
            let left = term_w.saturating_sub(w) / 2;
            return Some(Rect::new(top, left, w, h));
        }
    };
    Some(Rect::new(top, left, w, h))
}

/// Resolve a decoration size to a rect fully contained by `owner`.
pub fn resolve_owner_anchor(
    owner: Rect,
    size: (u16, u16),
    align: Align,
    row_offset: i32,
    col_offset: i32,
) -> Rect {
    let w = size.0.min(owner.width);
    let h = size.1.min(owner.height);
    let target_x = owner.left as i32 + align_x(align, owner.width);
    let target_y = owner.top as i32 + align_y(align, owner.height);
    let row = target_y - align_y(align, h) + row_offset;
    let col = target_x - align_x(align, w) + col_offset;
    Rect::new(
        clamp_within(row, owner.top, owner.height, h),
        clamp_within(col, owner.left, owner.width, w),
        w,
        h,
    )
}

fn clamp_within(v: i32, origin: u16, extent: u16, len: u16) -> u16 {
    let min = origin as i32;
    let max = origin as i32 + extent.saturating_sub(len) as i32;
    v.clamp(min, max) as u16
}

/// X-axis offset from a rect's left edge to the alignment point. `Center`
/// rounds down on odd widths so two centered rects of the same parity land
/// on the same column.
fn align_x(a: Align, width: u16) -> i32 {
    match a {
        Align::NW | Align::W | Align::SW => 0,
        Align::N | Align::Center | Align::S => width as i32 / 2,
        Align::NE | Align::E | Align::SE => width as i32,
    }
}

/// Y-axis offset from a rect's top edge to the alignment point. Mirror of
/// [`align_x`] for the vertical axis.
fn align_y(a: Align, height: u16) -> i32 {
    match a {
        Align::NW | Align::N | Align::NE => 0,
        Align::W | Align::Center | Align::E => height as i32 / 2,
        Align::SW | Align::S | Align::SE => height as i32,
    }
}

/// Corner-anchored point `(row, col)` → rectangle top-left for size `(w, h)`.
fn corner_to_topleft(corner: Corner, row: i32, col: i32, w: u16, h: u16) -> (i32, i32) {
    match corner {
        Corner::NW => (row, col),
        Corner::NE => (row, col - w as i32 + 1),
        Corner::SW => (row - h as i32 + 1, col),
        Corner::SE => (row - h as i32 + 1, col - w as i32 + 1),
    }
}

fn flip_vert(c: Corner) -> Corner {
    match c {
        Corner::NW => Corner::SW,
        Corner::NE => Corner::SE,
        Corner::SW => Corner::NW,
        Corner::SE => Corner::NE,
    }
}

fn flip_horiz(c: Corner) -> Corner {
    match c {
        Corner::NW => Corner::NE,
        Corner::NE => Corner::NW,
        Corner::SW => Corner::SE,
        Corner::SE => Corner::SW,
    }
}

fn clamp_axis(pos: i32, term: u16, span: u16) -> u16 {
    let max_start = term.saturating_sub(span) as i32;
    pos.clamp(0, max_start) as u16
}

#[cfg(test)]
mod tests {
    use super::WinId;
    use super::*;
    use crate::layout::{Align, Anchor, Constraint, Corner};

    #[test]
    fn overlay_defaults_are_sensible() {
        let layout = LayoutTree::vbox(vec![(Constraint::Fill, LayoutTree::leaf(WinId(42)))]);
        let ov = Overlay::new(layout, Anchor::ScreenCenter);
        assert_eq!(ov.z, 50);
        assert!(!ov.modal);
        assert_eq!(ov.anchor, Anchor::ScreenCenter);
    }

    #[test]
    fn overlay_builders_compose() {
        let layout = LayoutTree::vbox(vec![(Constraint::Fill, LayoutTree::leaf(WinId(42)))]);
        let ov = Overlay::new(
            layout,
            Anchor::Win {
                target: WinId(7).into(),
                attach: Align::NW,
                row_offset: 0,
                col_offset: 0,
            },
        )
        .with_z(100)
        .modal(true);
        assert_eq!(ov.z, 100);
        assert!(ov.modal);
        assert!(matches!(ov.anchor, Anchor::Win { .. }));
    }

    #[test]
    fn overlay_id_round_trips() {
        let id = OverlayId(7);
        assert_eq!(id, OverlayId(7));
        assert_ne!(id, OverlayId(8));
    }

    fn ctx<'a>(w: u16, h: u16, win_rects: &'a HashMap<WinId, Rect>) -> AnchorContext<'a> {
        AnchorContext {
            term_width: w,
            term_height: h,
            cursor: None,
            win_rects,
        }
    }

    #[test]
    fn screen_center_centers() {
        let rects = HashMap::new();
        let r = resolve_anchor(&Anchor::ScreenCenter, (40, 10), &ctx(80, 24, &rects)).unwrap();
        assert_eq!(r, Rect::new(7, 20, 40, 10));
    }

    #[test]
    fn screen_center_clamps_to_terminal() {
        let rects = HashMap::new();
        let r = resolve_anchor(&Anchor::ScreenCenter, (200, 50), &ctx(80, 24, &rects)).unwrap();
        assert_eq!(r, Rect::new(0, 0, 80, 24));
    }

    #[test]
    fn screen_at_nw_places_at_origin() {
        let rects = HashMap::new();
        let r = resolve_anchor(
            &Anchor::ScreenAt {
                row: 5,
                col: 10,
                corner: Corner::NW,
            },
            (20, 5),
            &ctx(80, 24, &rects),
        )
        .unwrap();
        assert_eq!(r, Rect::new(5, 10, 20, 5));
    }

    #[test]
    fn screen_at_se_places_with_corner_at_target() {
        let rects = HashMap::new();
        let r = resolve_anchor(
            &Anchor::ScreenAt {
                row: 10,
                col: 30,
                corner: Corner::SE,
            },
            (10, 4),
            &ctx(80, 24, &rects),
        )
        .unwrap();
        assert_eq!(r, Rect::new(7, 21, 10, 4));
    }

    #[test]
    fn cursor_anchor_flips_when_overflowing() {
        let rects = HashMap::new();
        let mut c = ctx(80, 24, &rects);
        c.cursor = Some((22, 5)); // near bottom
        let r = resolve_anchor(
            &Anchor::Cursor {
                corner: Corner::NW,
                row_offset: 0,
                col_offset: 0,
            },
            (10, 8),
            &c,
        )
        .unwrap();
        assert_eq!(r.top, 15);
        assert_eq!(r.left, 5);
    }

    #[test]
    fn cursor_anchor_returns_none_without_cursor() {
        let rects = HashMap::new();
        let r = resolve_anchor(
            &Anchor::Cursor {
                corner: Corner::NW,
                row_offset: 0,
                col_offset: 0,
            },
            (10, 5),
            &ctx(80, 24, &rects),
        );
        assert!(r.is_none());
    }

    #[test]
    fn win_anchor_attaches_to_target_corner() {
        let mut rects = HashMap::new();
        rects.insert(WinId(7), Rect::new(10, 20, 40, 8));
        let r = resolve_anchor(
            &Anchor::Win {
                target: WinId(7).into(),
                attach: Align::NW,
                row_offset: 0,
                col_offset: 0,
            },
            (15, 5),
            &ctx(80, 24, &rects),
        )
        .unwrap();
        assert_eq!(r, Rect::new(10, 20, 15, 5));
    }

    #[test]
    fn win_anchor_se_aligns_bottom_right() {
        let mut rects = HashMap::new();
        rects.insert(WinId(7), Rect::new(10, 20, 40, 8));
        let r = resolve_anchor(
            &Anchor::Win {
                target: WinId(7).into(),
                attach: Align::SE,
                row_offset: 0,
                col_offset: 0,
            },
            (10, 4),
            &ctx(80, 24, &rects),
        )
        .unwrap();
        assert_eq!(r, Rect::new(14, 50, 10, 4));
    }

    #[test]
    fn win_anchor_returns_none_for_unknown_target() {
        let rects = HashMap::new();
        let r = resolve_anchor(
            &Anchor::Win {
                target: WinId(999).into(),
                attach: Align::NW,
                row_offset: 0,
                col_offset: 0,
            },
            (10, 4),
            &ctx(80, 24, &rects),
        );
        assert!(r.is_none());
    }

    #[test]
    fn win_anchor_center_centers_inside_target_rect() {
        let mut rects = HashMap::new();
        // Target at (top=4, left=10, w=40, h=12); centering a 20x4 overlay
        // gives top = 4 + (12-4)/2 = 8, left = 10 + (40-20)/2 = 20.
        rects.insert(WinId(3), Rect::new(4, 10, 40, 12));
        let r = resolve_anchor(
            &Anchor::Win {
                target: WinId(3).into(),
                attach: Align::Center,
                row_offset: 0,
                col_offset: 0,
            },
            (20, 4),
            &ctx(80, 24, &rects),
        )
        .unwrap();
        assert_eq!(r, Rect::new(8, 20, 20, 4));
    }

    #[test]
    fn win_anchor_n_centers_horizontally_at_top_edge() {
        let mut rects = HashMap::new();
        // Target (top=4, left=10, w=40, h=12); attaching N puts overlay's
        // top-edge-midpoint on target's top-edge-midpoint: top = 4,
        // left = 10 + 40/2 - 20/2 = 20.
        rects.insert(WinId(3), Rect::new(4, 10, 40, 12));
        let r = resolve_anchor(
            &Anchor::Win {
                target: WinId(3).into(),
                attach: Align::N,
                row_offset: 0,
                col_offset: 0,
            },
            (20, 4),
            &ctx(80, 24, &rects),
        )
        .unwrap();
        assert_eq!(r, Rect::new(4, 20, 20, 4));
    }

    #[test]
    fn win_anchor_center_with_offset_nudges_resolved_rect() {
        let mut rects = HashMap::new();
        rects.insert(WinId(3), Rect::new(4, 10, 40, 12));
        let r = resolve_anchor(
            &Anchor::Win {
                target: WinId(3).into(),
                attach: Align::Center,
                row_offset: 1,
                col_offset: -2,
            },
            (20, 4),
            &ctx(80, 24, &rects),
        )
        .unwrap();
        assert_eq!(r, Rect::new(9, 18, 20, 4));
    }

    #[test]
    fn win_anchor_offsets_shift_position() {
        let mut rects = HashMap::new();
        rects.insert(WinId(7), Rect::new(10, 20, 40, 8));
        let r = resolve_anchor(
            &Anchor::Win {
                target: WinId(7).into(),
                attach: Align::NW,
                row_offset: -1,
                col_offset: 3,
            },
            (15, 5),
            &ctx(80, 24, &rects),
        )
        .unwrap();
        assert_eq!(r, Rect::new(9, 23, 15, 5));
    }

    #[test]
    fn screen_bottom_docks_full_height_above_statusline() {
        let rects = HashMap::new();
        let r = resolve_anchor(
            &Anchor::ScreenBottom { above_rows: 1 },
            (40, 24),
            &ctx(80, 24, &rects),
        )
        .unwrap();
        assert_eq!(r, Rect::new(0, 20, 40, 23));
    }

    #[test]
    fn screen_bottom_docks_short_layout_at_bottom() {
        let rects = HashMap::new();
        let r = resolve_anchor(
            &Anchor::ScreenBottom { above_rows: 1 },
            (60, 8),
            &ctx(80, 24, &rects),
        )
        .unwrap();
        assert_eq!(r, Rect::new(15, 10, 60, 8));
    }

    #[test]
    fn screen_bottom_with_no_reserved_rows_uses_full_screen() {
        let rects = HashMap::new();
        let r = resolve_anchor(
            &Anchor::ScreenBottom { above_rows: 0 },
            (80, 24),
            &ctx(80, 24, &rects),
        )
        .unwrap();
        assert_eq!(r, Rect::new(0, 0, 80, 24));
    }
}
