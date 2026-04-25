//! Transcript domain model.
//!
//! The content-addressed block store, layout cache, and all mutable
//! sidecar state (tool output, exec output, in-flight agents etc.)
//! owned by `App`. Held inside `app::transcript::Transcript`, which
//! adds projection / streaming / paint orchestration on top.

use crate::app::transcript_cache::ToolOutputRenderCache;
use crate::app::transcript_present::{gap_between, layout_block, Element};
use crate::content::{DisplayBlock, LayoutContext};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

/// In-flight blocking agent — a thin handle to a streaming `Block::Agent`.
/// The full state (slug, tool_calls, status, elapsed) lives in the block
/// itself and is refreshed via `rewrite` as engine events arrive.
pub struct ActiveAgent {
    pub agent_id: String,
    pub block_id: BlockId,
    pub start_time: Instant,
    /// Frozen elapsed time once the agent finishes; while `None`, live
    /// elapsed ticks are rewritten into the block on each spinner frame.
    pub final_elapsed: Option<Duration>,
}

/// In-flight tool call — a thin handle to a streaming `Block::ToolCall`.
/// The full state (status, output, user_message, elapsed) lives in
/// `tool_states` keyed by `call_id`; rewrites go through
/// `App::update_tool_state` which invalidates the layout cache.
pub struct ActiveTool {
    pub call_id: String,
    pub name: String,
    pub block_id: BlockId,
    pub start_time: Instant,
}

impl ActiveTool {
    pub(crate) fn elapsed(&self) -> Option<Duration> {
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

/// All data needed to show a confirm dialog. Flows unchanged from
/// `EngineEvent::RequestPermission` through `SessionControl`, `DeferredDialog`,
/// `ConfirmContext`, and `ConfirmDialog::new`.
#[derive(Clone)]
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
    /// keyed by `call_id`; look it up with `App::tool_state`.
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
        tool_calls: Vec<super::AgentToolEntry>,
        status: AgentBlockStatus,
        elapsed: Option<Duration>,
    },
}

impl Block {
    /// Stable content hash of this block. Two blocks with the same
    /// content hash produce identical `DisplayBlock`s for the same
    /// `LayoutKey` and `ToolState`. For `ToolCall`, `ToolState` (status
    /// / output / elapsed) is deliberately *not* hashed — mutable tool
    /// state lives separately and is invalidated via
    /// `BlockHistory::invalidate_block_layout`.
    ///
    /// Implementation: serialize through `serde_json::Value` first
    /// (whose `Map` is a `BTreeMap` without the `preserve_order`
    /// feature) so the `HashMap<String, Value>` arg fields are emitted
    /// in sorted-key order, then hash the resulting bytes. Without the
    /// intermediate `to_value` step, two blocks with identical content
    /// but different HashMap insertion orders would produce different
    /// hashes.
    pub fn content_hash(&self) -> u64 {
        let value = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
        let bytes = serde_json::to_vec(&value).unwrap_or_default();
        seahash::hash(&bytes)
    }

    /// Raw source text for the block, before markdown rendering. Used
    /// by whole-block yank so copying a rendered markdown block returns
    /// the original `**bold**`, `` `code` ``, fenced ```` ``` ```` blocks,
    /// `|` tables, `---` rules, etc. — instead of walking display cells
    /// (which strips inline markup).
    ///
    /// Returns `None` for structured blocks (tool calls, agent headers,
    /// confirm dialogs) that don't have a single "markdown source"; the
    /// caller falls back to cell-walking for those.
    pub fn raw_text(&self) -> Option<String> {
        match self {
            Block::User { text, .. } => Some(text.clone()),
            Block::Text { content }
            | Block::Thinking { content }
            | Block::Hint { content }
            | Block::AgentMessage { content, .. } => Some(content.clone()),
            Block::Compacted { summary } => Some(summary.clone()),
            Block::CodeLine { content, .. } => Some(content.clone()),
            Block::Exec { command, output } => Some(format!("$ {command}\n{output}")),
            Block::ToolCall { .. } | Block::Confirm { .. } | Block::Agent { .. } => None,
        }
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

/// A single runtime permission rule: one tool + one pattern.
#[derive(Clone)]
pub struct PermissionEntry {
    pub tool: String,
    pub pattern: String,
}

#[derive(Clone, PartialEq, serde::Serialize)]
pub enum ConfirmChoice {
    Yes,
    No,
    Always(ApprovalScope),
    AlwaysPatterns(Vec<String>, ApprovalScope),
    AlwaysDir(String, ApprovalScope),
}

/// Stable, monotonic per-session handle to a block. Independent of
/// block content: mutating a block in place keeps the same `BlockId`.
/// Layout cache invalidation on content change is handled via
/// [`LayoutKey::content_hash`], not by identity.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct BlockId(pub u64);

/// Per-block view state — how the block is presented inside the
/// transcript. Independent of the block's [`Status`] (a still-streaming
/// block can be `Collapsed`; a finished block can be `TrimmedHead`).
///
/// The layout cache keys on this so flipping view states invalidates
/// only the affected block, not the whole cache.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
pub enum ViewState {
    /// Full content — default.
    #[default]
    Expanded,
    /// One summary line only.
    Collapsed,
    /// Show the first `keep` rows of the block's content, elide the rest.
    TrimmedHead { keep: u16 },
    /// Show the last `keep` rows of the block's content, elide the rest.
    TrimmedTail { keep: u16 },
}

/// Lifecycle state of a block. Orthogonal to [`ViewState`]: a block
/// can be `Streaming` + `Collapsed`, `Done` + `Expanded`, etc.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
pub enum Status {
    /// Block content is still being produced (active stream, running
    /// tool). The layout cache should expect invalidation on every
    /// chunk; the renderer may apply a "live" style.
    Streaming,
    /// Block is final. Cached layouts remain valid until width changes.
    #[default]
    Done,
}

/// Cache key for a single `DisplayBlock` layout — the inputs to
/// `layout_block` that affect the laid-out output for a given block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct LayoutKey {
    pub width: u16,
    pub show_thinking: bool,
    pub view_state: ViewState,
    /// Content hash of the block this layout was produced for. When a
    /// block mutates (streaming append / rewrite), its content hash
    /// changes and the new `LayoutKey` misses the old cached layout
    /// — automatic invalidation by keying, not by eviction.
    pub content_hash: u64,
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
    pub(crate) const MAX_LAYOUTS: usize = 4;

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

pub(crate) struct BlockHistory {
    /// Append-only sequence of `BlockId`s. Each entry is a unique
    /// monotonic handle; positions are 1:1 with block instances.
    pub(crate) order: Vec<BlockId>,
    /// Per-instance block store.
    pub(crate) blocks: HashMap<BlockId, Block>,
    /// Cached content hash per `BlockId`. Populated on push / mutation
    /// so layout-key construction and persisted-cache re-keying can
    /// skip re-hashing the block bytes.
    pub(crate) content_hashes: HashMap<BlockId, u64>,
    /// Per-block layout cache, keyed by the monotonic `BlockId`. Cache
    /// invalidation on content change is handled via
    /// `LayoutKey::content_hash` + the bounded LRU in `BlockArtifact`.
    pub(crate) artifacts: HashMap<BlockId, BlockArtifact>,
    /// Monotonic counter driving fresh `BlockId`s on push.
    pub(crate) next_id: u64,
    /// Mutable sidecar state for `Block::ToolCall` entries, keyed by `call_id`.
    pub(crate) tool_states: HashMap<String, ToolState>,
    /// Per-block view state (collapsed / trimmed / expanded). Absent
    /// entries default to `ViewState::Expanded`. Mutating this map
    /// invalidates that block's layout cache — `LayoutKey` includes
    /// `view_state`.
    pub(crate) view_states: HashMap<BlockId, ViewState>,
    /// Per-block lifecycle state (streaming vs done). Absent entries
    /// default to `Status::Done`. Streaming blocks signal to callers
    /// that layout may change on the next frame.
    pub(crate) statuses: HashMap<BlockId, Status>,
    /// Terminal width when artifacts were last width-pruned.
    pub(crate) cache_width: usize,
    /// True iff the layout cache has changed since the last persisted save.
    /// When false, `save_session` skips writing the layout cache file.
    pub(crate) cache_dirty: bool,
    /// Block ids that transitioned from `Streaming` to `Done` since the
    /// last drain. Drained by the app loop to emit `block_done`
    /// autocmds into the Lua runtime.
    pub(crate) finished_blocks: Vec<BlockId>,
    /// Monotonic generation counter — bumped on every content mutation
    /// (push, rewrite, status change, view state change, truncate,
    /// clear). Used by `TranscriptSnapshot` to detect staleness.
    generation: u64,
}

impl BlockHistory {
    pub(crate) fn new() -> Self {
        Self {
            order: Vec::new(),
            blocks: HashMap::new(),
            content_hashes: HashMap::new(),
            artifacts: HashMap::new(),
            next_id: 0,
            tool_states: HashMap::new(),
            view_states: HashMap::new(),
            statuses: HashMap::new(),
            cache_width: 0,
            cache_dirty: false,
            finished_blocks: Vec::new(),
            generation: 0,
        }
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    fn bump_generation(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    /// Drain block ids that transitioned `Streaming` → `Done` since the
    /// last call.
    pub(crate) fn drain_finished_blocks(&mut self) -> Vec<BlockId> {
        std::mem::take(&mut self.finished_blocks)
    }

    /// Cached content hash for `id`. Falls back to re-hashing if the
    /// cache entry is missing (shouldn't happen in steady state).
    pub(crate) fn content_hash(&self, id: BlockId) -> u64 {
        if let Some(h) = self.content_hashes.get(&id) {
            return *h;
        }
        self.blocks.get(&id).map(|b| b.content_hash()).unwrap_or(0)
    }

    pub(crate) fn len(&self) -> usize {
        self.order.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    pub(crate) fn block_at(&self, i: usize) -> &Block {
        &self.blocks[&self.order[i]]
    }

    /// Current view state for `id`. Defaults to [`ViewState::Expanded`]
    /// when no explicit state has been set.
    pub(crate) fn view_state(&self, id: BlockId) -> ViewState {
        self.view_states.get(&id).copied().unwrap_or_default()
    }

    /// Set the view state for `id`. Invalidates cached layouts for
    /// that block so the next paint re-lays-out under the new state.
    pub(crate) fn set_view_state(&mut self, id: BlockId, state: ViewState) {
        let prev = self.view_states.get(&id).copied().unwrap_or_default();
        if prev == state {
            return;
        }
        if matches!(state, ViewState::Expanded) {
            self.view_states.remove(&id);
        } else {
            self.view_states.insert(id, state);
        }
        if let Some(art) = self.artifacts.get_mut(&id) {
            art.clear();
        }
        self.cache_dirty = true;
        self.bump_generation();
    }

    /// Current status for `id`. Defaults to [`Status::Done`].
    pub(crate) fn status(&self, id: BlockId) -> Status {
        self.statuses.get(&id).copied().unwrap_or_default()
    }

    /// Set the status for `id`. Does not invalidate the layout cache —
    /// status is a style concern, not a layout one.
    pub(crate) fn set_status(&mut self, id: BlockId, status: Status) {
        let was_streaming = matches!(
            self.statuses.get(&id).copied().unwrap_or_default(),
            Status::Streaming
        );
        if matches!(status, Status::Done) {
            self.statuses.remove(&id);
            if was_streaming {
                self.finished_blocks.push(id);
            }
        } else {
            self.statuses.insert(id, status);
        }
        self.bump_generation();
    }

    /// Append `block` and return a fresh monotonic `BlockId`. Each
    /// push produces a unique id; identical content at two positions
    /// gets distinct ids and distinct cache slots. Cross-session cache
    /// sharing of identical blocks is preserved at the persistence
    /// boundary (see [`Self::export_layouts_by_hash`]).
    pub(crate) fn push(&mut self, block: Block) -> BlockId {
        let hash = block.content_hash();
        let id = BlockId(self.next_id);
        self.next_id += 1;
        self.order.push(id);
        self.blocks.insert(id, block);
        self.content_hashes.insert(id, hash);
        self.artifacts.entry(id).or_default();
        self.cache_dirty = true;
        self.bump_generation();
        id
    }

    /// Push a `Block::ToolCall` alongside its initial `ToolState`.
    pub(crate) fn push_with_state(
        &mut self,
        block: Block,
        call_id: String,
        state: ToolState,
    ) -> BlockId {
        self.tool_states.insert(call_id, state);
        self.push(block)
    }

    /// Replace the content of an existing block in place. Preserves
    /// `BlockId`, `Status`, and `ViewState`; updates the cached content
    /// hash so the next layout-cache lookup misses stale entries
    /// automatically via `LayoutKey::content_hash`.
    ///
    /// The canonical path for streaming updates: the live streamer
    /// holds a `BlockId` from `push`, then calls `rewrite` as each
    /// chunk arrives. No-ops when the block doesn't exist (e.g. it
    /// was truncated by a rewind while a stream was in flight).
    pub(crate) fn rewrite(&mut self, id: BlockId, block: Block) {
        if !self.blocks.contains_key(&id) {
            return;
        }
        let hash = block.content_hash();
        if self.content_hashes.get(&id) == Some(&hash) {
            // Same content — nothing to do, cache stays warm.
            self.blocks.insert(id, block);
            return;
        }
        self.blocks.insert(id, block);
        self.content_hashes.insert(id, hash);
        self.cache_dirty = true;
        self.bump_generation();
    }

    /// Iterator over `BlockId`s currently in the `Streaming` state, in
    /// insertion order. Callers use this to find "the live block" for
    /// an in-flight stream without tracking separate handles.
    pub(crate) fn streaming_block_ids(&self) -> impl Iterator<Item = BlockId> + '_ {
        self.order
            .iter()
            .copied()
            .filter(|id| matches!(self.status(*id), Status::Streaming))
    }

    /// `BlockId` of the most recent `Block::ToolCall` whose `call_id` matches.
    pub(crate) fn tool_block_id(&self, call_id: &str) -> Option<BlockId> {
        self.order.iter().rev().copied().find(|id| {
            matches!(
                self.blocks.get(id),
                Some(Block::ToolCall { call_id: c, .. }) if c == call_id
            )
        })
    }

    /// Drop every cached layout for a single block id.
    pub(crate) fn invalidate_block_layout(&mut self, id: BlockId) {
        if let Some(artifact) = self.artifacts.get_mut(&id) {
            if !artifact.is_empty() {
                artifact.clear();
                self.cache_dirty = true;
                self.bump_generation();
            }
        }
    }

    pub(crate) fn clear(&mut self) {
        self.order.clear();
        self.blocks.clear();
        self.content_hashes.clear();
        self.artifacts.clear();
        self.next_id = 0;
        self.tool_states.clear();
        self.view_states.clear();
        self.statuses.clear();
        self.cache_dirty = true;
        self.bump_generation();
    }

    /// Width-aware invalidation: prune cached layouts that are no longer
    /// replayable at `new_width`. The bounded LRU in each artifact means
    /// layouts from previous widths survive and can be reused after a
    /// resize cycle.
    pub(crate) fn invalidate_for_width(&mut self, new_width: usize) {
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
    /// Streaming blocks participate in the main paint path like any other
    /// block — alt-buffer repaints every frame, so "live" vs "committed"
    /// is a style distinction, not a layout one.
    pub(crate) fn block_gap(&self, i: usize) -> u16 {
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
    /// Fill in per-block view state on a base layout key. Callers build
    /// `(width, show_thinking, view_state=Expanded)` without needing to
    /// know each block's individual view state; this substitutes the
    /// actual per-block value so the cache lookup + layout pass agree.
    pub(crate) fn resolve_key(&self, id: BlockId, base: LayoutKey) -> LayoutKey {
        LayoutKey {
            view_state: self.view_state(id),
            content_hash: self.content_hash(id),
            ..base
        }
    }

    pub(crate) fn ensure_rows(&mut self, i: usize, base: LayoutKey) -> u16 {
        let id = self.order[i];
        // While streaming with thinking hidden, the ephemeral overlay
        // renders the combined animated summary. Suppress the committed
        // thinking block so it doesn't appear as a second summary.
        if matches!(self.blocks.get(&id), Some(Block::Thinking { .. }))
            && !base.show_thinking
            && matches!(self.status(id), Status::Streaming)
        {
            return 0;
        }
        let key = self.resolve_key(id, base);
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
            view_state: key.view_state,
        };
        let display = layout_block(block, tool_state, &lctx);
        let rows = display.rows();
        let artifact = self.artifacts.get_mut(&id).unwrap();
        artifact.insert(key, display);
        self.cache_dirty = true;
        rows
    }

    pub(crate) fn truncate(&mut self, idx: usize) {
        if idx >= self.order.len() {
            return;
        }
        let removed: Vec<BlockId> = self.order.drain(idx..).collect();
        for id in removed {
            self.blocks.remove(&id);
            self.content_hashes.remove(&id);
            self.artifacts.remove(&id);
            self.view_states.remove(&id);
            self.statuses.remove(&id);
        }
        self.cache_dirty = true;
        self.bump_generation();
        self.gc_tool_states();
    }

    /// Drop tool states whose owning `Block::ToolCall` no longer appears in
    /// `order`.
    pub(crate) fn gc_tool_states(&mut self) {
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

    /// Plain-text rendering of the full transcript at the given width.
    #[cfg(test)]
    pub(crate) fn total_rows(&mut self, width: usize, show_thinking: bool) -> u16 {
        let key = LayoutKey {
            view_state: ViewState::Expanded,
            width: width as u16,
            show_thinking,
            content_hash: 0,
        };
        let mut total: u32 = 0;
        for i in 0..self.order.len() {
            total += self.block_gap(i) as u32;
            total += self.ensure_rows(i, key) as u32;
        }
        total.min(u16::MAX as u32) as u16
    }
}

/// Streaming state for incremental thinking output.
/// Completed lines are committed to block history immediately.
/// Only the current incomplete line lives in the overlay.
pub(crate) struct ActiveThinking {
    pub(crate) current_line: String,
    pub(crate) paragraph: String,
    pub(crate) streaming_id: Option<BlockId>,
}

/// Streaming state for incremental LLM text output.
/// Completed lines are committed to block history immediately.
/// Only the current incomplete line lives in the overlay.
pub(crate) struct ActiveText {
    pub(crate) current_line: String,
    pub(crate) paragraph: String,
    pub(crate) in_code_block: Option<String>,
    /// Table rows accumulated silently during streaming.
    pub(crate) table_rows: Vec<String>,
    /// Cached count of non-separator data rows (avoids recomputing per frame).
    pub(crate) table_data_rows: usize,
    /// Streaming block id for the in-flight paragraph (if any).
    pub(crate) streaming_id: Option<BlockId>,
    /// Streaming block id for the in-flight table (if any). Rewritten
    /// with the accumulated table text on each new row.
    pub(crate) table_streaming_id: Option<BlockId>,
    /// Streaming block id for the in-flight code line (if any).
    /// Rewritten as characters flow; set to `Done` on newline.
    pub(crate) code_line_streaming_id: Option<BlockId>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_artifact_bounded_lru_roundtrip() {
        // Resize cycle 100 → 80 → 100 → 80 must hit cache on every repeat
        // step, because the bounded LRU keeps both widths resident.
        let mut history = BlockHistory::new();
        let id = history.push(Block::Text {
            content: "the quick brown fox jumps over the lazy dog".into(),
        });

        let _ = history.total_rows(100, true);
        let _ = history.total_rows(80, true);
        let _ = history.total_rows(100, true);
        let _ = history.total_rows(80, true);

        let content_hash = history.content_hash(id);
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
            view_state: ViewState::Expanded,
            content_hash,
        };
        let k80 = LayoutKey {
            width: 80,
            show_thinking: true,
            view_state: ViewState::Expanded,
            content_hash,
        };
        assert!(keys.contains(&k100), "expected width=100 cached: {keys:?}");
        assert!(keys.contains(&k80), "expected width=80 cached: {keys:?}");
        assert!(keys.len() <= BlockArtifact::MAX_LAYOUTS);
    }

    #[test]
    fn rewrite_preserves_id_and_invalidates_layout_by_hash() {
        let mut history = BlockHistory::new();
        let id = history.push(Block::Text {
            content: "hello".into(),
        });

        let _ = history.total_rows(80, true);
        let h0 = history.content_hash(id);
        assert!(!history.artifacts.get(&id).unwrap().is_empty());

        history.rewrite(
            id,
            Block::Text {
                content: "hello world".into(),
            },
        );
        let h1 = history.content_hash(id);
        assert_ne!(h0, h1, "content hash must update on rewrite");
        assert_eq!(
            history.order.to_vec(),
            vec![id],
            "rewrite must not change order"
        );

        let _ = history.total_rows(80, true);
        let keys: Vec<u64> = history
            .artifacts
            .get(&id)
            .unwrap()
            .layouts
            .iter()
            .map(|(k, _)| k.content_hash)
            .collect();
        assert!(keys.contains(&h1), "new content hash cached: {keys:?}");
    }

    #[test]
    fn streaming_blocks_render_inline() {
        // Streaming blocks participate in the main paint path like any
        // other block — alt-buffer repaints every frame, and the "live"
        // status is a style distinction, not a layout one.
        let mut history = BlockHistory::new();
        history.push(Block::Text {
            content: "hello".into(),
        });
        let base_rows = history.total_rows(80, false);
        let streaming_id = history.push(Block::Text {
            content: "streaming content".into(),
        });
        history.set_status(streaming_id, Status::Streaming);
        assert!(
            history.total_rows(80, false) > base_rows,
            "streaming block must render inline",
        );
        assert!(history.block_gap(1) > 0, "streaming block takes its gap");
        let key = LayoutKey {
            width: 80,
            show_thinking: false,
            view_state: ViewState::Expanded,
            content_hash: 0,
        };
        assert!(history.ensure_rows(1, key) > 0);
        // Flipping to Done doesn't change rendering.
        history.set_status(streaming_id, Status::Done);
        assert!(history.total_rows(80, false) > base_rows);
    }

    #[test]
    fn streaming_block_ids_filters_on_status() {
        let mut history = BlockHistory::new();
        let a = history.push(Block::Text {
            content: "a".into(),
        });
        let b = history.push(Block::Text {
            content: "b".into(),
        });
        history.set_status(b, Status::Streaming);
        let streaming: Vec<BlockId> = history.streaming_block_ids().collect();
        assert_eq!(streaming, vec![b]);
        history.set_status(a, Status::Streaming);
        let streaming: Vec<BlockId> = history.streaming_block_ids().collect();
        assert_eq!(streaming, vec![a, b]);
        history.set_status(b, Status::Done);
        let streaming: Vec<BlockId> = history.streaming_block_ids().collect();
        assert_eq!(streaming, vec![a]);
    }

    #[test]
    fn identical_blocks_get_distinct_ids() {
        // Each push mints a fresh monotonic `BlockId`. Identical content
        // at two positions no longer shares a slot in `blocks`.
        let mut history = BlockHistory::new();
        let a = history.push(Block::Text {
            content: "same".into(),
        });
        let b = history.push(Block::Text {
            content: "same".into(),
        });
        assert_ne!(a, b);
        assert_eq!(history.order.len(), 2);
        assert_eq!(history.blocks.len(), 2);
        assert_eq!(history.content_hash(a), history.content_hash(b));
    }

    #[test]
    fn raw_text_preserves_markdown_markers() {
        // Whole-block yank must round-trip every inline / block
        // markdown construct — bold, italic, inline code, fenced code,
        // tables, horizontal rules — because the cell-walked fallback
        // strips the markers.
        let md = concat!(
            "**bold** and *italic* and `inline code`\n",
            "\n",
            "```rust\n",
            "let x = 1;\n",
            "```\n",
            "\n",
            "| col | val |\n",
            "| --- | --- |\n",
            "| a   | 1   |\n",
            "\n",
            "---\n",
        );
        let block = Block::Text { content: md.into() };
        assert_eq!(block.raw_text().as_deref(), Some(md));
    }

    #[test]
    fn raw_text_returns_user_text_verbatim() {
        let block = Block::User {
            text: "Explain **this** in detail.".into(),
            image_labels: vec!["[screenshot.png]".into()],
        };
        // Image labels are a render-time annotation, not part of the
        // user's typed message.
        assert_eq!(
            block.raw_text().as_deref(),
            Some("Explain **this** in detail.")
        );
    }

    #[test]
    fn raw_text_is_none_for_structured_blocks() {
        // Tool / confirm / agent blocks don't have a single markdown
        // source — yank falls back to cell-walking for them.
        assert!(Block::ToolCall {
            call_id: "c1".into(),
            name: "bash".into(),
            summary: "ls".into(),
            args: HashMap::new(),
        }
        .raw_text()
        .is_none());
        assert!(Block::Confirm {
            tool: "bash".into(),
            desc: "run ls".into(),
            choice: None,
        }
        .raw_text()
        .is_none());
        assert!(Block::Agent {
            agent_id: "a1".into(),
            slug: None,
            blocking: false,
            tool_calls: Vec::new(),
            status: AgentBlockStatus::Running,
            elapsed: None,
        }
        .raw_text()
        .is_none());
    }
}
