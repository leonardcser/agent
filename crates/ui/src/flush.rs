use crate::grid::{CellUpdate, Style};
use crossterm::style::{
    Attribute, Color, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use crossterm::{cursor, QueueableCommand};
use std::io::Write;

pub fn flush_diff<'a, W: Write>(
    w: &mut W,
    updates: impl Iterator<Item = CellUpdate<'a>>,
) -> std::io::Result<()> {
    let mut current = Style::default();
    let mut cursor_x: u16 = u16::MAX;
    let mut cursor_y: u16 = u16::MAX;

    for update in updates {
        if update.y != cursor_y || update.x != cursor_x {
            w.queue(cursor::MoveTo(update.x, update.y))?;
        }
        if update.cell.style != current {
            emit_style_diff(w, &current, &update.cell.style)?;
            current = update.cell.style;
        }
        let mut buf = [0u8; 4];
        let s = update.cell.symbol.encode_utf8(&mut buf);
        w.write_all(s.as_bytes())?;
        cursor_x = update.x + 1;
        cursor_y = update.y;
    }

    if cursor_x != u16::MAX {
        w.queue(SetAttribute(Attribute::Reset))?;
        w.queue(ResetColor)?;
    }

    Ok(())
}

fn emit_style_diff<W: Write>(w: &mut W, from: &Style, to: &Style) -> std::io::Result<()> {
    let need_unbold = from.bold && !to.bold;
    let need_undim = from.dim && !to.dim;
    let need_unitalic = from.italic && !to.italic;
    let need_uncrossed = from.crossedout && !to.crossedout;
    let need_ununderline = from.underline && !to.underline;

    let unsets = need_unbold as u8
        + need_undim as u8
        + need_unitalic as u8
        + need_uncrossed as u8
        + need_ununderline as u8;
    let intensity_conflict = (need_unbold && to.dim) || (need_undim && to.bold);

    if unsets >= 2 || intensity_conflict {
        w.queue(SetAttribute(Attribute::Reset))?;
        w.queue(ResetColor)?;

        if let Some(fg) = to.fg {
            w.queue(SetForegroundColor(fg))?;
        }
        if let Some(bg) = to.bg {
            w.queue(SetBackgroundColor(bg))?;
        }
        if to.bold {
            w.queue(SetAttribute(Attribute::Bold))?;
        }
        if to.dim {
            w.queue(SetAttribute(Attribute::Dim))?;
        }
        if to.italic {
            w.queue(SetAttribute(Attribute::Italic))?;
        }
        if to.crossedout {
            w.queue(SetAttribute(Attribute::CrossedOut))?;
        }
        if to.underline {
            w.queue(SetAttribute(Attribute::Underlined))?;
        }
        return Ok(());
    }

    if need_unbold || need_undim {
        w.queue(SetAttribute(Attribute::NormalIntensity))?;
        if need_unbold && to.dim {
            w.queue(SetAttribute(Attribute::Dim))?;
        }
        if need_undim && to.bold {
            w.queue(SetAttribute(Attribute::Bold))?;
        }
    }
    if need_unitalic {
        w.queue(SetAttribute(Attribute::NoItalic))?;
    }
    if need_uncrossed {
        w.queue(SetAttribute(Attribute::NotCrossedOut))?;
    }
    if need_ununderline {
        w.queue(SetAttribute(Attribute::NoUnderline))?;
    }

    if !from.bold && to.bold {
        w.queue(SetAttribute(Attribute::Bold))?;
    }
    if !from.dim && to.dim {
        w.queue(SetAttribute(Attribute::Dim))?;
    }
    if !from.italic && to.italic {
        w.queue(SetAttribute(Attribute::Italic))?;
    }
    if !from.crossedout && to.crossedout {
        w.queue(SetAttribute(Attribute::CrossedOut))?;
    }
    if !from.underline && to.underline {
        w.queue(SetAttribute(Attribute::Underlined))?;
    }

    if from.fg != to.fg {
        if let Some(fg) = to.fg {
            w.queue(SetForegroundColor(fg))?;
        } else {
            w.queue(SetForegroundColor(Color::Reset))?;
        }
    }
    if from.bg != to.bg {
        if let Some(bg) = to.bg {
            w.queue(SetBackgroundColor(bg))?;
        } else {
            w.queue(SetBackgroundColor(Color::Reset))?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::Grid;
    use crossterm::style::Color;

    #[test]
    fn flush_empty_diff_produces_no_output() {
        let a = Grid::new(5, 3);
        let b = Grid::new(5, 3);
        let mut out = Vec::new();
        flush_diff(&mut out, a.diff(&b)).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn flush_single_cell_produces_output() {
        let prev = Grid::new(5, 3);
        let mut curr = Grid::new(5, 3);
        curr.set(2, 1, 'X', Style::default());
        let mut out = Vec::new();
        flush_diff(&mut out, curr.diff(&prev)).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains('X'));
    }

    #[test]
    fn flush_styled_cell_emits_sgr() {
        let prev = Grid::new(5, 1);
        let mut curr = Grid::new(5, 1);
        curr.set(0, 0, 'A', Style::fg(Color::Red));
        let mut out = Vec::new();
        flush_diff(&mut out, curr.diff(&prev)).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains('A'));
        assert!(s.contains("\x1b["));
    }

    #[test]
    fn flush_resets_style_at_end() {
        let prev = Grid::new(3, 1);
        let mut curr = Grid::new(3, 1);
        curr.set(0, 0, 'A', Style::bold());
        let mut out = Vec::new();
        flush_diff(&mut out, curr.diff(&prev)).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.ends_with("\x1b[0m"));
    }
}
