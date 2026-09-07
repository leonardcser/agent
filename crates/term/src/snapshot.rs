//! Frame snapshot: a faithful, round-trippable capture of a rendered
//! terminal frame.
//!
//! Lifecycle:
//! - [`SnapshotFrame::from_grid`] captures a `Grid` snapshot.
//! - [`SnapshotFrame::text`] / [`SnapshotFrame::styles_text`] produce
//!   the diff-friendly text views consumed by `insta` (and humans).
//! - [`SnapshotFrame::parse`] is the lossless inverse of the two views.
//! - [`SnapshotFrame::blit_into`] replays the captured cells into a
//!   live `Grid` (e.g. the storybook viewer).
//!
//! The text views collapse wide-char continuation cells (`\0`) to
//! `' '` and `text()` removes trailing blank cells for diff cleanliness.
//! The styles sidecar carries a `dim: W H` header so `parse` recovers cell
//! dimensions losslessly - without it, a trailing wide-char continuation
//! that was removed would be unrecoverable.

use super::grid::{Cell, Grid, Style};
use smelt_style::{cell_width, style::Color};

/// Structured copy of one rendered frame. Wide-char continuation cells
/// (`\0`) collapse to a space in `rows`.
#[derive(Clone, Debug)]
pub struct SnapshotFrame {
    pub width: u16,
    pub height: u16,
    pub rows: Vec<String>,
    pub styles: Vec<Vec<Style>>,
}

impl SnapshotFrame {
    pub fn from_grid(grid: &Grid) -> Self {
        let w = grid.width();
        let h = grid.height();
        let mut rows = Vec::with_capacity(h as usize);
        let mut styles = Vec::with_capacity(h as usize);
        for y in 0..h {
            let mut row = String::with_capacity(w as usize);
            let mut row_styles = Vec::with_capacity(w as usize);
            for x in 0..w {
                let cell: &Cell = grid.cell(x, y);
                if cell.symbol.is_continuation() {
                    row.push(' ');
                } else {
                    row.push_str(cell.symbol.as_str());
                }
                row_styles.push(cell.style);
            }
            rows.push(row);
            styles.push(row_styles);
        }
        Self {
            width: w,
            height: h,
            rows,
            styles,
        }
    }

    /// Rows joined by `\n` with trailing blank cells stripped per row.
    pub fn text(&self) -> String {
        let mut out = String::with_capacity(self.rows.iter().map(|r| r.len() + 1).sum());
        for (i, row) in self.rows.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            let mut end = row.len();
            for (start, grapheme) in cell_width::grapheme_indices(row).rev() {
                if grapheme != " " {
                    break;
                }
                end = start;
            }
            out.push_str(&row[..end]);
        }
        out
    }

    /// Style sidecar. First line is `dim: W H`; remaining lines are
    /// one styled run each as `row col len fg=… bg=… attrs=…`. The
    /// dim header makes [`parse`](Self::parse) losslessly recover cell dimensions
    /// even when [`text`](Self::text) has removed trailing wide-char continuations.
    pub fn styles_text(&self) -> String {
        let mut out = format!("dim: {} {}\n", self.width, self.height);
        for (y, row) in self.styles.iter().enumerate() {
            let mut x = 0usize;
            while x < row.len() {
                let s = row[x];
                if s == Style::default() {
                    x += 1;
                    continue;
                }
                let mut end = x + 1;
                while end < row.len() && row[end] == s {
                    end += 1;
                }
                let len = end - x;
                out.push_str(&format!("{y:>3} {x:>3} {len:>3} {}\n", fmt_style(&s)));
                x = end;
            }
        }
        out
    }

    /// Reconstruct a frame from its text + styles views. Lossless
    /// when `styles_text` carries the `dim:` header. Without the
    /// header, dimensions are inferred from `text` (wide-char aware),
    /// which is correct unless a row ended on a wide glyph whose
    /// continuation slot was trimmed.
    pub fn parse(text: &str, styles_text: &str) -> Self {
        let header = parse_dim_header(styles_text);
        let text_lines: Vec<&str> = text.split('\n').collect();
        let inferred_width = text_lines
            .iter()
            .map(|l| row_visual_width(l))
            .max()
            .unwrap_or(0);
        let inferred_height = text_lines.len() as u16;
        let span_extent = parse_spans(styles_text)
            .iter()
            .map(|(y, c, len, _)| (*y + 1, *c + *len))
            .fold((0u16, 0u16), |(h, w), (rh, rw)| (h.max(rh), w.max(rw)));
        let width = header
            .map(|(w, _)| w)
            .unwrap_or(0)
            .max(inferred_width)
            .max(span_extent.1);
        let height = header
            .map(|(_, h)| h)
            .unwrap_or(0)
            .max(inferred_height)
            .max(span_extent.0);

        let mut rows = Vec::with_capacity(height as usize);
        for y in 0..height as usize {
            let line = text_lines.get(y).copied().unwrap_or("");
            rows.push(parse_row(line, width));
        }

        let mut styles = vec![vec![Style::default(); width as usize]; height as usize];
        for (y, c, len, style) in parse_spans(styles_text) {
            if y >= height {
                continue;
            }
            let start = c as usize;
            let end = ((c + len) as usize).min(width as usize);
            if start >= end {
                continue;
            }
            for cell in &mut styles[y as usize][start..end] {
                *cell = style;
            }
        }

        Self {
            width,
            height,
            rows,
            styles,
        }
    }

    /// Paint this frame's cells into `grid` starting at `(x_off, y_off)`,
    /// clipped to the grid bounds. Wide-char continuation slots in
    /// `rows` (stored as `' '` after a wide glyph) are reconstructed
    /// as `\0` so the destination grid's flush logic advances by the
    /// glyph's visual width and renders the same way the source did.
    pub fn blit_into(&self, grid: &mut Grid, x_off: u16, y_off: u16) {
        for (r, row) in self.rows.iter().enumerate() {
            let y = y_off.saturating_add(r as u16);
            if y >= grid.height() {
                break;
            }
            let mut graphemes = cell_width::graphemes(row).peekable();
            let mut cell_index: usize = 0;
            while let Some(grapheme) = graphemes.next() {
                let x = x_off.saturating_add(cell_index as u16);
                if x >= grid.width() {
                    break;
                }
                let width = cell_width::text_width(grapheme);
                let style = self
                    .styles
                    .get(r)
                    .and_then(|row| row.get(cell_index).copied())
                    .unwrap_or_default();
                grid.set_symbol(x, y, grapheme, style);
                if width == 2 {
                    // Snapshot rows serialize the continuation cell as one space.
                    if graphemes.peek() == Some(&" ") {
                        graphemes.next();
                    }
                    cell_index += 2;
                } else {
                    cell_index += 1;
                }
            }
        }
    }
}

fn fmt_style(s: &Style) -> String {
    let mut parts = Vec::new();
    if let Some(fg) = s.fg {
        parts.push(format!("fg={fg:?}"));
    }
    if let Some(bg) = s.bg {
        parts.push(format!("bg={bg:?}"));
    }
    let mut attrs = Vec::new();
    if s.bold {
        attrs.push("bold");
    }
    if s.dim {
        attrs.push("dim");
    }
    if s.italic {
        attrs.push("italic");
    }
    if s.underline {
        attrs.push("underline");
    }
    if s.crossedout {
        attrs.push("crossedout");
    }
    if s.reverse {
        attrs.push("reverse");
    }
    if !attrs.is_empty() {
        parts.push(format!("attrs={}", attrs.join("|")));
    }
    parts.join(" ")
}

// ── Parsing ───────────────────────────────────────────────────────

fn parse_dim_header(styles_text: &str) -> Option<(u16, u16)> {
    let first = styles_text.lines().next()?;
    let rest = first.strip_prefix("dim:")?.trim();
    let mut parts = rest.split_whitespace();
    let w: u16 = parts.next()?.parse().ok()?;
    let h: u16 = parts.next()?.parse().ok()?;
    Some((w, h))
}

/// Cell count of a serialized row. Each grapheme is one stored cell and a wide
/// grapheme is followed by an explicit space for its continuation. `text()` can
/// remove a final continuation space, so account for a trailing wide grapheme.
fn row_visual_width(row: &str) -> u16 {
    let graphemes: Vec<&str> = cell_width::graphemes(row).collect();
    let mut cells = graphemes.len().min(u16::MAX as usize) as u16;
    if graphemes
        .last()
        .is_some_and(|last| cell_width::text_width(last) == 2)
    {
        cells = cells.saturating_add(1);
    }
    cells
}

/// Reconstruct one cell-aligned serialized row. Grapheme sequences stay intact,
/// wide graphemes own a following continuation space, and short rows are padded.
fn parse_row(line: &str, width: u16) -> String {
    let mut graphemes = cell_width::graphemes(line).peekable();
    let mut out = String::with_capacity(line.len().max(width as usize));
    let mut cells: u16 = 0;
    while let Some(grapheme) = graphemes.next() {
        if cells >= width {
            break;
        }
        let grapheme_width = cell_width::text_width(grapheme);
        out.push_str(grapheme);
        cells += 1;
        if grapheme_width == 2 && cells < width {
            out.push(' ');
            cells += 1;
            if graphemes.peek() == Some(&" ") {
                graphemes.next();
            }
        }
    }
    while cells < width {
        out.push(' ');
        cells += 1;
    }
    out
}

fn parse_spans(styles_text: &str) -> Vec<(u16, u16, u16, Style)> {
    let mut out = Vec::new();
    for line in styles_text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("dim:") {
            continue;
        }
        // Numeric fields are right-aligned with padding, so use
        // `split_whitespace` (collapses runs) for the three numbers,
        // then recover the tail from the original string by scanning
        // past the third token.
        let mut nums = trimmed.split_whitespace();
        let Some(row) = nums.next().and_then(|s| s.parse::<u16>().ok()) else {
            continue;
        };
        let Some(col) = nums.next().and_then(|s| s.parse::<u16>().ok()) else {
            continue;
        };
        let Some(len_tok) = nums.next() else { continue };
        let Ok(len) = len_tok.parse::<u16>() else {
            continue;
        };
        // `len_tok` is a slice of `trimmed` - derive the tail offset
        // from pointer arithmetic via `str::find` on the same source.
        let tail_start = nums
            .next()
            .map(|first| first.as_ptr() as usize - trimmed.as_ptr() as usize)
            .unwrap_or(trimmed.len());
        let tail = trimmed.get(tail_start..).unwrap_or("").trim();
        let mut style = Style::default();
        apply_kv_string(&mut style, tail);
        out.push((row, col, len, style));
    }
    out
}

fn apply_kv_string(style: &mut Style, s: &str) {
    // Format from `fmt_style`: `fg=… bg=… attrs=…`, each optional.
    // Values are `Debug`-formatted, so `Rgb { r: 60, g: 20, b: 20 }`
    // contains spaces - split on known key prefixes, not whitespace.
    const KEYS: &[&str] = &["fg=", "bg=", "attrs="];
    let mut cursor = 0;
    while cursor < s.len() {
        let rest = &s[cursor..];
        let Some(off) = KEYS.iter().filter_map(|k| rest.find(k)).min() else {
            break;
        };
        let key_start = cursor + off;
        let Some(eq_rel) = s[key_start..].find('=') else {
            break;
        };
        let eq = key_start + eq_rel;
        let key = &s[key_start..eq];
        let value_start = eq + 1;
        let value_end = KEYS
            .iter()
            .filter_map(|k| {
                s[value_start..]
                    .find(&format!(" {k}"))
                    .map(|p| value_start + p)
            })
            .min()
            .unwrap_or(s.len());
        let value = s[value_start..value_end].trim();
        match key {
            "fg" => style.fg = parse_color(value),
            "bg" => style.bg = parse_color(value),
            "attrs" => apply_attrs(style, value),
            _ => {}
        }
        cursor = value_end;
    }
}

fn apply_attrs(style: &mut Style, value: &str) {
    for attr in value.split('|') {
        match attr.trim() {
            "bold" => style.bold = true,
            "dim" => style.dim = true,
            "italic" => style.italic = true,
            "underline" => style.underline = true,
            "crossedout" => style.crossedout = true,
            "reverse" => style.reverse = true,
            _ => {}
        }
    }
}

fn parse_color(s: &str) -> Option<Color> {
    if let Some(rest) = s.strip_prefix("Rgb { ") {
        let inner = rest.trim_end_matches(" }");
        let mut r = None;
        let mut g = None;
        let mut b = None;
        for kv in inner.split(',') {
            let kv = kv.trim();
            if let Some(v) = kv.strip_prefix("r:") {
                r = v.trim().parse().ok();
            } else if let Some(v) = kv.strip_prefix("g:") {
                g = v.trim().parse().ok();
            } else if let Some(v) = kv.strip_prefix("b:") {
                b = v.trim().parse().ok();
            }
        }
        return match (r, g, b) {
            (Some(r), Some(g), Some(b)) => Some(Color::Rgb { r, g, b }),
            _ => None,
        };
    }
    if let Some(rest) = s.strip_prefix("AnsiValue(") {
        let n: u8 = rest.trim_end_matches(')').parse().ok()?;
        return Some(Color::AnsiValue(n));
    }
    Some(match s {
        "Reset" => Color::Reset,
        "Black" => Color::Black,
        "DarkGrey" | "DarkGray" => Color::DarkGrey,
        "Red" => Color::Red,
        "DarkRed" => Color::DarkRed,
        "Green" => Color::Green,
        "DarkGreen" => Color::DarkGreen,
        "Yellow" => Color::Yellow,
        "DarkYellow" => Color::DarkYellow,
        "Blue" => Color::Blue,
        "DarkBlue" => Color::DarkBlue,
        "Magenta" => Color::Magenta,
        "DarkMagenta" => Color::DarkMagenta,
        "Cyan" => Color::Cyan,
        "DarkCyan" => Color::DarkCyan,
        "White" => Color::White,
        "Grey" | "Gray" => Color::Grey,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::Grid;

    fn snap_grid(setup: impl FnOnce(&mut Grid)) -> SnapshotFrame {
        let mut g = Grid::new(16, 3);
        setup(&mut g);
        SnapshotFrame::from_grid(&g)
    }

    #[test]
    fn roundtrip_ascii_preserves_text_and_styles() {
        let frame = snap_grid(|g| {
            g.put_str(
                0,
                0,
                "hello",
                Style {
                    bold: true,
                    ..Style::default()
                },
            );
        });
        let parsed = SnapshotFrame::parse(&frame.text(), &frame.styles_text());
        assert_eq!(parsed.width, frame.width);
        assert_eq!(parsed.height, frame.height);
        assert_eq!(parsed.rows, frame.rows);
        assert_eq!(parsed.styles, frame.styles);
    }

    #[test]
    fn roundtrip_with_trailing_wide_char() {
        // Row ends on a wide glyph - text() removes the continuation
        // slot; the `dim:` header recovers the width.
        let frame = snap_grid(|g| {
            g.put_str(0, 0, "你好", Style::default());
        });
        let parsed = SnapshotFrame::parse(&frame.text(), &frame.styles_text());
        assert_eq!(parsed.width, 16);
        assert_eq!(parsed.rows[0].chars().count(), 16);
        assert_eq!(parsed.rows[0].chars().next(), Some('你'));
    }

    #[test]
    fn text_keeps_spaces_inside_trailing_graphemes() {
        let frame = snap_grid(|g| {
            g.put_str(0, 0, "\u{600} ", Style::default());
        });

        assert_eq!(frame.text().lines().next(), Some("\u{600} "));
        let parsed = SnapshotFrame::parse(&frame.text(), &frame.styles_text());
        assert_eq!(parsed.rows, frame.rows);
    }

    #[test]
    fn blit_into_replays_wide_chars_correctly() {
        // Snapshot a wide-char source, parse it back, blit into a
        // larger grid, and confirm the destination cells match: the
        // wide chars at the right offset, with `\0` continuation
        // slots so flush will advance by the visual width.
        let frame = snap_grid(|g| {
            g.put_str(0, 0, "你好", Style::default());
        });
        let parsed = SnapshotFrame::parse(&frame.text(), &frame.styles_text());
        let mut dest = Grid::new(20, 3);
        parsed.blit_into(&mut dest, 2, 1);
        assert_eq!(dest.cell(2, 1).symbol, '你');
        assert_eq!(dest.cell(3, 1).symbol, '\0');
        assert_eq!(dest.cell(4, 1).symbol, '好');
        assert_eq!(dest.cell(5, 1).symbol, '\0');
    }

    #[test]
    fn styles_text_includes_dim_header_for_empty_styles() {
        let frame = snap_grid(|_| {});
        let styles = frame.styles_text();
        assert_eq!(styles.lines().next(), Some("dim: 16 3"));
    }

    #[test]
    fn parse_recovers_rgb_and_attrs() {
        let frame = snap_grid(|g| {
            g.put_str(
                0,
                0,
                "x",
                Style {
                    fg: Some(Color::Rgb {
                        r: 60,
                        g: 20,
                        b: 20,
                    }),
                    bg: Some(Color::AnsiValue(208)),
                    bold: true,
                    dim: true,
                    reverse: true,
                    ..Style::default()
                },
            );
        });
        let parsed = SnapshotFrame::parse(&frame.text(), &frame.styles_text());
        let s = parsed.styles[0][0];
        assert_eq!(
            s.fg,
            Some(Color::Rgb {
                r: 60,
                g: 20,
                b: 20
            })
        );
        assert_eq!(s.bg, Some(Color::AnsiValue(208)));
        assert!(s.bold);
        assert!(s.dim);
        assert!(s.reverse);
    }
}
