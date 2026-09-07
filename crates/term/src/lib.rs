//! Pure terminal renderer: double-buffered diff/flush, `LayoutTree`, `Grid`,
//! `paint_chrome`, and half-block-friendly cell primitives.
//! Editor concepts (`Window`, `Buffer`, overlays) live in `smelt-edit`.
//!
//! Key entry points:
//! - [`Compositor::render_with`] - drive a frame.
//! - [`paint_layout_tree`] - walk a [`LayoutTree`] and dispatch leaves.
//! - [`flush_diff`] - emit SGR escapes for a `Grid` diff.
//! - [`Grid`] / [`GridSlice`] - `set` / `put_str` (full overwrite; string
//!   writes return the clipped end column), `put_char` / `put_str_fg` /
//!   `put_line` (preserve bg where applicable).
//! - [`TerminalSession`] - raw-mode/alternate-screen lifecycle guard with
//!   suspend support for shell-outs.

pub mod ansi;
pub mod compositor;
pub mod flush;
pub mod geometry;
pub mod grid;
pub mod hit;
pub mod layout;
pub mod line;
pub mod session;
pub mod snapshot;
pub mod surface;

pub use compositor::Compositor;
pub use flush::flush_diff;
pub use geometry::Insets;
pub use grid::{
    display_width, truncate_width, Cell, CellSymbol, CellUpdate, Grid, GridSlice, Style, TextAlign,
};
pub use hit::HitRegistry;
pub use layout::{
    resolve_containers_with, resolve_layout, resolve_layout_ordered, resolve_layout_ordered_with,
    resolve_layout_with, Align, Axis, Border, Constraint, ContainerId, Corner, DividerStyles,
    Gutters, LayoutPaintOp, LayoutRect, LayoutStyle, LayoutTree, LeafSizer, Natural, NaturalRef,
    NoopSizer, PaintId, Rect, ResolvedLayout, ResolvedSplit, Split, SplitId, SplitInteraction,
    SplitOptions, SplitPane, SplitPhase, SplitRatio, SplitResizeMode, SplitResponse, SplitSize,
    StaticNatural,
};
pub use line::{Line, Span};
pub use session::{SuspendScreen, TerminalSession, TerminalSessionBuilder};
pub use smelt_style::style::Color;
pub use smelt_style::theme::Theme;
pub use snapshot::SnapshotFrame;
pub use surface::Surface;

/// Per-leaf paint callback: `(paint_id, leaf_rect, grid, theme, terminal_size)`.
/// The renderer calls this for each resolved [`LayoutTree::Leaf`].
pub type PaintDispatch<'a> =
    dyn FnMut(PaintId, Rect, &mut Grid, &std::sync::Arc<Theme>, (u16, u16)) + 'a;

pub struct PaintLayoutOptions<'a> {
    pub sizer: &'a dyn layout::LeafSizer,
    pub style: LayoutStyle,
}

/// Resolve `node` once and visit chrome and leaves in painter order.
pub fn walk_layout_tree_with(
    node: &LayoutTree,
    area: Rect,
    sizer: &dyn layout::LeafSizer,
    mut visit: impl FnMut(LayoutPaintOp),
) {
    for operation in node.resolve(area, sizer).into_operations() {
        visit(operation);
    }
}

/// Walk `node` against `area`, paint chrome on containers, and dispatch
/// each resolved leaf rect to `paint`. `Fit` constraints use the default
/// `NoopSizer` (contribute `0`); use `paint_layout_tree_with` to drive
/// content-aware sizing.
pub fn paint_layout_tree(
    grid: &mut Grid,
    theme: &std::sync::Arc<Theme>,
    node: &LayoutTree,
    area: Rect,
    term_size: (u16, u16),
    paint: &mut PaintDispatch,
) {
    paint_layout_tree_with(
        grid,
        theme,
        node,
        area,
        term_size,
        &layout::NoopSizer,
        paint,
    );
}

/// Like [`paint_layout_tree`] but uses `sizer` to resolve `Fit` constraints
/// against each leaf's natural size. Must use the same sizer as the rect
/// resolution that drives hit-testing and viewport setup, so painted rects
/// match.
pub fn paint_layout_tree_with(
    grid: &mut Grid,
    theme: &std::sync::Arc<Theme>,
    node: &LayoutTree,
    area: Rect,
    term_size: (u16, u16),
    sizer: &dyn layout::LeafSizer,
    paint: &mut PaintDispatch,
) {
    paint_layout_tree_with_options(
        grid,
        theme,
        node,
        area,
        term_size,
        PaintLayoutOptions {
            sizer,
            style: LayoutStyle::default(),
        },
        paint,
    );
}

pub fn paint_layout_tree_with_options(
    grid: &mut Grid,
    theme: &std::sync::Arc<Theme>,
    node: &LayoutTree,
    area: Rect,
    term_size: (u16, u16),
    options: PaintLayoutOptions<'_>,
    paint: &mut PaintDispatch,
) {
    node.resolve(area, options.sizer)
        .paint(grid, theme, term_size, options.style, paint);
}
