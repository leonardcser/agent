use std::time::{Duration, Instant};

use protocol::{HistoryItem, ReasoningEffort, TokenUsage, TurnMeta};
use smelt_core::content::stream_parser::{StreamParser, ToolStart};
use smelt_core::session::{
    ContextCheckpoint, ContextTokenIdentity, Session, SessionHeader, SessionStoreAddress,
};
use smelt_core::session_runtime::LiveSession;
use smelt_core::transcript_model::{Block, BlockId, BlockOrigin, ToolOutputRef, ToolStatus};

use crate::app::transcript::{
    TranscriptDocument, TranscriptRecordSaveBounds, TranscriptSaveHydrationError,
};
use crate::persist::SessionEpoch;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct PersistenceGeneration(u64);

impl Default for PersistenceGeneration {
    fn default() -> Self {
        Self::ZERO
    }
}

impl PersistenceGeneration {
    pub(crate) const ZERO: Self = Self(0);

    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }

    pub(crate) const fn get(self) -> u64 {
        self.0
    }

    pub(crate) fn checked_next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct DocumentChanges {
    current: PersistenceGeneration,
    durable: PersistenceGeneration,
    acknowledged_head: smelt_store::StoreHead,
    history_dirty_from: Option<usize>,
}

impl DocumentChanges {
    fn reserve_generation(&self) -> PersistenceGeneration {
        self.current
            .checked_next()
            .expect("session persistence generation overflow")
    }

    fn record(&mut self, generation: PersistenceGeneration, result: &DocumentChange) {
        if !result.canonical_changed() {
            return;
        }
        debug_assert_eq!(generation, self.reserve_generation());
        self.current = generation;
        if let Some(index) = result.history_dirty_from {
            self.history_dirty_from = Some(
                self.history_dirty_from
                    .map_or(index, |current| current.min(index)),
            );
        }
    }

    fn force_dirty(&mut self) {
        self.current = self.reserve_generation();
    }

    fn force_dirty_from(&mut self, history_index: usize) {
        self.force_dirty();
        self.history_dirty_from = Some(
            self.history_dirty_from
                .map_or(history_index, |current| current.min(history_index)),
        );
    }

    fn install_head(&mut self, head: smelt_store::StoreHead) {
        *self = Self {
            acknowledged_head: head,
            ..Self::default()
        };
    }

    fn mark_clean(&mut self, head: smelt_store::StoreHead) {
        self.durable = self.current;
        self.acknowledged_head = head;
        self.history_dirty_from = None;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SessionRecordSaveProjection {
    pub(crate) bounds: Option<TranscriptRecordSaveBounds>,
    pub(crate) final_len: usize,
}

impl SessionRecordSaveProjection {
    pub(crate) fn persisted_head(head: smelt_store::StoreHead) -> Self {
        Self {
            bounds: None,
            final_len: head
                .transcript_record_count
                .as_usize()
                .expect("persisted record count originated as usize"),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PreparedTranscriptRecordSuffix {
    pub(crate) start: smelt_store::TranscriptRecordIndex,
    pub(crate) records: Vec<smelt_core::TranscriptBlockRecordWithId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SessionBatchPreparationError {
    HydrationPending,
    Invalid(String),
}

impl From<String> for SessionBatchPreparationError {
    fn from(message: String) -> Self {
        Self::Invalid(message)
    }
}

impl std::fmt::Display for SessionBatchPreparationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HydrationPending => f.write_str("transcript hydration is pending"),
            Self::Invalid(message) => f.write_str(message),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PreparedSessionBatch {
    pub(crate) generation: PersistenceGeneration,
    pub(crate) record_projection: SessionRecordSaveProjection,
    pub(crate) identity: smelt_store::SessionIdentity,
    pub(crate) metadata: smelt_store::SessionMetadata,
    pub(crate) history: smelt_store::HistorySuffix,
    pub(crate) side_tables: smelt_store::SideTableSuffixes,
    pub(crate) records: Option<PreparedTranscriptRecordSuffix>,
}

impl PreparedSessionBatch {
    pub(crate) fn to_store_commit(
        &self,
        session_id: String,
        expected: smelt_store::StoreHead,
    ) -> Result<smelt_store::SessionCommit, smelt_store::StoreError> {
        let transcript_records = self
            .records
            .as_ref()
            .map(|records| {
                let start = records.start.as_usize().ok_or_else(|| {
                    smelt_store::StoreError::Integrity(
                        "prepared record start exceeds platform limits".into(),
                    )
                })?;
                let rows = records
                    .records
                    .iter()
                    .enumerate()
                    .map(|(offset, record)| {
                        transcript_record_row(start + offset, record, &self.history)
                    })
                    .collect::<Result<Vec<_>, smelt_store::StoreError>>()?;
                Ok::<_, smelt_store::StoreError>(smelt_store::TranscriptRecordSuffix {
                    start: records.start,
                    records: rows,
                })
            })
            .transpose()?;
        Ok(smelt_store::SessionCommit {
            session_id,
            expected,
            identity: self.identity.clone(),
            metadata: self.metadata.clone(),
            history: self.history.clone(),
            side_tables: self.side_tables.clone(),
            transcript_records,
        })
    }
}

pub(crate) struct SessionDocument {
    session: Session,
    transcript: crate::app::transcript::LoadedTranscript,
    live_session: Option<smelt_core::session_runtime::LiveSession>,
    store_head: Option<smelt_store::StoreHead>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChangeTracking {
    Enabled,
    Disabled,
}

pub(crate) struct TuiSessionDocument {
    pub(crate) transcript: TranscriptDocument,
    pub(crate) live_session: Option<LiveSession>,
    changes: DocumentChanges,
    persistence_epoch: Option<SessionEpoch>,
    change_tracking: ChangeTracking,
}

impl TuiSessionDocument {
    pub(crate) fn new(transcript: TranscriptDocument) -> Self {
        Self {
            transcript,
            live_session: None,
            changes: DocumentChanges::default(),
            persistence_epoch: None,
            change_tracking: ChangeTracking::Enabled,
        }
    }

    fn reserved_generation(&self) -> Option<PersistenceGeneration> {
        (self.change_tracking == ChangeTracking::Enabled).then(|| self.changes.reserve_generation())
    }

    fn record_change(
        &mut self,
        generation: Option<PersistenceGeneration>,
        change: &DocumentChange,
    ) {
        if let Some(generation) = generation {
            self.changes.record(generation, change);
        }
    }

    pub(super) fn enable_change_tracking(&mut self) {
        self.change_tracking = ChangeTracking::Enabled;
    }

    pub(super) fn disable_change_tracking(&mut self) {
        self.change_tracking = ChangeTracking::Disabled;
    }

    pub(super) fn apply_history(
        &mut self,
        session: &mut Session,
        mutation: HistoryMutation,
    ) -> DocumentChange {
        let generation = self.reserved_generation();
        let change = SessionDocument::apply_history(
            session,
            self.live_session.as_mut(),
            Some(&mut self.transcript),
            mutation,
        );
        self.record_change(generation, &change);
        change
    }

    pub(super) fn apply_transcript(&mut self, mutation: TranscriptMutation) -> DocumentChange {
        let generation = self.reserved_generation();
        let change = SessionDocument::apply_to_transcript(&mut self.transcript, mutation);
        self.record_change(generation, &change);
        change
    }

    pub(super) fn apply_stream(
        &mut self,
        parser: &mut StreamParser,
        mutation: StreamMutation,
    ) -> DocumentChange {
        let generation = self.reserved_generation();
        let change = SessionDocument::apply_stream(parser, &mut self.transcript, mutation);
        self.record_change(generation, &change);
        change
    }

    pub(super) fn apply_usage(
        &mut self,
        session: &mut Session,
        mutation: UsageMutation,
    ) -> DocumentChange {
        let generation = self.reserved_generation();
        let change = SessionDocument::apply_usage(session, mutation);
        self.record_change(generation, &change);
        change
    }

    pub(super) fn clear_token_baseline_for_loaded_model(
        &mut self,
        session: &mut Session,
        identity: ContextTokenIdentity,
    ) {
        SessionDocument::apply_usage(
            session,
            UsageMutation::ClearTokenBaselineIfMismatched { identity },
        );
    }

    pub(super) fn apply_metadata(
        &mut self,
        session: &mut Session,
        mutation: MetadataMutation,
    ) -> DocumentChange {
        let generation = self.reserved_generation();
        let change = SessionDocument::apply_metadata(session, mutation);
        self.record_change(generation, &change);
        change
    }

    pub(super) fn apply_turn_state(
        &mut self,
        session: &mut Session,
        mutation: TurnStateMutation,
    ) -> DocumentChange {
        let generation = self.reserved_generation();
        let change = SessionDocument::apply_turn_state(session, mutation);
        self.record_change(generation, &change);
        change
    }

    pub(crate) fn has_session_work(&self) -> bool {
        self.changes.current > self.changes.durable
    }

    pub(crate) fn generation(&self) -> PersistenceGeneration {
        self.changes.current
    }

    pub(crate) fn durable_generation(&self) -> PersistenceGeneration {
        self.changes.durable
    }

    pub(crate) fn records_persisted(&self) -> bool {
        self.transcript.history().record_dirty_from().is_none()
            && self.changes.current == self.changes.durable
    }

    pub(crate) fn acknowledged_head(&self) -> smelt_store::StoreHead {
        self.changes.acknowledged_head
    }

    pub(crate) fn bind_persistence(&mut self, epoch: SessionEpoch) {
        self.persistence_epoch = Some(epoch);
    }

    pub(crate) fn unbind_persistence(&mut self, epoch: SessionEpoch) {
        if self.persistence_epoch == Some(epoch) {
            self.persistence_epoch = None;
        }
    }

    pub(crate) fn mark_session_unpersisted(&mut self) {
        self.persistence_epoch = None;
        self.changes.install_head(smelt_store::StoreHead::default());
        self.changes.force_dirty_from(0);
        self.transcript.history_mut().require_record_resave_from(0);
    }

    pub(crate) fn install_materialized_session(&mut self, records_persisted: bool) {
        if records_persisted {
            self.transcript.history_mut().clear_record_dirty();
        } else {
            if self.transcript.history().record_dirty_from().is_none() {
                self.changes.force_dirty();
            }
            self.transcript.history_mut().require_record_resave_from(0);
        }
    }

    pub(crate) fn install_rewind_prefix(
        &mut self,
        transcript: crate::app::transcript::LoadedTranscript,
        record_count: usize,
    ) {
        self.transcript
            .install_rewind_prefix(transcript, record_count);
    }

    pub(crate) fn install_loaded_full_session(
        &mut self,
        transcript: crate::app::transcript::LoadedTranscript,
        store_head: Option<smelt_store::StoreHead>,
    ) {
        self.live_session = None;
        self.transcript.replace_loaded_transcript(transcript);
        self.changes.install_head(store_head.unwrap_or_default());
        self.persistence_epoch = None;
        self.changes.force_dirty_from(0);
        self.transcript.history_mut().require_record_resave_from(0);
    }

    pub(crate) fn install_loaded_store_session(
        &mut self,
        transcript: crate::app::transcript::LoadedTranscript,
        live_session: LiveSession,
        store_head: smelt_store::StoreHead,
    ) {
        debug_assert_eq!(
            store_head.history_len.as_usize(),
            Some(live_session.history_len())
        );
        self.live_session = Some(live_session);
        self.transcript.replace_loaded_transcript(transcript);
        self.changes.install_head(store_head);
        self.persistence_epoch = None;
    }

    #[cfg(test)]
    pub(crate) fn set_history_resave_from_for_test(&mut self, history_index: usize) {
        self.changes.force_dirty_from(history_index);
    }

    #[cfg(test)]
    pub(crate) fn dirty_history_from_for_test(&self) -> Option<usize> {
        self.changes.history_dirty_from
    }

    pub(crate) fn prepare_event_batch(
        &mut self,
        session: &mut Session,
        metadata: RuntimeSessionMetadata,
    ) -> Result<Option<PreparedSessionBatch>, SessionBatchPreparationError> {
        self.prepare_save_with_history(session, metadata, None)
    }

    pub(crate) fn prepare_fork_save(
        &mut self,
        session: &mut Session,
        metadata: RuntimeSessionMetadata,
        source_history: &[HistoryItem],
    ) -> Result<Option<PreparedSessionBatch>, SessionBatchPreparationError> {
        self.prepare_save_with_history(session, metadata, Some(source_history))
    }

    fn prepare_save_with_history(
        &mut self,
        session: &mut Session,
        metadata: RuntimeSessionMetadata,
        source_history: Option<&[HistoryItem]>,
    ) -> Result<Option<PreparedSessionBatch>, SessionBatchPreparationError> {
        let history = SessionHistoryRef::for_save(
            self.live_session.as_ref(),
            &session.history,
            source_history,
        );
        let history_len = history.len();
        let record_dirty = self.transcript.history().record_dirty_from().is_some();
        if self.changes.current == self.changes.durable && !record_dirty {
            return Ok(None);
        }
        if self.changes.acknowledged_head == smelt_store::StoreHead::default()
            && history_len == 0
            && self.transcript.is_empty()
        {
            return Ok(None);
        }

        self.apply_runtime_metadata(session, metadata);
        let history = SessionHistoryRef::for_save(
            self.live_session.as_ref(),
            &session.history,
            source_history,
        );
        let history_len = history.len();
        let acknowledged_history_len = self
            .changes
            .acknowledged_head
            .history_len
            .as_usize()
            .ok_or_else(|| "acknowledged history length exceeds platform limits".to_string())?;
        let acknowledged_record_len = self
            .changes
            .acknowledged_head
            .transcript_record_count
            .as_usize()
            .ok_or_else(|| "acknowledged record length exceeds platform limits".to_string())?;
        let history_start = self
            .changes
            .history_dirty_from
            .map_or(acknowledged_history_len, |dirty| {
                dirty.min(acknowledged_history_len)
            })
            .min(history_len);
        let history_items = history.range(history_start..history_len)?;
        let history = smelt_store::HistorySuffix {
            start: smelt_store::HistoryIndex::new(history_start as u64),
            final_len: smelt_store::HistoryLen::new(history_len as u64),
            items: history_items,
        };
        let identity = smelt_core::session::store_identity_from_session(session)
            .map_err(|error| error.to_string())?;
        let metadata = smelt_core::session::store_metadata_from_session(session, history_len)
            .map_err(|error| error.to_string())?;
        let side_tables = smelt_core::session::store_side_table_suffixes_from_session_at(
            session,
            history_start,
            history_len,
        )
        .map_err(|error| error.to_string())?;
        let record_bounds = self.transcript.record_save_bounds_at_or_before(
            self.changes.history_dirty_from,
            acknowledged_record_len,
        )?;
        let record_projection = SessionRecordSaveProjection {
            bounds: record_bounds,
            final_len: record_bounds
                .map_or(acknowledged_record_len, |bounds| bounds.record_end_idx),
        };
        self.transcript
            .pin_record_suffix_for_save(record_bounds)
            .map_err(|error| match error {
                TranscriptSaveHydrationError::Pending => {
                    SessionBatchPreparationError::HydrationPending
                }
                TranscriptSaveHydrationError::Failed => SessionBatchPreparationError::Invalid(
                    "hydrate canonical transcript record suffix".to_owned(),
                ),
            })?;
        let records = record_bounds.map(|bounds| PreparedTranscriptRecordSuffix {
            start: smelt_store::TranscriptRecordIndex::new(bounds.record_start_idx as u64),
            records: self
                .transcript
                .history()
                .block_records_with_ids_from(bounds.order_start),
        });
        self.transcript.unpin_record_suffix_after_save();
        Ok(Some(PreparedSessionBatch {
            generation: self.changes.current,
            record_projection,
            identity,
            metadata,
            history,
            side_tables,
            records,
        }))
    }

    pub(crate) fn prepare_turn_batch(
        &mut self,
        session: &mut Session,
        metadata: RuntimeSessionMetadata,
    ) -> Result<PreparedSessionBatch, SessionBatchPreparationError> {
        if self.changes.current == self.changes.durable
            && self.transcript.history().record_dirty_from().is_none()
        {
            self.changes.force_dirty();
        }
        self.prepare_event_batch(session, metadata)?.ok_or_else(|| {
            SessionBatchPreparationError::Invalid(
                "a canonical turn update requires persisted session history".to_string(),
            )
        })
    }

    pub(crate) fn acknowledge(
        &mut self,
        acknowledgement: &crate::persist::PersistenceAcknowledgement,
        session_id: &str,
        history_len: usize,
        checkpoint: Option<&ContextCheckpoint>,
    ) -> bool {
        self.acknowledge_from(acknowledgement, false, session_id, history_len, checkpoint)
    }

    pub(crate) fn acknowledge_coalesced_batch(
        &mut self,
        acknowledgement: &crate::persist::PersistenceAcknowledgement,
        session_id: &str,
        history_len: usize,
        checkpoint: Option<&ContextCheckpoint>,
    ) -> bool {
        self.acknowledge_from(acknowledgement, true, session_id, history_len, checkpoint)
    }

    fn acknowledge_from(
        &mut self,
        acknowledgement: &crate::persist::PersistenceAcknowledgement,
        coalesced: bool,
        session_id: &str,
        history_len: usize,
        checkpoint: Option<&ContextCheckpoint>,
    ) -> bool {
        if self.persistence_epoch != Some(acknowledgement.epoch)
            || acknowledgement.generation != self.changes.current
        {
            return false;
        }
        let record_projection = acknowledgement.record_projection;
        let previous = acknowledgement.previous;
        let receipt = &acknowledgement.receipt;
        let expected_record_len = record_projection.final_len;
        let revision_advanced_once = receipt
            .previous
            .revision
            .checked_add(1)
            .is_some_and(|revision| revision == receipt.current.revision);
        let revision_valid = if coalesced {
            receipt.previous.revision.get() >= previous.revision.get()
                && receipt.current.revision.get() >= receipt.previous.revision.get()
        } else {
            receipt.previous == previous
                && (receipt.current.revision == receipt.previous.revision || revision_advanced_once)
        };
        if receipt.session_id != session_id
            || receipt.current.history_len.as_usize() != Some(history_len)
            || receipt.current.transcript_record_count.as_usize() != Some(expected_record_len)
            || previous != self.changes.acknowledged_head
            || !revision_valid
        {
            return false;
        }
        if let Some(live_session) = self.live_session.as_mut() {
            live_session.compact_saved_prefix(
                history_len,
                receipt.current.revision.get(),
                checkpoint,
            );
        }
        if let Some(live_session) = self.live_session.as_ref() {
            self.transcript
                .set_store_address(live_session.store_address.as_ref().map(|store| {
                    super::transcript::TranscriptStoreAddress::new(
                        store.sessions_root.clone(),
                        store.session_id.clone(),
                        store.lineage_id.clone(),
                    )
                }));
        }
        if let Some(bounds) = record_projection.bounds {
            self.transcript
                .apply_persisted_record_suffix(bounds, expected_record_len);
        }
        self.transcript
            .schedule_durable_compaction(expected_record_len, record_projection.bounds);
        self.transcript.history_mut().clear_record_dirty();
        self.changes.mark_clean(receipt.current);
        true
    }

    pub(crate) fn mark_ephemeral_persisted(&mut self) {
        self.changes.durable = self.changes.current;
        self.changes.history_dirty_from = None;
        self.transcript.history_mut().clear_record_dirty();
    }

    pub(crate) fn has_unflushed_work(&self, _session: &Session) -> bool {
        self.changes.current > self.changes.durable
    }

    fn apply_runtime_metadata(&mut self, session: &mut Session, metadata: RuntimeSessionMetadata) {
        let generation = self.changes.reserve_generation();
        let result = SessionDocument::apply_metadata(
            session,
            MetadataMutation::UpdateRuntime {
                updated_at_ms: metadata.updated_at_ms,
                mode: metadata.mode,
                reasoning_effort: metadata.reasoning_effort,
                model: metadata.model,
                fast_mode: metadata.fast_mode,
            },
        );
        self.changes.record(generation, &result);
    }
}

pub(crate) struct FullSessionDocument {
    pub(crate) session: Session,
    pub(crate) transcript: crate::app::transcript::LoadedTranscript,
}

pub(crate) struct StoreBackedSessionDocument {
    pub(crate) session: Session,
    pub(crate) transcript: crate::app::transcript::LoadedTranscript,
    pub(crate) live_session: smelt_core::session_runtime::LiveSession,
    pub(crate) store_head: smelt_store::StoreHead,
}

#[derive(Clone, Copy)]
enum SessionHistoryRef<'a> {
    Materialized(&'a [HistoryItem]),
    StoreBacked(&'a LiveSession),
}

impl<'a> SessionHistoryRef<'a> {
    fn for_save(
        live_session: Option<&'a LiveSession>,
        session_history: &'a [HistoryItem],
        source_history: Option<&'a [HistoryItem]>,
    ) -> Self {
        live_session.map_or_else(
            || Self::Materialized(source_history.unwrap_or(session_history)),
            Self::StoreBacked,
        )
    }

    fn len(self) -> usize {
        match self {
            SessionHistoryRef::Materialized(history) => history.len(),
            SessionHistoryRef::StoreBacked(live) => live.history_len(),
        }
    }

    fn range(self, range: std::ops::Range<usize>) -> Result<Vec<HistoryItem>, String> {
        match self {
            SessionHistoryRef::Materialized(history) => {
                let end = range.end.min(history.len());
                let start = range.start.min(end);
                Ok(history[start..end].to_vec())
            }
            SessionHistoryRef::StoreBacked(live) => live.history_range(range),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RuntimeSessionMetadata {
    pub(crate) updated_at_ms: u64,
    pub(crate) mode: String,
    pub(crate) reasoning_effort: ReasoningEffort,
    pub(crate) model: Option<String>,
    pub(crate) fast_mode: bool,
}

pub(crate) enum HistoryMutation {
    AppendItem {
        item: HistoryItem,
    },
    CommitRequestItem {
        item: HistoryItem,
        block: Option<Box<Block>>,
        first_user_message: Option<String>,
    },
    TruncateFrom {
        index: usize,
        identity: ContextTokenIdentity,
    },
    RewindTo {
        index: usize,
        keep_checkpoint_at_boundary: bool,
        identity: ContextTokenIdentity,
    },
}

pub(crate) enum TranscriptMutation {
    AppendBlock {
        block: Block,
    },
    InsertCheckpointMarker {
        block_index: usize,
        history_index: usize,
        block: Block,
    },
    RemoveUnoriginatedBlockAt {
        block_index: usize,
    },
    ReplaceFromHistory {
        transcript: Box<smelt_core::content::transcript::Transcript>,
    },
    TruncateTo {
        block_index: usize,
    },
    Clear,
    UpdateCompactionPreview {
        summary: String,
    },
    ClearCompactionPreview,
    RewriteBlock {
        id: BlockId,
        block: Block,
    },
}

pub(crate) enum StreamMutation {
    AppendThinking {
        delta: String,
    },
    FlushThinking,
    AppendText {
        delta: String,
    },
    FlushText,
    SyncActiveToolElapsed {
        now: Instant,
    },
    StartTool {
        start: ToolStart,
        now: Instant,
    },
    AppendToolOutput {
        invocation_id: protocol::InvocationId,
        line: String,
    },
    AppendToolOutputSlice {
        block_id: BlockId,
        chunk: smelt_core::transcript_content::SharedContentSlice,
        line_start: bool,
    },
    SetToolStatus {
        invocation_id: protocol::InvocationId,
        status: ToolStatus,
        now: Instant,
    },
    SetToolUserMessage {
        invocation_id: protocol::InvocationId,
        message: String,
    },
    FinishTool {
        invocation_id: protocol::InvocationId,
        status: ToolStatus,
        output: Option<ToolOutputRef>,
        engine_elapsed: Option<Duration>,
        now: Instant,
    },
    FinalizeTools,
    PromoteToolDraft {
        stream_id: Option<String>,
        start: ToolStart,
        now: Instant,
    },
    ClearToolDrafts,
    StartToolDraft {
        stream_id: String,
        call_id: Option<String>,
        name: Option<String>,
    },
    AppendToolDraft {
        stream_id: String,
        call_id: Option<String>,
        name: Option<String>,
        delta: String,
    },
    FinishToolDraft {
        stream_id: String,
        call_id: String,
        name: String,
        arguments: String,
    },
    SetToolDraftSummary {
        block_id: BlockId,
        summary: protocol::StyledLines,
    },
    StartExec {
        command: String,
    },
    AppendExecOutput {
        chunk: String,
    },
    FinishExec {
        final_output: Option<String>,
    },
}

pub(crate) enum UsageMutation {
    RecordTokens {
        usage: TokenUsage,
        history_len: usize,
        identity: ContextTokenIdentity,
    },
    ClearTokenBaselineIfMismatched {
        identity: ContextTokenIdentity,
    },
    Accumulate {
        usage: TokenUsage,
        cost_usd: f64,
    },
}

pub(crate) enum MetadataMutation {
    SetTitle {
        title: String,
        slug: String,
        snapshot_history_len: usize,
    },
    UpdateRuntime {
        updated_at_ms: u64,
        mode: String,
        reasoning_effort: ReasoningEffort,
        model: Option<String>,
        fast_mode: bool,
    },
    SetFastMode {
        enabled: bool,
    },
    SetCwd {
        cwd: String,
    },
    RestoreAfterRewind {
        history_len: usize,
    },
}

pub(crate) enum TurnStateMutation {
    Finish {
        history_len: usize,
        meta: TurnMeta,
        update_context_token_history_len: bool,
    },
    InstallCheckpoint {
        kind: String,
        summary: String,
        first_live_message_index: usize,
        tokens_before: Option<u32>,
    },
    InstallCheckpointAtHistoryIndex {
        kind: String,
        summary: String,
        first_live_index: usize,
        tokens_before: Option<u32>,
        history_len: usize,
    },
    SetCheckpoint {
        checkpoint: Option<ContextCheckpoint>,
    },
    SetCheckpointTokensAfterEstimate {
        tokens: u32,
        history_len: usize,
    },
    PruneRewindable {
        history_len: usize,
        identity: ContextTokenIdentity,
    },
}

#[derive(Default)]
pub(crate) struct DocumentChange {
    pub(crate) session_dirty: bool,
    #[allow(dead_code)]
    pub(crate) transcript_dirty: bool,
    pub(crate) records_unpersisted: bool,
    pub(crate) context_tokens_updated: bool,
    pub(crate) applied: bool,
    pub(crate) history_idx: Option<usize>,
    pub(crate) history_dirty_from: Option<usize>,
    pub(crate) block_id: Option<BlockId>,
    pub(crate) presentation_changed: bool,
    pub(crate) turn_meta: Option<TurnMeta>,
}

impl DocumentChange {
    fn canonical_changed(&self) -> bool {
        self.session_dirty
            || self.history_dirty_from.is_some()
            || self.transcript_dirty
            || self.records_unpersisted
    }
}

impl FullSessionDocument {
    pub(crate) fn new(
        session: Session,
        transcript: crate::app::transcript::LoadedTranscript,
    ) -> Self {
        Self {
            session,
            transcript,
        }
    }
}

impl StoreBackedSessionDocument {
    pub(crate) fn new(
        session: Session,
        transcript: crate::app::transcript::LoadedTranscript,
        live_session: smelt_core::session_runtime::LiveSession,
        store_head: smelt_store::StoreHead,
    ) -> Self {
        Self {
            session,
            transcript,
            live_session,
            store_head,
        }
    }
}

impl SessionDocument {
    pub(crate) fn from_full_session(
        session: Session,
        transcript: crate::app::transcript::LoadedTranscript,
    ) -> Self {
        Self {
            session,
            transcript,
            live_session: None,
            store_head: None,
        }
    }

    pub(crate) fn from_store(
        header: SessionHeader,
        session: Session,
        store_address: SessionStoreAddress,
        store_head: smelt_store::StoreHead,
        transcript: crate::app::transcript::LoadedTranscript,
    ) -> Self {
        let live_session =
            smelt_core::session_runtime::LiveSession::from_store(header, store_address);
        Self {
            session,
            transcript,
            live_session: Some(live_session),
            store_head: Some(store_head),
        }
    }

    fn append_history_item_with_transcript_block(
        session: &mut Session,
        live_session: Option<&mut LiveSession>,
        transcript: &mut TranscriptDocument,
        item: HistoryItem,
        block: Option<Block>,
        first_user_message: Option<String>,
    ) -> DocumentChange {
        let before_generation = transcript.history().record_dirty_generation();
        let before_dirty_from = transcript.history().record_dirty_from();
        let idx = live_session
            .as_ref()
            .map_or(session.history.len(), |live| live.history_len());
        if let Some(message) = first_user_message {
            if session.first_user_message.is_none() {
                session.first_user_message = Some(message);
                session.snapshot_metadata_at(idx + 1);
            }
        }
        if let Some(block) = block {
            transcript.push_with_origin(block, BlockOrigin::History(idx));
        }
        if let Some(live_session) = live_session {
            let appended_idx = live_session.append_history(item);
            debug_assert_eq!(appended_idx, idx);
        } else {
            session.history.push(item);
        }
        let history = transcript.history();
        DocumentChange {
            session_dirty: true,
            transcript_dirty: history.record_dirty_generation() != before_generation
                || history.record_dirty_from() != before_dirty_from,
            applied: true,
            history_idx: Some(idx),
            history_dirty_from: Some(idx),
            ..Default::default()
        }
    }

    fn apply_history(
        session: &mut Session,
        mut live_session: Option<&mut LiveSession>,
        transcript: Option<&mut TranscriptDocument>,
        mutation: HistoryMutation,
    ) -> DocumentChange {
        let rewrites_history = matches!(
            mutation,
            HistoryMutation::TruncateFrom { .. } | HistoryMutation::RewindTo { .. }
        );
        let before_session = rewrites_history.then(|| session.clone());
        let before_live = rewrites_history
            .then(|| {
                live_session
                    .as_deref()
                    .map(|live| (live.live_start, live.live_history.len()))
            })
            .flatten();
        let mut result = match mutation {
            HistoryMutation::AppendItem { item } => {
                let idx = if let Some(live_session) = live_session.as_deref_mut() {
                    live_session.append_history(item)
                } else {
                    let idx = session.history.len();
                    session.history.push(item);
                    idx
                };
                DocumentChange {
                    session_dirty: true,
                    history_idx: Some(idx),
                    history_dirty_from: Some(idx),
                    ..Default::default()
                }
            }
            HistoryMutation::CommitRequestItem {
                item,
                block,
                first_user_message,
            } => Self::append_history_item_with_transcript_block(
                session,
                live_session.as_deref_mut(),
                transcript.expect("request history mutation requires a transcript"),
                item,
                block.map(|block| *block),
                first_user_message,
            ),
            HistoryMutation::TruncateFrom { index, identity } => {
                let dirty_from = if let Some(live_session) = live_session.as_deref_mut() {
                    let index = index.min(live_session.history_len());
                    live_session.truncate_from(index);
                    index
                } else {
                    let index = index.min(session.history.len());
                    session.history.truncate(index);
                    index
                };
                let turn_meta = session.prune_rewindable_snapshots(dirty_from);
                session.clear_context_tokens_baseline_if_mismatched(&identity);
                DocumentChange {
                    session_dirty: true,
                    history_dirty_from: Some(dirty_from),
                    turn_meta,
                    ..Default::default()
                }
            }
            HistoryMutation::RewindTo {
                index,
                keep_checkpoint_at_boundary,
                identity,
            } => {
                let dirty_from = if let Some(live_session) = live_session.as_deref_mut() {
                    let index = index.min(live_session.history_len());
                    live_session.truncate_from(index);
                    index
                } else {
                    let index = index.min(session.history.len());
                    session.history.truncate(index);
                    index
                };
                let turn_meta = session.restore_rewindable_snapshots_after_rewind(
                    dirty_from,
                    keep_checkpoint_at_boundary,
                );
                session.clear_context_tokens_baseline_if_mismatched(&identity);
                DocumentChange {
                    session_dirty: true,
                    history_dirty_from: Some(dirty_from),
                    turn_meta,
                    ..Default::default()
                }
            }
        };
        if before_session
            .as_ref()
            .is_some_and(|before| before == session)
            && before_live
                == live_session
                    .as_deref()
                    .map(|live| (live.live_start, live.live_history.len()))
        {
            result.session_dirty = false;
            result.history_dirty_from = None;
        }
        result
    }

    fn apply_usage(session: &mut Session, mutation: UsageMutation) -> DocumentChange {
        match mutation {
            UsageMutation::RecordTokens {
                usage,
                history_len,
                identity,
            } => {
                let Some(tokens) = usage.context_tokens.or(usage.prompt_tokens) else {
                    return DocumentChange::default();
                };
                if tokens == 0 {
                    return DocumentChange::default();
                }
                let changed = session.context_tokens != Some(tokens)
                    || session.context_tokens_history_len != Some(history_len)
                    || session.context_token_identity.as_ref() != Some(&identity)
                    || session.display_context_tokens != Some(tokens)
                    || session.display_context_token_identity.as_ref() != Some(&identity);
                session.record_context_tokens(tokens, history_len, identity);
                DocumentChange {
                    session_dirty: changed,
                    context_tokens_updated: changed,
                    ..Default::default()
                }
            }
            UsageMutation::ClearTokenBaselineIfMismatched { identity } => {
                let changed = session
                    .context_token_identity
                    .as_ref()
                    .is_some_and(|current| current != &identity);
                session.clear_context_tokens_baseline_if_mismatched(&identity);
                DocumentChange {
                    session_dirty: changed,
                    ..Default::default()
                }
            }
            UsageMutation::Accumulate { usage, cost_usd } => {
                let before_cost = session.session_cost_usd;
                let before_usage = session.session_usage.clone();
                session.session_cost_usd += cost_usd;
                session.session_usage.accumulate(&usage);
                DocumentChange {
                    session_dirty: session.session_cost_usd != before_cost
                        || session.session_usage != before_usage,
                    ..Default::default()
                }
            }
        }
    }

    fn apply_metadata(session: &mut Session, mutation: MetadataMutation) -> DocumentChange {
        match mutation {
            MetadataMutation::SetTitle {
                title,
                slug,
                snapshot_history_len,
            } => {
                let before_title = session.title.clone();
                let before_slug = session.slug.clone();
                let before_snapshots = session.metadata_snapshots.clone();
                session.title = Some(title);
                session.slug = Some(slug);
                session.snapshot_metadata_at(snapshot_history_len);
                DocumentChange {
                    session_dirty: session.title != before_title
                        || session.slug != before_slug
                        || session.metadata_snapshots != before_snapshots,
                    ..Default::default()
                }
            }
            MetadataMutation::UpdateRuntime {
                updated_at_ms,
                mode,
                reasoning_effort,
                model,
                fast_mode,
            } => {
                let changed = session.updated_at_ms != updated_at_ms
                    || session.mode.as_deref() != Some(mode.as_str())
                    || session.reasoning_effort.as_ref() != Some(&reasoning_effort)
                    || session.model != model
                    || session.fast_mode != Some(fast_mode);
                session.updated_at_ms = updated_at_ms;
                session.mode = Some(mode);
                session.reasoning_effort = Some(reasoning_effort);
                session.model = model;
                session.fast_mode = Some(fast_mode);
                DocumentChange {
                    session_dirty: changed,
                    ..Default::default()
                }
            }
            MetadataMutation::SetFastMode { enabled } => {
                let changed = session.fast_mode != Some(enabled);
                session.fast_mode = Some(enabled);
                DocumentChange {
                    session_dirty: changed,
                    ..Default::default()
                }
            }
            MetadataMutation::SetCwd { cwd } => {
                let changed = session.cwd.as_deref() != Some(cwd.as_str());
                session.cwd = Some(cwd);
                DocumentChange {
                    session_dirty: changed,
                    ..Default::default()
                }
            }
            MetadataMutation::RestoreAfterRewind { history_len } => {
                let before_title = session.title.clone();
                let before_slug = session.slug.clone();
                let before_first_user_message = session.first_user_message.clone();
                let before_snapshots = session.metadata_snapshots.clone();
                session.restore_metadata_after_rewind(history_len);
                DocumentChange {
                    session_dirty: session.title != before_title
                        || session.slug != before_slug
                        || session.first_user_message != before_first_user_message
                        || session.metadata_snapshots != before_snapshots,
                    ..Default::default()
                }
            }
        }
    }

    fn apply_turn_state(session: &mut Session, mutation: TurnStateMutation) -> DocumentChange {
        match mutation {
            TurnStateMutation::Finish {
                history_len,
                meta,
                update_context_token_history_len,
            } => {
                let before_turn_metas = session.turn_metas.clone();
                let before_context_snapshots = session.context_snapshots.clone();
                let before_context_tokens_history_len = session.context_tokens_history_len;
                session.finish_turn_state(history_len, meta, update_context_token_history_len);
                let changed = session.turn_metas != before_turn_metas
                    || session.context_snapshots != before_context_snapshots
                    || session.context_tokens_history_len != before_context_tokens_history_len;
                DocumentChange {
                    session_dirty: changed,
                    applied: changed,
                    ..Default::default()
                }
            }
            TurnStateMutation::InstallCheckpoint {
                kind,
                summary,
                first_live_message_index,
                tokens_before,
            } => {
                let installed = session.install_context_checkpoint(
                    kind,
                    summary,
                    first_live_message_index,
                    tokens_before,
                );
                DocumentChange {
                    session_dirty: installed,
                    applied: installed,
                    ..Default::default()
                }
            }
            TurnStateMutation::InstallCheckpointAtHistoryIndex {
                kind,
                summary,
                first_live_index,
                tokens_before,
                history_len,
            } => {
                let installed = session.install_context_checkpoint_at_history_index(
                    kind,
                    summary,
                    first_live_index,
                    tokens_before,
                    history_len,
                );
                DocumentChange {
                    session_dirty: installed,
                    applied: installed,
                    ..Default::default()
                }
            }
            TurnStateMutation::SetCheckpoint { checkpoint } => {
                if session.checkpoint == checkpoint {
                    return DocumentChange::default();
                }
                session.checkpoint = checkpoint;
                DocumentChange {
                    session_dirty: true,
                    ..Default::default()
                }
            }
            TurnStateMutation::SetCheckpointTokensAfterEstimate {
                tokens,
                history_len,
            } => {
                let changed = session.record_checkpoint_tokens_after_estimate(tokens, history_len);
                DocumentChange {
                    session_dirty: changed,
                    applied: changed,
                    ..Default::default()
                }
            }
            TurnStateMutation::PruneRewindable {
                history_len,
                identity,
            } => {
                let before_turn_metas = session.turn_metas.len();
                let before_context_snapshots = session.context_snapshots.len();
                let before_metadata_snapshots = session.metadata_snapshots.len();
                let before_context_tokens = session.context_tokens;
                let before_context_token_identity = session.context_token_identity.clone();
                let turn_meta = session.prune_rewindable_snapshots(history_len);
                session.clear_context_tokens_baseline_if_mismatched(&identity);
                let changed = session.turn_metas.len() != before_turn_metas
                    || session.context_snapshots.len() != before_context_snapshots
                    || session.metadata_snapshots.len() != before_metadata_snapshots
                    || session.context_tokens != before_context_tokens
                    || session.context_token_identity != before_context_token_identity;
                DocumentChange {
                    session_dirty: changed,
                    turn_meta,
                    ..Default::default()
                }
            }
        }
    }

    fn apply_stream(
        parser: &mut StreamParser,
        transcript: &mut TranscriptDocument,
        mutation: StreamMutation,
    ) -> DocumentChange {
        let before_generation = transcript.history().record_dirty_generation();
        let before_dirty_from = transcript.history().record_dirty_from();
        let mut applied = false;
        let mut block_id = None;
        let mut presentation_changed = false;
        match mutation {
            StreamMutation::AppendThinking { delta } => {
                parser.append_streaming_thinking(transcript.history_mut(), &delta);
            }
            StreamMutation::FlushThinking => {
                parser.flush_streaming_thinking(transcript.history_mut());
            }
            StreamMutation::AppendText { delta } => {
                parser.append_streaming_text(transcript.history_mut(), &delta);
            }
            StreamMutation::FlushText => {
                parser.flush_streaming_text(transcript.history_mut());
            }
            StreamMutation::SyncActiveToolElapsed { now } => {
                parser.sync_active_tool_elapsed_at(transcript.history_mut(), now);
            }
            StreamMutation::StartTool { start, now } => {
                parser.start_tool(transcript.history_mut(), start, now);
            }
            StreamMutation::AppendToolOutput {
                invocation_id,
                line,
            } => {
                parser.append_active_output_line(transcript.history_mut(), invocation_id, line);
            }
            StreamMutation::AppendToolOutputSlice {
                block_id,
                chunk,
                line_start,
            } => {
                parser.append_tool_output_slice(
                    transcript.history_mut(),
                    block_id,
                    chunk,
                    line_start,
                );
            }
            StreamMutation::SetToolStatus {
                invocation_id,
                status,
                now,
            } => {
                parser.set_active_status(transcript.history_mut(), invocation_id, status, now);
            }
            StreamMutation::SetToolUserMessage {
                invocation_id,
                message,
            } => {
                parser.set_active_user_message(transcript.history_mut(), invocation_id, message);
            }
            StreamMutation::FinishTool {
                invocation_id,
                status,
                output,
                engine_elapsed,
                now,
            } => {
                parser.finish_tool(
                    transcript.history_mut(),
                    invocation_id,
                    status,
                    output,
                    engine_elapsed,
                    now,
                );
            }
            StreamMutation::FinalizeTools => {
                parser.finalize_active_tools(transcript.history_mut());
            }
            StreamMutation::PromoteToolDraft {
                stream_id,
                start,
                now,
            } => {
                applied = parser.promote_tool_draft(
                    transcript.history_mut(),
                    stream_id.as_deref(),
                    start,
                    now,
                );
            }
            StreamMutation::ClearToolDrafts => {
                parser.clear_tool_drafts(transcript.history_mut());
            }
            StreamMutation::StartToolDraft {
                stream_id,
                call_id,
                name,
            } => {
                let change =
                    parser.start_tool_draft(transcript.history_mut(), stream_id, call_id, name);
                block_id = Some(change.block_id);
                presentation_changed = change.presentation_changed;
                applied = true;
            }
            StreamMutation::AppendToolDraft {
                stream_id,
                call_id,
                name,
                delta,
            } => {
                let change = parser.append_tool_draft(
                    transcript.history_mut(),
                    stream_id,
                    call_id,
                    name,
                    delta,
                );
                block_id = Some(change.block_id);
                presentation_changed = change.presentation_changed;
                applied = true;
            }
            StreamMutation::FinishToolDraft {
                stream_id,
                call_id,
                name,
                arguments,
            } => {
                let change = parser.finish_tool_draft(
                    transcript.history_mut(),
                    stream_id,
                    call_id,
                    name,
                    arguments,
                );
                block_id = Some(change.block_id);
                presentation_changed = change.presentation_changed;
                applied = true;
            }
            StreamMutation::SetToolDraftSummary {
                block_id: id,
                summary,
            } => {
                applied = parser.set_tool_draft_summary(transcript.history_mut(), id, summary);
                block_id = Some(id);
            }
            StreamMutation::StartExec { command } => {
                parser.start_exec(transcript.history_mut(), command);
            }
            StreamMutation::AppendExecOutput { chunk } => {
                parser.append_exec_output(transcript.history_mut(), chunk);
            }
            StreamMutation::FinishExec { final_output } => {
                parser.finish_exec(transcript.history_mut(), final_output);
            }
        }
        let history = transcript.history();
        let transcript_dirty = history.record_dirty_generation() != before_generation
            || history.record_dirty_from() != before_dirty_from;
        DocumentChange {
            transcript_dirty,
            records_unpersisted: transcript_dirty,
            applied,
            block_id,
            presentation_changed,
            ..Default::default()
        }
    }

    fn apply_to_transcript(
        transcript: &mut TranscriptDocument,
        mutation: TranscriptMutation,
    ) -> DocumentChange {
        let before_generation = transcript.history().record_dirty_generation();
        let before_dirty_from = transcript.history().record_dirty_from();
        let mut records_unpersisted = false;
        let mut block_id = None;
        let applied = match mutation {
            TranscriptMutation::AppendBlock { block } => {
                transcript.push(block);
                transcript.history().record_dirty_generation() != before_generation
                    || transcript.history().record_dirty_from() != before_dirty_from
            }
            TranscriptMutation::InsertCheckpointMarker {
                block_index,
                history_index,
                block,
            } => {
                transcript.insert_checkpoint_marker_at(block_index, history_index, block);
                transcript.history().record_dirty_generation() != before_generation
                    || transcript.history().record_dirty_from() != before_dirty_from
            }
            TranscriptMutation::RemoveUnoriginatedBlockAt { block_index } => {
                transcript.remove_unoriginated_at(block_index).is_some()
            }
            TranscriptMutation::ReplaceFromHistory {
                transcript: rebuilt,
            } => {
                transcript.replace_transcript(*rebuilt);
                transcript.history_mut().mark_changed();
                records_unpersisted = true;
                true
            }
            TranscriptMutation::TruncateTo { block_index } => {
                transcript.truncate_to(block_index);
                transcript.history().record_dirty_generation() != before_generation
                    || transcript.history().record_dirty_from() != before_dirty_from
            }
            TranscriptMutation::Clear => {
                transcript.clear();
                transcript.history().record_dirty_generation() != before_generation
                    || transcript.history().record_dirty_from() != before_dirty_from
            }
            TranscriptMutation::UpdateCompactionPreview { summary } => {
                block_id = transcript.set_compaction_preview(summary);
                block_id.is_some()
            }
            TranscriptMutation::ClearCompactionPreview => {
                block_id = transcript.clear_compaction_preview();
                block_id.is_some()
            }
            TranscriptMutation::RewriteBlock { id, block } => {
                transcript.history_mut().rewrite(id, block);
                true
            }
        };
        let history = transcript.history();
        DocumentChange {
            transcript_dirty: history.record_dirty_generation() != before_generation
                || history.record_dirty_from() != before_dirty_from,
            records_unpersisted,
            applied,
            block_id,
            ..Default::default()
        }
    }

    pub(crate) fn into_full(self) -> FullSessionDocument {
        FullSessionDocument::new(self.session, self.transcript)
    }

    pub(crate) fn into_store_backed(self) -> StoreBackedSessionDocument {
        StoreBackedSessionDocument::new(
            self.session,
            self.transcript,
            self.live_session
                .expect("store-backed session document includes live session state"),
            self.store_head
                .expect("store-backed session document includes a store head"),
        )
    }
}

fn transcript_record_row(
    record_idx: usize,
    record: &smelt_core::TranscriptBlockRecordWithId,
    history: &smelt_store::HistorySuffix,
) -> Result<smelt_store::StoredTranscriptBlock, smelt_store::StoreError> {
    let owned_record;
    let record_ref = match record.record.origin {
        Some(BlockOrigin::History(index))
            if !history_suffix_contains_matching_record_origin(history, index, record) =>
        {
            owned_record = smelt_core::TranscriptBlockRecord {
                origin: None,
                ..record.record.clone()
            };
            &owned_record
        }
        _ => &record.record,
    };
    smelt_core::transcript_model::transcript_block_row_with_block_idx(
        record_idx,
        record.block_id.get(),
        record_ref,
    )
}

fn history_suffix_contains_matching_record_origin(
    history: &smelt_store::HistorySuffix,
    history_idx: usize,
    record: &smelt_core::TranscriptBlockRecordWithId,
) -> bool {
    let history_start = history
        .start
        .as_usize()
        .expect("prepared history start originated as usize");
    let history_len = history
        .final_len
        .as_usize()
        .expect("prepared history length originated as usize");
    if history_idx >= history_len {
        return false;
    }
    if history_idx < history_start {
        return true;
    }
    history
        .items
        .get(history_idx - history_start)
        .is_some_and(|item| {
            protocol::transcript_block_kind_matches_history_item(record.record.block.kind(), item)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{Content, ReasoningEffort, StyledLines};
    use smelt_core::session::{
        AuthoritativeContextTokens, ContextTokenIdentity, DisplayContextTokens, SessionMeta,
    };
    use std::collections::HashMap;

    const INVOCATION_ID: protocol::InvocationId = protocol::InvocationId::new(1);

    fn token_identity() -> ContextTokenIdentity {
        ContextTokenIdentity {
            model: Some("model-a".into()),
            api_base: Some("https://api.example.test".into()),
            provider_type: Some("openai".into()),
        }
    }

    fn meta_with_token_identity() -> SessionMeta {
        let identity = token_identity();
        SessionMeta {
            id: "session-a".into(),
            title: Some("Title".into()),
            slug: Some("title".into()),
            first_user_message: Some("hello".into()),
            created_at_ms: 10,
            updated_at_ms: 20,
            mode: Some("agent".into()),
            reasoning_effort: Some(ReasoningEffort::Low),
            model: Some("model-a".into()),
            fast_mode: Some(true),
            cwd: Some("/tmp".into()),
            parent_id: Some("parent".into()),
            authoritative_context_tokens: Some(AuthoritativeContextTokens {
                tokens: 42,
                history_len: 3,
                identity: identity.clone(),
            }),
            display_context_tokens: Some(DisplayContextTokens {
                tokens: 42,
                identity: Some(identity),
            }),
            history_len: Some(3),
            checkpoint: None,
            checkpoint_events: Vec::new(),
            text_bytes: Some(128),
        }
    }

    fn empty_live_session_for(session: &Session, history_len: usize) -> LiveSession {
        let mut meta = meta_with_token_identity();
        meta.id = session.id.clone();
        meta.history_len = Some(history_len);
        let header = SessionHeader {
            meta,
            history_len,
            revision: 0,
            degraded_warnings: Vec::new(),
        };
        LiveSession::from_parts(header, std::path::PathBuf::new(), None)
    }

    fn apply_usage(session: &mut Session, mutation: UsageMutation) -> DocumentChange {
        SessionDocument::apply_usage(session, mutation)
    }

    fn apply_metadata(session: &mut Session, mutation: MetadataMutation) -> DocumentChange {
        SessionDocument::apply_metadata(session, mutation)
    }

    fn apply_turn_state(session: &mut Session, mutation: TurnStateMutation) -> DocumentChange {
        SessionDocument::apply_turn_state(session, mutation)
    }

    fn apply_history(session: &mut Session, mutation: HistoryMutation) -> DocumentChange {
        SessionDocument::apply_history(session, None, None, mutation)
    }

    fn apply_transcript(
        transcript: &mut TranscriptDocument,
        mutation: TranscriptMutation,
    ) -> DocumentChange {
        SessionDocument::apply_to_transcript(transcript, mutation)
    }

    fn apply_history_with_transcript(
        session: &mut Session,
        transcript: &mut TranscriptDocument,
        mutation: HistoryMutation,
    ) -> DocumentChange {
        SessionDocument::apply_history(session, None, Some(transcript), mutation)
    }

    fn apply_stream_parser_transcript(
        parser: &mut StreamParser,
        transcript: &mut TranscriptDocument,
        mutation: StreamMutation,
    ) -> DocumentChange {
        SessionDocument::apply_stream(parser, transcript, mutation)
    }

    #[test]
    fn full_session_load_constructs_document_before_install_projection() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        session.id = "full-document".into();
        session
            .history
            .push(HistoryItem::user(Content::text("hello")));
        let transcript = crate::app::transcript::LoadedTranscript::full(
            smelt_core::content::transcript::Transcript::new(),
        );

        let projection = SessionDocument::from_full_session(session, transcript).into_full();

        assert_eq!(projection.session.id, "full-document");
        assert_eq!(projection.session.history.len(), 1);
    }

    #[test]
    fn record_token_usage_mutation_updates_accounting_and_reports_dirty() {
        let identity = token_identity();
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));

        let result = apply_usage(
            &mut session,
            UsageMutation::RecordTokens {
                usage: TokenUsage {
                    context_tokens: Some(777),
                    ..Default::default()
                },
                history_len: 12,
                identity: identity.clone(),
            },
        );

        assert!(result.session_dirty);
        assert!(result.context_tokens_updated);
        assert_eq!(session.display_context_tokens(), Some(777));
        assert_eq!(session.context_tokens_history_len, Some(12));
        assert_eq!(session.context_token_identity, Some(identity.clone()));
        assert_eq!(session.display_context_token_identity, Some(identity));
    }

    #[test]
    fn zero_token_usage_mutation_is_noop() {
        let identity = token_identity();
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));

        let result = apply_usage(
            &mut session,
            UsageMutation::RecordTokens {
                usage: TokenUsage {
                    context_tokens: Some(0),
                    ..Default::default()
                },
                history_len: 0,
                identity,
            },
        );

        assert!(!result.session_dirty);
        assert!(!result.context_tokens_updated);
        assert_eq!(session.display_context_tokens(), None);
    }

    #[test]
    fn clear_context_tokens_baseline_mutation_reports_dirty_on_identity_mismatch() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        session.record_context_tokens(100, 0, token_identity());
        let mut new_identity = token_identity();
        new_identity.model = Some("model-b".into());

        let result = apply_usage(
            &mut session,
            UsageMutation::ClearTokenBaselineIfMismatched {
                identity: new_identity,
            },
        );

        assert!(result.session_dirty);
        assert_eq!(session.context_tokens, None);
        assert_eq!(session.context_token_identity, None);
    }

    #[test]
    fn accumulate_usage_mutation_updates_accounting_and_reports_dirty() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));

        let result = apply_usage(
            &mut session,
            UsageMutation::Accumulate {
                usage: TokenUsage {
                    prompt_tokens: Some(10),
                    completion_tokens: Some(5),
                    ..Default::default()
                },
                cost_usd: 0.25,
            },
        );

        assert!(result.session_dirty);
        assert_eq!(session.session_cost_usd, 0.25);
        assert_eq!(session.session_usage.prompt_tokens, Some(10));
        assert_eq!(session.session_usage.completion_tokens, Some(5));
    }

    #[test]
    fn commit_request_history_item_sets_first_user_snapshot_once() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        let mut transcript = TranscriptDocument::new();

        let result = apply_history_with_transcript(
            &mut session,
            &mut transcript,
            HistoryMutation::CommitRequestItem {
                item: HistoryItem::user(Content::text("hello")),
                block: None,
                first_user_message: Some("hello".into()),
            },
        );
        let second = apply_history_with_transcript(
            &mut session,
            &mut transcript,
            HistoryMutation::CommitRequestItem {
                item: HistoryItem::user(Content::text("second")),
                block: None,
                first_user_message: Some("ignored".into()),
            },
        );

        assert!(result.session_dirty);
        assert!(second.session_dirty);
        assert_eq!(session.first_user_message.as_deref(), Some("hello"));
        assert_eq!(
            session
                .metadata_snapshots
                .iter()
                .filter(|(idx, _)| *idx == 1)
                .count(),
            1
        );
    }

    #[test]
    fn title_mutation_updates_metadata_snapshot_and_reports_dirty() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));

        let result = apply_metadata(
            &mut session,
            MetadataMutation::SetTitle {
                title: "New title".into(),
                slug: "new-title".into(),
                snapshot_history_len: 2,
            },
        );

        assert!(result.session_dirty);
        assert_eq!(session.title.as_deref(), Some("New title"));
        assert_eq!(session.slug.as_deref(), Some("new-title"));
        assert!(session.metadata_snapshots.iter().any(|(idx, _)| *idx == 2));
    }

    #[test]
    fn restore_metadata_after_rewind_mutation_restores_snapshot() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        apply_metadata(
            &mut session,
            MetadataMutation::SetTitle {
                title: "First".into(),
                slug: "first".into(),
                snapshot_history_len: 1,
            },
        );
        apply_metadata(
            &mut session,
            MetadataMutation::SetTitle {
                title: "Second".into(),
                slug: "second".into(),
                snapshot_history_len: 2,
            },
        );

        let result = apply_metadata(
            &mut session,
            MetadataMutation::RestoreAfterRewind { history_len: 1 },
        );

        assert!(result.session_dirty);
        assert_eq!(session.title.as_deref(), Some("First"));
        assert_eq!(session.slug.as_deref(), Some("first"));
    }

    #[test]
    fn rewind_to_start_mutation_clears_history_snapshots_and_tokens() {
        let identity = token_identity();
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        session
            .history
            .push(HistoryItem::user(Content::text("prompt")));
        session.record_context_tokens(500, 1, identity.clone());
        session.snapshot_context_at(1);
        apply_metadata(
            &mut session,
            MetadataMutation::SetTitle {
                title: "Title".into(),
                slug: "title".into(),
                snapshot_history_len: 1,
            },
        );
        session.turn_metas.push((
            1,
            TurnMeta {
                elapsed_ms: 10,
                avg_tps: None,
                display_tps: None,
                interrupted: false,
            },
        ));

        let result = apply_history(
            &mut session,
            HistoryMutation::RewindTo {
                index: 0,
                keep_checkpoint_at_boundary: false,
                identity,
            },
        );

        assert!(result.session_dirty);
        assert!(session.history.is_empty());
        assert!(session.turn_metas.is_empty());
        assert!(session.context_snapshots.is_empty());
        assert_eq!(session.current_context_tokens(), None);
        assert_eq!(session.title, None);
        assert_eq!(session.slug, None);
    }

    #[test]
    fn runtime_metadata_mutation_updates_save_metadata_and_reports_dirty() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));

        let result = apply_metadata(
            &mut session,
            MetadataMutation::UpdateRuntime {
                updated_at_ms: 42,
                mode: "agent".into(),
                reasoning_effort: ReasoningEffort::High,
                model: Some("provider/model".into()),
                fast_mode: true,
            },
        );

        assert!(result.session_dirty);
        assert_eq!(session.updated_at_ms, 42);
        assert_eq!(session.mode.as_deref(), Some("agent"));
        assert_eq!(session.reasoning_effort, Some(ReasoningEffort::High));
        assert_eq!(session.model.as_deref(), Some("provider/model"));
        assert_eq!(session.fast_mode, Some(true));
    }

    #[test]
    fn cwd_mutation_updates_session_metadata_and_reports_dirty() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));

        let result = apply_metadata(
            &mut session,
            MetadataMutation::SetCwd {
                cwd: "/repo".into(),
            },
        );

        assert!(result.session_dirty);
        assert_eq!(session.cwd.as_deref(), Some("/repo"));
    }

    #[test]
    fn checkpoint_mutation_installs_checkpoint_and_reports_dirty() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        session.history = vec![
            HistoryItem::user(Content::text("old")),
            protocol::HistoryItem::Assistant(protocol::AssistantStep::terminal(
                Some(Content::text("old reply")),
                None,
                Vec::new(),
            )),
            HistoryItem::user(Content::text("recent")),
        ];

        let result = apply_turn_state(
            &mut session,
            TurnStateMutation::InstallCheckpoint {
                kind: "compaction".into(),
                summary: "summary".into(),
                first_live_message_index: 2,
                tokens_before: Some(100),
            },
        );

        assert!(result.applied);
        assert!(result.session_dirty);
        assert_eq!(session.checkpoint.as_ref().unwrap().first_live_index, 2);
    }

    #[test]
    fn finish_turn_state_mutation_appends_meta_and_snapshots_context() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        session.context_tokens = Some(123);
        session.history = vec![HistoryItem::user(Content::text("hello"))];
        let meta = TurnMeta {
            elapsed_ms: 10,
            avg_tps: Some(2.0),
            display_tps: Some(2.0),
            interrupted: false,
        };

        let result = apply_turn_state(
            &mut session,
            TurnStateMutation::Finish {
                history_len: 7,
                meta,
                update_context_token_history_len: true,
            },
        );

        assert!(result.session_dirty);
        assert!(result.applied);
        assert_eq!(session.turn_metas.len(), 1);
        assert_eq!(session.turn_metas[0].0, 7);
        assert_eq!(session.context_tokens_history_len, Some(7));
        assert!(session.context_snapshots.iter().any(|(idx, _)| *idx == 7));
    }

    #[test]
    fn prune_rewindable_state_mutation_truncates_snapshots_and_reports_dirty() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        let meta = TurnMeta {
            elapsed_ms: 10,
            avg_tps: None,
            display_tps: None,
            interrupted: false,
        };
        session.turn_metas.push((2, meta.clone()));
        session.turn_metas.push((4, meta));
        session.snapshot_metadata_at(2);
        session.snapshot_metadata_at(4);

        let result = apply_turn_state(
            &mut session,
            TurnStateMutation::PruneRewindable {
                history_len: 2,
                identity: token_identity(),
            },
        );

        assert!(result.session_dirty);
        assert!(result.turn_meta.is_some());
        assert_eq!(session.turn_metas.len(), 1);
        assert_eq!(session.metadata_snapshots.len(), 1);
    }

    #[test]
    fn append_history_item_mutation_updates_history_and_reports_dirty_range() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        session
            .history
            .push(HistoryItem::user(Content::text("existing")));

        let result = apply_history(
            &mut session,
            HistoryMutation::AppendItem {
                item: HistoryItem::user(Content::text("new")),
            },
        );

        assert!(result.session_dirty);
        assert_eq!(result.history_idx, Some(1));
        assert_eq!(result.history_dirty_from, Some(1));
        assert_eq!(session.history.len(), 2);
    }

    #[test]
    fn append_history_with_transcript_block_updates_both_projections() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        session
            .history
            .push(HistoryItem::user(Content::text("existing")));
        let mut transcript = TranscriptDocument::new();

        let result = apply_history_with_transcript(
            &mut session,
            &mut transcript,
            HistoryMutation::CommitRequestItem {
                item: HistoryItem::user(Content::text("new")),
                block: Some(Box::new(Block::User {
                    text: "new".into(),
                    image_labels: Vec::new(),
                    command: false,
                    sent_at_ms: None,
                })),
                first_user_message: None,
            },
        );

        assert!(result.session_dirty);
        assert!(result.transcript_dirty);
        assert_eq!(result.history_idx, Some(1));
        assert_eq!(result.history_dirty_from, Some(1));
        assert_eq!(session.history.len(), 2);
        assert_eq!(
            transcript.history().block_origin_at(0),
            Some(BlockOrigin::History(1))
        );
        assert_eq!(transcript.history().record_dirty_from(), Some(0));
    }

    #[test]
    fn commit_request_history_item_snapshots_first_user_message_atomically() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        let mut transcript = TranscriptDocument::new();

        let result = apply_history_with_transcript(
            &mut session,
            &mut transcript,
            HistoryMutation::CommitRequestItem {
                item: HistoryItem::user(Content::text("new")),
                block: Some(Box::new(Block::User {
                    text: "new".into(),
                    image_labels: Vec::new(),
                    command: false,
                    sent_at_ms: None,
                })),
                first_user_message: Some("new".into()),
            },
        );

        assert!(result.session_dirty);
        assert_eq!(result.history_idx, Some(0));
        assert_eq!(session.first_user_message.as_deref(), Some("new"));
        assert_eq!(session.metadata_snapshots.len(), 1);
        assert_eq!(session.metadata_snapshots[0].0, 1);
        assert_eq!(
            transcript.history().block_origin_at(0),
            Some(BlockOrigin::History(0))
        );
    }

    #[test]
    fn live_append_history_with_transcript_block_updates_both_projections() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        let mut live_session = empty_live_session_for(&session, 0);
        let mut transcript = TranscriptDocument::new();

        let result = SessionDocument::apply_history(
            &mut session,
            Some(&mut live_session),
            Some(&mut transcript),
            HistoryMutation::CommitRequestItem {
                item: HistoryItem::user(Content::text("new")),
                block: Some(Box::new(Block::User {
                    text: "new".into(),
                    image_labels: Vec::new(),
                    command: false,
                    sent_at_ms: None,
                })),
                first_user_message: None,
            },
        );

        assert!(result.session_dirty);
        assert!(result.transcript_dirty);
        assert_eq!(result.history_idx, Some(0));
        assert_eq!(live_session.history_len(), 1);
        assert_eq!(
            transcript.history().block_origin_at(0),
            Some(BlockOrigin::History(0))
        );
    }

    #[test]
    fn runtime_apply_routes_history_append_to_live_session() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        let mut document = TuiSessionDocument::new(TranscriptDocument::new());
        document.live_session = Some(empty_live_session_for(&session, 0));
        let result = document.apply_history(
            &mut session,
            HistoryMutation::AppendItem {
                item: HistoryItem::user(Content::text("live")),
            },
        );

        assert_eq!(result.history_idx, Some(0));
        assert!(session.history.is_empty());
        assert_eq!(
            document
                .live_session
                .as_ref()
                .expect("live session")
                .history_len(),
            1
        );
        assert_eq!(document.generation(), PersistenceGeneration::new(1));
        assert_eq!(document.dirty_history_from_for_test(), Some(0));
    }

    #[test]
    fn runtime_apply_routes_streaming_mutation_to_parser_and_transcript() {
        let mut document = TuiSessionDocument::new(TranscriptDocument::new());
        let mut parser = StreamParser::new();

        let result = document.apply_stream(
            &mut parser,
            StreamMutation::AppendText {
                delta: "hello".into(),
            },
        );

        assert!(result.transcript_dirty);
        assert_eq!(document.transcript.history().record_dirty_from(), Some(0));
        assert_eq!(document.generation(), PersistenceGeneration::new(1));
    }

    #[test]
    fn truncate_history_mutation_updates_history_and_reports_dirty_range() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        session.history = vec![
            HistoryItem::user(Content::text("one")),
            HistoryItem::user(Content::text("two")),
            HistoryItem::user(Content::text("three")),
        ];

        session.snapshot_context_at(3);
        let result = apply_history(
            &mut session,
            HistoryMutation::TruncateFrom {
                index: 1,
                identity: token_identity(),
            },
        );

        assert!(result.session_dirty);
        assert_eq!(result.history_dirty_from, Some(1));
        assert_eq!(session.history.len(), 1);
        assert!(session.context_snapshots.is_empty());
    }

    #[test]
    fn append_transcript_block_mutation_updates_transcript_and_reports_dirty() {
        let mut transcript = TranscriptDocument::new();

        let result = apply_transcript(
            &mut transcript,
            TranscriptMutation::AppendBlock {
                block: Block::Text {
                    content: "record-only text".into(),
                },
            },
        );

        assert!(result.applied);
        assert!(result.transcript_dirty);
        assert!(!transcript.history().is_empty());
        assert_eq!(transcript.history().record_dirty_from(), Some(0));
    }

    #[test]
    fn empty_append_transcript_block_mutation_reports_noop() {
        let mut transcript = TranscriptDocument::new();

        let result = apply_transcript(
            &mut transcript,
            TranscriptMutation::AppendBlock {
                block: Block::Text {
                    content: "  \n\t  ".into(),
                },
            },
        );

        assert!(!result.applied);
        assert!(!result.transcript_dirty);
        assert!(transcript.history().is_empty());
        assert_eq!(transcript.history().record_dirty_from(), None);
    }

    #[test]
    fn rewrite_transcript_block_mutation_updates_transcript_and_reports_dirty() {
        let mut transcript = TranscriptDocument::new();
        transcript.push(Block::Text {
            content: "old".into(),
        });
        let id = transcript.history().order[0];
        transcript.history_mut().clear_record_dirty();

        let result = apply_transcript(
            &mut transcript,
            TranscriptMutation::RewriteBlock {
                id,
                block: Block::Text {
                    content: "new".into(),
                },
            },
        );

        assert!(result.applied);
        assert!(result.transcript_dirty);
        assert_eq!(transcript.history().record_dirty_from(), Some(0));
        assert!(matches!(
            transcript.history().block(id),
            Some(Block::Text { content }) if content == "new"
        ));
    }

    #[test]
    fn replace_transcript_from_history_reports_dirty_and_unpersisted() {
        let mut transcript = TranscriptDocument::new();
        transcript.push(Block::Text {
            content: "old".into(),
        });
        transcript.history_mut().clear_record_dirty();

        let mut rebuilt = smelt_core::content::transcript::Transcript::new();
        rebuilt.push_with_origin(
            Block::Text {
                content: "rebuilt".into(),
            },
            BlockOrigin::History(0),
        );

        let result = apply_transcript(
            &mut transcript,
            TranscriptMutation::ReplaceFromHistory {
                transcript: Box::new(rebuilt),
            },
        );

        assert!(result.applied);
        assert!(result.transcript_dirty);
        assert!(result.records_unpersisted);
        assert_eq!(transcript.history().len(), 1);
        assert_eq!(transcript.history().record_dirty_from(), Some(0));
        assert_eq!(
            transcript.history().block_origin_at(0),
            Some(BlockOrigin::History(0))
        );
    }

    #[test]
    fn checkpoint_marker_transcript_mutation_reports_dirty() {
        let mut transcript = TranscriptDocument::new();
        transcript.push(Block::Text {
            content: "history text".into(),
        });
        transcript.history_mut().clear_record_dirty();

        let result = apply_transcript(
            &mut transcript,
            TranscriptMutation::InsertCheckpointMarker {
                block_index: 0,
                history_index: 3,
                block: Block::Compacted {
                    summary: "summary".into(),
                },
            },
        );

        assert!(result.applied);
        assert!(result.transcript_dirty);
        assert_eq!(
            transcript.history().block_origin_at(0),
            Some(BlockOrigin::Checkpoint { history_index: 3 })
        );
        assert_eq!(transcript.history().record_dirty_from(), Some(0));
    }

    #[test]
    fn remove_unoriginated_transcript_mutation_reports_when_applied() {
        let mut transcript = TranscriptDocument::new();
        transcript.push(Block::Text {
            content: "temporary".into(),
        });
        transcript.push_with_origin(
            Block::Text {
                content: "history".into(),
            },
            BlockOrigin::History(0),
        );
        transcript.history_mut().clear_record_dirty();

        let removed = apply_transcript(
            &mut transcript,
            TranscriptMutation::RemoveUnoriginatedBlockAt { block_index: 0 },
        );
        let skipped = apply_transcript(
            &mut transcript,
            TranscriptMutation::RemoveUnoriginatedBlockAt { block_index: 0 },
        );

        assert!(removed.applied);
        assert!(removed.transcript_dirty);
        assert!(!skipped.applied);
        assert!(!skipped.transcript_dirty);
        assert_eq!(transcript.history().len(), 1);
        assert_eq!(
            transcript.history().block_origin_at(0),
            Some(BlockOrigin::History(0))
        );
    }

    #[test]
    fn truncate_transcript_mutation_reports_dirty_when_it_removes_blocks() {
        let mut transcript = TranscriptDocument::new();
        transcript.push(Block::Text {
            content: "one".into(),
        });
        transcript.push(Block::Text {
            content: "two".into(),
        });
        transcript.history_mut().clear_record_dirty();

        let result = apply_transcript(
            &mut transcript,
            TranscriptMutation::TruncateTo { block_index: 1 },
        );

        assert!(result.applied);
        assert!(result.transcript_dirty);
        assert_eq!(transcript.history().len(), 1);
        assert_eq!(transcript.history().record_dirty_from(), Some(1));
    }

    #[test]
    fn clear_transcript_mutation_reports_dirty_and_removes_blocks() {
        let mut transcript = TranscriptDocument::new();
        transcript.push(Block::Text {
            content: "one".into(),
        });
        transcript.push(Block::Text {
            content: "two".into(),
        });
        transcript.history_mut().clear_record_dirty();

        let result = apply_transcript(&mut transcript, TranscriptMutation::Clear);

        assert!(result.applied);
        assert!(result.transcript_dirty);
        assert!(transcript.history().is_empty());
        assert_eq!(transcript.history().record_dirty_from(), Some(0));
    }

    #[test]
    fn compaction_preview_mutations_update_transcript_and_report_dirty() {
        let mut transcript = TranscriptDocument::new();

        let updated = apply_transcript(
            &mut transcript,
            TranscriptMutation::UpdateCompactionPreview {
                summary: "streaming summary".into(),
            },
        );
        let id = updated.block_id.expect("preview block id");

        assert!(updated.applied);
        assert!(updated.transcript_dirty);
        assert_eq!(transcript.compaction_preview_id(), Some(id));
        assert_eq!(transcript.history().record_dirty_from(), Some(0));

        transcript.history_mut().clear_record_dirty();
        let cleared = apply_transcript(&mut transcript, TranscriptMutation::ClearCompactionPreview);

        assert!(cleared.applied);
        assert!(cleared.transcript_dirty);
        assert_eq!(cleared.block_id, Some(id));
        assert_eq!(transcript.compaction_preview_id(), None);
    }

    #[test]
    fn stream_parser_noop_does_not_report_ambient_transcript_dirty() {
        let mut parser = StreamParser::new();
        let mut transcript = TranscriptDocument::new();
        transcript.push(Block::Text {
            content: "already dirty".into(),
        });
        assert_eq!(transcript.history().record_dirty_from(), Some(0));

        let result = apply_stream_parser_transcript(
            &mut parser,
            &mut transcript,
            StreamMutation::ClearToolDrafts,
        );

        assert!(!result.transcript_dirty);
        assert!(!result.records_unpersisted);
        assert_eq!(transcript.history().record_dirty_from(), Some(0));
    }

    #[test]
    fn streaming_text_mutation_updates_transcript_and_reports_dirty() {
        let mut parser = StreamParser::new();
        let mut transcript = TranscriptDocument::new();

        let result = apply_stream_parser_transcript(
            &mut parser,
            &mut transcript,
            StreamMutation::AppendText {
                delta: "hello".into(),
            },
        );

        assert!(result.transcript_dirty);
        assert!(result.records_unpersisted);
        assert!(!transcript.history().is_empty());
        assert_eq!(transcript.history().record_dirty_from(), Some(0));
    }

    #[test]
    fn active_tool_elapsed_mutation_does_not_dirty_persistence() {
        let mut parser = StreamParser::new();
        let mut transcript = TranscriptDocument::new();
        let now = Instant::now();

        apply_stream_parser_transcript(
            &mut parser,
            &mut transcript,
            StreamMutation::StartTool {
                start: ToolStart {
                    invocation_id: INVOCATION_ID,
                    call_id: "tool-1".into(),
                    name: "bash".into(),
                    summary: StyledLines::from_plain("bash"),
                    args: HashMap::new(),
                    preview_output: None,
                    called_at_ms: 0,
                },
                now,
            },
        );
        transcript.history_mut().clear_record_dirty();

        let result = apply_stream_parser_transcript(
            &mut parser,
            &mut transcript,
            StreamMutation::SyncActiveToolElapsed {
                now: now + Duration::from_secs(2),
            },
        );

        assert!(!result.transcript_dirty);
        assert!(!result.records_unpersisted);
        assert_eq!(transcript.history().record_dirty_from(), None);
        assert_eq!(
            transcript
                .history()
                .tool_state(transcript.history().order[0])
                .and_then(|state| state.elapsed),
            Some(Duration::from_secs(2)),
        );
    }

    #[test]
    fn tool_lifecycle_mutations_update_transcript_and_report_dirty() {
        let mut parser = StreamParser::new();
        let mut transcript = TranscriptDocument::new();
        let now = Instant::now();

        let started = apply_stream_parser_transcript(
            &mut parser,
            &mut transcript,
            StreamMutation::StartTool {
                start: ToolStart {
                    invocation_id: INVOCATION_ID,
                    call_id: "tool-1".into(),
                    name: "bash".into(),
                    summary: StyledLines::from_plain("bash"),
                    args: HashMap::new(),
                    preview_output: None,
                    called_at_ms: 0,
                },
                now,
            },
        );
        let output = apply_stream_parser_transcript(
            &mut parser,
            &mut transcript,
            StreamMutation::AppendToolOutput {
                invocation_id: INVOCATION_ID,
                line: "done".into(),
            },
        );
        let finished = apply_stream_parser_transcript(
            &mut parser,
            &mut transcript,
            StreamMutation::FinishTool {
                invocation_id: INVOCATION_ID,
                status: ToolStatus::Ok,
                output: None,
                engine_elapsed: None,
                now,
            },
        );

        assert!(started.transcript_dirty);
        assert!(output.transcript_dirty);
        assert!(finished.transcript_dirty);
        assert_eq!(transcript.history().record_dirty_from(), Some(0));
        assert_eq!(transcript.history().len(), 1);
    }

    #[test]
    fn draft_tool_mutations_update_transcript_and_report_dirty() {
        let mut parser = StreamParser::new();
        let mut transcript = TranscriptDocument::new();
        let now = Instant::now();

        let appended = apply_stream_parser_transcript(
            &mut parser,
            &mut transcript,
            StreamMutation::AppendToolDraft {
                stream_id: "stream-1".into(),
                call_id: Some("tool-1".into()),
                name: Some("bash".into()),
                delta: "{}".into(),
            },
        );
        transcript.history_mut().clear_record_dirty();
        let promoted = apply_stream_parser_transcript(
            &mut parser,
            &mut transcript,
            StreamMutation::PromoteToolDraft {
                stream_id: Some("stream-1".into()),
                start: ToolStart {
                    invocation_id: INVOCATION_ID,
                    call_id: "tool-1".into(),
                    name: "bash".into(),
                    summary: StyledLines::from_plain("bash"),
                    args: HashMap::new(),
                    preview_output: None,
                    called_at_ms: 0,
                },
                now,
            },
        );

        assert!(appended.transcript_dirty);
        assert!(promoted.applied);
        assert!(promoted.transcript_dirty);
        assert_eq!(transcript.history().record_dirty_from(), Some(0));
        assert_eq!(transcript.history().len(), 1);
    }

    #[test]
    fn exec_lifecycle_mutations_update_transcript_and_report_dirty() {
        let mut parser = StreamParser::new();
        let mut transcript = TranscriptDocument::new();

        let started = apply_stream_parser_transcript(
            &mut parser,
            &mut transcript,
            StreamMutation::StartExec {
                command: "echo hi".into(),
            },
        );
        let output = apply_stream_parser_transcript(
            &mut parser,
            &mut transcript,
            StreamMutation::AppendExecOutput { chunk: "hi".into() },
        );
        let finished = apply_stream_parser_transcript(
            &mut parser,
            &mut transcript,
            StreamMutation::FinishExec {
                final_output: Some("authoritative hi".into()),
            },
        );

        assert!(started.transcript_dirty);
        assert!(output.transcript_dirty);
        assert!(finished.transcript_dirty);
        assert_eq!(transcript.history().record_dirty_from(), Some(0));
        assert_eq!(transcript.history().len(), 1);
        let exec_id = transcript.history().last_block_id().expect("exec block id");
        let Block::Exec { output, .. } = transcript.history().block(exec_id).unwrap() else {
            panic!("expected exec block");
        };
        assert_eq!(output.snapshot(), "authoritative hi");
    }

    fn runtime_metadata() -> RuntimeSessionMetadata {
        RuntimeSessionMetadata {
            updated_at_ms: 20,
            mode: "agent".into(),
            reasoning_effort: ReasoningEffort::Low,
            model: Some("model-a".into()),
            fast_mode: false,
        }
    }

    fn receipt_for(
        intent: &PreparedSessionBatch,
        previous: smelt_store::StoreHead,
    ) -> smelt_store::SaveReceipt {
        let record_len =
            intent
                .records
                .as_ref()
                .map_or(previous.transcript_record_count, |suffix| {
                    smelt_store::TranscriptRecordCount::new(
                        suffix.start.get()
                            + u64::try_from(suffix.records.len()).expect("record count"),
                    )
                });
        smelt_store::SaveReceipt {
            session_id: intent.identity.id.clone(),
            previous,
            current: smelt_store::StoreHead {
                revision: previous.revision.checked_add(1).expect("revision"),
                history_len: intent.history.final_len,
                transcript_record_count: record_len,
            },
            lineage_id: None,
            history_text_bytes: 0,
        }
    }

    fn acknowledgement_for(
        epoch: SessionEpoch,
        intent: &PreparedSessionBatch,
        receipt: smelt_store::SaveReceipt,
    ) -> crate::persist::PersistenceAcknowledgement {
        crate::persist::PersistenceAcknowledgement {
            epoch,
            generation: intent.generation,
            record_projection: intent.record_projection,
            previous: receipt.previous,
            receipt,
        }
    }

    #[test]
    fn persistence_generation_is_checked() {
        assert_eq!(
            PersistenceGeneration::new(41).checked_next(),
            Some(PersistenceGeneration::new(42))
        );
        assert_eq!(PersistenceGeneration::new(u64::MAX).checked_next(), None);
    }

    #[test]
    fn no_op_canonical_mutation_does_not_advance_generation() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        session.updated_at_ms = 20;
        session.mode = Some("agent".into());
        session.reasoning_effort = Some(ReasoningEffort::Low);
        session.model = Some("model-a".into());
        session.fast_mode = Some(false);
        session.cwd = Some("/tmp".into());
        let mut document = TuiSessionDocument::new(TranscriptDocument::new());

        let changes = [
            document.apply_metadata(
                &mut session,
                MetadataMutation::SetFastMode { enabled: false },
            ),
            document.apply_metadata(
                &mut session,
                MetadataMutation::SetCwd { cwd: "/tmp".into() },
            ),
            document.apply_metadata(
                &mut session,
                MetadataMutation::UpdateRuntime {
                    updated_at_ms: 20,
                    mode: "agent".into(),
                    reasoning_effort: ReasoningEffort::Low,
                    model: Some("model-a".into()),
                    fast_mode: false,
                },
            ),
            document.apply_history(
                &mut session,
                HistoryMutation::TruncateFrom {
                    index: 0,
                    identity: token_identity(),
                },
            ),
            document.apply_transcript(TranscriptMutation::Clear),
        ];
        assert!(changes.iter().all(|change| !change.canonical_changed()));
        assert_eq!(document.generation(), PersistenceGeneration::ZERO);
    }

    #[test]
    fn materialized_intent_is_cumulative_and_matching_acknowledgement_cleans() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        let mut document = TuiSessionDocument::new(TranscriptDocument::new());
        document.apply_history(
            &mut session,
            HistoryMutation::AppendItem {
                item: HistoryItem::user(Content::text("first")),
            },
        );
        document.apply_history(
            &mut session,
            HistoryMutation::AppendItem {
                item: HistoryItem::user(Content::text("second")),
            },
        );

        let previous = document.acknowledged_head();
        let intent = document
            .prepare_event_batch(&mut session, runtime_metadata())
            .expect("prepare batch")
            .expect("dirty batch");
        assert_eq!(intent.history.start, smelt_store::HistoryIndex::ZERO);
        assert_eq!(intent.history.final_len, smelt_store::HistoryLen::new(2));
        assert_eq!(intent.history.items.len(), 2);

        let epoch = SessionEpoch::new(7);
        let acknowledgement = acknowledgement_for(epoch, &intent, receipt_for(&intent, previous));
        document.bind_persistence(epoch);
        assert!(document.acknowledge(&acknowledgement, &session.id, 2, None));
        assert_eq!(document.durable_generation(), intent.generation);
        assert_eq!(document.dirty_history_from_for_test(), None);
        assert!(!document.has_session_work());
    }

    #[test]
    fn acknowledgement_uses_submitted_record_projection() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        let mut document = TuiSessionDocument::new(TranscriptDocument::new());
        document.apply_transcript(TranscriptMutation::AppendBlock {
            block: Block::Text {
                content: "durable record".into(),
            },
        });

        let previous = document.acknowledged_head();
        let intent = document
            .prepare_event_batch(&mut session, runtime_metadata())
            .expect("prepare batch")
            .expect("dirty batch");
        assert!(intent.record_projection.bounds.is_some());
        document.transcript.history_mut().clear_record_dirty();

        let epoch = SessionEpoch::new(8);
        let acknowledgement = acknowledgement_for(epoch, &intent, receipt_for(&intent, previous));
        document.bind_persistence(epoch);
        assert!(document.acknowledge(&acknowledgement, &session.id, 0, None));
        assert_eq!(document.durable_generation(), intent.generation);
    }

    #[test]
    fn receipt_schedules_compaction_but_operation_pin_delays_dematerialization() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        let mut document = TuiSessionDocument::new(TranscriptDocument::new());
        document.apply_history(
            &mut session,
            HistoryMutation::CommitRequestItem {
                item: HistoryItem::user(Content::text("durable prompt")),
                block: Some(Box::new(Block::User {
                    text: "durable prompt".into(),
                    image_labels: Vec::new(),
                    command: false,
                    sent_at_ms: None,
                })),
                first_user_message: Some("durable prompt".into()),
            },
        );
        let id = document.transcript.history().order[0];

        assert!(!document.transcript.drain_compaction_slice());
        assert!(document.transcript.history().is_live(id));
        assert!(document.transcript.pin_operation_blocks(&[id]));

        let previous = document.acknowledged_head();
        let intent = document
            .prepare_event_batch(&mut session, runtime_metadata())
            .expect("prepare batch")
            .expect("dirty batch");
        let epoch = SessionEpoch::new(9);
        let acknowledgement = acknowledgement_for(epoch, &intent, receipt_for(&intent, previous));
        document.bind_persistence(epoch);
        assert!(document.acknowledge(&acknowledgement, &session.id, 1, None));

        assert!(!document.transcript.drain_compaction_slice());
        assert!(document.transcript.history().is_live(id));
        document.transcript.unpin_operation_blocks(&[id]);
        assert!(document.transcript.drain_compaction_slice());
        assert!(!document.transcript.history().is_materialized(id));
        assert_eq!(
            document.transcript.memory_snapshot().dematerialized_entries,
            1
        );
    }

    #[test]
    fn acknowledgement_rejects_wrong_scope_and_head() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        let mut document = TuiSessionDocument::new(TranscriptDocument::new());
        document.apply_history(
            &mut session,
            HistoryMutation::AppendItem {
                item: HistoryItem::user(Content::text("dirty")),
            },
        );
        let previous = document.acknowledged_head();
        let intent = document
            .prepare_event_batch(&mut session, runtime_metadata())
            .expect("prepare batch")
            .expect("dirty batch");
        let epoch = SessionEpoch::new(3);
        let acknowledgement = acknowledgement_for(epoch, &intent, receipt_for(&intent, previous));
        document.bind_persistence(epoch);

        let mut wrong_epoch = acknowledgement.clone();
        wrong_epoch.epoch = SessionEpoch::new(4);
        assert!(!document.acknowledge(&wrong_epoch, &session.id, 1, None));
        let mut older_generation = acknowledgement.clone();
        older_generation.generation = PersistenceGeneration::new(intent.generation.get() - 1);
        assert!(!document.acknowledge(&older_generation, &session.id, 1, None));
        let mut newer_generation = acknowledgement.clone();
        newer_generation.generation = PersistenceGeneration::new(intent.generation.get() + 1);
        assert!(!document.acknowledge(&newer_generation, &session.id, 1, None));
        assert!(!document.acknowledge(&acknowledgement, &session.id, 2, None));
        let mut wrong_session = acknowledgement.clone();
        wrong_session.receipt.session_id = "different-session".into();
        assert!(!document.acknowledge(&wrong_session, &session.id, 1, None));
        let mut wrong_record_len = acknowledgement.clone();
        wrong_record_len.receipt.current.transcript_record_count =
            smelt_store::TranscriptRecordCount::new(1);
        assert!(!document.acknowledge(&wrong_record_len, &session.id, 1, None));
        let mut wrong_head = acknowledgement.clone();
        wrong_head.receipt.previous.revision = smelt_store::Revision::new(9);
        assert!(!document.acknowledge(&wrong_head, &session.id, 1, None));
        let mut skipped_revision = acknowledgement.clone();
        skipped_revision.receipt.current.revision = skipped_revision
            .receipt
            .previous
            .revision
            .checked_add(2)
            .expect("revision");
        assert!(!document.acknowledge(&skipped_revision, &session.id, 1, None));
        assert!(document.has_session_work());
    }

    #[test]
    fn empty_unpublished_draft_does_not_build_intent() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        let mut document = TuiSessionDocument::new(TranscriptDocument::new());
        document.apply_metadata(
            &mut session,
            MetadataMutation::SetFastMode { enabled: true },
        );

        assert!(document
            .prepare_event_batch(&mut session, runtime_metadata())
            .expect("prepare draft")
            .is_none());
        assert!(document.has_session_work());
    }

    #[test]
    fn generated_mutation_sequences_build_complete_cumulative_intents() {
        for seed in 0..64_u64 {
            let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
            let mut document = TuiSessionDocument::new(TranscriptDocument::new());
            let mut state = seed.wrapping_add(1);
            for step in 0..24 {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                let history_len = session.history.len();
                match state % 5 {
                    0 => {
                        document.apply_history(
                            &mut session,
                            HistoryMutation::AppendItem {
                                item: HistoryItem::user(Content::text(format!("{seed}:{step}"))),
                            },
                        );
                    }
                    1 => {
                        document.apply_history(
                            &mut session,
                            HistoryMutation::TruncateFrom {
                                index: (state as usize) % (history_len + 1),
                                identity: token_identity(),
                            },
                        );
                    }
                    2 => {
                        document.apply_metadata(
                            &mut session,
                            MetadataMutation::SetFastMode {
                                enabled: state & 1 == 0,
                            },
                        );
                    }
                    3 => {
                        document.apply_metadata(
                            &mut session,
                            MetadataMutation::SetCwd {
                                cwd: format!("/tmp/{seed}/{step}"),
                            },
                        );
                    }
                    _ => {
                        document.apply_metadata(
                            &mut session,
                            MetadataMutation::SetTitle {
                                title: format!("title-{seed}-{step}"),
                                slug: format!("title-{seed}-{step}"),
                                snapshot_history_len: history_len,
                            },
                        );
                    }
                }
            }
            document.apply_history(
                &mut session,
                HistoryMutation::AppendItem {
                    item: HistoryItem::user(Content::text(format!("final-{seed}"))),
                },
            );

            let intent = document
                .prepare_event_batch(&mut session, runtime_metadata())
                .expect("prepare generated intent")
                .expect("generated session has content");
            let history_len = session.history.len();
            assert_eq!(intent.history.start, smelt_store::HistoryIndex::ZERO);
            assert_eq!(
                intent.history.final_len,
                smelt_store::HistoryLen::new(history_len as u64)
            );
            assert_eq!(intent.history.items, session.history);
            assert_eq!(
                intent.identity,
                smelt_core::session::store_identity_from_session(&session).unwrap()
            );
            assert_eq!(
                intent.metadata,
                smelt_core::session::store_metadata_from_session(&session, history_len).unwrap()
            );
            assert_eq!(
                intent.side_tables,
                smelt_core::session::store_side_table_suffixes_from_session_at(
                    &session,
                    0,
                    history_len,
                )
                .unwrap()
            );
        }
    }

    #[test]
    fn sparse_record_save_bridges_the_acknowledged_store_head() {
        use crate::app::transcript::{
            seed_test_transcript_rows, LoadedTranscript, TEST_LINEAGE_SESSION_ID,
        };

        let source_root = tempfile::tempdir().expect("source session root");
        let target_root = tempfile::tempdir().expect("target state root");
        let target_sessions_root = target_root.path().join("sessions");
        let mut transcript = smelt_core::content::transcript::Transcript::new();
        for index in 0..320 {
            transcript.push(Block::Text {
                content: format!("block {index}").into(),
            });
        }
        let records = transcript
            .history
            .block_records()
            .iter()
            .enumerate()
            .map(|(index, record)| {
                smelt_core::transcript_model::transcript_block_row_with_block_idx(
                    index,
                    index as u64,
                    record,
                )
                .expect("transcript row")
            })
            .collect::<Vec<_>>();
        let source_session_dir = seed_test_transcript_rows(source_root.path(), records.clone());
        seed_test_transcript_rows(&target_sessions_root, records[..316].to_vec());

        let source_reader = smelt_store::LineageSessionReader::open_existing(
            source_root.path(),
            TEST_LINEAGE_SESSION_ID,
        )
        .expect("source reader");
        let slice = source_reader
            .transcript_record_slice_with_total((300..320).into(), 320)
            .expect("sparse transcript tail");
        drop(source_reader);
        let loaded = LoadedTranscript::from_record_slice(slice, source_session_dir)
            .expect("loaded sparse tail");
        let target_reader = smelt_store::LineageSessionReader::open_existing(
            &target_sessions_root,
            TEST_LINEAGE_SESSION_ID,
        )
        .expect("target reader");
        let target_snapshot = target_reader.snapshot().expect("target snapshot");
        drop(target_reader);
        let target_head = target_snapshot.head;
        assert_eq!(target_head.transcript_record_count.get(), 316);

        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        session.id = TEST_LINEAGE_SESSION_ID.into();
        session.created_at_ms = u64::try_from(target_snapshot.identity.created_at)
            .expect("non-negative creation timestamp");
        session.parent_id = target_snapshot.identity.parent_id;
        let mut document =
            TuiSessionDocument::new(TranscriptDocument::from_loaded_transcript(loaded));
        document.changes.install_head(target_head);
        document.changes.force_dirty();
        document
            .transcript
            .history_mut()
            .require_record_resave_from(18);

        let mut metadata = runtime_metadata();
        metadata.updated_at_ms = session.created_at_ms.saturating_add(1);
        assert!(matches!(
            document.prepare_event_batch(&mut session, metadata.clone()),
            Err(SessionBatchPreparationError::HydrationPending)
        ));
        let request = document
            .transcript
            .take_pending_hydration_request()
            .expect("sparse save hydration request");
        let result = crate::app::transcript_hydration::execute_hydration_request_for_test(request)
            .expect("sparse save hydration result");
        document.transcript.install_hydration_result(result);
        let intent = document
            .prepare_event_batch(&mut session, metadata)
            .expect("prepare hydrated sparse save")
            .expect("dirty sparse save");
        let suffix = intent.records.as_ref().expect("record suffix");
        assert_eq!(suffix.start.get(), 300);
        assert_eq!(suffix.records.len(), 20);

        let generation = intent.generation;
        let (mut persistence, _) = crate::persist::SessionPersistence::spawn(
            smelt_core::session::SessionStorage::new(target_root.path().to_path_buf()),
            smelt_core::session_id::SessionId::parse(TEST_LINEAGE_SESSION_ID).expect("session id"),
            SessionEpoch::new(1),
            PersistenceGeneration::ZERO,
            target_head,
        )
        .expect("persistence actor");
        persistence.submit(intent).expect("submit sparse save");
        let close = persistence.close(
            generation,
            Instant::now() + Duration::from_secs(5),
            crate::persist::ClosePolicy::RequireDurable,
        );
        assert!(
            close.cause.is_none(),
            "sparse save failed: {:?}",
            close.cause
        );

        let saved = smelt_store::LineageSessionReader::open_existing(
            &target_sessions_root,
            TEST_LINEAGE_SESSION_ID,
        )
        .expect("saved reader")
        .transcript_record_slice_with_total((300..320).into(), 320)
        .expect("saved transcript tail");
        assert_eq!(saved.records.len(), 20);
        assert_eq!(saved.records[18].block_idx, 318);
    }

    #[test]
    fn store_backed_cumulative_intent_spans_lineage_prefix_and_live_suffix() {
        const ID: &str = "7777777777777777777777777777777777777777777777777777777777777777";
        let root = tempfile::tempdir().expect("session root");
        let mut stored = Session::new(1, std::path::PathBuf::from("/tmp"));
        stored.id = ID.into();
        stored.history = vec![
            HistoryItem::user(Content::text("stored-0")),
            HistoryItem::user(Content::text("stored-1")),
        ];
        let mut writer = smelt_store::OwnedLineageWriter::open(root.path(), ID).expect("writer");
        let command = smelt_core::session::initial_store_commit_from_session(&stored)
            .expect("initial commit");
        let receipt = writer.commit_session(&command).expect("store prefix");
        let mut meta = meta_with_token_identity();
        meta.id = ID.into();
        meta.history_len = Some(2);
        let header = SessionHeader {
            meta,
            history_len: 2,
            revision: receipt.current.revision.get(),
            degraded_warnings: Vec::new(),
        };
        let store_address = SessionStoreAddress::new(
            root.path().to_path_buf(),
            ID.into(),
            writer.lineage_id().to_string(),
        );
        let mut session = stored.clone();
        session.history.clear();
        let mut document = TuiSessionDocument::new(TranscriptDocument::new());
        document.live_session = Some(LiveSession::from_store(header, store_address));
        document.changes.install_head(receipt.current);
        document.set_history_resave_from_for_test(0);
        document.apply_history(
            &mut session,
            HistoryMutation::AppendItem {
                item: HistoryItem::user(Content::text("live-2")),
            },
        );

        let intent = document
            .prepare_event_batch(&mut session, runtime_metadata())
            .expect("prepare batch")
            .expect("dirty batch");
        assert_eq!(intent.history.start, smelt_store::HistoryIndex::ZERO);
        assert_eq!(intent.history.final_len, smelt_store::HistoryLen::new(3));
        assert_eq!(
            intent.history.items,
            vec![
                HistoryItem::user(Content::text("stored-0")),
                HistoryItem::user(Content::text("stored-1")),
                HistoryItem::user(Content::text("live-2")),
            ]
        );
        writer.release().expect("release writer");
    }

    #[test]
    fn matching_store_backed_acknowledgement_compacts_live_suffix() {
        let mut session = Session::new(1, std::path::PathBuf::from("/tmp"));
        let mut document = TuiSessionDocument::new(TranscriptDocument::new());
        document.live_session = Some(empty_live_session_for(&session, 2));
        document.changes.install_head(smelt_store::StoreHead {
            revision: smelt_store::Revision::new(5),
            history_len: smelt_store::HistoryLen::new(2),
            transcript_record_count: smelt_store::TranscriptRecordCount::ZERO,
        });
        document.apply_history(
            &mut session,
            HistoryMutation::AppendItem {
                item: HistoryItem::user(Content::text("live")),
            },
        );

        let previous = document.acknowledged_head();
        let intent = document
            .prepare_event_batch(&mut session, runtime_metadata())
            .expect("prepare batch")
            .expect("dirty batch");
        assert_eq!(intent.history.start, smelt_store::HistoryIndex::new(2));
        assert_eq!(intent.history.items.len(), 1);
        let epoch = SessionEpoch::new(9);
        let acknowledgement = acknowledgement_for(epoch, &intent, receipt_for(&intent, previous));
        document.bind_persistence(epoch);
        assert!(document.acknowledge(&acknowledgement, &session.id, 3, None));
        assert_eq!(
            document
                .live_session
                .as_ref()
                .expect("live session")
                .live_suffix_len(),
            0
        );
    }
}
