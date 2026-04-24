use super::*;

pub(crate) fn render_markdown_inner<S: LayoutSink>(
    out: &mut S,
    content: &str,
    width: usize,
    indent: &str,
    dim: bool,
    bctx: Option<&crate::render::BoxContext>,
) -> u16 {
    let _perf = crate::perf::begin("render:markdown");
    let max_cols = if let Some(b) = bctx {
        b.inner_w
    } else {
        width.saturating_sub(indent.len() + 1)
    };
    let lines: Vec<&str> = content.lines().collect();
    let mut i = 0;
    let mut rows = 0u16;
    // Track the last non-blank source line for heading gap suppression.
    let mut last_content_line: Option<&str> = None;
    while i < lines.len() {
        if lines[i].trim_start().starts_with("```") {
            // Blank line before code blocks — skip when preceded by a
            // blank line (already provides the gap) or a heading (headings
            // never get a trailing gap).
            let prev_blank = i > 0 && lines[i - 1].trim().is_empty();
            let after_heading = last_content_line.is_some_and(|l| l.trim_start().starts_with('#'));
            if rows > 0 && !prev_blank && !after_heading {
                out.newline();
                rows += 1;
            }
            let lang = lines[i].trim_start().trim_start_matches('`').trim();
            i += 1;
            let code_start = i;
            while i < lines.len() && !lines[i].trim_start().starts_with("```") {
                i += 1;
            }
            let code_lines = &lines[code_start..i];
            if i < lines.len() {
                i += 1;
            }
            rows += render_code_block(out, code_lines, lang, width, dim, bctx, true);
            last_content_line = None;
        } else if lines[i].trim_start().starts_with('|') {
            let table_start = i;
            while i < lines.len() && lines[i].trim_start().starts_with('|') {
                i += 1;
            }
            rows +=
                render_markdown_table_from_lines(out, &lines[table_start..i], dim, bctx, indent);
            last_content_line = None;
        } else if is_horizontal_rule(lines[i]) {
            // Blank line before horizontal rule unless preceded by blank or heading.
            let prev_blank = i > 0 && lines[i - 1].trim().is_empty();
            let after_heading = last_content_line.is_some_and(|l| l.trim_start().starts_with('#'));
            if rows > 0 && !prev_blank && !after_heading {
                out.newline();
                rows += 1;
            }
            rows += render_horizontal_rule(out, bctx, indent);
            // Blank line after horizontal rule unless followed by blank or heading.
            let mut next_i = i + 1;
            while next_i < lines.len() && lines[next_i].trim().is_empty() {
                next_i += 1;
            }
            let next_is_heading =
                next_i < lines.len() && lines[next_i].trim_start().starts_with('#');
            if next_i < lines.len() && !next_is_heading && !lines[next_i].trim().is_empty() {
                out.newline();
                rows += 1;
            }
            last_content_line = None;
            i += 1;
        } else {
            if lines[i].trim().is_empty() {
                // Skip blank lines after headings — headings never have
                // a trailing gap.
                let after_heading =
                    last_content_line.is_some_and(|l| l.trim_start().starts_with('#'));
                if after_heading {
                    i += 1;
                    continue;
                }
                // Skip blank lines before list items.
                let mut next_i = i + 1;
                while next_i < lines.len() && lines[next_i].trim().is_empty() {
                    next_i += 1;
                }
                if next_i < lines.len() && is_list_item(lines[next_i]) {
                    i += 1;
                    continue;
                }
            } else {
                last_content_line = Some(lines[i]);
            }
            let trimmed = lines[i].trim_start();
            {
                use crate::render::highlight::{
                    emit_inline_spans, inline_spans_width, parse_inline_spans, wrap_inline_spans,
                    InlineSpan, InlineStyle,
                };
                let leading_ws = &lines[i][..lines[i].len() - trimmed.len()];
                let mut line_spans: Vec<InlineSpan> = Vec::new();

                if trimmed.starts_with('#') {
                    line_spans.push(InlineSpan {
                        text: trimmed.to_string(),
                        style: InlineStyle {
                            bold: true,
                            dim,
                            fg: Some(theme::HEADING.into()),
                            ..Default::default()
                        },
                    });
                } else if trimmed.starts_with('>') {
                    line_spans.push(InlineSpan {
                        text: trimmed.to_string(),
                        style: InlineStyle {
                            dim: true,
                            italic: true,
                            ..Default::default()
                        },
                    });
                } else {
                    let (prefix, body) = split_list_prefix(trimmed);
                    if !leading_ws.is_empty() {
                        line_spans.push(InlineSpan {
                            text: leading_ws.to_string(),
                            style: InlineStyle {
                                dim,
                                ..Default::default()
                            },
                        });
                    }
                    if !prefix.is_empty() {
                        line_spans.push(InlineSpan {
                            text: prefix.to_string(),
                            style: InlineStyle {
                                dim: true,
                                ..Default::default()
                            },
                        });
                    }
                    line_spans.extend(parse_inline_spans(body, dim));
                }

                let wrapped = wrap_inline_spans(&line_spans, max_cols);
                if wrapped.len() > 1 {
                    out.mark_wrapped();
                }
                for (si, row_spans) in wrapped.iter().enumerate() {
                    if si == 0 {
                        out.set_source_text(lines[i]);
                    } else {
                        out.mark_soft_wrap_continuation();
                    }
                    if let Some(b) = bctx {
                        b.print_left(out);
                        emit_inline_spans(out, row_spans);
                        b.print_right(out, inline_spans_width(row_spans));
                    } else {
                        out.print(indent);
                        emit_inline_spans(out, row_spans);
                    }
                    out.newline();
                }
                rows += wrapped.len() as u16;
            }
            i += 1;
        }
    }
    rows
}

/// Split a list-item prefix (`- `, `* `, `1. `, etc.) from the line content.
/// Returns (prefix, rest). If not a list item, prefix is empty.
fn split_list_prefix(line: &str) -> (&str, &str) {
    // Ordered: "1. ", "12. ", etc.
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i > 0 && i < bytes.len() && bytes[i] == b'.' {
        let end = i + 1;
        if end < bytes.len() && bytes[end] == b' ' {
            return (&line[..end + 1], &line[end + 1..]);
        }
        return (&line[..end], &line[end..]);
    }
    // Unordered: "- " or "* "
    if line.starts_with("- ") || line.starts_with("* ") {
        return (&line[..2], &line[2..]);
    }
    ("", line)
}

/// Check if a line is a list item (ordered or unordered).
fn is_list_item(line: &str) -> bool {
    let trimmed = line.trim_start();
    // Unordered: "- " or "* "
    if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
        return true;
    }
    // Ordered: digits followed by "."
    let bytes = trimmed.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i > 0 && i < bytes.len() && bytes[i] == b'.' {
        return true;
    }
    false
}

/// Check if a line is a horizontal rule (---, ***, ___, etc.).
pub(super) fn is_horizontal_rule(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    // Count non-space characters - must be at least 3
    let non_space_count = trimmed.chars().filter(|&c| !c.is_whitespace()).count();
    if non_space_count < 3 {
        return false;
    }
    // Check if all non-space characters are the same and one of -, *, or _
    let mut first_char: Option<char> = None;
    for ch in trimmed.chars() {
        if ch == ' ' || ch == '\t' {
            continue;
        }
        if first_char.is_none() {
            first_char = Some(ch);
        } else {
            // All non-space chars must be the same
            if first_char != Some(ch) {
                return false;
            }
        }
        // Must be one of the valid HR characters
        if !matches!(ch, '-' | '*' | '_') {
            return false;
        }
    }
    first_char.is_some()
}

/// Render a horizontal rule line with dim styling (matching list markers).
/// Replaces the HR characters (---, ***, ___) with box-drawing chars (─) but
/// only renders 3 of them to match the visual weight of list markers.
fn render_horizontal_rule<S: LayoutSink>(
    out: &mut S,
    bctx: Option<&crate::render::BoxContext>,
    indent: &str,
) -> u16 {
    // Use box-drawing character, render only 3 chars (like list markers)
    let hr = "─".repeat(3);

    if let Some(b) = bctx {
        b.print_left(out);
    } else if !indent.is_empty() {
        out.print(indent);
    }

    out.push_dim();
    out.print_with_meta(
        &hr,
        crate::render::display::SpanMeta {
            selectable: true,
            copy_as: Some("---".into()),
        },
    );
    out.pop_style();

    if let Some(b) = bctx {
        b.print_right(out, 3);
    }

    out.newline();
    1
}

/// Parse pipe-delimited table lines into rows, then render.
fn render_markdown_table_from_lines<S: LayoutSink>(
    out: &mut S,
    lines: &[&str],
    dim: bool,
    bctx: Option<&crate::render::BoxContext>,
    indent: &str,
) -> u16 {
    let mut table_rows: Vec<Vec<String>> = Vec::new();
    for line in lines {
        if crate::render::is_table_separator(line) {
            continue;
        }
        let trimmed = line.trim().trim_start_matches('|').trim_end_matches('|');
        let cells: Vec<String> = trimmed.split('|').map(|c| c.trim().to_string()).collect();
        table_rows.push(cells);
    }
    // Attach the joined raw markdown source to the first rendered row.
    // Selections that include row 0 (the top border) reconstruct the
    // table verbatim via `copy_range`'s `source_text` shortcut; rows
    // 1..N are marked as soft-wrap continuations so they're skipped
    // once the source has been emitted. Sub-table selections that
    // exclude row 0 fall back to the rendered box-drawing chars.
    let raw_source = lines.join("\n");
    let mut tagged = SourceTextOnFirstRow::new(out, raw_source);
    render_markdown_table(&mut tagged, &table_rows, dim, bctx, indent)
}

/// `LayoutSink` adapter that injects `set_source_text` before the
/// first `newline()` and marks every subsequent `newline()` as a
/// soft-wrap continuation. Used to attach a raw markdown source string
/// to the first visual row of a multi-row rendered construct (tables)
/// where per-row source mapping is impractical because the renderer
/// builds its own visual layout.
struct SourceTextOnFirstRow<'a, S: LayoutSink> {
    inner: &'a mut S,
    pending_source: Option<String>,
}

impl<'a, S: LayoutSink> SourceTextOnFirstRow<'a, S> {
    fn new(inner: &'a mut S, source: String) -> Self {
        Self {
            inner,
            pending_source: Some(source),
        }
    }
}

impl<S: LayoutSink> LayoutSink for SourceTextOnFirstRow<'_, S> {
    fn print(&mut self, text: &str) {
        self.inner.print(text);
    }
    fn newline(&mut self) {
        if let Some(src) = self.pending_source.take() {
            self.inner.set_source_text(&src);
        } else {
            self.inner.mark_soft_wrap_continuation();
        }
        self.inner.newline();
    }
    fn mark_wrapped(&mut self) {
        self.inner.mark_wrapped();
    }
    fn fill_line_bg(&mut self, bg: crate::render::display::ColorValue, right_margin: u16) {
        self.inner.fill_line_bg(bg, right_margin);
    }
    fn snapshot_style(&self) -> crate::render::display::SpanStyle {
        self.inner.snapshot_style()
    }
    fn apply_style(&mut self, style: crate::render::display::SpanStyle) {
        self.inner.apply_style(style);
    }
    fn push_style(&mut self, style: crate::render::display::SpanStyle) {
        self.inner.push_style(style);
    }
    fn pop_style(&mut self) {
        self.inner.pop_style();
    }
    fn set_gutter_bg(&mut self, bg: crate::render::display::ColorValue) {
        self.inner.set_gutter_bg(bg);
    }
    fn mark_soft_wrap_continuation(&mut self) {
        self.inner.mark_soft_wrap_continuation();
    }
    fn set_source_text(&mut self, text: &str) {
        self.inner.set_source_text(text);
    }
    fn print_with_meta(&mut self, text: &str, meta: crate::render::display::SpanMeta) {
        self.inner.print_with_meta(text, meta);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::layout_out::SpanCollector;

    #[test]
    fn rendered_table_attaches_raw_source_to_first_row() {
        // Whole-table selection (any range that includes row 0) should
        // round-trip back to the raw `|col|val|` markdown — not the
        // rendered ┃ box-drawing chars. Achieved via the
        // `SourceTextOnFirstRow` wrapper: row 0 carries the joined
        // input lines as `source_text`; subsequent rows are marked as
        // soft-wrap continuations.
        let mut sink = SpanCollector::new(80);
        let md = "| col | val |\n| --- | --- |\n| a   | 1   |\n";
        render_markdown_inner(&mut sink, md, 80, "", false, None);
        let block = sink.finish();
        assert!(block.lines.len() >= 2, "table should render multiple rows");
        // Row 0 carries the full raw markdown table.
        assert_eq!(
            block.lines[0].source_text.as_deref(),
            Some("| col | val |\n| --- | --- |\n| a   | 1   |")
        );
        // Subsequent table rows are soft-wrap continuations; they are
        // skipped by `copy_range` once row 0's source has been emitted.
        for (i, line) in block.lines.iter().enumerate().skip(1) {
            assert!(
                line.soft_wrapped,
                "row {i} should be marked soft-wrap continuation"
            );
            assert!(
                line.source_text.is_none(),
                "row {i} should not carry its own source_text"
            );
        }
    }
}
