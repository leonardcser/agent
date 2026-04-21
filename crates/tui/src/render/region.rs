pub(crate) type ScrollbarGeom = ui::ScrollbarState;
pub(crate) type Viewport = ui::WindowViewport;
pub(crate) type ViewportHit = ui::ViewportHit;

#[cfg(test)]
mod tests {
    use super::*;
    use ui::Rect;

    fn bar(col: u16, rows: u16, total: u16) -> ScrollbarGeom {
        ScrollbarGeom::new(col, total, rows)
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
        let b = ScrollbarGeom::new(0, 10, 10);
        assert!(b.is_none());
    }

    #[test]
    fn viewport_hit_test() {
        let vp = Viewport::new(
            Rect::new(5, 0, 80, 10),
            78,
            50,
            0,
            ScrollbarGeom::new(79, 50, 10),
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
                let sb = super::super::scrollbar::Scrollbar::new(
                    total as usize,
                    rows as usize,
                    scroll_from_top as usize,
                );
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
                let sb2 = super::super::scrollbar::Scrollbar::new(
                    total as usize,
                    rows as usize,
                    click_scroll as usize,
                );
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
