use crate::app::{
    history::HistoryDeltaKind, CommandTurnStart, NotificationOperation, PendingHistoryAppend,
    PendingHistoryDelivery, PendingHistoryLifecycle, PendingTool, SessionControl, TuiApp,
    TurnState, CONFIRM_DEFER_MS,
};
use protocol::{Content, ContentPart, Decision, HistoryItem, UiCommand};
use smelt_core::working::{TurnOutcome, TurnPhase};
use smelt_core::*;
use std::path::PathBuf;
use std::time::Duration;

/// Owns the state machine data that exists only for an agent turn lifecycle.
///
/// `TuiApp` still orchestrates persistence, rendering, and engine I/O, while this
/// subsystem owns turn identity, callback invalidation, continuation, request-scoped
/// history, and the temporary dispatch state needed when an active turn is lent out.
pub(crate) struct TurnLifecycle {
    active: Option<TurnState>,
    next_turn_id: u64,
    last_terminal_turn_id: Option<u64>,
    next_continuation_token: u64,
    pending_continuation_token: Option<u64>,
    pub(super) pause: Option<TurnPause>,
    pub(super) request_overrides: smelt_core::custom_commands::CommandOverrides,
    pending_meta: Option<protocol::TurnMeta>,
    pending_history_appends: Vec<PendingHistoryAppend>,
    context_tokens_updated: bool,
    applied_mode: protocol::AgentMode,
    applied_reasoning_effort: protocol::ReasoningEffort,
    cancel_generation: u64,
    dispatching_turn_id: Option<u64>,
    dispatching_permissions: Option<std::sync::Arc<smelt_core::permissions::Permissions>>,
}

/// Stops automatic dispatch without treating an interrupted turn as idle work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TurnPause {
    pub(crate) kind: Option<protocol::EngineAskErrorKind>,
    pub(crate) retry_at_ms: Option<u64>,
}

pub(crate) struct DispatchingTurn {
    pub(crate) state: TurnState,
    previous_turn_id: Option<u64>,
    previous_permissions: Option<std::sync::Arc<smelt_core::permissions::Permissions>>,
}

pub(super) struct LuaToolCompletion {
    pub(super) content: String,
    pub(super) is_error: bool,
    pub(super) metadata: Option<serde_json::Value>,
    pub(super) display_content: Vec<protocol::ToolDisplayContent>,
    pub(super) attachment: Option<Box<protocol::ToolAttachment>>,
}

impl TurnLifecycle {
    pub(crate) fn new(
        applied_mode: protocol::AgentMode,
        applied_reasoning_effort: protocol::ReasoningEffort,
    ) -> Self {
        Self {
            active: None,
            next_turn_id: 1,
            last_terminal_turn_id: None,
            next_continuation_token: 1,
            pending_continuation_token: None,
            pause: None,
            request_overrides: Default::default(),
            pending_meta: None,
            pending_history_appends: Vec::new(),
            context_tokens_updated: false,
            applied_mode,
            applied_reasoning_effort,
            cancel_generation: 0,
            dispatching_turn_id: None,
            dispatching_permissions: None,
        }
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active.is_some()
    }

    pub(crate) fn active(&self) -> Option<&TurnState> {
        self.active.as_ref()
    }

    pub(crate) fn active_mut(&mut self) -> Option<&mut TurnState> {
        self.active.as_mut()
    }

    pub(crate) fn set_active(&mut self, turn: Option<TurnState>) {
        self.active = turn;
    }

    pub(crate) fn clear_active(&mut self) -> Option<TurnState> {
        self.active.take()
    }

    pub(crate) fn next_turn_id(&self) -> u64 {
        self.next_turn_id
    }

    pub(crate) fn last_terminal_turn_id(&self) -> Option<u64> {
        self.last_terminal_turn_id
    }

    pub(crate) fn mark_terminal(&mut self, turn_id: u64) {
        self.last_terminal_turn_id = Some(turn_id);
    }

    pub(crate) fn set_last_terminal_turn_id(&mut self, turn_id: Option<u64>) {
        self.last_terminal_turn_id = turn_id;
    }

    pub(crate) fn applied_mode(&self) -> &protocol::AgentMode {
        &self.applied_mode
    }

    pub(crate) fn set_applied_mode(&mut self, mode: protocol::AgentMode) {
        self.applied_mode = mode;
    }

    pub(crate) fn applied_reasoning_effort(&self) -> &protocol::ReasoningEffort {
        &self.applied_reasoning_effort
    }

    pub(crate) fn set_applied_reasoning_effort(
        &mut self,
        reasoning_effort: protocol::ReasoningEffort,
    ) {
        self.applied_reasoning_effort = reasoning_effort;
    }

    pub(crate) fn set_pending_meta(&mut self, meta: Option<protocol::TurnMeta>) {
        self.pending_meta = meta;
    }

    pub(crate) fn take_pending_meta(&mut self) -> Option<protocol::TurnMeta> {
        self.pending_meta.take()
    }

    pub(crate) fn mark_context_tokens_updated(&mut self) {
        self.context_tokens_updated = true;
    }

    pub(crate) fn clear_context_tokens_updated(&mut self) {
        self.context_tokens_updated = false;
    }

    pub(crate) fn take_context_tokens_updated(&mut self) -> bool {
        std::mem::take(&mut self.context_tokens_updated)
    }

    pub(crate) fn pending_history_appends(&self) -> &[PendingHistoryAppend] {
        &self.pending_history_appends
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn pending_history_append_count(&self) -> usize {
        self.pending_history_appends.len()
    }

    pub(crate) fn clear_pending_history_appends(&mut self) {
        self.pending_history_appends.clear();
    }

    pub(crate) fn retain_session_history_appends(&mut self) {
        self.pending_history_appends
            .retain(|pending| pending.lifecycle() == PendingHistoryLifecycle::SessionScoped);
    }

    pub(crate) fn take_pending_history_appends(&mut self) -> Vec<PendingHistoryAppend> {
        std::mem::take(&mut self.pending_history_appends)
    }

    pub(crate) fn take_pending_follow_up_note(&mut self) -> Option<protocol::HistoryNote> {
        // Earlier appends stay in chronological order as context for this trigger.
        let index = self.pending_history_appends.iter().rposition(|append| {
            append.delivery() == PendingHistoryDelivery::FollowUpIfUnconsumed
        })?;
        let append = self.pending_history_appends.remove(index);
        match append.item {
            HistoryItem::Note(note) => Some(note),
            _ => unreachable!("follow-up append must contain a history note"),
        }
    }

    pub(crate) fn pending_context_note(&self, name: &str) -> Option<Option<&str>> {
        self.pending_history_appends()
            .iter()
            .rev()
            .find(|pending| pending.context_name.as_deref() == Some(name))
            .map(|pending| {
                if pending.clear_context {
                    None
                } else {
                    pending.item.as_note().map(protocol::HistoryNote::text)
                }
            })
    }

    pub(crate) fn queue_history_append(
        &mut self,
        append: PendingHistoryAppend,
        mode_base: Option<&protocol::AgentMode>,
    ) {
        if append.coalescing_note_kind() == Some(protocol::HistoryNoteKind::ModeChange) {
            let Some(new_mode) = append.mode() else {
                self.replace_or_push_history_append(append);
                return;
            };
            let existing = self.pending_history_appends.iter().position(|pending| {
                pending.coalescing_note_kind() == Some(protocol::HistoryNoteKind::ModeChange)
            });
            if let Some(index) = existing {
                if mode_base.is_some_and(|base| base.as_str() == new_mode) {
                    self.pending_history_appends.remove(index);
                } else {
                    self.pending_history_appends[index] = append;
                }
            } else if mode_base.is_none_or(|base| base.as_str() != new_mode) {
                self.pending_history_appends.push(append);
            }
            return;
        }
        self.replace_or_push_history_append(append);
    }

    pub(crate) fn replace_or_push_history_append(&mut self, append: PendingHistoryAppend) {
        if append.coalescing_note_kind().is_some() {
            if let Some(existing) = self
                .pending_history_appends
                .iter_mut()
                .find(|pending| pending.same_coalescing_target(&append))
            {
                *existing = append;
                return;
            }
        }
        self.pending_history_appends.push(append);
    }

    pub(crate) fn remove_matching_history_append(&mut self, append: &PendingHistoryAppend) {
        self.pending_history_appends
            .retain(|pending| !pending.same_coalescing_target(append));
    }

    pub(crate) fn take_matching_history_append(
        &mut self,
        item: &protocol::HistoryItem,
    ) -> Option<PendingHistoryAppend> {
        let index = self
            .pending_history_appends()
            .iter()
            .position(|append| append.matches_history_item(item))?;
        Some(self.pending_history_appends.remove(index))
    }

    pub(crate) fn active_id(&self) -> Option<u64> {
        self.active
            .as_ref()
            .map(|turn| turn.turn_id)
            .or(self.dispatching_turn_id)
    }

    pub(crate) fn active_permissions(
        &self,
        fallback: std::sync::Arc<smelt_core::permissions::Permissions>,
    ) -> std::sync::Arc<smelt_core::permissions::Permissions> {
        self.active
            .as_ref()
            .map(|turn| turn.permissions.clone())
            .or_else(|| self.dispatching_permissions.clone())
            .unwrap_or(fallback)
    }

    pub(crate) fn refresh_active_permissions(
        &mut self,
        permissions: std::sync::Arc<smelt_core::permissions::Permissions>,
    ) {
        if self.dispatching_turn_id.is_some() {
            self.dispatching_permissions = Some(permissions);
        } else if let Some(turn) = self.active.as_mut() {
            turn.permissions = permissions;
        }
    }

    pub(crate) fn begin_dispatch(&mut self) -> Option<DispatchingTurn> {
        let state = self.active.take()?;
        let previous_turn_id = self.dispatching_turn_id.replace(state.turn_id);
        let previous_permissions = self
            .dispatching_permissions
            .replace(state.permissions.clone());
        Some(DispatchingTurn {
            state,
            previous_turn_id,
            previous_permissions,
        })
    }

    pub(crate) fn finish_dispatch(&mut self, mut dispatch: DispatchingTurn) {
        if let Some(permissions) = &self.dispatching_permissions {
            if !std::sync::Arc::ptr_eq(permissions, &dispatch.state.permissions) {
                dispatch.state.permissions = permissions.clone();
            }
        }
        self.dispatching_turn_id = dispatch.previous_turn_id;
        self.dispatching_permissions = dispatch.previous_permissions;
        self.active = Some(dispatch.state);
    }

    pub(crate) fn begin_prepared_turn(
        &mut self,
        overrides: smelt_core::custom_commands::CommandOverrides,
    ) {
        self.pending_continuation_token = None;
        self.pause = None;
        self.request_overrides = overrides;
    }

    pub(crate) fn record_started_turn(
        &mut self,
        turn_id: u64,
        mode: protocol::AgentMode,
        reasoning_effort: protocol::ReasoningEffort,
    ) {
        self.next_turn_id = turn_id.checked_add(1).unwrap_or(turn_id);
        self.applied_mode = mode;
        self.applied_reasoning_effort = reasoning_effort;
    }

    pub(crate) fn issue_continuation_token(&mut self) -> u64 {
        let token = self.next_continuation_token;
        self.next_continuation_token = self.next_continuation_token.wrapping_add(1).max(1);
        self.pending_continuation_token = Some(token);
        token
    }

    pub(crate) fn pending_continuation_token(&self) -> Option<u64> {
        self.pending_continuation_token
    }

    pub(crate) fn clear_continuation(&mut self) {
        self.pending_continuation_token = None;
    }

    pub(crate) fn consume_continuation(&mut self, token: u64) -> bool {
        if self.pending_continuation_token != Some(token) {
            return false;
        }
        self.pending_continuation_token = None;
        true
    }

    pub(crate) fn invalidate_turn_callbacks(&mut self) {
        self.cancel_generation = self.cancel_generation.wrapping_add(1);
        self.clear_continuation();
    }

    pub(crate) fn cancel_generation(&self) -> u64 {
        self.cancel_generation
    }

    pub(crate) fn reset_session(&mut self) {
        self.last_terminal_turn_id = None;
        self.pending_meta = None;
        self.pending_history_appends.clear();
        self.context_tokens_updated = false;
        self.pause = None;
        self.request_overrides = Default::default();
        self.clear_continuation();
    }
}

struct StagedTurnRollback {
    history_len: Option<usize>,
    transcript_len: Option<usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TerminalCommitStatus {
    Durable,
    Deferred,
}

impl TerminalCommitStatus {
    pub(crate) fn is_durable(self) -> bool {
        self == Self::Durable
    }
}

struct FinishTurnOutcome {
    start_queued: bool,
    terminal_commit: TerminalCommitStatus,
}

struct PreparedTurn {
    input: protocol::StartTurnInput,
    history: protocol::ModelHistorySource,
    kind: smelt_store::TurnKind,
    submitted_history_idx: smelt_store::HistoryIndex,
    continuation_of: Option<smelt_store::TurnId>,
    model_target: protocol::ModelTarget,
    request_config: protocol::RequestRuntimeConfig,
    reasoning_effort: protocol::ReasoningEffort,
    permission_overrides: Option<protocol::PermissionOverrides>,
    permissions: std::sync::Arc<smelt_core::permissions::Permissions>,
    request_overrides: smelt_core::custom_commands::CommandOverrides,
    rewind_history_idx: Option<usize>,
    rollback: Option<StagedTurnRollback>,
}

#[derive(Clone, Copy)]
enum PendingTurnSubmitState {
    Preparation,
    Persistence {
        generation: crate::app::session_document::PersistenceGeneration,
    },
}

pub(super) struct PendingTurnDispatch {
    command_id: crate::persist::CanonicalCommandId,
    dispatch: PreparedTurnDispatch,
    submit_state: PendingTurnSubmitState,
    cancelled_meta: Option<protocol::TurnMeta>,
}

struct PreparedTurnDispatch {
    turn: PreparedTurn,
    system_prompt: String,
    tools: Vec<protocol::ToolDef>,
    history: protocol::ModelHistorySource,
}

fn is_resumable_turn_error(kind: Option<protocol::EngineAskErrorKind>) -> bool {
    matches!(
        kind,
        Some(protocol::EngineAskErrorKind::Quota | protocol::EngineAskErrorKind::RateLimited)
    )
}

fn annotate_cwd_metadata(
    metadata: &mut Option<serde_json::Value>,
    committed: bool,
    error: Option<&str>,
) {
    let Some(serde_json::Value::Object(fields)) = metadata else {
        return;
    };
    fields.insert("pending".into(), serde_json::Value::Bool(false));
    fields.insert("cwd_committed".into(), serde_json::Value::Bool(committed));
    if let Some(error) = error {
        fields.insert(
            "cwd_error".into(),
            serde_json::Value::String(error.to_string()),
        );
    } else {
        fields.remove("cwd_error");
    }
}

impl TuiApp {
    /// Send a permission decision to the local engine.
    pub(crate) fn send_permission_decision(
        &mut self,
        request_id: u64,
        approved: bool,
        message: Option<String>,
    ) {
        self.core.engine.send(UiCommand::PermissionDecision {
            request_id,
            approved,
            message,
        });
    }

    pub(crate) fn active_permissions(
        &self,
    ) -> std::sync::Arc<smelt_core::permissions::Permissions> {
        self.conversation
            .active_permissions(self.core.permissions.snapshot())
    }

    pub(crate) fn with_dispatched_turn<R>(
        &mut self,
        body: impl FnOnce(&mut Self, &mut TurnState) -> R,
    ) -> Option<R> {
        let mut dispatch = self.conversation.begin_dispatch()?;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            body(self, &mut dispatch.state)
        }));
        self.conversation.finish_dispatch(dispatch);
        match result {
            Ok(value) => Some(value),
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    fn refresh_active_turn_permissions(&mut self) {
        self.conversation
            .refresh_active_permissions(self.core.permissions.snapshot());
    }

    fn agent_project_context(&self) -> protocol::AgentProjectContext {
        protocol::AgentProjectContext {
            cwd: self.core.env.cwd(),
            instructions: self.prompt_inputs.instructions.clone(),
            skill_section: self.prompt_inputs.skill_section.clone(),
            system_prompt_override: self.prompt_inputs.system_prompt_override.clone(),
            system_prompt: self.assemble_system_prompt(),
            tools: self.lua.tool_defs(
                self.core.config.mode.clone(),
                smelt_core::lua::ToolVisibility::Interactive,
            ),
        }
    }

    pub(crate) fn publish_agent_project_context(&self) {
        self.core
            .engine
            .send(protocol::UiCommand::UpdateAgentProjectContext(Box::new(
                self.agent_project_context(),
            )));
    }

    fn prepare_turn_context(&mut self) -> (String, Vec<protocol::ToolDef>) {
        let context = {
            let _perf = smelt_perf::perf::begin("agent:project_context");
            self.agent_project_context()
        };
        self.apply_pending_history_appends_for_request();
        (context.system_prompt, context.tools)
    }

    fn prepare_user_visible_turn(&mut self) {
        self.dismiss_notification_for_turn_start();
        self.clear_prompt_prediction();
        self.platform.set_sleep_inhibited(true);
        self.begin_turn();
        self.ensure_current_context_note();
        self.apply_pending_history_appends_for_request();
    }

    fn publish_turn_input(&mut self, submitted: Option<String>) {
        match submitted {
            Some(text) if !text.is_empty() => self.publish_input_submit(text),
            _ => self.invalidate_prompt_prediction(),
        }
    }

    fn continuation_target(&mut self) -> Option<smelt_store::TurnId> {
        match self.conversation.last_terminal_turn_id() {
            Some(turn_id) => Some(smelt_store::TurnId::new(turn_id)),
            None => {
                self.notify_turn_error_sticky(
                    "cannot continue: this session has no durable prior turn; submit a new request first"
                        .into(),
                );
                None
            }
        }
    }

    fn require_reopen_after_submit_failure(&mut self, cause: &crate::persist::PersistenceCause) {
        let reason = format!(
            "a canonical turn may already be durable; reopen the session before retrying ({})",
            cause.message
        );
        self.conversation.mark_read_only(reason);
    }

    fn expand_at_file_refs_in_text(&mut self, text: &str) -> String {
        smelt_core::file_ref::expand_at_file_refs(text, self.workspace.cwd(), &self.core.files)
    }

    fn expand_at_file_refs(&mut self, content: Content) -> Content {
        match content {
            Content::Text(text) => Content::Text(self.expand_at_file_refs_in_text(&text)),
            Content::Parts(parts) => Content::Parts(
                parts
                    .into_iter()
                    .map(|part| match part {
                        ContentPart::Text { text } => ContentPart::Text {
                            text: self.expand_at_file_refs_in_text(&text),
                        },
                        other => other,
                    })
                    .collect(),
            ),
        }
    }

    pub(crate) fn begin_agent_turn(
        &mut self,
        display: &str,
        content: Content,
    ) -> Option<TurnState> {
        let _perf = smelt_perf::perf::begin("agent:begin_turn");
        if self.block_read_only_mutation("submit a turn to this read-only session") {
            return None;
        }
        let model_target = self.resolve_model_target()?;
        let content = self.expand_at_file_refs(content);
        let text = content.text_content();
        let submitted = match smelt_buffer::text::trim_whitespace(&text) {
            "" => None,
            trimmed => Some(trimmed.to_string()),
        };
        if content.is_empty() {
            let continuation_of = self.continuation_target()?;
            self.prepare_user_visible_turn();
            self.publish_turn_input(submitted);
            let history = self.model_history_source();
            let submitted_history_idx = self.session_history_len().checked_sub(1)?;
            return self.dispatch_prepared_turn(PreparedTurn {
                input: protocol::StartTurnInput::user(content),
                history,
                kind: smelt_store::TurnKind::Continuation,
                submitted_history_idx: smelt_store::HistoryIndex::new(submitted_history_idx as u64),
                continuation_of: Some(continuation_of),
                model_target,
                request_config: self.core.config.request_runtime_config(),
                reasoning_effort: self.core.config.reasoning_effort.clone(),
                permission_overrides: None,
                permissions: self.core.permissions.snapshot(),
                request_overrides: Default::default(),
                rewind_history_idx: None,
                rollback: None,
            });
        }
        self.prepare_user_visible_turn();
        let rollback = StagedTurnRollback {
            history_len: Some(self.session_history_len()),
            transcript_len: Some(self.conversation.transcript().history().order.len()),
        };
        let first_user_message = self
            .conversation
            .session()
            .first_user_message
            .is_none()
            .then(|| text.clone().into_owned());
        let history = self.stage_request_history_item_with_first_user(
            protocol::history_item_from_user_content(content.clone()),
            Some(Block::User {
                text: display.to_string(),
                image_labels: content.image_labels(),
                command: false,
            }),
            first_user_message,
        );
        let submitted_history_idx = self.session_history_len().checked_sub(1)?;
        let rewind_history_idx = Some(submitted_history_idx);
        self.publish_turn_input(submitted);
        self.dispatch_prepared_turn(PreparedTurn {
            input: protocol::StartTurnInput::user(content),
            history,
            kind: smelt_store::TurnKind::User,
            submitted_history_idx: smelt_store::HistoryIndex::new(submitted_history_idx as u64),
            continuation_of: None,
            model_target,
            request_config: self.core.config.request_runtime_config(),
            reasoning_effort: self.core.config.reasoning_effort.clone(),
            permission_overrides: None,
            permissions: self.core.permissions.snapshot(),
            request_overrides: Default::default(),
            rewind_history_idx,
            rollback: Some(rollback),
        })
    }

    fn rollback_staged_turn(&mut self, rollback: &StagedTurnRollback) {
        if let Some(history_len) = rollback.history_len {
            self.rewind_session_history_to(history_len, false);
        }
        if let Some(transcript_len) = rollback.transcript_len {
            self.truncate_to(transcript_len);
        }
        self.sync_session_snapshot();
        self.publish_history_delta(HistoryDeltaKind::SubmitFailed);
    }

    fn dispatch_prepared_turn(&mut self, turn: PreparedTurn) -> Option<TurnState> {
        self.conversation
            .begin_prepared_turn(turn.request_overrides.clone());
        self.working.begin(TurnPhase::Working);

        self.core.signals.set_dyn(
            "turn_start",
            std::rc::Rc::new(smelt_core::signals::EventStub),
        );
        self.pump_lua();

        let (system_prompt, tools) = self.prepare_turn_context();
        let history = turn.history.clone();
        let dispatch = PreparedTurnDispatch {
            turn,
            system_prompt,
            tools,
            history,
        };
        if self.ephemeral() {
            let turn_id = self.conversation.next_turn_id();
            return self.finish_prepared_turn_dispatch(
                dispatch,
                turn_id,
                0,
                self.conversation.persistence_generation().get(),
                None,
                None,
            );
        }
        self.submit_prepared_turn_dispatch(dispatch)
    }

    fn submit_prepared_turn_dispatch(
        &mut self,
        dispatch: PreparedTurnDispatch,
    ) -> Option<TurnState> {
        let new_turn = smelt_store::NewTurn {
            kind: dispatch.turn.kind,
            submitted_history_idx: dispatch.turn.submitted_history_idx,
            continuation_of: dispatch.turn.continuation_of,
            created_at_ms: session::now_ms(),
        };
        match self.submit_canonical_turn(new_turn) {
            Ok(crate::app::conversation::CanonicalTurnSubmitOutcome::Durable(acknowledgement)) => {
                self.finish_prepared_turn_dispatch(
                    dispatch,
                    acknowledgement.receipt.turn_id.get(),
                    acknowledgement.receipt.session.current.revision.get(),
                    acknowledgement.persistence.generation.get(),
                    Some(std::time::Instant::now()),
                    acknowledgement.receipt.session.lineage_id.clone(),
                )
            }
            Ok(crate::app::conversation::CanonicalTurnSubmitOutcome::PendingPersistence {
                command_id,
                generation,
            }) => {
                self.defer_prepared_turn_dispatch(
                    command_id,
                    dispatch,
                    PendingTurnSubmitState::Persistence { generation },
                );
                None
            }
            Ok(crate::app::conversation::CanonicalTurnSubmitOutcome::PendingPreparation {
                command_id,
            }) => {
                self.defer_prepared_turn_dispatch(
                    command_id,
                    dispatch,
                    PendingTurnSubmitState::Preparation,
                );
                self.request_urgent_render();
                None
            }
            Err(cause) => {
                self.fail_prepared_turn_dispatch(dispatch, &cause);
                None
            }
        }
    }

    fn defer_prepared_turn_dispatch(
        &mut self,
        command_id: crate::persist::CanonicalCommandId,
        dispatch: PreparedTurnDispatch,
        submit_state: PendingTurnSubmitState,
    ) {
        debug_assert!(self.pending_turn_dispatch.is_none());
        self.pending_turn_dispatch = Some(PendingTurnDispatch {
            command_id,
            dispatch,
            submit_state,
            cancelled_meta: None,
        });
    }

    fn fail_prepared_turn_dispatch(
        &mut self,
        dispatch: PreparedTurnDispatch,
        cause: &crate::persist::PersistenceCause,
    ) {
        if cause.definitely_not_committed() {
            if let Some(rollback) = dispatch.turn.rollback.as_ref() {
                self.rollback_staged_turn(rollback);
            }
        } else if cause.requires_reopen() {
            self.require_reopen_after_submit_failure(cause);
        }
        self.platform.set_sleep_inhibited(false);
        self.working.finish(TurnOutcome::Errored);
        self.notify_session_persistence_failure(&self.conversation.session().id.clone(), cause);
    }

    fn finish_prepared_turn_dispatch(
        &mut self,
        dispatch: PreparedTurnDispatch,
        turn_id: u64,
        submitted_revision: u64,
        required_generation: u64,
        durable_receipt_at: Option<std::time::Instant>,
        committed_lineage_id: Option<String>,
    ) -> Option<TurnState> {
        let PreparedTurnDispatch {
            turn,
            system_prompt,
            tools,
            mut history,
        } = dispatch;
        let canonical = !self.ephemeral();
        if !matches!(turn.kind, smelt_store::TurnKind::Continuation) {
            if let Some(lineage_id) = committed_lineage_id {
                history = Self::store_model_history_source_for_committed_request(
                    &history,
                    lineage_id,
                    turn.submitted_history_idx.get() as usize,
                );
            }
        }
        self.conversation.record_started_turn(
            turn_id,
            self.core.config.mode.clone(),
            turn.reasoning_effort.clone(),
        );

        let permissions = turn.permissions.clone();
        let payload = protocol::StartTurnPayload {
            turn_id,
            input: turn.input,
            mode: self.core.config.mode.clone(),
            model_target: turn.model_target,
            request_config: turn.request_config,
            reasoning_effort: turn.reasoning_effort,
            fast_mode: self.fast_mode(),
            history,
            session_id: self.conversation.session().id.clone(),
            sessions_root: self.core.sessions.sessions_dir(),
            persistence: self
                .conversation
                .persistence_scope_at(required_generation, submitted_revision),
            permission_overrides: turn.permission_overrides,
            system_prompt: Some(system_prompt),
            tools,
        };
        if self
            .core
            .engine
            .try_send(UiCommand::StartTurn(Box::new(payload)))
            .is_err()
        {
            if canonical {
                let _ = self.enqueue_canonical_turn_transition(
                    smelt_store::TurnId::new(turn_id),
                    smelt_store::TurnState::Failed,
                    Some("engine_channel_rejected".into()),
                );
            }
            self.platform.set_sleep_inhibited(false);
            self.working.finish(TurnOutcome::Errored);
            self.notify_application_error_sticky(
                "engine stopped before accepting the request".into(),
            );
            return None;
        }
        for input in self.prompt.queued_request_inputs() {
            self.core.engine.send(UiCommand::Steer { input });
        }
        if let Some(durable_receipt_at) = durable_receipt_at {
            smelt_perf::perf::record_value(
                "persist:submit_turn:dispatch_after_receipt_ms",
                durable_receipt_at
                    .elapsed()
                    .as_millis()
                    .min(u128::from(u64::MAX)) as u64,
            );
        }
        smelt_perf::perf::record_value("agent:dispatch:start_turn:turn_id", turn_id);
        smelt_perf::perf::record_value("agent:dispatch:start_turn:revision", submitted_revision);
        smelt_perf::perf::record_value(
            "agent:dispatch:start_turn:at_us",
            smelt_perf::perf::timestamp_us(),
        );

        if canonical {
            if let Err(cause) = self.enqueue_canonical_turn_transition(
                smelt_store::TurnId::new(turn_id),
                smelt_store::TurnState::Running,
                None,
            ) {
                self.notify_session_persistence_failure(
                    &self.conversation.session().id.clone(),
                    &cause,
                );
            }
        }

        Some(TurnState {
            turn_id,
            canonical,
            pending: Vec::new(),
            permissions,
            submitted_history_idx: turn.submitted_history_idx.get() as usize,
            rewind_history_idx: turn.rewind_history_idx,
            assistant_output_started: false,
            _perf: smelt_perf::perf::begin("agent:turn"),
        })
    }

    pub(crate) fn turn_submission_is_pending(&self) -> bool {
        self.pending_turn_dispatch.is_some()
    }

    pub(super) fn abandon_pending_turn_submission(&mut self) {
        let Some(pending) = self.pending_turn_dispatch.take() else {
            return;
        };
        self.conversation
            .abandon_canonical_operation(pending.command_id);
    }

    pub(super) fn handle_canonical_submit_outcome(
        &mut self,
        outcome: crate::app::conversation::CanonicalTurnSubmitOutcome,
    ) -> bool {
        match outcome {
            crate::app::conversation::CanonicalTurnSubmitOutcome::Durable(acknowledgement) => {
                self.resume_turn_submission_after_persistence(*acknowledgement)
            }
            crate::app::conversation::CanonicalTurnSubmitOutcome::PendingPersistence {
                command_id,
                generation,
            } => {
                let Some(pending) = self.pending_turn_dispatch.as_mut() else {
                    return false;
                };
                if pending.command_id != command_id
                    || !matches!(pending.submit_state, PendingTurnSubmitState::Preparation)
                {
                    return false;
                }
                pending.submit_state = PendingTurnSubmitState::Persistence { generation };
                true
            }
            crate::app::conversation::CanonicalTurnSubmitOutcome::PendingPreparation { .. } => {
                false
            }
        }
    }

    pub(super) fn resume_turn_submission_after_persistence(
        &mut self,
        acknowledgement: crate::persist::SubmitTurnAcknowledgement,
    ) -> bool {
        let Some(pending) = self.pending_turn_dispatch.take() else {
            return false;
        };
        let matches_command = pending.command_id == acknowledgement.command_id;
        let matches_state = matches!(pending.submit_state, PendingTurnSubmitState::Preparation)
            || matches!(
                pending.submit_state,
                PendingTurnSubmitState::Persistence { generation }
                    if generation == acknowledgement.persistence.generation
            );
        if !matches_command || !matches_state {
            self.pending_turn_dispatch = Some(pending);
            return false;
        }
        if let Some(meta) = pending.cancelled_meta {
            self.record_finished_turn_state(meta);
            self.sync_agent_mode_applied();
            self.sync_reasoning_effort_applied();
            if let Err(cause) = self.enqueue_canonical_turn_transition(
                acknowledgement.receipt.turn_id,
                smelt_store::TurnState::Cancelled,
                Some("user_cancelled".into()),
            ) {
                self.notify_session_persistence_failure(
                    &self.conversation.session().id.clone(),
                    &cause,
                );
            }
            return true;
        }
        if let Some(turn) = self.finish_prepared_turn_dispatch(
            pending.dispatch,
            acknowledgement.receipt.turn_id.get(),
            acknowledgement.receipt.session.current.revision.get(),
            acknowledgement.persistence.generation.get(),
            Some(std::time::Instant::now()),
            acknowledgement.receipt.session.lineage_id.clone(),
        ) {
            self.conversation.set_active(Some(turn));
        }
        true
    }

    fn cancel_pending_turn_submission(
        &mut self,
        meta: protocol::TurnMeta,
    ) -> Option<TerminalCommitStatus> {
        match self.pending_turn_dispatch.as_ref()?.submit_state {
            PendingTurnSubmitState::Preparation => {
                let pending = self
                    .pending_turn_dispatch
                    .take()
                    .expect("pending prepared turn submission");
                self.conversation
                    .abandon_canonical_operation(pending.command_id);
                if let Some(rollback) = pending.dispatch.turn.rollback.as_ref() {
                    self.rollback_staged_turn(rollback);
                }
                Some(TerminalCommitStatus::Durable)
            }
            PendingTurnSubmitState::Persistence { .. } => {
                let pending = self
                    .pending_turn_dispatch
                    .as_mut()
                    .expect("pending persisted turn submission");
                if pending.cancelled_meta.is_none() {
                    pending.cancelled_meta = Some(meta);
                }
                Some(TerminalCommitStatus::Deferred)
            }
        }
    }

    pub(super) fn fail_pending_turn_submission(
        &mut self,
        command_id: Option<crate::persist::CanonicalCommandId>,
        cause: &crate::persist::PersistenceCause,
    ) -> bool {
        let should_fail = self.pending_turn_dispatch.as_ref().is_some_and(|pending| {
            command_id.is_none_or(|command_id| pending.command_id == command_id)
        });
        if !should_fail {
            return false;
        }
        let pending = self
            .pending_turn_dispatch
            .take()
            .expect("pending turn submission");
        self.conversation
            .abandon_canonical_operation(pending.command_id);
        self.fail_prepared_turn_dispatch(pending.dispatch, cause);
        true
    }

    pub(crate) fn begin_process_status_turn(
        &mut self,
        history_note: protocol::HistoryNote,
    ) -> Option<TurnState> {
        if self.block_read_only_mutation("submit a turn to this read-only session") {
            return None;
        }
        let model_target = self.resolve_model_target()?;
        self.invalidate_prompt_prediction();
        let lua = self.lua.execution();
        let block = crate::lua::scope_app(self, || {
            crate::app::history::history_note_to_block(&lua, &history_note)
        });
        let adds_history = !history_note.text().is_empty();
        let adds_block = block.is_some();
        if !adds_history && self.session_history_len() == 0 {
            return None;
        }
        self.prepare_user_visible_turn();
        let rollback = StagedTurnRollback {
            history_len: adds_history.then(|| self.session_history_len()),
            transcript_len: adds_block
                .then(|| self.conversation.transcript().history().order.len()),
        };
        let history = if adds_history {
            self.stage_request_history_item(HistoryItem::note(history_note.clone()), block)
        } else {
            if let Some(block) = block {
                self.push_block(block);
            }
            self.model_history_source()
        };
        let submitted_history_idx = self.session_history_len().checked_sub(1)?;
        let request_config = self.core.config.request_runtime_config();
        self.dispatch_prepared_turn(PreparedTurn {
            input: protocol::StartTurnInput::note(history_note),
            history,
            kind: smelt_store::TurnKind::Note,
            submitted_history_idx: smelt_store::HistoryIndex::new(submitted_history_idx as u64),
            continuation_of: None,
            model_target,
            request_config,
            reasoning_effort: self.core.config.reasoning_effort.clone(),
            permission_overrides: None,
            permissions: self.core.permissions.snapshot(),
            request_overrides: Default::default(),
            rewind_history_idx: None,
            rollback: (adds_history || adds_block).then_some(rollback),
        })
    }

    fn custom_command_parts(
        &self,
        cmd: smelt_core::custom_commands::CustomCommand,
    ) -> (
        String,
        String,
        smelt_core::custom_commands::CommandOverrides,
    ) {
        let evaluated = if self.core.config.settings.redact_secrets {
            engine::redact::redact(&cmd.body)
        } else {
            cmd.body
        };
        let display = if self.core.config.settings.redact_secrets {
            engine::redact::redact(&format!("/{}", cmd.display))
        } else {
            format!("/{}", cmd.display)
        };
        (display, evaluated, cmd.overrides)
    }

    pub(crate) fn begin_custom_command_turn(
        &mut self,
        cmd: smelt_core::custom_commands::CustomCommand,
    ) -> Option<TurnState> {
        let (display, evaluated, overrides) = self.custom_command_parts(cmd);
        self.begin_command_request_turn(display, evaluated, overrides, CommandTurnStart::Fresh)
    }

    pub(crate) fn begin_custom_command_continuation(
        &mut self,
        cmd: smelt_core::custom_commands::CustomCommand,
    ) -> Option<TurnState> {
        let (display, evaluated, overrides) = self.custom_command_parts(cmd);
        self.begin_command_request_turn(
            display,
            evaluated,
            overrides,
            CommandTurnStart::ContinueFromLast,
        )
    }

    fn resolve_command_model_target(
        &mut self,
        overrides: &smelt_core::custom_commands::CommandOverrides,
    ) -> Option<(protocol::ModelTarget, protocol::ModelCatalogMetadata)> {
        let target_model = overrides.model.as_deref();
        let target_provider = overrides.provider.as_deref();
        let resolved = match (target_model, target_provider) {
            (Some(reference), provider) => smelt_core::config::resolve_model_ref_with_provider(
                &self.core.config.available_models,
                reference,
                provider,
            ),
            (None, Some(provider)) => smelt_core::config::resolve_provider_ref(
                &self.core.config.available_models,
                provider,
            ),
            (None, None) => {
                let catalog = self
                    .core
                    .config
                    .active_model()
                    .map(|model| model.reasoning_catalog())
                    .unwrap_or_default();
                return self.resolve_model_target().map(|target| (target, catalog));
            }
        };
        let resolved = match resolved {
            Ok(model) => model.clone(),
            Err(error) => {
                self.notify_operation_error_sticky(
                    NotificationOperation::TurnStart,
                    error.to_string(),
                );
                return None;
            }
        };
        let api_key = self.resolve_api_key_for_env(&resolved.api_key_env)?;
        Some((resolved.target(api_key), resolved.reasoning_catalog()))
    }

    pub(crate) fn begin_command_request_turn(
        &mut self,
        display: String,
        evaluated: String,
        overrides: smelt_core::custom_commands::CommandOverrides,
        start: CommandTurnStart,
    ) -> Option<TurnState> {
        if self.block_read_only_mutation("submit a turn to this read-only session") {
            return None;
        }
        let submitted = match smelt_buffer::text::trim_whitespace(&evaluated) {
            "" => None,
            trimmed => Some(trimmed.to_string()),
        };
        let (mut model_target, model_catalog) = self.resolve_command_model_target(&overrides)?;
        let reasoning = if let Some(label) = overrides.reasoning_effort.as_deref() {
            let Some(effort) = protocol::ReasoningEffort::parse(label) else {
                self.notify_operation_error_sticky(
                    NotificationOperation::TurnStart,
                    "reasoning effort must not be empty".into(),
                );
                return None;
            };
            if !model_catalog.supports_reasoning_effort(&effort) {
                self.notify_operation_error_sticky(
                    NotificationOperation::TurnStart,
                    format!(
                        "reasoning effort '{}' is not supported by model '{}'",
                        effort.label(),
                        model_target.model
                    ),
                );
                return None;
            }
            effort
        } else {
            model_catalog.reconcile_reasoning_effort(self.core.config.reasoning_effort.clone())
        };
        let (kind, continuation_of) = match start {
            CommandTurnStart::Fresh => (smelt_store::TurnKind::Command, None),
            CommandTurnStart::ContinueFromLast => (
                smelt_store::TurnKind::Continuation,
                Some(self.continuation_target()?),
            ),
        };
        if evaluated.is_empty() && self.session_history_len() == 0 {
            return None;
        }
        self.prepare_user_visible_turn();
        let rollback = StagedTurnRollback {
            history_len: (!evaluated.is_empty()).then(|| self.session_history_len()),
            transcript_len: Some(self.conversation.transcript().history().order.len()),
        };

        let history = if !evaluated.is_empty() {
            let first_user_message = self
                .conversation
                .session()
                .first_user_message
                .is_none()
                .then(|| display.clone());
            self.stage_request_history_item_with_first_user(
                protocol::HistoryItem::user_command(
                    Content::text(evaluated.clone()),
                    display.clone(),
                ),
                Some(Block::User {
                    text: display.clone(),
                    image_labels: vec![],
                    command: true,
                }),
                first_user_message,
            )
        } else {
            if !display.is_empty() {
                self.push_block(Block::User {
                    text: display.clone(),
                    image_labels: vec![],
                    command: true,
                });
            }
            self.model_history_source()
        };
        self.publish_turn_input(submitted);

        let model_config_overrides = {
            let o = &overrides;
            if o.temperature.is_some()
                || o.top_p.is_some()
                || o.top_k.is_some()
                || o.min_p.is_some()
                || o.repeat_penalty.is_some()
            {
                Some(protocol::ModelConfigOverrides {
                    temperature: o.temperature,
                    top_p: o.top_p,
                    top_k: o.top_k,
                    min_p: o.min_p,
                    repeat_penalty: o.repeat_penalty,
                    tool_calling: None,
                    max_tokens: None,
                    thinking_budgets: None,
                })
            } else {
                None
            }
        };

        if let Some(ref request_overrides) = model_config_overrides {
            model_target.config = model_target.config.with_overrides(request_overrides);
        }

        let permission_overrides = {
            let o = &overrides;
            if o.tools.is_some() || !o.subcommands.is_empty() {
                let to_override =
                    |r: &smelt_core::custom_commands::RuleOverride| protocol::RuleSetOverride {
                        allow: r.allow.clone(),
                        ask: r.ask.clone(),
                        deny: r.deny.clone(),
                    };
                Some(protocol::PermissionOverrides {
                    tools: o.tools.as_ref().map(to_override),
                    subcommands: o
                        .subcommands
                        .iter()
                        .map(|(k, v)| (k.clone(), to_override(v)))
                        .collect(),
                })
            } else {
                None
            }
        };

        let current_permissions = self.core.permissions.snapshot();
        let permissions = permission_overrides
            .as_ref()
            .map(|overrides| std::sync::Arc::new(current_permissions.with_overrides(overrides)))
            .unwrap_or(current_permissions);

        if matches!(start, CommandTurnStart::ContinueFromLast) {
            self.working.continue_from_last(TurnPhase::Working);
        }

        let submitted_history_idx = self.session_history_len().checked_sub(1)?;
        let rewind_history_idx = (!evaluated.is_empty()).then_some(submitted_history_idx);
        let request_config = self.core.config.request_runtime_config();
        self.dispatch_prepared_turn(PreparedTurn {
            input: protocol::StartTurnInput::user_command(Content::text(evaluated), display),
            history,
            kind,
            submitted_history_idx: smelt_store::HistoryIndex::new(submitted_history_idx as u64),
            continuation_of,
            model_target,
            request_config,
            reasoning_effort: reasoning,
            permission_overrides,
            permissions,
            request_overrides: overrides,
            rewind_history_idx,
            rollback: Some(rollback),
        })
    }

    fn record_finished_turn_state(&mut self, ui_meta: protocol::TurnMeta) {
        let mut meta = match self.conversation.take_pending_meta() {
            Some(engine_meta) => protocol::TurnMeta {
                elapsed_ms: ui_meta.elapsed_ms,
                avg_tps: engine_meta.avg_tps.or(ui_meta.avg_tps),
                display_tps: engine_meta.display_tps.or(ui_meta.display_tps),
                interrupted: engine_meta.interrupted,
            },
            None => ui_meta,
        };
        if meta.display_tps.is_none() {
            meta.display_tps = meta.avg_tps.or_else(|| self.working.display_tps());
        }
        let history_len = self.session_history_len();
        let update_context_token_history_len = self.conversation.take_context_tokens_updated();
        self.conversation
            .finish_turn_state(history_len, meta, update_context_token_history_len);
    }

    fn cancel_turn_lua_tasks(&mut self) {
        self.lua.cancel_turn_tasks();
        self.discard_model_tool_cwd_change();
    }

    fn commit_terminal_turn(
        &mut self,
        turn_id: u64,
        state: smelt_store::TurnState,
        reason: Option<String>,
    ) -> TerminalCommitStatus {
        if self.ephemeral() {
            self.conversation.mark_terminal(turn_id);
            self.save_session();
            return TerminalCommitStatus::Durable;
        }
        match self.commit_canonical_turn_transition(
            smelt_store::TurnId::new(turn_id),
            state,
            reason,
        ) {
            Ok(crate::persist::TurnTransitionOutcome::Durable(_)) => {
                self.conversation.mark_terminal(turn_id);
                TerminalCommitStatus::Durable
            }
            Ok(crate::persist::TurnTransitionOutcome::Pending { .. }) => {
                self.request_urgent_render();
                TerminalCommitStatus::Deferred
            }
            Err(cause) => {
                self.notify_session_persistence_failure(
                    &self.conversation.session().id.clone(),
                    &cause,
                );
                TerminalCommitStatus::Deferred
            }
        }
    }

    /// Cancel tasks and invalidate late callbacks without finalizing the turn again.
    pub(crate) fn cancel_turn_work(&mut self) {
        self.cancel_request_hook();
        self.platform.set_sleep_inhibited(false);
        self.core.engine.send(UiCommand::Cancel);
        self.cancel_turn_lua_tasks();
        self.conversation.invalidate_turn_callbacks();
        self.busy_stack.clear();
        self.discard_pending_transcript_work();
        self.clear_compaction_preview();
    }

    /// Stop the engine turn without saving session or triggering auto-compact; used before rewind/clear.
    pub(crate) fn cancel_agent(&mut self) {
        let turn = self
            .conversation
            .active()
            .map(|turn| (turn.turn_id, turn.canonical));
        self.cancel_turn_work();
        self.conversation.set_turn_pause(Some(TurnPause {
            kind: Some(protocol::EngineAskErrorKind::Cancelled),
            retry_at_ms: None,
        }));
        // A turn is ending without going through `finish_turn`. Commit any
        // in-flight streaming buffers so the post-cancel state honors the
        // "no agent ⇒ no active stream" invariant (an empty thinking delta
        // arrived right before this can leave an empty `active_thinking`
        // sentinel that lingers past `agent = None`).
        self.flush_streaming_thinking();
        self.flush_streaming_text();
        self.clear_tool_drafts();
        self.conversation.clear_pending_history_appends();
        let meta = self.working.finish(TurnOutcome::Cancelled);
        self.record_finished_turn_state(meta);
        self.sync_agent_mode_applied();
        self.sync_reasoning_effort_applied();
        match turn {
            Some((turn_id, true)) => {
                if let Err(cause) = self.enqueue_canonical_turn_transition(
                    smelt_store::TurnId::new(turn_id),
                    smelt_store::TurnState::Cancelled,
                    Some("user_cancelled".into()),
                ) {
                    self.notify_session_persistence_failure(
                        &self.conversation.session().id.clone(),
                        &cause,
                    );
                }
            }
            Some((_, false)) | None => self.save_session(),
        }
        self.prompt.clear_queue();
    }

    pub(crate) fn resume_paused_turn(&mut self, token: Option<u64>) -> bool {
        let Some(pause) = self.conversation.turn_pause() else {
            return false;
        };
        if self.prompt_input_is_busy()
            || self.modal_blocks_agent()
            || token.is_some_and(|token| self.conversation.continuation_token() != Some(token))
        {
            return false;
        }
        let overrides = self.conversation.turn_request_overrides().clone();
        let turn = self.begin_command_request_turn(
            String::new(),
            String::new(),
            overrides,
            CommandTurnStart::ContinueFromLast,
        );
        let started = turn.is_some() || self.turn_submission_is_pending();
        self.conversation.set_active(turn);
        if !started {
            self.conversation.set_turn_pause(Some(pause));
            self.conversation.clear_continuation();
        }
        started
    }

    pub(crate) fn discard_turn(&mut self, end: crate::app::TurnEnd) -> TerminalCommitStatus {
        self.cancel_request_hook();
        let was_running = self.conversation.is_active();
        if was_running {
            let outcome = self.finish_turn_outcome(end);
            self.conversation.clear_active();
            if outcome.start_queued && outcome.terminal_commit.is_durable() {
                self.start_next_queued_input_if_idle();
            }
            outcome.terminal_commit
        } else if matches!(end, crate::app::TurnEnd::Cancelled) {
            if self.conversation.turn_pause().is_none() {
                self.conversation.set_turn_pause(Some(TurnPause {
                    kind: Some(protocol::EngineAskErrorKind::Cancelled),
                    retry_at_ms: None,
                }));
            }
            self.cancel_turn_work();
            // Archive an interrupted outcome so the prompt bar shows
            // "interrupted" rather than falling back to idle/done.
            let meta = self.working.finish(TurnOutcome::Cancelled);
            self.cancel_pending_turn_submission(meta)
                .unwrap_or(TerminalCommitStatus::Durable)
        } else {
            TerminalCommitStatus::Durable
        }
    }

    pub(crate) fn finish_turn(&mut self, end: crate::app::TurnEnd) -> bool {
        self.cancel_request_hook();
        let outcome = self.finish_turn_outcome(end);
        outcome.start_queued && outcome.terminal_commit.is_durable()
    }

    fn finish_turn_outcome(&mut self, end: crate::app::TurnEnd) -> FinishTurnOutcome {
        let _perf = smelt_perf::perf::begin("tui:finish_turn");
        use crate::app::TurnEnd;

        let turn = self
            .conversation
            .active()
            .map(|turn| (turn.turn_id, turn.canonical));
        let (terminal_state, terminal_reason) = match &end {
            TurnEnd::Complete => (smelt_store::TurnState::Completed, None),
            TurnEnd::Cancelled => (
                smelt_store::TurnState::Cancelled,
                Some("user_cancelled".to_string()),
            ),
            TurnEnd::Errored { kind, .. } => (
                smelt_store::TurnState::Failed,
                Some(kind.map_or_else(
                    || "engine_error".to_string(),
                    |kind| format!("engine_error:{}", kind.as_str()),
                )),
            ),
        };

        match end {
            TurnEnd::Cancelled => self.cancel_turn_work(),
            TurnEnd::Complete | TurnEnd::Errored { .. } => {
                self.platform.set_sleep_inhibited(false);
            }
        }

        let interrupted = !matches!(end, TurnEnd::Complete);
        let (error_kind, retry_at_ms) = match &end {
            TurnEnd::Errored { kind, retry_at_ms } => (*kind, *retry_at_ms),
            _ => (None, None),
        };
        self.conversation.set_turn_pause(match end {
            TurnEnd::Complete => None,
            TurnEnd::Cancelled => Some(TurnPause {
                kind: Some(protocol::EngineAskErrorKind::Cancelled),
                retry_at_ms: None,
            }),
            TurnEnd::Errored { kind, retry_at_ms } => Some(TurnPause { kind, retry_at_ms }),
        });
        let resumable = interrupted && is_resumable_turn_error(error_kind);
        let continuation_token = if !interrupted || resumable {
            Some(self.conversation.issue_continuation_token())
        } else {
            self.conversation.clear_continuation();
            None
        };
        self.core.signals.set_dyn(
            "turn_end",
            std::rc::Rc::new(smelt_core::signals::TurnEnd {
                cancelled: interrupted,
                continuation_token,
                error_kind: error_kind.map(|kind| kind.as_str().to_string()),
                retry_at_ms,
            }),
        );
        {
            let _perf = smelt_perf::perf::begin("tui:finish_turn:pump_lua");
            self.pump_lua();
        }
        {
            let _perf = smelt_perf::perf::begin("tui:finish_turn:flush_streams");
            self.flush_streaming_thinking();
            self.flush_streaming_text();
            self.clear_tool_drafts();
            self.finish_transcript_turn();
        }

        if matches!(end, TurnEnd::Complete) {
            // An append with follow-up delivery still pending at the turn boundary
            // was not included in an LLM request. Keep one as the follow-up trigger;
            // any others are committed below as context for that same turn.
            if let Some(note) = self.conversation.take_pending_follow_up_note() {
                self.prompt.queue_front(
                    crate::app::QueueStage::Turn,
                    crate::app::QueuedInput::ProcessStatus(note),
                );
            }
        }

        let (meta, start_queued) = {
            let _perf = smelt_perf::perf::begin("tui:finish_turn:working_finish");
            match end {
                TurnEnd::Complete => {
                    let start_queued = !self.prompt.queue_is_empty() && !self.busy_stack.is_busy();
                    let meta = if start_queued {
                        self.working
                            .finish_and_continue(TurnOutcome::Done, TurnPhase::Working)
                    } else {
                        self.working.finish(TurnOutcome::Done)
                    };
                    self.clear_prompt_prediction();
                    (meta, start_queued)
                }
                TurnEnd::Cancelled => {
                    self.conversation.retain_session_history_appends();
                    let meta = self.working.finish(TurnOutcome::Cancelled);
                    self.drain_queued_inputs_into_prompt();
                    self.restore_session_metadata_after_rewind(self.session_history_len());
                    (meta, false)
                }
                TurnEnd::Errored { .. } => {
                    let meta = self.working.finish(TurnOutcome::Errored);
                    (meta, false)
                }
            }
        };

        {
            let _perf = smelt_perf::perf::begin("tui:finish_turn:document_state");
            self.record_finished_turn_state(meta);
        }
        if matches!(end, TurnEnd::Complete | TurnEnd::Errored { .. }) {
            self.apply_pending_history_appends_for_request();
        }
        self.sync_agent_mode_applied();
        self.sync_reasoning_effort_applied();
        let terminal_commit = match turn {
            Some((turn_id, true)) => {
                self.commit_terminal_turn(turn_id, terminal_state, terminal_reason)
            }
            Some((turn_id, false)) => {
                self.conversation.mark_terminal(turn_id);
                if matches!(end, TurnEnd::Complete) {
                    self.schedule_session_save();
                } else {
                    self.save_session();
                }
                TerminalCommitStatus::Durable
            }
            None => {
                if matches!(end, TurnEnd::Complete) {
                    self.schedule_session_save();
                } else {
                    self.save_session();
                }
                TerminalCommitStatus::Durable
            }
        };
        FinishTurnOutcome {
            start_queued,
            terminal_commit,
        }
    }

    /// Invokes the Lua handler for a plugin-defined tool; synchronous handlers resolve immediately, async ones park until `drive_tasks` completes them.
    pub(crate) fn handle_tool_call(
        &mut self,
        request_id: u64,
        invocation_id: protocol::InvocationId,
        call_id: String,
        tool_name: String,
        args: std::collections::HashMap<String, serde_json::Value>,
    ) {
        let mode = self.core.config.mode.clone();
        let session_id = self.conversation.session().id.clone();
        let artifact_dir = self.current_artifact_dir();
        let now = self.core.clock.instant_now();
        let lua = self.lua.execution();
        let lua_call_id = call_id.clone();
        let (invocation, result) = crate::lua::scope_app(self, move || {
            lua.execute_tool_with_context(
                &tool_name,
                &args,
                crate::lua::ToolCallIds {
                    invocation_id,
                    request_id,
                    call_id: &lua_call_id,
                },
                crate::lua::ToolEnv {
                    mode,
                    session_id: &session_id,
                    artifact_dir: &artifact_dir,
                },
                now,
            )
        });
        match result {
            crate::lua::ToolExecResult::Immediate {
                content,
                is_error,
                metadata,
                display_content,
                attachment,
            } => {
                self.complete_lua_tool(
                    invocation,
                    call_id,
                    LuaToolCompletion {
                        content,
                        is_error,
                        metadata,
                        display_content,
                        attachment,
                    },
                );
            }
            crate::lua::ToolExecResult::Pending => {}
        }
    }

    pub(super) fn complete_lua_tool(
        &mut self,
        invocation: smelt_core::lua::ToolInvocationContext,
        call_id: String,
        completion: LuaToolCompletion,
    ) {
        let LuaToolCompletion {
            mut content,
            mut is_error,
            mut metadata,
            display_content,
            attachment,
        } = completion;
        match self.commit_tool_cwd_change(invocation, !is_error) {
            Ok(true) => {
                self.refresh_active_turn_permissions();
                annotate_cwd_metadata(&mut metadata, true, None);
            }
            Ok(false) => {}
            Err(error) => {
                if !content.trim().is_empty() {
                    content = format!("{error}\n\nOriginal tool result:\n{content}");
                } else {
                    content.clone_from(&error);
                }
                is_error = true;
                annotate_cwd_metadata(&mut metadata, false, Some(&error));
            }
        }
        self.core.engine.send(protocol::UiCommand::ToolResult {
            request_id: invocation.request_id,
            invocation_id: invocation.invocation_id,
            call_id,
            content,
            is_error,
            metadata,
            display_content,
            attachment: attachment.map(|attachment| *attachment),
        });
    }

    pub(crate) fn resolve_model_target(&mut self) -> Option<protocol::ModelTarget> {
        let Some(active) = self.core.config.active_model().cloned() else {
            self.notify_operation_error_sticky(
                NotificationOperation::TurnStart,
                "no model is available; configure a provider or wait for model refresh".into(),
            );
            return None;
        };
        let retry_missing_credentials = matches!(
            active.availability,
            smelt_core::ModelAvailability::Unavailable {
                reason: smelt_core::ModelUnavailableReason::MissingCredentials,
            }
        );
        if matches!(
            active.availability,
            smelt_core::ModelAvailability::Unavailable { .. }
        ) && !retry_missing_credentials
        {
            self.notify_operation_error_sticky(
                NotificationOperation::TurnStart,
                format!("model '{}' is unavailable", active.key),
            );
            return None;
        }
        let Some(api_key) = self.resolve_api_key_for_env(&active.api_key_env) else {
            if let Some(current) = self.core.config.active_model_mut() {
                current.availability = smelt_core::ModelAvailability::Unavailable {
                    reason: smelt_core::ModelUnavailableReason::MissingCredentials,
                };
                self.core.config.revision = self.core.config.revision.wrapping_add(1);
            }
            return None;
        };
        if retry_missing_credentials {
            if let Some(current) = self.core.config.active_model_mut() {
                current.availability = smelt_core::ModelAvailability::Available;
                self.core.config.revision = self.core.config.revision.wrapping_add(1);
            }
        }
        Some(active.target(api_key))
    }

    pub(crate) fn resolve_api_key_for_env(&mut self, key_env: &str) -> Option<String> {
        match lookup_api_key(key_env, |v| std::env::var(v)) {
            Ok(key) => Some(key),
            Err(err) => {
                self.notify_operation_error_sticky(NotificationOperation::TurnStart, err.message());
                None
            }
        }
    }

    pub(crate) fn handle_job_completed(&mut self, completion: smelt_core::process::JobCompletion) {
        let id = display_safe_process_id(&completion.id);
        let event = protocol::ProcessStatusEvent::background_process_completed(
            id,
            completion.exit_code,
            completion.termination,
        );
        self.handle_process_status_event(event);
    }

    fn handle_process_status_event(&mut self, event: protocol::ProcessStatusEvent) {
        let note = protocol::HistoryNote::process_status_event(event);
        if self.agent_is_running() || self.conversation.turn_pause().is_some() {
            self.queue_history_append(crate::app::PendingHistoryAppend::process_status(note));
        } else if self.prompt_input_is_busy() || !self.prompt.queue_is_empty() {
            self.prompt
                .try_queue_turn(crate::app::QueuedInput::ProcessStatus(note));
        } else {
            let turn = self.begin_process_status_turn(note);
            self.conversation.set_active(turn);
        }
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn session_path_grants(&self) -> Vec<smelt_core::permissions::SessionPathGrant> {
        self.core
            .permissions
            .approvals()
            .read()
            .unwrap()
            .session_path_grants()
    }

    pub(crate) fn grant_session_path(
        &mut self,
        mode: Option<protocol::AgentMode>,
        tool: String,
        access: smelt_core::permissions::PathAccess,
        dir: PathBuf,
    ) {
        let approval_store = self.core.permissions.approvals();
        let mut approvals = approval_store.write().unwrap();
        if let Some(mode) = mode {
            approvals.add_session_path_grant(mode, tool, access, dir);
        } else {
            approvals.add_session_path_trust(tool, access, dir);
        }
    }

    pub(crate) fn sync_permissions(
        &mut self,
        session_entries: Option<Vec<PermissionEntry>>,
        session_path_grants: Option<Vec<smelt_core::permissions::SessionPathGrant>>,
        workspace: Option<smelt_core::permissions::store::Replacement>,
        repository: Option<smelt_core::permissions::store::Replacement>,
    ) -> Result<(), String> {
        if workspace.is_some() && repository.is_some() {
            return Err(
                "sync one persisted permission scope at a time so each replacement is atomic"
                    .into(),
            );
        }

        let session_entries = session_entries
            .map(|entries| {
                let mut tools = Vec::new();
                let mut dirs = Vec::new();
                for entry in entries {
                    if entry.tool.is_empty() {
                        return Err("session permission tool name cannot be empty".to_string());
                    }
                    if entry.tool == "directory" {
                        if entry.pattern.is_empty() {
                            return Err("session permission directory cannot be empty".to_string());
                        }
                        dirs.push(PathBuf::from(entry.pattern));
                    } else {
                        let pattern = (entry.pattern != "*").then_some(entry.pattern);
                        if let Some(pattern) = pattern.as_deref() {
                            glob::Pattern::new(pattern).map_err(|error| {
                                format!("invalid session pattern for {}: {error}", entry.tool)
                            })?;
                        }
                        tools.push(smelt_core::permissions::SessionToolApproval {
                            tool: entry.tool,
                            pattern,
                        });
                    }
                }
                Ok((tools, dirs))
            })
            .transpose()?;

        if let Some(replacement) = workspace {
            self.core
                .permission_store
                .replace(
                    self.workspace.cwd(),
                    smelt_core::permissions::store::PersistenceScope::Workspace,
                    replacement,
                )
                .map_err(|error| format!("replace workspace permissions: {error}"))?;
        }
        if let Some(replacement) = repository {
            let repository_key = self
                .workspace
                .repository_permission_context()
                .map(|(key, _)| key.to_path_buf())
                .ok_or_else(|| "repository permissions require a Git repository".to_string())?;
            self.core
                .permission_store
                .replace(
                    &repository_key.to_string_lossy(),
                    smelt_core::permissions::store::PersistenceScope::Repository,
                    replacement,
                )
                .map_err(|error| format!("replace repository permissions: {error}"))?;
        }

        if session_entries.is_some() || session_path_grants.is_some() {
            let approval_store = self.core.permissions.approvals();
            let mut approvals = approval_store
                .write()
                .unwrap_or_else(|error| error.into_inner());
            if let Some((tools, dirs)) = session_entries {
                approvals.set_session_entries(tools, dirs);
            }
            if let Some(path_grants) = session_path_grants {
                approvals.set_session_path_grants(path_grants);
            }
        }
        Ok(())
    }

    pub(crate) fn revoke_permission(
        &mut self,
        scope: &str,
        tool: &str,
        pattern: &str,
    ) -> Result<bool, String> {
        if scope == "session" {
            return Ok(self
                .core
                .permissions
                .approvals()
                .write()
                .unwrap_or_else(|error| error.into_inner())
                .remove_session_entry(tool, pattern));
        }

        let (root, persistence_scope) = match scope {
            "workspace" => (
                std::path::PathBuf::from(self.workspace.cwd()),
                smelt_core::permissions::store::PersistenceScope::Workspace,
            ),
            "repository" => (
                self.workspace
                    .repository_permission_context()
                    .map(|(key, _)| key.to_path_buf())
                    .ok_or_else(|| "repository permissions require a Git repository".to_string())?,
                smelt_core::permissions::store::PersistenceScope::Repository,
            ),
            _ => return Err(format!("unsupported permission scope `{scope}`")),
        };
        self.core
            .permission_store
            .remove(&root.to_string_lossy(), persistence_scope, tool, pattern)
            .map_err(|error| format!("remove {scope} permission: {error}"))
    }

    pub(crate) fn clear_session_scoped_permissions_for_session_boundary(&mut self) {
        self.core
            .permissions
            .approvals()
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .clear_session();
    }

    fn reject_failed_persistent_permission(
        &mut self,
        invocation_id: protocol::InvocationId,
        request_id: u64,
        error: String,
    ) -> bool {
        let message = format!("permission was not saved: {error}");
        self.notify_workspace_error_sticky(message.clone());
        self.send_permission_decision(request_id, false, Some(message));
        self.finish_tool(invocation_id, ToolStatus::Denied, None, None);
        self.conversation.remove_pending_tool(invocation_id);
        false
    }

    /// Resolves a confirm dialog choice; returns `true` if the agent should be cancelled.
    pub(crate) fn resolve_confirm(
        &mut self,
        (choice, message): (ConfirmChoice, Option<String>),
        invocation_id: protocol::InvocationId,
        request_id: u64,
        tool_name: &str,
    ) -> bool {
        let label = match &choice {
            ConfirmChoice::Yes => "approved",
            ConfirmChoice::Grant(option) => option.label.as_str(),
            ConfirmChoice::No => "denied",
        };
        if let Some(ref msg) = message {
            self.set_active_user_message(invocation_id, format!("{label}: {msg}"));
        }
        match choice {
            ConfirmChoice::Yes => {
                self.set_active_status(invocation_id, ToolStatus::Pending);
                self.send_permission_decision(request_id, true, message);
                false
            }
            ConfirmChoice::Grant(option) => {
                match option.target {
                    ApprovalTarget::Session => {
                        let approval_store = self.core.permissions.approvals();
                        let mut approvals = approval_store.write().unwrap();
                        for grant in option.grants {
                            approvals.add_session_grant(grant);
                        }
                    }
                    ApprovalTarget::Workspace { root } => {
                        let result = add_persisted_grants(
                            &self.core.permission_store,
                            &root,
                            smelt_core::permissions::store::PersistenceScope::Workspace,
                            &option.grants,
                        );
                        if let Err(error) = result {
                            return self.reject_failed_persistent_permission(
                                invocation_id,
                                request_id,
                                error,
                            );
                        }
                    }
                    ApprovalTarget::Repository { key } => {
                        let result = add_persisted_grants(
                            &self.core.permission_store,
                            &key,
                            smelt_core::permissions::store::PersistenceScope::Repository,
                            &option.grants,
                        );
                        if let Err(error) = result {
                            return self.reject_failed_persistent_permission(
                                invocation_id,
                                request_id,
                                error,
                            );
                        }
                    }
                }
                self.set_active_status(invocation_id, ToolStatus::Pending);
                self.send_permission_decision(request_id, true, message);
                false
            }
            ConfirmChoice::No => {
                let has_message = message.is_some();
                self.send_permission_decision(request_id, false, message);
                self.finish_tool(invocation_id, ToolStatus::Denied, None, None);
                if has_message {
                    self.conversation.remove_pending_tool(invocation_id);
                    false
                } else {
                    engine::log::entry(
                        engine::log::Level::Info,
                        "agent_stop",
                        &serde_json::json!({
                            "reason": "confirm_denied",
                            "tool": tool_name,
                        }),
                    );
                    self.conversation.clear_pending_tools();
                    true
                }
            }
        }
    }

    fn permission_decision_for_confirm(&self, req: &ConfirmRequest) -> Decision {
        if let Some(error) = req.tool_paths_error.as_ref() {
            return Decision::Error(error.clone());
        }
        self.active_permissions()
            .evaluate_tool_with_paths_and_approvals(
                self.conversation.applied_mode().clone(),
                smelt_core::permissions::ToolOrigin::Lua,
                &req.tool_name,
                &req.args,
                &req.tool_paths,
            )
            .decision
    }

    fn resolve_confirm_by_policy(
        &mut self,
        req: &ConfirmRequest,
        approved: bool,
        handle_id: Option<u64>,
        label: &'static str,
        pending: Option<&mut Vec<PendingTool>>,
    ) {
        if let Some(handle_id) = handle_id {
            self.core.confirms.take(handle_id);
            self.core.signals.set_dyn(
                "confirm_resolved",
                std::rc::Rc::new(smelt_core::signals::ConfirmResolved {
                    handle_id,
                    decision: label.into(),
                }),
            );
        }

        if approved {
            self.set_active_status(req.invocation_id, ToolStatus::Pending);
            self.send_permission_decision(req.request_id, true, None);
        } else {
            self.send_permission_decision(req.request_id, false, None);
            self.finish_tool(req.invocation_id, ToolStatus::Denied, None, None);
            if let Some(pending) = pending {
                pending.retain(|pending| pending.invocation_id != req.invocation_id);
            } else {
                self.conversation.remove_pending_tool(req.invocation_id);
            }
        }
    }

    pub(crate) fn resolve_open_confirm_for_current_mode(&mut self, handle_id: u64) -> bool {
        let Some(req) = self
            .core
            .confirms
            .get(handle_id)
            .map(|entry| entry.req.clone())
        else {
            return false;
        };

        match self.permission_decision_for_confirm(&req) {
            Decision::Allow => {
                self.resolve_confirm_by_policy(&req, true, Some(handle_id), "auto_allow", None);
                true
            }
            Decision::Deny => {
                self.resolve_confirm_by_policy(&req, false, Some(handle_id), "auto_deny", None);
                true
            }
            Decision::Error(error) => {
                self.notify_workspace_error_sticky(error);
                false
            }
            Decision::Ask => false,
        }
    }

    /// Dispatches one engine-event control signal; returns the same
    /// `SessionControl` variant so callers can decide whether to continue
    /// draining, end the turn, or surface an error.
    pub(crate) fn dispatch_control(
        &mut self,
        ctrl: SessionControl,
        turn: &mut TurnState,
    ) -> SessionControl {
        let should_queue = self
            .timers
            .last_keypress
            .is_some_and(|t| t.elapsed() < Duration::from_millis(CONFIRM_DEFER_MS))
            && !self.prompt_buf().source().is_empty();

        match ctrl {
            SessionControl::Continue => SessionControl::Continue,
            SessionControl::Done => SessionControl::Done,
            SessionControl::Error { kind, retry_at_ms } => {
                SessionControl::Error { kind, retry_at_ms }
            }
            SessionControl::NeedsConfirm(mut req) => {
                if req.tool_name.is_empty() {
                    req.tool_name = turn
                        .pending
                        .last()
                        .map(|p| p.name.clone())
                        .unwrap_or_default();
                }
                if let Some(error) = req.tool_paths_error.clone() {
                    self.send_permission_decision(req.request_id, false, Some(error));
                    self.finish_tool(req.invocation_id, ToolStatus::Denied, None, None);
                    turn.pending
                        .retain(|pending| pending.invocation_id != req.invocation_id);
                    return SessionControl::Continue;
                }

                let outcome = turn.permissions.evaluate_tool_with_paths_and_approvals(
                    self.conversation.applied_mode().clone(),
                    smelt_core::permissions::ToolOrigin::Lua,
                    &req.tool_name,
                    &req.args,
                    &req.tool_paths,
                );
                match outcome.decision {
                    Decision::Allow => {
                        self.resolve_confirm_by_policy(&req, true, None, "auto_allow", None);
                        return SessionControl::Continue;
                    }
                    Decision::Deny => {
                        self.resolve_confirm_by_policy(
                            &req,
                            false,
                            None,
                            "auto_deny",
                            Some(&mut turn.pending),
                        );
                        return SessionControl::Continue;
                    }
                    Decision::Error(ref error) => {
                        self.notify_workspace_error_sticky(error.clone());
                    }
                    Decision::Ask => {}
                }

                if should_queue {
                    self.set_active_status(req.invocation_id, ToolStatus::Confirm);
                    self.overlays.defer_confirm(req);
                    return SessionControl::Continue;
                }

                let options = turn.permissions.approval_options(
                    &req.tool_name,
                    &req.approval_candidates,
                    &outcome,
                );
                req.grant_options = confirm_grant_options(
                    options.grant_sets,
                    self.workspace.cwd(),
                    self.workspace.repository_permission_context(),
                    self.core.env.home(),
                );

                self.close_focused_non_blocking_overlay();
                self.set_active_status(req.invocation_id, ToolStatus::Confirm);

                let snapshot = smelt_core::signals::ConfirmRequested {
                    handle_id: 0,
                    tool_name: req.tool_name.clone(),
                    summary: req.summary.clone(),
                    args: req.args.clone(),
                    grant_options: req.grant_options.clone(),
                };
                let handle_id = self.core.confirms.register(*req);
                self.core.signals.set_dyn(
                    "confirm_requested",
                    std::rc::Rc::new(smelt_core::signals::ConfirmRequested {
                        handle_id,
                        ..snapshot
                    }),
                );
                let lua = self.lua.execution();
                crate::lua::scope_app(self, move || lua.fire_confirm_open(handle_id));
                SessionControl::Continue
            }
        }
    }
}

fn confirm_grant_options(
    grant_sets: Vec<Vec<smelt_core::permissions::PermissionGrant>>,
    cwd: &str,
    repository: Option<(&std::path::Path, &std::path::Path)>,
    home: &std::path::Path,
) -> Vec<smelt_core::transcript_model::ConfirmApprovalOption> {
    let mut out = Vec::new();
    for (idx, grants) in grant_sets.into_iter().enumerate() {
        let subject = smelt_core::permissions::PermissionGrant::display_subjects(&grants, home);
        out.push(smelt_core::transcript_model::ConfirmApprovalOption {
            id: format!("grant_{idx}_session"),
            label: format!("allow {subject} for this session"),
            target: ApprovalTarget::Session,
            grants: grants.clone(),
        });
        out.push(smelt_core::transcript_model::ConfirmApprovalOption {
            id: format!("grant_{idx}_workspace"),
            label: format!(
                "allow {subject} in {} {}",
                if repository.is_some() {
                    "this worktree"
                } else {
                    "workspace"
                },
                pretty_cwd(cwd, home)
            ),
            target: ApprovalTarget::Workspace {
                root: std::path::PathBuf::from(cwd),
            },
            grants: grants.clone(),
        });
        if let Some((repository_key, display_root)) = repository {
            out.push(smelt_core::transcript_model::ConfirmApprovalOption {
                id: format!("grant_{idx}_repository"),
                label: format!(
                    "allow {subject} in project {} (all worktrees)",
                    pretty_path(display_root, home)
                ),
                target: ApprovalTarget::Repository {
                    key: repository_key.to_path_buf(),
                },
                grants,
            });
        }
    }
    out
}

fn pretty_cwd(cwd: &str, home: &std::path::Path) -> String {
    pretty_path(std::path::Path::new(cwd), home)
}

fn pretty_path(path: &std::path::Path, home: &std::path::Path) -> String {
    engine::paths::collapse_tilde_from(path, home)
        .to_string_lossy()
        .into_owned()
}

fn add_persisted_grants(
    store: &smelt_core::permissions::store::PermissionStore,
    root: &std::path::Path,
    scope: smelt_core::permissions::store::PersistenceScope,
    grants: &[smelt_core::permissions::PermissionGrant],
) -> Result<(), String> {
    store
        .add_grants(&root.to_string_lossy(), scope, grants)
        .map_err(|error| format!("save persisted permission: {error}"))
}

fn display_safe_process_id(id: &str) -> String {
    id.chars()
        .map(|ch| if ch.is_control() { '\u{FFFD}' } else { ch })
        .collect()
}

/// Reason an API-key env lookup failed; carries enough context for the
/// caller to surface a user-facing error.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ApiKeyError {
    /// The env var is not set in the process environment.
    NotSet { var: String },
    /// The env var exists but its bytes are not valid Unicode.
    NotUnicode { var: String },
}

impl ApiKeyError {
    pub(crate) fn message(&self) -> String {
        match self {
            ApiKeyError::NotSet { var } => format!(
                "environment variable '{var}' is not set but is required for API authentication"
            ),
            ApiKeyError::NotUnicode { var } => format!(
                "environment variable '{var}' contains non-Unicode data and cannot be used as an API key"
            ),
        }
    }
}

/// Look up an API key value via the injected `get_env` resolver.
///
/// An empty `key_env` is treated as "no auth required" and returns an empty
/// key. Otherwise the resolver is queried; `VarError::NotPresent` and
/// `NotUnicode` map to typed errors so the dispatcher can format a stable
/// user-facing message.
///
/// The resolver indirection keeps tests off the process-global env.
pub(crate) fn lookup_api_key(
    key_env: &str,
    get_env: impl FnOnce(&str) -> Result<String, std::env::VarError>,
) -> Result<String, ApiKeyError> {
    if key_env.is_empty() {
        return Ok(String::new());
    }
    match get_env(key_env) {
        Ok(key) => Ok(key),
        Err(std::env::VarError::NotPresent) => Err(ApiKeyError::NotSet {
            var: key_env.to_string(),
        }),
        Err(std::env::VarError::NotUnicode(_)) => Err(ApiKeyError::NotUnicode {
            var: key_env.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lineage_reader(
        app: &crate::app::test_harness::TestApp,
    ) -> smelt_store::LineageSessionReader {
        smelt_store::LineageSessionReader::open_existing(
            app.app.core.sessions.sessions_dir(),
            &app.app.conversation.session().id,
        )
        .expect("open canonical lineage session")
    }

    fn lineage_history(reader: &smelt_store::LineageSessionReader) -> Vec<protocol::HistoryItem> {
        let head = reader.snapshot().expect("read lineage state").head;
        reader
            .history_range(0, head.history_len.get())
            .expect("read lineage history")
    }

    fn lineage_turn(
        reader: &smelt_store::LineageSessionReader,
        turn_id: u64,
    ) -> smelt_store::StoredTurn {
        reader
            .turns()
            .expect("read lineage turns")
            .into_iter()
            .find(|turn| turn.turn_id == smelt_store::TurnId::new(turn_id))
            .expect("stored lineage turn")
    }

    fn save_record_backed_session(
        app: &crate::app::test_harness::TestApp,
        session: &smelt_core::session::Session,
        transcript_text: &str,
    ) {
        let receipt = app
            .app
            .core
            .sessions
            .save_result(session)
            .expect("save canonical session");
        let address = smelt_core::session::SessionStoreAddress::new(
            app.app.core.sessions.sessions_dir(),
            session.id.clone(),
            receipt.lineage_id.expect("saved session lineage"),
        );
        let mut transcript = smelt_core::content::transcript::Transcript::new();
        transcript.push(smelt_core::Block::Text {
            content: transcript_text.into(),
        });
        crate::persist::write_transcript_record_suffix(
            &address,
            0,
            &transcript.history.block_records(),
        )
        .expect("save canonical transcript records");
    }

    fn assert_catalog_converged(app: &crate::app::test_harness::TestApp) {
        assert!(app
            .app
            .core
            .sessions
            .wait_for_session_catalog(std::time::Duration::from_secs(5)));
        let canonical_revision = lineage_reader(app)
            .snapshot()
            .expect("read canonical session")
            .head
            .revision
            .get();
        let catalog_session = smelt_store::CatalogReader::open_existing(
            app.app.core.sessions.layout().catalog_path(),
        )
        .expect("open repaired session catalog")
        .expect("session catalog exists")
        .session(&app.app.conversation.session().id)
        .expect("read repaired catalog session")
        .expect("repaired catalog session exists");
        assert_eq!(catalog_session.source_revision, canonical_revision);
    }

    #[test]
    fn turn_lifecycle_owns_continuation_and_callback_generations() {
        let mut lifecycle = TurnLifecycle::new(
            protocol::AgentMode::parse("normal").unwrap(),
            protocol::ReasoningEffort::Off,
        );

        assert_eq!(lifecycle.cancel_generation(), 0);
        lifecycle.invalidate_turn_callbacks();
        assert_eq!(lifecycle.cancel_generation(), 1);

        let token = lifecycle.issue_continuation_token();
        assert_eq!(token, 1);
        assert!(!lifecycle.consume_continuation(token + 1));
        assert_eq!(lifecycle.pending_continuation_token(), Some(token));
        assert!(lifecycle.consume_continuation(token));
        assert_eq!(lifecycle.pending_continuation_token(), None);
    }

    #[test]
    fn turn_lifecycle_replaces_mode_changes_and_removes_base_mode() {
        let base = protocol::AgentMode::parse("normal").unwrap();
        let mut lifecycle = TurnLifecycle::new(base.clone(), protocol::ReasoningEffort::Off);

        lifecycle.queue_history_append(
            PendingHistoryAppend::mode_change("plan".to_string(), "plan mode".to_string()),
            Some(&base),
        );
        lifecycle.queue_history_append(
            PendingHistoryAppend::mode_change("auto".to_string(), "auto mode".to_string()),
            Some(&base),
        );

        assert_eq!(lifecycle.pending_history_append_count(), 1);
        assert_eq!(lifecycle.pending_history_appends()[0].mode(), Some("auto"));

        lifecycle.queue_history_append(
            PendingHistoryAppend::mode_change("normal".to_string(), "normal mode".to_string()),
            Some(&base),
        );
        assert_eq!(lifecycle.pending_history_append_count(), 0);
    }

    #[test]
    fn turn_lifecycle_replaces_and_removes_named_context() {
        let mut lifecycle = TurnLifecycle::new(
            protocol::AgentMode::parse("normal").unwrap(),
            protocol::ReasoningEffort::Off,
        );

        lifecycle.replace_or_push_history_append(PendingHistoryAppend::context(
            "project".to_string(),
            "first".to_string(),
        ));
        lifecycle.replace_or_push_history_append(PendingHistoryAppend::context(
            "project".to_string(),
            "second".to_string(),
        ));
        assert_eq!(lifecycle.pending_history_append_count(), 1);
        assert_eq!(
            lifecycle.pending_context_note("project"),
            Some(Some("second"))
        );

        lifecycle.replace_or_push_history_append(PendingHistoryAppend::clear_context(
            "project".to_string(),
        ));
        assert_eq!(lifecycle.pending_history_append_count(), 1);
        assert_eq!(lifecycle.pending_context_note("project"), Some(None));
        assert_eq!(lifecycle.pending_context_note("other"), None);
    }

    #[test]
    fn conversation_cancels_context_updates_that_restore_committed_state() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.conversation.append_history_item(HistoryItem::note(
            protocol::HistoryNote::named_context("project", "committed"),
        ));

        app.app
            .set_context_note("project".into(), Some("pending".into()));
        assert_eq!(app.app.conversation.pending_history_append_count(), 1);
        app.app
            .set_context_note("project".into(), Some("committed".into()));
        assert_eq!(app.app.conversation.pending_history_append_count(), 0);

        app.app.set_context_note("project".into(), None);
        assert_eq!(app.app.conversation.pending_history_append_count(), 1);
        app.app
            .set_context_note("project".into(), Some("committed".into()));
        assert_eq!(app.app.conversation.pending_history_append_count(), 0);
    }

    #[test]
    fn turn_lifecycle_extracts_matching_history_append() {
        let mut lifecycle = TurnLifecycle::new(
            protocol::AgentMode::parse("normal").unwrap(),
            protocol::ReasoningEffort::Off,
        );
        let append = PendingHistoryAppend::context("project".to_string(), "text".to_string());
        let item = append.history_item();
        lifecycle.replace_or_push_history_append(append);

        let extracted = lifecycle
            .take_matching_history_append(&item)
            .expect("matching append exists");
        assert_eq!(extracted.history_item(), item);
        assert_eq!(lifecycle.pending_history_append_count(), 0);
    }

    #[test]
    fn turn_lifecycle_retains_only_session_scoped_history() {
        let base = protocol::AgentMode::parse("normal").unwrap();
        let mut lifecycle = TurnLifecycle::new(base.clone(), protocol::ReasoningEffort::Off);
        lifecycle.queue_history_append(
            PendingHistoryAppend::mode_change("plan".to_string(), "plan mode".to_string()),
            Some(&base),
        );
        lifecycle.replace_or_push_history_append(PendingHistoryAppend::context(
            "project".to_string(),
            "text".to_string(),
        ));

        lifecycle.retain_session_history_appends();

        assert_eq!(lifecycle.pending_history_append_count(), 1);
        assert_eq!(lifecycle.pending_history_appends()[0].mode(), Some("plan"));
    }

    #[test]
    fn turn_lifecycle_keeps_active_identity_while_state_is_dispatched() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.start_turn(41);

        let dispatch = app
            .app
            .conversation
            .begin_dispatch()
            .expect("active turn enters dispatch");
        assert!(!app.app.conversation.is_active());
        assert_eq!(app.app.conversation.active_id(), Some(41));

        app.app.conversation.finish_dispatch(dispatch);
        assert_eq!(
            app.app.conversation.active().map(|turn| turn.turn_id),
            Some(41)
        );
    }

    #[test]
    fn dispatched_turn_is_restored_before_panic_resumes() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.start_turn(42);

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            app.app.with_dispatched_turn::<()>(|_, _| panic!("boom"));
        }));

        assert!(panic.is_err());
        assert_eq!(app.app.conversation.active_id(), Some(42));
        assert_eq!(
            app.app.conversation.active().map(|turn| turn.turn_id),
            Some(42)
        );
    }

    fn process_status_blocks(app: &crate::app::test_harness::TestApp) -> Vec<String> {
        let history = app.app.conversation.transcript().history();
        history
            .order
            .iter()
            .filter_map(|id| history.block(*id))
            .filter_map(|block| match block {
                Block::ProcessStatus { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    fn user_blocks(app: &crate::app::test_harness::TestApp) -> Vec<String> {
        let history = app.app.conversation.transcript().history();
        history
            .order
            .iter()
            .filter_map(|id| history.block(*id))
            .filter_map(|block| match block {
                Block::User { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    fn title_request_from_actions(app: &crate::app::test_harness::TestApp) -> u64 {
        app.actions()
            .iter()
            .find_map(|action| match action {
                crate::app::test_harness::Action::EngineSend(command) => match command.as_ref() {
                    protocol::UiCommand::EngineAsk { id, .. } => Some(*id),
                    _ => None,
                },
                _ => None,
            })
            .expect("submission should request a title")
    }

    fn complete_active_turn_with_assistant(
        app: &mut crate::app::test_harness::TestApp,
        text: &str,
    ) {
        let turn_id = app.current_turn_id().expect("turn is active");
        let first_index = app.app.session_history_len();
        app.feed_one(crate::app::test_harness::SourceEvent::engine(
            protocol::EngineEvent::HistoryAppended {
                turn_id,
                delta: protocol::CanonicalHistoryDelta::new(
                    first_index,
                    vec![HistoryItem::assistant(protocol::AssistantStep::terminal(
                        Some(Content::text(text)),
                        None,
                        Vec::new(),
                    ))],
                ),
            },
        ));
        assert!(app.finish_turn());
    }

    fn perf_value_max(snapshot: &smelt_perf::perf::Snapshot, label: &str) -> u64 {
        snapshot
            .values
            .iter()
            .find(|row| row.label == label)
            .map(|row| row.max)
            .unwrap_or(0)
    }

    fn perf_value_count(snapshot: &smelt_perf::perf::Snapshot, label: &str) -> usize {
        snapshot
            .values
            .iter()
            .find(|row| row.label == label)
            .map(|row| row.count)
            .unwrap_or(0)
    }

    fn perf_value_total(snapshot: &smelt_perf::perf::Snapshot, label: &str) -> u64 {
        snapshot
            .values
            .iter()
            .find(|row| row.label == label)
            .map(|row| row.total)
            .unwrap_or(0)
    }

    fn perf_value_last(snapshot: &smelt_perf::perf::Snapshot, label: &str) -> Option<u64> {
        snapshot
            .values
            .iter()
            .find(|row| row.label == label)
            .map(|row| row.last)
    }

    fn assert_perf_value_absent(snapshot: &smelt_perf::perf::Snapshot, label: &str) {
        let value = perf_value_max(snapshot, label);
        assert_eq!(value, 0, "{label} recorded {value}, expected no samples");
    }

    fn assert_perf_value_at_most(snapshot: &smelt_perf::perf::Snapshot, label: &str, max: u64) {
        let value = perf_value_max(snapshot, label);
        assert!(
            value <= max,
            "{label} recorded max {value}, expected <= {max}"
        );
    }

    fn assert_no_full_request_start_reads(snapshot: &smelt_perf::perf::Snapshot) {
        for label in [
            "store:history:read_all",
            "store:history:read_all_rows",
            "store:session:load_full_snapshot",
            "store:session:full_snapshot_rows_read",
            "store:transcript:search_blob_full",
            "store:transcript:read_records_full",
            "store:transcript:records_full_loaded",
            "session:rebuild_transcript_full_fallback",
            "session:display_only_load_full",
            "transcript:build_from_session:history_items",
        ] {
            assert_perf_value_absent(snapshot, label);
        }
    }

    fn large_saved_session_app(history_len: usize) -> crate::app::test_harness::TestApp {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let mut session =
            smelt_core::session::Session::new(app.app.core.env.pid(), app.app.core.env.cwd());
        session.first_user_message = Some("old user 0".into());
        session.history = (0..history_len)
            .map(|idx| {
                if idx.is_multiple_of(2) {
                    HistoryItem::user(Content::text(format!("old user {idx}")))
                } else {
                    HistoryItem::Assistant(protocol::AssistantStep::terminal(
                        Some(Content::text(format!("old assistant {idx}"))),
                        None,
                        Vec::new(),
                    ))
                }
            })
            .collect();
        app.app.load_session(session);
        app.app.restore_screen();
        app.app.ensure_current_context_note();
        app.app.apply_pending_history_appends_for_request();
        app.app.save_session();
        app.app.flush_persist();
        app
    }

    #[test]
    fn starting_user_turn_dismisses_visible_notification() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app
            .notify_turn_error_sticky("rate limit exceeded".to_string());
        assert!(app.app.notification_win().is_some());

        let turn = app
            .app
            .begin_agent_turn("try again", Content::text("try again"))
            .expect("test app has a usable model");
        app.app.conversation.set_active(Some(turn));

        assert!(app.app.notification_win().is_none());
    }

    #[test]
    fn starting_user_turn_dismisses_turn_start_failure() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.notify_operation_error_sticky(
            crate::app::NotificationOperation::TurnStart,
            "missing credentials".to_string(),
        );

        let turn = app
            .app
            .begin_agent_turn("try again", Content::text("try again"))
            .expect("test app has a usable model");
        app.app.conversation.set_active(Some(turn));

        assert!(app.app.notification_win().is_none());
    }

    #[test]
    fn starting_user_turn_preserves_session_notification() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let session_id = app.app.conversation.session().id.clone();
        app.app
            .notify_session_error_sticky("session failure".to_string());

        let turn = app
            .app
            .begin_agent_turn("try again", Content::text("try again"))
            .expect("test app has a usable model");
        app.app.conversation.set_active(Some(turn));

        assert!(app.app.overlays.notification().is_some_and(|notification| {
            matches!(
                &notification.scope,
                crate::app::NotificationScope::Session(owner_session_id)
                    if owner_session_id == &session_id
            ) && notification.summary == "session failure"
        }));
    }

    #[test]
    fn starting_user_turn_preserves_application_notification() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app
            .notify_application_error_sticky("application failure".to_string());

        let turn = app
            .app
            .begin_agent_turn("try again", Content::text("try again"))
            .expect("test app has a usable model");
        app.app.conversation.set_active(Some(turn));

        assert!(app.app.overlays.notification().is_some_and(|notification| {
            matches!(
                &notification.scope,
                crate::app::NotificationScope::Application
            ) && notification.summary == "application failure"
        }));
    }

    #[test]
    fn starting_command_continuation_dismisses_visible_notification() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let previous = app
            .app
            .begin_agent_turn("previous", Content::text("previous"))
            .expect("previous turn starts");
        app.app.conversation.set_active(Some(previous));
        app.app.discard_turn(crate::app::TurnEnd::Complete);
        app.app
            .notify_turn_error_sticky("quota exceeded".to_string());
        assert!(app.app.notification_win().is_some());

        let turn = app
            .app
            .begin_command_request_turn(
                "continue".into(),
                String::new(),
                smelt_core::custom_commands::CommandOverrides::default(),
                crate::app::CommandTurnStart::ContinueFromLast,
            )
            .expect("test app has a usable model");
        app.app.conversation.set_active(Some(turn));

        assert!(app.app.notification_win().is_none());
    }

    #[test]
    fn user_turn_commits_request_before_dispatch_without_duplicate_history() {
        let mut app = crate::app::test_harness::TestApp::builder().build();

        let turn = app
            .app
            .begin_agent_turn("first request", Content::text("first request"))
            .expect("test app has a usable model");
        app.app.conversation.set_active(Some(turn));

        assert!(matches!(
            app.app.conversation.session().history.last(),
            Some(HistoryItem::User { content, .. }) if content.text_content() == "first request"
        ));
        let payload = app
            .drain_engine_sends()
            .into_iter()
            .find_map(|cmd| match cmd {
                protocol::UiCommand::StartTurn(payload) => Some(payload),
                _ => None,
            })
            .expect("turn dispatched");
        let protocol::ModelHistorySource::Store { end_index, .. } = &payload.history else {
            panic!("interactive turn should dispatch store-backed model history");
        };
        assert_eq!(*end_index, app.app.conversation.session().history.len() - 1);
        assert_eq!(
            payload.input.provider_content().text_content(),
            "first request"
        );

        app.app.flush_persist();
        let loaded = crate::app::history::materialize_full_session(
            &app.app.core.sessions,
            &app.app.conversation.session().id,
            crate::app::history::FullSessionMaterializationReason::TestSavedSessionAssertion,
        )
        .expect("session saved");
        assert!(matches!(
            loaded.history.last(),
            Some(HistoryItem::User { content, .. }) if content.text_content() == "first request"
        ));
    }

    #[test]
    fn terminal_turn_commits_final_history_and_completed_state_together() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let turn = app
            .app
            .begin_agent_turn("finish me", Content::text("finish me"))
            .expect("turn starts");
        let turn_id = turn.turn_id;
        app.app.conversation.set_active(Some(turn));
        app.app
            .session_append_history(HistoryItem::assistant(protocol::AssistantStep::terminal(
                Some(Content::text("finished")),
                None,
                Vec::new(),
            )));

        app.app.discard_turn(crate::app::TurnEnd::Complete);

        let reader = lineage_reader(&app);
        let stored = lineage_turn(&reader, turn_id);
        assert_eq!(stored.state, smelt_store::TurnState::Completed);
        let history = lineage_history(&reader);
        assert!(matches!(
            history.last(),
            Some(HistoryItem::Assistant { .. })
        ));
        assert!(stored
            .finished_at_ms
            .is_some_and(|finished| finished >= stored.created_at_ms));
    }

    #[test]
    fn escape_interrupt_is_not_blocked_by_catalog_projection() {
        let mut app = crate::app::test_harness::TestApp::builder()
            .with_vim(true)
            .build();
        app.start_submitted_turn("interrupt me");
        app.dispatch_engine_event(protocol::EngineEvent::TextDelta {
            delta: "started".into(),
        });
        app.press(crossterm::event::KeyCode::Esc);
        assert!(app.agent_running(), "first Escape is the local Vim action");

        assert!(app
            .app
            .core
            .sessions
            .wait_for_session_catalog(std::time::Duration::from_secs(5)));
        let catalog = rusqlite::Connection::open(app.app.core.sessions.layout().catalog_path())
            .expect("open session catalog");
        catalog
            .execute_batch("BEGIN IMMEDIATE")
            .expect("hold session catalog write lock");

        let started = std::time::Instant::now();
        app.press(crossterm::event::KeyCode::Esc);
        let elapsed = started.elapsed();
        catalog
            .execute_batch("ROLLBACK")
            .expect("release catalog lock");

        assert!(!app.agent_running(), "second Escape cancels the agent");
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "interrupt waited {elapsed:?} for derived catalog persistence"
        );
        assert_catalog_converged(&app);
    }

    #[test]
    fn escape_interrupt_is_not_blocked_by_busy_persistence() {
        let mut app = crate::app::test_harness::TestApp::builder()
            .with_vim(true)
            .build();
        app.start_submitted_turn("interrupt while persistence is busy");
        app.press(crossterm::event::KeyCode::Esc);
        assert!(app.agent_running(), "first Escape is the local Vim action");
        app.app.flush_persist();
        let turn_id = app
            .app
            .conversation
            .active()
            .expect("active canonical turn")
            .turn_id;
        let release = app.app.conversation.pause_persistence();

        let started = std::time::Instant::now();
        app.press(crossterm::event::KeyCode::Esc);
        let elapsed = started.elapsed();
        release.send(()).expect("resume persistence actor");

        assert!(!app.agent_running(), "Escape cancels the agent");
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "interrupt waited {elapsed:?} for busy session persistence"
        );
        let flush = app.app.flush_persist();
        assert!(
            matches!(
                flush,
                crate::persist::PersistenceFlushOutcome::Durable { .. }
            ),
            "cancel transition did not become durable: {flush:?}"
        );
        assert_eq!(
            lineage_turn(&lineage_reader(&app), turn_id).state,
            smelt_store::TurnState::Cancelled
        );
    }

    #[test]
    fn rewind_during_active_turn_is_not_blocked_by_persistence() {
        let mut app = large_saved_session_app(4);
        let rewind_history_idx = app.app.rewind_turns().unwrap()[0].history_idx;
        app.start_submitted_turn("rewind this active turn");
        app.app.flush_persist();
        let release = app.app.conversation.pause_persistence();

        let started = std::time::Instant::now();
        app.app
            .rewind_to_history_index(Some(rewind_history_idx), false);
        let elapsed = started.elapsed();
        release.send(()).expect("resume persistence actor");

        assert!(!app.agent_running(), "rewind cancels the active turn");
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "rewind waited {elapsed:?} for canonical turn cancellation"
        );
        app.app.flush_persist();
        assert_eq!(lineage_history(&lineage_reader(&app)).len(), 0);
    }

    #[test]
    fn reset_fails_promptly_when_current_session_persistence_is_busy() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.session_append_history(protocol::HistoryItem::note(
            protocol::HistoryNote::context("durable baseline"),
        ));
        app.app.save_session_and_flush();
        let original_id = app.app.conversation.session().id.clone();
        app.app.session_append_history(protocol::HistoryItem::note(
            protocol::HistoryNote::context("preserve this change"),
        ));
        let release = app.app.conversation.pause_persistence();

        let started = std::time::Instant::now();
        app.app.reset_session();
        let elapsed = started.elapsed();

        assert_eq!(
            app.app.conversation.session().id,
            original_id,
            "failed reset keeps the current session"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "reset waited {elapsed:?} for busy session persistence"
        );
        release.send(()).expect("resume persistence actor");
        app.app.flush_persist();
        assert_eq!(lineage_history(&lineage_reader(&app)).len(), 2);
    }

    #[test]
    fn session_switch_fails_promptly_when_current_persistence_is_busy() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.session_append_history(protocol::HistoryItem::note(
            protocol::HistoryNote::context("switch target"),
        ));
        app.app.push_block(smelt_core::Block::Text {
            content: "switch target transcript".into(),
        });
        app.app.save_session_and_flush();
        let target_id = app.app.conversation.session().id.clone();
        app.app.reset_session();
        let current_id = app.app.conversation.session().id.clone();
        app.app.session_append_history(protocol::HistoryItem::note(
            protocol::HistoryNote::context("current baseline"),
        ));
        app.app.save_session_and_flush();
        app.app.session_append_history(protocol::HistoryItem::note(
            protocol::HistoryNote::context("preserve current change"),
        ));
        let release = app.app.conversation.pause_persistence();

        let started = std::time::Instant::now();
        let loaded = app.app.load_session_by_id(&target_id);
        let elapsed = started.elapsed();

        assert!(!loaded, "busy persistence prevents the session switch");
        assert_eq!(
            app.app.conversation.session().id,
            current_id,
            "failed switch keeps the current session"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "session switch waited {elapsed:?} for busy session persistence"
        );
        release.send(()).expect("resume persistence actor");
        app.app.flush_persist();
        assert_eq!(lineage_history(&lineage_reader(&app)).len(), 2);
    }

    #[test]
    fn fork_fails_promptly_when_current_persistence_is_busy() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.session_append_history(protocol::HistoryItem::note(
            protocol::HistoryNote::context("durable fork baseline"),
        ));
        app.app.save_session_and_flush();
        let original_id = app.app.conversation.session().id.clone();
        app.app.session_append_history(protocol::HistoryItem::note(
            protocol::HistoryNote::context("preserve before fork"),
        ));
        let session_ids_before =
            smelt_store::lineage_session_ids(app.app.core.sessions.sessions_dir())
                .expect("list sessions before fork");
        let release = app.app.conversation.pause_persistence();

        let started = std::time::Instant::now();
        app.app.fork_session();
        let elapsed = started.elapsed();

        assert_eq!(
            app.app.conversation.session().id,
            original_id,
            "failed fork keeps the current session"
        );
        assert_eq!(
            smelt_store::lineage_session_ids(app.app.core.sessions.sessions_dir())
                .expect("list sessions after fork"),
            session_ids_before,
            "failed fork does not publish a partial destination"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "fork waited {elapsed:?} for busy session persistence"
        );
        release.send(()).expect("resume persistence actor");
        app.app.flush_persist();
        assert_eq!(lineage_history(&lineage_reader(&app)).len(), 2);
    }

    #[test]
    fn delete_fails_promptly_when_lineage_persistence_is_busy() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.session_append_history(protocol::HistoryItem::note(
            protocol::HistoryNote::context("fork before delete"),
        ));
        app.app.push_block(smelt_core::Block::Text {
            content: "fork before delete transcript".into(),
        });
        app.app.save_session_and_flush();
        let source_id = app.app.conversation.session().id.clone();
        app.app.fork_session();
        assert_ne!(app.app.conversation.session().id, source_id);
        app.set_lua_string_global("SOURCE_SESSION_ID", source_id.clone())
            .expect("install source session id");
        let release = app.app.conversation.pause_persistence();

        let started = std::time::Instant::now();
        let result = app.run_lua_result("smelt.session.delete(SOURCE_SESSION_ID)");
        let elapsed = started.elapsed();

        assert!(result.is_err(), "busy lineage deletion fails safely");
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "delete waited {elapsed:?} for busy lineage persistence"
        );
        release.send(()).expect("resume persistence actor");
        assert!(
            smelt_store::LineageSessionReader::try_open_existing(
                app.app.core.sessions.sessions_dir(),
                &source_id,
            )
            .expect("inspect source branch")
            .is_some(),
            "timed-out deletion preserves the source session"
        );
    }

    #[test]
    fn closing_dirty_session_is_not_blocked_by_catalog_projection() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.session_append_history(protocol::HistoryItem::note(
            protocol::HistoryNote::context("catalog baseline"),
        ));
        app.app.save_session_and_flush();
        assert!(app
            .app
            .core
            .sessions
            .wait_for_session_catalog(std::time::Duration::from_secs(5)));

        let catalog = rusqlite::Connection::open(app.app.core.sessions.layout().catalog_path())
            .expect("open session catalog");
        catalog
            .execute_batch("BEGIN IMMEDIATE")
            .expect("hold session catalog write lock");
        app.start_submitted_turn("close this active agent");
        assert!(app.agent_running(), "agent is active before shutdown");

        let started = std::time::Instant::now();
        let closed = app.app.finalize_graceful_shutdown();
        let elapsed = started.elapsed();
        catalog
            .execute_batch("ROLLBACK")
            .expect("release catalog lock");

        assert!(closed.is_ok(), "active session closes: {closed:?}");
        assert!(!app.agent_running(), "shutdown clears the active agent");
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "close waited {elapsed:?} for derived catalog persistence"
        );
        assert_catalog_converged(&app);
    }

    #[test]
    fn session_list_is_not_blocked_by_an_exclusive_catalog_lock() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.session_append_history(protocol::HistoryItem::note(
            protocol::HistoryNote::context("catalog baseline"),
        ));
        app.app.save_session_and_flush();
        assert!(app
            .app
            .core
            .sessions
            .wait_for_session_catalog(std::time::Duration::from_secs(5)));
        let catalog = rusqlite::Connection::open(app.app.core.sessions.layout().catalog_path())
            .expect("open session catalog");
        catalog
            .execute_batch("PRAGMA locking_mode=EXCLUSIVE; BEGIN EXCLUSIVE")
            .expect("hold exclusive session catalog lock");

        let started = std::time::Instant::now();
        let listed = app.run_lua_result("smelt.session.list()");
        let elapsed = started.elapsed();
        catalog
            .execute_batch("ROLLBACK")
            .expect("release catalog lock");

        assert!(
            listed.is_ok(),
            "catalog contention degrades the list safely"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "session list waited {elapsed:?} for the derived catalog"
        );
    }

    #[test]
    fn session_load_is_not_blocked_by_an_exclusive_catalog_lock() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let mut target =
            smelt_core::session::Session::new(app.app.core.env.pid(), app.app.core.env.cwd());
        target
            .history
            .push(protocol::HistoryItem::note(protocol::HistoryNote::context(
                "catalog-locked resume target",
            )));
        let target_id = target.id.clone();
        app.app
            .core
            .sessions
            .save_result(&target)
            .expect("save resume target");
        assert!(app
            .app
            .core
            .sessions
            .wait_for_session_catalog(std::time::Duration::from_secs(5)));
        let current_id = app.app.conversation.session().id.clone();
        let catalog = rusqlite::Connection::open(app.app.core.sessions.layout().catalog_path())
            .expect("open session catalog");
        catalog
            .execute_batch("PRAGMA locking_mode=EXCLUSIVE; BEGIN EXCLUSIVE")
            .expect("hold exclusive session catalog lock");
        app.set_lua_string_global("TARGET_SESSION_ID", target_id)
            .expect("install target session id");

        let started = std::time::Instant::now();
        app.run_lua_result("smelt.session.load(TARGET_SESSION_ID)")
            .expect("request session load through the user-facing API");
        let dispatch_elapsed = started.elapsed();
        assert!(
            dispatch_elapsed < std::time::Duration::from_millis(100),
            "session load dispatch blocked the UI for {dispatch_elapsed:?}"
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        loop {
            if let Some(event) = app.try_recv_app_event() {
                let completed = matches!(&event, crate::app::AppEvent::SessionLoadCompleted(_));
                app.handle_app_event(event);
                if completed {
                    break;
                }
            } else {
                assert!(
                    std::time::Instant::now() < deadline,
                    "catalog-locked session load did not fail within its read deadline"
                );
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        catalog
            .execute_batch("ROLLBACK")
            .expect("release session catalog lock");

        assert_eq!(
            app.app.conversation.session().id,
            current_id,
            "failed session load preserves the current session"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "session load waited {:?} for the derived catalog",
            started.elapsed()
        );
    }

    #[test]
    fn session_load_rejects_missing_transcript_records_off_thread() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let mut target =
            smelt_core::session::Session::new(app.app.core.env.pid(), app.app.core.env.cwd());
        target
            .history
            .push(protocol::HistoryItem::note(protocol::HistoryNote::context(
                "recordless resume target",
            )));
        let target_id = target.id.clone();
        app.app
            .core
            .sessions
            .save_result(&target)
            .expect("save recordless resume target");
        let current_id = app.app.conversation.session().id.clone();
        app.app
            .session_load
            .set_delay(std::time::Duration::from_millis(500));
        app.set_lua_string_global("TARGET_SESSION_ID", target_id)
            .expect("install target session id");

        let started = std::time::Instant::now();
        app.run_lua_result("smelt.session.load(TARGET_SESSION_ID)")
            .expect("request session load through the user-facing API");
        let elapsed = started.elapsed();

        assert!(
            elapsed < std::time::Duration::from_millis(100),
            "session load blocked the UI for {elapsed:?}"
        );
        assert_eq!(app.app.conversation.session().id, current_id);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if let Some(event) = app.try_recv_app_event() {
                let completed = matches!(&event, crate::app::AppEvent::SessionLoadCompleted(_));
                app.handle_app_event(event);
                if completed {
                    break;
                }
            } else {
                assert!(
                    std::time::Instant::now() < deadline,
                    "background session load did not complete"
                );
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        assert_eq!(app.app.conversation.session().id, current_id);
    }

    #[test]
    fn latest_session_load_request_wins_without_head_of_line_blocking() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let mut first =
            smelt_core::session::Session::new(app.app.core.env.pid(), app.app.core.env.cwd());
        first
            .history
            .push(protocol::HistoryItem::note(protocol::HistoryNote::context(
                "stale resume target",
            )));
        let first_id = first.id.clone();
        save_record_backed_session(&app, &first, "stale resume transcript");
        let mut latest =
            smelt_core::session::Session::new(app.app.core.env.pid(), app.app.core.env.cwd());
        latest
            .history
            .push(protocol::HistoryItem::note(protocol::HistoryNote::context(
                "latest resume target",
            )));
        let latest_id = latest.id.clone();
        save_record_backed_session(&app, &latest, "latest resume transcript");
        let current_id = app.app.conversation.session().id.clone();
        app.app
            .session_load
            .set_delay(std::time::Duration::from_millis(500));
        app.set_lua_string_global("FIRST_SESSION_ID", first_id.clone())
            .expect("install first session id");
        app.set_lua_string_global("LATEST_SESSION_ID", latest_id.clone())
            .expect("install latest session id");

        app.run_lua_result("smelt.session.load(FIRST_SESSION_ID)")
            .expect("request first session load");
        std::thread::sleep(std::time::Duration::from_millis(25));
        app.app.session_load.set_delay(std::time::Duration::ZERO);
        let latest_requested_at = std::time::Instant::now();
        app.run_lua_result("smelt.session.load(LATEST_SESSION_ID)")
            .expect("request latest session load");
        assert_eq!(app.app.conversation.session().id, current_id);

        let deadline = latest_requested_at + std::time::Duration::from_millis(250);
        while app.app.conversation.session().id != latest_id {
            if let Some(event) = app.try_recv_app_event() {
                app.handle_app_event(event);
            } else {
                assert!(
                    std::time::Instant::now() < deadline,
                    "latest background session load did not complete"
                );
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        assert_ne!(app.app.conversation.session().id, first_id);
        assert_eq!(app.app.session_history_len(), 1);
    }

    #[test]
    fn forking_session_is_not_blocked_by_catalog_projection() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.session_append_history(protocol::HistoryItem::note(
            protocol::HistoryNote::context("catalog baseline"),
        ));
        app.app.push_block(smelt_core::Block::Text {
            content: "catalog baseline transcript".into(),
        });
        app.app.save_session_and_flush();
        assert!(app
            .app
            .core
            .sessions
            .wait_for_session_catalog(std::time::Duration::from_secs(5)));
        let original_id = app.app.conversation.session().id.clone();

        let catalog = rusqlite::Connection::open(app.app.core.sessions.layout().catalog_path())
            .expect("open session catalog");
        catalog
            .execute_batch("BEGIN IMMEDIATE")
            .expect("hold session catalog write lock");

        let started = std::time::Instant::now();
        app.app.fork_session();
        let elapsed = started.elapsed();
        catalog
            .execute_batch("ROLLBACK")
            .expect("release catalog lock");

        let fork_id = app.app.conversation.session().id.clone();
        assert_ne!(fork_id, original_id, "fork becomes the active session");
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "fork waited {elapsed:?} for derived catalog persistence"
        );
        assert!(app
            .app
            .core
            .sessions
            .resolve_session_for_read_result(&fork_id)
            .is_ok());
        assert_catalog_converged(&app);
    }

    #[test]
    fn canonical_submit_ignores_an_unrelated_catalog_marker_lock() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let unrelated_session = "f".repeat(64);
        let catalog_lock = smelt_store::CatalogMarkerLock::acquire(
            app.app.core.sessions.sessions_dir(),
            &unrelated_session,
        )
        .expect("hold unrelated catalog marker lock");

        let started = std::time::Instant::now();
        let turn = app.app.begin_agent_turn(
            "locked catalog marker",
            Content::text("locked catalog marker"),
        );
        let elapsed = started.elapsed();
        drop(catalog_lock);

        assert!(turn.is_some(), "canonical submission remains independent");
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "canonical submit waited {elapsed:?} for an unrelated catalog marker"
        );
    }

    #[test]
    fn same_session_catalog_marker_contention_retries_before_commit() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let session_id = app.app.conversation.session().id.clone();
        let catalog_lock = smelt_store::CatalogMarkerLock::acquire(
            app.app.core.sessions.sessions_dir(),
            &session_id,
        )
        .expect("hold current session catalog marker lock");
        app.app.session_append_history(protocol::HistoryItem::note(
            protocol::HistoryNote::context("blocked save"),
        ));
        let synchronized = std::sync::Arc::new(std::sync::Barrier::new(2));
        let release = {
            let synchronized = synchronized.clone();
            std::thread::spawn(move || {
                synchronized.wait();
                std::thread::sleep(std::time::Duration::from_millis(150));
                drop(catalog_lock);
            })
        };

        synchronized.wait();
        let started = std::time::Instant::now();
        app.app.save_session_and_flush();
        let elapsed = started.elapsed();
        release.join().unwrap();

        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "canonical save did not recover promptly from marker contention: {elapsed:?}"
        );
        assert!(!app.app.session_document_has_unflushed_work());
    }

    #[test]
    fn canonical_submit_is_not_blocked_by_full_catalog_reconciliation() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.session_append_history(protocol::HistoryItem::note(
            protocol::HistoryNote::context("catalog baseline"),
        ));
        app.app.save_session_and_flush();
        assert!(app
            .app
            .core
            .sessions
            .wait_for_session_catalog(std::time::Duration::from_secs(5)));

        let catalog = rusqlite::Connection::open(app.app.core.sessions.layout().catalog_path())
            .expect("open session catalog");
        catalog
            .execute_batch("BEGIN IMMEDIATE")
            .expect("hold session catalog write lock");
        app.app
            .core
            .sessions
            .request_session_catalog_reconciliation();
        std::thread::sleep(std::time::Duration::from_millis(50));

        let started = std::time::Instant::now();
        app.start_submitted_turn("commit during reconciliation");
        let elapsed = started.elapsed();
        catalog
            .execute_batch("ROLLBACK")
            .expect("release catalog lock");

        assert!(app.agent_running(), "canonical turn submission completes");
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "canonical submit waited {elapsed:?} for full catalog reconciliation"
        );
        assert!(app
            .app
            .core
            .sessions
            .wait_for_session_catalog(std::time::Duration::from_secs(5)));
        app.app.discard_turn(crate::app::TurnEnd::Cancelled);
        assert!(!app.agent_running());
    }

    #[test]
    fn terminal_transition_failure_keeps_queued_turn_from_starting() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let turn = app
            .app
            .begin_agent_turn("first", Content::text("first"))
            .expect("turn starts");
        let turn_id = turn.turn_id;
        app.app.conversation.set_active(Some(turn));
        let _ = app.app.flush_persist();
        app.push_queued_message("must remain queued".into());
        app.clear_actions();
        app.app
            .conversation
            .inject_commit_failure(smelt_store::SessionCommitFailure::OwnershipLost);

        app.app.discard_turn(crate::app::TurnEnd::Complete);

        assert!(!app.agent_running());
        assert_eq!(app.state().queued_inputs, vec!["must remain queued"]);
        assert!(app.actions().iter().all(|action| !matches!(
            action,
            crate::app::test_harness::Action::EngineSend(command)
                if matches!(command.as_ref(), protocol::UiCommand::StartTurn(_))
        )));
        assert!(app.app.session_document_has_unflushed_work());
        let reader = lineage_reader(&app);
        assert_eq!(
            lineage_turn(&reader, turn_id).state,
            smelt_store::TurnState::Running
        );
    }

    #[test]
    fn enter_submits_one_canonical_turn_before_engine_dispatch() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.type_text("phase zero submit");
        app.clear_actions();

        smelt_perf::perf::set_enabled(true);
        smelt_perf::perf::clear();
        app.press(crossterm::event::KeyCode::Enter);
        let snapshot = smelt_perf::perf::snapshot();
        smelt_perf::perf::set_enabled(false);

        let mut dispatched_turns = app.actions().iter().filter_map(|action| match action {
            crate::app::test_harness::Action::EngineSend(command) => match command.as_ref() {
                protocol::UiCommand::StartTurn(payload) => Some(payload.as_ref()),
                _ => None,
            },
            _ => None,
        });
        let payload = dispatched_turns.next().expect("Enter dispatched StartTurn");
        assert!(
            dispatched_turns.next().is_none(),
            "Enter must dispatch exactly one StartTurn"
        );
        assert_eq!(
            perf_value_count(&snapshot, "agent:dispatch:start_turn:at_us"),
            1,
            "dispatch instrumentation should observe exactly one StartTurn"
        );
        assert_eq!(
            perf_value_total(&snapshot, "persist:submit_turn:transactions"),
            1
        );
        for metric in [
            "persist:submit_turn:queue_wait_ms",
            "persist:submit_turn:transaction_ms",
            "persist:submit_turn:history_rows",
            "persist:submit_turn:transcript_record_rows",
            "persist:submit_turn:index_rows",
            "persist:submit_turn:dispatch_after_receipt_ms",
        ] {
            assert_eq!(
                perf_value_count(&snapshot, metric),
                1,
                "missing bounded SubmitTurn metric {metric}"
            );
        }
        assert_eq!(
            perf_value_total(&snapshot, "store:transaction:session_commit:attempts"),
            0,
            "Enter must not issue a preliminary ordinary save transaction"
        );
        assert_eq!(
            perf_value_last(&snapshot, "agent:dispatch:start_turn:turn_id"),
            Some(payload.turn_id)
        );

        let committed_at = perf_value_last(&snapshot, "persist:submit_turn:committed_at_us")
            .expect("SubmitTurn commit timestamp");
        let dispatched_at = perf_value_last(&snapshot, "agent:dispatch:start_turn:at_us")
            .expect("dispatch timestamp");
        assert!(
            committed_at <= dispatched_at,
            "expected SubmitTurn commit ({committed_at}) before dispatch ({dispatched_at})"
        );
        let reader = lineage_reader(&app);
        let head = reader.snapshot().expect("read canonical state").head;
        let stored_turn = lineage_turn(&reader, payload.turn_id);
        assert_eq!(stored_turn.kind, smelt_store::TurnKind::User);
        assert!(matches!(
            stored_turn.state,
            smelt_store::TurnState::Ready | smelt_store::TurnState::Running
        ));
        assert_eq!(
            stored_turn.submitted_revision.get(),
            payload.persistence.store_revision
        );
        assert_eq!(
            perf_value_last(&snapshot, "agent:dispatch:start_turn:revision"),
            Some(stored_turn.submitted_revision.get())
        );
        assert!(head.revision >= stored_turn.submitted_revision);
        let history = lineage_history(&reader);
        assert!(matches!(
            history.last(),
            Some(HistoryItem::User { content, .. }) if content.text_content() == "phase zero submit"
        ));
    }

    #[test]
    fn canonical_submit_failure_preserves_prompt_and_prevents_dispatch() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app
            .session_append_history(HistoryItem::note(protocol::HistoryNote::context(
                "published baseline",
            )));
        app.app.save_session_and_flush();
        app.type_text("retry this exact input");
        app.app.conversation.inject_commit_failure(
            smelt_store::SessionCommitFailure::InvalidCommand {
                message: "injected pre-commit rejection".into(),
            },
        );

        app.press(crossterm::event::KeyCode::Enter);

        assert_eq!(app.state().prompt_text, "retry this exact input");
        assert!(!app.state().agent_running);
        assert_eq!(app.app.session_history_len(), 2);
        let history = app.app.session_history_range(0..2).unwrap();
        assert!(history.last().is_some_and(|item| {
            item.as_note().is_some_and(|note| {
                note.context_name() == Some(protocol::DEFAULT_CONTEXT_NOTE_NAME)
            })
        }));
        assert!(history
            .iter()
            .all(|item| !matches!(item, HistoryItem::User { .. })));
        assert!(app.actions().iter().all(|action| !matches!(
            action,
            crate::app::test_harness::Action::EngineSend(command)
                if matches!(command.as_ref(), protocol::UiCommand::StartTurn(_))
        )));

        assert!(app.app.retry_blocked_persistence());
        app.clear_actions();
        app.press(crossterm::event::KeyCode::Enter);
        assert!(app.agent_running());
        let _ = app.app.flush_persist();
        let reader = lineage_reader(&app);
        let history = lineage_history(&reader);
        assert_eq!(history.len(), 3);
        assert_eq!(
            history
                .iter()
                .filter(|item| matches!(item, HistoryItem::User { content, .. } if content.text_content() == "retry this exact input"))
                .count(),
            1
        );
    }

    #[test]
    fn title_response_after_submit_rollback_cannot_poison_persistence_recovery() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app
            .session_append_history(HistoryItem::note(protocol::HistoryNote::context(
                "published baseline",
            )));
        app.app.save_session_and_flush();
        let session_id = app.app.conversation.session().id.clone();
        app.type_text("retry this exact input");
        app.app.conversation.inject_commit_failure(
            smelt_store::SessionCommitFailure::InvalidCommand {
                message: "injected pre-commit rejection".into(),
            },
        );
        let history_epoch = app
            .app
            .core
            .signals
            .get::<u64>("history_epoch")
            .unwrap_or_default();

        app.press(crossterm::event::KeyCode::Enter);
        assert_eq!(
            app.app.core.signals.get::<u64>("history_epoch"),
            Some(history_epoch.wrapping_add(1)),
            "rolling back the staged submission must invalidate history-bound work"
        );
        assert!(
            app.app.notification_win().is_some(),
            "the rejected canonical submission should surface its persistence failure"
        );
        let title_request = app
            .pending_ask_id()
            .expect("title request remains in flight");
        let rolled_back_history_len = app.app.session_history_len();

        app.respond_ask_with_text(
            title_request,
            r#"{"title":"Stale retry title","slug":"stale-retry"}"#,
        );

        assert_eq!(app.app.session_history_len(), rolled_back_history_len);
        assert_eq!(app.app.conversation.session().title, None);
        assert!(app
            .app
            .conversation
            .session()
            .metadata_snapshots
            .iter()
            .all(|(index, _)| *index <= rolled_back_history_len));

        assert!(
            app.app.retry_blocked_persistence(),
            "the failed canonical submission should be retryable"
        );
        app.clear_actions();
        // The initial title response was delivered directly by id, so discard its queued request
        // before identifying the retry's title request from newly emitted actions.
        let _ = app.drain_engine_sends();
        app.press(crossterm::event::KeyCode::Enter);
        assert!(app.agent_running(), "the preserved prompt should resubmit");
        let retry_title_request = title_request_from_actions(&app);
        complete_active_turn_with_assistant(&mut app, "retry completed");
        app.respond_ask_with_text(
            retry_title_request,
            r#"{"title":"Recovered retry","slug":"recovered-retry"}"#,
        );
        app.app.save_session_and_flush();
        assert!(
            app.app.notification_win().is_none(),
            "durable retry should dismiss the persistence failure"
        );

        app.clear_actions();
        app.type_text("follow up after recovery");
        app.press(crossterm::event::KeyCode::Enter);
        assert!(app.agent_running(), "a subsequent turn should start");
        let follow_up_title_request = title_request_from_actions(&app);
        complete_active_turn_with_assistant(&mut app, "follow-up completed");
        app.respond_ask_with_text(
            follow_up_title_request,
            r#"{"title":"Recovered follow-up","slug":"recovered-follow-up"}"#,
        );
        assert_eq!(
            app.app.conversation.session().title.as_deref(),
            Some("Recovered follow-up")
        );
        app.app.save_session();
        let outcome = app.app.flush_persist();
        assert!(
            matches!(
                outcome,
                crate::persist::PersistenceFlushOutcome::Durable { .. }
            ),
            "subsequent save should remain durable: {outcome:?}"
        );

        let saved = crate::app::history::materialize_full_session(
            &app.app.core.sessions,
            &session_id,
            crate::app::history::FullSessionMaterializationReason::TestSavedSessionAssertion,
        )
        .expect("recovered session should load from durable storage");
        let saved_history_len = saved.history.len();
        assert_eq!(saved.title.as_deref(), Some("Recovered follow-up"));
        for expected in ["retry this exact input", "follow up after recovery"] {
            assert_eq!(
                saved
                    .history
                    .iter()
                    .filter(|item| matches!(item, HistoryItem::User { content, .. } if content.text_content() == expected))
                    .count(),
                1,
                "{expected:?} should be durable exactly once"
            );
        }
        assert!(saved
            .metadata_snapshots
            .iter()
            .all(|(index, _)| *index <= saved_history_len));
        assert!(saved
            .turn_metas
            .iter()
            .all(|(index, _)| *index <= saved_history_len));
        assert!(saved
            .context_snapshots
            .iter()
            .all(|(index, _)| *index <= saved_history_len));
        assert!(
            app.app.notification_win().is_none(),
            "no session-save failure should remain after durable recovery"
        );
    }

    #[test]
    fn title_target_beyond_current_history_is_rejected() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app
            .session_append_history(HistoryItem::user(Content::text("current request")));
        let history_len = app.app.session_history_len();

        app.app.set_session_title(
            "Future title".into(),
            "future-title".into(),
            Some(history_len + 1),
        );

        assert_eq!(app.app.conversation.session().title, None);
        assert!(app.app.conversation.session().metadata_snapshots.is_empty());
    }

    #[test]
    fn command_submit_failure_rolls_back_request_and_retains_context_event() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app
            .session_append_history(HistoryItem::note(protocol::HistoryNote::context(
                "published baseline",
            )));
        app.app.save_session_and_flush();
        let baseline_history_len = app.app.session_history_len();
        let baseline_transcript_len = app.app.conversation.transcript().history().order.len();
        app.app.conversation.inject_commit_failure(
            smelt_store::SessionCommitFailure::InvalidCommand {
                message: "injected command rejection".into(),
            },
        );

        assert!(app
            .app
            .begin_command_request_turn(
                "/retry".into(),
                "retry command".into(),
                smelt_core::custom_commands::CommandOverrides::default(),
                crate::app::CommandTurnStart::Fresh,
            )
            .is_none());

        assert_eq!(app.app.session_history_len(), baseline_history_len + 1);
        assert!(app
            .app
            .session_history_range(baseline_history_len..baseline_history_len + 1)
            .unwrap()
            .first()
            .and_then(HistoryItem::as_note)
            .is_some_and(|note| {
                note.context_name() == Some(protocol::DEFAULT_CONTEXT_NOTE_NAME)
            }));
        assert_eq!(
            app.app.conversation.transcript().history().order.len(),
            baseline_transcript_len
        );
        assert!(app.app.conversation.session().first_user_message.is_none());
        assert!(user_blocks(&app).is_empty());
        assert!(app
            .drain_engine_sends()
            .into_iter()
            .all(|command| !matches!(command, protocol::UiCommand::StartTurn(_))));

        assert!(app.app.retry_blocked_persistence());
        let turn = app
            .app
            .begin_command_request_turn(
                "/retry".into(),
                "retry command".into(),
                smelt_core::custom_commands::CommandOverrides::default(),
                crate::app::CommandTurnStart::Fresh,
            )
            .expect("command retry starts");
        app.app.conversation.set_active(Some(turn));
        let reader = lineage_reader(&app);
        let history = lineage_history(&reader);
        assert_eq!(history.len(), baseline_history_len + 2);
        assert_eq!(
            history
                .iter()
                .filter(|item| matches!(item, HistoryItem::User { content, .. } if content.text_content() == "retry command"))
                .count(),
            1
        );
    }

    #[test]
    fn note_submit_failure_rolls_back_request_and_retains_context_event() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app
            .session_append_history(HistoryItem::note(protocol::HistoryNote::context(
                "published baseline",
            )));
        app.app.save_session_and_flush();
        let baseline_history_len = app.app.session_history_len();
        let baseline_transcript_len = app.app.conversation.transcript().history().order.len();
        let note = protocol::HistoryNote::process_status("background process 9 finished");
        app.app.conversation.inject_commit_failure(
            smelt_store::SessionCommitFailure::InvalidCommand {
                message: "injected note rejection".into(),
            },
        );

        assert!(app.app.begin_process_status_turn(note.clone()).is_none());

        assert_eq!(app.app.session_history_len(), baseline_history_len + 1);
        assert!(app
            .app
            .session_history_range(baseline_history_len..baseline_history_len + 1)
            .unwrap()
            .first()
            .and_then(HistoryItem::as_note)
            .is_some_and(|note| {
                note.context_name() == Some(protocol::DEFAULT_CONTEXT_NOTE_NAME)
            }));
        assert_eq!(
            app.app.conversation.transcript().history().order.len(),
            baseline_transcript_len
        );
        assert!(process_status_blocks(&app).is_empty());
        assert!(app
            .drain_engine_sends()
            .into_iter()
            .all(|command| !matches!(command, protocol::UiCommand::StartTurn(_))));

        assert!(app.app.retry_blocked_persistence());
        let turn = app
            .app
            .begin_process_status_turn(note)
            .expect("note retry starts");
        app.app.conversation.set_active(Some(turn));
        let reader = lineage_reader(&app);
        let history = lineage_history(&reader);
        assert_eq!(history.len(), baseline_history_len + 2);
        assert_eq!(
            history
                .iter()
                .filter(|item| item.note_kind() == Some(protocol::HistoryNoteKind::ProcessStatus))
                .count(),
            1
        );
    }

    #[test]
    fn legacy_history_without_turn_rows_explains_continuation_requirement() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app
            .session_append_history(HistoryItem::user(Content::text("legacy request")));
        app.app
            .session_append_history(HistoryItem::assistant(protocol::AssistantStep::terminal(
                Some(Content::text("legacy response")),
                None,
                Vec::new(),
            )));
        app.app.save_session_and_flush();
        assert_eq!(app.app.conversation.last_terminal_turn_id(), None);

        assert!(app.app.begin_agent_turn("", Content::text("")).is_none());
        assert!(app.app.overlays.notification().is_some_and(|notification| {
            notification.summary.contains("no durable prior turn")
        }));
        assert!(app
            .app
            .begin_command_request_turn(
                "/continue".into(),
                String::new(),
                smelt_core::custom_commands::CommandOverrides::default(),
                crate::app::CommandTurnStart::ContinueFromLast,
            )
            .is_none());
        assert!(app
            .drain_engine_sends()
            .into_iter()
            .all(|command| !matches!(command, protocol::UiCommand::StartTurn(_))));
        let reader = lineage_reader(&app);
        assert!(reader.turns().unwrap().is_empty());
    }

    #[test]
    fn engine_rejection_records_failed_after_durable_submit() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.disconnect_engine_commands();

        assert!(app
            .app
            .begin_agent_turn("rejected", Content::text("rejected"))
            .is_none());
        let _ = app.app.flush_persist();

        let reader = lineage_reader(&app);
        let turns = reader.turns().expect("read turns");
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].state, smelt_store::TurnState::Failed);
        assert_eq!(
            turns[0].terminal_reason.as_deref(),
            Some("engine_channel_rejected")
        );
    }

    #[test]
    fn committed_ready_turn_resumes_after_receipt_publication_retry() {
        let runtime = tempfile::tempdir().expect("create shared runtime root");
        let session_id;
        let sessions_root;
        {
            let mut app = crate::app::test_harness::TestApp::builder()
                .with_runtime_home(runtime.path())
                .build();
            app.app
                .session_append_history(HistoryItem::note(protocol::HistoryNote::context(
                    "published baseline",
                )));
            app.app.save_session_and_flush();
            session_id = app.app.conversation.session().id.clone();
            sessions_root = app.app.core.sessions.sessions_dir();
            app.type_text("durable before dispatch failure");
            app.clear_actions();
            app.app.conversation.inject_publish_failure();

            app.press(crossterm::event::KeyCode::Enter);

            assert!(!app.state().agent_running);
            assert!(app.app.turn_submission_is_pending());
            assert!(!app.app.conversation.is_read_only());
            assert!(app.actions().iter().all(|action| !matches!(
                action,
                crate::app::test_harness::Action::EngineSend(command)
                    if matches!(command.as_ref(), protocol::UiCommand::StartTurn(_))
            )));
            let reader = lineage_reader(&app);
            assert_eq!(
                reader.turns().unwrap()[0].state,
                smelt_store::TurnState::Ready
            );
            app.clear_actions();

            assert!(app.app.retry_blocked_persistence());
            let _ = app.app.flush_persist();

            assert!(app.state().agent_running);
            assert!(!app.app.turn_submission_is_pending());
            assert!(!app.app.conversation.is_read_only());
            assert_eq!(
                app.drain_engine_sends()
                    .iter()
                    .filter(|command| matches!(command, protocol::UiCommand::StartTurn(_)))
                    .count(),
                1
            );
            let _ = app.app.flush_persist();
            assert_eq!(reader.turns().unwrap().len(), 1);
            assert_eq!(
                reader.turns().unwrap()[0].state,
                smelt_store::TurnState::Running
            );
        }

        let writer = smelt_store::OwnedLineageWriter::open_existing(&sessions_root, &session_id)
            .expect("writable restart recovers nonterminal turn");
        let recovery = writer
            .startup_recovery()
            .expect("ready turn was interrupted on restart");
        assert_eq!(
            recovery.interrupted_turns,
            vec![smelt_store::TurnId::new(1)]
        );
        let reader =
            smelt_store::LineageSessionReader::open_existing(&sessions_root, &session_id).unwrap();
        assert_eq!(
            lineage_turn(&reader, 1).state,
            smelt_store::TurnState::Interrupted
        );
        writer.release().unwrap();
    }

    #[test]
    fn session_resume_interrupts_running_turn_without_redispatch() {
        let runtime = tempfile::tempdir().expect("create shared runtime root");
        let session_id;
        let turn_id;
        {
            let mut app = crate::app::test_harness::TestApp::builder()
                .with_runtime_home(runtime.path())
                .build();
            let turn = app
                .app
                .begin_agent_turn("before restart", Content::text("before restart"))
                .expect("turn starts");
            turn_id = turn.turn_id;
            app.app.conversation.set_active(Some(turn));
            let _ = app.app.flush_persist();
            session_id = app.app.conversation.session().id.clone();
            let reader = lineage_reader(&app);
            assert_eq!(
                lineage_turn(&reader, turn_id).state,
                smelt_store::TurnState::Running
            );
        }

        let mut resumed = crate::app::test_harness::TestApp::builder()
            .with_runtime_home(runtime.path())
            .build();
        resumed.clear_actions();
        resumed.app.load_session_by_id(&session_id);

        assert_eq!(
            resumed.app.conversation.session().id,
            session_id,
            "resume failed: {:?}",
            resumed
                .app
                .overlays
                .notification()
                .map(|notification| notification.summary.as_str())
        );
        assert!(!resumed.agent_running());
        assert!(resumed.actions().iter().all(|action| !matches!(
            action,
            crate::app::test_harness::Action::EngineSend(command)
                if matches!(command.as_ref(), protocol::UiCommand::StartTurn(_))
        )));
        assert_eq!(
            resumed.app.conversation.last_terminal_turn_id(),
            Some(turn_id)
        );
        assert!(!resumed.app.conversation.is_read_only());
        let reader = lineage_reader(&resumed);
        assert_eq!(
            lineage_turn(&reader, turn_id).state,
            smelt_store::TurnState::Interrupted
        );
        assert_eq!(
            resumed.app.conversation.acknowledged_head(),
            reader.snapshot().unwrap().head
        );
    }

    #[test]
    fn request_start_dispatches_store_history_without_full_reads() {
        const BASE_HISTORY_LEN: usize = 128;
        let mut app = large_saved_session_app(BASE_HISTORY_LEN);
        let old_history_len = app.app.conversation.session().history.len();
        assert!(old_history_len >= BASE_HISTORY_LEN);

        smelt_perf::perf::set_enabled(true);
        smelt_perf::perf::clear();
        let turn = app
            .app
            .begin_agent_turn("new request", Content::text("new request"))
            .expect("test app has a usable model");
        let snapshot = smelt_perf::perf::snapshot();
        smelt_perf::perf::set_enabled(false);
        app.app.conversation.set_active(Some(turn));

        assert_no_full_request_start_reads(&snapshot);
        assert_perf_value_at_most(&snapshot, "store:history:dirty_suffix_rows", 1);
        assert_perf_value_at_most(&snapshot, "store:session:history_rows_inserted", 1);
        assert_perf_value_at_most(&snapshot, "store:transcript:dirty_record_suffix_rows", 1);
        assert_perf_value_at_most(&snapshot, "store:transcript:record_db_rows_inserted", 1);

        let payload = app
            .drain_engine_sends()
            .into_iter()
            .find_map(|cmd| match cmd {
                protocol::UiCommand::StartTurn(payload) => Some(payload),
                _ => None,
            })
            .expect("turn dispatched");
        match payload.history {
            protocol::ModelHistorySource::Store {
                ref prefix,
                first_live_index,
                end_index,
                ref suffix,
                ..
            } => {
                assert!(prefix.is_empty());
                assert_eq!(first_live_index, 0);
                assert_eq!(end_index, old_history_len);
                assert!(suffix.is_empty());
            }
            protocol::ModelHistorySource::Items { .. } => {
                panic!("request start cloned full history")
            }
        }
        assert_eq!(
            payload.input.provider_content().text_content(),
            "new request"
        );
        assert_eq!(
            app.app.conversation.session().history.len(),
            old_history_len + 1
        );
    }

    #[test]
    fn request_start_appends_clear_for_deep_named_context() {
        const TRAILING_HISTORY_LEN: usize = 1024;
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let mut session =
            smelt_core::session::Session::new(app.app.core.env.pid(), app.app.core.env.cwd());
        session.first_user_message = Some("old user".into());
        session
            .history
            .push(HistoryItem::note(protocol::HistoryNote::named_context(
                "goal", "old goal",
            )));
        session.history.extend((0..TRAILING_HISTORY_LEN).map(|idx| {
            if idx.is_multiple_of(2) {
                HistoryItem::user(Content::text(format!("old user {idx}")))
            } else {
                HistoryItem::Assistant(protocol::AssistantStep::terminal(
                    Some(Content::text(format!("old assistant {idx}"))),
                    None,
                    Vec::new(),
                ))
            }
        }));
        app.app.load_session(session);
        app.app.restore_screen();
        app.app.ensure_current_context_note();
        app.app.apply_pending_history_appends_for_request();
        app.app.save_session();
        app.app.flush_persist();

        app.app.set_context_note("goal".into(), None);
        smelt_perf::perf::set_enabled(true);
        smelt_perf::perf::clear();
        let turn = app
            .app
            .begin_agent_turn("new request", Content::text("new request"))
            .unwrap_or_else(|| {
                panic!(
                    "request did not start: read_only={}, notification={:?}",
                    app.app.conversation.is_read_only(),
                    app.app
                        .overlays
                        .notification()
                        .map(|notification| notification.summary.as_str())
                )
            });
        let snapshot = smelt_perf::perf::snapshot();
        smelt_perf::perf::set_enabled(false);
        app.app.conversation.set_active(Some(turn));

        assert_no_full_request_start_reads(&snapshot);
        assert_perf_value_at_most(&snapshot, "store:history:dirty_suffix_rows", 2);
        assert_perf_value_at_most(&snapshot, "store:session:history_rows_inserted", 2);
        assert_perf_value_at_most(&snapshot, "store:session:history_rows_deleted", 0);
        let tail = app
            .app
            .session_history_range(
                app.app.session_history_len().saturating_sub(2)..app.app.session_history_len(),
            )
            .unwrap();
        assert!(tail[0]
            .as_note()
            .is_some_and(|note| note.context_name() == Some("goal") && note.text().is_empty()));
        assert!(matches!(tail[1], HistoryItem::User { .. }));
    }

    #[test]
    fn command_turn_metadata_snapshot_matches_committed_request() {
        let mut app = crate::app::test_harness::TestApp::builder().build();

        let turn = app
            .app
            .begin_command_request_turn(
                "/fix".into(),
                "fix it".into(),
                smelt_core::custom_commands::CommandOverrides::default(),
                crate::app::CommandTurnStart::Fresh,
            )
            .expect("test app has a usable model");
        app.app.conversation.set_active(Some(turn));

        assert_eq!(
            app.app.conversation.session().first_user_message.as_deref(),
            Some("/fix")
        );
        assert_eq!(
            app.app
                .conversation
                .session()
                .metadata_snapshots
                .as_slice()
                .last()
                .map(|(idx, _)| *idx),
            Some(app.app.conversation.session().history.len())
        );
        assert!(matches!(
            app.app.conversation.session().history.last(),
            Some(HistoryItem::User {
                display: Some(display),
                command: true,
                ..
            }) if display == "/fix"
        ));
        let transcript = app.app.conversation.transcript().history();
        assert!(transcript
            .order
            .last()
            .and_then(|id| transcript.block(*id))
            .is_some_and(|block| matches!(
                block,
                Block::User {
                    text,
                    command: true,
                    ..
                } if text == "/fix"
            )));
        assert!(app.drain_engine_sends().into_iter().any(|command| matches!(
            command,
            protocol::UiCommand::StartTurn(payload)
                if matches!(payload.input, protocol::StartTurnInput::User { command: true, .. })
        )));
    }

    #[test]
    fn expanding_at_file_records_absolute_path_as_read() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("note.txt");
        std::fs::write(&file, "hello\nworld").unwrap();

        let mut app = crate::app::test_harness::TestApp::builder()
            .with_cwd(tmp.path())
            .build();
        let expanded = app.app.expand_at_file_refs_in_text("summarize @note.txt");

        let path = file.to_string_lossy();
        assert!(expanded.contains(&format!(
            "<attached_file path=\"{path}\" tool=\"read_file\" already_read=\"true\" source=\"user_attachment\">"
        )));
        assert!(expanded.contains("Called the read_file tool with the following input:"));
        assert!(expanded.contains("   1\thello"));
        assert!(app.app.core.files.has(&path));
    }

    #[test]
    fn expanding_at_notebook_uses_notebook_renderer() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("nb.ipynb");
        std::fs::write(
            &file,
            r##"{"cells":[{"cell_type":"markdown","id":"intro","source":["# Title\n"]}]}"##,
        )
        .unwrap();

        let mut app = crate::app::test_harness::TestApp::builder()
            .with_cwd(tmp.path())
            .build();
        let expanded = app.app.expand_at_file_refs_in_text("summarize @nb.ipynb");

        let path = file.to_string_lossy();
        assert!(expanded.contains(&format!(
            "<attached_file path=\"{path}\" tool=\"read_file\" already_read=\"true\" source=\"user_attachment\">"
        )));
        assert!(expanded.contains("Called the read_file tool with the following input:"));
        assert!(expanded.contains("--- Cell 0 [markdown] id=intro ---"));
        assert!(app.app.core.files.has(&path));
    }

    #[test]
    fn idle_job_completion_starts_turn_with_process_status_block() {
        let mut app = crate::app::test_harness::TestApp::builder().build();

        app.app
            .handle_job_completed(smelt_core::process::JobCompletion {
                id: "1234".into(),
                exit_code: Some(0),
                termination: protocol::JobTermination::Exited,
            });

        assert!(app.app.agent_is_running());
        assert_eq!(user_blocks(&app), Vec::<String>::new());
        assert_eq!(
            process_status_blocks(&app),
            vec!["background process 1234 finished successfully"]
        );
    }

    #[test]
    fn idle_oom_completion_starts_turn_with_distinct_process_status() {
        let mut app = crate::app::test_harness::TestApp::builder().build();

        app.app
            .handle_job_completed(smelt_core::process::JobCompletion {
                id: "proc_123".into(),
                exit_code: None,
                termination: protocol::JobTermination::OutOfMemory,
            });

        assert!(app.app.agent_is_running());
        assert_eq!(
            process_status_blocks(&app),
            vec!["background process proc_123 was terminated after an out-of-memory event"]
        );
    }

    #[test]
    fn process_status_resize_keeps_transcript_cursor_in_bounds() {
        let mut app = crate::app::test_harness::TestApp::builder().build();

        app.app
            .handle_job_completed(smelt_core::process::JobCompletion {
                id: String::new(),
                exit_code: None,
                termination: protocol::JobTermination::Signaled,
            });
        app.render_silent();
        app.feed_one(crate::app::test_harness::SourceEvent::Resize {
            width: 1,
            height: 12,
        });
        app.render_silent();
        app.feed_one(crate::app::test_harness::SourceEvent::Resize {
            width: 50,
            height: 1,
        });
        app.render_silent();

        app.assert_invariants();
    }

    #[test]
    fn running_agent_job_completion_queues_process_status_append() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.start_turn(7);

        let before = app
            .app
            .conversation
            .pending_history_appends()
            .iter()
            .map(|append| append.history_item())
            .collect::<Vec<_>>();

        app.app
            .handle_job_completed(smelt_core::process::JobCompletion {
                id: "4242".into(),
                exit_code: Some(9),
                termination: protocol::JobTermination::Exited,
            });

        assert!(process_status_blocks(&app).is_empty());
        let expected = protocol::HistoryItem::note(protocol::HistoryNote::process_status_event(
            protocol::ProcessStatusEvent::background_process_completed(
                "4242",
                Some(9),
                protocol::JobTermination::Exited,
            ),
        ));
        let after = app
            .app
            .conversation
            .pending_history_appends()
            .iter()
            .map(|append| append.history_item())
            .collect::<Vec<_>>();
        assert_eq!(&after[..before.len()], before.as_slice());
        assert_eq!(&after[before.len()..], std::slice::from_ref(&expected));

        let process_status_appends: Vec<_> = app
            .app
            .conversation
            .pending_history_appends()
            .iter()
            .filter(|append| {
                append.history_item().note_kind() == Some(protocol::HistoryNoteKind::ProcessStatus)
            })
            .collect();
        assert_eq!(process_status_appends.len(), 1);
        assert_eq!(process_status_appends[0].history_item(), expected);
    }

    #[test]
    fn queued_process_status_starts_process_status_turn() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let note = protocol::HistoryNote::process_status("background process 77 exited");
        let text = note.text().to_string();

        assert!(
            app.app
                .start_queued_input(crate::app::QueuedInput::ProcessStatus(note))
                .is_ok(),
            "test app has a usable model"
        );

        assert!(app.app.agent_is_running());
        assert_eq!(process_status_blocks(&app), vec![text]);
        assert!(user_blocks(&app).is_empty());
    }

    #[test]
    fn process_status_history_update_stays_out_of_lua_conversation() {
        let mut app = crate::app::test_harness::TestApp::builder().build();

        app.app
            .handle_job_completed(smelt_core::process::JobCompletion {
                id: "751225".into(),
                exit_code: Some(1),
                termination: protocol::JobTermination::Exited,
            });
        let turn_id = app
            .app
            .active_agent_turn_id()
            .expect("process turn started");
        let (start_content, current_note) = app
            .drain_engine_sends()
            .into_iter()
            .find_map(|cmd| match cmd {
                protocol::UiCommand::StartTurn(payload) => Some((
                    payload.input.provider_content(),
                    payload.input.note_ref().cloned(),
                )),
                _ => None,
            })
            .expect("process turn dispatched to engine");
        let current_note = current_note.expect("process turn carries typed note");
        let expected_note = protocol::HistoryNote::process_status_event(
            protocol::ProcessStatusEvent::background_process_completed(
                "751225",
                Some(1),
                protocol::JobTermination::Exited,
            ),
        );
        assert_eq!(current_note, expected_note);
        assert_eq!(
            start_content.text_content(),
            protocol::process_status_note("background process 751225 exited with code 1")
        );

        app.feed_one(crate::app::test_harness::SourceEvent::engine(
            protocol::EngineEvent::HistoryUpdated {
                turn_id,
                update: protocol::CanonicalHistoryDelta::new(
                    0,
                    vec![
                        HistoryItem::user(Content::text("previous user message")),
                        HistoryItem::note(current_note.clone()),
                    ],
                ),
            },
        ));

        assert!(matches!(
            &app.app.conversation.session().history[1],
            HistoryItem::Note(protocol::HistoryNote::ProcessStatus { text, event })
                if text == "background process 751225 exited with code 1"
                    && event.as_ref().and_then(protocol::ProcessStatusEvent::process_id) == Some("751225")
                    && event.as_ref().and_then(|event| event.exit_code()) == Some(1)
        ));

        let (count, first_content, contains_marker): (i64, String, bool) = app
            .eval_lua(
                r#"
                    local rows = smelt.session.conversation()
                    local contains_marker = false
                    for _, row in ipairs(rows) do
                        if string.find(row.content, "%[smelt:process%]") then
                            contains_marker = true
                        end
                    end
                    return #rows, rows[1] and rows[1].content or "", contains_marker
                    "#,
            )
            .expect("conversation query succeeds");

        assert_eq!(count, 1);
        assert_eq!(first_content, "previous user message");
        assert!(!contains_marker);
    }

    #[test]
    fn lua_conversation_limit_returns_recent_rows() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        app.app.conversation.replace_history_for_harness(vec![
            HistoryItem::user(Content::text("first user")),
            HistoryItem::note(protocol::HistoryNote::named_context("hidden", "note")),
            protocol::HistoryItem::Assistant(protocol::AssistantStep::terminal(
                Some(Content::text("first assistant")),
                None,
                Vec::new(),
            )),
            HistoryItem::user(Content::text("second user")),
            protocol::HistoryItem::Assistant(protocol::AssistantStep::terminal(
                Some(Content::text("second assistant")),
                None,
                Vec::new(),
            )),
        ]);

        let (count, first, second): (i64, String, String) = app
            .eval_lua(
                r#"
                    local rows = smelt.session.conversation({ limit = 2 })
                    return #rows, rows[1] and rows[1].content or "", rows[2] and rows[2].content or ""
                    "#,
            )
            .expect("conversation limit query succeeds");

        assert_eq!(count, 2);
        assert_eq!(first, "second user");
        assert_eq!(second, "second assistant");
    }

    #[tokio::test]
    async fn cancelling_turn_does_not_kill_registered_background_process() {
        let mut app = crate::app::test_harness::TestApp::builder().build();
        let supervisor = app.app.core.jobs.clone();
        let id = supervisor
            .spawn_background(
                "echo alive; sleep 30",
                &smelt_core::process::ShellSpec::default(),
                &app.app.core.env.cwd(),
                std::time::Instant::now(),
            )
            .await
            .unwrap();
        app.start_turn(1);

        app.cancel();

        assert!(supervisor.snapshot_output(&id).unwrap().running);
        assert_eq!(supervisor.running_count(), 1);
        let _ = supervisor.stop(&id).await;
    }

    #[test]
    fn empty_key_env_returns_empty_string_without_consulting_environment() {
        // Callers configured with no API key (e.g. local-only models) hit
        // this path; the resolver must never be invoked.
        let mut called = false;
        let out = lookup_api_key("", |_| {
            called = true;
            Ok("nope".into())
        });
        assert_eq!(out, Ok(String::new()));
        assert!(!called, "resolver should be skipped for empty key_env");
    }

    #[test]
    fn nonempty_key_env_returns_resolved_value() {
        let out = lookup_api_key("MY_KEY", |var| {
            assert_eq!(var, "MY_KEY");
            Ok("secret".into())
        });
        assert_eq!(out, Ok("secret".into()));
    }

    #[test]
    fn missing_env_var_maps_to_not_set_error_with_var_name() {
        let out = lookup_api_key("MY_KEY", |_| Err(std::env::VarError::NotPresent));
        assert_eq!(
            out,
            Err(ApiKeyError::NotSet {
                var: "MY_KEY".into()
            })
        );
    }

    #[test]
    fn non_unicode_env_var_maps_to_not_unicode_error() {
        // `std::env::VarError::NotUnicode` carries an `OsString`; we don't
        // care about its payload - only the variant matters for our message.
        let out = lookup_api_key("MY_KEY", |_| {
            Err(std::env::VarError::NotUnicode(std::ffi::OsString::new()))
        });
        assert_eq!(
            out,
            Err(ApiKeyError::NotUnicode {
                var: "MY_KEY".into()
            })
        );
    }

    #[test]
    fn not_set_error_message_names_the_missing_var() {
        let msg = ApiKeyError::NotSet {
            var: "OPENAI_API_KEY".into(),
        }
        .message();
        assert!(msg.contains("OPENAI_API_KEY"));
        assert!(msg.contains("not set"));
    }

    #[test]
    fn not_unicode_error_message_names_the_var() {
        let msg = ApiKeyError::NotUnicode {
            var: "WEIRD_KEY".into(),
        }
        .message();
        assert!(msg.contains("WEIRD_KEY"));
        assert!(msg.contains("non-Unicode"));
    }
}
