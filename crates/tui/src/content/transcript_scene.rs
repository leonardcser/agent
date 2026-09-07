use mlua::{Lua, Table, Value};
use smelt_core::lua::TranscriptGroupSpec;
use smelt_core::transcript_content::ContentId;
use smelt_core::transcript_model::{
    BlockHistory, BlockId, ContentChannel, LayoutKey, TranscriptGroupChildMetadata,
    TranscriptPatchOperation, ViewState,
};
use std::collections::{HashMap, HashSet};
use std::ops::Range;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub(crate) enum RenderNodeId {
    Block(BlockId),
    Group(u64),
}

impl RenderNodeId {
    pub(crate) fn as_block_id(self) -> Option<BlockId> {
        match self {
            Self::Block(id) => Some(id),
            Self::Group(_) => None,
        }
    }
}

pub(crate) type NodeLayoutKey = LayoutKey;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RenderNode {
    Block { id: BlockId, block_index: usize },
    Group(Box<RenderGroupNode>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RenderGroupChild {
    pub(crate) id: BlockId,
    pub(crate) content_revision: u64,
    pub(crate) presentation_revision: u64,
    pub(crate) metadata: TranscriptGroupChildMetadata,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RenderGroupNode {
    pub(crate) id: u64,
    pub(crate) name: String,
    pub(crate) bucket: String,
    pub(crate) child_range: Range<usize>,
    pub(crate) children: Vec<RenderGroupChild>,
    pub(crate) presentation_revision: u64,
    pub(crate) default_view_state: ViewState,
}

impl RenderGroupNode {
    pub(crate) fn child_ids(&self) -> impl Iterator<Item = BlockId> + '_ {
        self.children.iter().map(|child| child.id)
    }

    fn update_child_revision(
        &mut self,
        history: &BlockHistory,
        id: BlockId,
        presentation_changed: bool,
    ) -> bool {
        let Some(child) = self.children.iter_mut().find(|child| child.id == id) else {
            return false;
        };
        let key = history.resolve_key(
            id,
            LayoutKey {
                width: 0,
                view_state: ViewState::Expanded,
                content_hash: 0,
                sidecar_hash: 0,
            },
        );
        child.content_revision = key.content_hash;
        child.presentation_revision = key.sidecar_hash;
        if presentation_changed {
            child.metadata = history
                .group_child_metadata(id)
                .expect("group child remains in transcript history");
        }
        true
    }
}

impl RenderNode {
    pub(crate) fn id(&self) -> RenderNodeId {
        match self {
            Self::Block { id, .. } => RenderNodeId::Block(*id),
            Self::Group(group) => RenderNodeId::Group(group.id),
        }
    }
}

pub(crate) const TRANSCRIPT_APPEND_HEADROOM: usize = 64;

#[derive(Clone, Debug, Default)]
struct RenderNodeIndex {
    blocks: Vec<(BlockId, usize)>,
    groups: HashMap<u64, usize>,
}

impl RenderNodeIndex {
    fn for_nodes(nodes: &[RenderNode]) -> Self {
        let mut index = Self {
            blocks: Vec::with_capacity(nodes.len().saturating_add(TRANSCRIPT_APPEND_HEADROOM)),
            groups: HashMap::new(),
        };
        for (node_index, node) in nodes.iter().enumerate() {
            match node.id() {
                RenderNodeId::Block(id) => index.blocks.push((id, node_index)),
                RenderNodeId::Group(id) => {
                    index.groups.insert(id, node_index);
                }
            }
        }
        index.blocks.sort_unstable_by_key(|(id, _)| *id);
        index
    }

    fn insert(&mut self, id: RenderNodeId, node_index: usize) {
        match id {
            RenderNodeId::Block(id) => match self.blocks.binary_search_by_key(&id, |(id, _)| *id) {
                Ok(pos) => self.blocks[pos] = (id, node_index),
                Err(pos) => self.blocks.insert(pos, (id, node_index)),
            },
            RenderNodeId::Group(id) => {
                self.groups.insert(id, node_index);
            }
        }
    }

    fn get(&self, id: RenderNodeId) -> Option<usize> {
        match id {
            RenderNodeId::Block(id) => self
                .blocks
                .binary_search_by_key(&id, |(id, _)| *id)
                .ok()
                .map(|pos| self.blocks[pos].1),
            RenderNodeId::Group(id) => self.groups.get(&id).copied(),
        }
    }

    fn contains(&self, id: RenderNodeId) -> bool {
        self.get(id).is_some()
    }

    fn truncate_from(&mut self, index: usize) {
        self.blocks.retain(|(_, node_index)| *node_index < index);
        self.groups.retain(|_, node_index| *node_index < index);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TranscriptDefaultViewPolicy {
    block_kinds: HashMap<String, ViewState>,
    tools: HashMap<String, ViewState>,
    groups: HashMap<String, ViewState>,
    disabled_groups: HashSet<String>,
}

impl Default for TranscriptDefaultViewPolicy {
    fn default() -> Self {
        let mut policy = Self {
            block_kinds: HashMap::new(),
            tools: HashMap::new(),
            groups: HashMap::new(),
            disabled_groups: HashSet::new(),
        };
        policy
            .block_kinds
            .insert("thinking".to_string(), ViewState::Peek);
        policy
            .block_kinds
            .insert("compacted".to_string(), ViewState::Collapsed);
        policy
            .block_kinds
            .insert("compaction_preview".to_string(), ViewState::Peek);
        for tool in [
            "load_skill",
            "read_file",
            "grep",
            "glob",
            "web_fetch",
            "enter_worktree",
        ] {
            policy.tools.insert(tool.to_string(), ViewState::Collapsed);
        }
        policy
    }
}

impl TranscriptDefaultViewPolicy {
    pub(crate) fn from_lua(lua: &smelt_core::lua::runtime::LuaRuntime) -> Self {
        let mut policy = Self::default();
        if let Err(err) = policy.apply_lua_config(&lua.lua) {
            lua.record_error(format!("smelt.settings.transcript.view: {err}"));
        }
        policy
    }

    fn apply_lua_config(&mut self, lua: &Lua) -> mlua::Result<()> {
        let globals = lua.globals();
        let smelt = globals.get::<Option<Table>>("smelt")?;
        let Some(smelt) = smelt else { return Ok(()) };
        let settings = smelt.get::<Option<Table>>("settings")?;
        let Some(settings) = settings else {
            return Ok(());
        };
        let transcript = settings.get::<Option<Table>>("transcript")?;
        let Some(transcript) = transcript else {
            return Ok(());
        };
        let config = transcript.get::<Option<Table>>("view")?;
        let Some(config) = config else { return Ok(()) };

        apply_view_section(&config, "blocks", &mut self.block_kinds)?;
        apply_view_section(&config, "tools", &mut self.tools)?;
        apply_group_view_section(&config, &mut self.groups, &mut self.disabled_groups)?;
        Ok(())
    }

    pub(crate) fn group_enabled(&self, name: &str) -> bool {
        !self.disabled_groups.contains(name)
    }

    pub(crate) fn node_default_view_state(
        &self,
        history: &BlockHistory,
        node: &RenderNode,
    ) -> ViewState {
        match node {
            RenderNode::Block { id, .. } => history
                .tool_name(*id)
                .and_then(|name| self.tools.get(name).copied())
                .or_else(|| {
                    history
                        .block_kind(*id)
                        .and_then(|kind| self.block_kinds.get(kind).copied())
                })
                .unwrap_or(ViewState::Expanded),
            RenderNode::Group(group) => self
                .groups
                .get(&group.name)
                .copied()
                .unwrap_or(group.default_view_state),
        }
    }

    #[cfg(test)]
    fn block_default_view_state(&self, block: &smelt_core::transcript_model::Block) -> ViewState {
        tool_name(block)
            .and_then(|name| self.tools.get(name).copied())
            .or_else(|| self.block_kinds.get(block_kind(block)).copied())
            .unwrap_or(ViewState::Expanded)
    }
}

fn apply_view_section(
    config: &Table,
    name: &str,
    target: &mut HashMap<String, ViewState>,
) -> mlua::Result<()> {
    let section = config.get::<Option<Table>>(name)?;
    let Some(section) = section else {
        return Ok(());
    };
    for pair in section.pairs::<String, Value>() {
        let (key, value) = pair?;
        if let Some(view_state) = lua_view_state(value)? {
            target.insert(key, view_state);
        }
    }
    Ok(())
}

fn apply_group_view_section(
    config: &Table,
    groups: &mut HashMap<String, ViewState>,
    disabled: &mut HashSet<String>,
) -> mlua::Result<()> {
    let section = config.get::<Option<Table>>("groups")?;
    let Some(section) = section else {
        return Ok(());
    };
    for pair in section.pairs::<String, Value>() {
        let (key, value) = pair?;
        if matches!(value, Value::Boolean(false) | Value::Nil) {
            groups.remove(&key);
            disabled.insert(key);
            continue;
        }
        if let Some(view_state) = lua_view_state(value)? {
            disabled.remove(&key);
            groups.insert(key, view_state);
        }
    }
    Ok(())
}

fn lua_view_state(value: Value) -> mlua::Result<Option<ViewState>> {
    match value {
        Value::String(s) => parse_lua_view_state(s.to_str()?.as_ref()).map(Some),
        Value::Table(t) => match t.get::<Option<String>>("default_view")?.as_deref() {
            Some(value) => parse_lua_view_state(value).map(Some),
            None => Ok(None),
        },
        Value::Nil | Value::Boolean(false) => Ok(None),
        other => Err(mlua::Error::external(format!(
            "expected \"collapsed\", \"peek\", or \"expanded\", got {}",
            other.type_name()
        ))),
    }
}

fn parse_lua_view_state(value: &str) -> mlua::Result<ViewState> {
    match value {
        "collapsed" => Ok(ViewState::Collapsed),
        "peek" => Ok(ViewState::Peek),
        "expanded" => Ok(ViewState::Expanded),
        other => Err(mlua::Error::external(format!(
            "unknown view state `{other}`; expected \"collapsed\", \"peek\", or \"expanded\""
        ))),
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct TranscriptPresentationState {
    overrides: HashMap<RenderNodeId, ViewState>,
    generation: u64,
}

impl TranscriptPresentationState {
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn effective_view_state(
        &self,
        policy: &TranscriptDefaultViewPolicy,
        plan: &TranscriptScene,
        history: &BlockHistory,
        index: usize,
    ) -> Option<ViewState> {
        let id = plan.node_id(index)?;
        self.overrides
            .get(&id)
            .copied()
            .or_else(|| plan.node_default_view_state(policy, history, index))
    }

    pub(crate) fn set(
        &mut self,
        policy: &TranscriptDefaultViewPolicy,
        plan: &TranscriptScene,
        history: &BlockHistory,
        id: RenderNodeId,
        view_state: ViewState,
    ) -> bool {
        if !plan.contains_id(id) {
            return false;
        }
        let Some(default) = plan.node_default_view_state_by_id(policy, history, id) else {
            return false;
        };
        let changed = if view_state == default {
            self.overrides.remove(&id).is_some()
        } else if self.overrides.get(&id) == Some(&view_state) {
            false
        } else {
            self.overrides.insert(id, view_state);
            true
        };
        if changed {
            self.bump();
        }
        changed
    }

    pub(crate) fn toggle(
        &mut self,
        policy: &TranscriptDefaultViewPolicy,
        plan: &TranscriptScene,
        history: &BlockHistory,
        id: RenderNodeId,
    ) -> bool {
        let Some(index) = plan.index_of(id) else {
            return false;
        };
        let Some(current) = self.effective_view_state(policy, plan, history, index) else {
            return false;
        };
        let Some(default) = plan.node_default_view_state(policy, history, index) else {
            return false;
        };
        let next = if matches!(current, ViewState::Expanded) {
            if matches!(default, ViewState::Expanded) {
                ViewState::Collapsed
            } else {
                default
            }
        } else {
            ViewState::Expanded
        };
        self.set(policy, plan, history, id, next)
    }

    pub(crate) fn set_all(
        &mut self,
        policy: &TranscriptDefaultViewPolicy,
        plan: &TranscriptScene,
        history: &BlockHistory,
        view_state: ViewState,
    ) -> bool {
        let mut changed = false;
        for id in plan.ids().collect::<Vec<_>>() {
            changed |= self.set(policy, plan, history, id, view_state);
        }
        changed
    }

    pub(crate) fn prune(&mut self, ids: impl IntoIterator<Item = RenderNodeId>) {
        let live: HashSet<RenderNodeId> = ids.into_iter().collect();
        let before = self.overrides.len();
        self.overrides.retain(|id, _| live.contains(id));
        if self.overrides.len() != before {
            self.bump();
        }
    }

    fn bump(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }
}

#[derive(Clone, Copy, Debug)]
struct TranscriptScenePrefix {
    len: usize,
    revision: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ContentAppend {
    pub(crate) content_id: ContentId,
    pub(crate) channel: ContentChannel,
    pub(crate) byte_range: Range<usize>,
}

#[derive(Default)]
pub(crate) struct TranscriptSceneRefresh {
    pub(crate) changed_blocks: HashSet<BlockId>,
    pub(crate) appended_ranges: HashMap<BlockId, Vec<ContentAppend>>,
    pub(crate) structure_changed: bool,
    pub(crate) prune_required: bool,
}

#[derive(Default)]
struct ContentPatchRefresh {
    changed_blocks: HashSet<BlockId>,
    appended_ranges: HashMap<BlockId, Vec<ContentAppend>>,
}

#[derive(Clone, Debug)]
pub(crate) struct TranscriptScene {
    pub(crate) history_generation: u64,
    pub(crate) history_order_generation: u64,
    history_record_generation: u64,
    history_patch_revision: u64,
    history_order_len: usize,
    pub(crate) group_generation: u64,
    pub(crate) group_cache_key: Option<u64>,
    pub(crate) nodes: Vec<RenderNode>,
    node_index: RenderNodeIndex,
    grouped_block_index_by_id: HashMap<BlockId, usize>,
    pub(crate) revision: u64,
    append_prefix: Option<TranscriptScenePrefix>,
}

impl TranscriptScene {
    pub(crate) fn empty() -> Self {
        Self {
            history_generation: 0,
            history_order_generation: 0,
            history_record_generation: 0,
            history_patch_revision: 0,
            history_order_len: 0,
            group_generation: 0,
            group_cache_key: None,
            nodes: Vec::new(),
            node_index: RenderNodeIndex::default(),
            grouped_block_index_by_id: HashMap::new(),
            revision: 0,
            append_prefix: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_history_with_groups(
        history: &BlockHistory,
        groups: &[TranscriptGroupSpec],
        group_generation: u64,
        group_cache_key: Option<u64>,
    ) -> Self {
        let _perf = smelt_perf::perf::begin("transcript:transcript_scene");
        Self::build_for_history_with_groups(history, groups, group_generation, group_cache_key)
    }

    pub(crate) fn refresh_for_history_with_groups(
        &mut self,
        history: &BlockHistory,
        groups: &[TranscriptGroupSpec],
        group_generation: u64,
        group_cache_key: Option<u64>,
    ) -> TranscriptSceneRefresh {
        let _perf = smelt_perf::perf::begin("transcript:transcript_scene");
        if self.group_generation == group_generation && self.group_cache_key == group_cache_key {
            if let Some(content) = self.try_apply_content_patches(history) {
                smelt_perf::perf::record_value("transcript:transcript_scene:reused", 1);
                return TranscriptSceneRefresh {
                    changed_blocks: content.changed_blocks,
                    appended_ranges: content.appended_ranges,
                    structure_changed: false,
                    prune_required: false,
                };
            }
        }
        if let Some(prune_required) =
            self.try_refresh_incremental(history, groups, group_generation, group_cache_key)
        {
            self.history_patch_revision = history.patch_revision();
            smelt_perf::perf::record_value("transcript:transcript_scene:reused", 1);
            return TranscriptSceneRefresh {
                changed_blocks: HashSet::new(),
                appended_ranges: HashMap::new(),
                structure_changed: true,
                prune_required,
            };
        }
        smelt_perf::perf::record_value("transcript:transcript_scene:reused", 0);
        let next_revision = self.revision.wrapping_add(1);
        *self =
            Self::build_for_history_with_groups(history, groups, group_generation, group_cache_key);
        self.revision = next_revision;
        TranscriptSceneRefresh {
            changed_blocks: HashSet::new(),
            appended_ranges: HashMap::new(),
            structure_changed: true,
            prune_required: true,
        }
    }

    fn update_group_child_revision(
        &mut self,
        history: &BlockHistory,
        id: BlockId,
        patch_revision: u64,
        presentation_changed: bool,
    ) {
        let Some(index) = self.grouped_block_index_by_id.get(&id).copied() else {
            return;
        };
        let Some(RenderNode::Group(group)) = self.nodes.get_mut(index) else {
            return;
        };
        if !group.update_child_revision(history, id, presentation_changed) {
            return;
        }
        if presentation_changed {
            group.presentation_revision = smelt_core::utils::hash_serializable(&(
                group.presentation_revision,
                patch_revision,
                id,
            ));
        }
    }

    fn try_apply_content_patches(&mut self, history: &BlockHistory) -> Option<ContentPatchRefresh> {
        if self.history_patch_revision == history.patch_revision() {
            return (self.history_generation == history.generation())
                .then(ContentPatchRefresh::default);
        }
        let mut content = ContentPatchRefresh::default();
        let mut replaced_blocks = HashSet::new();
        let mut scene_changed = false;
        for patch in history.patches_since(self.history_patch_revision)? {
            for operation in &patch.operations {
                match operation {
                    TranscriptPatchOperation::Append {
                        id,
                        content_id,
                        channel,
                        byte_range,
                    } => {
                        scene_changed = true;
                        content.changed_blocks.insert(*id);
                        self.update_group_child_revision(
                            history,
                            *id,
                            patch.revision,
                            byte_range.start == 0,
                        );
                        if !replaced_blocks.contains(id) {
                            content
                                .appended_ranges
                                .entry(*id)
                                .or_default()
                                .push(ContentAppend {
                                    content_id: *content_id,
                                    channel: *channel,
                                    byte_range: byte_range.clone(),
                                });
                        }
                    }
                    TranscriptPatchOperation::Replace { id } => {
                        scene_changed = true;
                        content.changed_blocks.insert(*id);
                        content.appended_ranges.remove(id);
                        replaced_blocks.insert(*id);
                        self.update_group_child_revision(history, *id, patch.revision, true);
                    }
                    TranscriptPatchOperation::SetStatus { id }
                    | TranscriptPatchOperation::SetSideState { id }
                    | TranscriptPatchOperation::Commit { id } => {
                        scene_changed = true;
                        content.changed_blocks.insert(*id);
                        self.update_group_child_revision(history, *id, patch.revision, true);
                    }
                    TranscriptPatchOperation::SetAnimationState { .. } => {}
                    TranscriptPatchOperation::Insert { .. }
                    | TranscriptPatchOperation::Remove { .. }
                    | TranscriptPatchOperation::Reset => return None,
                }
            }
        }
        self.history_generation = history.generation();
        self.history_record_generation = history.record_dirty_generation();
        self.history_patch_revision = history.patch_revision();
        if scene_changed {
            self.append_prefix = None;
            self.revision = self.revision.wrapping_add(1);
        }
        Some(content)
    }

    fn build_for_history_with_groups(
        history: &BlockHistory,
        groups: &[TranscriptGroupSpec],
        group_generation: u64,
        group_cache_key: Option<u64>,
    ) -> Self {
        let nodes = {
            let _perf = smelt_perf::perf::begin("transcript:transcript_scene:build_nodes");
            build_nodes(history, groups)
        };
        let node_index = {
            let _perf = smelt_perf::perf::begin("transcript:transcript_scene:node_index");
            RenderNodeIndex::for_nodes(&nodes)
        };
        let mut grouped_block_index_by_id = HashMap::new();
        {
            let _perf = smelt_perf::perf::begin("transcript:transcript_scene:grouped_block_index");
            for (index, node) in nodes.iter().enumerate() {
                if let RenderNode::Group(group) = node {
                    for id in group.child_ids() {
                        grouped_block_index_by_id.insert(id, index);
                    }
                }
            }
        }
        Self {
            history_generation: history.generation(),
            history_order_generation: history.order_generation(),
            history_record_generation: history.record_dirty_generation(),
            history_patch_revision: history.patch_revision(),
            history_order_len: history.order.len(),
            group_generation,
            group_cache_key,
            nodes,
            node_index,
            grouped_block_index_by_id,
            revision: 1,
            append_prefix: None,
        }
    }

    fn try_refresh_incremental(
        &mut self,
        history: &BlockHistory,
        groups: &[TranscriptGroupSpec],
        group_generation: u64,
        group_cache_key: Option<u64>,
    ) -> Option<bool> {
        if groups.is_empty()
            && self
                .nodes
                .iter()
                .any(|node| matches!(node, RenderNode::Group(_)))
        {
            return None;
        }
        if !groups.is_empty()
            && (self.group_generation != group_generation
                || self.group_cache_key != group_cache_key)
        {
            return None;
        }
        let old_order_len = self.history_order_len;
        if old_order_len > history.order.len()
            || history.record_changed_from_since(self.history_record_generation)? < old_order_len
        {
            return None;
        }
        let rebuild_from_block = if groups.is_empty() {
            old_order_len
        } else {
            group_append_recompute_start(history, groups, old_order_len)
        };
        let truncate_from_node = if rebuild_from_block == old_order_len {
            self.nodes.len()
        } else {
            let id = history.order.get(rebuild_from_block).copied()?;
            self.index_for_block(id)?
        };
        let old_revision = self.revision;
        let old_node_len = self.nodes.len();
        let pure_append = truncate_from_node == old_node_len && rebuild_from_block == old_order_len;
        let prune_required = truncate_from_node < old_node_len;
        self.truncate_from(truncate_from_node);
        {
            let _perf = smelt_perf::perf::begin("transcript:transcript_scene:append_nodes");
            for node in build_nodes_from(history, groups, rebuild_from_block) {
                self.push_node(node);
            }
        }
        self.history_generation = history.generation();
        self.history_order_generation = history.order_generation();
        self.history_record_generation = history.record_dirty_generation();
        self.history_order_len = history.order.len();
        self.group_generation = group_generation;
        self.group_cache_key = group_cache_key;
        self.revision = self.revision.wrapping_add(1);
        self.append_prefix = pure_append.then_some(TranscriptScenePrefix {
            len: old_node_len,
            revision: old_revision,
        });
        Some(prune_required)
    }

    fn push_node(&mut self, node: RenderNode) {
        let index = self.nodes.len();
        self.node_index.insert(node.id(), index);
        if let RenderNode::Group(group) = &node {
            for id in group.child_ids() {
                self.grouped_block_index_by_id.insert(id, index);
            }
        }
        self.nodes.push(node);
    }

    fn truncate_from(&mut self, index: usize) {
        self.nodes.truncate(index);
        self.node_index.truncate_from(index);
        self.grouped_block_index_by_id
            .retain(|_, node_index| *node_index < index);
    }

    pub(crate) fn len(&self) -> usize {
        self.nodes.len()
    }

    pub(crate) fn append_prefix(&self) -> Option<(usize, u64)> {
        self.append_prefix
            .map(|prefix| (prefix.len, prefix.revision))
    }

    pub(crate) fn node(&self, index: usize) -> Option<&RenderNode> {
        self.nodes.get(index)
    }

    pub(crate) fn node_id(&self, index: usize) -> Option<RenderNodeId> {
        self.nodes.get(index).map(RenderNode::id)
    }

    pub(crate) fn contains_id(&self, id: RenderNodeId) -> bool {
        self.node_index.contains(id)
    }

    pub(crate) fn index_of(&self, id: RenderNodeId) -> Option<usize> {
        self.node_index.get(id)
    }

    pub(crate) fn index_for_block(&self, id: BlockId) -> Option<usize> {
        self.node_index
            .get(RenderNodeId::Block(id))
            .or_else(|| self.grouped_block_index_by_id.get(&id).copied())
    }

    pub(crate) fn push_block_ids_for_node(&self, index: usize, out: &mut Vec<BlockId>) -> bool {
        match self.nodes.get(index) {
            Some(RenderNode::Block { id, .. }) => {
                out.push(*id);
                true
            }
            Some(RenderNode::Group(group)) => {
                out.extend(group.child_ids());
                true
            }
            None => false,
        }
    }

    pub(crate) fn representative_block_id_for_node(&self, index: usize) -> Option<BlockId> {
        match self.nodes.get(index)? {
            RenderNode::Block { id, .. } => Some(*id),
            RenderNode::Group(group) => group.children.first().map(|child| child.id),
        }
    }

    pub(crate) fn node_default_view_state(
        &self,
        policy: &TranscriptDefaultViewPolicy,
        history: &BlockHistory,
        index: usize,
    ) -> Option<ViewState> {
        self.nodes
            .get(index)
            .map(|node| policy.node_default_view_state(history, node))
    }

    pub(crate) fn node_default_view_state_by_id(
        &self,
        policy: &TranscriptDefaultViewPolicy,
        history: &BlockHistory,
        id: RenderNodeId,
    ) -> Option<ViewState> {
        self.index_of(id)
            .and_then(|index| self.node_default_view_state(policy, history, index))
    }

    pub(crate) fn node_key(
        &self,
        policy: &TranscriptDefaultViewPolicy,
        history: &BlockHistory,
        presentation: &TranscriptPresentationState,
        index: usize,
        base_key: LayoutKey,
    ) -> Option<NodeLayoutKey> {
        let view_state = presentation.effective_view_state(policy, self, history, index)?;
        self.node_key_with_view_state(history, index, base_key, view_state)
    }

    pub(crate) fn node_key_with_view_state(
        &self,
        history: &BlockHistory,
        index: usize,
        base_key: LayoutKey,
        view_state: ViewState,
    ) -> Option<NodeLayoutKey> {
        match self.nodes.get(index)? {
            RenderNode::Block { id, .. } => Some(history.resolve_key(
                *id,
                LayoutKey {
                    width: base_key.width,
                    view_state,
                    content_hash: 0,
                    sidecar_hash: 0,
                },
            )),
            RenderNode::Group(group) => Some(LayoutKey {
                width: base_key.width,
                view_state,
                content_hash: group.id,
                sidecar_hash: smelt_core::utils::hash_serializable(&(
                    group.presentation_revision,
                    self.group_generation,
                    self.group_cache_key,
                    view_state,
                )),
            }),
        }
    }

    pub(crate) fn rendered_node_gap(
        &self,
        history: &BlockHistory,
        index: usize,
        rendered_rows: usize,
    ) -> u16 {
        if rendered_rows == 0 {
            return 0;
        }
        self.node_start_block_index(index)
            .map(|block_index| history.block_gap(block_index))
            .unwrap_or(0)
    }

    fn node_start_block_index(&self, index: usize) -> Option<usize> {
        match self.nodes.get(index)? {
            RenderNode::Block { block_index, .. } => Some(*block_index),
            RenderNode::Group(group) => Some(group.child_range.start),
        }
    }

    pub(crate) fn ids(&self) -> impl Iterator<Item = RenderNodeId> + '_ {
        self.nodes.iter().map(RenderNode::id)
    }
}

#[derive(serde::Serialize)]
struct GroupIdKey<'a> {
    name: &'a str,
    bucket: &'a str,
    first_child: BlockId,
}

fn render_group_child(history: &BlockHistory, id: BlockId) -> RenderGroupChild {
    let key = history.resolve_key(
        id,
        LayoutKey {
            width: 0,
            view_state: ViewState::Expanded,
            content_hash: 0,
            sidecar_hash: 0,
        },
    );
    RenderGroupChild {
        id,
        content_revision: key.content_hash,
        presentation_revision: key.sidecar_hash,
        metadata: history
            .group_child_metadata(id)
            .expect("group child exists in transcript history"),
    }
}

fn build_nodes(history: &BlockHistory, groups: &[TranscriptGroupSpec]) -> Vec<RenderNode> {
    build_nodes_from(history, groups, 0)
}

fn build_nodes_from(
    history: &BlockHistory,
    groups: &[TranscriptGroupSpec],
    start: usize,
) -> Vec<RenderNode> {
    if groups.is_empty() {
        let remaining = history.order.len().saturating_sub(start);
        let headroom = if start == 0 {
            TRANSCRIPT_APPEND_HEADROOM
        } else {
            0
        };
        let mut nodes = Vec::with_capacity(remaining.saturating_add(headroom));
        nodes.extend(
            history
                .order
                .iter()
                .copied()
                .enumerate()
                .skip(start)
                .map(|(block_index, id)| RenderNode::Block { id, block_index }),
        );
        return nodes;
    }

    let mut nodes = Vec::new();
    let mut index = start;
    while index < history.order.len() {
        let Some((spec, bucket, end)) = matching_group_run(history, groups, index) else {
            nodes.push(RenderNode::Block {
                id: history.order[index],
                block_index: index,
            });
            index += 1;
            continue;
        };

        let children = history.order[index..end]
            .iter()
            .copied()
            .map(|id| render_group_child(history, id))
            .collect::<Vec<_>>();
        let id = smelt_core::utils::hash_serializable(&GroupIdKey {
            name: &spec.name,
            bucket: &bucket,
            first_child: children[0].id,
        });
        let presentation_revision = smelt_core::utils::hash_serializable(&(
            id,
            children
                .iter()
                .map(|child| (child.id, child.presentation_revision))
                .collect::<Vec<_>>(),
        ));
        nodes.push(RenderNode::Group(Box::new(RenderGroupNode {
            id,
            name: spec.name.clone(),
            bucket,
            child_range: index..end,
            children,
            presentation_revision,
            default_view_state: view_state(spec.default_view.as_deref()),
        })));
        index = end;
    }
    nodes
}

fn group_append_recompute_start(
    history: &BlockHistory,
    groups: &[TranscriptGroupSpec],
    old_order_len: usize,
) -> usize {
    if old_order_len == 0 || old_order_len >= history.order.len() {
        return old_order_len;
    }
    let mut start = old_order_len;
    while start > 0 && can_share_group_run(history, groups, start - 1, start) {
        start -= 1;
    }
    start
}

fn can_share_group_run(
    history: &BlockHistory,
    groups: &[TranscriptGroupSpec],
    left: usize,
    right: usize,
) -> bool {
    groups.iter().any(|spec| {
        selector_matches(history, spec, left)
            && selector_matches(history, spec, right)
            && group_bucket(history, spec, left).unwrap_or_else(|| spec.name.clone())
                == group_bucket(history, spec, right).unwrap_or_else(|| spec.name.clone())
    })
}

fn matching_group_run<'a>(
    history: &BlockHistory,
    groups: &'a [TranscriptGroupSpec],
    start: usize,
) -> Option<(&'a TranscriptGroupSpec, String, usize)> {
    groups.iter().find_map(|spec| {
        if !selector_matches(history, spec, start) {
            return None;
        }
        let bucket = group_bucket(history, spec, start).unwrap_or_else(|| spec.name.clone());
        let mut end = start + 1;
        while end < history.order.len()
            && selector_matches(history, spec, end)
            && group_bucket(history, spec, end).unwrap_or_else(|| spec.name.clone()) == bucket
        {
            end += 1;
        }
        (end - start >= spec.min).then_some((spec, bucket, end))
    })
}

fn selector_matches(
    history: &BlockHistory,
    spec: &TranscriptGroupSpec,
    block_index: usize,
) -> bool {
    let Some(id) = history.order.get(block_index).copied() else {
        return false;
    };
    if spec
        .selector
        .kind
        .as_deref()
        .is_some_and(|kind| history.block_kind(id) != Some(kind))
    {
        return false;
    }
    if spec
        .selector
        .name
        .as_deref()
        .is_some_and(|name| history.tool_name(id) != Some(name))
    {
        return false;
    }
    if !spec.selector.names.is_empty()
        && history.tool_name(id).is_none_or(|name| {
            !spec
                .selector
                .names
                .iter()
                .any(|candidate| candidate == name)
        })
    {
        return false;
    }
    if let Some(terminal) = spec.selector.terminal {
        let terminal_state = history.tool_status(id).is_some_and(|status| {
            matches!(
                status,
                smelt_core::ToolStatus::Ok
                    | smelt_core::ToolStatus::Err
                    | smelt_core::ToolStatus::Denied
            )
        });
        if terminal_state != terminal {
            return false;
        }
    }
    for field in &spec.selector.fields {
        if block_field(history, block_index, &field.field).as_deref() != Some(field.value.as_str())
        {
            return false;
        }
    }
    true
}

fn group_bucket(
    history: &BlockHistory,
    spec: &TranscriptGroupSpec,
    block_index: usize,
) -> Option<String> {
    let bucket = spec.bucket.as_ref()?;
    let values: Vec<_> = bucket
        .fields
        .iter()
        .map(|field| block_field(history, block_index, field).unwrap_or_default())
        .collect();
    Some(values.join("\u{1f}"))
}

fn block_field(history: &BlockHistory, block_index: usize, field: &str) -> Option<String> {
    let id = history.order.get(block_index).copied()?;
    match field {
        "kind" => history.block_kind(id).map(str::to_string),
        "name" => history.tool_name(id).map(str::to_string),
        "status" if history.is_tool_draft(id) => Some("drafting".to_string()),
        "status" => history
            .tool_status(id)
            .map(|status| status.label().to_string()),
        "event" | "event_type" | "process_id" | "exit_code" => history.process_field(id, field),
        field => field
            .strip_prefix("args.")
            .and_then(|arg| history.arg_field(id, arg).map(json_bucket_value)),
    }
}

fn json_bucket_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
fn tool_name(block: &smelt_core::transcript_model::Block) -> Option<&str> {
    match block {
        smelt_core::transcript_model::Block::ToolDraft(draft) => Some(&draft.name),
        smelt_core::transcript_model::Block::ToolCall { name, .. } => Some(name),
        _ => None,
    }
}

#[cfg(test)]
fn block_kind(block: &smelt_core::transcript_model::Block) -> &'static str {
    block.kind()
}

fn view_state(value: Option<&str>) -> ViewState {
    match value {
        Some("collapsed") => ViewState::Collapsed,
        Some("peek") => ViewState::Peek,
        Some("trimmed_head") => ViewState::TrimmedHead { keep: 1 },
        Some("trimmed_tail") => ViewState::TrimmedTail { keep: 1 },
        _ => ViewState::Expanded,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smelt_core::content::stream_parser::{StreamParser, ToolStart};
    use smelt_core::transcript_model::{
        Block, BlockHistory, ToolOutput, ToolState, ToolStateMutation, ToolStatus,
        TranscriptBlockRecord,
    };

    #[test]
    fn default_view_policy_peeks_thinking_blocks() {
        let policy = TranscriptDefaultViewPolicy::default();
        assert_eq!(
            policy.block_default_view_state(&Block::Thinking {
                title: None,
                summary_titles: Vec::new(),
                kind: protocol::ReasoningKind::Raw,
                content: "preview".into(),
            }),
            ViewState::Peek
        );
        assert_eq!(
            policy.block_default_view_state(&Block::Compacted {
                summary: "checkpoint".into(),
            }),
            ViewState::Collapsed
        );
        assert_eq!(
            policy.block_default_view_state(&Block::CompactionPreview {
                summary: "streaming".into(),
            }),
            ViewState::Peek
        );
        assert_eq!(
            policy.block_default_view_state(&Block::Text {
                content: "shown".into()
            }),
            ViewState::Expanded
        );
        for tool in [
            "load_skill",
            "read_file",
            "grep",
            "glob",
            "web_fetch",
            "enter_worktree",
        ] {
            assert_eq!(
                policy.block_default_view_state(&Block::ToolCall {
                    call_id: format!("call-{tool}"),
                    name: tool.into(),
                    summary: protocol::StyledLines::default(),
                    args: HashMap::new().into(),
                }),
                ViewState::Collapsed
            );
        }
        for tool in ["write_file", "edit_file", "edit_notebook"] {
            assert_eq!(
                policy.block_default_view_state(&Block::ToolCall {
                    call_id: format!("call-{tool}"),
                    name: tool.into(),
                    summary: protocol::StyledLines::default(),
                    args: HashMap::new().into(),
                }),
                ViewState::Expanded
            );
        }
    }

    #[test]
    fn default_view_policy_specific_tool_overrides_block_kind() {
        let mut policy = TranscriptDefaultViewPolicy::default();
        policy
            .block_kinds
            .insert("tool".to_string(), ViewState::Collapsed);
        policy
            .tools
            .insert("read_file".to_string(), ViewState::Expanded);

        assert_eq!(
            policy.block_default_view_state(&Block::ToolCall {
                call_id: "call".into(),
                name: "read_file".into(),
                summary: protocol::StyledLines::default(),
                args: HashMap::new().into(),
            }),
            ViewState::Expanded
        );
    }

    #[test]
    fn process_status_typed_fields_match_selectors_and_buckets() {
        let mut transcript = smelt_core::content::transcript::Transcript::new();
        transcript.push(Block::ProcessStatus {
            text: "Background process 1 finished successfully.".into(),
            event: Some(protocol::ProcessStatusEvent::background_process_completed(
                "1",
                Some(0),
                protocol::JobTermination::Exited,
            )),
        });
        transcript.push(Block::ProcessStatus {
            text: "Background process 2 finished successfully.".into(),
            event: Some(protocol::ProcessStatusEvent::background_process_completed(
                "2",
                Some(0),
                protocol::JobTermination::Exited,
            )),
        });
        transcript.push(Block::ProcessStatus {
            text: "legacy process status".into(),
            event: None,
        });
        let spec = TranscriptGroupSpec {
            name: "background-processes".into(),
            cache_key: None,
            priority: 0,
            registration_order: 0,
            min: 2,
            default_view: None,
            selector: smelt_core::lua::TranscriptGroupSelector {
                kind: Some("process_status".into()),
                name: None,
                names: Vec::new(),
                terminal: None,
                fields: vec![smelt_core::lua::TranscriptGroupFieldMatch {
                    field: "event".into(),
                    value: "background_process_completed".into(),
                }],
            },
            bucket: Some(smelt_core::lua::TranscriptGroupBucket {
                fields: vec!["exit_code".into()],
            }),
        };

        let plan = TranscriptScene::for_history_with_groups(&transcript.history, &[spec], 1, None);

        assert_eq!(plan.nodes.len(), 2);
        assert!(matches!(
            &plan.nodes[0],
            RenderNode::Group(group) if group.name == "background-processes" && group.bucket == "0" && group.child_range == (0..2)
        ));
        assert!(matches!(
            plan.nodes[1],
            RenderNode::Block { block_index: 2, .. }
        ));
    }

    #[test]
    fn tool_name_sets_group_mixed_adjacent_tools() {
        let mut transcript = smelt_core::content::transcript::Transcript::new();
        for name in ["read_file", "grep", "outline", "edit_file"] {
            transcript.push(Block::ToolCall {
                call_id: format!("call-{name}"),
                name: name.into(),
                summary: protocol::StyledLines::default(),
                args: HashMap::new().into(),
            });
        }
        let spec = TranscriptGroupSpec {
            name: "explore".into(),
            cache_key: None,
            priority: 0,
            registration_order: 0,
            min: 2,
            default_view: Some("collapsed".into()),
            selector: smelt_core::lua::TranscriptGroupSelector {
                kind: Some("tool".into()),
                name: None,
                names: vec!["read_file".into(), "grep".into(), "outline".into()],
                terminal: None,
                fields: Vec::new(),
            },
            bucket: None,
        };

        let plan = TranscriptScene::for_history_with_groups(&transcript.history, &[spec], 1, None);

        assert!(matches!(
            plan.nodes.as_slice(),
            [RenderNode::Group(group), RenderNode::Block { block_index: 3, .. }]
                if group.name == "explore" && group.child_range == (0..3)
        ));
    }

    #[test]
    fn stored_tool_children_group_without_payload_hydration() {
        let record =
            |call_id: &str, path: &str, status: ToolStatus, is_error: bool| TranscriptBlockRecord {
                block: Block::ToolCall {
                    call_id: call_id.into(),
                    name: "read_file".into(),
                    summary: protocol::StyledLines::from_plain(format!("read {path}")),
                    args: HashMap::from([(
                        "file_path".to_string(),
                        serde_json::Value::String(path.to_string()),
                    )])
                    .into(),
                },
                content_hash: 0,
                origin: None,
                tool_state: Some(ToolState {
                    status,
                    elapsed: None,
                    called_at_ms: Some(1_234),
                    elapsed_active: false,
                    output: Some(Box::new(ToolOutput {
                        content: "first\nsecond".into(),
                        is_error,
                        metadata: None,
                        content_fields: Vec::new(),
                    })),
                    user_message: None,
                    preview_output: None,
                }),
                tool_render_revision: 7,
            };
        let history = BlockHistory::from_block_records(vec![
            record("call-1", "/tmp/first.rs", ToolStatus::Err, true),
            record("call-2", "/tmp/second.rs", ToolStatus::Ok, false),
        ]);
        let spec = TranscriptGroupSpec {
            name: "explore".into(),
            cache_key: None,
            priority: 0,
            registration_order: 0,
            min: 2,
            default_view: Some("collapsed".into()),
            selector: smelt_core::lua::TranscriptGroupSelector {
                kind: Some("tool".into()),
                name: Some("read_file".into()),
                names: Vec::new(),
                terminal: Some(true),
                fields: Vec::new(),
            },
            bucket: None,
        };

        assert!(history.order.iter().all(|id| history.block(*id).is_none()));
        let scene = TranscriptScene::for_history_with_groups(&history, &[spec], 1, None);
        let RenderNode::Group(group) = &scene.nodes[0] else {
            panic!("stored tools did not form a group");
        };

        assert_eq!(group.child_range, 0..2);
        assert_eq!(
            group.children[0].metadata.name.as_deref(),
            Some("read_file")
        );
        assert_eq!(group.children[0].metadata.status, Some("err"));
        assert_eq!(
            group.children[0].metadata.summary_text.as_deref(),
            Some("read /tmp/first.rs")
        );
        assert_eq!(group.children[0].metadata.called_at_ms, Some(1_234));
        assert_eq!(
            group.children[0]
                .metadata
                .args
                .as_ref()
                .and_then(|args| args.get("file_path")),
            Some(&serde_json::Value::String("/tmp/first.rs".into()))
        );
        assert_eq!(group.children[0].metadata.output.content_lines, Some(2));
        assert_eq!(group.children[0].metadata.output.is_error, Some(true));
        assert_eq!(group.children[1].metadata.status, Some("ok"));
    }

    #[test]
    fn lower_priority_group_can_match_when_higher_priority_run_is_below_min() {
        let mut transcript = smelt_core::content::transcript::Transcript::new();
        transcript.push(Block::Text {
            content: "first".into(),
        });
        transcript.push(Block::Text {
            content: "second".into(),
        });

        let high_min = TranscriptGroupSpec {
            name: "high-min".into(),
            cache_key: None,
            priority: 10,
            registration_order: 0,
            min: 3,
            default_view: None,
            selector: smelt_core::lua::TranscriptGroupSelector {
                kind: Some("assistant".into()),
                name: None,
                names: Vec::new(),
                terminal: None,
                fields: Vec::new(),
            },
            bucket: None,
        };
        let low_min = TranscriptGroupSpec {
            name: "low-min".into(),
            cache_key: None,
            priority: 0,
            registration_order: 1,
            min: 2,
            default_view: None,
            selector: smelt_core::lua::TranscriptGroupSelector {
                kind: Some("assistant".into()),
                name: None,
                names: Vec::new(),
                terminal: None,
                fields: Vec::new(),
            },
            bucket: None,
        };

        let plan = TranscriptScene::for_history_with_groups(
            &transcript.history,
            &[high_min, low_min],
            1,
            None,
        );

        assert!(matches!(
            plan.nodes.as_slice(),
            [RenderNode::Group(group)] if group.name == "low-min" && group.child_range == (0..2)
        ));
    }

    #[test]
    fn ungrouped_plan_extends_for_append_without_pruning() {
        let mut transcript = smelt_core::content::transcript::Transcript::new();
        transcript.push(Block::Text {
            content: "first".into(),
        });
        transcript.push(Block::Text {
            content: "second".into(),
        });
        transcript.history.clear_record_dirty();
        let mut plan = TranscriptScene::for_history_with_groups(&transcript.history, &[], 0, None);
        let initial_order_generation = plan.history_order_generation;

        transcript.push(Block::Text {
            content: "third".into(),
        });
        let appended = *transcript.history.order.last().unwrap();
        transcript.history.clear_record_dirty();
        let refresh = plan.refresh_for_history_with_groups(&transcript.history, &[], 0, None);

        assert!(!refresh.prune_required);
        assert_ne!(plan.history_order_generation, initial_order_generation);
        assert_eq!(plan.len(), 3);
        assert_eq!(plan.index_for_block(appended), Some(2));
        assert!(matches!(
            plan.nodes[2],
            RenderNode::Block {
                id,
                block_index: 2
            } if id == appended
        ));
    }

    #[test]
    fn content_patch_refresh_preserves_append_ranges() {
        let mut transcript = smelt_core::content::transcript::Transcript::new();
        transcript.push(Block::User {
            text: "continue".into(),
            image_labels: Vec::new(),
            command: false,
            sent_at_ms: None,
        });
        let mut parser = StreamParser::new();
        parser.append_streaming_text(&mut transcript.history, "first line\n");
        let id = *transcript.history.order.last().unwrap();
        let initial_len = match transcript.history.block(id).unwrap() {
            Block::Text { content } => content.len(),
            block => panic!("expected streaming text, got {block:?}"),
        };
        let mut plan = TranscriptScene::for_history_with_groups(&transcript.history, &[], 0, None);

        parser.append_streaming_text(&mut transcript.history, "second line\n");
        let final_len = match transcript.history.block(id).unwrap() {
            Block::Text { content } => content.len(),
            block => panic!("expected streaming text, got {block:?}"),
        };
        let refresh = plan.refresh_for_history_with_groups(&transcript.history, &[], 0, None);

        assert!(!refresh.structure_changed);
        assert_eq!(refresh.changed_blocks, HashSet::from([id]));
        let appends = refresh.appended_ranges.get(&id).expect("append ranges");
        assert_eq!(appends.len(), 1);
        assert_eq!(appends[0].channel, ContentChannel::Primary);
        assert_eq!(appends[0].byte_range, initial_len..final_len);
        assert_eq!(
            appends[0].content_id,
            transcript
                .history
                .content(id, ContentChannel::Primary)
                .expect("primary content")
                .id()
        );
    }

    #[test]
    fn animation_patch_advances_history_without_invalidating_scene() {
        let mut transcript = smelt_core::content::transcript::Transcript::new();
        let mut parser = StreamParser::new();
        parser.start_tool(
            &mut transcript.history,
            ToolStart {
                invocation_id: protocol::InvocationId::new(1),
                call_id: "call-1".into(),
                name: "bash".into(),
                summary: protocol::StyledLines::from_plain("echo hi"),
                args: HashMap::new(),
                preview_output: None,
                called_at_ms: 0,
            },
            std::time::Instant::now(),
        );
        let id = transcript.history.order[0];
        let mut scene = TranscriptScene::for_history_with_groups(&transcript.history, &[], 0, None);
        let scene_revision = scene.revision;
        let patch_revision = scene.history_patch_revision;

        assert!(transcript.history.apply_tool_state_mutation(
            id,
            ToolStateMutation::SyncElapsed(std::time::Duration::from_millis(250)),
        ));
        let refresh = scene.refresh_for_history_with_groups(&transcript.history, &[], 0, None);

        assert!(scene.history_patch_revision > patch_revision);
        assert_eq!(
            scene.history_patch_revision,
            transcript.history.patch_revision()
        );
        assert_eq!(scene.history_generation, transcript.history.generation());
        assert_eq!(scene.revision, scene_revision);
        assert!(refresh.changed_blocks.is_empty());
        assert!(refresh.appended_ranges.is_empty());
        assert!(!refresh.structure_changed);
        assert!(!refresh.prune_required);
    }

    #[test]
    fn ungrouped_plan_reuses_structure_for_content_update() {
        let mut transcript = smelt_core::content::transcript::Transcript::new();
        transcript.push(Block::Text {
            content: "first".into(),
        });
        let id = *transcript.history.order.last().unwrap();
        transcript.push(Block::Text {
            content: "second".into(),
        });
        let mut plan = TranscriptScene::for_history_with_groups(&transcript.history, &[], 0, None);
        let initial_order_generation = plan.history_order_generation;
        let initial_revision = plan.revision;

        transcript.history.rewrite(
            id,
            Block::Text {
                content: "changed".into(),
            },
        );
        let refresh = plan.refresh_for_history_with_groups(&transcript.history, &[], 0, None);

        assert!(!refresh.prune_required);
        assert_eq!(plan.history_order_generation, initial_order_generation);
        assert_eq!(plan.len(), 2);
        assert_ne!(plan.revision, initial_revision);
        assert_eq!(plan.index_for_block(id), Some(0));
    }

    #[test]
    fn grouped_child_payload_append_isolated_from_siblings_and_group_presentation() {
        let mut transcript = smelt_core::content::transcript::Transcript::new();
        let mut parser = StreamParser::new();
        let first_invocation = protocol::InvocationId::new(1);
        let second_invocation = protocol::InvocationId::new(2);
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
                    args: HashMap::new(),
                    preview_output: None,
                    called_at_ms: 0,
                },
                std::time::Instant::now(),
            );
            parser.append_active_output_line(
                &mut transcript.history,
                invocation_id,
                "seed\n".into(),
            );
        }
        let spec = TranscriptGroupSpec {
            name: "terminal-tools".into(),
            cache_key: None,
            priority: 0,
            registration_order: 0,
            min: 2,
            default_view: Some("expanded".into()),
            selector: smelt_core::lua::TranscriptGroupSelector {
                kind: Some("tool".into()),
                name: Some("bash".into()),
                names: Vec::new(),
                terminal: None,
                fields: Vec::new(),
            },
            bucket: None,
        };
        let mut scene = TranscriptScene::for_history_with_groups(
            &transcript.history,
            std::slice::from_ref(&spec),
            1,
            None,
        );
        let initial_group = match &scene.nodes[0] {
            RenderNode::Group(group) => group.as_ref().clone(),
            node => panic!("expected group, got {node:?}"),
        };

        parser.append_active_output_line(
            &mut transcript.history,
            first_invocation,
            "continuation\n".into(),
        );
        let refresh = scene.refresh_for_history_with_groups(
            &transcript.history,
            std::slice::from_ref(&spec),
            1,
            None,
        );
        let appended_group = match &scene.nodes[0] {
            RenderNode::Group(group) => group.as_ref(),
            node => panic!("expected group, got {node:?}"),
        };

        assert!(!refresh.structure_changed);
        assert_eq!(appended_group.id, initial_group.id);
        assert_eq!(
            appended_group.presentation_revision,
            initial_group.presentation_revision
        );
        assert_eq!(appended_group.child_range, initial_group.child_range);
        assert_eq!(
            appended_group.child_ids().collect::<Vec<_>>(),
            initial_group.child_ids().collect::<Vec<_>>()
        );
        assert_eq!(
            refresh.appended_ranges.keys().copied().collect::<Vec<_>>(),
            vec![initial_group.children[0].id]
        );
        assert_eq!(appended_group.children[0], initial_group.children[0]);
        assert_eq!(appended_group.children[1], initial_group.children[1]);

        let before_status = appended_group.clone();
        parser.set_active_status(
            &mut transcript.history,
            first_invocation,
            ToolStatus::Ok,
            std::time::Instant::now(),
        );
        let refresh = scene.refresh_for_history_with_groups(
            &transcript.history,
            std::slice::from_ref(&spec),
            1,
            None,
        );
        let finished_group = match &scene.nodes[0] {
            RenderNode::Group(group) => group.as_ref(),
            node => panic!("expected group, got {node:?}"),
        };

        assert!(!refresh.structure_changed);
        assert_eq!(finished_group.id, initial_group.id);
        assert_ne!(
            finished_group.presentation_revision,
            before_status.presentation_revision
        );
        assert_ne!(
            finished_group.children[0].metadata,
            before_status.children[0].metadata
        );
        assert_eq!(finished_group.children[1], before_status.children[1]);
    }

    #[test]
    fn grouped_plan_rebuilds_on_append() {
        let mut transcript = smelt_core::content::transcript::Transcript::new();
        transcript.push(Block::Text {
            content: "first".into(),
        });
        transcript.push(Block::Text {
            content: "second".into(),
        });
        let spec = TranscriptGroupSpec {
            name: "assistant-blocks".into(),
            cache_key: None,
            priority: 0,
            registration_order: 0,
            min: 2,
            default_view: None,
            selector: smelt_core::lua::TranscriptGroupSelector {
                kind: Some("assistant".into()),
                name: None,
                names: Vec::new(),
                terminal: None,
                fields: Vec::new(),
            },
            bucket: None,
        };
        let mut plan = TranscriptScene::for_history_with_groups(
            &transcript.history,
            std::slice::from_ref(&spec),
            1,
            None,
        );

        transcript.push(Block::Text {
            content: "third".into(),
        });
        let refresh = plan.refresh_for_history_with_groups(&transcript.history, &[spec], 1, None);

        assert!(refresh.prune_required);
        assert!(
            matches!(plan.nodes.as_slice(), [RenderNode::Group(group)] if group.child_range == (0..3))
        );
    }

    #[test]
    fn disabling_groups_rebuilds_grouped_plan_as_blocks() {
        let mut transcript = smelt_core::content::transcript::Transcript::new();
        transcript.push(Block::Text {
            content: "first".into(),
        });
        transcript.push(Block::Text {
            content: "second".into(),
        });
        let spec = TranscriptGroupSpec {
            name: "assistant-blocks".into(),
            cache_key: None,
            priority: 0,
            registration_order: 0,
            min: 2,
            default_view: None,
            selector: smelt_core::lua::TranscriptGroupSelector {
                kind: Some("assistant".into()),
                name: None,
                names: Vec::new(),
                terminal: None,
                fields: Vec::new(),
            },
            bucket: None,
        };
        let mut plan = TranscriptScene::for_history_with_groups(
            &transcript.history,
            std::slice::from_ref(&spec),
            1,
            None,
        );

        let refresh = plan.refresh_for_history_with_groups(&transcript.history, &[], 2, None);

        assert!(refresh.prune_required);
        assert!(matches!(
            plan.nodes.as_slice(),
            [
                RenderNode::Block { block_index: 0, .. },
                RenderNode::Block { block_index: 1, .. }
            ]
        ));
    }

    #[test]
    fn default_view_policy_group_setting_overrides_registration_default() {
        let mut policy = TranscriptDefaultViewPolicy::default();
        policy
            .groups
            .insert("tools".to_string(), ViewState::Expanded);
        let transcript = smelt_core::content::transcript::Transcript::new();
        let node = RenderNode::Group(Box::new(RenderGroupNode {
            id: 1,
            name: "tools".into(),
            bucket: "tools".into(),
            child_range: 0..0,
            children: Vec::new(),
            presentation_revision: 0,
            default_view_state: ViewState::Collapsed,
        }));

        assert_eq!(
            policy.node_default_view_state(&transcript.history, &node),
            ViewState::Expanded
        );
    }
}
