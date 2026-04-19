//! Transcript domain state: block store + snapshot cache.
//!
//! `Transcript` owns the block history and the width-keyed display
//! snapshot. Streaming input parsing lives in `StreamParser` (owned
//! by `Screen`).

use super::display::SpanMeta;
use super::history::{Block, BlockHistory, BlockId, LayoutKey, Status, ToolState, ViewState};
use std::collections::HashMap;
use std::ops::Range;

/// One display cell in the snapshot, carrying the character and its
/// copy/selection metadata from the span that produced it.
#[derive(Clone, Debug)]
pub struct SnapshotCell {
    pub ch: char,
    pub meta: super::display::SpanMeta,
}

/// Cached, width-keyed projection of the full transcript into plain-text
/// rows with block↔row mappings. Built lazily by `Transcript::snapshot()`
/// and invalidated on any block mutation or width change.
pub struct TranscriptSnapshot {
    pub width: u16,
    pub show_thinking: bool,
    /// One entry per row in the full transcript (including gap rows).
    pub rows: Vec<String>,
    /// Per-cell metadata for each row, parallel to `rows`. Each inner
    /// vec has one `SnapshotCell` per display column. Used by
    /// `copy_range` to respect `SpanMeta.selectable` / `copy_as`.
    pub row_cells: Vec<Vec<SnapshotCell>>,
    /// True when this row is a soft-wrap continuation of the previous
    /// logical line. `copy_range` suppresses `\n` before these rows.
    pub soft_wrapped: Vec<bool>,
    /// Raw source text for each row. `Some(line)` on the first display
    /// row of a source line; `None` on soft-wrap continuations and rows
    /// without source annotation. `copy_range` uses this for
    /// fully-selected rows instead of cell-based reconstruction.
    pub source_text: Vec<Option<String>>,
    /// For each row, the `BlockId` that produced it (`None` for gap rows).
    pub block_of_row: Vec<Option<BlockId>>,
    /// Row range `[start..end)` for each block, in insertion order.
    pub row_of_block: HashMap<BlockId, Range<u16>>,
    /// Generation counter at build time — compared against
    /// `BlockHistory`'s counter to detect staleness.
    generation: u64,
}

impl TranscriptSnapshot {
    /// Viewport-slice the snapshot into visible rows, matching
    /// `paint_viewport`'s bottom-anchoring and skip logic.
    pub fn viewport_rows(&self, viewport_rows: u16, scroll_offset: u16) -> Vec<String> {
        if viewport_rows == 0 {
            return Vec::new();
        }
        let total = self.rows.len().min(u16::MAX as usize) as u16;
        let geom = super::viewport::ViewportGeom::new(total, viewport_rows, scroll_offset);
        let skip = geom.skip_from_top() as usize;
        let leading_blanks = geom.leading_blanks() as usize;

        let mut out = Vec::with_capacity(viewport_rows as usize);
        for _ in 0..leading_blanks {
            if out.len() >= viewport_rows as usize {
                break;
            }
            out.push(String::new());
        }
        for row in self.rows.iter().skip(skip) {
            if out.len() >= viewport_rows as usize {
                break;
            }
            out.push(row.clone());
        }
        out
    }

    /// Which block owns a given viewport row (accounting for scroll +
    /// leading blanks). Returns `None` for gap rows or leading blanks.
    pub fn viewport_block_at(
        &self,
        viewport_row: u16,
        viewport_rows: u16,
        scroll_offset: u16,
    ) -> Option<BlockId> {
        let total = self.rows.len().min(u16::MAX as usize) as u16;
        let geom = super::viewport::ViewportGeom::new(total, viewport_rows, scroll_offset);
        let leading = geom.leading_blanks();
        if viewport_row < leading {
            return None;
        }
        let skip = geom.skip_from_top();
        let abs_row = (viewport_row - leading) as usize + skip as usize;
        self.block_of_row.get(abs_row).copied().flatten()
    }

    /// Total rows in the snapshot.
    pub fn total_rows(&self) -> u16 {
        self.rows.len().min(u16::MAX as usize) as u16
    }

    /// Logical (content-only) text for each row. Non-selectable cells
    /// are stripped; `copy_as` substitutions are applied. This is what
    /// vim motions should navigate — the cursor operates on content,
    /// not on decorative padding.
    pub fn logical_rows(&self) -> Vec<String> {
        self.row_cells
            .iter()
            .map(|cells| {
                let mut s = String::new();
                for cell in cells {
                    if !cell.meta.selectable {
                        continue;
                    }
                    match &cell.meta.copy_as {
                        Some(sub) => s.push_str(sub),
                        None => s.push(cell.ch),
                    }
                }
                s
            })
            .collect()
    }

    /// Map a display `(row, col)` position to a byte offset in the
    /// logical (content-only) text produced by `logical_rows().join("\n")`.
    /// Returns `None` for non-selectable cells.
    pub fn display_to_logical(&self, row: usize, col: usize) -> Option<usize> {
        let mut byte_offset = 0usize;
        for (r, cells) in self.row_cells.iter().enumerate() {
            if r > 0 {
                byte_offset += 1; // \n separator
            }
            if r == row {
                let mut logical_col = 0usize;
                for (c, cell) in cells.iter().enumerate() {
                    if !cell.meta.selectable {
                        if c == col {
                            return None;
                        }
                        continue;
                    }
                    if c == col {
                        return Some(byte_offset + logical_col);
                    }
                    let ch_len = match &cell.meta.copy_as {
                        Some(sub) => sub.len(),
                        None => cell.ch.len_utf8(),
                    };
                    logical_col += ch_len;
                }
                return Some(byte_offset + logical_col);
            }
            // Accumulate this row's logical length
            for cell in cells {
                if !cell.meta.selectable {
                    continue;
                }
                let ch_len = match &cell.meta.copy_as {
                    Some(sub) => sub.len(),
                    None => cell.ch.len_utf8(),
                };
                byte_offset += ch_len;
            }
        }
        Some(byte_offset)
    }

    /// Map a byte offset in the logical text back to a display
    /// `(row, col)` position. Inverse of `display_to_logical`.
    pub fn logical_to_display(&self, byte: usize) -> (usize, usize) {
        let mut remaining = byte;
        for (r, cells) in self.row_cells.iter().enumerate() {
            if r > 0 {
                if remaining == 0 {
                    return (r, 0);
                }
                remaining = remaining.saturating_sub(1); // \n
            }
            for (c, cell) in cells.iter().enumerate() {
                if !cell.meta.selectable {
                    continue;
                }
                let ch_len = match &cell.meta.copy_as {
                    Some(sub) => sub.len(),
                    None => cell.ch.len_utf8(),
                };
                if remaining < ch_len {
                    return (r, c);
                }
                remaining -= ch_len;
            }
            if remaining == 0 {
                return (r, cells.len());
            }
        }
        let last_row = self.row_cells.len().saturating_sub(1);
        let last_col = self.row_cells.last().map(|c| c.len()).unwrap_or(0);
        (last_row, last_col)
    }

    /// Extract copy text from a rectangular range of display cells,
    /// respecting `SpanMeta`. Non-selectable cells are skipped;
    /// `copy_as` substitutions are applied; rows are joined with `\n`.
    pub fn copy_range(
        &self,
        start_row: usize,
        start_col: usize,
        end_row: usize,
        end_col: usize,
    ) -> String {
        let mut out = String::new();
        let end_row = end_row.min(self.row_cells.len().saturating_sub(1));
        for r in start_row..=end_row {
            let cells = match self.row_cells.get(r) {
                Some(c) => c,
                None => continue,
            };
            let is_soft = self.soft_wrapped.get(r).copied().unwrap_or(false);
            if r > start_row && !is_soft {
                out.push('\n');
            }

            let is_first = r == start_row;
            let is_last = r == end_row;
            let c_start = if is_first { start_col } else { 0 };
            let c_end = if is_last {
                end_col.min(cells.len())
            } else {
                cells.len()
            };
            let full_row = c_start == 0 && c_end == cells.len();

            if full_row && is_soft {
                continue;
            }

            if full_row {
                if let Some(src) = self.source_text.get(r).and_then(|s| s.as_deref()) {
                    out.push_str(src);
                    continue;
                }
            }

            for cell in &cells[c_start..c_end] {
                if !cell.meta.selectable {
                    continue;
                }
                match &cell.meta.copy_as {
                    Some(s) => out.push_str(s),
                    None => out.push(cell.ch),
                }
            }
        }
        out
    }

    /// Convert a byte offset in the `rows.join("\n")` text into a
    /// `(row, col_chars)` position. `col_chars` is a character index,
    /// matching `row_cells` indexing.
    pub fn byte_to_row_col(&self, byte: usize) -> (usize, usize) {
        let mut acc = 0usize;
        for (r, row) in self.rows.iter().enumerate() {
            let row_end = acc + row.len();
            if byte <= row_end {
                let col = row[..byte.saturating_sub(acc)].chars().count();
                return (r, col);
            }
            acc = row_end + 1; // +1 for the `\n` join separator
        }
        let last_row = self.rows.len().saturating_sub(1);
        let last_col = self.rows.last().map(|r| r.chars().count()).unwrap_or(0);
        (last_row, last_col)
    }

    /// Copy text from a byte range in the joined row text, respecting
    /// `SpanMeta`. This is the primary copy primitive — selection ranges
    /// expressed as byte offsets (from vim visual or cursor anchor) are
    /// converted to `(row, col)` and routed through `copy_range`.
    pub fn copy_byte_range(&self, start: usize, end: usize) -> String {
        let (sr, sc) = self.byte_to_row_col(start);
        let (er, ec) = self.byte_to_row_col(end);
        self.copy_range(sr, sc, er, ec)
    }

    /// Extract the selectable text of the block at `abs_row`. Uses
    /// `block_of_row` to find the block, then `row_of_block` for its
    /// full row range, and `copy_range` to get SpanMeta-aware text.
    pub fn block_text_at(&self, abs_row: usize) -> Option<String> {
        let block_id = (*self.block_of_row.get(abs_row)?)?;
        let range = self.row_of_block.get(&block_id)?;
        let start_row = range.start as usize;
        let end_row = (range.end as usize).saturating_sub(1);
        if end_row < start_row || start_row >= self.row_cells.len() {
            return None;
        }
        let end_col = self
            .row_cells
            .get(end_row)
            .map(|c| c.len().saturating_sub(1))
            .unwrap_or(0);
        let text = self.copy_range(start_row, 0, end_row, end_col);
        if text.is_empty() {
            None
        } else {
            Some(text)
        }
    }

    /// Snap a `(row, col)` position to the nearest selectable cell,
    /// searching forward then backward on the same row. Returns the
    /// adjusted `(row, col)` or `None` if the row has no selectable cells.
    pub fn snap_to_selectable(&self, row: usize, col: usize) -> Option<(usize, usize)> {
        let cells = self.row_cells.get(row)?;
        if cells.is_empty() {
            return None;
        }
        if cells.get(col).is_some_and(|c| c.meta.selectable) {
            return Some((row, col));
        }
        // Search forward
        for (c, cell) in cells.iter().enumerate().skip(col + 1) {
            if cell.meta.selectable {
                return Some((row, c));
            }
        }
        // Search backward
        for c in (0..col.min(cells.len())).rev() {
            if cells[c].meta.selectable {
                return Some((row, c));
            }
        }
        None
    }
}

pub struct Transcript {
    pub(super) history: BlockHistory,
    cached_snapshot: Option<TranscriptSnapshot>,
}

impl Transcript {
    pub fn new() -> Self {
        Self {
            history: BlockHistory::new(),
            cached_snapshot: None,
        }
    }

    /// Get or rebuild the cached transcript snapshot at the given width.
    /// The snapshot is invalidated when blocks change (generation mismatch)
    /// or width/show_thinking changes.
    pub fn snapshot(&mut self, width: u16, show_thinking: bool) -> &TranscriptSnapshot {
        let gen = self.history.generation();
        let valid = self.cached_snapshot.as_ref().is_some_and(|s| {
            s.generation == gen && s.width == width && s.show_thinking == show_thinking
        });
        if !valid {
            self.cached_snapshot = Some(self.build_snapshot(width, show_thinking));
        }
        self.cached_snapshot.as_ref().unwrap()
    }

    fn build_snapshot(&mut self, width: u16, show_thinking: bool) -> TranscriptSnapshot {
        let base_key = LayoutKey {
            view_state: ViewState::Expanded,
            width,
            show_thinking,
            content_hash: 0,
        };

        let mut rows: Vec<String> = Vec::new();
        let mut row_cells: Vec<Vec<SnapshotCell>> = Vec::new();
        let mut soft_wrapped: Vec<bool> = Vec::new();
        let mut source_text: Vec<Option<String>> = Vec::new();
        let mut block_of_row: Vec<Option<BlockId>> = Vec::new();
        let mut row_of_block: HashMap<BlockId, Range<u16>> = HashMap::new();

        for i in 0..self.history.order.len() {
            let gap = self.history.block_gap(i);
            for _ in 0..gap {
                rows.push(String::new());
                row_cells.push(Vec::new());
                soft_wrapped.push(false);
                source_text.push(None);
                block_of_row.push(None);
            }
            let _ = self.history.ensure_rows(i, base_key);
            let id = self.history.order[i];
            let bkey = self.history.resolve_key(id, base_key);
            let start = rows.len() as u16;
            if let Some(display) = self.history.artifacts.get(&id).and_then(|a| a.get(bkey)) {
                for line in &display.lines {
                    let mut text = String::new();
                    let mut cells = Vec::new();
                    for span in &line.spans {
                        let has_copy_as = span.meta.copy_as.is_some();
                        let mut first = true;
                        for ch in span.text.chars() {
                            text.push(ch);
                            if has_copy_as && !first {
                                cells.push(SnapshotCell {
                                    ch,
                                    meta: SpanMeta {
                                        selectable: span.meta.selectable,
                                        copy_as: Some(String::new()),
                                    },
                                });
                            } else {
                                cells.push(SnapshotCell {
                                    ch,
                                    meta: span.meta.clone(),
                                });
                            }
                            first = false;
                        }
                    }
                    rows.push(text);
                    row_cells.push(cells);
                    soft_wrapped.push(line.soft_wrapped);
                    source_text.push(line.source_text.clone());
                    block_of_row.push(Some(id));
                }
            }
            let end = rows.len() as u16;
            if end > start {
                row_of_block.insert(id, start..end);
            }
        }

        TranscriptSnapshot {
            width,
            show_thinking,
            rows,
            row_cells,
            soft_wrapped,
            source_text,
            block_of_row,
            row_of_block,
            generation: self.history.generation(),
        }
    }

    // ── Accessors ─────────────────────────────────────────────────────

    pub fn block_count(&self) -> usize {
        self.history.len()
    }

    pub fn blocks(&self) -> Vec<Block> {
        self.history
            .order
            .iter()
            .filter_map(|id| self.history.blocks.get(id).cloned())
            .collect()
    }

    pub fn tool_states_snapshot(&self) -> HashMap<String, ToolState> {
        self.history.tool_states.clone()
    }

    pub fn block(&self, id: BlockId) -> Option<&Block> {
        self.history.blocks.get(&id)
    }

    pub fn has_history(&self) -> bool {
        !self.history.is_empty()
    }

    pub fn tool_state(&self, call_id: &str) -> Option<&ToolState> {
        self.history.tool_states.get(call_id)
    }

    pub fn block_view_state(&self, id: BlockId) -> ViewState {
        self.history.view_state(id)
    }

    pub fn set_block_view_state(&mut self, id: BlockId, state: ViewState) {
        self.history.set_view_state(id, state);
    }

    pub fn block_status(&self, id: BlockId) -> Status {
        self.history.status(id)
    }

    pub fn set_block_status(&mut self, id: BlockId, status: Status) {
        self.history.set_status(id, status);
    }

    pub fn drain_finished_blocks(&mut self) -> Vec<BlockId> {
        self.history.drain_finished_blocks()
    }

    pub fn rewrite_block(&mut self, id: BlockId, block: Block) {
        self.history.rewrite(id, block);
    }

    pub fn push_streaming(&mut self, block: Block) -> BlockId {
        let id = self.history.push(block);
        self.history.set_status(id, Status::Streaming);
        id
    }

    pub fn streaming_block_ids(&self) -> Vec<BlockId> {
        self.history.streaming_block_ids().collect()
    }

    pub fn set_tool_state(&mut self, call_id: String, state: ToolState) {
        self.history.tool_states.insert(call_id, state);
    }

    pub fn update_tool_state(
        &mut self,
        call_id: &str,
        mutator: impl FnOnce(&mut ToolState),
    ) -> bool {
        let Some(state) = self.history.tool_states.get_mut(call_id) else {
            return false;
        };
        mutator(state);
        if let Some(id) = self.history.tool_block_id(call_id) {
            self.history.invalidate_block_layout(id);
        }
        true
    }

    // ── Mutations ─────────────────────────────────────────────────────

    pub fn push(&mut self, block: Block) {
        let block = match block {
            Block::Text { content } => {
                let t = content.trim();
                if t.is_empty() {
                    return;
                }
                Block::Text {
                    content: t.to_string(),
                }
            }
            Block::AgentMessage {
                from_id,
                from_slug,
                content,
            } => {
                let t = content.trim();
                if t.is_empty() {
                    return;
                }
                Block::AgentMessage {
                    from_id,
                    from_slug,
                    content: t.to_string(),
                }
            }
            Block::Thinking { content } => {
                let t = content.trim();
                if t.is_empty() {
                    return;
                }
                Block::Thinking {
                    content: t.to_string(),
                }
            }
            Block::Compacted { summary } => {
                let t = summary.trim();
                if t.is_empty() {
                    return;
                }
                Block::Compacted {
                    summary: t.to_string(),
                }
            }
            other => other,
        };
        self.history.push(block);
    }

    pub fn push_tool_call(&mut self, block: Block, state: ToolState) {
        debug_assert!(matches!(block, Block::ToolCall { .. }));
        let call_id = match &block {
            Block::ToolCall { call_id, .. } => call_id.clone(),
            _ => return,
        };
        self.history.push_with_state(block, call_id, state);
    }

    pub fn truncate_to(&mut self, block_idx: usize) {
        self.history.truncate(block_idx);
    }

    pub fn user_turns(&self) -> Vec<(usize, String)> {
        self.history
            .order
            .iter()
            .enumerate()
            .filter_map(|(i, id)| match self.history.blocks.get(id) {
                Some(Block::User { text, .. }) => Some((i, text.clone())),
                _ => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::display::SpanMeta;

    fn cell(ch: char) -> SnapshotCell {
        SnapshotCell {
            ch,
            meta: SpanMeta::default(),
        }
    }

    fn non_selectable(ch: char) -> SnapshotCell {
        SnapshotCell {
            ch,
            meta: SpanMeta {
                selectable: false,
                copy_as: None,
            },
        }
    }

    fn copy_as(ch: char, s: &str) -> SnapshotCell {
        SnapshotCell {
            ch,
            meta: SpanMeta {
                selectable: true,
                copy_as: Some(s.to_string()),
            },
        }
    }

    fn make_snapshot(row_cells: Vec<Vec<SnapshotCell>>) -> TranscriptSnapshot {
        let rows: Vec<String> = row_cells
            .iter()
            .map(|cells| cells.iter().map(|c| c.ch).collect())
            .collect();
        let len = rows.len();
        TranscriptSnapshot {
            width: 80,
            show_thinking: false,
            rows,
            row_cells,
            soft_wrapped: vec![false; len],
            source_text: vec![None; len],
            block_of_row: Vec::new(),
            row_of_block: HashMap::new(),
            generation: 0,
        }
    }

    #[test]
    fn copy_range_basic() {
        let snap = make_snapshot(vec![
            vec![cell('h'), cell('e'), cell('l'), cell('l'), cell('o')],
            vec![cell('w'), cell('o'), cell('r'), cell('l'), cell('d')],
        ]);
        assert_eq!(snap.copy_range(0, 0, 1, 5), "hello\nworld");
        assert_eq!(snap.copy_range(0, 2, 0, 5), "llo");
        assert_eq!(snap.copy_range(1, 0, 1, 3), "wor");
    }

    #[test]
    fn copy_range_skips_non_selectable() {
        let snap = make_snapshot(vec![vec![
            non_selectable('│'),
            non_selectable(' '),
            cell('h'),
            cell('i'),
        ]]);
        assert_eq!(snap.copy_range(0, 0, 0, 4), "hi");
    }

    #[test]
    fn copy_range_applies_copy_as() {
        let snap = make_snapshot(vec![vec![
            copy_as('+', ""),
            copy_as(' ', ""),
            cell('a'),
            cell('d'),
            cell('d'),
        ]]);
        assert_eq!(snap.copy_range(0, 0, 0, 5), "add");
    }

    #[test]
    fn snap_to_selectable_direct_hit() {
        let snap = make_snapshot(vec![vec![non_selectable('│'), cell('a'), cell('b')]]);
        assert_eq!(snap.snap_to_selectable(0, 1), Some((0, 1)));
    }

    #[test]
    fn snap_to_selectable_forward() {
        let snap = make_snapshot(vec![vec![
            non_selectable('│'),
            non_selectable(' '),
            cell('a'),
        ]]);
        assert_eq!(snap.snap_to_selectable(0, 0), Some((0, 2)));
    }

    #[test]
    fn snap_to_selectable_backward() {
        let snap = make_snapshot(vec![vec![
            cell('a'),
            non_selectable(' '),
            non_selectable('│'),
        ]]);
        assert_eq!(snap.snap_to_selectable(0, 2), Some((0, 0)));
    }

    #[test]
    fn snap_to_selectable_none() {
        let snap = make_snapshot(vec![vec![non_selectable('│'), non_selectable(' ')]]);
        assert_eq!(snap.snap_to_selectable(0, 0), None);
    }

    #[test]
    fn copy_range_uses_source_text_for_full_rows() {
        let mut snap = make_snapshot(vec![
            vec![cell('T'), cell('i'), cell('t'), cell('l'), cell('e')],
            vec![cell('h'), cell('e'), cell('l'), cell('l'), cell('o')],
        ]);
        snap.source_text[0] = Some("# Title".into());
        assert_eq!(snap.copy_range(0, 0, 0, 5), "# Title");
        assert_eq!(snap.copy_range(0, 0, 1, 5), "# Title\nhello");
    }

    #[test]
    fn copy_range_partial_row_ignores_source_text() {
        let mut snap = make_snapshot(vec![vec![
            cell('T'),
            cell('i'),
            cell('t'),
            cell('l'),
            cell('e'),
        ]]);
        snap.source_text[0] = Some("# Title".into());
        assert_eq!(snap.copy_range(0, 1, 0, 4), "itl");
    }

    #[test]
    fn copy_range_soft_wrapped_rows_coalesce() {
        let mut snap = make_snapshot(vec![
            vec![cell('h'), cell('e'), cell('l'), cell('l'), cell('o')],
            vec![cell('w'), cell('o'), cell('r'), cell('l'), cell('d')],
        ]);
        snap.source_text[0] = Some("hello world".into());
        snap.soft_wrapped[1] = true;
        assert_eq!(snap.copy_range(0, 0, 1, 5), "hello world");
    }
}
