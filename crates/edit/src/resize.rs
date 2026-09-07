//! Layout-owned resizing shared by pointer gestures, keyboard chords and Lua.

use super::*;
use layout::{Axis, ResolvedLayout, ResolvedSplit, SplitId};

pub(super) struct ResolvedOverlay {
    pub id: OverlayId,
    pub rect: Rect,
    pub z: u16,
    pub layout: ResolvedLayout,
}

pub(super) struct ResolvedDecoration {
    pub owner: WinId,
    pub rect: Rect,
    pub layout: ResolvedLayout,
}

/// Current geometry for one input operation or frame. Do not cache across
/// mutations: natural-size providers and retained split handles can change
/// independently of the host's layout tree.
pub(super) struct ResolvedUiLayout {
    pub root: ResolvedLayout,
    pub overlays: Vec<ResolvedOverlay>,
    pub decorations: Vec<ResolvedDecoration>,
}

impl ResolvedUiLayout {
    pub fn layouts(&self) -> impl DoubleEndedIterator<Item = &ResolvedLayout> {
        std::iter::once(&self.root)
            .chain(self.decorations.iter().map(|surface| &surface.layout))
            .chain(self.overlays.iter().map(|surface| &surface.layout))
    }

    pub fn split(&self, id: SplitId) -> Option<&ResolvedSplit> {
        self.layouts().rev().find_map(|layout| layout.split(id))
    }
}

impl Ui {
    pub(super) fn resolve_scene(&self, cursor: Option<(u16, u16)>) -> ResolvedUiLayout {
        let root = self.splits().resolve(self.surface.area(), self);
        let rects = root
            .leaves()
            .map(|(id, rect)| (WinId(id.0), rect))
            .collect();
        let overlays = self
            .resolve_overlays_with_rects(cursor, &rects)
            .into_iter()
            .map(|(id, rect, overlay)| ResolvedOverlay {
                id,
                rect,
                z: overlay.z,
                layout: overlay.layout.resolve(rect, self),
            })
            .collect();
        let decorations = self
            .resolve_decorations_with_rects(&rects)
            .into_iter()
            .map(|(_, owner, rect, decoration)| ResolvedDecoration {
                owner,
                rect,
                layout: decoration.layout.resolve(rect, self),
            })
            .collect();
        ResolvedUiLayout {
            root,
            overlays,
            decorations,
        }
    }

    /// Current geometry for a mounted split. Resolve again after changing sizes,
    /// content measurements, or the layout, just as with `LayoutTree::resolve`.
    pub fn resolved_split(&self, id: SplitId) -> Option<ResolvedSplit> {
        self.resolve_scene(None).split(id).cloned()
    }

    pub(super) fn layout_hit_test(
        &self,
        resolved: &ResolvedLayout,
        row: u16,
        col: u16,
    ) -> Option<HitTarget> {
        if let Some(resolved) = resolved.divider_at(row, col) {
            let split = &resolved.split;
            let edges = match split.axis() {
                Axis::Horizontal => ResizeEdges::east(),
                Axis::Vertical => ResizeEdges::south(),
            };
            return Some(HitTarget::Chrome {
                owner: ChromeOwner::Split(split.id()),
                action: ChromeAction::Resize(edges),
            });
        }
        let id = resolved.leaf_at(row, col)?;
        let win = WinId(id.0);
        if let Some(window) = self.wins.get(&win) {
            if window
                .viewport
                .and_then(|viewport| viewport.scrollbar.map(|bar| (viewport, bar)))
                .is_some_and(|(viewport, bar)| bar.contains(viewport.rect, row, col))
            {
                return Some(HitTarget::Scrollbar { owner: win });
            }
            Some(HitTarget::Window(win))
        } else {
            Some(HitTarget::Paint(id))
        }
    }

    /// Grow a window's nearest resizable ancestor along `axis` by signed cells.
    /// Split siblings absorb the inverse delta. Without a split, resize the
    /// containing overlay or docked dialog using its existing bounds.
    pub fn resize_window(&mut self, win: WinId, axis: Axis, delta: i32) -> bool {
        if !self.wins.contains_key(&win) {
            return false;
        }
        let delta = delta.clamp(-i32::from(u16::MAX), i32::from(u16::MAX));
        let scene = self.resolve_scene(None);
        let target = scene
            .layouts()
            .rev()
            .find_map(|layout| layout.split_for_leaf(win.into(), axis));
        if let Some((split, pane)) = target {
            split.resize(pane, delta);
            return true;
        }
        let owner = self
            .overlay_for_leaf(win)
            .map(ChromeOwner::Overlay)
            .or_else(|| {
                (axis == Axis::Vertical)
                    .then(|| {
                        self.docked_surfaces
                            .iter()
                            .find(|(_, surface)| surface.layout.contains_leaf(win))
                            .map(|(id, _)| ChromeOwner::Container(*id))
                    })
                    .flatten()
            });
        let Some(owner) = owner else { return false };
        let Some(rect) = self.chrome_owner_rect(owner) else {
            return false;
        };
        let edges = match (axis, owner) {
            (Axis::Horizontal, _) => ResizeEdges::east(),
            (Axis::Vertical, ChromeOwner::Container(_)) => ResizeEdges::north(),
            _ => ResizeEdges::south(),
        };
        self.resize_chrome(
            owner,
            rect,
            edges,
            if axis == Axis::Horizontal { delta } else { 0 },
            if edges.north {
                delta.saturating_neg()
            } else if axis == Axis::Vertical {
                delta
            } else {
                0
            },
        );
        true
    }

    /// Equalize every enclosing split without touching focus or document state.
    pub fn equalize_window(&self, win: WinId) -> bool {
        if !self.wins.contains_key(&win) {
            return false;
        }
        let mut found = false;
        for layout in self.resolve_scene(None).layouts() {
            for resolved in layout.splits() {
                if resolved.pane_for_leaf(win.into()).is_some() {
                    resolved.equalize();
                    found = true;
                }
            }
        }
        found
    }

    pub(crate) fn resize_chrome(
        &mut self,
        owner: ChromeOwner,
        rect: Rect,
        edges: ResizeEdges,
        dx: i32,
        dy: i32,
    ) {
        match owner {
            ChromeOwner::Overlay(id) => {
                let Some(index) = self.overlays.iter().position(|(oid, _)| *oid == id) else {
                    return;
                };
                let bounds =
                    overlay_resize_bounds(&self.overlays[index].1, self.terminal_size(), self);
                let (top, left, w, h) = resize_chrome_geometry(rect, edges, dx, dy, bounds);
                let overlay = &mut self.overlays[index].1;
                overlay.size_override = Some((w, h));
                overlay.anchor = anchor_after_resize(overlay.anchor.clone(), edges, top, left);
            }
            ChromeOwner::Container(id) => {
                let Some(bounds) = self.docked_surface_resize_bounds(id) else {
                    return;
                };
                let (_, _, _, height) = resize_chrome_geometry(rect, edges, dx, dy, bounds);
                if let Some(surface) = self.docked_surface_mut(id) {
                    surface.height_override = Some(height);
                    surface.expanded = false;
                }
            }
            ChromeOwner::Split(_) => {}
        }
    }
}
