//! Shared scrollbar helper: one source of truth for thumb math and
//! painting a single-column track on the viewport edge. Used by the
//! prompt input buffer and the content pane so both panes look and
//! behave identically.

use super::RenderOut;
use crate::theme;
use crossterm::{cursor, QueueableCommand};

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
        let thumb_pos = scroll_offset
            .saturating_mul(visible_rows.saturating_sub(thumb_size))
            .checked_div(max_scroll)
            .unwrap_or(0);
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

/// Paint the scrollbar as a single column at `col`, drawing `rows`
/// rows starting at screen row `top_row`. No-op when the scrollbar
/// is not needed.
pub(super) fn paint_column(
    out: &mut RenderOut,
    col: u16,
    top_row: u16,
    rows: u16,
    bar: &Scrollbar,
) {
    if !bar.visible {
        return;
    }
    for i in 0..rows {
        let bg = if bar.is_thumb(i as usize) {
            theme::scrollbar_thumb()
        } else {
            theme::scrollbar_track()
        };
        let _ = out.queue(cursor::MoveTo(col, top_row + i));
        out.push_bg(bg);
        out.print(" ");
        out.pop_style();
    }
}
