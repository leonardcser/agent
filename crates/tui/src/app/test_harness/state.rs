use super::*;

impl TestApp {
    pub fn builder() -> TestAppBuilder {
        TestAppBuilder::default()
    }

    /// Force the app into "agent turn active" state with the given
    /// `turn_id`. Subsequent engine source events flow through
    /// the active-turn dispatch path (`handle_engine_event` and
    /// `dispatch_control` with tool tracking) instead of the idle
    /// handler. No-op if a turn is already running.
    ///
    /// Used by the fuzz target to reach engine code paths a user would
    /// reach by submitting a prompt, without going through the full
    /// HTTP/auth-bearing `begin_agent_turn` flow.
    pub fn start_turn(&mut self, turn_id: u64) {
        if self.app.conversation.is_active() {
            return;
        }
        self.app.conversation.begin_turn();
        self.app
            .conversation
            .set_active(Some(crate::app::TurnState {
                turn_id,
                canonical: false,
                pending: Vec::new(),
                permissions: self.app.core.permissions.snapshot(),
                submitted_history_idx: self.app.session_history_len().saturating_sub(1),
                rewind_history_idx: None,
                assistant_output_started: false,
                _perf: smelt_perf::perf::begin("test_harness:turn"),
            }));
        // Production `dispatch_prepared_turn` flips `working` into `Working`
        // and publishes `turn_start` before yielding an active turn. The
        // harness short-circuits the HTTP-bearing dispatch path; mirror those
        // side effects so plugins and invariants see the same lifecycle.
        self.app
            .working
            .begin(smelt_core::working::TurnPhase::Working);
        self.app.core.signals.emit_dyn(
            "turn_start",
            std::rc::Rc::new(smelt_core::signals::EventStub),
        );
        self.app.pump_lua();
    }

    pub fn start_submitted_turn(&mut self, text: &str) {
        let sent_at_ms = engine::clock::unix_time_ms(self.app.core.clock.as_ref());
        let turn = self
            .app
            .begin_agent_turn(text, protocol::Content::text(text), sent_at_ms)
            .expect("test app has a usable model");
        self.app.conversation.set_active(Some(turn));
    }

    /// Complete the active synthetic turn through the normal engine-event path.
    pub fn finish_turn(&mut self) -> bool {
        let Some(turn_id) = self.current_turn_id() else {
            return false;
        };
        self.feed_one(SourceEvent::engine(EngineEvent::TurnComplete {
            turn_id,
            history: None,
            meta: None,
        }));
        true
    }

    /// Whether an agent turn is currently active.
    pub fn agent_running(&self) -> bool {
        self.app.agent_is_running()
    }

    /// Snapshot the pending invocation identities on the active turn. Returns
    /// an empty vector when no turn is active.
    pub fn pending_tool_invocation_ids(&self) -> Vec<protocol::InvocationId> {
        self.app
            .conversation
            .active()
            .map(|turn| turn.pending.iter().map(|tool| tool.invocation_id).collect())
            .unwrap_or_default()
    }

    /// Whether streaming `text` / `thinking` / exec buffers currently hold
    /// uncommitted content. Used by post-event invariants that assert a
    /// specific event flushed the relevant buffer.
    pub fn streaming_state(&self) -> StreamingState {
        let (text, thinking, exec) = self.app.conversation.streaming_state();
        StreamingState {
            text,
            thinking,
            exec,
        }
    }

    /// User-facing session history length. Used by post-event invariants that
    /// assert compaction or `set_history` replaced the conversation.
    pub fn session_message_count(&self) -> usize {
        self.app.session_history_len()
    }

    /// Canonical session history as seen by the TUI. Focused engine-event and
    /// persistence state machines compare this against independent suffix and
    /// append models after every transition.
    pub fn session_history(&self) -> &[protocol::HistoryItem] {
        &self.app.conversation.session().history
    }

    /// Read-only copy of current session state for integration assertions.
    pub fn session_snapshot(&self) -> smelt_core::session::Session {
        self.app.conversation.session().clone()
    }

    /// `turn_id` of the active agent turn, if any. Used by fuzz ops that
    /// synthesize engine events whose dispatch is gated on a matching id
    /// (e.g. `TurnComplete`, `Messages`).
    pub fn current_turn_id(&self) -> Option<u64> {
        self.app.active_agent_turn_id()
    }

    /// Number of messages already promoted into the active turn's request
    /// queue. Used by `Steered` invariants that assert ack drain semantics.
    pub fn queued_message_count(&self) -> usize {
        self.app.prompt.queued_request_len()
    }

    /// Whether completing the active turn would immediately start a queued
    /// input with non-empty display text.
    pub fn next_queued_input_starts_turn(&self) -> bool {
        self.app
            .prompt
            .queued_texts()
            .first()
            .is_some_and(|text| !text.is_empty())
    }

    /// Whether the follow-up started after TurnComplete will append a fresh
    /// context note. Without a checkpoint, the incoming model-history snapshot
    /// replaces local history and drops any current context note; with a
    /// checkpoint, only notes before `first_live_index` survive the merge.
    pub fn next_follow_up_appends_context_note(&self) -> bool {
        if !self.next_queued_input_starts_turn() && !self.has_pending_history_follow_up() {
            return false;
        }
        let context = self.app.current_context_note_text();
        if let Some(first_live_index) = self.checkpoint_first_live_index() {
            return !self
                .app
                .conversation
                .session()
                .history
                .iter()
                .take(first_live_index)
                .filter_map(protocol::HistoryItem::as_note)
                .any(|note| {
                    note.kind() == protocol::HistoryNoteKind::Context
                        && note.text() == context.as_str()
                });
        }
        true
    }

    /// Side-channel: push a synthetic queued message. In production
    /// `queued_inputs` is filled by pressing Enter on the prompt while a
    /// turn is active; the harness short-circuits that flow but honors
    /// the same `MAX_QUEUED_MESSAGES` cap so the fuzz observes the real
    /// drop-on-overflow behavior instead of unbounded growth.
    pub fn push_queued_message(&mut self, text: String) {
        self.app
            .prompt
            .try_queue_turn(crate::app::QueuedInput::request_from_text(
                text.clone(),
                text,
                engine::clock::unix_time_ms(self.app.core.clock.as_ref()),
            ));
    }

    /// Side-channel: seed prompt history without submitting through the engine.
    /// Storybook uses this to open reverse-history UI deterministically.
    pub fn push_history_entry(&mut self, text: String) {
        self.app.prompt.push_history(text);
    }

    /// Snapshot of the working-status bar's live state. Used by fuzz
    /// invariants that assert phase transitions (e.g. compaction ends with
    /// `animating == false`, `Retrying` event leaves `animating == true`).
    pub fn working_state(&self) -> WorkingSnapshot {
        WorkingSnapshot {
            animating: self.app.working.is_animating(),
            busy: self.app.busy_stack.is_busy(),
        }
    }

    /// Accumulated session cost in USD. Used by `TokenUsage` invariants
    /// asserting cost is monotonically non-decreasing.
    pub fn session_cost_usd(&self) -> f64 {
        self.app.conversation.session().session_cost_usd
    }

    /// Current authoritative context-token snapshot for the live history, when
    /// a non-background usage report has set one.
    pub fn context_tokens(&self) -> Option<u32> {
        self.app.conversation.session().current_context_tokens()
    }

    /// Active context checkpoint prefix length, if compaction installed one.
    /// `TuiApp::set_history` preserves this prefix and merges incoming model
    /// history after it.
    pub fn checkpoint_first_live_index(&self) -> Option<usize> {
        self.app
            .conversation
            .session()
            .checkpoint
            .as_ref()
            .map(|checkpoint| checkpoint.first_live_index)
    }

    /// Number of deferred model-history notes that will be committed when the
    /// active turn finishes successfully.
    pub fn pending_history_append_count(&self) -> usize {
        self.app.conversation.pending_history_append_count()
    }

    /// Whether an unconsumed deferred note will start a follow-up turn after
    /// the active turn finishes successfully.
    pub fn has_pending_history_follow_up(&self) -> bool {
        self.app
            .conversation
            .pending_history_appends()
            .iter()
            .any(|append| {
                append.delivery() == crate::app::PendingHistoryDelivery::FollowUpIfUnconsumed
            })
    }

    /// Set the configured context window size used by the prompt bar's
    /// percentage display.
    pub fn set_context_window(&mut self, context_window: Option<u32>) {
        self.app.core.config.context_window = context_window;
    }

    /// Restrict tool permissions to the app's injected workspace.
    pub fn restrict_permissions_to_cwd(&mut self) {
        let mut permissions = self.app.core.permissions.snapshot().as_ref().clone();
        permissions.set_workspace(std::path::PathBuf::from(self.app.workspace.cwd()));
        permissions.set_restrict_to_workspace(true);
        self.app.core.permissions.replace(permissions);
    }

    /// Allow tool paths outside the injected workspace in focused harness flows.
    pub fn allow_permissions_outside_cwd(&mut self) {
        let mut permissions = self.app.core.permissions.snapshot().as_ref().clone();
        permissions.set_restrict_to_workspace(false);
        self.app.core.permissions.replace(permissions);
    }

    pub(crate) fn replace_permissions_for_harness(
        &mut self,
        permissions: smelt_core::permissions::Permissions,
    ) {
        self.app.core.permissions.replace(permissions);
    }

    /// Force one tool through the confirmation path for deterministic harness flows.
    pub fn require_tool_confirmation(&mut self, tool: &str, patterns: &[String]) {
        let permissions =
            self.app
                .core
                .permissions
                .snapshot()
                .with_overrides(&protocol::PermissionOverrides {
                    tools: Some(protocol::RuleSetOverride {
                        ask: vec![tool.to_owned()],
                        ..Default::default()
                    }),
                    subcommands: [(
                        tool.to_owned(),
                        protocol::RuleSetOverride {
                            ask: patterns.to_vec(),
                            ..Default::default()
                        },
                    )]
                    .into_iter()
                    .collect(),
                });
        self.app.core.permissions.replace(permissions);
        self.app
            .conversation
            .refresh_active_permissions(self.app.core.permissions.snapshot());
    }

    /// Add one session-scoped blanket tool approval.
    pub fn approve_tool_for_session(&self, tool: &str) {
        self.app
            .core
            .permissions
            .approvals()
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .add_session_tool(tool, Vec::new());
    }

    /// Replace model-picker candidates without selecting a model.
    pub fn set_available_models(&mut self, models: Vec<smelt_core::config::ResolvedModel>) {
        self.app.core.config.available_models = models;
    }

    /// Install and select one synthetic model for model-backed scenarios.
    pub fn use_model(&mut self, model: smelt_core::config::ResolvedModel) {
        self.app.core.config.available_models = vec![model.clone()];
        self.app.core.config.set_model_selection(
            smelt_core::ModelSelectionState {
                requested_key: Some(model.key.clone()),
                requested_by: smelt_core::ModelSelectionSource::CatalogDefault,
                active: Some(smelt_core::ActiveModel::from_resolved(&model)),
            },
            &self.app.core.startup_overrides,
        );
    }

    pub(crate) fn set_model_selection_for_harness(
        &mut self,
        selection: smelt_core::ModelSelectionState,
    ) {
        self.app
            .core
            .config
            .set_model_selection(selection, &self.app.core.startup_overrides);
    }

    pub(crate) fn replace_active_model_for_harness(&mut self, model: smelt_core::ActiveModel) {
        let mut selection = self.app.core.config.model_selection.clone();
        selection.active = Some(model);
        self.app
            .core
            .config
            .set_model_selection(selection, &self.app.core.startup_overrides);
    }

    pub(crate) fn set_request_audit_for_harness(&mut self, mode: protocol::RequestAuditMode) {
        self.app.core.config.request_audit = mode;
    }

    pub(crate) fn set_configured_agent_mode_for_harness(&mut self, mode: protocol::AgentMode) {
        self.app.core.config.mode = mode;
    }

    pub(crate) fn install_skill_loader_for_harness(
        &mut self,
        loader: std::sync::Arc<engine::SkillLoader>,
    ) {
        self.app.core.skills = Some(loader);
    }

    /// Number of transcript blocks. Used by event invariants that assert
    /// a block was pushed.
    pub fn transcript_block_count(&self) -> usize {
        self.app.conversation.transcript().history().len()
    }

    /// Number of confirm dialogs currently registered with the core. Used
    /// by `RequestPermission` invariants to assert the dispatch either
    /// auto-approved (no confirm registered) or registered exactly one.
    pub fn pending_confirm_count(&self) -> usize {
        self.app.core.confirms.len()
    }

    /// Lowest-numbered pending confirm handle. Used by the resolve
    /// side-channel to pick a victim deterministically.
    pub fn first_pending_confirm(&self) -> Option<u64> {
        self.app.core.confirms.first_handle()
    }

    /// Number of dialogs deferred because a recent keystroke gated them.
    /// They get replayed on the next idle tick. Used to confirm the
    /// `should_queue` branch in `dispatch_control` was actually hit.
    pub fn pending_deferred_dialog_count(&self) -> usize {
        self.app.overlays.deferred_dialog_count()
    }

    /// Slice of `Action`s appended since `idx`. PostChecks use this to
    /// count or inspect specific `UiCommand` variants without each one
    /// growing a dedicated Snapshot field per variant. Pair with
    /// `Snapshot::action_count`: capture the index before dispatch, then
    /// call `actions_since(pre.action_count)` after.
    pub fn actions_since(&self, idx: usize) -> &[Action] {
        let len = self.actions.len();
        let start = idx.min(len);
        &self.actions[start..len]
    }

    /// Side-channel: resolve the first pending confirm with `Yes` or
    /// `No`. Mirrors what the Lua dialog calls into via
    /// `lua_handlers::handle_dialog_decision` without going through the
    /// Lua layer. Returns `true` when a confirm was consumed.
    pub fn resolve_first_confirm(&mut self, approve: bool, message: Option<String>) -> bool {
        let Some(handle_id) = self.first_pending_confirm() else {
            return false;
        };
        let Some(entry) = self.app.core.confirms.take(handle_id) else {
            return false;
        };
        let req = entry.req;
        let choice = if approve {
            smelt_core::transcript_model::ConfirmChoice::Yes
        } else {
            smelt_core::transcript_model::ConfirmChoice::No
        };
        {
            let cancel = self.app.resolve_confirm(
                (choice, message),
                req.invocation_id,
                req.request_id,
                &req.tool_name,
            );
            if cancel {
                self.app.discard_turn(crate::app::TurnEnd::Complete);
            }
        }
        self.drain_cmd();
        true
    }

    pub(crate) fn resolve_first_session_grant_where(
        &mut self,
        matches_grants: impl Fn(&[smelt_core::permissions::PermissionGrant]) -> bool,
    ) -> bool {
        self.resolve_first_grant_where(
            |target| matches!(target, smelt_core::ApprovalTarget::Session),
            matches_grants,
        )
    }

    pub(crate) fn resolve_first_grant_where(
        &mut self,
        matches_target: impl Fn(&smelt_core::ApprovalTarget) -> bool,
        matches_grants: impl Fn(&[smelt_core::permissions::PermissionGrant]) -> bool,
    ) -> bool {
        let Some(handle_id) = self.first_pending_confirm() else {
            return false;
        };
        let Some(req) = self
            .app
            .core
            .confirms
            .get(handle_id)
            .map(|entry| entry.req.clone())
        else {
            return false;
        };
        let Some(option) = req
            .grant_options
            .iter()
            .find(|option| matches_target(&option.target) && matches_grants(&option.grants))
            .cloned()
        else {
            return false;
        };
        let Some(entry) = self.app.core.confirms.take(handle_id) else {
            return false;
        };
        let req = entry.req;
        let cancel = self.app.resolve_confirm(
            (
                smelt_core::transcript_model::ConfirmChoice::Grant(option),
                None,
            ),
            req.invocation_id,
            req.request_id,
            &req.tool_name,
        );
        if cancel {
            self.app.discard_turn(crate::app::TurnEnd::Complete);
        }
        self.drain_cmd();
        true
    }

    /// Smallest pending `smelt.engine.ask` callback id, if any.
    /// Stories that drive `/btw` or other ask flows use this to
    /// pair their synthesised `EngineAskResponse` to the right id.
    pub fn pending_ask_id(&self) -> Option<u64> {
        let shared = self.app.lua.shared().core_arc();
        let cbs = shared.ask_callbacks.lock().ok()?;
        cbs.keys().min().copied()
    }

    /// Working directory string the live app uses (captured at
    /// construction). Stories that seed persisted-session fixtures
    /// match this value into catalog metadata so the resume dialog's
    /// workspace filter keeps the seeded entries.
    pub fn cwd_str(&self) -> &str {
        self.app.workspace.cwd()
    }

    pub fn runtime_home(&self) -> &std::path::Path {
        self.app.core.env.home()
    }

    pub fn runtime_dir(&self) -> &std::path::Path {
        self.app.core.env.runtime_dir()
    }

    pub fn runtime_cache_root(&self) -> std::path::PathBuf {
        self.app.core.env.cache_dir().clone()
    }

    pub fn shell_effect_paths(&self, command: &str) -> Vec<std::path::PathBuf> {
        let args = std::collections::HashMap::from([(
            "command".to_string(),
            serde_json::Value::String(command.to_string()),
        )]);
        self.app
            .core
            .permissions
            .snapshot()
            .effects_for_tool(smelt_core::permissions::ToolOrigin::Lua, "bash", &args)
            .into_iter()
            .flat_map(|effect| match effect {
                smelt_core::permissions::ToolEffect::Shell { paths, .. } => paths,
                _ => Vec::new(),
            })
            .map(|effect| effect.resolution.path().to_path_buf())
            .collect()
    }

    pub fn publish_public_status(&mut self) {
        self.app.publish_public_status();
    }

    pub fn public_status_path(&self) -> Option<&std::path::Path> {
        self.app.platform.public_status_path()
    }

    pub fn session_storage_root(&self) -> &std::path::Path {
        self.app.core.sessions.state_root()
    }

    /// Resume a canonical session through the app's normal storage path.
    pub fn resume_session(&mut self, id: &str) -> bool {
        self.app.load_session_by_id(id)
    }

    pub fn publish_session_catalog_commit(
        &self,
        command: &smelt_store::SessionCommit,
        receipt: &smelt_store::SaveReceipt,
    ) {
        self.app
            .core
            .sessions
            .publish_session_catalog_commit(command, receipt);
    }

    pub fn reconcile_session_catalog(&self) -> Result<(), String> {
        self.app
            .core
            .sessions
            .request_session_catalog_reconciliation();
        if self
            .app
            .core
            .sessions
            .wait_for_session_catalog(std::time::Duration::from_secs(5))
        {
            return Ok(());
        }

        let page = self
            .app
            .core
            .sessions
            .list_session_page_result(smelt_core::session::SessionListQuery {
                limit: 1_000,
                ..smelt_core::session::SessionListQuery::default()
            })
            .map_err(|err| err.to_string())?;
        Err(format!(
            "session catalog did not reconcile: {:?}",
            page.catalog.last_error
        ))
    }

    pub fn mark_project_trusted(&self, cwd: &std::path::Path) -> Result<String, String> {
        self.app.lua.mark_project_trusted(cwd)
    }

    /// Prompt cursor byte offset in source space.
    pub fn prompt_cpos(&self) -> usize {
        self.app
            .ui
            .win(crate::app::PROMPT_WIN)
            .map(|w| w.cpos())
            .unwrap_or(0)
    }

    pub fn prompt_endpoint(&self) -> usize {
        self.app.prompt_win().effective_endpoint()
    }

    pub fn prompt_source(&self) -> String {
        self.app
            .ui
            .buf(crate::app::PROMPT_EDIT_BUF)
            .map(|buf| buf.source().to_string())
            .unwrap_or_default()
    }

    pub fn prompt_attachment_count(&self) -> usize {
        self.app
            .ui
            .buf(crate::app::PROMPT_EDIT_BUF)
            .map_or(0, |buf| buf.attachment_ids.len())
    }

    pub(crate) fn configure_prompt_inputs_for_harness(
        &mut self,
        system_prompt_override: Option<String>,
        instructions: Option<String>,
        skill_section: Option<String>,
    ) {
        self.app.prompt_inputs.system_prompt_override = system_prompt_override;
        self.app.prompt_inputs.instructions = instructions;
        self.app.prompt_inputs.skill_section = skill_section;
    }

    pub(crate) fn task_label(&self) -> Option<&str> {
        self.app.task_label.as_deref()
    }

    /// Snapshot one well-known window without exposing the mutable UI tree.
    pub fn window_snapshot(&self, id: WinId) -> Option<WindowSnapshot> {
        let win = self.app.ui.win(id)?;
        let source_len = self.app.ui.buf(win.buf).map_or(0, |buf| buf.source().len());
        Some(WindowSnapshot {
            cpos: win.cpos(),
            source_len,
            vim_mode: win.vim_mode(),
            selection_anchor: win.selection_anchor(),
            viewport: win.viewport,
            gutter_pad_left: win.config.gutters.pad_left,
        })
    }

    pub(crate) fn paint_rect(
        &self,
        id: crate::smelt_edit::PaintId,
    ) -> Option<crate::smelt_edit::Rect> {
        self.app.ui.paint_rect(id)
    }

    pub(crate) fn split_rect(&self, win: WinId) -> Option<crate::smelt_edit::Rect> {
        self.app.ui.split_rect(win)
    }

    /// Effective prompt selection after Vim visual-mode normalization.
    pub fn prompt_selection_range(&self) -> Option<(usize, usize)> {
        let buf = self.app.ui.buf(crate::app::PROMPT_EDIT_BUF)?;
        let win = self.app.ui.win(crate::app::PROMPT_WIN)?;
        let endpoint = win.effective_endpoint();
        if win.vim_enabled() {
            if let Some(range) = crate::smelt_edit::vim::visual_range(
                win.vim_state(),
                buf.source(),
                endpoint,
                win.vim_mode(),
            ) {
                return Some(range);
            }
        }
        win.selection_range_at(endpoint, buf.source())
    }

    /// Whether paste should target prompt insertion or replace its active selection.
    pub fn prompt_paste_target_ready(&self, selection_range: Option<(usize, usize)>) -> bool {
        if self.prompt_text_input_ready() {
            return true;
        }
        if selection_range.is_none() || self.app.ui.any_drag_active() {
            return false;
        }
        let state = self.state();
        matches!(state.app_focus, AppFocus::Prompt)
            && !state.agent_running
            && !state.cmdline_open
            && state.focused_overlay.is_none()
            && state.active_modal.is_none()
            && state.picker_count == 0
            && state.term_focused
    }

    fn prompt_input_ready_base(&self, allow_pending_chord: bool) -> bool {
        let state = self.state();
        if !matches!(state.app_focus, AppFocus::Prompt)
            || state.agent_running
            || state.cmdline_open
            || state.focused_overlay.is_some()
            || state.active_modal.is_some()
            || state.picker_count > 0
            || !state.term_focused
        {
            return false;
        }
        let Some(win) = self.app.ui.win(crate::app::PROMPT_WIN) else {
            return false;
        };
        if win.vim_enabled() && !matches!(win.vim_mode(), VimMode::Insert) {
            return false;
        }
        if win.selection_anchor().is_some()
            || win.effective_endpoint() != win.cpos()
            || (!allow_pending_chord && self.app.timers.pending_chord.is_some())
            || self.app.timers.pending_pane_chord.is_some()
        {
            return false;
        }
        !self.app.ui.any_drag_active()
    }

    /// Whether plain text input should edit the prompt. Unlike
    /// `prompt_plain_insert_ready`, prediction placeholders are allowed: typing
    /// an ordinary printable key or paste while a placeholder is visible should
    /// still insert into the empty prompt; only placeholder accept/dismiss
    /// chords are special.
    pub fn prompt_text_input_ready(&self) -> bool {
        self.prompt_input_ready_base(false)
    }

    pub(crate) fn prompt_text_input_ready_for_turn_probe(&self) -> bool {
        // The turn-end probe intentionally preserves a stale Lua chord prefix,
        // such as the first Esc of the global Esc-Esc binding. The next
        // ordinary text key must decay that prefix and still edit the prompt,
        // so do not pre-filter those cases out of the probe.
        self.prompt_input_ready_base(true)
    }

    /// Whether a plain printable key should insert at the prompt cursor with
    /// no overlay/cmdline/selection/key-capture semantics in front of it.
    pub fn prompt_plain_insert_ready(&self) -> bool {
        self.prompt_input_ready_base(false)
            && !self
                .app
                .prompt
                .has_placeholder_options(crate::app::PROMPT_WIN)
    }

    pub fn prompt_plain_char_has_lua_keymap(&self, ch: char) -> bool {
        let chord = ch.to_string();
        let mode = self.app.current_vim_mode_label();
        self.app.lua.chord_has_binding(&chord, mode.as_deref())
    }

    pub fn model_history(&self) -> Vec<protocol::HistoryItem> {
        self.app.model_history()
    }

    pub fn assemble_system_prompt(&self) -> String {
        self.app.assemble_system_prompt()
    }

    pub(crate) fn discard_turn(&mut self, end: crate::app::TurnEnd) {
        self.app.discard_turn(end);
    }

    pub fn save_session_and_flush(&mut self) {
        // Harness callers immediately inspect or reopen the exact generation.
        // Use one completion barrier that tolerates a contended test runner
        // instead of proceeding after the production persistence deadline.
        self.app
            .save_session_and_flush_with_timeout(std::time::Duration::from_secs(120));
    }

    pub(crate) fn finalize_graceful_shutdown(&mut self) -> Result<(), String> {
        self.app.finalize_graceful_shutdown()
    }

    pub(crate) fn save_session(&mut self) {
        self.app.save_session();
    }

    pub(crate) fn set_transcript_session_dir_for_harness(
        &mut self,
        session_dir: std::path::PathBuf,
    ) {
        self.app
            .conversation
            .set_transcript_session_dir_for_harness(session_dir);
    }

    pub(crate) fn flush_persist(&mut self) -> crate::persist::PersistenceFlushOutcome {
        self.app.flush_persist()
    }

    pub(crate) fn session_document_has_unflushed_work(&self) -> bool {
        self.app.session_document_has_unflushed_work()
    }

    pub(crate) fn session_is_read_only(&self) -> bool {
        self.app.session_is_read_only()
    }

    pub(crate) fn load_session(&mut self, session: smelt_core::session::Session) {
        self.app.load_session(session);
    }

    pub(crate) fn load_store_backed_session(
        &mut self,
        document: crate::app::session_document::StoreBackedSessionDocument,
    ) {
        self.app.load_store_backed_session(document);
    }

    pub(crate) fn load_session_by_id(&mut self, id: &str) -> bool {
        self.resume_session(id)
    }

    pub(crate) fn fork_session(&mut self) {
        self.app.fork_session();
    }

    pub(crate) fn reset_session(&mut self) {
        self.app.reset_session();
    }

    pub(crate) fn restore_screen(&mut self) {
        self.app.restore_screen();
    }

    pub(crate) fn set_settings_for_harness(
        &mut self,
        settings: smelt_core::config::ResolvedSettings,
    ) {
        self.app.set_settings_for_harness(settings);
    }

    pub(crate) fn set_startup_setting_override_for_harness(
        &mut self,
        key: String,
        value: smelt_core::config::SettingValue,
    ) {
        self.app.core.startup_overrides.settings.insert(key, value);
    }

    pub(crate) fn commit_request_history_item(
        &mut self,
        item: protocol::HistoryItem,
        block: Option<smelt_core::transcript_model::Block>,
    ) -> protocol::ModelHistorySource {
        self.app.commit_request_history_item(item, block)
    }

    pub(crate) fn history_note_to_block(
        &self,
        note: &protocol::HistoryNote,
    ) -> Option<smelt_core::transcript_model::Block> {
        crate::app::history::history_note_to_block(&self.app.lua, note)
    }

    pub(crate) fn session_append_history(&mut self, item: protocol::HistoryItem) {
        self.app.session_append_history(item);
    }

    pub fn handle_job_completed(
        &mut self,
        id: String,
        exit_code: Option<i32>,
        termination: protocol::JobTermination,
    ) {
        self.app
            .handle_job_completed(smelt_core::process::JobCompletion {
                id,
                exit_code,
                termination,
            });
    }

    pub(crate) fn record_visible_token_usage(&mut self, usage: protocol::TokenUsage) {
        self.app.record_visible_token_usage(usage);
    }

    pub(crate) fn active_permissions(
        &self,
    ) -> std::sync::Arc<smelt_core::permissions::Permissions> {
        self.app.active_permissions()
    }

    pub(crate) fn session_path_grants(&self) -> Vec<smelt_core::permissions::SessionPathGrant> {
        self.app.session_path_grants()
    }

    pub fn focus_transcript(&mut self) {
        self.app.app_focus = AppFocus::Content;
        self.app.ui.set_focus(crate::app::TRANSCRIPT_WIN);
    }

    pub(crate) fn focus_prompt(&mut self) {
        self.app.app_focus = AppFocus::Prompt;
        self.app.ui.set_focus(crate::app::PROMPT_WIN);
    }

    pub(crate) fn active_docked_dialog(&self) -> Option<crate::smelt_edit::ContainerId> {
        self.app.active_docked_dialog()
    }

    pub(crate) fn notification_win(&self) -> Option<WinId> {
        self.app.overlays.notification_win()
    }

    pub(crate) fn notify_error(&mut self, message: String) {
        self.app.notify_error(message);
    }

    pub(crate) fn notify_application_error_sticky(&mut self, message: String) {
        self.app.notify_application_error_sticky(message);
    }

    pub(crate) fn notify_workspace_error_sticky(&mut self, message: String) {
        self.app.notify_workspace_error_sticky(message);
    }

    pub(crate) fn notify_session_error_sticky(&mut self, message: String) {
        self.app.notify_session_error_sticky(message);
    }

    pub(crate) fn dismiss_expired_notification(&mut self) -> bool {
        self.app.dismiss_expired_notification()
    }

    pub(crate) fn resolve_open_confirm_for_current_mode(&mut self, handle_id: u64) -> bool {
        self.app.resolve_open_confirm_for_current_mode(handle_id)
    }

    pub(crate) fn mode_pending(&self) -> bool {
        self.app.mode_pending()
    }

    pub(crate) fn sync_agent_mode_applied(&mut self) {
        self.app.sync_agent_mode_applied();
    }

    pub(crate) fn public_status_state_reason(
        &self,
    ) -> (
        smelt_core::public_status::PublicState,
        Option<smelt_core::public_status::PublicReason>,
    ) {
        self.app.public_status_state_reason()
    }

    pub(crate) fn set_terminal_focus_for_harness(&mut self, focused: bool) {
        self.app.set_terminal_focus_for_harness(focused);
    }

    pub(crate) fn apply_model(&mut self, key: &str, record: bool) {
        self.app.apply_model(key, record);
    }

    pub(crate) fn fast_mode(&self) -> bool {
        self.app.fast_mode()
    }

    pub(crate) fn set_fast_mode(&mut self, enabled: bool) {
        self.app.set_fast_mode(enabled);
    }

    pub(crate) fn warn_if_api_base_normalized(&mut self) {
        self.app.warn_if_api_base_normalized();
    }

    pub(crate) fn handle_managed_auth_checked(
        &mut self,
        snapshots: Vec<(
            engine::auth::AuthProvider,
            Option<u64>,
            Vec<protocol::ModelMetadata>,
        )>,
    ) {
        self.app.handle_managed_auth_checked(snapshots);
    }

    #[cfg(test)]
    pub(crate) fn install_http_client(&mut self, client: engine::HttpClient) {
        self.app.install_http_client(client);
    }

    #[cfg(test)]
    pub(crate) fn refresh_context_window_twice_for_harness(&mut self, api_key_env: &str) {
        self.app
            .core
            .config
            .active_model_mut()
            .expect("test app has an active model")
            .api_key_env = api_key_env.into();
        self.app
            .platform
            .enable_context_window_refresh_for_harness();
        self.app.refresh_context_window();
        self.app.refresh_context_window();
    }

    pub(crate) fn active_context_token_identity(
        &self,
    ) -> smelt_core::session::ContextTokenIdentity {
        self.app.active_context_token_identity()
    }

    pub(crate) fn rewind_to_history(
        &mut self,
        history_idx: usize,
    ) -> Option<(String, Vec<(String, String)>)> {
        self.app.rewind_to_history(history_idx)
    }

    pub(crate) fn rewind_to_start(&mut self) {
        self.app.rewind_to_start();
    }

    pub(crate) fn publish_shared_session_state(&self) {
        self.app.publish_shared_session_state();
    }

    pub(crate) fn publish_history_delta(&mut self, kind: crate::app::history::HistoryDeltaKind) {
        self.app.publish_history_delta(kind);
    }

    pub(crate) fn bump_epoch(&mut self, name: &str) {
        self.app.bump_epoch(name);
    }

    pub(crate) fn publish_input_submit(&mut self, text: &str) {
        self.app
            .core
            .signals
            .emit_dyn("input_submit", std::rc::Rc::new(text.to_string()));
        self.app.pump_lua();
    }

    pub(crate) fn configure_active_model_fast_mode(
        &mut self,
        provider_type: &str,
        supported: bool,
    ) {
        let model = self
            .app
            .core
            .config
            .active_model_mut()
            .expect("test app has an active model");
        model.provider_type = provider_type.to_string();
        model.config.supports_fast_mode = Some(supported);
    }

    pub(crate) fn set_active_model_fast_mode_support(&mut self, supported: bool) {
        self.app
            .core
            .config
            .active_model_mut()
            .expect("test app has an active model")
            .config
            .supports_fast_mode = Some(supported);
    }

    pub(crate) fn model_history_messages(&self) -> Vec<protocol::Message> {
        self.app.model_history_messages()
    }

    pub(crate) fn start_next_queued_input_if_idle(&mut self) -> bool {
        self.app.start_next_queued_input_if_idle()
    }

    pub(crate) fn placeholder_text(&mut self, win: WinId) -> Option<String> {
        self.app.placeholder_text(win)
    }

    pub(crate) fn clear_transcript(&mut self) {
        self.app.clear_transcript();
    }

    pub(crate) fn shutdown_context(&self) -> crate::app::ShutdownContext {
        self.app.conversation.shutdown_context()
    }

    pub(crate) fn grant_session_path(
        &mut self,
        mode: Option<protocol::AgentMode>,
        tool: String,
        access: smelt_core::permissions::PathAccess,
        dir: std::path::PathBuf,
    ) {
        self.app.grant_session_path(mode, tool, access, dir);
    }

    pub(crate) fn set_session_title(
        &mut self,
        title: String,
        slug: String,
        target_history_len: Option<usize>,
    ) {
        self.app.set_session_title(title, slug, target_history_len);
    }

    pub(crate) fn notify_session_save_failure(&mut self, session_id: &str, message: &str) {
        self.app.notify_session_save_failure(session_id, message);
    }

    pub(crate) fn dismiss_session_save_failure_notification(&mut self, session_id: &str) {
        self.app
            .dismiss_session_save_failure_notification(session_id);
    }

    pub(crate) fn session_history_range(
        &self,
        range: std::ops::Range<usize>,
    ) -> Vec<protocol::HistoryItem> {
        self.app
            .session_history_range(range)
            .expect("read session history range")
    }

    pub(crate) fn apply_history_append_to_history(
        &mut self,
        append: &protocol::HistoryAppend,
    ) -> protocol::HistoryAppendResult {
        self.app.apply_history_append_to_history(append)
    }

    pub(crate) fn rewind_history_for_harness(
        &mut self,
        index: usize,
        truncate: bool,
        identity: smelt_core::session::ContextTokenIdentity,
    ) {
        self.app
            .conversation
            .rewind_history(index, truncate, identity);
    }

    pub(crate) fn push_cmdline_history(
        &mut self,
        kind: crate::app::cmdline_history::CommandHistoryKind,
        command: String,
    ) {
        self.app.overlays.push_cmdline_history(kind, command);
    }

    pub(crate) fn session_set_checkpoint(
        &mut self,
        checkpoint: Option<smelt_core::ContextCheckpoint>,
    ) {
        self.app.session_set_checkpoint(checkpoint);
    }

    pub(crate) fn rewind_to_history_index(
        &mut self,
        history_idx: Option<usize>,
        restore_vim_insert: bool,
    ) {
        self.app
            .rewind_to_history_index(history_idx, restore_vim_insert);
    }

    pub(crate) fn set_prompt_placeholder_text(&mut self, text: String) {
        self.app.set_placeholder(crate::app::PROMPT_WIN, text);
    }

    pub(crate) fn managed_model_status(
        &self,
        provider: engine::auth::AuthProvider,
    ) -> smelt_core::ManagedModelsStatus {
        self.app.managed_model_catalog().provider(provider).status
    }

    pub(crate) fn begin_managed_model_refreshes(&mut self) -> Vec<smelt_core::RefreshToken> {
        self.app.begin_managed_model_refreshes()
    }

    #[cfg(test)]
    pub(crate) fn handle_managed_model_refresh(
        &mut self,
        token: smelt_core::RefreshToken,
        outcome: engine::auth::ManagedModelsRefreshOutcome,
    ) {
        self.app.handle_managed_models_refresh(token, outcome);
    }

    #[cfg(test)]
    pub(crate) fn activate_managed_model_retry_for_harness(
        &mut self,
        token: smelt_core::RefreshToken,
    ) -> bool {
        self.app.activate_managed_model_retry_for_harness(token)
    }

    #[cfg(test)]
    pub(crate) fn sync_managed_models_for_harness(
        &mut self,
        config: &smelt_core::config::Config,
        revision: u64,
    ) -> bool {
        self.app.sync_managed_models_for_harness(config, revision)
    }

    pub(crate) fn handle_app_event(&mut self, event: crate::app::AppEvent) {
        self.app.handle_app_event(event);
    }

    #[cfg(test)]
    pub(crate) fn try_recv_app_event(&mut self) -> Option<crate::app::AppEvent> {
        self.app.platform.try_recv_app_event()
    }

    #[cfg(test)]
    pub(crate) fn set_context_token_baseline_for_harness(&mut self, tokens: Option<u32>) {
        if let Some(tokens) = tokens {
            let identity = self.app.active_context_token_identity();
            self.app
                .conversation
                .record_context_tokens_for_harness(tokens, identity);
        } else {
            self.app
                .conversation
                .set_context_token_baseline_for_harness(None);
        }
    }

    pub(crate) fn replace_history_for_harness(&mut self, history: Vec<protocol::HistoryItem>) {
        self.app.conversation.replace_history_for_harness(history);
    }

    #[cfg(test)]
    pub(crate) fn set_session_id_for_harness(&mut self, id: String) {
        self.app.conversation.set_session_id_for_harness(id);
    }

    #[cfg(test)]
    pub(crate) fn install_live_session_for_harness(
        &mut self,
        live_session: smelt_core::session_runtime::LiveSession,
    ) {
        self.app
            .conversation
            .install_live_session_for_harness(live_session);
    }

    #[cfg(test)]
    pub(crate) fn set_history_resave_from_for_harness(&mut self, history_index: usize) {
        self.app
            .conversation
            .set_history_resave_from_for_harness(history_index);
    }

    #[cfg(test)]
    pub(crate) fn inject_commit_failure(&self, failure: smelt_store::SessionCommitFailure) {
        self.app.conversation.inject_commit_failure(failure);
    }

    pub(crate) fn set_applied_mode(&mut self, mode: protocol::AgentMode) {
        self.app.conversation.set_applied_mode(mode);
    }

    // Focused white-box probes are read-only and domain-scoped. Integration
    // actions stay on TestApp so tests cannot mutate the whole application.
    pub(crate) fn ui_probe(&self) -> &crate::smelt_edit::Ui {
        &self.app.ui
    }

    pub(crate) fn conversation_probe(&self) -> &crate::app::conversation::ConversationRuntime {
        &self.app.conversation
    }

    pub(crate) fn core_probe(&self) -> &smelt_core::Core {
        &self.app.core
    }

    pub(crate) fn lua_probe(&self) -> &crate::app::lua_handlers::LuaRuntimeController {
        &self.app.lua
    }

    pub(crate) fn overlays_probe(&self) -> &crate::app::overlay_runtime::OverlayRuntime {
        &self.app.overlays
    }

    pub(crate) fn workspace_probe(&self) -> &crate::app::cwd::WorkspaceState {
        &self.app.workspace
    }

    pub(crate) fn prompt_probe(&self) -> &crate::app::prompt_runtime::PromptRuntime {
        &self.app.prompt
    }

    pub(crate) fn working_probe(&self) -> &smelt_core::working::WorkingState {
        &self.app.working
    }

    pub(crate) fn timers_probe(&self) -> &crate::app::Timers {
        &self.app.timers
    }

    pub(crate) fn well_known_probe(&self) -> &crate::app::WellKnown {
        &self.app.well_known
    }

    pub(crate) fn auto_reload_probe(&self) -> &crate::auto_reload::AutoReloadController {
        &self.app.auto_reload
    }

    pub(crate) fn paint_registry_probe(&self) -> &crate::lua::paint::PaintRegistry {
        &self.app.paint_registry
    }
}
