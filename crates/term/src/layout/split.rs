//! Runtime-neutral sizing, geometry, and interaction for two-pane layouts.

use super::{inset_for_chrome, Chrome, LayoutTree, Rect};
use crate::Style;
use std::num::NonZeroU16;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    Horizontal,
    Vertical,
}

impl Axis {
    pub fn extent(self, rect: Rect) -> u16 {
        match self {
            Self::Horizontal => rect.width,
            Self::Vertical => rect.height,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SplitId(u64);

/// A validated fraction in `0..=1`, independent of any screen geometry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SplitRatio {
    numerator: u16,
    denominator: NonZeroU16,
}

impl SplitRatio {
    pub fn new(numerator: u16, denominator: u16) -> Option<Self> {
        if numerator > denominator {
            return None;
        }
        Some(Self {
            numerator,
            denominator: NonZeroU16::new(denominator)?,
        })
    }

    pub fn numerator(self) -> u16 {
        self.numerator
    }

    pub fn denominator(self) -> u16 {
        self.denominator.get()
    }
}

/// Preferred first-pane size, excluding the divider. Persist these plain values,
/// not a `SplitId`. Geometry clamps the preference without overwriting it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitSize {
    Cells(u16),
    Ratio(SplitRatio),
}

impl SplitSize {
    /// A fraction in `0..=1`. Rejects zero denominators and oversized shares.
    pub fn ratio(numerator: u16, denominator: u16) -> Option<Self> {
        SplitRatio::new(numerator, denominator).map(Self::Ratio)
    }

    /// Convert the preference to cells without applying pane minima or clamping
    /// a cell preference. Use this for persistence only if the host wants cells.
    pub fn resolve(self, available: u16) -> u16 {
        match self {
            Self::Cells(n) => n,
            Self::Ratio(ratio) => {
                (u32::from(available) * u32::from(ratio.numerator())
                    / u32::from(ratio.denominator())) as u16
            }
        }
    }
}

impl Default for SplitSize {
    fn default() -> Self {
        Self::ratio(1, 2).unwrap()
    }
}

/// How a user resize is retained when the terminal changes size.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SplitResizeMode {
    /// Keep the chosen first-pane cell count, for sidebars and tool panels.
    Cells,
    /// Keep the chosen fraction of available space.
    #[default]
    Proportional,
}

/// Divider styles are ordinary terminal styles, independent of theme names.
#[derive(Clone, Copy, Debug, Default)]
pub struct DividerStyles {
    pub normal: Style,
    pub active: Style,
}

impl DividerStyles {
    pub fn get(self, active: bool) -> Style {
        if active {
            self.active
        } else {
            self.normal
        }
    }
}

/// Construction options, copied into an immutable retained split configuration.
#[derive(Clone, Copy, Debug)]
pub struct SplitOptions {
    /// Initial first-pane preference, also used by `Split::reset`.
    pub size: SplitSize,
    /// Pane minima including child chrome. Each pane reserves at least one cell
    /// when space permits; zero is treated as one.
    pub minimum: [u16; 2],
    pub resize_mode: SplitResizeMode,
    /// Override the renderer's divider styles for this split.
    pub styles: Option<DividerStyles>,
}

impl Default for SplitOptions {
    fn default() -> Self {
        Self {
            size: SplitSize::default(),
            minimum: [1, 1],
            resize_mode: SplitResizeMode::default(),
            styles: None,
        }
    }
}

#[derive(Debug)]
struct SplitState {
    id: SplitId,
    axis: Axis,
    options: SplitOptions,
    position: Mutex<SplitSize>,
}

/// A retained split handle. Clones share identity, immutable configuration, and
/// preferred size. Geometry belongs to each resolved layout, not this handle.
/// Keep the handle when rebuilding a tree. Each occurrence must have its own
/// identity, so do not place the same handle twice in a single layout.
#[derive(Clone, Debug)]
pub struct Split(Arc<SplitState>);

#[derive(Clone, Copy, Debug)]
pub struct SplitGeometry {
    pub panes: [Rect; 2],
    pub divider: Rect,
    pub available: u16,
}

impl Split {
    pub fn new(axis: Axis, mut options: SplitOptions) -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        options.minimum = options.minimum.map(|n| n.max(1));
        Self(Arc::new(SplitState {
            id: SplitId(NEXT_ID.fetch_add(1, Ordering::Relaxed)),
            axis,
            position: Mutex::new(options.size),
            options,
        }))
    }

    pub fn id(&self) -> SplitId {
        self.0.id
    }

    pub fn axis(&self) -> Axis {
        self.0.axis
    }

    pub fn options(&self) -> SplitOptions {
        self.0.options
    }

    pub fn preferred_size(&self) -> SplitSize {
        *self.0.position.lock().unwrap()
    }

    /// Restore a persisted preference without applying temporary screen bounds.
    /// Returns whether the preference changed.
    pub fn set_preferred_size(&self, size: SplitSize) -> bool {
        let mut position = self.0.position.lock().unwrap();
        let changed = *position != size;
        *position = size;
        changed
    }

    pub fn reset(&self) -> bool {
        self.set_preferred_size(self.0.options.size)
    }

    pub(super) fn natural_extents(&self, first: u16, second: u16) -> [u16; 2] {
        // Ratios need a containing extent. The initial cell size remains a
        // natural-size hint when a user chooses proportional sizing.
        let preferred = match self.preferred_size() {
            SplitSize::Cells(n) => n,
            SplitSize::Ratio(_) => match self.0.options.size {
                SplitSize::Cells(n) => n,
                SplitSize::Ratio(_) => 0,
            },
        };
        let [min_first, min_second] = self.0.options.minimum;
        [first.max(min_first).max(preferred), second.max(min_second)]
    }

    fn bounds(&self, available: u16) -> (u16, u16) {
        if available < 2 {
            return (available, available);
        }
        let [first, second] = self.0.options.minimum;
        if u32::from(first) + u32::from(second) > u32::from(available) {
            let first = (u32::from(available) * u32::from(first)
                / (u32::from(first) + u32::from(second))) as u16;
            let first = first.clamp(1, available - 1);
            return (first, first);
        }
        (first, available - second)
    }

    pub fn layout(&self, area: Rect, chrome: &Chrome) -> SplitGeometry {
        let inner = inset_for_chrome(area, chrome);
        let extent = self.axis().extent(inner);
        let gap = u16::from(extent >= 3);
        let available = extent.saturating_sub(gap);
        let wanted = self.preferred_size().resolve(available);
        let (min, max) = self.bounds(available);
        let first = wanted.clamp(min, max);
        let second = available - first;
        let (panes, divider) = match self.axis() {
            Axis::Horizontal => (
                [
                    Rect::new(inner.top, inner.left, first, inner.height),
                    Rect::new(
                        inner.top,
                        inner.left.saturating_add(first + gap),
                        second,
                        inner.height,
                    ),
                ],
                Rect::new(
                    inner.top,
                    inner.left.saturating_add(first),
                    gap,
                    inner.height,
                ),
            ),
            Axis::Vertical => (
                [
                    Rect::new(inner.top, inner.left, inner.width, first),
                    Rect::new(
                        inner.top.saturating_add(first + gap),
                        inner.left,
                        inner.width,
                        second,
                    ),
                ],
                Rect::new(
                    inner.top.saturating_add(first),
                    inner.left,
                    inner.width,
                    gap,
                ),
            ),
        };
        SplitGeometry {
            panes,
            divider,
            available,
        }
    }

    /// Apply a user size through the same bounds as layout resolution.
    /// A no-op at the current bounds never destroys an unclamped preference.
    pub fn set_position(&self, cells: i32, available: u16) -> bool {
        if available == 0 {
            return false;
        }
        let (min, max) = self.bounds(available);
        let cells = cells.clamp(i32::from(min), i32::from(max)) as u16;
        if self.preferred_size().resolve(available).clamp(min, max) == cells {
            return false;
        }
        self.set_preferred_size(match self.0.options.resize_mode {
            SplitResizeMode::Cells => SplitSize::Cells(cells),
            SplitResizeMode::Proportional => SplitSize::ratio(cells, available).unwrap(),
        })
    }
}

/// A resolved divider shared by painting and pointer hit-testing.
#[derive(Clone, Copy, Debug)]
pub struct SplitDivider {
    pub id: SplitId,
    pub axis: Axis,
    pub rect: Rect,
    styles: Option<DividerStyles>,
    join_start: bool,
    join_end: bool,
}

impl SplitDivider {
    pub fn new(split: &Split, rect: Rect, children: &[LayoutTree; 2]) -> Self {
        let borders = [0, 1].map(|i| children[i].chrome().border.unwrap_or_default());
        let (join_start, join_end) = match split.axis() {
            Axis::Horizontal => (
                borders.iter().all(|b| b.top.is_some()),
                borders.iter().all(|b| b.bottom.is_some()),
            ),
            Axis::Vertical => (
                borders.iter().all(|b| b.left.is_some()),
                borders.iter().all(|b| b.right.is_some()),
            ),
        };
        Self {
            id: split.id(),
            axis: split.axis(),
            rect,
            styles: split.options().styles,
            join_start,
            join_end,
        }
    }

    pub fn style(self, defaults: DividerStyles, active: bool) -> Style {
        self.styles.unwrap_or(defaults).get(active)
    }

    pub fn paint(self, grid: &mut crate::Grid, style: Style) {
        if self.rect.width == 0 || self.rect.height == 0 {
            return;
        }
        let (len, line, start, end) = match self.axis {
            Axis::Horizontal => (self.rect.height, '│', '┬', '┴'),
            Axis::Vertical => (self.rect.width, '─', '├', '┤'),
        };
        for n in 0..len {
            let ch = if n == 0 && self.join_start {
                start
            } else if n == len - 1 && self.join_end {
                end
            } else {
                line
            };
            let (row, col) = match self.axis {
                Axis::Horizontal => (self.rect.top.saturating_add(n), self.rect.left),
                Axis::Vertical => (self.rect.top, self.rect.left.saturating_add(n)),
            };
            let mut bytes = [0; 4];
            grid.put_str(col, row, ch.encode_utf8(&mut bytes), style);
        }
    }
}

/// Lifecycle of a consumed split gesture event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitPhase {
    Started,
    Dragging,
    Finished,
    /// Capture ended without a final pointer update. The last size is retained.
    Cancelled,
}

/// Redraw on any response, including start and end (active chrome changes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub struct SplitResponse {
    pub id: SplitId,
    pub phase: SplitPhase,
    /// Whether any preference changed since this gesture began, at every phase.
    pub changed: bool,
}

impl SplitResponse {
    /// Both release and cancellation end capture and retain the preferred size.
    /// Persist when this and `changed` are true.
    pub fn ended(self) -> bool {
        matches!(self.phase, SplitPhase::Finished | SplitPhase::Cancelled)
    }
}

#[derive(Clone, Copy, Debug)]
struct SplitDrag {
    id: SplitId,
    grab_offset: i32,
    changed: bool,
}

/// Pointer capture for split dividers. No event loop, keybinding, focus, or
/// persistence policy is required. Re-resolve geometry for every update.
#[derive(Clone, Copy, Debug, Default)]
pub struct SplitInteraction {
    drag: Option<SplitDrag>,
}

impl SplitInteraction {
    pub fn active_id(&self) -> Option<SplitId> {
        self.drag.map(|drag| drag.id)
    }

    pub fn begin(
        &mut self,
        resolved: &super::ResolvedSplit,
        row: u16,
        col: u16,
    ) -> Option<SplitResponse> {
        if self.drag.is_some() || !resolved.divider.rect.contains(row, col) {
            return None;
        }
        let rect = resolved.divider.rect;
        let grab_offset = match resolved.split.axis() {
            Axis::Horizontal => i32::from(col) - i32::from(rect.left),
            Axis::Vertical => i32::from(row) - i32::from(rect.top),
        };
        let id = resolved.split.id();
        self.drag = Some(SplitDrag {
            id,
            grab_offset,
            changed: false,
        });
        Some(SplitResponse {
            id,
            changed: false,
            phase: SplitPhase::Started,
        })
    }

    /// Update against current geometry, even outside the divider or terminal.
    /// Missing/replaced splits cancel capture without mutating a stale handle.
    pub fn update(
        &mut self,
        resolved: Option<&super::ResolvedSplit>,
        row: u16,
        col: u16,
    ) -> Option<SplitResponse> {
        let drag = self.drag.as_mut()?;
        let Some(resolved) = resolved.filter(|resolved| resolved.split.id() == drag.id) else {
            return self.cancel();
        };
        let first = resolved.geometry.panes[0];
        let cells = match resolved.split.axis() {
            Axis::Horizontal => i32::from(col) - i32::from(first.left),
            Axis::Vertical => i32::from(row) - i32::from(first.top),
        } - drag.grab_offset;
        let changed = resolved
            .split
            .set_position(cells, resolved.geometry.available);
        drag.changed |= changed;
        Some(SplitResponse {
            id: drag.id,
            changed: drag.changed,
            phase: SplitPhase::Dragging,
        })
    }

    /// Apply the release position against current geometry and end capture.
    /// A missing or replaced split produces `Cancelled`, never a stale mutation.
    pub fn release(
        &mut self,
        resolved: Option<&super::ResolvedSplit>,
        row: u16,
        col: u16,
    ) -> Option<SplitResponse> {
        let response = self.update(resolved, row, col)?;
        if response.ended() {
            Some(response)
        } else {
            self.end(SplitPhase::Finished)
        }
    }

    fn end(&mut self, phase: SplitPhase) -> Option<SplitResponse> {
        self.drag.take().map(|drag| SplitResponse {
            id: drag.id,
            changed: drag.changed,
            phase,
        })
    }

    /// Release capture without a final position update, retaining the last size.
    /// Use on focus loss, keyboard input, or removal of the containing layout.
    pub fn cancel(&mut self) -> Option<SplitResponse> {
        self.end(SplitPhase::Cancelled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_bounds_and_ratio_survive_tiny_terminals() {
        for axis in [Axis::Horizontal, Axis::Vertical] {
            let split = Split::new(
                axis,
                SplitOptions {
                    size: SplitSize::Cells(34),
                    minimum: [24, 32],
                    ..SplitOptions::default()
                },
            );
            for extent in [0, 1, 2, 3, 12, 56, 57, 100, u16::MAX] {
                let rect = match axis {
                    Axis::Horizontal => Rect::new(0, 0, extent, 20),
                    Axis::Vertical => Rect::new(0, 0, 20, extent),
                };
                let geometry = split.layout(rect, &Chrome::default());
                assert_eq!(
                    axis.extent(geometry.panes[0])
                        + axis.extent(geometry.panes[1])
                        + axis.extent(geometry.divider),
                    extent
                );
                if extent >= 3 {
                    assert!(axis.extent(geometry.panes[0]) > 0);
                    assert!(axis.extent(geometry.panes[1]) > 0);
                }
                split.set_position(i32::MAX, geometry.available);
                split.set_position(i32::MIN, geometry.available);
            }
            split.set_position(60, 100);
            let clone = split.clone();
            let big = Rect::new(0, 0, 201, 201);
            assert_eq!(
                axis.extent(clone.layout(big, &Chrome::default()).panes[0]),
                120
            );
            split.layout(Rect::new(0, 0, 10, 10), &Chrome::default());
            assert_eq!(
                axis.extent(clone.layout(big, &Chrome::default()).panes[0]),
                120
            );
            clone.set_preferred_size(SplitSize::default());
            assert_eq!(
                axis.extent(split.layout(big, &Chrome::default()).panes[0]),
                100
            );
        }
    }
}
