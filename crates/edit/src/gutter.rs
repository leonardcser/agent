//! Per-row gutter column rendered to the left of buffer content.
//!
//! A [`GutterProvider`] is queried by the window renderer at paint time. It owns the
//! width of the reserved column and produces a [`GutterCell`] per row - text plus a
//! pre-resolved style. The window paints the gutter cells into the leftmost cells of
//! each row; content paint shifts right by `gutter_width`.
//!
//! Use this for line numbers, diff old/new line numbers, signs, fold markers - anything
//! that's per-row metadata distinct from the buffer's text content. The provider can
//! read the buffer and theme; it cannot mutate them.
//!
//! Provider impls **must** return cells of exactly `width()` display columns. The
//! renderer pads with spaces if the text is shorter and truncates if longer.

use std::sync::Mutex;

use smelt_buffer::buffer::{BufId, Buffer, SourceLine};
use smelt_buffer::style::Style;
use smelt_buffer::theme::Theme;

/// A single row's gutter content.
#[derive(Clone, Debug)]
pub struct GutterCell {
    pub text: String,
    pub style: Style,
}

/// Computes a fixed-width gutter column for a buffer. The width is buffer-dependent
/// (e.g. line numbers scale with line count) but stable per render frame; the renderer
/// caches it once per paint via [`GutterProvider::width`].
pub trait GutterProvider: Send + Sync {
    /// Reserved column count for the gutter. Queried once per render.
    fn width(&self, buf: &Buffer) -> u16;
    /// Cell content for an absolute buffer row. `None` → blank cells.
    fn cell(&self, buf: &Buffer, theme: &Theme, row: usize) -> Option<GutterCell>;
}

/// Line numbers, derived from each row's `SourceLine` metadata. Plain buffers with no
/// mapping fall back to `row + 1`. Diff buffers automatically render old + new columns
/// - the same provider works for both.
///
/// Layout:
/// - plain (all rows linear or unset): `" N "` (one pad each side).
/// - diff (any row carries `Diff`): `" O  N "` - old column, gap, new column.
/// - synthetic rows render as blanks of the full width.
///
/// Widths are cached by `(BufId, changedtick)` so the full-buffer scan runs once per
/// buffer mutation, not once per paint × row.
pub struct LineNumberGutter {
    cache: Mutex<Option<WidthsCache>>,
}

#[derive(Clone, Copy)]
struct WidthsCache {
    buf_id: BufId,
    changedtick: u64,
    widths: Widths,
}

impl Default for LineNumberGutter {
    fn default() -> Self {
        Self::new()
    }
}

impl LineNumberGutter {
    pub const fn new() -> Self {
        Self {
            cache: Mutex::new(None),
        }
    }
}

/// Scan source-line metadata across the buffer to size the gutter columns. `old_digits`
/// is 0 for non-diff buffers; `new_digits` is always set.
#[derive(Default, Clone, Copy)]
struct Widths {
    old_digits: u16,
    new_digits: u16,
}

fn digits_of(n: u32) -> u16 {
    let mut d: u16 = 1;
    let mut n = n.max(1);
    while n >= 10 {
        n /= 10;
        d += 1;
    }
    d
}

impl LineNumberGutter {
    /// Widen the gutter to fit explicit `SourceLine::Linear` / `SourceLine::Diff`
    /// stamps. Rows with no stamp (or `Synthetic`) contribute no width - a buffer
    /// whose rows are all unstamped/synthetic gets a zero-width gutter so unrelated
    /// content (e.g. transcript text rows) doesn't lose horizontal space.
    ///
    /// Cached by `(BufId, changedtick)`: a stable buffer hits the cache on every
    /// subsequent `width()` / `cell()` call within a paint and across paints.
    fn widths(&self, buf: &Buffer) -> Widths {
        let buf_id = buf.id();
        let changedtick = buf.changedtick();
        if let Ok(guard) = self.cache.lock() {
            if let Some(entry) = guard.as_ref() {
                if entry.buf_id == buf_id && entry.changedtick == changedtick {
                    return entry.widths;
                }
            }
        }
        let widths = self.compute_widths(buf);
        if let Ok(mut guard) = self.cache.lock() {
            *guard = Some(WidthsCache {
                buf_id,
                changedtick,
                widths,
            });
        }
        widths
    }

    fn compute_widths(&self, buf: &Buffer) -> Widths {
        let mut w = match buf.source_line_bounds() {
            Some(SourceLine::Diff { old, new }) => Widths {
                old_digits: digits_of(old.unwrap_or(0)),
                new_digits: digits_of(new.unwrap_or(0)),
            },
            Some(SourceLine::Linear { lineno }) => Widths {
                old_digits: 0,
                new_digits: digits_of(lineno),
            },
            Some(SourceLine::Synthetic) | None => Widths::default(),
        };
        for row in 0..buf.line_count() {
            match buf.source_line_at(row) {
                Some(SourceLine::Linear { lineno }) => {
                    w.new_digits = w.new_digits.max(digits_of(lineno));
                }
                Some(SourceLine::Diff { old, new }) => {
                    if let Some(o) = old {
                        w.old_digits = w.old_digits.max(digits_of(o));
                    }
                    if let Some(nn) = new {
                        w.new_digits = w.new_digits.max(digits_of(nn));
                    }
                }
                Some(SourceLine::Synthetic) | None => {}
            }
        }
        w
    }
}

impl GutterProvider for LineNumberGutter {
    fn width(&self, buf: &Buffer) -> u16 {
        let w = self.widths(buf);
        if w.new_digits == 0 && w.old_digits == 0 {
            // No stamped rows → no gutter column.
            0
        } else if w.old_digits == 0 {
            // Plain: " N "
            w.new_digits + 2
        } else {
            // Diff: " O  N "
            w.old_digits + 2 + w.new_digits + 2
        }
    }

    fn cell(&self, buf: &Buffer, theme: &Theme, row: usize) -> Option<GutterCell> {
        if row >= buf.line_count() {
            return None;
        }
        let widths = self.widths(buf);
        let total_width = self.width(buf) as usize;
        if total_width == 0 {
            return None;
        }
        let style = theme.get("Comment");
        let text = match buf.source_line_at(row) {
            // Rows without an explicit stamp render as blanks of the full width
            // so neighbouring stamped rows align cleanly.
            Some(SourceLine::Synthetic) | None => " ".repeat(total_width),
            Some(SourceLine::Diff { old, new }) => {
                if widths.old_digits == 0 {
                    // Defensive: caller marked Diff but no old digits found.
                    format_one(new.unwrap_or(row as u32 + 1), widths.new_digits)
                } else {
                    format_two(old, new, widths.old_digits, widths.new_digits)
                }
            }
            Some(SourceLine::Linear { lineno }) => {
                if widths.old_digits == 0 {
                    format_one(lineno, widths.new_digits)
                } else {
                    // Linear row inside a diff buffer: align under the "new" column.
                    format_two(None, Some(lineno), widths.old_digits, widths.new_digits)
                }
            }
        };
        Some(GutterCell { text, style })
    }
}

fn format_one(n: u32, digits: u16) -> String {
    format!(" {n:>w$} ", w = digits as usize)
}

fn format_two(old: Option<u32>, new: Option<u32>, old_w: u16, new_w: u16) -> String {
    let old_cell = match old {
        Some(n) => format!("{n:>w$}", w = old_w as usize),
        None => " ".repeat(old_w as usize),
    };
    let new_cell = match new {
        Some(n) => format!("{n:>w$}", w = new_w as usize),
        None => " ".repeat(new_w as usize),
    };
    format!(" {old_cell}  {new_cell} ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use smelt_buffer::buffer::{BufCreateOpts, BufId, Buffer, LineDecoration};

    fn buf_with_lines(id: u64, n: usize) -> Buffer {
        let mut buf = Buffer::new(BufId(id), BufCreateOpts::default());
        let lines: Vec<String> = (0..n).map(|i| format!("line {i}")).collect();
        buf.set_all_lines(lines);
        buf
    }

    fn stamp_source_line(buf: &mut Buffer, row: usize, sl: SourceLine) {
        let dec = LineDecoration {
            source_line: Some(sl),
            ..buf.decoration_at(row).clone()
        };
        buf.set_decoration(row, dec);
    }

    #[test]
    fn unstamped_buffer_has_no_gutter() {
        let g = LineNumberGutter::new();
        // Without any `SourceLine` stamps the gutter is zero-width and
        // produces no per-row cell - text-only buffers don't lose horizontal
        // space to an empty column.
        let buf = buf_with_lines(0, 12);
        let theme = Theme::new();
        assert_eq!(g.width(&buf), 0);
        assert!(g.cell(&buf, &theme, 0).is_none());
    }

    #[test]
    fn plain_buffer_width_scales_with_stamped_linenos() {
        let g = LineNumberGutter::new();
        let theme = Theme::new();
        let mut single = buf_with_lines(0, 1);
        stamp_source_line(&mut single, 0, SourceLine::Linear { lineno: 1 });
        assert_eq!(g.width(&single), 3); // " 1 "
        let mut wide = buf_with_lines(1, 1);
        stamp_source_line(&mut wide, 0, SourceLine::Linear { lineno: 100 });
        assert_eq!(g.width(&wide), 5); // " 100 "
        assert_eq!(g.cell(&wide, &theme, 0).unwrap().text, " 100 ");
    }

    #[test]
    fn linear_source_line_overrides_row_index() {
        let g = LineNumberGutter::new();
        let mut buf = buf_with_lines(0, 2);
        stamp_source_line(&mut buf, 0, SourceLine::Linear { lineno: 42 });
        stamp_source_line(&mut buf, 1, SourceLine::Linear { lineno: 43 });
        let theme = Theme::new();
        assert_eq!(g.cell(&buf, &theme, 0).unwrap().text, " 42 ");
        assert_eq!(g.cell(&buf, &theme, 1).unwrap().text, " 43 ");
    }

    #[test]
    fn diff_buffer_renders_old_and_new_columns() {
        let g = LineNumberGutter::new();
        let mut buf = buf_with_lines(0, 3);
        stamp_source_line(
            &mut buf,
            0,
            SourceLine::Diff {
                old: Some(10),
                new: Some(10),
            },
        );
        stamp_source_line(
            &mut buf,
            1,
            SourceLine::Diff {
                old: None,
                new: Some(11),
            },
        );
        stamp_source_line(
            &mut buf,
            2,
            SourceLine::Diff {
                old: Some(11),
                new: None,
            },
        );
        let theme = Theme::new();
        // widths: old_digits = 2 (max 11), new_digits = 2 (max 11).
        // layout: " OO  NN "
        assert_eq!(g.cell(&buf, &theme, 0).unwrap().text, " 10  10 ");
        assert_eq!(g.cell(&buf, &theme, 1).unwrap().text, "     11 ");
        assert_eq!(g.cell(&buf, &theme, 2).unwrap().text, " 11     ");
    }

    #[test]
    fn synthetic_row_renders_blank() {
        let g = LineNumberGutter::new();
        let mut buf = buf_with_lines(0, 2);
        stamp_source_line(&mut buf, 0, SourceLine::Linear { lineno: 1 });
        stamp_source_line(&mut buf, 1, SourceLine::Synthetic);
        let theme = Theme::new();
        let cell = g.cell(&buf, &theme, 1).unwrap();
        let w = g.width(&buf) as usize;
        assert_eq!(cell.text, " ".repeat(w));
    }

    #[test]
    fn widths_recompute_when_changedtick_advances() {
        let g = LineNumberGutter::new();
        let mut buf = buf_with_lines(0, 1);
        stamp_source_line(&mut buf, 0, SourceLine::Linear { lineno: 1 });
        assert_eq!(g.width(&buf), 3); // " 1 "
                                      // Mutate to require more digits - cache must invalidate via changedtick.
        stamp_source_line(&mut buf, 0, SourceLine::Linear { lineno: 9999 });
        assert_eq!(g.width(&buf), 6); // " 9999 "
    }
}
