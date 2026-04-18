//! Screen regions recorded during paint and consumed by event handlers
//! for mouse hit-testing and scroll math.
//!
//! Each region is a small record of *what the last paint pass drew and
//! where*. Mouse handlers look the event up in a region instead of
//! recomputing layout (viewport rows, total rows, scrollbar column) on
//! every tick. This keeps paint and hit-test in lockstep — the classic
//! source of "click is off by one column" / "click scrolls to the wrong
//! place" drift bugs.

/// Geometry of a single-column scrollbar painted during the last frame.
/// Stored with enough information to map a click row back into a scroll
/// offset without re-measuring the transcript.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ScrollbarGeom {
    /// Screen column the track lives on.
    pub col: u16,
    /// First screen row of the track.
    pub top_row: u16,
    /// Number of rows the track spans (viewport height).
    pub rows: u16,
    /// Total content rows at the time of paint.
    pub total_rows: u16,
}

impl ScrollbarGeom {
    /// Maximum scroll amount (in content rows) — how far above the
    /// bottom (or below the top, depending on the caller's convention)
    /// the buffer can be scrolled.
    pub fn max_scroll(&self) -> u16 {
        self.total_rows.saturating_sub(self.rows)
    }

    /// Thumb height in screen rows. Matches `scrollbar::Scrollbar::new`
    /// so hit-testing and painting agree on the geometry.
    pub fn thumb_size(&self) -> u16 {
        let rows = self.rows as usize;
        let total = self.total_rows as usize;
        if total == 0 || rows == 0 {
            return 0;
        }
        ((rows * rows) / total).max(1) as u16
    }

    /// Highest row the top of the thumb can occupy. When the content
    /// overflows by exactly one row this collapses to zero; callers
    /// should treat that as "no interactive scrollbar".
    pub fn max_thumb_top(&self) -> u16 {
        self.rows.saturating_sub(self.thumb_size())
    }

    /// Translate a thumb-top screen offset (0..=max_thumb_top) into a
    /// top-relative scroll offset (0..=max_scroll). This is the inverse
    /// of the forward mapping in `scrollbar::Scrollbar::new`: the thumb
    /// scale and the buffer scale are different (the thumb moves over
    /// `max_thumb_top` rows while the buffer moves over `max_scroll`),
    /// and this method handles the proportional mapping in one place.
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

    /// Is `(row, col)` inside the scrollbar track?
    pub fn contains(&self, row: u16, col: u16) -> bool {
        col == self.col && row >= self.top_row && row < self.top_row + self.rows
    }
}

/// Screen region occupied by the transcript viewport in the last frame.
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
pub(crate) struct TranscriptRegion {
    /// First screen row of the viewport.
    pub top_row: u16,
    /// Number of viewport rows.
    pub rows: u16,
    /// Width available for transcript content (excludes scrollbar column).
    pub content_width: u16,
    /// Scrollbar geometry when the content overflows; `None` otherwise.
    pub scrollbar: Option<ScrollbarGeom>,
    /// Total transcript rows at the time of paint.
    pub total_rows: u16,
    /// Clamped scroll offset (rows above the bottom of the viewport).
    pub scroll_offset: u16,
}

/// Result of hit-testing a mouse event against a `TranscriptRegion`.
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
pub(crate) enum TranscriptHit {
    /// Event lands on the scrollbar column — caller should jump/drag
    /// the viewport using `scroll_offset_for_row`. `row` is the screen
    /// row relative to `ScrollbarGeom::top_row`.
    Scrollbar { row: u16 },
    /// Event lands on the content area; `row` and `col` are 0-based
    /// within the viewport and `content_width`. Reserved for the
    /// upcoming `position_content_cursor_from_click` migration.
    Content { row: u16, col: u16 },
}

impl TranscriptRegion {
    /// Is `(row, col)` inside this region (either content or scrollbar)?
    pub fn contains(&self, row: u16, col: u16) -> bool {
        let _ = col;
        row >= self.top_row && row < self.top_row + self.rows
    }

    /// Classify a mouse event against this region. Returns `None` if the
    /// event is outside the viewport entirely.
    pub fn hit(&self, row: u16, col: u16) -> Option<TranscriptHit> {
        if !self.contains(row, col) {
            return None;
        }
        if let Some(bar) = self.scrollbar {
            if col == bar.col {
                return Some(TranscriptHit::Scrollbar {
                    row: row.saturating_sub(bar.top_row),
                });
            }
        }
        let rel_row = row - self.top_row;
        let max_col = self.content_width.saturating_sub(1);
        Some(TranscriptHit::Content {
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
}
