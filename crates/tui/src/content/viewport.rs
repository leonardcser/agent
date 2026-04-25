//! Viewport geometry: the single source of truth for
//! `(viewport_rows, total, scroll_top) ↔ (row_on_screen, line_in_buffer)`.
//!
//! Top-relative convention: `scroll_top` is the index of the first
//! visible content line. `0` = first line of content at the top of the
//! viewport. `max_scroll()` = last page of content visible (stuck to
//! bottom / newest).
//!
//! When `total < viewport_rows`, content top-anchors: the first content
//! line is at screen row 0, with trailing blank rows below.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ViewportGeom {
    pub total: u16,
    pub viewport_rows: u16,
    pub scroll_top: u16,
}

impl ViewportGeom {
    pub fn new(total: u16, viewport_rows: u16, scroll_top: u16) -> Self {
        Self {
            total,
            viewport_rows,
            scroll_top,
        }
    }

    /// Maximum scroll offset — any larger value clamps to this.
    /// `scroll_top == max_scroll()` means stuck to the bottom.
    pub fn max_scroll(&self) -> u16 {
        self.total.saturating_sub(self.viewport_rows)
    }

    /// Normalized scroll position (clamped to `[0, max_scroll]`).
    pub fn clamped_scroll(&self) -> u16 {
        self.scroll_top.min(self.max_scroll())
    }

    /// Trailing blank rows below the content when it's shorter
    /// than the viewport (content top-anchors).
    pub fn trailing_blanks(&self) -> u16 {
        self.viewport_rows.saturating_sub(self.total)
    }

    /// Lines to skip from the top of the flattened transcript before
    /// painting the viewport slice. With top-relative scroll this is
    /// just the clamped scroll position.
    pub fn skip_from_top(&self) -> u16 {
        self.clamped_scroll()
    }

    /// Number of content rows visible in the viewport.
    pub fn visible_content_rows(&self) -> u16 {
        self.total.min(self.viewport_rows)
    }

    /// Screen row for a buffer line index, or `None` if offscreen.
    /// Lines are 0-indexed from the top of the flattened buffer; the
    /// returned row is 0-indexed from the top of the viewport.
    pub fn row_of_line(&self, line_idx: u16) -> Option<u16> {
        if line_idx >= self.total {
            return None;
        }
        let skip = self.skip_from_top();
        if line_idx < skip {
            return None;
        }
        let row = line_idx - skip;
        (row < self.viewport_rows).then_some(row)
    }

    /// Buffer line index for a screen row, or `None` if the row lands in
    /// a trailing blank (no content at that row).
    pub fn line_of_row(&self, row: u16) -> Option<u16> {
        let skip = self.skip_from_top();
        let line = skip.saturating_add(row);
        (line < self.total).then_some(line)
    }

    /// `true` when the viewport is snapped to the newest content.
    pub fn stuck_to_bottom(&self) -> bool {
        self.clamped_scroll() >= self.max_scroll()
    }

    /// Apply a `delta` growth in total lines while preserving the user's
    /// visual pin (their top-row stays on the same content line).
    ///
    /// Pin semantics: if `stuck_to_bottom()` was `true`, grows
    /// `scroll_top` to stay stuck. Otherwise, holds `scroll_top`
    /// constant so the same content stays onscreen.
    pub fn apply_growth(&mut self, delta: u16) {
        let was_stuck = self.stuck_to_bottom();
        self.total = self.total.saturating_add(delta);
        if was_stuck {
            self.scroll_top = self.max_scroll();
        }
        self.scroll_top = self.clamped_scroll();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_content_top_anchors() {
        let g = ViewportGeom::new(3, 10, 0);
        assert_eq!(g.trailing_blanks(), 7);
        assert_eq!(g.skip_from_top(), 0);
        assert_eq!(g.row_of_line(0), Some(0));
        assert_eq!(g.row_of_line(2), Some(2));
        assert_eq!(g.row_of_line(3), None);
        assert_eq!(g.line_of_row(0), Some(0));
        assert_eq!(g.line_of_row(2), Some(2));
        assert_eq!(g.line_of_row(3), None); // trailing blank
    }

    #[test]
    fn exact_fit_no_trailing_blanks() {
        let g = ViewportGeom::new(10, 10, 0);
        assert_eq!(g.trailing_blanks(), 0);
        assert_eq!(g.max_scroll(), 0);
        assert_eq!(g.row_of_line(0), Some(0));
        assert_eq!(g.row_of_line(9), Some(9));
        assert_eq!(g.row_of_line(10), None);
    }

    #[test]
    fn overflow_scrollable_bottom() {
        // scroll_top = max_scroll = 10 → stuck to bottom
        let g = ViewportGeom::new(20, 10, 10);
        assert!(g.stuck_to_bottom());
        assert_eq!(g.skip_from_top(), 10);
        assert_eq!(g.row_of_line(10), Some(0));
        assert_eq!(g.row_of_line(19), Some(9));
        assert_eq!(g.row_of_line(9), None);
        assert_eq!(g.line_of_row(0), Some(10));
        assert_eq!(g.line_of_row(9), Some(19));
    }

    #[test]
    fn overflow_scrollable_mid() {
        let g = ViewportGeom::new(20, 10, 5);
        assert!(!g.stuck_to_bottom());
        assert_eq!(g.skip_from_top(), 5);
        assert_eq!(g.row_of_line(5), Some(0));
        assert_eq!(g.row_of_line(14), Some(9));
        assert_eq!(g.line_of_row(0), Some(5));
    }

    #[test]
    fn scroll_clamps_to_max() {
        let g = ViewportGeom::new(20, 10, 999);
        assert_eq!(g.clamped_scroll(), 10);
        assert_eq!(g.skip_from_top(), 10);
        assert_eq!(g.line_of_row(0), Some(10));
        assert_eq!(g.line_of_row(9), Some(19));
    }

    #[test]
    fn apply_growth_preserves_pin() {
        // Scrolled to line 5 (not stuck). Growth adds 3 rows below.
        // scroll_top stays at 5 — same content visible.
        let mut g = ViewportGeom::new(20, 10, 5);
        g.apply_growth(3);
        assert_eq!(g.total, 23);
        assert_eq!(g.scroll_top, 5);
        assert_eq!(g.skip_from_top(), 5);
    }

    #[test]
    fn apply_growth_stuck_stays_stuck() {
        // Stuck to bottom (scroll_top = max_scroll = 10).
        let mut g = ViewportGeom::new(20, 10, 10);
        assert!(g.stuck_to_bottom());
        g.apply_growth(5);
        assert!(g.stuck_to_bottom());
        assert_eq!(g.total, 25);
        assert_eq!(g.scroll_top, 15); // new max_scroll
    }

    #[test]
    fn empty_buffer_all_trailing_blanks() {
        let g = ViewportGeom::new(0, 10, 0);
        assert_eq!(g.trailing_blanks(), 10);
        assert_eq!(g.row_of_line(0), None);
        for row in 0..10 {
            assert_eq!(g.line_of_row(row), None);
        }
    }

    #[test]
    fn matrix_row_line_roundtrip() {
        let viewport = 10u16;
        for &total in &[0u16, 1, viewport - 1, viewport, viewport + 1, 2 * viewport] {
            let max = total.saturating_sub(viewport);
            for &scroll in &[0u16, 1, max.saturating_sub(1), max, max + 1] {
                let g = ViewportGeom::new(total, viewport, scroll);
                for row in 0..viewport {
                    if let Some(line) = g.line_of_row(row) {
                        assert_eq!(
                            g.row_of_line(line),
                            Some(row),
                            "roundtrip failed total={total} scroll={scroll} row={row} line={line}"
                        );
                    }
                }
                let skip = g.skip_from_top();
                if skip > 0 {
                    assert_eq!(g.row_of_line(skip - 1), None);
                }
                if total > 0 {
                    assert_eq!(g.row_of_line(total), None);
                }
                assert!(g.clamped_scroll() <= g.max_scroll());
            }
        }
    }

    #[test]
    fn single_line_viewport_one() {
        let g = ViewportGeom::new(1, 1, 0);
        assert_eq!(g.trailing_blanks(), 0);
        assert_eq!(g.row_of_line(0), Some(0));
        assert_eq!(g.line_of_row(0), Some(0));
    }

    #[test]
    fn apply_growth_clamps_to_new_max() {
        let mut g = ViewportGeom::new(10, 10, 0);
        assert!(g.stuck_to_bottom());
        g.apply_growth(100);
        assert_eq!(g.total, 110);
        assert!(g.stuck_to_bottom());
        assert_eq!(g.scroll_top, 100);
    }

    #[test]
    fn trailing_blank_click_returns_none() {
        let g = ViewportGeom::new(3, 10, 0);
        assert_eq!(g.line_of_row(0), Some(0));
        assert_eq!(g.line_of_row(2), Some(2));
        for row in 3..10 {
            assert_eq!(g.line_of_row(row), None, "row {row} should be blank");
        }
    }

    #[test]
    fn zero_viewport_never_panics() {
        let g = ViewportGeom::new(10, 0, 5);
        assert_eq!(g.trailing_blanks(), 0);
        let _ = g.line_of_row(0);
        let _ = g.row_of_line(0);
    }

    #[test]
    fn scrolled_to_top() {
        let g = ViewportGeom::new(20, 10, 0);
        assert!(!g.stuck_to_bottom());
        assert_eq!(g.skip_from_top(), 0);
        assert_eq!(g.row_of_line(0), Some(0));
        assert_eq!(g.row_of_line(9), Some(9));
        assert_eq!(g.row_of_line(10), None);
    }
}
