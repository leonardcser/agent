//! Buffer viewport — the bridge between a scrollable buffer and its
//! on-screen window.
//!
//! A `Viewport` records *what the last paint pass drew and where*:
//! screen rect, content dimensions, scroll state, and optional
//! scrollbar geometry.  Mouse handlers hit-test against viewports
//! instead of recomputing layout on every tick, keeping paint and
//! input in lockstep.

/// Geometry of a single-column scrollbar painted during the last frame.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ScrollbarGeom {
    pub col: u16,
    pub top_row: u16,
    pub rows: u16,
    pub total_rows: u16,
}

impl ScrollbarGeom {
    pub fn max_scroll(&self) -> u16 {
        self.total_rows.saturating_sub(self.rows)
    }

    pub fn thumb_size(&self) -> u16 {
        let rows = self.rows as usize;
        let total = self.total_rows as usize;
        if total == 0 || rows == 0 {
            return 0;
        }
        ((rows * rows) / total).max(1) as u16
    }

    pub fn max_thumb_top(&self) -> u16 {
        self.rows.saturating_sub(self.thumb_size())
    }

    pub fn scroll_from_top_for_thumb(&self, thumb_top: u16) -> u16 {
        let max_thumb = self.max_thumb_top();
        let max_scroll = self.max_scroll();
        if max_thumb == 0 || max_scroll == 0 {
            return 0;
        }
        let thumb_top = thumb_top.min(max_thumb);
        let from_top =
            (thumb_top as u32 * max_scroll as u32 + max_thumb as u32 / 2) / max_thumb as u32;
        from_top.min(u16::MAX as u32) as u16
    }

    pub fn contains(&self, row: u16, col: u16) -> bool {
        col == self.col && row >= self.top_row && row < self.top_row + self.rows
    }
}

#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
pub(crate) struct Viewport {
    pub top_row: u16,
    pub rows: u16,
    pub content_width: u16,
    pub total_rows: u16,
    pub scroll_offset: u16,
    pub scrollbar: Option<ScrollbarGeom>,
}

#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
pub(crate) enum ViewportHit {
    Scrollbar { row: u16 },
    Content { row: u16, col: u16 },
}

impl Viewport {
    pub fn contains(&self, row: u16, _col: u16) -> bool {
        row >= self.top_row && row < self.top_row + self.rows
    }

    pub fn hit(&self, row: u16, col: u16) -> Option<ViewportHit> {
        if !self.contains(row, col) {
            return None;
        }
        if let Some(bar) = self.scrollbar {
            if col == bar.col {
                return Some(ViewportHit::Scrollbar {
                    row: row.saturating_sub(bar.top_row),
                });
            }
        }
        let rel_row = row - self.top_row;
        let max_col = self.content_width.saturating_sub(1);
        Some(ViewportHit::Content {
            row: rel_row,
            col: col.min(max_col),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(rows: u16, total: u16) -> ScrollbarGeom {
        ScrollbarGeom {
            col: 0,
            top_row: 0,
            rows,
            total_rows: total,
        }
    }

    #[test]
    fn click_top_jumps_to_start_click_bottom_jumps_to_end() {
        let b = bar(10, 40);
        assert_eq!(b.scroll_from_top_for_thumb(0), 0);
        assert_eq!(
            b.scroll_from_top_for_thumb(b.max_thumb_top()),
            b.max_scroll()
        );
    }

    #[test]
    fn click_middle_lands_near_middle_scroll() {
        let b = bar(10, 40);
        let mid_thumb = b.max_thumb_top() / 2;
        let s = b.scroll_from_top_for_thumb(mid_thumb);
        let half = b.max_scroll() / 2;
        let bucket = (b.max_scroll() + b.max_thumb_top() - 1) / b.max_thumb_top().max(1);
        assert!(
            s.abs_diff(half) <= bucket,
            "mid thumb {} mapped to scroll {} (expected ~{}, bucket {})",
            mid_thumb,
            s,
            half,
            bucket
        );
    }

    #[test]
    fn no_overflow_disables_bar_math() {
        let b = bar(10, 10);
        assert_eq!(b.max_scroll(), 0);
        assert_eq!(b.scroll_from_top_for_thumb(5), 0);
    }

    #[test]
    fn viewport_hit_test() {
        let vp = Viewport {
            top_row: 5,
            rows: 10,
            content_width: 78,
            total_rows: 50,
            scroll_offset: 0,
            scrollbar: Some(ScrollbarGeom {
                col: 79,
                top_row: 5,
                rows: 10,
                total_rows: 50,
            }),
        };
        assert!(vp.hit(3, 0).is_none());
        assert!(matches!(
            vp.hit(5, 0),
            Some(ViewportHit::Content { row: 0, .. })
        ));
        assert!(matches!(
            vp.hit(5, 79),
            Some(ViewportHit::Scrollbar { row: 0 })
        ));
    }
}
