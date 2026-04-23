//! Shared scrollbar helper: one source of truth for thumb math and
//! painting a single-column track on the viewport edge. Used by the
//! prompt input buffer and the content pane so both panes look and
//! behave identically.

/// Thumb geometry for a single-column scrollbar over `visible_rows`
/// rows showing `total_rows` of content, scrolled by `scroll_offset`
/// rows from the top.
#[derive(Clone, Copy)]
pub(super) struct Scrollbar {
    thumb_start: usize,
    thumb_end: usize,
    pub visible: bool,
}

impl Scrollbar {
    pub(super) fn new(total_rows: usize, visible_rows: usize, scroll_offset: usize) -> Self {
        if total_rows <= visible_rows || visible_rows == 0 {
            return Self {
                thumb_start: 0,
                thumb_end: 0,
                visible: false,
            };
        }
        let thumb_size = (visible_rows * visible_rows / total_rows).max(1);
        let max_scroll = total_rows - visible_rows;
        let max_thumb = visible_rows.saturating_sub(thumb_size);
        let thumb_pos = (scroll_offset * max_thumb + max_scroll / 2)
            .checked_div(max_scroll)
            .unwrap_or(0)
            .min(max_thumb);
        Self {
            thumb_start: thumb_pos,
            thumb_end: thumb_pos + thumb_size,
            visible: true,
        }
    }

    /// `true` if row index `i` (0-based within the visible window) is
    /// on the thumb, `false` if on the track.
    pub(super) fn is_thumb(&self, i: usize) -> bool {
        self.visible && i >= self.thumb_start && i < self.thumb_end
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ui::{Rect, ScrollbarState, ViewportHit, WindowViewport};

    fn bar(col: u16, rows: u16, total: u16) -> ScrollbarState {
        ScrollbarState::new(col, total, rows)
            .expect("scrollbar should exist when content overflows viewport")
    }

    #[test]
    fn click_top_jumps_to_start_click_bottom_jumps_to_end() {
        let b = bar(0, 10, 40);
        assert_eq!(b.scroll_from_top_for_thumb(0), 0);
        assert_eq!(
            b.scroll_from_top_for_thumb(b.max_thumb_top()),
            b.max_scroll()
        );
    }

    #[test]
    fn click_middle_lands_near_middle_scroll() {
        let b = bar(0, 10, 40);
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
        let b = ScrollbarState::new(0, 10, 10);
        assert!(b.is_none());
    }

    #[test]
    fn viewport_hit_test() {
        let vp = WindowViewport::new(
            Rect::new(5, 0, 80, 10),
            78,
            50,
            0,
            ScrollbarState::new(79, 50, 10),
        );
        assert!(vp.hit(3, 0).is_none());
        assert!(matches!(
            vp.hit(5, 0),
            Some(ViewportHit::Content { row: 0, .. })
        ));
        assert!(matches!(vp.hit(5, 79), Some(ViewportHit::Scrollbar)));
    }

    #[test]
    fn scrollbar_render_click_roundtrip() {
        for &(rows, total) in &[(10u16, 40u16), (20, 100), (5, 50), (30, 31)] {
            let b = bar(0, rows, total);
            let max_scroll = b.max_scroll();
            if max_scroll == 0 {
                continue;
            }
            for scroll_from_top in 0..=max_scroll {
                let sb = Scrollbar::new(total as usize, rows as usize, scroll_from_top as usize);
                let mut thumb_top = None;
                for i in 0..rows as usize {
                    if sb.is_thumb(i) {
                        thumb_top = Some(i as u16);
                        break;
                    }
                }
                let Some(thumb_top) = thumb_top else {
                    continue;
                };
                let click_scroll = b.scroll_from_top_for_thumb(thumb_top);
                let sb2 = Scrollbar::new(total as usize, rows as usize, click_scroll as usize);
                let mut thumb_top2 = None;
                for i in 0..rows as usize {
                    if sb2.is_thumb(i) {
                        thumb_top2 = Some(i as u16);
                        break;
                    }
                }
                assert_eq!(
                    thumb_top,
                    thumb_top2.unwrap_or(0),
                    "roundtrip failed: rows={rows} total={total} scroll={scroll_from_top} thumb={thumb_top} click_scroll={click_scroll} thumb2={thumb_top2:?}"
                );
            }
        }
    }
}
