//! Renderer facade: bundles a [`Compositor`], [`Arc<Theme>`], [`LayoutTree`],
//! and terminal size. Use directly for standalone renderers; `smelt-edit`'s
//! `Ui` wraps it and adds editor-specific state.

use std::io::Write;
use std::sync::Arc;

use smelt_style::theme::Theme;

use crate::compositor::Compositor;
use crate::grid::{Grid, GridSlice};
use crate::layout::{LayoutStyle, LayoutTree, NoopSizer, PaintId, Rect, ResolvedLayout};

#[derive(Clone)]
pub struct Surface {
    compositor: Compositor,
    layout: LayoutTree,
    theme: Arc<Theme>,
    size: (u16, u16),
    layout_style: LayoutStyle,
}

impl Surface {
    pub fn new(width: u16, height: u16) -> Self {
        Self::with_theme(width, height, Theme::new())
    }

    pub fn with_theme(width: u16, height: u16, theme: Theme) -> Self {
        Self {
            compositor: Compositor::new(width, height),
            layout: LayoutTree::vbox(Vec::new()),
            theme: Arc::new(theme),
            size: (width, height),
            layout_style: LayoutStyle::default(),
        }
    }

    pub fn terminal_size(&self) -> (u16, u16) {
        self.size
    }

    pub fn set_terminal_size(&mut self, w: u16, h: u16) {
        self.size = (w, h);
        self.compositor.resize(w, h);
    }

    /// Full-screen rect at the current terminal size.
    pub fn area(&self) -> Rect {
        Rect::new(0, 0, self.size.0, self.size.1)
    }

    pub fn layout(&self) -> &LayoutTree {
        &self.layout
    }

    pub fn set_layout(&mut self, layout: LayoutTree) {
        self.layout = layout;
    }

    pub fn theme(&self) -> &Arc<Theme> {
        &self.theme
    }

    /// Mutable theme handle; copy-on-write via `Arc::make_mut`.
    pub fn theme_mut(&mut self) -> &mut Theme {
        Arc::make_mut(&mut self.theme)
    }

    pub fn force_redraw(&mut self) {
        self.compositor.force_redraw();
    }

    /// Resolved screen rect for a `PaintId` leaf, or `None` if not present.
    pub fn paint_rect(&self, id: PaintId) -> Option<Rect> {
        self.resolve_layout().leaf_rect(id)
    }

    /// Resolve current geometry for hit-testing and programmatic resizing.
    pub fn resolve_layout(&self) -> ResolvedLayout {
        self.layout.resolve(self.area(), &NoopSizer)
    }

    /// Configure divider appearance and the active split without replacing layout.
    pub fn set_layout_style(&mut self, style: LayoutStyle) {
        self.layout_style = style;
    }

    pub fn compositor(&self) -> &Compositor {
        &self.compositor
    }

    pub fn compositor_mut(&mut self) -> &mut Compositor {
        &mut self.compositor
    }

    /// Walk the layout tree, paint chrome, and hand each leaf's `GridSlice` to `paint`.
    pub fn render<W, F>(&mut self, w: &mut W, mut paint: F) -> std::io::Result<()>
    where
        W: Write,
        F: FnMut(PaintId, &mut GridSlice<'_>, &Arc<Theme>),
    {
        let layout = self.resolve_layout();
        self.render_resolved(w, &layout, &mut paint)
    }

    /// Paint a geometry snapshot already used for viewport setup or hit-testing.
    /// The caller must resolve it again after layout or terminal-size changes.
    pub fn render_resolved<W, F>(
        &mut self,
        w: &mut W,
        layout: &ResolvedLayout,
        mut paint: F,
    ) -> std::io::Result<()>
    where
        W: Write,
        F: FnMut(PaintId, &mut GridSlice<'_>, &Arc<Theme>),
    {
        let size = self.size;
        let style = self.layout_style;
        let theme_arc = Arc::clone(&self.theme);
        self.compositor
            .render_with(&self.theme, w, move |grid, _theme| {
                let theme = &theme_arc;
                let mut dispatch = |id: PaintId,
                                    leaf: Rect,
                                    grid: &mut Grid,
                                    theme: &Arc<Theme>,
                                    _ts: (u16, u16)| {
                    let mut slice = grid.slice_mut(leaf);
                    paint(id, &mut slice, theme);
                };
                layout.paint(grid, theme, size, style, &mut dispatch);
            })
    }

    /// Paint directly into the compositor's grid, bypassing the layout tree.
    pub fn render_raw<W, F>(&mut self, w: &mut W, paint: F) -> std::io::Result<()>
    where
        W: Write,
        F: FnOnce(&mut Grid, &Theme),
    {
        self.compositor.render_with(&self.theme, w, paint)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{Constraint, PaintId};

    #[test]
    fn area_reports_full_terminal_rect() {
        let s = Surface::new(80, 24);
        assert_eq!(s.area(), Rect::new(0, 0, 80, 24));
    }

    #[test]
    fn set_terminal_size_updates_reported_size_and_area() {
        let mut s = Surface::new(10, 5);
        s.set_terminal_size(40, 20);
        assert_eq!(s.terminal_size(), (40, 20));
        assert_eq!(s.area(), Rect::new(0, 0, 40, 20));
    }

    #[test]
    fn paint_rect_returns_leaf_rect_resolved_against_current_size() {
        // A layout with two equal-height panes split vertically: each
        // leaf should resolve to half the surface area.
        let mut s = Surface::new(80, 24);
        let top = PaintId(1);
        let bottom = PaintId(2);
        s.set_layout(LayoutTree::vbox(vec![
            (Constraint::Fill, LayoutTree::leaf(top)),
            (Constraint::Fill, LayoutTree::leaf(bottom)),
        ]));
        let top_rect = s.paint_rect(top).expect("top leaf resolved");
        let bot_rect = s.paint_rect(bottom).expect("bottom leaf resolved");
        assert_eq!(top_rect.height + bot_rect.height, 24);
        assert_eq!(top_rect.width, 80);
        assert_eq!(bot_rect.width, 80);
    }

    #[test]
    fn paint_rect_returns_none_for_unknown_leaf() {
        let s = Surface::new(80, 24);
        assert_eq!(s.paint_rect(PaintId(999)), None);
    }

    #[test]
    fn theme_mut_allows_in_place_mutation() {
        use smelt_style::style::{Color, Style};
        let mut s = Surface::new(10, 5);
        s.theme_mut().set("Error", Style::new().fg(Color::Red));
        // The same theme handle now resolves "Error".
        assert_eq!(s.theme().get("Error").fg, Some(Color::Red));
    }
}
