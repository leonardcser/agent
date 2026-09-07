//! Resolved layout snapshots shared by painting, hit-testing, and interaction.

use super::{Chrome, ChromePaintCtx, LayoutTree, LeafSizer, PaintId, Rect};
use super::{DividerStyles, Split, SplitDivider, SplitGeometry, SplitId};
use crate::{Grid, PaintDispatch, Theme};
use std::{collections::HashMap, ops::Range, sync::Arc};

#[derive(Clone, Copy, Debug)]
struct ResolvedLeaf {
    order: usize,
    rect: Rect,
    parent: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitPane {
    First,
    Second,
}

/// A split's screen geometry and retained sizing handle at resolution time.
#[derive(Clone, Debug)]
pub struct ResolvedSplit {
    pub split: Split,
    pub geometry: SplitGeometry,
    pub divider: SplitDivider,
    leaves: Arc<HashMap<PaintId, ResolvedLeaf>>,
    panes: [Range<usize>; 2],
    parent: Option<usize>,
}

impl ResolvedSplit {
    pub fn pane_for_leaf(&self, id: PaintId) -> Option<SplitPane> {
        let order = self.leaves.get(&id)?.order;
        if self.panes[0].contains(&order) {
            Some(SplitPane::First)
        } else if self.panes[1].contains(&order) {
            Some(SplitPane::Second)
        } else {
            None
        }
    }

    /// Balance the panes, retaining cells or a ratio according to the split's
    /// resize policy. Cell-mode callers can persist the resulting cell count.
    pub fn equalize(&self) -> bool {
        match self.split.options().resize_mode {
            super::SplitResizeMode::Cells => self.split.set_position(
                i32::from(self.geometry.available / 2),
                self.geometry.available,
            ),
            super::SplitResizeMode::Proportional => {
                self.split.set_preferred_size(super::SplitSize::default())
            }
        }
    }

    /// Grow one pane by signed cells, using the same bounds as pointer resizing.
    pub fn resize(&self, pane: SplitPane, delta: i32) -> bool {
        let delta = match pane {
            SplitPane::First => delta,
            SplitPane::Second => delta.saturating_neg(),
        };
        let first = self.split.axis().extent(self.geometry.panes[0]);
        self.split.set_position(
            i32::from(first).saturating_add(delta),
            self.geometry.available,
        )
    }
}

/// One operation in painter order, retaining resolved geometry rather than
/// measuring again during painting. Later entries shadow earlier hit targets.
#[derive(Clone, Debug)]
pub enum LayoutPaintOp {
    Chrome {
        area: Rect,
        chrome: Chrome,
        root: bool,
    },
    Leaf {
        id: PaintId,
        rect: Rect,
    },
    Divider(SplitDivider),
}

/// Appearance and active gesture state, independent of geometry or input policy.
#[derive(Clone, Copy, Default)]
pub struct LayoutStyle {
    pub root_chrome: ChromePaintCtx,
    pub dividers: DividerStyles,
    pub active_split: Option<SplitId>,
}

/// An immutable geometry snapshot. Resolve again after changing preferred sizes,
/// content measurements, or terminal size. Handles still refer to retained splits.
#[derive(Clone, Debug, Default)]
pub struct ResolvedLayout {
    operations: Vec<LayoutPaintOp>,
    leaves: Arc<HashMap<PaintId, ResolvedLeaf>>,
    splits: Vec<ResolvedSplit>,
    split_indices: HashMap<SplitId, usize>,
}

impl ResolvedLayout {
    pub fn new(tree: &LayoutTree, area: Rect, sizer: &dyn LeafSizer) -> Self {
        // End markers delimit pane membership in a single leaf-order index.
        // Neither traversal nor storage grows with the number of ancestors.
        enum Visit<'a> {
            Node(&'a LayoutTree, Rect, Option<usize>),
            EndPane {
                split: usize,
                pane: usize,
                start: usize,
            },
            Pane(&'a LayoutTree, Rect, usize, usize),
        }
        struct PendingSplit {
            split: Split,
            geometry: SplitGeometry,
            divider: SplitDivider,
            panes: [Range<usize>; 2],
            parent: Option<usize>,
        }
        let mut operations = Vec::new();
        let mut leaves = HashMap::new();
        let mut splits: Vec<PendingSplit> = Vec::new();
        let mut split_indices = HashMap::new();
        let mut stack = vec![Visit::Node(tree, area, None)];
        let mut order = 0;
        while let Some(visit) = stack.pop() {
            let (node, area, parent) = match visit {
                Visit::Node(node, area, parent) => (node, area, parent),
                Visit::Pane(node, area, split, pane) => {
                    stack.push(Visit::EndPane {
                        split,
                        pane,
                        start: order,
                    });
                    stack.push(Visit::Node(node, area, Some(split)));
                    continue;
                }
                Visit::EndPane { split, pane, start } => {
                    splits[split].panes[pane] = start..order;
                    continue;
                }
            };
            let chrome = node.chrome();
            operations.push(LayoutPaintOp::Chrome {
                area,
                chrome: chrome.clone(),
                root: operations.is_empty(),
            });
            match node {
                LayoutTree::Leaf { id, .. } => {
                    let rect = super::inset_for_chrome(area, chrome);
                    operations.push(LayoutPaintOp::Leaf { id: *id, rect });
                    leaves.insert(
                        *id,
                        ResolvedLeaf {
                            order,
                            rect,
                            parent,
                        },
                    );
                    order += 1;
                }
                LayoutTree::Frame { child, .. } => stack.push(Visit::Node(
                    child,
                    super::inset_for_chrome(area, chrome),
                    parent,
                )),
                LayoutTree::Split {
                    split, children, ..
                } => {
                    let geometry = split.layout(area, chrome);
                    let divider = SplitDivider::new(split, geometry.divider, children);
                    operations.push(LayoutPaintOp::Divider(divider));
                    let index = splits.len();
                    split_indices.insert(split.id(), index);
                    splits.push(PendingSplit {
                        split: split.clone(),
                        geometry,
                        divider,
                        panes: [0..0, 0..0],
                        parent,
                    });
                    for pane in (0..2).rev() {
                        stack.push(Visit::Pane(
                            &children[pane],
                            geometry.panes[pane],
                            index,
                            pane,
                        ));
                    }
                }
                LayoutTree::Vbox { items, .. } | LayoutTree::Hbox { items, .. } => {
                    let (_, rects) = super::layout_box_children(
                        items,
                        chrome,
                        area,
                        matches!(node, LayoutTree::Vbox { .. }),
                        sizer,
                    );
                    for ((_, child), rect) in items.iter().zip(rects).rev() {
                        stack.push(Visit::Node(child, rect, parent));
                    }
                }
            }
        }
        let leaves = Arc::new(leaves);
        let splits = splits
            .into_iter()
            .map(|pending| ResolvedSplit {
                split: pending.split,
                geometry: pending.geometry,
                divider: pending.divider,
                panes: pending.panes,
                parent: pending.parent,
                leaves: Arc::clone(&leaves),
            })
            .collect();
        Self {
            operations,
            leaves,
            splits,
            split_indices,
        }
    }

    pub fn operations(&self) -> &[LayoutPaintOp] {
        &self.operations
    }

    pub fn into_operations(self) -> Vec<LayoutPaintOp> {
        self.operations
    }

    pub fn leaves(&self) -> impl DoubleEndedIterator<Item = (PaintId, Rect)> + '_ {
        self.operations.iter().filter_map(|op| match op {
            LayoutPaintOp::Leaf { id, rect } => Some((*id, *rect)),
            _ => None,
        })
    }

    pub fn containers(&self) -> impl DoubleEndedIterator<Item = (super::ContainerId, Rect)> + '_ {
        self.operations.iter().filter_map(|op| match op {
            LayoutPaintOp::Chrome { area, chrome, .. } => chrome.container.map(|id| (id, *area)),
            _ => None,
        })
    }

    pub fn leaf_rect(&self, id: PaintId) -> Option<Rect> {
        self.leaves.get(&id).map(|leaf| leaf.rect)
    }

    pub fn leaf_at(&self, row: u16, col: u16) -> Option<PaintId> {
        self.leaves()
            .rev()
            .find_map(|(id, rect)| rect.contains(row, col).then_some(id))
    }

    pub fn splits(&self) -> impl DoubleEndedIterator<Item = &ResolvedSplit> {
        self.splits.iter()
    }

    pub fn split(&self, id: SplitId) -> Option<&ResolvedSplit> {
        self.split_indices
            .get(&id)
            .map(|&index| &self.splits[index])
    }

    pub fn divider_at(&self, row: u16, col: u16) -> Option<&ResolvedSplit> {
        self.splits()
            .rev()
            .find(|split| split.divider.rect.contains(row, col))
    }

    pub fn split_for_leaf(
        &self,
        id: PaintId,
        axis: super::Axis,
    ) -> Option<(&ResolvedSplit, SplitPane)> {
        let mut parent = self.leaves.get(&id)?.parent;
        while let Some(index) = parent {
            let resolved = &self.splits[index];
            if resolved.split.axis() == axis {
                return resolved.pane_for_leaf(id).map(|pane| (resolved, pane));
            }
            parent = resolved.parent;
        }
        None
    }

    pub fn paint(
        &self,
        grid: &mut Grid,
        theme: &Arc<Theme>,
        term_size: (u16, u16),
        style: LayoutStyle,
        paint: &mut PaintDispatch,
    ) {
        for op in &self.operations {
            match op {
                LayoutPaintOp::Chrome { area, chrome, root } => {
                    let context = if *root {
                        style.root_chrome
                    } else {
                        ChromePaintCtx::empty()
                    };
                    super::paint_chrome_with(grid, *area, chrome, theme, context);
                }
                LayoutPaintOp::Leaf { id, rect } => paint(*id, *rect, grid, theme, term_size),
                LayoutPaintOp::Divider(divider) => {
                    divider.paint(
                        grid,
                        divider.style(style.dividers, style.active_split == Some(divider.id)),
                    );
                }
            }
        }
    }
}
