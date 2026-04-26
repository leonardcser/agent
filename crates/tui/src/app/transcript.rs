//! Transcript ownership on `App` — block history, streaming state
//! (thinking / text / tools / agents / exec), projection to a
//! ui::Buffer, and the transcript-cursor glyph cache.

use super::transcript_model::{
    AgentBlockStatus, Block, BlockId, Status, ToolOutputRef, ToolState, ToolStatus, ViewState,
};
use super::*;
use crate::app::transcript_cache::{PersistedLayoutCache, RenderCache};
use crate::app::transcript_present as blocks;
use crate::app::transcript_present::{
    gap_between, render_thinking_summary, thinking_summary, Element,
};
use crate::content::layout_out::SpanCollector;
use crate::content::selection::wrap_and_locate_cursor;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

pub(crate) struct TranscriptData {
    pub clamped_scroll: u16,
    pub total_rows: u16,
    pub scrollbar_col: u16,
    pub viewport: ui::WindowViewport,
}

pub(crate) struct TranscriptCursor {
    pub clamped_line: u16,
    pub clamped_col: u16,
    pub soft_cursor: Option<crate::content::window_view::SoftCursor>,
}

impl App {
    pub fn block_count(&self) -> usize {
        self.transcript.block_count()
    }

    pub fn blocks(&self) -> Vec<Block> {
        self.transcript.blocks()
    }

    pub fn tool_states_snapshot(&self) -> HashMap<String, ToolState> {
        self.transcript.tool_states_snapshot()
    }

    pub fn start_active_agent(&mut self, agent_id: String) {
        self.parser
            .start_active_agent(&mut self.transcript.history, agent_id);
    }

    pub fn update_active_agent(
        &mut self,
        agent_id: &str,
        slug: Option<&str>,
        tool_calls: &[crate::app::AgentToolEntry],
        status: AgentBlockStatus,
    ) {
        self.parser.update_active_agent(
            &mut self.transcript.history,
            agent_id,
            slug,
            tool_calls,
            status,
        );
    }

    pub fn cancel_active_agents(&mut self) {
        self.parser
            .cancel_active_agents(&mut self.transcript.history);
    }

    pub fn finish_active_agent(&mut self, agent_id: &str) {
        self.parser
            .finish_active_agent(&mut self.transcript.history, agent_id);
    }

    pub fn finish_all_active_agents(&mut self) {
        self.parser
            .finish_all_active_agents(&mut self.transcript.history);
    }

    pub fn begin_turn(&mut self) {
        self.parser.begin_turn();
    }

    pub fn push_tool_call(&mut self, block: Block, state: ToolState) {
        self.transcript.push_tool_call(block, state);
    }

    pub fn push_block(&mut self, block: Block) {
        self.transcript.push(block);
    }

    pub fn append_streaming_thinking(&mut self, delta: &str) {
        self.parser
            .append_streaming_thinking(&mut self.transcript.history, delta);
    }

    pub fn flush_streaming_thinking(&mut self) {
        self.parser
            .flush_streaming_thinking(&mut self.transcript.history);
    }

    fn thinking_summary_gap(&self) -> u16 {
        if let Some(last) = self
            .transcript
            .history
            .order
            .iter()
            .rev()
            .filter_map(|id| self.transcript.history.blocks.get(id))
            .find(|b| !matches!(b, Block::Thinking { .. }))
        {
            gap_between(
                &Element::Block(last),
                &Element::Block(&Block::Thinking {
                    content: String::new(),
                }),
            )
        } else if self.transcript.history.is_empty() {
            0
        } else {
            1
        }
    }

    pub fn append_streaming_text(&mut self, delta: &str) {
        self.parser
            .append_streaming_text(&mut self.transcript.history, delta);
    }

    pub fn flush_streaming_text(&mut self) {
        self.parser
            .flush_streaming_text(&mut self.transcript.history);
    }

    pub fn start_tool(
        &mut self,
        call_id: String,
        name: String,
        summary: String,
        args: HashMap<String, serde_json::Value>,
    ) {
        self.parser
            .start_tool(&mut self.transcript.history, call_id, name, summary, args);
    }

    pub fn start_exec(&mut self, command: String) {
        self.parser
            .start_exec(&mut self.transcript.history, command);
    }

    pub fn append_exec_output(&mut self, chunk: &str) {
        self.parser
            .append_exec_output(&mut self.transcript.history, chunk);
    }

    pub fn finish_exec(&mut self, exit_code: Option<i32>) {
        self.parser.finish_exec(exit_code);
    }

    pub fn finalize_exec(&mut self) {
        self.parser.finalize_exec(&mut self.transcript.history);
    }

    pub fn has_active_exec(&self) -> bool {
        self.parser.has_active_exec()
    }

    pub fn append_active_output(&mut self, call_id: &str, chunk: &str) {
        self.parser
            .append_active_output(&mut self.transcript.history, call_id, chunk);
    }

    pub fn set_active_status(&mut self, call_id: &str, status: ToolStatus) {
        self.parser
            .set_active_status(&mut self.transcript.history, call_id, status);
    }

    pub fn set_active_user_message(&mut self, call_id: &str, msg: String) {
        self.parser
            .set_active_user_message(&mut self.transcript.history, call_id, msg);
    }

    pub fn finish_tool(
        &mut self,
        call_id: &str,
        status: ToolStatus,
        output: Option<ToolOutputRef>,
        engine_elapsed: Option<Duration>,
    ) {
        self.parser.finish_tool(
            &mut self.transcript.history,
            call_id,
            status,
            output,
            engine_elapsed,
        );
    }

    pub fn has_transcript_content(&mut self, show_thinking: bool) -> bool {
        !self.transcript.history.is_empty() || self.has_ephemeral(show_thinking)
    }

    /// Full transcript as one string per display row. Cheap when there
    /// are no ephemeral rows (returns an `Arc::clone` of the cached
    /// snapshot); otherwise clones the vec once to append ephemeral
    /// rows. Callers treat it as a `&[String]` via deref coercion.
    pub fn full_transcript_display_text(&mut self, show_thinking: bool) -> Arc<Vec<String>> {
        let tw = self.transcript_width() as u16;
        if !self.has_ephemeral(show_thinking) {
            let snap = self.transcript.snapshot(tw, show_thinking);
            return Arc::clone(&snap.rows);
        }
        let mut col = SpanCollector::new(tw);
        self.render_ephemeral_into(&mut col, tw as usize, show_thinking);
        let ephemeral_lines = col.finish().lines;
        let snap = self.transcript.snapshot(tw, show_thinking);
        let mut rows: Vec<String> = (*snap.rows).clone();
        for line in ephemeral_lines {
            let mut s = String::new();
            for span in &line.spans {
                s.push_str(&span.text);
            }
            rows.push(s);
        }
        Arc::new(rows)
    }

    /// Byte positions in `rows.join("\n")` of each `\n` separator,
    /// partitioned into soft-wrap continuations and real line breaks.
    /// Soft-wrap positions are "transparent" to word-select; hard
    /// positions are the boundaries used by line-select. Ephemeral
    /// rows (appended after the snapshot) are treated as hard breaks.
    pub fn transcript_line_breaks(&mut self, show_thinking: bool) -> (Vec<usize>, Vec<usize>) {
        let tw = self.transcript_width() as u16;
        let snap = self.transcript.snapshot(tw, show_thinking);
        let rows = snap.rows.clone();
        let mut soft = Vec::new();
        let mut hard = Vec::new();
        let mut pos = 0usize;
        for (i, row) in rows.iter().enumerate() {
            pos += row.len();
            if i + 1 < rows.len() {
                let next_is_soft = snap.soft_wrapped.get(i + 1).copied().unwrap_or(false);
                if next_is_soft {
                    soft.push(pos);
                } else {
                    hard.push(pos);
                }
                pos += 1;
            }
        }
        // Every boundary between the snapshot's last row and subsequent
        // ephemeral rows (and between ephemeral rows themselves) is a
        // hard break.
        let snap_row_count = rows.len();
        if self.has_ephemeral(show_thinking) {
            let mut col = SpanCollector::new(tw);
            self.render_ephemeral_into(&mut col, tw as usize, show_thinking);
            let mut first_ephemeral = true;
            for line in col.finish().lines {
                if !first_ephemeral || snap_row_count > 0 {
                    hard.push(pos);
                    pos += 1;
                }
                first_ephemeral = false;
                for span in &line.spans {
                    pos += span.text.len();
                }
            }
        }
        (soft, hard)
    }

    pub fn full_transcript_nav_text(&mut self, show_thinking: bool) -> Vec<String> {
        let tw = self.transcript_width() as u16;
        let snap = self.transcript.snapshot(tw, show_thinking);
        let mut rows = snap.nav_rows();
        if self.has_ephemeral(show_thinking) {
            let mut col = SpanCollector::new(tw);
            self.render_ephemeral_into(&mut col, tw as usize, show_thinking);
            for line in col.finish().lines {
                let mut s = String::new();
                for span in &line.spans {
                    if !span.meta.selectable {
                        continue;
                    }
                    s.push_str(&span.text);
                }
                rows.push(s);
            }
        }
        rows
    }

    pub fn nav_col_to_display_col(
        &mut self,
        abs_row: usize,
        nav_col: usize,
        show_thinking: bool,
    ) -> usize {
        let tw = self.transcript_width() as u16;
        let snap = self.transcript.snapshot(tw, show_thinking);
        let snap_rows = snap.row_cells.len();
        if abs_row < snap_rows {
            snap.nav_col_to_display_col(abs_row, nav_col)
        } else {
            nav_col
        }
    }

    pub fn display_col_to_nav_col(
        &mut self,
        abs_row: usize,
        display_col: usize,
        show_thinking: bool,
    ) -> usize {
        let tw = self.transcript_width() as u16;
        let snap = self.transcript.snapshot(tw, show_thinking);
        let snap_rows = snap.row_cells.len();
        if abs_row < snap_rows {
            snap.display_col_to_nav_col(abs_row, display_col)
        } else {
            display_col
        }
    }

    pub fn block_text_at_row(&mut self, abs_row: usize, show_thinking: bool) -> Option<String> {
        let tw = self.transcript_width() as u16;
        // Prefer the block's raw markdown source (text-bearing variants
        // expose `Block::raw_text`) so yanking a rendered markdown block
        // returns `**bold**`, `` `code` ``, fenced blocks, tables etc.
        // verbatim. Fall back to cell-walking for structured blocks
        // (tool / agent / confirm) whose "raw" form isn't a single
        // string.
        let block_id = {
            let snap = self.transcript.snapshot(tw, show_thinking);
            snap.block_of_row.get(abs_row).copied().flatten()
        };
        if let Some(id) = block_id {
            if let Some(raw) = self.transcript.block(id).and_then(|b| b.raw_text()) {
                return Some(raw);
            }
        }
        let snap = self.transcript.snapshot(tw, show_thinking);
        snap.block_text_at(abs_row)
    }

    pub fn snap_col_to_selectable(
        &mut self,
        abs_row: usize,
        col: usize,
        show_thinking: bool,
    ) -> usize {
        let tw = self.transcript_width() as u16;
        let snap = self.transcript.snapshot(tw, show_thinking);
        snap.snap_to_selectable(abs_row, col)
            .map(|(_, c)| c)
            .unwrap_or(col)
    }

    pub fn snap_cpos_to_selectable(
        &mut self,
        rows: &[String],
        cpos: usize,
        show_thinking: bool,
    ) -> usize {
        let tw = self.transcript_width() as u16;
        let snap = self.transcript.snapshot(tw, show_thinking);
        let (row, col) = snap.byte_to_row_col(cpos);
        if let Some((_, snapped_col)) = snap.snap_to_selectable(row, col) {
            if snapped_col == col {
                return cpos;
            }
            let mut offset = 0;
            for (r, line) in rows.iter().enumerate() {
                if r == row {
                    let byte_col: usize =
                        line.chars().take(snapped_col).map(|c| c.len_utf8()).sum();
                    return offset + byte_col;
                }
                offset += line.len() + 1;
            }
        }
        cpos
    }

    pub fn copy_display_range(&mut self, start: usize, end: usize, show_thinking: bool) -> String {
        let tw = self.transcript_width() as u16;
        let snap = self.transcript.snapshot(tw, show_thinking);
        snap.copy_byte_range(start, end)
    }

    pub fn finish_transcript_turn(&mut self) {
        let _perf = crate::perf::begin("render:finish_turn");
        self.parser
            .finalize_active_tools(&mut self.transcript.history);
    }

    pub fn finalize_active_tools(&mut self) {
        self.parser
            .finalize_active_tools(&mut self.transcript.history);
    }

    pub fn finalize_active_tools_as(&mut self, status: ToolStatus) {
        self.parser
            .finalize_active_tools_as(&mut self.transcript.history, status);
    }

    pub fn tool_state(&self, call_id: &str) -> Option<&ToolState> {
        self.transcript.tool_state(call_id)
    }

    pub fn block_view_state(&self, id: BlockId) -> ViewState {
        self.transcript.block_view_state(id)
    }

    pub fn set_block_view_state(&mut self, id: BlockId, state: ViewState) {
        self.transcript.set_block_view_state(id, state);
    }

    pub fn block_status(&self, id: BlockId) -> Status {
        self.transcript.block_status(id)
    }

    pub fn set_block_status(&mut self, id: BlockId, status: Status) {
        self.transcript.set_block_status(id, status);
    }

    pub fn drain_finished_blocks(&mut self) -> Vec<BlockId> {
        self.transcript.drain_finished_blocks()
    }

    pub fn rewrite_block(&mut self, id: BlockId, block: Block) {
        self.transcript.rewrite_block(id, block);
    }

    pub fn push_streaming(&mut self, block: Block) -> BlockId {
        self.transcript.push_streaming(block)
    }

    pub fn streaming_block_ids(&self) -> Vec<BlockId> {
        self.transcript.streaming_block_ids()
    }

    pub fn update_tool_state(
        &mut self,
        call_id: &str,
        mutator: impl FnOnce(&mut ToolState),
    ) -> bool {
        self.transcript.update_tool_state(call_id, mutator)
    }

    pub fn set_tool_state(&mut self, call_id: String, state: ToolState) {
        self.transcript.set_tool_state(call_id, state);
    }

    /// Invalidate the width-dependent block layout cache when the
    /// terminal width changes. Called from the resize handler; the
    /// projection picks up the fresh layouts on the next render.
    pub fn invalidate_for_width(&mut self, width: u16) {
        if width as usize != self.transcript.history.cache_width {
            self.transcript.history.invalidate_for_width(width as usize);
        }
    }

    pub fn clear_transcript(&mut self) {
        self.transcript.history.clear();
        self.parser.clear();
    }

    pub fn has_history(&self) -> bool {
        self.transcript.has_history()
    }

    pub fn export_render_cache(&self) -> Option<RenderCache> {
        let mut cache = RenderCache::new(String::new());
        for id in &self.transcript.history.order {
            if let Some(Block::ToolCall { call_id, .. }) = self.transcript.history.blocks.get(id) {
                if let Some(state) = self.transcript.history.tool_states.get(call_id) {
                    if let Some(out) = state.output.as_deref() {
                        if let Some(ir) = &out.render_cache {
                            cache.insert_tool_output(call_id.clone(), ir.clone());
                        }
                    }
                }
            }
        }
        if cache.tool_outputs.is_empty() {
            None
        } else {
            Some(cache)
        }
    }

    pub fn layout_cache_dirty(&self) -> bool {
        self.transcript.history.cache_dirty
    }

    pub fn export_layout_cache(&mut self) -> Option<PersistedLayoutCache> {
        if self.transcript.history.is_empty() {
            return None;
        }
        let mut cache = PersistedLayoutCache::new(crate::theme::is_light());
        for id in &self.transcript.history.order {
            let Some(block) = self.transcript.history.blocks.get(id) else {
                continue;
            };
            let persist = match block {
                Block::ToolCall { call_id, .. } => self
                    .transcript
                    .history
                    .tool_states
                    .get(call_id)
                    .map(|s| s.is_terminal())
                    .unwrap_or(false),
                _ => true,
            };
            if !persist {
                continue;
            }
            let hash = self.transcript.history.content_hash(*id);
            if cache.blocks.contains_key(&hash) {
                continue;
            }
            if let Some(artifact) = self.transcript.history.artifacts.get(id) {
                if !artifact.is_empty() {
                    cache.blocks.insert(hash, artifact.clone());
                }
            }
        }
        self.transcript.history.cache_dirty = false;
        if cache.blocks.is_empty() {
            return None;
        }
        crate::perf::record_value("layout_cache:artifacts", cache.blocks.len() as u64);
        let total_layouts: usize = cache.blocks.values().map(|a| a.layouts.len()).sum();
        crate::perf::record_value("layout_cache:layouts", total_layouts as u64);
        Some(cache)
    }

    pub fn import_layout_cache(&mut self, cache: PersistedLayoutCache) {
        if !cache.is_compatible(crate::theme::is_light()) {
            return;
        }
        let nw = self.ui.terminal_size().0;
        let mut by_hash: HashMap<u64, BlockId> = HashMap::new();
        for id in &self.transcript.history.order {
            let hash = self.transcript.history.content_hash(*id);
            by_hash.entry(hash).or_insert(*id);
        }
        for (hash, mut artifact) in cache.blocks {
            let Some(id) = by_hash.get(&hash).copied() else {
                continue;
            };
            let Some(block) = self.transcript.history.blocks.get(&id) else {
                continue;
            };
            let allow = match block {
                Block::ToolCall { call_id, .. } => self
                    .transcript
                    .history
                    .tool_states
                    .get(call_id)
                    .map(|s| s.is_terminal())
                    .unwrap_or(false),
                _ => true,
            };
            if !allow {
                continue;
            }
            artifact
                .layouts
                .retain(|(k, b)| k.width == nw || b.is_valid_at(nw));
            if artifact.is_empty() {
                continue;
            }
            self.transcript
                .history
                .artifacts
                .entry(id)
                .and_modify(|a| {
                    for (k, b) in &artifact.layouts {
                        a.insert(*k, b.clone());
                    }
                })
                .or_insert(artifact);
        }
        self.transcript.history.cache_width = nw as usize;
        self.transcript.history.cache_dirty = false;
    }

    pub fn user_turns(&self) -> Vec<(usize, String)> {
        self.transcript.user_turns()
    }

    pub fn truncate_to(&mut self, block_idx: usize) {
        self.transcript.truncate_to(block_idx);
        self.parser.clear_tools_and_agents();
    }

    /// Update spinner animation state. Call before rendering. Returns
    /// `true` if the spinner frame changed and the caller should
    /// redraw.
    pub fn update_spinner(&mut self) -> bool {
        let mut changed = false;
        if let (Some(elapsed), Some(prev_frame)) =
            (self.working.elapsed(), self.working.last_spinner_frame())
        {
            let frame = crate::content::spinner_frame_index(elapsed);
            if frame != prev_frame {
                self.working.set_last_spinner_frame(frame);
                changed = true;
            }
        }
        self.parser.tick_active_agents(&mut self.transcript.history);
        changed
    }

    /// Project transcript blocks into a `ui::Buffer`. Gated by generation —
    /// skips work when nothing changed since the last projection.
    pub(crate) fn project_transcript_buffer(
        &mut self,
        width: usize,
        viewport_rows: u16,
        scroll_top: u16,
        show_thinking: bool,
    ) -> TranscriptData {
        let gutters = crate::window::TRANSCRIPT_GUTTERS;
        let tw = (gutters.content_width(width as u16) as usize).max(1);
        let theme = crate::theme::snapshot();

        let ephemeral_lines: Vec<crate::content::display::DisplayLine> =
            if self.has_ephemeral(show_thinking) {
                let mut col = SpanCollector::new(tw as u16);
                self.render_ephemeral_into(&mut col, tw, show_thinking);
                col.finish().lines
            } else {
                Vec::new()
            };

        self.transcript_projection.project(
            &mut self.transcript.history,
            tw as u16,
            show_thinking,
            &theme,
            &ephemeral_lines,
        );

        let total_rows = self.transcript_projection.total_lines() as u16;

        let geom =
            crate::content::viewport::ViewportGeom::new(total_rows, viewport_rows, scroll_top);
        let clamped_scroll = geom.clamped_scroll();

        let layer_w = gutters.layer_width(width as u16);
        let scrollbar_col = match gutters.scrollbar {
            Some(crate::window::GutterSide::Left) => 0,
            _ => layer_w.saturating_sub(1),
        };

        // Snapshot visible rows for the soft-cursor glyph lookup in
        // `compute_transcript_cursor`.
        let buf = self.transcript_projection.buf();
        let start = clamped_scroll as usize;
        let end = (start + viewport_rows as usize).min(buf.line_count());
        self.last_viewport_text = buf.get_lines(start, end).to_vec();

        let viewport = ui::WindowViewport::new(
            ui::Rect::new(0, 0, tw as u16, viewport_rows),
            tw as u16,
            total_rows,
            clamped_scroll,
            ui::ScrollbarState::new(scrollbar_col, total_rows, viewport_rows),
        );

        TranscriptData {
            clamped_scroll,
            total_rows,
            scrollbar_col,
            viewport,
        }
    }

    /// Compute transcript cursor position for the compositor pipeline.
    pub(crate) fn compute_transcript_cursor(
        &self,
        width: usize,
        viewport_rows: u16,
        history_cursor_line: u16,
        history_cursor_col: u16,
        transcript_owns_cursor: bool,
        viewport: Option<&ui::WindowViewport>,
    ) -> TranscriptCursor {
        let gutters = crate::window::TRANSCRIPT_GUTTERS;
        let tw = (gutters.content_width(width as u16) as usize).max(1);

        if !transcript_owns_cursor || viewport_rows == 0 {
            return TranscriptCursor {
                clamped_line: history_cursor_line,
                clamped_col: history_cursor_col,
                soft_cursor: None,
            };
        }

        let visible = viewport
            .map(|v| v.total_rows.min(viewport_rows))
            .unwrap_or(viewport_rows);
        let max_line = visible.saturating_sub(1);
        let line = history_cursor_line.min(max_line);
        let max_col = (tw as u16).saturating_sub(1);
        let col = history_cursor_col.min(max_col);
        let under: char = self
            .last_viewport_text
            .get(line as usize)
            .map(|row| {
                let byte = ui::text::cell_to_byte(row, col as usize);
                row[byte..].chars().next()
            })
            .and_then(|c| c)
            .unwrap_or(' ');

        TranscriptCursor {
            clamped_line: line,
            clamped_col: history_cursor_col,
            soft_cursor: Some(crate::content::window_view::SoftCursor {
                col,
                row: line,
                glyph: under,
            }),
        }
    }

    /// Build the transcript's per-line selection ranges (absolute
    /// buffer line index, col_start, col_end) in display-cell units.
    /// Used by the per-frame sync to overlay selection-bg on top of
    /// the projected buffer's text highlights. Cheap no-op when no
    /// vim visual, cursor anchor, or yank-flash is active.
    pub(crate) fn transcript_selection_highlights(
        &mut self,
        scroll_top: u16,
        viewport_rows: u16,
    ) -> Vec<(usize, u16, u16)> {
        let vim_visual = matches!(
            self.transcript_window.vim.as_ref().map(|v| v.mode()),
            Some(crate::vim::ViMode::Visual | crate::vim::ViMode::VisualLine)
        );
        let anchor_set = self.transcript_window.win_cursor.anchor().is_some();
        let yank_flash = self
            .transcript_window
            .kill_ring
            .yank_flash_range(std::time::Instant::now())
            .is_some();
        if !vim_visual && !anchor_set && !yank_flash {
            return Vec::new();
        }

        let rows = self.full_transcript_display_text(self.settings.show_thinking);
        if rows.is_empty() {
            return Vec::new();
        }
        let buf = rows.join("\n");
        let cpos = self.transcript_window.compute_cpos(&rows);
        let active_selection = if let Some(vim) = self.transcript_window.vim.as_ref() {
            match vim.mode() {
                crate::vim::ViMode::Visual | crate::vim::ViMode::VisualLine => {
                    vim.visual_range(&buf, cpos)
                }
                _ => self.transcript_window.win_cursor.range(cpos),
            }
        } else {
            self.transcript_window.win_cursor.range(cpos)
        };
        // Fall back to the yank-flash range so the selection bg
        // briefly paints over the yanked text after `y`-family vim ops
        // (mirrors nvim's `vim.highlight.on_yank`).
        let (s, e) = match active_selection.or_else(|| {
            self.transcript_window
                .kill_ring
                .yank_flash_range(std::time::Instant::now())
        }) {
            Some(range) => range,
            None => return Vec::new(),
        };
        if s >= e {
            return Vec::new();
        }
        let first = scroll_top as usize;
        let last = (first + viewport_rows as usize).min(rows.len());
        let mut line_start = rows[..first].iter().map(|r| r.len() + 1).sum::<usize>();
        let mut out = Vec::new();
        for (idx, row) in rows.iter().enumerate().take(last).skip(first) {
            let line_end = line_start + row.len();
            if e > line_start && s <= line_end {
                let clip_s = s.saturating_sub(line_start).min(row.len());
                let clip_e = e.saturating_sub(line_start).min(row.len());
                let start_cell = ui::text::byte_to_cell(row, clip_s) as u16;
                let end_cell = ui::text::byte_to_cell(row, clip_e) as u16;
                if end_cell > start_cell {
                    out.push((idx, start_cell, end_cell));
                } else if row.is_empty() && s <= line_start && e > line_start {
                    // Empty line inside the selection: paint a single
                    // virtual cell so the user can see the line is part
                    // of the range. Mirrors vim's "$" virtual-space
                    // behavior on empty lines in v / V mode.
                    out.push((idx, 0, 1));
                }
            }
            line_start = line_end + 1;
        }
        out
    }

    fn has_ephemeral(&self, show_thinking: bool) -> bool {
        self.parser.has_active_thinking() && !show_thinking
    }

    fn render_ephemeral_into(&self, out: &mut SpanCollector, width: usize, show_thinking: bool) {
        let Some(at) = self.parser.active_thinking() else {
            return;
        };
        if show_thinking {
            return;
        }
        let mut combined = at.paragraph.clone();
        if !at.current_line.is_empty() {
            if !combined.is_empty() {
                combined.push('\n');
            }
            combined.push_str(&at.current_line);
        }
        if !combined.is_empty() {
            let (label, line_count) = thinking_summary(&combined);
            crate::content::emit_newlines(out, self.thinking_summary_gap());
            render_thinking_summary(out, width, &label, line_count, true);
        }
    }

    pub(crate) fn measure_prompt_height(
        &self,
        state: &crate::input::PromptState,
        width: usize,
        queued: &[String],
        prediction: Option<&str>,
    ) -> u16 {
        let usable = width.saturating_sub(2);
        let text_w = usable.saturating_sub(2).max(1);

        let stash: u16 = if state.stash.is_some() { 1 } else { 0 };

        let mut queued_rows = 0u16;
        for msg in queued {
            let geom = blocks::UserBlockGeometry::new(msg, text_w);
            for line in &geom.lines {
                let w = crate::content::layout_out::display_width(line);
                queued_rows += if w == 0 { 1 } else { w.div_ceil(text_w) as u16 };
            }
        }

        let show_prediction = prediction.is_some() && state.buf.is_empty();
        let input_rows: u16 = if show_prediction {
            1
        } else {
            let (visual_lines, _, _, _) = wrap_and_locate_cursor(&state.buf, &[], 0, usable);
            visual_lines.len() as u16
        };

        queued_rows + stash + 1 + input_rows + 1 + 1
    }
}
