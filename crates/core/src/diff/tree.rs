//! Compact file-tree projection. Folder changes move node indices, never rendered
//! strings or syntax spans; only viewport rows are formatted and styled.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::{Arc, Mutex};

use smelt_buffer::buffer::{Span, SpanMeta};
use smelt_buffer::cell_width::text_width;
use smelt_buffer::document::{DocumentRow, DocumentSnapshot, DocumentViewport, RowSource};
use smelt_buffer::theme::{intern, HlGroup};

use super::{status_group, Diff, Section};
use crate::content::width::truncate_to_cells;

#[derive(Debug)]
struct Node {
    section: Section,
    group: bool,
    separator: bool,
    status: char,
    binary: bool,
    path: String,
    label: String,
    depth: usize,
    parent: Option<usize>,
    file: Option<usize>,
    first: usize,
    end: usize,
}

impl Node {
    fn is_directory(&self) -> bool {
        !self.group && !self.separator && self.file.is_none()
    }
}

#[derive(Debug)]
struct Counts {
    added: String,
    deleted: String,
    short_added: String,
    short_deleted: String,
}

impl Counts {
    fn new(added: u64, deleted: u64) -> Self {
        Self {
            added: if added == 0 {
                String::new()
            } else {
                format!("+{added}")
            },
            deleted: if deleted == 0 {
                String::new()
            } else {
                format!("-{deleted}")
            },
            short_added: if added == 0 {
                String::new()
            } else {
                format!("+{}", compact(added))
            },
            short_deleted: if deleted == 0 {
                String::new()
            } else {
                format!("-{}", compact(deleted))
            },
        }
    }
}

#[derive(Debug)]
struct State {
    visible: Arc<Vec<usize>>,
    collapsed: HashSet<usize>,
    width: usize,
    generation: u64,
}

impl State {
    fn reindex(&mut self, nodes: &[Node]) {
        let visible = Arc::make_mut(&mut self.visible);
        visible.clear();
        let mut index = 0;
        while index < nodes.len() {
            let node = &nodes[index];
            visible.push(index);
            index = if self.collapsed.contains(&index) {
                node.end
            } else {
                index + 1
            };
        }
        self.generation += 1;
    }

    fn row(&self, node: usize) -> Option<u64> {
        self.visible.binary_search(&node).ok().map(|row| row as u64)
    }
}

pub struct TreeNode {
    pub section: Section,
    pub group: bool,
    pub children: bool,
    pub file: Option<usize>,
    pub first: usize,
    pub parent: Option<u64>,
    pub expanded: bool,
}

/// Immutable directory and count index, built with the snapshot off-thread.
#[derive(Debug)]
pub struct TreeIndex {
    nodes: Vec<Node>,
    files: Vec<usize>,
    groups: [usize; 2],
    counts: Vec<Counts>,
    visible: Arc<Vec<usize>>,
}

impl TreeIndex {
    pub fn new(diff: &Diff) -> Self {
        let mut nodes: Vec<Node> = Vec::new();
        let mut files = Vec::with_capacity(diff.files.len());
        let mut groups = [0; 2];
        let mut counts: Vec<_> = diff
            .files
            .iter()
            .map(|file| Counts::new(file.additions, file.deletions))
            .collect();
        for section in [Section::Unstaged, Section::Staged] {
            let start = diff.files.partition_point(|file| file.section < section);
            let end = diff.files.partition_point(|file| file.section <= section);
            if section == Section::Staged {
                nodes.push(Node {
                    section,
                    group: false,
                    separator: true,
                    status: ' ',
                    binary: false,
                    path: String::new(),
                    label: String::new(),
                    depth: 0,
                    parent: None,
                    file: None,
                    first: start,
                    end: nodes.len() + 1,
                });
            }
            let group = nodes.len();
            groups[section as usize] = group;
            nodes.push(Node {
                section,
                group: true,
                separator: false,
                status: ' ',
                binary: false,
                path: String::new(),
                label: format!("{} ({})", section.name(), end - start),
                depth: 0,
                parent: None,
                file: None,
                first: start,
                end: 0,
            });
            let (mut added, mut deleted) = (0, 0);
            let mut directories = HashMap::new();
            for (file, entry) in diff.files.iter().enumerate().take(end).skip(start) {
                added += entry.additions;
                deleted += entry.deletions;
                let mut parent = Some(group);
                let mut path = Vec::new();
                let mut parts = entry.raw_path.split(|byte| *byte == b'/').peekable();
                let mut depth = 1;
                while let Some(part) = parts.next() {
                    if !path.is_empty() {
                        path.push(b'/');
                    }
                    path.extend_from_slice(part);
                    if parts.peek().is_some() {
                        let node = *directories.entry(path.clone()).or_insert_with(|| {
                            let node = nodes.len();
                            nodes.push(Node {
                                section,
                                group: false,
                                separator: false,
                                status: ' ',
                                binary: false,
                                path: format!("{}\0{:x?}", section.name(), path),
                                label: label(&String::from_utf8_lossy(part)),
                                depth,
                                parent,
                                file: None,
                                first: file,
                                end: 0,
                            });
                            node
                        });
                        parent = Some(node);
                    } else {
                        files.push(nodes.len());
                        nodes.push(Node {
                            section,
                            group: false,
                            separator: false,
                            status: entry.status.chars().next().unwrap_or(' '),
                            binary: entry.binary,
                            path: String::new(),
                            label: label(&String::from_utf8_lossy(part)),
                            depth,
                            parent,
                            file: Some(file),
                            first: file,
                            end: nodes.len() + 1,
                        });
                        let mut ancestor = parent;
                        while let Some(index) = ancestor {
                            nodes[index].end = nodes.len();
                            ancestor = nodes[index].parent;
                        }
                    }
                    depth += 1;
                }
            }
            nodes[group].end = nodes.len();
            counts.push(Counts::new(added, deleted));
        }
        Self {
            visible: Arc::new((0..nodes.len()).collect()),
            nodes,
            files,
            groups,
            counts,
        }
    }
}

/// Independent folds and fallback width over a shared immutable tree index.
#[derive(Debug)]
pub struct DiffTree {
    index: Arc<TreeIndex>,
    state: Mutex<State>,
}

impl DiffTree {
    pub fn new(diff: &Diff) -> Self {
        Self::from_index(Arc::new(TreeIndex::new(diff)))
    }

    pub fn from_index(index: Arc<TreeIndex>) -> Self {
        let state = State {
            visible: Arc::clone(&index.visible),
            collapsed: HashSet::new(),
            width: 30,
            generation: 0,
        };
        Self {
            index,
            state: Mutex::new(state),
        }
    }

    pub fn restore_collapsed(&self, paths: &[String]) {
        let paths: HashSet<_> = paths.iter().collect();
        let mut state = self.state.lock().unwrap();
        if paths.is_empty() && state.collapsed.is_empty() {
            return;
        }
        let collapsed = self
            .index
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, node)| !node.path.is_empty() && paths.contains(&node.path))
            .map(|(index, _)| index)
            .collect();
        if state.collapsed != collapsed {
            state.collapsed = collapsed;
            state.reindex(&self.index.nodes);
        }
    }

    pub fn section_row(&self, section: Section) -> u64 {
        self.state
            .lock()
            .unwrap()
            .row(self.index.groups[section as usize])
            .unwrap()
    }

    pub fn node(&self, row: u64) -> Option<TreeNode> {
        let state = self.state.lock().unwrap();
        let index = *state.visible.get(usize::try_from(row).ok()?)?;
        let node = &self.index.nodes[index];
        if node.separator {
            return None;
        }
        Some(TreeNode {
            section: node.section,
            group: node.group,
            children: node.is_directory(),
            file: node.file,
            first: node.first,
            parent: node.parent.and_then(|index| state.row(index)),
            expanded: !state.collapsed.contains(&index),
        })
    }

    pub fn file_row(&self, file: usize, reveal: bool) -> Option<u64> {
        let index = *self.index.files.get(file)?;
        let mut state = self.state.lock().unwrap();
        if reveal {
            let mut parent = self.index.nodes[index].parent;
            let mut changed = false;
            while let Some(index) = parent {
                changed |= state.collapsed.remove(&index);
                parent = self.index.nodes[index].parent;
            }
            if changed {
                state.reindex(&self.index.nodes);
            }
        }
        state.row(index)
    }

    pub fn toggle(&self, row: u64) -> Option<u64> {
        let mut state = self.state.lock().unwrap();
        let index = *state.visible.get(usize::try_from(row).ok()?)?;
        if !self.index.nodes[index].is_directory() {
            return None;
        }
        if !state.collapsed.remove(&index) {
            state.collapsed.insert(index);
        }
        state.reindex(&self.index.nodes);
        state.row(index)
    }

    pub fn collapsed(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap()
            .collapsed
            .iter()
            .map(|index| self.index.nodes[*index].path.clone())
            .collect()
    }

    pub fn set_width(&self, width: usize) {
        let mut state = self.state.lock().unwrap();
        let width = width.min(u16::MAX as usize);
        if state.width != width {
            state.width = width;
            state.generation += 1;
        }
    }

    fn render(&self, index: usize, state: &State, width: usize) -> DocumentRow {
        let node = &self.index.nodes[index];
        let arrow = if state.collapsed.contains(&index) {
            "▶"
        } else {
            "▼"
        };
        if node.separator {
            return DocumentRow {
                text: " ".repeat(width),
                ..Default::default()
            };
        }
        let depth = node.depth.saturating_sub(1);
        if node.is_directory() {
            let indent = " ".repeat((depth * 2).min(width));
            return DocumentRow {
                text: fit(&format!("{indent}{arrow} {}/", node.label), width),
                spans: vec![span(0, width, intern("Comment"))],
                ..Default::default()
            };
        }
        let counts = &self.index.counts[node
            .file
            .unwrap_or(self.index.files.len() + node.section as usize)];
        let count_width = |added: &str, deleted: &str| {
            [added, deleted]
                .into_iter()
                .filter(|count| !count.is_empty())
                .map(|count| count.len() + 1)
                .sum::<usize>()
        };
        let name = &node.label;
        let prefix_width = if node.group { 0 } else { depth * 2 + 2 };
        let available = width.saturating_sub(prefix_width + text_width(name));
        let (added, deleted) = if count_width(&counts.added, &counts.deleted) > available {
            (&counts.short_added, &counts.short_deleted)
        } else {
            (&counts.added, &counts.deleted)
        };
        let suffix = if node.binary {
            7
        } else {
            count_width(added, deleted)
        };
        let counts_fit = suffix < width;
        let label_width = if counts_fit { width - suffix } else { width };
        let indent = " ".repeat((depth * 2).min(label_width.saturating_sub(5)));
        let prefix = if node.group {
            String::new()
        } else {
            format!("{indent}{} ", node.status)
        };
        let mut row = DocumentRow {
            text: truncate_to_cells(&format!("{prefix}{name}"), label_width, "…"),
            ..Default::default()
        };
        let mut col = text_width(&row.text);
        if counts_fit {
            if node.binary {
                row.text.push_str(" binary");
                row.spans.push(span(col + 1, col + 7, intern("Comment")));
                col += 7;
            } else {
                for (count, group) in [
                    (added, "SmeltDiffAddCount"),
                    (deleted, "SmeltDiffDeleteCount"),
                ] {
                    if !count.is_empty() {
                        row.text.push(' ');
                        row.text.push_str(count);
                        row.spans
                            .push(span(col + 1, col + 1 + count.len(), intern(group)));
                        col += 1 + count.len();
                    }
                }
            }
        }
        row.text
            .extend(std::iter::repeat_n(' ', width.saturating_sub(col)));
        if !node.group && indent.len() < label_width.saturating_sub(1) {
            row.spans.push(span(
                indent.len(),
                indent.len() + 1,
                intern(status_group(node.status)),
            ));
        }
        row
    }
}

impl RowSource for DiffTree {
    fn snapshot(&self) -> DocumentSnapshot {
        let state = self.state.lock().unwrap();
        DocumentSnapshot {
            generation: state.generation,
            total_rows: state.visible.len() as u64,
        }
    }

    fn rows(&self, range: Range<u64>, _theme: &crate::theme::Theme) -> Vec<DocumentRow> {
        self.rows_at(range, None)
    }

    fn viewport_rows(
        &self,
        viewport: &DocumentViewport,
        _theme: &crate::theme::Theme,
    ) -> Vec<DocumentRow> {
        self.rows_at(viewport.rows.clone(), Some(usize::from(viewport.width)))
    }
}

impl DiffTree {
    fn rows_at(&self, range: Range<u64>, width: Option<usize>) -> Vec<DocumentRow> {
        let state = self.state.lock().unwrap();
        let width = width.unwrap_or(state.width);
        let start = range.start.min(state.visible.len() as u64) as usize;
        let end = range.end.min(state.visible.len() as u64) as usize;
        state
            .visible
            .get(start..end)
            .unwrap_or_default()
            .iter()
            .map(|index| self.render(*index, &state, width))
            .collect()
    }
}

fn span(start: usize, end: usize, hl: HlGroup) -> Span {
    Span {
        col_start: start as u16,
        col_end: end as u16,
        hl,
        meta: SpanMeta::default(),
        hl_eol: false,
        on_cursor_row: false,
    }
}

fn fit(text: &str, width: usize) -> String {
    let mut text = truncate_to_cells(text, width, "…");
    text.extend(std::iter::repeat_n(
        ' ',
        width.saturating_sub(text_width(&text)),
    ));
    text
}

fn label(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_control() {
            out.extend(ch.escape_default());
        } else {
            out.push(ch);
        }
    }
    out
}

fn compact(n: u64) -> String {
    if n < 1000 {
        return n.to_string();
    }
    let (scale, suffix) = if n >= 1_000_000 {
        (1_000_000., "m")
    } else {
        (1000., "k")
    };
    let number = format!("{:.1}", n as f64 / scale);
    format!("{}{suffix}", number.strip_suffix(".0").unwrap_or(&number))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Diff {
        let mut diff = Diff::parse(
            ["README", "src/z.rs", "src/nested/界.rs", "tests/z.rs"]
                .iter()
                .map(|path| format!("diff --git a/{path} b/{path}\n@@ -1 +1 @@\n-old\n+new\n"))
                .collect(),
        );
        diff.files[0].additions = 1_000_000;
        diff.files[0].deletions = 12_340;
        diff
    }

    #[test]
    fn tree_folds_reveal_ancestors_and_preserve_unrelated_collapsed_directories() {
        let diff = fixture();
        let tree = DiffTree::new(&diff);
        assert_eq!(tree.snapshot().total_rows, 10);
        assert_eq!(tree.file_row(0, false), Some(3));
        assert_eq!(tree.node(3).unwrap().parent, Some(2));
        assert_eq!(tree.toggle(5), Some(5));
        let tests_fold = tree.collapsed();
        assert_eq!(tree.toggle(2), Some(2));
        assert_eq!(tree.toggle(1), Some(1));
        assert_eq!(tree.snapshot().total_rows, 6);
        assert!(tree.file_row(0, false).is_none());
        assert_eq!(tree.file_row(0, true), Some(3));
        assert_eq!(tree.collapsed(), tests_fold);
        assert_eq!(tree.node(5).unwrap().first, 2);
        assert_eq!(tree.node(4).unwrap().file, Some(1));
        let restored = DiffTree::new(&diff);
        restored.restore_collapsed(&tree.collapsed());
        assert_eq!(restored.snapshot().total_rows, tree.snapshot().total_rows);
        assert!(tree.node(u64::MAX).is_none());
        assert!(tree.toggle(3).is_none());
        assert!(tree.toggle(u64::MAX).is_none());
        assert!(tree.file_row(usize::MAX, true).is_none());
        assert!(tree
            .rows(Range { start: 5, end: 2 }, &crate::theme::Theme::default())
            .is_empty());
    }

    #[test]
    fn sections_are_fixed_and_separated() {
        let mut staged = fixture();
        for file in &mut staged.files {
            file.section = Section::Staged;
        }
        for diff in [fixture(), staged, Diff::parse(String::new())] {
            let tree = DiffTree::new(&diff);
            let total = tree.snapshot().total_rows;
            tree.restore_collapsed(&["unstaged".into(), "staged".into()]);
            assert!(tree.collapsed().is_empty());
            for section in [Section::Unstaged, Section::Staged] {
                let row = tree.section_row(section);
                let node = tree.node(row).unwrap();
                assert!(node.group && node.expanded && !node.children);
                assert_eq!(tree.toggle(row), None);
                assert!(tree.rows(row..row + 1, &crate::theme::Theme::default())[0]
                    .text
                    .starts_with(section.name()));
            }
            let gap = tree.section_row(Section::Staged) - 1;
            assert!(tree.rows(gap..gap + 1, &crate::theme::Theme::default())[0]
                .text
                .trim()
                .is_empty());
            assert!(tree.node(gap).is_none());
            assert!(tree.toggle(gap).is_none());
            assert_eq!(tree.snapshot().total_rows, total);
            assert!(!tree
                .rows(0..total, &crate::theme::Theme::default())
                .iter()
                .any(|row| matches!(row.text.trim(), "empty" | "all staged")));
            if diff.files.is_empty() {
                assert_eq!(total, 3);
            }
        }
    }

    #[test]
    fn tree_filenames_and_spans_fit_unicode_cell_widths() {
        for path in [
            "crates/core/src/diff/tree.rs",
            "界界/e\u{301}.rs",
            "src/👩\u{200d}💻.rs",
            "README",
        ] {
            let diff = Diff::parse(format!(
                "diff --git a/{path} b/{path}\n@@ -1 +1 @@\n-old\n+new\n"
            ));
            let tree = DiffTree::new(&diff);
            let file_row = tree.file_row(0, false).unwrap();
            let name = path.rsplit('/').next().unwrap();
            for width in 0..70 {
                tree.set_width(width);
                for row in tree.rows(0..100, &crate::theme::Theme::default()) {
                    assert_eq!(text_width(&row.text), width, "{width}: {}", row.text);
                    assert!(row.spans.iter().all(
                        |span| span.col_start <= span.col_end && span.col_end as usize <= width
                    ));
                }
                if width < text_width(name) + (file_row as usize - 1) * 2 + 8 {
                    continue;
                }
                let row = &tree.rows(file_row..file_row + 1, &crate::theme::Theme::default())[0];
                assert!(row.text.contains(name), "{width}: {}", row.text);
                assert!(row.text.trim_end().ends_with(" +1 -1"));
            }
        }
    }

    #[test]
    fn tree_counts_follow_each_label_without_shared_columns() {
        let tree = DiffTree::new(&fixture());
        for width in [30, 40, 60] {
            tree.set_width(width);
            for (index, label, added, deleted) in [
                (0, "unstaged (4)", "+1000003", "-12343"),
                (3, "    M 界.rs", "+1000000", "-12340"),
                (4, "  M z.rs", "+1", "-1"),
                (7, "M README", "+1", "-1"),
            ] {
                let row = &tree.rows(index..index + 1, &crate::theme::Theme::default())[0];
                assert_eq!(row.text.trim_end(), format!("{label} {added} {deleted}"));
                let col = text_width(label) + 1;
                assert_eq!(
                    (row.spans[0].col_start, row.spans[0].col_end),
                    (col as u16, (col + added.len()) as u16)
                );
                let col = col + added.len() + 1;
                assert_eq!(
                    (row.spans[1].col_start, row.spans[1].col_end),
                    (col as u16, (col + deleted.len()) as u16)
                );
            }
        }
    }

    #[test]
    fn tree_single_counts_and_binary_labels_follow_filenames() {
        for (path, added, deleted, binary, suffix) in [
            ("add.rs", 123, 0, false, " +123"),
            ("delete.rs", 0, 12, false, " -12"),
            ("empty.rs", 0, 0, false, ""),
            ("界.png", 0, 0, true, " binary"),
        ] {
            let mut diff = Diff::parse(format!(
                "diff --git a/{path} b/{path}\n@@ -1 +1 @@\n-old\n+new\n"
            ));
            diff.files[0].additions = added;
            diff.files[0].deletions = deleted;
            diff.files[0].binary = binary;
            let tree = DiffTree::new(&diff);
            for width in 0..=50 {
                tree.set_width(width);
                for row in tree.rows(0..100, &crate::theme::Theme::default()) {
                    assert_eq!(text_width(&row.text), width, "{width}: {}", row.text);
                    assert!(row.spans.iter().all(|span| span.col_end as usize <= width));
                }
            }
            assert_eq!(
                tree.rows(1..2, &crate::theme::Theme::default())[0]
                    .text
                    .trim_end(),
                format!("M {path}{suffix}")
            );
        }
    }

    #[test]
    fn tree_rows_fit_narrow_unicode_names_large_counts_and_group_totals() {
        let tree = DiffTree::new(&fixture());
        for width in 0..50 {
            tree.set_width(width);
            for row in tree.rows(0..100, &crate::theme::Theme::default()) {
                assert_eq!(text_width(&row.text), width, "{width}: {}", row.text);
                assert!(row.spans.iter().all(|span| span.col_end as usize <= width));
            }
        }
        tree.set_width(20);
        let row = &tree.rows(3..4, &crate::theme::Theme::default())[0];
        assert!(row.text.ends_with("+1m -12.3k"), "{}", row.text);
        assert_eq!(row.spans[0].hl, intern("SmeltDiffAddCount"));
        assert_eq!(row.spans[1].hl, intern("SmeltDiffDeleteCount"));
        let generation = tree.snapshot().generation;
        tree.set_width(20);
        assert_eq!(tree.snapshot().generation, generation);
        tree.set_width(40);
        assert!(tree.snapshot().generation > generation);
        assert!(tree.rows(3..4, &crate::theme::Theme::default())[0]
            .text
            .contains("M 界.rs"));
        let group = &tree.rows(0..1, &crate::theme::Theme::default())[0].text;
        assert!(
            group.contains("unstaged (4)") && group.contains("+1000003 -12343"),
            "{group}"
        );
        assert_eq!(label("line\t\n\u{1b}"), "line\\t\\n\\u{1b}");
    }
}
