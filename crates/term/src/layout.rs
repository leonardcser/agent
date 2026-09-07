pub use super::geometry::Rect;
use std::collections::HashMap;

mod resolved;
mod split;
pub use resolved::{LayoutPaintOp, LayoutStyle, ResolvedLayout, ResolvedSplit, SplitPane};
pub use split::{
    Axis, DividerStyles, Split, SplitDivider, SplitGeometry, SplitId, SplitInteraction,
    SplitOptions, SplitPhase, SplitRatio, SplitResizeMode, SplitResponse, SplitSize,
};

/// Opaque leaf identifier. Hosts mint and dispatch on these; the renderer
/// treats them as opaque. Wide enough for common host-side id types.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PaintId(pub u64);

impl PaintId {
    pub fn raw(self) -> u64 {
        self.0
    }
}

/// Stable identity for a host-owned container embedded in a layout tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ContainerId(pub u64);

/// Sizing constraint for a layout child along the parent's primary
/// axis. Resolved by `resolve_constraints` against the parent's total
/// size, in declaration order:
///
/// 1. Hard sizes first (`Length`, `Percentage`, `Ratio`, `Max`) and
///    `Fit` (resolved against the leaf's natural size via the active
///    `LeafSizer`) consume their exact share of the available space.
/// 2. `Min(n)` reserves at least `n` cells, then competes with
///    `Fill` for the remainder.
/// 3. `Fill` (and any unsatisfied `Min`) splits whatever remains
///    evenly.
///
/// `Fit` reports the leaf's natural size from `LeafSizer`; with the
/// default `NoopSizer` it contributes 0 (i.e. behaves like `Fill`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Constraint {
    /// Exactly `n` cells along the axis.
    Length(u16),
    /// `p` percent of the parent's total size, clamped to remaining.
    Percentage(u16),
    /// Proportional share `num / denom` of the parent. Multiple
    /// `Ratio` siblings split proportionally to one another.
    Ratio(u16, u16),
    /// At least `n` cells; competes with `Fill` for the remainder
    /// once the minimum is satisfied.
    Min(u16),
    /// At most `n` cells. Acts like `Length(n)` when the parent has
    /// at least `n` available; smaller parents shrink it.
    Max(u16),
    /// Fill the remaining space; siblings split evenly.
    Fill,
    /// Size to the leaf's natural content. Falls back to `Fill`
    /// until leaves expose a natural-size hook.
    Fit,
}

/// A sizing `Constraint` paired with its subtree; used as `Vbox`/`Hbox` items.
pub type Item = (Constraint, LayoutTree);

/// How a container places children when its assigned rect is larger than the
/// children's resolved sizes plus the minimum gap.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Justify {
    /// Pack children at the start of the primary axis.
    #[default]
    Start,
    /// Add all surplus cells to the gaps between children, keeping the first
    /// child at the start and the last child at the end.
    SpaceBetween,
}

/// Chrome (gap, border, title, padding) shared by all layout nodes.
#[derive(Clone, Debug, Default)]
pub struct Chrome {
    /// Cells between adjacent children; `0` packs flush.
    pub gap: u16,
    /// Surplus-space placement along the primary axis.
    pub justify: Justify,
    /// Frame around the container; each enabled side reserves one row/col. `None` = no inset.
    pub border: Option<Border>,
    /// Title in the top border row. Requires `border = Some(_)`; renders as a styled
    /// [`Line`](crate::line::Line).
    pub title: Option<crate::line::Line<'static>>,
    /// Uniform inner padding (cells) on all four sides, *inside* any
    /// border. Increases the container's natural size by `2 * padding` on
    /// each axis; children are laid out in the padded-inset rect.
    pub padding: u16,
    /// Stable host identity for this node when it represents an interactive container.
    pub container: Option<ContainerId>,
}

/// Per-leaf natural-size hook. Plugins attach one to a leaf to drive
/// content-aware sizing without going through `LeafSizer`. When present,
/// it takes precedence over the sizer's reported size for `Fit`
/// resolution. Implementations are typically a static `(w, h)` pair or a
/// shared mutable cell that the plugin updates on user actions (variant
/// cycling, content change, etc.).
pub trait Natural: Send + Sync {
    /// Natural `(width, height)` for the leaf at the given available cap.
    /// `cap` is the inner-cap (border/gap already subtracted); impls may
    /// ignore it.
    fn size(&self, cap: (u16, u16)) -> (u16, u16);
}

/// Shared reference to a `Natural` hook; cheap to clone.
pub type NaturalRef = std::sync::Arc<dyn Natural>;

/// Fixed natural size. Use for leaves whose extent is known at layout
/// construction time and doesn't change between frames.
#[derive(Clone, Copy, Debug)]
pub struct StaticNatural(pub u16, pub u16);

impl Natural for StaticNatural {
    fn size(&self, _cap: (u16, u16)) -> (u16, u16) {
        (self.0, self.1)
    }
}

#[derive(Clone)]
pub enum LayoutTree {
    /// Terminal node; the host matches on `PaintId` in its paint dispatcher.
    /// Carries its own `Chrome` so leaves can have a border/title without a
    /// synthetic wrapper container. `natural`, when set, overrides the
    /// active `LeafSizer` for this leaf's natural-size reporting.
    Leaf {
        id: PaintId,
        chrome: Chrome,
        natural: Option<NaturalRef>,
    },
    /// Vertical container; children stack top-to-bottom.
    Vbox { items: Vec<Item>, chrome: Chrome },
    /// Horizontal container; children pack left-to-right.
    Hbox { items: Vec<Item>, chrome: Chrome },
    /// Chrome around one subtree, preserving its natural size and filling the inset area.
    Frame {
        child: Box<LayoutTree>,
        chrome: Chrome,
    },
    /// Two resizable subtrees separated by a shared pointer handle.
    Split {
        split: Split,
        children: Box<[LayoutTree; 2]>,
        chrome: Chrome,
    },
}

impl std::fmt::Debug for LayoutTree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LayoutTree::Leaf {
                id,
                chrome,
                natural,
            } => f
                .debug_struct("Leaf")
                .field("id", id)
                .field("chrome", chrome)
                .field("natural", &natural.as_ref().map(|_| "<NaturalRef>"))
                .finish(),
            LayoutTree::Vbox { items, chrome } => f
                .debug_struct("Vbox")
                .field("items", items)
                .field("chrome", chrome)
                .finish(),
            LayoutTree::Hbox { items, chrome } => f
                .debug_struct("Hbox")
                .field("items", items)
                .field("chrome", chrome)
                .finish(),
            LayoutTree::Frame { child, chrome } => f
                .debug_struct("Frame")
                .field("child", child)
                .field("chrome", chrome)
                .finish(),
            LayoutTree::Split {
                split,
                children,
                chrome,
            } => f
                .debug_struct("Split")
                .field("split", split)
                .field("children", children)
                .field("chrome", chrome)
                .finish(),
        }
    }
}

impl LayoutTree {
    /// Vertical container. Use `.with_gap` / `.with_border` / `.with_title` to add chrome.
    pub fn vbox(items: Vec<Item>) -> Self {
        Self::Vbox {
            items,
            chrome: Chrome::default(),
        }
    }

    /// Horizontal container. Use `.with_gap` / `.with_border` / `.with_title` to add chrome.
    pub fn hbox(items: Vec<Item>) -> Self {
        Self::Hbox {
            items,
            chrome: Chrome::default(),
        }
    }

    pub fn frame(child: Self) -> Self {
        Self::Frame {
            child: Box::new(child),
            chrome: Chrome::default(),
        }
    }

    pub fn split(split: Split, first: Self, second: Self) -> Self {
        Self::Split {
            split,
            children: Box::new([first, second]),
            chrome: Chrome::default(),
        }
    }

    /// Terminal leaf. Accepts anything `Into<PaintId>`.
    pub fn leaf(id: impl Into<PaintId>) -> Self {
        Self::Leaf {
            id: id.into(),
            chrome: Chrome::default(),
            natural: None,
        }
    }

    /// Attach a `Natural` hook to a leaf node so it can drive its own
    /// `Fit` size. No-op for containers.
    pub fn with_natural(mut self, n: NaturalRef) -> Self {
        if let Self::Leaf { natural, .. } = &mut self {
            *natural = Some(n);
        }
        self
    }

    pub fn chrome_mut(&mut self) -> &mut Chrome {
        match self {
            Self::Leaf { chrome, .. }
            | Self::Vbox { chrome, .. }
            | Self::Hbox { chrome, .. }
            | Self::Frame { chrome, .. }
            | Self::Split { chrome, .. } => chrome,
        }
    }

    pub fn chrome(&self) -> &Chrome {
        match self {
            Self::Leaf { chrome, .. }
            | Self::Vbox { chrome, .. }
            | Self::Hbox { chrome, .. }
            | Self::Frame { chrome, .. }
            | Self::Split { chrome, .. } => chrome,
        }
    }

    pub fn with_gap(mut self, g: u16) -> Self {
        self.chrome_mut().gap = g;
        self
    }

    pub fn with_justify(mut self, justify: Justify) -> Self {
        self.chrome_mut().justify = justify;
        self
    }

    pub fn with_padding(mut self, p: u16) -> Self {
        self.chrome_mut().padding = p;
        self
    }

    pub fn with_border(mut self, b: Border) -> Self {
        self.chrome_mut().border = Some(b);
        self
    }

    pub fn with_title(mut self, t: impl Into<crate::line::Line<'static>>) -> Self {
        self.chrome_mut().title = Some(t.into());
        self
    }

    /// Mark this node as a stable host-owned container.
    pub fn with_container(mut self, id: ContainerId) -> Self {
        self.chrome_mut().container = Some(id);
        self
    }

    /// Replace the root chrome's title in place.
    pub fn set_title(&mut self, t: Option<crate::line::Line<'static>>) {
        self.chrome_mut().title = t;
    }

    /// Replace the root chrome's border.
    pub fn set_border(&mut self, b: Option<Border>) {
        self.chrome_mut().border = b;
    }

    /// Whether `id` appears as a leaf in this tree (depth-first structural check).
    pub fn contains_leaf(&self, id: impl Into<PaintId>) -> bool {
        let id = id.into();
        self.contains_leaf_id(id)
    }

    fn contains_leaf_id(&self, id: PaintId) -> bool {
        match self {
            LayoutTree::Leaf { id: p, .. } => *p == id,
            LayoutTree::Frame { child, .. } => child.contains_leaf_id(id),
            LayoutTree::Split { children, .. } => {
                children.iter().any(|child| child.contains_leaf_id(id))
            }
            LayoutTree::Vbox { items, .. } | LayoutTree::Hbox { items, .. } => {
                items.iter().any(|(_, child)| child.contains_leaf_id(id))
            }
        }
    }

    /// Split handles in depth-first declaration order, without resolving geometry
    /// or invoking natural-size providers. Repeated handles remain visible so
    /// hosts can validate mount identity across composed trees.
    pub fn splits(&self) -> impl Iterator<Item = &Split> {
        let mut pending = vec![self];
        std::iter::from_fn(move || {
            while let Some(node) = pending.pop() {
                match node {
                    Self::Leaf { .. } => {}
                    Self::Frame { child, .. } => pending.push(child),
                    Self::Vbox { items, .. } | Self::Hbox { items, .. } => {
                        pending.extend(items.iter().rev().map(|(_, child)| child));
                    }
                    Self::Split {
                        split, children, ..
                    } => {
                        pending.extend(children.iter().rev());
                        return Some(split);
                    }
                }
            }
            None
        })
    }

    /// All leaf `PaintId`s in depth-first declaration order.
    pub fn leaves_in_order(&self) -> Vec<PaintId> {
        let mut out = Vec::new();
        self.collect_leaves(&mut out);
        out
    }

    fn collect_leaves(&self, out: &mut Vec<PaintId>) {
        match self {
            LayoutTree::Leaf { id, .. } => out.push(*id),
            LayoutTree::Frame { child, .. } => child.collect_leaves(out),
            LayoutTree::Split { children, .. } => {
                for child in children.iter() {
                    child.collect_leaves(out);
                }
            }
            LayoutTree::Vbox { items, .. } | LayoutTree::Hbox { items, .. } => {
                for (_, child) in items {
                    child.collect_leaves(out);
                }
            }
        }
    }

    /// Natural `(width, height)` bounded by `cap`. `Fill` contributes `0`;
    /// `Fit` contributes the leaf's natural size from the default `NoopSizer`
    /// (also `0`); chrome (border, gap) is added on top. Result is always
    /// `<= cap`.
    pub fn natural_size(&self, cap: (u16, u16)) -> (u16, u16) {
        self.natural_size_with(cap, &NoopSizer)
    }

    /// Natural `(width, height)` bounded by `cap`, asking `sizer` for each
    /// leaf's intrinsic size. `Fit` contributes the sizer's reported size on
    /// the primary axis; `Fill` always contributes `0`. Chrome (border, gap)
    /// is added on top. Result is always `<= cap`.
    pub fn natural_size_with(&self, cap: (u16, u16), sizer: &dyn LeafSizer) -> (u16, u16) {
        match self {
            LayoutTree::Leaf {
                id,
                chrome,
                natural,
            } => {
                let (cw, ch) = chrome_overhead(chrome);
                let inner_cap = (cap.0.saturating_sub(cw), cap.1.saturating_sub(ch));
                let (w, h) = natural
                    .as_deref()
                    .map(|n| n.size(inner_cap))
                    .unwrap_or_else(|| sizer.leaf_natural_size(*id, inner_cap));
                ((w + cw).min(cap.0), (h + ch).min(cap.1))
            }
            LayoutTree::Vbox { items, chrome } => natural_box(items, chrome, cap, true, sizer),
            LayoutTree::Hbox { items, chrome } => natural_box(items, chrome, cap, false, sizer),
            LayoutTree::Frame { child, chrome } => {
                let (cw, ch) = chrome_overhead(chrome);
                let inner = (cap.0.saturating_sub(cw), cap.1.saturating_sub(ch));
                let (w, h) = child.natural_size_with(inner, sizer);
                (
                    w.saturating_add(cw).min(cap.0),
                    h.saturating_add(ch).min(cap.1),
                )
            }
            LayoutTree::Split {
                split,
                children,
                chrome,
            } => {
                let (cw, ch) = chrome_overhead(chrome);
                let inner = (cap.0.saturating_sub(cw), cap.1.saturating_sub(ch));
                let sizes = [0, 1].map(|i| children[i].natural_size_with(inner, sizer));
                let (w, h) = match split.axis() {
                    Axis::Horizontal => {
                        let [a, b] = split.natural_extents(sizes[0].0, sizes[1].0);
                        (
                            a.saturating_add(b).saturating_add(1),
                            sizes[0].1.max(sizes[1].1),
                        )
                    }
                    Axis::Vertical => {
                        let [a, b] = split.natural_extents(sizes[0].1, sizes[1].1);
                        (
                            sizes[0].0.max(sizes[1].0),
                            a.saturating_add(b).saturating_add(1),
                        )
                    }
                };
                (
                    w.saturating_add(cw).min(cap.0),
                    h.saturating_add(ch).min(cap.1),
                )
            }
        }
    }

    /// Visit resolved nodes in painter order. All consumers share this geometry.
    pub fn visit<'a>(
        &'a self,
        area: Rect,
        sizer: &dyn LeafSizer,
        visit: &mut impl FnMut(&'a Self, Rect),
    ) {
        self.visit_with_geometry(area, sizer, &mut |node, area, _| visit(node, area));
    }

    pub fn resolve(&self, area: Rect, sizer: &dyn LeafSizer) -> ResolvedLayout {
        ResolvedLayout::new(self, area, sizer)
    }

    fn visit_with_geometry<'a>(
        &'a self,
        area: Rect,
        sizer: &dyn LeafSizer,
        visit: &mut impl FnMut(&'a Self, Rect, Option<SplitGeometry>),
    ) {
        let geometry = match self {
            Self::Split { split, chrome, .. } => Some(split.layout(area, chrome)),
            _ => None,
        };
        visit(self, area, geometry);
        match self {
            Self::Leaf { .. } => {}
            Self::Frame { child, chrome } => {
                child.visit_with_geometry(inset_for_chrome(area, chrome), sizer, visit)
            }
            Self::Split { children, .. } => {
                for (child, rect) in children.iter().zip(geometry.unwrap().panes) {
                    child.visit_with_geometry(rect, sizer, visit);
                }
            }
            Self::Vbox { items, chrome } | Self::Hbox { items, chrome } => {
                let (_, rects) = layout_box_children(
                    items,
                    chrome,
                    area,
                    matches!(self, Self::Vbox { .. }),
                    sizer,
                );
                for ((_, child), rect) in items.iter().zip(rects) {
                    child.visit_with_geometry(rect, sizer, visit);
                }
            }
        }
    }
}

/// Resolves a leaf's natural size for `Fit` constraints and the natural-size
/// pass. Hosts implement this against their leaf store (e.g. window buffer
/// line counts) to drive content-aware sizing.
pub trait LeafSizer {
    /// Natural `(width, height)` for `id` at the given available cap. Return
    /// `(0, 0)` for leaves with no intrinsic size.
    fn leaf_natural_size(&self, id: PaintId, cap: (u16, u16)) -> (u16, u16);
}

/// Default sizer: every leaf reports `(0, 0)`. Used by callers that don't
/// need content-aware sizing (split layout, tests, storybook).
pub struct NoopSizer;

impl LeafSizer for NoopSizer {
    fn leaf_natural_size(&self, _id: PaintId, _cap: (u16, u16)) -> (u16, u16) {
        (0, 0)
    }
}

/// `(width, height)` reserved by `chrome.border`'s per-side toggles.
fn chrome_border_dims(chrome: &Chrome) -> (u16, u16) {
    let Some(b) = chrome.border else {
        return (0, 0);
    };
    let bw = u16::from(b.left.is_some()) + u16::from(b.right.is_some());
    let bh = u16::from(b.top.is_some()) + u16::from(b.bottom.is_some());
    (bw, bh)
}

/// Total `(width, height)` overhead from chrome: border + uniform padding on
/// all four sides. Padding contributes `2 * chrome.padding` on each axis.
fn chrome_overhead(chrome: &Chrome) -> (u16, u16) {
    let (bw, bh) = chrome_border_dims(chrome);
    let pad2 = chrome.padding.saturating_mul(2);
    (bw.saturating_add(pad2), bh.saturating_add(pad2))
}

fn natural_box(
    items: &[Item],
    chrome: &Chrome,
    cap: (u16, u16),
    vertical: bool,
    sizer: &dyn LeafSizer,
) -> (u16, u16) {
    let (cap_w, cap_h) = cap;
    let (chrome_w, chrome_h) = chrome_overhead(chrome);
    let gaps = chrome
        .gap
        .saturating_mul(items.len().saturating_sub(1) as u16);

    // Inner cap subtracts chrome (border + padding) and gap from the primary axis.
    let (primary_cap, secondary_cap) = if vertical {
        (
            cap_h.saturating_sub(chrome_h).saturating_sub(gaps),
            cap_w.saturating_sub(chrome_w),
        )
    } else {
        (
            cap_w.saturating_sub(chrome_w).saturating_sub(gaps),
            cap_h.saturating_sub(chrome_h),
        )
    };

    let inner_cap = if vertical {
        (secondary_cap, primary_cap)
    } else {
        (primary_cap, secondary_cap)
    };

    let mut primary = 0u16;
    let mut secondary = 0u16;
    for (constraint, child) in items {
        let (child_w, child_h) = child.natural_size_with(inner_cap, sizer);
        let primary_size = match constraint {
            Constraint::Length(n) | Constraint::Max(n) | Constraint::Min(n) => *n,
            Constraint::Percentage(p) => {
                ((primary_cap as u32 * *p as u32) / 100).min(primary_cap as u32) as u16
            }
            Constraint::Ratio(num, denom) => {
                if *denom == 0 {
                    0
                } else {
                    ((primary_cap as u32 * *num as u32) / *denom as u32).min(primary_cap as u32)
                        as u16
                }
            }
            // `Fit` reports the leaf's natural size (via the sizer); `Fill`
            // is elastic and contributes `0` to the parent's demand.
            Constraint::Fit => {
                if vertical {
                    child_h
                } else {
                    child_w
                }
            }
            Constraint::Fill => 0,
        };
        let cross_size = if vertical { child_w } else { child_h };
        primary = primary.saturating_add(primary_size);
        secondary = secondary.max(cross_size);
    }
    let (primary_chrome, secondary_chrome) = if vertical {
        (chrome_h, chrome_w)
    } else {
        (chrome_w, chrome_h)
    };
    primary = primary.saturating_add(gaps).saturating_add(primary_chrome);
    secondary = secondary.saturating_add(secondary_chrome);

    let (w, h) = if vertical {
        (secondary, primary)
    } else {
        (primary, secondary)
    };
    (w.min(cap_w), h.min(cap_h))
}

/// Which corner of a rectangle serves as its anchor point. Used by
/// `ScreenAt` and `Cursor` (whose flip-on-overflow logic specifically
/// pivots on real corners). For window-relative anchoring see [`Align`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Corner {
    NW,
    NE,
    SW,
    SE,
}

/// 9-point alignment inside a rectangle. Used by `Anchor::Win`: the
/// chosen alignment picks the same point on both the target and the
/// overlay, so `Center` places the overlay's center on the target's
/// center, `NW` flush-mounts top-left to top-left, `N` puts the
/// overlay's top edge midpoint on the target's top edge midpoint, etc.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    NW,
    N,
    NE,
    W,
    Center,
    E,
    SW,
    S,
    SE,
}

impl From<Corner> for Align {
    fn from(c: Corner) -> Self {
        match c {
            Corner::NW => Align::NW,
            Corner::NE => Align::NE,
            Corner::SW => Align::SW,
            Corner::SE => Align::SE,
        }
    }
}

/// Screen position for an anchored overlay. Carries position only;
/// sizing lives on the container's layout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Anchor {
    /// Centered on screen.
    ScreenCenter,
    /// Absolute screen position; `corner` is placed at `(row, col)`.
    ScreenAt { row: i32, col: i32, corner: Corner },
    /// Anchored to the text cursor; flips to the opposite corner on screen overflow.
    Cursor {
        corner: Corner,
        row_offset: i32,
        col_offset: i32,
    },
    /// Anchored to another window. `attach` picks the alignment point on
    /// both the target rect and the overlay rect - see [`Align`]. `NW`
    /// flush-mounts top-left; `Center` centers the overlay inside the
    /// target; `N`/`S`/`E`/`W` align the overlay's matching edge midpoint
    /// to the target's. `row_offset`/`col_offset` nudge the resolved rect
    /// away from the alignment point - useful for compensating chrome
    /// rows (gutter, status line) the host adds inside the target.
    Win {
        target: PaintId,
        attach: Align,
        row_offset: i32,
        col_offset: i32,
    },
    /// Docked to the bottom of the screen; height clamps to `term_h - above_rows`.
    ScreenBottom { above_rows: u16 },
}

/// Glyph family painted along a border edge.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BorderStyle {
    #[default]
    Single,
    Double,
    Rounded,
    /// Light double-dash (`╌` / `╎`). Corners fall back to single-style.
    Dashed,
}

/// Styling for one edge of a `Border`. Currently only `color` (a theme highlight
/// group resolved at paint time). `EdgeStyle::default()` paints with the
/// terminal's default fg.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EdgeStyle {
    pub color: Option<smelt_style::theme::HlGroup>,
}

impl EdgeStyle {
    pub const fn new() -> Self {
        Self { color: None }
    }
    pub const fn with_color(hl: smelt_style::theme::HlGroup) -> Self {
        Self { color: Some(hl) }
    }
}

impl From<()> for EdgeStyle {
    fn from(_: ()) -> Self {
        Self::new()
    }
}

impl From<smelt_style::theme::HlGroup> for EdgeStyle {
    fn from(hl: smelt_style::theme::HlGroup) -> Self {
        Self::with_color(hl)
    }
}

/// A frame around a container: glyph family plus per-side `Option<EdgeStyle>`.
/// A side that is `None` is not drawn and reserves no row/column. A side that is
/// `Some(_)` reserves one row/column and is painted in the resolved fg.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Border {
    pub style: BorderStyle,
    pub top: Option<EdgeStyle>,
    pub right: Option<EdgeStyle>,
    pub bottom: Option<EdgeStyle>,
    pub left: Option<EdgeStyle>,
}

impl Border {
    /// All sides disabled; glyph family `Single`. Use as a base for builders.
    pub const OFF: Self = Self {
        style: BorderStyle::Single,
        top: None,
        right: None,
        bottom: None,
        left: None,
    };

    pub const fn single() -> Self {
        Self {
            style: BorderStyle::Single,
            ..Self::OFF
        }
    }
    pub const fn rounded() -> Self {
        Self {
            style: BorderStyle::Rounded,
            ..Self::OFF
        }
    }
    pub const fn double() -> Self {
        Self {
            style: BorderStyle::Double,
            ..Self::OFF
        }
    }

    pub fn top(mut self, e: impl Into<EdgeStyle>) -> Self {
        self.top = Some(e.into());
        self
    }
    pub fn right(mut self, e: impl Into<EdgeStyle>) -> Self {
        self.right = Some(e.into());
        self
    }
    pub fn bottom(mut self, e: impl Into<EdgeStyle>) -> Self {
        self.bottom = Some(e.into());
        self
    }
    pub fn left(mut self, e: impl Into<EdgeStyle>) -> Self {
        self.left = Some(e.into());
        self
    }
    /// Enable every side with `e`. Copy bound lets callers pass `()` or a `HlGroup`.
    pub fn all<E: Into<EdgeStyle> + Copy>(self, e: E) -> Self {
        self.top(e).right(e).bottom(e).left(e)
    }

    pub fn any_side(&self) -> bool {
        self.top.is_some() || self.right.is_some() || self.bottom.is_some() || self.left.is_some()
    }

    /// `Border::single().all(())` - single glyphs on all four sides, default color.
    pub fn single_all() -> Self {
        Self::single().all(())
    }
    pub fn rounded_all() -> Self {
        Self::rounded().all(())
    }
    pub fn double_all() -> Self {
        Self::double().all(())
    }

    /// Compatibility shortcuts for the three most common presets.
    pub const SINGLE: Border = Border {
        style: BorderStyle::Single,
        top: Some(EdgeStyle::new()),
        right: Some(EdgeStyle::new()),
        bottom: Some(EdgeStyle::new()),
        left: Some(EdgeStyle::new()),
    };
    pub const DOUBLE: Border = Border {
        style: BorderStyle::Double,
        top: Some(EdgeStyle::new()),
        right: Some(EdgeStyle::new()),
        bottom: Some(EdgeStyle::new()),
        left: Some(EdgeStyle::new()),
    };
    pub const ROUNDED: Border = Border {
        style: BorderStyle::Rounded,
        top: Some(EdgeStyle::new()),
        right: Some(EdgeStyle::new()),
        bottom: Some(EdgeStyle::new()),
        left: Some(EdgeStyle::new()),
    };
}

#[derive(Clone, Copy, Debug)]
pub struct Gutters {
    pub pad_left: u16,
    pub pad_right: u16,
    pub scrollbar: bool,
}

impl Default for Gutters {
    /// Any buffer-backed window is scrollable by default. The scrollbar paints
    /// only when content overflows; the column it occupies is reserved either
    /// way so content width is stable across overflow transitions. Surfaces
    /// that can't scroll by construction (single-line status strips, cursor-
    /// driven list/option/input panels) must opt out with `scrollbar: false`.
    fn default() -> Self {
        Self {
            pad_left: 0,
            pad_right: 0,
            scrollbar: true,
        }
    }
}

impl Gutters {
    pub fn scrollbar_width(&self) -> u16 {
        if self.scrollbar {
            1
        } else {
            0
        }
    }

    /// Width inside the left padding (still includes the scrollbar column if any).
    pub fn layer_width(&self, total: u16) -> u16 {
        total.saturating_sub(self.pad_left)
    }

    /// Inner content width once `pad_left`, `pad_right`, and the scrollbar column are subtracted.
    pub fn content_width(&self, total: u16) -> u16 {
        self.layer_width(total)
            .saturating_sub(self.pad_right)
            .saturating_sub(self.scrollbar_width())
    }

    /// Inner content width after a data-driven gutter column has been reserved.
    pub fn content_width_with_gutter(&self, total: u16, gutter_width: u16) -> u16 {
        self.content_width(total.saturating_sub(gutter_width))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayoutRect {
    pub id: PaintId,
    pub rect: Rect,
}

/// Resolve the tree against `area` and return the rect of every leaf. `Fit`
/// constraints contribute `0` (no leaf-size hook); use `resolve_layout_with`
/// to drive content-aware sizing.
pub fn resolve_layout(tree: &LayoutTree, area: Rect) -> HashMap<PaintId, Rect> {
    resolve_layout_with(tree, area, &NoopSizer)
}

/// Resolve the tree against `area` using `sizer` to size `Fit` constraints
/// from each leaf's natural size. `Fit` items claim their natural primary
/// extent before `Fill` siblings split the remainder.
pub fn resolve_layout_with(
    tree: &LayoutTree,
    area: Rect,
    sizer: &dyn LeafSizer,
) -> HashMap<PaintId, Rect> {
    resolve_layout_ordered_with(tree, area, sizer)
        .into_iter()
        .map(|r| (r.id, r.rect))
        .collect()
}

/// Resolve the tree against `area` and return every leaf in declaration order.
/// Unlike [`resolve_layout`], repeated `PaintId`s are preserved.
pub fn resolve_layout_ordered(tree: &LayoutTree, area: Rect) -> Vec<LayoutRect> {
    resolve_layout_ordered_with(tree, area, &NoopSizer)
}

/// Resolve the tree against `area` using `sizer` and preserve every leaf in
/// declaration order. Repeated `PaintId`s appear once per leaf.
pub fn resolve_layout_ordered_with(
    tree: &LayoutTree,
    area: Rect,
    sizer: &dyn LeafSizer,
) -> Vec<LayoutRect> {
    let mut result = Vec::new();
    resolve_node_ordered(tree, area, sizer, &mut result);
    result
}

/// Resolve every stable host-owned container to its outer node rectangle.
pub fn resolve_containers_with(
    tree: &LayoutTree,
    area: Rect,
    sizer: &dyn LeafSizer,
) -> HashMap<ContainerId, Rect> {
    let mut result = HashMap::new();
    resolve_container_nodes(tree, area, sizer, &mut result);
    result
}

/// Inner area after subtracting the border's per-side reservations.
/// Returns `area` unchanged when `border` is `None`. Does not account for
/// `Chrome.padding`; prefer [`inset_for_chrome`] when you have the full
/// `Chrome`.
pub fn inset_for_border(area: Rect, border: Option<Border>) -> Rect {
    let Some(b) = border else {
        return area;
    };
    let top_pad = if b.top.is_some() { 1 } else { 0 };
    let bot_pad = if b.bottom.is_some() { 1 } else { 0 };
    let left_pad = if b.left.is_some() { 1 } else { 0 };
    let right_pad = if b.right.is_some() { 1 } else { 0 };
    let h = area.height.saturating_sub(top_pad).saturating_sub(bot_pad);
    let w = area
        .width
        .saturating_sub(left_pad)
        .saturating_sub(right_pad);
    Rect::new(area.top + top_pad, area.left + left_pad, w, h)
}

/// Inner area after subtracting both `chrome.border` reservations and
/// `chrome.padding` (uniform on all four sides). Returns `area` unchanged
/// when both are zero.
pub fn inset_for_chrome(area: Rect, chrome: &Chrome) -> Rect {
    let bordered = inset_for_border(area, chrome.border);
    let p = chrome.padding;
    if p == 0 {
        return bordered;
    }
    let top = bordered.top + p;
    let left = bordered.left + p;
    let w = bordered.width.saturating_sub(p).saturating_sub(p);
    let h = bordered.height.saturating_sub(p).saturating_sub(p);
    Rect::new(top, left, w, h)
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ChromePaintCtx {
    pub top: Option<smelt_style::theme::HlGroup>,
    pub right: Option<smelt_style::theme::HlGroup>,
    pub bottom: Option<smelt_style::theme::HlGroup>,
    pub left: Option<smelt_style::theme::HlGroup>,
}

impl ChromePaintCtx {
    pub fn empty() -> Self {
        Self::default()
    }
}

/// Paint a container's border and title into `grid` at `area`.
/// Corners are drawn only when both adjacent edges are enabled. Active side
/// edges own their endpoint corners; otherwise top/bottom wins when adjacent
/// edge colors disagree. Title requires `border.top.is_some()`.
pub fn paint_chrome(
    grid: &mut crate::grid::Grid,
    area: Rect,
    chrome: &Chrome,
    theme: &crate::Theme,
) {
    paint_chrome_with(grid, area, chrome, theme, ChromePaintCtx::empty());
}

pub fn paint_chrome_with(
    grid: &mut crate::grid::Grid,
    area: Rect,
    chrome: &Chrome,
    theme: &crate::Theme,
    ctx: ChromePaintCtx,
) {
    let Some(border) = chrome.border else {
        return;
    };
    if !border.any_side() {
        return;
    }
    if area.width == 0 || area.height == 0 {
        return;
    }
    let (h, v, tl, tr, bl, br) = match border.style {
        BorderStyle::Single => ('─', '│', '┌', '┐', '└', '┘'),
        BorderStyle::Double => ('═', '║', '╔', '╗', '╚', '╝'),
        BorderStyle::Rounded => ('─', '│', '╭', '╮', '╰', '╯'),
        BorderStyle::Dashed => ('╌', '╎', '┌', '┐', '└', '┘'),
    };
    let edge_style =
        |e: Option<EdgeStyle>, active: Option<smelt_style::theme::HlGroup>| -> super::grid::Style {
            let mut style = e
                .and_then(|s| s.color)
                .map(|hl| theme.resolve(hl))
                .unwrap_or_default();
            style.bg = None;
            if let Some(hl) = active {
                let mut active_style = theme.resolve(hl);
                active_style.bg = None;
                style = merge_title_span_style(style, active_style);
            }
            style
        };
    let top_style = edge_style(border.top, ctx.top);
    let title_style = edge_style(border.top, None);
    let bot_style = edge_style(border.bottom, ctx.bottom);
    let left_style = edge_style(border.left, ctx.left);
    let right_style = edge_style(border.right, ctx.right);
    let corner_style =
        |horizontal: super::grid::Style,
         horizontal_active: Option<smelt_style::theme::HlGroup>,
         vertical: super::grid::Style,
         vertical_active: Option<smelt_style::theme::HlGroup>| {
            if horizontal_active.is_some() {
                horizontal
            } else if vertical_active.is_some() {
                vertical
            } else {
                horizontal
            }
        };
    let top_left_style = corner_style(top_style, ctx.top, left_style, ctx.left);
    let top_right_style = corner_style(top_style, ctx.top, right_style, ctx.right);
    let bot_left_style = corner_style(bot_style, ctx.bottom, left_style, ctx.left);
    let bot_right_style = corner_style(bot_style, ctx.bottom, right_style, ctx.right);
    let right = area.left + area.width - 1;
    let bottom = area.top + area.height - 1;

    if border.top.is_some() {
        for col in area.left..=right {
            grid.set(col, area.top, h, top_style);
        }
    }
    if border.bottom.is_some() && bottom != area.top {
        for col in area.left..=right {
            grid.set(col, bottom, h, bot_style);
        }
    }
    if border.left.is_some() {
        for row in area.top..=bottom {
            grid.set(area.left, row, v, left_style);
        }
    }
    if border.right.is_some() && right != area.left {
        for row in area.top..=bottom {
            grid.set(right, row, v, right_style);
        }
    }
    // Corners only when both adjacent edges are present. Active side edges own
    // their endpoint corners; otherwise top/bottom keeps the existing priority.
    if border.top.is_some() && border.left.is_some() {
        grid.set(area.left, area.top, tl, top_left_style);
    }
    if border.top.is_some() && border.right.is_some() && right != area.left {
        grid.set(right, area.top, tr, top_right_style);
    }
    if border.bottom.is_some() && border.left.is_some() && bottom != area.top {
        grid.set(area.left, bottom, bl, bot_left_style);
    }
    if border.bottom.is_some() && border.right.is_some() && bottom != area.top && right != area.left
    {
        grid.set(right, bottom, br, bot_right_style);
    }

    if border.top.is_some() {
        if let Some(title) = chrome.title.as_ref() {
            // Inset title by one cell from each end so it reads as `─title──`
            // regardless of whether the left/right sides are enabled.
            let title_left = area.left + 1;
            let title_right_excl = right;
            if title_right_excl > title_left {
                let limit = title_right_excl;
                let mut col = title_left;
                for span in &title.spans {
                    if col >= limit {
                        break;
                    }
                    let span_style = merge_title_span_style(title_style, span.style);
                    let mut written = false;
                    for grapheme in smelt_style::cell_width::graphemes(span.text.as_ref()) {
                        let width = smelt_style::cell_width::text_width_u16(grapheme);
                        if col + width > limit {
                            break;
                        }
                        grid.set_symbol(col, area.top, grapheme, span_style);
                        col += width;
                        written = true;
                    }
                    if !written {
                        break;
                    }
                }
            }
        }
    }
}

/// Merge a title span's style over the chrome style: span fg/bg override when set; attrs OR.
fn merge_title_span_style(
    base: crate::grid::Style,
    span: crate::grid::Style,
) -> crate::grid::Style {
    crate::grid::Style {
        fg: span.fg.or(base.fg),
        bg: span.bg.or(base.bg),
        bold: base.bold || span.bold,
        dim: base.dim || span.dim,
        italic: base.italic || span.italic,
        underline: base.underline || span.underline,
        crossedout: base.crossedout || span.crossedout,
        reverse: base.reverse || span.reverse,
    }
}

fn resolve_container_nodes(
    node: &LayoutTree,
    area: Rect,
    sizer: &dyn LeafSizer,
    out: &mut HashMap<ContainerId, Rect>,
) {
    node.visit(area, sizer, &mut |node, area| {
        if let Some(id) = node.chrome().container {
            out.insert(id, area);
        }
    });
}

fn resolve_node_ordered(
    node: &LayoutTree,
    area: Rect,
    sizer: &dyn LeafSizer,
    out: &mut Vec<LayoutRect>,
) {
    node.visit(area, sizer, &mut |node, area| {
        if let LayoutTree::Leaf { id, chrome, .. } = node {
            out.push(LayoutRect {
                id: *id,
                rect: inset_for_chrome(area, chrome),
            });
        }
    });
}

/// Lay out a container's children. Returns `(inner_area, child_rects)`. Both
/// `resolve_box` and the renderer in `term::paint_layout_tree_with` call this
/// to keep their geometry in sync (hit-test rects must match painted rects).
pub fn layout_box_children(
    items: &[Item],
    chrome: &Chrome,
    area: Rect,
    vertical: bool,
    sizer: &dyn LeafSizer,
) -> (Rect, Vec<Rect>) {
    let inner = inset_for_chrome(area, chrome);
    let total_gap = chrome
        .gap
        .saturating_mul(items.len().saturating_sub(1) as u16);
    let primary_total = if vertical { inner.height } else { inner.width };
    let available = primary_total.saturating_sub(total_gap);

    // Compute `Fit` children's natural caps along the primary axis.
    let fit_caps: Vec<Option<u16>> = items
        .iter()
        .map(|(c, child)| match c {
            Constraint::Fit => {
                let leaf_cap = if vertical {
                    (inner.width, available)
                } else {
                    (available, inner.height)
                };
                let (nw, nh) = child.natural_size_with(leaf_cap, sizer);
                Some(if vertical { nh } else { nw })
            }
            _ => None,
        })
        .collect();

    let sizes = resolve_constraints_with_fit_caps(items, available, &fit_caps);
    let used_sizes = sizes
        .iter()
        .fold(0u16, |acc, size| acc.saturating_add(*size));
    let used = used_sizes.saturating_add(total_gap);
    let extra = primary_total.saturating_sub(used);
    let gap_count = items.len().saturating_sub(1) as u16;
    let (extra_per_gap, mut extra_remainder) =
        if chrome.justify == Justify::SpaceBetween && gap_count > 0 {
            (extra / gap_count, extra % gap_count)
        } else {
            (0, 0)
        };

    let mut rects = Vec::with_capacity(items.len());
    let mut offset = 0u16;
    for (i, &size) in sizes.iter().enumerate() {
        let r = if vertical {
            Rect::new(inner.top + offset, inner.left, inner.width, size)
        } else {
            Rect::new(inner.top, inner.left + offset, size, inner.height)
        };
        rects.push(r);
        offset = offset.saturating_add(size);
        if i + 1 < items.len() {
            offset = offset
                .saturating_add(chrome.gap)
                .saturating_add(extra_per_gap);
            if extra_remainder > 0 {
                offset = offset.saturating_add(1);
                extra_remainder -= 1;
            }
        }
    }
    (inner, rects)
}

pub fn resolve_constraints(items: &[Item], total: u16) -> Vec<u16> {
    let caps: Vec<Option<u16>> = vec![None; items.len()];
    resolve_constraints_with_fit_caps(items, total, &caps)
}

/// Resolve item sizes given pre-computed natural-size caps for `Fit` children.
/// `fit_caps[i]` is the natural size along the primary axis for a `Fit` child;
/// `None` means uncapped (used for non-Fit children or when the caller has no
/// sizer). Container resolvers (`resolve_box`, `paint_layout_tree_with`)
/// compute these and pass them through; the bare `resolve_constraints` wrapper
/// treats `Fit` as uncapped (equivalent to `Fill`).
pub fn resolve_constraints_with_fit_caps(
    items: &[Item],
    total: u16,
    fit_caps: &[Option<u16>],
) -> Vec<u16> {
    let mut sizes = vec![0u16; items.len()];
    let mut remaining = total;

    // Pass 1: hard claimants (`Length`, `Percentage`) take their exact share.
    for (i, (c, _)) in items.iter().enumerate() {
        match c {
            Constraint::Length(n) => {
                let n = (*n).min(remaining);
                sizes[i] = n;
                remaining -= n;
            }
            Constraint::Percentage(pct) => {
                let n = ((total as u32 * *pct as u32) / 100).min(remaining as u32) as u16;
                sizes[i] = n;
                remaining -= n;
            }
            _ => {}
        }
    }

    // Pass 2: `Ratio` siblings split a sub-pool proportionally.
    let ratio_total: u32 = items
        .iter()
        .filter_map(|(c, _)| match c {
            Constraint::Ratio(num, _) => Some(*num as u32),
            _ => None,
        })
        .sum();
    let ratio_pool = remaining;
    let mut consumed = 0u16;
    for (i, (c, _)) in items.iter().enumerate() {
        if let Constraint::Ratio(num, _) = c {
            let n = (ratio_pool as u32 * *num as u32)
                .checked_div(ratio_total)
                .unwrap_or(0) as u16;
            sizes[i] = n;
            consumed += n;
        }
    }
    remaining -= consumed.min(remaining);

    // Pass 3: elastic children (`Fill`, `Fit`, `Min`, `Max`) share the remainder.
    // Each elastic child has a `(floor, cap)`:
    //   * `Fill`     → `(0, MAX)`
    //   * `Fit`      → `(0, natural)`     - natural comes from `fit_caps`
    //   * `Min(n)`   → `(n, MAX)`
    //   * `Max(n)`   → `(0, n)`
    // Equal-share baseline; clamp to caps; surplus from cap-hit children
    // redistributes to siblings with headroom; floors raising the total over
    // budget claw back proportionally.
    let elastic: Vec<(usize, u16, u16)> = items
        .iter()
        .enumerate()
        .filter_map(|(i, (c, _))| {
            elastic_bounds(*c, fit_caps.get(i).copied().flatten()).map(|(f, cap)| (i, f, cap))
        })
        .collect();
    if elastic.is_empty() || remaining == 0 {
        // Still need to honor floors (`Min(n)`) even when no remainder - they
        // can claim space by displacing earlier hard claimants? No: passes
        // 1-2 already locked those in. Min with no remainder gets 0.
        return sizes;
    }

    // Distribute `remaining` across elastic children, never exceeding each
    // child's cap. Each round pours an equal portion to children with
    // headroom, clipping at their cap; any unallocated residue (because
    // someone's headroom was smaller than its share) rolls into the next
    // round and goes to the remaining uncapped siblings. Terminates when
    // either the budget is exhausted or every child is at its cap.
    let mut shares = vec![0u16; elastic.len()];
    let caps: Vec<u32> = elastic.iter().map(|&(_, _, c)| c as u32).collect();
    let mut to_allocate = remaining as u32;
    loop {
        let uncapped: Vec<usize> = (0..elastic.len())
            .filter(|&k| (shares[k] as u32) < caps[k])
            .collect();
        if uncapped.is_empty() || to_allocate == 0 {
            break;
        }
        let m = uncapped.len() as u32;
        let per = to_allocate / m;
        let mut leftover = to_allocate % m;
        let mut allocated: u32 = 0;
        for &k in &uncapped {
            let want = per + u32::from(leftover > 0);
            leftover = leftover.saturating_sub(1);
            let room = caps[k] - shares[k] as u32;
            let take = want.min(room);
            shares[k] = shares[k].saturating_add(take as u16);
            allocated += take;
        }
        if allocated == 0 {
            break; // every remaining uncapped child has zero room (defensive)
        }
        to_allocate = to_allocate.saturating_sub(allocated);
    }

    // Apply floors. If floors push total over `remaining`, take back from
    // non-floored children first, then proportionally from floored ones.
    for (k, &(_, floor, _)) in elastic.iter().enumerate() {
        if shares[k] < floor {
            shares[k] = floor;
        }
    }
    let total_shares: u32 = shares.iter().map(|&v| v as u32).sum();
    if total_shares > remaining as u32 {
        let mut over = (total_shares - remaining as u32) as u16;
        for (k, &(_, floor, _)) in elastic.iter().enumerate() {
            if over == 0 {
                break;
            }
            if floor == 0 {
                let take = shares[k].min(over);
                shares[k] -= take;
                over -= take;
            }
        }
        if over > 0 {
            let floored_total: u32 = elastic
                .iter()
                .enumerate()
                .filter(|(_, &(_, f, _))| f > 0)
                .map(|(k, _)| shares[k] as u32)
                .sum();
            if let Some(divisor) = std::num::NonZeroU32::new(floored_total) {
                for (k, &(_, f, _)) in elastic.iter().enumerate() {
                    if f > 0 {
                        let take = ((shares[k] as u32 * over as u32) / divisor) as u16;
                        shares[k] = shares[k].saturating_sub(take);
                    }
                }
                // Mop up rounding residual from any floored child.
                let new_total: u32 = shares.iter().map(|&v| v as u32).sum();
                let mut residual = new_total.saturating_sub(remaining as u32) as u16;
                for (k, &(_, f, _)) in elastic.iter().enumerate() {
                    if residual == 0 {
                        break;
                    }
                    if f > 0 {
                        let take = shares[k].min(residual);
                        shares[k] -= take;
                        residual -= take;
                    }
                }
            }
        }
    }

    for (k, &(i, _, _)) in elastic.iter().enumerate() {
        sizes[i] = shares[k];
    }
    sizes
}

/// `(floor, cap)` bounds for an elastic constraint. `Length`/`Percentage`/`Ratio`
/// return `None` - they're hard claimants resolved in earlier passes.
fn elastic_bounds(c: Constraint, fit_cap: Option<u16>) -> Option<(u16, u16)> {
    match c {
        Constraint::Fill => Some((0, u16::MAX)),
        Constraint::Fit => Some((0, fit_cap.unwrap_or(u16::MAX))),
        Constraint::Min(n) => Some((n, u16::MAX)),
        Constraint::Max(n) => Some((0, n)),
        Constraint::Length(_) | Constraint::Percentage(_) | Constraint::Ratio(_, _) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: PaintId = PaintId(100);
    const B: PaintId = PaintId(101);
    const C: PaintId = PaintId(102);

    #[test]
    fn gutters_content_width_reserves_data_gutter_and_scrollbar() {
        let gutters = Gutters {
            pad_left: 2,
            pad_right: 1,
            scrollbar: true,
        };
        assert_eq!(gutters.content_width_with_gutter(20, 3), 13);
    }

    #[test]
    fn gutters_content_width_saturates_when_gutter_consumes_area() {
        let gutters = Gutters {
            pad_left: 2,
            pad_right: 1,
            scrollbar: true,
        };
        assert_eq!(gutters.content_width_with_gutter(4, 4), 0);
    }

    #[test]
    fn single_leaf_fills_area() {
        let tree = LayoutTree::leaf(A);
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result[&A], Rect::new(0, 0, 80, 24));
    }

    #[test]
    fn vertical_split_fixed_and_fill() {
        let tree = LayoutTree::vbox(vec![
            (Constraint::Fill, LayoutTree::leaf(A)),
            (Constraint::Length(5), LayoutTree::leaf(B)),
        ]);
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result[&A], Rect::new(0, 0, 80, 19));
        assert_eq!(result[&B], Rect::new(19, 0, 80, 5));
    }

    #[test]
    fn vertical_space_between_puts_surplus_into_gap() {
        let tree = LayoutTree::vbox(vec![
            (Constraint::Length(2), LayoutTree::leaf(A)),
            (Constraint::Length(3), LayoutTree::leaf(B)),
        ])
        .with_gap(1)
        .with_justify(Justify::SpaceBetween);
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 10));
        assert_eq!(result[&A], Rect::new(0, 0, 80, 2));
        assert_eq!(result[&B], Rect::new(7, 0, 80, 3));
    }

    #[test]
    fn vertical_split_pct_and_fill() {
        let tree = LayoutTree::vbox(vec![
            (Constraint::Fill, LayoutTree::leaf(A)),
            (Constraint::Percentage(25), LayoutTree::leaf(B)),
        ]);
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result[&B].height, 6);
        assert_eq!(result[&A].height, 18);
    }

    #[test]
    fn horizontal_split() {
        let tree = LayoutTree::hbox(vec![
            (Constraint::Length(20), LayoutTree::leaf(A)),
            (Constraint::Fill, LayoutTree::leaf(B)),
        ]);
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result[&A], Rect::new(0, 0, 20, 24));
        assert_eq!(result[&B], Rect::new(0, 20, 60, 24));
    }

    #[test]
    fn multiple_fills_distribute_evenly() {
        let tree = LayoutTree::vbox(vec![
            (Constraint::Fill, LayoutTree::leaf(A)),
            (Constraint::Fill, LayoutTree::leaf(B)),
        ]);
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result[&A].height, 12);
        assert_eq!(result[&B].height, 12);
    }

    #[test]
    fn rect_contains() {
        let r = Rect::new(5, 10, 20, 10);
        assert!(r.contains(5, 10));
        assert!(r.contains(14, 29));
        assert!(!r.contains(15, 10));
        assert!(!r.contains(5, 30));
    }

    #[test]
    fn nested_split() {
        let tree = LayoutTree::vbox(vec![
            (
                Constraint::Fill,
                LayoutTree::hbox(vec![
                    (Constraint::Fill, LayoutTree::leaf(A)),
                    (Constraint::Fill, LayoutTree::leaf(B)),
                ]),
            ),
            (Constraint::Length(4), LayoutTree::leaf(C)),
        ]);
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result[&C], Rect::new(20, 0, 80, 4));
        assert_eq!(result[&A], Rect::new(0, 0, 40, 20));
        assert_eq!(result[&B], Rect::new(0, 40, 40, 20));
    }

    #[test]
    fn ordered_resolution_preserves_repeated_ids() {
        let tree = LayoutTree::vbox(vec![
            (Constraint::Length(1), LayoutTree::leaf(A)),
            (Constraint::Length(1), LayoutTree::leaf(A)),
        ]);
        let result = resolve_layout_ordered(&tree, Rect::new(0, 0, 10, 2));
        assert_eq!(
            result,
            vec![
                LayoutRect {
                    id: A,
                    rect: Rect::new(0, 0, 10, 1),
                },
                LayoutRect {
                    id: A,
                    rect: Rect::new(1, 0, 10, 1),
                },
            ]
        );
    }

    #[test]
    fn map_resolution_keeps_last_repeated_id() {
        let tree = LayoutTree::vbox(vec![
            (Constraint::Length(1), LayoutTree::leaf(A)),
            (Constraint::Length(1), LayoutTree::leaf(A)),
        ]);
        let result = resolve_layout(&tree, Rect::new(0, 0, 10, 2));
        assert_eq!(result[&A], Rect::new(1, 0, 10, 1));
    }

    #[test]
    fn min_competes_with_fill_for_equal_share_when_floor_satisfied() {
        let tree = LayoutTree::vbox(vec![
            (Constraint::Min(3), LayoutTree::leaf(A)),
            (Constraint::Fill, LayoutTree::leaf(B)),
        ]);
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result[&A].height, 12);
        assert_eq!(result[&B].height, 12);
    }

    #[test]
    fn min_clamps_up_to_floor_when_equal_share_too_small() {
        let tree = LayoutTree::vbox(vec![
            (Constraint::Min(20), LayoutTree::leaf(A)),
            (Constraint::Fill, LayoutTree::leaf(B)),
        ]);
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result[&A].height, 20);
        assert_eq!(result[&B].height, 4);
    }

    #[test]
    fn min_zero_alone_consumes_all_remaining() {
        let tree = LayoutTree::vbox(vec![(Constraint::Min(0), LayoutTree::leaf(A))]);
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result[&A].height, 24);
    }

    #[test]
    fn min_with_length_sibling_takes_remainder() {
        let tree = LayoutTree::vbox(vec![
            (Constraint::Length(10), LayoutTree::leaf(A)),
            (Constraint::Min(0), LayoutTree::leaf(B)),
        ]);
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result[&A].height, 10);
        assert_eq!(result[&B].height, 14);
    }

    #[test]
    fn two_mins_split_evenly_when_total_overruns_floors() {
        let tree = LayoutTree::vbox(vec![
            (Constraint::Min(20), LayoutTree::leaf(A)),
            (Constraint::Min(20), LayoutTree::leaf(B)),
        ]);
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result[&A].height + result[&B].height, 24);
        assert!((result[&A].height as i32 - result[&B].height as i32).abs() <= 1);
    }

    #[test]
    fn max_caps_at_ceiling_when_parent_has_room() {
        let tree = LayoutTree::vbox(vec![
            (Constraint::Max(5), LayoutTree::leaf(A)),
            (Constraint::Fill, LayoutTree::leaf(B)),
        ]);
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result[&A].height, 5);
        assert_eq!(result[&B].height, 19);
    }

    #[test]
    fn max_shrinks_when_parent_smaller_than_ceiling() {
        let tree = LayoutTree::vbox(vec![(Constraint::Max(50), LayoutTree::leaf(A))]);
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result[&A].height, 24);
    }

    #[test]
    fn ratio_splits_remaining_proportionally() {
        let tree = LayoutTree::hbox(vec![
            (Constraint::Ratio(1, 3), LayoutTree::leaf(A)),
            (Constraint::Ratio(2, 3), LayoutTree::leaf(B)),
        ]);
        let result = resolve_layout(&tree, Rect::new(0, 0, 90, 24));
        assert_eq!(result[&A].width, 30);
        assert_eq!(result[&B].width, 60);
    }

    #[test]
    fn ratio_competes_with_length_for_remaining() {
        let tree = LayoutTree::hbox(vec![
            (Constraint::Length(20), LayoutTree::leaf(A)),
            (Constraint::Ratio(1, 2), LayoutTree::leaf(B)),
            (Constraint::Ratio(1, 2), LayoutTree::leaf(C)),
        ]);
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result[&A].width, 20);
        assert_eq!(result[&B].width, 30);
        assert_eq!(result[&C].width, 30);
    }

    #[test]
    fn fit_with_noop_sizer_contributes_zero() {
        let tree = LayoutTree::vbox(vec![
            (Constraint::Fit, LayoutTree::leaf(A)),
            (Constraint::Fill, LayoutTree::leaf(B)),
        ]);
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        // `NoopSizer`: A reports 0 → `Fit` claims 0, `Fill` takes the rest.
        assert_eq!(result[&A].height, 0);
        assert_eq!(result[&B].height, 24);
    }

    struct FixedSizer(u16);

    impl LeafSizer for FixedSizer {
        fn leaf_natural_size(&self, _id: PaintId, _cap: (u16, u16)) -> (u16, u16) {
            (0, self.0)
        }
    }

    /// Per-leaf-id natural heights, capped at the available extent.
    struct PerLeafSizer(std::collections::HashMap<PaintId, u16>);

    impl LeafSizer for PerLeafSizer {
        fn leaf_natural_size(&self, id: PaintId, cap: (u16, u16)) -> (u16, u16) {
            (0, self.0.get(&id).copied().unwrap_or(0).min(cap.1))
        }
    }

    /// Confirm-dialog-shaped vbox: small leaf Fits + one elastic Fit panel
    /// (the preview). At every terminal height the panels must collectively
    /// consume the entire dialog inner area - no leftover rows below the last
    /// panel (that's the "3 whitespaces under reason" bug).
    #[test]
    fn confirm_dialog_layout_consumes_all_rows_at_varying_heights() {
        let header = PaintId(101);
        let preview = PaintId(102);
        let allow = PaintId(103);
        let options = PaintId(104);
        let spacer = PaintId(105);
        let reason = PaintId(106);

        let mut naturals = std::collections::HashMap::new();
        naturals.insert(header, 1);
        naturals.insert(preview, 50); // big content (file diff/text)
        naturals.insert(allow, 1);
        naturals.insert(options, 4);
        naturals.insert(spacer, 1);
        naturals.insert(reason, 1);
        let sizer = PerLeafSizer(naturals);

        let tree = LayoutTree::vbox(vec![
            (Constraint::Fit, LayoutTree::leaf(header)),
            (Constraint::Fit, LayoutTree::leaf(preview)),
            (Constraint::Fit, LayoutTree::leaf(allow)),
            (Constraint::Fit, LayoutTree::leaf(options)),
            (Constraint::Fit, LayoutTree::leaf(spacer)),
            (Constraint::Fit, LayoutTree::leaf(reason)),
        ]);

        for h in [8u16, 10, 12, 15, 18, 20, 24, 30, 40] {
            let result = resolve_layout_with(&tree, Rect::new(0, 0, 80, h), &sizer);
            let used: u16 = result.values().map(|r| r.height).sum();
            assert_eq!(
                used,
                h,
                "h={h}: panels used {used} rows, leaving {} unused",
                h - used
            );
            // Smalls keep their natural; preview absorbs the slack.
            assert_eq!(result[&header].height, 1, "h={h}: header");
            assert_eq!(result[&allow].height, 1, "h={h}: allow");
            assert_eq!(result[&spacer].height, 1, "h={h}: spacer");
            assert_eq!(result[&reason].height, 1, "h={h}: reason");
        }
    }

    /// Same layout, but the preview has no content (`bash`-style confirm).
    /// Without an elastic outlet for surplus, smaller terminals must still
    /// pack the smalls tightly with zero whitespace below the last panel.
    #[test]
    fn confirm_dialog_no_preview_packs_tight_at_varying_heights() {
        let header = PaintId(101);
        let preview = PaintId(102);
        let allow = PaintId(103);
        let options = PaintId(104);
        let spacer = PaintId(105);
        let reason = PaintId(106);

        let mut naturals = std::collections::HashMap::new();
        naturals.insert(header, 1);
        naturals.insert(preview, 0); // collapses (no content)
        naturals.insert(allow, 1);
        naturals.insert(options, 4);
        naturals.insert(spacer, 1);
        naturals.insert(reason, 1);
        let sizer = PerLeafSizer(naturals);

        let tree = LayoutTree::vbox(vec![
            (Constraint::Fit, LayoutTree::leaf(header)),
            (Constraint::Fit, LayoutTree::leaf(preview)),
            (Constraint::Fit, LayoutTree::leaf(allow)),
            (Constraint::Fit, LayoutTree::leaf(options)),
            (Constraint::Fit, LayoutTree::leaf(spacer)),
            (Constraint::Fit, LayoutTree::leaf(reason)),
        ]);

        // Natural sum is 8; with the dialog sized via max_height it picks
        // min(8, terminal). For terminals >= 8 the dialog should be 8 tall
        // and every panel takes its natural.
        for h in [8u16, 10, 12, 15, 20, 24] {
            let nat = tree.natural_size_with((80, h), &sizer);
            assert_eq!(nat.1, 8, "h={h}: dialog natural should equal sum-of-smalls");
            let dialog_h = nat.1.min(h);
            let result = resolve_layout_with(&tree, Rect::new(0, 0, 80, dialog_h), &sizer);
            let used: u16 = result.values().map(|r| r.height).sum();
            assert_eq!(used, dialog_h, "h={h}: total {used} != dialog_h {dialog_h}");
        }
    }

    #[test]
    fn fit_with_sizer_uses_leaf_natural_height() {
        let tree = LayoutTree::vbox(vec![
            (Constraint::Fit, LayoutTree::leaf(A)),
            (Constraint::Fill, LayoutTree::leaf(B)),
        ]);
        let sizer = FixedSizer(3);
        let result = resolve_layout_with(&tree, Rect::new(0, 0, 80, 24), &sizer);
        assert_eq!(result[&A].height, 3, "Fit claims sizer-reported natural");
        assert_eq!(result[&B].height, 21, "Fill takes the remainder");
    }

    #[test]
    fn fit_shares_with_fill_when_sizer_overflows() {
        // `Fit` is elastic-with-cap; when its natural exceeds the budget it
        // still shares fairly with `Fill` siblings instead of starving them.
        let tree = LayoutTree::vbox(vec![
            (Constraint::Fit, LayoutTree::leaf(A)),
            (Constraint::Fill, LayoutTree::leaf(B)),
        ]);
        // Sizer reports 50, but parent only has 10 rows.
        let sizer = FixedSizer(50);
        let result = resolve_layout_with(&tree, Rect::new(0, 0, 80, 10), &sizer);
        assert_eq!(result[&A].height, 5);
        assert_eq!(result[&B].height, 5);
    }

    #[test]
    fn natural_size_with_sizer_reports_leaf_height() {
        let tree = LayoutTree::vbox(vec![(Constraint::Fit, LayoutTree::leaf(A))]);
        let sizer = FixedSizer(5);
        assert_eq!(tree.natural_size_with((80, 24), &sizer), (0, 5));
    }

    #[test]
    fn natural_size_fill_contributes_zero_with_sizer() {
        let tree = LayoutTree::vbox(vec![(Constraint::Fill, LayoutTree::leaf(A))]);
        let sizer = FixedSizer(5);
        // Fill is elastic - even with a sizer reporting 5, it contributes 0
        // to the parent's natural-size demand.
        assert_eq!(tree.natural_size_with((80, 24), &sizer), (0, 0));
    }

    #[test]
    fn zero_height_produces_empty_rects() {
        let tree = LayoutTree::vbox(vec![
            (Constraint::Length(30), LayoutTree::leaf(A)),
            (Constraint::Fill, LayoutTree::leaf(B)),
        ]);
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 10));
        assert_eq!(result[&A].height, 10);
        assert_eq!(result[&B].height, 0);
    }

    #[test]
    fn gap_inserts_spacing_between_children() {
        let tree = LayoutTree::vbox(vec![
            (Constraint::Fill, LayoutTree::leaf(A)),
            (Constraint::Fill, LayoutTree::leaf(B)),
            (Constraint::Fill, LayoutTree::leaf(C)),
        ])
        .with_gap(2);
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result[&A], Rect::new(0, 0, 80, 7));
        assert_eq!(result[&B].top, 9);
        assert_eq!(result[&C].top, 18);
    }

    #[test]
    fn border_insets_children_by_one_each_side() {
        let tree = LayoutTree::vbox(vec![(Constraint::Fill, LayoutTree::leaf(A))])
            .with_border(Border::SINGLE);
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result[&A], Rect::new(1, 1, 78, 22));
    }

    #[test]
    fn border_and_gap_compose() {
        let tree = LayoutTree::vbox(vec![
            (Constraint::Fill, LayoutTree::leaf(A)),
            (Constraint::Fill, LayoutTree::leaf(B)),
        ])
        .with_border(Border::SINGLE)
        .with_gap(1)
        .with_title("dialog");
        let result = resolve_layout(&tree, Rect::new(0, 0, 80, 24));
        assert_eq!(result[&A].top, 1);
        assert_eq!(result[&A].height + result[&B].height, 21);
        assert_eq!(result[&B].top, result[&A].top + result[&A].height + 1);
    }

    #[test]
    fn natural_size_leaf_is_zero() {
        let tree = LayoutTree::leaf(A);
        assert_eq!(tree.natural_size((80, 24)), (0, 0));
    }

    #[test]
    fn natural_size_vbox_lengths_sum_along_primary() {
        let tree = LayoutTree::vbox(vec![
            (Constraint::Length(5), LayoutTree::leaf(A)),
            (Constraint::Length(5), LayoutTree::leaf(B)),
        ]);
        assert_eq!(tree.natural_size((80, 24)), (0, 10));
    }

    #[test]
    fn natural_size_hbox_lengths_sum_along_primary() {
        let tree = LayoutTree::hbox(vec![
            (Constraint::Length(20), LayoutTree::leaf(A)),
            (Constraint::Length(10), LayoutTree::leaf(B)),
        ]);
        assert_eq!(tree.natural_size((80, 24)), (30, 0));
    }

    #[test]
    fn natural_size_vbox_gap_adds_to_primary() {
        let tree = LayoutTree::vbox(vec![
            (Constraint::Length(3), LayoutTree::leaf(A)),
            (Constraint::Length(4), LayoutTree::leaf(B)),
            (Constraint::Length(5), LayoutTree::leaf(C)),
        ])
        .with_gap(2);
        assert_eq!(tree.natural_size((80, 24)), (0, 16));
    }

    #[test]
    fn natural_size_border_adds_two_each_axis() {
        let tree = LayoutTree::vbox(vec![(Constraint::Length(10), LayoutTree::leaf(A))])
            .with_border(Border::SINGLE);
        assert_eq!(tree.natural_size((80, 24)), (2, 12));
    }

    #[test]
    fn natural_size_percentage_resolves_against_cap() {
        let tree = LayoutTree::vbox(vec![(Constraint::Percentage(50), LayoutTree::leaf(A))]);
        assert_eq!(tree.natural_size((80, 24)), (0, 12));
    }

    #[test]
    fn natural_size_ratio_resolves_against_cap() {
        let tree = LayoutTree::hbox(vec![
            (Constraint::Ratio(1, 4), LayoutTree::leaf(A)),
            (Constraint::Ratio(1, 4), LayoutTree::leaf(B)),
        ]);
        assert_eq!(tree.natural_size((80, 24)), (40, 0));
    }

    #[test]
    fn natural_size_fill_contributes_zero() {
        let tree = LayoutTree::vbox(vec![
            (Constraint::Length(3), LayoutTree::leaf(A)),
            (Constraint::Fill, LayoutTree::leaf(B)),
        ]);
        assert_eq!(tree.natural_size((80, 24)), (0, 3));
    }

    #[test]
    fn natural_size_clamps_to_cap() {
        let tree = LayoutTree::vbox(vec![(Constraint::Length(100), LayoutTree::leaf(A))]);
        assert_eq!(tree.natural_size((80, 24)), (0, 24));
    }

    #[test]
    fn leaves_in_order_walks_depth_first() {
        let tree = LayoutTree::vbox(vec![
            (Constraint::Fill, LayoutTree::leaf(A)),
            (
                Constraint::Length(5),
                LayoutTree::hbox(vec![
                    (Constraint::Fill, LayoutTree::leaf(B)),
                    (Constraint::Fill, LayoutTree::leaf(C)),
                ]),
            ),
        ]);
        assert_eq!(tree.leaves_in_order(), vec![A, B, C]);
    }

    #[test]
    fn leaves_in_order_single_leaf() {
        let tree = LayoutTree::leaf(A);
        assert_eq!(tree.leaves_in_order(), vec![A]);
    }

    #[test]
    fn leaf_carries_its_own_chrome() {
        let tree = LayoutTree::leaf(A)
            .with_border(Border::SINGLE)
            .with_title("hi");
        assert_eq!(tree.leaves_in_order(), vec![A]);
        assert!(tree.contains_leaf(A));
        match &tree {
            LayoutTree::Leaf { chrome, .. } => {
                assert!(chrome.border.is_some());
                assert!(chrome.title.is_some());
            }
            _ => panic!("expected Leaf with chrome"),
        }
    }

    #[test]
    fn leaf_with_chrome_resolves_inside_inset_rect() {
        let tree = LayoutTree::leaf(A).with_border(Border::SINGLE);
        let area = Rect::new(0, 0, 10, 6);
        let rects = resolve_layout(&tree, area);
        let inner = rects.get(&A).copied().expect("leaf rect resolved");
        assert_eq!(inner, Rect::new(1, 1, 8, 4));
    }

    #[test]
    fn contains_leaf_finds_direct_leaf() {
        let tree = LayoutTree::leaf(A);
        assert!(tree.contains_leaf(A));
        assert!(!tree.contains_leaf(B));
    }

    #[test]
    fn contains_leaf_walks_nested_containers() {
        let tree = LayoutTree::vbox(vec![
            (Constraint::Fill, LayoutTree::leaf(A)),
            (
                Constraint::Length(5),
                LayoutTree::hbox(vec![(Constraint::Fill, LayoutTree::leaf(B))]),
            ),
        ]);
        assert!(tree.contains_leaf(A));
        assert!(tree.contains_leaf(B));
        assert!(!tree.contains_leaf(C));
    }

    #[test]
    fn natural_size_nested_chrome_composes() {
        let tree = LayoutTree::vbox(vec![(
            Constraint::Length(5),
            LayoutTree::hbox(vec![
                (Constraint::Length(20), LayoutTree::leaf(A)),
                (Constraint::Length(10), LayoutTree::leaf(B)),
            ]),
        )])
        .with_border(Border::SINGLE);
        assert_eq!(tree.natural_size((80, 24)), (32, 7));
    }

    #[test]
    fn paint_chrome_no_border_is_noop() {
        let mut grid = crate::grid::Grid::new(10, 5);
        let chrome = Chrome::default();
        paint_chrome(
            &mut grid,
            Rect::new(0, 0, 10, 5),
            &chrome,
            &crate::Theme::default(),
        );
        assert_eq!(grid.cell(0, 0).symbol, ' ');
    }

    #[test]
    fn paint_chrome_single_border_draws_corners_and_edges() {
        let mut grid = crate::grid::Grid::new(10, 5);
        let chrome = Chrome {
            border: Some(Border::SINGLE),
            ..Chrome::default()
        };
        paint_chrome(
            &mut grid,
            Rect::new(0, 0, 10, 5),
            &chrome,
            &crate::Theme::default(),
        );
        assert_eq!(grid.cell(0, 0).symbol, '┌');
        assert_eq!(grid.cell(9, 0).symbol, '┐');
        assert_eq!(grid.cell(0, 4).symbol, '└');
        assert_eq!(grid.cell(9, 4).symbol, '┘');
        assert_eq!(grid.cell(5, 0).symbol, '─');
        assert_eq!(grid.cell(0, 2).symbol, '│');
    }

    #[test]
    fn paint_chrome_with_context_overrides_active_edges() {
        let mut grid = crate::grid::Grid::new(10, 5);
        let mut theme = crate::Theme::default();
        theme.set(
            "ActiveEdge",
            crate::grid::Style {
                fg: Some(crate::grid::Color::Yellow),
                ..crate::grid::Style::default()
            },
        );
        let active = theme.id_for("ActiveEdge");
        let chrome = Chrome {
            border: Some(Border::SINGLE),
            ..Chrome::default()
        };
        paint_chrome_with(
            &mut grid,
            Rect::new(0, 0, 10, 5),
            &chrome,
            &theme,
            ChromePaintCtx {
                right: Some(active),
                bottom: Some(active),
                ..ChromePaintCtx::empty()
            },
        );
        assert_eq!(grid.cell(9, 2).style.fg, Some(crate::grid::Color::Yellow));
        assert_eq!(grid.cell(9, 0).style.fg, Some(crate::grid::Color::Yellow));
        assert_eq!(grid.cell(9, 4).style.fg, Some(crate::grid::Color::Yellow));
        assert_eq!(grid.cell(5, 4).style.fg, Some(crate::grid::Color::Yellow));
        assert_eq!(grid.cell(0, 4).style.fg, Some(crate::grid::Color::Yellow));
        assert_eq!(grid.cell(5, 0).style.fg, None);
        assert_eq!(grid.cell(0, 0).style.fg, None);
    }

    #[test]
    fn paint_chrome_with_context_keeps_title_on_base_style() {
        let mut grid = crate::grid::Grid::new(12, 4);
        let mut theme = crate::Theme::default();
        theme.set(
            "ActiveEdge",
            crate::grid::Style {
                fg: Some(crate::grid::Color::Yellow),
                ..crate::grid::Style::default()
            },
        );
        let active = theme.id_for("ActiveEdge");
        let chrome = Chrome {
            border: Some(Border::SINGLE),
            title: Some("title".into()),
            ..Chrome::default()
        };
        paint_chrome_with(
            &mut grid,
            Rect::new(0, 0, 12, 4),
            &chrome,
            &theme,
            ChromePaintCtx {
                top: Some(active),
                ..ChromePaintCtx::empty()
            },
        );

        assert_eq!(grid.cell(7, 0).style.fg, Some(crate::grid::Color::Yellow));
        assert_eq!(grid.cell(1, 0).symbol, 't');
        assert_eq!(grid.cell(1, 0).style.fg, None);
    }

    #[test]
    fn paint_chrome_title_paints_styled_spans() {
        use crate::grid::Color;
        use crate::line::{Line, Span};
        let mut grid = crate::grid::Grid::new(20, 3);
        let red = crate::grid::Style::new().fg(Color::Red);
        let chrome = Chrome {
            border: Some(Border::ROUNDED),
            title: Some(Line::from_spans([
                Span::raw("ok "),
                Span::styled("FAIL", red),
            ])),
            ..Chrome::default()
        };
        paint_chrome(
            &mut grid,
            Rect::new(0, 0, 20, 3),
            &chrome,
            &crate::Theme::default(),
        );
        assert_eq!(grid.cell(1, 0).symbol, 'o');
        assert_eq!(grid.cell(1, 0).style.fg, None);
        assert_eq!(grid.cell(4, 0).symbol, 'F');
        assert_eq!(grid.cell(4, 0).style.fg, Some(Color::Red));
    }

    #[test]
    fn paint_chrome_title_lands_on_top_border() {
        let mut grid = crate::grid::Grid::new(20, 5);
        let chrome = Chrome {
            border: Some(Border::ROUNDED),
            title: Some("hello".into()),
            ..Chrome::default()
        };
        paint_chrome(
            &mut grid,
            Rect::new(0, 0, 20, 5),
            &chrome,
            &crate::Theme::default(),
        );
        assert_eq!(grid.cell(0, 0).symbol, '╭');
        assert_eq!(grid.cell(1, 0).symbol, 'h');
        assert_eq!(grid.cell(5, 0).symbol, 'o');
        assert_eq!(grid.cell(6, 0).symbol, '─');
    }

    #[test]
    fn paint_chrome_truncates_title_to_inner_width() {
        let mut grid = crate::grid::Grid::new(8, 3);
        let chrome = Chrome {
            border: Some(Border::SINGLE),
            title: Some("muchtoolong".into()),
            ..Chrome::default()
        };
        paint_chrome(
            &mut grid,
            Rect::new(0, 0, 8, 3),
            &chrome,
            &crate::Theme::default(),
        );
        assert_eq!(grid.cell(0, 0).symbol, '┌');
        assert_eq!(grid.cell(1, 0).symbol, 'm');
        assert_eq!(grid.cell(6, 0).symbol, 'o');
        assert_eq!(grid.cell(7, 0).symbol, '┐');
    }

    #[test]
    fn resolves_stable_container_outer_rect() {
        let container = ContainerId(7);
        let tree = LayoutTree::vbox(vec![
            (Constraint::Length(3), LayoutTree::leaf(A)),
            (
                Constraint::Length(5),
                LayoutTree::leaf(B)
                    .with_border(Border::SINGLE)
                    .with_container(container),
            ),
        ]);

        let rects = resolve_containers_with(&tree, Rect::new(2, 4, 20, 8), &NoopSizer);

        assert_eq!(rects.get(&container), Some(&Rect::new(5, 4, 20, 5)));
        assert_eq!(
            resolve_layout(&tree, Rect::new(2, 4, 20, 8))[&B],
            Rect::new(6, 5, 18, 3)
        );
    }
}
