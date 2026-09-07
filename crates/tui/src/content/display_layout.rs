use crate::content::transcript_scene::{
    NodeLayoutKey, RenderGroupNode, RenderNode, RenderNodeId, TranscriptDefaultViewPolicy,
};
use crate::smelt_edit::{BufCreateOpts, BufId, Buffer, Theme};
use smelt_core::buffer::LineDecoration;
use smelt_core::content::block_layout::{
    BlockLayout, ContentRenderSpec, GutterSpec, HboxItem, IrLeaf, LayoutIr, LineSpec, LuaLeaf,
    RetainedContentSpec, RetainedInlineSyntax, SourceViewIr, StyleSpec, TextSpec,
};
use smelt_core::content::builder::{LineBuilder, Outcome};
use smelt_core::content::highlight::InlineOptions;
use smelt_core::lua::runtime::{LuaRuntime, TranscriptRenderNode};
use smelt_core::theme::intern;
use smelt_core::transcript_content::TranscriptContent;
#[cfg(test)]
use smelt_core::transcript_model::BlockId;
use smelt_core::transcript_model::{Block, BlockHistory, Status, ViewState};
use std::cell::RefCell;
use std::collections::{hash_map::DefaultHasher, HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

pub(crate) const DISPLAY_RENDERER_VERSION: u64 = 12;

pub(crate) fn transcript_renderer_cache_key(
    lua: &LuaRuntime,
    inline_options: &InlineOptions,
) -> Option<u64> {
    let mut key = lua.transcript_renderer_cache_key();
    let icon_hash = inline_options
        .file_icons
        .enabled
        .then(|| inline_options.file_icons.cache_hash());
    for hash in [icon_hash, lua.transcript_settings_cache_key()]
        .into_iter()
        .flatten()
    {
        key = Some(match key {
            Some(base) => base ^ hash.rotate_left(17),
            None => hash,
        });
    }
    key
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct DisplayCacheKey {
    pub(crate) content_hash: u64,
    pub(crate) sidecar_hash: u64,
    pub(crate) renderer_version: u64,
    pub(crate) renderer_generation: u64,
    pub(crate) renderer_cache_key: Option<u64>,
    pub(crate) render_context_hash: u64,
}

impl DisplayCacheKey {
    pub(crate) fn new(
        content_hash: u64,
        sidecar_hash: u64,
        renderer_generation: u64,
        renderer_cache_key: Option<u64>,
        render_context_hash: u64,
    ) -> Self {
        Self {
            content_hash,
            sidecar_hash,
            renderer_version: DISPLAY_RENDERER_VERSION,
            renderer_generation,
            renderer_cache_key,
            render_context_hash,
        }
    }

    fn from_node_key(
        key: NodeLayoutKey,
        renderer_generation: u64,
        renderer_cache_key: Option<u64>,
    ) -> Self {
        Self::new(
            key.content_hash,
            key.sidecar_hash,
            renderer_generation,
            renderer_cache_key,
            smelt_core::utils::hash_serializable(&key.view_state),
        )
    }

    fn same_render_inputs_except_content(self, other: Self) -> bool {
        self.sidecar_hash == other.sidecar_hash
            && self.renderer_version == other.renderer_version
            && self.renderer_generation == other.renderer_generation
            && self.renderer_cache_key == other.renderer_cache_key
            && self.render_context_hash == other.render_context_hash
    }
}

#[derive(Clone)]
pub(crate) struct TranscriptRenderEnv<'a> {
    pub(crate) lua: &'a LuaRuntime,
    pub(crate) renderer_generation: u64,
    pub(crate) renderer_cache_key: Option<u64>,
    pub(crate) now_ms: u64,
    pub(crate) refresh_now: std::time::Instant,
}

impl<'a> TranscriptRenderEnv<'a> {
    #[cfg(test)]
    pub(crate) fn new(lua: &'a LuaRuntime) -> Self {
        Self::with_inline_options(lua, InlineOptions::default())
    }

    pub(crate) fn with_inline_options(lua: &'a LuaRuntime, inline_options: InlineOptions) -> Self {
        Self {
            lua,
            renderer_generation: lua.transcript_renderer_generation(),
            renderer_cache_key: transcript_renderer_cache_key(lua, &inline_options),
            now_ms: lua.transcript_now_ms(),
            refresh_now: lua.transcript_instant_now(),
        }
    }

    pub(crate) fn with_renderer(
        lua: &'a LuaRuntime,
        renderer_generation: u64,
        renderer_cache_key: Option<u64>,
    ) -> Self {
        Self {
            lua,
            renderer_generation,
            renderer_cache_key,
            now_ms: lua.transcript_now_ms(),
            refresh_now: lua.transcript_instant_now(),
        }
    }
}

pub(crate) struct CompileJob {
    id: RenderNodeId,
    key: DisplayCacheKey,
    view_state: ViewState,
    source: CompileJobSource,
}

enum CompileJobSource {
    Metadata {
        node: TranscriptRenderNode,
        content_sources: Vec<TranscriptContent>,
    },
    Group {
        node: TranscriptRenderNode,
        children: Vec<GroupChildCompileSource>,
    },
    Ready(LayoutIr),
}

struct GroupChildCompileSource {
    node: TranscriptRenderNode,
    view_state: ViewState,
    content_sources: Vec<TranscriptContent>,
}

struct CompiledLayout {
    layout: LayoutIr,
    refresh_after_ms: Option<u64>,
}

fn earliest_delay(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(delay), None) | (None, Some(delay)) => Some(delay),
        (None, None) => None,
    }
}

fn blank_line_layout() -> LayoutIr {
    BlockLayout::Leaf(IrLeaf::Line(LineSpec {
        spans: vec![protocol::StyledSpan::default()],
        hl_group: None,
        syntax_highlights: Default::default(),
    }))
}

impl CompileJob {
    fn compile(
        self,
        env: TranscriptRenderEnv<'_>,
    ) -> (
        RenderNodeId,
        DisplayCacheKey,
        LayoutIr,
        Option<std::time::Instant>,
    ) {
        let compiled = match self.source {
            CompileJobSource::Metadata {
                node,
                content_sources,
            } => {
                let mut cache = CompileLayoutCache {
                    content_sources: &content_sources,
                    group_children: None,
                    refresh_after_ms: None,
                };
                compile_node_with_lua(env.clone(), &node, self.view_state, &mut cache)
            }
            CompileJobSource::Group { node, children } => {
                let mut child_layouts = Vec::with_capacity(children.len().saturating_mul(2));
                let mut refresh_after_ms = None;
                for child in children {
                    let mut cache = CompileLayoutCache {
                        content_sources: &child.content_sources,
                        group_children: None,
                        refresh_after_ms: None,
                    };
                    let compiled = compile_node_with_lua(
                        env.clone(),
                        &child.node,
                        child.view_state,
                        &mut cache,
                    );
                    if !child_layouts.is_empty() {
                        child_layouts.push(blank_line_layout());
                    }
                    child_layouts.push(compiled.layout);
                    refresh_after_ms = earliest_delay(refresh_after_ms, compiled.refresh_after_ms);
                }
                let children = BlockLayout::Vbox(child_layouts);
                let mut cache = CompileLayoutCache {
                    content_sources: &[],
                    group_children: Some(&children),
                    refresh_after_ms: None,
                };
                let mut compiled =
                    compile_node_with_lua(env.clone(), &node, self.view_state, &mut cache);
                compiled.refresh_after_ms =
                    earliest_delay(compiled.refresh_after_ms, refresh_after_ms);
                compiled
            }
            CompileJobSource::Ready(layout) => CompiledLayout {
                layout,
                refresh_after_ms: None,
            },
        };
        (
            self.id,
            self.key,
            compiled.layout,
            compiled.refresh_after_ms.and_then(|after_ms| {
                env.refresh_now
                    .checked_add(std::time::Duration::from_millis(after_ms))
            }),
        )
    }
}

#[derive(Clone)]
pub(crate) struct MeasureCtx {
    pub width: u16,
    pub view_state: ViewState,
    pub inline_options: InlineOptions,
}

#[derive(Clone)]
pub(crate) struct RenderCtx<'a> {
    pub width: u16,
    pub view_state: ViewState,
    pub theme: &'a Theme,
    pub history: Option<&'a BlockHistory>,
    pub inline_options: InlineOptions,
}

struct CachedLayout {
    key: DisplayCacheKey,
    layout: LayoutIr,
    syntax_theme_revision: Option<u64>,
    measurements: VecDeque<CachedMeasurement>,
    rendered_ranges: VecDeque<CachedRenderRange>,
    weight: usize,
    refresh_at: Option<std::time::Instant>,
    content_hash_pending: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MeasurementKey {
    width: u16,
    inline_options: InlineOptions,
}

struct CachedMeasurement {
    key: MeasurementKey,
    layout: crate::content::display_renderers::MeasuredLayout,
    retained_bytes: usize,
    dirty: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RenderRangeKey {
    width: u16,
    view_state: ViewState,
    theme_revision: u64,
    row_start: usize,
    row_count: usize,
}

struct CachedRenderRange {
    key: RenderRangeKey,
    buffer: Buffer,
    line_count: usize,
    retained_bytes: usize,
    dirty: bool,
}

impl CachedRenderRange {
    fn new(key: RenderRangeKey, buffer: Buffer, line_count: usize) -> Self {
        let retained_bytes = rendered_buffer_retained_bytes(&buffer);
        Self {
            key,
            buffer,
            line_count,
            retained_bytes,
            dirty: false,
        }
    }
}

fn rendered_buffer_retained_bytes(buffer: &Buffer) -> usize {
    std::mem::size_of::<Buffer>()
        .saturating_add(
            buffer
                .lines()
                .len()
                .saturating_mul(std::mem::size_of::<String>()),
        )
        .saturating_add(buffer.lines().iter().map(String::capacity).sum::<usize>())
        .saturating_add(
            buffer
                .line_count()
                .saturating_mul(std::mem::size_of::<LineDecoration>()),
        )
}

// Syntax colors are paint state. Compile them with the render theme, not during layout measurement.
fn ensure_cached_syntax_theme(
    entry: &mut CachedLayout,
    theme: &Theme,
    inline_syntax: &mut InlineSyntaxCache,
) -> isize {
    if entry.syntax_theme_revision == Some(theme.revision()) {
        return 0;
    }
    let old_bytes = entry.layout.retained_bytes();
    compile_layout_syntax(&mut entry.layout, inline_syntax, theme);
    let new_bytes = entry.layout.retained_bytes();
    entry.syntax_theme_revision = Some(theme.revision());
    entry.weight = entry
        .weight
        .saturating_sub(old_bytes)
        .saturating_add(new_bytes);
    new_bytes as isize - old_bytes as isize
}

fn ensure_cached_measurement(entry: &mut CachedLayout, key: MeasurementKey) -> isize {
    let _perf = smelt_perf::perf::begin("transcript:layout_cache:ensure_measurement");
    if let Some(position) = entry
        .measurements
        .iter()
        .position(|measurement| measurement.key == key)
    {
        let measurement = entry
            .measurements
            .remove(position)
            .expect("cached measurement position");
        entry.measurements.push_back(measurement);
    }
    if entry
        .measurements
        .back()
        .is_some_and(|measurement| measurement.key == key && !measurement.dirty)
    {
        return 0;
    }
    if let Some(measurement) = entry
        .measurements
        .back_mut()
        .filter(|measurement| measurement.key == key)
    {
        if crate::content::display_renderers::refresh_layout_ir_content_measurements(
            &entry.layout,
            &mut measurement.layout,
            key.width,
            &key.inline_options,
        ) {
            measurement.dirty = false;
            return 0;
        }
    }

    let mut retained_delta = 0isize;
    if entry
        .measurements
        .back()
        .is_some_and(|measurement| measurement.key == key)
    {
        let old = entry
            .measurements
            .pop_back()
            .expect("dirty cached measurement");
        entry.weight = entry.weight.saturating_sub(old.retained_bytes);
        retained_delta -= old.retained_bytes as isize;
    } else if entry.measurements.len() >= 2 {
        let old = entry
            .measurements
            .pop_front()
            .expect("oldest cached measurement");
        entry.weight = entry.weight.saturating_sub(old.retained_bytes);
        retained_delta -= old.retained_bytes as isize;
    }

    let layout = crate::content::display_renderers::measure_layout_ir_plan(
        &entry.layout,
        key.width,
        &key.inline_options,
    );
    let retained_bytes = layout.retained_bytes();
    entry.weight = entry.weight.saturating_add(retained_bytes);
    retained_delta += retained_bytes as isize;
    entry.measurements.push_back(CachedMeasurement {
        key,
        layout,
        retained_bytes,
        dirty: false,
    });
    retained_delta
}

const INLINE_SYNTAX_CACHE_BUDGET: usize = 256 * 1024;

struct CachedInlineSyntax {
    language: String,
    source: String,
    spans: Arc<[smelt_core::content::highlight::InlineSyntaxSpan]>,
    retained_bytes: usize,
}

#[derive(Default)]
struct InlineSyntaxCache {
    entries: HashMap<u64, Vec<CachedInlineSyntax>>,
    lru: VecDeque<u64>,
    retained_bytes: usize,
    theme_revision: u64,
}

impl InlineSyntaxCache {
    fn get_or_compile(
        &mut self,
        theme: &Theme,
        language: &str,
        source: &str,
    ) -> Arc<[smelt_core::content::highlight::InlineSyntaxSpan]> {
        self.ensure_theme(theme.revision());
        let mut hasher = DefaultHasher::new();
        language.hash(&mut hasher);
        source.hash(&mut hasher);
        let key = hasher.finish();
        if let Some(spans) = self.entries.get(&key).and_then(|entries| {
            entries
                .iter()
                .find(|entry| entry.language == language && entry.source == source)
                .map(|entry| Arc::clone(&entry.spans))
        }) {
            self.touch(key);
            return spans;
        }

        let spans: Arc<[_]> = smelt_core::content::highlight::InlineSyntax::new(language, theme)
            .highlight_spans(source)
            .into();
        let retained_bytes = std::mem::size_of::<CachedInlineSyntax>()
            .saturating_add(language.len())
            .saturating_add(source.len())
            .saturating_add(spans.len().saturating_mul(std::mem::size_of::<
                smelt_core::content::highlight::InlineSyntaxSpan,
            >()));
        if retained_bytes <= INLINE_SYNTAX_CACHE_BUDGET {
            self.entries
                .entry(key)
                .or_default()
                .push(CachedInlineSyntax {
                    language: language.to_owned(),
                    source: source.to_owned(),
                    spans: Arc::clone(&spans),
                    retained_bytes,
                });
            self.retained_bytes = self.retained_bytes.saturating_add(retained_bytes);
            self.touch(key);
            while self.retained_bytes > INLINE_SYNTAX_CACHE_BUDGET {
                if !self.evict_oldest() {
                    break;
                }
            }
        }
        spans
    }

    fn ensure_theme(&mut self, theme_revision: u64) {
        if self.theme_revision == theme_revision {
            return;
        }
        self.entries.clear();
        self.lru.clear();
        self.retained_bytes = 0;
        self.theme_revision = theme_revision;
    }

    fn touch(&mut self, key: u64) {
        self.lru.retain(|candidate| *candidate != key);
        self.lru.push_back(key);
    }

    fn evict_oldest(&mut self) -> bool {
        let Some(key) = self.lru.pop_front() else {
            return false;
        };
        let Some(entries) = self.entries.remove(&key) else {
            return false;
        };
        self.retained_bytes = self.retained_bytes.saturating_sub(
            entries
                .iter()
                .map(|entry| entry.retained_bytes)
                .sum::<usize>(),
        );
        true
    }
}

struct CompileLayoutCache<'a> {
    content_sources: &'a [TranscriptContent],
    group_children: Option<&'a LayoutIr>,
    refresh_after_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct DisplayMemorySnapshot {
    pub(crate) layout_bytes: usize,
    pub(crate) pinned_layout_bytes: usize,
    pub(crate) oversize_debt_bytes: usize,
}

pub(crate) struct LayoutCache {
    blocks: HashMap<RenderNodeId, CachedLayout>,
    inline_syntax: InlineSyntaxCache,
    lru: RefCell<VecDeque<RenderNodeId>>,
    pinned: HashSet<RenderNodeId>,
    retained_bytes: usize,
    budget: usize,
    earliest_refresh_at: Option<std::time::Instant>,
}

impl Default for LayoutCache {
    fn default() -> Self {
        Self {
            blocks: HashMap::new(),
            inline_syntax: InlineSyntaxCache::default(),
            lru: RefCell::new(VecDeque::new()),
            pinned: HashSet::new(),
            retained_bytes: 0,
            budget: 16 * 1024 * 1024,
            earliest_refresh_at: None,
        }
    }
}

impl LayoutCache {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn set_budget(&mut self, budget: usize) {
        self.budget = budget;
        self.enforce_budget();
    }

    pub(crate) fn set_pinned_nodes(&mut self, ids: impl IntoIterator<Item = RenderNodeId>) {
        self.pinned.clear();
        self.pinned.extend(ids);
        self.enforce_budget();
    }

    pub(crate) fn retained_bytes(&self) -> usize {
        self.retained_bytes
            .saturating_add(self.inline_syntax.retained_bytes)
    }

    pub(crate) fn memory_snapshot(&self) -> DisplayMemorySnapshot {
        let pinned_layout_bytes = self
            .pinned
            .iter()
            .filter_map(|id| self.blocks.get(id).map(|entry| entry.weight))
            .sum();
        DisplayMemorySnapshot {
            layout_bytes: self
                .retained_bytes
                .saturating_add(self.inline_syntax.retained_bytes),
            pinned_layout_bytes,
            oversize_debt_bytes: self.retained_bytes().saturating_sub(self.budget),
        }
    }

    pub(crate) fn apply_content_append(
        &mut self,
        id: RenderNodeId,
        content: &TranscriptContent,
        ranges: &[std::ops::Range<usize>],
    ) -> bool {
        let Some(entry) = self.blocks.get_mut(&id) else {
            return false;
        };
        if ranges
            .iter()
            .any(|range| range.end < range.start || range.end > content.len())
            || retained_content_spec_mut(&mut entry.layout, content.id()).is_none()
        {
            return false;
        }
        entry.content_hash_pending = true;
        for measurement in &mut entry.measurements {
            measurement.dirty = true;
        }
        for range in &mut entry.rendered_ranges {
            range.dirty = true;
        }
        true
    }

    fn promote_appended_content_key(&mut self, id: RenderNodeId, key: DisplayCacheKey) -> bool {
        let Some(entry) = self.blocks.get_mut(&id) else {
            return false;
        };
        if !entry.content_hash_pending || !entry.key.same_render_inputs_except_content(key) {
            return false;
        }
        entry.key = key;
        entry.content_hash_pending = false;
        self.touch(id);
        true
    }

    fn touch(&self, id: RenderNodeId) {
        let mut lru = self.lru.borrow_mut();
        lru.retain(|candidate| *candidate != id);
        lru.push_back(id);
    }

    fn recompute_earliest_refresh_at(&mut self) {
        self.earliest_refresh_at = self
            .blocks
            .values()
            .filter_map(|entry| entry.refresh_at)
            .min();
    }

    fn enforce_budget(&mut self) {
        let mut attempts = self.lru.borrow().len();
        while self.retained_bytes() > self.budget && attempts > 0 {
            attempts -= 1;
            let Some(id) = self.lru.borrow_mut().pop_front() else {
                break;
            };
            if self.pinned.contains(&id) {
                self.lru.borrow_mut().push_back(id);
                continue;
            }
            if let Some(entry) = self.blocks.remove(&id) {
                self.retained_bytes = self.retained_bytes.saturating_sub(entry.weight);
                attempts = self.lru.borrow().len();
            }
        }
        while self.retained_bytes() > self.budget && self.inline_syntax.evict_oldest() {}
        self.recompute_earliest_refresh_at();
        smelt_perf::perf::record_value(
            "transcript:render_cache:retained_bytes",
            self.retained_bytes() as u64,
        );
        smelt_perf::perf::record_value(
            "transcript:render_cache:pinned_bytes",
            self.pinned
                .iter()
                .filter_map(|id| self.blocks.get(id).map(|entry| entry.weight))
                .sum::<usize>() as u64,
        );
        smelt_perf::perf::record_value(
            "transcript:render_cache:oversize_debt_bytes",
            self.retained_bytes().saturating_sub(self.budget) as u64,
        );
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.blocks.len()
    }

    #[cfg(test)]
    pub(crate) fn ensure_many(
        &mut self,
        env: TranscriptRenderEnv<'_>,
        history: &BlockHistory,
        ids: &[BlockId],
        keys: &[NodeLayoutKey],
    ) -> usize {
        let renderer_generation = env.renderer_generation;
        let renderer_cache_key = env.renderer_cache_key;
        let blocks = ids
            .iter()
            .copied()
            .zip(keys.iter().copied())
            .filter_map(|(id, key)| {
                history
                    .order
                    .iter()
                    .position(|candidate| *candidate == id)
                    .map(|index| {
                        (
                            index,
                            RenderNode::Block {
                                id,
                                block_index: index,
                            },
                            key,
                        )
                    })
            });
        let jobs = self.collect_compile_jobs(
            history,
            &TranscriptDefaultViewPolicy::default(),
            renderer_generation,
            renderer_cache_key,
            blocks,
        );
        let compiled = jobs.len();
        self.compile_and_insert(env, jobs);
        compiled
    }

    /// Returns compile jobs for cache misses. The caller can run these jobs on
    /// the current thread or schedule them onto a worker pool, then insert the
    /// results with `insert_compiled_blocks`.
    pub(crate) fn collect_compile_jobs(
        &mut self,
        history: &BlockHistory,
        policy: &TranscriptDefaultViewPolicy,
        renderer_generation: u64,
        renderer_cache_key: Option<u64>,
        nodes: impl IntoIterator<Item = (usize, RenderNode, NodeLayoutKey)>,
    ) -> Vec<CompileJob> {
        let _perf = smelt_perf::perf::begin("transcript:layout_cache:ensure_many");

        let mut jobs = Vec::new();
        let mut requested = 0;
        let mut removed_cached_layout = false;
        for (index, node, key) in nodes {
            requested += 1;
            let id = node.id();
            let display_key =
                DisplayCacheKey::from_node_key(key, renderer_generation, renderer_cache_key);
            let cached_key = self.blocks.get(&id).map(|cached| cached.key);
            if cached_key == Some(display_key) {
                self.touch(id);
                continue;
            }
            smelt_perf::perf::record_value(
                if cached_key.is_some() {
                    "transcript:layout_cache:key_miss"
                } else {
                    "transcript:layout_cache:entry_miss"
                },
                1,
            );
            if let Some(cached_key) = cached_key {
                for (changed, label) in [
                    (
                        cached_key.content_hash != display_key.content_hash,
                        "transcript:layout_cache:content_key_miss",
                    ),
                    (
                        cached_key.sidecar_hash != display_key.sidecar_hash,
                        "transcript:layout_cache:sidecar_key_miss",
                    ),
                    (
                        cached_key.renderer_generation != display_key.renderer_generation,
                        "transcript:layout_cache:renderer_key_miss",
                    ),
                    (
                        cached_key.render_context_hash != display_key.render_context_hash,
                        "transcript:layout_cache:context_key_miss",
                    ),
                ] {
                    if changed {
                        smelt_perf::perf::record_value(label, 1);
                    }
                }
            }
            smelt_perf::perf::record_value(
                if matches!(&node, RenderNode::Group(_)) {
                    "transcript:layout_cache:group_miss"
                } else {
                    "transcript:layout_cache:block_miss"
                },
                1,
            );
            if self.promote_appended_content_key(id, display_key) {
                continue;
            }
            let source = match node {
                RenderNode::Block {
                    id: block_id,
                    block_index,
                } => {
                    let Some(block) = history.block(block_id) else {
                        if let Some(removed) = self.blocks.remove(&id) {
                            self.retained_bytes =
                                self.retained_bytes.saturating_sub(removed.weight);
                            removed_cached_layout = true;
                        }
                        self.lru.borrow_mut().retain(|candidate| *candidate != id);
                        continue;
                    };
                    if history.status(block_id) == Some(Status::Streaming) {
                        let live_layout = if key.view_state == ViewState::Expanded {
                            expanded_live_streaming_layout(block)
                        } else {
                            None
                        };
                        if let Some(layout) = live_layout {
                            CompileJobSource::Ready(layout)
                        } else {
                            let Some(node) = block_render_node(history, block_id, block_index)
                            else {
                                continue;
                            };
                            CompileJobSource::Metadata {
                                node,
                                content_sources: block_content_sources(
                                    block,
                                    history.tool_state(block_id),
                                ),
                            }
                        }
                    } else {
                        let Some(node) = block_render_node(history, block_id, block_index) else {
                            continue;
                        };
                        CompileJobSource::Metadata {
                            node,
                            content_sources: block_content_sources(
                                block,
                                history.tool_state(block_id),
                            ),
                        }
                    }
                }
                RenderNode::Group(group) => CompileJobSource::Group {
                    node: group_render_node(index, &group, key.view_state),
                    children: if key.view_state == ViewState::Expanded {
                        group_child_compile_sources(history, policy, &group)
                    } else {
                        Vec::new()
                    },
                },
            };
            jobs.push(CompileJob {
                id,
                key: display_key,
                view_state: key.view_state,
                source,
            });
        }
        if removed_cached_layout {
            self.recompute_earliest_refresh_at();
        }
        smelt_perf::perf::record_value("transcript:layout_cache:requested", requested);
        smelt_perf::perf::record_value("transcript:layout_cache:compiled", jobs.len() as u64);
        jobs
    }

    pub(crate) fn compile_and_insert(
        &mut self,
        env: TranscriptRenderEnv<'_>,
        jobs: Vec<CompileJob>,
    ) {
        let _perf = smelt_perf::perf::begin("transcript:layout_cache:compile_and_insert");
        let mut layouts = Vec::with_capacity(jobs.len());
        {
            let _perf = smelt_perf::perf::begin("transcript:layout_cache:compile_layouts");
            for job in jobs {
                layouts.push(job.compile(env.clone()));
            }
        }
        self.insert_compiled_blocks(layouts);
    }

    pub(crate) fn insert_compiled_blocks(
        &mut self,
        layouts: Vec<(
            RenderNodeId,
            DisplayCacheKey,
            LayoutIr,
            Option<std::time::Instant>,
        )>,
    ) {
        let _perf = smelt_perf::perf::begin("transcript:layout_cache:insert_cache");
        for (id, key, layout, refresh_at) in layouts {
            let weight = layout.retained_bytes();
            if let Some(previous) = self.blocks.remove(&id) {
                self.retained_bytes = self.retained_bytes.saturating_sub(previous.weight);
            }
            self.retained_bytes = self.retained_bytes.saturating_add(weight);
            self.blocks.insert(
                id,
                CachedLayout {
                    key,
                    layout,
                    syntax_theme_revision: None,
                    measurements: VecDeque::new(),
                    rendered_ranges: VecDeque::new(),
                    weight,
                    refresh_at,
                    content_hash_pending: false,
                },
            );
            self.touch(id);
        }
        self.enforce_budget();
    }

    pub(crate) fn retain_nodes(&mut self, ids: impl IntoIterator<Item = RenderNodeId>) {
        let live: HashSet<RenderNodeId> = ids.into_iter().collect();
        self.blocks.retain(|id, entry| {
            if live.contains(id) {
                true
            } else {
                self.retained_bytes = self.retained_bytes.saturating_sub(entry.weight);
                false
            }
        });
        self.lru.borrow_mut().retain(|id| live.contains(id));
        self.pinned.retain(|id| live.contains(id));
        self.enforce_budget();
    }

    pub(crate) fn next_refresh_at(&self) -> Option<std::time::Instant> {
        self.earliest_refresh_at
    }

    pub(crate) fn expire_due_refreshes(&mut self, now: std::time::Instant) -> Vec<RenderNodeId> {
        let due = self
            .blocks
            .iter()
            .filter_map(|(id, entry)| {
                entry
                    .refresh_at
                    .filter(|deadline| *deadline <= now)
                    .map(|_| *id)
            })
            .collect::<Vec<_>>();
        for id in &due {
            if let Some(entry) = self.blocks.remove(id) {
                self.retained_bytes = self.retained_bytes.saturating_sub(entry.weight);
            }
        }
        if !due.is_empty() {
            let due_set = due.iter().copied().collect::<HashSet<_>>();
            self.lru.borrow_mut().retain(|id| !due_set.contains(id));
            self.recompute_earliest_refresh_at();
        }
        due
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn with_rendered_range<T>(
        &mut self,
        id: RenderNodeId,
        key: NodeLayoutKey,
        renderer_generation: u64,
        renderer_cache_key: Option<u64>,
        ctx: RenderCtx<'_>,
        row_start: usize,
        row_count: usize,
        visit: impl FnOnce(&Buffer, usize) -> T,
    ) -> Option<T> {
        let display_key =
            DisplayCacheKey::from_node_key(key, renderer_generation, renderer_cache_key);
        let range_key = RenderRangeKey {
            width: ctx.width,
            view_state: ctx.view_state,
            theme_revision: ctx.theme.revision(),
            row_start,
            row_count,
        };

        let mut retained_delta = 0isize;
        let result = {
            let inline_syntax = &mut self.inline_syntax;
            let entry = self
                .blocks
                .get_mut(&id)
                .filter(|cached| cached.key == display_key)?;
            retained_delta += ensure_cached_syntax_theme(entry, ctx.theme, inline_syntax);
            retained_delta += ensure_cached_measurement(
                entry,
                MeasurementKey {
                    width: ctx.width,
                    inline_options: ctx.inline_options.clone(),
                },
            );
            let position = entry
                .rendered_ranges
                .iter()
                .position(|range| range.key == range_key);
            if let Some(position) = position {
                let range = entry
                    .rendered_ranges
                    .remove(position)
                    .expect("cached render range position");
                entry.rendered_ranges.push_back(range);
            }

            let needs_render = entry
                .rendered_ranges
                .back()
                .is_none_or(|range| range.key != range_key || range.dirty);
            if needs_render {
                let reusable_position = entry
                    .rendered_ranges
                    .back()
                    .filter(|range| range.key == range_key)
                    .map(|_| entry.rendered_ranges.len().saturating_sub(1))
                    .or_else(|| entry.rendered_ranges.iter().rposition(|range| range.dirty))
                    .or_else(|| (entry.rendered_ranges.len() >= 2).then_some(0));
                let buffer = if let Some(position) = reusable_position {
                    let old = entry
                        .rendered_ranges
                        .remove(position)
                        .expect("reusable cached render range");
                    entry.weight = entry.weight.saturating_sub(old.retained_bytes);
                    retained_delta -= old.retained_bytes as isize;
                    old.buffer
                } else {
                    Buffer::new(BufId(0), BufCreateOpts::default())
                };
                let measured = &entry
                    .measurements
                    .back()
                    .expect("measurement ensured above")
                    .layout;
                let (buffer, line_count) = render_layout_range_to_buffer(
                    &entry.layout,
                    measured,
                    ctx,
                    row_start,
                    row_count,
                    buffer,
                );
                let rendered = CachedRenderRange::new(range_key, buffer, line_count);
                let new_bytes = rendered.retained_bytes;
                entry.weight = entry.weight.saturating_add(new_bytes);
                retained_delta += new_bytes as isize;
                entry.rendered_ranges.push_back(rendered);
                while entry.rendered_ranges.len() > 2 {
                    if let Some(old) = entry.rendered_ranges.pop_front() {
                        entry.weight = entry.weight.saturating_sub(old.retained_bytes);
                        retained_delta -= old.retained_bytes as isize;
                    }
                }
            }

            let rendered = entry.rendered_ranges.back()?;
            visit(&rendered.buffer, rendered.line_count)
        };

        if retained_delta >= 0 {
            self.retained_bytes = self.retained_bytes.saturating_add(retained_delta as usize);
        } else {
            self.retained_bytes = self
                .retained_bytes
                .saturating_sub(retained_delta.unsigned_abs());
        }
        self.touch(id);
        self.enforce_budget();
        Some(result)
    }

    pub(crate) fn measure_height(
        &mut self,
        id: RenderNodeId,
        key: NodeLayoutKey,
        renderer_generation: u64,
        renderer_cache_key: Option<u64>,
        ctx: MeasureCtx,
    ) -> Option<u64> {
        let display_key =
            DisplayCacheKey::from_node_key(key, renderer_generation, renderer_cache_key);
        let (rows, retained_delta) = {
            let entry = self
                .blocks
                .get_mut(&id)
                .filter(|cached| cached.key == display_key)?;
            let retained_delta = ensure_cached_measurement(
                entry,
                MeasurementKey {
                    width: ctx.width,
                    inline_options: ctx.inline_options,
                },
            );
            let rows = entry
                .measurements
                .back()
                .expect("measurement ensured above")
                .layout
                .rows() as u64;
            (ctx.view_state.measured_height(rows), retained_delta)
        };
        if retained_delta >= 0 {
            self.retained_bytes = self.retained_bytes.saturating_add(retained_delta as usize);
        } else {
            self.retained_bytes = self
                .retained_bytes
                .saturating_sub(retained_delta.unsigned_abs());
        }
        self.touch(id);
        self.enforce_budget();
        Some(rows)
    }

    pub(crate) fn get(
        &self,
        id: RenderNodeId,
        key: NodeLayoutKey,
        renderer_generation: u64,
        renderer_cache_key: Option<u64>,
    ) -> Option<&LayoutIr> {
        let display_key =
            DisplayCacheKey::from_node_key(key, renderer_generation, renderer_cache_key);
        let layout = self
            .blocks
            .get(&id)
            .filter(|cached| cached.key == display_key)
            .map(|cached| &cached.layout)?;
        self.touch(id);
        Some(layout)
    }
}

fn block_content_sources(
    block: &Block,
    state: Option<&smelt_core::transcript_model::ToolState>,
) -> Vec<TranscriptContent> {
    let mut sources = block
        .registered_contents()
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    if let Some(state) = state {
        sources.extend(state.registered_contents().cloned());
    }
    sources
}

pub(crate) fn live_streaming_markdown_content(block: &Block) -> Option<&TranscriptContent> {
    match block {
        Block::Text { content } => Some(content),
        Block::Thinking {
            title: None,
            summary_titles,
            content,
            ..
        } if summary_titles.is_empty() => Some(content),
        _ => None,
    }
}

fn expanded_live_streaming_layout(block: &Block) -> Option<LayoutIr> {
    let content = live_streaming_markdown_content(block)?;
    match block {
        Block::Text { .. } => Some(markdown_content_layout(content.clone(), false, false)),
        Block::Thinking { .. } => Some(BlockLayout::Gutter {
            child: Box::new(BlockLayout::Style {
                child: Box::new(markdown_content_layout(content.clone(), true, true)),
                spec: StyleSpec {
                    dim: true,
                    italic: true,
                    ..StyleSpec::default()
                },
            }),
            spec: GutterSpec {
                text: "│ ".into(),
                styled: true,
            },
        }),
        _ => None,
    }
}

fn markdown_content_layout(content: TranscriptContent, dim: bool, italic: bool) -> LayoutIr {
    BlockLayout::Leaf(IrLeaf::Content(RetainedContentSpec {
        content,
        render: ContentRenderSpec::Markdown {
            dim,
            italic,
            inline: false,
        },
    }))
}

fn retained_content_spec_mut(
    layout: &mut LayoutIr,
    content_id: smelt_core::transcript_content::ContentId,
) -> Option<&mut RetainedContentSpec> {
    match layout {
        BlockLayout::Leaf(IrLeaf::Content(spec)) if spec.content.id() == content_id => Some(spec),
        BlockLayout::Vbox(items) => items
            .iter_mut()
            .find_map(|item| retained_content_spec_mut(item, content_id)),
        BlockLayout::Hbox(items) => items
            .iter_mut()
            .find_map(|item| retained_content_spec_mut(&mut item.layout, content_id)),
        BlockLayout::Gutter { child, .. }
        | BlockLayout::RowPrefix { child, .. }
        | BlockLayout::Panel { child, .. }
        | BlockLayout::Style { child, .. }
        | BlockLayout::Cap { child, .. }
        | BlockLayout::Refresh { child, .. } => retained_content_spec_mut(child, content_id),
        BlockLayout::Empty | BlockLayout::Leaf(_) => None,
    }
}

#[cfg(test)]
pub(crate) fn compile_block(block: &Block) -> LayoutIr {
    let lua = LuaRuntime::new();
    let content_sources = block_content_sources(block, None);
    let mut cache = CompileLayoutCache {
        content_sources: &content_sources,
        group_children: None,
        refresh_after_ms: None,
    };
    let node =
        smelt_core::lua::runtime::transcript_block_render_node(BlockId::new(0), 0, block, None);
    compile_node_with_lua(
        TranscriptRenderEnv::new(&lua),
        &node,
        ViewState::Expanded,
        &mut cache,
    )
    .layout
}

fn group_render_node(
    node_index: usize,
    group: &RenderGroupNode,
    view_state: ViewState,
) -> TranscriptRenderNode {
    TranscriptRenderNode::group(
        group.id,
        node_index,
        group.name.clone(),
        group.bucket.clone(),
        view_state_label(view_state),
        group
            .children
            .iter()
            .map(|child| child.metadata.clone())
            .collect(),
        group.child_ids().collect(),
    )
}

fn group_child_compile_sources(
    history: &BlockHistory,
    policy: &TranscriptDefaultViewPolicy,
    group: &RenderGroupNode,
) -> Vec<GroupChildCompileSource> {
    group
        .children
        .iter()
        .enumerate()
        .filter_map(|(offset, child)| {
            let block_index = group.child_range.start.saturating_add(offset);
            let block = history.block(child.id)?;
            let view_state = policy.node_default_view_state(
                history,
                &RenderNode::Block {
                    id: child.id,
                    block_index,
                },
            );
            Some(GroupChildCompileSource {
                node: block_render_node(history, child.id, block_index)?,
                view_state,
                content_sources: block_content_sources(block, history.tool_state(child.id)),
            })
        })
        .collect()
}

fn block_render_node(
    history: &BlockHistory,
    id: smelt_core::transcript_model::BlockId,
    block_index: usize,
) -> Option<TranscriptRenderNode> {
    let _perf = smelt_perf::perf::begin("transcript:layout_cache:block_render_metadata");
    let block = history.block(id)?;
    let state = match block {
        Block::ToolCall { .. } => history.tool_state(id),
        _ => None,
    };
    Some(smelt_core::lua::runtime::transcript_block_render_node(
        id,
        block_index,
        block,
        state,
    ))
}

fn view_state_label(view_state: ViewState) -> &'static str {
    match view_state {
        ViewState::Expanded => "expanded",
        ViewState::Peek => "peek",
        ViewState::Collapsed => "collapsed",
        ViewState::TrimmedHead { .. } => "trimmed_head",
        ViewState::TrimmedTail { .. } => "trimmed_tail",
    }
}

fn compile_node_with_lua(
    env: TranscriptRenderEnv<'_>,
    node: &TranscriptRenderNode,
    view_state: ViewState,
    cache: &mut CompileLayoutCache<'_>,
) -> CompiledLayout {
    let kind = node.kind();
    let index = node.index();
    let layout = env
        .lua
        .render_transcript_layout(node, view_state, env.now_ms);
    match compile_layout_ir_with_cache(&layout, cache) {
        Ok(layout) => CompiledLayout {
            layout,
            refresh_after_ms: cache.refresh_after_ms,
        },
        Err(error) => {
            env.lua.record_error(format!(
                "transcript render `{kind}` #{index}: compile layout IR: {error}"
            ));
            CompiledLayout {
                layout: BlockLayout::Leaf(IrLeaf::Text(TextSpec {
                    content: format!("{kind} render error"),
                    hl_group: Some("ErrorMsg".into()),
                    ansi: false,
                })),
                refresh_after_ms: None,
            }
        }
    }
}

pub(crate) fn compile_layout_ir(layout: &BlockLayout) -> Result<LayoutIr, String> {
    let mut cache = CompileLayoutCache {
        content_sources: &[],
        group_children: None,
        refresh_after_ms: None,
    };
    compile_layout_ir_with_cache(layout, &mut cache)
}

fn compile_layout_ir_with_cache(
    layout: &BlockLayout,
    cache: &mut CompileLayoutCache<'_>,
) -> Result<LayoutIr, String> {
    match layout {
        BlockLayout::Empty => Ok(BlockLayout::Empty),
        BlockLayout::Leaf(LuaLeaf::Text(spec)) => Ok(BlockLayout::Leaf(IrLeaf::Text(TextSpec {
            content: spec.content.clone(),
            hl_group: spec.hl_group.clone(),
            ansi: spec.ansi,
        }))),
        BlockLayout::Leaf(LuaLeaf::Content(spec)) => {
            let content = cache
                .content_sources
                .iter()
                .find(|content| content.id() == spec.id)
                .cloned()
                .ok_or_else(|| format!("unknown transcript content id {}", spec.id.get()))?;
            Ok(BlockLayout::Leaf(IrLeaf::Content(RetainedContentSpec {
                content,
                render: spec.render.clone(),
            })))
        }
        BlockLayout::Leaf(LuaLeaf::ContentDiff(spec)) => {
            let source = |id: smelt_core::transcript_content::ContentId| {
                cache
                    .content_sources
                    .iter()
                    .find(|content| content.id() == id)
                    .ok_or_else(|| format!("unknown transcript content id {}", id.get()))
            };
            let old = source(spec.old_id)?;
            let new = source(spec.new_id)?;
            let ext = spec
                .lang
                .as_deref()
                .map(smelt_core::content::highlight::lang_to_ext);
            let ir =
                smelt_core::content::highlight::build_retained_diff_ir(old, new, &spec.path, ext);
            Ok(BlockLayout::Leaf(IrLeaf::SourceView(SourceViewIr::Diff(
                ir,
            ))))
        }
        BlockLayout::Leaf(LuaLeaf::GroupChildren) => cache
            .group_children
            .cloned()
            .ok_or_else(|| "group child layouts are only available in group renderers".to_string()),
        BlockLayout::Leaf(LuaLeaf::Runs(spec)) => Ok(BlockLayout::Leaf(IrLeaf::Runs(spec.clone()))),
        BlockLayout::Leaf(LuaLeaf::Line(spec)) => Ok(BlockLayout::Leaf(IrLeaf::Line(spec.clone()))),
        BlockLayout::Leaf(LuaLeaf::Markdown(spec)) => {
            Ok(BlockLayout::Leaf(IrLeaf::Markdown(spec.clone())))
        }
        BlockLayout::Leaf(LuaLeaf::Code(spec)) => Ok(BlockLayout::Leaf(IrLeaf::Code(spec.clone()))),
        BlockLayout::Leaf(LuaLeaf::Separator(spec)) => {
            Ok(BlockLayout::Leaf(IrLeaf::Separator(spec.clone())))
        }
        BlockLayout::Leaf(LuaLeaf::Diff(spec)) => {
            let ext = spec
                .lang
                .as_deref()
                .map(smelt_core::content::highlight::lang_to_ext);
            let ir = if spec.full_file {
                smelt_core::content::highlight::build_diff_ir_ext_with_base(
                    &spec.old,
                    &spec.new,
                    &spec.path,
                    &spec.anchor,
                    ext,
                    Some(&spec.old),
                )
            } else {
                smelt_core::content::highlight::build_diff_ir_ext_with_source(
                    &spec.old,
                    &spec.new,
                    &spec.path,
                    &spec.anchor,
                    ext,
                    &spec.base,
                )
            };
            Ok(BlockLayout::Leaf(IrLeaf::SourceView(SourceViewIr::Diff(
                ir,
            ))))
        }
        BlockLayout::Leaf(LuaLeaf::FileView(spec)) => {
            let ext = spec
                .lang
                .as_deref()
                .map(smelt_core::content::highlight::lang_to_ext)
                .or_else(|| {
                    std::path::Path::new(&spec.path)
                        .extension()
                        .and_then(|e| e.to_str())
                });
            let ir = smelt_core::content::highlight::build_file_view_ir(&spec.content, ext);
            Ok(BlockLayout::Leaf(IrLeaf::SourceView(SourceViewIr::Diff(
                ir,
            ))))
        }
        BlockLayout::Vbox(items) => items
            .iter()
            .map(|layout| compile_layout_ir_with_cache(layout, cache))
            .collect::<Result<Vec<_>, _>>()
            .map(BlockLayout::Vbox),
        BlockLayout::Hbox(items) => items
            .iter()
            .map(|item| {
                Ok(HboxItem {
                    constraint: item.constraint,
                    layout: compile_layout_ir_with_cache(&item.layout, cache)?,
                    copy_owner: item.copy_owner,
                })
            })
            .collect::<Result<Vec<_>, _>>()
            .map(BlockLayout::Hbox),
        BlockLayout::Gutter { child, spec } => Ok(BlockLayout::Gutter {
            child: Box::new(compile_layout_ir_with_cache(child, cache)?),
            spec: spec.clone(),
        }),
        BlockLayout::RowPrefix { child, spec } => Ok(BlockLayout::RowPrefix {
            child: Box::new(compile_layout_ir_with_cache(child, cache)?),
            spec: spec.clone(),
        }),
        BlockLayout::Panel { child, spec } => Ok(BlockLayout::Panel {
            child: Box::new(compile_layout_ir_with_cache(child, cache)?),
            spec: spec.clone(),
        }),
        BlockLayout::Style { child, spec } => Ok(BlockLayout::Style {
            child: Box::new(compile_layout_ir_with_cache(child, cache)?),
            spec: spec.clone(),
        }),
        BlockLayout::Cap { child, spec } => Ok(BlockLayout::Cap {
            child: Box::new(compile_layout_ir_with_cache(child, cache)?),
            spec: spec.clone(),
        }),
        BlockLayout::Refresh { child, spec } => {
            cache.refresh_after_ms = Some(
                cache
                    .refresh_after_ms
                    .map_or(spec.after_ms, |current| current.min(spec.after_ms)),
            );
            compile_layout_ir_with_cache(child, cache)
        }
    }
}

fn compile_layout_syntax(
    layout: &mut LayoutIr,
    inline_syntax: &mut InlineSyntaxCache,
    theme: &Theme,
) {
    match layout {
        BlockLayout::Empty
        | BlockLayout::Leaf(IrLeaf::Text(_))
        | BlockLayout::Leaf(IrLeaf::Content(_))
        | BlockLayout::Leaf(IrLeaf::Markdown(_))
        | BlockLayout::Leaf(IrLeaf::Code(_))
        | BlockLayout::Leaf(IrLeaf::Separator(_))
        | BlockLayout::Leaf(IrLeaf::SourceView(_)) => {}
        BlockLayout::Leaf(IrLeaf::Runs(spec)) => {
            spec.syntax_highlights = compile_styled_syntax(&spec.lines.0, inline_syntax, theme);
        }
        BlockLayout::Leaf(IrLeaf::Line(spec)) => {
            spec.syntax_highlights = compile_span_syntax(&spec.spans, inline_syntax, theme);
        }
        BlockLayout::Vbox(items) => {
            for child in items {
                compile_layout_syntax(child, inline_syntax, theme);
            }
        }
        BlockLayout::Hbox(items) => {
            for item in items {
                compile_layout_syntax(&mut item.layout, inline_syntax, theme);
            }
        }
        BlockLayout::Gutter { child, .. }
        | BlockLayout::RowPrefix { child, .. }
        | BlockLayout::Panel { child, .. }
        | BlockLayout::Style { child, .. }
        | BlockLayout::Cap { child, .. }
        | BlockLayout::Refresh { child, .. } => compile_layout_syntax(child, inline_syntax, theme),
    }
}

fn compile_styled_syntax(
    lines: &[Vec<protocol::StyledSpan>],
    inline_syntax: &mut InlineSyntaxCache,
    theme: &Theme,
) -> RetainedInlineSyntax {
    if !lines.iter().flatten().any(has_inline_syntax) {
        return RetainedInlineSyntax::default();
    }
    let source_span_count = lines.iter().map(Vec::len).sum();
    let mut source_spans = Vec::with_capacity(source_span_count);
    let mut line_offsets = Vec::with_capacity(lines.len().saturating_add(1));
    line_offsets.push(0);
    for spans in lines {
        append_span_syntax(spans, inline_syntax, theme, &mut source_spans);
        line_offsets.push(source_spans.len());
    }
    RetainedInlineSyntax::new(source_spans, line_offsets)
}

fn compile_span_syntax(
    spans: &[protocol::StyledSpan],
    inline_syntax: &mut InlineSyntaxCache,
    theme: &Theme,
) -> RetainedInlineSyntax {
    if !spans.iter().any(has_inline_syntax) {
        return RetainedInlineSyntax::default();
    }
    let mut source_spans = Vec::with_capacity(spans.len());
    append_span_syntax(spans, inline_syntax, theme, &mut source_spans);
    let source_span_count = source_spans.len();
    RetainedInlineSyntax::new(source_spans, vec![0, source_span_count])
}

fn append_span_syntax(
    spans: &[protocol::StyledSpan],
    inline_syntax: &mut InlineSyntaxCache,
    theme: &Theme,
    out: &mut Vec<Arc<[smelt_core::content::highlight::InlineSyntaxSpan]>>,
) {
    out.extend(spans.iter().map(|span| {
        span.syntax
            .as_deref()
            .filter(|_| span.selectable)
            .map_or_else(Arc::default, |language| {
                inline_syntax.get_or_compile(theme, language, &span.text)
            })
    }));
}

fn has_inline_syntax(span: &protocol::StyledSpan) -> bool {
    span.selectable && span.syntax.is_some()
}

#[cfg(test)]
pub(crate) fn measure_block(layout: &LayoutIr, ctx: MeasureCtx) -> u64 {
    let _perf = smelt_perf::perf::begin("transcript:measure_block:layout");
    let expanded_rows = crate::content::display_renderers::measure_layout_ir_with_options(
        layout,
        ctx.width,
        &ctx.inline_options,
    ) as u64;
    ctx.view_state.measured_height(expanded_rows)
}

pub(crate) fn render_block_into(
    buf: &mut Buffer,
    layout: &LayoutIr,
    ctx: RenderCtx<'_>,
) -> Outcome {
    render_block_into_mode(buf, layout, ctx, false)
}

fn render_block_into_mode(
    buf: &mut Buffer,
    layout: &LayoutIr,
    ctx: RenderCtx<'_>,
    replacing: bool,
) -> Outcome {
    let outcome = {
        let mut out = if replacing {
            LineBuilder::replacing(buf, ctx.theme, ctx.width)
        } else {
            LineBuilder::new(buf, ctx.theme, ctx.width)
        };
        render_expanded_block(
            &mut out,
            layout,
            ctx.width as usize,
            ctx.history,
            &ctx.inline_options,
        );
        out.finish()
    };
    apply_view_state(buf, ctx.theme, ctx.width, ctx.view_state, outcome)
}

fn render_layout_range_to_buffer(
    layout: &LayoutIr,
    measured: &crate::content::display_renderers::MeasuredLayout,
    ctx: RenderCtx<'_>,
    row_start: usize,
    row_count: usize,
    mut buffer: Buffer,
) -> (Buffer, usize) {
    let outcome = {
        let _perf = smelt_perf::perf::begin("transcript:layout_cache:render_range_to_buffer");
        render_block_range_into_mode(
            &mut buffer,
            layout,
            measured,
            ctx,
            row_start,
            row_count,
            true,
        )
    };
    (buffer, outcome.line_count)
}

fn render_block_range_into_mode(
    buf: &mut Buffer,
    layout: &LayoutIr,
    measured: &crate::content::display_renderers::MeasuredLayout,
    ctx: RenderCtx<'_>,
    row_start: usize,
    row_count: usize,
    replacing: bool,
) -> Outcome {
    if row_count == 0 {
        return Outcome::default();
    }
    if ctx.view_state != ViewState::Expanded {
        return render_projected_view_range(
            buf, layout, measured, ctx, row_start, row_count, replacing,
        );
    }
    let mut out = if replacing {
        LineBuilder::replacing(buf, ctx.theme, ctx.width)
    } else {
        LineBuilder::new(buf, ctx.theme, ctx.width)
    };
    render_expanded_block_range(&mut out, layout, measured, (row_start, row_count), &ctx);
    out.finish()
}

fn render_projected_view_range(
    buf: &mut Buffer,
    layout: &LayoutIr,
    measured: &crate::content::display_renderers::MeasuredLayout,
    ctx: RenderCtx<'_>,
    row_start: usize,
    row_count: usize,
    replacing: bool,
) -> Outcome {
    let total = measured.rows();
    let projected_total = ctx.view_state.measured_height(total as u64) as usize;
    let start = row_start.min(projected_total);
    let end = start.saturating_add(row_count).min(projected_total);
    let mut out = if replacing {
        LineBuilder::replacing(buf, ctx.theme, ctx.width)
    } else {
        LineBuilder::new(buf, ctx.theme, ctx.width)
    };

    match ctx.view_state {
        ViewState::Expanded | ViewState::Peek => {
            render_expanded_block_range(
                &mut out,
                layout,
                measured,
                (start, end.saturating_sub(start)),
                &ctx,
            );
        }
        ViewState::Collapsed if total > 1 => {
            if start == 0 && end > 0 {
                render_expanded_block_range(&mut out, layout, measured, (0, 1), &ctx);
            }
            if start <= 1 && end > 1 {
                render_view_ellipsis(&mut out, &format!("… {} more lines", total - 1));
            }
        }
        ViewState::TrimmedHead { keep } if total > usize::from(keep) => {
            let keep = usize::from(keep);
            let source_start = start.min(keep);
            let source_end = end.min(keep);
            if source_start < source_end {
                render_expanded_block_range(
                    &mut out,
                    layout,
                    measured,
                    (source_start, source_end - source_start),
                    &ctx,
                );
            }
            if start <= keep && end > keep {
                render_view_ellipsis(&mut out, &format!("… {} more lines", total - keep));
            }
        }
        ViewState::TrimmedTail { keep } if total > usize::from(keep) => {
            let keep = usize::from(keep);
            if start == 0 && end > 0 {
                render_view_ellipsis(&mut out, &format!("… {} more lines above", total - keep));
            }
            let source_start = start.max(1);
            if source_start < end {
                render_expanded_block_range(
                    &mut out,
                    layout,
                    measured,
                    (
                        total
                            .saturating_sub(keep)
                            .saturating_add(source_start.saturating_sub(1)),
                        end - source_start,
                    ),
                    &ctx,
                );
            }
        }
        ViewState::Collapsed | ViewState::TrimmedHead { .. } | ViewState::TrimmedTail { .. } => {
            render_expanded_block_range(
                &mut out,
                layout,
                measured,
                (start, end.saturating_sub(start)),
                &ctx,
            );
        }
    }
    out.finish()
}

fn render_view_ellipsis(out: &mut LineBuilder, text: &str) {
    out.push_dim();
    out.push_hl(intern("Comment"));
    out.print(text);
    out.pop_style();
    out.pop_style();
    out.newline();
}

fn render_expanded_block(
    out: &mut LineBuilder,
    layout: &LayoutIr,
    width: usize,
    history: Option<&BlockHistory>,
    inline_options: &InlineOptions,
) -> u16 {
    let _perf = smelt_perf::perf::begin("render:layout");
    if let Some(history) = history {
        crate::content::display_renderers::render_layout_ir_into_with_history(
            out,
            layout,
            width as u16,
            history,
            inline_options,
        )
    } else {
        crate::content::display_renderers::render_layout_ir_into(out, layout, width as u16)
    }
}

fn render_expanded_block_range(
    out: &mut LineBuilder,
    layout: &LayoutIr,
    measured: &crate::content::display_renderers::MeasuredLayout,
    rows: (usize, usize),
    ctx: &RenderCtx<'_>,
) -> u16 {
    let _perf = smelt_perf::perf::begin("render:layout:range");
    let (row_start, row_count) = rows;
    crate::content::display_renderers::render_layout_ir_range_into_measured(
        out,
        layout,
        measured,
        ctx.width,
        row_start,
        row_count,
        ctx.history,
        &ctx.inline_options,
    )
}

fn apply_view_state(
    buf: &mut Buffer,
    theme: &Theme,
    width: u16,
    state: ViewState,
    outcome: Outcome,
) -> Outcome {
    let total = outcome.line_count;
    let target_total = state.measured_height(total as u64) as usize;
    let start = buf.line_count().saturating_sub(total);
    match state {
        ViewState::Expanded | ViewState::Peek => outcome,
        ViewState::Collapsed => {
            if state.elides_rows(total as u64) {
                let hidden = total - 1;
                buf.set_lines(start + 1, start + total, vec![]);
                let after_truncate_outcome = Outcome {
                    line_count: 1,
                    ..outcome
                };
                let with_ellipsis = append_ellipsis(
                    buf,
                    theme,
                    width,
                    &format!("… {hidden} more lines"),
                    after_truncate_outcome,
                );
                Outcome {
                    line_count: target_total,
                    ..with_ellipsis
                }
            } else {
                outcome
            }
        }
        ViewState::TrimmedHead { keep } => {
            let keep = keep as usize;
            if state.elides_rows(total as u64) {
                let hidden = total - keep;
                buf.set_lines(start + keep, start + total, vec![]);
                let after_truncate_outcome = Outcome {
                    line_count: keep,
                    ..outcome
                };
                let with_ellipsis = append_ellipsis(
                    buf,
                    theme,
                    width,
                    &format!("… {hidden} more lines"),
                    after_truncate_outcome,
                );
                Outcome {
                    line_count: target_total,
                    ..with_ellipsis
                }
            } else {
                outcome
            }
        }
        ViewState::TrimmedTail { keep } => {
            let keep = keep as usize;
            if state.elides_rows(total as u64) {
                let hidden = total - keep;
                buf.set_lines(start, start + (total - keep), vec![]);
                let mut kept_lines: Vec<String> = (0..keep)
                    .map(|i| buf.get_line(start + i).unwrap_or("").to_string())
                    .collect();
                let kept_decorations: Vec<_> = (0..keep)
                    .map(|i| buf.decoration_at(start + i).clone())
                    .collect();
                let kept_highlights: Vec<_> =
                    (0..keep).map(|i| buf.highlights_at(start + i)).collect();
                buf.set_lines(start, start + keep, vec![]);
                append_ellipsis(
                    buf,
                    theme,
                    width,
                    &format!("… {hidden} more lines above"),
                    Outcome {
                        line_count: 0,
                        ..outcome
                    },
                );
                let cur_len = buf.line_count();
                buf.set_lines(cur_len, cur_len, std::mem::take(&mut kept_lines));
                for (i, hl_list) in kept_highlights.into_iter().enumerate() {
                    let row = cur_len + i;
                    for span in hl_list {
                        buf.add_highlight_group_with_meta(
                            row,
                            span.col_start,
                            span.col_end,
                            span.hl,
                            span.meta,
                        );
                    }
                }
                for (i, dec) in kept_decorations.into_iter().enumerate() {
                    if dec != smelt_core::buffer::LineDecoration::default() {
                        buf.set_decoration(cur_len + i, dec);
                    }
                }
                Outcome {
                    line_count: target_total,
                    ..outcome
                }
            } else {
                outcome
            }
        }
    }
}

fn append_ellipsis(
    buf: &mut Buffer,
    theme: &Theme,
    width: u16,
    text: &str,
    outcome: Outcome,
) -> Outcome {
    let added = {
        let mut col = LineBuilder::new(buf, theme, width);
        col.push_dim();
        col.push_hl(intern("Comment"));
        col.print(text);
        col.pop_style();
        col.pop_style();
        col.newline();
        col.finish()
    };
    Outcome {
        line_count: outcome.line_count + added.line_count,
        was_wrapped: outcome.was_wrapped || added.was_wrapped,
        max_line_width: outcome.max_line_width.max(added.max_line_width),
        layout_width: outcome.layout_width,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::smelt_edit::{BufCreateOpts, BufId};
    use smelt_core::content::block_layout::RunsSpec;
    use smelt_core::content::transcript::Transcript;
    use smelt_core::transcript_model::LayoutKey;

    fn base_key(history: &BlockHistory, id: BlockId) -> NodeLayoutKey {
        history.resolve_key(
            id,
            LayoutKey {
                width: 80,
                view_state: ViewState::Expanded,
                content_hash: 0,
                sidecar_hash: 0,
            },
        )
    }

    fn rendered_buffer(block: &LayoutIr, width: u16) -> Buffer {
        let theme = Theme::default();
        let mut buf = Buffer::new(BufId(0), BufCreateOpts::default());
        render_block_into(
            &mut buf,
            block,
            RenderCtx {
                width,
                view_state: ViewState::Expanded,
                theme: &theme,
                history: None,
                inline_options: Default::default(),
            },
        );
        buf
    }

    fn rendered_rows(block: &LayoutIr, width: u16) -> u64 {
        rendered_buffer(block, width).line_count() as u64
    }

    #[test]
    fn projected_view_ranges_render_only_requested_retained_rows() {
        let content = TranscriptContent::from(
            (0..100_000)
                .map(|line| format!("line {line:06}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let layout = BlockLayout::Leaf(IrLeaf::Content(RetainedContentSpec {
            content: content.clone(),
            render: ContentRenderSpec::Text {
                hl_group: None,
                ansi: false,
            },
        }));
        let options = InlineOptions::default();
        let measured =
            crate::content::display_renderers::measure_layout_ir_plan(&layout, 80, &options);
        let retained_before = content.retained_bytes();
        let theme = Theme::default();
        let cases = [
            (
                ViewState::Collapsed,
                0,
                2,
                vec!["line 000000", "… 99999 more lines"],
            ),
            (
                ViewState::TrimmedHead { keep: 2 },
                1,
                2,
                vec!["line 000001", "… 99998 more lines"],
            ),
            (
                ViewState::TrimmedTail { keep: 2 },
                0,
                2,
                vec!["… 99998 more lines above", "line 099998"],
            ),
            (
                ViewState::TrimmedTail { keep: 2 },
                1,
                2,
                vec!["line 099998", "line 099999"],
            ),
        ];

        for (view_state, row_start, row_count, expected) in cases {
            let mut buffer = Buffer::new(BufId(0), BufCreateOpts::default());
            let outcome = render_block_range_into_mode(
                &mut buffer,
                &layout,
                &measured,
                RenderCtx {
                    width: 80,
                    view_state,
                    theme: &theme,
                    history: None,
                    inline_options: options.clone(),
                },
                row_start,
                row_count,
                true,
            );
            assert_eq!(outcome.line_count, expected.len());
            assert_eq!(buffer.lines(), expected);
        }
        assert_eq!(content.retained_bytes(), retained_before);
    }

    fn measured_rows(block: &LayoutIr, width: u16) -> u64 {
        measure_block(
            block,
            MeasureCtx {
                width,
                view_state: ViewState::Expanded,
                inline_options: Default::default(),
            },
        )
    }

    #[test]
    fn static_block_measurement_matches_rendered_rows() {
        let blocks = [
            Block::Text {
                content: "# Heading\n\nParagraph with **bold** text that wraps across several rows at narrow widths.\n\n```rust\nfn main() { println!(\"hello\"); }\n```\n\n| col | val |\n| --- | --- |\n| a | table cell that wraps |"
                    .into(),
            },
            Block::User {
                text: "Please inspect @crates/tui/src/content/display_layout.rs and this long line that wraps."
                    .into(),
                image_labels: vec![],
                command: false,
                sent_at_ms: None,
            },
            Block::ProcessStatus {
                text: "running a long process status that wraps on narrow terminals".into(),
                event: None,
            },
            Block::Thinking {
                title: None,
                summary_titles: Vec::new(),
                kind: protocol::ReasoningKind::Raw,
                content: "**Plan**\nThink through a long line that wraps in expanded thinking mode.".into(),
            },
            Block::Exec {
                command: "echo a very long shell command that wraps".into(),
                output: "output line that is also long enough to wrap in the transcript".into(),
            },
            Block::Compacted {
                summary: "A compacted **summary** with enough text to wrap.".into(),
            },
            Block::Mode {
                text: "plan".into(),
                icon: "◈ ".into(),
                hl_group: "SmeltAccent".into(),
            },
        ];

        for block in blocks {
            let display = compile_block(&block);
            assert_eq!(
                measured_rows(&display, 36),
                rendered_rows(&display, 36),
                "measurement mismatch for {block:?}"
            );
        }
    }

    #[test]
    fn layout_ir_measurement_matches_rendered_rows() {
        let display = BlockLayout::Vbox(vec![
            BlockLayout::Leaf(IrLeaf::Runs(RunsSpec {
                lines: protocol::StyledLines(vec![vec![protocol::StyledSpan {
                    text: "echo hello && echo world && echo done".into(),
                    syntax: Some("bash".into()),
                    ..Default::default()
                }]]),
                hl_group: Some("SmeltToolPending".into()),
                continuation_indent: 0,
                syntax_highlights: Default::default(),
            })),
            BlockLayout::Leaf(IrLeaf::Text(TextSpec {
                content: "output line that wraps at narrow widths".into(),
                hl_group: None,
                ansi: false,
            })),
        ]);

        assert_eq!(measured_rows(&display, 24), rendered_rows(&display, 24));
    }

    #[test]
    fn retained_content_diff_resolves_ids_without_lua_source_payloads() {
        let old: TranscriptContent = "fn answer() -> i32 { 41 }\n".into();
        let new: TranscriptContent = "fn answer() -> i32 { 42 }\n".into();
        let input = BlockLayout::Leaf(LuaLeaf::ContentDiff(
            smelt_core::content::block_layout::ContentDiffSpec {
                old_id: old.id(),
                new_id: new.id(),
                anchor_id: None,
                path: "src/lib.rs".into(),
                lang: None,
                full_file: true,
            },
        ));
        let sources = [old, new];
        let mut cache = CompileLayoutCache {
            content_sources: &sources,
            group_children: None,
            refresh_after_ms: None,
        };

        let compiled = compile_layout_ir_with_cache(&input, &mut cache).unwrap();
        let rendered = rendered_buffer(&compiled, 80).lines().join("\n");
        assert!(rendered.contains("41"), "{rendered}");
        assert!(rendered.contains("42"), "{rendered}");
        assert_eq!(measured_rows(&compiled, 80), rendered_rows(&compiled, 80));
    }

    #[test]
    fn retained_inline_syntax_matches_live_wrapped_render_and_reuses_compilation() {
        let spec = RunsSpec {
            lines: protocol::StyledLines(vec![vec![protocol::StyledSpan {
                text: "printf '%s\\n' alpha beta gamma delta".into(),
                syntax: Some("bash".into()),
                ..Default::default()
            }]]),
            hl_group: Some("SmeltToolPending".into()),
            continuation_indent: 3,
            syntax_highlights: Default::default(),
        };
        let fallback = BlockLayout::Leaf(IrLeaf::Runs(spec.clone()));
        let input = BlockLayout::Leaf(LuaLeaf::Runs(spec));
        let mut cache = CompileLayoutCache {
            content_sources: &[],
            group_children: None,
            refresh_after_ms: None,
        };
        let mut compiled = compile_layout_ir_with_cache(&input, &mut cache).unwrap();
        let mut compiled_again = compile_layout_ir_with_cache(&input, &mut cache).unwrap();
        let mut inline_syntax = InlineSyntaxCache::default();
        let theme = Theme::default();
        compile_layout_syntax(&mut compiled, &mut inline_syntax, &theme);
        compile_layout_syntax(&mut compiled_again, &mut inline_syntax, &theme);

        let width = 14;
        let fallback_buffer = rendered_buffer(&fallback, width);
        let compiled_buffer = rendered_buffer(&compiled, width);
        assert_eq!(compiled_buffer.lines(), fallback_buffer.lines());
        assert!(compiled_buffer.line_count() > 1);
        for row in 0..compiled_buffer.line_count() {
            assert_eq!(
                compiled_buffer.highlights_at(row),
                fallback_buffer.highlights_at(row)
            );
            assert_eq!(
                compiled_buffer.decoration_at(row),
                fallback_buffer.decoration_at(row)
            );
        }

        let BlockLayout::Leaf(IrLeaf::Runs(first)) = &compiled else {
            panic!("compiled runs leaf changed shape");
        };
        let BlockLayout::Leaf(IrLeaf::Runs(second)) = &compiled_again else {
            panic!("compiled runs leaf changed shape");
        };
        let first_spans = first
            .syntax_highlights
            .spans(0, 0)
            .expect("first retained syntax ranges");
        let second_spans = second
            .syntax_highlights
            .spans(0, 0)
            .expect("second retained syntax ranges");
        assert!(!first_spans.is_empty());
        assert!(std::ptr::eq(first_spans.as_ptr(), second_spans.as_ptr()));
    }

    #[test]
    fn layout_cache_lru_accounts_replacement_access_and_pins() {
        let mut model = LayoutCache::new();
        model.set_budget(usize::MAX);
        let first = RenderNodeId::Block(BlockId::new(1));
        let second = RenderNodeId::Block(BlockId::new(2));
        let key = DisplayCacheKey::new(1, 0, 0, None, 0);
        let small = compile_block(&Block::Text {
            content: "small layout".into(),
        });
        let weight = small.retained_bytes();

        model.insert_compiled_blocks(vec![
            (first, key, small.clone(), None),
            (second, key, small.clone(), None),
        ]);
        assert_eq!(model.retained_bytes, weight * 2);
        model.touch(first);
        model.set_budget(weight);
        assert!(model.blocks.contains_key(&first));
        assert!(!model.blocks.contains_key(&second));
        assert_eq!(model.retained_bytes, weight);

        let replacement = compile_block(&Block::Text {
            content: "replacement ".repeat(128).into(),
        });
        let replacement_weight = replacement.retained_bytes();
        model.set_budget(usize::MAX);
        model.insert_compiled_blocks(vec![(first, key, replacement, None)]);
        assert_eq!(model.blocks.len(), 1);
        assert_eq!(model.retained_bytes, replacement_weight);

        model.set_pinned_nodes([first]);
        model.set_budget(0);
        let pinned = model.memory_snapshot();
        assert_eq!(pinned.pinned_layout_bytes, replacement_weight);
        assert_eq!(pinned.oversize_debt_bytes, replacement_weight);
        assert!(model.blocks.contains_key(&first));
        model.set_pinned_nodes(std::iter::empty());
        assert!(model.blocks.is_empty());
        assert_eq!(model.retained_bytes(), 0);
    }

    #[test]
    fn layout_cache_caches_width_independent_blocks() {
        let mut transcript = Transcript::new();
        transcript.push(Block::CodeLine {
            content: "fn main() {}".into(),
            lang: "rust".into(),
        });
        let id = transcript.history.order[0];
        let key = base_key(&transcript.history, id);
        let mut narrow = key;
        narrow.width = 40;

        let lua = smelt_core::lua::runtime::LuaRuntime::new();
        let mut model = LayoutCache::new();
        assert_eq!(
            model.ensure_many(
                TranscriptRenderEnv::new(&lua),
                &transcript.history,
                &[id],
                &[key],
            ),
            1
        );
        assert_eq!(
            model.ensure_many(
                TranscriptRenderEnv::new(&lua),
                &transcript.history,
                &[id],
                &[narrow],
            ),
            0
        );
    }

    #[test]
    fn dirty_shifted_render_range_reuses_its_row_allocations() {
        let id = RenderNodeId::Block(BlockId::new(1));
        let key = LayoutKey {
            width: 80,
            view_state: ViewState::Expanded,
            content_hash: 1,
            sidecar_hash: 0,
        };
        let layout = BlockLayout::Leaf(IrLeaf::Text(TextSpec {
            content: format!("{}\n{}", "a".repeat(64), "b".repeat(32)),
            hl_group: None,
            ansi: false,
        }));
        let mut cache = LayoutCache::new();
        cache.set_budget(usize::MAX);
        cache.insert_compiled_blocks(vec![(
            id,
            DisplayCacheKey::from_node_key(key, 0, None),
            layout,
            None,
        )]);
        let theme = Theme::default();
        let render_ctx = || RenderCtx {
            width: key.width,
            view_state: key.view_state,
            theme: &theme,
            history: None,
            inline_options: Default::default(),
        };

        let first_ptr = cache
            .with_rendered_range(id, key, 0, None, render_ctx(), 0, 1, |buf, _| {
                buf.lines()[0].as_ptr()
            })
            .expect("first rendered range");
        cache.blocks.get_mut(&id).unwrap().rendered_ranges[0].dirty = true;
        let shifted_ptr = cache
            .with_rendered_range(id, key, 0, None, render_ctx(), 1, 1, |buf, _| {
                buf.lines()[0].as_ptr()
            })
            .expect("shifted rendered range");

        assert_eq!(shifted_ptr, first_ptr);
        let ranges = &cache.blocks.get(&id).unwrap().rendered_ranges;
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].key.row_start, 1);
        assert!(!ranges[0].dirty);
    }

    #[test]
    fn refresh_compilation_is_visually_transparent_and_uses_earliest_delay() {
        let child = BlockLayout::Leaf(LuaLeaf::Runs(RunsSpec {
            lines: protocol::StyledLines(vec![vec![protocol::StyledSpan {
                text: "select me".into(),
                selectable: true,
                ..Default::default()
            }]]),
            hl_group: Some("SmeltAccent".into()),
            continuation_indent: 2,
            syntax_highlights: Default::default(),
        }));
        let layout = BlockLayout::Refresh {
            child: Box::new(BlockLayout::Vbox(vec![
                BlockLayout::Refresh {
                    child: Box::new(child),
                    spec: smelt_core::content::block_layout::RefreshSpec { after_ms: 400 },
                },
                BlockLayout::Refresh {
                    child: Box::new(BlockLayout::Empty),
                    spec: smelt_core::content::block_layout::RefreshSpec { after_ms: 75 },
                },
            ])),
            spec: smelt_core::content::block_layout::RefreshSpec { after_ms: 250 },
        };
        let mut cache = CompileLayoutCache {
            content_sources: &[],
            group_children: None,
            refresh_after_ms: None,
        };

        let compiled = compile_layout_ir_with_cache(&layout, &mut cache).unwrap();

        assert_eq!(cache.refresh_after_ms, Some(75));
        assert_eq!(measured_rows(&compiled, 10), rendered_rows(&compiled, 10));
        let BlockLayout::Vbox(items) = compiled else {
            panic!("refresh wrappers must be stripped from display IR");
        };
        let BlockLayout::Leaf(IrLeaf::Runs(spec)) = &items[0] else {
            panic!("refresh child layout changed during compilation");
        };
        assert_eq!(spec.lines.0[0][0].text, "select me");
        assert!(spec.lines.0[0][0].selectable);
        assert_eq!(spec.continuation_indent, 2);
    }

    #[test]
    fn refresh_deadlines_are_replaced_removed_retained_and_evicted_with_nodes() {
        let start = std::time::Instant::now();
        let key = DisplayCacheKey::new(1, 0, 0, None, 0);
        let layout = compile_block(&Block::Text {
            content: "cached".into(),
        });
        let first = RenderNodeId::Block(BlockId::new(1));
        let second = RenderNodeId::Block(BlockId::new(2));
        let static_node = RenderNodeId::Block(BlockId::new(3));
        let mut model = LayoutCache::new();
        model.set_budget(usize::MAX);
        model.insert_compiled_blocks(vec![
            (
                first,
                key,
                layout.clone(),
                Some(start + std::time::Duration::from_millis(100)),
            ),
            (
                second,
                key,
                layout.clone(),
                Some(start + std::time::Duration::from_millis(200)),
            ),
            (static_node, key, layout.clone(), None),
        ]);

        assert_eq!(
            model.next_refresh_at(),
            Some(start + std::time::Duration::from_millis(100))
        );
        assert!(model
            .expire_due_refreshes(start + std::time::Duration::from_millis(99))
            .is_empty());
        assert_eq!(
            model.expire_due_refreshes(start + std::time::Duration::from_millis(100)),
            vec![first]
        );
        assert!(model.blocks.contains_key(&second));
        assert!(model.blocks.contains_key(&static_node));

        model.insert_compiled_blocks(vec![(second, key, layout.clone(), None)]);
        assert_eq!(model.next_refresh_at(), None);

        model.insert_compiled_blocks(vec![(
            first,
            key,
            layout.clone(),
            Some(start + std::time::Duration::from_secs(1)),
        )]);
        model.retain_nodes([static_node]);
        assert_eq!(model.next_refresh_at(), None);

        model.insert_compiled_blocks(vec![(
            first,
            key,
            layout,
            Some(start + std::time::Duration::from_secs(1)),
        )]);
        model.set_budget(0);
        assert!(model.blocks.is_empty());
        assert_eq!(model.next_refresh_at(), None);
    }

    #[test]
    fn due_refresh_recompiles_only_its_top_level_node() {
        let mut transcript = Transcript::new();
        transcript.push(Block::User {
            text: "dynamic".into(),
            image_labels: Vec::new(),
            command: false,
            sent_at_ms: None,
        });
        transcript.push(Block::User {
            text: "static".into(),
            image_labels: Vec::new(),
            command: false,
            sent_at_ms: None,
        });
        let ids = transcript.history.order.clone();
        let keys = ids
            .iter()
            .map(|id| base_key(&transcript.history, *id))
            .collect::<Vec<_>>();
        let lua = smelt_core::lua::runtime::LuaRuntime::new();
        lua.lua
            .load(
                r#"
                smelt.transcript.set_renderer(function(node)
                  local view = smelt.layout.text(node.text)
                  if node.text == "dynamic" then
                    return smelt.layout.refresh(view, { after_ms = 100 })
                  end
                  return view
                end)
                "#,
            )
            .exec()
            .unwrap();
        let renderer_generation = lua.transcript_renderer_generation();
        let renderer_cache_key = lua.transcript_renderer_cache_key();
        let start = std::time::Instant::now();
        let env = TranscriptRenderEnv {
            lua: &lua,
            renderer_generation,
            renderer_cache_key,
            now_ms: 1_700_000_000_000,
            refresh_now: start,
        };
        let mut model = LayoutCache::new();

        assert_eq!(
            model.ensure_many(env.clone(), &transcript.history, &ids, &keys),
            2
        );
        assert_eq!(
            model.next_refresh_at(),
            Some(start + std::time::Duration::from_millis(100))
        );
        assert_eq!(
            model.expire_due_refreshes(start + std::time::Duration::from_millis(100)),
            vec![RenderNodeId::Block(ids[0])]
        );
        assert_eq!(
            model.ensure_many(
                TranscriptRenderEnv {
                    refresh_now: start + std::time::Duration::from_millis(100),
                    ..env
                },
                &transcript.history,
                &ids,
                &keys,
            ),
            1
        );
        assert_eq!(model.blocks.len(), 2);
    }
}
