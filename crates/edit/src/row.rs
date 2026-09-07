use std::ops::Range;

use crate::{text, BufId, Buffer, WinId};
use smelt_buffer::buffer::{CopyOutput, SpanAction};
use smelt_term::Rect;

pub type RowIndex = u64;

pub fn add_signed_row(row: RowIndex, delta: isize) -> RowIndex {
    let magnitude = RowIndex::try_from(delta.unsigned_abs()).unwrap_or(RowIndex::MAX);
    if delta >= 0 {
        row.saturating_add(magnitude)
    } else {
        row.saturating_sub(magnitude)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DocumentHandle(pub u64);

impl DocumentHandle {
    pub fn raw(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DocPosition {
    pub row: RowIndex,
    pub byte_col: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DocRange {
    pub start: DocPosition,
    pub end: DocPosition,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TextRange {
    Bytes(Range<usize>),
    Rows(DocRange),
}

impl TextRange {
    pub fn rows(&self) -> Option<DocRange> {
        match self {
            Self::Rows(range) => Some(*range),
            Self::Bytes(_) => None,
        }
    }

    pub fn start_position(&self) -> Option<DocPosition> {
        self.rows().map(|range| range.start)
    }
}

impl From<DocRange> for TextRange {
    fn from(range: DocRange) -> Self {
        Self::Rows(range)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowBreak {
    Soft,
    Hard,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplayAction {
    pub cell_start: usize,
    pub cell_end: usize,
    pub action: SpanAction,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DisplayRow {
    pub text: String,
    /// Separator between the previous row and this row in joined display text.
    /// `None` means this is the first row in the returned slice.
    pub break_before: Option<RowBreak>,
    /// Byte ranges in `text` that are selectable/searchable display text.
    pub selectable_ranges: Vec<Range<usize>>,
    /// Cell ranges in `text` that trigger actions such as opening links/files.
    pub actions: Vec<DisplayAction>,
}

impl DisplayRow {
    pub fn new(text: String, selectable_ranges: Vec<Range<usize>>) -> Self {
        Self {
            text,
            break_before: None,
            selectable_ranges,
            actions: Vec::new(),
        }
    }

    pub fn with_actions(mut self, actions: Vec<DisplayAction>) -> Self {
        self.actions = actions;
        self
    }

    pub fn with_break_before(mut self, break_before: RowBreak) -> Self {
        self.break_before = Some(break_before);
        self
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DisplaySnapshot {
    pub generation: u64,
    pub total_rows: RowIndex,
}

pub trait DisplayDocument {
    fn snapshot(&mut self) -> DisplaySnapshot;
    fn materialize(&mut self, range: Range<RowIndex>) -> DisplayRows;
    fn copy_range(&mut self, range: TextRange) -> Option<CopyOutput>;

    fn search_matches(
        &mut self,
        query: &str,
        _origin: DocPosition,
        _forward: bool,
        chunk_rows: RowIndex,
    ) -> Vec<DocRange> {
        let total_rows = self.snapshot().total_rows;
        scan_document_rows(self, query, 0, total_rows, chunk_rows)
    }

    fn search_next_match(
        &mut self,
        query: &str,
        origin: DocPosition,
        forward: bool,
        chunk_rows: RowIndex,
    ) -> Option<DocRange> {
        let total_rows = self.snapshot().total_rows;
        search_document_next(self, query, origin, forward, total_rows, chunk_rows)
    }

    fn action_at(&mut self, pos: DocPosition) -> Option<SpanAction> {
        let row = self
            .materialize(pos.row..pos.row.saturating_add(1))
            .rows
            .into_iter()
            .next()?;
        let byte_col = text::snap_grapheme(&row.text, pos.byte_col.min(row.text.len()));
        let cell = text::byte_to_cell(&row.text, byte_col);
        row.actions
            .into_iter()
            .rev()
            .find(|action| cell >= action.cell_start && cell < action.cell_end)
            .map(|action| action.action)
    }
}

pub(crate) fn scan_document_rows<D: DisplayDocument + ?Sized>(
    document: &mut D,
    query: &str,
    start: RowIndex,
    total_rows: RowIndex,
    chunk_rows: RowIndex,
) -> Vec<DocRange> {
    if query.is_empty() || start >= total_rows {
        return Vec::new();
    }
    let mut matches = Vec::new();
    let chunk_rows = chunk_rows.max(1);
    let mut row = start;
    while row < total_rows {
        let count = chunk_rows.min(total_rows - row);
        let display = document.materialize(row..row.saturating_add(count));
        collect_display_matches(&display.rows, row, query, &mut matches);
        row = row.saturating_add(count);
    }
    matches
}

pub(crate) fn search_document_next<D: DisplayDocument + ?Sized>(
    document: &mut D,
    query: &str,
    origin: DocPosition,
    forward: bool,
    total_rows: RowIndex,
    chunk_rows: RowIndex,
) -> Option<DocRange> {
    if query.is_empty() || total_rows == 0 {
        return None;
    }
    let start = origin.row.min(total_rows.saturating_sub(1));
    if forward {
        search_forward_range(document, query, start..total_rows, Some(origin), chunk_rows).or_else(
            || {
                search_forward_range(
                    document,
                    query,
                    0..start.saturating_add(1),
                    None,
                    chunk_rows,
                )
            },
        )
    } else {
        search_backward_range(
            document,
            query,
            0..start.saturating_add(1),
            Some(origin),
            chunk_rows,
        )
        .or_else(|| search_backward_range(document, query, start..total_rows, None, chunk_rows))
    }
}

fn search_forward_range<D: DisplayDocument + ?Sized>(
    document: &mut D,
    query: &str,
    range: std::ops::Range<RowIndex>,
    min_pos: Option<DocPosition>,
    chunk_rows: RowIndex,
) -> Option<DocRange> {
    if range.start >= range.end {
        return None;
    }
    let chunk_rows = chunk_rows.max(1);
    let mut row = range.start;
    while row < range.end {
        let count = chunk_rows.min(range.end - row);
        let display = document.materialize(row..row.saturating_add(count));
        if let Some(found) = first_forward_match(&display.rows, row, query, min_pos) {
            return Some(found);
        }
        row = row.saturating_add(count);
    }
    None
}

fn search_backward_range<D: DisplayDocument + ?Sized>(
    document: &mut D,
    query: &str,
    range: std::ops::Range<RowIndex>,
    max_pos: Option<DocPosition>,
    chunk_rows: RowIndex,
) -> Option<DocRange> {
    if range.start >= range.end {
        return None;
    }
    let chunk_rows = chunk_rows.max(1);
    let mut end = range.end;
    while end > range.start {
        let start = end.saturating_sub(chunk_rows).max(range.start);
        let display = document.materialize(start..end);
        if let Some(found) = first_backward_match(&display.rows, start, query, max_pos) {
            return Some(found);
        }
        end = start;
    }
    None
}

fn first_forward_match(
    rows: &[DisplayRow],
    start: RowIndex,
    query: &str,
    min_pos: Option<DocPosition>,
) -> Option<DocRange> {
    for (offset, row) in rows.iter().enumerate() {
        let row_index = start.saturating_add(offset as RowIndex);
        for (byte_col, _) in row.text.match_indices(query) {
            let Some(range) = complete_row_match(row, row_index, byte_col, byte_col + query.len())
            else {
                continue;
            };
            if min_pos.is_some_and(|pos| {
                range.start.row < pos.row
                    || (range.start.row == pos.row && range.start.byte_col < pos.byte_col)
            }) {
                continue;
            }
            return Some(range);
        }
    }
    None
}

fn first_backward_match(
    rows: &[DisplayRow],
    start: RowIndex,
    query: &str,
    max_pos: Option<DocPosition>,
) -> Option<DocRange> {
    for (offset, row) in rows.iter().enumerate().rev() {
        let row_index = start.saturating_add(offset as RowIndex);
        let matches = row.text.match_indices(query).collect::<Vec<_>>();
        for (byte_col, _) in matches.into_iter().rev() {
            let Some(range) = complete_row_match(row, row_index, byte_col, byte_col + query.len())
            else {
                continue;
            };
            if max_pos.is_some_and(|pos| {
                range.start.row > pos.row
                    || (range.start.row == pos.row && range.start.byte_col > pos.byte_col)
            }) {
                continue;
            }
            return Some(range);
        }
    }
    None
}

pub(crate) fn scan_document_row_window<D: DisplayDocument + ?Sized>(
    document: &mut D,
    query: &str,
    origin: DocPosition,
    total_rows: RowIndex,
    chunk_rows: RowIndex,
) -> Vec<DocRange> {
    if total_rows == 0 {
        return Vec::new();
    }
    let chunk_rows = chunk_rows.max(1);
    let mut matches = Vec::new();
    let start = origin.row.min(total_rows.saturating_sub(1));
    let count = chunk_rows.min(total_rows - start);
    let display = document.materialize(start..start.saturating_add(count));
    collect_display_matches(&display.rows, start, query, &mut matches);
    if matches.is_empty() && start > 0 {
        let count = chunk_rows.min(start);
        let display = document.materialize(0..count);
        collect_display_matches(&display.rows, 0, query, &mut matches);
    }
    matches
}

fn collect_display_matches(
    rows: &[DisplayRow],
    start: RowIndex,
    query: &str,
    matches: &mut Vec<DocRange>,
) {
    for (offset, row) in rows.iter().enumerate() {
        let row_index = start.saturating_add(offset as RowIndex);
        matches.extend(display_row_matches(row, row_index, query));
    }
}

pub fn row_match_is_selectable(row: &DisplayRow, byte_col: usize, end_col: usize) -> bool {
    row.selectable_ranges
        .iter()
        .any(|range| range.start <= byte_col && end_col <= range.end)
}

fn complete_row_match(
    row: &DisplayRow,
    row_index: RowIndex,
    byte_col: usize,
    end_col: usize,
) -> Option<DocRange> {
    let range = text::covering_grapheme_range(&row.text, byte_col..end_col);
    row_match_is_selectable(row, range.start, range.end)
        .then(|| doc_range_for_match(row_index, range.start, range.end))
}

pub fn display_row_matches<'a>(
    row: &'a DisplayRow,
    row_index: RowIndex,
    query: &'a str,
) -> impl Iterator<Item = DocRange> + 'a {
    let mut previous = None;
    row.text
        .match_indices(query)
        .filter_map(move |(byte_col, _)| {
            let range = complete_row_match(row, row_index, byte_col, byte_col + query.len())?;
            if previous == Some(range) {
                return None;
            }
            previous = Some(range);
            Some(range)
        })
}

pub fn doc_range_for_match(row: RowIndex, byte_col: usize, end_col: usize) -> DocRange {
    DocRange {
        start: DocPosition { row, byte_col },
        end: DocPosition {
            row,
            byte_col: end_col,
        },
    }
}

pub struct StaticRowsDocument {
    rows: Vec<DisplayRow>,
    generation: u64,
}

impl StaticRowsDocument {
    pub fn new(rows: Vec<DisplayRow>) -> Self {
        Self {
            rows,
            generation: 0,
        }
    }

    pub fn from_text_rows(rows: Vec<String>) -> Self {
        Self::new(
            rows.into_iter()
                .map(|text| {
                    let range = 0..text.len();
                    DisplayRow::new(text, vec![range])
                })
                .collect(),
        )
    }

    pub fn set_rows(&mut self, rows: Vec<DisplayRow>) {
        self.rows = rows;
        self.generation = self.generation.wrapping_add(1);
    }
}

impl DisplayDocument for StaticRowsDocument {
    fn snapshot(&mut self) -> DisplaySnapshot {
        DisplaySnapshot {
            generation: self.generation,
            total_rows: self.rows.len() as RowIndex,
        }
    }

    fn materialize(&mut self, range: Range<RowIndex>) -> DisplayRows {
        let start = row_to_usize(range.start).min(self.rows.len());
        let end = row_to_usize(range.end).min(self.rows.len());
        DisplayRows {
            rows: self.rows[start..end].to_vec(),
        }
    }

    fn copy_range(&mut self, range: TextRange) -> Option<CopyOutput> {
        let range = range.rows()?;
        let start_row = range.start.row.min(range.end.row);
        let end_row = range.start.row.max(range.end.row);
        let mut out = String::new();
        for row_idx in start_row..=end_row {
            let row = self.rows.get(row_to_usize(row_idx))?;
            if !out.is_empty() {
                out.push('\n');
            }
            if start_row == end_row {
                let start = range.start.byte_col.min(range.end.byte_col);
                let end = range.start.byte_col.max(range.end.byte_col);
                out.push_str(smelt_buffer::text::slice_snapped_graphemes(
                    &row.text,
                    start..end,
                ));
            } else if row_idx == range.start.row {
                out.push_str(smelt_buffer::text::slice_snapped_graphemes(
                    &row.text,
                    range.start.byte_col..row.text.len(),
                ));
            } else if row_idx == range.end.row {
                out.push_str(smelt_buffer::text::slice_snapped_graphemes(
                    &row.text,
                    0..range.end.byte_col,
                ));
            } else {
                out.push_str(&row.text);
            }
        }
        Some(CopyOutput::same(out))
    }
}

pub struct RowSourceDocument<'a> {
    pub source: &'a dyn smelt_buffer::document::RowSource,
    pub width: u16,
    pub theme: &'a smelt_buffer::theme::Theme,
}

impl DisplayDocument for RowSourceDocument<'_> {
    fn snapshot(&mut self) -> DisplaySnapshot {
        let snapshot = self.source.snapshot();
        DisplaySnapshot {
            generation: snapshot.generation,
            total_rows: snapshot.total_rows,
        }
    }

    fn materialize(&mut self, range: Range<RowIndex>) -> DisplayRows {
        let rows = self
            .source
            .viewport_rows(
                &smelt_buffer::document::DocumentViewport {
                    rows: range,
                    width: self.width,
                    cursor: None,
                },
                self.theme,
            )
            .into_iter()
            .map(|row| {
                let selectable = crate::selectable_byte_ranges_for_line(&row.text, &row.spans);
                DisplayRow::new(row.text, selectable).with_break_before(RowBreak::Hard)
            })
            .collect();
        DisplayRows { rows }
    }

    fn copy_range(&mut self, range: TextRange) -> Option<CopyOutput> {
        let range = range.rows()?;
        if (range.start.row, range.start.byte_col) >= (range.end.row, range.end.byte_col) {
            return None;
        }
        let total = self.source.snapshot().total_rows;
        let mut text_out = String::new();
        let mut first = true;
        let end_row = range
            .end
            .row
            .saturating_add(u64::from(range.end.byte_col > 0))
            .min(total);
        let mut base = range.start.row;
        while base < end_row {
            let end = base.saturating_add(256).min(end_row);
            let rows = self.materialize(base..end).rows;
            if rows.len() as u64 != end - base {
                return None;
            }
            for (offset, row) in rows.into_iter().enumerate() {
                let index = base + offset as u64;
                let start = if index == range.start.row {
                    range.start.byte_col
                } else {
                    0
                };
                let end = if index == range.end.row {
                    range.end.byte_col
                } else {
                    row.text.len()
                };
                if !first {
                    text_out.push('\n');
                }
                first = false;
                for selectable in row.selectable_ranges {
                    text_out.push_str(smelt_buffer::text::slice(
                        &row.text,
                        start.max(selectable.start)..end.min(selectable.end),
                    ));
                }
            }
            base = end;
        }
        Some(CopyOutput::same(text_out))
    }
}

pub struct BufferDocument<'a> {
    buf: &'a Buffer,
}

impl<'a> BufferDocument<'a> {
    pub fn new(buf: &'a Buffer) -> Self {
        Self { buf }
    }
}

impl DisplayDocument for BufferDocument<'_> {
    fn snapshot(&mut self) -> DisplaySnapshot {
        DisplaySnapshot {
            generation: self.buf.changedtick(),
            total_rows: self.buf.line_count() as RowIndex,
        }
    }

    fn materialize(&mut self, range: Range<RowIndex>) -> DisplayRows {
        let start = row_to_usize(range.start).min(self.buf.line_count());
        let end = row_to_usize(range.end).min(self.buf.line_count());
        let rows = self
            .buf
            .get_lines(start, end)
            .iter()
            .map(|line| DisplayRow::new(line.clone(), std::iter::once(0..line.len()).collect()))
            .collect();
        DisplayRows { rows }
    }

    fn copy_range(&mut self, range: TextRange) -> Option<CopyOutput> {
        match range {
            TextRange::Bytes(range) => Some(self.buf.copy_range(range)),
            TextRange::Rows(range) => copy_buffer_doc_range(self.buf, range),
        }
    }
}

pub(crate) fn copy_buffer_doc_range(buf: &Buffer, range: DocRange) -> Option<CopyOutput> {
    let lines = buf.lines();
    if lines.is_empty()
        || (range.start.row, range.start.byte_col) >= (range.end.row, range.end.byte_col)
    {
        return None;
    }
    let start_row = range.start.row.min(range.end.row);
    let end_row = range.start.row.max(range.end.row);
    let mut out = String::new();
    for row_idx in start_row..=end_row {
        if row_idx == end_row && range.end.byte_col == 0 && end_row > start_row {
            break;
        }
        let row = lines.get(row_to_usize(row_idx))?;
        if !out.is_empty() {
            out.push('\n');
        }
        if start_row == end_row {
            let start = range.start.byte_col.min(range.end.byte_col);
            let end = range.start.byte_col.max(range.end.byte_col);
            out.push_str(smelt_buffer::text::slice_snapped_graphemes(row, start..end));
        } else if row_idx == range.start.row {
            out.push_str(smelt_buffer::text::slice_snapped_graphemes(
                row,
                range.start.byte_col..row.len(),
            ));
        } else if row_idx == range.end.row {
            out.push_str(smelt_buffer::text::slice_snapped_graphemes(
                row,
                0..range.end.byte_col,
            ));
        } else {
            out.push_str(row);
        }
    }
    Some(CopyOutput::same(out))
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DisplayRows {
    pub rows: Vec<DisplayRow>,
}

impl DisplayRows {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn into_text_rows(self) -> Vec<String> {
        self.rows.into_iter().map(|row| row.text).collect()
    }

    pub fn text_rows(&self) -> Vec<String> {
        self.rows.iter().map(|row| row.text.clone()).collect()
    }

    pub fn selectable_text(&self) -> String {
        self.rows
            .iter()
            .flat_map(|row| {
                row.selectable_ranges.iter().filter_map(|range| {
                    let text =
                        smelt_buffer::text::slice_snapped_graphemes(&row.text, range.clone());
                    (!text.is_empty()).then(|| text.to_string())
                })
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn soft_breaks(&self) -> Vec<usize> {
        self.breaks(RowBreak::Soft)
    }

    pub fn hard_breaks(&self) -> Vec<usize> {
        self.breaks(RowBreak::Hard)
    }

    fn breaks(&self, kind: RowBreak) -> Vec<usize> {
        let mut breaks = Vec::new();
        let mut pos = 0usize;
        for (i, row) in self.rows.iter().enumerate() {
            if i > 0 {
                if row.break_before == Some(kind) {
                    breaks.push(pos);
                }
                pos = pos.saturating_add(1);
            }
            pos = pos.saturating_add(row.text.len());
        }
        breaks
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MaterializedRows {
    pub clamped_scroll: RowIndex,
    pub row_base: RowIndex,
    pub total_rows: RowIndex,
    pub materialized_rows: RowIndex,
}

impl MaterializedRows {
    pub fn materialized_range(self) -> Range<RowIndex> {
        self.row_base
            ..self
                .row_base
                .saturating_add(self.materialized_rows)
                .min(self.total_rows)
    }

    pub fn contains_abs_row(self, row: RowIndex) -> bool {
        self.materialized_range().contains(&row)
    }

    pub fn viewport_range(self, scroll_top: RowIndex, viewport_rows: u16) -> Range<RowIndex> {
        scroll_top
            ..scroll_top
                .saturating_add(RowIndex::from(viewport_rows))
                .min(self.total_rows)
    }

    pub fn covers_viewport(self, scroll_top: RowIndex, viewport_rows: u16) -> bool {
        if scroll_top > self.total_rows {
            return false;
        }
        let viewport = self.viewport_range(scroll_top, viewport_rows);
        let materialized = self.materialized_range();
        materialized.start <= viewport.start && viewport.end <= materialized.end
    }

    pub fn nearest_backed_scroll_top(
        self,
        scroll_top: RowIndex,
        viewport_rows: u16,
    ) -> Option<RowIndex> {
        let viewport_rows = RowIndex::from(viewport_rows);
        let materialized = self.materialized_range();
        let min_backed_scroll = materialized.start;
        let max_backed_scroll = if materialized.end >= self.total_rows {
            self.total_rows
        } else {
            materialized.end.checked_sub(viewport_rows)?
        };
        if min_backed_scroll > max_backed_scroll {
            return None;
        }
        Some(scroll_top.clamp(min_backed_scroll, max_backed_scroll))
    }

    pub fn local_row(self, abs: RowIndex) -> RowIndex {
        abs.saturating_sub(self.row_base)
    }

    pub fn absolute_row(self, local: RowIndex) -> RowIndex {
        self.row_base.saturating_add(local)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MaterializeRequest {
    pub win: WinId,
    pub buf: BufId,
    pub document_handle: Option<DocumentHandle>,
    pub rect: Rect,
    pub gutter_width: u16,
    pub content_width: u16,
    pub scroll_top: RowIndex,
    pub follow_tail: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PreparedWindowRequest {
    pub win: WinId,
    pub buf: BufId,
    pub document_handle: Option<DocumentHandle>,
    /// Resolved layout area used to validate reuse against the current split.
    pub rect: Rect,
    /// Committed interactive viewport, which may retain a smaller exact row window.
    pub viewport_rect: Rect,
    pub gutter_width: u16,
    pub content_width: u16,
}

pub fn row_to_usize(row: RowIndex) -> usize {
    row.min(usize::MAX as RowIndex) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_row_arithmetic_saturates_at_both_bounds() {
        assert_eq!(add_signed_row(5, 3), 8);
        assert_eq!(add_signed_row(5, -3), 2);
        assert_eq!(add_signed_row(2, -3), 0);
        assert_eq!(add_signed_row(RowIndex::MAX - 1, 3), RowIndex::MAX);
        assert_eq!(add_signed_row(5, isize::MIN), 0);
    }

    #[test]
    fn static_rows_document_materializes_and_copies_ranges() {
        let mut doc = StaticRowsDocument::from_text_rows(vec!["alpha".into(), "beta".into()]);
        assert_eq!(doc.snapshot().total_rows, 2);
        assert_eq!(doc.materialize(1..2).text_rows(), vec!["beta"]);

        let copied = doc
            .copy_range(TextRange::Rows(DocRange {
                start: DocPosition {
                    row: 0,
                    byte_col: 1,
                },
                end: DocPosition {
                    row: 1,
                    byte_col: 2,
                },
            }))
            .expect("copy range");
        assert_eq!(copied.kill_ring, "lpha\nbe");
    }

    #[test]
    fn buffer_document_copies_row_ranges() {
        let mut buf = Buffer::new(BufId(1), crate::BufCreateOpts::default());
        buf.set_all_lines(vec!["alpha".into(), "beta".into(), "gamma".into()]);
        let mut doc = BufferDocument::new(&buf);

        let copied = doc
            .copy_range(TextRange::Rows(DocRange {
                start: DocPosition {
                    row: 0,
                    byte_col: 1,
                },
                end: DocPosition {
                    row: 2,
                    byte_col: 2,
                },
            }))
            .expect("copy range");
        assert_eq!(copied.kill_ring, "lpha\nbeta\nga");
    }

    #[test]
    fn row_document_copy_snaps_stale_offsets_to_graphemes() {
        let grapheme = "e\u{301}";
        let mut doc = StaticRowsDocument::from_text_rows(vec![format!("{grapheme}x")]);
        let copied = doc
            .copy_range(TextRange::Rows(DocRange {
                start: DocPosition {
                    row: 0,
                    byte_col: 1,
                },
                end: DocPosition {
                    row: 0,
                    byte_col: grapheme.len(),
                },
            }))
            .expect("copy range");

        assert_eq!(copied.kill_ring, grapheme);
    }

    #[test]
    fn search_next_match_scans_document_chunks_until_first_match() {
        struct CountingDoc {
            rows: Vec<DisplayRow>,
            materialized: Vec<Range<RowIndex>>,
        }

        impl DisplayDocument for CountingDoc {
            fn snapshot(&mut self) -> DisplaySnapshot {
                DisplaySnapshot {
                    generation: 0,
                    total_rows: self.rows.len() as RowIndex,
                }
            }

            fn materialize(&mut self, range: Range<RowIndex>) -> DisplayRows {
                self.materialized.push(range.clone());
                let start = row_to_usize(range.start).min(self.rows.len());
                let end = row_to_usize(range.end).min(self.rows.len());
                DisplayRows {
                    rows: self.rows[start..end].to_vec(),
                }
            }

            fn copy_range(&mut self, _range: TextRange) -> Option<CopyOutput> {
                None
            }
        }

        let mut rows = (0..20)
            .map(|i| DisplayRow::new(format!("row {i}"), std::iter::once(0..5).collect()))
            .collect::<Vec<_>>();
        rows[3] = DisplayRow::new("hidden needle".into(), Vec::new());
        rows[12] = DisplayRow::new("visible needle".into(), std::iter::once(0..14).collect());
        let mut doc = CountingDoc {
            rows,
            materialized: Vec::new(),
        };

        let found = doc.search_next_match(
            "needle",
            DocPosition {
                row: 0,
                byte_col: 0,
            },
            true,
            4,
        );

        assert_eq!(
            found,
            Some(DocRange {
                start: DocPosition {
                    row: 12,
                    byte_col: 8
                },
                end: DocPosition {
                    row: 12,
                    byte_col: 14
                },
            })
        );
        assert_eq!(doc.materialized, vec![0..4, 4..8, 8..12, 12..16]);
    }

    #[test]
    fn search_next_match_wraps_backward_from_origin() {
        let mut doc = StaticRowsDocument::from_text_rows(vec![
            "first needle".into(),
            "middle".into(),
            "last needle".into(),
        ]);

        let found = doc.search_next_match(
            "needle",
            DocPosition {
                row: 0,
                byte_col: 0,
            },
            false,
            2,
        );

        assert_eq!(
            found,
            Some(DocRange {
                start: DocPosition {
                    row: 2,
                    byte_col: 5
                },
                end: DocPosition {
                    row: 2,
                    byte_col: 11
                },
            })
        );
    }

    #[test]
    fn search_next_match_skips_chrome_and_reports_utf8_byte_ranges() {
        let hidden = "chrome βγ".to_string();
        let visible = "select βγ here".to_string();
        let start = visible.find("βγ").expect("query start");
        let end = start + "βγ".len();
        let mut doc = StaticRowsDocument::new(vec![
            DisplayRow::new(hidden, std::iter::once(0.."chrome ".len()).collect()),
            DisplayRow::new(visible, std::iter::once(start..end).collect()),
        ]);

        let found = doc.search_next_match(
            "βγ",
            DocPosition {
                row: 0,
                byte_col: 0,
            },
            true,
            8,
        );

        assert_eq!(
            found,
            Some(DocRange {
                start: DocPosition {
                    row: 1,
                    byte_col: start
                },
                end: DocPosition {
                    row: 1,
                    byte_col: end
                },
            })
        );
    }

    #[test]
    fn display_search_matches_expand_to_complete_graphemes() {
        let text = "e\u{301} 🇨🇦 9\u{fe0f}".to_string();
        let row = DisplayRow::new(text.clone(), std::iter::once(0..text.len()).collect());
        for (query, grapheme) in [
            ("e", "e\u{301}"),
            ("\u{301}", "e\u{301}"),
            ("🇨", "🇨🇦"),
            ("9", "9\u{fe0f}"),
        ] {
            let start = text.find(grapheme).unwrap();
            assert_eq!(
                display_row_matches(&row, 4, query).collect::<Vec<_>>(),
                vec![DocRange {
                    start: DocPosition {
                        row: 4,
                        byte_col: start,
                    },
                    end: DocPosition {
                        row: 4,
                        byte_col: start + grapheme.len(),
                    },
                }],
                "query={query:?}"
            );
        }
    }

    #[test]
    fn display_rows_track_selectable_text_and_visual_breaks() {
        let unicode = "α keep".to_string();
        let soft = "wrapped".to_string();
        let hard = "chrome visible".to_string();
        let visible_start = "chrome ".len();
        let rows = DisplayRows {
            rows: vec![
                DisplayRow::new(unicode.clone(), std::iter::once(0.."α".len()).collect()),
                DisplayRow::new(soft.clone(), std::iter::once(0..soft.len()).collect())
                    .with_break_before(RowBreak::Soft),
                DisplayRow::new(
                    hard.clone(),
                    std::iter::once(visible_start..hard.len()).collect(),
                )
                .with_break_before(RowBreak::Hard),
            ],
        };

        assert_eq!(rows.selectable_text(), "α\nwrapped\nvisible");
        assert_eq!(rows.soft_breaks(), vec![unicode.len()]);
        assert_eq!(rows.hard_breaks(), vec![unicode.len() + 1 + soft.len()]);
    }

    #[test]
    fn display_document_action_at_snaps_stale_utf8_offsets() {
        let action = SpanAction::OpenUrl("https://example.test/unicode".into());
        let mut doc = StaticRowsDocument::new(vec![DisplayRow::new(
            "α link".into(),
            std::iter::once(0.."α link".len()).collect(),
        )
        .with_actions(vec![DisplayAction {
            cell_start: 0,
            cell_end: 1,
            action: action.clone(),
        }])]);

        assert_eq!(
            doc.action_at(DocPosition {
                row: 0,
                byte_col: 1,
            }),
            Some(action)
        );
    }

    #[test]
    fn materialized_rows_translate_between_absolute_and_local_rows() {
        let rows = MaterializedRows {
            clamped_scroll: 105,
            row_base: 100,
            total_rows: 1_000,
            materialized_rows: 25,
        };

        assert_eq!(rows.materialized_range(), 100..125);
        assert!(rows.contains_abs_row(100));
        assert!(rows.contains_abs_row(124));
        assert!(!rows.contains_abs_row(125));
        assert_eq!(rows.local_row(117), 17);
        assert_eq!(rows.local_row(1), 0);
        assert_eq!(rows.absolute_row(7), 107);
    }

    #[test]
    fn materialized_rows_validate_and_clamp_backed_viewports() {
        let rows = MaterializedRows {
            clamped_scroll: 110,
            row_base: 100,
            total_rows: 1_000,
            materialized_rows: 40,
        };

        assert!(rows.covers_viewport(100, 20));
        assert!(rows.covers_viewport(120, 20));
        assert!(!rows.covers_viewport(121, 20));
        assert_eq!(rows.nearest_backed_scroll_top(90, 20), Some(100));
        assert_eq!(rows.nearest_backed_scroll_top(130, 20), Some(120));

        let short = MaterializedRows {
            clamped_scroll: 100,
            row_base: 100,
            total_rows: 1_000,
            materialized_rows: 10,
        };
        assert_eq!(short.nearest_backed_scroll_top(100, 20), None);
    }

    #[test]
    fn materialized_rows_range_is_clamped_to_total_rows() {
        let rows = MaterializedRows {
            clamped_scroll: 0,
            row_base: 8,
            total_rows: 10,
            materialized_rows: 10,
        };

        assert_eq!(rows.materialized_range(), 8..10);
        assert!(rows.contains_abs_row(9));
        assert!(!rows.contains_abs_row(10));
    }

    #[test]
    fn display_document_action_at_uses_row_actions() {
        let action = SpanAction::OpenUrl("https://example.test".into());
        let mut doc = StaticRowsDocument::new(vec![DisplayRow::new(
            "open link".into(),
            std::iter::once(0..9).collect(),
        )
        .with_actions(vec![DisplayAction {
            cell_start: 5,
            cell_end: 9,
            action: action.clone(),
        }])]);

        assert_eq!(
            doc.action_at(DocPosition {
                row: 0,
                byte_col: 6
            }),
            Some(action)
        );
        assert_eq!(
            doc.action_at(DocPosition {
                row: 0,
                byte_col: 4
            }),
            None
        );
    }
}
