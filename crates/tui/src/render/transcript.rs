//! Transcript domain state: block store + streaming handles.
//!
//! `Transcript` owns the block history and the in-flight streaming
//! state (thinking / text / tools / agents / exec). `Screen` holds a
//! `Transcript` and delegates all content mutations through it.

use super::history::{
    ActiveAgent, ActiveText, ActiveThinking, ActiveTool, AgentBlockStatus, Block, BlockHistory,
    BlockId, LayoutKey, Status, ToolOutput, ToolOutputRef, ToolState, ToolStatus, ViewState,
};
use super::is_table_separator;
use std::collections::HashMap;
use std::ops::Range;
use std::time::{Duration, Instant};

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
            if r > start_row {
                out.push('\n');
            }
            let c_start = if r == start_row { start_col } else { 0 };
            let c_end = if r == end_row {
                end_col.min(cells.len())
            } else {
                cells.len()
            };
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

    /// Snap a `(row, col)` position to the nearest selectable cell,
    /// searching forward then backward on the same row. Returns the
    /// adjusted `(row, col)` or `None` if the row has no selectable cells.
    pub fn snap_to_selectable(&self, row: usize, col: usize) -> Option<(usize, usize)> {
        let cells = self.row_cells.get(row)?;
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
        for c in (0..col).rev() {
            if cells[c].meta.selectable {
                return Some((row, c));
            }
        }
        None
    }
}

pub struct Transcript {
    pub(super) history: BlockHistory,
    pub(super) active_thinking: Option<ActiveThinking>,
    pub(super) active_text: Option<ActiveText>,
    pub(super) stream_exec_id: Option<BlockId>,
    pub(super) active_tools: Vec<ActiveTool>,
    pub(super) active_agents: Vec<ActiveAgent>,
    cached_snapshot: Option<TranscriptSnapshot>,
}

impl Transcript {
    pub fn new() -> Self {
        Self {
            history: BlockHistory::new(),
            active_thinking: None,
            active_text: None,
            stream_exec_id: None,
            active_tools: Vec::new(),
            active_agents: Vec::new(),
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
        let mut block_of_row: Vec<Option<BlockId>> = Vec::new();
        let mut row_of_block: HashMap<BlockId, Range<u16>> = HashMap::new();

        for i in 0..self.history.order.len() {
            let gap = self.history.block_gap(i);
            for _ in 0..gap {
                rows.push(String::new());
                row_cells.push(Vec::new());
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
                        for ch in span.text.chars() {
                            text.push(ch);
                            cells.push(SnapshotCell {
                                ch,
                                meta: span.meta.clone(),
                            });
                        }
                    }
                    rows.push(text);
                    row_cells.push(cells);
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
            block_of_row,
            row_of_block,
            generation: self.history.generation(),
        }
    }

    pub fn clear_active_state(&mut self) {
        self.active_thinking = None;
        self.active_text = None;
        self.active_tools.clear();
        self.active_agents.clear();
        self.stream_exec_id = None;
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

    pub fn has_active_exec(&self) -> bool {
        self.stream_exec_id.is_some()
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

    // ── Turn lifecycle ────────────────────────────────────────────────

    pub fn begin_turn(&mut self) {
        self.active_tools.clear();
    }

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

    // ── Streaming thinking ───────────────────────────────────────────

    pub fn append_streaming_thinking(&mut self, delta: &str) {
        let at = self.active_thinking.get_or_insert_with(|| ActiveThinking {
            current_line: String::new(),
            paragraph: String::new(),
            streaming_id: None,
        });

        for ch in delta.chars() {
            if ch == '\r' {
                continue;
            }
            if ch == '\n' {
                let line = std::mem::take(&mut at.current_line);
                if line.trim().is_empty() && !at.paragraph.is_empty() {
                    at.paragraph.push('\n');
                    let para = std::mem::take(&mut at.paragraph);
                    if let Some(id) = at.streaming_id.take() {
                        self.history.rewrite(id, Block::Thinking { content: para });
                        self.history.set_status(id, Status::Done);
                    } else {
                        self.history.push(Block::Thinking { content: para });
                    }
                } else {
                    if !at.paragraph.is_empty() {
                        at.paragraph.push('\n');
                    }
                    at.paragraph.push_str(&line);
                }
            } else {
                at.current_line.push(ch);
            }
        }
        let preview = match (at.paragraph.is_empty(), at.current_line.is_empty()) {
            (true, true) => None,
            (true, false) => Some(at.current_line.clone()),
            (false, true) => Some(at.paragraph.clone()),
            (false, false) => Some(format!("{}\n{}", at.paragraph, at.current_line)),
        };
        if let Some(content) = preview.filter(|t| !t.trim().is_empty()) {
            let block = Block::Thinking { content };
            if let Some(id) = at.streaming_id {
                self.history.rewrite(id, block);
            } else {
                let id = self.history.push(block);
                self.history.set_status(id, Status::Streaming);
                at.streaming_id = Some(id);
            }
        }
    }

    pub fn flush_streaming_thinking(&mut self) {
        if let Some(mut at) = self.active_thinking.take() {
            if !at.current_line.is_empty() {
                if !at.paragraph.is_empty() {
                    at.paragraph.push('\n');
                }
                at.paragraph.push_str(&at.current_line);
            }
            let trimmed = at.paragraph.trim().to_string();
            if let Some(id) = at.streaming_id {
                if trimmed.is_empty() {
                    self.history.rewrite(
                        id,
                        Block::Thinking {
                            content: String::new(),
                        },
                    );
                } else {
                    self.history
                        .rewrite(id, Block::Thinking { content: trimmed });
                }
                self.history.set_status(id, Status::Done);
            } else if !trimmed.is_empty() {
                self.history.push(Block::Thinking { content: trimmed });
            }
        }
    }

    // ── Streaming text ───────────────────────────────────────────────

    pub fn append_streaming_text(&mut self, delta: &str) {
        if self.active_thinking.is_some() {
            self.flush_streaming_thinking();
        }

        let at = self.active_text.get_or_insert_with(|| ActiveText {
            current_line: String::new(),
            paragraph: String::new(),
            in_code_block: None,
            table_rows: Vec::new(),
            table_data_rows: 0,
            streaming_id: None,
            table_streaming_id: None,
            code_line_streaming_id: None,
        });

        for ch in delta.chars() {
            if ch == '\r' {
                continue;
            }
            if ch == '\n' {
                let line = std::mem::take(&mut at.current_line);
                Self::process_text_line(&mut self.history, at, &line);
            } else {
                at.current_line.push(ch);
            }
        }
        Self::sync_streaming_text(&mut self.history, at);
    }

    fn sync_streaming_text(history: &mut BlockHistory, at: &mut ActiveText) {
        if let Some(ref lang) = at.in_code_block {
            if !at.current_line.is_empty() {
                let block = Block::CodeLine {
                    content: at.current_line.clone(),
                    lang: lang.clone(),
                };
                if let Some(id) = at.code_line_streaming_id {
                    history.rewrite(id, block);
                } else {
                    let id = history.push(block);
                    history.set_status(id, Status::Streaming);
                    at.code_line_streaming_id = Some(id);
                }
            }
            return;
        }
        let in_table = !at.table_rows.is_empty() || at.current_line.trim_start().starts_with('|');
        if in_table {
            let mut content = String::new();
            for row in &at.table_rows {
                if !content.is_empty() {
                    content.push('\n');
                }
                content.push_str(row);
            }
            if at.current_line.trim_start().starts_with('|') {
                if !content.is_empty() {
                    content.push('\n');
                }
                content.push_str(&at.current_line);
            }
            if content.is_empty() {
                return;
            }
            let block = Block::Text { content };
            if let Some(id) = at.table_streaming_id {
                history.rewrite(id, block);
            } else {
                let id = history.push(block);
                history.set_status(id, Status::Streaming);
                at.table_streaming_id = Some(id);
            }
            return;
        }
        let preview = match (at.paragraph.is_empty(), at.current_line.is_empty()) {
            (true, true) => None,
            (true, false) => Some(at.current_line.clone()),
            (false, true) => Some(at.paragraph.clone()),
            (false, false) => Some(format!("{}\n{}", at.paragraph, at.current_line)),
        };
        let Some(content) = preview.filter(|t| !t.trim().is_empty()) else {
            return;
        };
        let block = Block::Text { content };
        if let Some(id) = at.streaming_id {
            history.rewrite(id, block);
        } else {
            let id = history.push(block);
            history.set_status(id, Status::Streaming);
            at.streaming_id = Some(id);
        }
    }

    fn process_text_line(history: &mut BlockHistory, at: &mut ActiveText, line: &str) {
        let trimmed = line.trim_start();

        if trimmed.starts_with("```") {
            if at.in_code_block.is_some() {
                if let Some(id) = at.code_line_streaming_id.take() {
                    history.set_status(id, Status::Done);
                }
                at.in_code_block = None;
                return;
            } else {
                Self::flush_paragraph(history, at);
                Self::flush_table(history, at);
                let lang = trimmed.trim_start_matches('`').trim().to_string();
                at.in_code_block = Some(lang);
                return;
            }
        }

        if let Some(ref lang) = at.in_code_block {
            let block = Block::CodeLine {
                content: line.to_string(),
                lang: lang.clone(),
            };
            if let Some(id) = at.code_line_streaming_id.take() {
                history.rewrite(id, block);
                history.set_status(id, Status::Done);
            } else {
                history.push(block);
            }
            return;
        }

        if trimmed.starts_with('|') {
            Self::flush_paragraph(history, at);
            if !is_table_separator(line) {
                at.table_data_rows += 1;
            }
            at.table_rows.push(line.to_string());
            return;
        }

        if line.trim().is_empty() {
            if !at.table_rows.is_empty() {
                return;
            }
            if !at.paragraph.is_empty() {
                Self::flush_paragraph(history, at);
            }
            return;
        }

        Self::flush_table(history, at);

        if !at.paragraph.is_empty() {
            at.paragraph.push('\n');
        }
        at.paragraph.push_str(line);
    }

    fn flush_table(history: &mut BlockHistory, at: &mut ActiveText) {
        if !at.table_rows.is_empty() {
            let content = std::mem::take(&mut at.table_rows).join("\n");
            if let Some(id) = at.table_streaming_id.take() {
                history.rewrite(id, Block::Text { content });
                history.set_status(id, Status::Done);
            } else {
                history.push(Block::Text { content });
            }
            at.table_data_rows = 0;
        } else if let Some(id) = at.table_streaming_id.take() {
            history.set_status(id, Status::Done);
        }
    }

    fn flush_paragraph(history: &mut BlockHistory, at: &mut ActiveText) {
        let para = std::mem::take(&mut at.paragraph);
        let trimmed = para.trim().to_string();
        if let Some(id) = at.streaming_id.take() {
            if trimmed.is_empty() {
                history.rewrite(
                    id,
                    Block::Text {
                        content: String::new(),
                    },
                );
            } else {
                history.rewrite(id, Block::Text { content: trimmed });
            }
            history.set_status(id, Status::Done);
        } else if !trimmed.is_empty() {
            history.push(Block::Text { content: trimmed });
        }
    }

    pub fn flush_streaming_text(&mut self) {
        self.flush_streaming_thinking();
        if let Some(mut at) = self.active_text.take() {
            if at.in_code_block.is_some() {
                if at.current_line.trim_start().starts_with("```") {
                    at.current_line.clear();
                    if let Some(id) = at.code_line_streaming_id.take() {
                        self.history.set_status(id, Status::Done);
                    }
                } else if !at.current_line.is_empty() {
                    let lang = at.in_code_block.as_ref().unwrap().clone();
                    let block = Block::CodeLine {
                        content: std::mem::take(&mut at.current_line),
                        lang,
                    };
                    if let Some(id) = at.code_line_streaming_id.take() {
                        self.history.rewrite(id, block);
                        self.history.set_status(id, Status::Done);
                    } else {
                        self.history.push(block);
                    }
                } else if let Some(id) = at.code_line_streaming_id.take() {
                    self.history.set_status(id, Status::Done);
                }
                at.in_code_block = None;
            }
            if !at.current_line.is_empty() && at.current_line.trim_start().starts_with('|') {
                at.table_rows.push(std::mem::take(&mut at.current_line));
            }
            Self::flush_table(&mut self.history, &mut at);
            if !at.current_line.is_empty() {
                if !at.paragraph.is_empty() {
                    at.paragraph.push('\n');
                }
                at.paragraph.push_str(&at.current_line);
            }
            Self::flush_paragraph(&mut self.history, &mut at);
        }
    }

    // ── Tool lifecycle ───────────────────────────────────────────────

    pub fn start_tool(
        &mut self,
        call_id: String,
        name: String,
        summary: String,
        args: HashMap<String, serde_json::Value>,
    ) {
        let start_time = Instant::now();
        let block = Block::ToolCall {
            call_id: call_id.clone(),
            name: name.clone(),
            summary,
            args,
        };
        let state = ToolState {
            status: ToolStatus::Pending,
            elapsed: None,
            output: None,
            user_message: None,
        };
        let block_id = self.history.push_with_state(block, call_id.clone(), state);
        self.history.set_status(block_id, Status::Streaming);
        self.active_tools.push(ActiveTool {
            call_id,
            name,
            block_id,
            start_time,
        });
    }

    fn resolve_active_call_id(&self, call_id: &str) -> Option<String> {
        if !call_id.is_empty() {
            return Some(call_id.to_string());
        }
        self.active_tools
            .last()
            .map(|t| t.call_id.clone())
            .or_else(|| self.last_tool_call_id())
    }

    fn last_tool_call_id(&self) -> Option<String> {
        self.history
            .order
            .iter()
            .rev()
            .find_map(|id| match self.history.blocks.get(id) {
                Some(Block::ToolCall { call_id, .. }) => Some(call_id.clone()),
                _ => None,
            })
    }

    pub fn append_active_output(&mut self, call_id: &str, chunk: &str) {
        let Some(cid) = self.resolve_active_call_id(call_id) else {
            return;
        };
        let chunk = chunk.to_string();
        self.update_tool_state(&cid, move |state| match state.output {
            Some(ref mut out) => {
                if !out.content.is_empty() {
                    out.content.push('\n');
                }
                out.content.push_str(&chunk);
            }
            None => {
                state.output = Some(Box::new(ToolOutput {
                    content: chunk,
                    is_error: false,
                    metadata: None,
                    render_cache: None,
                }));
            }
        });
    }

    pub fn set_active_status(&mut self, call_id: &str, status: ToolStatus) {
        let Some(cid) = self.resolve_active_call_id(call_id) else {
            return;
        };
        if let Some(active) = self.active_tools.iter_mut().find(|t| t.call_id == cid) {
            if matches!(
                self.history.tool_states.get(&cid).map(|s| s.status),
                Some(ToolStatus::Confirm)
            ) && status == ToolStatus::Pending
            {
                active.start_time = Instant::now();
            }
        }
        self.update_tool_state(&cid, |state| state.status = status);
    }

    pub fn set_active_user_message(&mut self, call_id: &str, msg: String) {
        let Some(cid) = self.resolve_active_call_id(call_id) else {
            return;
        };
        self.update_tool_state(&cid, |state| state.user_message = Some(msg));
    }

    pub fn finish_tool(
        &mut self,
        call_id: &str,
        status: ToolStatus,
        output: Option<ToolOutputRef>,
        engine_elapsed: Option<Duration>,
    ) {
        let Some(cid) = self.resolve_active_call_id(call_id) else {
            return;
        };
        let active_idx = self.active_tools.iter().position(|t| t.call_id == cid);
        let elapsed = if status == ToolStatus::Denied {
            None
        } else if let Some(idx) = active_idx {
            let tool = &self.active_tools[idx];
            engine_elapsed.or_else(|| tool.elapsed())
        } else {
            engine_elapsed
        };
        self.update_tool_state(&cid, |state| {
            state.status = status;
            if let Some(out) = output {
                state.output = Some(out);
            }
            state.elapsed = elapsed;
        });
        if let Some(idx) = active_idx {
            let block_id = self.active_tools[idx].block_id;
            self.active_tools.remove(idx);
            self.history.set_status(block_id, Status::Done);
        }
    }

    pub fn finalize_active_tools(&mut self) {
        self.finalize_active_tools_as(ToolStatus::Err);
    }

    pub fn finalize_active_tools_as(&mut self, status: ToolStatus) {
        self.finish_all_active_agents();
        let tools: Vec<ActiveTool> = self.active_tools.drain(..).collect();
        for tool in tools {
            let elapsed = if status == ToolStatus::Denied {
                None
            } else {
                tool.elapsed()
            };
            self.history.set_status(tool.block_id, Status::Done);
            let cid = tool.call_id.clone();
            self.update_tool_state(&cid, |state| {
                state.status = status;
                state.elapsed = elapsed;
            });
        }
    }

    // ── Exec lifecycle ───────────────────────────────────────────────

    pub fn start_exec(&mut self, command: String) {
        let id = self.history.push(Block::Exec {
            command,
            output: String::new(),
        });
        self.history.set_status(id, Status::Streaming);
        self.stream_exec_id = Some(id);
    }

    pub fn append_exec_output(&mut self, chunk: &str) {
        let Some(id) = self.stream_exec_id else {
            return;
        };
        let Some(Block::Exec { command, output }) = self.history.blocks.get(&id).cloned() else {
            return;
        };
        let mut new_output = output;
        if !new_output.is_empty() && !new_output.ends_with('\n') {
            new_output.push('\n');
        }
        new_output.push_str(chunk);
        self.history.rewrite(
            id,
            Block::Exec {
                command,
                output: new_output,
            },
        );
    }

    pub fn finish_exec(&mut self, _exit_code: Option<i32>) {}

    pub fn finalize_exec(&mut self) {
        let Some(id) = self.stream_exec_id.take() else {
            return;
        };
        if let Some(Block::Exec { command, output }) = self.history.blocks.get(&id).cloned() {
            let mut trimmed = output;
            trimmed.truncate(trimmed.trim_end().len());
            self.history.rewrite(
                id,
                Block::Exec {
                    command,
                    output: trimmed,
                },
            );
        }
        self.history.set_status(id, Status::Done);
    }

    // ── Agent lifecycle ──────────────────────────────────────────────

    pub fn start_active_agent(&mut self, agent_id: String) {
        let start_time = Instant::now();
        let block = Block::Agent {
            agent_id: agent_id.clone(),
            slug: None,
            blocking: true,
            tool_calls: Vec::new(),
            status: AgentBlockStatus::Running,
            elapsed: Some(Duration::from_secs(0)),
        };
        let block_id = self.history.push(block);
        self.history.set_status(block_id, Status::Streaming);
        self.active_agents.push(ActiveAgent {
            agent_id,
            block_id,
            start_time,
            final_elapsed: None,
        });
    }

    pub fn update_active_agent(
        &mut self,
        agent_id: &str,
        slug: Option<&str>,
        tool_calls: &[crate::app::AgentToolEntry],
        status: AgentBlockStatus,
    ) {
        let (block_id, elapsed) = {
            let Some(active) = self
                .active_agents
                .iter_mut()
                .find(|a| a.agent_id == agent_id)
            else {
                return;
            };
            if status != AgentBlockStatus::Running && active.final_elapsed.is_none() {
                active.final_elapsed = Some(active.start_time.elapsed());
            }
            let elapsed = active
                .final_elapsed
                .unwrap_or_else(|| active.start_time.elapsed());
            (active.block_id, elapsed)
        };
        self.history.rewrite(
            block_id,
            Block::Agent {
                agent_id: agent_id.to_string(),
                slug: slug.map(str::to_string),
                blocking: true,
                tool_calls: tool_calls.to_vec(),
                status,
                elapsed: Some(elapsed),
            },
        );
    }

    pub fn cancel_active_agents(&mut self) {
        type AgentCancel = (
            BlockId,
            String,
            Duration,
            Vec<crate::app::AgentToolEntry>,
            Option<String>,
        );
        let updates: Vec<AgentCancel> = self
            .active_agents
            .iter_mut()
            .map(|a| {
                if a.final_elapsed.is_none() {
                    a.final_elapsed = Some(a.start_time.elapsed());
                }
                let elapsed = a.final_elapsed.unwrap_or_else(|| a.start_time.elapsed());
                let (slug, tool_calls) = match self.history.blocks.get(&a.block_id) {
                    Some(Block::Agent {
                        slug, tool_calls, ..
                    }) => (slug.clone(), tool_calls.clone()),
                    _ => (None, Vec::new()),
                };
                (a.block_id, a.agent_id.clone(), elapsed, tool_calls, slug)
            })
            .collect();
        for (block_id, agent_id, elapsed, tool_calls, slug) in updates {
            self.history.rewrite(
                block_id,
                Block::Agent {
                    agent_id,
                    slug,
                    blocking: true,
                    tool_calls,
                    status: AgentBlockStatus::Error,
                    elapsed: Some(elapsed),
                },
            );
        }
    }

    pub fn finish_active_agent(&mut self, agent_id: &str) {
        let Some(idx) = self
            .active_agents
            .iter()
            .position(|a| a.agent_id == agent_id)
        else {
            return;
        };
        let mut active = self.active_agents.remove(idx);
        if active.final_elapsed.is_none() {
            active.final_elapsed = Some(active.start_time.elapsed());
        }
        let elapsed = active
            .final_elapsed
            .unwrap_or_else(|| active.start_time.elapsed());
        let (slug, tool_calls, status) = match self.history.blocks.get(&active.block_id) {
            Some(Block::Agent {
                slug,
                tool_calls,
                status,
                ..
            }) => {
                let next = if *status == AgentBlockStatus::Running {
                    AgentBlockStatus::Done
                } else {
                    *status
                };
                (slug.clone(), tool_calls.clone(), next)
            }
            _ => (None, Vec::new(), AgentBlockStatus::Done),
        };
        self.history.rewrite(
            active.block_id,
            Block::Agent {
                agent_id: active.agent_id,
                slug,
                blocking: true,
                tool_calls,
                status,
                elapsed: Some(elapsed),
            },
        );
        self.history.set_status(active.block_id, Status::Done);
    }

    pub fn finish_all_active_agents(&mut self) {
        let ids: Vec<String> = self
            .active_agents
            .iter()
            .map(|a| a.agent_id.clone())
            .collect();
        for id in ids {
            self.finish_active_agent(&id);
        }
    }

    pub fn tick_active_agents(&mut self) {
        let ticks: Vec<(BlockId, Duration)> = self
            .active_agents
            .iter()
            .filter(|a| a.final_elapsed.is_none())
            .map(|a| (a.block_id, a.start_time.elapsed()))
            .collect();
        for (block_id, elapsed) in ticks {
            let Some(Block::Agent {
                agent_id,
                slug,
                tool_calls,
                status,
                ..
            }) = self.history.blocks.get(&block_id).cloned()
            else {
                continue;
            };
            self.history.rewrite(
                block_id,
                Block::Agent {
                    agent_id,
                    slug,
                    blocking: true,
                    tool_calls,
                    status,
                    elapsed: Some(elapsed),
                },
            );
        }
    }

    // ── Bulk operations ──────────────────────────────────────────────

    pub fn truncate_to(&mut self, block_idx: usize) {
        self.history.truncate(block_idx);
        self.active_tools.clear();
        self.active_agents.clear();
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
        TranscriptSnapshot {
            width: 80,
            show_thinking: false,
            rows,
            row_cells,
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
        let snap = make_snapshot(vec![vec![
            non_selectable('│'),
            cell('a'),
            cell('b'),
        ]]);
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
        let snap = make_snapshot(vec![vec![
            non_selectable('│'),
            non_selectable(' '),
        ]]);
        assert_eq!(snap.snap_to_selectable(0, 0), None);
    }
}
