//! Compact unified-patch index with random-access, foldable row projections.
//! Patch text is stored once; only requested display rows are allocated/styled.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Arc, Mutex};

use crate::style::{Color, Style};
use smelt_buffer::buffer::{LineDecoration, SourceLine, Span, SpanMeta};
use smelt_buffer::document::{DocumentRow, DocumentSnapshot, DocumentViewport, RowSource, ViewId};
use smelt_buffer::theme::{intern, intern_anonymous_style, HlGroup, Theme};

mod inline;
mod syntax;
pub mod tree;
use syntax::{Chunk, SyntaxCache};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    File,
    Hunk,
    Context,
    Add,
    Delete,
    Meta,
}

#[derive(Clone, Copy, Debug)]
struct Line {
    start: usize,
    end: usize,
    old: u32,
    new: u32,
    kind: Kind,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Section {
    #[default]
    Unstaged,
    Staged,
}

impl Section {
    pub fn name(self) -> &'static str {
        match self {
            Self::Unstaged => "unstaged",
            Self::Staged => "staged",
        }
    }
}

#[derive(Clone, Debug)]
pub struct File {
    pub path: String,
    pub old_path: String,
    pub status: &'static str,
    pub additions: u64,
    pub deletions: u64,
    pub(crate) raw_path: Vec<u8>,
    pub(crate) raw_old_path: Vec<u8>,
    pub section: Section,
    pub binary: bool,
    start: usize,
    source_ordered: bool,
}

impl File {
    pub fn highlight_group(&self) -> &'static str {
        status_group(self.status.chars().next().unwrap_or(' '))
    }
}

fn status_group(status: char) -> &'static str {
    match status {
        'A' | '?' => "SmeltSuccess",
        'D' | 'U' => "ErrorMsg",
        'R' | 'C' => "SmeltProcess",
        'M' | 'T' => "WarningMsg",
        _ => "Normal",
    }
}

fn foreground(color: Color) -> HlGroup {
    intern_anonymous_style(Style {
        fg: Some(color),
        ..Default::default()
    })
}

#[derive(Debug)]
pub struct Diff {
    patch: String,
    lines: Vec<Line>,
    pub files: Vec<File>,
    hunks: Vec<usize>,
    changes: Vec<inline::ChangeBlock>,
    max_old: u32,
    max_new: u32,
}

impl Diff {
    pub fn parse(patch: String) -> Self {
        Self::parse_cancellable(patch, || false).expect("uncancelled diff index")
    }

    pub(crate) fn parse_cancellable(patch: String, cancelled: impl Fn() -> bool) -> Option<Self> {
        let boundary = patch.len();
        Self::parse_sections(patch, boundary, cancelled, |_, _| true)
    }

    pub(crate) fn parse_sections(
        patch: String,
        staged_start: usize,
        cancelled: impl Fn() -> bool,
        include_file: impl Fn(&[u8], usize) -> bool,
    ) -> Option<Self> {
        let mut lines = Vec::new();
        let mut files: Vec<File> = Vec::new();
        let mut hunks = Vec::new();
        let (mut max_old, mut max_new) = (0, 0);
        let (mut old, mut new) = (0u32, 0u32);
        let (mut old_left, mut new_left) = (0u32, 0u32);
        let mut offset = 0;
        let mut included = false;
        for (index, raw) in patch.split_inclusive('\n').enumerate() {
            if index.is_multiple_of(1024) && cancelled() {
                return None;
            }
            let text = raw.strip_suffix('\n').unwrap_or(raw);
            let start = offset;
            offset += raw.len();
            if text.starts_with("* Unmerged path ") {
                included = false;
                continue;
            }
            if let Some(header) = text.strip_prefix("diff --git ") {
                let (old_path, path) = header_paths(header);
                included = include_file(&path, start);
                if !included {
                    continue;
                }
                files.push(File {
                    path: String::from_utf8_lossy(&path).into_owned(),
                    old_path: String::from_utf8_lossy(&old_path).into_owned(),
                    raw_path: path,
                    raw_old_path: old_path,
                    section: if start < staged_start {
                        Section::Unstaged
                    } else {
                        Section::Staged
                    },
                    binary: false,
                    status: "M",
                    additions: 0,
                    deletions: 0,
                    start: lines.len(),
                    source_ordered: true,
                });
                lines.push(Line {
                    start,
                    end: start + text.len(),
                    old: 0,
                    new: 0,
                    kind: Kind::File,
                });
                old = 0;
                new = 0;
                old_left = 0;
                new_left = 0;
                continue;
            }
            if !included {
                continue;
            }
            let Some(file) = files.last_mut() else {
                continue;
            };
            if old_left > 0 || new_left > 0 {
                let kind = match text.as_bytes().first() {
                    Some(b' ') if old_left > 0 && new_left > 0 => Kind::Context,
                    Some(b'+') if new_left > 0 => Kind::Add,
                    Some(b'-') if old_left > 0 => Kind::Delete,
                    Some(b'\\') => Kind::Meta,
                    _ => {
                        old_left = 0;
                        new_left = 0;
                        Kind::Meta
                    }
                };
                if matches!(kind, Kind::Context | Kind::Add | Kind::Delete) {
                    if matches!(kind, Kind::Add | Kind::Delete)
                        && lines
                            .last()
                            .is_some_and(|line| matches!(line.kind, Kind::Context | Kind::Hunk))
                    {
                        hunks.push(lines.len());
                    }
                    lines.push(Line {
                        start: start + 1,
                        end: start + text.len(),
                        old,
                        new,
                        kind,
                    });
                    if kind != Kind::Add {
                        old = old.saturating_add(1);
                        old_left -= 1;
                    }
                    if kind != Kind::Delete {
                        new = new.saturating_add(1);
                        new_left -= 1;
                    }
                    if kind == Kind::Add {
                        file.additions += 1;
                    }
                    if kind == Kind::Delete {
                        file.deletions += 1;
                    }
                    continue;
                }
            }
            if let Some((a, b, c, d)) = hunk_range(text) {
                file.source_ordered &= a >= old && c >= new;
                (old, old_left, new, new_left) = (a, b, c, d);
                max_old = max_old.max(a.saturating_add(b).saturating_sub(1));
                max_new = max_new.max(c.saturating_add(d).saturating_sub(1));
                lines.push(Line {
                    start,
                    end: start + text.len(),
                    old,
                    new,
                    kind: Kind::Hunk,
                });
            } else if let Some(path) = text.strip_prefix("--- ") {
                let path = patch_path(path);
                if path != b"/dev/null" {
                    file.old_path = String::from_utf8_lossy(&path).into_owned();
                    file.raw_old_path = path;
                }
            } else if let Some(path) = text.strip_prefix("+++ ") {
                let path = patch_path(path);
                if path != b"/dev/null" {
                    file.path = String::from_utf8_lossy(&path).into_owned();
                    file.raw_path = path;
                }
            } else if text.starts_with("index ") || text.is_empty() {
                continue;
            } else {
                if text.starts_with("Binary files ") || text == "GIT binary patch" {
                    file.binary = true;
                }
                if text.starts_with("new file mode ") {
                    file.status = "A";
                }
                if text.starts_with("deleted file mode ") {
                    file.status = "D";
                }
                if let Some(path) = text.strip_prefix("rename from ") {
                    file.raw_old_path = unquote(path);
                    file.old_path = String::from_utf8_lossy(&file.raw_old_path).into_owned();
                    file.status = "R";
                }
                if let Some(path) = text.strip_prefix("rename to ") {
                    file.raw_path = unquote(path);
                    file.path = String::from_utf8_lossy(&file.raw_path).into_owned();
                    file.status = "R";
                }
                if text.starts_with("old mode ") && file.status == "M" {
                    file.status = "T";
                }
                lines.push(Line {
                    start,
                    end: start + text.len(),
                    old,
                    new,
                    kind: Kind::Meta,
                });
            }
        }
        let mut diff = Self {
            patch,
            lines,
            files,
            hunks,
            changes: Vec::new(),
            max_old,
            max_new,
        };
        diff.sort_files(&cancelled)?;
        Some(diff)
    }

    /// Keep unresolved paths visible even when the worktree matches the ours
    /// stage, or that stage is missing and Git emits no unified patch.
    pub(crate) fn add_unmerged_file(&mut self, path: Vec<u8>) {
        self.files.push(File {
            path: String::from_utf8_lossy(&path).into_owned(),
            old_path: String::from_utf8_lossy(&path).into_owned(),
            raw_old_path: path.clone(),
            raw_path: path,
            section: Section::Unstaged,
            status: "U",
            binary: false,
            additions: 0,
            deletions: 0,
            start: self.lines.len(),
            source_ordered: true,
        });
        for (kind, text) in [
            (Kind::File, ""),
            (Kind::Meta, "conflicted"),
            (Kind::Meta, "resolve conflicts, then stage"),
        ] {
            let start = self.patch.len();
            self.patch.push_str(text);
            self.lines.push(Line {
                start,
                end: self.patch.len(),
                old: 0,
                new: 0,
                kind,
            });
            self.patch.push('\n');
        }
    }

    /// Directory-first preorder shared by the preview and Lua tree. Only compact
    /// row indexes move; patch bytes and per-file source order remain untouched.
    pub(crate) fn sort_files(&mut self, cancelled: &impl Fn() -> bool) -> Option<()> {
        fn compare(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
            let (mut a, mut b) = (
                a.split(|b| *b == b'/').peekable(),
                b.split(|b| *b == b'/').peekable(),
            );
            while let (Some(left), Some(right)) = (a.next(), b.next()) {
                let order = b
                    .peek()
                    .is_some()
                    .cmp(&a.peek().is_some())
                    .then_with(|| left.cmp(right));
                if !order.is_eq() {
                    return order;
                }
            }
            std::cmp::Ordering::Equal
        }
        let mut order: Vec<_> = (0..self.files.len()).collect();
        order.sort_by(|a, b| {
            let (a, b) = (&self.files[*a], &self.files[*b]);
            a.section
                .cmp(&b.section)
                .then_with(|| compare(&a.raw_path, &b.raw_path))
        });
        if order.iter().copied().eq(0..order.len()) {
            return self.index_changes(cancelled);
        }
        let mut starts: Vec<_> = self.files.iter().map(|f| f.start).collect();
        starts.push(self.lines.len());
        let mut files: Vec<_> = std::mem::take(&mut self.files)
            .into_iter()
            .map(Some)
            .collect();
        let mut lines = Vec::with_capacity(self.lines.len());
        let mut hunks = Vec::with_capacity(self.hunks.len());
        for index in order {
            if cancelled() {
                return None;
            }
            let mut file = files[index].take().unwrap();
            let range = starts[index]..starts[index + 1];
            file.start = lines.len();
            let first = self.hunks.partition_point(|row| *row < range.start);
            hunks.extend(
                self.hunks[first..]
                    .iter()
                    .take_while(|row| **row < range.end)
                    .map(|row| file.start + row - range.start),
            );
            lines.extend_from_slice(&self.lines[range]);
            self.files.push(file);
        }
        self.lines = lines;
        self.hunks = hunks;
        self.index_changes(cancelled)
    }

    fn source(&self, raw: usize, newline: bool) -> &str {
        let line = self.lines[raw];
        let end =
            line.end + usize::from(newline && self.patch.as_bytes().get(line.end) == Some(&b'\n'));
        smelt_buffer::text::slice(&self.patch, line.start..end)
    }

    /// Retained patch and row indexes, excluding the bounded viewport syntax cache.
    pub fn indexed_bytes(&self) -> usize {
        self.patch.capacity()
            + self.lines.capacity() * std::mem::size_of::<Line>()
            + self.hunks.capacity() * std::mem::size_of::<usize>()
            + self.changes.capacity() * std::mem::size_of::<inline::ChangeBlock>()
    }
}

fn hunk_range(text: &str) -> Option<(u32, u32, u32, u32)> {
    let text = text.strip_prefix("@@ -")?;
    let (old, rest) = text.split_once(" +")?;
    let (new, _) = rest.split_once(" @@")?;
    fn range(s: &str) -> Option<(u32, u32)> {
        match s.split_once(',') {
            Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
            None => Some((s.parse().ok()?, 1)),
        }
    }
    let (a, b) = range(old)?;
    let (c, d) = range(new)?;
    Some((a, b, c, d))
}

fn header_paths(header: &str) -> (Vec<u8>, Vec<u8>) {
    if header.starts_with('"') {
        let mut escaped = false;
        for (i, c) in header.char_indices().skip(1) {
            if c == '"' && !escaped {
                let (a, b) = header.split_at(i + 1);
                return (strip_side(unquote(a)), strip_side(unquote(b.trim_start())));
            }
            escaped = c == '\\' && !escaped;
        }
    }
    // Git leaves spaces unquoted. For same-path headers, match both sides
    // before treating a literal " b/" inside the filename as the separator.
    if let Some(paths) = header.strip_prefix("a/") {
        for (index, _) in paths.match_indices(" b/") {
            let (a, b) = paths.split_at(index);
            if b.strip_prefix(" b/") == Some(a) {
                return (a.as_bytes().to_vec(), a.as_bytes().to_vec());
            }
        }
    }
    if let Some((a, b)) = header.rsplit_once(" b/") {
        return (strip_side(unquote(a)), b.as_bytes().to_vec());
    }
    (header.as_bytes().to_vec(), header.as_bytes().to_vec())
}

fn strip_side(path: Vec<u8>) -> Vec<u8> {
    path.strip_prefix(b"a/")
        .or_else(|| path.strip_prefix(b"b/"))
        .unwrap_or(&path)
        .to_vec()
}

fn patch_path(path: &str) -> Vec<u8> {
    strip_side(unquote(path.split('\t').next().unwrap_or(path)))
}

/// Decode Git's C-quoted paths, including octal-encoded UTF-8 bytes.
fn unquote(path: &str) -> Vec<u8> {
    let Some(path) = path.strip_prefix('"').and_then(|p| p.strip_suffix('"')) else {
        return path.as_bytes().to_vec();
    };
    let mut bytes = path.bytes().peekable();
    let mut out = Vec::new();
    while let Some(b) = bytes.next() {
        if b != b'\\' {
            out.push(b);
            continue;
        }
        let Some(b) = bytes.next() else { break };
        if (b'0'..=b'7').contains(&b) {
            let mut value = u16::from(b - b'0');
            for _ in 0..2 {
                if bytes.peek().is_some_and(|b| (b'0'..=b'7').contains(b)) {
                    value = value * 8 + u16::from(bytes.next().unwrap() - b'0');
                } else {
                    break;
                }
            }
            out.push(value as u8);
        } else {
            out.push(match b {
                b'n' => b'\n',
                b't' => b'\t',
                b'r' => b'\r',
                b'b' => 8,
                b'f' => 12,
                b'v' => 11,
                b'a' => 7,
                _ => b,
            });
        }
    }
    out
}

#[derive(Clone, Debug)]
struct Segment {
    raw: Range<usize>,
    start: u64,
    fold: bool,
    expanded: bool,
}

impl Segment {
    fn len(&self) -> u64 {
        if self.raw.is_empty() {
            2
        } else if self.fold {
            1 + if self.expanded {
                self.raw.len() as u64
            } else {
                0
            }
        } else {
            self.raw.len() as u64
        }
    }
}

#[derive(Clone, Debug, Default)]
struct Projection {
    segments: Vec<Segment>,
    generation: u64,
    total: u64,
}

impl Projection {
    fn reindex(&mut self) {
        let mut start = 0;
        for segment in &mut self.segments {
            segment.start = start;
            start += segment.len();
        }
        self.total = start;
        self.generation += 1;
    }
    fn segment(&self, row: u64) -> Option<&Segment> {
        self.segments.get(
            self.segments
                .partition_point(|segment| segment.start + segment.len() <= row),
        )
    }
    fn source_rows(&self, rows: Range<u64>) -> impl Iterator<Item = usize> + '_ {
        (rows.start..rows.end.min(self.total)).filter_map(|row| {
            let segment = self.segment(row)?;
            if segment.raw.is_empty() || (segment.fold && row == segment.start) {
                None
            } else {
                Some(segment.raw.start + (row - segment.start - u64::from(segment.fold)) as usize)
            }
        })
    }

    fn raw_row(&self, row: u64) -> Option<usize> {
        let segment = self.segment(row)?;
        let offset = if segment.raw.is_empty() {
            0
        } else if segment.fold {
            row.saturating_sub(segment.start + 1)
        } else {
            row - segment.start
        };
        Some(segment.raw.start + offset as usize)
    }
    fn display_row(&self, raw: usize) -> u64 {
        let index = self
            .segments
            .partition_point(|segment| segment.raw.end <= raw);
        self.segments.get(index).map_or(self.total, |segment| {
            segment.start
                + if segment.fold && !segment.expanded {
                    0
                } else {
                    (raw - segment.raw.start) as u64 + u64::from(segment.fold)
                }
        })
    }
}

#[derive(Debug)]
pub struct DiffView {
    pub diff: Arc<Diff>,
    tree: Arc<tree::TreeIndex>,
    context: usize,
    projection: Mutex<Projection>,
    syntax: Arc<SyntaxCache>,
}

impl DiffView {
    pub fn new(diff: Arc<Diff>, context: usize) -> Self {
        let mut projection = Projection::default();
        let mut begin = 0;
        let mut i = 0;
        while i < diff.lines.len() {
            if i > 0 && diff.lines[i].kind == Kind::File {
                if begin < i {
                    projection.segments.push(Segment {
                        raw: begin..i,
                        start: 0,
                        fold: false,
                        expanded: false,
                    });
                }
                projection.segments.push(Segment {
                    raw: i..i,
                    start: 0,
                    fold: false,
                    expanded: false,
                });
                begin = i;
            }
            if diff.lines[i].kind != Kind::Context {
                i += 1;
                continue;
            }
            let start = i;
            while i < diff.lines.len() && diff.lines[i].kind == Kind::Context {
                i += 1;
            }
            let keep = context.min((i - start) / 2);
            if i - start > keep.saturating_mul(2).saturating_add(1) {
                let fold = start + keep..i - keep;
                if begin < fold.start {
                    projection.segments.push(Segment {
                        raw: begin..fold.start,
                        start: 0,
                        fold: false,
                        expanded: false,
                    });
                }
                begin = fold.end;
                projection.segments.push(Segment {
                    raw: fold,
                    start: 0,
                    fold: true,
                    expanded: false,
                });
            }
        }
        if begin < diff.lines.len() {
            projection.segments.push(Segment {
                raw: begin..diff.lines.len(),
                start: 0,
                fold: false,
                expanded: false,
            });
        }
        projection.reindex();
        Self {
            syntax: Arc::new(SyntaxCache::new(Arc::clone(&diff))),
            tree: Arc::new(tree::TreeIndex::new(&diff)),
            context,
            diff,
            projection: Mutex::new(projection),
        }
    }

    pub fn context(&self) -> usize {
        self.context
    }

    /// Independent folds over the same immutable patch, tree index and syntax cache.
    pub fn fork(&self) -> Self {
        Self {
            diff: Arc::clone(&self.diff),
            tree: Arc::clone(&self.tree),
            context: self.context,
            projection: Mutex::new(self.projection.lock().unwrap().clone()),
            syntax: Arc::clone(&self.syntax),
        }
    }

    pub fn tree(&self) -> tree::DiffTree {
        tree::DiffTree::from_index(Arc::clone(&self.tree))
    }

    pub(crate) fn set_wakeup(&self, wakeup: Option<tokio::sync::mpsc::UnboundedSender<()>>) {
        self.syntax.set_wakeup(wakeup);
    }

    pub fn file_row(&self, index: usize) -> Option<u64> {
        Some(
            self.projection
                .lock()
                .unwrap()
                .display_row(self.diff.files.get(index)?.start),
        )
    }

    pub fn file_at(&self, row: u64) -> Option<usize> {
        let raw = self.projection.lock().unwrap().raw_row(row)?;
        self.diff
            .files
            .partition_point(|file| file.start <= raw)
            .checked_sub(1)
    }

    pub fn hunk(&self, row: u64, forward: bool) -> Option<u64> {
        let projection = self.projection.lock().unwrap();
        if forward {
            let index = self
                .diff
                .hunks
                .partition_point(|raw| projection.display_row(*raw) <= row);
            self.diff
                .hunks
                .get(index)
                .map(|raw| projection.display_row(*raw))
        } else {
            let index = self
                .diff
                .hunks
                .partition_point(|raw| projection.display_row(*raw) < row);
            index
                .checked_sub(1)
                .map(|index| projection.display_row(self.diff.hunks[index]))
        }
    }

    /// Toggle the fold containing `row`, returning its marker row for anchoring.
    pub fn toggle_fold(&self, row: u64) -> Option<u64> {
        let mut projection = self.projection.lock().unwrap();
        let index = projection
            .segments
            .partition_point(|segment| segment.start + segment.len() <= row);
        let segment = projection.segments.get_mut(index)?;
        if !segment.fold {
            return None;
        }
        segment.expanded = !segment.expanded;
        let row = segment.start;
        projection.reindex();
        Some(row)
    }

    /// Restore expanded source ranges and semantic cursor/viewport anchors from
    /// a previous snapshot. File identity and source line numbers survive changes
    /// in file ordering and fold sizes; raw display offsets do not.
    pub fn restore(&self, previous: &Self, cursor: u64, top: u64) -> (Option<u64>, Option<u64>) {
        if std::ptr::eq(self, previous) {
            return (Some(cursor), Some(top));
        }
        let (anchors, expanded) = {
            let projection = previous.projection.lock().unwrap();
            let anchors = [cursor, top].map(|row| {
                let segment = projection.segment(row)?;
                Some((
                    projection.raw_row(row)?,
                    segment.fold && row == segment.start,
                    if segment.raw.is_empty() {
                        segment.start + 2 - row
                    } else {
                        0
                    },
                ))
            });
            let expanded: Vec<_> = projection
                .segments
                .iter()
                .filter(|s| s.fold && s.expanded)
                .map(|s| s.raw.clone())
                .collect();
            (anchors, expanded)
        };
        let files = self
            .diff
            .files
            .iter()
            .enumerate()
            .map(|(index, file)| ((file.section, file.raw_path.as_slice()), index))
            .collect();
        let restore_raw = |raw| self.restore_raw(&previous.diff, &files, raw);
        let mut ranges: Vec<_> = expanded
            .into_iter()
            .filter_map(|range| {
                let restored = restore_raw(range.start)?..restore_raw(range.end - 1)? + 1;
                (!restored.is_empty()).then_some(restored)
            })
            .collect();
        ranges.sort_by_key(|range| range.start);
        let mut expanded: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
        for range in ranges {
            if let Some(last) = expanded.last_mut().filter(|last| last.end >= range.start) {
                last.end = last.end.max(range.end);
            } else {
                expanded.push(range);
            }
        }
        let mut projection = self.projection.lock().unwrap();
        let mut changed = false;
        for segment in &mut projection.segments {
            if !segment.fold {
                continue;
            }
            let index = expanded.partition_point(|range| range.end <= segment.raw.start);
            let open = expanded
                .get(index)
                .is_some_and(|range| range.start < segment.raw.end);
            changed |= segment.expanded != open;
            segment.expanded = open;
        }
        if changed {
            projection.reindex();
        }
        let [cursor, top] = anchors.map(|anchor| {
            let (raw, marker, separator) = anchor?;
            let raw = restore_raw(raw)?;
            let row = projection.display_row(raw);
            Some(if marker {
                projection
                    .segment(row)
                    .filter(|s| s.fold)
                    .map_or(row, |s| s.start)
            } else {
                row.saturating_sub(separator)
            })
        });
        (cursor, top)
    }

    fn restore_raw(
        &self,
        previous: &Diff,
        files: &HashMap<(Section, &[u8]), usize>,
        raw: usize,
    ) -> Option<usize> {
        let file = previous
            .files
            .partition_point(|file| file.start <= raw)
            .checked_sub(1)?;
        let old_file = &previous.files[file];
        let file = *files.get(&(old_file.section, old_file.raw_path.as_slice()))?;
        let start = self.diff.files[file].start;
        let end = self
            .diff
            .files
            .get(file + 1)
            .map_or(self.diff.lines.len(), |file| file.start);
        let source = previous.lines.get(raw)?;
        if !matches!(
            source.kind,
            Kind::Context | Kind::Add | Kind::Delete | Kind::Hunk
        ) {
            return Some((start + raw - old_file.start).min(end.saturating_sub(1)));
        }
        let old_side = source.kind != Kind::Add && source.old > 0;
        let line_number = |line: &Line| if old_side { line.old } else { line.new };
        let target = line_number(source);
        let accepts = |line: &Line| {
            if source.kind == Kind::Hunk {
                line.kind == Kind::Hunk
            } else {
                line.kind == Kind::Context
                    || line.kind == if old_side { Kind::Delete } else { Kind::Add }
            }
        };
        let lines = &self.diff.lines[start..end];
        let index = if self.diff.files[file].source_ordered {
            let lower = lines.partition_point(|line| line_number(line) < target);
            if source.kind == Kind::Hunk && lines.get(lower).is_some_and(accepts) {
                Some(lower)
            } else {
                let upper = lines.partition_point(|line| line_number(line) <= target);
                lines[..upper].iter().rposition(accepts).or_else(|| {
                    lines[upper..]
                        .iter()
                        .position(accepts)
                        .map(|index| upper + index)
                })
            }
        } else {
            // Arbitrary patches can contain overlapping or out-of-order hunks.
            // Only those files need a linear nearest-source fallback.
            lines
                .iter()
                .enumerate()
                .filter(|(_, line)| accepts(line))
                .min_by_key(|(index, line)| {
                    (
                        line_number(line).abs_diff(target),
                        line.kind != source.kind,
                        index.abs_diff(raw - old_file.start),
                    )
                })
                .map(|(index, _)| index)
        };
        Some(start + index.unwrap_or(0))
    }

    fn line(&self, raw: usize, syntax: Option<&Chunk>, theme: &Theme) -> DocumentRow {
        let line = self.diff.lines[raw];
        let source = self.diff.source(raw, false);
        let (text, group, prefix) = match line.kind {
            Kind::File => {
                let file =
                    &self.diff.files[self.diff.files.partition_point(|file| file.start <= raw) - 1];
                (
                    format!(
                        "{}  {}  {}",
                        file.status,
                        display_text(&file.path),
                        file.section.name()
                    ),
                    "Normal",
                    0,
                )
            }
            Kind::Add => (format!("+ {}", display_text(source)), "SmeltDiffAddBg", 2),
            Kind::Delete => (
                format!("- {}", display_text(source)),
                "SmeltDiffDeleteBg",
                2,
            ),
            Kind::Context => (format!("  {}", display_text(source)), "Normal", 2),
            Kind::Hunk | Kind::Meta => (display_text(source), "Comment", 0),
        };
        let mut row = styled_row(text, group, prefix);
        if matches!(line.kind, Kind::Add | Kind::Delete) {
            row.decoration.window_bg = theme.resolve(intern(group)).bg;
            row.spans[1].hl = foreground(if line.kind == Kind::Add {
                Color::Green
            } else {
                Color::Red
            });
        } else if line.kind == Kind::File {
            let file =
                &self.diff.files[self.diff.files.partition_point(|file| file.start <= raw) - 1];
            let section_start = row.text.len() - file.section.name().len();
            let mut spans = vec![
                (0, file.status.len(), intern(file.highlight_group())),
                (section_start, row.text.len(), intern("Comment")),
            ];
            for (count, sign, group) in [
                (file.additions, '+', "SmeltDiffAddCount"),
                (file.deletions, '-', "SmeltDiffDeleteCount"),
            ] {
                if count > 0 {
                    row.text.push(' ');
                    let start = row.text.len();
                    row.text.push_str(&format!("{sign}{count}"));
                    spans.push((start, row.text.len(), intern(group)));
                }
            }
            if file.binary {
                row.text.push_str("  binary");
                spans.push((row.text.len() - 6, row.text.len(), intern("Comment")));
            }
            for (start, end, hl) in spans {
                row.spans.push(Span {
                    col_start: smelt_buffer::text::byte_to_cell(&row.text, start)
                        .min(u16::MAX as usize) as u16,
                    col_end: smelt_buffer::text::byte_to_cell(&row.text, end).min(u16::MAX as usize)
                        as u16,
                    hl,
                    meta: SpanMeta::default(),
                    hl_eol: false,
                    on_cursor_row: false,
                });
            }
        }
        if let Some(syntax) = syntax.filter(|_| prefix > 0) {
            let colors = syntax.colors(theme);
            let background = match line.kind {
                Kind::Add => 1,
                Kind::Delete => 2,
                _ => 0,
            };
            let first = syntax.spans.partition_point(|span| span.end <= line.start);
            let (mut byte, mut source_byte) = (usize::from(prefix), line.start);
            for span in &syntax.spans[first..] {
                let end = span.end.min(line.end);
                let start_col = smelt_buffer::text::byte_to_cell(&row.text, byte);
                byte += display_text(smelt_buffer::text::slice(
                    &self.diff.patch,
                    source_byte..end,
                ))
                .len();
                let end_col = smelt_buffer::text::byte_to_cell(&row.text, byte);
                row.spans.push(Span {
                    col_start: start_col.min(u16::MAX as usize) as u16,
                    col_end: end_col.min(u16::MAX as usize) as u16,
                    hl: colors[span.scope][background],
                    meta: SpanMeta::default(),
                    hl_eol: false,
                    on_cursor_row: false,
                });
                source_byte = end;
                if end == line.end {
                    break;
                }
            }
            syntax.highlight_inline(
                raw,
                &mut row,
                if line.kind == Kind::Add {
                    "SmeltDiffAddInlineBg"
                } else {
                    "SmeltDiffDeleteInlineBg"
                },
                theme,
            );
        }
        row.decoration.source_line = Some(match line.kind {
            Kind::Add => SourceLine::Diff {
                old: None,
                new: Some(line.new),
            },
            Kind::Delete => SourceLine::Diff {
                old: Some(line.old),
                new: None,
            },
            Kind::Context => SourceLine::Diff {
                old: Some(line.old),
                new: Some(line.new),
            },
            _ => SourceLine::Synthetic,
        });
        row
    }
}

impl RowSource for DiffView {
    fn prepare(&self, view: ViewId, viewport: &DocumentViewport) {
        let projection = self.projection.lock().unwrap();
        let selected = projection
            .raw_row(viewport.cursor.unwrap_or(viewport.rows.start))
            .and_then(|raw| {
                self.diff
                    .files
                    .partition_point(|file| file.start <= raw)
                    .checked_sub(1)
            });
        self.syntax.prepare(
            view,
            &self.diff,
            projection.source_rows(viewport.rows.clone()),
            selected,
            viewport
                .rows
                .end
                .saturating_sub(viewport.rows.start)
                .min(usize::MAX as u64) as usize,
        );
    }

    fn release(&self, view: ViewId) {
        self.syntax.release(view);
    }

    fn line_number_bounds(&self) -> Option<SourceLine> {
        Some(SourceLine::Diff {
            old: Some(self.diff.max_old),
            new: Some(self.diff.max_new),
        })
    }

    fn snapshot(&self) -> DocumentSnapshot {
        let projection = self.projection.lock().unwrap();
        DocumentSnapshot {
            generation: projection.generation.wrapping_add(self.syntax.generation()),
            total_rows: projection.total,
        }
    }

    fn rows(&self, range: Range<u64>, theme: &Theme) -> Vec<DocumentRow> {
        let projection = self.projection.lock().unwrap();
        let end = range.end.min(projection.total);
        if range.start >= end {
            return Vec::new();
        }
        enum Row {
            Source(usize),
            Fold(bool, usize),
            Blank,
            Divider,
        }
        let rows: Vec<_> = (range.start..end)
            .map(|row| {
                let segment = projection.segment(row).unwrap();
                if segment.raw.is_empty() {
                    if row == segment.start {
                        Row::Blank
                    } else {
                        Row::Divider
                    }
                } else if segment.fold && row == segment.start {
                    Row::Fold(segment.expanded, segment.raw.len())
                } else {
                    let offset = row - segment.start - u64::from(segment.fold);
                    Row::Source(segment.raw.start + offset as usize)
                }
            })
            .collect();
        let chunks = self.syntax.cached(
            &self.diff,
            rows.iter().filter_map(|row| {
                if let Row::Source(raw) = row {
                    Some(*raw)
                } else {
                    None
                }
            }),
        );
        rows.into_iter()
            .map(|row| match row {
                Row::Source(raw) => {
                    let chunk = chunks
                        .range(..=raw)
                        .next_back()
                        .map(|(_, chunk)| chunk.as_ref())
                        .filter(|chunk| chunk.raw.contains(&raw));
                    self.line(raw, chunk, theme)
                }
                Row::Blank => DocumentRow::default(),
                Row::Divider => DocumentRow {
                    decoration: LineDecoration {
                        horizontal_rule: true,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                Row::Fold(expanded, count) => styled_row(
                    format!(
                        "{} {count} unchanged lines",
                        if expanded { "▼" } else { "▶" }
                    ),
                    "Comment",
                    0,
                ),
            })
            .collect()
    }

    fn is_pending(&self) -> bool {
        self.syntax.pending()
    }
}

fn styled_row(text: String, group: &str, prefix: u16) -> DocumentRow {
    let mut spans = vec![Span {
        col_start: 0,
        col_end: u16::MAX,
        hl: intern(group),
        meta: SpanMeta::default(),
        hl_eol: true,
        on_cursor_row: false,
    }];
    if prefix > 0 {
        spans.push(Span {
            col_start: 0,
            col_end: prefix,
            hl: intern("Comment"),
            meta: SpanMeta::unselectable(),
            hl_eol: false,
            on_cursor_row: false,
        });
    }
    DocumentRow {
        text,
        spans,
        decoration: LineDecoration {
            pre_formatted: true,
            ..Default::default()
        },
    }
}

/// Make terminal control characters visible rather than interpreting patch data.
fn display_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\t' => out.push_str("    "),
            ch if ch.is_control() => {
                out.extend(ch.escape_default());
            }
            ch => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    static THEME: std::sync::LazyLock<Theme> = std::sync::LazyLock::new(Theme::default);

    fn highlighted(view: &DiffView, range: Range<u64>) -> Vec<DocumentRow> {
        let subscription = smelt_buffer::document::RowView::new(Arc::new(view.fork()));
        subscription.prepare(&DocumentViewport {
            rows: range.clone(),
            width: 100,
            cursor: Some(range.start),
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while view.is_pending() {
            assert!(
                std::time::Instant::now() < deadline,
                "syntax worker stalled"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        view.rows(range, &THEME)
    }

    #[test]
    fn distant_views_keep_independent_demand_and_text_access_never_requests_syntax() {
        use smelt_buffer::document::RowView;
        let patch = "diff --git a/file.txt b/file.txt\n@@ -0,0 +1,30000 @@\n".to_string()
            + &"+plain text\n".repeat(30000);
        let original = Arc::new(DiffView::new(Arc::new(Diff::parse(patch)), 3));
        original.rows(12000..12500, &THEME);
        assert_eq!(original.syntax.stats(), (0, 0, 0));
        assert!(!original.is_pending());
        let independent = Arc::new(original.fork());
        let far = RowView::new(original.clone());
        let near = RowView::new(independent.clone());
        far.prepare(&DocumentViewport {
            rows: 29000..29040,
            width: 80,
            cursor: Some(29000),
        });
        near.prepare(&DocumentViewport {
            rows: 0..40,
            width: 80,
            cursor: Some(0),
        });
        let demand = independent.syntax.demands();
        assert_eq!(demand.0, 2);
        original.rows(8000..8500, &THEME);
        assert_eq!(
            independent.syntax.demands(),
            demand,
            "text access changed viewport demand"
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while independent.is_pending() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(original.rows(29000..29001, &THEME)[0].spans.len(), 3);
        assert_eq!(independent.rows(2..3, &THEME)[0].spans.len(), 3);
        drop(far);
        drop(original);
        assert_eq!(independent.syntax.demands().0, 1);
        near.prepare(&DocumentViewport {
            rows: 1000..1040,
            width: 80,
            cursor: Some(1000),
        });
        while independent.is_pending() {
            assert!(
                std::time::Instant::now() < deadline,
                "dropping a sibling stopped the worker"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(independent.rows(1000..1001, &THEME)[0].spans.len(), 3);
        drop(near);
        assert_eq!(independent.syntax.demands().0, 0);
        assert!(!independent.is_pending());
    }

    #[test]
    fn restoration_distinguishes_source_sides_and_out_of_order_hunks() {
        for body in [
            "@@ -0,0 +1,2 @@\n+first\n+last\n\\ No newline at end of file\n",
            "@@ -1,2 +0,0 @@\n-first\n-last\n\\ No newline at end of file\n",
            "@@ -1,2 +1,3 @@\n first\n-old\n+new\n+added\n\\ No newline at end of file\n",
            "@@ -10 +10 @@\n-ten\n+TEN\n@@ -1 +1 @@\n-one\n+ONE\n",
        ] {
            let patch = format!("diff --git a/z.rs b/z.rs\n{body}");
            let previous = DiffView::new(Arc::new(Diff::parse(patch.clone())), 3);
            let current = DiffView::new(
                Arc::new(Diff::parse(format!(
                    "diff --git a/a.rs b/a.rs\n@@ -0,0 +1 @@\n+first file\n{patch}"
                ))),
                3,
            );
            for row in 0..previous.snapshot().total_rows {
                let (cursor, top) = current.restore(&previous, row, row);
                let cursor = cursor.unwrap();
                assert_eq!(Some(cursor), top);
                assert_eq!(
                    current.rows(cursor..cursor + 1, &THEME)[0].text,
                    previous.rows(row..row + 1, &THEME)[0].text,
                    "anchor at row {row} in {body}"
                );
            }
        }
    }

    #[test]
    fn restored_folds_and_source_anchors_survive_insertions_and_file_reordering() {
        let patch = "diff --git a/z.rs b/z.rs\n@@ -1,101 +1,101 @@\n".to_string()
            + &(1..=100)
                .map(|i| format!(" context {i}\n"))
                .collect::<String>()
            + "-old\n+new\n";
        let previous = DiffView::new(Arc::new(Diff::parse(patch)), 3);
        previous.toggle_fold(5).unwrap();
        let independent = previous.fork();
        independent.toggle_fold(5).unwrap();
        assert_eq!(previous.snapshot().total_rows, 105);
        assert_eq!(independent.snapshot().total_rows, 11);
        let patch = "diff --git a/a.rs b/a.rs\n@@ -0,0 +1 @@\n+added file\n\
                     diff --git a/z.rs b/z.rs\n@@ -1,101 +1,102 @@\n+inserted first\n"
            .to_string()
            + &(1..=100)
                .map(|i| format!(" context {i}\n"))
                .collect::<String>()
            + "-old\n+new\n";
        let current = DiffView::new(Arc::new(Diff::parse(patch)), 3);
        let (cursor, top) = current.restore(&previous, 39, 30);
        let cursor = cursor.unwrap();
        let top = top.unwrap();
        assert_eq!(
            current.rows(cursor..cursor + 1, &THEME)[0].text,
            previous.rows(39..40, &THEME)[0].text
        );
        assert_eq!(
            current.rows(top..top + 1, &THEME)[0].text,
            previous.rows(30..31, &THEME)[0].text
        );
        assert_eq!(current.snapshot().total_rows, 111);
        assert_eq!(previous.snapshot().total_rows, 105);
    }

    #[test]
    fn syntax_matches_full_source_on_both_sides_after_folds_and_random_seeks() {
        use crate::content::highlight::InlineSyntax;
        use crate::style::Color;
        let patch = "diff --git a/code.rs b/code.rs\n@@ -1,205 +1,205 @@\n /* comment\n".to_string()
            + &" still a comment\n".repeat(200)
            + " */\n-/* old-only comment\n+let value = \"new\";\n let common = 42;\n-*/\n+\tlet end = \"界 e\u{301}\u{1b}\";\n";
        let view = DiffView::new(Arc::new(Diff::parse(patch)), 3);
        let mut old = InlineSyntax::new("rs", &THEME);
        let mut new = InlineSyntax::new("rs", &THEME);
        let mut expected = Vec::new();
        for (raw, line) in view.diff.lines.iter().enumerate() {
            let source = view.diff.source(raw, false);
            let spans = match line.kind {
                Kind::Context => {
                    old.highlight_spans(source);
                    new.highlight_spans(source)
                }
                Kind::Add => new.highlight_spans(source),
                Kind::Delete => old.highlight_spans(source),
                _ => continue,
            };
            let text = display_text(source);
            let mut colors = vec![None; smelt_buffer::text::byte_to_cell(&text, text.len())];
            for span in spans {
                let start = display_text(smelt_buffer::text::slice(source, 0..span.byte_start));
                let end = display_text(smelt_buffer::text::slice(source, 0..span.byte_end));
                let a = smelt_buffer::text::byte_to_cell(&start, start.len());
                let b = smelt_buffer::text::byte_to_cell(&end, end.len());
                let [r, g, b_color] = span.foreground;
                colors[a..b].fill(Some(Color::Rgb { r, g, b: b_color }));
            }
            expected.push((raw, colors));
        }
        assert!(view.toggle_fold(5).is_some());
        for (raw, colors) in expected.into_iter().rev() {
            let row = view.projection.lock().unwrap().display_row(raw);
            let rendered = highlighted(&view, row..row + 1).pop().unwrap();
            let mut actual = vec![None; colors.len()];
            for span in rendered.spans.iter().skip(2) {
                let a = usize::from(span.col_start)
                    .saturating_sub(2)
                    .min(actual.len());
                let b = usize::from(span.col_end)
                    .saturating_sub(2)
                    .min(actual.len());
                actual[a..b].fill(THEME.resolve(span.hl).fg);
            }
            assert_eq!(actual, colors, "row {raw}: {}", rendered.text);
        }
    }

    #[test]
    fn shared_syntax_cache_keeps_explicit_themes_isolated() {
        let view = DiffView::new(
            Arc::new(Diff::parse(
                "diff --git a/main.rs b/main.rs\n@@ -1 +1 @@\n-let answer = \"old\";\n+let answer = \"new\";\n".into(),
            )),
            3,
        );
        highlighted(&view, 0..4);
        let sibling = view.fork();
        let snapshot = view.snapshot();
        let mut dark = Theme::default();
        dark.set(
            "SmeltDiffAddBg",
            crate::style::Style::new().bg(Color::DarkGreen),
        );
        dark.set(
            "SmeltDiffAddInlineBg",
            crate::style::Style::new().bg(Color::Green),
        );
        let mut light = dark.clone();
        light.set_light(true);
        light.set(
            "SmeltDiffAddBg",
            crate::style::Style::new().bg(Color::DarkBlue),
        );
        light.set(
            "SmeltDiffAddInlineBg",
            crate::style::Style::new().bg(Color::Blue),
        );
        let first = view.rows(3..4, &dark).pop().unwrap();
        let second = sibling.rows(3..4, &light).pop().unwrap();
        assert_eq!(first.text, second.text);
        assert_eq!(first.decoration.window_bg, Some(Color::DarkGreen));
        assert_eq!(second.decoration.window_bg, Some(Color::DarkBlue));
        assert!(first
            .spans
            .iter()
            .any(|span| dark.resolve(span.hl).bg == Some(Color::Green)));
        assert!(second
            .spans
            .iter()
            .any(|span| light.resolve(span.hl).bg == Some(Color::Blue)));
        assert!(first
            .spans
            .iter()
            .zip(&second.spans)
            .any(|(a, b)| { dark.resolve(a.hl).fg != light.resolve(b.hl).fg }));
        assert_eq!(view.rows(3..4, &dark)[0].spans, first.spans);
        assert_eq!(view.snapshot(), snapshot);
        assert_eq!(sibling.snapshot(), snapshot);
    }

    #[test]
    fn syntax_is_lazy_thread_safe_and_coalesces_runs() {
        fn send_sync<T: Send + Sync>() {}
        send_sync::<DiffView>();
        let patch = "diff --git a/file.txt b/file.txt\n@@ -0,0 +1,10000 @@\n".to_string()
            + &"+plain text\n".repeat(10000);
        let view = DiffView::new(Arc::new(Diff::parse(patch.clone())), 3);
        assert_eq!(
            view.syntax.stats(),
            (0, 0, 0),
            "opening parsed offscreen syntax"
        );
        let rows = highlighted(&view, 0..40);
        assert_eq!(rows[2].spans.len(), 3);
        let (chunks, bytes, parsed) = view.syntax.stats();
        assert_eq!(chunks, 1);
        assert!(bytes < 1024);
        assert!(parsed <= 128, "viewport parsed {parsed} rows");
        let calls = std::cell::Cell::new(0);
        assert!(Diff::parse_cancellable(patch, || {
            calls.set(calls.get() + 1);
            calls.get() > 1
        })
        .is_none());
    }

    #[test]
    fn syntax_cache_is_bounded_reuses_checkpoints_and_notifies_repaints() {
        let patch = "diff --git a/file.txt b/file.txt\n@@ -0,0 +1,21000 @@\n".to_string()
            + &"+plain text\n".repeat(21000);
        let view = DiffView::new(Arc::new(Diff::parse(patch)), 3);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        view.set_wakeup(Some(tx));
        let generation = view.snapshot().generation;
        for row in (0..20480).step_by(128) {
            highlighted(&view, row..row + 40);
        }
        let (chunks, bytes, parsed) = view.syntax.stats();
        assert_eq!(chunks, 64);
        assert!(bytes < 64 * 1024);
        assert!(view.snapshot().generation > generation);
        assert!(rx.try_recv().is_ok(), "syntax never requested a repaint");
        highlighted(&view, 12000..12040);
        let rebuilt = view.syntax.stats().2 - parsed;
        assert!(rebuilt > 0, "test did not revisit an evicted chunk");
        assert!(
            rebuilt <= 4096 + 128,
            "checkpoint not reused: {rebuilt} rows"
        );
        let parsed = view.syntax.stats().2;
        highlighted(&view, 20000..20040);
        assert_eq!(view.syntax.stats().2, parsed, "cached syntax was reparsed");
        assert_eq!(view.syntax.stats().0, 64);
    }

    #[test]
    fn syntax_workers_release_the_patch_when_closed_idle_or_during_a_seek() {
        use std::time::{Duration, Instant};
        for active in [false, true] {
            let patch = "diff --git a/large.rs b/large.rs\n@@ -0,0 +1,100000 @@\n".to_string()
                + &"+let value = compute(42, \"hello\");\n".repeat(100000);
            let diff = Arc::new(Diff::parse(patch));
            let weak = Arc::downgrade(&diff);
            let view = DiffView::new(diff, 3);
            let subscription = smelt_buffer::document::RowView::new(Arc::new(view.fork()));
            if active {
                subscription.prepare(&DocumentViewport {
                    rows: 99000..99040,
                    width: 100,
                    cursor: Some(99000),
                });
                let deadline = Instant::now() + Duration::from_secs(5);
                while view.syntax.stats().2 == 0 {
                    assert!(Instant::now() < deadline, "worker did not start");
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            drop(subscription);
            drop(view);
            let deadline = Instant::now() + Duration::from_secs(5);
            while weak.upgrade().is_some() {
                assert!(
                    Instant::now() < deadline,
                    "closed worker retained its patch"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }

    #[test]
    fn unified_patch_indexes_line_numbers_files_and_metadata() {
        let patch = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,2 +1,2 @@\n same\n-old\n+new\n\\ No newline at end of file\ndiff --git a/b b/b\nnew file mode 100644\n--- /dev/null\n+++ b/b\n@@ -0,0 +1 @@\n+added\n";
        let view = DiffView::new(Arc::new(Diff::parse(patch.into())), 3);
        assert_eq!(view.diff.files.len(), 2);
        assert_eq!(
            (view.diff.files[0].additions, view.diff.files[0].deletions),
            (1, 1)
        );
        assert_eq!(view.diff.files[1].status, "A");
        assert_eq!(view.file_at(view.file_row(1).unwrap()), Some(1));
        let rows = view.rows(0..100, &THEME);
        assert_eq!(rows[4].text, "+ new");
        assert_eq!(
            rows[4].decoration.source_line,
            Some(SourceLine::Diff {
                old: None,
                new: Some(2)
            })
        );
        assert!(view.rows(Range { start: 100, end: 0 }, &THEME).is_empty());
        assert!(view.hunk(0, true).is_some());
    }

    #[test]
    fn directory_order_preserves_patch_bytes_syntax_folds_and_hunks() {
        let paths = [
            "z.rs",
            "src.rs",
            "src/z.rs",
            "a.rs",
            "src/nested/z.rs",
            "src/nested/a.rs",
            "界/a.rs",
            "src/a.rs",
        ];
        let patches: Vec<_> = paths.iter().map(|path| {
            format!("diff --git a/{path} b/{path}\n@@ -1,23 +1,23 @@\n /* comment\n{} */\n-let old = 1;\n+let value = \"{path}\";\n", " context\n".repeat(20))
        }).collect();
        let patch = patches.concat();
        let diff = Diff::parse(patch.clone());
        assert_eq!(diff.patch, patch);
        assert_eq!(
            diff.files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            [
                "src/nested/a.rs",
                "src/nested/z.rs",
                "src/a.rs",
                "src/z.rs",
                "界/a.rs",
                "a.rs",
                "src.rs",
                "z.rs"
            ]
        );
        let view = DiffView::new(Arc::new(diff), 3);
        for index in (0..paths.len()).rev() {
            let path = &view.diff.files[index].path;
            let expected = DiffView::new(
                Arc::new(Diff::parse(
                    patches[paths.iter().position(|p| p == path).unwrap()].clone(),
                )),
                3,
            );
            let start = view.file_row(index).unwrap();
            assert_eq!(view.file_at(start), Some(index));
            for expanded in [false, true] {
                if expanded {
                    assert_eq!(view.toggle_fold(start + 5), Some(start + 5));
                    assert_eq!(expected.toggle_fold(5), Some(5));
                }
                let total = expected.snapshot().total_rows;
                let actual = highlighted(&view, start..start + total);
                let expected_rows = highlighted(&expected, 0..total);
                for (a, e) in actual.iter().zip(&expected_rows) {
                    assert_eq!(a.text, e.text, "{path}");
                    assert_eq!(a.spans, e.spans, "{path}: {}", a.text);
                    assert_eq!(a.decoration.source_line, e.decoration.source_line);
                }
                assert_eq!(actual.len(), expected_rows.len());
                assert_eq!(
                    view.hunk(start, true),
                    expected.hunk(0, true).map(|row| start + row)
                );
            }
        }
    }

    #[test]
    fn context_folds_expand_without_reindexing_patch_text() {
        let mut patch = "diff --git a/x b/x\n@@ -1,21 +1,21 @@\n".to_string();
        for _ in 0..20 {
            patch.push_str(" unchanged\n");
        }
        patch.push_str("-old\n+new\n");
        let view = DiffView::new(Arc::new(Diff::parse(patch)), 3);
        let total = view.snapshot().total_rows;
        assert_eq!(total, 11);
        assert!(view.rows(5..6, &THEME)[0].text.contains("14 unchanged"));
        assert_eq!(view.toggle_fold(5), Some(5));
        assert_eq!(view.snapshot().total_rows, total + 14);
        assert_eq!(view.toggle_fold(10), Some(5));
        assert_eq!(view.snapshot().total_rows, total);
    }

    #[test]
    fn change_navigation_crosses_full_context_folds_and_files() {
        let patch = "diff --git a/a b/a\n@@ -1,12 +1,12 @@\n-old\n+new\n".to_string()
            + &" context\n".repeat(10)
            + "-old end\n+new end\ndiff --git a/b b/b\n@@ -0,0 +1 @@\n+added\n";
        let view = DiffView::new(Arc::new(Diff::parse(patch)), 3);
        let first = view.hunk(0, true).unwrap();
        let second = view.hunk(first, true).unwrap();
        let third = view.hunk(second, true).unwrap();
        assert_eq!(view.rows(first..first + 1, &THEME)[0].text, "- old");
        assert_eq!(view.rows(second..second + 1, &THEME)[0].text, "- old end");
        assert_eq!(view.file_at(third), Some(1));
        assert_eq!(view.hunk(third, true), None);
        assert_eq!(view.hunk(second, false), Some(first));
        assert_eq!(view.hunk(first, false), None);
        assert_eq!(view.toggle_fold(first + 5), Some(first + 5));
        assert_eq!(view.hunk(first, true), Some(second + 4));
        assert_eq!(view.file_at(u64::MAX), None);
        assert_eq!(view.toggle_fold(u64::MAX), None);
    }

    #[test]
    #[ignore = "timed benchmark: cargo xtask bench-diff"]
    fn diff_index_benchmark() {
        use std::time::Instant;
        for count in [10_000, 1_000_000] {
            for context in [false, true] {
                let patch = format!(
                    "diff --git a/large.rs b/large.rs\n@@ -1,{} +1,{} @@\n",
                    if context { count + 1 } else { 1 },
                    count + 1
                ) + &if context {
                    " let value = compute(42, \"hello\"); // context\n"
                } else {
                    "+let value = compute(42, \"hello\"); // addition\n"
                }
                .repeat(count)
                    + "-old\n+new\n";
                let patch_bytes = patch.len();
                let start = Instant::now();
                let view = DiffView::new(Arc::new(Diff::parse(patch)), 3);
                let load = start.elapsed();
                let mut samples = Vec::new();
                for i in 0..200 {
                    let start = Instant::now();
                    if context {
                        assert_eq!(view.toggle_fold(5), Some(5));
                    }
                    let total = view.snapshot().total_rows;
                    let row = if i % 2 == 0 {
                        total.saturating_sub(40)
                    } else {
                        0
                    };
                    let rows = std::hint::black_box(view.rows(row..row + 40, &THEME));
                    assert!(rows.len() <= 40);
                    samples.push(start.elapsed());
                }
                samples.sort();
                println!("diff index {count} rows context={context}: patch_bytes={patch_bytes} indexed_bytes={} load={load:?} immediate viewport/fold p50={:?} p95={:?}", view.diff.indexed_bytes(), samples[100], samples[190]);
                assert!(samples[190] < std::time::Duration::from_millis(16));
                if !cfg!(debug_assertions) {
                    assert!(load < std::time::Duration::from_secs(1));
                }
                if context {
                    assert_eq!(view.toggle_fold(5), Some(5));
                }
                let ready = highlighted(&view, 0..40);
                assert!(ready[2].spans.len() > 3, "warm viewport lacks syntax");
                let mut samples = Vec::new();
                for _ in 0..200 {
                    let start = Instant::now();
                    std::hint::black_box(view.rows(0..40, &THEME));
                    samples.push(start.elapsed());
                }
                samples.sort();
                println!(
                    "  cached syntax viewport p50={:?} p95={:?}",
                    samples[100], samples[190]
                );
                assert!(samples[190] < std::time::Duration::from_millis(16));
            }
        }
    }

    #[test]
    fn quoted_paths_and_controls_are_safe() {
        let patch = "diff --git \"a/hi\\t\\303\\251\" \"b/hi\\t\\303\\251\"\nnew file mode 100644\n@@ -0,0 +1 @@\n+\u{1b}[31m界\tend\n";
        let view = DiffView::new(Arc::new(Diff::parse(patch.into())), 3);
        assert_eq!(view.diff.files[0].path, "hi\té");
        let rows = view.rows(0..10, &THEME);
        assert!(!rows.iter().any(|row| row.text.contains('\u{1b}')));
        assert!(rows.last().unwrap().text.contains("界    end"));
    }
}
