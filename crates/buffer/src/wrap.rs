/// Wrap text with a first-row prefix and a continuation prefix.
/// Prefer spaces as break points, omitting the separating space at a wrap.
/// Oversized words break at grapheme boundaries; a prefix and its first text
/// grapheme stay together even when they exceed the available width.
/// Explicit newlines always start continuation rows. A zero width disables
/// width-based breaks while preserving explicit and empty lines.
pub fn wrap_prefixed(prefix: &str, text: &str, cont_prefix: &str, width: usize) -> Vec<String> {
    let mut rows = Vec::new();
    let mut current_prefix = prefix;
    for line in text.split('\n') {
        let mut remaining = line;
        loop {
            let (row, consumed) = prefixed_row(current_prefix, remaining, width);
            rows.push(row);
            current_prefix = cont_prefix;
            if consumed >= remaining.len() {
                break;
            }
            debug_assert!(consumed > 0, "a wrapped row must consume text");
            remaining = crate::text::slice(remaining, consumed..remaining.len());
        }
    }
    rows
}

fn prefixed_row(prefix: &str, text: &str, width: usize) -> (String, usize) {
    if width == 0 {
        return (format!("{prefix}{text}"), text.len());
    }
    let joined = format!("{prefix}{text}");
    let mut row = prefix.to_string();
    let mut consumed = 0;
    let mut word_break = None;
    for (start, grapheme) in crate::cell_width::grapheme_indices(&joined) {
        let end = start + grapheme.len();
        if end <= prefix.len() {
            continue;
        }
        let consumed_end = end - prefix.len();
        if grapheme == " " && consumed > 0 {
            word_break = Some((row.len(), consumed_end));
        }
        let piece = crate::text::slice(text, consumed..consumed_end);
        if consumed > 0 && crate::cell_width::joined_text_width([row.as_str(), piece]) > width {
            if let Some((row_end, next)) = word_break {
                row.truncate(row_end);
                return (row, next);
            }
            break;
        }
        row.push_str(piece);
        consumed = consumed_end;
    }
    (row, consumed)
}

/// Wrap `line` to `width` display columns, breaking at word boundaries.
/// Words wider than `width` are broken grapheme-by-grapheme.
///
/// Returns byte ranges within `line`. When `line` contains newlines, each
/// logical line is wrapped independently and embedded newlines force breaks
/// (the `'\n'` byte itself is not included in any chunk).
///
/// At least one chunk is always returned (even for empty input).
pub fn wrap_line_ranges(line: &str, width: usize) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    visit_line_ranges(line, width, |start, end| out.push((start, end)));
    out
}

/// Visit wrapped byte ranges for `line` without allocating the range list.
pub fn visit_line_ranges(line: &str, width: usize, mut visit: impl FnMut(usize, usize)) -> usize {
    let mut count = 0usize;
    let mut visit_counted = |start, end| {
        count += 1;
        visit(start, end);
    };
    if line.is_empty() {
        visit_counted(0, 0);
        return count;
    }
    if width == 0 {
        visit_counted(0, line.len());
        return count;
    }
    let mut logical_start = 0usize;
    loop {
        let rel = line[logical_start..].find('\n');
        let logical_end = rel.map(|p| logical_start + p).unwrap_or(line.len());
        wrap_logical(line, logical_start, logical_end, width, &mut visit_counted);
        match rel {
            Some(p) => {
                logical_start = logical_start + p + 1; // skip '\n'
                if logical_start > line.len() {
                    break;
                }
                if logical_start == line.len() {
                    // Trailing newline → final empty logical line.
                    visit_counted(logical_start, logical_start);
                    break;
                }
            }
            None => break,
        }
    }
    count
}

pub fn count_line_ranges(line: &str, width: usize) -> usize {
    visit_line_ranges(line, width, |_, _| {})
}

fn wrap_logical(
    line: &str,
    start: usize,
    end: usize,
    width: usize,
    visit: &mut impl FnMut(usize, usize),
) {
    if start == end {
        visit(start, end);
        return;
    }
    let mut chunk_start = start;
    let mut chunk_end = start;
    let mut col = 0usize;
    let mut word_start = start;
    let boundaries = crate::cell_width::grapheme_indices(&line[start..end])
        .map(|(offset, grapheme)| (start + offset, grapheme))
        .chain(std::iter::once((end, "")));
    for (i, grapheme) in boundaries {
        let at_end = i == end;
        let at_space = grapheme == " ";
        if !(at_space || at_end) {
            continue;
        }
        // Process the word `line[word_start..i]`.
        let word_end = i;
        let word_w: usize = crate::cell_width::text_width(&line[word_start..word_end]);
        let trailing = if at_space { 1 } else { 0 };
        let total_w = word_w + trailing;
        // If word+space doesn't fit on current line and chunk has content, emit.
        if col + total_w > width && col > 0 {
            let current = &line[chunk_start..chunk_end];
            if !(word_w > width && current.chars().all(|ch| ch == ' ')) {
                visit(chunk_start, chunk_end);
                chunk_start = word_start;
                col = 0;
            }
        }
        if word_w > width {
            // Grapheme-break the word.
            for (offset, grapheme) in
                crate::cell_width::grapheme_indices(&line[word_start..word_end])
            {
                let idx = word_start + offset;
                let end_idx = idx + grapheme.len();
                let grapheme_width = crate::cell_width::text_width(grapheme);
                if col + grapheme_width > width && col > 0 {
                    visit(chunk_start, chunk_end);
                    chunk_start = idx;
                    col = 0;
                }
                chunk_end = end_idx;
                col += grapheme_width;
            }
        } else {
            chunk_end = word_end;
            col += word_w;
        }
        if at_space {
            // Append the space (may force a wrap if it overflows; rare with width≥1).
            if col + 1 > width && col > 0 {
                visit(chunk_start, chunk_end);
                chunk_start = word_end + 1;
                chunk_end = word_end + 1;
                col = 0;
            } else {
                chunk_end = word_end + 1;
                col += 1;
            }
            word_start = word_end + 1;
        }
    }
    visit(chunk_start, chunk_end);
}

/// Wrap `line` to `width` display columns, breaking at word boundaries.
/// Words wider than `width` are broken grapheme-by-grapheme.
pub fn wrap_line(line: &str, width: usize) -> Vec<String> {
    wrap_line_ranges(line, width)
        .into_iter()
        .map(|(s, e)| line[s..e].to_string())
        .collect()
}

/// Like [`wrap_line`] but returns borrowed slices into `line` instead of
/// allocating a `String` per chunk. The caller must ensure `line` outlives
/// the returned slices.
pub fn wrap_line_borrowed(line: &str, width: usize) -> Vec<&str> {
    wrap_line_ranges(line, width)
        .into_iter()
        .map(|(s, e)| &line[s..e])
        .collect()
}

#[cfg(test)]
mod wrap_tests {
    use super::*;

    #[test]
    fn prefixed_wrap_prefers_words_and_omits_break_spaces() {
        assert_eq!(
            wrap_prefixed(" 1. ", "hello world", "    ", 12),
            [" 1. hello", "    world"]
        );
        assert_eq!(
            wrap_prefixed(" 1. ", "hello world", "    ", 9),
            [" 1. hello", "    world"]
        );
        assert_eq!(
            wrap_prefixed("=> ", "one two three", "> ", 8),
            ["=> one", "> two", "> three"]
        );
    }

    #[test]
    fn prefixed_wrap_preserves_explicit_and_empty_lines() {
        for width in [0, 10] {
            assert_eq!(wrap_prefixed(" 1. ", "", "    ", width), [" 1. "]);
            assert_eq!(
                wrap_prefixed(" 1. ", "first\n\nlast\n", "    ", width),
                [" 1. first", "    ", "    last", "    "]
            );
        }
        assert_eq!(
            wrap_prefixed(" 1. ", "hello world\n界 e\u{301}", "    ", 0),
            [" 1. hello world", "    界 e\u{301}"]
        );
    }

    #[test]
    fn prefixed_wrap_keeps_graphemes_atomic_even_in_narrow_rows() {
        let text = "e\u{301}界👩\u{200d}💻";
        assert_eq!(
            wrap_prefixed(" 1. ", text, "    ", 6),
            [" 1. e\u{301}", "    界", "    👩\u{200d}💻"]
        );
        assert_eq!(
            wrap_prefixed(" 1. ", "界界", "    ", 1),
            [" 1. 界", "    界"]
        );
        assert_eq!(
            wrap_prefixed("", "ab\u{fe0f}x", "9", 2),
            ["ab\u{fe0f}", "9x"]
        );
        assert_eq!(
            wrap_prefixed("", "ab\n\u{fe0f}x", "9", 2),
            ["ab", "9\u{fe0f}", "9x"]
        );
        assert_eq!(wrap_prefixed("", "\u{600} x", "", 1), ["\u{600} ", "x"]);
    }

    #[test]
    fn empty_line_returns_single_empty_chunk() {
        assert_eq!(wrap_line_ranges("", 10), vec![(0, 0)]);
    }

    #[test]
    fn zero_width_returns_whole_line() {
        assert_eq!(wrap_line_ranges("hello world", 0), vec![(0, 11)]);
    }

    #[test]
    fn no_wrap_when_within_width() {
        assert_eq!(wrap_line_ranges("hello", 10), vec![(0, 5)]);
    }

    #[test]
    fn breaks_at_word_boundary() {
        // "hello world" with width 7: "hello " (6) fits; "world" forces wrap.
        let r = wrap_line_ranges("hello world", 7);
        let chunks: Vec<&str> = r.iter().map(|(s, e)| &"hello world"[*s..*e]).collect();
        assert_eq!(chunks, vec!["hello ", "world"]);
    }

    #[test]
    fn oversized_word_char_breaks() {
        // "abcdefghij" with width 4 → "abcd", "efgh", "ij".
        let s = "abcdefghij";
        let r = wrap_line_ranges(s, 4);
        let chunks: Vec<&str> = r.iter().map(|(a, b)| &s[*a..*b]).collect();
        assert_eq!(chunks, vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn oversized_word_wraps_only_between_graphemes() {
        let source = "e\u{301}👩\u{200d}💻9\u{fe0f}🇨🇦";

        assert_eq!(
            wrap_line(source, 2),
            ["e\u{301}", "👩\u{200d}💻", "9\u{fe0f}", "🇨🇦"]
        );
    }

    #[test]
    fn spaces_inside_graphemes_are_not_word_boundaries() {
        let source = "\u{600} x";
        let boundaries: Vec<usize> = crate::cell_width::grapheme_indices(source)
            .map(|(start, _)| start)
            .chain(std::iter::once(source.len()))
            .collect();

        for (start, end) in wrap_line_ranges(source, 1) {
            assert!(boundaries.contains(&start), "invalid start {start}");
            assert!(boundaries.contains(&end), "invalid end {end}");
        }
    }

    #[test]
    fn leading_spaces_stay_with_oversized_word() {
        let s = "  abcdef";
        let r = wrap_line_ranges(s, 4);
        let chunks: Vec<&str> = r.iter().map(|(a, b)| &s[*a..*b]).collect();
        assert_eq!(chunks, vec!["  ab", "cdef"]);
    }

    #[test]
    fn embedded_newline_forces_break() {
        let s = "a\nb";
        let r = wrap_line_ranges(s, 10);
        let chunks: Vec<&str> = r.iter().map(|(a, b)| &s[*a..*b]).collect();
        assert_eq!(chunks, vec!["a", "b"]);
    }

    #[test]
    fn control_chars_count_as_visible_cells() {
        let s = "\0\0\0\0x";
        let r = wrap_line_ranges(s, 3);
        let chunks: Vec<&str> = r.iter().map(|(a, b)| &s[*a..*b]).collect();
        assert_eq!(chunks, vec!["\0\0\0", "\0x"]);
    }

    #[test]
    fn count_line_ranges_matches_ranges() {
        for (line, width) in [
            ("", 10),
            ("a\nb", 10),
            ("hello world", 7),
            ("abcdefghij", 4),
        ] {
            assert_eq!(
                count_line_ranges(line, width),
                wrap_line_ranges(line, width).len()
            );
        }
    }

    #[test]
    fn visit_line_ranges_matches_ranges() {
        let line = "the quick brown fox";
        let mut visited = Vec::new();
        let count = visit_line_ranges(line, 10, |start, end| visited.push((start, end)));
        assert_eq!(count, visited.len());
        assert_eq!(visited, wrap_line_ranges(line, 10));
    }

    #[test]
    fn wrap_line_matches_ranges() {
        let s = "the quick brown fox";
        let by_string = wrap_line(s, 10);
        let by_ranges: Vec<String> = wrap_line_ranges(s, 10)
            .into_iter()
            .map(|(a, b)| s[a..b].to_string())
            .collect();
        assert_eq!(by_string, by_ranges);
    }
}
