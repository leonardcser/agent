use crate::buffer::{Buffer, SpanStyle};
use crate::layout::{Border, Rect};
use crossterm::style::{
    Attribute, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use crossterm::{cursor, terminal, QueueableCommand};
use std::io::Write;

pub struct FloatFrame {
    pub rect: Rect,
    pub border: Border,
    pub title: Option<String>,
    pub title_style: SpanStyle,
    pub scroll_offset: usize,
}

pub fn render_float<W: Write>(
    w: &mut W,
    buf: &Buffer,
    frame: &FloatFrame,
) -> std::io::Result<()> {
    let r = frame.rect;
    if r.width < 3 || r.height < 2 {
        return Ok(());
    }

    let border_chars = match frame.border {
        Border::None => None,
        Border::Single => Some(("─", "│", "┌", "┐", "└", "┘")),
        Border::Double => Some(("═", "║", "╔", "╗", "╚", "╝")),
        Border::Rounded => Some(("─", "│", "╭", "╮", "╰", "╯")),
    };

    let (content_top, content_left, content_w, content_h) = if border_chars.is_some() {
        (
            r.top + 1,
            r.left + 1,
            r.width.saturating_sub(2) as usize,
            r.height.saturating_sub(2) as usize,
        )
    } else {
        (r.top, r.left, r.width as usize, r.height as usize)
    };

    if let Some((h, v, tl, tr, bl, br)) = border_chars {
        // Top border with title
        w.queue(cursor::MoveTo(r.left, r.top))?;
        apply_style(w, &frame.title_style)?;
        w.queue(Print(tl))?;

        if let Some(ref title) = frame.title {
            let max_title = (r.width as usize).saturating_sub(4);
            let truncated: String = title.chars().take(max_title).collect();
            let title_len = truncated.chars().count();
            w.queue(Print(h))?;
            w.queue(Print(&truncated))?;
            w.queue(Print(h))?;
            let remaining = (r.width as usize).saturating_sub(title_len + 3);
            for _ in 0..remaining {
                w.queue(Print(h))?;
            }
        } else {
            for _ in 0..r.width.saturating_sub(2) {
                w.queue(Print(h))?;
            }
        }
        w.queue(Print(tr))?;
        reset_style(w)?;

        // Content rows
        let lines = buf.lines();
        for row_idx in 0..content_h {
            let screen_row = content_top + row_idx as u16;
            w.queue(cursor::MoveTo(r.left, screen_row))?;
            apply_style(w, &frame.title_style)?;
            w.queue(Print(v))?;
            reset_style(w)?;

            let line_idx = frame.scroll_offset + row_idx;
            if line_idx < lines.len() {
                let line = &lines[line_idx];
                render_styled_line(w, line, buf.highlights_at(line_idx), content_w)?;
            } else {
                // Empty line — fill with spaces
                let fill: String = " ".repeat(content_w);
                w.queue(Print(fill))?;
            }

            apply_style(w, &frame.title_style)?;
            w.queue(Print(v))?;
            reset_style(w)?;
        }

        // Bottom border
        let bottom_row = r.top + r.height - 1;
        w.queue(cursor::MoveTo(r.left, bottom_row))?;
        apply_style(w, &frame.title_style)?;
        w.queue(Print(bl))?;
        for _ in 0..r.width.saturating_sub(2) {
            w.queue(Print(h))?;
        }
        w.queue(Print(br))?;
        reset_style(w)?;
    } else {
        // No border — render lines directly
        let lines = buf.lines();
        for row_idx in 0..content_h {
            let screen_row = content_top + row_idx as u16;
            w.queue(cursor::MoveTo(content_left, screen_row))?;
            let line_idx = frame.scroll_offset + row_idx;
            if line_idx < lines.len() {
                let line = &lines[line_idx];
                render_styled_line(w, line, buf.highlights_at(line_idx), content_w)?;
            } else {
                w.queue(terminal::Clear(terminal::ClearType::UntilNewLine))?;
            }
        }
    }

    Ok(())
}

fn render_styled_line<W: Write>(
    w: &mut W,
    line: &str,
    spans: &[crate::buffer::Span],
    max_width: usize,
) -> std::io::Result<()> {
    if spans.is_empty() {
        let truncated: String = line.chars().take(max_width).collect();
        let printed = truncated.chars().count();
        w.queue(Print(truncated))?;
        if printed < max_width {
            let pad: String = " ".repeat(max_width - printed);
            w.queue(Print(pad))?;
        }
        return Ok(());
    }

    let chars: Vec<char> = line.chars().collect();
    let mut col = 0usize;
    while col < max_width && col < chars.len() {
        let active = spans
            .iter()
            .find(|s| col >= s.col_start as usize && col < s.col_end as usize);
        if let Some(span) = active {
            apply_style(w, &span.style)?;
            let end = (span.col_end as usize).min(max_width).min(chars.len());
            let chunk: String = chars[col..end].iter().collect();
            w.queue(Print(chunk))?;
            reset_style(w)?;
            col = end;
        } else {
            w.queue(Print(chars[col]))?;
            col += 1;
        }
    }
    if col < max_width {
        let pad: String = " ".repeat(max_width - col);
        w.queue(Print(pad))?;
    }
    Ok(())
}

fn apply_style<W: Write>(w: &mut W, style: &SpanStyle) -> std::io::Result<()> {
    if let Some(fg) = style.fg {
        w.queue(SetForegroundColor(fg))?;
    }
    if let Some(bg) = style.bg {
        w.queue(SetBackgroundColor(bg))?;
    }
    if style.bold {
        w.queue(SetAttribute(Attribute::Bold))?;
    }
    if style.dim {
        w.queue(SetAttribute(Attribute::Dim))?;
    }
    if style.italic {
        w.queue(SetAttribute(Attribute::Italic))?;
    }
    Ok(())
}

fn reset_style<W: Write>(w: &mut W) -> std::io::Result<()> {
    w.queue(ResetColor)?;
    w.queue(SetAttribute(Attribute::Reset))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::BufCreateOpts;
    use crate::BufId;
    use crossterm::style::Color;

    fn make_buf(lines: Vec<&str>) -> Buffer {
        let mut buf = Buffer::new(BufId(1), BufCreateOpts::default());
        buf.set_all_lines(lines.into_iter().map(String::from).collect());
        buf
    }

    #[test]
    fn render_bordered_float() {
        let buf = make_buf(vec!["hello", "world"]);
        let frame = FloatFrame {
            rect: Rect::new(0, 0, 20, 4),
            border: Border::Rounded,
            title: Some("test".into()),
            title_style: SpanStyle::default(),
            scroll_offset: 0,
        };
        let mut out = Vec::new();
        render_float(&mut out, &buf, &frame).unwrap();
        let rendered = String::from_utf8(out).unwrap();
        assert!(rendered.contains("test"));
    }

    #[test]
    fn render_with_scroll_offset() {
        let buf = make_buf(vec!["line0", "line1", "line2", "line3"]);
        let frame = FloatFrame {
            rect: Rect::new(0, 0, 20, 4),
            border: Border::Single,
            title: None,
            title_style: SpanStyle::default(),
            scroll_offset: 2,
        };
        let mut out = Vec::new();
        render_float(&mut out, &buf, &frame).unwrap();
        let rendered = String::from_utf8(out).unwrap();
        assert!(rendered.contains("line2"));
        assert!(!rendered.contains("line0"));
    }

    #[test]
    fn render_styled_spans() {
        let mut buf = make_buf(vec!["hello world"]);
        buf.add_highlight(0, 0, 5, SpanStyle::fg(Color::Red));
        let frame = FloatFrame {
            rect: Rect::new(0, 0, 20, 3),
            border: Border::Single,
            title: None,
            title_style: SpanStyle::default(),
            scroll_offset: 0,
        };
        let mut out = Vec::new();
        render_float(&mut out, &buf, &frame).unwrap();
        let rendered = String::from_utf8(out).unwrap();
        // Should contain the color escape for red
        assert!(rendered.contains("hello"));
    }
}
