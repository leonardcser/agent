use std::sync::{Arc, Mutex};

use super::{
    agent::TurnLifecycle, session_document::TuiSessionDocument, transcript::ResumePreviewCache,
    CommittedTranscriptView, SessionAccess, SessionPersistence, SharedSessionState,
};

pub(crate) struct PersistenceReport {
    pub(crate) acknowledged_session_id: Option<String>,
    pub(crate) canonical_completions: Vec<crate::persist::CanonicalCommandCompletion>,
    pub(crate) failure: Option<PersistenceFailureReport>,
    pub(crate) audit_warning: Option<String>,
}

pub(crate) struct PersistenceFailureReport {
    pub(crate) session_id: String,
    pub(crate) target: PersistenceFailureTarget,
    pub(crate) cause: crate::persist::PersistenceCause,
}

#[derive(Clone, Copy)]
pub(crate) enum PersistenceFailureTarget {
    Command(crate::persist::CanonicalCommandId),
    AllCanonicalCommands,
    None,
}

pub(crate) enum CanonicalTurnSubmitOutcome {
    Durable(Box<crate::persist::SubmitTurnAcknowledgement>),
    PendingPersistence {
        command_id: crate::persist::CanonicalCommandId,
        generation: super::session_document::PersistenceGeneration,
    },
    PendingPreparation {
        command_id: crate::persist::CanonicalCommandId,
    },
}

pub(crate) enum CanonicalOperationEvent {
    Submit(CanonicalTurnSubmitOutcome),
    TransitionDurable(Box<crate::persist::TurnTransitionAcknowledgement>),
    Failed {
        command_id: crate::persist::CanonicalCommandId,
        cause: crate::persist::PersistenceCause,
    },
}

#[derive(Clone, Copy)]
enum CanonicalTransitionDispatch {
    Enqueue,
    Commit,
}

#[derive(Clone)]
enum CanonicalOperationPayload {
    Submit {
        metadata: super::session_document::RuntimeSessionMetadata,
        turn: smelt_store::NewTurn,
    },
    Transition {
        metadata: super::session_document::RuntimeSessionMetadata,
        turn_id: smelt_store::TurnId,
        state: smelt_store::TurnState,
        at_ms: u64,
        terminal_reason: Option<String>,
        dispatch: CanonicalTransitionDispatch,
    },
}

#[derive(Clone)]
struct CanonicalOperation {
    command_id: crate::persist::CanonicalCommandId,
    payload: CanonicalOperationPayload,
}

#[derive(Clone, Copy)]
enum InflightCanonicalCommand {
    Submit,
    Transition {
        generation: super::session_document::PersistenceGeneration,
        terminal: bool,
    },
}

enum CanonicalOperationProgress {
    DeferredPreparation,
    Submit(CanonicalTurnSubmitOutcome),
    TransitionDurable(Box<crate::persist::TurnTransitionAcknowledgement>),
    Queued,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SaveStatus {
    SkippedReadOnly,
    Blocked,
    DeferredHydration,
    Unchanged,
    DurableEphemeral,
    Submitted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PersistencePreparationBlock {
    desired: super::session_document::PersistenceGeneration,
    cause: crate::persist::PersistenceCause,
}

/// Owns canonical conversation state and the runtime machinery that keeps its
/// session, transcript, turn, and durable store projections aligned.
pub(crate) struct ConversationRuntime {
    session: smelt_core::session::Session,
    document: TuiSessionDocument,
    committed_transcript_view: Option<CommittedTranscriptView>,
    parser: smelt_core::content::stream_parser::StreamParser,
    /// Earliest canonical suffix whose transcript projection must be rebuilt
    /// after live stream-parser blocks finish.
    pending_transcript_history_rebuild_from: Option<usize>,
    resume_preview_cache: ResumePreviewCache,
    shared_session: Arc<Mutex<Option<SharedSessionState>>>,
    turn: TurnLifecycle,
    persistence: Option<crate::persist::SessionPersistence>,
    persistence_epoch: crate::persist::SessionEpoch,
    observed_persistence_status: Option<crate::persist::SessionPersistenceStatus>,
    persistence_preparation_block: Option<PersistencePreparationBlock>,
    next_canonical_command_id: u64,
    canonical_operations: std::collections::VecDeque<CanonicalOperation>,
    inflight_canonical_commands:
        std::collections::HashMap<crate::persist::CanonicalCommandId, InflightCanonicalCommand>,
    access: SessionAccess,
    sessions: smelt_core::session::SessionStorage,
    storage: SessionPersistence,
}

impl ConversationRuntime {
    pub(crate) fn new(
        session: smelt_core::session::Session,
        mut transcript: super::transcript::TranscriptDocument,
        resume_preview_cache: ResumePreviewCache,
        shared_session: Arc<Mutex<Option<SharedSessionState>>>,
        turn: TurnLifecycle,
        sessions: smelt_core::session::SessionStorage,
        storage: SessionPersistence,
    ) -> Self {
        transcript.set_store_address(storage.transcript_store_address(&sessions, &session));
        Self {
            session,
            document: TuiSessionDocument::new(transcript),
            committed_transcript_view: None,
            parser: smelt_core::content::stream_parser::StreamParser::new(),
            pending_transcript_history_rebuild_from: None,
            resume_preview_cache,
            shared_session,
            turn,
            persistence: None,
            persistence_epoch: crate::persist::SessionEpoch::ZERO,
            observed_persistence_status: None,
            persistence_preparation_block: None,
            next_canonical_command_id: 1,
            canonical_operations: std::collections::VecDeque::new(),
            inflight_canonical_commands: std::collections::HashMap::new(),
            access: SessionAccess::Owned,
            sessions,
            storage,
        }
    }

    pub(crate) fn session(&self) -> &smelt_core::session::Session {
        &self.session
    }

    pub(crate) fn install_startup_runtime(&mut self, runtime: &smelt_core::RuntimeState) {
        self.session.mode = Some(runtime.mode.as_str().to_string());
        self.session.reasoning_effort = Some(runtime.reasoning_effort.clone());
        self.session.model = runtime.active_model().map(|model| model.key.clone());
        self.session.fast_mode = Some(runtime.settings.fast_mode);
        self.turn.set_applied_mode(runtime.mode.clone());
        self.turn
            .set_applied_reasoning_effort(runtime.reasoning_effort.clone());
    }

    pub(crate) fn transcript(&self) -> &super::transcript::TranscriptDocument {
        &self.document.transcript
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn active_tool_block_id(
        &self,
        invocation_id: protocol::InvocationId,
    ) -> Option<smelt_core::transcript_model::BlockId> {
        self.parser.active_tool_block_id(invocation_id)
    }

    pub(crate) fn active_tool_output_content(
        &self,
        invocation_id: protocol::InvocationId,
    ) -> Option<smelt_core::transcript_content::TranscriptContent> {
        let block_id = self.parser.active_tool_block_id(invocation_id)?;
        self.document
            .transcript
            .history()
            .tool_state(block_id)?
            .output
            .as_ref()
            .map(|output| output.content.clone())
    }

    pub(crate) fn defer_last_reasoning_summary_hydration(
        &mut self,
    ) -> Option<smelt_core::transcript_model::BlockId> {
        self.document
            .transcript
            .defer_last_reasoning_summary_hydration()
    }

    pub(crate) fn deferred_engine_block_is_ready(
        &self,
        id: smelt_core::transcript_model::BlockId,
    ) -> bool {
        self.document.transcript.deferred_engine_block_is_ready(id)
    }

    pub(crate) fn deferred_engine_block_failed(
        &self,
        id: smelt_core::transcript_model::BlockId,
    ) -> bool {
        self.document.transcript.deferred_engine_block_failed(id)
    }

    pub(crate) fn release_deferred_engine_block(
        &mut self,
        id: smelt_core::transcript_model::BlockId,
    ) {
        self.document.transcript.release_deferred_engine_block(id);
    }

    pub(crate) fn release_all_deferred_engine_blocks(&mut self) {
        self.document
            .transcript
            .release_all_deferred_engine_blocks();
    }

    pub(crate) fn promote_last_reasoning_summary(
        &mut self,
    ) -> Option<super::transcript::ReasoningSummarySnapshot> {
        self.document.transcript.promote_last_reasoning_summary()
    }

    pub(crate) fn transcript_compaction_preview_id(
        &self,
    ) -> Option<smelt_core::transcript_model::BlockId> {
        self.document.transcript.compaction_preview_id()
    }

    pub(super) fn set_transcript_search_hydration_pin(
        &mut self,
        block_id: Option<smelt_core::transcript_model::BlockId>,
    ) {
        self.document.transcript.set_search_hydration_pin(block_id);
    }

    pub(crate) fn activate_transcript_search_record_window(
        &mut self,
        width: u16,
        block_idx: u64,
        viewport_rows: u16,
    ) -> bool {
        self.document
            .transcript
            .activate_record_window_for_block_idx(width, block_idx, viewport_rows)
    }

    pub(crate) fn transcript_search_block_at_row(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
        row: crate::smelt_edit::RowIndex,
        forward: bool,
    ) -> Option<smelt_core::transcript_model::BlockId> {
        self.document
            .transcript
            .block_id_at_or_before_row(lua, width, row, forward)
    }

    pub(crate) fn materialize_transcript_search_layout(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
        block_indices: Option<&[u64]>,
    ) -> crate::content::transcript_buf::TranscriptSearchLayout {
        match block_indices {
            Some(block_indices) => self
                .document
                .transcript
                .materialize_exact_loaded_search_layout_for_blocks(lua, width, block_indices),
            None => self
                .document
                .transcript
                .materialize_exact_loaded_search_layout(lua, width),
        }
    }

    pub(crate) fn transcript_search_total_rows(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
    ) -> crate::smelt_edit::RowIndex {
        self.document
            .transcript
            .approximate_scrollbar_total_rows(lua, width)
    }

    pub(crate) fn transcript_search_anchor_at_row(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
        row: crate::smelt_edit::RowIndex,
    ) -> super::transcript::TranscriptSearchAnchor {
        self.document
            .transcript
            .search_anchor_at_row(lua, width, row)
    }

    pub(crate) fn transcript_search_matches_for_row_range(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
        theme: &crate::smelt_edit::Theme,
        start: crate::smelt_edit::RowIndex,
        count: crate::smelt_edit::RowIndex,
        query: &str,
    ) -> Vec<super::transcript::TranscriptSearchMatch> {
        self.document
            .transcript
            .search_matches_for_row_range(lua, width, theme, start, count, query)
    }

    pub(crate) fn build_transcript_rows(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
        theme: &crate::smelt_edit::Theme,
    ) -> std::sync::Arc<Vec<String>> {
        self.document.transcript.build_rows(lua, width, theme)
    }

    pub(super) fn transcript_position_anchor(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
        position: crate::smelt_edit::DocPosition,
    ) -> super::transcript::TranscriptPositionAnchor {
        self.document
            .transcript
            .position_anchor(lua, width, position)
    }

    pub(super) fn resolve_transcript_position_anchor(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
        anchor: super::transcript::TranscriptPositionAnchor,
    ) -> crate::smelt_edit::DocPosition {
        self.document
            .transcript
            .resolve_position_anchor(lua, width, anchor)
    }

    pub(super) fn projected_transcript_search_match_row(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
        viewport_rows: u16,
        matched: super::transcript::TranscriptSearchMatch,
    ) -> Option<crate::smelt_edit::RowIndex> {
        self.document
            .transcript
            .projected_search_match_row(lua, width, viewport_rows, matched)
    }

    pub(super) fn transcript_search_range_anchor(
        &mut self,
        matched: super::transcript::TranscriptSearchMatch,
        query: String,
    ) -> super::transcript::TranscriptSearchRangeAnchor {
        self.document.transcript.search_range_anchor(matched, query)
    }

    pub(super) fn resolve_transcript_search_range_anchor(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
        theme: &crate::smelt_edit::Theme,
        anchor: super::transcript::TranscriptSearchRangeAnchor,
    ) -> super::transcript::TranscriptSearchMatch {
        self.document
            .transcript
            .resolve_search_range_anchor(lua, width, theme, anchor)
    }

    pub(crate) fn transcript_node_metadata_at_row(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
        row: crate::smelt_edit::RowIndex,
    ) -> Option<crate::content::transcript_buf::TranscriptNodeRow> {
        self.document
            .transcript
            .node_metadata_at_row(lua, width, row)
    }

    pub(crate) fn fold_transcript_node_at_row(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
        row: crate::smelt_edit::RowIndex,
        action: crate::content::transcript_buf::FoldAction,
        activation: crate::content::transcript_buf::FoldActivation,
    ) -> bool {
        self.document
            .transcript
            .fold_node_at_row(lua, width, row, action, activation)
    }

    pub(crate) fn fold_transcript_node(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
        id: crate::content::transcript_scene::RenderNodeId,
        action: crate::content::transcript_buf::FoldAction,
    ) -> bool {
        self.document.transcript.fold_node(lua, width, id, action)
    }

    pub(crate) fn fold_all_transcript_nodes(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
        action: crate::content::transcript_buf::FoldAction,
    ) -> bool {
        self.document.transcript.fold_all(lua, width, action)
    }

    pub(crate) fn fold_transcript_block_kind(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
        kind: &str,
        action: crate::content::transcript_buf::FoldAction,
    ) -> bool {
        self.document
            .transcript
            .fold_block_kind(lua, width, kind, action)
    }

    pub(crate) fn materialize_transcript_block_snapshots(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
    ) -> Vec<super::transcript::TranscriptBlockSnapshot> {
        self.document
            .transcript
            .materialize_block_snapshots(lua, width)
    }

    pub(crate) fn transcript_record_block_reveal_position(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
        record_index: usize,
        row_offset: crate::smelt_edit::RowIndex,
        viewport_rows: u16,
    ) -> Option<super::transcript::TranscriptBlockRevealPosition> {
        self.document.transcript.record_block_reveal_position(
            lua,
            width,
            record_index,
            row_offset,
            viewport_rows,
        )
    }

    pub(crate) fn drain_finished_transcript_blocks(
        &mut self,
    ) -> Vec<smelt_core::transcript_model::BlockId> {
        self.document.transcript.drain_finished_blocks()
    }

    pub(crate) fn with_transcript_display_document<R>(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
        theme: &crate::smelt_edit::Theme,
        f: impl FnOnce(&mut dyn crate::smelt_edit::DisplayDocument) -> R,
    ) -> R {
        let mut document = super::transcript::TranscriptDisplayDocument::new(
            &mut self.document.transcript,
            lua,
            width,
            theme,
        );
        f(&mut document)
    }

    pub(super) fn trace_retained_transcript_frame(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
        viewport_rows: u16,
    ) {
        self.document
            .transcript
            .trace_retained_view_frame(lua, width, viewport_rows);
    }

    pub(super) fn reanchor_retained_transcript_viewport(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
        viewport_rows: u16,
        scroll_top: crate::smelt_edit::RowIndex,
    ) {
        self.document
            .transcript
            .reanchor_retained_viewport(lua, width, viewport_rows, scroll_top);
    }

    pub(super) fn prepare_transcript_window(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        theme: &std::sync::Arc<crate::smelt_edit::Theme>,
        ui: &mut crate::smelt_edit::Ui,
        request: crate::smelt_edit::MaterializeRequest,
        render_now: std::time::Instant,
        search_projection: super::render_loop::TranscriptSearchProjection<'_>,
    ) {
        super::render_loop::prepare_transcript_window(
            &mut self.document.transcript,
            lua,
            theme,
            ui,
            request,
            render_now,
            search_projection,
        );
    }

    pub(super) fn transcript_hydration_context_id(&self) -> u64 {
        self.document.transcript.hydration_context_id()
    }

    pub(super) fn transcript_hydration_is_pending(&self) -> bool {
        self.document.transcript.hydration_is_pending()
    }

    pub(super) fn pending_transcript_scrollbar_display_scroll_top(
        &self,
    ) -> Option<crate::smelt_edit::RowIndex> {
        self.document
            .transcript
            .pending_scrollbar_display_scroll_top()
    }

    pub(super) fn take_pending_transcript_hydration_request(
        &mut self,
    ) -> Option<super::transcript_hydration::TranscriptHydrationRequest> {
        self.document.transcript.take_pending_hydration_request()
    }

    pub(super) fn install_transcript_hydration_result(
        &mut self,
        result: super::transcript_hydration::TranscriptHydrationWorkerResult,
    ) -> bool {
        self.document.transcript.install_hydration_result(result)
    }

    pub(super) fn loaded_transcript_block_ids(&self) -> Vec<smelt_core::transcript_model::BlockId> {
        self.document.transcript.history().order.clone()
    }

    pub(super) fn pin_deferred_transcript_operation(
        &mut self,
        ids: &[smelt_core::transcript_model::BlockId],
    ) {
        self.document.transcript.pin_deferred_operation_blocks(ids);
    }

    pub(super) fn deferred_transcript_operation_is_ready(
        &self,
        ids: &[smelt_core::transcript_model::BlockId],
    ) -> bool {
        self.document
            .transcript
            .deferred_operation_blocks_are_ready(ids)
    }

    pub(super) fn deferred_transcript_operation_failed(
        &self,
        ids: &[smelt_core::transcript_model::BlockId],
    ) -> bool {
        self.document
            .transcript
            .deferred_operation_blocks_failed(ids)
    }

    pub(super) fn request_deferred_transcript_operation(
        &mut self,
        ids: &[smelt_core::transcript_model::BlockId],
    ) {
        self.document
            .transcript
            .request_deferred_operation_blocks(ids);
    }

    pub(super) fn unpin_transcript_operation(
        &mut self,
        ids: &[smelt_core::transcript_model::BlockId],
    ) {
        self.document.transcript.unpin_operation_blocks(ids);
    }

    pub(crate) fn transcript_trace_anchor_at_row(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
        row: crate::smelt_edit::RowIndex,
    ) -> super::transcript_scroll_trace::TranscriptTraceAnchor {
        self.document
            .transcript
            .trace_anchor_at_row(lua, width, row)
    }

    pub(crate) fn record_transcript_scroll_trace_event(
        &mut self,
        kind: impl Into<String>,
        data: serde_json::Value,
    ) {
        self.document
            .transcript
            .record_scroll_trace_event(kind, data);
    }

    pub(crate) fn prime_transcript_local_scroll_base(
        &mut self,
        lua: &smelt_core::lua::runtime::LuaRuntime,
        width: u16,
        viewport_rows: u16,
        scroll_top: crate::smelt_edit::RowIndex,
    ) {
        self.document
            .transcript
            .prime_local_scroll_base(lua, width, viewport_rows, scroll_top);
    }

    #[cfg(test)]
    pub(crate) fn transcript_extent_store_read_count_for_harness(&self) -> usize {
        self.document
            .transcript
            .extent_store_read_count_for_harness()
    }

    #[cfg(test)]
    pub(crate) fn reset_transcript_projection_counters_for_harness(&mut self) {
        self.document
            .transcript
            .reset_projection_counters_for_harness();
    }

    #[cfg(test)]
    pub(crate) fn transcript_projection_counters_for_harness(
        &self,
    ) -> crate::content::transcript_buf::TranscriptProjectionCounters {
        self.document.transcript.projection_counters_for_harness()
    }

    pub(crate) fn set_pending_transcript_projection(
        &mut self,
        intent: super::transcript_scroll_trace::TranscriptScrollIntent,
        restore: super::transcript::TranscriptProjectionRestore,
        local_scroll_top: Option<crate::smelt_edit::RowIndex>,
        hint: Option<super::transcript::TranscriptProjectionHint>,
    ) -> super::transcript_scroll_trace::TranscriptScrollIntent {
        self.document.transcript.set_pending_projection_with_hint(
            intent,
            restore,
            local_scroll_top,
            hint,
        )
    }

    pub(crate) fn defer_transcript_projection_until_hydrated(&mut self) {
        self.document.transcript.defer_projection_until_hydrated();
    }

    pub(crate) fn set_next_transcript_scroll_trace_input(
        &mut self,
        input: super::transcript_scroll_trace::TranscriptScrollTraceRenderInput,
    ) {
        self.document.transcript.set_next_scroll_trace_input(input);
    }

    pub(crate) fn clear_pending_transcript_local_scroll(&mut self) {
        self.document.transcript.clear_pending_local_scroll_top();
    }

    pub(crate) fn drain_transcript_compaction_slice(&mut self) -> bool {
        self.document.transcript.drain_compaction_slice()
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn replace_transcript_document_for_harness(
        &mut self,
        transcript: super::transcript::TranscriptDocument,
    ) {
        self.document.transcript = transcript;
    }

    #[cfg(test)]
    pub(crate) fn replace_loaded_transcript_for_harness(
        &mut self,
        transcript: super::transcript::LoadedTranscript,
    ) {
        self.document
            .transcript
            .replace_loaded_transcript(transcript);
    }

    #[cfg(feature = "transcript-bench")]
    pub(crate) fn ensure_transcript_blocks_hydrated_for_harness(
        &mut self,
        ids: &[smelt_core::transcript_model::BlockId],
    ) -> bool {
        self.document.transcript.ensure_hydrated_ids(ids)
    }

    #[cfg(test)]
    pub(crate) fn require_transcript_record_resave_from_for_harness(&mut self, index: usize) {
        self.document
            .transcript
            .history_mut()
            .require_record_resave_from(index);
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn set_transcript_session_dir_for_harness(
        &mut self,
        session_dir: std::path::PathBuf,
    ) {
        let session_id = session_dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let sessions_root = session_dir
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| self.sessions.sessions_dir());
        let lineage_id = smelt_store::LineageSessionReader::try_open_existing(
            &sessions_root,
            session_id.clone(),
        )
        .ok()
        .flatten()
        .map(|reader| reader.lineage_id().to_string())
        .unwrap_or_else(|| session_id.clone());
        self.document.transcript.set_store_address(Some(
            smelt_core::session::SessionStoreAddress::new(sessions_root, session_id, lineage_id),
        ));
    }

    #[cfg(test)]
    pub(crate) fn set_transcript_memory_budget_for_harness(
        &mut self,
        budget: super::transcript::TranscriptMemoryBudget,
    ) {
        self.document.transcript.set_memory_budget(budget);
    }

    #[cfg(test)]
    pub(crate) fn drain_transcript_compaction_for_harness(&mut self) {
        while self.document.transcript.drain_compaction_slice() {}
    }

    #[cfg(test)]
    pub(crate) fn transcript_tail_state_for_harness(
        &self,
    ) -> Option<(usize, smelt_core::transcript_model::BlockId, bool)> {
        let history = self.document.transcript.history();
        let id = history.last_block_id()?;
        Some((history.len(), id, history.is_materialized(id)))
    }

    #[cfg(test)]
    pub(crate) fn transcript_memory_snapshot_for_harness(
        &self,
    ) -> super::transcript::TranscriptMemorySnapshot {
        self.document.transcript.memory_snapshot()
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn set_transcript_scroll_trace_for_harness(&mut self, enabled: bool) {
        self.document.transcript.set_scroll_trace_enabled(enabled);
    }

    #[cfg(test)]
    pub(crate) fn set_transcript_scroll_trace_timings_for_harness(&mut self, enabled: bool) {
        self.document
            .transcript
            .set_scroll_trace_timings_enabled(enabled);
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn take_transcript_scroll_trace_frames_for_harness(
        &mut self,
    ) -> Vec<super::transcript_scroll_trace::TranscriptScrollTraceFrame> {
        self.document.transcript.take_scroll_trace_frames()
    }

    #[cfg(test)]
    pub(crate) fn take_transcript_interaction_trace_events_for_harness(
        &mut self,
    ) -> Vec<super::transcript_scroll_trace::TranscriptInteractionTraceEvent> {
        self.document
            .transcript
            .take_scroll_trace_interaction_events()
    }

    pub(crate) fn with_pinned_transcript_blocks<R>(
        &mut self,
        ids: &[smelt_core::transcript_model::BlockId],
        f: impl FnOnce(&smelt_core::transcript_model::BlockHistory) -> R,
    ) -> Option<R> {
        if !self.document.transcript.pin_operation_blocks(ids) {
            return None;
        }
        let result = f(self.document.transcript.history());
        self.document.transcript.unpin_operation_blocks(ids);
        Some(result)
    }

    pub(crate) fn live_session(&self) -> Option<&smelt_core::session_runtime::LiveSession> {
        self.document.live_session.as_ref()
    }

    pub(crate) fn has_live_session(&self) -> bool {
        self.document.live_session.is_some()
    }

    pub(crate) fn acknowledged_head(&self) -> smelt_store::StoreHead {
        self.document.acknowledged_head()
    }

    pub(crate) fn persistence_generation(&self) -> super::session_document::PersistenceGeneration {
        self.document.generation()
    }

    pub(crate) fn transcript_records_persisted(&self) -> bool {
        self.document.records_persisted()
    }

    pub(crate) fn has_document_work(&self) -> bool {
        self.document.has_session_work()
    }

    pub(crate) fn automatic_save_blocked(&self) -> bool {
        self.persistence_preparation_block.is_some()
            || self.persistence.as_ref().is_some_and(|persistence| {
                matches!(
                    persistence.status().state,
                    crate::persist::PersistenceState::Blocked { .. }
                        | crate::persist::PersistenceState::OwnershipLost { .. }
                        | crate::persist::PersistenceState::Stopped { cause: Some(_), .. }
                )
            })
    }

    pub(crate) fn prepare_fork_save(
        &mut self,
        forked: &mut smelt_core::session::Session,
        metadata: super::session_document::RuntimeSessionMetadata,
    ) -> Result<Option<super::session_document::PreparedSessionBatch>, String> {
        self.document
            .prepare_fork_save(forked, metadata, &self.session.history)
            .map_err(|error| error.to_string())
    }

    pub(crate) fn refresh_live_session_header(&mut self) {
        let Some(live) = self.document.live_session.as_mut() else {
            return;
        };
        if let Some((header, _)) = self.sessions.load_store_header_for_id(live.id()) {
            live.replace_header(header);
        }
    }

    pub(crate) fn clear_live_session(&mut self) {
        self.document.live_session = None;
    }

    pub(crate) fn install_loaded_full_session(
        &mut self,
        transcript: super::transcript::LoadedTranscript,
        store_head: Option<smelt_store::StoreHead>,
    ) {
        self.document
            .install_loaded_full_session(transcript, store_head);
    }

    pub(crate) fn install_loaded_store_session(
        &mut self,
        transcript: super::transcript::LoadedTranscript,
        live_session: smelt_core::session_runtime::LiveSession,
        store_head: smelt_store::StoreHead,
    ) {
        self.document
            .install_loaded_store_session(transcript, live_session, store_head);
    }

    pub(crate) fn install_materialized_transcript(
        &mut self,
        transcript: super::transcript::LoadedTranscript,
        records_persisted: bool,
    ) {
        self.document
            .transcript
            .replace_loaded_transcript(transcript);
        self.document
            .install_materialized_session(records_persisted);
    }

    pub(crate) fn install_rewind_prefix(
        &mut self,
        transcript: super::transcript::LoadedTranscript,
        record_count: usize,
    ) {
        self.document
            .install_rewind_prefix(transcript, record_count);
        self.clear_stream_tools();
    }

    fn apply_history_mutation(
        &mut self,
        mutation: super::session_document::HistoryMutation,
    ) -> super::session_document::DocumentChange {
        self.document.apply_history(&mut self.session, mutation)
    }

    fn apply_transcript_mutation(
        &mut self,
        mutation: super::session_document::TranscriptMutation,
    ) -> super::session_document::DocumentChange {
        self.document.apply_transcript(mutation)
    }

    fn apply_stream_mutation(
        &mut self,
        mutation: super::session_document::StreamMutation,
    ) -> super::session_document::DocumentChange {
        self.document.apply_stream(&mut self.parser, mutation)
    }

    fn apply_usage_mutation(
        &mut self,
        mutation: super::session_document::UsageMutation,
    ) -> super::session_document::DocumentChange {
        self.document.apply_usage(&mut self.session, mutation)
    }

    fn apply_metadata_mutation(
        &mut self,
        mutation: super::session_document::MetadataMutation,
    ) -> super::session_document::DocumentChange {
        self.document.apply_metadata(&mut self.session, mutation)
    }

    fn apply_turn_state_mutation(
        &mut self,
        mutation: super::session_document::TurnStateMutation,
    ) -> super::session_document::DocumentChange {
        self.document.apply_turn_state(&mut self.session, mutation)
    }

    fn plan_history_append(
        &self,
        append: &protocol::HistoryAppend,
    ) -> Result<protocol::HistoryAppendPlan, String> {
        if let Some(live) = self.document.live_session.as_ref() {
            live.plan_history_append(append)
        } else {
            protocol::plan_history_append(self.session.history.as_slice(), append)
                .map_err(|never| match never {})
        }
    }

    pub(crate) fn apply_history_append(
        &mut self,
        append: &protocol::HistoryAppend,
        identity: smelt_core::session::ContextTokenIdentity,
    ) -> Result<protocol::HistoryAppendResult, String> {
        use protocol::HistoryAppendPlan;

        let plan = self.plan_history_append(append)?;
        let result = match plan {
            HistoryAppendPlan::Unchanged => protocol::HistoryAppendResult::Unchanged,
            HistoryAppendPlan::Push => {
                self.append_history_item(append.item.clone());
                protocol::HistoryAppendResult::Pushed
            }
            HistoryAppendPlan::ReplaceLast | HistoryAppendPlan::RemoveLast => {
                let index = self
                    .history_len()
                    .checked_sub(1)
                    .ok_or_else(|| "tail history mutation requires an existing item".to_string())?;
                self.truncate_history(index, identity);
                if plan == HistoryAppendPlan::ReplaceLast {
                    self.append_history_item(append.item.clone());
                }
                plan.result()
            }
        };
        Ok(result)
    }

    pub(crate) fn append_history_item(&mut self, item: protocol::HistoryItem) -> usize {
        self.apply_history_mutation(super::session_document::HistoryMutation::AppendItem { item })
            .history_idx
            .expect("history append returns its canonical index")
    }

    pub(crate) fn commit_request_history_item(
        &mut self,
        item: protocol::HistoryItem,
        block: Option<smelt_core::transcript_model::Block>,
        first_user_message: Option<String>,
    ) -> usize {
        self.apply_history_mutation(
            super::session_document::HistoryMutation::CommitRequestItem {
                item,
                block: block.map(Box::new),
                first_user_message,
            },
        )
        .history_idx
        .expect("request history commit returns its canonical index")
    }

    pub(crate) fn truncate_history(
        &mut self,
        index: usize,
        identity: smelt_core::session::ContextTokenIdentity,
    ) -> Option<protocol::TurnMeta> {
        self.apply_history_mutation(super::session_document::HistoryMutation::TruncateFrom {
            index,
            identity,
        })
        .turn_meta
    }

    pub(crate) fn rewind_history(
        &mut self,
        index: usize,
        keep_checkpoint_at_boundary: bool,
        identity: smelt_core::session::ContextTokenIdentity,
    ) -> Option<protocol::TurnMeta> {
        self.apply_history_mutation(super::session_document::HistoryMutation::RewindTo {
            index,
            keep_checkpoint_at_boundary,
            identity,
        })
        .turn_meta
    }

    pub(crate) fn prune_rewindable_state(
        &mut self,
        history_len: usize,
        identity: smelt_core::session::ContextTokenIdentity,
    ) -> Option<protocol::TurnMeta> {
        self.apply_turn_state_mutation(
            super::session_document::TurnStateMutation::PruneRewindable {
                history_len,
                identity,
            },
        )
        .turn_meta
    }

    pub(crate) fn update_runtime_metadata(
        &mut self,
        metadata: super::session_document::RuntimeSessionMetadata,
    ) {
        self.apply_metadata_mutation(super::session_document::MetadataMutation::UpdateRuntime {
            updated_at_ms: metadata.updated_at_ms,
            mode: metadata.mode,
            reasoning_effort: metadata.reasoning_effort,
            model: metadata.model,
            fast_mode: metadata.fast_mode,
        });
    }

    pub(crate) fn set_title(&mut self, title: String, slug: String, snapshot_history_len: usize) {
        self.apply_metadata_mutation(super::session_document::MetadataMutation::SetTitle {
            title,
            slug,
            snapshot_history_len,
        });
    }

    pub(crate) fn restore_metadata_after_rewind(&mut self, history_len: usize) {
        self.apply_metadata_mutation(
            super::session_document::MetadataMutation::RestoreAfterRewind { history_len },
        );
    }

    pub(crate) fn set_fast_mode(&mut self, enabled: bool) {
        self.apply_metadata_mutation(super::session_document::MetadataMutation::SetFastMode {
            enabled,
        });
    }

    pub(crate) fn set_cwd(&mut self, cwd: String) {
        self.apply_metadata_mutation(super::session_document::MetadataMutation::SetCwd { cwd });
    }

    pub(crate) fn record_context_tokens(
        &mut self,
        usage: protocol::TokenUsage,
        identity: smelt_core::session::ContextTokenIdentity,
    ) -> bool {
        let history_len = self.history_len();
        self.apply_usage_mutation(super::session_document::UsageMutation::RecordTokens {
            usage,
            history_len,
            identity,
        })
        .context_tokens_updated
    }

    pub(crate) fn accumulate_usage(&mut self, usage: protocol::TokenUsage, cost_usd: f64) {
        self.apply_usage_mutation(super::session_document::UsageMutation::Accumulate {
            usage,
            cost_usd,
        });
    }

    pub(crate) fn finish_turn_state(
        &mut self,
        history_len: usize,
        meta: protocol::TurnMeta,
        update_context_token_history_len: bool,
    ) -> bool {
        self.apply_turn_state_mutation(super::session_document::TurnStateMutation::Finish {
            history_len,
            meta,
            update_context_token_history_len,
        })
        .applied
    }

    pub(crate) fn set_checkpoint(
        &mut self,
        checkpoint: Option<smelt_core::session::ContextCheckpoint>,
    ) {
        self.apply_turn_state_mutation(super::session_document::TurnStateMutation::SetCheckpoint {
            checkpoint,
        });
    }

    pub(crate) fn install_checkpoint(
        &mut self,
        kind: String,
        summary: String,
        first_live_message_index: usize,
        tokens_before: Option<u32>,
    ) -> bool {
        self.apply_turn_state_mutation(
            super::session_document::TurnStateMutation::InstallCheckpoint {
                kind,
                summary,
                first_live_message_index,
                tokens_before,
            },
        )
        .applied
    }

    pub(crate) fn install_checkpoint_at_history_index(
        &mut self,
        kind: String,
        summary: String,
        first_live_index: usize,
        tokens_before: Option<u32>,
        history_len: usize,
    ) -> bool {
        self.apply_turn_state_mutation(
            super::session_document::TurnStateMutation::InstallCheckpointAtHistoryIndex {
                kind,
                summary,
                first_live_index,
                tokens_before,
                history_len,
            },
        )
        .applied
    }

    pub(crate) fn set_checkpoint_tokens_after_estimate(
        &mut self,
        tokens: u32,
        history_len: usize,
    ) -> bool {
        self.apply_turn_state_mutation(
            super::session_document::TurnStateMutation::SetCheckpointTokensAfterEstimate {
                tokens,
                history_len,
            },
        )
        .applied
    }

    pub(crate) fn append_block(&mut self, block: smelt_core::transcript_model::Block) -> bool {
        self.apply_transcript_mutation(super::session_document::TranscriptMutation::AppendBlock {
            block,
        })
        .applied
    }

    pub(crate) fn insert_checkpoint_marker(
        &mut self,
        block_index: usize,
        history_index: usize,
        block: smelt_core::transcript_model::Block,
    ) {
        self.apply_transcript_mutation(
            super::session_document::TranscriptMutation::InsertCheckpointMarker {
                block_index,
                history_index,
                block,
            },
        );
    }

    pub(crate) fn remove_unoriginated_block(&mut self, block_index: usize) -> bool {
        self.apply_transcript_mutation(
            super::session_document::TranscriptMutation::RemoveUnoriginatedBlockAt { block_index },
        )
        .applied
    }

    pub(crate) fn pending_transcript_history_rebuild_from(&self) -> Option<usize> {
        self.pending_transcript_history_rebuild_from
    }

    pub(crate) fn defer_transcript_history_rebuild_from(&mut self, first_index: usize) {
        self.pending_transcript_history_rebuild_from = Some(
            self.pending_transcript_history_rebuild_from
                .map_or(first_index, |pending| pending.min(first_index)),
        );
    }

    pub(crate) fn clear_pending_transcript_history_rebuild(&mut self) {
        self.pending_transcript_history_rebuild_from = None;
    }

    pub(crate) fn has_live_transcript_blocks(&self) -> bool {
        self.parser.has_live_transcript_blocks()
    }

    pub(crate) fn replace_transcript_from_history(
        &mut self,
        transcript: smelt_core::content::transcript::Transcript,
    ) {
        self.apply_transcript_mutation(
            super::session_document::TranscriptMutation::ReplaceFromHistory {
                transcript: Box::new(transcript),
            },
        );
        self.clear_pending_transcript_history_rebuild();
    }

    pub(crate) fn truncate_transcript(&mut self, block_index: usize) {
        self.apply_transcript_mutation(super::session_document::TranscriptMutation::TruncateTo {
            block_index,
        });
    }

    pub(crate) fn clear_transcript(&mut self) {
        self.clear_pending_history_appends();
        self.clear_pending_transcript_history_rebuild();
        self.apply_transcript_mutation(super::session_document::TranscriptMutation::Clear);
        self.parser.clear();
    }

    pub(crate) fn update_compaction_preview(
        &mut self,
        summary: String,
    ) -> Option<smelt_core::transcript_model::BlockId> {
        self.apply_transcript_mutation(
            super::session_document::TranscriptMutation::UpdateCompactionPreview { summary },
        )
        .block_id
    }

    pub(crate) fn clear_compaction_preview(&mut self) {
        self.apply_transcript_mutation(
            super::session_document::TranscriptMutation::ClearCompactionPreview,
        );
    }

    pub(crate) fn rewrite_block(
        &mut self,
        id: smelt_core::transcript_model::BlockId,
        block: smelt_core::transcript_model::Block,
    ) -> bool {
        self.apply_transcript_mutation(super::session_document::TranscriptMutation::RewriteBlock {
            id,
            block,
        })
        .applied
    }

    pub(crate) fn append_streaming_thinking(&mut self, delta: String) {
        self.apply_stream_mutation(super::session_document::StreamMutation::AppendThinking {
            delta,
        });
    }

    pub(crate) fn flush_streaming_thinking(&mut self) {
        self.apply_stream_mutation(super::session_document::StreamMutation::FlushThinking);
    }

    pub(crate) fn append_streaming_text(&mut self, delta: String) {
        self.apply_stream_mutation(super::session_document::StreamMutation::AppendText { delta });
    }

    pub(crate) fn flush_streaming_text(&mut self) {
        self.apply_stream_mutation(super::session_document::StreamMutation::FlushText);
    }

    pub(crate) fn sync_active_tool_elapsed(&mut self, now: std::time::Instant) {
        self.apply_stream_mutation(
            super::session_document::StreamMutation::SyncActiveToolElapsed { now },
        );
    }

    pub(crate) fn start_tool(
        &mut self,
        start: smelt_core::content::stream_parser::ToolStart,
        now: std::time::Instant,
    ) {
        self.apply_stream_mutation(super::session_document::StreamMutation::StartTool {
            start,
            now,
        });
    }

    pub(super) fn append_tool_output_line(
        &mut self,
        invocation_id: protocol::InvocationId,
        line: String,
    ) -> Option<super::transcript_work::PendingToolOutputAppend> {
        let block_id = self.parser.active_tool_block_id(invocation_id)?;
        if line.len() <= super::transcript_work::STREAM_CONTENT_SLICE_BYTES {
            self.apply_stream_mutation(super::session_document::StreamMutation::AppendToolOutput {
                invocation_id,
                line,
            });
            return None;
        }

        let source = Arc::new(line);
        let end = super::transcript_work::stream_content_slice_end(
            &source,
            0,
            super::transcript_work::STREAM_CONTENT_SLICE_BYTES,
        );
        self.append_tool_output_slice(
            block_id,
            smelt_core::transcript_content::SharedContentSlice::new(Arc::clone(&source), 0..end),
            true,
        );
        (end < source.len()).then(|| {
            super::transcript_work::PendingToolOutputAppend::new(
                block_id,
                invocation_id,
                source,
                end,
                false,
            )
        })
    }

    pub(super) fn advance_tool_output_append(
        &mut self,
        mut pending: super::transcript_work::PendingToolOutputAppend,
    ) -> Option<super::transcript_work::PendingToolOutputAppend> {
        let start = pending.offset;
        let end = super::transcript_work::stream_content_slice_end(
            &pending.source,
            start,
            super::transcript_work::STREAM_CONTENT_SLICE_BYTES,
        );
        let chunk = smelt_core::transcript_content::SharedContentSlice::new(
            Arc::clone(&pending.source),
            start..end,
        );
        let line_start = std::mem::take(&mut pending.line_start);
        pending.offset = end;
        self.append_tool_output_slice(pending.block_id, chunk, line_start);
        (end < pending.source.len()).then_some(pending)
    }

    fn append_tool_output_slice(
        &mut self,
        block_id: smelt_core::transcript_model::BlockId,
        chunk: smelt_core::transcript_content::SharedContentSlice,
        line_start: bool,
    ) {
        self.apply_stream_mutation(
            super::session_document::StreamMutation::AppendToolOutputSlice {
                block_id,
                chunk,
                line_start,
            },
        );
    }

    pub(crate) fn set_tool_status(
        &mut self,
        invocation_id: protocol::InvocationId,
        status: smelt_core::transcript_model::ToolStatus,
        now: std::time::Instant,
    ) {
        self.apply_stream_mutation(super::session_document::StreamMutation::SetToolStatus {
            invocation_id,
            status,
            now,
        });
    }

    pub(crate) fn set_tool_user_message(
        &mut self,
        invocation_id: protocol::InvocationId,
        message: String,
    ) {
        self.apply_stream_mutation(
            super::session_document::StreamMutation::SetToolUserMessage {
                invocation_id,
                message,
            },
        );
    }

    pub(crate) fn finish_tool(
        &mut self,
        invocation_id: protocol::InvocationId,
        status: smelt_core::transcript_model::ToolStatus,
        output: Option<smelt_core::transcript_model::ToolOutputRef>,
        engine_elapsed: Option<std::time::Duration>,
        now: std::time::Instant,
    ) {
        self.apply_stream_mutation(super::session_document::StreamMutation::FinishTool {
            invocation_id,
            status,
            output,
            engine_elapsed,
            now,
        });
    }

    pub(crate) fn finalize_tools(&mut self) {
        self.apply_stream_mutation(super::session_document::StreamMutation::FinalizeTools);
    }

    pub(crate) fn promote_tool_draft(
        &mut self,
        stream_id: Option<String>,
        start: smelt_core::content::stream_parser::ToolStart,
        now: std::time::Instant,
    ) -> bool {
        self.apply_stream_mutation(super::session_document::StreamMutation::PromoteToolDraft {
            stream_id,
            start,
            now,
        })
        .applied
    }

    pub(crate) fn clear_stream_tool_drafts(&mut self) {
        self.apply_stream_mutation(super::session_document::StreamMutation::ClearToolDrafts);
    }

    pub(crate) fn start_tool_draft(
        &mut self,
        stream_id: String,
        call_id: Option<String>,
        name: Option<String>,
    ) -> Option<smelt_core::transcript_model::BlockId> {
        self.apply_stream_mutation(super::session_document::StreamMutation::StartToolDraft {
            stream_id,
            call_id,
            name,
        })
        .block_id
    }

    pub(crate) fn append_tool_draft(
        &mut self,
        stream_id: String,
        call_id: Option<String>,
        name: Option<String>,
        delta: String,
    ) -> Option<(smelt_core::transcript_model::BlockId, bool)> {
        let change =
            self.apply_stream_mutation(super::session_document::StreamMutation::AppendToolDraft {
                stream_id,
                call_id,
                name,
                delta,
            });
        change
            .block_id
            .map(|block_id| (block_id, change.presentation_changed))
    }

    pub(crate) fn finish_tool_draft(
        &mut self,
        stream_id: String,
        call_id: String,
        name: String,
        arguments: String,
    ) -> Option<smelt_core::transcript_model::BlockId> {
        self.apply_stream_mutation(super::session_document::StreamMutation::FinishToolDraft {
            stream_id,
            call_id,
            name,
            arguments,
        })
        .block_id
    }

    pub(crate) fn set_tool_draft_summary(
        &mut self,
        block_id: smelt_core::transcript_model::BlockId,
        summary: protocol::StyledLines,
    ) {
        self.apply_stream_mutation(
            super::session_document::StreamMutation::SetToolDraftSummary { block_id, summary },
        );
    }

    pub(crate) fn tool_draft_preview(
        &self,
        block_id: smelt_core::transcript_model::BlockId,
    ) -> Option<(
        String,
        std::collections::HashMap<String, serde_json::Value>,
        bool,
    )> {
        let smelt_core::transcript_model::Block::ToolDraft(draft) =
            self.document.transcript.history().block(block_id)?
        else {
            return None;
        };
        Some((
            draft.name.clone(),
            draft.arguments.preview().clone(),
            draft.finished,
        ))
    }

    pub(crate) fn start_exec(&mut self, command: String) {
        self.apply_stream_mutation(super::session_document::StreamMutation::StartExec { command });
    }

    pub(crate) fn append_exec_output(&mut self, chunk: String) {
        self.apply_stream_mutation(super::session_document::StreamMutation::AppendExecOutput {
            chunk,
        });
    }

    pub(crate) fn finish_exec(&mut self, final_output: Option<String>) {
        self.apply_stream_mutation(super::session_document::StreamMutation::FinishExec {
            final_output,
        });
    }

    pub(crate) fn begin_turn(&mut self) {
        self.turn.clear_context_tokens_updated();
        self.parser.begin_turn();
    }

    pub(crate) fn is_active(&self) -> bool {
        self.turn.is_active()
    }

    pub(crate) fn active(&self) -> Option<&super::TurnState> {
        self.turn.active()
    }

    pub(crate) fn set_active(&mut self, turn: Option<super::TurnState>) {
        self.turn.set_active(turn);
    }

    pub(crate) fn clear_active(&mut self) -> Option<super::TurnState> {
        self.turn.clear_active()
    }

    pub(crate) fn active_id(&self) -> Option<u64> {
        self.turn.active_id()
    }

    pub(crate) fn next_turn_id(&self) -> u64 {
        self.turn.next_turn_id()
    }

    pub(crate) fn last_terminal_turn_id(&self) -> Option<u64> {
        self.turn.last_terminal_turn_id()
    }

    pub(crate) fn mark_terminal(&mut self, turn_id: u64) {
        self.turn.mark_terminal(turn_id);
    }

    pub(crate) fn applied_mode(&self) -> &protocol::AgentMode {
        self.turn.applied_mode()
    }

    pub(crate) fn set_applied_mode(&mut self, mode: protocol::AgentMode) {
        self.turn.set_applied_mode(mode);
    }

    pub(crate) fn applied_reasoning_effort(&self) -> &protocol::ReasoningEffort {
        self.turn.applied_reasoning_effort()
    }

    pub(crate) fn set_applied_reasoning_effort(
        &mut self,
        reasoning_effort: protocol::ReasoningEffort,
    ) {
        self.turn.set_applied_reasoning_effort(reasoning_effort);
    }

    pub(crate) fn active_permissions(
        &self,
        fallback: Arc<smelt_core::permissions::Permissions>,
    ) -> Arc<smelt_core::permissions::Permissions> {
        self.turn.active_permissions(fallback)
    }

    pub(crate) fn refresh_active_permissions(
        &mut self,
        permissions: Arc<smelt_core::permissions::Permissions>,
    ) {
        self.turn.refresh_active_permissions(permissions);
    }

    fn coalesce_context_history_append(
        &mut self,
        append: &super::PendingHistoryAppend,
    ) -> Result<bool, String> {
        if append.context_name().is_none() {
            return Ok(false);
        }

        self.turn.remove_matching_history_append(append);
        let history_append = append.history_append(None);
        let plan = self.plan_history_append(&history_append)?;
        debug_assert!(matches!(
            plan,
            protocol::HistoryAppendPlan::Unchanged | protocol::HistoryAppendPlan::Push
        ));
        if plan == protocol::HistoryAppendPlan::Push {
            self.turn.replace_or_push_history_append(append.clone());
        }
        Ok(true)
    }

    pub(crate) fn queue_history_append(
        &mut self,
        append: super::PendingHistoryAppend,
        mode_base: Option<&protocol::AgentMode>,
    ) -> Result<(), String> {
        if !self.coalesce_context_history_append(&append)? {
            self.turn.queue_history_append(append, mode_base);
        }
        Ok(())
    }

    pub(crate) fn pending_context_note(&self, name: &str) -> Option<Option<&str>> {
        self.turn.pending_context_note(name)
    }

    pub(crate) fn replace_or_push_history_append(
        &mut self,
        append: super::PendingHistoryAppend,
    ) -> Result<(), String> {
        if !self.coalesce_context_history_append(&append)? {
            self.turn.replace_or_push_history_append(append);
        }
        Ok(())
    }

    pub(crate) fn remove_matching_history_append(&mut self, append: &super::PendingHistoryAppend) {
        self.turn.remove_matching_history_append(append);
    }

    pub(crate) fn take_pending_history_appends(&mut self) -> Vec<super::PendingHistoryAppend> {
        self.turn.take_pending_history_appends()
    }

    pub(crate) fn take_pending_follow_up_note(&mut self) -> Option<protocol::HistoryNote> {
        self.turn.take_pending_follow_up_note()
    }

    pub(crate) fn take_matching_history_append(
        &mut self,
        item: &protocol::HistoryItem,
    ) -> Option<super::PendingHistoryAppend> {
        self.turn.take_matching_history_append(item)
    }

    pub(crate) fn clear_pending_history_appends(&mut self) {
        self.turn.clear_pending_history_appends();
    }

    pub(crate) fn retain_session_history_appends(&mut self) {
        self.turn.retain_session_history_appends();
    }

    pub(crate) fn set_pending_meta(&mut self, meta: Option<protocol::TurnMeta>) {
        self.turn.set_pending_meta(meta);
    }

    pub(crate) fn take_pending_meta(&mut self) -> Option<protocol::TurnMeta> {
        self.turn.take_pending_meta()
    }

    pub(crate) fn mark_context_tokens_updated(&mut self) {
        self.turn.mark_context_tokens_updated();
    }

    pub(crate) fn take_context_tokens_updated(&mut self) -> bool {
        self.turn.take_context_tokens_updated()
    }

    pub(crate) fn begin_dispatch(&mut self) -> Option<super::agent::DispatchingTurn> {
        self.turn.begin_dispatch()
    }

    pub(crate) fn finish_dispatch(&mut self, dispatch: super::agent::DispatchingTurn) {
        self.turn.finish_dispatch(dispatch);
    }

    pub(crate) fn begin_prepared_turn(
        &mut self,
        overrides: smelt_core::custom_commands::CommandOverrides,
    ) {
        self.turn.begin_prepared_turn(overrides);
    }

    pub(crate) fn turn_request_overrides(&self) -> &smelt_core::custom_commands::CommandOverrides {
        &self.turn.request_overrides
    }

    pub(crate) fn record_started_turn(
        &mut self,
        turn_id: u64,
        mode: protocol::AgentMode,
        reasoning_effort: protocol::ReasoningEffort,
    ) {
        self.turn
            .record_started_turn(turn_id, mode, reasoning_effort);
    }

    pub(crate) fn issue_continuation_token(&mut self) -> u64 {
        self.turn.issue_continuation_token()
    }

    pub(crate) fn clear_continuation(&mut self) {
        self.turn.clear_continuation();
    }

    pub(crate) fn turn_pause(&self) -> Option<super::agent::TurnPause> {
        self.turn.pause
    }

    pub(crate) fn set_turn_pause(&mut self, pause: Option<super::agent::TurnPause>) {
        self.turn.pause = pause;
    }

    pub(crate) fn continuation_token(&self) -> Option<u64> {
        self.turn.pending_continuation_token()
    }

    pub(crate) fn consume_continuation(&mut self, token: u64) -> bool {
        self.turn.consume_continuation(token)
    }

    pub(crate) fn invalidate_turn_callbacks(&mut self) {
        self.turn.invalidate_turn_callbacks();
    }

    pub(crate) fn cancel_generation(&self) -> u64 {
        self.turn.cancel_generation()
    }

    pub(crate) fn remove_pending_tool(&mut self, invocation_id: protocol::InvocationId) {
        if let Some(turn) = self.turn.active_mut() {
            turn.pending
                .retain(|pending| pending.invocation_id != invocation_id);
        }
    }

    pub(crate) fn clear_pending_tools(&mut self) {
        if let Some(turn) = self.turn.active_mut() {
            turn.pending.clear();
        }
    }

    #[cfg(test)]
    pub(crate) fn install_live_session_for_harness(
        &mut self,
        live_session: smelt_core::session_runtime::LiveSession,
    ) {
        self.document.live_session = Some(live_session);
    }

    #[cfg(test)]
    pub(crate) fn set_history_resave_from_for_harness(&mut self, history_index: usize) {
        self.document
            .set_history_resave_from_for_test(history_index);
    }

    #[cfg(test)]
    pub(crate) fn install_session_for_harness(&mut self, session: smelt_core::session::Session) {
        self.document.live_session = None;
        self.session = session;
    }

    #[cfg(test)]
    pub(crate) fn set_session_id_for_harness(&mut self, id: String) {
        self.session.id = id;
    }

    #[cfg(test)]
    pub(crate) fn set_session_mode_for_harness(&mut self, mode: Option<String>) {
        self.session.mode = mode;
    }

    #[cfg(test)]
    pub(crate) fn record_context_tokens_for_harness(
        &mut self,
        tokens: u32,
        identity: smelt_core::session::ContextTokenIdentity,
    ) {
        let history_len = self.history_len();
        self.session
            .record_context_tokens(tokens, history_len, identity);
    }

    #[cfg(test)]
    pub(crate) fn set_context_token_baseline_for_harness(&mut self, tokens: Option<u32>) {
        self.session.context_tokens = tokens;
        self.session.context_tokens_history_len = tokens.map(|_| self.session.history.len());
        if tokens.is_none() {
            self.session.display_context_tokens = None;
            self.session.display_context_token_identity = None;
        }
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn replace_history_for_harness(&mut self, history: Vec<protocol::HistoryItem>) {
        self.document.live_session = None;
        self.session.history = history;
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn reset_context_accounting_for_harness(&mut self) {
        self.session.context_tokens = None;
        self.session.context_tokens_history_len = None;
        self.session.context_token_identity = None;
        self.session.display_context_tokens = None;
        self.session.display_context_token_identity = None;
        self.session.checkpoint = None;
        self.session.context_snapshots.clear();
        self.session.turn_metas.clear();
    }

    pub(crate) fn pending_history_appends(&self) -> &[super::PendingHistoryAppend] {
        self.turn.pending_history_appends()
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn pending_history_append_count(&self) -> usize {
        self.turn.pending_history_append_count()
    }

    #[cfg(test)]
    pub(crate) fn pending_continuation_token(&self) -> Option<u64> {
        self.turn.pending_continuation_token()
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn streaming_state(&self) -> (bool, bool, bool) {
        (
            self.parser.has_active_text(),
            self.parser.has_active_thinking(),
            self.parser.has_active_exec(),
        )
    }

    pub(crate) fn has_active_exec(&self) -> bool {
        self.parser.has_active_exec()
    }

    pub(crate) fn set_active_tools_paused(&mut self, paused: bool, now: std::time::Instant) {
        self.parser
            .set_active_tools_paused(self.document.transcript.history_mut(), paused, now);
    }

    pub(crate) fn clear_stream_parser(&mut self) {
        self.parser.clear();
    }

    pub(crate) fn clear_stream_tools(&mut self) {
        self.parser.clear_tools();
    }

    pub(crate) fn clear_token_baseline(
        &mut self,
        identity: smelt_core::session::ContextTokenIdentity,
    ) {
        self.apply_usage_mutation(
            super::session_document::UsageMutation::ClearTokenBaselineIfMismatched { identity },
        );
    }

    pub(crate) fn clear_token_baseline_for_loaded_model(
        &mut self,
        identity: smelt_core::session::ContextTokenIdentity,
    ) {
        self.document
            .clear_token_baseline_for_loaded_model(&mut self.session, identity);
    }

    pub(crate) fn tool_draft_state(&self, call_id: &str) -> (Option<String>, bool) {
        let Some(block_id) = self
            .parser
            .tool_draft_block_for_call(self.document.transcript.history(), call_id)
        else {
            return (None, false);
        };
        let Some(smelt_core::transcript_model::Block::ToolDraft(draft)) =
            self.document.transcript.history().block(block_id)
        else {
            return (None, false);
        };
        (Some(draft.stream_id.clone()), draft.finished)
    }

    pub(crate) fn next_transcript_refresh_at(&self) -> Option<std::time::Instant> {
        self.document.transcript.next_refresh_at()
    }

    pub(crate) fn take_resume_preview(
        &mut self,
        key: &str,
    ) -> Option<super::transcript::TranscriptDocument> {
        self.resume_preview_cache.take(key)
    }

    pub(crate) fn store_resume_preview(
        &mut self,
        key: String,
        view: super::transcript::TranscriptDocument,
    ) {
        self.resume_preview_cache.store(key, view);
    }

    pub(crate) fn invalidate_transcript_theme(&mut self) {
        self.document.transcript.invalidate_theme();
        self.resume_preview_cache.invalidate_theme();
    }

    pub(crate) fn set_transcript_inline_options(
        &mut self,
        options: smelt_core::content::highlight::InlineOptions,
    ) {
        self.document.transcript.set_inline_options(options.clone());
        self.resume_preview_cache.set_inline_options(options);
    }

    pub(crate) fn invalidate_transcript_renderer(
        &mut self,
        generation: u64,
        cache_key: Option<u64>,
    ) {
        self.document
            .transcript
            .invalidate_renderer_if_changed(generation, cache_key);
        self.resume_preview_cache
            .invalidate_renderer_if_changed(generation, cache_key);
    }

    pub(crate) fn is_ephemeral(&self) -> bool {
        self.storage.is_ephemeral()
    }

    pub(crate) fn current_artifact_dir(&self) -> std::path::PathBuf {
        self.storage.artifact_dir(&self.sessions, &self.session)
    }

    pub(crate) fn sessions(&self) -> &smelt_core::session::SessionStorage {
        &self.sessions
    }

    pub(crate) fn history_len(&self) -> usize {
        self.document
            .live_session
            .as_ref()
            .map_or(self.session.history.len(), |live| live.history_len())
    }

    pub(crate) fn history_is_empty(&self) -> bool {
        self.document
            .live_session
            .as_ref()
            .map_or(self.session.history.is_empty(), |live| live.is_empty())
    }

    pub(crate) fn history_range(
        &self,
        range: std::ops::Range<usize>,
    ) -> Result<Vec<protocol::HistoryItem>, String> {
        if let Some(live) = &self.document.live_session {
            return live.history_range(range);
        }
        let end = range.end.min(self.session.history.len());
        let start = range.start.min(end);
        Ok(self.session.history[start..end].to_vec())
    }

    pub(crate) fn scan_history(
        &self,
        range: std::ops::Range<usize>,
        chunk_items: usize,
        mut visit: impl FnMut(usize, &[protocol::HistoryItem]),
    ) -> Result<(), String> {
        if let Some(live) = &self.document.live_session {
            return live.scan_history(range, chunk_items, visit);
        }
        let end = range.end.min(self.session.history.len());
        let start = range.start.min(end);
        let chunk_items = chunk_items.max(1);
        for (chunk_offset, items) in self.session.history[start..end]
            .chunks(chunk_items)
            .enumerate()
        {
            visit(
                start.saturating_add(chunk_offset.saturating_mul(chunk_items)),
                items,
            );
        }
        Ok(())
    }

    pub(crate) fn history_tail(
        &self,
        max_items: usize,
        max_bytes: Option<usize>,
    ) -> Result<Vec<protocol::HistoryItem>, String> {
        if let Some(live) = &self.document.live_session {
            return live.history_tail(max_items, max_bytes);
        }
        smelt_core::session_runtime::bounded_history_tail(
            &self.session.history,
            max_items,
            max_bytes,
        )
    }

    pub(crate) fn has_resume_hint_messages(&self) -> bool {
        self.history_len() > 0
            || !self.document.transcript.is_empty()
            || self.document.live_session.is_some()
    }

    pub(crate) fn shutdown_context(&self) -> super::ShutdownContext {
        super::ShutdownContext {
            session_id: self.session.id.clone(),
            has_messages: self.has_resume_hint_messages(),
            ephemeral: self.is_ephemeral(),
        }
    }

    pub(crate) fn publish_shared_state(&self) {
        if let Ok(mut guard) = self.shared_session.lock() {
            *guard = Some(SharedSessionState {
                id: self.session.id.clone(),
                has_messages: self.has_resume_hint_messages(),
                ephemeral: self.is_ephemeral(),
            });
        }
    }

    pub(crate) fn clear_shared_state(&self) {
        if let Ok(mut guard) = self.shared_session.lock() {
            *guard = None;
        }
    }

    pub(crate) fn commit_transcript_view(&mut self, state: super::TranscriptViewState) -> bool {
        if self
            .committed_transcript_view
            .as_ref()
            .is_some_and(|view| view.state == state)
        {
            return false;
        }
        let revision = self
            .committed_transcript_view
            .as_ref()
            .map(|view| view.revision.wrapping_add(1))
            .unwrap_or(1);
        self.committed_transcript_view = Some(CommittedTranscriptView { revision, state });
        true
    }

    pub(crate) fn committed_transcript_view(&self) -> Option<CommittedTranscriptView> {
        self.committed_transcript_view.clone()
    }

    pub(crate) fn is_read_only(&self) -> bool {
        self.access.is_read_only()
    }

    pub(crate) fn read_only_reason(&self) -> String {
        match &self.access {
            SessionAccess::ReadOnly { reason } => reason.clone(),
            SessionAccess::Owned => "session is read-only".to_string(),
        }
    }

    pub(crate) fn mark_read_only(&mut self, reason: String) {
        self.access = SessionAccess::ReadOnly { reason };
        self.document.disable_change_tracking();
    }

    pub(crate) fn has_persistence(&self) -> bool {
        self.persistence.is_some()
    }

    pub(crate) fn request_search_projection(&self) -> bool {
        self.persistence
            .as_ref()
            .is_some_and(crate::persist::SessionPersistence::request_search_projection)
    }

    pub(crate) fn delete_branch_through_persistence(
        &self,
        target: &smelt_core::session_id::SessionId,
    ) -> Result<bool, String> {
        let Some(persistence) = self.persistence.as_ref() else {
            return Ok(false);
        };
        let root = self.sessions.sessions_dir();
        let active = smelt_store::LineageSessionReader::try_open_existing(&root, &self.session.id)
            .map_err(|error| error.to_string())?;
        let target_reader =
            smelt_store::LineageSessionReader::try_open_existing(root, target.as_str())
                .map_err(|error| error.to_string())?;
        let (Some(active), Some(target_reader)) = (active, target_reader) else {
            return Ok(false);
        };
        if active.lineage_id() != target_reader.lineage_id() {
            return Ok(false);
        }
        persistence
            .delete_branch(
                target.clone(),
                std::time::Instant::now() + crate::persist::INTERACTIVE_PERSISTENCE_DEADLINE,
            )
            .map_err(|cause| cause.message)?;
        Ok(true)
    }

    pub(crate) fn has_unflushed_work(&self) -> bool {
        !self.is_ephemeral() && self.document.has_unflushed_work(&self.session)
    }

    pub(crate) fn persistence_scope(&self) -> protocol::PersistenceScope {
        self.persistence_scope_at(
            self.document.generation().get(),
            self.document.acknowledged_head().revision.get(),
        )
    }

    pub(crate) fn persistence_scope_at(
        &self,
        required_generation: u64,
        store_revision: u64,
    ) -> protocol::PersistenceScope {
        protocol::PersistenceScope {
            epoch: self
                .persistence
                .as_ref()
                .map_or(0, |actor| actor.epoch().get()),
            required_generation,
            store_revision,
        }
    }

    #[cfg(test)]
    pub(crate) fn shared_state(&self) -> Option<SharedSessionState> {
        self.shared_session.lock().ok()?.clone()
    }

    #[cfg(test)]
    pub(crate) fn persistence_status(&self) -> Option<crate::persist::SessionPersistenceStatus> {
        self.persistence
            .as_ref()
            .map(|persistence| persistence.status())
    }

    #[cfg(test)]
    pub(crate) fn inject_commit_failure(&self, failure: smelt_store::SessionCommitFailure) {
        self.persistence
            .as_ref()
            .expect("persistence actor")
            .inject_commit_failure(failure);
    }

    #[cfg(test)]
    pub(crate) fn inject_publish_failure(&self) {
        self.persistence
            .as_ref()
            .expect("persistence actor")
            .inject_publish_failure();
    }

    pub(crate) fn reset(&mut self, pid: u32, cwd: std::path::PathBuf) -> String {
        let old_id = self.session.id.clone();
        self.session = smelt_core::session::Session::new(pid, cwd);
        self.clear_live_session();
        self.document.transcript.set_store_address(
            self.storage
                .transcript_store_address(&self.sessions, &self.session),
        );
        self.access = SessionAccess::Owned;
        self.document.enable_change_tracking();
        self.turn.reset_session();
        self.canonical_operations.clear();
        self.inflight_canonical_commands.clear();
        self.document.mark_session_unpersisted();
        self.clear_shared_state();
        old_id
    }

    pub(crate) fn install_loaded_session(
        &mut self,
        loaded: smelt_core::session::Session,
    ) -> Option<String> {
        self.session = loaded;
        self.turn.reset_session();
        self.canonical_operations.clear();
        self.inflight_canonical_commands.clear();
        self.session.cwd.clone()
    }

    pub(crate) fn claim_writer_access(&mut self) -> Result<(), String> {
        debug_assert!(self.persistence.is_none());
        if self.is_ephemeral() {
            self.access = SessionAccess::Owned;
            self.document.enable_change_tracking();
            return Ok(());
        }
        let epoch = self
            .persistence_epoch
            .checked_next()
            .expect("session persistence epoch overflow");
        let session_id = smelt_core::session_id::SessionId::parse(&self.session.id)
            .map_err(|error| format!("invalid session id: {error}"))?;
        let (persistence, startup) = crate::persist::SessionPersistence::spawn(
            self.sessions.clone(),
            session_id,
            epoch,
            self.document.durable_generation(),
            self.document.acknowledged_head(),
        )
        .map_err(|cause| cause.message)?;
        self.persistence_epoch = epoch;
        self.observed_persistence_status = None;
        self.document.bind_persistence(epoch);
        self.persistence = Some(persistence);
        self.access = SessionAccess::Owned;
        self.document.enable_change_tracking();
        self.turn.set_last_terminal_turn_id(
            startup
                .latest_terminal_turn_id
                .map(smelt_store::TurnId::get),
        );
        if let Some(recovery) = startup.recovery {
            let acknowledgement = crate::persist::PersistenceAcknowledgement {
                epoch,
                generation: self.document.generation(),
                record_projection:
                    super::session_document::SessionRecordSaveProjection::persisted_head(
                        recovery.session.current,
                    ),
                previous: recovery.session.previous,
                receipt: recovery.session,
            };
            let acknowledged = self.document.acknowledge(
                &acknowledgement,
                &self.session.id,
                self.history_len(),
                self.session.checkpoint.as_ref(),
            );
            if !acknowledged {
                let reason =
                    "startup turn recovery receipt did not match the session document".to_string();
                self.mark_read_only(reason.clone());
                return Err(reason);
            }
        }
        Ok(())
    }

    pub(crate) fn drain_persistence_report(&mut self) -> Option<PersistenceReport> {
        let persistence = self.persistence.as_ref()?;
        let status_woke = persistence.drain_status_wake();
        if !status_woke
            && !persistence.is_finished()
            && persistence.status().canonical_completions.is_empty()
        {
            return None;
        }
        let status = persistence.take_status();
        if self.observed_persistence_status.as_ref() == Some(&status)
            && status.canonical_completions.is_empty()
        {
            return None;
        }
        self.observed_persistence_status = Some(status.clone());

        let mut acknowledged_session_id = None;
        let acknowledgement_applied =
            status
                .acknowledgement
                .as_ref()
                .is_none_or(|acknowledgement| {
                    let applied = self.apply_persistence_acknowledgement(acknowledgement);
                    if applied {
                        acknowledged_session_id = Some(acknowledgement.receipt.session_id.clone());
                    }
                    applied
                });
        let durable_generation = self.document.durable_generation();
        let canonical_completions = status
            .canonical_completions
            .iter()
            .filter(|completion| {
                acknowledgement_applied
                    || match completion {
                        crate::persist::CanonicalCommandCompletion::Submit(acknowledgement) => {
                            acknowledgement.persistence.generation <= durable_generation
                        }
                        crate::persist::CanonicalCommandCompletion::Transition(acknowledgement) => {
                            acknowledgement.persistence.generation <= durable_generation
                        }
                        crate::persist::CanonicalCommandCompletion::Failed { .. } => true,
                    }
            })
            .cloned()
            .collect::<Vec<_>>();

        let failure = match &status.state {
            crate::persist::PersistenceState::Blocked { desired, cause, .. } => {
                let command_id =
                    canonical_completions
                        .iter()
                        .find_map(|completion| match completion {
                            crate::persist::CanonicalCommandCompletion::Failed {
                                command_id,
                                generation,
                                cause: command_cause,
                            } if generation == desired && command_cause == cause => {
                                Some(*command_id)
                            }
                            _ => None,
                        });
                Some(PersistenceFailureReport {
                    session_id: self.session.id.clone(),
                    target: command_id.map_or(
                        PersistenceFailureTarget::None,
                        PersistenceFailureTarget::Command,
                    ),
                    cause: cause.clone(),
                })
            }
            crate::persist::PersistenceState::OwnershipLost { cause, .. } => {
                self.mark_read_only(cause.message.clone());
                Some(PersistenceFailureReport {
                    session_id: self.session.id.clone(),
                    target: PersistenceFailureTarget::AllCanonicalCommands,
                    cause: cause.clone(),
                })
            }
            crate::persist::PersistenceState::Stopped {
                cause: Some(cause), ..
            } => Some(PersistenceFailureReport {
                session_id: self.session.id.clone(),
                target: PersistenceFailureTarget::AllCanonicalCommands,
                cause: cause.clone(),
            }),
            crate::persist::PersistenceState::Idle { .. }
            | crate::persist::PersistenceState::Saving { .. }
            | crate::persist::PersistenceState::Durable { .. }
            | crate::persist::PersistenceState::Stopped { cause: None, .. } => None,
        };
        Some(PersistenceReport {
            acknowledged_session_id,
            canonical_completions,
            failure,
            audit_warning: status.latest_audit_warning.map(|warning| warning.message),
        })
    }

    pub(crate) fn save(
        &mut self,
        metadata: super::session_document::RuntimeSessionMetadata,
    ) -> Result<SaveStatus, String> {
        if self.is_read_only() {
            return Ok(SaveStatus::SkippedReadOnly);
        }
        if self.persistence_preparation_block.is_some() {
            return Ok(SaveStatus::Blocked);
        }
        if self.is_ephemeral() {
            self.apply_metadata_mutation(
                super::session_document::MetadataMutation::UpdateRuntime {
                    updated_at_ms: metadata.updated_at_ms,
                    mode: metadata.mode,
                    reasoning_effort: metadata.reasoning_effort,
                    model: metadata.model,
                    fast_mode: metadata.fast_mode,
                },
            );
            self.publish_shared_state();
            self.document.mark_ephemeral_persisted();
            return Ok(SaveStatus::DurableEphemeral);
        }
        if let Some(live) = self.document.live_session.as_ref() {
            smelt_perf::perf::record_value(
                "live_session:suffix_items",
                live.live_suffix_len() as u64,
            );
            smelt_perf::perf::record_value(
                "live_session:suffix_bytes",
                live.live_suffix_bytes() as u64,
            );
        }
        let intent = match self
            .document
            .prepare_event_batch(&mut self.session, metadata)
        {
            Ok(Some(intent)) => intent,
            Ok(None) => return Ok(SaveStatus::Unchanged),
            Err(super::session_document::SessionBatchPreparationError::HydrationPending) => {
                return Ok(SaveStatus::DeferredHydration);
            }
            Err(super::session_document::SessionBatchPreparationError::Invalid(message)) => {
                self.persistence_preparation_block = Some(PersistencePreparationBlock {
                    desired: self.document.generation(),
                    cause: crate::persist::PersistenceCause::invariant(message.clone()),
                });
                return Err(message);
            }
        };
        if self.persistence.is_none() {
            if let Err(reason) = self.claim_writer_access() {
                self.mark_read_only(reason.clone());
                return Err(reason);
            }
        }
        self.publish_shared_state();
        self.persistence
            .as_ref()
            .ok_or_else(|| "persistence actor is unavailable".to_string())?
            .submit(intent)
            .map_err(|cause| cause.message)?;
        Ok(SaveStatus::Submitted)
    }

    pub(crate) fn retry_blocked_persistence(
        &mut self,
        metadata: super::session_document::RuntimeSessionMetadata,
    ) -> Result<SaveStatus, String> {
        let preparation_blocked = self.persistence_preparation_block.is_some();
        let actor_blocked_at = self
            .persistence
            .as_ref()
            .and_then(|persistence| match persistence.status().state {
                crate::persist::PersistenceState::Blocked { desired, .. }
                | crate::persist::PersistenceState::OwnershipLost { desired, .. } => Some(desired),
                _ => None,
            });
        if !preparation_blocked && actor_blocked_at.is_none() {
            return Ok(SaveStatus::Unchanged);
        }

        let latest_generation = self.document.generation();
        let prepare_latest = preparation_blocked
            || actor_blocked_at.is_some_and(|blocked| blocked < latest_generation);
        if preparation_blocked {
            self.document.transcript.retry_failed_hydrations();
        }
        if prepare_latest {
            let intent = match self
                .document
                .prepare_event_batch(&mut self.session, metadata)
            {
                Ok(intent) => intent,
                Err(super::session_document::SessionBatchPreparationError::HydrationPending) => {
                    self.persistence_preparation_block = None;
                    return Ok(SaveStatus::DeferredHydration);
                }
                Err(super::session_document::SessionBatchPreparationError::Invalid(message)) => {
                    self.persistence_preparation_block = Some(PersistencePreparationBlock {
                        desired: self.document.generation(),
                        cause: crate::persist::PersistenceCause::invariant(message.clone()),
                    });
                    return Err(message);
                }
            };
            if let Some(intent) = intent {
                if self.persistence.is_none() {
                    if let Err(reason) = self.claim_writer_access() {
                        self.mark_read_only(reason.clone());
                        return Err(reason);
                    }
                }
                self.publish_shared_state();
                self.persistence
                    .as_ref()
                    .ok_or_else(|| "persistence actor is unavailable".to_string())?
                    .submit(intent)
                    .map_err(|cause| cause.message)?;
            }
            self.persistence_preparation_block = None;
        }

        if actor_blocked_at.is_some() {
            self.persistence
                .as_ref()
                .ok_or_else(|| "persistence actor is unavailable".to_string())?
                .retry_blocked()
                .map_err(|cause| cause.message)?;
        }
        Ok(SaveStatus::Submitted)
    }

    pub(crate) fn flush_persistence_until(
        &self,
        deadline: std::time::Instant,
    ) -> crate::persist::PersistenceFlushOutcome {
        let target = self.document.generation();
        if let Some(block) = self.persistence_preparation_block.as_ref() {
            return crate::persist::PersistenceFlushOutcome::Blocked {
                epoch: self.persistence_epoch,
                target: target.max(block.desired),
                durable: self.document.durable_generation(),
                cause: block.cause.clone(),
            };
        }
        let Some(persistence) = self.persistence.as_ref() else {
            return crate::persist::PersistenceFlushOutcome::Stopped {
                epoch: self.persistence_epoch,
                target,
                durable: self.document.durable_generation(),
                cause: crate::persist::PersistenceCause::unavailable(
                    "persistence actor is unavailable",
                ),
            };
        };
        persistence.flush(target, deadline)
    }

    #[cfg(test)]
    pub(crate) fn pause_persistence(&self) -> std::sync::mpsc::Sender<()> {
        self.persistence
            .as_ref()
            .expect("persistence actor is running")
            .pause()
    }

    #[cfg(test)]
    pub(crate) fn install_persistence_commit_barrier(
        &self,
    ) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        self.persistence
            .as_ref()
            .expect("persistence actor is running")
            .install_commit_barrier()
    }

    pub(crate) fn append_request_audit(
        &self,
        scope: protocol::PersistenceScope,
        entry: protocol::request_log::RequestLogEntry,
        payload_mode: smelt_store::RequestAuditPayloadMode,
    ) -> Result<bool, crate::persist::PersistenceCause> {
        let Some(actor) = self.persistence.as_ref() else {
            return Ok(false);
        };
        let epoch = crate::persist::SessionEpoch::new(scope.epoch);
        if epoch != actor.epoch() || self.is_ephemeral() || self.is_read_only() {
            return Ok(false);
        }
        actor.append_request_audit(crate::persist::RequestAuditIntent {
            epoch,
            required_generation: super::session_document::PersistenceGeneration::new(
                scope.required_generation,
            ),
            entry,
            payload_mode,
            payload_capture_skipped_bytes: None,
        })?;
        Ok(true)
    }

    pub(crate) fn close_persistence_until(
        &mut self,
        policy: crate::persist::ClosePolicy,
        deadline: std::time::Instant,
    ) -> Result<Option<String>, String> {
        let target = self.document.generation();
        if policy == crate::persist::ClosePolicy::RequireDurable {
            if let Some(block) = self.persistence_preparation_block.as_ref() {
                return Err(block.cause.message.clone());
            }
        }
        let preparation_failure = self
            .persistence_preparation_block
            .take()
            .map(|block| block.cause.message);
        let Some(persistence) = self.persistence.as_mut() else {
            return Ok(preparation_failure);
        };
        let epoch = persistence.epoch();
        let outcome = persistence.close(target, deadline, policy);
        if let Some(acknowledgement) = outcome.acknowledgement.as_ref() {
            self.apply_persistence_acknowledgement(acknowledgement);
        }
        if outcome.durable < outcome.target && outcome.omitted.is_none() {
            return Err(outcome.cause.map_or_else(
                || {
                    format!(
                        "generation {} did not become durable before session close",
                        target.get()
                    )
                },
                |cause| cause.message,
            ));
        }
        self.document.unbind_persistence(epoch);
        self.persistence = None;
        self.observed_persistence_status = None;
        self.canonical_operations.clear();
        self.inflight_canonical_commands.clear();
        Ok(outcome
            .cause
            .map(|cause| cause.message)
            .or(preparation_failure))
    }

    pub(crate) fn submit_canonical_turn(
        &mut self,
        metadata: super::session_document::RuntimeSessionMetadata,
        turn: smelt_store::NewTurn,
    ) -> Result<CanonicalTurnSubmitOutcome, crate::persist::PersistenceCause> {
        if self.persistence.is_none() {
            if let Err(reason) = self.claim_writer_access() {
                self.mark_read_only(reason.clone());
                return Err(crate::persist::PersistenceCause::unavailable(reason));
            }
        }
        if self.access.is_read_only() {
            return Err(crate::persist::PersistenceCause::unavailable(
                "session is read-only",
            ));
        }
        let command_id = self.allocate_canonical_command_id();
        let operation = CanonicalOperation {
            command_id,
            payload: CanonicalOperationPayload::Submit { metadata, turn },
        };
        let can_drive = self.canonical_operations.is_empty();
        self.canonical_operations.push_back(operation);
        if !can_drive {
            return Ok(CanonicalTurnSubmitOutcome::PendingPreparation { command_id });
        }
        match self.process_front_canonical_operation() {
            Ok(CanonicalOperationProgress::Submit(outcome)) => Ok(outcome),
            Ok(CanonicalOperationProgress::DeferredPreparation) => {
                Ok(CanonicalTurnSubmitOutcome::PendingPreparation { command_id })
            }
            Ok(
                CanonicalOperationProgress::TransitionDurable(_)
                | CanonicalOperationProgress::Queued,
            ) => {
                unreachable!("new canonical submission is the front operation")
            }
            Err(cause) => {
                self.discard_canonical_operation(command_id);
                Err(cause)
            }
        }
    }

    pub(crate) fn enqueue_canonical_turn_transition(
        &mut self,
        metadata: super::session_document::RuntimeSessionMetadata,
        turn_id: smelt_store::TurnId,
        state: smelt_store::TurnState,
        terminal_reason: Option<String>,
    ) -> Result<(), crate::persist::PersistenceCause> {
        let command_id = self.allocate_canonical_command_id();
        let operation = CanonicalOperation {
            command_id,
            payload: CanonicalOperationPayload::Transition {
                metadata,
                turn_id,
                state,
                at_ms: smelt_core::session::now_ms(),
                terminal_reason,
                dispatch: CanonicalTransitionDispatch::Enqueue,
            },
        };
        let can_drive = self.canonical_operations.is_empty();
        self.canonical_operations.push_back(operation);
        if !can_drive {
            return Ok(());
        }
        match self.process_front_canonical_operation() {
            Ok(
                CanonicalOperationProgress::DeferredPreparation
                | CanonicalOperationProgress::Queued,
            ) => Ok(()),
            Ok(
                CanonicalOperationProgress::TransitionDurable(_)
                | CanonicalOperationProgress::Submit(_),
            ) => {
                unreachable!("enqueued transition cannot complete synchronously")
            }
            Err(cause) => {
                self.discard_canonical_operation(command_id);
                Err(cause)
            }
        }
    }

    pub(crate) fn commit_canonical_turn_transition(
        &mut self,
        metadata: super::session_document::RuntimeSessionMetadata,
        turn_id: smelt_store::TurnId,
        state: smelt_store::TurnState,
        terminal_reason: Option<String>,
    ) -> Result<crate::persist::TurnTransitionOutcome, crate::persist::PersistenceCause> {
        let command_id = self.allocate_canonical_command_id();
        let operation = CanonicalOperation {
            command_id,
            payload: CanonicalOperationPayload::Transition {
                metadata,
                turn_id,
                state,
                at_ms: smelt_core::session::now_ms(),
                terminal_reason,
                dispatch: CanonicalTransitionDispatch::Commit,
            },
        };
        let can_drive = self.canonical_operations.is_empty();
        self.canonical_operations.push_back(operation);
        if !can_drive {
            return Ok(crate::persist::TurnTransitionOutcome::Pending {
                command_id,
                generation: self.document.generation(),
            });
        }
        match self.process_front_canonical_operation() {
            Ok(CanonicalOperationProgress::TransitionDurable(acknowledgement)) => Ok(
                crate::persist::TurnTransitionOutcome::Durable(acknowledgement),
            ),
            Ok(
                CanonicalOperationProgress::DeferredPreparation
                | CanonicalOperationProgress::Queued,
            ) => Ok(crate::persist::TurnTransitionOutcome::Pending {
                command_id,
                generation: self.document.generation(),
            }),
            Ok(CanonicalOperationProgress::Submit(_)) => {
                unreachable!("committed transition is the front operation")
            }
            Err(cause) => {
                self.discard_canonical_operation(command_id);
                Err(cause)
            }
        }
    }

    pub(crate) fn canonical_operations_are_pending(&self) -> bool {
        !self.canonical_operations.is_empty() || !self.inflight_canonical_commands.is_empty()
    }

    pub(crate) fn confirm_canonical_completion(
        &mut self,
        command_id: crate::persist::CanonicalCommandId,
    ) -> bool {
        if self
            .inflight_canonical_commands
            .remove(&command_id)
            .is_none()
        {
            return false;
        }
        if let Some(persistence) = self.persistence.as_ref() {
            persistence.confirm_canonical_completion(command_id);
        }
        true
    }

    pub(crate) fn abandon_canonical_operation(
        &mut self,
        command_id: crate::persist::CanonicalCommandId,
    ) {
        self.discard_canonical_operation(command_id);
        self.inflight_canonical_commands.remove(&command_id);
        if let Some(persistence) = self.persistence.as_ref() {
            persistence.confirm_canonical_completion(command_id);
        }
    }

    pub(crate) fn abandon_all_canonical_operations(&mut self) {
        let mut command_ids = self
            .canonical_operations
            .drain(..)
            .map(|operation| operation.command_id)
            .collect::<Vec<_>>();
        command_ids.extend(
            self.inflight_canonical_commands
                .drain()
                .map(|(command_id, _)| command_id),
        );
        if let Some(persistence) = self.persistence.as_ref() {
            for command_id in command_ids {
                persistence.confirm_canonical_completion(command_id);
            }
        }
    }

    pub(crate) fn drive_canonical_operations(&mut self) -> Vec<CanonicalOperationEvent> {
        let mut events = Vec::new();
        loop {
            if self.canonical_operations.is_empty() {
                break;
            }
            let command_id = self
                .canonical_operations
                .front()
                .expect("front canonical operation")
                .command_id;
            match self.process_front_canonical_operation() {
                Ok(CanonicalOperationProgress::DeferredPreparation) => break,
                Ok(CanonicalOperationProgress::Queued) => {}
                Ok(CanonicalOperationProgress::Submit(outcome)) => {
                    events.push(CanonicalOperationEvent::Submit(outcome));
                    break;
                }
                Ok(CanonicalOperationProgress::TransitionDurable(acknowledgement)) => {
                    events.push(CanonicalOperationEvent::TransitionDurable(acknowledgement));
                }
                Err(cause) => {
                    self.discard_canonical_operation(command_id);
                    events.push(CanonicalOperationEvent::Failed { command_id, cause });
                    break;
                }
            }
        }
        events
    }

    fn process_front_canonical_operation(
        &mut self,
    ) -> Result<CanonicalOperationProgress, crate::persist::PersistenceCause> {
        let operation = self
            .canonical_operations
            .front()
            .cloned()
            .expect("front canonical operation");
        let metadata = match &operation.payload {
            CanonicalOperationPayload::Submit { metadata, .. }
            | CanonicalOperationPayload::Transition { metadata, .. } => metadata.clone(),
        };
        let session = match self
            .document
            .prepare_turn_batch(&mut self.session, metadata)
        {
            Ok(session) => session,
            Err(super::session_document::SessionBatchPreparationError::HydrationPending) => {
                return Ok(CanonicalOperationProgress::DeferredPreparation);
            }
            Err(super::session_document::SessionBatchPreparationError::Invalid(message)) => {
                return Err(crate::persist::PersistenceCause::invariant(message));
            }
        };
        self.publish_shared_state();
        match operation.payload {
            CanonicalOperationPayload::Submit { turn, .. } => {
                let outcome = self
                    .persistence
                    .as_ref()
                    .ok_or_else(|| {
                        crate::persist::PersistenceCause::unavailable(
                            "persistence actor is unavailable",
                        )
                    })?
                    .submit_turn(
                        crate::persist::SubmitTurnIntent {
                            command_id: operation.command_id,
                            session,
                            turn,
                        },
                        std::time::Instant::now() + crate::persist::DEFAULT_PERSISTENCE_DEADLINE,
                    )?;
                match outcome {
                    crate::persist::SubmitTurnOutcome::Durable(acknowledgement) => {
                        if !self.apply_persistence_acknowledgement(&acknowledgement.persistence) {
                            return Err(crate::persist::PersistenceCause::invariant(
                                "turn submission receipt did not match the session document",
                            ));
                        }
                        self.canonical_operations.pop_front();
                        Ok(CanonicalOperationProgress::Submit(
                            CanonicalTurnSubmitOutcome::Durable(acknowledgement),
                        ))
                    }
                    crate::persist::SubmitTurnOutcome::Pending {
                        command_id,
                        generation,
                    } => {
                        self.canonical_operations.pop_front();
                        self.inflight_canonical_commands
                            .insert(command_id, InflightCanonicalCommand::Submit);
                        Ok(CanonicalOperationProgress::Submit(
                            CanonicalTurnSubmitOutcome::PendingPersistence {
                                command_id,
                                generation,
                            },
                        ))
                    }
                }
            }
            CanonicalOperationPayload::Transition {
                turn_id,
                state,
                at_ms,
                terminal_reason,
                dispatch,
                ..
            } => {
                let generation = session.generation;
                let intent = crate::persist::TurnTransitionIntent {
                    command_id: operation.command_id,
                    session,
                    turn_id,
                    state,
                    at_ms,
                    terminal_reason,
                };
                match dispatch {
                    CanonicalTransitionDispatch::Enqueue => {
                        self.persistence
                            .as_ref()
                            .ok_or_else(|| {
                                crate::persist::PersistenceCause::unavailable(
                                    "persistence actor is unavailable",
                                )
                            })?
                            .enqueue_turn_transition(intent)?;
                        self.canonical_operations.pop_front();
                        self.inflight_canonical_commands.insert(
                            operation.command_id,
                            InflightCanonicalCommand::Transition {
                                generation,
                                terminal: state.is_terminal(),
                            },
                        );
                        Ok(CanonicalOperationProgress::Queued)
                    }
                    CanonicalTransitionDispatch::Commit => {
                        let outcome = self
                            .persistence
                            .as_ref()
                            .ok_or_else(|| {
                                crate::persist::PersistenceCause::unavailable(
                                    "persistence actor is unavailable",
                                )
                            })?
                            .transition_turn(
                                intent,
                                std::time::Instant::now()
                                    + crate::persist::DEFAULT_PERSISTENCE_DEADLINE,
                            )?;
                        match outcome {
                            crate::persist::TurnTransitionOutcome::Durable(acknowledgement) => {
                                if !self
                                    .apply_persistence_acknowledgement(&acknowledgement.persistence)
                                {
                                    return Err(crate::persist::PersistenceCause::invariant(
                                        "turn transition receipt did not match the session document",
                                    ));
                                }
                                self.canonical_operations.pop_front();
                                Ok(CanonicalOperationProgress::TransitionDurable(
                                    acknowledgement,
                                ))
                            }
                            crate::persist::TurnTransitionOutcome::Pending {
                                command_id,
                                generation,
                            } => {
                                self.canonical_operations.pop_front();
                                self.inflight_canonical_commands.insert(
                                    command_id,
                                    InflightCanonicalCommand::Transition {
                                        generation,
                                        terminal: state.is_terminal(),
                                    },
                                );
                                Ok(CanonicalOperationProgress::Queued)
                            }
                        }
                    }
                }
            }
        }
    }

    fn allocate_canonical_command_id(&mut self) -> crate::persist::CanonicalCommandId {
        let command_id = crate::persist::CanonicalCommandId::new(self.next_canonical_command_id);
        self.next_canonical_command_id = self
            .next_canonical_command_id
            .checked_add(1)
            .expect("canonical command ID overflow");
        command_id
    }

    fn discard_canonical_operation(&mut self, command_id: crate::persist::CanonicalCommandId) {
        self.canonical_operations
            .retain(|operation| operation.command_id != command_id);
    }

    fn apply_persistence_acknowledgement(
        &mut self,
        acknowledgement: &crate::persist::PersistenceAcknowledgement,
    ) -> bool {
        let applied = self.document.acknowledge_coalesced_batch(
            acknowledgement,
            &self.session.id,
            self.history_len(),
            self.session.checkpoint.as_ref(),
        );
        if applied {
            self.refresh_transcript_store_address(&acknowledgement.receipt);
            if let Some(persistence) = self.persistence.as_ref() {
                persistence.confirm_acknowledgement(acknowledgement);
            }
        }
        if applied || acknowledgement.generation <= self.document.durable_generation() {
            self.confirm_superseded_nonterminal_commands(acknowledgement.generation);
        }
        applied
    }

    fn confirm_superseded_nonterminal_commands(
        &mut self,
        durable_generation: super::session_document::PersistenceGeneration,
    ) {
        // A later cumulative acknowledgement makes nonterminal transitions durable.
        // Equal-generation receipts still need exact command confirmation.
        let command_ids = self
            .inflight_canonical_commands
            .iter()
            .filter_map(|(command_id, command)| match command {
                InflightCanonicalCommand::Transition {
                    generation,
                    terminal: false,
                } if *generation < durable_generation => Some(*command_id),
                InflightCanonicalCommand::Submit => None,
                InflightCanonicalCommand::Transition { .. } => None,
            })
            .collect::<Vec<_>>();
        for command_id in command_ids {
            self.inflight_canonical_commands.remove(&command_id);
            if let Some(persistence) = self.persistence.as_ref() {
                persistence.confirm_canonical_completion(command_id);
            }
        }
    }

    fn refresh_transcript_store_address(&mut self, receipt: &smelt_store::SaveReceipt) {
        let Some(lineage_id) = receipt.lineage_id.clone() else {
            return;
        };
        self.document.transcript.set_store_address(Some(
            smelt_core::session::SessionStoreAddress::new(
                self.sessions.sessions_dir(),
                self.session.id.clone(),
                lineage_id,
            ),
        ));
    }
}
