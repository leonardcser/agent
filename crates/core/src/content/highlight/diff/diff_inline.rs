use similar::{ChangeTag, InlineChangeMode, InlineChangeOptions, TextDiff};
use smelt_buffer::text;
use std::time::{Duration, Instant};

use super::{DiffByteRange, DiffLine};

// These thresholds are visual policy, not correctness rules. Character-level
// inline diffs are useful only while they still read as coherent chunks: low
// similarity falls back to full-line emphasis, tiny equal islands are absorbed,
// and very fragmented ranges collapse to one span.
const INLINE_MIN_RATIO: f32 = 0.35;
const LINE_PAIR_MIN_SIMILARITY: f32 = 0.45;
const INLINE_CHAR_GAP: usize = 3;
const INLINE_NON_WORD_GAP_BYTES: usize = 5;
const INLINE_FRAGMENT_RANGES: usize = 4;
pub(crate) const MAX_LINE_ALIGNMENT_LINES: usize = 80;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LineAlignment {
    Pair { old: usize, new: usize },
    OldOnly(usize),
    NewOnly(usize),
}

fn changed_line_text(line: &DiffLine) -> Option<&str> {
    match line {
        DiffLine::Delete { text, .. } | DiffLine::Insert { text, .. } => Some(text),
        _ => None,
    }
}

fn changed_line_highlights_mut(line: &mut DiffLine) -> Option<&mut Vec<DiffByteRange>> {
    match line {
        DiffLine::Delete { highlights, .. } | DiffLine::Insert { highlights, .. } => {
            Some(highlights)
        }
        _ => None,
    }
}

pub(super) fn full_line_highlight(text: &str) -> Vec<DiffByteRange> {
    (!text.is_empty())
        .then_some(DiffByteRange {
            start: 0,
            end: text.len(),
        })
        .into_iter()
        .collect()
}

fn inline_options() -> InlineChangeOptions {
    let mut options = InlineChangeOptions::new();
    options
        .mode(InlineChangeMode::Chars)
        .min_ratio(INLINE_MIN_RATIO)
        .semantic_cleanup(true);
    options
}

fn sorted_merged_ranges(mut ranges: Vec<DiffByteRange>) -> Vec<DiffByteRange> {
    ranges.sort_by_key(|range| (range.start, range.end));
    let mut out: Vec<DiffByteRange> = Vec::with_capacity(ranges.len());
    for range in ranges {
        if range.start >= range.end {
            continue;
        }
        if let Some(last) = out.last_mut() {
            if range.start <= last.end {
                last.end = last.end.max(range.end);
                continue;
            }
        }
        out.push(range);
    }
    out
}

fn range_chars(line: &str, range: &DiffByteRange) -> usize {
    text::slice(line, range.start..range.end).chars().count()
}

fn merge_small_gap(line: &str, left: &DiffByteRange, right: &DiffByteRange) -> bool {
    if left.end >= right.start {
        return true;
    }

    let gap = text::slice(line, left.end..right.start);
    let gap_chars = gap.chars().count();
    let changed_chars = range_chars(line, left) + range_chars(line, right);
    if gap_chars <= INLINE_CHAR_GAP && changed_chars >= gap_chars.saturating_mul(4) {
        return true;
    }

    !gap.is_empty()
        && gap.len() <= INLINE_NON_WORD_GAP_BYTES
        && gap.chars().all(|ch| !ch.is_alphanumeric())
        && changed_chars >= gap_chars.saturating_mul(2)
}

fn expand_highlights_to_graphemes(line: &str, ranges: Vec<DiffByteRange>) -> Vec<DiffByteRange> {
    let ranges = ranges
        .into_iter()
        .map(|range| {
            let range = text::covering_grapheme_range(line, range.start..range.end);
            DiffByteRange {
                start: range.start,
                end: range.end,
            }
        })
        .collect();
    sorted_merged_ranges(ranges)
}

fn coalesce_highlight_ranges(line: &str, ranges: Vec<DiffByteRange>) -> Vec<DiffByteRange> {
    let mut ranges = sorted_merged_ranges(ranges);
    for _ in 0..4 {
        if ranges.len() < 2 {
            break;
        }
        let mut merged = Vec::with_capacity(ranges.len());
        let mut changed = false;
        for range in ranges {
            if let Some(last) = merged.last_mut() {
                if merge_small_gap(line, last, &range) {
                    last.end = range.end;
                    changed = true;
                    continue;
                }
            }
            merged.push(range);
        }
        ranges = merged;
        if !changed {
            break;
        }
    }

    if ranges.len() >= INLINE_FRAGMENT_RANGES {
        let total_changed: usize = ranges.iter().map(|range| range_chars(line, range)).sum();
        let total_gap: usize = ranges
            .windows(2)
            .map(|pair| {
                text::slice(line, pair[0].end..pair[1].start)
                    .chars()
                    .count()
            })
            .sum();
        if total_gap <= 6 || total_gap <= total_changed / 2 {
            let start = ranges.first().map(|range| range.start).unwrap_or(0);
            let end = ranges.last().map(|range| range.end).unwrap_or(start);
            ranges = vec![DiffByteRange { start, end }];
        }
    }

    ranges
}

pub(super) fn inline_highlights_for_pair(
    old: &str,
    new: &str,
) -> (Vec<DiffByteRange>, Vec<DiffByteRange>) {
    inline_highlights_for_pair_deadline(old, new, Instant::now() + Duration::from_millis(500))
}

pub(crate) fn inline_highlights_for_pair_deadline(
    old: &str,
    new: &str,
    deadline: Instant,
) -> (Vec<DiffByteRange>, Vec<DiffByteRange>) {
    let diff = TextDiff::from_lines(old, new);
    let mut old_pos = 0usize;
    let mut new_pos = 0usize;
    let mut old_ranges = Vec::new();
    let mut new_ranges = Vec::new();

    for change in
        diff.iter_all_inline_changes_with_options_deadline(inline_options(), Some(deadline))
    {
        for (emphasized, value) in change.iter_strings_lossy() {
            let len = value.len();
            match change.tag() {
                ChangeTag::Equal => {
                    old_pos += len;
                    new_pos += len;
                }
                ChangeTag::Delete => {
                    let end = old_pos + len;
                    if emphasized {
                        old_ranges.push(DiffByteRange {
                            start: old_pos,
                            end,
                        });
                    }
                    old_pos = end;
                }
                ChangeTag::Insert => {
                    let end = new_pos + len;
                    if emphasized {
                        new_ranges.push(DiffByteRange {
                            start: new_pos,
                            end,
                        });
                    }
                    new_pos = end;
                }
            }
        }
    }

    if old_ranges.is_empty() && new_ranges.is_empty() && old != new {
        old_ranges = full_line_highlight(old);
        new_ranges = full_line_highlight(new);
    } else {
        old_ranges = coalesce_highlight_ranges(old, old_ranges);
        new_ranges = coalesce_highlight_ranges(new, new_ranges);
    }

    (
        expand_highlights_to_graphemes(old, old_ranges),
        expand_highlights_to_graphemes(new, new_ranges),
    )
}

fn pairing_similarity(old: &str, new: &str, deadline: Option<Instant>) -> f32 {
    if old == new {
        return 1.0;
    }
    let old = old.trim();
    let new = new.trim();
    if old.is_empty() && new.is_empty() {
        return 1.0;
    }
    if old.is_empty() || new.is_empty() {
        return 0.0;
    }

    let mut config = TextDiff::configure();
    if let Some(deadline) = deadline {
        config.deadline(deadline);
    }
    let raw_ratio = config.diff_chars(old, new).ratio();
    if raw_ratio >= LINE_PAIR_MIN_SIMILARITY {
        return raw_ratio;
    }

    // Pairing is case-tolerant so pure casing edits stay on one changed row;
    // the later inline diff still highlights the actual character changes.
    let old_lower = old.to_lowercase();
    let new_lower = new.to_lowercase();
    raw_ratio.max(config.diff_chars(&old_lower, &new_lower).ratio())
}

fn positional_alignment(old_len: usize, new_len: usize) -> Vec<LineAlignment> {
    let pairs = old_len.min(new_len);
    let mut out = Vec::with_capacity(old_len.max(new_len));
    out.extend((0..pairs).map(|i| LineAlignment::Pair { old: i, new: i }));
    out.extend((pairs..old_len).map(LineAlignment::OldOnly));
    out.extend((pairs..new_len).map(LineAlignment::NewOnly));
    out
}

pub(super) fn align_changed_lines(old: &[&str], new: &[&str]) -> Vec<LineAlignment> {
    align_changed_lines_deadline(old, new, None)
}

pub(crate) fn align_changed_lines_deadline(
    old: &[&str],
    new: &[&str],
    deadline: Option<Instant>,
) -> Vec<LineAlignment> {
    let m = old.len();
    let n = new.len();
    if m == 0 {
        return (0..n).map(LineAlignment::NewOnly).collect();
    }
    if n == 0 {
        return (0..m).map(LineAlignment::OldOnly).collect();
    }
    if m + n > MAX_LINE_ALIGNMENT_LINES {
        return positional_alignment(m, n);
    }

    let mut scores = Vec::with_capacity(m);
    for old_line in old {
        let mut row = Vec::with_capacity(n);
        for new_line in new {
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return positional_alignment(m, n);
            }
            row.push(pairing_similarity(old_line, new_line, deadline));
        }
        scores.push(row);
    }

    let mut dp = vec![vec![0.0f32; n + 1]; m + 1];
    for i in (0..m).rev() {
        for j in (0..n).rev() {
            let skip_old = dp[i + 1][j];
            let skip_new = dp[i][j + 1];
            let similarity = scores[i][j];
            let pair = if similarity >= LINE_PAIR_MIN_SIMILARITY {
                similarity + dp[i + 1][j + 1]
            } else {
                f32::NEG_INFINITY
            };
            dp[i][j] = skip_old.max(skip_new).max(pair);
        }
    }

    let mut out = Vec::with_capacity(m.max(n));
    let mut i = 0usize;
    let mut j = 0usize;
    while i < m || j < n {
        if i == m {
            out.push(LineAlignment::NewOnly(j));
            j += 1;
            continue;
        }
        if j == n {
            out.push(LineAlignment::OldOnly(i));
            i += 1;
            continue;
        }

        let similarity = scores[i][j];
        let pair = if similarity >= LINE_PAIR_MIN_SIMILARITY {
            similarity + dp[i + 1][j + 1]
        } else {
            f32::NEG_INFINITY
        };
        let skip_old = dp[i + 1][j];
        let skip_new = dp[i][j + 1];
        let best = dp[i][j];

        if pair.is_finite() && pair >= best - f32::EPSILON {
            out.push(LineAlignment::Pair { old: i, new: j });
            i += 1;
            j += 1;
        } else if skip_new > skip_old + f32::EPSILON {
            out.push(LineAlignment::NewOnly(j));
            j += 1;
        } else {
            out.push(LineAlignment::OldOnly(i));
            i += 1;
        }
    }

    out
}

pub(super) fn annotate_inline_highlights(lines: &mut [DiffLine]) {
    let mut i = 0usize;
    while i < lines.len() {
        if !matches!(lines[i], DiffLine::Delete { .. }) {
            i += 1;
            continue;
        }

        let del_start = i;
        while i < lines.len() && matches!(lines[i], DiffLine::Delete { .. }) {
            i += 1;
        }
        let ins_start = i;
        while i < lines.len() && matches!(lines[i], DiffLine::Insert { .. }) {
            i += 1;
        }
        let del_end = ins_start;
        let ins_end = i;

        let old_lines: Vec<&str> = lines[del_start..del_end]
            .iter()
            .map(|line| changed_line_text(line).unwrap_or_default())
            .collect();
        let new_lines: Vec<&str> = lines[ins_start..ins_end]
            .iter()
            .map(|line| changed_line_text(line).unwrap_or_default())
            .collect();

        let mut old_highlights = vec![None; old_lines.len()];
        let mut new_highlights = vec![None; new_lines.len()];
        for alignment in align_changed_lines(&old_lines, &new_lines) {
            match alignment {
                LineAlignment::Pair { old, new } => {
                    let (old_ranges, new_ranges) =
                        inline_highlights_for_pair(old_lines[old], new_lines[new]);
                    old_highlights[old] = Some(old_ranges);
                    new_highlights[new] = Some(new_ranges);
                }
                LineAlignment::OldOnly(old) => {
                    old_highlights[old] = Some(full_line_highlight(old_lines[old]));
                }
                LineAlignment::NewOnly(new) => {
                    new_highlights[new] = Some(full_line_highlight(new_lines[new]));
                }
            }
        }

        for (offset, ranges) in old_highlights.into_iter().enumerate() {
            if let Some(ranges) = ranges {
                if let Some(highlights) =
                    changed_line_highlights_mut(&mut lines[del_start + offset])
                {
                    *highlights = ranges;
                }
            }
        }
        for (offset, ranges) in new_highlights.into_iter().enumerate() {
            if let Some(ranges) = ranges {
                if let Some(highlights) =
                    changed_line_highlights_mut(&mut lines[ins_start + offset])
                {
                    *highlights = ranges;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ranges(items: &[(usize, usize)]) -> Vec<DiffByteRange> {
        items
            .iter()
            .map(|&(start, end)| DiffByteRange { start, end })
            .collect()
    }

    #[test]
    fn coalesces_tiny_equal_gaps_between_inline_ranges() {
        let merged = coalesce_highlight_ranges("aa-bb", ranges(&[(0, 2), (3, 5)]));
        assert_eq!(merged.len(), 1);
        assert_eq!((merged[0].start, merged[0].end), (0, 5));
    }

    #[test]
    fn keeps_distant_inline_ranges_separate() {
        let merged = coalesce_highlight_ranges("aa......bb", ranges(&[(0, 2), (8, 10)]));
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn inline_highlights_expand_to_complete_graphemes() {
        for grapheme in ["e\u{301}", "9\u{fe0f}", "👩\u{200d}💻", "🇨🇦"] {
            let text = format!("a{grapheme}z");
            let start = 1;
            let inside = start + grapheme.chars().next().unwrap().len_utf8();
            let expanded = expand_highlights_to_graphemes(
                &text,
                ranges(&[(inside, (inside + 1).min(start + grapheme.len()))]),
            );

            assert_eq!(expanded.len(), 1);
            assert_eq!(
                (expanded[0].start, expanded[0].end),
                (start, start + grapheme.len())
            );
        }
    }

    #[test]
    fn aligns_changed_lines_by_similarity_not_position() {
        let old = vec![
            "let name = user.name();",
            "let age = user.age();",
            "render(name, age);",
        ];
        let new = vec![
            "let id = user.id();",
            "let name = user.display_name();",
            "let age = user.age();",
            "render_user(name, age);",
        ];

        assert_eq!(
            align_changed_lines(&old, &new),
            vec![
                LineAlignment::NewOnly(0),
                LineAlignment::Pair { old: 0, new: 1 },
                LineAlignment::Pair { old: 1, new: 2 },
                LineAlignment::Pair { old: 2, new: 3 },
            ]
        );
    }

    #[test]
    fn unrelated_alignment_keeps_delete_before_insert() {
        let old = vec!["abcdefgh"];
        let new = vec!["12345678"];

        assert_eq!(
            align_changed_lines(&old, &new),
            vec![LineAlignment::OldOnly(0), LineAlignment::NewOnly(0)]
        );
    }

    #[test]
    fn unrelated_pairs_fall_back_to_full_line_highlight() {
        let (old, new) = inline_highlights_for_pair("abcdefgh", "12345678");
        assert_eq!((old[0].start, old[0].end), (0, 8));
        assert_eq!((new[0].start, new[0].end), (0, 8));
    }
}
