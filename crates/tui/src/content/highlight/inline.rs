//! Inline markdown rendering: emphasis grammar (`*`, `_`, `` ` ``,
//! `~~`), inline-span flattening + word-wrap, and the markdown table
//! renderer that uses both.

use crate::content::display::{ColorRole, ColorValue, NamedColor};
use crate::content::layout_out::SpanCollector;
use crate::content::term_width;
use unicode_width::UnicodeWidthStr;

use super::util::{
    breakable_positions, can_open_emphasis, find_closing_run, find_code_close, find_strike_close,
    run_length, strip_markdown_markers,
};

pub(crate) fn render_markdown_table(
    out: &mut SpanCollector,
    rows: &[Vec<String>],
    dim: bool,
    bctx: Option<&super::super::BoxContext>,
    indent: &str,
) -> u16 {
    if rows.is_empty() {
        return 0;
    }

    let num_cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if num_cols == 0 {
        return 0;
    }

    // Calculate column widths based on visual (stripped) content.
    let max_table = if let Some(b) = bctx {
        b.inner_w.saturating_sub(1)
    } else {
        term_width().saturating_sub(2)
    };
    let mut col_widths = vec![0usize; num_cols];
    for row in rows {
        for (c, cell) in row.iter().enumerate() {
            let visual = strip_markdown_markers(cell).width();
            col_widths[c] = col_widths[c].max(visual);
        }
    }

    // Borders: "┃" + (" col ┃") * num_cols → 3 * num_cols + 1.
    let overhead = 3 * num_cols + 1;

    // Minimum column widths: the longest unwrappable segment per column.
    let mut min_widths = vec![0usize; num_cols];
    for row in rows {
        for (c, cell) in row.iter().enumerate() {
            min_widths[c] = min_widths[c].max(min_visual_width(cell));
        }
    }

    // Shrink columns by wrapping until the table fits, or we hit minimums.
    let total: usize = col_widths.iter().sum::<usize>() + overhead;
    if total > max_table {
        let avail = max_table.saturating_sub(overhead);
        let min_total: usize = min_widths.iter().sum();

        if min_total > avail {
            // Can't fit even at minimum widths — switch to stacked layout.
            return render_table_stacked(out, rows, dim);
        }

        // Shrink proportionally but clamp to min_widths.
        let content_total: usize = col_widths.iter().sum();
        if content_total > 0 {
            // First pass: proportional shrink.
            let mut new_widths: Vec<usize> = col_widths
                .iter()
                .zip(min_widths.iter())
                .map(|(&w, &min)| ((w * avail) / content_total).max(min))
                .collect();

            // Redistribute any excess from clamped columns.
            loop {
                let used: usize = new_widths.iter().sum();
                if used <= avail {
                    break;
                }
                let excess = used - avail;
                // Find columns that can still shrink.
                let shrinkable: Vec<usize> = (0..num_cols)
                    .filter(|&c| new_widths[c] > min_widths[c])
                    .collect();
                if shrinkable.is_empty() {
                    break;
                }
                let per_col = (excess / shrinkable.len()).max(1);
                for &c in &shrinkable {
                    let reduce = per_col.min(new_widths[c] - min_widths[c]);
                    new_widths[c] -= reduce;
                }
            }
            col_widths = new_widths;
        }
    }

    let mut total_rows = 0u16;

    let bar = |out: &mut SpanCollector, dim: bool| {
        out.set_fg(ColorValue::Role(ColorRole::Bar));
        if dim {
            out.set_dim();
        }
    };
    let reset = |out: &mut SpanCollector, _dim: bool| {
        out.reset_style();
    };

    let render_table_row =
        |out: &mut SpanCollector, row: &[String], widths: &[usize], dim: bool| -> u16 {
            let wrapped: Vec<Vec<String>> = row
                .iter()
                .enumerate()
                .map(|(c, cell)| {
                    let w = widths.get(c).copied().unwrap_or(0);
                    wrap_cell_words(out, cell, w)
                })
                .collect();
            let height = wrapped.iter().map(|w| w.len()).max().unwrap_or(1);

            for vline in 0..height {
                if let Some(b) = bctx {
                    b.print_left(out);
                } else if !indent.is_empty() {
                    out.print_gutter(indent);
                }
                bar(out, dim);
                out.print_gutter("┃");
                reset(out, dim);
                let mut line_cols = 1; // "┃"
                for (c, width) in widths.iter().enumerate() {
                    let text = wrapped
                        .get(c)
                        .and_then(|w| w.get(vline))
                        .map(|s| s.as_str())
                        .unwrap_or("");
                    let visual_len = strip_markdown_markers(text).width();
                    out.print_gutter(" ");
                    print_inline_styled(out, text, dim);
                    let pad = width.saturating_sub(visual_len);
                    if pad > 0 {
                        out.print_gutter(&" ".repeat(pad));
                    }
                    out.print_gutter(" ");
                    bar(out, dim);
                    out.print_gutter("┃");
                    reset(out, dim);
                    line_cols += width + 3; // " content pad ┃"
                }
                if let Some(b) = bctx {
                    b.print_right(out, line_cols);
                }
                out.newline();
            }
            height as u16
        };

    // left, horizontal, junction, right
    let render_border =
        |out: &mut SpanCollector, widths: &[usize], dim: bool, l: &str, j: &str, r: &str| -> u16 {
            if let Some(b) = bctx {
                b.print_left(out);
            } else if !indent.is_empty() {
                out.print_gutter(indent);
            }
            bar(out, dim);
            out.print_gutter(l);
            let mut line_cols = 1; // "l"
            for (c, width) in widths.iter().enumerate() {
                let seg = width + 2;
                out.print_gutter(&"━".repeat(seg));
                line_cols += seg;
                if c + 1 < widths.len() {
                    out.print_gutter(j);
                    line_cols += 1;
                }
            }
            out.print_gutter(r);
            line_cols += 1;
            reset(out, dim);
            if let Some(b) = bctx {
                b.print_right(out, line_cols);
            }
            out.newline();
            1
        };

    // Top border
    total_rows += render_border(out, &col_widths, dim, "┏", "┳", "┓");

    // Header
    if let Some(header) = rows.first() {
        total_rows += render_table_row(out, header, &col_widths, dim);
        total_rows += render_border(out, &col_widths, dim, "┣", "╋", "┫");
    }

    // Data rows
    for row in rows.iter().skip(1) {
        total_rows += render_table_row(out, row, &col_widths, dim);
    }

    // Bottom border
    total_rows += render_border(out, &col_widths, dim, "┗", "┻", "┛");

    total_rows
}

/// Stacked layout for tables too wide for the terminal.
/// Each data row becomes a block of "Header: value" lines, separated by blank lines.
fn render_table_stacked(out: &mut SpanCollector, rows: &[Vec<String>], dim: bool) -> u16 {
    let header = match rows.first() {
        Some(h) => h,
        None => return 0,
    };

    let label_width = header
        .iter()
        .map(|h| strip_markdown_markers(h).width())
        .max()
        .unwrap_or(0);

    // "  label  value" → indent for continuation lines is 2 + label_width + 2
    let value_indent = 2 + label_width + 2;
    let value_width = term_width().saturating_sub(value_indent);

    let mut total_rows = 0u16;
    for (ri, row) in rows.iter().skip(1).enumerate() {
        if ri > 0 {
            out.newline();
            total_rows += 1;
        }
        for (c, cell) in row.iter().enumerate() {
            let label = header.get(c).map(|s| s.as_str()).unwrap_or("");
            let label_visual = strip_markdown_markers(label).width();
            let pad = label_width.saturating_sub(label_visual);

            let wrapped = wrap_cell_words(out, cell, value_width);
            for (li, line) in wrapped.iter().enumerate() {
                if li == 0 {
                    out.print("  ");
                    out.set_fg(ColorValue::Named(NamedColor::DarkGrey));
                    if dim {
                        out.set_dim();
                    }
                    print_inline_styled(out, label, dim);
                    if pad > 0 {
                        out.print_string(" ".repeat(pad));
                    }
                    out.reset_style();
                    out.print("  ");
                } else {
                    out.print_string(" ".repeat(value_indent));
                }
                print_inline_styled(out, line, dim);
                out.newline();
                total_rows += 1;
            }
        }
    }
    total_rows
}

/// Word-wrap cell text so each line's visual width (after stripping markers) fits within `max_width`.
/// Only breaks at spaces that are outside inline markdown spans.
fn wrap_cell_words(out: &mut SpanCollector, text: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![text.to_string()];
    }

    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let breakable = breakable_positions(text);

    let mut lines = Vec::new();
    let mut line_start = 0usize;
    let mut last_break = None::<usize>;
    for ci in 0..len {
        if breakable[ci] {
            last_break = Some(ci);
        }
        let visual_width =
            strip_markdown_markers(&chars[line_start..=ci].iter().collect::<String>()).width();

        if visual_width > max_width {
            if let Some(bp) = last_break {
                let line: String = chars[line_start..bp].iter().collect();
                lines.push(line);
                line_start = bp + 1;
                last_break = None;
            }
        }
    }
    if line_start < len {
        let line: String = chars[line_start..].iter().collect();
        lines.push(line);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    if lines.len() > 1 {
        out.mark_wrapped();
    }
    lines
}

/// Find the visual width of the longest unwrappable segment in text.
/// Used to compute minimum column widths.
fn min_visual_width(text: &str) -> usize {
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let breakable = breakable_positions(text);

    let mut max_w = 0usize;
    let mut seg_start = 0;
    for ci in 0..len {
        if breakable[ci] {
            if ci > seg_start {
                let seg: String = chars[seg_start..ci].iter().collect();
                max_w = max_w.max(strip_markdown_markers(&seg).width());
            }
            seg_start = ci + 1;
        }
    }
    if seg_start < len {
        let seg: String = chars[seg_start..].iter().collect();
        max_w = max_w.max(strip_markdown_markers(&seg).width());
    }
    max_w
}

/// Render inline markdown spans: `**bold**`, `__bold__`, `*italic*`, `_italic_`,
/// `***bold+italic***`, `` `code` ``, `~~strikethrough~~`.
/// Everything else passes through literally.
pub(crate) fn print_inline_styled(out: &mut SpanCollector, text: &str, dim: bool) {
    if dim {
        out.push_dim();
    }
    let chars: Vec<char> = text.chars().collect();
    let nodes = parse_inline(&chars, 0, chars.len());
    emit_inline_nodes(out, &nodes);
    if dim {
        out.pop_style();
    }
}

// ── Inline markdown AST + parser ─────────────────────────────────────────
//
// `print_inline_styled` parses its input into a small `InlineNode` tree
// and then walks the tree to emit spans. The tree approach is what lets
// nested spans (bold containing italic, code inside italic, …) render
// correctly: each inner node pushes a style on top of the outer one
// instead of flatly resetting between spans.
//
// Delimiter matching is **strict** on count: an opener of length N can
// only match a closer of length N. That prevents the "inverted" case
// where e.g. `**text*` used to flip an unclosed bold into an italic by
// letting a single `*` close a double `**`. Runs that don't match
// anything are emitted as literal text *as a whole run*, so the trailing
// `*` of `**text*` never gets re-scanned as a new italic opener.

enum InlineNode {
    Text(String),
    Code(String),
    Strike(Vec<InlineNode>),
    Bold(Vec<InlineNode>),
    Italic(Vec<InlineNode>),
    BoldItalic(Vec<InlineNode>),
}

/// Parse the slice `chars[start..end]` into a flat list of `InlineNode`s.
/// Recurses into emphasis/strikethrough content so nesting works, but
/// treats code-span content as literal.
fn parse_inline(chars: &[char], start: usize, end: usize) -> Vec<InlineNode> {
    let mut nodes: Vec<InlineNode> = Vec::new();
    let mut plain = String::new();
    let mut i = start;

    macro_rules! flush_plain {
        () => {
            if !plain.is_empty() {
                nodes.push(InlineNode::Text(std::mem::take(&mut plain)));
            }
        };
    }

    while i < end {
        // Code span (precedence over emphasis: CommonMark §6.1).
        if chars[i] == '`' {
            if let Some(close) = find_code_close(chars, i + 1, end) {
                flush_plain!();
                let content: String = chars[i + 1..close].iter().collect();
                nodes.push(InlineNode::Code(content));
                i = close + 1;
                continue;
            }
        }

        // Strikethrough `~~text~~`.
        if i + 1 < end && chars[i] == '~' && chars[i + 1] == '~' {
            if let Some(close) = find_strike_close(chars, i + 2, end) {
                flush_plain!();
                let inner = parse_inline(chars, i + 2, close);
                nodes.push(InlineNode::Strike(inner));
                i = close + 2;
                continue;
            }
        }

        // Emphasis: `*italic*`, `**bold**`, `***both***`.
        if chars[i] == '*' || chars[i] == '_' {
            let marker = chars[i];
            let open_run = run_length(chars, i, end, marker);

            if (1..=3).contains(&open_run) && can_open_emphasis(chars, i, open_run, end, marker) {
                if let Some(close) = find_closing_run(chars, i + open_run, end, marker, open_run) {
                    flush_plain!();
                    let inner = parse_inline(chars, i + open_run, close);
                    let node = match open_run {
                        1 => InlineNode::Italic(inner),
                        2 => InlineNode::Bold(inner),
                        3 => InlineNode::BoldItalic(inner),
                        _ => unreachable!("run length checked by contains()"),
                    };
                    nodes.push(node);
                    i = close + open_run;
                    continue;
                }
            }

            // No match — emit the ENTIRE run as literal and skip past it.
            // Emitting char-by-char would let the tail of the run re-enter
            // the parser as a new opener (the "inverted emphasis" bug).
            for _ in 0..open_run {
                plain.push(marker);
            }
            i += open_run;
            continue;
        }

        plain.push(chars[i]);
        i += 1;
    }

    flush_plain!();
    nodes
}

/// Walk an `InlineNode` tree and emit its spans to the sink. Uses
/// `push_style`/`pop_style` so inner nodes inherit the outer style —
/// e.g. italic inside bold becomes a single span with both attributes.
fn emit_inline_nodes(out: &mut SpanCollector, nodes: &[InlineNode]) {
    for node in nodes {
        match node {
            InlineNode::Text(s) => out.print(s),
            InlineNode::Code(s) => {
                out.push_fg(ColorValue::Role(ColorRole::Accent));
                out.print(s);
                out.pop_style();
            }
            InlineNode::Strike(children) => {
                out.push_crossedout();
                emit_inline_nodes(out, children);
                out.pop_style();
            }
            InlineNode::Bold(children) => {
                out.push_bold();
                emit_inline_nodes(out, children);
                out.pop_style();
            }
            InlineNode::Italic(children) => {
                out.push_italic();
                emit_inline_nodes(out, children);
                out.pop_style();
            }
            InlineNode::BoldItalic(children) => {
                let mut style = out.snapshot_style();
                style.bold = true;
                style.italic = true;
                out.push_style(style);
                emit_inline_nodes(out, children);
                out.pop_style();
            }
        }
    }
}

// ── Parse-then-wrap pipeline ─────────────────────────────────────────
//
// Inline markdown is parsed into styled spans FIRST, then wrapped by
// display width. This preserves formatting across soft-wrap boundaries.

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct InlineStyle {
    pub bold: bool,
    pub italic: bool,
    pub dim: bool,
    pub crossedout: bool,
    pub code: bool,
    pub fg: Option<super::super::display::ColorValue>,
}

#[derive(Clone, Debug)]
pub(crate) struct InlineSpan {
    pub text: String,
    pub style: InlineStyle,
}

pub(crate) fn parse_inline_spans(text: &str, dim: bool) -> Vec<InlineSpan> {
    let chars: Vec<char> = text.chars().collect();
    let nodes = parse_inline(&chars, 0, chars.len());
    let base = InlineStyle {
        dim,
        ..Default::default()
    };
    let mut out = Vec::new();
    flatten_nodes_into(&nodes, &base, &mut out);
    out
}

fn flatten_nodes_into(nodes: &[InlineNode], style: &InlineStyle, out: &mut Vec<InlineSpan>) {
    for node in nodes {
        match node {
            InlineNode::Text(s) if !s.is_empty() => {
                out.push(InlineSpan {
                    text: s.clone(),
                    style: style.clone(),
                });
            }
            InlineNode::Text(_) => {}
            InlineNode::Code(s) => {
                out.push(InlineSpan {
                    text: s.clone(),
                    style: InlineStyle {
                        code: true,
                        ..*style
                    },
                });
            }
            InlineNode::Bold(ch) => {
                flatten_nodes_into(
                    ch,
                    &InlineStyle {
                        bold: true,
                        ..*style
                    },
                    out,
                );
            }
            InlineNode::Italic(ch) => {
                flatten_nodes_into(
                    ch,
                    &InlineStyle {
                        italic: true,
                        ..*style
                    },
                    out,
                );
            }
            InlineNode::BoldItalic(ch) => {
                flatten_nodes_into(
                    ch,
                    &InlineStyle {
                        bold: true,
                        italic: true,
                        ..*style
                    },
                    out,
                );
            }
            InlineNode::Strike(ch) => {
                flatten_nodes_into(
                    ch,
                    &InlineStyle {
                        crossedout: true,
                        ..*style
                    },
                    out,
                );
            }
        }
    }
}

pub(crate) fn wrap_inline_spans(spans: &[InlineSpan], max_cols: usize) -> Vec<Vec<InlineSpan>> {
    use unicode_width::UnicodeWidthChar;

    if max_cols == 0 || spans.is_empty() {
        return vec![spans.to_vec()];
    }

    let mut rows: Vec<Vec<InlineSpan>> = Vec::new();
    let mut cur_row: Vec<InlineSpan> = Vec::new();
    let mut col = 0usize;

    for span in spans {
        let mut remaining = span.text.as_str();
        while !remaining.is_empty() {
            let word_end = remaining
                .find(' ')
                .map(|i| i + 1)
                .unwrap_or(remaining.len());
            let word = &remaining[..word_end];
            remaining = &remaining[word_end..];

            let word_width: usize = word.chars().map(|c| c.width().unwrap_or(0)).sum();

            if col + word_width > max_cols && col > 0 {
                rows.push(std::mem::take(&mut cur_row));
                col = 0;
            }

            if word_width > max_cols {
                for ch in word.chars() {
                    let cw = ch.width().unwrap_or(0);
                    if col + cw > max_cols && col > 0 {
                        rows.push(std::mem::take(&mut cur_row));
                        col = 0;
                    }
                    append_char_to_row(&mut cur_row, ch, &span.style);
                    col += cw;
                }
            } else {
                append_text_to_row(&mut cur_row, word, &span.style);
                col += word_width;
            }
        }
    }

    if !cur_row.is_empty() || rows.is_empty() {
        rows.push(cur_row);
    }

    rows
}

fn append_text_to_row(row: &mut Vec<InlineSpan>, text: &str, style: &InlineStyle) {
    if let Some(last) = row.last_mut() {
        if last.style == *style {
            last.text.push_str(text);
            return;
        }
    }
    row.push(InlineSpan {
        text: text.to_string(),
        style: style.clone(),
    });
}

fn append_char_to_row(row: &mut Vec<InlineSpan>, ch: char, style: &InlineStyle) {
    if let Some(last) = row.last_mut() {
        if last.style == *style {
            last.text.push(ch);
            return;
        }
    }
    row.push(InlineSpan {
        text: ch.to_string(),
        style: style.clone(),
    });
}

pub(crate) fn emit_inline_spans(out: &mut SpanCollector, spans: &[InlineSpan]) {
    use super::super::display::{ColorRole, ColorValue, SpanStyle};

    for span in spans {
        let style = SpanStyle {
            fg: if span.style.code {
                Some(ColorValue::Role(ColorRole::Accent))
            } else {
                span.style.fg
            },
            bold: span.style.bold,
            italic: span.style.italic,
            dim: span.style.dim,
            crossedout: span.style.crossedout,
            ..Default::default()
        };
        out.push_style(style);
        out.print(&span.text);
        out.pop_style();
    }
}

pub(crate) fn inline_spans_width(spans: &[InlineSpan]) -> usize {
    spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.text.as_str()))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::super::super::display::{ColorRole, ColorValue, SpanStyle};
    use super::super::super::layout_out::SpanCollector;
    use super::super::syntax::render_code_block;
    use super::*;

    /// Render `text` through `print_inline_styled` (dim=false) and return
    /// a compact `Vec<(tag, text)>` representation of the span tree.
    /// Tags: "plain", "bold", "italic", "bi" (bold+italic), "code",
    /// "strike". Adjacent spans with the same style are merged by the
    /// sink, so you get one entry per visible style run.
    fn parse(text: &str) -> Vec<(&'static str, String)> {
        let mut sink = SpanCollector::new(200);
        print_inline_styled(&mut sink, text, false);
        let block = sink.finish();
        let line = match block.lines.into_iter().next() {
            Some(l) => l,
            None => return Vec::new(),
        };
        line.spans
            .into_iter()
            .filter(|s| !s.text.is_empty())
            .map(|s| (tag_for(&s.style), s.text))
            .collect()
    }

    fn tag_for(style: &SpanStyle) -> &'static str {
        // Code spans carry an accent foreground; they can also inherit
        // bold/italic when nested inside emphasis, in which case the
        // rendered span shows both attributes at once.
        let is_code = matches!(style.fg, Some(ColorValue::Role(ColorRole::Accent)));
        match (style.bold, style.italic, style.crossedout, is_code) {
            (false, false, false, false) => "plain",
            (true, false, false, false) => "bold",
            (false, true, false, false) => "italic",
            (true, true, false, false) => "bi",
            (false, false, true, false) => "strike",
            (false, false, false, true) => "code",
            (true, false, false, true) => "bold+code",
            (false, true, false, true) => "italic+code",
            (true, true, false, true) => "bi+code",
            _ => "mixed",
        }
    }

    // Tag shorthands.
    fn p(s: &str) -> (&'static str, String) {
        ("plain", s.into())
    }
    fn b(s: &str) -> (&'static str, String) {
        ("bold", s.into())
    }
    fn i(s: &str) -> (&'static str, String) {
        ("italic", s.into())
    }
    fn bi(s: &str) -> (&'static str, String) {
        ("bi", s.into())
    }
    fn c(s: &str) -> (&'static str, String) {
        ("code", s.into())
    }
    fn s(s: &str) -> (&'static str, String) {
        ("strike", s.into())
    }

    // ── Plain ──────────────────────────────────────────────────────────

    #[test]
    fn plain_text() {
        assert_eq!(parse("hello world"), vec![p("hello world")]);
    }

    #[test]
    fn empty_string() {
        assert_eq!(parse(""), vec![]);
    }

    // ── Bold ───────────────────────────────────────────────────────────

    #[test]
    fn bold_star() {
        assert_eq!(parse("**hello**"), vec![b("hello")]);
    }

    #[test]
    fn bold_underscore() {
        assert_eq!(parse("__hello__"), vec![b("hello")]);
    }

    #[test]
    fn bold_within_text() {
        assert_eq!(parse("a **bold** c"), vec![p("a "), b("bold"), p(" c")]);
    }

    // ── Italic ─────────────────────────────────────────────────────────

    #[test]
    fn italic_star() {
        assert_eq!(parse("*hello*"), vec![i("hello")]);
    }

    #[test]
    fn italic_underscore() {
        assert_eq!(parse("_hello_"), vec![i("hello")]);
    }

    #[test]
    fn italic_within_text() {
        assert_eq!(parse("a *word* b"), vec![p("a "), i("word"), p(" b")]);
    }

    // ── Bold + italic (triple delimiters) ──────────────────────────────

    #[test]
    fn bold_italic_star() {
        assert_eq!(parse("***both***"), vec![bi("both")]);
    }

    #[test]
    fn bold_italic_underscore() {
        assert_eq!(parse("___both___"), vec![bi("both")]);
    }

    // ── Inline code ────────────────────────────────────────────────────

    #[test]
    fn inline_code() {
        assert_eq!(parse("`foo`"), vec![c("foo")]);
    }

    #[test]
    fn inline_code_with_stars_inside() {
        // Stars inside backticks are literal.
        assert_eq!(parse("`*not bold*`"), vec![c("*not bold*")]);
    }

    #[test]
    fn inline_code_with_underscores_inside() {
        assert_eq!(parse("`_not italic_`"), vec![c("_not italic_")]);
    }

    #[test]
    fn inline_code_around_text() {
        assert_eq!(
            parse("call `foo()` please"),
            vec![p("call "), c("foo()"), p(" please")]
        );
    }

    // ── Strikethrough ──────────────────────────────────────────────────

    #[test]
    fn strikethrough_basic() {
        assert_eq!(parse("~~gone~~"), vec![s("gone")]);
    }

    // ── Intraword underscores (CommonMark: NOT emphasis) ──────────────

    #[test]
    fn intraword_underscore_identifier() {
        // `snake_case_variable` — underscores are part of the identifier.
        assert_eq!(parse("snake_case_variable"), vec![p("snake_case_variable")]);
    }

    #[test]
    fn intraword_underscore_in_url() {
        assert_eq!(
            parse("https://example.com/foo_bar_baz"),
            vec![p("https://example.com/foo_bar_baz")]
        );
    }

    #[test]
    fn intraword_underscore_between_letters() {
        assert_eq!(parse("foo_bar"), vec![p("foo_bar")]);
    }

    // ── Unclosed delimiters (should stay literal) ─────────────────────

    #[test]
    fn unclosed_bold_stays_literal() {
        assert_eq!(parse("**text"), vec![p("**text")]);
    }

    #[test]
    fn unclosed_italic_stays_literal() {
        assert_eq!(parse("*text"), vec![p("*text")]);
    }

    #[test]
    fn unclosed_code_stays_literal() {
        assert_eq!(parse("`unclosed"), vec![p("`unclosed")]);
    }

    /// Regression: `**text*` (3 stars) is an unclosed bold, NOT an
    /// opened bold that collapses to italic. Previously the parser
    /// dropped the leading `*` and produced an italic, giving the user
    /// an "inverted" result (italic instead of bold).
    #[test]
    fn odd_star_count_does_not_invert_emphasis() {
        assert_eq!(parse("**text*"), vec![p("**text*")]);
    }

    #[test]
    fn odd_star_count_trailing_double() {
        assert_eq!(parse("*text**"), vec![p("*text**")]);
    }

    // ── Nested emphasis (CommonMark supports this) ────────────────────

    #[test]
    fn bold_containing_italic() {
        // `**bold *italic* bold**` — inner italic must render inside bold.
        assert_eq!(
            parse("**bold *it* bold**"),
            vec![b("bold "), bi("it"), b(" bold")]
        );
    }

    #[test]
    fn italic_containing_bold() {
        assert_eq!(
            parse("*it **bold** it*"),
            vec![i("it "), bi("bold"), i(" it")]
        );
    }

    #[test]
    fn bold_containing_code() {
        // Code span nested inside bold inherits the outer bold, so the
        // inner span carries both attributes at once.
        assert_eq!(
            parse("**call `foo()` now**"),
            vec![b("call "), ("bold+code", "foo()".into()), b(" now")]
        );
    }

    // ── Precedence: code > emphasis ───────────────────────────────────

    #[test]
    fn code_inside_italic() {
        // `*a `code` b*` — italic wrapping, code inside. The inner code
        // span inherits italic, so it's italic+code.
        assert_eq!(
            parse("*a `code` b*"),
            vec![i("a "), ("italic+code", "code".into()), i(" b")]
        );
    }

    #[test]
    fn code_containing_italic_stars() {
        // The `*` inside a code span is literal.
        assert_eq!(
            parse("before `*x*` after"),
            vec![p("before "), c("*x*"), p(" after")]
        );
    }

    // ── Multiple runs on one line ─────────────────────────────────────

    #[test]
    fn bold_then_italic() {
        assert_eq!(parse("**a** and *b*"), vec![b("a"), p(" and "), i("b")]);
    }

    #[test]
    fn adjacent_bolds() {
        assert_eq!(parse("**a** **b**"), vec![b("a"), p(" "), b("b")]);
    }

    // ── Asterisk as literal ───────────────────────────────────────────

    #[test]
    fn asterisk_as_multiplication() {
        // `a * b` — stars with whitespace on both sides, not emphasis.
        assert_eq!(parse("a * b = c"), vec![p("a * b = c")]);
    }

    #[test]
    fn trailing_lone_star() {
        assert_eq!(parse("note*"), vec![p("note*")]);
    }

    #[test]
    fn star_right_after_word() {
        assert_eq!(parse("footnote*"), vec![p("footnote*")]);
    }

    // ── Stress: edge cases the spec cares about ──────────────────────

    #[test]
    fn space_before_closing_delim_rejects_emphasis() {
        // `**text **` — close preceded by space is NOT right-flanking.
        assert_eq!(parse("**text **"), vec![p("**text **")]);
    }

    #[test]
    fn space_after_opening_delim_rejects_emphasis() {
        // `** text**` — open followed by space is NOT left-flanking.
        assert_eq!(parse("** text**"), vec![p("** text**")]);
    }

    #[test]
    fn four_star_run_is_literal() {
        // Runs of 4+ delimiters have no standard meaning; keep them literal.
        assert_eq!(parse("****text****"), vec![p("****text****")]);
    }

    #[test]
    fn deeply_nested_bold_italic_code() {
        // `**outer *inner `code` inner* outer**`
        assert_eq!(
            parse("**a *b `c` d* e**"),
            vec![
                b("a "),
                bi("b "),
                ("bi+code", "c".into()),
                bi(" d"),
                b(" e"),
            ]
        );
    }

    #[test]
    fn bold_italic_containing_plain_text() {
        assert_eq!(parse("***a b c***"), vec![bi("a b c")]);
    }

    #[test]
    fn two_italic_runs_separated_by_text() {
        assert_eq!(
            parse("start *a* mid *b* end"),
            vec![p("start "), i("a"), p(" mid "), i("b"), p(" end"),]
        );
    }

    #[test]
    fn mixed_underscore_and_star_dont_match() {
        // `*foo_` — `*` opener, `_` is just a literal char, not a closer.
        assert_eq!(parse("*foo_"), vec![p("*foo_")]);
    }

    #[test]
    fn underscore_surrounded_by_non_alnum_can_italic() {
        // `(_foo_)` — `_` is not intraword here because `(` and `)` are
        // not alphanumeric. CommonMark permits this as italic.
        assert_eq!(parse("(_foo_)"), vec![p("("), i("foo"), p(")")]);
    }

    #[test]
    fn star_can_italic_intraword() {
        // Unlike `_`, `*` does not have the intraword restriction.
        assert_eq!(parse("foo*bar*baz"), vec![p("foo"), i("bar"), p("baz")]);
    }

    #[test]
    fn code_with_backtick_literal() {
        // A backtick inside a code span closes it — our single-backtick
        // parser can't represent literal backticks inside a code span.
        // `` `a`b` `` → code("a") + plain("b`").
        assert_eq!(parse("`a`b`"), vec![c("a"), p("b`")]);
    }

    #[test]
    fn strip_markers_matches_parse_for_nested() {
        // The visible width used by wrapping code must match the text
        // that the parser actually emits.
        let text = "**bold *it* bold**";
        let stripped = strip_markdown_markers(text);
        assert_eq!(stripped, "bold it bold");
        // And matches what print_inline_styled would emit:
        let emitted: String = parse(text).into_iter().map(|(_, t)| t).collect();
        assert_eq!(emitted, stripped);
    }

    #[test]
    fn strip_markers_handles_intraword_underscore() {
        // Must not strip `_` that are intraword — they're part of the
        // identifier, not emphasis markers.
        assert_eq!(
            strip_markdown_markers("call foo_bar_baz() now"),
            "call foo_bar_baz() now"
        );
    }

    #[test]
    fn strip_markers_matches_parse_for_unclosed_bold() {
        // The old parser produced `*` + italic("text") for `**text*`,
        // giving width=4 after stripping. The new parser keeps the run
        // literal, so stripping should return the whole thing.
        assert_eq!(strip_markdown_markers("**text*"), "**text*");
    }

    /// Source-text round-trip for fenced code blocks: opening fence on
    /// the first row, closing fence on the last, raw line per row in
    /// between. Lets vim-visual / click-drag selections that cover any
    /// subset of code rows reconstruct the markdown — fences re-attach
    /// when the first / last row is in the selection.
    #[test]
    fn render_code_block_with_fence_attaches_source_text_per_line() {
        let mut sink = SpanCollector::new(80);
        let lines = ["let x = 1;", "let y = 2;", "let z = 3;"];
        render_code_block(&mut sink, &lines, "rust", 80, false, None, true);
        let block = sink.finish();
        assert_eq!(block.lines.len(), 3);
        assert_eq!(
            block.lines[0].source_text.as_deref(),
            Some("```rust\nlet x = 1;")
        );
        assert_eq!(block.lines[1].source_text.as_deref(), Some("let y = 2;"));
        assert_eq!(
            block.lines[2].source_text.as_deref(),
            Some("let z = 3;\n```")
        );
    }

    #[test]
    fn render_code_block_single_line_wraps_with_both_fences() {
        let mut sink = SpanCollector::new(80);
        render_code_block(&mut sink, &["let x = 1;"], "rust", 80, false, None, true);
        let block = sink.finish();
        assert_eq!(block.lines.len(), 1);
        assert_eq!(
            block.lines[0].source_text.as_deref(),
            Some("```rust\nlet x = 1;\n```")
        );
    }

    #[test]
    fn render_code_block_without_fence_sets_raw_source_per_line() {
        // Block::CodeLine streaming path: no fences, but each line
        // still gets its raw source so partial selections preserve it.
        let mut sink = SpanCollector::new(80);
        render_code_block(&mut sink, &["let x = 1;"], "rust", 80, false, None, false);
        let block = sink.finish();
        assert_eq!(block.lines.len(), 1);
        assert_eq!(block.lines[0].source_text.as_deref(), Some("let x = 1;"));
    }
}
