//! Block history and block-domain types.
//!
//! Holds the content-addressed block store, layout cache, and all
//! mutable sidecar state (tool output, exec output, in-flight agents
//! etc.) that `Screen` tracks. The main rendering loop is in
//! `BlockHistory::render`, called by `Screen::render_pending_blocks`
//! and `Screen::redraw`.

use super::blocks::{gap_between, layout_block, Element};
use super::cache::ToolOutputRenderCache;
use super::context::{LayoutContext, PaintContext};
use super::display::DisplayBlock;
use super::paint::paint_block;
use super::RenderOut;
use crossterm::QueueableCommand;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

/// All data needed to show a confirm dialog. Flows unchanged from
/// `EngineEvent::RequestPermission` through `SessionControl`, `DeferredDialog`,
/// `ConfirmContext`, and `ConfirmDialog::new`.
pub struct ConfirmRequest {
    pub call_id: String,
    pub tool_name: String,
    pub desc: String,
    pub args: std::collections::HashMap<String, serde_json::Value>,
    pub approval_patterns: Vec<String>,
    /// Set during dispatch when paths outside the workspace are detected.
    pub outside_dir: Option<std::path::PathBuf>,
    pub summary: Option<String>,
    pub request_id: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ToolStatus {
    Pending,
    Confirm,
    Ok,
    Err,
    Denied,
}

#[derive(Clone)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    pub metadata: Option<serde_json::Value>,
    pub render_cache: Option<ToolOutputRenderCache>,
}

pub type ToolOutputRef = Box<ToolOutput>;

/// Mutable sidecar for a committed `Block::ToolCall`, keyed by `call_id` on
/// `BlockHistory::tool_states`. Holds every field of a tool block that can
/// change after the block has been pushed (status flip, streamed output,
/// finalized elapsed, etc.). Splitting this out keeps `Block::ToolCall`
/// immutable so its layout can be cached permanently once terminal.
#[derive(Clone)]
pub struct ToolState {
    pub status: ToolStatus,
    pub elapsed: Option<Duration>,
    pub output: Option<ToolOutputRef>,
    pub user_message: Option<String>,
}

impl ToolState {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status,
            ToolStatus::Ok | ToolStatus::Err | ToolStatus::Denied
        )
    }
}

pub struct ActiveExec {
    pub command: String,
    pub output: String,
    pub start_time: Instant,
    pub finished: bool,
    pub exit_code: Option<i32>,
}

/// A blocking agent rendered in the dynamic section (like an active tool).
pub struct ActiveAgent {
    pub agent_id: String,
    pub slug: Option<String>,
    pub tool_calls: Vec<crate::app::AgentToolEntry>,
    pub status: AgentBlockStatus,
    pub start_time: Instant,
    /// Frozen elapsed time once the agent finishes.
    pub final_elapsed: Option<Duration>,
}

pub struct ActiveTool {
    pub call_id: String,
    pub name: String,
    pub summary: String,
    pub args: HashMap<String, serde_json::Value>,
    pub status: ToolStatus,
    pub output: Option<ToolOutputRef>,
    pub user_message: Option<String>,
    pub start_time: Instant,
}

impl ActiveTool {
    pub(super) fn elapsed(&self) -> Option<Duration> {
        if matches!(
            self.name.as_str(),
            "bash" | "web_fetch" | "read_process_output" | "stop_process" | "peek_agent"
        ) {
            Some(self.start_time.elapsed())
        } else {
            None
        }
    }
}

#[derive(Clone)]
pub struct ResumeEntry {
    pub id: String,
    pub title: String,
    pub subtitle: Option<String>,
    pub updated_at_ms: u64,
    pub created_at_ms: u64,
    pub cwd: Option<String>,
    pub parent_id: Option<String>,
    /// Nesting depth for display (0 = root, 1 = fork, etc.)
    pub depth: usize,
    /// Cached text-content size in bytes (None if unknown).
    pub size_bytes: Option<u64>,
}

#[derive(Clone, serde::Serialize)]
pub enum Block {
    User {
        text: String,
        /// Bracketed labels for image attachments (e.g. `[screenshot.png]`).
        /// Used to accent-highlight them in the rendered message.
        image_labels: Vec<String>,
    },
    Thinking {
        content: String,
    },
    Text {
        content: String,
    },
    /// A single line of code from a streaming code block.
    CodeLine {
        content: String,
        lang: String,
    },
    /// Immutable handle to a committed tool call. The mutable result
    /// (status, elapsed, output, user_message) lives in `BlockHistory::tool_states`
    /// keyed by `call_id`; look it up with `Screen::tool_state`.
    ToolCall {
        call_id: String,
        name: String,
        summary: String,
        args: HashMap<String, serde_json::Value>,
    },
    Confirm {
        tool: String,
        desc: String,
        choice: Option<ConfirmChoice>,
    },
    Hint {
        content: String,
    },
    Exec {
        command: String,
        output: String,
    },
    Compacted {
        summary: String,
    },
    AgentMessage {
        from_id: String,
        from_slug: String,
        content: String,
    },
    /// Inline agent block — shows a spawned subagent's progress.
    Agent {
        agent_id: String,
        slug: Option<String>,
        blocking: bool,
        tool_calls: Vec<crate::app::AgentToolEntry>,
        status: AgentBlockStatus,
        elapsed: Option<Duration>,
    },
}

impl Block {
    /// Stable content hash of this block. Two blocks with the same id
    /// produce identical `DisplayBlock`s for the same `LayoutKey` and
    /// `ToolState`. For `ToolCall`, `ToolState` (status / output / elapsed)
    /// is deliberately *not* hashed — mutable tool state lives separately
    /// and is invalidated via `BlockHistory::invalidate_block_layout`.
    ///
    /// Implementation: serialize through `serde_json::Value` first (whose
    /// `Map` is a `BTreeMap` without the `preserve_order` feature) so the
    /// `HashMap<String, Value>` arg fields are emitted in sorted-key
    /// order, then hash the resulting bytes. Without the intermediate
    /// `to_value` step, two blocks with identical content but different
    /// HashMap insertion orders would produce different ids.
    pub fn id(&self) -> BlockId {
        let value = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
        let bytes = serde_json::to_vec(&value).unwrap_or_default();
        BlockId(seahash::hash(&bytes))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum AgentBlockStatus {
    Running,
    Done,
    Error,
}

#[derive(Clone, Copy, PartialEq, serde::Serialize)]
pub enum ApprovalScope {
    Session,
    Workspace,
}

#[derive(Clone, PartialEq, serde::Serialize)]
pub enum ConfirmChoice {
    Yes,
    YesAutoApply,
    No,
    Always(ApprovalScope),
    AlwaysPatterns(Vec<String>, ApprovalScope),
    AlwaysDir(String, ApprovalScope),
}

#[derive(Clone, Copy, PartialEq)]
pub enum Throbber {
    Working,
    Retrying { delay: Duration, attempt: u32 },
    Compacting,
    Done,
    Interrupted,
}

/// Stable content hash of a `Block`. Computed once at construction; two
/// blocks with the same id are guaranteed to lay out identically given the
/// same `LayoutKey` and (for tool blocks) `ToolState`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct BlockId(pub u64);

/// Cache key for a single `DisplayBlock` layout — the inputs to
/// `layout_block` that affect the laid-out output for a given block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct LayoutKey {
    pub width: u16,
    pub show_thinking: bool,
}

/// Per-block cached artifacts. Keeps a bounded LRU of the most recent
/// `LayoutKey → DisplayBlock` pairs so that resize cycles (e.g. 100→80→100)
/// can hit the cache on every step.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct BlockArtifact {
    /// `(LayoutKey, DisplayBlock)` entries ordered most-recently-used first.
    pub layouts: Vec<(LayoutKey, DisplayBlock)>,
}

impl BlockArtifact {
    pub(super) const MAX_LAYOUTS: usize = 4;

    pub fn get(&self, key: LayoutKey) -> Option<&DisplayBlock> {
        self.layouts.iter().find(|(k, _)| *k == key).map(|(_, b)| b)
    }

    pub fn insert(&mut self, key: LayoutKey, block: DisplayBlock) {
        self.layouts.retain(|(k, _)| *k != key);
        self.layouts.insert(0, (key, block));
        self.layouts.truncate(Self::MAX_LAYOUTS);
    }

    pub fn clear(&mut self) {
        self.layouts.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.layouts.is_empty()
    }

    /// Drop cached layouts whose replay would be stale at `new_width`.
    /// Layouts whose `layout_width` equals `new_width` or whose source
    /// didn't wrap and still fits are preserved.
    pub fn invalidate_for_width(&mut self, new_width: u16) {
        self.layouts
            .retain(|(k, b)| k.width == new_width || b.is_valid_at(new_width));
    }
}

pub(super) struct BlockHistory {
    /// Append-only sequence of `BlockId`s. The "ith block" is
    /// `blocks[&order[i]]`. Duplicate ids are allowed: two identical blocks
    /// at different positions resolve to the same entry in `blocks` /
    /// `artifacts`.
    pub(super) order: Vec<BlockId>,
    /// Content-addressed block store.
    pub(super) blocks: HashMap<BlockId, Block>,
    /// Content-addressed per-block layout cache.
    pub(super) artifacts: HashMap<BlockId, BlockArtifact>,
    /// Mutable sidecar state for `Block::ToolCall` entries, keyed by `call_id`.
    pub(super) tool_states: HashMap<String, ToolState>,
    /// Terminal width when artifacts were last width-pruned.
    pub(super) cache_width: usize,
    /// True iff the layout cache has changed since the last persisted save.
    /// When false, `save_session` skips writing the layout cache file.
    pub(super) cache_dirty: bool,
    pub(super) flushed: usize,
    pub(super) last_block_rows: u16,
    /// When true, the leading gap of the next unflushed block is suppressed.
    /// Set after a dialog dismiss where ScrollUp pushed the gap into scrollback.
    pub(super) suppress_leading_gap: bool,
    /// Rows to skip from the top of the next-rendered first block.
    /// Set by `redraw` when a single block exceeds the redraw budget;
    /// consumed by the next `render` call and reset to 0 afterwards.
    pub(super) pending_head_skip: u16,
}

impl BlockHistory {
    pub(super) fn new() -> Self {
        Self {
            order: Vec::new(),
            blocks: HashMap::new(),
            artifacts: HashMap::new(),
            tool_states: HashMap::new(),
            cache_width: 0,
            cache_dirty: false,
            flushed: 0,
            last_block_rows: 0,
            suppress_leading_gap: false,
            pending_head_skip: 0,
        }
    }

    pub(super) fn len(&self) -> usize {
        self.order.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    pub(super) fn block_at(&self, i: usize) -> &Block {
        &self.blocks[&self.order[i]]
    }

    pub(super) fn last_block(&self) -> Option<&Block> {
        self.order.last().and_then(|id| self.blocks.get(id))
    }

    /// Append `block` and return its `BlockId`. Duplicate content merges
    /// into the existing entry in `blocks`/`artifacts`, so two identical
    /// blocks share their cached layouts.
    pub(super) fn push(&mut self, block: Block) -> BlockId {
        let id = block.id();
        self.order.push(id);
        self.blocks.entry(id).or_insert(block);
        self.artifacts.entry(id).or_default();
        self.cache_dirty = true;
        id
    }

    /// Push a `Block::ToolCall` alongside its initial `ToolState`.
    pub(super) fn push_with_state(
        &mut self,
        block: Block,
        call_id: String,
        state: ToolState,
    ) -> BlockId {
        self.tool_states.insert(call_id, state);
        self.push(block)
    }

    /// `BlockId` of the most recent `Block::ToolCall` whose `call_id` matches.
    pub(super) fn tool_block_id(&self, call_id: &str) -> Option<BlockId> {
        self.order.iter().rev().copied().find(|id| {
            matches!(
                self.blocks.get(id),
                Some(Block::ToolCall { call_id: c, .. }) if c == call_id
            )
        })
    }

    /// Drop every cached layout for a single block id.
    pub(super) fn invalidate_block_layout(&mut self, id: BlockId) {
        if let Some(artifact) = self.artifacts.get_mut(&id) {
            if !artifact.is_empty() {
                artifact.clear();
                self.cache_dirty = true;
            }
        }
    }

    pub(super) fn has_unflushed(&self) -> bool {
        self.flushed < self.order.len()
    }

    pub(super) fn clear(&mut self) {
        self.order.clear();
        self.blocks.clear();
        self.artifacts.clear();
        self.tool_states.clear();
        self.flushed = 0;
        self.last_block_rows = 0;
        self.cache_dirty = true;
    }

    /// Width-aware invalidation: prune cached layouts that are no longer
    /// replayable at `new_width`. The bounded LRU in each artifact means
    /// layouts from previous widths survive and can be reused after a
    /// resize cycle.
    pub(super) fn invalidate_for_width(&mut self, new_width: usize) {
        let _perf = crate::perf::begin("history:invalidate_for_width");
        let nw = new_width as u16;
        let mut dirty = false;
        for artifact in self.artifacts.values_mut() {
            let before = artifact.layouts.len();
            artifact.invalidate_for_width(nw);
            if artifact.layouts.len() != before {
                dirty = true;
            }
        }
        self.cache_width = new_width;
        if dirty {
            self.cache_dirty = true;
        }
    }

    /// Gap (in rows) before the block at `i`, based on adjacency rules.
    pub(super) fn block_gap(&self, i: usize) -> u16 {
        if i > 0 {
            gap_between(
                &Element::Block(self.block_at(i - 1)),
                &Element::Block(self.block_at(i)),
            )
        } else {
            0
        }
    }

    /// Rows the block at `i` would occupy under `key`. Lays the block out
    /// if no cached layout exists, so that the caller's subsequent render
    /// pass gets a cache hit.
    pub(super) fn ensure_rows(&mut self, i: usize, key: LayoutKey) -> u16 {
        let id = self.order[i];
        if let Some(rows) = self
            .artifacts
            .get(&id)
            .and_then(|a| a.get(key))
            .map(|d| d.rows())
        {
            return rows;
        }
        let block = &self.blocks[&id];
        let tool_state = if let Block::ToolCall { call_id, .. } = block {
            self.tool_states.get(call_id)
        } else {
            None
        };
        let lctx = LayoutContext {
            width: key.width,
            show_thinking: key.show_thinking,
        };
        let display = layout_block(block, tool_state, &lctx);
        let rows = display.rows();
        let artifact = self.artifacts.get_mut(&id).unwrap();
        artifact.insert(key, display);
        self.cache_dirty = true;
        rows
    }

    pub(super) fn truncate(&mut self, idx: usize) {
        if idx >= self.order.len() {
            return;
        }
        let removed: Vec<BlockId> = self.order.drain(idx..).collect();
        let live: HashSet<BlockId> = self.order.iter().copied().collect();
        for id in removed {
            if !live.contains(&id) {
                self.blocks.remove(&id);
                self.artifacts.remove(&id);
            }
        }
        self.flushed = self.flushed.min(self.order.len());
        self.cache_dirty = true;
        self.gc_tool_states();
    }

    /// Drop tool states whose owning `Block::ToolCall` no longer appears in
    /// `order`.
    pub(super) fn gc_tool_states(&mut self) {
        let live: HashSet<String> = self
            .order
            .iter()
            .filter_map(|id| self.blocks.get(id))
            .filter_map(|b| {
                if let Block::ToolCall { call_id, .. } = b {
                    Some(call_id.clone())
                } else {
                    None
                }
            })
            .collect();
        self.tool_states.retain(|cid, _| live.contains(cid));
    }

    /// Render unflushed blocks. Returns total rows printed.
    pub(super) fn render(&mut self, out: &mut RenderOut, width: usize, show_thinking: bool) -> u16 {
        if !self.has_unflushed() {
            return 0;
        }
        let _perf = crate::perf::begin("history:render");
        let use_cache = out.row.is_none();

        if use_cache && width != self.cache_width {
            self.invalidate_for_width(width);
        }

        let theme = crate::theme::snapshot();
        let pctx = PaintContext {
            theme: &theme,
            term_width: width as u16,
        };
        let key = LayoutKey {
            width: width as u16,
            show_thinking,
        };

        let mut total = 0u16;
        let last_idx = self.order.len().saturating_sub(1);
        let mut first = true;
        // Consume any pending head-skip set by the redraw path. Skip
        // the leading gap on the first block too, since we're starting
        // mid-block visually.
        let head_skip = std::mem::take(&mut self.pending_head_skip);
        for i in self.flushed..self.order.len() {
            let head_skip_block = if first { head_skip } else { 0 };
            let gap = if first && (self.suppress_leading_gap || head_skip > 0) {
                0
            } else {
                self.block_gap(i)
            };
            first = false;
            for _ in 0..gap {
                out.scroll_newline();
            }

            let id = self.order[i];
            let block = &self.blocks[&id];
            let tool_state = if let Block::ToolCall { call_id, .. } = block {
                self.tool_states.get(call_id)
            } else {
                None
            };

            let rows = if use_cache {
                if let Some(cached) = self.artifacts.get(&id).and_then(|a| a.get(key)) {
                    let _p = crate::perf::begin("history:cache_hit");
                    paint_block(out, cached, &pctx, head_skip_block as usize);
                    cached.rows().saturating_sub(head_skip_block)
                } else {
                    let _p = crate::perf::begin("history:cache_miss");
                    let lctx = LayoutContext {
                        width: width as u16,
                        show_thinking,
                    };
                    let display = layout_block(block, tool_state, &lctx);
                    paint_block(out, &display, &pctx, head_skip_block as usize);
                    let rows = display.rows().saturating_sub(head_skip_block);
                    let artifact = self.artifacts.get_mut(&id).unwrap();
                    artifact.insert(key, display);
                    self.cache_dirty = true;
                    rows
                }
            } else {
                let _p = crate::perf::begin("history:overlay_render");
                let lctx = LayoutContext {
                    width: width as u16,
                    show_thinking,
                };
                let display = layout_block(block, tool_state, &lctx);
                paint_block(out, &display, &pctx, head_skip_block as usize);
                display.rows().saturating_sub(head_skip_block)
            };

            total += gap + rows;
            if i == last_idx {
                self.last_block_rows = rows + gap;
            }
        }
        self.suppress_leading_gap = false;
        self.flushed = self.order.len();
        total
    }

    /// Flat-line viewport painter. Lays out every block, then paints the
    /// slice of rendered rows that fits the viewport, shifted upward by
    /// `scroll_offset` (0 = stuck to bottom).
    ///
    /// Overlay-only: each row is placed via `MoveTo`. Callers own clearing
    /// the viewport region prior to calling (so rows left blank by a short
    /// transcript stay cleared).
    ///
    /// Returns the clamped scroll offset (for the caller to sync state).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn paint_viewport(
        &mut self,
        out: &mut RenderOut,
        width: usize,
        show_thinking: bool,
        top_row: u16,
        viewport_rows: u16,
        scroll_offset: u16,
        extra_lines: &[super::display::DisplayLine],
    ) -> u16 {
        let _perf = crate::perf::begin("history:paint_viewport");
        if viewport_rows == 0 || (self.order.is_empty() && extra_lines.is_empty()) {
            return 0;
        }
        if width != self.cache_width {
            self.invalidate_for_width(width);
            self.cache_width = width;
        }
        let key = LayoutKey {
            width: width as u16,
            show_thinking,
        };
        let mut per_block: Vec<(u16, u16)> = Vec::with_capacity(self.order.len());
        let mut total: u32 = 0;
        for i in 0..self.order.len() {
            let gap = self.block_gap(i);
            let rows = self.ensure_rows(i, key);
            total += gap as u32 + rows as u32;
            per_block.push((gap, rows));
        }
        total += extra_lines.len() as u32;
        let total = total.min(u16::MAX as u32) as u16;

        // Clamp scroll.
        let max_scroll = total.saturating_sub(viewport_rows);
        let scroll = scroll_offset.min(max_scroll);
        // Lines to skip from the top of the flat transcript.
        let skip = total.saturating_sub(viewport_rows).saturating_sub(scroll);

        let theme = crate::theme::snapshot();
        let pctx = super::context::PaintContext {
            theme: &theme,
            term_width: width as u16,
        };

        out.row = Some(top_row);
        out.move_to(0, top_row);

        let mut remaining_skip = skip as u32;
        let mut painted: u16 = 0;

        'blocks: for (i, (gap, _rows)) in per_block.iter().enumerate() {
            // Gap lines (blank rows).
            for _ in 0..*gap {
                if remaining_skip > 0 {
                    remaining_skip -= 1;
                    continue;
                }
                if painted >= viewport_rows {
                    break 'blocks;
                }
                // Blank row: clear to EOL then advance.
                let _ = out.queue(crossterm::terminal::Clear(
                    crossterm::terminal::ClearType::CurrentLine,
                ));
                out.overlay_newline();
                painted += 1;
            }
            let id = self.order[i];
            let display = self.artifacts.get(&id).and_then(|a| a.get(key));
            let Some(display) = display else { continue };
            for line in &display.lines {
                if remaining_skip > 0 {
                    remaining_skip -= 1;
                    continue;
                }
                if painted >= viewport_rows {
                    break 'blocks;
                }
                super::paint::paint_line(out, line, &pctx);
                painted += 1;
            }
        }

        // Paint ephemeral tail after committed blocks.
        for line in extra_lines {
            if remaining_skip > 0 {
                remaining_skip -= 1;
                continue;
            }
            if painted >= viewport_rows {
                break;
            }
            super::paint::paint_line(out, line, &pctx);
            painted += 1;
        }

        scroll
    }
}

/// Streaming state for incremental thinking output.
/// Completed lines are committed to block history immediately.
/// Only the current incomplete line lives in the overlay.
pub(super) struct ActiveThinking {
    pub(super) current_line: String,
    pub(super) paragraph: String,
}

/// Streaming state for incremental LLM text output.
/// Completed lines are committed to block history immediately.
/// Only the current incomplete line lives in the overlay.
pub(super) struct ActiveText {
    pub(super) current_line: String,
    pub(super) paragraph: String,
    pub(super) in_code_block: Option<String>,
    /// Table rows accumulated silently during streaming.
    pub(super) table_rows: Vec<String>,
    /// Cached count of non-separator data rows (avoids recomputing per frame).
    pub(super) table_data_rows: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_thinking_keeps_gap_above_summary() {
        let mut history = BlockHistory::new();
        history.push(Block::Text {
            content: "hello".into(),
        });
        history.push(Block::Thinking {
            content: "alpha\nbeta".into(),
        });

        let mut out = RenderOut::buffer();
        history.render(&mut out, 80, false);
        let rendered = String::from_utf8(out.into_bytes()).unwrap();
        assert!(rendered.contains("hello"));
        assert!(rendered.contains("thinking (2 lines)"));
        // Gap row is either "\r\n\r\n" (new form, since scroll-mode
        // `scroll_newline` drops the redundant Clear::UntilNewLine) or
        // "\r\n\x1b[K\r\n" (old form), depending on history path.
        assert!(
            rendered.contains("\r\n\r\n") || rendered.contains("\r\n\u{1b}[K\r\n"),
            "rendered: {rendered:?}"
        );
    }

    #[test]
    fn block_artifact_bounded_lru_roundtrip() {
        // Resize cycle 100 → 80 → 100 → 80 must hit cache on every repeat
        // step, because the bounded LRU keeps both widths resident.
        let mut history = BlockHistory::new();
        let id = history.push(Block::Text {
            content: "the quick brown fox jumps over the lazy dog".into(),
        });

        let mut sink = RenderOut::buffer();
        history.render(&mut sink, 100, true);
        history.flushed = 0;
        history.render(&mut sink, 80, true);
        history.flushed = 0;
        history.render(&mut sink, 100, true);
        history.flushed = 0;
        history.render(&mut sink, 80, true);

        let keys: Vec<LayoutKey> = history
            .artifacts
            .get(&id)
            .unwrap()
            .layouts
            .iter()
            .map(|(k, _)| *k)
            .collect();
        let k100 = LayoutKey {
            width: 100,
            show_thinking: true,
        };
        let k80 = LayoutKey {
            width: 80,
            show_thinking: true,
        };
        assert!(keys.contains(&k100), "expected width=100 cached: {keys:?}");
        assert!(keys.contains(&k80), "expected width=80 cached: {keys:?}");
        assert!(keys.len() <= BlockArtifact::MAX_LAYOUTS);
    }

    #[test]
    fn duplicate_block_ids_share_artifact() {
        // Two identical blocks at different positions should resolve to the
        // same `BlockId` and share a single entry in `blocks` / `artifacts`.
        let mut history = BlockHistory::new();
        let a = history.push(Block::Text {
            content: "same".into(),
        });
        let b = history.push(Block::Text {
            content: "same".into(),
        });
        assert_eq!(a, b);
        assert_eq!(history.order.len(), 2);
        assert_eq!(history.blocks.len(), 1);
        assert_eq!(history.artifacts.len(), 1);
    }
}
