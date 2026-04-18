//! Viewport geometry: the single source of truth for
//! `(viewport_rows, total, scroll_offset) ↔ (row_on_screen, line_in_buffer)`.
//!
//! Alt-buffer convention: `scroll_offset` measures rows from the bottom of
//! the content. `0` = stuck to the bottom (newest content at the last row).
//! Increasing `scroll_offset` moves the viewport upward through older
//! content; `max_scroll()` clamps to the top.
//!
//! When `total < viewport_rows`, the short content bottom-anchors with
//! `leading_blanks = viewport_rows - total` empty rows at the top, matching
//! `BlockHistory::paint_viewport` and `viewport_text`.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ViewportGeom {
    pub total: u16,
    pub viewport_rows: u16,
    pub scroll_offset: u16,
}

impl ViewportGeom {
    pub fn new(total: u16, viewport_rows: u16, scroll_offset: u16) -> Self {
        Self {
            total,
            viewport_rows,
            scroll_offset,
        }
    }

    /// Maximum scroll offset — any larger value clamps to this.
    pub fn max_scroll(&self) -> u16 {
        self.total.saturating_sub(self.viewport_rows)
    }

    /// Normalized scroll offset.
    pub fn clamped_scroll(&self) -> u16 {
        self.scroll_offset.min(self.max_scroll())
    }

    /// Leading blank rows painted above the content when it's shorter
    /// than the viewport (content bottom-anchors).
    pub fn leading_blanks(&self) -> u16 {
        self.viewport_rows.saturating_sub(self.total)
    }

    /// Lines to skip from the top of the flattened transcript before
    /// painting the viewport slice.
    pub fn skip_from_top(&self) -> u16 {
        let max = self.max_scroll();
        max.saturating_sub(self.clamped_scroll())
    }

    /// Screen row for a buffer line index, or `None` if offscreen.
    /// Lines are 0-indexed from the top of the flattened buffer; the
    /// returned row is 0-indexed from the top of the viewport.
    pub fn row_of_line(&self, line_idx: u16) -> Option<u16> {
        let skip = self.skip_from_top();
        let leading = self.leading_blanks();
        if line_idx < skip {
            return None;
        }
        let offset = line_idx - skip;
        let row = leading + offset;
        (row < self.viewport_rows).then_some(row)
    }

    /// Buffer line index for a screen row, or `None` if the row lands in
    /// a leading blank (no content at that row).
    pub fn line_of_row(&self, row: u16) -> Option<u16> {
        let leading = self.leading_blanks();
        if row < leading {
            return None;
        }
        let offset = row - leading;
        let skip = self.skip_from_top();
        let line = skip.saturating_add(offset);
        (line < self.total).then_some(line)
    }

    /// `true` when the viewport is snapped to the newest content.
    pub fn stuck_to_bottom(&self) -> bool {
        self.clamped_scroll() == 0
    }

    /// Apply a `delta` growth in total lines while preserving the user's
    /// visual pin (their top-row stays on the same content line).
    ///
    /// Pin semantics: if `stuck_to_bottom()` was `true`, stays stuck.
    /// Otherwise, grows `scroll_offset` so the same line-range remains
    /// onscreen even as the transcript gets taller below.
    pub fn apply_growth(&mut self, delta: u16) {
        if !self.stuck_to_bottom() {
            self.scroll_offset = self.scroll_offset.saturating_add(delta);
        }
        self.total = self.total.saturating_add(delta);
        self.scroll_offset = self.clamped_scroll();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_content_bottom_anchors() {
        let g = ViewportGeom::new(3, 10, 0);
        assert_eq!(g.leading_blanks(), 7);
        assert_eq!(g.skip_from_top(), 0);
        assert_eq!(g.row_of_line(0), Some(7));
        assert_eq!(g.row_of_line(2), Some(9));
        assert_eq!(g.row_of_line(3), None);
        assert_eq!(g.line_of_row(6), None); // leading blank
        assert_eq!(g.line_of_row(7), Some(0));
        assert_eq!(g.line_of_row(9), Some(2));
    }

    #[test]
    fn exact_fit_no_leading_blanks() {
        let g = ViewportGeom::new(10, 10, 0);
        assert_eq!(g.leading_blanks(), 0);
        assert_eq!(g.max_scroll(), 0);
        assert_eq!(g.row_of_line(0), Some(0));
        assert_eq!(g.row_of_line(9), Some(9));
        assert_eq!(g.row_of_line(10), None);
    }

    #[test]
    fn overflow_scrollable_bottom() {
        let g = ViewportGeom::new(20, 10, 0);
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
        assert_eq!(g.skip_from_top(), 0);
        assert_eq!(g.line_of_row(0), Some(0));
        assert_eq!(g.line_of_row(9), Some(9));
    }

    #[test]
    fn apply_growth_preserves_pin() {
        let mut g = ViewportGeom::new(20, 10, 5);
        g.apply_growth(3);
        // Same content window still shown: skip_from_top stays 5.
        assert_eq!(g.total, 23);
        assert_eq!(g.scroll_offset, 8);
        assert_eq!(g.skip_from_top(), 5);
    }

    #[test]
    fn apply_growth_stuck_stays_stuck() {
        let mut g = ViewportGeom::new(20, 10, 0);
        g.apply_growth(5);
        assert!(g.stuck_to_bottom());
        assert_eq!(g.total, 25);
        assert_eq!(g.scroll_offset, 0);
    }

    #[test]
    fn empty_buffer_all_leading_blanks() {
        let g = ViewportGeom::new(0, 10, 0);
        assert_eq!(g.leading_blanks(), 10);
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
                // row_of_line ∘ line_of_row = id for every onscreen row.
                for row in 0..viewport {
                    if let Some(line) = g.line_of_row(row) {
                        assert_eq!(
                            g.row_of_line(line),
                            Some(row),
                            "roundtrip failed total={total} scroll={scroll} row={row} line={line}"
                        );
                    }
                }
                // Lines before skip and past total map to None.
                let skip = g.skip_from_top();
                if skip > 0 {
                    assert_eq!(g.row_of_line(skip - 1), None);
                }
                if total > 0 {
                    assert_eq!(g.row_of_line(total), None);
                }
                // The clamp never produces scroll > max_scroll.
                assert!(g.clamped_scroll() <= g.max_scroll());
            }
        }
    }

    #[test]
    fn single_line_viewport_one() {
        let g = ViewportGeom::new(1, 1, 0);
        assert_eq!(g.leading_blanks(), 0);
        assert_eq!(g.row_of_line(0), Some(0));
        assert_eq!(g.line_of_row(0), Some(0));
    }

    #[test]
    fn apply_growth_clamps_to_new_max() {
        let mut g = ViewportGeom::new(10, 10, 0);
        // Viewport exactly filled — scroll==0 is stuck-to-bottom.
        g.apply_growth(100);
        assert_eq!(g.total, 110);
        assert!(g.stuck_to_bottom());
    }

    #[test]
    fn zero_viewport_never_panics() {
        let g = ViewportGeom::new(10, 0, 5);
        // With no visible rows, the concept of "which row is line N"
        // is degenerate; just exercise the code paths don't panic.
        assert_eq!(g.leading_blanks(), 0);
        let _ = g.line_of_row(0);
        let _ = g.row_of_line(0);
    }
}
