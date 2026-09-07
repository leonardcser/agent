//! Viewport-only inline comparisons. Replacement runs are indexed once, so a
//! seek into a million-line replacement never walks or aligns the whole run.

use std::ops::Range;
use std::time::{Duration, Instant};

use super::{display_text, Diff, Kind};
use crate::content::highlight::diff::diff_inline::{
    align_changed_lines_deadline, inline_highlights_for_pair_deadline, LineAlignment,
    MAX_LINE_ALIGNMENT_LINES,
};
use crate::content::highlight::diff::DiffByteRange;

const ALIGN_BYTES: usize = 8192;
const PAIR_BYTES: usize = 8192;
const WORK_BUDGET: Duration = Duration::from_millis(5);

#[derive(Debug)]
pub(super) struct ChangeBlock {
    old: Range<usize>,
    new: Range<usize>,
}

#[derive(Debug)]
pub(super) struct InlineRow {
    pub raw: usize,
    pub ranges: Vec<DiffByteRange>,
}

impl Diff {
    pub(super) fn index_changes(&mut self, cancelled: &impl Fn() -> bool) -> Option<()> {
        self.changes.clear();
        let mut old: Option<Range<usize>> = None;
        let mut new: Option<Range<usize>> = None;
        for raw in 0..=self.lines.len() {
            if raw.is_multiple_of(1024) && cancelled() {
                return None;
            }
            let kind = self.lines.get(raw).map(|line| line.kind);
            if kind == Some(Kind::Meta) && self.source(raw, false).starts_with("\\ No newline") {
                continue;
            }
            if !matches!(kind, Some(Kind::Add | Kind::Delete))
                || (kind == Some(Kind::Delete) && new.is_some())
            {
                if let (Some(old), Some(new)) = (old.take(), new.take()) {
                    self.changes.push(ChangeBlock { old, new });
                }
            }
            match kind {
                Some(Kind::Delete) => old.get_or_insert(raw..raw).end = raw + 1,
                Some(Kind::Add) if old.is_some() => new.get_or_insert(raw..raw).end = raw + 1,
                _ => {}
            }
        }
        Some(())
    }
}

pub(super) fn rows(
    diff: &Diff,
    wanted: Range<usize>,
    cancelled: impl Fn() -> bool,
) -> Option<Vec<InlineRow>> {
    let first = diff
        .changes
        .partition_point(|block| block.new.end <= wanted.start);
    let mut rows = Vec::new();
    for block in diff.changes[first..]
        .iter()
        .take_while(|block| block.old.start < wanted.end)
    {
        if cancelled() {
            return None;
        }
        let small = block.old.len() + block.new.len() <= MAX_LINE_ALIGNMENT_LINES
            && block
                .old
                .clone()
                .chain(block.new.clone())
                .map(|raw| diff.lines[raw].end - diff.lines[raw].start)
                .sum::<usize>()
                <= ALIGN_BYTES;
        let pairs: Vec<_> = if small {
            let old: Vec<_> = block
                .old
                .clone()
                .map(|raw| display_text(diff.source(raw, false)))
                .collect();
            let new: Vec<_> = block
                .new
                .clone()
                .map(|raw| display_text(diff.source(raw, false)))
                .collect();
            align_changed_lines_deadline(
                &old.iter().map(String::as_str).collect::<Vec<_>>(),
                &new.iter().map(String::as_str).collect::<Vec<_>>(),
                Some(Instant::now() + WORK_BUDGET),
            )
            .into_iter()
            .map(|pair| match pair {
                LineAlignment::Pair { old, new } => {
                    (Some(block.old.start + old), Some(block.new.start + new))
                }
                LineAlignment::OldOnly(old) => (Some(block.old.start + old), None),
                LineAlignment::NewOnly(new) => (None, Some(block.new.start + new)),
            })
            .collect()
        } else {
            // Large runs use positional pairing, but allocate only viewport pairs.
            (block.old.start.max(wanted.start)..block.new.end.min(wanted.end))
                .filter_map(|raw| {
                    if block.old.contains(&raw) {
                        let new = block.new.start + raw - block.old.start;
                        Some((Some(raw), block.new.contains(&new).then_some(new)))
                    } else if block.new.contains(&raw) {
                        let old = block.old.start + raw - block.new.start;
                        let old = block.old.contains(&old).then_some(old);
                        if old.is_some_and(|old| wanted.contains(&old)) {
                            None
                        } else {
                            Some((old, Some(raw)))
                        }
                    } else {
                        None
                    }
                })
                .collect()
        };
        for (old, new) in pairs {
            if cancelled() {
                return None;
            }
            if !old.is_some_and(|raw| wanted.contains(&raw))
                && !new.is_some_and(|raw| wanted.contains(&raw))
            {
                continue;
            }
            let old_text = old.map_or("", |raw| diff.source(raw, false));
            let new_text = new.map_or("", |raw| diff.source(raw, false));
            // Extremely long lines retain their row emphasis. Bound both the
            // comparison input and its deadline, not just the number of rows.
            if old_text.len() + new_text.len() > PAIR_BYTES {
                continue;
            }
            let (old_ranges, new_ranges) = inline_highlights_for_pair_deadline(
                &display_text(old_text),
                &display_text(new_text),
                Instant::now() + WORK_BUDGET,
            );
            for (raw, ranges) in [(old, old_ranges), (new, new_ranges)] {
                if let Some(raw) = raw.filter(|raw| wanted.contains(raw)) {
                    if !ranges.is_empty() {
                        rows.push(InlineRow { raw, ranges });
                    }
                }
            }
        }
    }
    rows.sort_unstable_by_key(|row| row.raw);
    Some(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_alignment_skips_inserted_lines_and_pairs_across_no_newline_markers() {
        let diff = Diff::parse("diff --git a/main.rs b/main.rs\n@@ -1,2 +1,3 @@\n-let count = 41;\n-return count;\n+log!(\"start\");\n+let count = 42;\n+return count;\n".into());
        let actual = rows(&diff, 0..7, || false).unwrap();
        assert_eq!(
            actual.iter().map(|row| row.raw).collect::<Vec<_>>(),
            [2, 4, 5]
        );
        let (old, new) = inline_highlights_for_pair_deadline(
            "let count = 41;",
            "let count = 42;",
            Instant::now() + Duration::from_secs(1),
        );
        assert_eq!(actual[0].ranges, old);
        assert_eq!(actual[2].ranges, new);
        let diff = Diff::parse("diff --git a/main.rs b/main.rs\n@@ -1 +1 @@\n-let count = 41;\n\\ No newline at end of file\n+let count = 42;\n\\ No newline at end of file\n".into());
        let actual = rows(&diff, 0..6, || false).unwrap();
        assert_eq!(actual.iter().map(|row| row.raw).collect::<Vec<_>>(), [2, 4]);
        assert_eq!(actual[0].ranges, old);
        assert_eq!(actual[1].ranges, new);
    }

    #[test]
    fn million_line_replacements_only_compare_requested_pairs_and_can_cancel() {
        let count = 500_000;
        let patch = format!("diff --git a/main.rs b/main.rs\n@@ -1,{count} +1,{count} @@\n")
            + &"-let answer = old_value;\n".repeat(count)
            + &"+let answer = new_value;\n".repeat(count);
        let diff = Diff::parse(patch);
        assert_eq!(diff.changes.len(), 1);
        for wanted in [2..42, count - 10..count + 30, 2 * count - 38..2 * count + 2] {
            let calls = std::cell::Cell::new(0);
            let actual = rows(&diff, wanted.clone(), || {
                calls.set(calls.get() + 1);
                false
            })
            .unwrap();
            assert_eq!(actual.len(), wanted.len());
            assert!(actual.iter().all(|row| wanted.contains(&row.raw)));
            assert!(
                calls.get() <= wanted.len() + 1,
                "offscreen pair comparisons: {}",
                calls.get()
            );
        }
        assert!(rows(&diff, 2..42, || true).is_none());
        assert!(rows(&diff, 0..2, || false).unwrap().is_empty());
    }

    #[test]
    fn very_long_lines_keep_row_emphasis_without_expensive_inline_comparisons() {
        let text = "界".repeat(PAIR_BYTES);
        let diff = Diff::parse(format!(
            "diff --git a/main.rs b/main.rs\n@@ -1 +1 @@\n-{text} old\n+{text} new\n"
        ));
        assert!(rows(&diff, 0..4, || false).unwrap().is_empty());
    }
}
