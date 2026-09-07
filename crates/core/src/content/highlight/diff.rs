//! Inline diff rendering: live `print_inline_diff` for tools that
//! produce a fresh diff per render, plus the persisted `DiffIr`
//! IR that `edit_file` / `edit_notebook` produce once and replay.

use serde::{Deserialize, Serialize};
use similar::{ChangeTag, TextDiff};
use std::borrow::Cow;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};
use syntect::easy::HighlightLines;
use syntect::highlighting::HighlightState;
use syntect::parsing::ParseState;

use super::{syntax_theme, GutterStyle, SYNTAX_SET};
use crate::content::builder::LineBuilder;
use crate::content::default_width;
use crate::content::inline_line::{BreakPolicy, InlineLine, InlineRun};
use crate::style::Color;
use crate::transcript_content::{
    ContentRead, ContentTextWindow, SharedContentSlice, TranscriptContent,
};
use smelt_buffer::buffer::SpanMeta;

mod diff_inline;

use diff_inline::{
    align_changed_lines, annotate_inline_highlights, full_line_highlight,
    inline_highlights_for_pair, LineAlignment,
};

struct DiffChange {
    tag: ChangeTag,
    value: String,
}

struct DiffViewData {
    file_content: String,
    start_line: usize,
    first_mod: usize,
    view_start: usize,
    view_end: usize,
    max_display_lineno: usize,
    changes: Vec<DiffChange>,
    is_full_file: bool,
}

const SYNTAX_CHECKPOINT_INTERVAL: usize = 128;
const MAX_SYNTAX_CHECKPOINTS: usize = u16::MAX as usize / SYNTAX_CHECKPOINT_INTERVAL + 2;
const MAX_ROW_LAYOUTS: usize = 2;
const MAX_HIGHLIGHT_LINE_BYTES: usize = 16 * 1024;
const MAX_HIGHLIGHT_FILE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffIr {
    pub(crate) max_display_lineno: usize,
    pub(crate) syntax_ext: String,
    pub(crate) lines: Vec<DiffLine>,
    #[serde(skip)]
    render_cache: Arc<Mutex<DiffRenderCache>>,
}

impl DiffIr {
    pub fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            .saturating_add(self.syntax_ext.capacity())
            .saturating_add(
                self.lines
                    .capacity()
                    .saturating_mul(std::mem::size_of::<DiffLine>()),
            )
            .saturating_add(self.lines.iter().map(DiffLine::dynamic_bytes).sum())
    }
}

#[derive(Clone, Default)]
pub struct RetainedFileViewCache {
    syntax: Arc<Mutex<Option<DiffSyntaxCache>>>,
}

impl std::fmt::Debug for RetainedFileViewCache {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let syntax = self
            .syntax
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        formatter
            .debug_struct("RetainedFileViewCache")
            .field(
                "syntax_checkpoints",
                &syntax.as_ref().map_or(0, |syntax| syntax.checkpoints.len()),
            )
            .finish()
    }
}

impl RetainedFileViewCache {
    pub fn retained_bytes(&self) -> usize {
        let syntax = self
            .syntax
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        syntax.as_ref().map_or(0, |syntax| {
            syntax
                .checkpoints
                .capacity()
                .saturating_mul(std::mem::size_of::<SyntaxCheckpoint>())
        })
    }
}

#[derive(Debug, Default)]
struct DiffRenderCache {
    row_layouts: VecDeque<Arc<DiffRowLayout>>,
    syntax: Option<DiffSyntaxCache>,
}

#[derive(Debug)]
struct DiffRowLayout {
    max_content: usize,
    line_starts: Vec<usize>,
    total_rows: usize,
}

impl DiffRowLayout {
    fn line_index_for_row(&self, row: usize) -> Option<usize> {
        if row >= self.total_rows {
            return None;
        }
        Some(
            self.line_starts
                .partition_point(|start| *start <= row)
                .saturating_sub(1),
        )
    }
}

#[derive(Debug)]
struct DiffSyntaxCache {
    theme_id: usize,
    checkpoints: Vec<SyntaxCheckpoint>,
}

#[derive(Clone, Debug)]
struct SyntaxCheckpoint {
    line_index: usize,
    highlight_state: HighlightState,
    parse_state: ParseState,
}

#[cfg(test)]
thread_local! {
    static PREFIX_SYNTAX_LINES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn diff_ir(max_display_lineno: usize, syntax_ext: String, lines: Vec<DiffLine>) -> DiffIr {
    DiffIr {
        max_display_lineno,
        syntax_ext,
        lines,
        render_cache: Arc::default(),
    }
}

/// UTF-8 byte range into a rendered diff line. Diff lines expand tabs before
/// inline ranges are computed, so these offsets always refer to displayed text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffByteRange {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum DiffLine {
    Context {
        lineno: usize,
        text: String,
    },
    Delete {
        lineno: usize,
        text: String,
        #[serde(default)]
        highlights: Vec<DiffByteRange>,
    },
    Insert {
        lineno: usize,
        text: String,
        #[serde(default)]
        highlights: Vec<DiffByteRange>,
    },
    Ellipsis,
}

impl DiffLine {
    fn dynamic_bytes(&self) -> usize {
        match self {
            Self::Context { text, .. } => text.capacity(),
            Self::Delete {
                text, highlights, ..
            }
            | Self::Insert {
                text, highlights, ..
            } => text.capacity().saturating_add(
                highlights
                    .capacity()
                    .saturating_mul(std::mem::size_of::<DiffByteRange>()),
            ),
            Self::Ellipsis => 0,
        }
    }
}

fn diff_line_layout(text: &str) -> InlineLine<()> {
    InlineLine::new(vec![InlineRun::new(
        text.to_string(),
        (),
        BreakPolicy::PreserveSpaces,
    )])
}

fn context_line(lineno: usize, text: String) -> DiffLine {
    DiffLine::Context { lineno, text }
}

fn delete_line(lineno: usize, text: String) -> DiffLine {
    DiffLine::Delete {
        lineno,
        text,
        highlights: Vec::new(),
    }
}

fn insert_line(lineno: usize, text: String) -> DiffLine {
    DiffLine::Insert {
        lineno,
        text,
        highlights: Vec::new(),
    }
}

fn push_expanded_tabs(out: &mut String, s: &str) {
    let mut rest = s;
    while let Some(tab) = rest.find('\t') {
        out.push_str(&rest[..tab]);
        out.push_str("    ");
        rest = &rest[tab + 1..];
    }
    out.push_str(rest);
}

fn expanded_line(line: &str) -> String {
    if line.contains('\t') {
        let mut out = String::with_capacity(line.len());
        push_expanded_tabs(&mut out, line);
        out
    } else {
        line.to_string()
    }
}

fn expanded_change_line(extra_indent: &str, value: &str) -> String {
    let value = value.trim_end_matches('\n');
    let mut out = String::with_capacity(extra_indent.len() + value.len());
    out.push_str(extra_indent);
    push_expanded_tabs(&mut out, value);
    out
}

#[cfg(test)]
pub(crate) fn build_diff_ir(old: &str, new: &str, path: &str, anchor: &str) -> DiffIr {
    build_diff_ir_ext(old, new, path, anchor, None)
}

/// All-Context IR for a single-file view (write_file, notebook insert,
/// `smelt.syntax.render_file`). Same IR as the diff renderer so a single
/// `print_diff_ir` pipeline serves both - line numbers, wrap math,
/// and bg-spanning all live in one place.
pub fn build_file_view_ir(content: &str, ext: Option<&str>) -> DiffIr {
    let _perf = smelt_perf::perf::begin("render:build_file_view_ir");
    let syntax_ext = ext.unwrap_or("txt").to_string();
    let lines: Vec<DiffLine> = content
        .lines()
        .enumerate()
        .map(|(i, line)| context_line(i + 1, line.to_string()))
        .collect();
    let max_display_lineno = lines.len().max(1);
    diff_ir(max_display_lineno, syntax_ext, lines)
}

fn retained_lines<'a>(
    content: &'a ContentRead<'_>,
    lines: std::ops::Range<usize>,
) -> Vec<Cow<'a, str>> {
    let end = lines.end.min(content.logical_line_count());
    (lines.start.min(end)..end)
        .filter_map(|line| {
            let range = content.line_range(line)?;
            let end = content
                .line_range(line.saturating_add(1))
                .map_or_else(|| content.len(), |next| next.start);
            Some(content.slice(range.start..end))
        })
        .collect()
}

fn common_prefix_bytes(old: &[SharedContentSlice], new: &[SharedContentSlice]) -> usize {
    let (mut old_chunk, mut new_chunk) = (0usize, 0usize);
    let (mut old_offset, mut new_offset) = (0usize, 0usize);
    let mut common = 0usize;
    while old_chunk < old.len() && new_chunk < new.len() {
        if old_offset == old[old_chunk].len() {
            old_chunk = old_chunk.saturating_add(1);
            old_offset = 0;
            continue;
        }
        if new_offset == new[new_chunk].len() {
            new_chunk = new_chunk.saturating_add(1);
            new_offset = 0;
            continue;
        }
        let old_bytes = &old[old_chunk].as_bytes()[old_offset..];
        let new_bytes = &new[new_chunk].as_bytes()[new_offset..];
        let compared = old_bytes.len().min(new_bytes.len());
        if old_bytes[..compared] != new_bytes[..compared] {
            let equal = old_bytes[..compared]
                .iter()
                .zip(&new_bytes[..compared])
                .take_while(|(old, new)| old == new)
                .count();
            return common.saturating_add(equal);
        }
        common = common.saturating_add(compared);
        old_offset = old_offset.saturating_add(compared);
        new_offset = new_offset.saturating_add(compared);
    }
    common
}

fn common_suffix_bytes(old: &[SharedContentSlice], new: &[SharedContentSlice]) -> usize {
    let (mut old_chunk, mut new_chunk) = (old.len(), new.len());
    let (mut old_end, mut new_end) = (0usize, 0usize);
    let mut common = 0usize;
    loop {
        while old_end == 0 {
            if old_chunk == 0 {
                return common;
            }
            old_chunk -= 1;
            old_end = old[old_chunk].len();
        }
        while new_end == 0 {
            if new_chunk == 0 {
                return common;
            }
            new_chunk -= 1;
            new_end = new[new_chunk].len();
        }
        let compared = old_end.min(new_end);
        let old_bytes = &old[old_chunk].as_bytes()[old_end - compared..old_end];
        let new_bytes = &new[new_chunk].as_bytes()[new_end - compared..new_end];
        if old_bytes != new_bytes {
            let equal = old_bytes
                .iter()
                .rev()
                .zip(new_bytes.iter().rev())
                .take_while(|(old, new)| old == new)
                .count();
            return common.saturating_add(equal);
        }
        common = common.saturating_add(compared);
        old_end -= compared;
        new_end -= compared;
    }
}

/// Builds a full-file diff from retained content without materializing either source.
/// Only changed lines and bounded context are copied into the resulting IR.
pub fn build_retained_diff_ir(
    old: &TranscriptContent,
    new: &TranscriptContent,
    path: &str,
    syntax_ext: Option<&str>,
) -> DiffIr {
    let _perf = smelt_perf::perf::begin("render:build_retained_diff_ir");
    let old_read = old.read();
    let new_read = new.read();
    let old_line_count = old_read.logical_line_count();
    let new_line_count = new_read.logical_line_count();
    let prefix_bytes = common_prefix_bytes(old_read.chunks(), new_read.chunks());
    let common_prefix_lines = old_read
        .whole_line_prefix_len(prefix_bytes)
        .min(new_read.whole_line_prefix_len(prefix_bytes));
    let suffix_bytes = common_suffix_bytes(old_read.chunks(), new_read.chunks());
    let mut common_suffix_lines = old_read
        .whole_line_suffix_len(old_read.len().saturating_sub(suffix_bytes))
        .min(new_read.whole_line_suffix_len(new_read.len().saturating_sub(suffix_bytes)));
    common_suffix_lines = common_suffix_lines.min(
        old_line_count
            .min(new_line_count)
            .saturating_sub(common_prefix_lines),
    );

    let context_rows = 3usize;
    let old_start = common_prefix_lines.saturating_sub(context_rows);
    let new_start = old_start;
    let old_end = old_line_count
        .saturating_sub(common_suffix_lines)
        .saturating_add(context_rows)
        .min(old_line_count);
    let new_end = new_line_count
        .saturating_sub(common_suffix_lines)
        .saturating_add(context_rows)
        .min(new_line_count);
    let old_lines = retained_lines(&old_read, old_start..old_end);
    let new_lines = retained_lines(&new_read, new_start..new_end);
    let old_refs = old_lines.iter().map(Cow::as_ref).collect::<Vec<_>>();
    let new_refs = new_lines.iter().map(Cow::as_ref).collect::<Vec<_>>();
    let diff = TextDiff::configure().diff_slices(&old_refs, &new_refs);
    let tags = diff
        .iter_all_changes()
        .map(|change| change.tag())
        .collect::<Vec<_>>();
    let visible = compute_visibility(&tags, context_rows, |tag| *tag);
    let mut first_mod = None;
    let mut last_mod = None;
    let mut new_line = new_start;
    for tag in &tags {
        match tag {
            ChangeTag::Equal => new_line = new_line.saturating_add(1),
            ChangeTag::Delete => {
                first_mod.get_or_insert(new_line);
                last_mod = Some(new_line);
            }
            ChangeTag::Insert => {
                first_mod.get_or_insert(new_line);
                last_mod = Some(new_line);
                new_line = new_line.saturating_add(1);
            }
        }
    }
    let view_start = first_mod.unwrap_or(0).saturating_sub(context_rows);
    let view_end = last_mod
        .unwrap_or(0)
        .saturating_add(1 + context_rows)
        .min(old_line_count);

    let mut lines = Vec::new();
    let mut emitted_any = false;
    let mut pending_ellipsis = false;
    for (index, change) in diff.iter_all_changes().enumerate() {
        match change.tag() {
            ChangeTag::Equal if visible[index] => {
                if pending_ellipsis {
                    pending_ellipsis = false;
                    lines.push(DiffLine::Ellipsis);
                }
                let line = change
                    .new_index()
                    .expect("equal retained diff change has a new line")
                    .saturating_add(new_start);
                if line >= view_start && line < view_end {
                    lines.push(context_line(
                        line + 1,
                        expanded_change_line("", change.value()),
                    ));
                    emitted_any = true;
                }
            }
            ChangeTag::Equal => {
                if emitted_any {
                    pending_ellipsis = true;
                }
            }
            ChangeTag::Delete => {
                if pending_ellipsis {
                    pending_ellipsis = false;
                    lines.push(DiffLine::Ellipsis);
                }
                let line = change
                    .old_index()
                    .expect("deleted retained diff change has an old line")
                    .saturating_add(old_start);
                lines.push(delete_line(
                    line + 1,
                    expanded_change_line("", change.value()),
                ));
                emitted_any = true;
            }
            ChangeTag::Insert => {
                if pending_ellipsis {
                    pending_ellipsis = false;
                    lines.push(DiffLine::Ellipsis);
                }
                let line = change
                    .new_index()
                    .expect("inserted retained diff change has a new line")
                    .saturating_add(new_start);
                lines.push(insert_line(
                    line + 1,
                    expanded_change_line("", change.value()),
                ));
                emitted_any = true;
            }
        }
    }
    annotate_inline_highlights(&mut lines);

    let syntax_ext = syntax_ext
        .or_else(|| Path::new(path).extension().and_then(|ext| ext.to_str()))
        .unwrap_or("txt")
        .to_string();
    diff_ir(old_line_count.max(new_line_count), syntax_ext, lines)
}

pub fn build_diff_ir_ext(
    old: &str,
    new: &str,
    path: &str,
    anchor: &str,
    syntax_ext: Option<&str>,
) -> DiffIr {
    build_diff_ir_ext_with_base(old, new, path, anchor, syntax_ext, None)
}

pub fn build_diff_ir_ext_with_base(
    old: &str,
    new: &str,
    path: &str,
    anchor: &str,
    syntax_ext: Option<&str>,
    file_content: Option<&str>,
) -> DiffIr {
    build_diff_ir_ext_inner(
        old,
        new,
        path,
        anchor,
        syntax_ext,
        file_content,
        file_content.is_some(),
    )
}

pub fn build_diff_ir_ext_with_source(
    old: &str,
    new: &str,
    path: &str,
    anchor: &str,
    syntax_ext: Option<&str>,
    file_content: &str,
) -> DiffIr {
    build_diff_ir_ext_inner(
        old,
        new,
        path,
        anchor,
        syntax_ext,
        Some(file_content),
        false,
    )
}

fn build_diff_ir_ext_inner(
    old: &str,
    new: &str,
    path: &str,
    anchor: &str,
    syntax_ext: Option<&str>,
    file_content: Option<&str>,
    explicit_full_file: bool,
) -> DiffIr {
    let _perf = smelt_perf::perf::begin("render:build_diff_ir");
    let dv = compute_diff_view(old, new, path, anchor, file_content, explicit_full_file);
    let file_lines: Vec<&str> = dv.file_content.lines().collect();
    let lookup = if !anchor.is_empty() { anchor } else { old };
    let lookup_indent = lookup
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take_while(|c| c.is_whitespace())
        .count();
    let file_indent = file_lines
        .get(dv.start_line)
        .unwrap_or(&"")
        .chars()
        .take_while(|c| c.is_whitespace())
        .count();
    let extra_indent = if dv.is_full_file {
        String::new()
    } else {
        " ".repeat(file_indent.saturating_sub(lookup_indent))
    };

    let syntax_ext = syntax_ext
        .unwrap_or_else(|| {
            Path::new(path)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("txt")
        })
        .to_string();

    let ctx = 3usize;
    let visible = compute_change_visibility(&dv.changes, ctx);
    let mut lines = Vec::new();

    if !dv.is_full_file {
        let ctx_before_end = dv.start_line.min(dv.first_mod);
        let ctx_before_start = dv.view_start.min(ctx_before_end);
        for (idx, line) in file_lines[ctx_before_start..ctx_before_end]
            .iter()
            .enumerate()
        {
            let text = expanded_line(line);
            lines.push(context_line(ctx_before_start + idx + 1, text));
        }
    }

    let track_start = if dv.is_full_file { 0 } else { dv.start_line };
    let mut old_lineno = track_start;
    let mut new_lineno = track_start;
    let mut pending_ellipsis = false;
    let mut emitted_any = !lines.is_empty();
    for (ci, change) in dv.changes.iter().enumerate() {
        match change.tag {
            ChangeTag::Equal => {
                if visible[ci] {
                    if pending_ellipsis {
                        pending_ellipsis = false;
                        lines.push(DiffLine::Ellipsis);
                    }
                    if new_lineno >= dv.view_start && new_lineno < dv.view_end {
                        let raw = expanded_change_line(&extra_indent, &change.value);
                        lines.push(context_line(new_lineno + 1, raw));
                        emitted_any = true;
                    }
                } else if emitted_any {
                    pending_ellipsis = true;
                }
                old_lineno += 1;
                new_lineno += 1;
            }
            ChangeTag::Delete => {
                if pending_ellipsis {
                    pending_ellipsis = false;
                    lines.push(DiffLine::Ellipsis);
                }
                let raw = expanded_change_line(&extra_indent, &change.value);
                lines.push(delete_line(old_lineno + 1, raw));
                old_lineno += 1;
            }
            ChangeTag::Insert => {
                if pending_ellipsis {
                    pending_ellipsis = false;
                    lines.push(DiffLine::Ellipsis);
                }
                let raw = expanded_change_line(&extra_indent, &change.value);
                lines.push(insert_line(new_lineno + 1, raw));
                new_lineno += 1;
            }
        }
    }

    if !dv.is_full_file {
        let after_start = new_lineno;
        let after_end = dv.view_end.min(file_lines.len());
        for (idx, line) in file_lines
            .iter()
            .take(after_end)
            .skip(after_start)
            .enumerate()
        {
            let text = expanded_line(line);
            lines.push(context_line(after_start + idx + 1, text));
        }
    }

    annotate_inline_highlights(&mut lines);

    diff_ir(dv.max_display_lineno, syntax_ext, lines)
}

#[cfg(test)]
fn change(tag: ChangeTag, value: &str) -> DiffChange {
    DiffChange {
        tag,
        value: value.to_string(),
    }
}

fn compute_diff_view(
    old: &str,
    new: &str,
    path: &str,
    anchor: &str,
    file_content: Option<&str>,
    explicit_full_file: bool,
) -> DiffViewData {
    let file_content = file_content
        .map(str::to_string)
        .unwrap_or_else(|| std::fs::read_to_string(path).unwrap_or_default());
    let file_lines_count = file_content.lines().count();
    let lookup = if !anchor.is_empty() {
        anchor
    } else if !old.is_empty() {
        old
    } else {
        new
    };
    let start_line = if lookup.is_empty() {
        0
    } else {
        file_content
            .find(lookup)
            .map(|pos| file_content[..pos].bytes().filter(|&b| b == b'\n').count())
            .unwrap_or(0)
    };

    let old_line_count = old.lines().count();
    let new_line_count = new.lines().count();
    let is_full_file = explicit_full_file
        || old_line_count == file_lines_count
        || new_line_count == file_lines_count;
    let projected_file_lines_count = projected_file_line_count(
        file_lines_count,
        old_line_count,
        new_line_count,
        is_full_file,
    );

    let diff = TextDiff::from_lines(old, new);
    let changes: Vec<DiffChange> = diff
        .iter_all_changes()
        .map(|c| DiffChange {
            tag: c.tag(),
            value: c.value().to_string(),
        })
        .collect();
    let ctx = 3usize;
    let mut first_mod: Option<usize> = None;
    let mut last_mod: Option<usize> = None;
    let track_start = if is_full_file { 0 } else { start_line };
    let mut new_line = track_start;
    let mut old_line = track_start;
    for c in &changes {
        match c.tag {
            ChangeTag::Equal => {
                new_line += 1;
                old_line += 1;
            }
            ChangeTag::Delete => {
                if first_mod.is_none() {
                    first_mod = Some(new_line);
                }
                last_mod = Some(new_line);
                old_line += 1;
            }
            ChangeTag::Insert => {
                if first_mod.is_none() {
                    first_mod = Some(new_line);
                }
                last_mod = Some(new_line);
                new_line += 1;
            }
        }
    }
    let first_mod = first_mod.unwrap_or(track_start);
    let last_mod = last_mod.unwrap_or(track_start);
    let view_start = first_mod.saturating_sub(ctx);
    let view_end = (last_mod + 1 + ctx).min(file_lines_count);
    let gutter_max_lineno = diff_gutter_max_lineno(
        view_end,
        old_line,
        new_line,
        file_lines_count,
        projected_file_lines_count,
    );

    DiffViewData {
        file_content,
        start_line,
        first_mod,
        view_start,
        view_end,
        max_display_lineno: gutter_max_lineno,
        changes,
        is_full_file,
    }
}

fn projected_file_line_count(
    file_lines_count: usize,
    old_line_count: usize,
    new_line_count: usize,
    is_full_file: bool,
) -> usize {
    if is_full_file {
        file_lines_count
    } else {
        file_lines_count
            .saturating_sub(old_line_count)
            .saturating_add(new_line_count)
    }
}

fn diff_gutter_max_lineno(
    view_end: usize,
    old_line: usize,
    new_line: usize,
    file_lines_count: usize,
    projected_file_lines_count: usize,
) -> usize {
    // Snippet diffs still reserve line-number space for the whole file: the
    // current length matters for deletions, the projected length for insertions.
    view_end
        .max(old_line)
        .max(new_line)
        .max(file_lines_count)
        .max(projected_file_lines_count)
}

/// Mark equal lines within `ctx` of any non-Equal change as visible; collapse the rest.
fn compute_visibility<T>(changes: &[T], ctx: usize, tag: impl Fn(&T) -> ChangeTag) -> Vec<bool> {
    let n = changes.len();
    let mut visible = vec![false; n];
    let mut d = usize::MAX;
    // Forward pass: visible based on distance from preceding non-Equal.
    for i in 0..n {
        if tag(&changes[i]) != ChangeTag::Equal {
            d = 0;
            visible[i] = true;
        } else {
            visible[i] = d <= ctx;
        }
        d = d.saturating_add(1);
    }
    // Backward pass: also catch equal lines near a following non-Equal.
    d = usize::MAX;
    for i in (0..n).rev() {
        if tag(&changes[i]) != ChangeTag::Equal {
            d = 0;
        } else if d <= ctx {
            visible[i] = true;
        }
        d = d.saturating_add(1);
    }
    visible
}

fn compute_change_visibility(changes: &[DiffChange], ctx: usize) -> Vec<bool> {
    compute_visibility(changes, ctx, |change| change.tag)
}

/// Render a syntax-highlighted inline diff; `skip` rows are skipped, at most `max_rows` emitted.
/// Syntax is inferred from `path`'s extension.
#[allow(clippy::too_many_arguments)]
pub fn print_inline_diff(
    out: &mut LineBuilder,
    old: &str,
    new: &str,
    path: &str,
    anchor: &str,
    gutter: GutterStyle,
    skip: u16,
    max_rows: u16,
) -> u16 {
    print_inline_diff_ext(out, old, new, path, anchor, None, gutter, 0, skip, max_rows)
}

/// Like [`print_inline_diff`] but with an explicit syntect language/extension token
/// (bypasses the `path`-based extension sniff) and `indent_cells` of non-selectable
/// leading indent per row - used by the tool-block worker to align diff content with
/// the tool name's column without a separate replay-time wrapper.
#[allow(clippy::too_many_arguments)]
pub fn print_inline_diff_ext(
    out: &mut LineBuilder,
    old: &str,
    new: &str,
    path: &str,
    anchor: &str,
    syntax_ext: Option<&str>,
    gutter: GutterStyle,
    indent_cells: u16,
    skip: u16,
    max_rows: u16,
) -> u16 {
    let _perf = smelt_perf::perf::begin("render:inline_diff_cold");
    let cache = build_diff_ir_ext(old, new, path, anchor, syntax_ext);
    print_diff_ir(out, &cache, gutter, indent_cells, skip, max_rows)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RenderMeta {
    fg: (u8, u8, u8),
    highlighted: bool,
}

#[derive(Debug, Clone)]
struct RenderSpan {
    text: String,
    meta: RenderMeta,
}

fn range_contains(ranges: &[DiffByteRange], idx: usize) -> bool {
    ranges
        .iter()
        .any(|range| idx >= range.start && idx < range.end)
}

fn push_render_span(spans: &mut Vec<RenderSpan>, text: String, meta: RenderMeta) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = spans.last_mut() {
        if last.meta == meta {
            last.text.push_str(&text);
            return;
        }
    }
    spans.push(RenderSpan { text, meta });
}

fn syntax_spans_for_line_with_highlights(
    h: &mut HighlightLines,
    line: &str,
    highlights: &[DiffByteRange],
) -> Vec<RenderSpan> {
    let mut line_with_nl = String::with_capacity(line.len() + 1);
    line_with_nl.push_str(line);
    line_with_nl.push('\n');
    let mut byte_pos = 0usize;
    let mut spans = Vec::new();
    h.highlight_line(&line_with_nl, &SYNTAX_SET)
        .unwrap_or_default()
        .into_iter()
        .for_each(|(style, text)| {
            let text = text.trim_end_matches('\n').trim_end_matches('\r');
            if text.is_empty() {
                return;
            }
            let fg = (style.foreground.r, style.foreground.g, style.foreground.b);
            let mut chunk = String::new();
            let mut chunk_highlighted: Option<bool> = None;
            for ch in text.chars() {
                let highlighted = range_contains(highlights, byte_pos);
                if let Some(current) = chunk_highlighted {
                    if current != highlighted {
                        push_render_span(
                            &mut spans,
                            std::mem::take(&mut chunk),
                            RenderMeta {
                                fg,
                                highlighted: current,
                            },
                        );
                    }
                }
                chunk_highlighted = Some(highlighted);
                chunk.push(ch);
                byte_pos += ch.len_utf8();
            }
            if let Some(highlighted) = chunk_highlighted {
                push_render_span(&mut spans, chunk, RenderMeta { fg, highlighted });
            }
        });
    spans
}

#[cfg(test)]
fn syntax_spans_for_line(h: &mut HighlightLines, line: &str) -> Vec<RenderSpan> {
    syntax_spans_for_line_with_highlights(h, line, &[])
}

fn print_syntax_spans(
    out: &mut LineBuilder,
    spans: &[RenderSpan],
    row_bg: Option<Color>,
    inline_bg: Option<Color>,
) -> usize {
    let col =
        smelt_buffer::cell_width::joined_text_width(spans.iter().map(|span| span.text.as_str()));
    for span in spans {
        if span.text.is_empty() {
            continue;
        }
        let bg = if span.meta.highlighted {
            inline_bg.or(row_bg)
        } else {
            row_bg
        };
        if let Some(bg_color) = bg {
            out.set_bg(bg_color);
        }
        out.set_fg(Color::Rgb {
            r: span.meta.fg.0,
            g: span.meta.fg.1,
            b: span.meta.fg.2,
        });
        out.print(&span.text);
    }
    out.reset_style();
    col
}

fn split_syntax_spans_into_rows_with_highlights(
    h: &mut HighlightLines,
    line: &str,
    highlights: &[DiffByteRange],
    max_width: usize,
) -> Vec<Vec<RenderSpan>> {
    let spans = syntax_spans_for_line_with_highlights(h, line, highlights);
    let line = InlineLine::new(
        spans
            .into_iter()
            .map(|span| InlineRun::new(span.text, span.meta, BreakPolicy::PreserveSpaces))
            .collect(),
    );
    line.wrap_ranges(max_width.max(1))
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|run| RenderSpan {
                    text: run.text,
                    meta: run.meta,
                })
                .collect()
        })
        .collect()
}

#[cfg(test)]
fn split_syntax_spans_into_rows(
    h: &mut HighlightLines,
    line: &str,
    max_width: usize,
) -> Vec<Vec<RenderSpan>> {
    split_syntax_spans_into_rows_with_highlights(h, line, &[], max_width)
}

fn diff_line_rows(layout: &InlineLine<()>, max_width: usize) -> usize {
    layout.wrap_rows(max_width.max(1))
}

#[derive(Clone, Copy)]
struct DiffSidePalette {
    row_bg: Option<Color>,
    inline_bg: Option<Color>,
}

#[derive(Clone, Copy)]
struct DiffPalette {
    add: DiffSidePalette,
    del: DiffSidePalette,
}

fn diff_palette(theme: &crate::theme::Theme) -> DiffPalette {
    let del_row = theme.get("SmeltDiffDeleteBg").bg;
    let add_row = theme.get("SmeltDiffAddBg").bg;
    DiffPalette {
        add: DiffSidePalette {
            row_bg: add_row,
            inline_bg: theme.get("SmeltDiffAddInlineBg").bg.or(add_row),
        },
        del: DiffSidePalette {
            row_bg: del_row,
            inline_bg: theme.get("SmeltDiffDeleteInlineBg").bg.or(del_row),
        },
    }
}

impl DiffPalette {
    fn split_side(self, side: SplitSide) -> DiffSidePalette {
        match side {
            SplitSide::Left => self.del,
            SplitSide::Right => self.add,
        }
    }
}

fn render_cache(cache: &DiffIr) -> std::sync::MutexGuard<'_, DiffRenderCache> {
    cache
        .render_cache
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

fn diff_row_layout(cache: &DiffIr, max_content: usize) -> Arc<DiffRowLayout> {
    if let Some(layout) = render_cache(cache)
        .row_layouts
        .iter()
        .find(|layout| layout.max_content == max_content)
        .cloned()
    {
        return layout;
    }

    let mut line_starts = Vec::with_capacity(cache.lines.len());
    let mut total_rows = 0usize;
    for line in &cache.lines {
        line_starts.push(total_rows);
        total_rows = total_rows.saturating_add(match line {
            DiffLine::Context { text, .. }
            | DiffLine::Delete { text, .. }
            | DiffLine::Insert { text, .. } => diff_line_rows(&diff_line_layout(text), max_content),
            DiffLine::Ellipsis => 1,
        });
    }
    let layout = Arc::new(DiffRowLayout {
        max_content,
        line_starts,
        total_rows,
    });
    let mut render_cache = render_cache(cache);
    if let Some(existing) = render_cache
        .row_layouts
        .iter()
        .find(|candidate| candidate.max_content == max_content)
        .cloned()
    {
        return existing;
    }
    render_cache.row_layouts.push_back(layout.clone());
    while render_cache.row_layouts.len() > MAX_ROW_LAYOUTS {
        render_cache.row_layouts.pop_front();
    }
    layout
}

fn cache_syntax_checkpoint(
    cache: &DiffIr,
    theme_id: usize,
    line_index: usize,
    highlighter: HighlightLines<'static>,
    theme: &'static syntect::highlighting::Theme,
) -> HighlightLines<'static> {
    let (highlight_state, parse_state) = highlighter.state();
    if line_index <= u16::MAX as usize {
        let mut render_cache = render_cache(cache);
        if let Some(syntax_cache) = render_cache
            .syntax
            .as_mut()
            .filter(|syntax_cache| syntax_cache.theme_id == theme_id)
        {
            match syntax_cache
                .checkpoints
                .binary_search_by_key(&line_index, |checkpoint| checkpoint.line_index)
            {
                Ok(_) => {}
                Err(index) if syntax_cache.checkpoints.len() < MAX_SYNTAX_CHECKPOINTS => {
                    syntax_cache.checkpoints.insert(
                        index,
                        SyntaxCheckpoint {
                            line_index,
                            highlight_state: highlight_state.clone(),
                            parse_state: parse_state.clone(),
                        },
                    );
                }
                Err(_) => {}
            }
        }
    }
    HighlightLines::from_state(theme, highlight_state, parse_state)
}

fn advance_syntax_line(highlighter: &mut HighlightLines<'static>, line: &str) {
    let mut line_with_nl = String::with_capacity(line.len() + 1);
    line_with_nl.push_str(line);
    line_with_nl.push('\n');
    let _ = highlighter.highlight_line(&line_with_nl, &SYNTAX_SET);
}

fn highlighter_at_line(
    cache: &DiffIr,
    line_index: usize,
    syntax: &'static syntect::parsing::SyntaxReference,
    theme: &'static syntect::highlighting::Theme,
) -> HighlightLines<'static> {
    let theme_id = std::ptr::from_ref(theme) as usize;
    let checkpoint = {
        let mut render_cache = render_cache(cache);
        let reset = render_cache
            .syntax
            .as_ref()
            .is_none_or(|syntax_cache| syntax_cache.theme_id != theme_id);
        if reset {
            let initial = HighlightLines::new(syntax, theme);
            let (highlight_state, parse_state) = initial.state();
            render_cache.syntax = Some(DiffSyntaxCache {
                theme_id,
                checkpoints: vec![SyntaxCheckpoint {
                    line_index: 0,
                    highlight_state,
                    parse_state,
                }],
            });
        }
        render_cache
            .syntax
            .as_ref()
            .and_then(|syntax_cache| {
                syntax_cache
                    .checkpoints
                    .iter()
                    .rev()
                    .find(|checkpoint| checkpoint.line_index <= line_index)
            })
            .cloned()
            .expect("syntax cache always contains the initial checkpoint")
    };

    let mut highlighter =
        HighlightLines::from_state(theme, checkpoint.highlight_state, checkpoint.parse_state);
    let mut prefix_syntax_lines = 0usize;
    for index in checkpoint.line_index..line_index {
        if index > checkpoint.line_index && index.is_multiple_of(SYNTAX_CHECKPOINT_INTERVAL) {
            highlighter = cache_syntax_checkpoint(cache, theme_id, index, highlighter, theme);
        }
        match &cache.lines[index] {
            DiffLine::Context { text, .. }
            | DiffLine::Delete { text, .. }
            | DiffLine::Insert { text, .. } => {
                advance_syntax_line(&mut highlighter, text);
                prefix_syntax_lines = prefix_syntax_lines.saturating_add(1);
            }
            DiffLine::Ellipsis => {
                highlighter = HighlightLines::new(syntax, theme);
            }
        }
    }
    if line_index.is_multiple_of(SYNTAX_CHECKPOINT_INTERVAL) {
        highlighter = cache_syntax_checkpoint(cache, theme_id, line_index, highlighter, theme);
    }
    smelt_perf::perf::record_value(
        "render:inline_diff_cached:prefix_syntax_lines",
        prefix_syntax_lines as u64,
    );
    #[cfg(test)]
    PREFIX_SYNTAX_LINES.with(|count| {
        count.set(count.get().saturating_add(prefix_syntax_lines));
    });
    highlighter
}

fn cache_retained_file_checkpoint(
    cache: &RetainedFileViewCache,
    theme_id: usize,
    line_index: usize,
    highlighter: HighlightLines<'static>,
    theme: &'static syntect::highlighting::Theme,
) -> HighlightLines<'static> {
    let (highlight_state, parse_state) = highlighter.state();
    if line_index <= u16::MAX as usize {
        let mut syntax = cache
            .syntax
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(syntax) = syntax.as_mut().filter(|syntax| syntax.theme_id == theme_id) {
            match syntax
                .checkpoints
                .binary_search_by_key(&line_index, |checkpoint| checkpoint.line_index)
            {
                Ok(_) => {}
                Err(index) if syntax.checkpoints.len() < MAX_SYNTAX_CHECKPOINTS => {
                    syntax.checkpoints.insert(
                        index,
                        SyntaxCheckpoint {
                            line_index,
                            highlight_state: highlight_state.clone(),
                            parse_state: parse_state.clone(),
                        },
                    );
                }
                Err(_) => {}
            }
        }
    }
    HighlightLines::from_state(theme, highlight_state, parse_state)
}

fn retained_file_highlighter_at_line(
    content: &TranscriptContent,
    cache: &RetainedFileViewCache,
    line_index: usize,
    syntax: &'static syntect::parsing::SyntaxReference,
    theme: &'static syntect::highlighting::Theme,
) -> HighlightLines<'static> {
    let theme_id = std::ptr::from_ref(theme) as usize;
    let checkpoint = {
        let mut retained = cache
            .syntax
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if retained
            .as_ref()
            .is_none_or(|retained| retained.theme_id != theme_id)
        {
            let initial = HighlightLines::new(syntax, theme);
            let (highlight_state, parse_state) = initial.state();
            *retained = Some(DiffSyntaxCache {
                theme_id,
                checkpoints: vec![SyntaxCheckpoint {
                    line_index: 0,
                    highlight_state,
                    parse_state,
                }],
            });
        }
        retained
            .as_ref()
            .and_then(|retained| {
                retained
                    .checkpoints
                    .iter()
                    .rev()
                    .find(|checkpoint| checkpoint.line_index <= line_index)
            })
            .cloned()
            .expect("retained file syntax cache contains its initial checkpoint")
    };

    let mut highlighter =
        HighlightLines::from_state(theme, checkpoint.highlight_state, checkpoint.parse_state);
    let content = content.read();
    for index in checkpoint.line_index..line_index {
        if index > checkpoint.line_index && index.is_multiple_of(SYNTAX_CHECKPOINT_INTERVAL) {
            highlighter =
                cache_retained_file_checkpoint(cache, theme_id, index, highlighter, theme);
        }
        let Some(line_range) = content.line_range(index) else {
            break;
        };
        if line_range.len() > MAX_HIGHLIGHT_LINE_BYTES {
            highlighter = HighlightLines::new(syntax, theme);
            continue;
        }
        let line = content.slice(line_range);
        advance_syntax_line(&mut highlighter, &line);
    }
    drop(content);
    if line_index.is_multiple_of(SYNTAX_CHECKPOINT_INTERVAL) {
        highlighter =
            cache_retained_file_checkpoint(cache, theme_id, line_index, highlighter, theme);
    }
    highlighter
}

fn visit_render_span_rows(
    spans: &[RenderSpan],
    max_width: usize,
    row_range: std::ops::Range<usize>,
    mut visit: impl FnMut(usize, &[RenderSpan]),
) {
    let max_width = max_width.max(1);
    let start_row = row_range.start;
    let end_row = row_range.end.max(start_row);
    if start_row == end_row {
        return;
    }

    let mut row_index = 0usize;
    let mut row = Vec::new();
    let mut column = 0usize;
    for span in spans {
        let mut segment_start = 0usize;
        for (offset, ch) in span.text.char_indices() {
            let width = smelt_buffer::cell_width::char_width(ch);
            if column.saturating_add(width) > max_width && column > 0 {
                if (start_row..end_row).contains(&row_index) {
                    push_render_span(
                        &mut row,
                        smelt_buffer::text::slice(&span.text, segment_start..offset).to_owned(),
                        span.meta.clone(),
                    );
                    visit(row_index, &row);
                }
                row.clear();
                row_index = row_index.saturating_add(1);
                if row_index >= end_row {
                    return;
                }
                column = 0;
                segment_start = offset;
            }
            column = column.saturating_add(width);
        }
        if (start_row..end_row).contains(&row_index) {
            push_render_span(
                &mut row,
                smelt_buffer::text::slice(&span.text, segment_start..span.text.len()).to_owned(),
                span.meta.clone(),
            );
        }
    }
    if (start_row..end_row).contains(&row_index) {
        visit(row_index, &row);
    }
}

fn emit_retained_file_prefix(
    out: &mut LineBuilder,
    gutter: GutterStyle,
    indent_text: &str,
    line: usize,
    visual_row: usize,
    lineno_digits: usize,
    blank_prefix: &str,
) {
    if !indent_text.is_empty() {
        out.print_gutter(indent_text);
    }
    emit_diff_prefix(
        out,
        gutter,
        smelt_buffer::buffer::SourceLine::Linear {
            lineno: line.saturating_add(1).min(u32::MAX as usize) as u32,
        },
        visual_row,
        lineno_digits,
        blank_prefix,
    );
    out.print("  ");
}

fn print_file_text(out: &mut LineBuilder, text: &str) {
    let mut start = 0usize;
    for (offset, ch) in text.char_indices() {
        if ch != '\t' {
            continue;
        }
        out.print(smelt_buffer::text::slice(text, start..offset));
        out.print("    ");
        start = offset.saturating_add(ch.len_utf8());
    }
    out.print(smelt_buffer::text::slice(text, start..text.len()));
}

fn retained_file_widths(
    content: &TranscriptContent,
    width: u16,
    gutter: GutterStyle,
    indent_cells: u16,
) -> (usize, u16) {
    let line_count = content.read().logical_line_count();
    let lineno_digits = line_count.max(1).to_string().len();
    let prefix_cells = match gutter {
        GutterStyle::Stamped => 0,
        GutterStyle::InlineLineNumbers => lineno_digits + 2,
        GutterStyle::None => 0,
    };
    let layout_width = if width == 0 {
        default_width()
    } else {
        usize::from(width)
    };
    let max_content = layout_width
        .saturating_sub(usize::from(indent_cells) + prefix_cells + 2)
        .max(1);
    (max_content, max_content.min(u16::MAX as usize) as u16)
}

pub fn measure_retained_file_view(
    content: &TranscriptContent,
    width: u16,
    gutter: GutterStyle,
    indent_cells: u16,
) -> usize {
    let (_, content_width) = retained_file_widths(content, width, gutter, indent_cells);
    content.file_layout_rows(content_width)
}

pub fn measure_retained_file_view_edge(
    content: &TranscriptContent,
    width: u16,
    gutter: GutterStyle,
    indent_cells: u16,
    max_rows: usize,
) -> ContentTextWindow {
    let (_, content_width) = retained_file_widths(content, width, gutter, indent_cells);
    content.visit_text_layout_head_rows(content_width, false, max_rows, |_| {})
}

#[allow(clippy::too_many_arguments)]
pub fn print_retained_file_view_edge(
    out: &mut LineBuilder,
    content: &TranscriptContent,
    cache: &RetainedFileViewCache,
    syntax_ext: &str,
    gutter: GutterStyle,
    indent_cells: u16,
    layout_width: u16,
    max_rows: usize,
    tail: bool,
) -> ContentTextWindow {
    if max_rows == 0 {
        return ContentTextWindow {
            row_count: 0,
            truncated: !content.is_empty(),
        };
    }
    if content.len() <= MAX_HIGHLIGHT_FILE_BYTES {
        let total = measure_retained_file_view(content, layout_width, gutter, indent_cells);
        let row_count = total.min(max_rows);
        let skip = if tail {
            total.saturating_sub(row_count)
        } else {
            0
        };
        print_retained_file_view(
            out,
            content,
            cache,
            syntax_ext,
            gutter,
            indent_cells,
            layout_width,
            skip,
            row_count,
        );
        return ContentTextWindow {
            row_count,
            truncated: total > row_count,
        };
    }

    let line_count = content.read().logical_line_count();
    let lineno_digits = line_count.max(1).to_string().len();
    let prefix_cells = match gutter {
        GutterStyle::Stamped => 0,
        GutterStyle::InlineLineNumbers => lineno_digits + 2,
        GutterStyle::None => 0,
    };
    let blank_prefix = " ".repeat(prefix_cells);
    let indent_text = " ".repeat(usize::from(indent_cells));
    let (_, content_width) = retained_file_widths(content, layout_width, gutter, indent_cells);
    out.mark_wrapped();
    let mut emit = |row: crate::transcript_content::ContentTextRow<'_>| {
        emit_retained_file_prefix(
            out,
            gutter,
            &indent_text,
            row.logical_line(),
            row.row_offset(),
            lineno_digits,
            &blank_prefix,
        );
        row.visit_text(|text| print_file_text(out, text));
        out.newline();
    };
    if tail {
        content.visit_text_layout_tail_rows(content_width, false, max_rows, &mut emit)
    } else {
        content.visit_text_layout_head_rows(content_width, false, max_rows, &mut emit)
    }
}

#[allow(clippy::too_many_arguments)]
pub fn print_retained_file_view(
    out: &mut LineBuilder,
    content: &TranscriptContent,
    cache: &RetainedFileViewCache,
    syntax_ext: &str,
    gutter: GutterStyle,
    indent_cells: u16,
    layout_width: u16,
    skip: usize,
    max_rows: usize,
) -> u16 {
    let _perf = smelt_perf::perf::begin("render:retained_file_view");
    let line_count = content.read().logical_line_count();
    if line_count == 0 {
        return 0;
    }
    let lineno_digits = line_count.max(1).to_string().len();
    let prefix_cells = match gutter {
        GutterStyle::Stamped => 0,
        GutterStyle::InlineLineNumbers => lineno_digits + 2,
        GutterStyle::None => 0,
    };
    let blank_prefix = " ".repeat(prefix_cells);
    let indent_text = " ".repeat(usize::from(indent_cells));
    let (max_content, content_width) =
        retained_file_widths(content, layout_width, gutter, indent_cells);
    let emit_limit = if max_rows == 0 { usize::MAX } else { max_rows };
    let row_end = skip.saturating_add(emit_limit);
    let ranges = content.file_layout_ranges(content_width, skip..row_end);
    let Some(first_line) = ranges.first().map(|range| range.line) else {
        return 0;
    };
    if content.len() > MAX_HIGHLIGHT_FILE_BYTES {
        let mut emitted = 0u16;
        out.mark_wrapped();
        for range in ranges {
            let mut visual_row = range.row_offset;
            let row_end = range.row_offset.saturating_add(range.row_count);
            content.visit_file_layout_line_rows(
                content_width,
                range.line,
                range.row_offset..row_end,
                |row| {
                    emit_retained_file_prefix(
                        out,
                        gutter,
                        &indent_text,
                        range.line,
                        visual_row,
                        lineno_digits,
                        &blank_prefix,
                    );
                    row.visit_text(|text| print_file_text(out, text));
                    out.newline();
                    emitted = emitted.saturating_add(1);
                    visual_row = visual_row.saturating_add(1);
                },
            );
        }
        return emitted;
    }

    let syntax = SYNTAX_SET
        .find_syntax_by_extension(syntax_ext)
        .unwrap_or_else(|| SYNTAX_SET.find_syntax_plain_text());
    let theme = syntax_theme(out.theme());
    let theme_id = std::ptr::from_ref(theme) as usize;
    let mut highlighter =
        retained_file_highlighter_at_line(content, cache, first_line, syntax, theme);
    let mut emitted = 0u16;
    out.mark_wrapped();
    for range in ranges {
        if range.line > first_line && range.line.is_multiple_of(SYNTAX_CHECKPOINT_INTERVAL) {
            highlighter =
                cache_retained_file_checkpoint(cache, theme_id, range.line, highlighter, theme);
        }
        let Some(line_range) = content.read().line_range(range.line) else {
            break;
        };
        let row_end = range.row_offset.saturating_add(range.row_count);
        if line_range.len() > MAX_HIGHLIGHT_LINE_BYTES {
            let mut visual_row = range.row_offset;
            content.visit_file_layout_line_rows(
                content_width,
                range.line,
                range.row_offset..row_end,
                |row| {
                    emit_retained_file_prefix(
                        out,
                        gutter,
                        &indent_text,
                        range.line,
                        visual_row,
                        lineno_digits,
                        &blank_prefix,
                    );
                    row.visit_text(|text| print_file_text(out, text));
                    out.newline();
                    emitted = emitted.saturating_add(1);
                    visual_row = visual_row.saturating_add(1);
                },
            );
            highlighter = HighlightLines::new(syntax, theme);
            continue;
        }

        let spans = {
            let read = content.read();
            let line = read.slice(line_range);
            let expanded = line.contains('\t').then(|| line.replace('\t', "    "));
            let line = expanded.as_deref().unwrap_or(&line);
            syntax_spans_for_line_with_highlights(&mut highlighter, line, &[])
        };
        visit_render_span_rows(
            &spans,
            max_content,
            range.row_offset..row_end,
            |visual_row, row| {
                emit_retained_file_prefix(
                    out,
                    gutter,
                    &indent_text,
                    range.line,
                    visual_row,
                    lineno_digits,
                    &blank_prefix,
                );
                print_syntax_spans(out, row, None, None);
                out.newline();
                emitted = emitted.saturating_add(1);
            },
        );
    }
    emitted
}

pub fn measure_retained_code_block(content: &TranscriptContent, width: u16) -> usize {
    if content.is_empty() {
        1
    } else {
        content.file_layout_rows(width.max(1))
    }
}

pub fn measure_retained_code_block_edge(
    content: &TranscriptContent,
    width: u16,
    max_rows: usize,
) -> ContentTextWindow {
    if content.is_empty() {
        return ContentTextWindow {
            row_count: usize::from(max_rows > 0),
            truncated: max_rows == 0,
        };
    }
    content.visit_text_layout_head_rows(width.max(1), false, max_rows, |_| {})
}

pub fn print_retained_code_block_edge(
    out: &mut LineBuilder,
    content: &TranscriptContent,
    cache: &RetainedFileViewCache,
    lang: &str,
    width: u16,
    max_rows: usize,
    tail: bool,
) -> ContentTextWindow {
    if max_rows == 0 {
        return ContentTextWindow {
            row_count: 0,
            truncated: true,
        };
    }
    if content.len() <= MAX_HIGHLIGHT_FILE_BYTES {
        let total = measure_retained_code_block(content, width);
        let row_count = total.min(max_rows);
        let skip = if tail {
            total.saturating_sub(row_count)
        } else {
            0
        };
        print_retained_code_block(out, content, cache, lang, width, skip, row_count);
        return ContentTextWindow {
            row_count,
            truncated: total > row_count,
        };
    }

    let width = width.max(1);
    let bg = out
        .theme()
        .resolve(crate::theme::intern("SmeltCodeBlockBg"))
        .bg
        .unwrap_or(Color::Reset);
    out.mark_wrapped();
    let mut emit = |row: crate::transcript_content::ContentTextRow<'_>| {
        let text = row.text();
        if row.row_offset() == 0 {
            out.set_source_text(&text);
        } else {
            out.mark_soft_wrap_continuation();
        }
        out.set_bg(bg);
        print_file_text(out, &text);
        out.fill_line_bg(bg);
        out.newline();
    };
    if tail {
        content.visit_text_layout_tail_rows(width, false, max_rows, &mut emit)
    } else {
        content.visit_text_layout_head_rows(width, false, max_rows, &mut emit)
    }
}

pub fn print_retained_code_block(
    out: &mut LineBuilder,
    content: &TranscriptContent,
    cache: &RetainedFileViewCache,
    lang: &str,
    width: u16,
    skip: usize,
    max_rows: usize,
) -> u16 {
    let _perf = smelt_perf::perf::begin("render:retained_code_block");
    let width = width.max(1);
    let bg = out
        .theme()
        .resolve(crate::theme::intern("SmeltCodeBlockBg"))
        .bg
        .unwrap_or(Color::Reset);
    if content.is_empty() {
        if skip > 0 {
            return 0;
        }
        out.set_source_text("");
        out.fill_line_bg(bg);
        out.newline();
        return 1;
    }

    let emit_limit = if max_rows == 0 { usize::MAX } else { max_rows };
    let row_end = skip.saturating_add(emit_limit);
    let ranges = content.file_layout_ranges(width, skip..row_end);
    let Some(first_line) = ranges.first().map(|range| range.line) else {
        return 0;
    };
    out.mark_wrapped();

    if content.len() > MAX_HIGHLIGHT_FILE_BYTES {
        let mut emitted = 0u16;
        for range in ranges {
            let source_line = content
                .read()
                .line(range.line)
                .map(Cow::into_owned)
                .unwrap_or_default();
            let copy_line = expanded_code_line(&source_line);
            let mut visual_row = range.row_offset;
            let row_end = range.row_offset.saturating_add(range.row_count);
            content.visit_file_layout_line_rows(
                width,
                range.line,
                range.row_offset..row_end,
                |row| {
                    emit_retained_code_row_start(out, &copy_line, visual_row);
                    out.set_bg(bg);
                    row.visit_text(|text| print_file_text(out, text));
                    out.fill_line_bg(bg);
                    out.newline();
                    emitted = emitted.saturating_add(1);
                    visual_row = visual_row.saturating_add(1);
                },
            );
        }
        return emitted;
    }

    let syntax = SYNTAX_SET
        .find_syntax_by_extension(super::lang_to_ext(lang))
        .or_else(|| SYNTAX_SET.find_syntax_by_name(lang))
        .unwrap_or_else(|| SYNTAX_SET.find_syntax_plain_text());
    let theme = syntax_theme(out.theme());
    let theme_id = std::ptr::from_ref(theme) as usize;
    let mut highlighter =
        retained_file_highlighter_at_line(content, cache, first_line, syntax, theme);
    let mut emitted = 0u16;
    for range in ranges {
        if range.line > first_line && range.line.is_multiple_of(SYNTAX_CHECKPOINT_INTERVAL) {
            highlighter =
                cache_retained_file_checkpoint(cache, theme_id, range.line, highlighter, theme);
        }
        let Some(line_range) = content.read().line_range(range.line) else {
            break;
        };
        let source_line = content.read().slice(line_range.clone()).into_owned();
        let copy_line = expanded_code_line(&source_line);
        let row_end = range.row_offset.saturating_add(range.row_count);
        if line_range.len() > MAX_HIGHLIGHT_LINE_BYTES {
            let mut visual_row = range.row_offset;
            content.visit_file_layout_line_rows(
                width,
                range.line,
                range.row_offset..row_end,
                |row| {
                    emit_retained_code_row_start(out, &copy_line, visual_row);
                    out.set_bg(bg);
                    row.visit_text(|text| print_file_text(out, text));
                    out.fill_line_bg(bg);
                    out.newline();
                    emitted = emitted.saturating_add(1);
                    visual_row = visual_row.saturating_add(1);
                },
            );
            highlighter = HighlightLines::new(syntax, theme);
            continue;
        }

        let syntax_line = copy_line.as_ref();
        let spans = syntax_spans_for_line_with_highlights(&mut highlighter, syntax_line, &[]);
        visit_render_span_rows(
            &spans,
            usize::from(width),
            range.row_offset..row_end,
            |visual_row, row| {
                emit_retained_code_row_start(out, syntax_line, visual_row);
                let cols = print_syntax_spans(out, row, Some(bg), None);
                if cols < usize::from(width) {
                    out.fill_line_bg(bg);
                }
                out.newline();
                emitted = emitted.saturating_add(1);
            },
        );
    }
    emitted
}

fn expanded_code_line(line: &str) -> Cow<'_, str> {
    if line.contains('\t') {
        Cow::Owned(line.replace('\t', "    "))
    } else {
        Cow::Borrowed(line)
    }
}

fn emit_retained_code_row_start(out: &mut LineBuilder, source_line: &str, visual_row: usize) {
    if visual_row == 0 {
        out.set_source_text(source_line);
    } else {
        out.mark_soft_wrap_continuation();
    }
}

pub fn measure_diff_ir(
    cache: &DiffIr,
    width: u16,
    gutter: GutterStyle,
    indent_cells: u16,
) -> usize {
    let lineno_digits = format!("{}", cache.max_display_lineno).len();
    let prefix_cells = match gutter {
        GutterStyle::Stamped => 0,
        GutterStyle::InlineLineNumbers => lineno_digits + 2,
        GutterStyle::None => 0,
    };
    let sign_prefix = 2;
    let layout_width = if width == 0 {
        default_width()
    } else {
        width as usize
    };
    let max_content = layout_width
        .saturating_sub(indent_cells as usize + prefix_cells + sign_prefix)
        .max(1);

    diff_row_layout(cache, max_content).total_rows
}

pub fn print_diff_ir(
    out: &mut LineBuilder,
    cache: &DiffIr,
    gutter: GutterStyle,
    indent_cells: u16,
    skip: u16,
    max_rows: u16,
) -> u16 {
    let layout_width = out.layout_width();
    print_diff_ir_with_width(
        out,
        cache,
        gutter,
        indent_cells,
        layout_width,
        usize::from(skip),
        usize::from(max_rows),
    )
}

pub fn print_diff_ir_with_width(
    out: &mut LineBuilder,
    cache: &DiffIr,
    gutter: GutterStyle,
    indent_cells: u16,
    layout_width: u16,
    skip: usize,
    max_rows: usize,
) -> u16 {
    let _perf = smelt_perf::perf::begin("render:inline_diff_cached");

    // Single line-number column (` N `). The body sign ("+ "/"- "/"  ") is always
    // written inline regardless of mode. `Stamped` hands off the line number via
    // `SourceLine`; the host window's `LineNumberGutter` paints the column outside
    // the content width - so the in-content prefix is just the sign.
    let lineno_digits = format!("{}", cache.max_display_lineno).len();
    let prefix_cells = match gutter {
        GutterStyle::Stamped => 0,
        GutterStyle::InlineLineNumbers => lineno_digits + 2,
        GutterStyle::None => 0,
    };
    let sign_prefix = 2;
    let indent = indent_cells as usize;
    let indent_str = " ".repeat(indent);
    let layout_width = if layout_width == 0 {
        default_width()
    } else {
        layout_width as usize
    };
    let max_content = layout_width
        .saturating_sub(indent + prefix_cells + sign_prefix)
        .max(1);
    let blank_prefix = " ".repeat(prefix_cells);
    // Content re-wraps per row at `layout_width`, so the layout is width-pinned.
    out.mark_wrapped();
    let emit_limit = if max_rows == 0 {
        u16::MAX
    } else {
        max_rows.min(usize::from(u16::MAX)) as u16
    };
    // Diff row fills come from the render theme. Themes that omit
    // `SmeltDiffAddBg` / `SmeltDiffDeleteBg` produce diffs without a row
    // background (text still highlights via syntax colors).
    let palette = diff_palette(out.theme());

    let syntax = SYNTAX_SET
        .find_syntax_by_extension(&cache.syntax_ext)
        .unwrap_or_else(|| SYNTAX_SET.find_syntax_plain_text());
    let syntax_theme = syntax_theme(out.theme());
    let row_layout = diff_row_layout(cache, max_content);
    let Some(first_line_index) = row_layout.line_index_for_row(skip) else {
        smelt_perf::perf::record_value("render:inline_diff_cached:source_lines", 0);
        return 0;
    };
    let mut h = highlighter_at_line(cache, first_line_index, syntax, syntax_theme);
    let theme_id = std::ptr::from_ref(syntax_theme) as usize;

    let mut seen_rows = row_layout.line_starts[first_line_index];
    let mut emitted = 0u16;
    let mut source_lines = 0u64;
    'lines: for (line_index, line) in cache.lines.iter().enumerate().skip(first_line_index) {
        if emitted >= emit_limit {
            break;
        }
        if line_index > first_line_index && line_index.is_multiple_of(SYNTAX_CHECKPOINT_INTERVAL) {
            h = cache_syntax_checkpoint(cache, theme_id, line_index, h, syntax_theme);
        }
        match line {
            DiffLine::Ellipsis => {
                h = HighlightLines::new(syntax, syntax_theme);
                if seen_rows < skip {
                    seen_rows = seen_rows.saturating_add(1);
                    continue;
                }
                if indent > 0 {
                    out.print_gutter(&indent_str);
                }
                if matches!(gutter, GutterStyle::Stamped) {
                    out.set_source_line(smelt_buffer::buffer::SourceLine::Synthetic);
                } else if prefix_cells > 0 {
                    out.print(&blank_prefix);
                }
                out.set_fg(Color::DarkGrey);
                out.print("⋮");
                out.reset_style();
                out.newline();
                emitted = emitted.saturating_add(1);
                seen_rows = seen_rows.saturating_add(1);
            }
            _ => {
                let (source_line, sign, bg, inline_bg, text, highlights) = match line {
                    DiffLine::Context { lineno, text } => (
                        smelt_buffer::buffer::SourceLine::Linear {
                            lineno: *lineno as u32,
                        },
                        None,
                        None,
                        None,
                        text.as_str(),
                        &[][..],
                    ),
                    DiffLine::Delete {
                        lineno,
                        text,
                        highlights,
                    } => (
                        smelt_buffer::buffer::SourceLine::Linear {
                            lineno: *lineno as u32,
                        },
                        Some(('-', Color::Red)),
                        palette.del.row_bg,
                        palette.del.inline_bg,
                        text.as_str(),
                        highlights.as_slice(),
                    ),
                    DiffLine::Insert {
                        lineno,
                        text,
                        highlights,
                    } => (
                        smelt_buffer::buffer::SourceLine::Linear {
                            lineno: *lineno as u32,
                        },
                        Some(('+', Color::Green)),
                        palette.add.row_bg,
                        palette.add.inline_bg,
                        text.as_str(),
                        highlights.as_slice(),
                    ),
                    DiffLine::Ellipsis => unreachable!(),
                };
                source_lines = source_lines.saturating_add(1);
                let visual_rows = split_syntax_spans_into_rows_with_highlights(
                    &mut h,
                    text,
                    highlights,
                    max_content,
                );
                let pad_meta = SpanMeta::unselectable();
                for (vi, vrow) in visual_rows.iter().enumerate() {
                    if seen_rows < skip {
                        seen_rows = seen_rows.saturating_add(1);
                        continue;
                    }
                    if emitted >= emit_limit {
                        break 'lines;
                    }
                    // For delete/insert rows the bg extends under the indent
                    // (the leftmost cells of the row) and across the trailing
                    // pad, so the strip reads as a single change-band. Indent
                    // and pad cells stay non-selectable; `pad_row_to_layout_width`
                    // emits the trailing spaces with the active bg.
                    if let Some(bgv) = bg {
                        out.set_bg(bgv);
                    }
                    if indent > 0 {
                        out.print_gutter(&indent_str);
                    }
                    emit_diff_prefix(out, gutter, source_line, vi, lineno_digits, &blank_prefix);
                    if let Some((ch, color)) = sign {
                        // bg is None when the theme doesn't define
                        // `SmeltDiffAddBg` / `SmeltDiffDeleteBg`; in that
                        // case skip the row-fill but still emit the sign.
                        if let Some(bgv) = bg {
                            out.set_bg(bgv);
                        }
                        if vi == 0 {
                            out.set_fg(color);
                            out.print(&format!("{} ", ch));
                        } else {
                            out.print("  ");
                        }
                        print_syntax_spans(out, vrow, bg, inline_bg);
                        if let Some(bgv) = bg {
                            out.set_bg(bgv);
                            out.pad_row_to_layout_width(pad_meta.clone());
                        }
                        out.reset_style();
                    } else {
                        out.print("  ");
                        print_syntax_spans(out, vrow, None, None);
                    }
                    out.newline();
                    emitted = emitted.saturating_add(1);
                    seen_rows = seen_rows.saturating_add(1);
                }
            }
        }
    }
    smelt_perf::perf::record_value("render:inline_diff_cached:source_lines", source_lines);
    emitted
}

/// Emit the diff row's left-of-content prefix per the chosen gutter style. `Stamped`
/// hands off via `SourceLine` (host window's `LineNumberGutter` paints the column);
/// `InlineLineNumbers` writes a local ` N ` column with the theme's `Comment` group;
/// `None` writes nothing.
fn emit_diff_prefix(
    out: &mut LineBuilder,
    gutter: GutterStyle,
    source_line: smelt_buffer::buffer::SourceLine,
    vi: usize,
    lineno_digits: usize,
    blank_prefix: &str,
) {
    match gutter {
        GutterStyle::Stamped => {
            out.stamp_chunk(vi, source_line);
        }
        GutterStyle::InlineLineNumbers => {
            out.push_hl(crate::theme::intern("Comment"));
            if vi == 0 {
                let lineno = match source_line {
                    smelt_buffer::buffer::SourceLine::Linear { lineno } => Some(lineno),
                    smelt_buffer::buffer::SourceLine::Diff { new, .. } => new,
                    _ => None,
                };
                out.print_gutter(&format!(
                    " {:>w$} ",
                    lineno.map(|n| n.to_string()).unwrap_or_default(),
                    w = lineno_digits
                ));
            } else {
                out.print_gutter(blank_prefix);
            }
            out.pop_style();
        }
        GutterStyle::None => {}
    }
}

// ─── Side-by-side diff renderer ──────────────────────────────────────
//
// Decoupled from output: `compute_split_diff` returns a `SplitDiffPlan`
// describing every row on both sides; `print_split_diff_side` paints one
// side from the plan. Two passes share the same plan, so callers can
// render to two buffers without holding both `LineBuilder`s live at once.
// The convenience `print_split_diff` wraps both passes when the caller
// can hand over both sinks simultaneously (used in tests).

/// One visual row of a side-by-side diff. Synthesised lines (padding
/// where one side has fewer rows in a delete/insert block) are encoded as
/// `None`; concrete rows carry the source line number for the gutter and
/// a `removed` flag that controls per-line bg + syntax-highlight scope.
#[derive(Clone, Debug)]
pub struct SplitDiffRow {
    pub left: Option<SplitDiffCell>,
    pub right: Option<SplitDiffCell>,
}

#[derive(Clone, Debug)]
pub struct SplitDiffCell {
    pub text: String,
    pub lineno: u32,
    /// True for delete/insert rows (paints the row-fill bg); false for
    /// `Equal` context rows.
    pub changed: bool,
    pub highlights: Vec<DiffByteRange>,
}

/// Self-contained diff IR. Computed once, then replayed per side.
#[derive(Clone, Debug)]
pub struct SplitDiffPlan {
    pub rows: Vec<SplitDiffRow>,
}

/// Walk `old` vs `new` at line granularity and produce an aligned row plan.
/// Consecutive delete/insert blocks are paired by line similarity, preserving
/// order, so intraline highlighting compares corresponding lines instead of
/// blindly zipping unrelated rows. Unmatched rows get full-line highlights and
/// `None` padding on the opposite side.
pub fn compute_split_diff(old: &str, new: &str) -> SplitDiffPlan {
    let _perf = smelt_perf::perf::begin("render:compute_split_diff");
    let diff = TextDiff::from_lines(old, new);
    let mut rows: Vec<SplitDiffRow> = Vec::new();
    let mut old_lineno: u32 = 0;
    let mut new_lineno: u32 = 0;
    let mut pending_dels: Vec<SplitDiffCell> = Vec::new();
    let mut pending_ins: Vec<SplitDiffCell> = Vec::new();

    let flush = |rows: &mut Vec<SplitDiffRow>,
                 dels: &mut Vec<SplitDiffCell>,
                 ins: &mut Vec<SplitDiffCell>| {
        let old_lines: Vec<&str> = dels.iter().map(|cell| cell.text.as_str()).collect();
        let new_lines: Vec<&str> = ins.iter().map(|cell| cell.text.as_str()).collect();

        for alignment in align_changed_lines(&old_lines, &new_lines) {
            match alignment {
                LineAlignment::Pair { old, new } => {
                    let mut left = dels[old].clone();
                    let mut right = ins[new].clone();
                    let (left_highlights, right_highlights) =
                        inline_highlights_for_pair(&left.text, &right.text);
                    left.highlights = left_highlights;
                    right.highlights = right_highlights;
                    rows.push(SplitDiffRow {
                        left: Some(left),
                        right: Some(right),
                    });
                }
                LineAlignment::OldOnly(old) => {
                    let mut left = dels[old].clone();
                    left.highlights = full_line_highlight(&left.text);
                    rows.push(SplitDiffRow {
                        left: Some(left),
                        right: None,
                    });
                }
                LineAlignment::NewOnly(new) => {
                    let mut right = ins[new].clone();
                    right.highlights = full_line_highlight(&right.text);
                    rows.push(SplitDiffRow {
                        left: None,
                        right: Some(right),
                    });
                }
            }
        }

        dels.clear();
        ins.clear();
    };

    for change in diff.iter_all_changes() {
        let text = change.value().trim_end_matches('\n').replace('\t', "    ");
        match change.tag() {
            ChangeTag::Equal => {
                flush(&mut rows, &mut pending_dels, &mut pending_ins);
                old_lineno += 1;
                new_lineno += 1;
                let left = SplitDiffCell {
                    text: text.clone(),
                    lineno: old_lineno,
                    changed: false,
                    highlights: Vec::new(),
                };
                let right = SplitDiffCell {
                    text,
                    lineno: new_lineno,
                    changed: false,
                    highlights: Vec::new(),
                };
                rows.push(SplitDiffRow {
                    left: Some(left),
                    right: Some(right),
                });
            }
            ChangeTag::Delete => {
                old_lineno += 1;
                pending_dels.push(SplitDiffCell {
                    text,
                    lineno: old_lineno,
                    changed: true,
                    highlights: Vec::new(),
                });
            }
            ChangeTag::Insert => {
                new_lineno += 1;
                pending_ins.push(SplitDiffCell {
                    text,
                    lineno: new_lineno,
                    changed: true,
                    highlights: Vec::new(),
                });
            }
        }
    }
    flush(&mut rows, &mut pending_dels, &mut pending_ins);
    SplitDiffPlan { rows }
}

/// Which side of the plan to render.
#[derive(Clone, Copy)]
pub enum SplitSide {
    Left,
    Right,
}

fn paint_diff_line(
    out: &mut LineBuilder,
    text: &str,
    h: &mut HighlightLines,
    bg: Option<Color>,
    inline_bg: Option<Color>,
    highlights: &[DiffByteRange],
    source_line: smelt_buffer::buffer::SourceLine,
) {
    if let Some(bg) = bg {
        out.fill_line_bg(bg);
    }
    out.set_source_line(source_line);
    let spans = syntax_spans_for_line_with_highlights(h, text, highlights);
    print_syntax_spans(out, &spans, bg, inline_bg);
    out.reset_style();
    out.newline();
}

fn paint_synthetic_row(out: &mut LineBuilder) {
    out.set_source_line(smelt_buffer::buffer::SourceLine::Synthetic);
    out.newline();
}

/// Render one side of `plan` into `out`. Delete rows get the dark-red
/// row fill on the left side, insert rows the dark-green fill on the
/// right; equal rows are highlighted but not filled; `None` cells emit
/// a synthetic padding row.
pub fn print_split_diff_side(
    out: &mut LineBuilder,
    plan: &SplitDiffPlan,
    syntax_ext: Option<&str>,
    side: SplitSide,
) {
    let _perf = smelt_perf::perf::begin("render:split_diff_side");
    let ext = syntax_ext.unwrap_or("txt");
    let syntax = SYNTAX_SET
        .find_syntax_by_extension(ext)
        .unwrap_or_else(|| SYNTAX_SET.find_syntax_plain_text());
    let theme = syntax_theme(out.theme());
    let mut h = HighlightLines::new(syntax, theme);
    let side_palette = diff_palette(out.theme()).split_side(side);
    for row in &plan.rows {
        let cell = match side {
            SplitSide::Left => row.left.as_ref(),
            SplitSide::Right => row.right.as_ref(),
        };
        match cell {
            Some(c) => paint_diff_line(
                out,
                &c.text,
                &mut h,
                if c.changed { side_palette.row_bg } else { None },
                if c.changed {
                    side_palette.inline_bg
                } else {
                    None
                },
                &c.highlights,
                smelt_buffer::buffer::SourceLine::Linear { lineno: c.lineno },
            ),
            None => paint_synthetic_row(out),
        }
    }
}

/// Convenience: render both sides in one call when the caller can hold
/// both `LineBuilder`s simultaneously. Equivalent to computing the plan
/// once and calling [`print_split_diff_side`] for each side.
pub fn print_split_diff(
    left: &mut LineBuilder,
    right: &mut LineBuilder,
    old: &str,
    new: &str,
    syntax_ext: Option<&str>,
) {
    let plan = compute_split_diff(old, new);
    print_split_diff_side(left, &plan, syntax_ext, SplitSide::Left);
    print_split_diff_side(right, &plan, syntax_ext, SplitSide::Right);
}

#[cfg(test)]
mod split_tests {
    use super::*;
    use crate::content::builder::render_into;
    use smelt_buffer::buffer::{BufCreateOpts, BufId, Buffer, SourceLine};
    use smelt_buffer::theme::Theme;

    fn split(old: &str, new: &str) -> (Buffer, Buffer) {
        let theme = Theme::default();
        let mut left = Buffer::new(BufId(0), BufCreateOpts::default());
        let mut right = Buffer::new(BufId(1), BufCreateOpts::default());
        render_into(&mut left, 80, &theme, |lsink| {
            render_into(&mut right, 80, &theme, |rsink| {
                print_split_diff(lsink, rsink, old, new, Some("txt"));
            });
        });
        (left, right)
    }

    fn texts(buf: &Buffer) -> Vec<String> {
        (0..buf.line_count())
            .map(|r| buf.get_line(r).unwrap_or("").to_string())
            .collect()
    }

    fn source_lines(buf: &Buffer) -> Vec<Option<SourceLine>> {
        (0..buf.line_count())
            .map(|r| buf.decoration_at(r).source_line)
            .collect()
    }

    #[test]
    fn split_diff_pairs_one_to_one_change() {
        let (l, r) = split("alpha\nbeta\ngamma\n", "alpha\nBETA\ngamma\n");
        assert_eq!(l.line_count(), r.line_count());
        assert_eq!(texts(&l), vec!["alpha", "beta", "gamma"]);
        assert_eq!(texts(&r), vec!["alpha", "BETA", "gamma"]);
        for sl in source_lines(&l).iter().chain(source_lines(&r).iter()) {
            assert!(matches!(sl, Some(SourceLine::Linear { .. })));
        }
    }

    fn highlighted_text(cell: &SplitDiffCell) -> String {
        cell.highlights
            .iter()
            .map(|range| &cell.text[range.start..range.end])
            .collect()
    }

    #[test]
    fn split_diff_marks_changed_characters_inside_paired_rows() {
        let plan = compute_split_diff("let x = 1;\n", "let x = 42;\n");
        let row = plan
            .rows
            .iter()
            .find(|row| row.left.as_ref().is_some_and(|cell| cell.changed))
            .expect("changed row");
        let left = row.left.as_ref().unwrap();
        let right = row.right.as_ref().unwrap();
        assert_eq!(highlighted_text(left), "1");
        assert_eq!(highlighted_text(right), "42");
    }

    #[test]
    fn split_diff_aligns_similar_lines_inside_change_block() {
        let plan = compute_split_diff(
            "let name = user.name();\nlet age = user.age();\nrender(name, age);\n",
            "let id = user.id();\nlet name = user.display_name();\nlet age = user.years();\nrender_user(name, age);\n",
        );
        let changed: Vec<(Option<String>, Option<String>)> = plan
            .rows
            .iter()
            .filter(|row| {
                row.left.as_ref().is_some_and(|cell| cell.changed)
                    || row.right.as_ref().is_some_and(|cell| cell.changed)
            })
            .map(|row| {
                (
                    row.left.as_ref().map(|cell| cell.text.clone()),
                    row.right.as_ref().map(|cell| cell.text.clone()),
                )
            })
            .collect();

        assert_eq!(
            changed,
            vec![
                (None, Some("let id = user.id();".to_string())),
                (
                    Some("let name = user.name();".to_string()),
                    Some("let name = user.display_name();".to_string()),
                ),
                (
                    Some("let age = user.age();".to_string()),
                    Some("let age = user.years();".to_string()),
                ),
                (
                    Some("render(name, age);".to_string()),
                    Some("render_user(name, age);".to_string()),
                ),
            ]
        );
    }

    #[test]
    fn split_diff_pads_with_synthetic_when_one_side_only_inserts() {
        let (l, r) = split("alpha\ngamma\n", "alpha\nNEW\ngamma\n");
        assert_eq!(l.line_count(), r.line_count());
        assert_eq!(l.line_count(), 3);
        assert!(matches!(source_lines(&l)[1], Some(SourceLine::Synthetic)));
    }

    #[test]
    fn split_diff_pads_with_synthetic_when_one_side_only_deletes() {
        let (l, r) = split("alpha\nOLD\ngamma\n", "alpha\ngamma\n");
        assert_eq!(l.line_count(), r.line_count());
        assert_eq!(l.line_count(), 3);
        assert!(matches!(source_lines(&r)[1], Some(SourceLine::Synthetic)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::builder::{display_width, test_util::render_test};

    fn chunks(parts: &[&str]) -> Vec<SharedContentSlice> {
        parts
            .iter()
            .map(|part| SharedContentSlice::from_owned((*part).to_string()))
            .collect()
    }

    #[test]
    fn retained_common_byte_scans_cross_chunks_and_partial_codepoints() {
        let cases = [
            (chunks(&["α", "beta"]), chunks(&["αb", "eta"]), 6, 6),
            (chunks(&["αx"]), chunks(&["βx"]), 1, 1),
            (chunks(&["xĀ"]), chunks(&["yƀ"]), 0, 1),
            (chunks(&["body"]), chunks(&["x", "body"]), 0, 4),
            (chunks(&["bo", "dy"]), chunks(&["body", "x"]), 4, 0),
            (chunks(&["a"]), chunks(&["b"]), 0, 0),
            (chunks(&[]), chunks(&["content"]), 0, 0),
            (chunks(&["content"]), chunks(&[]), 0, 0),
            (chunks(&["", "same", ""]), chunks(&["same"]), 4, 4),
        ];
        for (old, new, expected_prefix, expected_suffix) in cases {
            assert_eq!(
                common_prefix_bytes(&old, &new),
                expected_prefix,
                "prefix differs for {old:?} and {new:?}"
            );
            assert_eq!(
                common_suffix_bytes(&old, &new),
                expected_suffix,
                "suffix differs for {old:?} and {new:?}"
            );
        }
    }

    fn count_lines(cache: &DiffIr) -> (usize, usize, usize, usize) {
        let (mut ctx, mut del, mut ins, mut ell) = (0, 0, 0, 0);
        for l in &cache.lines {
            match l {
                DiffLine::Context { .. } => ctx += 1,
                DiffLine::Delete { .. } => del += 1,
                DiffLine::Insert { .. } => ins += 1,
                DiffLine::Ellipsis => ell += 1,
            }
        }
        (ctx, del, ins, ell)
    }

    fn assert_text_layouts_measure(cache: &DiffIr) {
        for line in &cache.lines {
            let text = match line {
                DiffLine::Context { text, .. }
                | DiffLine::Delete { text, .. }
                | DiffLine::Insert { text, .. } => text,
                DiffLine::Ellipsis => continue,
            };
            assert_eq!(
                diff_line_layout(text).measure_unwrapped(),
                display_width(text.as_str())
            );
        }
    }

    #[test]
    fn diff_ir_text_measures_for_tabs_and_unicode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unicode.rs");
        let old = "fn main() {\n\tlet s = \"é😀\";\n}\n";
        let new = "fn main() {\n\tlet s = \"é😀!\";\n}\n";
        std::fs::write(&path, new).unwrap();
        let cache = build_diff_ir(old, new, path.to_str().unwrap(), "");
        assert_text_layouts_measure(&cache);
    }

    #[test]
    fn diff_ir_marks_changed_characters_inside_replaced_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("numbers.rs");
        let old = "let x = 1;\n";
        let new = "let x = 42;\n";
        std::fs::write(&path, new).unwrap();
        let cache = build_diff_ir(old, new, path.to_str().unwrap(), "");

        let mut saw_delete = false;
        let mut saw_insert = false;
        for line in &cache.lines {
            match line {
                DiffLine::Delete {
                    text, highlights, ..
                } => {
                    assert_eq!(text, "let x = 1;");
                    let highlighted: String = highlights
                        .iter()
                        .map(|range| &text[range.start..range.end])
                        .collect();
                    assert_eq!(highlighted, "1");
                    saw_delete = true;
                }
                DiffLine::Insert {
                    text, highlights, ..
                } => {
                    assert_eq!(text, "let x = 42;");
                    let highlighted: String = highlights
                        .iter()
                        .map(|range| &text[range.start..range.end])
                        .collect();
                    assert_eq!(highlighted, "42");
                    saw_insert = true;
                }
                _ => {}
            }
        }
        assert!(saw_delete);
        assert!(saw_insert);
    }

    #[test]
    fn syntax_spans_preserve_inline_highlight_metadata_when_wrapping() {
        let theme = syntax_theme(&crate::theme::Theme::default());
        let syntax = SYNTAX_SET.find_syntax_plain_text();
        let mut h = HighlightLines::new(syntax, theme);
        let rows = split_syntax_spans_into_rows_with_highlights(
            &mut h,
            "abcdef",
            &[DiffByteRange { start: 2, end: 5 }],
            3,
        );
        let flags: Vec<(String, bool)> = rows
            .iter()
            .flat_map(|row| {
                row.iter()
                    .map(|span| (span.text.clone(), span.meta.highlighted))
            })
            .collect();
        assert_eq!(
            flags,
            vec![
                ("ab".to_string(), false),
                ("c".to_string(), true),
                ("de".to_string(), true),
                ("f".to_string(), false),
            ]
        );
    }

    #[test]
    fn split_syntax_spans_into_rows_wraps_at_max_width() {
        let theme = syntax_theme(&crate::theme::Theme::default());
        let syntax = SYNTAX_SET.find_syntax_plain_text();
        let mut h = HighlightLines::new(syntax, theme);
        let rows = split_syntax_spans_into_rows(&mut h, "abcdefghij", 4);
        assert_eq!(rows.len(), 3);
        let concat: String = rows
            .iter()
            .flat_map(|r| r.iter().map(|s| s.text.as_str()))
            .collect();
        assert_eq!(concat, "abcdefghij");
        assert_eq!(rows[0][0].text, "abcd");
        assert_eq!(rows[1][0].text, "efgh");
        assert_eq!(rows[2][0].text, "ij");
    }

    #[test]
    fn split_syntax_spans_into_rows_counts_display_width() {
        let theme = syntax_theme(&crate::theme::Theme::default());
        let syntax = SYNTAX_SET.find_syntax_plain_text();
        let mut h = HighlightLines::new(syntax, theme);
        let rows = split_syntax_spans_into_rows(&mut h, "😀abc", 2);
        assert_eq!(rows[0][0].text, "😀");
        assert_eq!(rows[1][0].text, "ab");
    }

    #[test]
    fn split_syntax_spans_into_rows_clamps_max_width_to_one() {
        let theme = syntax_theme(&crate::theme::Theme::default());
        let syntax = SYNTAX_SET.find_syntax_plain_text();
        let mut h = HighlightLines::new(syntax, theme);
        let rows = split_syntax_spans_into_rows(&mut h, "ab", 0);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0].text, "a");
        assert_eq!(rows[1][0].text, "b");
    }

    #[test]
    fn split_syntax_spans_into_rows_emits_empty_row_for_empty_input() {
        let theme = syntax_theme(&crate::theme::Theme::default());
        let syntax = SYNTAX_SET.find_syntax_plain_text();
        let mut h = HighlightLines::new(syntax, theme);
        let rows = split_syntax_spans_into_rows(&mut h, "", 4);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].is_empty());
    }

    #[test]
    fn syntax_spans_for_line_returns_non_empty_for_plain_text() {
        let theme = syntax_theme(&crate::theme::Theme::default());
        let syntax = SYNTAX_SET.find_syntax_plain_text();
        let mut h = HighlightLines::new(syntax, theme);
        let spans = syntax_spans_for_line(&mut h, "hello world");
        assert!(!spans.is_empty());
        let concat: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(concat, "hello world");
    }

    #[test]
    fn syntax_spans_for_line_strips_trailing_newline_and_cr() {
        let theme = syntax_theme(&crate::theme::Theme::default());
        let syntax = SYNTAX_SET.find_syntax_plain_text();
        let mut h = HighlightLines::new(syntax, theme);
        let spans = syntax_spans_for_line(&mut h, "ab");
        let concat: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert!(!concat.ends_with('\n'));
        assert!(!concat.ends_with('\r'));
    }

    #[test]
    fn compute_change_visibility_marks_all_equal_lines_when_no_changes() {
        let changes = vec![
            change(ChangeTag::Equal, "a"),
            change(ChangeTag::Equal, "b"),
            change(ChangeTag::Equal, "c"),
        ];
        let vis = compute_change_visibility(&changes, 3);
        assert_eq!(vis, vec![false, false, false]);
    }

    #[test]
    fn compute_change_visibility_expands_context_around_modifications() {
        // Indices:        0       1       2       3       4       5       6       7
        let changes = vec![
            change(ChangeTag::Equal, "a"),
            change(ChangeTag::Equal, "b"),
            change(ChangeTag::Equal, "c"),
            change(ChangeTag::Equal, "d"),
            change(ChangeTag::Delete, "x"),
            change(ChangeTag::Equal, "e"),
            change(ChangeTag::Equal, "f"),
            change(ChangeTag::Equal, "g"),
        ];
        let vis = compute_change_visibility(&changes, 2);
        // ctx=2: lines within 2 of the Delete (idx 4) are visible
        // before: 2,3 visible; 0,1 hidden
        // delete: 4 visible
        // after: 5,6 visible; 7 hidden
        assert_eq!(vis, vec![false, false, true, true, true, true, true, false]);
    }

    #[test]
    fn compute_change_visibility_with_zero_context_marks_only_changes() {
        let changes = vec![
            change(ChangeTag::Equal, "a"),
            change(ChangeTag::Insert, "x"),
            change(ChangeTag::Equal, "b"),
        ];
        let vis = compute_change_visibility(&changes, 0);
        assert_eq!(vis, vec![false, true, false]);
    }

    #[test]
    fn compute_change_visibility_handles_empty_changes() {
        let vis = compute_change_visibility(&[], 3);
        assert!(vis.is_empty());
    }

    #[test]
    fn compute_change_visibility_treats_insert_and_delete_symmetrically() {
        let with_insert = vec![
            change(ChangeTag::Equal, "a"),
            change(ChangeTag::Equal, "b"),
            change(ChangeTag::Insert, "+"),
            change(ChangeTag::Equal, "c"),
        ];
        let with_delete = vec![
            change(ChangeTag::Equal, "a"),
            change(ChangeTag::Equal, "b"),
            change(ChangeTag::Delete, "-"),
            change(ChangeTag::Equal, "c"),
        ];
        assert_eq!(
            compute_change_visibility(&with_insert, 1),
            compute_change_visibility(&with_delete, 1)
        );
    }

    #[test]
    fn build_diff_ir_empty_inputs_produce_no_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.txt");
        std::fs::write(&path, "").unwrap();
        let cache = build_diff_ir("", "", path.to_str().unwrap(), "");
        let (ctx, del, ins, _) = count_lines(&cache);
        assert_eq!((ctx, del, ins), (0, 0, 0));
    }

    #[test]
    fn retained_diff_matches_full_file_string_diff() {
        let distant_old = (0..20)
            .map(|line| format!("line {line}\tvalue"))
            .collect::<Vec<_>>()
            .join("\n");
        let distant_new = distant_old
            .replacen("line 4\tvalue", "line 4\tchanged", 1)
            .replacen("line 15\tvalue", "line 15\tchanged", 1);
        let cases = [
            ("empty", "", ""),
            ("pure insertion", "", "new\nlines\n"),
            ("pure deletion", "old\nlines\n", ""),
            ("missing final newline", "same\n", "same"),
            ("unicode and tabs", "alpha\tβeta\n", "alpha\tγamma\n"),
            ("distant changes", &distant_old, &distant_new),
        ];

        let assert_matches = |case: &str, old: &str, new: &str| {
            let old_content = TranscriptContent::from(old.to_owned());
            let new_content = TranscriptContent::from(new.to_owned());
            let old_read = old_content.read();
            let new_read = new_content.read();
            assert_eq!(
                retained_lines(&old_read, 0..old_read.logical_line_count()),
                old.split_inclusive('\n').collect::<Vec<_>>(),
                "old retained lines differ for {case}"
            );
            assert_eq!(
                retained_lines(&new_read, 0..new_read.logical_line_count()),
                new.split_inclusive('\n').collect::<Vec<_>>(),
                "new retained lines differ for {case}"
            );
            drop((old_read, new_read));
            let expected =
                build_diff_ir_ext_with_base(old, new, "example.rs", old, Some("rs"), Some(old));
            let retained =
                build_retained_diff_ir(&old_content, &new_content, "example.rs", Some("rs"));

            assert_eq!(
                retained.max_display_lineno, expected.max_display_lineno,
                "line number width differs for {case}"
            );
            assert_eq!(
                retained.syntax_ext, expected.syntax_ext,
                "syntax differs for {case}"
            );
            assert_eq!(retained.lines, expected.lines, "lines differ for {case}");
        };

        for (case, old, new) in cases {
            assert_matches(case, old, new);
        }

        let mut seed = 0x5eed_u64;
        let mut generated_source = || {
            let mut source = String::new();
            for _ in 0..(seed as usize % 12) {
                seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                let line = match seed % 5 {
                    0 => "alpha",
                    1 => "beta\tvalue",
                    2 => "γamma",
                    3 => "repeated",
                    _ => "",
                };
                source.push_str(line);
                if seed & 1 == 0 {
                    source.push('\n');
                } else {
                    source.push_str("\r\n");
                }
            }
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            if seed & 1 == 0 {
                source.pop();
            }
            source
        };
        for case in 0..64 {
            let old = generated_source();
            let new = generated_source();
            assert_matches(&format!("generated case {case}"), &old, &new);
        }

        let large_old = (0..240)
            .map(|line| format!("line {line}: repeated {}", line % 7))
            .collect::<Vec<_>>()
            .join("\n");
        let large_new = large_old
            .replacen("line 40: repeated 5", "line 40: changed", 1)
            .replacen("line 200: repeated 4", "line 200: changed", 1);
        assert_matches("large distant changes", &large_old, &large_new);
    }

    #[test]
    fn build_diff_ir_pure_insert_emits_insert_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "new line\n").unwrap();
        let cache = build_diff_ir("", "new line\n", path.to_str().unwrap(), "");
        let (_, del, ins, _) = count_lines(&cache);
        assert!(
            ins >= 1,
            "expected at least one insert, got cache={cache:?}"
        );
        assert_eq!(del, 0);
    }

    #[test]
    fn build_diff_ir_pure_delete_emits_delete_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "").unwrap();
        let cache = build_diff_ir("removed\n", "", path.to_str().unwrap(), "removed\n");
        let (_, del, ins, _) = count_lines(&cache);
        assert!(del >= 1);
        assert_eq!(ins, 0);
    }

    #[test]
    fn build_diff_ir_replacement_produces_delete_and_insert() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "line one\nline two\nline three\n").unwrap();
        let cache = build_diff_ir(
            "line two\n",
            "LINE TWO\n",
            path.to_str().unwrap(),
            "line two\n",
        );
        let (_, del, ins, _) = count_lines(&cache);
        assert!(del >= 1);
        assert!(ins >= 1);
        // Surrounding context lines should be present.
        let saw_context = cache
            .lines
            .iter()
            .any(|l| matches!(l, DiffLine::Context { .. }));
        assert!(saw_context);
    }

    #[test]
    fn build_diff_ir_anchor_locates_position_in_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(
            &path,
            "alpha\nbeta\ngamma\ndelta\nepsilon\nzeta\neta\ntheta\n",
        )
        .unwrap();
        let cache = build_diff_ir("gamma\n", "GAMMA\n", path.to_str().unwrap(), "gamma\n");
        // The first emitted line number for the delete should be near gamma (line 3).
        let first_lineno = cache.lines.iter().find_map(|l| match l {
            DiffLine::Delete { lineno, .. } => Some(*lineno),
            _ => None,
        });
        assert_eq!(first_lineno, Some(3));
    }

    #[test]
    fn build_diff_ir_falls_back_when_path_missing() {
        let cache = build_diff_ir(
            "foo\n",
            "bar\n",
            "/nonexistent/path/that/does/not/exist",
            "",
        );
        let (_, del, ins, _) = count_lines(&cache);
        assert!(del >= 1);
        assert!(ins >= 1);
    }

    #[test]
    fn build_diff_ir_ext_uses_overridden_syntax() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.unknownext");
        std::fs::write(&path, "let x = 1;\nlet y = 2;\n").unwrap();
        let cache_plain = build_diff_ir_ext(
            "let x = 1;\n",
            "let x = 99;\n",
            path.to_str().unwrap(),
            "let x = 1;\n",
            None,
        );
        let cache_rs = build_diff_ir_ext(
            "let x = 1;\n",
            "let x = 99;\n",
            path.to_str().unwrap(),
            "let x = 1;\n",
            Some("rs"),
        );
        assert_eq!(cache_plain.syntax_ext, "unknownext");
        assert_eq!(cache_rs.syntax_ext, "rs");
        assert_eq!(count_lines(&cache_plain), count_lines(&cache_rs));
    }

    #[test]
    fn build_diff_ir_collapses_distant_unchanged_lines_with_ellipsis() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        // Long file so view_end caps off; we still expect the cache to be non-trivial.
        let body: String = (0..40).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&path, &body).unwrap();
        let mut old = String::new();
        for i in 0..40 {
            old.push_str(&format!("line {i}\n"));
        }
        let mut new = old.clone();
        new = new
            .replace("line 5", "LINE 5")
            .replace("line 30", "LINE 30");
        let cache = build_diff_ir(&old, &new, path.to_str().unwrap(), "");
        let (_, _, _, ell) = count_lines(&cache);
        assert!(ell >= 1, "expected at least one ellipsis collapse");
    }

    #[test]
    fn build_diff_ir_snippet_insert_no_duplication() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.rs");
        // File on disk already has the inserted content.
        std::fs::write(
            &path,
            "auto_reload: Bool = false;\n\
             compact_threshold: Number = 0.80;\n\
             /// Anthropic prompt cache TTL. `false` uses the 5-minute ephemeral\n\
             /// TTL; `true` opts into the 1-hour TTL. Has no effect on\n\
             /// non-Anthropic providers.\n\
             cache_ttl_long: Bool = false;\n",
        )
        .unwrap();

        let old = "compact_threshold: Number = 0.80;\n";
        let new = "compact_threshold: Number = 0.80;\n\
                   /// Anthropic prompt cache TTL. `false` uses the 5-minute ephemeral\n\
                   /// TTL; `true` opts into the 1-hour TTL. Has no effect on\n\
                   /// non-Anthropic providers.\n\
                   cache_ttl_long: Bool = false;\n";
        let cache = build_diff_ir(old, new, path.to_str().unwrap(), old);

        let mut insert_texts: Vec<String> = Vec::new();
        let mut ctx_texts: Vec<String> = Vec::new();
        for line in &cache.lines {
            match line {
                DiffLine::Insert { text, .. } => insert_texts.push(text.clone()),
                DiffLine::Context { text, .. } => ctx_texts.push(text.clone()),
                _ => {}
            }
        }
        for ins in &insert_texts {
            assert!(
                !ctx_texts.contains(ins),
                "insert line {ins:?} duplicated as context"
            );
        }
    }

    #[test]
    fn build_diff_ir_full_file_has_correct_line_numbers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        let old = "fn main() {\n    let x = 1;\n}\n";
        let new = "fn main() {\n    let x = 42;\n}\n";
        std::fs::write(&path, new).unwrap();
        let cache = build_diff_ir(old, new, path.to_str().unwrap(), "    let x = 1;\n");

        let mut saw_delete = false;
        let mut saw_insert = false;
        for line in &cache.lines {
            match line {
                DiffLine::Delete { lineno, text, .. } => {
                    assert_eq!(*lineno, 2, "delete should be on line 2, got {lineno}");
                    assert!(text.contains("let x = 1"));
                    saw_delete = true;
                }
                DiffLine::Insert { lineno, text, .. } => {
                    assert_eq!(*lineno, 2, "insert should be on line 2, got {lineno}");
                    assert!(text.contains("let x = 42"));
                    saw_insert = true;
                }
                DiffLine::Context { text, .. } => {
                    // Context lines should not duplicate the change lines
                    assert!(
                        !text.contains("let x = 1") && !text.contains("let x = 42"),
                        "context line should not duplicate change: {text}"
                    );
                }
                _ => {}
            }
        }
        assert!(saw_delete, "expected a delete line");
        assert!(saw_insert, "expected an insert line");
    }

    #[test]
    fn full_file_diff_can_use_pre_edit_content_after_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        let old = "fn main() {\n    let x = 1;\n}\n";
        let new = "fn main() {\n    let x = 42;\n    println!(\"{x}\");\n}\n";

        std::fs::write(&path, old).unwrap();
        let before = build_diff_ir(old, new, path.to_str().unwrap(), "    let x = 1;\n");

        std::fs::write(&path, new).unwrap();
        let after = build_diff_ir_ext_with_base(
            old,
            new,
            path.to_str().unwrap(),
            "    let x = 1;\n",
            None,
            Some(old),
        );

        assert_eq!(format!("{:?}", before.lines), format!("{:?}", after.lines));
        assert_eq!(before.max_display_lineno, after.max_display_lineno);
    }

    #[test]
    fn diff_ir_round_trips_through_json_without_runtime_caches() {
        let cache = build_file_view_ir("alpha\nbeta\n", Some("txt"));
        assert_eq!(measure_diff_ir(&cache, 80, GutterStyle::None, 0), 2);
        render_test(80, |out| {
            print_diff_ir(out, &cache, GutterStyle::None, 0, 0, 1);
        });
        {
            let runtime = render_cache(&cache);
            assert!(!runtime.row_layouts.is_empty());
            assert!(runtime.syntax.is_some());
        }

        let encoded = serde_json::to_string(&cache).unwrap();
        let decoded: DiffIr = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.syntax_ext, "txt");
        assert_eq!(count_lines(&decoded), (2, 0, 0, 0));
        {
            let runtime = render_cache(&decoded);
            assert!(runtime.row_layouts.is_empty());
            assert!(runtime.syntax.is_none());
        }
        assert_eq!(measure_diff_ir(&decoded, 80, GutterStyle::None, 0), 2);
    }

    #[test]
    fn retained_file_view_measures_appends_and_renders_bounded_tail_rows() {
        let content = TranscriptContent::from(
            (0..300)
                .map(|line| format!("let value_{line} = {line};"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let cache = RetainedFileViewCache::default();
        assert_eq!(
            measure_retained_file_view(&content, 80, GutterStyle::InlineLineNumbers, 0),
            300
        );
        render_test(80, |out| {
            assert_eq!(
                print_retained_file_view(
                    out,
                    &content,
                    &cache,
                    "rs",
                    GutterStyle::InlineLineNumbers,
                    0,
                    80,
                    299,
                    1,
                ),
                1
            );
        });
        assert!(cache.retained_bytes() > 0);

        content.append_owned("\nlet appended = true;".into());
        assert_eq!(
            measure_retained_file_view(&content, 80, GutterStyle::InlineLineNumbers, 0),
            301
        );
        render_test(80, |out| {
            assert_eq!(
                print_retained_file_view(
                    out,
                    &content,
                    &cache,
                    "rs",
                    GutterStyle::InlineLineNumbers,
                    0,
                    80,
                    300,
                    1,
                ),
                1
            );
        });
    }

    #[test]
    fn retained_file_view_streams_bounded_rows_from_a_chunked_megabyte_line() {
        let content = TranscriptContent::new();
        let cache = RetainedFileViewCache::default();
        let mut rows = 0usize;
        for _ in 0..128 {
            content.append_owned("x".repeat(8 * 1024));
            let next = measure_retained_file_view(&content, 80, GutterStyle::InlineLineNumbers, 0);
            assert!(next >= rows);
            rows = next;
        }
        assert!(rows > 10_000);

        let block = render_test(80, |out| {
            assert_eq!(
                print_retained_file_view(
                    out,
                    &content,
                    &cache,
                    "json",
                    GutterStyle::InlineLineNumbers,
                    0,
                    80,
                    rows.saturating_sub(2),
                    2,
                ),
                2
            );
        });
        assert_eq!(block.lines.len(), 2);
        assert!(block.lines.iter().all(|line| line.text.contains('x')));
        assert_eq!(cache.retained_bytes(), 0);
    }

    #[test]
    fn print_diff_ir_zero_max_rows_emits_no_limit() {
        let cache = diff_ir(1, "txt".to_string(), vec![context_line(1, "x".to_string())]);
        let block = render_test(80, |out| {
            let emitted = print_diff_ir(out, &cache, GutterStyle::Stamped, 0, 0, 0);
            // 0 means "no limit" - should emit the single line.
            assert_eq!(emitted, 1);
        });
        assert!(block.outcome.line_count >= 1);
    }

    #[test]
    fn print_diff_ir_respects_max_rows() {
        let cache = diff_ir(
            3,
            "txt".to_string(),
            (1..=3)
                .map(|i| context_line(i, format!("line{i}")))
                .collect(),
        );
        render_test(80, |out| {
            let emitted = print_diff_ir(out, &cache, GutterStyle::Stamped, 0, 0, 2);
            assert_eq!(emitted, 2);
        });
    }

    #[test]
    fn print_diff_ir_skips_leading_rows() {
        let cache = diff_ir(
            3,
            "txt".to_string(),
            (1..=3)
                .map(|i| context_line(i, format!("line{i}")))
                .collect(),
        );
        let block = render_test(80, |out| {
            let emitted = print_diff_ir(out, &cache, GutterStyle::Stamped, 0, 2, 0);
            assert_eq!(emitted, 1);
        });
        let joined: String = block
            .lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("line3"));
        assert!(!joined.contains("line1"));
        assert!(!joined.contains("line2"));
    }

    #[test]
    fn print_diff_ir_skips_visual_rows_inside_wrapped_line() {
        let cache = diff_ir(
            1,
            "txt".to_string(),
            vec![context_line(1, "abcdefghij".to_string())],
        );
        let block = render_test(6, |out| {
            let emitted = print_diff_ir(out, &cache, GutterStyle::None, 0, 1, 1);
            assert_eq!(emitted, 1);
        });
        let joined: String = block
            .lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("efgh"), "got: {joined:?}");
        assert!(!joined.contains("abcd"), "got: {joined:?}");
    }

    #[test]
    fn deep_diff_ranges_reuse_bounded_syntax_checkpoints() {
        let content = (0..5_000)
            .map(|i| format!("let value_{i} = {i};"))
            .collect::<Vec<_>>()
            .join("\n");
        let cache = build_file_view_ir(&content, Some("rs"));

        render_test(80, |out| {
            assert_eq!(
                print_diff_ir(out, &cache, GutterStyle::None, 0, 4_000, 20),
                20
            );
        });
        PREFIX_SYNTAX_LINES.with(|count| count.set(0));
        render_test(80, |out| {
            assert_eq!(
                print_diff_ir(out, &cache, GutterStyle::None, 0, 4_010, 20),
                20
            );
        });

        let scanned = PREFIX_SYNTAX_LINES.with(std::cell::Cell::get);
        assert!(
            scanned < SYNTAX_CHECKPOINT_INTERVAL,
            "deep range replayed {scanned} prefix lines"
        );
        let runtime = render_cache(&cache);
        let checkpoints = runtime
            .syntax
            .as_ref()
            .expect("deep render syntax cache")
            .checkpoints
            .len();
        assert!(checkpoints > 1);
        assert!(checkpoints <= MAX_SYNTAX_CHECKPOINTS);
    }

    #[test]
    fn deep_diff_range_matches_full_render_with_multiline_syntax_and_wrapping() {
        let mut lines = Vec::new();
        for i in 0..240 {
            let line = match i {
                10 => "/* begin multiline comment".to_string(),
                210 => "end multiline comment */".to_string(),
                145 => {
                    "comment unicode é😀 and a wrapped tail abcdefghijklmnopqrstuvwxyz".to_string()
                }
                _ => format!("comment row {i:03} abcdefghijklmnopqrstuvwxyz"),
            };
            lines.push(line);
        }
        let content = lines.join("\n");
        let full_cache = build_file_view_ir(&content, Some("rs"));
        let range_cache = build_file_view_ir(&content, Some("rs"));
        let skip = 170;
        let count = 18;
        let full = render_test(32, |out| {
            print_diff_ir(out, &full_cache, GutterStyle::None, 0, 0, 0);
        });
        let range = render_test(32, |out| {
            assert_eq!(
                print_diff_ir(out, &range_cache, GutterStyle::None, 0, skip, count),
                count
            );
        });
        let signature = |line: &crate::content::builder::test_util::TestLine| {
            (
                line.text.clone(),
                line.spans
                    .iter()
                    .map(|span| format!("{}|{:?}|{:?}", span.text, span.style, span.meta))
                    .collect::<Vec<_>>(),
            )
        };
        let expected = full
            .lines
            .iter()
            .skip(usize::from(skip))
            .take(usize::from(count))
            .map(signature)
            .collect::<Vec<_>>();
        let actual = range.lines.iter().map(signature).collect::<Vec<_>>();
        assert_eq!(actual, expected);
    }

    #[test]
    fn diff_row_layout_cache_is_width_specific_and_bounded() {
        let cache = build_file_view_ir(
            "alpha abcdefghijklmnopqrstuvwxyz\nbeta abcdefghijklmnopqrstuvwxyz",
            Some("txt"),
        );
        let shared = cache.clone();
        let narrow = measure_diff_ir(&cache, 12, GutterStyle::None, 0);
        let wide = measure_diff_ir(&cache, 80, GutterStyle::None, 0);
        assert!(narrow > wide);
        assert_eq!(wide, 2);
        assert_eq!(measure_diff_ir(&shared, 12, GutterStyle::None, 0), narrow);
        let _ = measure_diff_ir(&cache, 24, GutterStyle::None, 0);

        let runtime = render_cache(&shared);
        assert_eq!(runtime.row_layouts.len(), MAX_ROW_LAYOUTS);
        assert!(runtime
            .row_layouts
            .iter()
            .all(|layout| !layout.line_starts.is_empty()));
    }

    #[test]
    fn print_diff_ir_renders_delete_insert_and_ellipsis_markers() {
        let cache = diff_ir(
            10,
            "txt".to_string(),
            vec![
                context_line(1, "ctx".to_string()),
                DiffLine::Ellipsis,
                delete_line(5, "old".to_string()),
                insert_line(5, "new".to_string()),
            ],
        );
        let block = render_test(80, |out| {
            print_diff_ir(out, &cache, GutterStyle::InlineLineNumbers, 0, 0, 0);
        });
        let joined: String = block
            .lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("ctx"));
        assert!(joined.contains("old"));
        assert!(joined.contains("new"));
        assert!(joined.contains("⋮"));
        assert!(joined.contains('-'));
        assert!(joined.contains('+'));
    }

    #[test]
    fn print_inline_diff_end_to_end_emits_visible_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "fn main() {\n    let x = 1;\n}\n").unwrap();
        let block = render_test(80, |out| {
            let emitted = print_inline_diff(
                out,
                "    let x = 1;\n",
                "    let x = 42;\n",
                path.to_str().unwrap(),
                "    let x = 1;\n",
                GutterStyle::InlineLineNumbers,
                0,
                0,
            );
            assert!(emitted > 0);
        });
        let joined: String = block
            .lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        // Delete and insert markers should both appear.
        assert!(joined.contains('-'));
        assert!(joined.contains('+'));
    }
}
