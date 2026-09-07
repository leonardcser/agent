use super::display_layout::{
    render_block_into, CompileJob, LayoutCache, MeasureCtx, RenderCtx, TranscriptRenderEnv,
};
use crate::content::estimate_text_rows;
use crate::content::transcript_scene::{
    NodeLayoutKey, RenderNode, RenderNodeId, TranscriptDefaultViewPolicy,
    TranscriptPresentationState, TranscriptScene, TRANSCRIPT_APPEND_HEADROOM,
};
use crate::smelt_edit::Theme;
use crate::smelt_edit::{
    add_signed_row, clamp_scroll, row_to_usize, BufCreateOpts, BufId, Buffer, CopyOutput,
    DisplayRow, DisplayRows, DocRange, MaterializedRows, RowBreak, RowIndex,
};
use smelt_buffer::coords::{copy_byte_range, CopyRangeAccumulator, CopyRow};
#[cfg(test)]
use smelt_core::buffer::{LineDecoration, Span};
use smelt_core::buffer::{RenderedBufferRebuild, RenderedRowMetadata};
use smelt_core::content::highlight::InlineOptions;
use smelt_core::transcript_model::{BlockHistory, BlockId, BlockText, LayoutKey, ViewState};
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

const COPY_CHUNK_NODES: usize = 64;

pub(crate) struct TranscriptProjection {
    transcript_scene: TranscriptScene,
    default_view_policy: TranscriptDefaultViewPolicy,
    presentation: TranscriptPresentationState,
    layout_cache: LayoutCache,
    pending_changed_blocks: HashSet<BlockId>,
    active_width: u16,
    visible: VisibleProjectionState,
    measurements: MeasurementIndexStore,
    projection_generation: u64,
    renderer_generation: Option<u64>,
    renderer_cache_key: Option<u64>,
    inline_options: InlineOptions,
    display_layout_budget: usize,
    full_rows_budget: usize,
    #[cfg(test)]
    counters: TranscriptProjectionCounters,
}

#[derive(Clone, Debug)]
pub(crate) struct TranscriptExactHeightObservation {
    pub(crate) block_id: BlockId,
    pub(crate) key: LayoutKey,
    pub(crate) rows: RowIndex,
}

#[derive(Clone, Debug)]
pub(crate) struct TranscriptExactHeightSnapshot {
    pub(crate) width: u16,
    pub(crate) renderer_generation: u64,
    pub(crate) renderer_cache_key: Option<u64>,
    pub(crate) presentation_generation: u64,
    pub(crate) observations: Vec<TranscriptExactHeightObservation>,
}

#[derive(Clone, Copy)]
struct ProjectedRowIdentity {
    exact: ProjectionAnchor,
    content: Option<ProjectionAnchor>,
}

#[derive(Default)]
struct VisibleProjectionState {
    materialized: Option<MaterializedProjection>,
    /// Block layout from the last visible `project()`. Surfaced to Lua via `visible_blocks`.
    block_layout: Vec<LayoutEntry>,
    /// Absolute row represented by local row 0 in the backing buffer.
    row_base: RowIndex,
    /// Total rows in the logical transcript represented by the visible projection.
    total_rows: RowIndex,
    /// Exact and content-aware identities for each backing buffer line.
    row_identities: Vec<ProjectedRowIdentity>,
    /// Cached `build_rows` result for full-text consumers (Lua API, vim navigation).
    full_rows: Option<CachedRows>,
}

struct MeasurementIndexStore {
    active: TranscriptHeightIndex,
    entries: VecDeque<DisplayRowIndexEntry>,
    retained_bytes: usize,
    budget: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct TranscriptRenderMemorySnapshot {
    pub(crate) layout_bytes: usize,
    pub(crate) pinned_layout_bytes: usize,
    pub(crate) height_index_bytes: usize,
    pub(crate) height_index_cache_bytes: usize,
    pub(crate) visible_rows_bytes: usize,
    pub(crate) full_rows_bytes: usize,
    pub(crate) oversize_debt_bytes: usize,
}

impl Default for MeasurementIndexStore {
    fn default() -> Self {
        Self {
            active: TranscriptHeightIndex::default(),
            entries: VecDeque::new(),
            retained_bytes: 0,
            budget: 2 * 1024 * 1024,
        }
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TranscriptProjectionCounters {
    pub full_row_builds: usize,
    pub full_layout_materializations: usize,
    pub layout_cache: usize,
    pub exact_height_measured_blocks: usize,
    pub projection_planning_passes: usize,
    pub range_materialized_blocks: usize,
    pub max_range_materialized_blocks: usize,
    pub range_materialized_rows: usize,
    pub max_range_materialized_rows: usize,
}

#[cfg(test)]
thread_local! {
    static FULL_LAYOUT_BUFFER_RENDERS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(super) fn record_full_layout_buffer_render() {
    FULL_LAYOUT_BUFFER_RENDERS.with(|count| count.set(count.get().saturating_add(1)));
}

#[cfg(test)]
fn reset_full_layout_buffer_renders() {
    FULL_LAYOUT_BUFFER_RENDERS.with(|count| count.set(0));
}

#[cfg(test)]
fn full_layout_buffer_renders() -> usize {
    FULL_LAYOUT_BUFFER_RENDERS.with(|count| count.get())
}

struct CachedRows {
    rows: Arc<Vec<String>>,
    generation: u64,
    renderer_generation: u64,
    renderer_cache_key: Option<u64>,
    presentation_generation: u64,
    width: u16,
}

fn cached_rows_retained_bytes(rows: &Arc<Vec<String>>) -> usize {
    rows.capacity()
        .saturating_mul(std::mem::size_of::<String>())
        .saturating_add(rows.iter().map(String::capacity).sum::<usize>())
}

#[derive(Clone, Copy)]
struct RowIndexKey {
    width: u16,
    renderer_generation: u64,
    renderer_cache_key: Option<u64>,
    presentation_generation: u64,
    base_key: LayoutKey,
}

#[derive(Clone)]
struct DisplayRowIndexEntry {
    width: u16,
    renderer_generation: u64,
    renderer_cache_key: Option<u64>,
    nodes: Vec<DisplayRowIndexNode>,
}

#[derive(Clone)]
struct DisplayRowIndexNode {
    id: RenderNodeId,
    key: NodeLayoutKey,
    exact_height: u64,
}

impl DisplayRowIndexEntry {
    fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(
            self.nodes
                .capacity()
                .saturating_mul(std::mem::size_of::<DisplayRowIndexNode>()),
        )
    }
}

impl RowIndexKey {
    fn new(
        width: u16,
        renderer_generation: u64,
        renderer_cache_key: Option<u64>,
        presentation_generation: u64,
    ) -> Self {
        Self {
            width,
            renderer_generation,
            renderer_cache_key,
            presentation_generation,
            base_key: base_layout_key(width),
        }
    }
}

#[derive(Default)]
struct TranscriptHeightIndex {
    nodes: Vec<TranscriptHeightNode>,
    prefix_rows: Vec<RowIndex>,
    prefix_dirty_from: Option<usize>,
    generation: u64,
    renderer_generation: u64,
    renderer_cache_key: Option<u64>,
    presentation_generation: u64,
    width: u16,
}

struct TranscriptHeightNode {
    id: RenderNodeId,
    key: NodeLayoutKey,
    estimated_height: RowIndex,
    exact_height: Option<RowIndex>,
}

impl TranscriptHeightNode {
    fn measured_or_estimated_height(&self) -> RowIndex {
        self.exact_height.unwrap_or(self.estimated_height)
    }
}

impl TranscriptHeightIndex {
    fn is_current(&self, plan: &TranscriptScene, key: RowIndexKey) -> bool {
        self.generation == plan.revision
            && self.renderer_generation == key.renderer_generation
            && self.renderer_cache_key == key.renderer_cache_key
            && self.presentation_generation == key.presentation_generation
            && self.width == key.width
            && self.nodes.len() == plan.len()
    }

    fn rebuild_if_stale(
        &mut self,
        history: &BlockHistory,
        plan: &TranscriptScene,
        policy: &TranscriptDefaultViewPolicy,
        presentation: &TranscriptPresentationState,
        key: RowIndexKey,
    ) {
        let gen = plan.revision;
        if self.generation == gen
            && self.renderer_generation == key.renderer_generation
            && self.renderer_cache_key == key.renderer_cache_key
            && self.presentation_generation == key.presentation_generation
            && self.width == key.width
        {
            return;
        }

        let keep_measurements = self.renderer_generation == key.renderer_generation
            && self.renderer_cache_key == key.renderer_cache_key
            && self.width == key.width;
        let old_nodes = if keep_measurements {
            std::mem::take(&mut self.nodes)
        } else {
            Vec::new()
        };
        self.nodes.clear();
        self.nodes
            .reserve(plan.len().saturating_add(TRANSCRIPT_APPEND_HEADROOM));
        for index in 0..plan.len() {
            let Some(id) = plan.node_id(index) else {
                continue;
            };
            let Some(node_key) = plan.node_key(policy, history, presentation, index, key.base_key)
            else {
                continue;
            };
            let old_same_index = old_nodes.get(index).filter(|node| node.id == id);
            let estimated_height = old_same_index
                .map(TranscriptHeightNode::measured_or_estimated_height)
                .or_else(|| {
                    old_nodes
                        .iter()
                        .find(|node| node.id == id)
                        .map(TranscriptHeightNode::measured_or_estimated_height)
                })
                .unwrap_or_else(|| estimate_node_height(history, plan, index, node_key));
            let same_previous = index == 0
                || old_nodes
                    .get(index.saturating_sub(1))
                    .zip(self.nodes.get(index.saturating_sub(1)))
                    .is_some_and(|(old, new)| old.id == new.id && old.key == new.key);
            let exact_height = old_same_index
                .filter(|node| node.key == node_key && same_previous)
                .and_then(|node| node.exact_height);
            self.nodes.push(TranscriptHeightNode {
                id,
                key: node_key,
                estimated_height,
                exact_height,
            });
        }
        self.generation = gen;
        self.renderer_generation = key.renderer_generation;
        self.renderer_cache_key = key.renderer_cache_key;
        self.presentation_generation = key.presentation_generation;
        self.width = key.width;
        self.rebuild_prefix_rows();
    }

    fn set_exact_height(&mut self, index: usize, rows: RowIndex) -> bool {
        let Some(node) = self.nodes.get_mut(index) else {
            return false;
        };
        if node.exact_height == Some(rows) {
            return false;
        }
        node.exact_height = Some(rows);
        self.mark_prefix_dirty_from(index);
        true
    }

    fn invalidate_for_width(&mut self, width: u16) {
        if self.width == width {
            return;
        }
        self.width = width;
        if self.nodes.is_empty() {
            self.prefix_dirty_from = None;
            return;
        }
        for node in &mut self.nodes {
            node.estimated_height = node.measured_or_estimated_height();
            node.exact_height = None;
            node.key.width = width;
        }
        self.mark_prefix_dirty_from(0);
    }

    /// Reuse an unchanged index or extend it for a structural append.
    fn try_reuse_or_extend(
        &mut self,
        history: &BlockHistory,
        plan: &TranscriptScene,
        policy: &TranscriptDefaultViewPolicy,
        presentation: &TranscriptPresentationState,
        key: RowIndexKey,
    ) -> bool {
        let old_len = self.nodes.len();
        if self.renderer_generation != key.renderer_generation
            || self.renderer_cache_key != key.renderer_cache_key
        {
            return false;
        }
        if old_len > plan.len() {
            return false;
        }
        if old_len == plan.len()
            && self.generation == plan.revision
            && self.renderer_generation == key.renderer_generation
            && self.renderer_cache_key == key.renderer_cache_key
            && self.presentation_generation == key.presentation_generation
            && self.width == key.width
        {
            return true;
        }
        if old_len == plan.len()
            && self.generation == plan.revision
            && self.renderer_generation == key.renderer_generation
            && self.renderer_cache_key == key.renderer_cache_key
            && self.presentation_generation == key.presentation_generation
        {
            self.invalidate_for_width(key.width);
            return true;
        }
        if self.width == key.width && self.presentation_generation == key.presentation_generation {
            if let Some((prefix_len, prefix_revision)) = plan.append_prefix() {
                if old_len == prefix_len && self.generation == prefix_revision {
                    for index in old_len..plan.len() {
                        let Some(id) = plan.node_id(index) else {
                            return false;
                        };
                        let Some(node_key) =
                            plan.node_key(policy, history, presentation, index, key.base_key)
                        else {
                            return false;
                        };
                        self.nodes.push(TranscriptHeightNode {
                            id,
                            key: node_key,
                            estimated_height: estimate_node_height(history, plan, index, node_key),
                            exact_height: None,
                        });
                    }
                    self.generation = plan.revision;
                    self.renderer_generation = key.renderer_generation;
                    self.renderer_cache_key = key.renderer_cache_key;
                    self.presentation_generation = key.presentation_generation;
                    self.mark_prefix_dirty_from(old_len);
                    return true;
                }
            }
        }
        false
    }

    fn apply_changed_blocks(
        &mut self,
        history: &BlockHistory,
        plan: &TranscriptScene,
        policy: &TranscriptDefaultViewPolicy,
        presentation: &TranscriptPresentationState,
        key: RowIndexKey,
        block_ids: &HashSet<BlockId>,
    ) -> bool {
        if self.renderer_generation != key.renderer_generation
            || self.renderer_cache_key != key.renderer_cache_key
            || self.presentation_generation != key.presentation_generation
            || self.width != key.width
            || self.nodes.len() != plan.len()
        {
            return false;
        }

        let mut changed_indices = Vec::with_capacity(block_ids.len());
        for id in block_ids {
            let Some(index) = plan.index_for_block(*id) else {
                return false;
            };
            changed_indices.push(index);
        }
        changed_indices.sort_unstable();
        changed_indices.dedup();
        let mut dirty_from = None;
        for index in changed_indices {
            let Some(node_key) = plan.node_key(policy, history, presentation, index, key.base_key)
            else {
                return false;
            };
            let estimated_height = estimate_node_height(history, plan, index, node_key);
            let Some(node) = self.nodes.get_mut(index) else {
                return false;
            };
            if Some(node.id) != plan.node_id(index) {
                return false;
            }
            node.key = node_key;
            node.estimated_height = estimated_height;
            node.exact_height = None;

            let next_index = index.saturating_add(1);
            if let Some(next) = self.nodes.get_mut(next_index) {
                next.estimated_height = estimate_node_height(history, plan, next_index, next.key);
                next.exact_height = None;
            }
            dirty_from = Some(dirty_from.map_or(index, |dirty: usize| dirty.min(index)));
        }
        if let Some(index) = dirty_from {
            self.mark_prefix_dirty_from(index);
        }
        self.generation = plan.revision;
        true
    }

    fn is_exact_for(&self, plan: &TranscriptScene, key: RowIndexKey) -> bool {
        self.is_current(plan, key) && self.nodes.iter().all(|node| node.exact_height.is_some())
    }

    fn refresh_prefix_rows(&mut self) {
        let Some(start) = self.prefix_dirty_from else {
            return;
        };
        self.rebuild_prefix_rows_from(start);
    }

    fn prefix_row(&self, index: usize) -> RowIndex {
        self.prefix_rows.get(index).copied().unwrap_or(0)
    }

    fn total_rows(&self) -> RowIndex {
        self.prefix_rows.last().copied().unwrap_or(0)
    }

    #[allow(dead_code)]
    fn scroll_anchor_at_row(&self, row: RowIndex) -> Option<ProjectionAnchor> {
        let index = self.node_index_before_or_at_row(row)?;
        let node = self.nodes.get(index)?;
        let first_row = self.prefix_row(index);
        let rows = node.measured_or_estimated_height().max(1);
        Some(ProjectionAnchor::Node {
            id: node.id,
            row_offset: row.saturating_sub(first_row).min(rows.saturating_sub(1)),
        })
    }

    fn row_for_node_anchor(&self, id: RenderNodeId, row_offset: RowIndex) -> Option<RowIndex> {
        let index = self.nodes.iter().position(|node| node.id == id)?;
        let node = self.nodes.get(index)?;
        let rows = node.measured_or_estimated_height().max(1);
        Some(
            self.prefix_row(index)
                .saturating_add(row_offset.min(rows.saturating_sub(1))),
        )
    }

    fn search_layout_hash(&self, projection_generation: u64) -> u64 {
        smelt_core::utils::hash_serializable(&(
            projection_generation,
            self.generation,
            self.renderer_generation,
            self.renderer_cache_key,
            self.presentation_generation,
            self.width,
            self.nodes.len(),
            self.total_rows(),
        ))
    }

    fn start_index_for_row(&self, row: RowIndex) -> usize {
        let idx = self.prefix_rows.partition_point(|prefix| *prefix <= row);
        idx.saturating_sub(1).min(self.nodes.len())
    }

    fn node_index_at_row(&self, row: RowIndex) -> Option<usize> {
        if self.nodes.is_empty() || row >= self.total_rows() {
            return None;
        }
        Some(
            self.start_index_for_row(row)
                .min(self.nodes.len().saturating_sub(1)),
        )
    }

    fn block_index(&self, id: BlockId) -> Option<usize> {
        self.nodes
            .iter()
            .position(|node| node.id.as_block_id() == Some(id))
    }

    fn node_index(&self, id: RenderNodeId) -> Option<usize> {
        self.nodes.iter().position(|node| node.id == id)
    }

    fn node_index_before_or_at_row(&self, row: RowIndex) -> Option<usize> {
        if self.nodes.is_empty() {
            return None;
        }
        if row >= self.total_rows() {
            return Some(self.nodes.len().saturating_sub(1));
        }
        self.node_index_at_row(row)
    }

    fn end_index_for_row_end(&self, row_end: RowIndex) -> usize {
        self.prefix_rows
            .partition_point(|prefix| *prefix < row_end)
            .min(self.nodes.len())
    }

    fn node_range_for_rows(&self, rows: std::ops::Range<RowIndex>) -> std::ops::Range<usize> {
        if rows.start >= rows.end {
            return 0..0;
        }
        let first = self.start_index_for_row(rows.start);
        let end = self.end_index_for_row_end(rows.end).max(first);
        first..end
    }

    fn hydrate_from_cache(
        &mut self,
        history: &BlockHistory,
        plan: &TranscriptScene,
        policy: &TranscriptDefaultViewPolicy,
        presentation: &TranscriptPresentationState,
        entry: &DisplayRowIndexEntry,
        key: RowIndexKey,
    ) -> bool {
        if entry.renderer_generation != key.renderer_generation
            || entry.renderer_cache_key != key.renderer_cache_key
            || entry.width != key.width
        {
            return false;
        }
        if entry.nodes.len() != plan.len() {
            return false;
        }
        let mut nodes = Vec::with_capacity(entry.nodes.len());
        for (index, cached) in entry.nodes.iter().enumerate() {
            let Some(id) = plan.node_id(index) else {
                return false;
            };
            if cached.id != id {
                return false;
            }
            let Some(node_key) = plan.node_key(policy, history, presentation, index, key.base_key)
            else {
                return false;
            };
            if cached.key != node_key {
                return false;
            }
            nodes.push(TranscriptHeightNode {
                id,
                key: node_key,
                estimated_height: cached.exact_height,
                exact_height: Some(cached.exact_height),
            });
        }
        self.nodes = nodes;
        self.generation = plan.revision;
        self.renderer_generation = key.renderer_generation;
        self.renderer_cache_key = key.renderer_cache_key;
        self.presentation_generation = key.presentation_generation;
        self.width = key.width;
        self.rebuild_prefix_rows();
        true
    }

    fn exact_height_snapshot(&self) -> TranscriptExactHeightSnapshot {
        TranscriptExactHeightSnapshot {
            width: self.width,
            renderer_generation: self.renderer_generation,
            renderer_cache_key: self.renderer_cache_key,
            presentation_generation: self.presentation_generation,
            observations: self
                .nodes
                .iter()
                .filter_map(|node| {
                    Some(TranscriptExactHeightObservation {
                        block_id: node.id.as_block_id()?,
                        key: node.key,
                        rows: node.exact_height?,
                    })
                })
                .collect(),
        }
    }

    fn cache_entry(&self) -> Option<DisplayRowIndexEntry> {
        if self.nodes.is_empty() || self.nodes.iter().any(|node| node.exact_height.is_none()) {
            return None;
        }
        Some(DisplayRowIndexEntry {
            width: self.width,
            renderer_generation: self.renderer_generation,
            renderer_cache_key: self.renderer_cache_key,
            nodes: self
                .nodes
                .iter()
                .map(|node| DisplayRowIndexNode {
                    id: node.id,
                    key: node.key,
                    exact_height: node.exact_height.unwrap_or(node.estimated_height),
                })
                .collect(),
        })
    }

    fn rebuild_prefix_rows(&mut self) {
        self.prefix_rows.clear();
        self.prefix_rows.reserve(
            self.nodes
                .len()
                .saturating_add(1)
                .saturating_add(TRANSCRIPT_APPEND_HEADROOM),
        );
        self.prefix_rows.push(0);
        let mut total: RowIndex = 0;
        for node in &self.nodes {
            total = total.saturating_add(node.exact_height.unwrap_or(node.estimated_height));
            self.prefix_rows.push(total);
        }
        self.prefix_dirty_from = None;
    }

    fn rebuild_prefix_rows_from(&mut self, start: usize) {
        if start == 0 || self.prefix_rows.len() <= start {
            self.rebuild_prefix_rows();
            return;
        }
        self.prefix_rows.truncate(start + 1);
        let mut total = self.prefix_rows[start];
        for node in self.nodes.iter().skip(start) {
            total = total.saturating_add(node.exact_height.unwrap_or(node.estimated_height));
            self.prefix_rows.push(total);
        }
        self.prefix_dirty_from = None;
    }

    fn mark_prefix_dirty_from(&mut self, index: usize) {
        self.prefix_dirty_from = Some(
            self.prefix_dirty_from
                .map_or(index, |existing| existing.min(index)),
        );
    }
}

impl TranscriptHeightIndex {
    fn retained_bytes(&self) -> usize {
        self.nodes
            .capacity()
            .saturating_mul(std::mem::size_of::<TranscriptHeightNode>())
            .saturating_add(
                self.prefix_rows
                    .capacity()
                    .saturating_mul(std::mem::size_of::<RowIndex>()),
            )
    }
}

impl MeasurementIndexStore {
    fn clear(&mut self) {
        self.active = TranscriptHeightIndex::default();
        self.entries.clear();
        self.retained_bytes = 0;
    }

    fn invalidate_nodes(&mut self, ids: &HashSet<RenderNodeId>, scene: &TranscriptScene) {
        let mut first_dirty = None;
        for id in ids {
            let Some(index) = scene.index_of(*id) else {
                continue;
            };
            let Some(node) = self
                .active
                .nodes
                .get_mut(index)
                .filter(|node| node.id == *id)
            else {
                continue;
            };
            node.exact_height = None;
            first_dirty = Some(first_dirty.map_or(index, |current: usize| current.min(index)));
        }
        if let Some(index) = first_dirty {
            self.active.mark_prefix_dirty_from(index);
        }

        // Every retained entry is a complete index for another width. A timed
        // node refresh invalidates that node at every width, so no entry remains
        // reusable. Clearing the bounded entry set avoids scanning every node.
        self.entries.clear();
        self.retained_bytes = 0;
    }

    fn set_budget(&mut self, budget: usize) {
        self.budget = budget;
        self.enforce_budget();
    }

    fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    fn enforce_budget(&mut self) {
        while self.retained_bytes > self.budget && !self.entries.is_empty() {
            if let Some(entry) = self.entries.pop_front() {
                self.retained_bytes = self.retained_bytes.saturating_sub(entry.retained_bytes());
            }
        }
        smelt_perf::perf::record_value(
            "transcript:height_index_cache:retained_bytes",
            self.retained_bytes as u64,
        );
    }

    fn remember_active(&mut self) {
        if let Some(entry) = self.active.cache_entry() {
            self.retained_bytes =
                upsert_row_index_entry(&mut self.entries, entry, self.retained_bytes);
            self.enforce_budget();
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
enum ProjectionAnchor {
    Node {
        id: RenderNodeId,
        row_offset: RowIndex,
    },
    RenderedBlockRow {
        id: BlockId,
        row_offset: RowIndex,
    },
    RenderedBlockDisplayOffset {
        id: BlockId,
        row_offset: RowIndex,
        display_offset: usize,
    },
}

impl ProjectionAnchor {
    fn rendered_block_row(id: BlockId, row_offset: RowIndex) -> Self {
        Self::RenderedBlockRow { id, row_offset }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct StableRowAnchor(ProjectionAnchor);

impl StableRowAnchor {
    pub(crate) fn block_id(self) -> Option<BlockId> {
        match self.0 {
            ProjectionAnchor::Node { id, .. } => id.as_block_id(),
            ProjectionAnchor::RenderedBlockRow { id, .. }
            | ProjectionAnchor::RenderedBlockDisplayOffset { id, .. } => Some(id),
        }
    }
}

impl From<TranscriptRowAnchor> for StableRowAnchor {
    fn from(anchor: TranscriptRowAnchor) -> Self {
        Self(ProjectionAnchor::Node {
            id: anchor.id,
            row_offset: anchor.row_offset,
        })
    }
}

#[derive(Clone, Copy)]
struct ResolvedProjectionTarget {
    requested: ScrollTarget,
    anchor: Option<ProjectionAnchor>,
}

#[derive(Clone, Copy)]
struct LayoutEntry {
    id: BlockId,
    /// First absolute row of the block, after its leading gap.
    start: RowIndex,
    rows: RowIndex,
}

#[derive(Clone, Debug)]
pub(crate) struct TranscriptSearchLayout {
    pub(crate) generation: u64,
    pub(crate) entries: Vec<TranscriptSearchLayoutEntry>,
}

#[derive(Clone, Debug)]
pub(crate) struct TranscriptSearchLayoutEntry {
    pub(crate) block_ids: Vec<BlockId>,
    pub(crate) first_row: RowIndex,
    pub(crate) rows: RowIndex,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ProjectKey {
    generation: u64,
    width: u16,
    renderer_generation: u64,
    renderer_cache_key: Option<u64>,
    presentation_generation: u64,
    row_generation: u64,
    mode: ProjectionMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProjectionMode {
    Visible { viewport_rows: u16 },
}

#[derive(Clone, Copy)]
struct MaterializedProjection {
    key: ProjectKey,
    buf_id: BufId,
    changedtick: u64,
}

pub(crate) struct ProjectionPlan {
    key: ProjectKey,
    target: ResolvedProjectionTarget,
    scroll_top: RowIndex,
    total_rows: RowIndex,
    viewport_rows: u16,
    row_window: std::ops::Range<RowIndex>,
    node_range: std::ops::Range<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ExactRowTapeHandle {
    key: ProjectKey,
    rows: MaterializedRows,
}

#[derive(Clone, Copy)]
pub(crate) struct ExactRowTapeState {
    pub(crate) rows: MaterializedRows,
    pub(crate) top_anchor: Option<StableRowAnchor>,
}

#[derive(Clone, Copy)]
pub(crate) struct ExactRowTapeProjection {
    key: ProjectKey,
    rows: MaterializedRows,
    top_anchor: Option<StableRowAnchor>,
}

impl ExactRowTapeProjection {
    pub(crate) fn rows(self) -> MaterializedRows {
        self.rows
    }

    pub(crate) fn top_anchor(self) -> Option<StableRowAnchor> {
        self.top_anchor
    }
}

struct ProjectionRequest {
    key: ProjectKey,
    target: ResolvedProjectionTarget,
    viewport_rows: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FoldAction {
    Toggle,
    Peek,
    Open,
    Close,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FoldActivation {
    AnyNodeRow,
    ExplicitTargetOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FoldAtRow {
    pub(crate) row: RowIndex,
    pub(crate) action: FoldAction,
    pub(crate) activation: FoldActivation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TranscriptNodeRow {
    pub(crate) id: RenderNodeId,
    pub(crate) index: usize,
    pub(crate) first_row: RowIndex,
    pub(crate) rows: RowIndex,
    pub(crate) row_offset: RowIndex,
    pub(crate) view_state: ViewState,
    pub(crate) explicit_fold_target: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TranscriptRowAnchor {
    pub(crate) id: RenderNodeId,
    pub(crate) row_offset: RowIndex,
}

impl TranscriptNodeRow {
    pub(crate) fn can_activate(self, activation: FoldActivation) -> bool {
        match activation {
            FoldActivation::AnyNodeRow => true,
            FoldActivation::ExplicitTargetOnly => self.explicit_fold_target,
        }
    }
}

fn is_explicit_fold_target(view_state: ViewState, row_offset: RowIndex, rows: RowIndex) -> bool {
    match view_state {
        ViewState::Collapsed | ViewState::Peek => true,
        ViewState::TrimmedHead { .. } => row_offset.saturating_add(1) == rows,
        ViewState::TrimmedTail { .. } => row_offset == 0,
        ViewState::Expanded => false,
    }
}

fn layout_view_state(
    _id: RenderNodeId,
    _view_state: ViewState,
    _history: &BlockHistory,
) -> ViewState {
    ViewState::Expanded
}

impl ProjectionPlan {
    pub(crate) fn node_range(&self) -> std::ops::Range<usize> {
        self.node_range.clone()
    }

    pub(crate) fn scroll_top(&self) -> RowIndex {
        self.scroll_top
    }

    pub(crate) fn total_rows(&self) -> RowIndex {
        self.total_rows
    }

    fn row_window(&self) -> std::ops::Range<RowIndex> {
        self.row_window.clone()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScrollAnchor {
    ExactRow(RowIndex),
    ReflowStableRow(RowIndex),
    StableRowDelta {
        row: RowIndex,
        anchor: Option<StableRowAnchor>,
        delta: isize,
    },
    Tail,
}

impl ScrollAnchor {
    fn as_scroll_top(self) -> RowIndex {
        match self {
            Self::ExactRow(row) | Self::ReflowStableRow(row) => row,
            Self::StableRowDelta { row, delta, .. } => add_signed_row(row, delta),
            Self::Tail => RowIndex::MAX,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScrollTarget {
    /// Materialize only the visible window around the anchor.
    Visible(ScrollAnchor),
}

impl ScrollTarget {
    pub(crate) fn visible_row(row: RowIndex) -> Self {
        Self::Visible(ScrollAnchor::ExactRow(row))
    }

    pub(crate) fn visible_reflow_stable_row(row: RowIndex) -> Self {
        Self::Visible(ScrollAnchor::ReflowStableRow(row))
    }

    pub(crate) fn visible_stable_row_delta(
        row: RowIndex,
        anchor: Option<TranscriptRowAnchor>,
        delta: isize,
    ) -> Self {
        Self::visible_stable_anchor_delta(row, anchor.map(StableRowAnchor::from), delta)
    }

    pub(crate) fn visible_stable_anchor_delta(
        row: RowIndex,
        anchor: Option<StableRowAnchor>,
        delta: isize,
    ) -> Self {
        Self::Visible(ScrollAnchor::StableRowDelta { row, anchor, delta })
    }

    pub(crate) fn visible_tail() -> Self {
        Self::Visible(ScrollAnchor::Tail)
    }

    fn anchor(self) -> ScrollAnchor {
        match self {
            Self::Visible(anchor) => anchor,
        }
    }

    fn as_scroll_top(self) -> RowIndex {
        self.anchor().as_scroll_top()
    }

    fn mode(self, viewport_rows: u16) -> ProjectionMode {
        match self {
            Self::Visible(_) => ProjectionMode::Visible { viewport_rows },
        }
    }
}

struct ProjectRows<'a> {
    texts: &'a mut Vec<String>,
    text_cursor: usize,
    metadata: &'a mut RenderedRowMetadata,
    layout: &'a mut Vec<LayoutEntry>,
    row_identities: &'a mut Vec<ProjectedRowIdentity>,
}

impl ProjectRows<'_> {
    fn push_text(&mut self, text: &str) {
        if let Some(row) = self.texts.get_mut(self.text_cursor) {
            row.clear();
            row.push_str(text);
        } else {
            self.texts.push(text.to_string());
        }
        self.text_cursor = self.text_cursor.saturating_add(1);
    }

    fn text_len(&self) -> usize {
        self.text_cursor
    }

    fn finish_texts(&mut self) {
        self.texts.truncate(self.text_cursor);
    }
}

struct MaterializedTranscriptRange {
    row_base: RowIndex,
    total_rows: RowIndex,
    rebuild: RenderedBufferRebuild,
    layout: Vec<LayoutEntry>,
    row_identities: Vec<ProjectedRowIdentity>,
}

fn base_layout_key(width: u16) -> LayoutKey {
    LayoutKey {
        view_state: ViewState::Expanded,
        width,
        content_hash: 0,
        sidecar_hash: 0,
    }
}

fn estimate_text_rows_with_prefix(prefix: &str, text: &str, width: u16) -> RowIndex {
    let width = usize::from(width.max(1));
    let prefix_cells = if prefix.is_ascii() {
        prefix.len()
    } else {
        smelt_buffer::text::byte_to_cell(prefix, prefix.len())
    };
    let mut rows = 0;
    let mut first = true;
    for line in text.lines() {
        let mut cells = if line.is_ascii() {
            line.len()
        } else {
            smelt_buffer::text::byte_to_cell(line, line.len())
        };
        if first {
            cells = cells.saturating_add(prefix_cells);
            first = false;
        }
        rows += cells.max(1).div_ceil(width) as RowIndex;
    }
    rows.max(1)
}

fn estimate_content_rows(
    content: &smelt_core::transcript_content::TranscriptContent,
    width: u16,
) -> RowIndex {
    let content = content.read();
    if content.is_empty() {
        return 0;
    }
    let width = usize::from(width.max(1));
    let wrapped_rows = content.display_cells().max(1).div_ceil(width) as RowIndex;
    wrapped_rows.max(content.logical_line_count() as RowIndex)
}

fn estimate_block_text_rows(history: &BlockHistory, id: BlockId, width: u16) -> RowIndex {
    let tool_output = history
        .tool_state(id)
        .and_then(|state| state.output.as_ref())
        .map(|output| &output.content);
    if let Some(content) = tool_output {
        return estimate_content_rows(content, width);
    }
    if !history.is_materialized(id) {
        let estimated_bytes = history.estimated_text_bytes(id);
        if estimated_bytes > 0 {
            return estimated_bytes.div_ceil(u64::from(width.max(1)));
        }
    }
    match history.row_estimate_text(id) {
        Some(BlockText::Plain(text)) => estimate_text_rows(text, width),
        Some(BlockText::Content(content)) => estimate_content_rows(content, width),
        Some(BlockText::Prefixed { prefix, text }) => {
            estimate_text_rows_with_prefix(prefix, text, width)
        }
        Some(BlockText::Thinking {
            title,
            summary_titles,
            content,
        }) => {
            let content_rows = estimate_content_rows(content, width);
            let title_rows = if summary_titles.is_empty() {
                title.map_or(0, |title| estimate_text_rows(title, width))
            } else {
                summary_titles.iter().fold(0 as RowIndex, |rows, title| {
                    rows.saturating_add(estimate_text_rows(title, width))
                })
            };
            content_rows.saturating_add(title_rows).max(1)
        }
        Some(BlockText::Exec { command, output }) => {
            let command_rows = estimate_text_rows_with_prefix("$ ", command, width);
            command_rows.saturating_add(estimate_content_rows(output, width))
        }
        None => 1,
    }
}

fn estimate_node_height(
    history: &BlockHistory,
    plan: &TranscriptScene,
    index: usize,
    key: NodeLayoutKey,
) -> RowIndex {
    let rows = match plan.node(index) {
        Some(RenderNode::Block { id, .. }) => estimate_block_text_rows(history, *id, key.width),
        Some(RenderNode::Group(group)) => group
            .child_ids()
            .map(|id| estimate_block_text_rows(history, id, key.width))
            .sum::<RowIndex>()
            .max(1),
        None => 1,
    };
    let rows = key.view_state.measured_height(rows as u64) as RowIndex;
    let gap = plan.rendered_node_gap(history, index, rows as usize) as RowIndex;
    gap.saturating_add(rows).max(1)
}

fn subtract_byte_range(ranges: &mut Vec<std::ops::Range<usize>>, remove: std::ops::Range<usize>) {
    if remove.start >= remove.end {
        return;
    }
    let mut out = Vec::with_capacity(ranges.len() + 1);
    for range in ranges.drain(..) {
        if remove.end <= range.start || remove.start >= range.end {
            out.push(range);
            continue;
        }
        if range.start < remove.start {
            out.push(range.start..remove.start);
        }
        if remove.end < range.end {
            out.push(remove.end..range.end);
        }
    }
    *ranges = out;
}

fn selectable_row_string(buf: &Buffer, row: usize) -> String {
    let Some(line) = buf.get_line(row) else {
        return String::new();
    };
    let mut ranges: Vec<std::ops::Range<usize>> = std::iter::once(0..line.len()).collect();
    for span in buf
        .highlights_at(row)
        .into_iter()
        .filter(|span| !span.meta.selectable)
    {
        let start = smelt_buffer::text::cell_to_byte(line, span.col_start as usize);
        let end = smelt_buffer::text::cell_to_byte(line, span.col_end as usize);
        subtract_byte_range(&mut ranges, start..end);
    }
    ranges
        .into_iter()
        .map(|range| smelt_buffer::text::slice(line, range))
        .collect()
}

fn display_offset_for_buffer_row(buf: &Buffer, rows: usize, row: RowIndex) -> usize {
    let start = row_to_usize(row).min(rows);
    (0..start).fold(0usize, |offset, row| {
        offset.saturating_add(selectable_row_string(buf, row).len())
    })
}

fn display_offsets_for_buffer_rows(buf: &Buffer, rows: usize) -> Vec<usize> {
    (0..rows)
        .scan(0usize, |offset, row| {
            let current = *offset;
            *offset = offset.saturating_add(selectable_row_string(buf, row).len());
            Some(current)
        })
        .collect()
}

fn row_offset_for_display_offset(
    buf: &Buffer,
    rows: usize,
    display_offset: usize,
    fallback: RowIndex,
) -> RowIndex {
    if rows == 0 {
        return 0;
    }
    let offsets = display_offsets_for_buffer_rows(buf, rows);
    if offsets.iter().all(|offset| *offset == 0) {
        return fallback.min(rows.saturating_sub(1) as RowIndex);
    }
    let mut row = offsets.partition_point(|offset| *offset <= display_offset);
    row = row.saturating_sub(1).min(rows.saturating_sub(1));
    while row > 0 && offsets[row - 1] == offsets[row] {
        row -= 1;
    }
    row as RowIndex
}

fn upsert_row_index_entry(
    entries: &mut VecDeque<DisplayRowIndexEntry>,
    entry: DisplayRowIndexEntry,
    retained_bytes: usize,
) -> usize {
    let entry_bytes = entry.retained_bytes();
    if let Some(existing) = entries.iter_mut().find(|existing| {
        existing.width == entry.width
            && existing.renderer_generation == entry.renderer_generation
            && existing.renderer_cache_key == entry.renderer_cache_key
    }) {
        let old_bytes = existing.retained_bytes();
        *existing = entry;
        retained_bytes
            .saturating_sub(old_bytes)
            .saturating_add(entry_bytes)
    } else {
        entries.push_back(entry);
        retained_bytes.saturating_add(entry_bytes)
    }
}

#[allow(clippy::too_many_arguments)]
fn render_cached_layout_to_buffer(
    layout_cache: &LayoutCache,
    id: RenderNodeId,
    key: NodeLayoutKey,
    renderer_generation: u64,
    renderer_cache_key: Option<u64>,
    theme: &Theme,
    history: &BlockHistory,
    inline_options: &InlineOptions,
) -> Option<(Buffer, usize)> {
    let layout = layout_cache.get(id, key, renderer_generation, renderer_cache_key)?;
    #[cfg(test)]
    record_full_layout_buffer_render();
    let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());
    let outcome = {
        let _perf = smelt_perf::perf::begin("transcript:layout_cache:render_full_to_buffer");
        render_block_into(
            &mut buf,
            layout,
            RenderCtx {
                width: key.width,
                view_state: layout_view_state(id, key.view_state, history),
                theme,
                history: Some(history),
                inline_options: inline_options.clone(),
            },
        )
    };
    Some((buf, outcome.line_count))
}

impl TranscriptProjection {
    pub(crate) fn new() -> Self {
        Self {
            transcript_scene: TranscriptScene::empty(),
            default_view_policy: TranscriptDefaultViewPolicy::default(),
            presentation: TranscriptPresentationState::default(),
            layout_cache: LayoutCache::new(),
            pending_changed_blocks: HashSet::new(),
            active_width: 0,
            visible: VisibleProjectionState::default(),
            measurements: MeasurementIndexStore::default(),
            projection_generation: 0,
            renderer_generation: None,
            renderer_cache_key: None,
            inline_options: InlineOptions::default(),
            display_layout_budget: 12 * 1024 * 1024,
            full_rows_budget: 2 * 1024 * 1024,
            #[cfg(test)]
            counters: TranscriptProjectionCounters::default(),
        }
    }

    fn refresh_transcript_scene(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &BlockHistory,
    ) {
        let policy = TranscriptDefaultViewPolicy::from_lua(lua);
        let policy_changed = policy != self.default_view_policy;
        if policy_changed {
            self.default_view_policy = policy;
            self.reset_layout_cache();
            self.measurements.clear();
            self.clear_visible_state();
            self.visible.full_rows = None;
            self.projection_generation = self.projection_generation.wrapping_add(1);
        }
        let group_generation = lua.transcript_group_generation();
        let group_cache_key = lua.transcript_group_cache_key();
        if policy_changed
            || self.transcript_scene.history_generation != history.generation()
            || self.transcript_scene.group_generation != group_generation
            || self.transcript_scene.group_cache_key != group_cache_key
        {
            let groups: Vec<_> = lua
                .transcript_group_specs()
                .into_iter()
                .filter(|spec| self.default_view_policy.group_enabled(&spec.name))
                .collect();
            let refresh = self.transcript_scene.refresh_for_history_with_groups(
                history,
                &groups,
                group_generation,
                group_cache_key,
            );
            if refresh.structure_changed {
                self.pending_changed_blocks.clear();
                self.layout_cache.retain_nodes(self.transcript_scene.ids());
            } else {
                for (block_id, appends) in &refresh.appended_ranges {
                    let Some(index) = self.transcript_scene.index_for_block(*block_id) else {
                        continue;
                    };
                    let Some(render_id) = (match self.transcript_scene.node(index) {
                        Some(RenderNode::Block { id, .. }) if id == block_id => {
                            Some(RenderNodeId::Block(*block_id))
                        }
                        Some(RenderNode::Group(group)) => Some(RenderNodeId::Group(group.id)),
                        _ => None,
                    }) else {
                        continue;
                    };
                    for append in appends {
                        let Some(content) = history.content_by_id(append.content_id) else {
                            continue;
                        };
                        self.layout_cache.apply_content_append(
                            render_id,
                            content,
                            std::slice::from_ref(&append.byte_range),
                        );
                    }
                }
                self.pending_changed_blocks.extend(refresh.changed_blocks);
            }
            if refresh.prune_required {
                self.presentation.prune(self.transcript_scene.ids());
            }
        }
    }

    pub(crate) fn inline_options(&self) -> &InlineOptions {
        &self.inline_options
    }

    fn reset_layout_cache(&mut self) {
        self.layout_cache = LayoutCache::new();
        self.layout_cache.set_budget(self.display_layout_budget);
    }

    pub(crate) fn set_memory_budget(&mut self, bytes: usize) {
        let auxiliary = bytes / 4;
        self.display_layout_budget = bytes.saturating_sub(auxiliary);
        self.layout_cache.set_budget(self.display_layout_budget);
        self.measurements.set_budget(auxiliary / 2);
        self.full_rows_budget = auxiliary.saturating_sub(auxiliary / 2);
        if self
            .visible
            .full_rows
            .as_ref()
            .is_some_and(|cached| cached_rows_retained_bytes(&cached.rows) > self.full_rows_budget)
        {
            self.visible.full_rows = None;
        }
    }

    pub(crate) fn memory_snapshot(&self) -> TranscriptRenderMemorySnapshot {
        let display = self.layout_cache.memory_snapshot();
        let height_index_bytes = self.measurements.active.retained_bytes();
        let height_index_cache_bytes = self.measurements.retained_bytes();
        let visible_rows_bytes = self
            .visible
            .block_layout
            .capacity()
            .saturating_mul(std::mem::size_of::<LayoutEntry>())
            .saturating_add(
                self.visible
                    .row_identities
                    .capacity()
                    .saturating_mul(std::mem::size_of::<ProjectedRowIdentity>()),
            );
        let full_rows_bytes = self
            .visible
            .full_rows
            .as_ref()
            .map_or(0, |cached| cached_rows_retained_bytes(&cached.rows));
        let retained_budgeted = display
            .layout_bytes
            .saturating_add(height_index_bytes)
            .saturating_add(height_index_cache_bytes)
            .saturating_add(visible_rows_bytes)
            .saturating_add(full_rows_bytes);
        let budget = self
            .display_layout_budget
            .saturating_add(self.measurements.budget)
            .saturating_add(self.full_rows_budget);
        TranscriptRenderMemorySnapshot {
            layout_bytes: display.layout_bytes,
            pinned_layout_bytes: display.pinned_layout_bytes,
            height_index_bytes,
            height_index_cache_bytes,
            visible_rows_bytes,
            full_rows_bytes,
            oversize_debt_bytes: retained_budgeted
                .saturating_sub(budget)
                .max(display.oversize_debt_bytes),
        }
    }

    pub(crate) fn set_inline_options(&mut self, options: InlineOptions) {
        if self.inline_options == options {
            return;
        }
        self.inline_options = options;
        self.reset_layout_cache();
        self.measurements.clear();
        self.clear_visible_state();
        self.visible.full_rows = None;
        self.projection_generation = self.projection_generation.wrapping_add(1);
    }

    pub(crate) fn projection_generation(&self) -> u64 {
        self.projection_generation
    }

    pub(crate) fn exact_height_snapshot(&self) -> TranscriptExactHeightSnapshot {
        self.measurements.active.exact_height_snapshot()
    }

    pub(crate) fn height_suffix_is_exact(&self, anchor: TranscriptRowAnchor) -> bool {
        self.measurements
            .active
            .node_index(anchor.id)
            .is_some_and(|index| {
                self.measurements.active.nodes[index..]
                    .iter()
                    .all(|node| node.exact_height.is_some())
            })
    }

    pub(crate) fn invalidate_renderer_if_changed(
        &mut self,
        generation: u64,
        cache_key: Option<u64>,
    ) -> bool {
        if self.renderer_generation == Some(generation) && self.renderer_cache_key == cache_key {
            return false;
        }
        let initialized = self.renderer_generation.is_some();
        self.renderer_generation = Some(generation);
        self.renderer_cache_key = cache_key;
        if !initialized {
            return false;
        }
        self.reset_layout_cache();
        self.measurements.clear();
        self.clear_visible_state();
        self.visible.full_rows = None;
        self.projection_generation = self.projection_generation.wrapping_add(1);
        true
    }

    /// Snapshot of the visibly laid-out blocks: `(BlockId, first_row, rows)`.
    /// Used by Lua's `smelt.transcript.visible_blocks()` to map block indices
    /// back to display rows without forcing full transcript materialization.
    pub(crate) fn visible_block_layout(
        &self,
    ) -> impl Iterator<Item = (BlockId, RowIndex, RowIndex)> + '_ {
        self.visible
            .block_layout
            .iter()
            .map(|e| (e.id, e.start, e.rows))
    }

    pub(crate) fn next_refresh_at(&self) -> Option<std::time::Instant> {
        self.layout_cache.next_refresh_at()
    }

    fn expire_due_refreshes(&mut self, now: std::time::Instant) {
        let due = self.layout_cache.expire_due_refreshes(now);
        if due.is_empty() {
            return;
        }
        let due = due.into_iter().collect::<HashSet<_>>();
        self.measurements
            .invalidate_nodes(&due, &self.transcript_scene);
        self.clear_visible_state();
        self.visible.full_rows = None;
        self.projection_generation = self.projection_generation.wrapping_add(1);
    }

    #[cfg(test)]
    pub(crate) fn layout_cache_len(&self) -> usize {
        self.layout_cache.len()
    }

    #[cfg(test)]
    pub(crate) fn counters(&self) -> TranscriptProjectionCounters {
        self.counters
    }

    #[cfg(test)]
    pub(crate) fn reset_counters(&mut self) {
        self.counters = TranscriptProjectionCounters::default();
    }

    fn finish_compile_jobs(&mut self, env: TranscriptRenderEnv<'_>, jobs: Vec<CompileJob>) {
        let compiled = jobs.len();
        self.layout_cache.compile_and_insert(env, jobs);
        if compiled > 0 {
            self.projection_generation = self.projection_generation.wrapping_add(1);
        }
        #[cfg(test)]
        {
            self.counters.layout_cache += compiled;
        }
        let _ = compiled;
    }

    fn refresh_measurement_node_key(&mut self, history: &BlockHistory, index: usize) {
        let Some(node_key) = self.transcript_scene.node_key(
            &self.default_view_policy,
            history,
            &self.presentation,
            index,
            base_layout_key(self.measurements.active.width),
        ) else {
            return;
        };
        let Some(node) = self.measurements.active.nodes.get_mut(index) else {
            return;
        };
        if node.key == node_key {
            return;
        }
        node.key = node_key;
        node.estimated_height =
            estimate_node_height(history, &self.transcript_scene, index, node_key);
        node.exact_height = None;
        self.measurements.active.mark_prefix_dirty_from(index);
    }

    fn pin_display_node_range(&mut self, range: std::ops::Range<usize>) {
        let ids = range.filter_map(|index| self.transcript_scene.node(index).map(RenderNode::id));
        self.layout_cache.set_pinned_nodes(ids);
    }

    fn ensure_node_indices(
        &mut self,
        env: TranscriptRenderEnv<'_>,
        history: &BlockHistory,
        indices: impl Iterator<Item = usize> + Clone,
    ) {
        for index in indices.clone() {
            self.refresh_measurement_node_key(history, index);
        }
        self.measurements.active.refresh_prefix_rows();
        let jobs = {
            let row_nodes = &self.measurements.active.nodes;
            let nodes = indices
                .filter_map(|index| {
                    row_nodes.get(index).and_then(|row| {
                        self.transcript_scene
                            .node(index)
                            .cloned()
                            .map(|node| (index, node, row.key))
                    })
                })
                .collect::<Vec<_>>();
            self.layout_cache.collect_compile_jobs(
                history,
                &self.default_view_policy,
                env.renderer_generation,
                env.renderer_cache_key,
                nodes,
            )
        };
        self.finish_compile_jobs(env, jobs);
    }

    fn clear_visible_state(&mut self) {
        self.visible.materialized = None;
        self.visible.block_layout.clear();
        self.visible.row_base = 0;
        self.visible.row_identities.clear();
        self.visible.total_rows = 0;
    }

    pub(crate) fn invalidate_source_sequence(&mut self) {
        self.transcript_scene = TranscriptScene::empty();
        self.measurements.clear();
        self.clear_visible_state();
        self.visible.full_rows = None;
        self.projection_generation = self.projection_generation.wrapping_add(1);
    }

    fn invalidate_presentation_projection(&mut self) {
        self.clear_visible_state();
        self.visible.full_rows = None;
        self.projection_generation = self.projection_generation.wrapping_add(1);
    }

    fn clear_width_dependent_state(&mut self) {
        self.measurements.remember_active();
        self.clear_visible_state();
        self.visible.full_rows = None;
        self.projection_generation = self.projection_generation.wrapping_add(1);
    }

    fn gc_if_stale(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &BlockHistory,
        width: u16,
    ) {
        self.refresh_transcript_scene(lua, history);
        if width != self.active_width {
            // Width changes invalidate row indexes and materialized rows, but
            // display layouts are width-independent and stay reusable.
            self.active_width = width;
            self.clear_width_dependent_state();
        }
    }

    /// Clear rendered visible state so the next projection repaints with the current theme.
    /// Display layouts and exact measurements are theme-independent.
    pub(crate) fn invalidate_theme(&mut self) {
        self.clear_visible_state();
    }

    fn target_has_projection(&self, key: ProjectKey, buf: &Buffer) -> bool {
        self.visible.materialized.is_some_and(|m| {
            m.key == key && m.buf_id == buf.id() && m.changedtick == buf.changedtick()
        })
    }

    fn last_project_key(&self) -> Option<ProjectKey> {
        self.visible.materialized.map(|m| m.key)
    }

    fn mark_projected_into(&mut self, key: ProjectKey, buf: &Buffer) {
        self.visible.materialized = Some(MaterializedProjection {
            key,
            buf_id: buf.id(),
            changedtick: buf.changedtick(),
        });
    }

    fn try_hydrate_row_index(
        active: &mut TranscriptHeightIndex,
        entries: &VecDeque<DisplayRowIndexEntry>,
        history: &BlockHistory,
        plan: &TranscriptScene,
        policy: &TranscriptDefaultViewPolicy,
        presentation: &TranscriptPresentationState,
        key: RowIndexKey,
    ) -> bool {
        if active.is_current(plan, key) {
            return true;
        }
        let Some(entry) = entries.iter().find(|entry| {
            entry.width == key.width
                && entry.renderer_generation == key.renderer_generation
                && entry.renderer_cache_key == key.renderer_cache_key
        }) else {
            smelt_perf::perf::record_value("transcript:row_index_cache:miss", 1);
            return false;
        };
        let hydrated = active.hydrate_from_cache(history, plan, policy, presentation, entry, key);
        smelt_perf::perf::record_value(
            if hydrated {
                "transcript:row_index_cache:hydrated"
            } else {
                "transcript:row_index_cache:hydrate_reject"
            },
            1,
        );
        hydrated
    }

    pub(crate) fn rebuild_row_index(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &mut BlockHistory,
        width: u16,
    ) {
        let env = TranscriptRenderEnv::with_inline_options(lua, self.inline_options.clone());
        self.rebuild_row_index_with_env(env, history, width);
    }

    fn prepare_row_index_with_env(
        &mut self,
        env: TranscriptRenderEnv<'_>,
        history: &mut BlockHistory,
        width: u16,
    ) -> RowIndexKey {
        let _perf = smelt_perf::perf::begin("transcript:prepare_row_index");
        smelt_perf::perf::record_value(
            "transcript:prepare_row_index:blocks",
            history.order.len() as u64,
        );
        smelt_perf::perf::record_value(
            "transcript:prepare_row_index:generation",
            history.generation(),
        );
        let renderer_generation = env.renderer_generation;
        let renderer_cache_key = env.renderer_cache_key;
        self.invalidate_renderer_if_changed(renderer_generation, renderer_cache_key);
        self.gc_if_stale(env.lua, history, width);
        self.expire_due_refreshes(env.refresh_now);
        let row_key = RowIndexKey::new(
            width,
            renderer_generation,
            renderer_cache_key,
            self.presentation.generation(),
        );
        let changed_blocks = std::mem::take(&mut self.pending_changed_blocks);
        let patched_index = !changed_blocks.is_empty()
            && self.measurements.active.apply_changed_blocks(
                history,
                &self.transcript_scene,
                &self.default_view_policy,
                &self.presentation,
                row_key,
                &changed_blocks,
            );
        let plan = &self.transcript_scene;
        let hydrated_index = !patched_index
            && Self::try_hydrate_row_index(
                &mut self.measurements.active,
                &self.measurements.entries,
                history,
                plan,
                &self.default_view_policy,
                &self.presentation,
                row_key,
            );
        let reused_index = patched_index
            || hydrated_index
            || self.measurements.active.try_reuse_or_extend(
                history,
                plan,
                &self.default_view_policy,
                &self.presentation,
                row_key,
            );
        smelt_perf::perf::record_value(
            "transcript:prepare_row_index:reused_index",
            u64::from(reused_index),
        );
        if !reused_index {
            let _perf = smelt_perf::perf::begin("transcript:prepare_row_index:rebuild_index");
            self.measurements.active.rebuild_if_stale(
                history,
                plan,
                &self.default_view_policy,
                &self.presentation,
                row_key,
            );
        } else {
            self.measurements.active.refresh_prefix_rows();
        }
        row_key
    }

    fn collect_missing_node_indices(&self, indices: impl IntoIterator<Item = usize>) -> Vec<usize> {
        indices
            .into_iter()
            .filter(|&index| {
                self.measurements
                    .active
                    .nodes
                    .get(index)
                    .is_some_and(|node| node.exact_height.is_none())
            })
            .collect()
    }

    fn missing_node_indices(&self) -> Vec<usize> {
        let _perf = smelt_perf::perf::begin("transcript:row_index:collect_missing");
        self.collect_missing_node_indices(0..self.measurements.active.nodes.len())
    }

    fn measure_node_indices(
        &mut self,
        history: &BlockHistory,
        renderer_generation: u64,
        renderer_cache_key: Option<u64>,
        indices: Vec<usize>,
    ) -> bool {
        if indices.is_empty() {
            return false;
        }
        smelt_perf::perf::record_value(
            "transcript:row_index:exactify_missing",
            indices.len() as u64,
        );
        let mut changed = false;
        for index in indices {
            changed |= self.measure_cached_layout_height(
                history,
                index,
                renderer_generation,
                renderer_cache_key,
            );
        }
        if changed {
            self.measurements.active.refresh_prefix_rows();
        }
        changed
    }

    fn exactify_node_indices(
        &mut self,
        env: TranscriptRenderEnv<'_>,
        history: &BlockHistory,
        indices: impl IntoIterator<Item = usize>,
    ) -> bool {
        let renderer_generation = env.renderer_generation;
        let renderer_cache_key = env.renderer_cache_key;
        let missing = self.collect_missing_node_indices(indices);
        if missing.is_empty() {
            return false;
        }
        self.ensure_node_indices(env, history, missing.iter().copied());
        self.measure_node_indices(history, renderer_generation, renderer_cache_key, missing)
    }

    fn exactify_node_range(
        &mut self,
        env: TranscriptRenderEnv<'_>,
        history: &BlockHistory,
        range: std::ops::Range<usize>,
    ) -> bool {
        let end = range.end.min(self.measurements.active.nodes.len());
        if range.start >= end {
            return false;
        }
        self.exactify_node_indices(env, history, range.start..end)
    }

    fn ensure_projectable_node_range(
        &mut self,
        env: TranscriptRenderEnv<'_>,
        history: &BlockHistory,
        range: std::ops::Range<usize>,
    ) -> bool {
        let end = range.end.min(self.measurements.active.nodes.len());
        if range.start >= end {
            return false;
        }
        let indices = range.start..end;
        let renderer_generation = env.renderer_generation;
        let renderer_cache_key = env.renderer_cache_key;
        self.ensure_node_indices(env, history, indices.clone());
        let missing = self.collect_missing_node_indices(indices);
        self.measure_node_indices(history, renderer_generation, renderer_cache_key, missing)
    }

    fn stable_row_delta_hydration_range(
        &self,
        anchor: ProjectionAnchor,
        delta: isize,
        viewport_rows: u16,
    ) -> Option<std::ops::Range<usize>> {
        let anchor_index = match anchor {
            ProjectionAnchor::Node { id, .. } => self.measurements.active.node_index(id),
            ProjectionAnchor::RenderedBlockRow { id, .. }
            | ProjectionAnchor::RenderedBlockDisplayOffset { id, .. } => {
                self.measurements.active.block_index(id)
            }
        }?;
        let distance = delta.unsigned_abs();
        let viewport_nodes = usize::from(viewport_rows.max(1));
        let (start, end) = if delta < 0 {
            (
                anchor_index.saturating_sub(distance),
                anchor_index.saturating_add(viewport_nodes),
            )
        } else {
            (
                anchor_index,
                anchor_index
                    .saturating_add(distance)
                    .saturating_add(viewport_nodes),
            )
        };
        Some(start..end.min(self.measurements.active.nodes.len()))
    }

    fn exactify_stable_row_delta_path(
        &mut self,
        env: TranscriptRenderEnv<'_>,
        history: &BlockHistory,
        anchor: ProjectionAnchor,
        delta: isize,
    ) {
        let anchor_index = match anchor {
            ProjectionAnchor::Node { id, .. } => self.measurements.active.node_index(id),
            ProjectionAnchor::RenderedBlockRow { id, .. }
            | ProjectionAnchor::RenderedBlockDisplayOffset { id, .. } => {
                self.measurements.active.block_index(id)
            }
        };
        let Some(anchor_index) = anchor_index else {
            return;
        };
        let distance = delta.unsigned_abs();
        let range = if delta < 0 {
            anchor_index.saturating_sub(distance)..anchor_index.saturating_add(1)
        } else {
            anchor_index
                ..anchor_index
                    .saturating_add(distance)
                    .saturating_add(1)
                    .min(self.measurements.active.nodes.len())
        };
        let _ = self.exactify_node_range(env, history, range);
    }

    fn exactify_row_range(
        &mut self,
        env: TranscriptRenderEnv<'_>,
        history: &BlockHistory,
        rows: std::ops::Range<RowIndex>,
    ) -> (std::ops::Range<usize>, bool) {
        if rows.start >= rows.end {
            return (0..0, false);
        }
        let mut changed_any = false;
        let mut node_range = self.measurements.active.node_range_for_rows(rows.clone());
        let pass_budget = self.measurements.active.nodes.len().saturating_add(1);
        for _ in 0..pass_budget {
            let changed = self.exactify_node_range(env.clone(), history, node_range.clone());
            changed_any |= changed;
            let total_rows = self.measurements.active.total_rows();
            let end = rows.end.min(total_rows);
            let refined = if rows.start < end {
                self.measurements
                    .active
                    .node_range_for_rows(rows.start..end)
            } else {
                0..0
            };
            if !changed && refined == node_range {
                return (refined, changed_any);
            }
            node_range = refined;
        }
        smelt_perf::perf::record_value("transcript:row_index:exactify_budget_exhausted", 1);
        (node_range, changed_any)
    }

    pub(crate) fn row_range_hydration_ids(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &mut BlockHistory,
        width: u16,
        rows: std::ops::Range<RowIndex>,
    ) -> Vec<BlockId> {
        let env = TranscriptRenderEnv::with_inline_options(lua, self.inline_options.clone());
        self.prepare_row_index_with_env(env, history, width);
        let mut ids = Vec::new();
        for index in self.measurements.active.node_range_for_rows(rows) {
            self.transcript_scene
                .push_block_ids_for_node(index, &mut ids);
        }
        ids.sort_unstable_by_key(|id| id.get());
        ids.dedup();
        ids
    }

    fn rebuild_row_index_with_env(
        &mut self,
        env: TranscriptRenderEnv<'_>,
        history: &mut BlockHistory,
        width: u16,
    ) {
        let _perf = smelt_perf::perf::begin("transcript:rebuild_row_index");
        let row_key = self.prepare_row_index_with_env(env.clone(), history, width);
        if self
            .measurements
            .active
            .is_exact_for(&self.transcript_scene, row_key)
        {
            self.measurements.remember_active();
            return;
        }

        let missing = self.missing_node_indices();
        self.layout_cache.set_pinned_nodes(
            missing
                .iter()
                .filter_map(|index| self.transcript_scene.node(*index).map(RenderNode::id)),
        );
        smelt_perf::perf::record_value(
            "transcript:rebuild_row_index:missing",
            missing.len() as u64,
        );
        if let (Some(first), Some(last)) = (missing.first(), missing.last()) {
            smelt_perf::perf::record_value(
                "transcript:rebuild_row_index:missing:first_index",
                *first as u64,
            );
            smelt_perf::perf::record_value(
                "transcript:rebuild_row_index:missing:last_index",
                *last as u64,
            );
        }
        self.exactify_node_indices(env, history, missing);
        self.measurements.remember_active();
    }

    fn measure_cached_layout_height(
        &mut self,
        history: &BlockHistory,
        index: usize,
        renderer_generation: u64,
        renderer_cache_key: Option<u64>,
    ) -> bool {
        let Some(node) = self.measurements.active.nodes.get(index) else {
            return false;
        };
        if node.exact_height.is_some() {
            return true;
        }
        let id = node.id;
        let key = node.key;
        let Some(rows) = self.layout_cache.measure_height(
            id,
            key,
            renderer_generation,
            renderer_cache_key,
            MeasureCtx {
                width: key.width,
                view_state: layout_view_state(id, key.view_state, history),
                inline_options: self.inline_options.clone(),
            },
        ) else {
            return false;
        };
        let rows = rows as RowIndex;
        let gap = self
            .transcript_scene
            .rendered_node_gap(history, index, rows as usize) as RowIndex;
        self.set_exact_height(index, gap.saturating_add(rows));
        true
    }

    fn set_exact_height(&mut self, index: usize, rows: RowIndex) {
        let measured = self.measurements.active.set_exact_height(index, rows);
        if measured {
            self.projection_generation = self.projection_generation.wrapping_add(1);
        }
        #[cfg(test)]
        if measured {
            self.counters.exact_height_measured_blocks += 1;
        }
    }

    fn exact_block_layout(&self, history: &BlockHistory) -> Vec<LayoutEntry> {
        let mut layout = Vec::with_capacity(self.measurements.active.nodes.len());
        let mut running_total: RowIndex = 0;
        for (i, measured_node) in self.measurements.active.nodes.iter().enumerate() {
            debug_assert!(
                measured_node.exact_height.is_some(),
                "exact block layout requested before height measurement"
            );
            let Some(exact_height) = measured_node.exact_height else {
                continue;
            };
            let gap = (self
                .transcript_scene
                .rendered_node_gap(history, i, exact_height as usize)
                as RowIndex)
                .min(exact_height);
            running_total = running_total.saturating_add(gap);
            let start = running_total;
            let rows = exact_height.saturating_sub(gap);
            match self.transcript_scene.node(i) {
                Some(RenderNode::Block { id, .. }) => layout.push(LayoutEntry {
                    id: *id,
                    start,
                    rows,
                }),
                Some(RenderNode::Group(group)) => {
                    layout.extend(group.child_ids().map(|id| LayoutEntry { id, start, rows }));
                }
                None => {}
            }
            running_total = running_total.saturating_add(rows);
        }
        layout
    }

    fn search_layout(&self, generation: u64, history: &BlockHistory) -> TranscriptSearchLayout {
        let indices = 0..self.measurements.active.nodes.len();
        self.search_layout_for_node_indices(generation, history, indices)
    }

    fn search_layout_for_node_indices(
        &self,
        generation: u64,
        history: &BlockHistory,
        indices: impl IntoIterator<Item = usize>,
    ) -> TranscriptSearchLayout {
        let mut entries = Vec::new();
        for i in indices {
            let Some(measured_node) = self.measurements.active.nodes.get(i) else {
                continue;
            };
            let height = measured_node.measured_or_estimated_height();
            let gap = (self
                .transcript_scene
                .rendered_node_gap(history, i, height as usize) as RowIndex)
                .min(height);
            let first_row = self.measurements.active.prefix_row(i).saturating_add(gap);
            let rows = height.saturating_sub(gap);
            let mut block_ids = Vec::new();
            self.transcript_scene
                .push_block_ids_for_node(i, &mut block_ids);
            if !block_ids.is_empty() {
                entries.push(TranscriptSearchLayoutEntry {
                    block_ids,
                    first_row,
                    rows,
                });
            }
        }
        TranscriptSearchLayout {
            generation,
            entries,
        }
    }

    pub(crate) fn prepare_layout(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &mut BlockHistory,
        width: u16,
    ) {
        let env = TranscriptRenderEnv::with_inline_options(lua, self.inline_options.clone());
        self.prepare_row_index_with_env(env, history, width);
    }

    pub(crate) fn estimated_total_rows(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &mut BlockHistory,
        width: u16,
    ) -> RowIndex {
        self.prepare_layout(lua, history, width);
        self.measurements.active.total_rows()
    }

    #[cfg(test)]
    pub(crate) fn exact_total_rows(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &mut BlockHistory,
        width: u16,
    ) -> RowIndex {
        self.rebuild_row_index(lua, history, width);
        self.measurements.active.total_rows()
    }

    pub(crate) fn node_at_row(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &mut BlockHistory,
        width: u16,
        row: RowIndex,
    ) -> Option<TranscriptNodeRow> {
        let env = TranscriptRenderEnv::with_inline_options(lua, self.inline_options.clone());
        self.prepare_row_index_with_env(env.clone(), history, width);
        self.exactify_row_range(env.clone(), history, row..row.saturating_add(1));
        self.node_at_prepared_row(history, row)
    }

    fn node_at_prepared_row(
        &self,
        history: &BlockHistory,
        row: RowIndex,
    ) -> Option<TranscriptNodeRow> {
        let index = self.measurements.active.node_index_at_row(row)?;
        self.node_at_prepared_index(history, index, row)
    }

    fn node_at_prepared_index(
        &self,
        history: &BlockHistory,
        index: usize,
        row: RowIndex,
    ) -> Option<TranscriptNodeRow> {
        let node = self.measurements.active.nodes.get(index)?;
        let first_row = self.measurements.active.prefix_row(index);
        let rows = node.exact_height.unwrap_or(node.estimated_height);
        let row_offset = row.saturating_sub(first_row).min(rows.saturating_sub(1));
        let view_state = self.presentation.effective_view_state(
            &self.default_view_policy,
            &self.transcript_scene,
            history,
            index,
        )?;
        let explicit_fold_target = is_explicit_fold_target(view_state, row_offset, rows);
        Some(TranscriptNodeRow {
            id: node.id,
            index,
            first_row,
            rows,
            row_offset,
            view_state,
            explicit_fold_target,
        })
    }

    pub(crate) fn fold_node_at_row(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &mut BlockHistory,
        width: u16,
        request: FoldAtRow,
    ) -> bool {
        let Some(node) = self.node_at_row(lua, history, width, request.row) else {
            return false;
        };
        if !node.can_activate(request.activation) {
            return false;
        }
        self.fold_node(history, node.id, request.action)
    }

    pub(crate) fn fold_node(
        &mut self,
        history: &BlockHistory,
        id: RenderNodeId,
        action: FoldAction,
    ) -> bool {
        let changed = match action {
            FoldAction::Toggle => self.presentation.toggle(
                &self.default_view_policy,
                &self.transcript_scene,
                history,
                id,
            ),
            FoldAction::Open => self.presentation.set(
                &self.default_view_policy,
                &self.transcript_scene,
                history,
                id,
                ViewState::Expanded,
            ),
            FoldAction::Peek => self.presentation.set(
                &self.default_view_policy,
                &self.transcript_scene,
                history,
                id,
                ViewState::Peek,
            ),
            FoldAction::Close => self.presentation.set(
                &self.default_view_policy,
                &self.transcript_scene,
                history,
                id,
                ViewState::Collapsed,
            ),
        };
        if changed {
            self.invalidate_presentation_projection();
        }
        changed
    }

    pub(crate) fn fold_all(&mut self, history: &BlockHistory, action: FoldAction) -> bool {
        let view_state = match action {
            FoldAction::Open => ViewState::Expanded,
            FoldAction::Peek => ViewState::Peek,
            FoldAction::Close => ViewState::Collapsed,
            FoldAction::Toggle => return false,
        };
        let changed = self.presentation.set_all(
            &self.default_view_policy,
            &self.transcript_scene,
            history,
            view_state,
        );
        if changed {
            self.invalidate_presentation_projection();
        }
        changed
    }

    pub(crate) fn fold_block_kind(
        &mut self,
        history: &BlockHistory,
        kind: &str,
        action: FoldAction,
    ) -> bool {
        let targets: Vec<_> = self
            .transcript_scene
            .nodes
            .iter()
            .enumerate()
            .filter_map(|(index, node)| match node {
                RenderNode::Block { id, .. } => history
                    .block_kind(*id)
                    .filter(|block_kind| *block_kind == kind)
                    .map(|_| (index, RenderNodeId::Block(*id))),
                RenderNode::Group(_) => None,
            })
            .collect();
        if targets.is_empty() {
            return false;
        }
        let view_state = match action {
            FoldAction::Open => ViewState::Expanded,
            FoldAction::Peek => ViewState::Peek,
            FoldAction::Close => ViewState::Collapsed,
            FoldAction::Toggle => {
                let any_folded = targets.iter().any(|(index, _)| {
                    !matches!(
                        self.presentation.effective_view_state(
                            &self.default_view_policy,
                            &self.transcript_scene,
                            history,
                            *index,
                        ),
                        Some(ViewState::Expanded)
                    )
                });
                if any_folded {
                    ViewState::Expanded
                } else {
                    ViewState::Collapsed
                }
            }
        };
        let mut changed = false;
        for (_, id) in targets {
            changed |= self.presentation.set(
                &self.default_view_policy,
                &self.transcript_scene,
                history,
                id,
                view_state,
            );
        }
        if changed {
            self.invalidate_presentation_projection();
        }
        changed
    }

    pub(crate) fn node_metadata_at_row(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &mut BlockHistory,
        width: u16,
        row: RowIndex,
    ) -> Option<TranscriptNodeRow> {
        self.node_at_row(lua, history, width, row)
    }

    pub(crate) fn row_anchor_at_row(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &mut BlockHistory,
        width: u16,
        row: RowIndex,
    ) -> Option<TranscriptRowAnchor> {
        self.node_at_row(lua, history, width, row)
            .map(|node| TranscriptRowAnchor {
                id: node.id,
                row_offset: node.row_offset,
            })
    }

    pub(crate) fn materialized_row_anchor(&self, row: RowIndex) -> Option<TranscriptRowAnchor> {
        let identity = row
            .checked_sub(self.visible.row_base)
            .and_then(|local| usize::try_from(local).ok())
            .and_then(|local| self.visible.row_identities.get(local))?;
        match identity.exact {
            ProjectionAnchor::Node { id, row_offset } => {
                Some(TranscriptRowAnchor { id, row_offset })
            }
            ProjectionAnchor::RenderedBlockRow { .. }
            | ProjectionAnchor::RenderedBlockDisplayOffset { .. } => None,
        }
    }

    fn stable_materialized_row_anchor(&self, row: RowIndex) -> Option<StableRowAnchor> {
        row.checked_sub(self.visible.row_base)
            .and_then(|local| usize::try_from(local).ok())
            .and_then(|local| self.visible.row_identities.get(local))
            .map(|identity| StableRowAnchor(identity.content.unwrap_or(identity.exact)))
    }

    pub(crate) fn representative_block_id_for_node_index(&self, index: usize) -> Option<BlockId> {
        self.transcript_scene
            .representative_block_id_for_node(index)
    }

    pub(crate) fn representative_block_id_for_row_anchor(
        &self,
        anchor: TranscriptRowAnchor,
    ) -> Option<BlockId> {
        let index = self.measurements.active.node_index(anchor.id)?;
        self.transcript_scene
            .representative_block_id_for_node(index)
    }

    pub(crate) fn row_for_anchor(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &mut BlockHistory,
        width: u16,
        anchor: TranscriptRowAnchor,
    ) -> Option<RowIndex> {
        self.prepare_layout(lua, history, width);
        self.measurements
            .active
            .row_for_node_anchor(anchor.id, anchor.row_offset)
    }

    pub(crate) fn row_for_stable_anchor(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &mut BlockHistory,
        width: u16,
        theme: &Theme,
        anchor: StableRowAnchor,
    ) -> Option<RowIndex> {
        self.prepare_layout(lua, history, width);
        self.scroll_top_for_anchor(history, theme, anchor.0)
    }

    pub(crate) fn stable_anchor_is_present(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &mut BlockHistory,
        width: u16,
        anchor: StableRowAnchor,
    ) -> bool {
        self.prepare_layout(lua, history, width);
        match anchor.0 {
            ProjectionAnchor::Node { id, .. } => self.measurements.active.node_index(id).is_some(),
            ProjectionAnchor::RenderedBlockRow { id, .. }
            | ProjectionAnchor::RenderedBlockDisplayOffset { id, .. } => {
                self.measurements.active.block_index(id).is_some()
            }
        }
    }

    pub(crate) fn row_anchor_for_block(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &mut BlockHistory,
        width: u16,
        block_id: BlockId,
        row_offset: RowIndex,
    ) -> Option<TranscriptRowAnchor> {
        self.exact_block_row_target(lua, history, width, block_id, row_offset)
            .map(|(anchor, _)| anchor)
    }

    pub(crate) fn row_anchor_for_block_node_row(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &mut BlockHistory,
        width: u16,
        block_id: BlockId,
        row_offset: RowIndex,
    ) -> Option<TranscriptRowAnchor> {
        self.prepare_layout(lua, history, width);
        let index = self.transcript_scene.index_for_block(block_id)?;
        let id = self.transcript_scene.node(index)?.id();
        Some(TranscriptRowAnchor { id, row_offset })
    }

    pub(crate) fn block_row_offset_for_anchor(
        &self,
        history: &BlockHistory,
        anchor: TranscriptRowAnchor,
    ) -> Option<RowIndex> {
        anchor.id.as_block_id()?;
        let Some(index) = self.measurements.active.node_index(anchor.id) else {
            return Some(anchor.row_offset);
        };
        let Some(exact_height) = self
            .measurements
            .active
            .nodes
            .get(index)
            .and_then(|node| node.exact_height)
        else {
            return Some(anchor.row_offset);
        };
        let gap = (self
            .transcript_scene
            .rendered_node_gap(history, index, exact_height as usize)
            as RowIndex)
            .min(exact_height);
        anchor.row_offset.checked_sub(gap)
    }

    pub(crate) fn exact_block_row_target(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &mut BlockHistory,
        width: u16,
        block_id: BlockId,
        row_offset: RowIndex,
    ) -> Option<(TranscriptRowAnchor, RowIndex)> {
        let env = TranscriptRenderEnv::with_inline_options(lua, self.inline_options.clone());
        self.prepare_row_index_with_env(env.clone(), history, width);
        let index = self.transcript_scene.index_for_block(block_id)?;
        self.pin_display_node_range(index..index.saturating_add(1));
        let _ = self.exactify_node_range(env, history, index..index.saturating_add(1));
        self.measurements.remember_active();

        let target = (|| {
            let node = self.measurements.active.nodes.get(index)?;
            let exact_height = node.exact_height?;
            let gap =
                (self
                    .transcript_scene
                    .rendered_node_gap(history, index, exact_height as usize)
                    as RowIndex)
                    .min(exact_height);
            let block_rows = exact_height.saturating_sub(gap);
            let block_row_offset = row_offset.min(block_rows.saturating_sub(1));
            let node_row_offset = gap.saturating_add(block_row_offset);
            let target_row = self
                .measurements
                .active
                .prefix_row(index)
                .saturating_add(node_row_offset);
            Some((
                TranscriptRowAnchor {
                    id: node.id,
                    row_offset: node_row_offset,
                },
                target_row,
            ))
        })();
        self.layout_cache.set_pinned_nodes(std::iter::empty());
        target
    }

    fn display_offset_for_node_row(
        &mut self,
        history: &BlockHistory,
        theme: &Theme,
        id: RenderNodeId,
        row_offset: RowIndex,
    ) -> Option<usize> {
        if row_offset == 0 {
            return Some(0);
        }
        let index = self
            .measurements
            .active
            .nodes
            .iter()
            .position(|node| node.id == id)?;
        let node = self.measurements.active.nodes.get(index)?;
        let key = node.key;
        let renderer_generation = self.measurements.active.renderer_generation;
        let renderer_cache_key = self.measurements.active.renderer_cache_key;
        let (block_buf, rendered_rows) = render_cached_layout_to_buffer(
            &self.layout_cache,
            id,
            key,
            renderer_generation,
            renderer_cache_key,
            theme,
            history,
            &self.inline_options,
        )?;
        Some(display_offset_for_buffer_row(
            &block_buf,
            rendered_rows,
            row_offset,
        ))
    }

    fn anchor_with_display_offset(
        &mut self,
        history: &BlockHistory,
        theme: &Theme,
        anchor: ProjectionAnchor,
    ) -> ProjectionAnchor {
        match anchor {
            ProjectionAnchor::RenderedBlockRow { id, row_offset } => {
                match self.display_offset_for_node_row(
                    history,
                    theme,
                    RenderNodeId::Block(id),
                    row_offset,
                ) {
                    Some(display_offset) => ProjectionAnchor::RenderedBlockDisplayOffset {
                        id,
                        row_offset,
                        display_offset,
                    },
                    None => ProjectionAnchor::RenderedBlockRow { id, row_offset },
                }
            }
            anchor => anchor,
        }
    }

    fn stable_row_anchor_for(
        &mut self,
        history: &BlockHistory,
        theme: &Theme,
        row: RowIndex,
    ) -> Option<ProjectionAnchor> {
        let anchor = row
            .checked_sub(self.visible.row_base)
            .and_then(|local| usize::try_from(local).ok())
            .and_then(|local| self.visible.row_identities.get(local))
            .and_then(|identity| identity.content)
            .or_else(|| self.measurements.active.scroll_anchor_at_row(row))?;
        Some(self.anchor_with_display_offset(history, theme, anchor))
    }

    fn exact_row_anchor_for(&self, row: RowIndex) -> Option<ProjectionAnchor> {
        row.checked_sub(self.visible.row_base)
            .and_then(|local| usize::try_from(local).ok())
            .and_then(|local| self.visible.row_identities.get(local))
            .map(|identity| identity.exact)
            .or_else(|| self.measurements.active.scroll_anchor_at_row(row))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn projection_hydration_ids(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &mut BlockHistory,
        width: u16,
        theme: &Theme,
        scroll_target: ScrollTarget,
        viewport_rows: u16,
    ) -> (Vec<BlockId>, ProjectionPlan) {
        let viewport_rows = viewport_rows.max(1);
        let env = TranscriptRenderEnv::with_inline_options(lua, self.inline_options.clone());
        let anchor = match scroll_target {
            ScrollTarget::Visible(ScrollAnchor::ReflowStableRow(row)) => {
                self.stable_row_anchor_for(history, theme, row)
            }
            ScrollTarget::Visible(ScrollAnchor::StableRowDelta { row, anchor, .. }) => anchor
                .map(|anchor| anchor.0)
                .or_else(|| self.exact_row_anchor_for(row)),
            ScrollTarget::Visible(ScrollAnchor::ExactRow(_) | ScrollAnchor::Tail) => None,
        };
        self.prepare_row_index_with_env(env.clone(), history, width);
        let request = ProjectionRequest {
            key: self.project_key(
                width,
                env.renderer_generation,
                env.renderer_cache_key,
                scroll_target,
                viewport_rows,
            ),
            target: ResolvedProjectionTarget {
                requested: scroll_target,
                anchor,
            },
            viewport_rows,
        };
        let plan = self.plan_projection_from_prepared(history, theme, &request);
        let mut ids = self.projection_hydration_ids_for_plan(&plan);
        if let (Some(anchor), ScrollTarget::Visible(ScrollAnchor::StableRowDelta { delta, .. })) =
            (anchor, scroll_target)
        {
            if let Some(range) = self.stable_row_delta_hydration_range(anchor, delta, viewport_rows)
            {
                for index in range {
                    self.transcript_scene
                        .push_block_ids_for_node(index, &mut ids);
                }
            }
        }
        ids.sort_unstable_by_key(|id| id.get());
        ids.dedup();
        (ids, plan)
    }

    pub(crate) fn projection_hydration_ids_for_plan(&self, plan: &ProjectionPlan) -> Vec<BlockId> {
        let mut ids = Vec::new();
        for index in plan.node_range() {
            self.transcript_scene
                .push_block_ids_for_node(index, &mut ids);
        }
        if let Some(
            ProjectionAnchor::RenderedBlockRow { id, .. }
            | ProjectionAnchor::RenderedBlockDisplayOffset { id, .. },
        ) = plan.target.anchor
        {
            ids.push(id);
        }
        ids.sort_unstable_by_key(|id| id.get());
        ids.dedup();
        ids
    }

    pub(crate) fn refine_projection_plan(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &mut BlockHistory,
        theme: &Theme,
        plan: ProjectionPlan,
    ) -> ProjectionPlan {
        let env = TranscriptRenderEnv::with_inline_options(lua, self.inline_options.clone());
        if self.transcript_scene.history_generation != history.generation()
            || self.transcript_scene.group_generation != lua.transcript_group_generation()
            || self.transcript_scene.group_cache_key != lua.transcript_group_cache_key()
            || self.transcript_scene.revision != plan.key.generation
            || env.renderer_generation != plan.key.renderer_generation
            || env.renderer_cache_key != plan.key.renderer_cache_key
            || self.presentation.generation() != plan.key.presentation_generation
            || self.projection_generation != plan.key.row_generation
        {
            self.prepare_row_index_with_env(env.clone(), history, plan.key.width);
        }
        let request = ProjectionRequest {
            key: self.project_key(
                plan.key.width,
                env.renderer_generation,
                env.renderer_cache_key,
                plan.target.requested,
                plan.viewport_rows,
            ),
            target: plan.target,
            viewport_rows: plan.viewport_rows,
        };
        self.plan_projection_progressive(env, history, theme, request)
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn plan_projection_measured(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &mut BlockHistory,
        width: u16,
        theme: &Theme,
        scroll_target: ScrollTarget,
        viewport_rows: u16,
    ) -> ProjectionPlan {
        let _perf = smelt_perf::perf::begin("transcript:plan_projection_measured");
        let viewport_rows = viewport_rows.max(1);
        let env = TranscriptRenderEnv::with_inline_options(lua, self.inline_options.clone());
        let anchor = match scroll_target {
            ScrollTarget::Visible(ScrollAnchor::ReflowStableRow(row)) => {
                self.stable_row_anchor_for(history, theme, row)
            }
            ScrollTarget::Visible(ScrollAnchor::StableRowDelta { row, anchor, .. }) => anchor
                .map(|anchor| anchor.0)
                .or_else(|| self.exact_row_anchor_for(row)),
            ScrollTarget::Visible(ScrollAnchor::ExactRow(_) | ScrollAnchor::Tail) => None,
        };
        self.prepare_row_index_with_env(env.clone(), history, width);
        let key = self.project_key(
            width,
            env.renderer_generation,
            env.renderer_cache_key,
            scroll_target,
            viewport_rows,
        );
        self.plan_projection_progressive(
            env,
            history,
            theme,
            ProjectionRequest {
                key,
                target: ResolvedProjectionTarget {
                    requested: scroll_target,
                    anchor,
                },
                viewport_rows,
            },
        )
    }

    fn project_key(
        &self,
        width: u16,
        renderer_generation: u64,
        renderer_cache_key: Option<u64>,
        scroll_target: ScrollTarget,
        viewport_rows: u16,
    ) -> ProjectKey {
        ProjectKey {
            generation: self.transcript_scene.revision,
            width,
            renderer_generation,
            renderer_cache_key,
            presentation_generation: self.presentation.generation(),
            row_generation: self.projection_generation,
            mode: scroll_target.mode(viewport_rows),
        }
    }

    fn plan_projection_progressive(
        &mut self,
        env: TranscriptRenderEnv<'_>,
        history: &BlockHistory,
        theme: &Theme,
        mut request: ProjectionRequest,
    ) -> ProjectionPlan {
        if let (Some(anchor), ScrollTarget::Visible(ScrollAnchor::StableRowDelta { delta, .. })) =
            (request.target.anchor, request.target.requested)
        {
            self.exactify_stable_row_delta_path(env.clone(), history, anchor, delta);
        }
        if let Some(
            ProjectionAnchor::RenderedBlockRow { id, .. }
            | ProjectionAnchor::RenderedBlockDisplayOffset { id, .. },
        ) = request.target.anchor
        {
            if let Some(index) = self.measurements.active.block_index(id) {
                let _ =
                    self.exactify_node_range(env.clone(), history, index..index.saturating_add(1));
            }
        }
        let mut plan = self.plan_projection_from_prepared(history, theme, &request);
        let pass_budget = self.measurements.active.nodes.len().saturating_add(1);
        for _ in 0..pass_budget {
            #[cfg(test)]
            {
                self.counters.projection_planning_passes =
                    self.counters.projection_planning_passes.saturating_add(1);
            }
            self.pin_display_node_range(plan.node_range());
            let changed =
                self.ensure_projectable_node_range(env.clone(), history, plan.node_range());
            request.key = self.project_key(
                request.key.width,
                env.renderer_generation,
                env.renderer_cache_key,
                request.target.requested,
                request.viewport_rows,
            );
            let refined = self.plan_projection_from_prepared(history, theme, &request);
            if !changed
                && refined.node_range == plan.node_range
                && refined.scroll_top == plan.scroll_top
            {
                return refined;
            }
            plan = refined;
        }
        smelt_perf::perf::record_value("transcript:projection:planning_budget_exhausted", 1);
        plan
    }

    fn scroll_top_for_anchor(
        &self,
        history: &BlockHistory,
        theme: &Theme,
        anchor: ProjectionAnchor,
    ) -> Option<RowIndex> {
        match anchor {
            ProjectionAnchor::Node { id, row_offset } => {
                self.measurements.active.row_for_node_anchor(id, row_offset)
            }
            ProjectionAnchor::RenderedBlockRow { id, row_offset } => {
                let index = self.measurements.active.block_index(id)?;
                let node = self.measurements.active.nodes.get(index)?;
                let exact_height = node.exact_height?;
                let gap =
                    (self
                        .transcript_scene
                        .rendered_node_gap(history, index, exact_height as usize)
                        as RowIndex)
                        .min(exact_height);
                let block_rows = exact_height.saturating_sub(gap);
                Some(
                    self.measurements
                        .active
                        .prefix_row(index)
                        .saturating_add(gap)
                        .saturating_add(row_offset.min(block_rows.saturating_sub(1))),
                )
            }
            ProjectionAnchor::RenderedBlockDisplayOffset {
                id,
                row_offset,
                display_offset,
            } => {
                let index = self.measurements.active.block_index(id)?;
                let node = self.measurements.active.nodes.get(index)?;
                let exact_height = node.exact_height?;
                let gap =
                    (self
                        .transcript_scene
                        .rendered_node_gap(history, index, exact_height as usize)
                        as RowIndex)
                        .min(exact_height);
                let block_rows = exact_height.saturating_sub(gap);
                let offset = {
                    let (block_buf, rendered_rows) = render_cached_layout_to_buffer(
                        &self.layout_cache,
                        node.id,
                        node.key,
                        self.measurements.active.renderer_generation,
                        self.measurements.active.renderer_cache_key,
                        theme,
                        history,
                        &self.inline_options,
                    )?;
                    row_offset_for_display_offset(
                        &block_buf,
                        rendered_rows,
                        display_offset,
                        row_offset,
                    )
                };
                Some(
                    self.measurements
                        .active
                        .prefix_row(index)
                        .saturating_add(gap)
                        .saturating_add(offset.min(block_rows.saturating_sub(1))),
                )
            }
        }
    }

    fn plan_projection_from_prepared(
        &self,
        history: &BlockHistory,
        theme: &Theme,
        request: &ProjectionRequest,
    ) -> ProjectionPlan {
        let total_rows = self.measurements.active.total_rows();
        let requested_scroll_top = request
            .target
            .anchor
            .and_then(|anchor| self.scroll_top_for_anchor(history, theme, anchor))
            .map(|row| match request.target.requested.anchor() {
                ScrollAnchor::StableRowDelta { delta, .. } => add_signed_row(row, delta),
                ScrollAnchor::ExactRow(_)
                | ScrollAnchor::ReflowStableRow(_)
                | ScrollAnchor::Tail => row,
            })
            .unwrap_or_else(|| request.target.requested.as_scroll_top());
        let scroll_top = clamp_scroll(requested_scroll_top, total_rows, request.viewport_rows);
        let visible_rows = request.viewport_rows.max(1) as RowIndex;
        let viewport_end = scroll_top.saturating_add(visible_rows).min(total_rows);
        // Exact row heights make the visible window precise; keep half a viewport
        // preloaded so nearby scrolls can reuse the materialized buffer.
        let preload_rows = visible_rows / 2;
        let row_window = match request.target.requested {
            ScrollTarget::Visible(
                ScrollAnchor::ExactRow(_)
                | ScrollAnchor::ReflowStableRow(_)
                | ScrollAnchor::StableRowDelta { .. },
            ) => {
                let start = scroll_top.saturating_sub(preload_rows);
                let end = viewport_end.saturating_add(preload_rows).min(total_rows);
                start..end
            }
            ScrollTarget::Visible(ScrollAnchor::Tail) => {
                let start = scroll_top.saturating_sub(preload_rows);
                start..total_rows
            }
        };
        let node_range = self
            .measurements
            .active
            .node_range_for_rows(row_window.clone());
        ProjectionPlan {
            key: request.key,
            target: request.target,
            scroll_top,
            total_rows,
            viewport_rows: request.viewport_rows,
            row_window,
            node_range,
        }
    }

    pub(crate) fn exact_row_tape_handle(
        &self,
        rows: MaterializedRows,
    ) -> Option<ExactRowTapeHandle> {
        let key = self.last_project_key()?;
        let materialized_rows = self.visible.row_identities.len() as RowIndex;
        if rows.row_base != self.visible.row_base
            || rows.total_rows != self.visible.total_rows
            || rows.materialized_rows != materialized_rows
        {
            return None;
        }
        Some(ExactRowTapeHandle { key, rows })
    }

    fn resolve_exact_row_tape(
        &self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &BlockHistory,
        handle: ExactRowTapeHandle,
        width: u16,
        viewport_rows: u16,
    ) -> Option<ExactRowTapeState> {
        let viewport_rows = viewport_rows.max(1);
        let key = self.last_project_key()?;
        let env = TranscriptRenderEnv::with_inline_options(lua, self.inline_options.clone());
        let materialized_rows = self.visible.row_identities.len() as RowIndex;
        if key != handle.key
            || key.generation != self.transcript_scene.revision
            || key.width != width
            || key.renderer_generation != env.renderer_generation
            || key.renderer_cache_key != env.renderer_cache_key
            || key.presentation_generation != self.presentation.generation()
            || key.row_generation != self.projection_generation
            || key.mode != (ProjectionMode::Visible { viewport_rows })
            || self.transcript_scene.history_generation != history.generation()
            || self.transcript_scene.group_generation != lua.transcript_group_generation()
            || self.transcript_scene.group_cache_key != lua.transcript_group_cache_key()
            || handle.rows.row_base != self.visible.row_base
            || handle.rows.total_rows != self.visible.total_rows
            || handle.rows.materialized_rows != materialized_rows
        {
            return None;
        }
        Some(ExactRowTapeState {
            rows: handle.rows,
            top_anchor: self.stable_materialized_row_anchor(handle.rows.clamped_scroll),
        })
    }

    pub(crate) fn exact_row_tape_state(
        &self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &BlockHistory,
        handle: ExactRowTapeHandle,
        width: u16,
        viewport_rows: u16,
    ) -> Option<ExactRowTapeState> {
        self.resolve_exact_row_tape(lua, history, handle, width, viewport_rows)
    }

    pub(crate) fn retarget_exact_row_tape(
        &self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &BlockHistory,
        handle: ExactRowTapeHandle,
        width: u16,
        viewport_rows: u16,
        scroll_top: RowIndex,
    ) -> Option<ExactRowTapeHandle> {
        let mut state = self.resolve_exact_row_tape(lua, history, handle, width, viewport_rows)?;
        if !state.rows.covers_viewport(scroll_top, viewport_rows) {
            return None;
        }
        state.rows.clamped_scroll = scroll_top;
        Some(ExactRowTapeHandle {
            key: handle.key,
            rows: state.rows,
        })
    }

    pub(crate) fn exact_row_tape_matches_buffer(
        &self,
        handle: ExactRowTapeHandle,
        buf: &Buffer,
    ) -> bool {
        self.target_has_projection(handle.key, buf)
    }

    pub(crate) fn plan_exact_row_tape_scroll(
        &self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &BlockHistory,
        handle: ExactRowTapeHandle,
        width: u16,
        delta: isize,
        viewport_rows: u16,
    ) -> Option<ExactRowTapeProjection> {
        let viewport_rows = viewport_rows.max(1);
        let state = self.resolve_exact_row_tape(lua, history, handle, width, viewport_rows)?;
        let current_scroll = state.rows.clamped_scroll;
        let target = add_signed_row(current_scroll, delta);
        let delta_rows = RowIndex::try_from(delta.unsigned_abs()).ok()?;
        if current_scroll.abs_diff(target) != delta_rows {
            return None;
        }
        let max_scroll = self
            .visible
            .total_rows
            .saturating_sub(RowIndex::from(viewport_rows));
        if target > max_scroll {
            return None;
        }
        let target = clamp_scroll(target, self.visible.total_rows, viewport_rows);
        let materialized_rows = self.visible.row_identities.len() as RowIndex;
        let materialized_end = self.visible.row_base.saturating_add(materialized_rows);
        let viewport_end = target.saturating_add(RowIndex::from(viewport_rows));
        if target < self.visible.row_base || viewport_end > materialized_end {
            return None;
        }

        Some(ExactRowTapeProjection {
            key: handle.key,
            rows: MaterializedRows {
                clamped_scroll: target,
                row_base: self.visible.row_base,
                total_rows: self.visible.total_rows,
                materialized_rows,
            },
            top_anchor: self.stable_materialized_row_anchor(target),
        })
    }

    pub(crate) fn apply_exact_row_tape_scroll(
        &self,
        buf: &Buffer,
        plan: ExactRowTapeProjection,
    ) -> Option<MaterializedRows> {
        self.target_has_projection(plan.key, buf)
            .then_some(plan.rows)
    }

    /// Render a bounded row window into `buf`.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn project(
        &mut self,
        buf: &mut Buffer,
        history: &mut BlockHistory,
        width: u16,
        theme: &Theme,
        scroll_target: ScrollTarget,
        viewport_rows: u16,
    ) -> MaterializedRows {
        let lua = smelt_core::lua::runtime::LuaRuntime::new();
        let plan = self.plan_projection_measured(
            &lua,
            history,
            width,
            theme,
            scroll_target,
            viewport_rows,
        );
        self.project_planned(&lua, buf, history, theme, plan)
    }

    pub(crate) fn project_planned(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        buf: &mut Buffer,
        history: &mut BlockHistory,
        theme: &Theme,
        plan: ProjectionPlan,
    ) -> MaterializedRows {
        let _perf = smelt_perf::perf::begin("transcript:project_planned");
        let mut plan = plan;
        let current_env =
            TranscriptRenderEnv::with_inline_options(lua, self.inline_options.clone());
        if self.transcript_scene.history_generation != history.generation()
            || self.transcript_scene.group_generation != lua.transcript_group_generation()
            || self.transcript_scene.group_cache_key != lua.transcript_group_cache_key()
            || self.transcript_scene.revision != plan.key.generation
            || current_env.renderer_generation != plan.key.renderer_generation
            || current_env.renderer_cache_key != plan.key.renderer_cache_key
            || self.presentation.generation() != plan.key.presentation_generation
            || self.projection_generation != plan.key.row_generation
        {
            self.prepare_row_index_with_env(current_env.clone(), history, plan.key.width);
            let key = self.project_key(
                plan.key.width,
                current_env.renderer_generation,
                current_env.renderer_cache_key,
                plan.target.requested,
                plan.viewport_rows,
            );
            plan = self.plan_projection_progressive(
                current_env.clone(),
                history,
                theme,
                ProjectionRequest {
                    key,
                    target: plan.target,
                    viewport_rows: plan.viewport_rows,
                },
            );
        }

        let row = plan.scroll_top;
        if let Some(out) =
            self.reuse_visible_projection_for_row(buf, plan.key, row, plan.viewport_rows)
        {
            return out;
        }

        match plan.target.requested {
            ScrollTarget::Visible(_) => {
                let out = self.project_visible_range(lua, buf, history, theme, &plan);
                debug_assert_materialized_viewport(out, plan.viewport_rows);
                out
            }
        }
    }

    fn reuse_visible_projection_for_row(
        &self,
        buf: &Buffer,
        key: ProjectKey,
        row: RowIndex,
        viewport_rows: u16,
    ) -> Option<MaterializedRows> {
        let prev = self.last_project_key()?;
        if prev.generation != key.generation
            || prev.width != key.width
            || prev.renderer_generation != key.renderer_generation
            || prev.renderer_cache_key != key.renderer_cache_key
            || prev.presentation_generation != key.presentation_generation
            || prev.row_generation != key.row_generation
            || prev.mode != key.mode
        {
            return None;
        }

        if !self.target_has_projection(prev, buf) {
            return None;
        }

        let viewport_rows = viewport_rows.max(1);
        let total_rows = self.visible.total_rows;
        let clamped_scroll = clamp_scroll(row, total_rows, viewport_rows);
        let materialized_rows = self.visible.row_identities.len() as RowIndex;
        let materialized_end = self.visible.row_base.saturating_add(materialized_rows);
        let viewport_end = clamped_scroll
            .saturating_add(RowIndex::from(viewport_rows))
            .min(total_rows);
        if clamped_scroll >= self.visible.row_base && viewport_end <= materialized_end {
            return Some(MaterializedRows {
                clamped_scroll,
                row_base: self.visible.row_base,
                total_rows,
                materialized_rows,
            });
        }
        None
    }

    fn project_visible_range(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        buf: &mut Buffer,
        history: &mut BlockHistory,
        theme: &Theme,
        plan: &ProjectionPlan,
    ) -> MaterializedRows {
        let _perf = smelt_perf::perf::begin("transcript:project_visible_range");
        smelt_perf::perf::record_value(
            "transcript:project_visible_range:blocks",
            plan.node_range.len() as u64,
        );
        let rebuild = buf.begin_rendered_lines_rebuild();
        let layout = std::mem::take(&mut self.visible.block_layout);
        let row_identities = std::mem::take(&mut self.visible.row_identities);
        let materialized = self.collect_nodes_range(
            TranscriptRenderEnv::with_renderer(
                lua,
                plan.key.renderer_generation,
                plan.key.renderer_cache_key,
            ),
            history,
            theme,
            plan.node_range(),
            Some(plan.row_window()),
            rebuild,
            layout,
            row_identities,
        );
        let row_base = materialized.row_base;
        let total_rows = materialized.total_rows;
        let materialized_rows = materialized.rebuild.lines.len() as RowIndex;
        {
            let _perf = smelt_perf::perf::begin("transcript:project_visible_range:buffer_install");
            buf.finish_rendered_lines_rebuild(materialized.rebuild);
        }
        self.visible.block_layout = materialized.layout;
        self.visible.row_identities = materialized.row_identities;
        self.visible.row_base = row_base;
        self.visible.total_rows = total_rows;
        self.mark_projected_into(plan.key, buf);
        debug_assert!(total_rows >= row_base);
        debug_assert!(row_base.saturating_add(materialized_rows) <= total_rows);
        let clamped_scroll = clamp_scroll(plan.scroll_top, total_rows, plan.viewport_rows);
        MaterializedRows {
            clamped_scroll,
            row_base,
            total_rows,
            materialized_rows,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn collect_nodes_range(
        &mut self,
        env: TranscriptRenderEnv<'_>,
        history: &BlockHistory,
        theme: &Theme,
        node_range: std::ops::Range<usize>,
        row_clip: Option<std::ops::Range<RowIndex>>,
        mut rebuild: RenderedBufferRebuild,
        mut layout: Vec<LayoutEntry>,
        mut row_identities: Vec<ProjectedRowIdentity>,
    ) -> MaterializedTranscriptRange {
        let _perf = smelt_perf::perf::begin("transcript:collect_nodes_range");
        let start = node_range.start.min(self.measurements.active.nodes.len());
        let end = node_range.end.min(self.measurements.active.nodes.len());
        smelt_perf::perf::record_value(
            "transcript:collect_nodes_range:blocks",
            end.saturating_sub(start) as u64,
        );
        #[cfg(test)]
        {
            let count = end.saturating_sub(start);
            self.counters.range_materialized_blocks += count;
            self.counters.max_range_materialized_blocks =
                self.counters.max_range_materialized_blocks.max(count);
        }
        let row_base = row_clip
            .as_ref()
            .map(|range| range.start)
            .unwrap_or_else(|| self.measurements.active.prefix_row(start));
        layout.clear();
        layout.reserve(end.saturating_sub(start));
        row_identities.clear();
        {
            let mut rows = ProjectRows {
                texts: &mut rebuild.lines,
                text_cursor: 0,
                metadata: &mut rebuild.metadata,
                layout: &mut layout,
                row_identities: &mut row_identities,
            };

            let block_indices = start..end;
            let _ = self.exactify_node_indices(env, history, block_indices.clone());
            for block_index in block_indices {
                let id = self.measurements.active.nodes[block_index].id;
                let key = self.measurements.active.nodes[block_index].key;
                self.append_projected_node(
                    history,
                    theme,
                    block_index,
                    id,
                    key,
                    row_clip.as_ref(),
                    &mut rows,
                );
            }
            rows.finish_texts();
        }

        debug_assert_eq!(rebuild.lines.len(), row_identities.len());
        smelt_perf::perf::record_value(
            "transcript:collect_nodes_range:rows",
            rebuild.lines.len() as u64,
        );
        #[cfg(test)]
        {
            let count = rebuild.lines.len();
            self.counters.range_materialized_rows += count;
            self.counters.max_range_materialized_rows =
                self.counters.max_range_materialized_rows.max(count);
        }
        self.measurements.active.refresh_prefix_rows();
        MaterializedTranscriptRange {
            row_base,
            total_rows: self.measurements.active.total_rows(),
            rebuild,
            layout,
            row_identities,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn append_projected_node(
        &mut self,
        history: &BlockHistory,
        theme: &Theme,
        block_index: usize,
        id: RenderNodeId,
        key: NodeLayoutKey,
        row_clip: Option<&std::ops::Range<RowIndex>>,
        rows: &mut ProjectRows<'_>,
    ) {
        let renderer_generation = self.measurements.active.renderer_generation;
        let renderer_cache_key = self.measurements.active.renderer_cache_key;
        let node_start = self.measurements.active.prefix_row(block_index);
        let exact_rows = self
            .measurements
            .active
            .nodes
            .get(block_index)
            .and_then(|node| node.exact_height);
        debug_assert!(
            exact_rows.is_some(),
            "projected node was not measured before rendering"
        );
        let Some(full_rows) = exact_rows else {
            smelt_perf::perf::record_value("transcript:projected_node:missing_measurement", 1);
            return;
        };
        let gap = self
            .transcript_scene
            .rendered_node_gap(history, block_index, full_rows as usize)
            as RowIndex;
        self.set_exact_height(block_index, full_rows);

        let node_end = node_start.saturating_add(full_rows);
        let clip_start = row_clip
            .map_or(node_start, |range| range.start)
            .max(node_start);
        let clip_end = row_clip.map_or(node_end, |range| range.end).min(node_end);
        if clip_start >= clip_end {
            return;
        }

        let gap_end = node_start.saturating_add(gap).min(node_end);
        let gap_start = clip_start.max(node_start);
        let gap_clip_end = clip_end.min(gap_end);
        for row in gap_start..gap_clip_end {
            rows.push_text("");
            rows.metadata
                .push_default_decoration(rows.text_len().saturating_sub(1));
            rows.row_identities.push(ProjectedRowIdentity {
                exact: ProjectionAnchor::Node {
                    id,
                    row_offset: row.saturating_sub(node_start),
                },
                content: None,
            });
        }

        let block_id = id.as_block_id();
        let block_start = gap_end;
        let block_rows = full_rows.saturating_sub(gap);
        let block_end = block_start.saturating_add(block_rows);
        let visible_start = clip_start.max(block_start);
        let visible_end = clip_end.min(block_end);
        if visible_start < visible_end {
            let local_row_start = visible_start.saturating_sub(block_start);
            let local_row_count = visible_end.saturating_sub(visible_start);
            let inline_options = self.inline_options.clone();
            let rendered = self.layout_cache.with_rendered_range(
                id,
                key,
                renderer_generation,
                renderer_cache_key,
                RenderCtx {
                    width: key.width,
                    view_state: layout_view_state(id, key.view_state, history),
                    theme,
                    history: Some(history),
                    inline_options,
                },
                row_to_usize(local_row_start),
                row_to_usize(local_row_count),
                |node_buf, rendered_rows| {
                    let _perf = smelt_perf::perf::begin(
                        "transcript:project_visible_range:clone_display_rows",
                    );
                    for r in 0..rendered_rows {
                        let row_idx = rows.text_len();
                        rows.push_text(node_buf.get_line(r).unwrap_or(""));
                        node_buf.append_rendered_row_metadata(r, row_idx, rows.metadata);
                        rows.row_identities.push(ProjectedRowIdentity {
                            exact: ProjectionAnchor::Node {
                                id,
                                row_offset: gap
                                    .saturating_add(local_row_start)
                                    .saturating_add(r as RowIndex),
                            },
                            content: block_id.map(|id| {
                                ProjectionAnchor::rendered_block_row(
                                    id,
                                    local_row_start.saturating_add(r as RowIndex),
                                )
                            }),
                        });
                    }
                },
            );
            if rendered.is_none() {
                return;
            }
        }
        if let Some(block_id) = block_id {
            rows.layout.push(LayoutEntry {
                id: block_id,
                start: block_start,
                rows: block_rows,
            });
        }
    }

    /// Exact full block layout for snapshot/navigation APIs. This may measure every
    /// transcript block, but it does not concatenate display rows and does not
    /// re-render blocks when the exact height index is already current.
    pub(crate) fn materialize_block_layout(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &mut BlockHistory,
        width: u16,
    ) -> Vec<(BlockId, RowIndex, RowIndex)> {
        #[cfg(test)]
        {
            self.counters.full_layout_materializations =
                self.counters.full_layout_materializations.saturating_add(1);
        }
        let node_count = self.transcript_scene.len();
        self.pin_display_node_range(0..node_count);
        self.rebuild_row_index(lua, history, width);
        let layout = self
            .exact_block_layout(history)
            .into_iter()
            .map(|e| (e.id, e.start, e.rows))
            .collect();
        self.layout_cache.set_pinned_nodes(std::iter::empty());
        layout
    }

    pub(crate) fn materialize_search_layout(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &mut BlockHistory,
        width: u16,
    ) -> TranscriptSearchLayout {
        let env = TranscriptRenderEnv::with_inline_options(lua, self.inline_options.clone());
        self.prepare_row_index_with_env(env, history, width);
        let generation = self
            .measurements
            .active
            .search_layout_hash(self.projection_generation);
        self.search_layout(generation, history)
    }

    pub(crate) fn materialize_search_layout_for_blocks(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &mut BlockHistory,
        width: u16,
        block_indices: &[u64],
    ) -> TranscriptSearchLayout {
        let env = TranscriptRenderEnv::with_inline_options(lua, self.inline_options.clone());
        self.prepare_row_index_with_env(env.clone(), history, width);
        let mut seen = HashSet::new();
        let indices = block_indices
            .iter()
            .filter_map(|idx| self.transcript_scene.index_for_block(BlockId::new(*idx)))
            .filter(|index| seen.insert(*index))
            .collect::<Vec<_>>();
        self.layout_cache.set_pinned_nodes(
            indices
                .iter()
                .filter_map(|index| self.transcript_scene.node(*index).map(RenderNode::id)),
        );
        let _ = self.exactify_node_indices(env, history, indices.clone());
        self.measurements.remember_active();
        let generation = self
            .measurements
            .active
            .search_layout_hash(self.projection_generation);
        self.search_layout_for_node_indices(generation, history, indices)
    }

    pub(crate) fn block_id_at_or_before_row(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &mut BlockHistory,
        width: u16,
        row: RowIndex,
        forward: bool,
    ) -> Option<BlockId> {
        let node = self.node_at_row(lua, history, width, row)?;
        match self.transcript_scene.node(node.index)? {
            RenderNode::Block { id, .. } => Some(*id),
            RenderNode::Group(group) if forward => group.children.first().map(|child| child.id),
            RenderNode::Group(group) => group.children.last().map(|child| child.id),
        }
    }

    /// Render the full transcript history into a regular buffer. This is for
    /// small transcript-shaped surfaces outside the main transcript viewport
    /// (for example a streaming `/btw` dialog) that still want the exact same
    /// block parser, markdown layout, and highlights.
    pub(crate) fn project_all(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        buf: &mut Buffer,
        history: &mut BlockHistory,
        width: u16,
        theme: &Theme,
    ) {
        self.rebuild_row_index(lua, history, width);
        let materialized = self.collect_nodes_range(
            TranscriptRenderEnv::with_renderer(
                lua,
                self.measurements.active.renderer_generation,
                self.measurements.active.renderer_cache_key,
            ),
            history,
            theme,
            0..self.measurements.active.nodes.len(),
            None,
            RenderedBufferRebuild::default(),
            Vec::new(),
            Vec::new(),
        );
        buf.finish_rendered_lines_rebuild(materialized.rebuild);
    }

    /// Full display rows. Cached by transcript, renderer, width, and presentation;
    /// callers get a free `Arc::clone`.
    pub(crate) fn build_rows(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &mut BlockHistory,
        width: u16,
        theme: &Theme,
    ) -> Arc<Vec<String>> {
        let _perf = smelt_perf::perf::begin("transcript:build_rows");
        let env = TranscriptRenderEnv::with_inline_options(lua, self.inline_options.clone());
        let renderer_generation = env.renderer_generation;
        let renderer_cache_key = env.renderer_cache_key;
        self.invalidate_renderer_if_changed(renderer_generation, renderer_cache_key);
        self.gc_if_stale(env.lua, history, width);
        self.expire_due_refreshes(env.refresh_now);
        let gen = self.transcript_scene.revision;
        if let Some(c) = &self.visible.full_rows {
            if c.generation == gen
                && c.renderer_generation == renderer_generation
                && c.renderer_cache_key == renderer_cache_key
                && c.presentation_generation == self.presentation.generation()
                && c.width == width
            {
                return Arc::clone(&c.rows);
            }
        }
        #[cfg(test)]
        {
            self.counters.full_row_builds += 1;
        }
        smelt_perf::perf::record_value("transcript:build_rows:blocks", history.order.len() as u64);
        let row_key = RowIndexKey::new(
            width,
            renderer_generation,
            renderer_cache_key,
            self.presentation.generation(),
        );
        let changed_blocks = std::mem::take(&mut self.pending_changed_blocks);
        let patched_index = !changed_blocks.is_empty()
            && self.measurements.active.apply_changed_blocks(
                history,
                &self.transcript_scene,
                &self.default_view_policy,
                &self.presentation,
                row_key,
                &changed_blocks,
            );
        let plan = &self.transcript_scene;
        let hydrated_index = !patched_index
            && Self::try_hydrate_row_index(
                &mut self.measurements.active,
                &self.measurements.entries,
                history,
                plan,
                &self.default_view_policy,
                &self.presentation,
                row_key,
            );
        let reused_index = patched_index
            || hydrated_index
            || self.measurements.active.try_reuse_or_extend(
                history,
                plan,
                &self.default_view_policy,
                &self.presentation,
                row_key,
            );
        if !reused_index {
            self.measurements.active.rebuild_if_stale(
                history,
                plan,
                &self.default_view_policy,
                &self.presentation,
                row_key,
            );
        }
        let mut rows: Vec<String> = Vec::new();
        let block_indices = 0..self.measurements.active.nodes.len();
        self.ensure_node_indices(env, history, block_indices.clone());
        for i in block_indices {
            let Some(node) = self.measurements.active.nodes.get(i) else {
                continue;
            };
            let id = node.id;
            let bkey = node.key;
            let Some((block_buf, block_rows)) = render_cached_layout_to_buffer(
                &self.layout_cache,
                id,
                bkey,
                renderer_generation,
                renderer_cache_key,
                theme,
                history,
                &self.inline_options,
            ) else {
                continue;
            };
            let gap = self
                .transcript_scene
                .rendered_node_gap(history, i, block_rows);
            self.set_exact_height(i, (gap as usize).saturating_add(block_rows) as RowIndex);
            for _ in 0..gap {
                rows.push(String::new());
            }
            for r in 0..block_rows {
                rows.push(block_buf.get_line(r).unwrap_or("").to_string());
            }
        }
        self.measurements.active.refresh_prefix_rows();
        self.measurements.remember_active();
        let rows = Arc::new(rows);
        let retained = cached_rows_retained_bytes(&rows);
        if retained <= self.full_rows_budget {
            self.visible.full_rows = Some(CachedRows {
                rows: Arc::clone(&rows),
                generation: gen,
                renderer_generation,
                renderer_cache_key,
                presentation_generation: self.presentation.generation(),
                width,
            });
        } else {
            self.visible.full_rows = None;
        }
        smelt_perf::perf::record_value(
            "transcript:full_rows_cache:retained_bytes",
            self.visible
                .full_rows
                .as_ref()
                .map_or(0, |cached| cached_rows_retained_bytes(&cached.rows)) as u64,
        );
        smelt_perf::perf::record_value(
            "transcript:full_rows_cache:oversize_transient_bytes",
            retained.saturating_sub(self.full_rows_budget) as u64,
        );
        rows
    }

    pub(crate) fn display_rows_for_range(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &mut BlockHistory,
        width: u16,
        theme: &Theme,
        rows: std::ops::Range<RowIndex>,
    ) -> DisplayRows {
        let _perf = smelt_perf::perf::begin("transcript:display_rows_for_range");
        let mut start = rows.start;
        let mut end = rows.end;
        let count = end.saturating_sub(start);
        smelt_perf::perf::record_value("transcript:display_rows_for_range:rows", count);
        smelt_perf::perf::record_value("transcript:exactified_rows", count);
        if count == 0 || end <= start {
            return DisplayRows::empty();
        }

        let env = TranscriptRenderEnv::with_inline_options(lua, self.inline_options.clone());
        self.prepare_row_index_with_env(env.clone(), history, width);
        let _ = self.exactify_row_range(env.clone(), history, start..end);
        let total_rows = self.measurements.active.total_rows();
        if total_rows == 0 {
            return DisplayRows::empty();
        }
        if start >= total_rows {
            start = total_rows.saturating_sub(count);
            end = total_rows;
        } else {
            end = end.min(total_rows);
        }
        let node_range = self.measurements.active.node_range_for_rows(start..end);
        if node_range.start >= node_range.end {
            return DisplayRows::empty();
        }

        let materialized = self.collect_nodes_range(
            TranscriptRenderEnv::with_renderer(
                lua,
                self.measurements.active.renderer_generation,
                self.measurements.active.renderer_cache_key,
            ),
            history,
            theme,
            node_range,
            Some(start..end),
            RenderedBufferRebuild::default(),
            Vec::new(),
            Vec::new(),
        );
        let local_start = row_to_usize(start.saturating_sub(materialized.row_base));
        let local_end = row_to_usize(end.saturating_sub(materialized.row_base))
            .min(materialized.rebuild.lines.len());
        if local_start >= local_end {
            return DisplayRows::empty();
        }
        let texts = &materialized.rebuild.lines;
        let metadata = &materialized.rebuild.metadata;
        let mut soft_wrapped = vec![false; texts.len()];
        let mut actions = vec![Vec::new(); texts.len()];
        let mut selectable_ranges: Vec<Vec<std::ops::Range<usize>>> = texts
            .iter()
            .map(|row| {
                if row.is_empty() {
                    Vec::new()
                } else {
                    std::iter::once(0..row.len()).collect()
                }
            })
            .collect();
        for (row_index, row) in texts.iter().enumerate() {
            let highlights = metadata.highlights_at(row_index);
            soft_wrapped[row_index] = metadata
                .decoration_at(row_index)
                .is_some_and(|decoration| decoration.soft_wrapped);
            actions[row_index] = crate::smelt_edit::display_actions_for_spans(&highlights);
            selectable_ranges[row_index] =
                crate::smelt_edit::selectable_byte_ranges_for_line(row, &highlights);
        }
        let rows = texts[local_start..local_end]
            .iter()
            .cloned()
            .zip(selectable_ranges[local_start..local_end].iter().cloned())
            .zip(actions[local_start..local_end].iter().cloned())
            .enumerate()
            .map(|(offset, ((text, selectable_ranges), actions))| {
                let row = DisplayRow::new(text, selectable_ranges).with_actions(actions);
                if offset == 0 {
                    row
                } else if soft_wrapped[local_start + offset] {
                    row.with_break_before(RowBreak::Soft)
                } else {
                    row.with_break_before(RowBreak::Hard)
                }
            })
            .collect();
        DisplayRows { rows }
    }

    pub(crate) fn copy_range(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &mut BlockHistory,
        width: u16,
        theme: &Theme,
        range: DocRange,
    ) -> CopyOutput {
        let _perf = smelt_perf::perf::begin("transcript:copy_range");
        if (range.start.row, range.start.byte_col) >= (range.end.row, range.end.byte_col) {
            return CopyOutput::default();
        }
        let env = TranscriptRenderEnv::with_inline_options(lua, self.inline_options.clone());
        self.prepare_row_index_with_env(env.clone(), history, width);
        let requested_end_row = range.end.row.saturating_add(1);
        smelt_perf::perf::record_value(
            "transcript:exactified_rows",
            requested_end_row.saturating_sub(range.start.row),
        );
        let _ = self.exactify_row_range(env.clone(), history, range.start.row..requested_end_row);
        let total_rows = self.measurements.active.total_rows();
        if total_rows == 0 || range.start.row >= total_rows {
            return CopyOutput::default();
        }
        let end_row = range.end.row.min(total_rows.saturating_sub(1));
        let node_range = self
            .measurements
            .active
            .node_range_for_rows(range.start.row..end_row.saturating_add(1));
        if node_range.start >= node_range.end {
            return CopyOutput::default();
        }

        let mut copied = CopyRangeAccumulator::new(range.start.row, end_row);
        let renderer_env = TranscriptRenderEnv::with_renderer(
            lua,
            self.measurements.active.renderer_generation,
            self.measurements.active.renderer_cache_key,
        );
        let mut start = node_range.start;
        while start < node_range.end {
            let end = start.saturating_add(COPY_CHUNK_NODES).min(node_range.end);
            let chunk_start_row = self
                .measurements
                .active
                .prefix_row(start)
                .max(range.start.row);
            let chunk_end_row = self
                .measurements
                .active
                .prefix_row(end)
                .min(end_row.saturating_add(1));
            let materialized = self.collect_nodes_range(
                renderer_env.clone(),
                history,
                theme,
                start..end,
                Some(chunk_start_row..chunk_end_row),
                RenderedBufferRebuild::default(),
                Vec::new(),
                Vec::new(),
            );
            append_copy_chunk(&mut copied, materialized, range, end_row);
            start = end;
        }
        CopyOutput::same(copied.finish())
    }
}

fn append_copy_chunk(
    copied: &mut CopyRangeAccumulator,
    materialized: MaterializedTranscriptRange,
    range: DocRange,
    end_row: RowIndex,
) {
    let row_count = materialized.rebuild.lines.len();
    if row_count == 0 {
        return;
    }
    let row_base = materialized.row_base;
    let chunk_end_row = row_base.saturating_add(row_count as RowIndex - 1);
    let start_row = range.start.row.max(row_base);
    let end_row = end_row.min(chunk_end_row);
    if start_row > end_row {
        return;
    }

    let highlights = (0..row_count)
        .map(|row| materialized.rebuild.metadata.highlights_at(row))
        .collect::<Vec<_>>();
    let decorations = (0..row_count)
        .map(|row| {
            materialized
                .rebuild
                .metadata
                .decoration_at(row)
                .cloned()
                .unwrap_or_default()
        })
        .collect::<Vec<_>>();

    let start_local = row_to_usize(start_row.saturating_sub(row_base));
    let end_local = row_to_usize(end_row.saturating_sub(row_base));
    for local in start_local..=end_local {
        let abs_row = row_base.saturating_add(local as RowIndex);
        let Some(text) = materialized.rebuild.lines.get(local) else {
            continue;
        };
        let start_col = if abs_row == range.start.row {
            range.start.byte_col
        } else {
            0
        };
        let end_col = if abs_row == range.end.row {
            range.end.byte_col
        } else {
            text.len()
        };
        let start_byte = smelt_buffer::text::snap_grapheme(text, start_col.min(text.len()));
        let end_byte = smelt_buffer::text::snap_grapheme(text, end_col.min(text.len()));
        let cell_start = smelt_buffer::text::byte_to_cell(text, start_byte);
        let cell_end = smelt_buffer::text::byte_to_cell(text, end_byte);
        let highlight_row = highlights.get(local).map(Vec::as_slice).unwrap_or(&[]);
        let Some(decoration) = decorations.get(local) else {
            continue;
        };
        copied.push_row(
            abs_row,
            CopyRow {
                text,
                highlights: highlight_row,
                decoration,
            },
            cell_start,
            cell_end,
        );
    }
}

fn debug_assert_materialized_viewport(rows: MaterializedRows, viewport_rows: u16) {
    if viewport_rows == 0 {
        debug_assert!(
            rows.clamped_scroll <= rows.total_rows,
            "empty viewport scroll {} exceeds total rows {}",
            rows.clamped_scroll,
            rows.total_rows
        );
        return;
    }
    let viewport_end = rows
        .clamped_scroll
        .saturating_add(viewport_rows as RowIndex)
        .min(rows.total_rows);
    debug_assert!(
        rows.clamped_scroll >= rows.row_base,
        "materialized range starts at {}, after viewport start {}",
        rows.row_base,
        rows.clamped_scroll
    );
    debug_assert!(
        viewport_end <= rows.row_base.saturating_add(rows.materialized_rows),
        "materialized range {}..{} does not cover viewport {}..{}",
        rows.row_base,
        rows.row_base.saturating_add(rows.materialized_rows),
        rows.clamped_scroll,
        viewport_end
    );
}

/// Yank transform for the transcript. Both destinations use logical copy text
/// so render-only wraps and non-selectable chrome never leak into a paste.
pub(crate) struct TranscriptCopier;

impl smelt_core::buffer::BufferCopy for TranscriptCopier {
    fn copy(
        &self,
        buf: &Buffer,
        _src: &str,
        range: std::ops::Range<usize>,
    ) -> smelt_core::buffer::CopyOutput {
        smelt_core::buffer::CopyOutput::same(copy_byte_range(buf, range.start, range.end))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use smelt_core::content::stream_parser::{StreamParser, ToolStart};
    use smelt_core::content::transcript::Transcript;
    use smelt_core::transcript_model::{
        Block, BlockHistory, BlockId, ToolOutput, ToolState, ToolStatus,
    };

    static NEXT_INVOCATION_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

    #[test]
    fn document_copy_ranges_never_split_grapheme_clusters() {
        let materialized = |text: String| MaterializedTranscriptRange {
            row_base: 0,
            total_rows: 1,
            rebuild: RenderedBufferRebuild {
                lines: vec![text],
                ..RenderedBufferRebuild::default()
            },
            layout: Vec::new(),
            row_identities: Vec::new(),
        };

        for grapheme in ["e\u{301}", "👩\u{200d}💻", "9\u{fe0f}", "🇨🇦"] {
            let text = format!("a{grapheme}b");
            let inside = 1 + grapheme.chars().next().unwrap().len_utf8();

            let mut head = CopyRangeAccumulator::new(0, 0);
            append_copy_chunk(
                &mut head,
                materialized(text.clone()),
                DocRange {
                    start: crate::smelt_edit::DocPosition {
                        row: 0,
                        byte_col: 0,
                    },
                    end: crate::smelt_edit::DocPosition {
                        row: 0,
                        byte_col: inside,
                    },
                },
                0,
            );
            assert_eq!(head.finish(), "a", "{grapheme:?}");

            let mut tail = CopyRangeAccumulator::new(0, 0);
            append_copy_chunk(
                &mut tail,
                materialized(text.clone()),
                DocRange {
                    start: crate::smelt_edit::DocPosition {
                        row: 0,
                        byte_col: inside,
                    },
                    end: crate::smelt_edit::DocPosition {
                        row: 0,
                        byte_col: text.len(),
                    },
                },
                0,
            );
            assert_eq!(tail.finish(), format!("{grapheme}b"));
        }
    }

    fn next_invocation_id() -> protocol::InvocationId {
        protocol::InvocationId::new(
            NEXT_INVOCATION_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        )
    }

    fn test_lua() -> smelt_core::lua::runtime::LuaRuntime {
        smelt_core::lua::runtime::LuaRuntime::new()
    }

    #[test]
    fn measurement_index_cache_evicts_oldest_entry_with_tracked_bytes() {
        fn entry(width: u16, block_id: u64) -> DisplayRowIndexEntry {
            DisplayRowIndexEntry {
                width,
                renderer_generation: 1,
                renderer_cache_key: None,
                nodes: vec![DisplayRowIndexNode {
                    id: RenderNodeId::Block(BlockId::new(block_id)),
                    key: LayoutKey {
                        width,
                        view_state: ViewState::Expanded,
                        content_hash: block_id,
                        sidecar_hash: 0,
                    },
                    exact_height: 1,
                }],
            }
        }

        let first = entry(80, 1);
        let second = entry(100, 2);
        let mut store = MeasurementIndexStore::default();
        store.set_budget(second.retained_bytes());
        store.retained_bytes =
            upsert_row_index_entry(&mut store.entries, first, store.retained_bytes);
        store.retained_bytes =
            upsert_row_index_entry(&mut store.entries, second, store.retained_bytes);
        store.enforce_budget();

        assert_eq!(store.entries.len(), 1);
        assert_eq!(store.entries[0].width, 100);
        assert_eq!(store.retained_bytes(), store.entries[0].retained_bytes());
    }

    #[test]
    fn timed_node_invalidation_uses_scene_index_and_clears_width_snapshots() {
        let lua = test_lua();
        let mut transcript = Transcript::new();
        transcript.push(Block::Text {
            content: "before".into(),
        });
        transcript.push(Block::Text {
            content: "refreshing".into(),
        });
        let refreshing_id = transcript.history.order[1];
        let mut projection = TranscriptProjection::new();
        projection.prepare_layout(&lua, &mut transcript.history, 80);

        for node in &mut projection.measurements.active.nodes {
            node.exact_height = Some(1);
        }
        projection.measurements.active.rebuild_prefix_rows();
        let cached = projection
            .measurements
            .active
            .cache_entry()
            .expect("complete width snapshot");
        projection.measurements.retained_bytes = cached.retained_bytes();
        projection.measurements.entries.push_back(cached);

        projection.measurements.invalidate_nodes(
            &HashSet::from([RenderNodeId::Block(refreshing_id)]),
            &projection.transcript_scene,
        );

        assert_eq!(
            projection.measurements.active.nodes[0].exact_height,
            Some(1)
        );
        assert_eq!(projection.measurements.active.nodes[1].exact_height, None);
        assert_eq!(projection.measurements.active.prefix_dirty_from, Some(1));
        assert!(projection.measurements.entries.is_empty());
        assert_eq!(projection.measurements.retained_bytes, 0);
    }

    #[test]
    fn transcript_copier_uses_logical_text_for_both_destinations() {
        let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());
        buf.set_all_lines(vec!["alpha ".into(), "beta".into(), "gamma".into()]);
        buf.set_decoration(
            1,
            LineDecoration {
                soft_wrapped: true,
                copy_continuation: true,
                ..LineDecoration::default()
            },
        );
        buf.set_copier(std::sync::Arc::new(TranscriptCopier));

        let copied = buf.copy_range(0..buf.text().len());

        assert_eq!(copied, CopyOutput::same("alpha beta\ngamma".into()));
    }

    fn register_terminal_tool_group(lua: &smelt_core::lua::runtime::LuaRuntime, min: usize) {
        let chunk = r#"
            smelt.transcript.groups.register({
              name = "terminal-tools",
              selector = { kind = "tool", terminal = true },
              min = __MIN__,
              default_view = "expanded",
              cache_key = "test.terminal-tools:v1",
            })
            smelt.transcript.extend_renderer("test.terminal-tools", function(next, node, ctx)
              if node.kind ~= "group" or node.name ~= "terminal-tools" then
                return next(node, ctx)
              end
              local items = {
                smelt.layout.text("group:" .. node.name .. ":" .. tostring(node.child_count) .. ":" .. node.view_state),
              }
              items[#items + 1] = smelt.layout.group_children()
              return smelt.layout.vbox(items)
            end, { cache_key = "test.terminal-tools-renderer:v1" })
        "#
        .replace("__MIN__", &min.to_string());
        lua.lua.load(chunk.as_str()).exec().expect("register group");
    }

    fn push_tool(
        parser: &mut StreamParser,
        history: &mut BlockHistory,
        call_id: &str,
        summary: &str,
        status: ToolStatus,
    ) {
        let invocation_id = next_invocation_id();
        parser.start_tool(
            history,
            ToolStart {
                invocation_id,
                call_id: call_id.into(),
                name: "bash".into(),
                summary: protocol::StyledLines::from_plain(summary),
                args: std::collections::HashMap::new(),
                preview_output: None,
                called_at_ms: 0,
            },
            std::time::Instant::now(),
        );
        parser.set_active_status(history, invocation_id, status, std::time::Instant::now());
    }

    fn push_named_tool(
        transcript: &mut Transcript,
        call_id: &str,
        name: &str,
        summary: &str,
        status: ToolStatus,
        args: std::collections::HashMap<String, serde_json::Value>,
    ) {
        let is_error = matches!(status, ToolStatus::Err);
        transcript.push_tool_call(
            Block::ToolCall {
                call_id: call_id.into(),
                name: name.into(),
                summary: protocol::StyledLines::from_plain(summary),
                args: args.into(),
            },
            ToolState {
                status,
                elapsed: Some(std::time::Duration::from_millis(25)),
                called_at_ms: None,
                elapsed_active: false,
                output: Some(Box::new(ToolOutput {
                    content: format!("{summary} output").into(),
                    is_error,
                    metadata: None,
                    content_fields: Vec::new(),
                })),
                user_message: None,
                preview_output: None,
            },
        );
    }

    fn tool_args(entries: &[(&str, &str)]) -> std::collections::HashMap<String, serde_json::Value> {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_string(), serde_json::json!(*value)))
            .collect()
    }

    fn install_read_file_renderer(lua: &smelt_core::lua::runtime::LuaRuntime) {
        lua.lua
            .load(
                r#"
                smelt.text = smelt.text or {}
                smelt.text.line_count = smelt.text.line_count or function(content)
                  if content == nil or content == "" then return 0 end
                  local _, n = tostring(content):gsub("\n", "")
                  return n + 1
                end
                require('smelt.transcript')
                require('smelt.tools.read_file')
                local presentation = assert(smelt.transcript.get_tool_presentation("read_file"))
                assert(presentation.compact({ output = { content_lines = 1 } }) == "1 lines")
                "#,
            )
            .exec()
            .expect("load read_file renderer");
    }

    #[test]
    fn elapsed_update_reuses_tool_layout_until_declarative_refresh() {
        let lua = test_lua();
        let theme = Theme::default();
        let mut transcript = Transcript::new();
        transcript.push(Block::User {
            text: "run the tool".into(),
            image_labels: Vec::new(),
            command: false,
            sent_at_ms: None,
        });
        transcript.push_tool_call(
            Block::ToolCall {
                call_id: "call-1".into(),
                name: "bash".into(),
                summary: protocol::StyledLines::from_plain("cargo test"),
                args: std::collections::HashMap::new().into(),
            },
            ToolState {
                status: ToolStatus::Pending,
                elapsed: Some(std::time::Duration::from_millis(1)),
                called_at_ms: None,
                elapsed_active: true,
                output: None,
                user_message: None,
                preview_output: None,
            },
        );
        let tool_block_id = transcript
            .history
            .last_block_id()
            .expect("tool block should exist");
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(101), Default::default());
        let first = project_with_lua(
            &mut projection,
            &lua,
            &mut buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_tail(),
            20,
        );
        assert!(first.total_rows > 0);
        projection.reset_counters();

        let generation = transcript.history.generation();
        assert!(transcript.history.apply_tool_state_mutation(
            tool_block_id,
            smelt_core::transcript_model::ToolStateMutation::SyncElapsed(
                std::time::Duration::from_secs(2),
            ),
        ));
        assert!(transcript.history.generation() > generation);

        let second = project_with_lua(
            &mut projection,
            &lua,
            &mut buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_tail(),
            20,
        );
        let counters = projection.counters();
        assert_eq!(second.total_rows, first.total_rows);
        assert_eq!(counters.layout_cache, 0);
        assert_eq!(counters.exact_height_measured_blocks, 0);
        assert!(snapshot(&buf).iter().all(|row| !row.line.contains("2.0s")));
    }

    #[test]
    fn exact_block_row_target_measures_only_the_requested_node() {
        let lua = test_lua();
        let mut transcript = Transcript::new();
        for i in 0..400 {
            transcript.push(Block::Text {
                content: format!("assistant response {i}\n{}", "detail line\n".repeat(8)).into(),
            });
        }
        let target_id = transcript.history.order[237];
        let mut projection = TranscriptProjection::new();
        projection.reset_counters();

        let (anchor, target_row) = projection
            .exact_block_row_target(&lua, &mut transcript.history, 80, target_id, 3)
            .expect("exact target for loaded block");
        let counters = projection.counters();

        assert_eq!(anchor.id, RenderNodeId::Block(target_id));
        assert!(target_row > 0);
        assert!(anchor.row_offset >= 3);
        assert_eq!(counters.full_row_builds, 0);
        assert_eq!(counters.layout_cache, 1);
        assert_eq!(counters.exact_height_measured_blocks, 1);
        assert_eq!(counters.range_materialized_blocks, 0);
        assert_eq!(counters.range_materialized_rows, 0);
    }

    #[test]
    fn copy_range_materializes_only_selected_nodes() {
        let lua = test_lua();
        let theme = Theme::default();
        let mut transcript = Transcript::new();
        for i in 0..200 {
            transcript.push(Block::Text {
                content: format!("assistant response {i}\n{}", "detail line\n".repeat(8)).into(),
            });
        }
        let mut projection = TranscriptProjection::new();
        projection.reset_counters();

        let out = projection.copy_range(
            &lua,
            &mut transcript.history,
            80,
            &theme,
            DocRange {
                start: crate::smelt_edit::DocPosition {
                    row: 0,
                    byte_col: 0,
                },
                end: crate::smelt_edit::DocPosition {
                    row: 0,
                    byte_col: usize::MAX,
                },
            },
        );

        let counters = projection.counters();
        assert!(!out.clipboard.is_empty());
        assert_eq!(counters.full_row_builds, 0);
        assert!(counters.exact_height_measured_blocks <= 1);
        assert!(counters.range_materialized_blocks <= 2);
    }

    #[test]
    fn copy_range_streams_large_unloaded_middle_selection() {
        let lua = test_lua();
        let theme = Theme::default();
        let mut transcript = Transcript::new();
        for i in 0..300 {
            transcript.push(Block::Text {
                content: format!("copy-token-{i:03}").into(),
            });
        }
        let mut projection = TranscriptProjection::new();
        let estimated_total = projection.estimated_total_rows(&lua, &mut transcript.history, 80);
        assert!(estimated_total > 220);
        projection.reset_counters();

        let out = projection.copy_range(
            &lua,
            &mut transcript.history,
            80,
            &theme,
            DocRange {
                start: crate::smelt_edit::DocPosition {
                    row: 40,
                    byte_col: 0,
                },
                end: crate::smelt_edit::DocPosition {
                    row: 220,
                    byte_col: usize::MAX,
                },
            },
        );

        let counters = projection.counters();
        assert!(out.kill_ring.contains("copy-token-050"));
        assert!(out.kill_ring.contains("copy-token-100"));
        assert!(!out.kill_ring.contains("copy-token-001"));
        assert!(!out.kill_ring.contains("copy-token-250"));
        assert_eq!(counters.full_row_builds, 0);
        assert!(counters.range_materialized_blocks > COPY_CHUNK_NODES);
        assert!(counters.range_materialized_blocks < 300);
        assert!(counters.max_range_materialized_blocks <= COPY_CHUNK_NODES);
        assert!(counters.range_materialized_rows < estimated_total as usize);
        assert!(counters.exact_height_measured_blocks < 300);
    }

    #[test]
    fn display_rows_for_range_exactifies_only_requested_window() {
        let lua = test_lua();
        let theme = Theme::default();
        let mut transcript = Transcript::new();
        for i in 0..200 {
            transcript.push(Block::Text {
                content: format!("assistant response {i}\n{}", "detail line\n".repeat(8)).into(),
            });
        }
        let mut projection = TranscriptProjection::new();
        let total_rows = projection.estimated_total_rows(&lua, &mut transcript.history, 80);
        projection.reset_counters();

        let rows = projection.display_rows_for_range(
            &lua,
            &mut transcript.history,
            80,
            &theme,
            total_rows.saturating_sub(2)..total_rows.saturating_sub(1),
        );

        let counters = projection.counters();
        assert!(!rows.rows.is_empty());
        assert_eq!(counters.full_row_builds, 0);
        assert!(counters.exact_height_measured_blocks <= 2);
        assert!(counters.range_materialized_blocks <= 2);
    }

    #[test]
    fn scrolled_projection_does_not_render_full_block_for_row_anchors() {
        let lua = test_lua();
        let theme = Theme::default();
        let mut transcript = Transcript::new();
        transcript.push(Block::Text {
            content: (0..2_000)
                .map(|i| {
                    format!(
                        "large visible block line {i:04}: {}",
                        "scroll anchor text ".repeat(4)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
                .into(),
        });
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(12), Default::default());
        let total_rows = projection.estimated_total_rows(&lua, &mut transcript.history, 80);
        reset_full_layout_buffer_renders();

        let rows = project_with_lua(
            &mut projection,
            &lua,
            &mut buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_row(total_rows / 2),
            24,
        );

        assert!(rows.materialized_rows > 0);
        assert_eq!(
            full_layout_buffer_renders(),
            0,
            "exact-row scrolling should not fully re-render a large block just to build row anchors"
        );
    }

    #[test]
    fn read_file_summary_shows_default_limit_only_when_reached() {
        let lua = test_lua();
        install_read_file_renderer(&lua);
        lua.lua
            .load(
                r#"
                local short = "one\ntwo"
                local full = string.rep("line\n", 1999) .. "line"
                assert(smelt.tools.read_file_summary({ file_path = "a.rs" }, short) == "a.rs")
                assert(smelt.tools.read_file_summary({ file_path = "a.rs" }, full) == "a.rs:1-2000")
                assert(smelt.tools.read_file_summary({ file_path = "a.rs", offset = 120 }, short) == "a.rs:120")
                assert(smelt.tools.read_file_summary({ file_path = "a.rs", offset = 120, limit = 40 }, short) == "a.rs:120-159")
                "#,
            )
            .exec()
            .expect("assert read_file summary ranges");
    }

    #[derive(Debug, PartialEq)]
    struct RowSnapshot {
        line: String,
        highlights: Vec<Span>,
        decoration: LineDecoration,
    }

    fn snapshot(buf: &Buffer) -> Vec<RowSnapshot> {
        (0..buf.line_count())
            .map(|row| RowSnapshot {
                line: buf.get_line(row).unwrap_or("").to_string(),
                highlights: buf.highlights_at(row),
                decoration: buf.decoration_at(row).clone(),
            })
            .collect()
    }

    fn project_fresh(history: &mut smelt_core::transcript_model::BlockHistory) -> Vec<RowSnapshot> {
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(99), Default::default());
        projection.project(
            &mut buf,
            history,
            80,
            &theme,
            ScrollTarget::visible_row(0),
            80,
        );
        snapshot(&buf)
    }

    #[test]
    fn project_renders_text_block_into_buffer() {
        let mut transcript = Transcript::new();
        transcript.push(Block::Text {
            content: "hello".into(),
        });
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(1), Default::default());

        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_row(0),
            80,
        );

        assert!(buf.line_count() > 0);
        assert_eq!(buf.get_line(buf.line_count() - 1), Some("hello"));
    }

    #[test]
    fn fold_override_collapses_block_without_mutating_semantic_layout_key() {
        let lua = test_lua();
        let mut transcript = Transcript::new();
        transcript.push(Block::Text {
            content: "one\ntwo\nthree".into(),
        });
        let id = transcript.history.order[0];
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(77), Default::default());

        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_row(0),
            80,
        );
        assert!(buf.lines().iter().any(|line| line == "three"));
        let generation_before_fold = projection.projection_generation();

        assert!(projection.fold_node_at_row(
            &lua,
            &mut transcript.history,
            80,
            FoldAtRow {
                row: 0,
                action: FoldAction::Close,
                activation: FoldActivation::AnyNodeRow,
            },
        ));
        assert!(projection.projection_generation() > generation_before_fold);
        let history_key = transcript.history.resolve_key(id, base_layout_key(80));
        assert_eq!(history_key.view_state, ViewState::Expanded);

        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_row(0),
            80,
        );
        assert_eq!(buf.line_count(), 3);
        assert!(!buf.lines().iter().any(|line| line.contains("more lines")));
        let node = projection
            .node_metadata_at_row(&lua, &mut transcript.history, 80, 1)
            .expect("collapsed block row");
        assert!(node.explicit_fold_target);
    }

    #[test]
    fn collapsed_group_renderer_owns_its_rendered_rows() {
        let lua = test_lua();
        lua.lua
            .load(
                r#"
                smelt.transcript.groups.register({
                  name = "custom-read-group",
                  priority = 10,
                  selector = { kind = "tool", name = "read_file", terminal = true },
                  min = 2,
                  default_view = "collapsed",
                  cache_key = "test.custom-read-group:v1",
                })
                smelt.transcript.extend_renderer("test.custom-read-group", function(next, node, ctx)
                  if node.kind ~= "group" or node.name ~= "custom-read-group" then
                    return next(node, ctx)
                  end
                  return smelt.layout.vbox({
                    smelt.layout.text("custom summary " .. node.view_state),
                    smelt.layout.text("custom detail")
                  })
                end, { cache_key = "test.custom-read-group-renderer:v1" })
                "#,
            )
            .exec()
            .expect("register custom group");
        let mut transcript = Transcript::new();
        push_named_tool(
            &mut transcript,
            "read-1",
            "read_file",
            "a.rs",
            ToolStatus::Ok,
            tool_args(&[("file_path", "a.rs")]),
        );
        push_named_tool(
            &mut transcript,
            "read-2",
            "read_file",
            "b.rs",
            ToolStatus::Ok,
            tool_args(&[("file_path", "b.rs")]),
        );
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();

        let rows = projection.build_rows(&lua, &mut transcript.history, 80, &theme);

        assert!(rows.iter().any(|row| row == "custom summary collapsed"));
        assert!(rows.iter().any(|row| row == "custom detail"));
        assert!(!rows.iter().any(|row| row.contains("more lines")));
    }

    #[test]
    fn thinking_blocks_default_peek_and_can_expand() {
        let lua = test_lua();
        let mut transcript = Transcript::new();
        transcript.push(Block::Thinking {
            title: None,
            summary_titles: Vec::new(),
            kind: protocol::ReasoningKind::Raw,
            content: "first thought\nsecond thought\nthird thought\nfourth thought\nfifth thought"
                .into(),
        });
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(78), Default::default());

        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_row(0),
            80,
        );
        assert!(buf
            .lines()
            .iter()
            .any(|line| line.contains("1 line omitted")));
        assert!(!buf
            .lines()
            .iter()
            .any(|line| line.contains("second thought")));

        assert!(projection.fold_node_at_row(
            &lua,
            &mut transcript.history,
            80,
            FoldAtRow {
                row: 1,
                action: FoldAction::Open,
                activation: FoldActivation::ExplicitTargetOnly,
            },
        ));
        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_row(0),
            80,
        );
        assert!(buf
            .lines()
            .iter()
            .any(|line| line.contains("second thought")));

        assert!(projection.fold_node_at_row(
            &lua,
            &mut transcript.history,
            80,
            FoldAtRow {
                row: 1,
                action: FoldAction::Toggle,
                activation: FoldActivation::AnyNodeRow,
            },
        ));
        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_row(0),
            80,
        );
        assert!(buf
            .lines()
            .iter()
            .any(|line| line.contains("1 line omitted")));
        assert!(!buf
            .lines()
            .iter()
            .any(|line| line.contains("second thought")));
    }

    #[test]
    fn fold_kind_toggle_uses_aggregate_state() {
        let lua = test_lua();
        let mut transcript = Transcript::new();
        transcript.push(Block::Thinking {
            title: None,
            summary_titles: Vec::new(),
            kind: protocol::ReasoningKind::Raw,
            content: "a\nb".into(),
        });
        transcript.push(Block::Thinking {
            title: None,
            summary_titles: Vec::new(),
            kind: protocol::ReasoningKind::Raw,
            content: "c\nd".into(),
        });
        let mut projection = TranscriptProjection::new();
        projection.rebuild_row_index(&lua, &mut transcript.history, 80);

        assert!(projection.fold_block_kind(&transcript.history, "thinking", FoldAction::Toggle));
        let first = projection
            .node_metadata_at_row(&lua, &mut transcript.history, 80, 0)
            .expect("first thinking node");
        let second = projection
            .node_metadata_at_row(&lua, &mut transcript.history, 80, first.rows)
            .expect("second thinking node");
        assert_eq!(first.view_state, ViewState::Expanded);
        assert_eq!(second.view_state, ViewState::Expanded);

        assert!(projection.fold_node(&transcript.history, first.id, FoldAction::Close));
        assert!(projection.fold_block_kind(&transcript.history, "thinking", FoldAction::Toggle));
        let first = projection
            .node_metadata_at_row(&lua, &mut transcript.history, 80, 0)
            .expect("first thinking node");
        let second = projection
            .node_metadata_at_row(&lua, &mut transcript.history, 80, first.rows)
            .expect("second thinking node");
        assert_eq!(first.view_state, ViewState::Expanded);
        assert_eq!(second.view_state, ViewState::Expanded);

        assert!(projection.fold_block_kind(&transcript.history, "thinking", FoldAction::Toggle));
        let first = projection
            .node_metadata_at_row(&lua, &mut transcript.history, 80, 0)
            .expect("first thinking node");
        assert_eq!(first.view_state, ViewState::Collapsed);
    }

    #[test]
    fn explicit_fold_targets_are_limited_to_affordance_rows() {
        assert!(!is_explicit_fold_target(ViewState::Expanded, 0, 3));
        assert!(is_explicit_fold_target(ViewState::Collapsed, 0, 2));
        assert!(is_explicit_fold_target(ViewState::Collapsed, 1, 2));
        assert!(!is_explicit_fold_target(
            ViewState::TrimmedHead { keep: 2 },
            0,
            3
        ));
        assert!(is_explicit_fold_target(
            ViewState::TrimmedHead { keep: 2 },
            2,
            3
        ));
        assert!(is_explicit_fold_target(
            ViewState::TrimmedTail { keep: 2 },
            0,
            3
        ));
        assert!(!is_explicit_fold_target(
            ViewState::TrimmedTail { keep: 2 },
            1,
            3
        ));
    }

    #[test]
    fn project_inserts_gap_around_mode_blocks() {
        let mut transcript = Transcript::new();
        transcript.push(Block::Text {
            content: "before".into(),
        });
        transcript.push(Block::Mode {
            text: "now in apply mode".into(),
            icon: "● ".into(),
            hl_group: "SmeltModeApply".into(),
        });
        transcript.push(Block::Text {
            content: "after".into(),
        });

        let rows = project_fresh(&mut transcript.history);
        let lines: Vec<&str> = rows.iter().map(|row| row.line.as_str()).collect();
        assert_eq!(
            lines,
            vec!["before", "", "● now in apply mode", "", "after"]
        );
    }

    #[test]
    fn user_chrome_uses_indented_background_fill_not_fake_full_width_text() {
        let mut transcript = Transcript::new();
        transcript.push(Block::User {
            text: "hello".into(),
            image_labels: vec![],
            command: false,
            sent_at_ms: None,
        });

        let rows = project_fresh(&mut transcript.history);

        assert_eq!(rows.first().map(|row| row.line.as_str()), Some(" "));
        assert_eq!(rows.last().map(|row| row.line.as_str()), Some(" "));
        assert!(rows
            .first()
            .is_some_and(|row| row.decoration.fill_bg.is_some()));
        assert!(rows
            .last()
            .is_some_and(|row| row.decoration.fill_bg.is_some()));
        assert!(rows.first().is_some_and(|row| {
            row.highlights
                .iter()
                .any(|span| span.col_start == 0 && span.col_end == 1 && !span.meta.selectable)
        }));
        assert!(rows.last().is_some_and(|row| {
            row.highlights
                .iter()
                .any(|span| span.col_start == 0 && span.col_end == 1 && !span.meta.selectable)
        }));
        assert!(rows.iter().any(|row| row.line == " hello"));
        assert!(!rows.iter().any(|row| row.line.len() >= 80));
    }

    #[test]
    fn planned_projection_rechecks_renderer_identity_before_rendering() {
        let lua = test_lua();
        let mut transcript = Transcript::new();
        transcript.push(Block::Text {
            content: "before".into(),
        });
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let plan = projection.plan_projection_measured(
            &lua,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_row(0),
            10,
        );
        lua.lua
            .load(
                r#"
                smelt.transcript.set_renderer(function(block, ctx)
                  local _ = block
                  local _ = ctx
                  return smelt.layout.text("after")
                end, { cache_key = "test.planned_projection.after:v1" })
                "#,
            )
            .exec()
            .expect("set renderer");
        let mut buf = Buffer::new(crate::smelt_edit::BufId(23), Default::default());

        projection.project_planned(&lua, &mut buf, &mut transcript.history, &theme, plan);

        assert!(buf.lines().iter().any(|line| line == "after"));
        assert!(!buf.lines().iter().any(|line| line == "before"));
    }

    #[test]
    fn exact_row_tape_handle_rejects_stale_projection_generations() {
        let lua = test_lua();
        let mut transcript = Transcript::new();
        for index in 0..20 {
            transcript.push(Block::Text {
                content: format!("row {index}").into(),
            });
        }
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(24), Default::default());
        let plan = projection.plan_projection_measured(
            &lua,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_row(5),
            5,
        );
        let rows =
            projection.project_planned(&lua, &mut buf, &mut transcript.history, &theme, plan);
        let handle = projection
            .exact_row_tape_handle(rows)
            .expect("exact row tape handle");
        assert!(projection
            .exact_row_tape_state(&lua, &transcript.history, handle, 80, 5)
            .is_some());

        projection.invalidate_theme();

        assert!(projection
            .exact_row_tape_state(&lua, &transcript.history, handle, 80, 5)
            .is_none());
    }

    #[test]
    fn exact_row_tape_handle_retargets_within_materialized_rows() {
        let lua = test_lua();
        let mut transcript = Transcript::new();
        for index in 0..20 {
            transcript.push(Block::Text {
                content: format!("row {index}").into(),
            });
        }
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(25), Default::default());
        let plan = projection.plan_projection_measured(
            &lua,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_row(5),
            5,
        );
        let rows =
            projection.project_planned(&lua, &mut buf, &mut transcript.history, &theme, plan);
        let handle = projection
            .exact_row_tape_handle(rows)
            .expect("exact row tape handle");
        let retarget_scroll = rows.clamped_scroll.saturating_add(1);
        assert!(rows.covers_viewport(retarget_scroll, 5));

        let retargeted = projection
            .retarget_exact_row_tape(&lua, &transcript.history, handle, 80, 5, retarget_scroll)
            .expect("retargeted exact row tape handle");
        let state = projection
            .exact_row_tape_state(&lua, &transcript.history, retargeted, 80, 5)
            .expect("retargeted exact row tape state");

        assert_eq!(state.rows.clamped_scroll, retarget_scroll);
        assert!(projection
            .retarget_exact_row_tape(
                &lua,
                &transcript.history,
                handle,
                80,
                5,
                rows.total_rows.saturating_add(1),
            )
            .is_none());
    }

    #[test]
    fn exact_row_tape_handle_rejects_scene_refresh_without_projection() {
        let lua = test_lua();
        let mut transcript = Transcript::new();
        transcript.push(Block::Text {
            content: "before".into(),
        });
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(25), Default::default());
        let plan = projection.plan_projection_measured(
            &lua,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_row(0),
            5,
        );
        let rows =
            projection.project_planned(&lua, &mut buf, &mut transcript.history, &theme, plan);
        let handle = projection
            .exact_row_tape_handle(rows)
            .expect("exact row tape handle");

        transcript.push(Block::Text {
            content: "after".into(),
        });
        projection.prepare_layout(&lua, &mut transcript.history, 80);

        assert!(projection
            .exact_row_tape_state(&lua, &transcript.history, handle, 80, 5)
            .is_none());
    }

    #[test]
    fn visible_projection_matches_fresh_after_markdown_table_growth() {
        let mut transcript = Transcript::new();
        transcript.push(Block::User {
            text: "show a table".into(),
            image_labels: vec![],
            command: false,
            sent_at_ms: None,
        });
        let mut parser = StreamParser::new();
        parser.append_streaming_text(
            &mut transcript.history,
            "| Name | Value |\n| --- | --- |\n| alpha |",
        );

        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(2), Default::default());
        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_row(0),
            80,
        );

        parser.append_streaming_text(&mut transcript.history, " 1 |");
        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_row(0),
            80,
        );

        let projected = snapshot(&buf);
        let fresh = project_fresh(&mut transcript.history);
        assert_eq!(projected, fresh);
    }

    #[test]
    fn visible_projection_rerenders_tool_state_changes() {
        let mut transcript = Transcript::new();
        transcript.push(Block::User {
            text: "run ls".into(),
            image_labels: vec![],
            command: false,
            sent_at_ms: None,
        });
        let mut parser = StreamParser::new();
        let invocation_id = next_invocation_id();
        parser.start_tool(
            &mut transcript.history,
            ToolStart {
                invocation_id,
                call_id: "call-1".into(),
                name: "bash".into(),
                summary: protocol::StyledLines::from_plain("ls"),
                args: std::collections::HashMap::new(),
                preview_output: None,
                called_at_ms: 0,
            },
            std::time::Instant::now(),
        );

        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(3), Default::default());
        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_row(0),
            80,
        );
        let before = snapshot(&buf);

        parser.append_active_output_line(&mut transcript.history, invocation_id, "done".into());
        parser.set_active_status(
            &mut transcript.history,
            invocation_id,
            ToolStatus::Ok,
            std::time::Instant::now(),
        );
        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_row(0),
            80,
        );

        let projected = snapshot(&buf);
        let fresh = project_fresh(&mut transcript.history);
        assert_ne!(projected, before);
        assert_eq!(projected, fresh);
    }

    #[test]
    fn lua_registered_groups_replace_adjacent_terminal_tools() {
        let lua = test_lua();
        let mut transcript = Transcript::new();
        let mut parser = StreamParser::new();
        push_tool(
            &mut parser,
            &mut transcript.history,
            "call-1",
            "first",
            ToolStatus::Ok,
        );
        push_tool(
            &mut parser,
            &mut transcript.history,
            "call-2",
            "second failed",
            ToolStatus::Err,
        );
        transcript.push(Block::Text {
            content: "after".into(),
        });
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();

        let before_group = projection.build_rows(&lua, &mut transcript.history, 80, &theme);
        assert!(before_group.iter().any(|line| line.contains("bash")));
        assert!(!before_group.iter().any(|line| line.starts_with("group:")));

        register_terminal_tool_group(&lua, 2);
        let grouped = projection.build_rows(&lua, &mut transcript.history, 80, &theme);

        assert!(matches!(
            projection.transcript_scene.nodes.as_slice(),
            [crate::content::transcript_scene::RenderNode::Group(group), crate::content::transcript_scene::RenderNode::Block { .. }] if group.children.len() == 2
        ));
        assert!(grouped
            .iter()
            .any(|line| line == "group:terminal-tools:2:expanded"));
        for child_header in ["* bash first", "* bash second failed"] {
            assert!(
                grouped.iter().any(|line| {
                    line.strip_prefix(child_header)
                        .is_some_and(|invoked_at| !invoked_at.trim().is_empty())
                }),
                "missing invocation time after {child_header:?} in grouped rows: {grouped:?}"
            );
        }
        assert!(grouped.iter().any(|line| line == "after"));
    }

    #[test]
    fn grouped_child_content_append_reuses_compiled_sibling_layouts() {
        let lua = test_lua();
        register_terminal_tool_group(&lua, 2);
        let mut transcript = Transcript::new();
        let mut parser = StreamParser::new();
        let first_invocation = next_invocation_id();
        let second_invocation = next_invocation_id();
        for (invocation_id, call_id) in
            [(first_invocation, "call-1"), (second_invocation, "call-2")]
        {
            parser.start_tool(
                &mut transcript.history,
                ToolStart {
                    invocation_id,
                    call_id: call_id.into(),
                    name: "bash".into(),
                    summary: protocol::StyledLines::from_plain(call_id),
                    args: std::collections::HashMap::new(),
                    preview_output: None,
                    called_at_ms: 0,
                },
                std::time::Instant::now(),
            );
            parser.append_active_output_line(
                &mut transcript.history,
                invocation_id,
                "seed output\n".into(),
            );
        }
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let initial = projection.build_rows(&lua, &mut transcript.history, 80, &theme);
        assert!(
            initial.iter().any(|line| line.contains("seed output")),
            "missing retained tool output in grouped rows: {initial:?}"
        );
        projection.reset_counters();

        parser.append_active_output_line(
            &mut transcript.history,
            first_invocation,
            "unique continuation\n".into(),
        );
        let appended = projection.build_rows(&lua, &mut transcript.history, 80, &theme);

        assert!(
            appended
                .iter()
                .any(|line| line.contains("unique continuation")),
            "missing appended retained tool output in grouped rows: {appended:?}"
        );
        assert_eq!(projection.counters().layout_cache, 0);
    }

    #[test]
    fn group_min_threshold_keeps_original_blocks() {
        let lua = test_lua();
        register_terminal_tool_group(&lua, 3);
        let mut transcript = Transcript::new();
        let mut parser = StreamParser::new();
        push_tool(
            &mut parser,
            &mut transcript.history,
            "call-1",
            "first",
            ToolStatus::Ok,
        );
        push_tool(
            &mut parser,
            &mut transcript.history,
            "call-2",
            "second",
            ToolStatus::Ok,
        );
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();

        let rows = projection.build_rows(&lua, &mut transcript.history, 80, &theme);

        assert_eq!(projection.transcript_scene.nodes.len(), 2);
        assert!(projection
            .transcript_scene
            .nodes
            .iter()
            .all(|node| matches!(
                node,
                crate::content::transcript_scene::RenderNode::Block { .. }
            )));
        assert!(!rows.iter().any(|line| line.starts_with("group:")));
        assert_eq!(rows.iter().filter(|line| line.contains("bash")).count(), 2);
    }

    #[test]
    fn non_matching_block_breaks_registered_groups() {
        let lua = test_lua();
        register_terminal_tool_group(&lua, 2);
        let mut transcript = Transcript::new();
        let mut parser = StreamParser::new();
        push_tool(
            &mut parser,
            &mut transcript.history,
            "call-1",
            "first",
            ToolStatus::Ok,
        );
        transcript.push(Block::Text {
            content: "between".into(),
        });
        push_tool(
            &mut parser,
            &mut transcript.history,
            "call-2",
            "second",
            ToolStatus::Ok,
        );
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();

        let rows = projection.build_rows(&lua, &mut transcript.history, 80, &theme);

        assert_eq!(projection.transcript_scene.nodes.len(), 3);
        assert!(projection
            .transcript_scene
            .nodes
            .iter()
            .all(|node| matches!(
                node,
                crate::content::transcript_scene::RenderNode::Block { .. }
            )));
        assert!(!rows.iter().any(|line| line.starts_with("group:")));
        assert!(rows.iter().any(|line| line == "between"));
    }

    #[test]
    fn grouped_node_gap_uses_semantic_child_boundary() {
        let lua = test_lua();
        lua.lua
            .load(
                r#"
                smelt.transcript.groups.register({
                  name = "assistant-pair",
                  selector = { kind = "assistant" },
                  min = 2,
                  default_view = "expanded",
                  cache_key = "test.assistant-pair:v1",
                })
                smelt.transcript.extend_renderer("test.assistant-pair", function(next, node, ctx)
                  if node.kind == "group" and node.name == "assistant-pair" then
                    return smelt.layout.text("assistant group")
                  end
                  return next(node, ctx)
                end, { cache_key = "test.assistant-pair-renderer:v1" })
                "#,
            )
            .exec()
            .expect("register assistant group");
        let mut transcript = Transcript::new();
        transcript.push(Block::Text {
            content: "first".into(),
        });
        transcript.push(Block::Text {
            content: "## heading".into(),
        });
        transcript.push(Block::CodeLine {
            content: "x".into(),
            lang: "rust".into(),
        });
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();

        let rows = projection.build_rows(&lua, &mut transcript.history, 80, &theme);

        assert_eq!(
            rows.as_ref(),
            &vec!["assistant group".to_string(), "x".to_string()]
        );
    }

    #[test]
    fn terminal_group_keeps_failed_child_in_order() {
        let lua = test_lua();
        register_terminal_tool_group(&lua, 2);
        let mut transcript = Transcript::new();
        let mut parser = StreamParser::new();
        push_tool(
            &mut parser,
            &mut transcript.history,
            "call-1",
            "ok child",
            ToolStatus::Ok,
        );
        push_tool(
            &mut parser,
            &mut transcript.history,
            "call-2",
            "failed child",
            ToolStatus::Err,
        );
        push_tool(
            &mut parser,
            &mut transcript.history,
            "call-3",
            "later ok",
            ToolStatus::Ok,
        );
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();

        let rows = projection.build_rows(&lua, &mut transcript.history, 80, &theme);

        assert!(matches!(
            projection.transcript_scene.nodes.as_slice(),
            [crate::content::transcript_scene::RenderNode::Group(group)] if group.children.len() == 3
        ));
        let child_rows: Vec<_> = rows
            .iter()
            .filter(|line| line.starts_with("* bash "))
            .map(String::as_str)
            .collect();
        let expected_headers = ["* bash ok child", "* bash failed child", "* bash later ok"];
        assert_eq!(child_rows.len(), expected_headers.len());
        for (row, expected_header) in child_rows.iter().zip(expected_headers) {
            assert!(
                row.strip_prefix(expected_header)
                    .is_some_and(|invoked_at| !invoked_at.trim().is_empty()),
                "expected {expected_header:?} with an invocation time, got {row:?}"
            );
        }
    }

    #[test]
    fn built_in_explore_group_replaces_adjacent_terminal_calls() {
        let lua = test_lua();
        install_read_file_renderer(&lua);
        let mut transcript = Transcript::new();
        push_named_tool(
            &mut transcript,
            "read-1",
            "read_file",
            "crates/core/src/transcript_model.rs",
            ToolStatus::Ok,
            tool_args(&[("file_path", "crates/core/src/transcript_model.rs")]),
        );
        push_named_tool(
            &mut transcript,
            "read-2",
            "read_file",
            "crates/tui/src/content/display_layout.rs",
            ToolStatus::Ok,
            tool_args(&[("file_path", "crates/tui/src/content/display_layout.rs")]),
        );
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();

        let rows = projection.build_rows(&lua, &mut transcript.history, 80, &theme);

        assert!(matches!(
            projection.transcript_scene.nodes.as_slice(),
            [crate::content::transcript_scene::RenderNode::Group(group)] if group.name == "explore" && group.children.len() == 2
        ));
        assert!(
            rows.iter().any(|line| line == "* explore ×2"),
            "rows: {rows:?}"
        );
        assert!(rows
            .iter()
            .any(|line| line == "  read_file crates/core/src/transcript_model.rs"));
        assert!(rows
            .iter()
            .any(|line| line == "  read_file crates/tui/src/content/display_layout.rs"));

        let group_id = projection.transcript_scene.nodes[0].id();
        assert!(projection.fold_node(&transcript.history, group_id, FoldAction::Open));
        let expanded = projection.build_rows(&lua, &mut transcript.history, 80, &theme);
        assert!(
            expanded.iter().any(|line| line.contains("* read_file")),
            "expanded rows: {expanded:?}"
        );
        assert!(
            expanded.iter().any(|line| line.trim() == "1 lines"),
            "expanded rows: {expanded:?}"
        );
        assert!(!expanded.iter().any(|line| line.contains(" output")));
        assert!(projection.fold_node_at_row(
            &lua,
            &mut transcript.history,
            80,
            FoldAtRow {
                row: 2,
                action: FoldAction::Toggle,
                activation: FoldActivation::AnyNodeRow,
            },
        ));
        let collapsed = projection.build_rows(&lua, &mut transcript.history, 80, &theme);
        assert!(collapsed.iter().any(|line| line == "* explore ×2"));
        assert!(!collapsed.iter().any(|line| line == "  2 lines"));
    }

    #[test]
    fn built_in_tool_group_collapsed_list_shows_tail() {
        let lua = test_lua();
        install_read_file_renderer(&lua);
        let mut transcript = Transcript::new();
        for i in 1..=6 {
            let path = format!("file-{i}.rs");
            push_named_tool(
                &mut transcript,
                &format!("read-{i}"),
                "read_file",
                &path,
                ToolStatus::Ok,
                tool_args(&[("file_path", &path)]),
            );
        }
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();

        let rows = projection.build_rows(&lua, &mut transcript.history, 80, &theme);

        assert!(rows.iter().any(|line| line == "* explore ×6"));
        assert!(rows.iter().any(|line| line == "  … 1 above"));
        assert!(!rows.iter().any(|line| line == "  read_file file-1.rs"));
        for i in 2..=6 {
            assert!(
                rows.iter()
                    .any(|line| line == &format!("  read_file file-{i}.rs")),
                "rows: {rows:?}"
            );
        }
    }

    #[test]
    fn grouped_children_remain_in_semantic_block_layout() {
        let lua = test_lua();
        install_read_file_renderer(&lua);
        let mut transcript = Transcript::new();
        push_named_tool(
            &mut transcript,
            "read-1",
            "read_file",
            "crates/core/src/transcript_model.rs",
            ToolStatus::Ok,
            tool_args(&[("file_path", "crates/core/src/transcript_model.rs")]),
        );
        push_named_tool(
            &mut transcript,
            "read-2",
            "read_file",
            "crates/tui/src/content/display_layout.rs",
            ToolStatus::Ok,
            tool_args(&[("file_path", "crates/tui/src/content/display_layout.rs")]),
        );
        let first = transcript.history.order[0];
        let second = transcript.history.order[1];
        let mut projection = TranscriptProjection::new();

        let layout = projection.materialize_block_layout(&lua, &mut transcript.history, 80);
        let search_layout = projection.materialize_search_layout(&lua, &mut transcript.history, 80);

        assert!(matches!(
            projection.transcript_scene.nodes.as_slice(),
            [crate::content::transcript_scene::RenderNode::Group(group)] if group.name == "explore" && group.child_ids().collect::<Vec<_>>() == [first, second]
        ));
        assert_eq!(layout.len(), 2);
        assert_eq!(layout[0].0, first);
        assert_eq!(layout[1].0, second);
        assert_eq!(layout[0].1, layout[1].1);
        assert_eq!(layout[0].2, layout[1].2);
        assert!(layout[0].2 > 0);
        assert_eq!(search_layout.entries.len(), 1);
        assert_eq!(search_layout.entries[0].block_ids, vec![first, second]);
        assert_eq!(search_layout.entries[0].first_row, layout[0].1);
        assert_eq!(search_layout.entries[0].rows, layout[0].2);
    }

    #[test]
    fn built_in_explore_group_mixes_adjacent_tools() {
        let lua = test_lua();
        let mut transcript = Transcript::new();
        push_named_tool(
            &mut transcript,
            "read-1",
            "read_file",
            "src/lib.rs",
            ToolStatus::Ok,
            tool_args(&[("file_path", "src/lib.rs")]),
        );
        push_named_tool(
            &mut transcript,
            "grep-1",
            "grep",
            "RenderNode",
            ToolStatus::Ok,
            tool_args(&[("pattern", "RenderNode")]),
        );
        push_named_tool(
            &mut transcript,
            "glob-1",
            "glob",
            "**/*.rs",
            ToolStatus::Ok,
            tool_args(&[("pattern", "**/*.rs")]),
        );
        push_named_tool(
            &mut transcript,
            "outline-1",
            "outline",
            "src/lib.rs",
            ToolStatus::Ok,
            tool_args(&[("file_path", "src/lib.rs")]),
        );
        push_named_tool(
            &mut transcript,
            "symbol-1",
            "find_symbol",
            "RenderNode",
            ToolStatus::Ok,
            tool_args(&[("query", "RenderNode")]),
        );
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();

        let rows = projection.build_rows(&lua, &mut transcript.history, 80, &theme);

        assert!(matches!(
            projection.transcript_scene.nodes.as_slice(),
            [crate::content::transcript_scene::RenderNode::Group(group)]
                if group.name == "explore" && group.children.len() == 5
        ));
        assert!(rows.iter().any(|line| line == "* explore ×5"));
        assert!(rows.iter().any(|line| line == "  read_file src/lib.rs"));
        assert!(rows.iter().any(|line| line == "  grep \"RenderNode\""));
        assert!(rows.iter().any(|line| line == "  glob **/*.rs"));
        assert!(rows.iter().any(|line| line == "  outline src/lib.rs"));
        assert!(rows.iter().any(|line| line == "  find_symbol RenderNode"));
    }

    #[test]
    fn built_in_lsp_group_mixes_adjacent_semantic_tools() {
        let lua = test_lua();
        let mut transcript = Transcript::new();
        let names = [
            "inspect_symbol",
            "inspect_symbol_at",
            "find_definition",
            "find_references",
            "diagnostics",
        ];
        for (index, name) in names.iter().copied().enumerate() {
            push_named_tool(
                &mut transcript,
                &format!("lsp-{index}"),
                name,
                name,
                ToolStatus::Ok,
                tool_args(&[]),
            );
        }
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();

        let rows = projection.build_rows(&lua, &mut transcript.history, 80, &theme);

        assert!(matches!(
            projection.transcript_scene.nodes.as_slice(),
            [crate::content::transcript_scene::RenderNode::Group(group)]
                if group.name == "lsp" && group.children.len() == 5
        ));
        assert!(rows.iter().any(|line| line == "* lsp ×5"));
        for name in names {
            assert!(rows.iter().any(|line| line == &format!("  {name}")));
        }
    }

    #[test]
    fn built_in_tool_groups_leave_lsp_status_and_renames_standalone() {
        let lua = test_lua();
        let mut transcript = Transcript::new();
        for (index, name) in ["language_server_status", "preview_rename", "rename_symbol"]
            .into_iter()
            .enumerate()
        {
            push_named_tool(
                &mut transcript,
                &format!("standalone-{index}"),
                name,
                name,
                ToolStatus::Ok,
                tool_args(&[]),
            );
        }
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();

        projection.build_rows(&lua, &mut transcript.history, 80, &theme);

        assert_eq!(projection.transcript_scene.nodes.len(), 3);
        assert!(projection
            .transcript_scene
            .nodes
            .iter()
            .all(|node| matches!(
                node,
                crate::content::transcript_scene::RenderNode::Block { .. }
            )));
    }

    #[test]
    fn built_in_explore_group_includes_mixed_pending_calls() {
        let lua = test_lua();
        let mut transcript = Transcript::new();
        push_named_tool(
            &mut transcript,
            "read-1",
            "read_file",
            "a.rs",
            ToolStatus::Ok,
            tool_args(&[("file_path", "a.rs")]),
        );
        push_named_tool(
            &mut transcript,
            "grep-1",
            "grep",
            "needle",
            ToolStatus::Ok,
            tool_args(&[("pattern", "needle")]),
        );
        push_named_tool(
            &mut transcript,
            "read-pending",
            "read_file",
            "b.rs",
            ToolStatus::Pending,
            tool_args(&[("file_path", "b.rs")]),
        );
        push_named_tool(
            &mut transcript,
            "read-2",
            "read_file",
            "c.rs",
            ToolStatus::Ok,
            tool_args(&[("file_path", "c.rs")]),
        );
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();

        let rows = projection.build_rows(&lua, &mut transcript.history, 80, &theme);

        assert!(matches!(
            projection.transcript_scene.nodes.as_slice(),
            [crate::content::transcript_scene::RenderNode::Group(group)]
                if group.name == "explore" && group.children.len() == 4
        ));
        assert!(rows.iter().any(|line| line.starts_with("* explore ×4")));
    }

    #[test]
    fn built_in_tool_group_summary_surfaces_errors_and_denials() {
        let lua = test_lua();
        let mut transcript = Transcript::new();
        push_named_tool(
            &mut transcript,
            "read-1",
            "read_file",
            "ok.rs",
            ToolStatus::Ok,
            tool_args(&[("file_path", "ok.rs")]),
        );
        push_named_tool(
            &mut transcript,
            "read-2",
            "read_file",
            "err.rs",
            ToolStatus::Err,
            tool_args(&[("file_path", "err.rs")]),
        );
        push_named_tool(
            &mut transcript,
            "read-3",
            "read_file",
            "denied.rs",
            ToolStatus::Denied,
            tool_args(&[("file_path", "denied.rs")]),
        );
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();

        let rows = projection.build_rows(&lua, &mut transcript.history, 80, &theme);

        assert!(matches!(
            projection.transcript_scene.nodes.as_slice(),
            [crate::content::transcript_scene::RenderNode::Group(group)] if group.name == "explore" && group.children.len() == 3
        ));
        assert!(
            rows.iter()
                .any(|line| line == "* explore ×3 (1 error, 1 denied)"),
            "rows: {rows:?}"
        );
        assert!(rows.iter().any(|line| line == "  read_file err.rs"));
    }

    #[test]
    fn built_in_background_process_completion_group_uses_typed_event_fields() {
        let lua = test_lua();
        let mut transcript = Transcript::new();
        transcript.push(Block::ProcessStatus {
            text: "Background process 1 finished successfully.".into(),
            event: Some(protocol::ProcessStatusEvent::background_process_completed(
                "1",
                Some(0),
                protocol::JobTermination::Exited,
            )),
        });
        transcript.push(Block::ProcessStatus {
            text: "Background process 2 exited with code 7.".into(),
            event: Some(protocol::ProcessStatusEvent::background_process_completed(
                "2",
                Some(7),
                protocol::JobTermination::Exited,
            )),
        });
        transcript.push(Block::ProcessStatus {
            text: "legacy process note".into(),
            event: None,
        });
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();

        let rows = projection.build_rows(&lua, &mut transcript.history, 80, &theme);

        assert!(matches!(
            projection.transcript_scene.nodes.as_slice(),
            [
                crate::content::transcript_scene::RenderNode::Group(group),
                crate::content::transcript_scene::RenderNode::Block { .. },
            ] if group.name == "background_process_completed" && group.children.len() == 2
        ));
        assert!(
            rows.iter()
                .any(|line| line
                    == "background processes finished: 2, 1 failed: 2 exited with code 7")
        );
        assert!(rows.iter().any(|line| line == "legacy process note"));
    }

    #[test]
    fn visible_projection_resolves_scroll_top_from_semantic_anchor_after_prefix_changes() {
        let theme = Theme::default();
        let transcript = Transcript::new();
        let mut projection = TranscriptProjection::new();
        let layout_key = base_layout_key(80);
        projection.measurements.active.nodes = vec![
            TranscriptHeightNode {
                id: RenderNodeId::Block(BlockId::new(1)),
                key: layout_key,
                estimated_height: 100,
                exact_height: None,
            },
            TranscriptHeightNode {
                id: RenderNodeId::Block(BlockId::new(2)),
                key: layout_key,
                estimated_height: 10,
                exact_height: None,
            },
        ];
        projection.measurements.active.rebuild_prefix_rows();

        let anchor = projection
            .measurements
            .active
            .scroll_anchor_at_row(105)
            .expect("row lands in second node before measurements change");
        projection.measurements.active.nodes[0].exact_height = Some(1);
        projection.measurements.active.rebuild_prefix_rows();

        let request = ProjectionRequest {
            key: ProjectKey {
                generation: 0,
                width: 80,
                renderer_generation: 0,
                renderer_cache_key: None,
                presentation_generation: 0,
                row_generation: 0,
                mode: ProjectionMode::Visible { viewport_rows: 3 },
            },
            target: ResolvedProjectionTarget {
                requested: ScrollTarget::visible_row(105),
                anchor: Some(anchor),
            },
            viewport_rows: 3,
        };
        let plan = projection.plan_projection_from_prepared(&transcript.history, &theme, &request);

        assert_eq!(plan.scroll_top, 6);
        assert_eq!(plan.node_range(), 1..2);
    }

    #[test]
    fn visible_projection_honors_requested_row_when_prefix_measurements_shrink() {
        let lua = test_lua();
        lua.lua
            .load(
                r#"
                require("smelt.transcript")
                smelt.transcript.extend_renderer("semantic-anchor-test", function(next, block, ctx)
                  if block.kind == "tool" and block.name == "prefix_short" then
                    return smelt.layout.text("prefix exact row")
                  end
                  if block.kind == "tool" and block.name == "target_rows" then
                    return smelt.layout.content(block.output.content_id, { format = "text" })
                  end
                  return next(block, ctx)
                end, { cache_key = "semantic-anchor-test:v1" })
                "#,
            )
            .exec()
            .expect("register semantic anchor renderer");
        let theme = Theme::default();
        let mut transcript = Transcript::new();
        let prefix_output = (0..120)
            .map(|i| format!("prefix estimated row {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        transcript.push_tool_call(
            Block::ToolCall {
                call_id: "prefix".into(),
                name: "prefix_short".into(),
                summary: protocol::StyledLines::from_plain("prefix"),
                args: std::collections::HashMap::new().into(),
            },
            ToolState {
                status: ToolStatus::Ok,
                elapsed: None,
                called_at_ms: None,
                elapsed_active: false,
                output: Some(Box::new(ToolOutput {
                    content: prefix_output.into(),
                    is_error: false,
                    metadata: None,
                    content_fields: Vec::new(),
                })),
                user_message: None,
                preview_output: None,
            },
        );
        let target_output = (0..60)
            .map(|i| format!("target row {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        transcript.push_tool_call(
            Block::ToolCall {
                call_id: "target".into(),
                name: "target_rows".into(),
                summary: protocol::StyledLines::from_plain("target"),
                args: std::collections::HashMap::new().into(),
            },
            ToolState {
                status: ToolStatus::Ok,
                elapsed: None,
                called_at_ms: None,
                elapsed_active: false,
                output: Some(Box::new(ToolOutput {
                    content: target_output.into(),
                    is_error: false,
                    metadata: None,
                    content_fields: Vec::new(),
                })),
                user_message: None,
                preview_output: None,
            },
        );
        let target = transcript.history.order[1];
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(103), Default::default());

        let rows = project_with_lua(
            &mut projection,
            &lua,
            &mut buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_row(125),
            40,
        );

        let target_layout = projection
            .visible_block_layout()
            .find(|(id, _, _)| *id == target)
            .expect("target block is materialized");
        assert!(
            rows.clamped_scroll >= target_layout.1
                && rows.clamped_scroll < target_layout.1.saturating_add(target_layout.2),
            "scroll top should remain in the target block after prefix exactification"
        );
        let top_line = buf
            .get_line((rows.clamped_scroll - rows.row_base) as usize)
            .expect("visible top row");
        assert_eq!(top_line, "target row 20");
        assert!(
            rows.total_rows < 125,
            "the requested numeric row should be clamped against exact heights"
        );
    }

    #[test]
    fn visible_projection_converges_after_many_large_estimate_corrections() {
        let lua = test_lua();
        lua.lua
            .load(
                r#"
                require("smelt.transcript")
                smelt.transcript.extend_renderer("many-estimate-corrections", function(next, block, ctx)
                  if block.kind == "tool" and block.name == "estimated_large_exact_short" then
                    return smelt.layout.text("exact row " .. block.call_id)
                  end
                  return next(block, ctx)
                end, { cache_key = "many-estimate-corrections:v1" })
                "#,
            )
            .exec()
            .expect("register estimate correction renderer");
        let theme = Theme::default();
        let mut transcript = Transcript::new();
        let estimated_large_output = "estimated row\n".repeat(7_500);
        for index in 0..20 {
            transcript.push_tool_call(
                Block::ToolCall {
                    call_id: format!("short-{index}"),
                    name: "estimated_large_exact_short".into(),
                    summary: protocol::StyledLines::from_plain(format!("short {index}")),
                    args: std::collections::HashMap::new().into(),
                },
                ToolState {
                    status: ToolStatus::Ok,
                    elapsed: None,
                    called_at_ms: None,
                    elapsed_active: false,
                    output: Some(Box::new(ToolOutput {
                        content: estimated_large_output.clone().into(),
                        is_error: false,
                        metadata: None,
                        content_fields: Vec::new(),
                    })),
                    user_message: None,
                    preview_output: None,
                },
            );
        }
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(104), Default::default());

        let rows = project_with_lua(
            &mut projection,
            &lua,
            &mut buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_row(66_515),
            13,
        );

        assert!(rows.materialized_rows > 0);
        assert!(rows.clamped_scroll >= rows.row_base);
        assert!(
            rows.clamped_scroll.saturating_add(13)
                <= rows.row_base.saturating_add(rows.materialized_rows)
        );
        let planning_passes = projection.counters().projection_planning_passes;
        assert!(
            planning_passes > 8,
            "fixture must exercise more than the old arbitrary pass cap"
        );
        assert!(
            planning_passes <= 21,
            "planning exceeded one progress pass per node plus convergence: {planning_passes}"
        );
    }

    #[test]
    fn search_layout_uses_estimates_without_measuring_every_block() {
        let mut transcript = Transcript::new();
        for i in 0..100 {
            transcript.push(Block::Text {
                content: format!("line {i} alpha").into(),
            });
        }
        let mut projection = TranscriptProjection::new();

        let layout = projection.materialize_search_layout(&test_lua(), &mut transcript.history, 80);

        assert_eq!(layout.entries.len(), 100);
        assert!(layout.entries.iter().any(|entry| entry.first_row > 0));
        let counters = projection.counters();
        assert_eq!(counters.full_row_builds, 0);
        assert_eq!(counters.layout_cache, 0);
        assert_eq!(counters.exact_height_measured_blocks, 0);

        projection.reset_counters();
        let candidate_layout = projection.materialize_search_layout_for_blocks(
            &test_lua(),
            &mut transcript.history,
            80,
            &[3, 77],
        );
        assert_eq!(candidate_layout.entries.len(), 2);
        assert_eq!(candidate_layout.entries[0].block_ids, vec![BlockId::new(3)]);
        assert_eq!(
            candidate_layout.entries[1].block_ids,
            vec![BlockId::new(77)]
        );
        let counters = projection.counters();
        assert_eq!(counters.full_row_builds, 0);
        assert_eq!(counters.layout_cache, 2);
        assert_eq!(counters.exact_height_measured_blocks, 2);
    }

    #[test]
    fn build_rows_materializes_full_transcript() {
        let mut transcript = Transcript::new();
        for i in 0..100 {
            transcript.push(Block::Text {
                content: format!("line {i}").into(),
            });
        }
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();

        let rows = projection.build_rows(&test_lua(), &mut transcript.history, 80, &theme);

        assert!(rows.iter().any(|line| line == "line 99"));
        assert!(rows.iter().any(|line| line == "line 0"));
        let counters = projection.counters();
        assert_eq!(counters.full_row_builds, 1);
        assert_eq!(counters.layout_cache, 100);
        assert_eq!(counters.exact_height_measured_blocks, 100);

        projection.reset_counters();
        let cached = projection.build_rows(&test_lua(), &mut transcript.history, 80, &theme);
        assert_eq!(cached.len(), rows.len());
        assert_eq!(
            projection.counters(),
            TranscriptProjectionCounters::default()
        );
    }

    #[test]
    fn exact_total_rows_measures_without_building_full_rows() {
        let mut transcript = Transcript::new();
        for i in 0..100 {
            transcript.push(Block::Text {
                content: format!("line {i}").into(),
            });
        }
        let mut projection = TranscriptProjection::new();

        let total = projection.exact_total_rows(&test_lua(), &mut transcript.history, 80);

        assert_eq!(total, 199);
        let counters = projection.counters();
        assert_eq!(counters.full_row_builds, 0);
        assert_eq!(counters.layout_cache, 100);
        assert_eq!(counters.exact_height_measured_blocks, 100);

        projection.reset_counters();
        assert_eq!(
            projection.exact_total_rows(&test_lua(), &mut transcript.history, 80),
            total
        );
        assert_eq!(
            projection.counters(),
            TranscriptProjectionCounters::default(),
            "repeated exact row-count queries should use the exact height index"
        );
    }

    #[test]
    fn tool_state_changes_invalidate_measured_rows() {
        let mut transcript = Transcript::new();
        let mut parser = StreamParser::new();
        let invocation_id = next_invocation_id();
        parser.start_tool(
            &mut transcript.history,
            ToolStart {
                invocation_id,
                call_id: "call-1".into(),
                name: "bash".into(),
                summary: protocol::StyledLines::from_plain("echo hi"),
                args: std::collections::HashMap::new(),
                preview_output: None,
                called_at_ms: 0,
            },
            std::time::Instant::now(),
        );
        let mut projection = TranscriptProjection::new();
        let pending_total = projection.exact_total_rows(&test_lua(), &mut transcript.history, 80);

        for line in ["first", "second", "third"] {
            parser.append_active_output_line(
                &mut transcript.history,
                invocation_id,
                line.to_string(),
            );
        }
        parser.set_active_status(
            &mut transcript.history,
            invocation_id,
            ToolStatus::Ok,
            std::time::Instant::now(),
        );
        projection.reset_counters();

        let finished_total = projection.exact_total_rows(&test_lua(), &mut transcript.history, 80);

        assert!(
            finished_total > pending_total,
            "finished tool output should add measured rows"
        );
        assert!(
            projection.counters().exact_height_measured_blocks > 0,
            "state changes must force row heights to be measured again"
        );
    }

    #[test]
    fn code_line_heights_measure_without_rendering_syntax() {
        let mut transcript = Transcript::new();
        for _ in 0..3 {
            transcript.push(Block::CodeLine {
                content: "x".repeat(40),
                lang: "rust".into(),
            });
        }
        let mut projection = TranscriptProjection::new();

        let total = projection.exact_total_rows(&test_lua(), &mut transcript.history, 10);

        assert_eq!(total, 12);
        assert_eq!(projection.layout_cache_len(), 3);
        let counters = projection.counters();
        assert_eq!(counters.full_row_builds, 0);
        assert_eq!(counters.layout_cache, 3);
        assert_eq!(counters.exact_height_measured_blocks, 3);
    }

    #[test]
    fn exact_total_rows_keeps_layout_cache_width_independent() {
        let mut projection = TranscriptProjection::new();
        let block_count = 537;
        let mut transcript = Transcript::new();
        for i in 0..block_count {
            transcript.push(Block::Text {
                content: format!("line {i}").into(),
            });
        }

        let total = projection.exact_total_rows(&test_lua(), &mut transcript.history, 80);

        assert_eq!(total, (block_count as RowIndex).saturating_mul(2) - 1);
        assert_eq!(projection.layout_cache_len(), block_count);
        let counters = projection.counters();
        assert_eq!(counters.full_row_builds, 0);
        assert_eq!(counters.layout_cache, block_count);
        assert_eq!(counters.exact_height_measured_blocks, block_count);

        projection.reset_counters();
        let total_narrow = projection.exact_total_rows(&test_lua(), &mut transcript.history, 40);
        assert!(total_narrow >= total);
        let counters = projection.counters();
        assert_eq!(counters.full_row_builds, 0);
        assert_eq!(
            counters.layout_cache, 0,
            "display layouts are width-independent and should not be recompiled"
        );
        assert_eq!(
            counters.exact_height_measured_blocks, block_count,
            "width change must remeasure all block heights"
        );
    }

    #[test]
    fn theme_invalidation_preserves_exact_measurements() {
        let lua = test_lua();
        let mut projection = TranscriptProjection::new();
        let mut transcript = Transcript::new();
        for i in 0..128 {
            transcript.push(Block::Text {
                content: format!("line {i} {}", "x".repeat(64)).into(),
            });
        }

        let total = projection.exact_total_rows(&lua, &mut transcript.history, 40);
        assert!(total > 0);
        projection.reset_counters();

        projection.invalidate_theme();
        assert_eq!(
            projection.exact_total_rows(&lua, &mut transcript.history, 40),
            total
        );
        assert_eq!(
            projection.counters().exact_height_measured_blocks,
            0,
            "theme changes must not discard exact row measurements"
        );
    }

    #[test]
    fn width_revisit_reuses_cached_exact_measurements() {
        let lua = test_lua();
        let mut projection = TranscriptProjection::new();
        let mut transcript = Transcript::new();
        for i in 0..128 {
            transcript.push(Block::Text {
                content: format!("line {i} {}", "x".repeat(64)).into(),
            });
        }

        let wide_total = projection.exact_total_rows(&lua, &mut transcript.history, 80);
        let narrow_total = projection.exact_total_rows(&lua, &mut transcript.history, 40);
        assert!(narrow_total >= wide_total);
        projection.reset_counters();

        assert_eq!(
            projection.exact_total_rows(&lua, &mut transcript.history, 80),
            wide_total
        );
        assert_eq!(
            projection.counters().exact_height_measured_blocks,
            0,
            "revisiting a measured width should hydrate the exact row index from memory"
        );
    }

    #[test]
    fn visible_width_change_remeasures_only_visible_window() {
        let mut projection = TranscriptProjection::new();
        let block_count = 512;
        let mut transcript = Transcript::new();
        for i in 0..block_count {
            transcript.push(Block::Text {
                content: format!("block {i} {}", "wrapped text ".repeat(12)).into(),
            });
        }
        let theme = Theme::default();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(31), Default::default());

        projection.exact_total_rows(&test_lua(), &mut transcript.history, 80);
        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_tail(),
            12,
        );
        projection.reset_counters();

        let out = projection.project(
            &mut buf,
            &mut transcript.history,
            40,
            &theme,
            ScrollTarget::visible_tail(),
            12,
        );

        assert!(buf.lines().iter().any(|line| line.contains("block 511")));
        assert!(out.materialized_rows <= 24);
        let counters = projection.counters();
        assert!(
            counters.exact_height_measured_blocks < block_count / 8,
            "visible width change should not remeasure every block: {counters:?}"
        );
        assert_eq!(
            counters.layout_cache, 0,
            "display layouts are width-independent across visible width changes"
        );
    }

    #[test]
    fn incremental_row_index_only_measures_appended_blocks() {
        let mut projection = TranscriptProjection::new();
        let mut transcript = Transcript::new();
        for i in 0..50 {
            transcript.push(Block::Text {
                content: format!("line {i}").into(),
            });
        }

        let total = projection.exact_total_rows(&test_lua(), &mut transcript.history, 80);
        assert_eq!(total, 99);
        let first_counters = projection.counters();
        assert_eq!(first_counters.exact_height_measured_blocks, 50);

        projection.reset_counters();
        for i in 50..100 {
            transcript.push(Block::Text {
                content: format!("line {i}").into(),
            });
        }
        let total_after = projection.exact_total_rows(&test_lua(), &mut transcript.history, 80);
        assert_eq!(total_after, 199);
        let second_counters = projection.counters();
        assert_eq!(
            second_counters.exact_height_measured_blocks, 50,
            "only appended blocks should be measured"
        );
        assert_eq!(
            second_counters.layout_cache, 50,
            "only appended blocks should be compiled"
        );
    }

    #[test]
    fn streaming_append_updates_retained_tail_without_recompiling() {
        let lua = test_lua();
        let mut transcript = Transcript::new();
        for i in 0..500 {
            transcript.push(Block::Text {
                content: format!("history line {i}").into(),
            });
        }
        transcript.push(Block::User {
            text: "continue".into(),
            image_labels: Vec::new(),
            command: false,
            sent_at_ms: None,
        });
        let mut parser = StreamParser::new();
        parser.append_streaming_text(&mut transcript.history, "streaming response\n");

        let mut projection = TranscriptProjection::new();
        projection.exact_total_rows(&lua, &mut transcript.history, 80);
        let initial_ids = projection.transcript_scene.ids().collect::<Vec<_>>();
        let initial_keys = projection
            .measurements
            .active
            .nodes
            .iter()
            .map(|node| node.key)
            .collect::<Vec<_>>();
        let tail_index = initial_ids.len().saturating_sub(1);
        projection.reset_counters();

        parser.append_streaming_text(&mut transcript.history, "more text\n");
        projection.exact_total_rows(&lua, &mut transcript.history, 80);

        let counters = projection.counters();
        assert_eq!(
            projection.transcript_scene.ids().collect::<Vec<_>>(),
            initial_ids
        );
        assert_eq!(
            projection.measurements.active.nodes.len(),
            initial_keys.len()
        );
        for (index, node) in projection.measurements.active.nodes.iter().enumerate() {
            if index != tail_index {
                assert_eq!(node.key, initial_keys[index]);
            }
        }
        assert_ne!(
            projection.measurements.active.nodes[tail_index].key,
            initial_keys[tail_index]
        );
        assert_eq!(counters.layout_cache, 0);
        assert_eq!(counters.exact_height_measured_blocks, 1);
    }

    #[test]
    fn finalized_streaming_text_uses_custom_renderer() {
        let lua = test_lua();
        lua.lua
            .load(
                r#"
                smelt.transcript.set_renderer(function(block, ctx)
                  if block.kind == "assistant" then
                    return smelt.layout.gutter(
                      smelt.layout.content(block.content_id, { format = "markdown" }),
                      { text = "custom:" }
                    )
                  end
                  return smelt.transcript.defaults.render(block, ctx)
                end, { cache_key = "test.finalized-stream:v1" })
                "#,
            )
            .exec()
            .expect("set renderer");
        let mut transcript = Transcript::new();
        transcript.push(Block::User {
            text: "continue".into(),
            image_labels: Vec::new(),
            command: false,
            sent_at_ms: None,
        });
        let mut parser = StreamParser::new();
        parser.append_streaming_text(&mut transcript.history, "streaming response\n");
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(102), Default::default());

        project_with_lua(
            &mut projection,
            &lua,
            &mut buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_tail(),
            20,
        );
        assert!(snapshot(&buf)
            .iter()
            .any(|row| row.line.contains("streaming response")));
        assert!(!snapshot(&buf)
            .iter()
            .any(|row| row.line.contains("custom:")));

        parser.flush_streaming_text(&mut transcript.history);
        projection.reset_counters();
        project_with_lua(
            &mut projection,
            &lua,
            &mut buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_tail(),
            20,
        );

        let final_rows = snapshot(&buf);
        assert!(
            final_rows
                .iter()
                .any(|row| row.line.contains("custom:streaming response")),
            "final rows: {final_rows:#?}"
        );
        assert_eq!(projection.counters().layout_cache, 1);
    }

    #[test]
    fn incremental_row_index_remeasures_rewritten_block_and_successor() {
        let mut projection = TranscriptProjection::new();
        let mut transcript = Transcript::new();
        for i in 0..50 {
            transcript.push(Block::Text {
                content: format!("line {i}").into(),
            });
        }
        projection.exact_total_rows(&test_lua(), &mut transcript.history, 80);
        projection.reset_counters();

        transcript.history.rewrite(
            transcript.history.order[10],
            Block::Text {
                content: "rewritten block with different height".into(),
            },
        );
        projection.exact_total_rows(&test_lua(), &mut transcript.history, 80);

        let counters = projection.counters();
        assert_eq!(
            counters.exact_height_measured_blocks, 2,
            "same-order rewrite should remeasure the changed block and following gap: {counters:?}"
        );
        assert_eq!(counters.layout_cache, 1);
    }

    #[test]
    fn incremental_row_index_rebuilds_when_order_prefix_changes() {
        let mut projection = TranscriptProjection::new();
        let mut transcript = Transcript::new();
        for i in 0..50 {
            transcript.push(Block::Text {
                content: format!("line {i}").into(),
            });
        }
        projection.exact_total_rows(&test_lua(), &mut transcript.history, 80);
        projection.reset_counters();

        transcript.history.order.remove(10);
        transcript.history.mark_changed();
        projection.exact_total_rows(&test_lua(), &mut transcript.history, 80);

        let counters = projection.counters();
        assert!(
            counters.exact_height_measured_blocks >= 39,
            "order-prefix change should force a rebuild from the changed point: {counters:?}"
        );
    }

    #[test]
    fn range_rows_reuse_exact_height_index_without_full_rows() {
        let mut transcript = Transcript::new();
        for i in 0..100 {
            transcript.push(Block::Text {
                content: format!("line {i}").into(),
            });
        }
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        assert_eq!(
            projection.exact_total_rows(&test_lua(), &mut transcript.history, 80),
            199
        );

        projection.reset_counters();
        let rows = projection.display_rows_for_range(
            &test_lua(),
            &mut transcript.history,
            80,
            &theme,
            150..153,
        );

        let text: Vec<_> = rows.rows.iter().map(|row| row.text.as_str()).collect();
        assert_eq!(text, vec!["line 75", "", "line 76"]);
        let counters = projection.counters();
        assert_eq!(counters.full_row_builds, 0);
        assert_eq!(counters.layout_cache, 0);
        assert_eq!(counters.exact_height_measured_blocks, 0);
        assert!(
            counters.range_materialized_blocks < transcript.history.order.len(),
            "range rows should materialize only intersecting blocks, got {counters:?}"
        );
    }

    #[test]
    fn copy_range_treats_doc_columns_as_bytes_for_multibyte_text() {
        let mut transcript = Transcript::new();
        let content = "alpha ’ beta gamma";
        transcript.push(Block::Text {
            content: content.into(),
        });
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();

        let copied = projection.copy_range(
            &test_lua(),
            &mut transcript.history,
            80,
            &theme,
            DocRange {
                start: crate::smelt_edit::DocPosition {
                    row: 0,
                    byte_col: "alpha ’ ".len(),
                },
                end: crate::smelt_edit::DocPosition {
                    row: 0,
                    byte_col: "alpha ’ beta".len(),
                },
            },
        );

        assert_eq!(copied.clipboard, "beta");
        assert_eq!(copied.kill_ring, "beta");
    }

    #[test]
    fn copy_range_reuses_exact_height_index_without_full_rows() {
        let mut transcript = Transcript::new();
        for i in 0..100 {
            transcript.push(Block::Text {
                content: format!("line {i}").into(),
            });
        }
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        assert_eq!(
            projection.exact_total_rows(&test_lua(), &mut transcript.history, 80),
            199
        );

        projection.reset_counters();
        let copied = projection.copy_range(
            &test_lua(),
            &mut transcript.history,
            80,
            &theme,
            DocRange {
                start: crate::smelt_edit::DocPosition {
                    row: 150,
                    byte_col: 0,
                },
                end: crate::smelt_edit::DocPosition {
                    row: 150,
                    byte_col: "line 75".len(),
                },
            },
        );

        assert_eq!(copied.clipboard, "line 75");
        assert_eq!(copied.kill_ring, "line 75");
        let counters = projection.counters();
        assert_eq!(counters.full_row_builds, 0);
        assert_eq!(counters.layout_cache, 0);
        assert_eq!(counters.exact_height_measured_blocks, 0);
        assert!(
            counters.range_materialized_blocks < transcript.history.order.len(),
            "copy should materialize only intersecting blocks, got {counters:?}"
        );
    }

    #[test]
    fn copy_large_range_streams_materialization_chunks() {
        let mut transcript = Transcript::new();
        for i in 0..180 {
            transcript.push(Block::Text {
                content: format!("line {i}").into(),
            });
        }
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let total_rows = projection.exact_total_rows(&test_lua(), &mut transcript.history, 80);

        projection.reset_counters();
        let copied = projection.copy_range(
            &test_lua(),
            &mut transcript.history,
            80,
            &theme,
            DocRange {
                start: crate::smelt_edit::DocPosition {
                    row: 0,
                    byte_col: 0,
                },
                end: crate::smelt_edit::DocPosition {
                    row: total_rows.saturating_sub(1),
                    byte_col: "line 179".len(),
                },
            },
        );

        let expected = (0..180)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        assert_eq!(copied.clipboard, expected);
        assert_eq!(copied.kill_ring, expected);
        let counters = projection.counters();
        assert_eq!(counters.full_row_builds, 0);
        assert!(
            counters.max_range_materialized_blocks <= COPY_CHUNK_NODES,
            "copy should never materialize the entire selected node set at once: {counters:?}"
        );
        assert!(
            counters.range_materialized_blocks >= transcript.history.order.len(),
            "large copy should stream through selected nodes: {counters:?}"
        );
    }

    #[test]
    fn visible_tail_projection_materializes_bounded_tail_window() {
        let mut transcript = Transcript::new();
        for i in 0..100 {
            transcript.push(Block::Text {
                content: format!("line {i}").into(),
            });
        }
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(12), Default::default());

        let output = projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_tail(),
            5,
        );

        assert!(output.row_base > 0);
        assert!(output.materialized_rows < output.total_rows);
        assert_eq!(output.materialized_rows, buf.line_count() as RowIndex);
        assert_eq!(output.clamped_scroll, output.total_rows.saturating_sub(5));
        assert!(buf.lines().iter().any(|line| line == "line 99"));
        assert!(!buf.lines().iter().any(|line| line == "line 0"));
        assert!(
            projection.counters().range_materialized_blocks < transcript.history.order.len(),
            "tail projection should materialize a bounded block range"
        );
    }

    #[test]
    fn visible_projection_preload_scales_with_viewport() {
        let mut transcript = Transcript::new();
        for i in 0..100 {
            transcript.push(Block::Text {
                content: format!("line {i}").into(),
            });
        }
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(18), Default::default());
        let viewport_rows = 10;
        let max_materialized = (viewport_rows as RowIndex).saturating_mul(2);

        let top = projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_row(0),
            viewport_rows,
        );
        assert!(
            top.materialized_rows <= max_materialized,
            "top projection should preload relative to viewport height: {top:?}"
        );
        assert!(buf.lines().iter().any(|line| line == "line 0"));

        let tail = projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_tail(),
            viewport_rows,
        );
        assert!(
            tail.materialized_rows <= max_materialized,
            "tail projection should preload relative to viewport height: {tail:?}"
        );
        assert_eq!(
            tail.clamped_scroll,
            tail.total_rows.saturating_sub(viewport_rows as RowIndex)
        );
        assert!(buf.lines().iter().any(|line| line == "line 99"));
    }

    #[test]
    fn range_rows_match_full_projection_slice() {
        let mut transcript = Transcript::new();
        for i in 0..20 {
            transcript.push(Block::Text {
                content: format!("line {i}").into(),
            });
        }
        let theme = Theme::default();
        let mut full_projection = TranscriptProjection::new();
        let mut full_buf = Buffer::new(crate::smelt_edit::BufId(21), Default::default());
        full_projection.project(
            &mut full_buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_row(0),
            10,
        );
        let expected = full_buf.lines()[5..12].to_vec();

        let mut range_projection = TranscriptProjection::new();
        let range = range_projection.display_rows_for_range(
            &test_lua(),
            &mut transcript.history,
            80,
            &theme,
            5..12,
        );

        let text: Vec<_> = range.rows.iter().map(|row| row.text.clone()).collect();
        assert_eq!(text, expected);
    }

    #[test]
    fn range_breaks_match_full_projection_for_wrapped_rows() {
        let mut transcript = Transcript::new();
        transcript.push(Block::Text {
            content: "one two three four five six seven eight nine ten".into(),
        });
        transcript.push(Block::Text {
            content: "after".into(),
        });
        let theme = Theme::default();
        let mut full_projection = TranscriptProjection::new();
        let mut full_buf = Buffer::new(crate::smelt_edit::BufId(22), Default::default());
        full_projection.project(
            &mut full_buf,
            &mut transcript.history,
            18,
            &theme,
            ScrollTarget::visible_row(0),
            20,
        );
        let mut range_projection = TranscriptProjection::new();
        let range = range_projection.display_rows_for_range(
            &test_lua(),
            &mut transcript.history,
            18,
            &theme,
            0..full_buf.line_count() as RowIndex,
        );
        assert!(
            !range.soft_breaks().is_empty(),
            "fixture should produce soft wraps"
        );

        let text: Vec<_> = range.rows.iter().map(|row| row.text.clone()).collect();
        assert_eq!(text, full_buf.lines().to_vec());
    }

    #[test]
    fn visible_tail_projection_uses_measured_prefix_heights() {
        let mut transcript = Transcript::new();
        for i in 0..40 {
            transcript.push(Block::Text {
                content: format!("block {i}\ncontinued {i}").into(),
            });
        }
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let full_rows = projection
            .build_rows(&test_lua(), &mut transcript.history, 80, &theme)
            .len() as RowIndex;
        let mut buf = Buffer::new(crate::smelt_edit::BufId(5), Default::default());

        let output = projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_tail(),
            5,
        );

        assert!(output.row_base > 0);
        assert!(output.materialized_rows < full_rows);
        assert_eq!(output.materialized_rows, buf.line_count() as RowIndex);
        assert_eq!(output.total_rows, full_rows);
        assert_eq!(output.clamped_scroll, full_rows.saturating_sub(5));
        assert!(buf.lines().iter().any(|line| line == "block 39"));
        assert!(!buf.lines().iter().any(|line| line == "block 0"));
    }

    #[test]
    fn cold_tail_projection_refines_visible_heights_without_global_measurement() {
        let mut transcript = Transcript::new();
        for i in 0..120 {
            let content = (0..5)
                .map(|j| format!("block {i} line {j}"))
                .collect::<Vec<_>>()
                .join("\n");
            transcript.push(Block::Text {
                content: content.into(),
            });
        }
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(14), Default::default());
        let exact_total = 120 * 5 + 119;

        let tail = projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_tail(),
            10,
        );

        assert!(tail.total_rows >= exact_total);
        assert!(tail.total_rows > 10);
        assert_eq!(tail.clamped_scroll, tail.total_rows.saturating_sub(10));
        assert!(buf.lines().iter().any(|line| line == "block 119 line 4"));
        assert!(!buf.lines().iter().any(|line| line == "block 0 line 0"));
        assert!(
            projection.counters().exact_height_measured_blocks < transcript.history.order.len(),
            "cold tail projection should not measure every block: {:?}",
            projection.counters()
        );

        let target = tail.clamped_scroll.saturating_sub(30);
        assert!(target > 0);
        let scrolled = projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_row(target),
            10,
        );

        assert_eq!(scrolled.clamped_scroll, target);
        assert!(scrolled.row_base > 0);
        assert!(!buf.lines().iter().any(|line| line == "block 0 line 0"));
    }

    #[test]
    fn visible_projection_materializes_tall_output_block_when_scrolled_inside_it() {
        let mut transcript = Transcript::new();
        for i in 0..5 {
            transcript.push(Block::Text {
                content: format!("before {i}").into(),
            });
        }
        transcript.push(Block::Text {
            content: (0..80)
                .map(|i| format!("tool output line {i}"))
                .collect::<Vec<_>>()
                .join("\n")
                .into(),
        });
        for i in 0..20 {
            transcript.push(Block::Text {
                content: format!("after {i}").into(),
            });
        }
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(15), Default::default());

        let scroll_top = 10 + 50;
        let output = projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_row(scroll_top),
            8,
        );

        assert_eq!(output.clamped_scroll, scroll_top);
        assert!(output.row_base <= scroll_top);
        assert!(output.row_base.saturating_add(output.materialized_rows) >= scroll_top + 8);
        assert!(buf.lines().iter().any(|line| line == "tool output line 50"));
        assert!(!buf.lines().iter().any(|line| line == "after 0"));
    }

    #[test]
    fn visible_projection_covers_viewport_crossing_tall_block_boundary() {
        let mut transcript = Transcript::new();
        transcript.push(Block::Text {
            content: (0..80)
                .map(|i| format!("tool output line {i}"))
                .collect::<Vec<_>>()
                .join("\n")
                .into(),
        });
        transcript.push(Block::Text {
            content: (0..10)
                .map(|i| format!("after boundary line {i}"))
                .collect::<Vec<_>>()
                .join("\n")
                .into(),
        });
        let after_id = *transcript.history.order.last().expect("after block id");
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(16), Default::default());
        let after_start = projection
            .materialize_block_layout(&test_lua(), &mut transcript.history, 80)
            .into_iter()
            .find(|(id, _, _)| *id == after_id)
            .map(|(_, start, _)| start)
            .expect("after block layout");
        let scroll_top = after_start.saturating_sub(2);
        let viewport_rows = 5;

        let output = projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_row(scroll_top),
            viewport_rows,
        );

        assert_eq!(output.clamped_scroll, scroll_top);
        assert!(output.row_base <= scroll_top);
        assert!(
            output.row_base.saturating_add(output.materialized_rows)
                >= scroll_top + viewport_rows as RowIndex
        );
        assert!(buf
            .lines()
            .iter()
            .any(|line| line == "after boundary line 0"));
    }

    #[test]
    fn visible_projection_keeps_pinned_row_stable_while_tail_streams() {
        let mut transcript = Transcript::new();
        for i in 0..30 {
            let content = (0..5)
                .map(|j| format!("block {i} line {j}"))
                .collect::<Vec<_>>()
                .join("\n");
            transcript.push(Block::Text {
                content: content.into(),
            });
        }
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(13), Default::default());
        let anchor_id = transcript.history.order[10];
        let anchor_row = projection
            .materialize_block_layout(&test_lua(), &mut transcript.history, 80)
            .into_iter()
            .find(|(id, _, _)| *id == anchor_id)
            .map(|(_, start, _)| start)
            .expect("anchor block layout");

        let before = projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_row(anchor_row),
            5,
        );
        assert!(buf.lines().iter().any(|line| line == "block 10 line 0"));

        let tail_id = *transcript.history.order.last().expect("tail block");
        transcript.history.rewrite(
            tail_id,
            Block::Text {
                content: format!("{}\nstreamed tail line", "tail\n".repeat(20)).into(),
            },
        );
        let after = projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_row(anchor_row),
            5,
        );

        assert_eq!(after.clamped_scroll, before.clamped_scroll);
        assert_eq!(after.row_base, before.row_base);
        assert!(buf.lines().iter().any(|line| line == "block 10 line 0"));
    }

    #[test]
    fn cached_tail_projection_rewrites_mutated_target_buffer() {
        let mut transcript = Transcript::new();
        for i in 0..40 {
            transcript.push(Block::Text {
                content: format!("line {i}").into(),
            });
        }
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(8), Default::default());
        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_tail(),
            5,
        );

        buf.set_all_lines(vec!["other session".into()]);
        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_tail(),
            5,
        );

        assert!(buf.lines().iter().any(|line| line == "line 39"));
        assert!(!buf.lines().iter().any(|line| line == "other session"));
    }

    #[test]
    fn cached_tail_projection_rewrites_new_target_buffer() {
        let mut transcript = Transcript::new();
        for i in 0..40 {
            transcript.push(Block::Text {
                content: format!("line {i}").into(),
            });
        }
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut first_buf = Buffer::new(crate::smelt_edit::BufId(8), Default::default());
        projection.project(
            &mut first_buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_tail(),
            5,
        );

        let mut second_buf = Buffer::new(crate::smelt_edit::BufId(9), Default::default());
        second_buf.set_all_lines(vec!["other session".into()]);
        projection.project(
            &mut second_buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_tail(),
            5,
        );

        assert!(second_buf.lines().iter().any(|line| line == "line 39"));
        assert!(!second_buf
            .lines()
            .iter()
            .any(|line| line == "other session"));
    }

    #[test]
    fn cached_projection_can_be_reused_after_shared_preview_buffer_switches_sessions() {
        let mut first = Transcript::new();
        for i in 0..40 {
            first.push(Block::Text {
                content: format!("first {i}").into(),
            });
        }
        let mut second = Transcript::new();
        for i in 0..40 {
            second.push(Block::Text {
                content: format!("second {i}").into(),
            });
        }
        let theme = Theme::default();
        let mut first_projection = TranscriptProjection::new();
        let mut second_projection = TranscriptProjection::new();
        let mut shared = Buffer::new(crate::smelt_edit::BufId(11), Default::default());

        first_projection.project(
            &mut shared,
            &mut first.history,
            80,
            &theme,
            ScrollTarget::visible_tail(),
            5,
        );
        second_projection.project(
            &mut shared,
            &mut second.history,
            80,
            &theme,
            ScrollTarget::visible_tail(),
            5,
        );

        first_projection.project(
            &mut shared,
            &mut first.history,
            80,
            &theme,
            ScrollTarget::visible_tail(),
            5,
        );

        assert!(shared.lines().iter().any(|line| line == "first 39"));
        assert!(!shared
            .lines()
            .iter()
            .any(|line| line.starts_with("second ")));
    }

    #[test]
    fn materialized_block_layout_is_exact_after_tail_projection() {
        let mut transcript = Transcript::new();
        for i in 0..40 {
            transcript.push(Block::Text {
                content: format!("line {i}").into(),
            });
        }
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(6), Default::default());

        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_tail(),
            5,
        );
        let visible_count = projection.visible_block_layout().count();
        assert!(visible_count < transcript.history.order.len());

        let layout = projection.materialize_block_layout(&test_lua(), &mut transcript.history, 80);
        assert_eq!(layout.len(), transcript.history.order.len());
        assert_eq!(layout.first().map(|(_, start, _)| *start), Some(0));
        assert_eq!(layout.last().map(|(_, _, rows)| *rows), Some(1));
        assert_eq!(projection.visible_block_layout().count(), visible_count);
    }

    #[test]
    fn visible_tail_uses_exact_total_after_full_row_materialization() {
        let mut transcript = Transcript::new();
        for i in 0..40 {
            let lines = (0..10)
                .map(|j| format!("block {i} line {j}"))
                .collect::<Vec<_>>()
                .join("\n");
            transcript.push(Block::Text {
                content: lines.into(),
            });
        }
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let full_rows = projection.build_rows(&test_lua(), &mut transcript.history, 80, &theme);
        assert_eq!(full_rows.len() as RowIndex, 439);
        let mut buf = Buffer::new(crate::smelt_edit::BufId(7), Default::default());

        let tail = projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_tail(),
            5,
        );
        assert_eq!(tail.total_rows, 439);
        assert!(tail.row_base > 0);
        assert!(tail.materialized_rows < tail.total_rows);
        assert_eq!(tail.materialized_rows, buf.line_count() as RowIndex);
        assert_eq!(tail.clamped_scroll, tail.total_rows.saturating_sub(5));
        assert!(buf.lines().iter().any(|line| line == "block 39 line 9"));
        assert!(!buf.lines().iter().any(|line| line == "block 0 line 0"));

        let top = projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_row(0),
            5,
        );
        assert_eq!(top.row_base, 0);
        assert!(top.materialized_rows < top.total_rows);
        assert!(buf.lines().iter().any(|line| line == "block 0 line 0"));
        assert!(!buf.lines().iter().any(|line| line == "block 39 line 9"));
    }

    #[test]
    fn visible_projection_preserves_rendered_row_anchor_across_width_change() {
        let mut transcript = Transcript::new();
        transcript.push(Block::Text {
            content: format!("ANCHOR\n{}\nafter", "before wrapping content ".repeat(6)).into(),
        });
        for i in 0..20 {
            transcript.push(Block::Text {
                content: format!("tail {i}").into(),
            });
        }
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(24), Default::default());
        let old_width = 30;
        let new_width = 14;
        let viewport_rows = 5;
        let old_rows =
            projection.build_rows(&test_lua(), &mut transcript.history, old_width, &theme);
        let anchor_row = old_rows
            .iter()
            .position(|line| line.contains("ANCHOR"))
            .expect("anchor line") as RowIndex;

        let before = projection.project(
            &mut buf,
            &mut transcript.history,
            old_width,
            &theme,
            ScrollTarget::visible_row(anchor_row),
            viewport_rows,
        );
        let before_local = row_to_usize(before.clamped_scroll.saturating_sub(before.row_base));
        assert!(buf
            .get_line(before_local)
            .is_some_and(|line| line.contains("ANCHOR")));

        let after = projection.project(
            &mut buf,
            &mut transcript.history,
            new_width,
            &theme,
            ScrollTarget::visible_row(anchor_row),
            viewport_rows,
        );
        let after_local = row_to_usize(after.clamped_scroll.saturating_sub(after.row_base));

        assert!(
            buf.get_line(after_local)
                .is_some_and(|line| line.contains("ANCHOR")),
            "width shrink should keep the same rendered content at viewport top; got {:?}",
            buf.get_line(after_local)
        );

        let widened = projection.project(
            &mut buf,
            &mut transcript.history,
            old_width,
            &theme,
            ScrollTarget::visible_row(after.clamped_scroll),
            viewport_rows,
        );
        let widened_local = row_to_usize(widened.clamped_scroll.saturating_sub(widened.row_base));

        assert!(
            buf.get_line(widened_local)
                .is_some_and(|line| line.contains("ANCHOR")),
            "width expansion should keep the same rendered content at viewport top; got {:?}",
            buf.get_line(widened_local)
        );
    }

    #[test]
    fn visible_projection_preserves_block_anchor_across_width_change() {
        let mut transcript = Transcript::new();
        for i in 0..20 {
            transcript.push(Block::Text {
                content: format!("block {i} {}", "wrapped text ".repeat(20)).into(),
            });
        }
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(10), Default::default());

        let anchor_id = transcript.history.order[10];
        let anchor_row = projection
            .materialize_block_layout(&test_lua(), &mut transcript.history, 80)
            .into_iter()
            .find(|(id, _, _)| *id == anchor_id)
            .map(|(_, start, _)| start)
            .expect("anchor block layout");

        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_row(anchor_row),
            5,
        );
        assert!(buf.lines().iter().any(|line| line.contains("block 10")));

        projection.project(
            &mut buf,
            &mut transcript.history,
            24,
            &theme,
            ScrollTarget::visible_reflow_stable_row(anchor_row),
            5,
        );
        assert!(buf.lines().iter().any(|line| line.contains("block 10")));
    }

    fn next_u64(state: &mut u64) -> u64 {
        let mut x = *state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *state = x;
        x
    }

    fn randomish_text(seed: &mut u64, words: usize) -> String {
        const WORDS: &[&str] = &[
            "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta", "iota", "kappa",
            "lambda", "mu", "nu", "xi", "omicron", "pi",
        ];
        (0..words)
            .map(|_| WORDS[(next_u64(seed) as usize) % WORDS.len()])
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn randomish_block(i: usize, seed: &mut u64) -> Block {
        match (next_u64(seed) % 7) as usize {
            0 => Block::User {
                text: randomish_text(seed, 12),
                image_labels: vec![],
                command: false,
                sent_at_ms: None,
            },
            1 => Block::Text {
                content: format!(
                    "# Heading {i}\n\n{}\n\n- {}\n- `code {i}` and **bold**",
                    randomish_text(seed, 18),
                    randomish_text(seed, 8)
                )
                .into(),
            },
            2 => Block::Thinking {
                title: None,
                summary_titles: Vec::new(),
                kind: protocol::ReasoningKind::Raw,
                content: randomish_text(seed, 20).into(),
            },
            3 => Block::CodeLine {
                content: format!("let value_{i} = {};", next_u64(seed) % 10_000),
                lang: "rust".into(),
            },
            4 => Block::Exec {
                command: format!("echo {i}"),
                output: format!("{}\n{}", randomish_text(seed, 8), randomish_text(seed, 10)).into(),
            },
            5 => Block::Compacted {
                summary: randomish_text(seed, 24),
            },
            _ => Block::ProcessStatus {
                text: format!("process status {i}: {}", randomish_text(seed, 6)),
                event: None,
            },
        }
    }

    #[test]
    fn current_projection_range_matches_full_rows_for_randomish_blocks_and_widths() {
        for width in [18, 31, 80] {
            let mut seed = 0x5eed_u64 + width as u64;
            let mut transcript = Transcript::new();
            for i in 0..96 {
                transcript.push(randomish_block(i, &mut seed));
            }
            transcript.push(Block::Mode {
                text: "now in apply mode".into(),
                icon: "● ".into(),
                hl_group: "SmeltModeApply".into(),
            });

            let theme = Theme::default();
            let mut projection = TranscriptProjection::new();
            let measured = projection.exact_total_rows(&test_lua(), &mut transcript.history, width);
            let full_rows =
                projection.build_rows(&test_lua(), &mut transcript.history, width, &theme);
            assert_eq!(measured as usize, full_rows.len(), "width {width}");

            let range_rows = projection.display_rows_for_range(
                &test_lua(),
                &mut transcript.history,
                width,
                &theme,
                0..measured,
            );
            let range_text: Vec<_> = range_rows
                .rows
                .iter()
                .map(|row| row.text.as_str())
                .collect();
            let full_text: Vec<_> = full_rows.iter().map(String::as_str).collect();
            assert_eq!(range_text, full_text, "width {width}");
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn project_with_lua(
        projection: &mut TranscriptProjection,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        buf: &mut Buffer,
        history: &mut BlockHistory,
        width: u16,
        theme: &Theme,
        scroll_target: ScrollTarget,
        viewport_rows: u16,
    ) -> MaterializedRows {
        let plan = projection.plan_projection_measured(
            lua,
            history,
            width,
            theme,
            scroll_target,
            viewport_rows,
        );
        projection.project_planned(lua, buf, history, theme, plan)
    }

    #[cfg(feature = "transcript-bench")]
    #[rustfmt::skip]
    pub(crate) mod benchmark_support {
        use super::*;

    fn push_large_mixed_transcript_fixture(
        transcript: &mut Transcript,
        target_bytes: usize,
    ) -> usize {
        let mut approx_bytes = 0usize;
        let mut i = 0usize;
        while approx_bytes < target_bytes {
            let user = format!(
                "Investigate transcript layout batch {i}. {}",
                "preserve exact copy navigation scrollbar resize preview ".repeat(8)
            );
            approx_bytes += user.len();
            transcript.push(Block::User {
                text: user,
                image_labels: vec![],
                command: false,
                sent_at_ms: None,
            });

            let markdown = large_mixed_markdown_payload(i);
            approx_bytes += markdown.len();
            transcript.push(Block::Text {
                content: markdown.into(),
            });

            if i.is_multiple_of(5) {
                let reasoning = format!(
                    "{}\n{}",
                    "consider cached width-independent measurement ".repeat(20),
                    "validate row-count and copy-source equivalence ".repeat(20)
                );
                approx_bytes += reasoning.len();
                transcript.push(Block::Thinking {
                    title: None,
                    summary_titles: Vec::new(),
                    content: reasoning.into(),
                    kind: protocol::ReasoningKind::Raw,
                });
            }

            if i.is_multiple_of(7) {
                let command = format!("python scripts/analyze_layout.py --batch {i}");
                let output = (0..24)
                    .map(|j| {
                        format!(
                            "result {i}.{j}: {}",
                            "tool output wraps and truncates ".repeat(10)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                approx_bytes += command.len() + output.len();
                transcript.push(Block::Exec {
                    command,
                    output: output.into(),
                });
            }

            if i.is_multiple_of(11) {
                let call_id = format!("mixed-fixture-call-{i}");
                let output = (0..80)
                    .map(|j| {
                        format!(
                            "tool line {i}.{j}: {}",
                            "visible materialization ".repeat(12)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                approx_bytes += output.len();
                transcript.push_tool_call(
                    Block::ToolCall {
                        call_id: call_id.clone(),
                        name: "edit_file".into(),
                        summary: protocol::StyledLines::from_plain(format!("edit mixed_{i}.rs")),
                        args: std::collections::HashMap::new().into(),
                    },
                    ToolState {
                        status: ToolStatus::Ok,
                        elapsed: Some(std::time::Duration::from_millis(1_250)),
                        called_at_ms: None,
                        elapsed_active: false,
                        output: Some(Box::new(ToolOutput {
                            content: output.into(),
                            is_error: false,
                            metadata: None,
                            content_fields: Vec::new(),
                        })),
                        user_message: None,
                        preview_output: None,
                    },
                );
            }

            i += 1;
        }
        approx_bytes
    }

    fn large_mixed_markdown_payload(i: usize) -> String {
        format!(
            "# Batch {i}\n\n{}\n\n| file | rows | notes |\n| --- | ---: | --- |\n| transcript_{i}.rs | {} | {} |\n| render_{i}.rs | {} | {} |\n\n```rust\nfn batch_{i}() {{\n    let rows = {};\n    println!(\"{{rows}}\");\n}}\n```\n\n- {}\n- {}",
            "markdown paragraphs with inline `code`, **bold spans**, links, and wrap pressure ".repeat(18),
            i * 3 + 1,
            "table cells wrap under preview width ".repeat(8),
            i * 3 + 2,
            "copy source must remain markdown exact ".repeat(8),
            i * 17,
            "resize should preserve visible block anchors ".repeat(10),
            "first render should not materialize irrelevant rows ".repeat(10),
        )
    }

    fn approx_history_bytes(history: &BlockHistory) -> usize {
        let mut bytes = 0usize;
        for id in &history.order {
            if let Some(block) = history.block(*id) {
                bytes += block.raw_text().map_or(0, |text| text.len());
                if let Block::ToolCall { .. } = block {
                    if let Some(state) = history.tool_state(*id) {
                        bytes += state
                            .output
                            .as_ref()
                            .map_or(0, |output| output.content.len());
                    }
                }
            }
        }
        bytes
    }

    fn push_markdown_heavy_transcript_fixture(
        transcript: &mut Transcript,
        target_bytes: usize,
    ) -> usize {
        let mut approx_bytes = 0usize;
        let mut i = 0usize;
        while approx_bytes < target_bytes {
            let content = format!(
                "# Markdown-heavy document {i}\n\n{}\n\n## Table\n\n| column | value | notes |\n| --- | ---: | --- |\n| alpha | {} | {} |\n| beta | {} | {} |\n\n## Code\n\n```rust\nfn markdown_heavy_{i}() -> usize {{\n    let mut total = 0;\n    for n in 0..{} {{ total += n; }}\n    total\n}}\n```\n\n{}\n\n{}",
                "Paragraph with links, `inline code`, emphasis, and wrap pressure. ".repeat(42),
                i * 11 + 1,
                "table cells intentionally contain enough content to wrap at narrow widths ".repeat(18),
                i * 11 + 2,
                "copy/search/source exactness must survive markdown measurement ".repeat(18),
                i % 127 + 32,
                "- bullet item with long content ".repeat(44),
                "> quoted reasoning line ".repeat(36),
            );
            approx_bytes += content.len();
            transcript.push(Block::Text {
                content: content.into(),
            });
            i += 1;
        }
        approx_bytes
    }

    fn push_tool_output_heavy_transcript_fixture(
        transcript: &mut Transcript,
        target_bytes: usize,
    ) -> usize {
        let mut approx_bytes = 0usize;
        let mut i = 0usize;
        while approx_bytes < target_bytes {
            let call_id = format!("tool-output-heavy-{i}");
            let command = format!("cargo test package_{i} -- --nocapture");
            let output = (0..160)
                .map(|j| {
                    format!(
                        "tool output line {i}.{j}: {}",
                        "ansi-free terminal output wraps exactly and keeps tail caps honest "
                            .repeat(8)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            approx_bytes += command.len() + output.len();
            transcript.push_tool_call(
                Block::ToolCall {
                    call_id: call_id.clone(),
                    name: "bash".into(),
                    summary: protocol::StyledLines::from_plain(command),
                    args: std::collections::HashMap::new().into(),
                },
                ToolState {
                    status: ToolStatus::Ok,
                    elapsed: Some(std::time::Duration::from_millis(2_000 + i as u64)),
                    called_at_ms: None,
                    elapsed_active: false,
                    output: Some(Box::new(ToolOutput {
                        content: output.into(),
                        is_error: false,
                        metadata: None,
                        content_fields: Vec::new(),
                    })),
                    user_message: None,
                    preview_output: None,
                },
            );
            i += 1;
        }
        approx_bytes
    }

    fn push_many_tiny_blocks_transcript_fixture(
        transcript: &mut Transcript,
        target_bytes: usize,
    ) -> usize {
        let mut approx_bytes = 0usize;
        let mut i = 0usize;
        while approx_bytes < target_bytes {
            let text = format!("tiny block {i} alpha beta gamma");
            approx_bytes += text.len();
            match i % 6 {
                0 => transcript.push(Block::User {
                    text,
                    image_labels: vec![],
                    command: false,
                    sent_at_ms: None,
                }),
                1 => transcript.push(Block::Text {
                    content: text.into(),
                }),
                2 => transcript.push(Block::CodeLine {
                    content: format!("let tiny_{i} = {i};"),
                    lang: "rust".into(),
                }),
                3 => transcript.push(Block::Thinking {
                    title: None,
                    summary_titles: Vec::new(),
                    content: text.into(),
                    kind: protocol::ReasoningKind::Raw,
                }),
                4 => transcript.push(Block::Compacted { summary: text }),
                _ => transcript.push(Block::ProcessStatus { text, event: None }),
            }
            i += 1;
        }
        approx_bytes
    }

    fn push_few_huge_blocks_transcript_fixture(
        transcript: &mut Transcript,
        target_bytes: usize,
    ) -> usize {
        let mut approx_bytes = 0usize;
        let mut i = 0usize;
        while approx_bytes < target_bytes {
            let content = format!(
                "# Huge block {i}\n\n{}\n\n```text\n{}\n```\n\n{}",
                "large paragraph with wrapping pressure and markdown spans `code` **bold** ".repeat(900),
                "preformatted output still contributes exact rows without visible materialization\n".repeat(420),
                "closing paragraph ".repeat(700),
            );
            approx_bytes += content.len();
            transcript.push(Block::Text {
                content: content.into(),
            });
            i += 1;
        }
        approx_bytes
    }

    #[derive(Clone, Copy)]
    struct TranscriptBenchWorkload {
        name: &'static str,
        target_bytes: usize,
        build: fn(&mut Transcript, usize) -> usize,
    }

    #[derive(Clone, Copy, Debug)]
    struct TranscriptBenchSample {
        input_bytes: usize,
        generated_bytes: usize,
        blocks: usize,
        total_rows: RowIndex,
        first_ms: f64,
        resize_ms: f64,
        theme_ms: f64,
        scroll12_ms: f64,
        visible_ms: f64,
        copy_ms: f64,
        append_ms: f64,
        no_cache_ms: f64,
        allocs: u64,
        bytes_allocated: u64,
        alloc_current_bytes: usize,
        alloc_peak_bytes: usize,
        visible_rows: usize,
        copied_rows: RowIndex,
        appended_rows: RowIndex,
        scroll_materialized_rows: u64,
        first_counters: TranscriptProjectionCounters,
        resize_counters: TranscriptProjectionCounters,
        theme_counters: TranscriptProjectionCounters,
        scroll_counters: TranscriptProjectionCounters,
        visible_counters: TranscriptProjectionCounters,
        copy_counters: TranscriptProjectionCounters,
        append_counters: TranscriptProjectionCounters,
        no_cache_counters: TranscriptProjectionCounters,
    }

    #[derive(Clone, Copy, Debug)]
    struct MetricStats {
        mean: f64,
        stddev: f64,
        min: f64,
        max: f64,
    }

    impl MetricStats {
        fn from(values: &[f64]) -> Self {
            let mean = values.iter().sum::<f64>() / values.len() as f64;
            let variance = if values.len() > 1 {
                values
                    .iter()
                    .map(|value| {
                        let delta = value - mean;
                        delta * delta
                    })
                    .sum::<f64>()
                    / (values.len() - 1) as f64
            } else {
                0.0
            };
            let min = values
                .iter()
                .copied()
                .fold(f64::INFINITY, |acc, value| acc.min(value));
            let max = values
                .iter()
                .copied()
                .fold(f64::NEG_INFINITY, |acc, value| acc.max(value));
            Self {
                mean,
                stddev: variance.sqrt(),
                min,
                max,
            }
        }

        fn display(self) -> String {
            format!("{:.1}±{:.1}", self.mean, self.stddev)
        }
    }

    fn elapsed_ms(elapsed: std::time::Duration) -> f64 {
        elapsed.as_secs_f64() * 1_000.0
    }

    fn counters_json(counters: TranscriptProjectionCounters) -> String {
        format!(
            "{{\"full_row_builds\":{},\"layout_cache\":{},\"exact_height_measured_blocks\":{},\"range_materialized_blocks\":{},\"max_range_materialized_blocks\":{},\"range_materialized_rows\":{},\"max_range_materialized_rows\":{}}}",
            counters.full_row_builds,
            counters.layout_cache,
            counters.exact_height_measured_blocks,
            counters.range_materialized_blocks,
            counters.max_range_materialized_blocks,
            counters.range_materialized_rows,
            counters.max_range_materialized_rows,
        )
    }

    fn selectable_copy_range_from_rows(
        start_row: RowIndex,
        rows: &DisplayRows,
        total_rows: RowIndex,
    ) -> Option<DocRange> {
        rows.rows.iter().enumerate().find_map(|(offset, row)| {
            let selectable = row
                .selectable_ranges
                .iter()
                .find(|range| range.start < range.end)?;
            let row_idx = start_row.saturating_add(offset as RowIndex);
            Some(DocRange {
                start: crate::smelt_edit::DocPosition {
                    row: row_idx,
                    byte_col: selectable.start,
                },
                end: crate::smelt_edit::DocPosition {
                    row: row_idx.saturating_add(79).min(total_rows.saturating_sub(1)),
                    byte_col: usize::MAX,
                },
            })
        })
    }

    fn find_selectable_copy_range(
        projection: &mut TranscriptProjection,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        history: &mut BlockHistory,
        width: u16,
        theme: &Theme,
        preferred_row: RowIndex,
        total_rows: RowIndex,
    ) -> Option<DocRange> {
        if total_rows == 0 {
            return None;
        }
        let chunk = 80;
        let start = preferred_row.min(total_rows.saturating_sub(1));
        for row in (start..total_rows).step_by(chunk as usize) {
            let rows = projection.display_rows_for_range(
                lua,
                history,
                width,
                theme,
                row..row.saturating_add(chunk).min(total_rows),
            );
            if let Some(range) = selectable_copy_range_from_rows(row, &rows, total_rows) {
                if !projection
                    .copy_range(lua, history, width, theme, range)
                    .kill_ring
                    .is_empty()
                {
                    return Some(range);
                }
            }
        }
        let mut row = start;
        while row > 0 {
            row = row.saturating_sub(chunk).min(row);
            let rows = projection.display_rows_for_range(
                lua,
                history,
                width,
                theme,
                row..row.saturating_add(chunk).min(total_rows),
            );
            if let Some(range) = selectable_copy_range_from_rows(row, &rows, total_rows) {
                if !projection
                    .copy_range(lua, history, width, theme, range)
                    .kill_ring
                    .is_empty()
                {
                    return Some(range);
                }
            }
        }
        None
    }

    fn perf_value_max(snapshot: &smelt_perf::perf::Snapshot, label: &str) -> u64 {
        snapshot
            .values
            .iter()
            .find(|row| row.label == label)
            .map(|row| row.max)
            .unwrap_or(0)
    }

    fn assert_resume_tail_perf_gates(snapshot: &smelt_perf::perf::Snapshot, history_items: usize) {
        for label in [
            "store:history:read_all",
            "store:history:read_all_rows",
            "store:session:load_full_snapshot",
            "store:session:full_snapshot_rows_read",
            "store:transcript:read_records_full",
            "store:transcript:records_full_loaded",
            "transcript:build_from_session:history_items",
        ] {
            let value = perf_value_max(snapshot, label);
            assert_eq!(
                value, 0,
                "display-only resume recorded {label}={value}, expected no full-session work"
            );
        }
        let loaded = perf_value_max(snapshot, "store:transcript:records_loaded");
        assert!(
            loaded <= 256,
            "display-only resume loaded {loaded} records from {history_items} history items"
        );
        let record_total = perf_value_max(snapshot, "transcript:sqlite:record_total");
        assert!(
            record_total >= history_items as u64 / 2,
            "display-only resume did not observe total record count: {record_total} for {history_items} history items"
        );
    }

    fn resume_perf_metric(label: &str) -> bool {
        [
            "transcript:resume_tail",
            "transcript:record_window",
            "store:db",
            "store:transcript",
        ]
        .iter()
        .any(|prefix| label.starts_with(prefix))
    }

    fn print_resume_perf_snapshot(snapshot: &smelt_perf::perf::Snapshot) {
        for row in snapshot
            .durations
            .iter()
            .filter(|row| resume_perf_metric(row.label))
        {
            eprintln!(
                "TRANSCRIPT_RESUME_PERF_DURATION metric={} count={} last_us={} total_us={} p95_us={} max_us={}",
                row.label, row.count, row.last_us, row.total_us, row.p95_us, row.max_us
            );
            eprintln!(
                "TRANSCRIPT_RESUME_PERF_DURATION_JSON {{\"type\":\"resume_perf_duration\",\"metric\":\"{}\",\"count\":{},\"last_us\":{},\"total_us\":{},\"p95_us\":{},\"max_us\":{}}}",
                row.label, row.count, row.last_us, row.total_us, row.p95_us, row.max_us
            );
        }
        for row in snapshot
            .values
            .iter()
            .filter(|row| resume_perf_metric(row.label))
        {
            eprintln!(
                "TRANSCRIPT_RESUME_PERF_VALUE metric={} count={} last={} total={} p95={} max={}",
                row.label, row.count, row.last, row.total, row.p95, row.max
            );
            eprintln!(
                "TRANSCRIPT_RESUME_PERF_VALUE_JSON {{\"type\":\"resume_perf_value\",\"metric\":\"{}\",\"count\":{},\"last\":{},\"total\":{},\"p95\":{},\"max\":{}}}",
                row.label, row.count, row.last, row.total, row.p95, row.max
            );
        }
    }

    fn print_layout_alloc_snapshot(workload: &str, snapshot: &smelt_perf::perf::Snapshot) {
        for row in snapshot
            .allocs
            .iter()
            .filter(|row| row.label.starts_with("transcript:"))
        {
            eprintln!(
                "TRANSCRIPT_LAYOUT_ALLOC workload={} metric={} count={} allocs_last={} allocs_total={} bytes_last={} bytes_total={} bytes_p95={} bytes_max={}",
                workload,
                row.label,
                row.count,
                row.allocs_last,
                row.allocs_total,
                row.bytes_last,
                row.bytes_total,
                row.bytes_p95,
                row.bytes_max
            );
            eprintln!(
                "TRANSCRIPT_LAYOUT_ALLOC_JSON {{\"type\":\"layout_alloc\",\"workload\":\"{}\",\"metric\":\"{}\",\"count\":{},\"allocs_last\":{},\"allocs_total\":{},\"bytes_last\":{},\"bytes_total\":{},\"bytes_p95\":{},\"bytes_max\":{}}}",
                workload,
                row.label,
                row.count,
                row.allocs_last,
                row.allocs_total,
                row.bytes_last,
                row.bytes_total,
                row.bytes_p95,
                row.bytes_max
            );
        }
    }

    fn transcript_bench_runs() -> usize {
        std::env::var("SMELT_TRANSCRIPT_BENCH_RUNS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|runs| *runs > 0)
            .unwrap_or(3)
    }

    fn transcript_bench_workloads() -> Vec<TranscriptBenchWorkload> {
        let all = vec![
            TranscriptBenchWorkload {
                name: "mixed_10mib",
                target_bytes: 10 * 1024 * 1024,
                build: push_large_mixed_transcript_fixture,
            },
            TranscriptBenchWorkload {
                name: "mixed_50mib",
                target_bytes: 50 * 1024 * 1024,
                build: push_large_mixed_transcript_fixture,
            },
            TranscriptBenchWorkload {
                name: "markdown_4mib",
                target_bytes: 4 * 1024 * 1024,
                build: push_markdown_heavy_transcript_fixture,
            },
            TranscriptBenchWorkload {
                name: "tool_output_4mib",
                target_bytes: 4 * 1024 * 1024,
                build: push_tool_output_heavy_transcript_fixture,
            },
            TranscriptBenchWorkload {
                name: "tiny_blocks_1mib",
                target_bytes: 1024 * 1024,
                build: push_many_tiny_blocks_transcript_fixture,
            },
            TranscriptBenchWorkload {
                name: "huge_blocks_4mib",
                target_bytes: 4 * 1024 * 1024,
                build: push_few_huge_blocks_transcript_fixture,
            },
        ];
        let Some(filter) = std::env::var("SMELT_TRANSCRIPT_BENCH_WORKLOADS")
            .ok()
            .filter(|filter| !filter.trim().is_empty())
        else {
            return all;
        };
        let names: std::collections::HashSet<_> = filter
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .collect();
        all.into_iter()
            .filter(|workload| names.contains(workload.name))
            .collect()
    }

    const RESUME_RECORD_BLOCK_TEXT_BYTES: usize = 4 * 1024;
    const RESUME_RECORD_WRITE_CHUNK: usize = 2048;

    fn resume_record_content(block_idx: usize) -> String {
        let mut content = format!("# Resume benchmark response {block_idx}\n\n");
        let paragraph = "This record-backed resume fixture stores full transcript text in lineage storage without building a full in-memory transcript first. It keeps enough markdown and wrapping pressure to exercise tail hydration and rendering.\n";
        while content.len() < RESUME_RECORD_BLOCK_TEXT_BYTES {
            content.push_str(paragraph);
        }
        smelt_buffer::text::grapheme_prefix(&content, RESUME_RECORD_BLOCK_TEXT_BYTES).to_string()
    }

    fn resume_block_record(
        block_idx: usize,
        content: String,
    ) -> smelt_store::StoredTranscriptBlock {
        let record = smelt_core::Block::Text {
            content: content.clone().into(),
        };
        let content_hash = record.content_hash();
        let indexed_text = format!("resume benchmark block {block_idx}");
        smelt_store::StoredTranscriptBlock {
            block_idx: block_idx as u64,
            history_idx: None,
            kind: record.kind().to_string(),
            tool_call_id: None,
            tool_name: None,
            content_hash: content_hash.to_string(),
            estimated_text_bytes: content.len() as u64,
            preview_text: format!("resume benchmark response {block_idx}"),
            indexed_text,
            block_json: serde_json::to_string(&record).expect("serialize resume record"),
            origin_json: None,
            tool_state_json: None,
            tool_render_revision: 0,
        }
    }

    fn write_record_backed_resume_fixture(
        sessions: &smelt_core::session::SessionStorage,
        session: &smelt_core::session::Session,
        target_bytes: usize,
    ) -> (usize, usize, f64) {
        let setup_start = std::time::Instant::now();
        let mut writer = smelt_store::OwnedLineageWriter::open(
            sessions.sessions_dir(),
            session.id.clone(),
        )
        .expect("open record resume fixture lineage");
        let command = smelt_core::session::initial_store_commit_from_session(session)
            .expect("prepare record resume fixture state");
        writer
            .commit_session(&command)
            .expect("initialize record resume fixture state");
        let target_bytes = target_bytes.max(RESUME_RECORD_BLOCK_TEXT_BYTES);
        let mut generated_bytes = 0usize;
        let mut record_count = 0usize;
        while generated_bytes < target_bytes {
            let mut records = Vec::with_capacity(RESUME_RECORD_WRITE_CHUNK);
            while generated_bytes < target_bytes && records.len() < RESUME_RECORD_WRITE_CHUNK {
                let content = resume_record_content(record_count);
                generated_bytes = generated_bytes.saturating_add(content.len());
                records.push(resume_block_record(record_count, content));
                record_count += 1;
            }
            let head = writer.store_head().expect("read record resume fixture head");
            let mut command =
                smelt_core::session::initial_store_commit_from_session(session)
                    .expect("prepare record resume fixture append");
            command.expected = head;
            command.history.start = smelt_store::HistoryIndex::new(head.history_len.get());
            command.history.final_len = head.history_len;
            command.history.items.clear();
            command.side_tables = smelt_store::SideTableSuffixes {
                start: smelt_store::HistoryIndex::new(head.history_len.get()),
                ..smelt_store::SideTableSuffixes::default()
            };
            command.transcript_records = Some(smelt_store::TranscriptRecordSuffix {
                start: smelt_store::TranscriptRecordIndex::new(
                    head.transcript_record_count.get(),
                ),
                records,
            });
            writer
                .commit_session(&command)
                .expect("write record resume fixture chunk");
        }
        writer.release().expect("release record resume fixture");
        let setup_ms = elapsed_ms(setup_start.elapsed());
        (record_count, generated_bytes, setup_ms)
    }

    fn run_true_resume_bench_sample(target_bytes: usize) {
        smelt_perf::perf::clear();
        smelt_perf::perf::set_enabled(true);
        let lua = crate::lua::LuaRuntime::new();
        let theme = Theme::default();
        let session = smelt_core::session::Session::new(
            std::process::id(),
            std::env::current_dir().unwrap_or_default(),
        );
        let session_id = session.id.clone();
        let storage_dir = tempfile::tempdir().expect("resume benchmark storage");
        let sessions = smelt_core::session::SessionStorage::new(storage_dir.path().join("smelt"));
        let (record_count, generated_bytes, setup_ms) =
            write_record_backed_resume_fixture(&sessions, &session, target_bytes);

        smelt_perf::perf::clear();
        let tail_alloc_before = smelt_perf::alloc::snapshot();
        let tail_load_start = std::time::Instant::now();
        let tail_resumed = crate::app::history::load_transcript_tail_from_sqlite_id(
            &sessions,
            &session_id,
            100,
            40,
        )
        .expect("tail-load benchmark transcript records");
        let mut tail_document =
            crate::app::transcript::TranscriptDocument::from_loaded_transcript(tail_resumed);
        let tail_load_ms = elapsed_ms(tail_load_start.elapsed());
        let mut tail_buf = Buffer::new(crate::smelt_edit::BufId(94), Default::default());
        let tail_render_start = std::time::Instant::now();
        let tail_plan = tail_document
            .plan_viewport_projection_measured(
                &lua,
                100,
                &theme,
                crate::app::transcript::TranscriptViewportProjectionInput {
                    fallback_scroll_top: 0,
                    follow_tail: true,
                    width_changed: false,
                    previous_width: None,
                },
                40,
            )
            .expect("resume benchmark projection hydration");
        let tail_rows = tail_document
            .project_applied_viewport(&lua, &mut tail_buf, &theme, tail_plan)
            .materialized_rows;
        let tail_render_ms = elapsed_ms(tail_render_start.elapsed());
        let tail_alloc_after = smelt_perf::alloc::snapshot();
        let tail_alloc = smelt_perf::alloc::delta(tail_alloc_before, tail_alloc_after);
        let tail_retained_bytes =
            tail_alloc_after.current_bytes as i64 - tail_alloc_before.current_bytes as i64;
        assert!(tail_rows.total_rows > 0);
        let tail_snapshot = smelt_perf::perf::snapshot();
        print_resume_perf_snapshot(&tail_snapshot);
        assert_resume_tail_perf_gates(&tail_snapshot, record_count);
        assert_eq!(
            perf_value_max(&tail_snapshot, "transcript:sqlite:record_total"),
            record_count as u64,
            "display-only resume did not observe the record-backed fixture size"
        );

        drop(tail_document);
        sessions
            .delete(&session_id)
            .expect("delete resume benchmark session");
        smelt_perf::perf::set_enabled(false);
        eprintln!(
            "TRANSCRIPT_TRUE_RESUME_SAMPLE mode=record_backed target_bytes={} generated_bytes={} records={} rows={} setup_ms={:.3} tail_load_ms={:.3} tail_render_ms={:.3} tail_bytes_allocated={} tail_bytes_deallocated={} tail_current_bytes_before={} tail_current_bytes_after={} tail_retained_bytes={}",
            target_bytes,
            generated_bytes,
            record_count,
            tail_rows.total_rows,
            setup_ms,
            tail_load_ms,
            tail_render_ms,
            tail_alloc.bytes_allocated,
            tail_alloc.bytes_deallocated,
            tail_alloc_before.current_bytes,
            tail_alloc_after.current_bytes,
            tail_retained_bytes,
        );
        eprintln!(
            "TRANSCRIPT_TRUE_RESUME_JSON {{\"type\":\"resume_summary\",\"mode\":\"record_backed\",\"target_bytes\":{},\"generated_bytes\":{},\"records\":{},\"rows\":{},\"setup_ms\":{:.3},\"tail_load_ms\":{:.3},\"tail_render_ms\":{:.3},\"tail_bytes_allocated\":{},\"tail_bytes_deallocated\":{},\"tail_current_bytes_before\":{},\"tail_current_bytes_after\":{},\"tail_retained_bytes\":{}}}",
            target_bytes,
            generated_bytes,
            record_count,
            tail_rows.total_rows,
            setup_ms,
            tail_load_ms,
            tail_render_ms,
            tail_alloc.bytes_allocated,
            tail_alloc.bytes_deallocated,
            tail_alloc_before.current_bytes,
            tail_alloc_after.current_bytes,
            tail_retained_bytes,
        );
    }
    fn assert_projection_bench_gates(counters: TranscriptProjectionCounters, label: &str) {
        const MAX_MATERIALIZED_ROWS_PER_OPERATION: usize = 80;
        assert!(
            counters.max_range_materialized_rows <= MAX_MATERIALIZED_ROWS_PER_OPERATION,
            "{label} materialized {} rows, expected <= {MAX_MATERIALIZED_ROWS_PER_OPERATION}",
            counters.max_range_materialized_rows,
        );
        assert!(
            counters.max_range_materialized_blocks <= MAX_MATERIALIZED_ROWS_PER_OPERATION,
            "{label} materialized {} blocks, expected <= {MAX_MATERIALIZED_ROWS_PER_OPERATION}",
            counters.max_range_materialized_blocks,
        );
    }

    fn run_transcript_bench_sample(workload: TranscriptBenchWorkload) -> TranscriptBenchSample {
        smelt_perf::perf::clear();
        smelt_perf::perf::set_enabled(true);
        smelt_perf::alloc::set_enabled(true);

        let lua = test_lua();
        let mut transcript = Transcript::new();
        let generated_bytes = (workload.build)(&mut transcript, workload.target_bytes);
        let input_bytes = approx_history_bytes(&transcript.history);
        let blocks = transcript.history.order.len();
        let theme = Theme::default();

        let mut cold = TranscriptProjection::new();
        let mut cold_buf = Buffer::new(crate::smelt_edit::BufId(77), Default::default());
        let alloc_start = smelt_perf::alloc::snapshot();
        let first_start = std::time::Instant::now();
        let first = project_with_lua(
            &mut cold,
            &lua,
            &mut cold_buf,
            &mut transcript.history,
            100,
            &theme,
            ScrollTarget::visible_tail(),
            40,
        );
        let first_ms = elapsed_ms(first_start.elapsed());
        let first_alloc = smelt_perf::alloc::delta(alloc_start, smelt_perf::alloc::snapshot());
        let first_snapshot = smelt_perf::perf::snapshot();
        print_layout_alloc_snapshot(workload.name, &first_snapshot);
        let first_counters = cold.counters();

        cold.reset_counters();
        let resize_start = std::time::Instant::now();
        let resized = project_with_lua(
            &mut cold,
            &lua,
            &mut cold_buf,
            &mut transcript.history,
            72,
            &theme,
            ScrollTarget::visible_row(first.clamped_scroll),
            40,
        );
        let resize_ms = elapsed_ms(resize_start.elapsed());
        let resize_counters = cold.counters();

        cold.invalidate_theme();
        cold.reset_counters();
        let theme_start = std::time::Instant::now();
        let themed = project_with_lua(
            &mut cold,
            &lua,
            &mut cold_buf,
            &mut transcript.history,
            72,
            &theme,
            ScrollTarget::visible_row(resized.clamped_scroll),
            40,
        );
        let theme_ms = elapsed_ms(theme_start.elapsed());
        let theme_counters = cold.counters();

        cold.reset_counters();
        let scroll_start = std::time::Instant::now();
        let mut scroll_materialized_rows = 0u64;
        let max_scroll = resized.total_rows.saturating_sub(1);
        let step = (resized.total_rows / 12).max(1);
        for i in 0..12u64 {
            let row = i.saturating_mul(step).min(max_scroll);
            let rows = project_with_lua(
                &mut cold,
                &lua,
                &mut cold_buf,
                &mut transcript.history,
                72,
                &theme,
                ScrollTarget::visible_row(row),
                40,
            );
            scroll_materialized_rows =
                scroll_materialized_rows.saturating_add(rows.materialized_rows);
        }
        let scroll12_ms = elapsed_ms(scroll_start.elapsed());
        let scroll_counters = cold.counters();

        cold.reset_counters();
        let visible_start = std::time::Instant::now();
        let mid = resized.total_rows / 2;
        let visible =
            cold.display_rows_for_range(&lua, &mut transcript.history, 72, &theme, mid..mid + 80);
        let visible_ms = elapsed_ms(visible_start.elapsed());
        let visible_counters = cold.counters();

        let mut copy_probe = TranscriptProjection::new();
        copy_probe.set_inline_options(cold.inline_options().clone());
        let copy_range = find_selectable_copy_range(
            &mut copy_probe,
            &lua,
            &mut transcript.history,
            72,
            &theme,
            mid,
            resized.total_rows,
        )
        .expect("benchmark transcript should contain selectable display text");
        copy_probe.reset_counters();
        let copy_start = std::time::Instant::now();
        let copied = copy_probe.copy_range(&lua, &mut transcript.history, 72, &theme, copy_range);
        let copy_ms = elapsed_ms(copy_start.elapsed());
        let copy_counters = copy_probe.counters();

        let mut no_cache = TranscriptProjection::new();
        let mut no_cache_buf = Buffer::new(crate::smelt_edit::BufId(80), Default::default());
        let no_cache_start = std::time::Instant::now();
        let no_cache_projection = project_with_lua(
            &mut no_cache,
            &lua,
            &mut no_cache_buf,
            &mut transcript.history,
            100,
            &theme,
            ScrollTarget::visible_tail(),
            40,
        );
        let no_cache_ms = elapsed_ms(no_cache_start.elapsed());
        let no_cache_counters = no_cache.counters();

        transcript.push(Block::Text {
            content: "live append benchmark tail\n".repeat(16).into(),
        });
        cold.reset_counters();
        let append_start = std::time::Instant::now();
        let appended = project_with_lua(
            &mut cold,
            &lua,
            &mut cold_buf,
            &mut transcript.history,
            72,
            &theme,
            ScrollTarget::visible_tail(),
            40,
        );
        let append_ms = elapsed_ms(append_start.elapsed());
        let append_counters = cold.counters();
        let appended_rows = appended.total_rows.saturating_sub(resized.total_rows);

        assert!(generated_bytes >= workload.target_bytes);
        assert!(input_bytes > 0);
        assert!(first.total_rows > 0);
        assert!(resized.total_rows > 0);
        assert_eq!(themed.total_rows, resized.total_rows);
        assert!(
            !visible.rows.is_empty(),
            "visible range was empty at mid={mid} total_rows={} counters={visible_counters:?}",
            resized.total_rows
        );
        assert!(
            !copied.kill_ring.is_empty(),
            "copy range was empty at start={:?} end={:?} total_rows={} counters={copy_counters:?}",
            copy_range.start,
            copy_range.end,
            resized.total_rows
        );
        assert_eq!(no_cache_projection.total_rows, first.total_rows);
        assert!(
            appended.materialized_rows > 0,
            "live append repaint produced no materialized rows: {appended:?}"
        );
        assert!(first_counters.layout_cache > 0);
        assert!(first_counters.layout_cache <= blocks);
        assert!(resize_counters.layout_cache <= blocks);
        assert!(resize_counters.exact_height_measured_blocks <= blocks);
        assert!(theme_counters.layout_cache <= blocks);
        assert!(theme_counters.exact_height_measured_blocks <= blocks);
        assert!(scroll_counters.layout_cache <= blocks);
        assert!(scroll_counters.exact_height_measured_blocks <= blocks);
        assert!(visible_counters.layout_cache <= blocks);
        assert!(visible_counters.exact_height_measured_blocks <= blocks);
        assert!(copy_counters.layout_cache <= blocks);
        assert!(copy_counters.exact_height_measured_blocks <= blocks);
        assert!(append_counters.layout_cache <= blocks + 1);
        assert!(append_counters.exact_height_measured_blocks <= blocks + 1);
        assert_eq!(
            no_cache_counters.layout_cache,
            first_counters.layout_cache
        );
        assert_projection_bench_gates(first_counters, "first paint");
        assert_projection_bench_gates(resize_counters, "resize");
        assert_projection_bench_gates(theme_counters, "theme refresh");
        assert_projection_bench_gates(visible_counters, "visible range");
        assert_projection_bench_gates(copy_counters, "copy range");
        assert_projection_bench_gates(append_counters, "live append");
        assert_projection_bench_gates(no_cache_counters, "no-cache first paint");
        assert_projection_bench_gates(scroll_counters, "scroll jump");
        assert!(
            scroll_counters.range_materialized_rows <= 12 * 80,
            "scroll benchmark materialized {} rows over 12 jumps, expected <= 960",
            scroll_counters.range_materialized_rows,
        );

        smelt_perf::alloc::set_enabled(false);
        smelt_perf::perf::set_enabled(false);
        smelt_perf::perf::clear();

        TranscriptBenchSample {
            input_bytes,
            generated_bytes,
            blocks,
            total_rows: first.total_rows,
            first_ms,
            resize_ms,
            theme_ms,
            scroll12_ms,
            visible_ms,
            copy_ms,
            append_ms,
            no_cache_ms,
            allocs: first_alloc.allocs,
            bytes_allocated: first_alloc.bytes_allocated,
            alloc_current_bytes: first_alloc.current_bytes,
            alloc_peak_bytes: first_alloc.peak_bytes,
            visible_rows: visible.rows.len(),
            copied_rows: copy_range
                .end
                .row
                .saturating_sub(copy_range.start.row)
                .saturating_add(1),
            appended_rows,
            scroll_materialized_rows,
            first_counters,
            resize_counters,
            theme_counters,
            scroll_counters,
            visible_counters,
            copy_counters,
            append_counters,
            no_cache_counters,
        }
    }

    fn print_transcript_bench_summary(
        workload: TranscriptBenchWorkload,
        samples: &[TranscriptBenchSample],
    ) {
        let first = MetricStats::from(
            &samples
                .iter()
                .map(|sample| sample.first_ms)
                .collect::<Vec<_>>(),
        );
        let resize = MetricStats::from(
            &samples
                .iter()
                .map(|sample| sample.resize_ms)
                .collect::<Vec<_>>(),
        );
        let theme = MetricStats::from(
            &samples
                .iter()
                .map(|sample| sample.theme_ms)
                .collect::<Vec<_>>(),
        );
        let scroll = MetricStats::from(
            &samples
                .iter()
                .map(|sample| sample.scroll12_ms)
                .collect::<Vec<_>>(),
        );
        let visible = MetricStats::from(
            &samples
                .iter()
                .map(|sample| sample.visible_ms)
                .collect::<Vec<_>>(),
        );
        let copy = MetricStats::from(
            &samples
                .iter()
                .map(|sample| sample.copy_ms)
                .collect::<Vec<_>>(),
        );
        let append = MetricStats::from(
            &samples
                .iter()
                .map(|sample| sample.append_ms)
                .collect::<Vec<_>>(),
        );
        let no_cache = MetricStats::from(
            &samples
                .iter()
                .map(|sample| sample.no_cache_ms)
                .collect::<Vec<_>>(),
        );
        let sample = samples[0];
        eprintln!(
            "| {:<18} | {:>8.2} | {:>6} | {:>8} | {:>12} | {:>12} | {:>12} | {:>12} | {:>12} | {:>12} | {:>12} | {:>12} |",
            workload.name,
            sample.input_bytes as f64 / (1024.0 * 1024.0),
            sample.blocks,
            sample.total_rows,
            first.display(),
            resize.display(),
            theme.display(),
            scroll.display(),
            visible.display(),
            copy.display(),
            append.display(),
            no_cache.display(),
        );
        eprintln!(
            "TRANSCRIPT_LAYOUT_BENCH_SUMMARY workload={} runs={} input_bytes={} generated_bytes={} blocks={} rows={} first_mean_ms={:.3} first_stddev_ms={:.3} resize_mean_ms={:.3} resize_stddev_ms={:.3} theme_mean_ms={:.3} theme_stddev_ms={:.3} scroll12_mean_ms={:.3} scroll12_stddev_ms={:.3} visible_mean_ms={:.3} visible_stddev_ms={:.3} copy_mean_ms={:.3} copy_stddev_ms={:.3} append_mean_ms={:.3} append_stddev_ms={:.3} no_cache_mean_ms={:.3} no_cache_stddev_ms={:.3} allocs={} bytes_allocated={} alloc_current_bytes={} alloc_peak_bytes={} visible_rows={} copied_rows={} appended_rows={} scroll_materialized_rows={} first_min_ms={:.3} first_max_ms={:.3}",
            workload.name,
            samples.len(),
            sample.input_bytes,
            sample.generated_bytes,
            sample.blocks,
            sample.total_rows,
            first.mean,
            first.stddev,
            resize.mean,
            resize.stddev,
            theme.mean,
            theme.stddev,
            scroll.mean,
            scroll.stddev,
            visible.mean,
            visible.stddev,
            copy.mean,
            copy.stddev,
            append.mean,
            append.stddev,
            no_cache.mean,
            no_cache.stddev,
            sample.allocs,
            sample.bytes_allocated,
            sample.alloc_current_bytes,
            sample.alloc_peak_bytes,
            sample.visible_rows,
            sample.copied_rows,
            sample.appended_rows,
            sample.scroll_materialized_rows,
            first.min,
            first.max,
        );
        eprintln!(
            "TRANSCRIPT_LAYOUT_BENCH_JSON {{\"type\":\"layout_summary\",\"workload\":\"{}\",\"runs\":{},\"input_bytes\":{},\"generated_bytes\":{},\"blocks\":{},\"rows\":{},\"first_mean_ms\":{:.3},\"first_stddev_ms\":{:.3},\"resize_mean_ms\":{:.3},\"resize_stddev_ms\":{:.3},\"theme_mean_ms\":{:.3},\"theme_stddev_ms\":{:.3},\"scroll12_mean_ms\":{:.3},\"scroll12_stddev_ms\":{:.3},\"visible_mean_ms\":{:.3},\"visible_stddev_ms\":{:.3},\"copy_mean_ms\":{:.3},\"copy_stddev_ms\":{:.3},\"append_mean_ms\":{:.3},\"append_stddev_ms\":{:.3},\"no_cache_mean_ms\":{:.3},\"no_cache_stddev_ms\":{:.3},\"allocs\":{},\"bytes_allocated\":{},\"alloc_current_bytes\":{},\"alloc_peak_bytes\":{},\"visible_rows\":{},\"copied_rows\":{},\"appended_rows\":{},\"scroll_materialized_rows\":{},\"first_min_ms\":{:.3},\"first_max_ms\":{:.3}}}",
            workload.name,
            samples.len(),
            sample.input_bytes,
            sample.generated_bytes,
            sample.blocks,
            sample.total_rows,
            first.mean,
            first.stddev,
            resize.mean,
            resize.stddev,
            theme.mean,
            theme.stddev,
            scroll.mean,
            scroll.stddev,
            visible.mean,
            visible.stddev,
            copy.mean,
            copy.stddev,
            append.mean,
            append.stddev,
            no_cache.mean,
            no_cache.stddev,
            sample.allocs,
            sample.bytes_allocated,
            sample.alloc_current_bytes,
            sample.alloc_peak_bytes,
            sample.visible_rows,
            sample.copied_rows,
            sample.appended_rows,
            sample.scroll_materialized_rows,
            first.min,
            first.max,
        );
        eprintln!(
            "TRANSCRIPT_LAYOUT_COUNTERS_JSON {{\"type\":\"layout_counters\",\"workload\":\"{}\",\"first\":{},\"resize\":{},\"theme\":{},\"scroll12\":{},\"visible\":{},\"copy\":{},\"append\":{},\"no_cache\":{}}}",
            workload.name,
            counters_json(sample.first_counters),
            counters_json(sample.resize_counters),
            counters_json(sample.theme_counters),
            counters_json(sample.scroll_counters),
            counters_json(sample.visible_counters),
            counters_json(sample.copy_counters),
            counters_json(sample.append_counters),
            counters_json(sample.no_cache_counters),
        );
        eprintln!(
            "TRANSCRIPT_LAYOUT_BENCH_COUNTERS workload={} first={:?} resize={:?} theme={:?} scroll12={:?} visible={:?} copy={:?} append={:?} no_cache={:?}",
            workload.name,
            sample.first_counters,
            sample.resize_counters,
            sample.theme_counters,
            sample.scroll_counters,
            sample.visible_counters,
            sample.copy_counters,
            sample.append_counters,
            sample.no_cache_counters,
        );
    }

    pub(crate) fn run_layout_benchmark() {
        let runs = transcript_bench_runs();
        let workloads = transcript_bench_workloads();
        assert!(!workloads.is_empty(), "no benchmark workloads selected");
        eprintln!(
            "TRANSCRIPT_LAYOUT_BENCH runs={runs} workloads={}",
            workloads.len()
        );
        eprintln!(
            "| workload           |      MiB | blocks |     rows |     first ms |    resize ms |     theme ms |   scroll12 ms |   visible ms |      copy ms |    append ms |   nocache ms |"
        );
        eprintln!(
            "|--------------------|----------|--------|----------|--------------|--------------|--------------|--------------|--------------|--------------|--------------|--------------|"
        );
        for workload in workloads {
            let _warmup = run_transcript_bench_sample(workload);
            let mut samples = Vec::with_capacity(runs);
            for run in 0..runs {
                let sample = run_transcript_bench_sample(workload);
                eprintln!(
                    "TRANSCRIPT_LAYOUT_BENCH_SAMPLE workload={} run={} input_bytes={} generated_bytes={} blocks={} rows={} first_ms={:.3} resize_ms={:.3} theme_ms={:.3} scroll12_ms={:.3} visible_ms={:.3} copy_ms={:.3} append_ms={:.3} no_cache_ms={:.3} allocs={} bytes_allocated={} alloc_current_bytes={} alloc_peak_bytes={}",
                    workload.name,
                    run + 1,
                    sample.input_bytes,
                    sample.generated_bytes,
                    sample.blocks,
                    sample.total_rows,
                    sample.first_ms,
                    sample.resize_ms,
                    sample.theme_ms,
                    sample.scroll12_ms,
                    sample.visible_ms,
                    sample.copy_ms,
                    sample.append_ms,
                    sample.no_cache_ms,
                    sample.allocs,
                    sample.bytes_allocated,
                    sample.alloc_current_bytes,
                    sample.alloc_peak_bytes,
                );
                samples.push(sample);
            }
            print_transcript_bench_summary(workload, &samples);
        }
    }

    pub(crate) fn run_true_resume_benchmark() {
        let target_bytes = std::env::var("SMELT_TRANSCRIPT_RESUME_BENCH_BYTES")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(10 * 1024 * 1024);
        run_true_resume_bench_sample(target_bytes);
    }

    }

    fn project_tool_title(name: &str, summary: protocol::StyledLines) -> Buffer {
        let mut transcript = Transcript::new();
        let mut parser = StreamParser::new();
        parser.start_tool(
            &mut transcript.history,
            ToolStart {
                invocation_id: next_invocation_id(),
                call_id: "call-1".into(),
                name: name.into(),
                summary,
                args: std::collections::HashMap::new(),
                preview_output: None,
                called_at_ms: 0,
            },
            std::time::Instant::now(),
        );

        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(7), Default::default());
        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_row(0),
            80,
        );
        buf
    }

    fn copy_first_row(buf: &Buffer) -> String {
        let end = buf.get_line(0).unwrap_or("").len();
        copy_byte_range(buf, 0, end)
    }

    #[test]
    fn copy_tool_title_includes_pill_name_and_summary() {
        let buf = project_tool_title("bash", protocol::StyledLines::from_plain("ls -la"));

        assert_eq!(copy_first_row(&buf), "* bash ls -la");
    }

    #[test]
    fn copy_tool_title_includes_pill_and_name_when_summary_is_empty() {
        let buf = project_tool_title("bash", protocol::StyledLines::empty());

        assert_eq!(copy_first_row(&buf), "* bash");
    }

    #[test]
    fn copy_tool_title_excludes_non_selectable_suffix() {
        let summary = protocol::StyledLines(vec![vec![
            protocol::StyledSpan {
                text: "echo hi".into(),
                ..Default::default()
            },
            protocol::StyledSpan {
                text: "(timeout: 2m)".into(),
                selectable: false,
                title_suffix: true,
                ..Default::default()
            },
        ]]);
        let buf = project_tool_title("bash", summary);

        assert!(buf.get_line(0).unwrap_or("").contains("timeout"));
        assert_eq!(copy_first_row(&buf), "* bash echo hi");
    }

    #[test]
    fn expanded_group_root_rendered_child_titles_are_selectable() {
        let lua = test_lua();
        install_read_file_renderer(&lua);
        let mut transcript = Transcript::new();
        push_named_tool(
            &mut transcript,
            "read-1",
            "read_file",
            "a.rs",
            ToolStatus::Ok,
            tool_args(&[("file_path", "a.rs")]),
        );
        push_named_tool(
            &mut transcript,
            "read-2",
            "read_file",
            "b.rs",
            ToolStatus::Ok,
            tool_args(&[("file_path", "b.rs")]),
        );
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        projection.build_rows(&lua, &mut transcript.history, 80, &theme);
        let group_id = projection.transcript_scene.nodes[0].id();
        assert!(projection.fold_node(&transcript.history, group_id, FoldAction::Open));
        let expanded = projection.build_rows(&lua, &mut transcript.history, 80, &theme);
        assert!(
            expanded.iter().any(|line| line == "* read_file a.rs")
                && expanded.iter().any(|line| line == "* read_file b.rs"),
            "expanded rows: {expanded:?}"
        );
        let mut buf = Buffer::new(crate::smelt_edit::BufId(8), Default::default());

        projection.project(
            &mut buf,
            &mut transcript.history,
            80,
            &theme,
            ScrollTarget::visible_row(0),
            80,
        );

        let copied = copy_byte_range(&buf, 0, buf.text().len());
        assert!(copied.contains("* read_file a.rs"), "copied: {copied:?}");
        assert!(copied.contains("* read_file b.rs"), "copied: {copied:?}");
    }

    #[test]
    fn table_rows_are_hard_breaks_not_soft() {
        let mut transcript = Transcript::new();
        transcript.push(Block::Text {
            content: "before".into(),
        });
        transcript.push(Block::Text {
            content: "| a | b |\n| - | - |\n| 1 | 2 |".into(),
        });
        transcript.push(Block::Text {
            content: "after".into(),
        });
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(3), Default::default());
        projection.project(
            &mut buf,
            &mut transcript.history,
            40,
            &theme,
            ScrollTarget::visible_row(0),
            80,
        );

        let display = projection.display_rows_for_range(
            &test_lua(),
            &mut transcript.history,
            40,
            &theme,
            0..buf.line_count() as RowIndex,
        );
        let soft = display.soft_breaks();
        // For a transcript with only a table, every row boundary should be a
        // hard break (no soft breaks) so triple-click selects one display row.
        // The table is at rows 2-6; verify no soft break falls inside it.
        let lines = buf.lines();
        let mut table_start = None;
        let mut table_end = None;
        for (i, line) in lines.iter().enumerate() {
            if line.contains('\u{2503}') {
                if table_start.is_none() {
                    table_start = Some(i);
                }
                table_end = Some(i);
            }
        }
        let (t0, t1) = (
            table_start.expect("table start"),
            table_end.expect("table end"),
        );
        let mut acc = 0usize;
        for (i, line) in lines.iter().enumerate() {
            acc += line.len();
            if i + 1 < lines.len() {
                let break_pos = acc;
                if i >= t0 && i < t1 && soft.contains(&break_pos) {
                    panic!(
                        "row {} boundary at {} should be hard, not soft",
                        i + 1,
                        break_pos
                    );
                }
                acc += 1; // '\n'
            }
        }
    }

    #[test]
    fn line_break_offsets_include_join_newlines() {
        let mut transcript = Transcript::new();
        transcript.push(Block::Text {
            content: "aa\nbbb\nc".into(),
        });
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let rows = projection.build_rows(&test_lua(), &mut transcript.history, 80, &theme);

        let display = projection.display_rows_for_range(
            &test_lua(),
            &mut transcript.history,
            80,
            &theme,
            0..rows.len() as RowIndex,
        );
        assert!(
            display.soft_breaks().is_empty(),
            "unwrapped source lines must be hard breaks"
        );
        assert_eq!(
            display.hard_breaks(),
            crate::smelt_edit::hard_breaks_for_lines(&rows)
        );
    }

    #[test]
    fn table_full_selection_copies_raw_markdown() {
        let mut transcript = Transcript::new();
        transcript.push(Block::Text {
            content: "| a | b |\n| - | - |\n| 1 | 2 |".into(),
        });
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(4), Default::default());
        projection.project(
            &mut buf,
            &mut transcript.history,
            40,
            &theme,
            ScrollTarget::visible_row(0),
            80,
        );

        // Select the entire table (all rows).
        let total_bytes = buf.text().len();
        let copied = copy_byte_range(&buf, 0, total_bytes);
        assert_eq!(copied, "| a | b |\n| - | - |\n| 1 | 2 |");
    }

    #[test]
    fn table_single_row_selection_copies_cell_contents() {
        let mut transcript = Transcript::new();
        transcript.push(Block::Text {
            content: "| a | b |\n| - | - |\n| 1 | 2 |".into(),
        });
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(5), Default::default());
        projection.project(
            &mut buf,
            &mut transcript.history,
            40,
            &theme,
            ScrollTarget::visible_row(0),
            80,
        );

        let lines = buf.lines();
        // Find the data row (contains '1' and '2').
        let data_row = lines
            .iter()
            .position(|l| l.contains('1') && l.contains('2'))
            .expect("data row");
        let mut acc = 0usize;
        for (i, line) in lines.iter().enumerate() {
            if i == data_row {
                let row_end = acc + line.len();
                let copied = copy_byte_range(&buf, acc, row_end);
                // Should emit selectable cells only, no borders or padding.
                assert_eq!(copied, "12");
                break;
            }
            acc += line.len() + 1;
        }
    }

    #[test]
    fn narrow_stacked_table_does_not_overflow_transcript_width() {
        let mut transcript = Transcript::new();
        transcript.push(Block::Text {
            content: "\
| Approach | Worth it? | Risk | Notes |
| --- | --- | --- | --- |
| Revert pre-pruning and add retry loop | Yes fixes cache and matches reference | Low | Post-compaction token recompute |
"
            .into(),
        });
        let theme = Theme::default();
        let mut projection = TranscriptProjection::new();
        let mut buf = Buffer::new(crate::smelt_edit::BufId(6), Default::default());
        projection.project(
            &mut buf,
            &mut transcript.history,
            24,
            &theme,
            ScrollTarget::visible_row(0),
            80,
        );

        let lines = buf.lines();
        assert!(
            lines.iter().all(|line| !line.contains('┏')),
            "expected stacked fallback, got {lines:?}"
        );
        for line in lines {
            let width = smelt_core::content::builder::display_width(line);
            assert!(
                width <= 24,
                "projected table row overflowed transcript width: width={width}, line={line:?}"
            );
        }
    }
}
