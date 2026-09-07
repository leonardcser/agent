use smelt_core::buffer::SpanMeta;
use smelt_core::content::builder::{display_width, LineBuilder};

pub(super) fn apply_temp_decoration(
    out: &mut LineBuilder,
    buf: &smelt_core::buffer::Buffer,
    row: usize,
    copy_fill_bg: bool,
) {
    let dec = buf.decoration_at(row).clone();
    if let Some(source) = dec.source_text.as_deref() {
        out.set_source_text(source);
    }
    if let Some(source) = dec.external_source_text.as_deref() {
        out.set_external_source_text(source);
    }
    if let Some(source_line) = dec.source_line {
        out.set_source_line(source_line);
    }
    if dec.soft_wrapped {
        out.mark_soft_wrap_continuation();
    } else if dec.copy_continuation {
        out.mark_copy_continuation();
    }
    if dec.copy_excluded {
        out.exclude_from_copy();
    }
    if dec.cell_selectable {
        out.mark_cell_selectable();
    }
    if dec.block_selectable {
        out.mark_block_selectable();
    }
    if copy_fill_bg {
        if let Some(bg) = dec.fill_bg {
            out.fill_line_bg(bg);
        }
    }
}

pub(super) fn emit_buffer_row_clipped(
    buf: &smelt_core::buffer::Buffer,
    row: u16,
    max_cols: u16,
    out: &mut LineBuilder,
    style_overlay: Option<(bool, bool)>,
) -> u16 {
    let mut highlights = Vec::new();
    emit_buffer_row_clipped_with_scratch(buf, row, max_cols, out, style_overlay, &mut highlights)
}

pub(super) fn emit_buffer_row_clipped_with_scratch(
    buf: &smelt_core::buffer::Buffer,
    row: u16,
    max_cols: u16,
    out: &mut LineBuilder,
    style_overlay: Option<(bool, bool)>,
    highlights: &mut Vec<smelt_core::buffer::Span>,
) -> u16 {
    let text = buf.get_line(row as usize).unwrap_or("");
    highlights.clear();
    buf.highlights_at_into(row as usize, highlights);
    highlights.sort_by_key(|h| h.col_start);

    let text_width = display_width_u16(text);
    let start_width = out.current_line_width();
    let max_line_cols = start_width.saturating_add(max_cols);
    let mut col_idx: u16 = 0;

    for h in highlights.iter() {
        if h.col_end <= col_idx {
            continue;
        }
        if h.col_start > col_idx {
            let end = h.col_start.min(text_width);
            let plain = smelt_buffer::text::slice_cells(text, col_idx as usize, end as usize);
            let style = style_overlay.map(|overlay| overlay_style(None, overlay));
            if !emit_to_line_width(out, plain, style, SpanMeta::default(), max_line_cols) {
                return out.current_line_width().saturating_sub(start_width);
            }
            col_idx = end;
        }
        let end = h.col_end.min(text_width);
        if end <= col_idx {
            continue;
        }
        let segment = smelt_buffer::text::slice_cells(text, col_idx as usize, end as usize);
        let style = overlay_style(
            Some(out.theme().resolve(h.hl)),
            style_overlay.unwrap_or_default(),
        );
        if !emit_to_line_width(out, segment, Some(style), h.meta.clone(), max_line_cols) {
            return out.current_line_width().saturating_sub(start_width);
        }
        col_idx = end;
    }
    if col_idx < text_width || (col_idx == 0 && text_width == 0 && !text.is_empty()) {
        let tail = if text_width == 0 {
            text
        } else {
            smelt_buffer::text::slice_cells(text, col_idx as usize, text_width as usize)
        };
        let style = style_overlay.map(|overlay| overlay_style(None, overlay));
        emit_to_line_width(out, tail, style, SpanMeta::default(), max_line_cols);
    }
    out.current_line_width().saturating_sub(start_width)
}

fn display_width_u16(text: &str) -> u16 {
    display_width(text).min(u16::MAX as usize) as u16
}

fn overlay_style(
    base: Option<smelt_core::style::Style>,
    overlay: (bool, bool),
) -> smelt_core::style::Style {
    let mut style = base.unwrap_or_default();
    if overlay.0 {
        style.dim = true;
    }
    if overlay.1 {
        style.italic = true;
    }
    style
}

fn emit_to_line_width(
    out: &mut LineBuilder,
    segment: &str,
    style: Option<smelt_core::style::Style>,
    meta: SpanMeta,
    max_line_cols: u16,
) -> bool {
    let keep = out.fitting_prefix_len(segment, max_line_cols);
    if keep == 0 {
        return segment.is_empty();
    }
    let text = smelt_buffer::text::slice(segment, 0..keep);
    if let Some(style) = style {
        out.append_resolved_span(text, style, meta);
    } else {
        out.print(text);
    }
    keep == segment.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::smelt_edit::{BufCreateOpts, BufId, Buffer, Theme};
    use smelt_core::buffer::LineDecoration;

    #[test]
    fn temp_decoration_preserves_both_copy_sources() {
        let mut source = Buffer::new(BufId(1), BufCreateOpts::default());
        source.set_decoration(
            0,
            LineDecoration {
                source_text: Some("line source".into()),
                external_source_text: Some("external source".into()),
                ..LineDecoration::default()
            },
        );
        let mut destination = Buffer::new(BufId(2), BufCreateOpts::default());
        let theme = Theme::default();
        {
            let mut out = LineBuilder::new(&mut destination, &theme, 80);
            apply_temp_decoration(&mut out, &source, 0, false);
            out.print("rendered");
            out.newline();
            out.finish();
        }

        let decoration = destination.decoration_at(0);
        assert_eq!(decoration.source_text.as_deref(), Some("line source"));
        assert_eq!(
            decoration.external_source_text.as_deref(),
            Some("external source")
        );
    }
}
