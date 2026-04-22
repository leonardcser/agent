//! Top-level chat screen: block history, streaming state, prompt composition.
//!
//! `Screen` is the render module's main state object — it owns the
//! block history, active streaming overlays (thinking / text / tools /
//! agents / exec), and all the flags that feed the status line and
//! prompt rendering. `draw_frame` is the single entry point called
//! from the main loop; it renders blocks (scroll mode), the ephemeral
//! overlay, and the prompt (or dialog placement) atomically.

use super::blocks;
use super::blocks::{gap_between, render_thinking_summary, thinking_summary, Element};
use super::cache::{PersistedLayoutCache, RenderCache};
use super::history::{
    AgentBlockStatus, Block, BlockId, Status, ToolOutputRef, ToolState, ToolStatus, ViewState,
};
use super::transcript::Transcript;
use super::transcript_buf::TranscriptProjection;

pub(crate) struct TranscriptData {
    pub clamped_scroll: u16,
    pub total_rows: u16,
    pub scrollbar_col: u16,
}

pub(crate) struct TranscriptCursor {
    pub clamped_line: u16,
    pub clamped_col: u16,
    pub soft_cursor: Option<super::window_view::SoftCursor>,
}

/// Visual selection in the content pane, captured from vim state.
/// Line indices are 0-based from the top of the full transcript; cols
/// count chars on that line.
#[derive(Clone, Copy, Debug)]
pub struct ContentVisualRange {
    pub start_line: usize,
    pub start_col: usize,
    pub end_line: usize,
    pub end_col: usize,
    pub kind: ContentVisualKind,
}

#[derive(Clone, Copy, Debug)]
pub enum ContentVisualKind {
    Char,
    Line,
}

use super::layout_out::{LayoutSink, SpanCollector};
use super::prompt::PromptState;
use super::selection::wrap_and_locate_cursor;
use super::{emit_newlines, Frame, StdioBackend, TerminalBackend, SPINNER_FRAMES};
use crate::input::InputState;

use crossterm::{cursor, terminal, QueueableCommand};
use std::collections::HashMap;
use std::time::Duration;

pub struct Screen {
    pub(crate) transcript: Transcript,
    parser: super::stream_parser::StreamParser,
    prompt: PromptState,
    dirty: bool,
    /// Plain-text snapshot of each visible row (top to bottom) captured
    /// during `draw_viewport_frame`. Used by the content pane's motion
    /// handlers and yank to reason over what the user actually sees.
    last_viewport_text: Vec<String>,
    last_viewport_lines: Vec<super::display::DisplayLine>,
    /// Gutter reservations for the transcript window (padding +
    /// scrollbar column). Pushed from `App` so the paint path picks up
    /// the authoritative source without a reverse dependency.
    transcript_gutters: crate::window::WindowGutters,
    last_transcript_viewport: Option<super::region::Viewport>,
    /// Centralized layout state computed at the start of each frame.
    pub(crate) layout: super::layout::LayoutState,
    /// Buffer-backed transcript projection — blocks projected at event time.
    pub(crate) transcript_projection: TranscriptProjection,
    /// Terminal I/O backend (real terminal or test buffer).
    backend: Box<dyn TerminalBackend>,
}

/// A short ephemeral notification rendered above the prompt bar.
#[derive(Clone)]
pub struct Notification {
    pub message: String,
    pub is_error: bool,
}

impl Default for Screen {
    fn default() -> Self {
        Self::new()
    }
}

impl Screen {
    pub fn new() -> Self {
        Self::with_backend(Box::new(StdioBackend))
    }

    pub fn with_backend(backend: Box<dyn TerminalBackend>) -> Self {
        Self {
            transcript: Transcript::new(),
            parser: super::stream_parser::StreamParser::new(),
            prompt: PromptState::new(),
            dirty: true,
            last_viewport_text: Vec::new(),
            last_viewport_lines: Vec::new(),
            transcript_gutters: crate::window::WindowGutters {
                pad_left: 1,
                scrollbar: Some(crate::window::GutterSide::Right),
            },
            last_transcript_viewport: None,
            layout: super::layout::LayoutState::compute(&super::layout::LayoutInput {
                term_width: 80,
                term_height: 24,
                prompt_height: 3,
            }),
            transcript_projection: TranscriptProjection::new(ui::buffer::Buffer::new(
                ui::BufId(0),
                ui::buffer::BufCreateOpts {
                    modifiable: true,
                    buftype: ui::buffer::BufType::Nofile,
                },
            )),
            backend,
        }
    }

    pub fn size(&self) -> (u16, u16) {
        self.backend.size()
    }

    /// Width available for transcript content. Reserves the rightmost
    /// column for the scrollbar track so the scrollbar never overpaints
    /// rendered content and mouse hit-testing has a stable target.
    pub fn transcript_gutters(&self) -> &crate::window::WindowGutters {
        &self.transcript_gutters
    }

    pub fn transcript_width(&self) -> usize {
        let (w, _) = self.backend.size();
        (self.transcript_gutters.content_width(w) as usize).max(1)
    }

    /// Expose the backend for dialogs that need output + size.
    pub fn backend(&self) -> &dyn TerminalBackend {
        &*self.backend
    }

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
        self.dirty = true;
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
        self.dirty = true;
    }

    pub fn cancel_active_agents(&mut self) {
        self.parser
            .cancel_active_agents(&mut self.transcript.history);
    }

    pub fn finish_active_agent(&mut self, agent_id: &str) {
        self.parser
            .finish_active_agent(&mut self.transcript.history, agent_id);
        self.dirty = true;
    }

    pub fn finish_all_active_agents(&mut self) {
        self.parser
            .finish_all_active_agents(&mut self.transcript.history);
        self.dirty = true;
    }

    pub fn begin_turn(&mut self) {
        self.parser.begin_turn();
    }

    pub fn push_tool_call(&mut self, block: Block, state: ToolState) {
        self.transcript.push_tool_call(block, state);
        self.dirty = true;
    }

    pub fn push(&mut self, block: Block) {
        self.transcript.push(block);
        self.dirty = true;
    }

    pub fn append_streaming_thinking(&mut self, delta: &str) {
        self.parser
            .append_streaming_thinking(&mut self.transcript.history, delta);
        self.dirty = true;
    }

    pub fn flush_streaming_thinking(&mut self) {
        self.parser
            .flush_streaming_thinking(&mut self.transcript.history);
        self.dirty = true;
    }

    /// Gap before a thinking summary overlay, skipping over hidden thinking blocks.
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
        self.dirty = true;
    }

    pub fn flush_streaming_text(&mut self) {
        self.parser
            .flush_streaming_text(&mut self.transcript.history);
        self.dirty = true;
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
        self.dirty = true;
    }

    pub fn start_exec(&mut self, command: String) {
        self.parser
            .start_exec(&mut self.transcript.history, command);
        self.dirty = true;
    }

    pub fn append_exec_output(&mut self, chunk: &str) {
        self.parser
            .append_exec_output(&mut self.transcript.history, chunk);
        self.dirty = true;
    }

    pub fn finish_exec(&mut self, exit_code: Option<i32>) {
        self.parser.finish_exec(exit_code);
        self.dirty = true;
    }

    pub fn finalize_exec(&mut self) {
        self.parser.finalize_exec(&mut self.transcript.history);
        self.dirty = true;
    }

    pub fn has_active_exec(&self) -> bool {
        self.parser.has_active_exec()
    }

    pub fn append_active_output(&mut self, call_id: &str, chunk: &str) {
        self.parser
            .append_active_output(&mut self.transcript.history, call_id, chunk);
        self.dirty = true;
    }

    pub fn set_active_status(&mut self, call_id: &str, status: ToolStatus) {
        self.parser
            .set_active_status(&mut self.transcript.history, call_id, status);
        self.dirty = true;
    }

    pub fn set_active_user_message(&mut self, call_id: &str, msg: String) {
        self.parser
            .set_active_user_message(&mut self.transcript.history, call_id, msg);
        self.dirty = true;
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
        self.dirty = true;
    }

    pub(crate) fn prompt_input_scroll(&self) -> usize {
        self.prompt.input_scroll
    }

    pub(crate) fn set_prompt_input_scroll(&mut self, scroll: usize) {
        self.prompt.input_scroll = scroll;
    }

    pub(crate) fn set_prompt_viewport(&mut self, vp: Option<super::region::Viewport>) {
        self.prompt.viewport = vp;
    }

    pub(crate) fn set_transcript_viewport(&mut self, vp: Option<super::region::Viewport>) {
        self.last_transcript_viewport = vp;
    }

    pub(crate) fn mark_clean(&mut self) {
        self.dirty = false;
    }

    pub(crate) fn measure_prompt_height_pub(
        &self,
        state: &crate::input::InputState,
        width: usize,
        queued: &[String],
        prediction: Option<&str>,
        has_notification: bool,
    ) -> u16 {
        self.measure_prompt_height(state, width, queued, prediction, has_notification)
    }

    pub(crate) fn transcript_viewport(&self) -> Option<super::region::Viewport> {
        self.last_transcript_viewport
    }

    pub(crate) fn input_viewport(&self) -> Option<super::region::Viewport> {
        self.prompt.viewport
    }

    /// Overwrite the prompt's top-relative input scroll offset. Used by
    /// scrollbar click/drag to jump the input viewport.
    pub(crate) fn set_input_scroll(&mut self, offset: usize) {
        self.prompt.input_scroll = offset;
        self.dirty = true;
    }

    /// Plain-text rendering of the last-painted viewport rows (top to
    /// bottom). Used by the content pane's vim-style motions and yank.
    pub fn viewport_text_rows(&self) -> &[String] {
        &self.last_viewport_text
    }

    /// Plain-text rendering of the full transcript (including any
    /// ephemeral streaming content). Used by the content pane as the
    /// vim buffer so motions span the entire conversation, not just the
    /// current viewport slice.
    pub fn has_transcript_content(&mut self, show_thinking: bool) -> bool {
        !self.transcript.history.is_empty() || self.has_ephemeral(show_thinking)
    }

    pub fn full_transcript_text(&mut self, show_thinking: bool) -> Vec<String> {
        let tw = self.transcript_width() as u16;
        let snap = self.transcript.snapshot(tw, show_thinking);
        let mut rows = snap.rows.clone();
        if self.has_ephemeral(show_thinking) {
            let mut col = SpanCollector::new(tw);
            self.render_ephemeral_into(&mut col, tw as usize, show_thinking);
            for line in col.finish().lines {
                let mut s = String::new();
                for span in &line.spans {
                    s.push_str(&span.text);
                }
                rows.push(s);
            }
        }
        rows
    }

    /// Full transcript display text — every character including gutters
    /// and padding. Cursor motions operate on this buffer; non-selectable
    /// cells are skipped via `snap_to_selectable` after each motion.
    pub fn full_transcript_display_text(&mut self, show_thinking: bool) -> Vec<String> {
        let tw = self.transcript_width() as u16;
        let snap = self.transcript.snapshot(tw, show_thinking);
        let mut rows = snap.rows.clone();
        if self.has_ephemeral(show_thinking) {
            let mut col = SpanCollector::new(tw);
            self.render_ephemeral_into(&mut col, tw as usize, show_thinking);
            for line in col.finish().lines {
                let mut s = String::new();
                for span in &line.spans {
                    s.push_str(&span.text);
                }
                rows.push(s);
            }
        }
        rows
    }

    /// Navigation-only transcript text: selectable display characters
    /// only (gutters, padding stripped). This is the buffer that vim
    /// motions and cursor positioning operate on.
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

    /// Map a nav column to a display column for an absolute row.
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

    /// Map a display column to a nav column for an absolute row.
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

    /// Extract the full selectable text of the block at `abs_row`.
    pub fn block_text_at_row(&mut self, abs_row: usize, show_thinking: bool) -> Option<String> {
        let tw = self.transcript_width() as u16;
        let snap = self.transcript.snapshot(tw, show_thinking);
        snap.block_text_at(abs_row)
    }

    /// Snap a transcript `(row, col)` to the nearest selectable cell.
    /// Returns the adjusted column, or the original if no snap needed.
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

    /// Snap a byte offset in the display-text buffer to the nearest
    /// selectable cell. Returns the (possibly adjusted) byte offset.
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
            // Convert snapped (row, col) back to byte offset.
            let mut offset = 0;
            for (r, line) in rows.iter().enumerate() {
                if r == row {
                    let byte_col: usize =
                        line.chars().take(snapped_col).map(|c| c.len_utf8()).sum();
                    return offset + byte_col;
                }
                offset += line.len() + 1; // +1 for \n
            }
        }
        cpos
    }

    /// Copy text from a display-text byte range, applying `copy_as`
    /// substitutions via the snapshot's `copy_range`.
    pub fn copy_display_range(&mut self, start: usize, end: usize, show_thinking: bool) -> String {
        let tw = self.transcript_width() as u16;
        let snap = self.transcript.snapshot(tw, show_thinking);
        snap.copy_byte_range(start, end)
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty || self.transcript.history.has_unflushed()
    }

    /// Center the input viewport on the cursor (vim `zz`).
    pub fn center_input_scroll(&mut self) {
        // The actual centering happens in draw_prompt_sections using a
        // sentinel value. We set input_scroll to usize::MAX so the
        // scroll logic knows to center instead of preserving position.
        self.prompt.input_scroll = usize::MAX;
        self.dirty = true;
    }

    pub fn finish_turn(&mut self) {
        let _perf = crate::perf::begin("render:finish_turn");
        self.parser
            .finalize_active_tools(&mut self.transcript.history);
        self.mark_blocks_dirty();
    }

    pub fn finalize_active_tools(&mut self) {
        self.parser
            .finalize_active_tools(&mut self.transcript.history);
        self.dirty = true;
    }

    pub fn finalize_active_tools_as(&mut self, status: ToolStatus) {
        self.parser
            .finalize_active_tools_as(&mut self.transcript.history, status);
        self.dirty = true;
    }

    pub fn tool_state(&self, call_id: &str) -> Option<&ToolState> {
        self.transcript.tool_state(call_id)
    }

    pub fn block_view_state(&self, id: BlockId) -> ViewState {
        self.transcript.block_view_state(id)
    }

    pub fn set_block_view_state(&mut self, id: BlockId, state: ViewState) {
        self.transcript.set_block_view_state(id, state);
        self.dirty = true;
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
        self.dirty = true;
    }

    pub fn push_streaming(&mut self, block: Block) -> BlockId {
        let id = self.transcript.push_streaming(block);
        self.dirty = true;
        id
    }

    pub fn streaming_block_ids(&self) -> Vec<BlockId> {
        self.transcript.streaming_block_ids()
    }

    pub fn update_tool_state(
        &mut self,
        call_id: &str,
        mutator: impl FnOnce(&mut ToolState),
    ) -> bool {
        let result = self.transcript.update_tool_state(call_id, mutator);
        if result {
            self.dirty = true;
        }
        result
    }

    pub fn set_tool_state(&mut self, call_id: String, state: ToolState) {
        self.transcript.set_tool_state(call_id, state);
    }

    /// Whether any content (blocks, active tool, active exec) exists above
    pub fn mark_blocks_dirty(&mut self) {
        self.dirty = true;
    }

    /// Force a full repaint on the next tick.
    pub fn redraw(&mut self) {
        let _perf = crate::perf::begin("redraw");
        let (w, _) = self.size();
        if w as usize != self.transcript.history.cache_width {
            self.transcript.history.invalidate_for_width(w as usize);
        }
        self.dirty = true;
    }

    pub fn clear(&mut self) {
        self.transcript.history.clear();
        self.parser.clear();
        self.prompt = PromptState::new();

        let mut frame = Frame::begin(&*self.backend);
        let _ = frame.queue(cursor::MoveTo(0, 0));
        let _ = frame.queue(terminal::Clear(terminal::ClearType::All));
        let _ = frame.queue(terminal::Clear(terminal::ClearType::Purge));
    }

    pub fn has_history(&self) -> bool {
        self.transcript.has_history()
    }

    /// Snapshot the per-tool intermediate representations stored on
    /// committed `Block::ToolCall` blocks. The IR is width-independent and
    /// expensive to rebuild (it contains the LCS diff and syntect tokens),
    /// so we persist it alongside the session and reattach on resume.
    /// Returns `None` if no IR has been built yet.
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

    /// Whether the layout cache has changed since the last
    /// `export_layout_cache`. Used by `save_session` to skip writing the
    /// cache file when nothing would change on disk.
    pub fn layout_cache_dirty(&self) -> bool {
        self.transcript.history.cache_dirty
    }

    /// Export a content-addressed snapshot of every cached block artifact
    /// that is safe to persist. Tool blocks whose `ToolState` is not yet
    /// terminal are skipped — their layout captures transient state.
    pub fn export_layout_cache(&mut self) -> Option<PersistedLayoutCache> {
        if self.transcript.history.is_empty() {
            return None;
        }
        let mut cache = PersistedLayoutCache::new(crate::theme::is_light());
        // Walk `order`, re-keying artifacts by content hash so another
        // session (with different monotonic `BlockId`s) can install the
        // same cache.
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

    /// Install a previously persisted layout cache. Entries for block ids
    /// not currently in history are ignored; missing ids just become cache
    /// misses on the next render. Tool blocks in a non-terminal state
    /// still skip cache adoption so the next render rebuilds their layout.
    pub fn import_layout_cache(&mut self, cache: PersistedLayoutCache) {
        if !cache.is_compatible(crate::theme::is_light()) {
            return;
        }
        let nw = self.size().0;
        // Map cached content hashes onto the first live `BlockId` with
        // matching content. Each hash installs once — duplicates in the
        // current history all reuse the same cached artifact via the
        // shared `content_hash` field in `LayoutKey`.
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
        self.redraw();
    }

    /// Update spinner animation state. Call before rendering. Returns
    /// `true` if the spinner frame changed and the caller should
    /// redraw.
    pub fn update_spinner(&mut self, working: &mut super::working::WorkingState) -> bool {
        let mut changed = false;
        if let Some(elapsed) = working.elapsed() {
            let frame = (elapsed.as_millis() / 150) as usize % SPINNER_FRAMES.len();
            if frame != working.last_spinner_frame {
                working.last_spinner_frame = frame;
                changed = true;
            }
        }
        // Refresh live elapsed on any streaming agent blocks so their
        // duration ticks up without needing an explicit engine event.
        self.parser.tick_active_agents(&mut self.transcript.history);
        changed
    }

    /// Returns true when there is content or prompt work to render.
    pub fn needs_draw(&self, is_dialog: bool, show_thinking: bool) -> bool {
        let has_new_blocks = self.transcript.history.has_unflushed();
        if is_dialog {
            has_new_blocks || (self.has_ephemeral(show_thinking) && self.dirty)
        } else {
            has_new_blocks || self.dirty
        }
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
        let gutters = self.transcript_gutters;
        let tw = (gutters.content_width(width as u16) as usize).max(1);
        let theme = crate::theme::snapshot();

        let ephemeral_lines: Vec<crate::render::display::DisplayLine> =
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

        let geom = super::viewport::ViewportGeom::new(total_rows, viewport_rows, scroll_top);
        let clamped_scroll = geom.clamped_scroll();

        let layer_w = gutters.layer_width(width as u16);
        let scrollbar_col = match gutters.scrollbar {
            Some(crate::window::GutterSide::Left) => 0,
            _ => layer_w.saturating_sub(1),
        };

        // Update viewport text and display lines for vim motions/yank/selection.
        let buf = self.transcript_projection.buf();
        let start = clamped_scroll as usize;
        let end = (start + viewport_rows as usize).min(buf.line_count());
        self.last_viewport_text = buf.get_lines(start, end).to_vec();
        self.last_viewport_lines = self
            .transcript_projection
            .viewport_display_lines(clamped_scroll, viewport_rows);

        self.last_transcript_viewport = Some(super::region::Viewport::new(
            ui::Rect::new(0, 0, tw as u16, viewport_rows),
            tw as u16,
            total_rows,
            clamped_scroll,
            ui::ScrollbarState::new(scrollbar_col, total_rows, viewport_rows),
        ));

        TranscriptData {
            clamped_scroll,
            total_rows,
            scrollbar_col,
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
    ) -> TranscriptCursor {
        let gutters = self.transcript_gutters;
        let tw = (gutters.content_width(width as u16) as usize).max(1);

        if !transcript_owns_cursor || viewport_rows == 0 {
            return TranscriptCursor {
                clamped_line: history_cursor_line,
                clamped_col: history_cursor_col,
                soft_cursor: None,
            };
        }

        let visible = self
            .last_transcript_viewport
            .as_ref()
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
                let byte = crate::text_utils::cell_to_byte(row, col as usize);
                row[byte..].chars().next()
            })
            .and_then(|c| c)
            .unwrap_or(' ');

        TranscriptCursor {
            clamped_line: line,
            clamped_col: history_cursor_col,
            soft_cursor: Some(super::window_view::SoftCursor {
                col,
                row: line,
                glyph: under,
            }),
        }
    }

    /// Whether the animated thinking-summary overlay is active. All
    /// other streams (text, tables, code lines, tools, agents, exec)
    /// flow through streaming blocks in the main paint path; only the
    /// aggregate thinking summary (shown when `show_thinking == false`)
    /// remains as an overlay because it's a synthesized summary, not a
    /// stream.
    fn has_ephemeral(&self, show_thinking: bool) -> bool {
        self.parser.has_active_thinking() && !show_thinking
    }

    /// Paint the animated thinking-summary above the prompt when
    /// thinking is hidden. Every other live element renders as a
    /// streaming block in the main transcript.
    fn render_ephemeral_into<S: LayoutSink>(&self, out: &mut S, width: usize, show_thinking: bool) {
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
            emit_newlines(out, self.thinking_summary_gap());
            render_thinking_summary(out, width, &label, line_count, true);
        }
    }

    /// Flat-line viewport draw path. Paints transcript in the top
    /// Measure prompt height without painting.
    fn measure_prompt_height(
        &self,
        state: &InputState,
        width: usize,
        queued: &[String],
        prediction: Option<&str>,
        has_notification: bool,
    ) -> u16 {
        let usable = width.saturating_sub(2);
        let text_w = usable.saturating_sub(2).max(1);

        // Extra rows: notification + queued + stash + btw.
        let notification: u16 = if has_notification { 1 } else { 0 };
        let stash: u16 = if state.stash.is_some() { 1 } else { 0 };

        let mut queued_rows = 0u16;
        for msg in queued {
            let geom = blocks::UserBlockGeometry::new(msg, text_w);
            for line in &geom.lines {
                let w = super::layout_out::display_width(line);
                queued_rows += if w == 0 { 1 } else { w.div_ceil(text_w) as u16 };
            }
        }

        // Input rows.
        let show_prediction = prediction.is_some() && state.buf.is_empty();
        let input_rows: u16 = if show_prediction {
            1
        } else {
            let (visual_lines, _, _, _) = wrap_and_locate_cursor(&state.buf, &[], 0, usable);
            visual_lines.len() as u16
        };

        notification
            + queued_rows
            + stash
            + 1 // top bar
            + input_rows
            + 1 // bottom bar
            + 1 // status line (always present)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_render_cache_skips_blocks_without_ir() {
        let mut screen = Screen::new();
        screen.push(Block::Thinking {
            content: "alpha\nbeta".into(),
        });
        // Thinking blocks don't carry tool-output IR, so the cache is empty.
        assert!(screen.export_render_cache().is_none());
    }
}
