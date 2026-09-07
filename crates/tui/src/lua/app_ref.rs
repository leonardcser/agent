//! Scoped TUI host access for Lua bindings.
//!
//! TUI and Core capabilities use separate scoped slots. The Core slot contains a
//! zero-state bridge that enters the TUI slot only for a synchronous Core
//! callback, so no overlapping mutable frontend and Core borrows are installed.

use crate::app::{NotificationOperation, TuiApp};
use scoped_tls_hkt::scoped_thread_local;

pub(crate) struct SessionStatusSnapshot {
    pub(crate) active_model: Option<smelt_core::runtime_state::ActiveModel>,
    pub(crate) cost: f64,
    pub(crate) mode: String,
    pub(crate) mode_pending: bool,
    pub(crate) reasoning: String,
    pub(crate) reasoning_pending: bool,
    pub(crate) fast_supported: bool,
    pub(crate) fast_active: bool,
    pub(crate) context_state: &'static str,
    pub(crate) context_tokens: Option<u32>,
    pub(crate) context_window: Option<u32>,
    pub(crate) context_stale: bool,
}

pub(crate) struct SessionInfoSnapshot {
    pub(crate) id: String,
    pub(crate) artifact_dir: std::path::PathBuf,
    pub(crate) ephemeral: bool,
    pub(crate) title: Option<String>,
    pub(crate) slug: Option<String>,
    pub(crate) first_user_message: Option<String>,
    pub(crate) parent_id: Option<String>,
    pub(crate) created_at_ms: u64,
    pub(crate) updated_at_ms: u64,
    pub(crate) cwd: String,
    pub(crate) session_cwd: Option<String>,
    pub(crate) active_model: Option<smelt_core::runtime_state::ActiveModel>,
    pub(crate) mode: String,
    pub(crate) reasoning: String,
    pub(crate) context_tokens: Option<u32>,
    pub(crate) context_tokens_stale: bool,
    pub(crate) context_window: Option<u32>,
    pub(crate) cost: f64,
    pub(crate) history_count: usize,
    pub(crate) message_count: usize,
    pub(crate) message_count_approximate: bool,
    pub(crate) turn_count: usize,
    pub(crate) usage: protocol::TokenUsage,
    pub(crate) managed_worktree: bool,
    pub(crate) project: String,
    pub(crate) branch: String,
    pub(crate) worktree: String,
    pub(crate) worktree_path: String,
}

pub(crate) enum DeferredLuaOperation {
    WindowKeymap {
        window: crate::smelt_edit::WinId,
        key: crate::smelt_edit::KeyBind,
    },
    WindowEvent {
        window: crate::smelt_edit::WinId,
        event: crate::smelt_edit::WinEvent,
        callback_id: u64,
    },
    PaintEvent {
        paint: crate::smelt_edit::layout::PaintId,
        event: crate::smelt_edit::WinEvent,
        callback_id: u64,
    },
    ModalKeymap {
        modal: crate::smelt_edit::ModalId,
        key: crate::smelt_edit::KeyBind,
    },
    OverlayKeymap {
        overlay: crate::smelt_edit::OverlayId,
        key: crate::smelt_edit::KeyBind,
    },
}

thread_local! {
    static DEFERRED_LUA_OPERATIONS: std::cell::RefCell<Vec<DeferredLuaOperation>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

pub(crate) fn defer_registered_lua_operation(
    shared: &std::sync::Arc<crate::lua::LuaShared>,
    callback_id: u64,
    operation: DeferredLuaOperation,
) -> bool {
    let registered = shared
        .callbacks
        .lock()
        .is_ok_and(|callbacks| callbacks.contains_key(&callback_id));
    if registered {
        DEFERRED_LUA_OPERATIONS.with(|operations| operations.borrow_mut().push(operation));
    }
    registered
}

pub(crate) struct RuntimeLuaHost<'a> {
    app: &'a mut TuiApp,
}

impl RuntimeLuaHost<'_> {
    pub(crate) fn schedule_runtime_reconcile(&mut self) {
        self.app.schedule_runtime_reconcile();
    }

    pub(crate) fn active_provider_type(&self) -> Option<String> {
        self.app
            .core
            .config
            .active_model()
            .map(|model| model.provider_type.clone())
    }

    pub(crate) fn active_api_base(&self) -> Option<String> {
        self.app
            .core
            .config
            .active_model()
            .map(|model| model.api_base.clone())
    }

    pub(crate) fn active_api_key_env(&self) -> Option<String> {
        self.app
            .core
            .config
            .active_model()
            .map(|model| model.api_key_env.clone())
    }

    pub(crate) fn active_model_config(&self) -> Option<protocol::ModelConfig> {
        self.app
            .core
            .config
            .active_model()
            .map(|model| model.config.clone())
    }

    pub(crate) fn context_window(&self) -> Option<u32> {
        self.app.core.config.context_window
    }

    pub(crate) fn active_model(&self) -> Option<smelt_core::runtime_state::ActiveModel> {
        self.app.core.config.active_model().cloned()
    }

    pub(crate) fn model_status(&self) -> crate::app::ModelStatusSnapshot {
        self.app.model_status_snapshot()
    }

    pub(crate) fn apply_model_ref(&mut self, name: &str) -> Result<(), String> {
        let key =
            smelt_core::config::resolve_model_ref(&self.app.core.config.available_models, name)
                .map(|model| model.key.clone())
                .map_err(|error| error.to_string())?;
        self.app.apply_model(&key, true);
        Ok(())
    }

    pub(crate) fn runtime_status(&self) -> serde_json::Value {
        fn revision_status(
            desired_revision: u64,
            observed_revision: u64,
            error: Option<String>,
        ) -> serde_json::Value {
            let status = if error.is_some() {
                "degraded"
            } else if desired_revision == observed_revision {
                "ready"
            } else {
                "pending"
            };
            serde_json::json!({
                "desired_revision": desired_revision,
                "observed_revision": observed_revision,
                "status": status,
                "error": error,
            })
        }

        let controllers = self.app.runtime_controller_status();
        let model = self.app.model_status_snapshot();
        let failure = self.app.lua_reload_failure().map(|failure| {
            serde_json::json!({
                "phase": failure.location.phase,
                "path": failure
                    .location
                    .path
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned()),
            })
        });
        let managed_providers = model
            .providers
            .iter()
            .map(|provider| {
                (
                    provider.name.clone(),
                    serde_json::json!({
                        "authenticated": provider.authenticated,
                        "status": provider.status,
                        "request_id": provider.request_id,
                        "auth_revision": provider.auth_revision,
                        "desired_revision": provider.desired_revision,
                    }),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        let mcp = controllers.mcp.map_or_else(
            || serde_json::json!({ "status": "unavailable" }),
            |status| {
                revision_status(
                    status.desired_revision,
                    status.observed_revision,
                    status.error,
                )
            },
        );
        serde_json::json!({
            "lua_generation": self.app.core.lua_generation,
            "runtime_revision": self.app.core.config.revision,
            "reload": {
                "pending": self.app.lua_reload_pending(),
                "waiting_for_safe_point": self.app.lua_reload_pending()
                    && !self.app.can_reload_lua_now(),
                "failure": failure,
            },
            "model": {
                "current": model.current,
                "requested": model.requested,
                "availability": model.availability,
                "reason": model.reason,
            },
            "managed_providers": managed_providers,
            "controllers": {
                "mcp": mcp,
                "lsp": revision_status(
                    controllers.lsp.desired_revision,
                    controllers.lsp.observed_revision,
                    None,
                ),
                "watcher": revision_status(
                    controllers.auto_reload.desired_revision,
                    controllers.auto_reload.observed_revision,
                    controllers.auto_reload.error,
                ),
                "context_window": revision_status(
                    controllers.context_window.desired_revision,
                    controllers.context_window.observed_revision,
                    controllers.context_window.error,
                ),
            },
        })
    }

    pub(crate) fn setting_value(&self, key: &str) -> Option<smelt_core::config::SettingValue> {
        let decl = smelt_core::config::setting_decl(key)?;
        Some((decl.read)(&self.app.core.config.settings))
    }

    pub(crate) fn apply_theme(&mut self, spec: &crate::theme::ThemeSpec) -> Result<(), String> {
        let is_light = self.app.ui.theme().is_light();
        let theme = crate::theme::compile(spec, is_light)?;
        self.app.install_theme(theme);
        Ok(())
    }

    pub(crate) fn set_theme_group(&mut self, group: String, style: smelt_core::style::Style) {
        self.app.mutate_theme(|theme| theme.set(group, style));
    }

    pub(crate) fn theme_is_light(&self) -> bool {
        self.app.ui.theme().is_light()
    }

    pub(crate) fn theme_group(&self, group: &str) -> smelt_core::style::Style {
        self.app.ui.theme().get(group)
    }

    pub(crate) fn theme_snapshot(&self) -> Vec<(String, smelt_core::style::Style)> {
        let theme = self.app.ui.theme();
        let mut groups = Vec::with_capacity(theme.len());
        for (id, style) in theme.iter() {
            if let Some(name) = smelt_core::theme::name_of(id) {
                if !name.starts_with("__anon__/") {
                    groups.push((name, *style));
                }
            }
        }
        groups.sort_by(|(left, _), (right, _)| left.cmp(right));
        groups
    }

    pub(crate) fn record_notice(
        &mut self,
        kind: smelt_core::messages::MessageKind,
        source: String,
        message: String,
    ) {
        self.app.record_notice(kind, source, message);
    }

    pub(crate) fn notify_error(&mut self, message: String) {
        self.app.notify_error(message);
    }
}

pub(crate) struct ConversationLuaHost<'a> {
    app: &'a mut TuiApp,
}

impl ConversationLuaHost<'_> {
    pub(crate) fn replace_prompt_text(&mut self, text: String) {
        {
            let mut context = crate::input::prompt_ctx_mut(&mut self.app.ui);
            self.app.prompt.replace_text(&mut context, text);
        }
        let _ = self.app.queue_prompt_text_changed_if_dirty();
    }

    pub(crate) fn prompt_text(&self) -> String {
        self.app
            .prompt_buf()
            .source()
            .replace(smelt_buffer::attachment::ATTACHMENT_MARKER, "")
    }

    pub(crate) fn prompt_cursor(&mut self, position: Option<i64>) -> i64 {
        match position {
            Some(position) => {
                let requested = position.max(0) as usize;
                let context = crate::input::prompt_ctx_mut(&mut self.app.ui);
                let snapped = smelt_buffer::text::snap_grapheme(context.buf.source(), requested);
                context.win.set_cpos(snapped);
                context.win.clear_selection_anchor();
                context.win.clamp_anchors_to_source(context.buf.source());
                snapped as i64
            }
            None => self.app.prompt_win().cpos() as i64,
        }
    }

    pub(crate) fn replace_prompt_range(&mut self, start: i64, end: i64, text: &str) -> i64 {
        let cursor = {
            let context = crate::input::prompt_ctx_mut(&mut self.app.ui);
            let source = context.buf.source();
            let start = smelt_buffer::text::snap_grapheme(source, start.max(0) as usize);
            let end = smelt_buffer::text::snap_grapheme(source, end.max(0) as usize).max(start);
            context.buf.text_mut().replace_range(start..end, text);
            let cursor = start + text.len();
            let cursor = if text.is_empty() {
                smelt_buffer::text::snap_grapheme(context.buf.source(), cursor)
            } else {
                smelt_buffer::text::ceil_grapheme(context.buf.source(), cursor)
            };
            context.win.set_cpos(cursor);
            context.win.clear_selection_anchor();
            context.win.clamp_anchors_to_source(context.buf.source());
            cursor
        };
        let _ = self.app.queue_prompt_text_changed_if_dirty();
        cursor as i64
    }

    pub(crate) fn queued_prompt_texts(&self) -> Vec<String> {
        if self.app.prompt_input_is_busy() {
            self.app.prompt.queued_texts()
        } else {
            Vec::new()
        }
    }

    pub(crate) fn queued_prompt_rows(&self) -> Vec<(String, &'static str)> {
        if self.app.prompt_input_is_busy() {
            self.app
                .prompt
                .queued_rows()
                .into_iter()
                .map(|row| (row.text, row.stage.as_str()))
                .collect()
        } else {
            Vec::new()
        }
    }

    pub(crate) fn prompt_has_stash(&self) -> bool {
        self.app.prompt.has_stash()
    }

    pub(crate) fn transcript_navigation_block(
        &self,
        session_id: &str,
        navigation_generation: u64,
        anchor: crate::app::transcript::TranscriptSemanticAnchor,
        role: Option<&str>,
        previous: bool,
    ) -> Option<crate::app::transcript::TranscriptNavigationBlock> {
        if self.app.conversation.session().id != session_id
            || self
                .app
                .conversation
                .transcript()
                .history()
                .navigation_generation()
                != navigation_generation
        {
            return None;
        }
        if previous {
            self.app
                .conversation
                .transcript()
                .previous_navigation_block_from(anchor, role)
        } else {
            self.app
                .conversation
                .transcript()
                .next_navigation_block_from(anchor, role)
        }
    }

    pub(crate) fn render_transcript_stream(
        &mut self,
        buffer_id: crate::smelt_edit::BufId,
        width: Option<u16>,
        projection: &mut crate::content::transcript_buf::TranscriptProjection,
        history: &mut smelt_core::transcript_model::BlockHistory,
    ) {
        let target_width = self
            .app
            .ui
            .iter_wins()
            .filter(|(_, window)| window.buf == buffer_id)
            .filter_map(|(window_id, _)| self.app.ui.win_content_width(window_id))
            .max();
        let width = width
            .or(target_width)
            .unwrap_or_else(|| crate::content::term_width().saturating_sub(2).max(1) as u16)
            .max(1);
        let theme = self.app.ui.theme().clone();
        let Some(buffer) = self.app.ui.buf_mut(buffer_id) else {
            return;
        };
        projection.project_all(&self.app.lua, buffer, history, width, &theme);
    }

    pub(crate) fn set_compaction_preview(&mut self, summary: Option<String>) {
        if let Some(summary) = summary {
            self.app.update_compaction_preview(summary);
        } else {
            self.app.clear_compaction_preview();
        }
    }

    pub(crate) fn loaded_transcript_text(&mut self) -> String {
        self.app
            .materialize_loaded_transcript_display_rows_expensive()
            .join("\n")
    }

    pub(crate) fn request_loaded_transcript_text(&mut self, callback: mlua::Function) {
        self.app.request_transcript_detail(
            crate::app::transcript_detail::TranscriptDetailKind::LoadedText,
            callback,
        );
    }

    pub(crate) fn request_loaded_transcript_blocks(&mut self, callback: mlua::Function) {
        self.app.request_transcript_detail(
            crate::app::transcript_detail::TranscriptDetailKind::LoadedBlocks,
            callback,
        );
    }

    pub(crate) fn transcript_is_empty(&self) -> bool {
        self.app.conversation.transcript().is_empty()
    }

    pub(crate) fn loaded_transcript_blocks(
        &mut self,
    ) -> Vec<crate::app::transcript::TranscriptBlockSnapshot> {
        self.app.loaded_transcript_block_snapshots()
    }

    pub(crate) fn visible_transcript_blocks(
        &self,
    ) -> Vec<crate::app::transcript::TranscriptBlockSnapshot> {
        self.app.visible_transcript_block_snapshots()
    }

    pub(crate) fn transcript_rows(
        &mut self,
        start: crate::smelt_edit::RowIndex,
        count: crate::smelt_edit::RowIndex,
    ) -> Vec<String> {
        self.app.transcript_visible_rows(start, count)
    }

    pub(crate) fn loaded_transcript_block_at_row(
        &mut self,
        row: crate::smelt_edit::RowIndex,
    ) -> Option<crate::app::transcript::TranscriptBlockSnapshot> {
        self.app.loaded_transcript_block_at_row(row)
    }

    pub(crate) fn committed_transcript_view(&self) -> Option<crate::app::CommittedTranscriptView> {
        self.app.conversation.committed_transcript_view()
    }

    pub(crate) fn reveal_transcript_target_at_top(
        &mut self,
        session_id: &str,
        record_index: usize,
        block_id: smelt_core::transcript_model::BlockId,
        top_padding: crate::smelt_edit::RowIndex,
        move_cursor: bool,
    ) -> bool {
        self.app.conversation.session().id == session_id
            && self.app.reveal_transcript_target_at_top(
                record_index,
                block_id,
                top_padding,
                move_cursor,
            )
    }

    pub(crate) fn transcript_node_at_row(
        &mut self,
        row: crate::smelt_edit::RowIndex,
    ) -> Option<crate::content::transcript_buf::TranscriptNodeRow> {
        self.app.transcript_node_at_row(row)
    }

    pub(crate) fn fold_transcript_node_at_row(
        &mut self,
        row: crate::smelt_edit::RowIndex,
        action: crate::content::transcript_buf::FoldAction,
        activation: crate::content::transcript_buf::FoldActivation,
    ) -> bool {
        self.app
            .fold_transcript_node_at_row(row, action, activation)
    }

    pub(crate) fn fold_transcript_node(
        &mut self,
        id: crate::content::transcript_scene::RenderNodeId,
        action: crate::content::transcript_buf::FoldAction,
    ) -> bool {
        self.app.fold_transcript_node(id, action)
    }

    pub(crate) fn fold_all_transcript_nodes(
        &mut self,
        action: crate::content::transcript_buf::FoldAction,
    ) -> bool {
        self.app.fold_all_transcript_nodes(action)
    }

    pub(crate) fn fold_transcript_block_kind(
        &mut self,
        kind: &str,
        action: crate::content::transcript_buf::FoldAction,
    ) -> bool {
        self.app.fold_transcript_block_kind(kind, action)
    }

    pub(crate) fn prompt_history(&self) -> Vec<String> {
        self.app
            .prompt
            .history_entries()
            .map(String::from)
            .collect()
    }
}

pub(crate) struct AgentLuaHost<'a> {
    app: &'a mut TuiApp,
}

impl AgentLuaHost<'_> {
    pub(crate) fn confirm_exists(&self, handle_id: u64) -> bool {
        self.app.core.confirms.get(handle_id).is_some()
    }

    pub(crate) fn resolve_open_confirm_for_current_mode(&mut self, handle_id: u64) -> bool {
        self.app.resolve_open_confirm_for_current_mode(handle_id)
    }

    pub(crate) fn confirm_preview_request(
        &self,
        handle_id: u64,
    ) -> Option<(String, std::collections::HashMap<String, serde_json::Value>)> {
        self.app
            .core
            .confirms
            .get(handle_id)
            .map(|entry| (entry.req.tool_name.clone(), entry.req.args.clone()))
    }

    pub(crate) fn resolve_confirm(
        &mut self,
        handle_id: u64,
        decision: &str,
        message: Option<String>,
    ) {
        let Some(entry) = self.app.core.confirms.take(handle_id) else {
            return;
        };
        let choice = crate::lua::api::confirm::parse_decision(decision, &entry.req);
        self.app.core.signals.emit_dyn(
            "confirm_resolved",
            std::rc::Rc::new(smelt_core::signals::ConfirmResolved {
                handle_id,
                decision: crate::lua::api::confirm::decision_label(&choice),
            }),
        );
        let invocation_id = entry.req.invocation_id;
        let request_id = entry.req.request_id;
        let tool_name = entry.req.tool_name.clone();
        self.app
            .handle_confirm_resolve(choice, message, invocation_id, request_id, &tool_name);
    }

    pub(crate) fn cancel_engine_work(&mut self) {
        if self.app.prompt.queue_is_empty() {
            self.app.discard_turn(crate::app::TurnEnd::Cancelled);
        } else {
            self.app.drain_queued_inputs_into_prompt();
        }
    }

    pub(crate) fn agent_is_running(&self) -> bool {
        self.app.agent_is_running()
    }

    pub(crate) fn turn_lifecycle_is_active(&self) -> bool {
        self.app.turn_lifecycle_is_active()
    }

    pub(crate) fn reload_lua_now(&mut self) {
        if self.app.prompt_input_is_busy() {
            self.app
                .notify_error("cannot reload while agent is working".into());
            return;
        }
        while self.app.close_active_modal() {}
        self.app.schedule_lua_reload();
    }

    pub(crate) fn schedule_lua_reload(&mut self) -> bool {
        self.app.schedule_lua_reload()
    }

    pub(crate) fn submit_custom_command(
        &mut self,
        command: smelt_core::custom_commands::CustomCommand,
    ) {
        if self.app.prompt_input_is_busy() {
            let text = if self.app.core.config.settings.redact_secrets {
                engine::redact::redact(&command.body)
            } else {
                command.body.clone()
            };
            let display = if self.app.core.config.settings.redact_secrets {
                engine::redact::redact(&format!("/{}", command.display))
            } else {
                format!("/{}", command.display)
            };
            let queued =
                crate::app::QueuedInput::custom_command_request(display, text, command.overrides);
            let target = smelt_core::lua::current_command_queue_target()
                .map(crate::app::QueueStage::from_command_target)
                .unwrap_or(crate::app::QueueStage::Turn);
            match target {
                crate::app::QueueStage::Turn => {
                    self.app.prompt.try_queue_turn(queued);
                }
                crate::app::QueueStage::Request => {
                    self.app.queue_input_for_request(queued);
                }
            }
            return;
        }
        let turn = self.app.begin_custom_command_turn(command);
        self.app.conversation.set_active(turn);
    }

    pub(crate) fn submit_custom_command_continuation(
        &mut self,
        command: smelt_core::custom_commands::CustomCommand,
        continuation_token: u64,
    ) -> bool {
        if self.app.prompt_input_is_busy()
            || !self.app.consume_continuation_token(continuation_token)
        {
            return false;
        }
        let turn = self.app.begin_custom_command_continuation(command);
        let started = turn.is_some() || self.app.turn_submission_is_pending();
        self.app.conversation.set_active(turn);
        started
    }

    fn resolve_ask_reasoning_effort(
        catalog: &protocol::ModelCatalogMetadata,
        requested: Option<protocol::ReasoningEffort>,
        model: &str,
    ) -> Result<protocol::ReasoningEffort, String> {
        match requested {
            Some(effort) if catalog.supports_reasoning_effort(&effort) => Ok(effort),
            Some(effort) => Err(format!(
                "reasoning effort '{}' is not supported by model '{}'",
                effort.label(),
                model
            )),
            None => Ok(catalog.reconcile_reasoning_effort(protocol::ReasoningEffort::Off)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn dispatch_engine_ask(
        &mut self,
        id: u64,
        system: String,
        mut messages: Vec<protocol::Message>,
        model_reference: Option<String>,
        question: Option<String>,
        response_format: Option<protocol::AskResponseFormat>,
        reasoning_effort: Option<protocol::ReasoningEffort>,
        stream: bool,
        visible_retries: bool,
    ) -> Result<(), String> {
        if let Some(question) = question {
            messages.push(protocol::Message::user(protocol::Content::text(&question)));
        }
        let Some((target, catalog)) = self.resolve_ask_target(model_reference.as_deref()) else {
            return Err("no usable model is available".into());
        };
        let reasoning_effort =
            Self::resolve_ask_reasoning_effort(&catalog, reasoning_effort, &target.model)?;
        let request_config = self.app.core.config.request_runtime_config();
        let session_id = self.app.conversation.session().id.clone();
        let persistence = self.app.conversation.persistence_scope();
        self.app.core.engine.send(protocol::UiCommand::EngineAsk {
            id,
            system,
            messages,
            target: Box::new(target),
            request_config,
            response_format,
            reasoning_effort,
            fast_mode: false,
            tools: Vec::new(),
            session_id,
            persistence,
            stream,
            visible_retries,
        });
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn dispatch_inherited_engine_ask(
        &mut self,
        id: u64,
        mut messages: Vec<protocol::Message>,
        model_reference: Option<String>,
        question: Option<String>,
        response_format: Option<protocol::AskResponseFormat>,
        reasoning_effort: Option<protocol::ReasoningEffort>,
        stream: bool,
        visible_retries: bool,
    ) -> Result<(), String> {
        let system = self.app.assemble_system_prompt();
        if messages.is_empty() {
            messages = self.app.model_history_messages();
        }
        if let Some(question) = question {
            messages.push(protocol::Message::user(protocol::Content::text(&question)));
        }
        let Some((target, catalog)) = self.resolve_ask_target(model_reference.as_deref()) else {
            return Err("no usable model is available".into());
        };
        let reasoning_effort =
            Self::resolve_ask_reasoning_effort(&catalog, reasoning_effort, &target.model)?;
        let request_config = self.app.core.config.request_runtime_config();
        let session_id = self.app.conversation.session().id.clone();
        let persistence = self.app.conversation.persistence_scope();
        let tools = self.app.lua.tool_defs(
            self.app.core.config.mode.clone(),
            smelt_core::lua::ToolVisibility::Interactive,
        );
        self.app.core.engine.send(protocol::UiCommand::EngineAsk {
            id,
            system,
            messages,
            target: Box::new(target),
            request_config,
            response_format,
            reasoning_effort,
            fast_mode: self.app.fast_mode(),
            tools,
            session_id,
            persistence,
            stream,
            visible_retries,
        });
        Ok(())
    }

    fn resolve_ask_target(
        &mut self,
        model_reference: Option<&str>,
    ) -> Option<(protocol::ModelTarget, protocol::ModelCatalogMetadata)> {
        let Some(reference) = model_reference else {
            let catalog = self
                .app
                .core
                .config
                .active_model()
                .map(|model| model.reasoning_catalog())
                .unwrap_or_default();
            return self
                .app
                .resolve_model_target()
                .map(|target| (target, catalog));
        };
        let resolved = match smelt_core::config::resolve_model_ref(
            &self.app.core.config.available_models,
            reference,
        ) {
            Ok(model) => model.clone(),
            Err(error) => {
                self.app.notify_operation_error_sticky(
                    NotificationOperation::TurnStart,
                    format!("smelt.engine: {error}"),
                );
                return None;
            }
        };
        let api_key = self.app.resolve_api_key_for_env(&resolved.api_key_env)?;
        let catalog = resolved.reasoning_catalog();
        Some((resolved.target(api_key), catalog))
    }

    pub(crate) fn busy_registration(&mut self, label: String) -> smelt_core::lua::reg::LuaReg {
        let token = self.app.busy_stack.push_token(label);
        smelt_core::lua::reg::LuaReg::new(move || token.release())
    }

    pub(crate) fn context_recalculation_registration(
        &mut self,
        label: String,
    ) -> crate::app::host_dispatch::ContextRecalculation {
        self.app.begin_context_recalculation(label)
    }

    pub(crate) fn is_busy(&self) -> bool {
        self.app.busy_stack.is_busy()
    }

    pub(crate) fn work_guard(&self) -> (Option<u64>, u64) {
        (
            self.app.active_agent_turn_id(),
            self.app.conversation.cancel_generation(),
        )
    }

    pub(crate) fn work_guard_is_current(&self, turn_id: Option<u64>, generation: u64) -> bool {
        self.app.conversation.cancel_generation() == generation
            && self.app.active_agent_turn_id() == turn_id
    }

    pub(crate) fn focused_vim_mode_label(&self) -> Option<String> {
        self.app.focused_vim_mode_label()
    }

    pub(crate) fn focused_vim_mode(&self) -> Option<crate::smelt_edit::VimMode> {
        self.app.focused_vim_mode()
    }

    pub(crate) fn set_focused_vim_mode(&mut self, mode: crate::smelt_edit::VimMode) {
        self.app.set_focused_vim_mode(mode);
    }

    pub(crate) fn prompt_vim_enabled(&self) -> bool {
        self.app.prompt.vim_enabled(self.app.prompt_win())
    }

    pub(crate) fn set_mode(&mut self, mode: protocol::AgentMode) {
        self.app.set_mode(mode, true);
    }

    pub(crate) fn set_reasoning_effort(
        &mut self,
        effort: protocol::ReasoningEffort,
    ) -> Result<(), String> {
        self.app.set_reasoning_effort(effort, true)
    }

    pub(crate) fn focus_name(&self) -> &'static str {
        match self.app.app_focus {
            crate::app::AppFocus::Content => "transcript",
            crate::app::AppFocus::Prompt => "prompt",
        }
    }

    pub(crate) fn request_quit(&mut self) {
        self.app.pending_quit = true;
    }
}

pub(crate) struct PlatformLuaHost<'a> {
    app: &'a mut TuiApp,
}

impl PlatformLuaHost<'_> {
    pub(crate) fn write_terminal_control(&mut self, bytes: &[u8]) -> std::io::Result<bool> {
        self.app.write_terminal_control(bytes)
    }

    pub(crate) fn set_terminal_title(&mut self, bytes: &[u8]) -> std::io::Result<bool> {
        self.app.set_terminal_title(bytes)
    }

    pub(crate) fn clear_terminal_title(&mut self, bytes: &[u8]) -> std::io::Result<bool> {
        self.app.clear_terminal_title(bytes)
    }

    pub(crate) fn terminal_size(&self) -> std::io::Result<(u16, u16)> {
        self.app.platform_terminal_size()
    }

    pub(crate) fn terminal_is_focused(&self) -> bool {
        self.app.terminal_is_focused()
    }

    pub(crate) fn ui_size(&self) -> (u16, u16) {
        (self.app.last_width, self.app.last_height)
    }

    pub(crate) fn start_inspect_server(&mut self, task_id: u64) {
        let sink = self.app.lua.shared().resume_sink();
        self.app.start_inspect_server(task_id, sink);
    }

    pub(crate) fn stop_inspect_server(&mut self, task_id: u64) {
        let sink = self.app.lua.shared().resume_sink();
        self.app.stop_inspect_server(task_id, sink);
    }

    pub(crate) fn inspect_server_url(&self) -> Option<String> {
        self.app.inspect_server_url()
    }

    pub(crate) fn metrics_entries(&self) -> Vec<crate::metrics::MetricsEntry> {
        crate::metrics::load(self.app.core.sessions.state_root())
    }

    pub(crate) fn sync_permissions(
        &mut self,
        session_entries: Option<Vec<smelt_core::PermissionEntry>>,
        path_grants: Option<Vec<smelt_core::permissions::SessionPathGrant>>,
        workspace: Option<smelt_core::permissions::store::Replacement>,
        repository: Option<smelt_core::permissions::store::Replacement>,
    ) -> Result<(), String> {
        self.app
            .sync_permissions(session_entries, path_grants, workspace, repository)
    }

    pub(crate) fn revoke_permission(
        &mut self,
        scope: &str,
        tool: &str,
        pattern: &str,
    ) -> Result<bool, String> {
        self.app.revoke_permission(scope, tool, pattern)
    }

    pub(crate) fn grant_session_path(&mut self, grant: smelt_core::permissions::SessionPathGrant) {
        self.app
            .grant_session_path(grant.mode, grant.tool, grant.access, grant.dir);
    }
}

pub(crate) struct UiLuaHost<'a> {
    app: &'a mut TuiApp,
}

impl UiLuaHost<'_> {
    pub(crate) fn with_ui<R>(
        &mut self,
        callback: impl FnOnce(&mut crate::smelt_edit::Ui) -> R,
    ) -> R {
        callback(&mut self.app.ui)
    }

    pub(crate) fn close_decoration(&mut self, id: crate::smelt_edit::DecorationId) {
        self.app.close_decoration(id);
    }

    pub(crate) fn close_overlay_leaf(&mut self, id: crate::smelt_edit::WinId) {
        self.app.close_overlay_leaf(id);
    }

    pub(crate) fn focus_window(&mut self, id: crate::smelt_edit::WinId) {
        let focused = self.app.focus_window(id);
        if !focused
            && self.app.ui.active_modal().is_none()
            && self.app.lua.shared().layout_refresh_pending()
        {
            self.app.lua.shared().request_focus_after_layout(id);
        }
    }

    pub(crate) fn open_decoration(
        &mut self,
        owner: crate::smelt_edit::WinId,
        options: mlua::Table,
    ) -> Result<crate::smelt_edit::DecorationId, String> {
        crate::lua::ui_ops::open_decoration(self.app, owner, options)
    }

    pub(crate) fn configure_input_leaf(
        &mut self,
        window: crate::smelt_edit::WinId,
        placeholder: String,
    ) {
        crate::lua::ui_ops::configure_input_leaf(self.app, window, placeholder);
    }

    pub(crate) fn set_input_text(&mut self, window: crate::smelt_edit::WinId, text: &str) {
        let normalized = crate::line_input::normalize_single_line(text);
        let Some(buffer_id) = self.app.ui.win(window).map(|window| window.buf) else {
            return;
        };
        let placeholder = self.app.prompt.placeholder_text(window).map(str::to_string);
        if let Some(buffer) = self.app.ui.buf_mut(buffer_id) {
            buffer.set_lines(0, buffer.line_count(), vec![normalized.clone()]);
            if normalized.is_empty() {
                if let Some(placeholder) = placeholder {
                    crate::content::prompt_buf::set_placeholder_extmark(buffer, Some(placeholder));
                }
            } else {
                crate::content::prompt_buf::set_placeholder_extmark(buffer, Some(String::new()));
            }
        }
        if let Some(window) = self.app.ui.win_mut(window) {
            window.set_cursor_byte_single_line(&normalized, normalized.len());
            window.clear_selection_anchor();
        }
    }

    pub(crate) fn configure_list_leaf(
        &mut self,
        window: crate::smelt_edit::WinId,
        initial_cursor: u64,
    ) {
        crate::lua::ui_ops::configure_list_leaf(self.app, window, initial_cursor);
    }

    pub(crate) fn set_cursor_row(&mut self, window: crate::smelt_edit::WinId, row: u64) {
        crate::lua::ui_ops::set_cursor_row(self.app, window, row);
    }

    pub(crate) fn cursor_row(&mut self, window: crate::smelt_edit::WinId) -> Option<u64> {
        crate::lua::ui_ops::cursor_row(self.app, window)
    }

    pub(crate) fn move_cursor(&mut self, window: crate::smelt_edit::WinId, delta: isize) {
        crate::lua::ui_ops::move_cursor(self.app, window, delta);
    }

    pub(crate) fn reveal_position(
        &mut self,
        window: crate::smelt_edit::WinId,
        row: u64,
        top_padding: u64,
        bottom_padding: u64,
        cursor: bool,
    ) {
        let transcript_scroll_intent = (window == crate::app::TRANSCRIPT_WIN)
            .then_some(crate::app::reveal::RevealScrollIntent::Position);
        self.app.reveal_position(
            window,
            crate::smelt_edit::DocPosition { row, byte_col: 0 },
            crate::app::reveal::RevealOptions {
                top_padding,
                bottom_padding,
                cursor,
                transcript_scroll_intent,
            },
        );
    }

    pub(crate) fn set_window_keymap(
        &mut self,
        window: crate::smelt_edit::WinId,
        key: crate::smelt_edit::KeyBind,
        callback: crate::smelt_edit::Callback,
    ) {
        let previous = self.app.ui.win_set_keymap(window, key, callback);
        crate::lua::drop_displaced_lua_handle(self.app, previous);
    }

    pub(crate) fn clear_window_keymap(
        &mut self,
        window: crate::smelt_edit::WinId,
        key: crate::smelt_edit::KeyBind,
    ) -> bool {
        let previous = self.app.ui.win_clear_keymap(window, key);
        let removed = previous.is_some();
        crate::lua::drop_displaced_lua_handle(self.app, previous);
        removed
    }

    pub(crate) fn set_placeholder(
        &mut self,
        window: crate::smelt_edit::WinId,
        text: String,
        options: crate::app::PlaceholderOpts,
    ) {
        if text.is_empty() {
            self.app.clear_placeholder(window);
        } else {
            self.app.set_placeholder(window, text);
            self.app.set_placeholder_options(window, options);
        }
    }

    pub(crate) fn clear_placeholder(&mut self, window: crate::smelt_edit::WinId) {
        self.app.clear_placeholder(window);
    }

    pub(crate) fn placeholder_text(&mut self, window: crate::smelt_edit::WinId) -> Option<String> {
        self.app.placeholder_text(window)
    }

    pub(crate) fn register_window_event(
        &mut self,
        window: crate::smelt_edit::WinId,
        event: crate::smelt_edit::WinEvent,
        callback: crate::smelt_edit::Callback,
    ) {
        self.app.ui.win_on_event(window, event, callback);
    }

    pub(crate) fn clear_window_event(
        &mut self,
        window: crate::smelt_edit::WinId,
        event: crate::smelt_edit::WinEvent,
        callback_id: u64,
    ) -> bool {
        let previous = self
            .app
            .ui
            .win_clear_event_by_id(window, event, callback_id);
        let removed = previous.is_some();
        crate::lua::drop_displaced_lua_handle(self.app, previous);
        removed
    }

    pub(crate) fn register_paint_event(
        &mut self,
        paint: crate::smelt_edit::layout::PaintId,
        event: crate::smelt_edit::WinEvent,
        callback: crate::smelt_edit::Callback,
    ) {
        self.app.ui.leaf_on_event(paint, event, callback);
    }

    pub(crate) fn clear_paint_event(
        &mut self,
        paint: crate::smelt_edit::layout::PaintId,
        event: crate::smelt_edit::WinEvent,
        callback_id: u64,
    ) -> bool {
        let previous = self
            .app
            .ui
            .leaf_clear_event_by_id(paint, event, callback_id);
        let removed = previous.is_some();
        crate::lua::drop_displaced_lua_handle(self.app, previous);
        removed
    }

    pub(crate) fn remove_paint(&mut self, paint: crate::smelt_edit::layout::PaintId) -> bool {
        for callback_id in self.app.ui.leaf_clear_callbacks(paint) {
            self.app.lua.remove_callback(callback_id);
        }
        if let Some(callback_id) = self.app.paint_registry.unregister(paint) {
            self.app.lua.remove_callback(callback_id);
            true
        } else {
            false
        }
    }

    pub(crate) fn register_paint(
        &mut self,
        callback_id: u64,
        name: Option<String>,
    ) -> crate::smelt_edit::layout::PaintId {
        let (paint_id, previous) = self.app.paint_registry.register(callback_id, name);
        for stale in self.app.ui.leaf_clear_callbacks(paint_id) {
            self.app.lua.remove_callback(stale);
        }
        if let Some(previous) = previous {
            self.app.lua.remove_callback(previous);
        }
        paint_id
    }

    pub(crate) fn close_docked_dialog(&mut self, dialog: crate::smelt_edit::ContainerId) {
        self.app.close_docked_dialog(dialog);
    }

    pub(crate) fn toggle_docked_dialog_expanded(&mut self, dialog: crate::smelt_edit::ContainerId) {
        self.app.toggle_docked_dialog_expanded(dialog);
    }

    pub(crate) fn set_modal_keymap(
        &mut self,
        modal: crate::smelt_edit::ModalId,
        key: crate::smelt_edit::KeyBind,
        callback: crate::smelt_edit::Callback,
    ) {
        let previous = self.app.ui.modal_set_keymap(modal, key, callback);
        crate::lua::drop_displaced_lua_handle(self.app, previous);
    }

    pub(crate) fn clear_modal_keymap(
        &mut self,
        modal: crate::smelt_edit::ModalId,
        key: crate::smelt_edit::KeyBind,
    ) -> bool {
        let previous = self.app.ui.modal_clear_keymap(modal, key);
        let removed = previous.is_some();
        crate::lua::drop_displaced_lua_handle(self.app, previous);
        removed
    }

    pub(crate) fn open_docked_dialog(
        &mut self,
        layout: crate::lua::api::overlay_layout::LayoutNode,
        height: crate::smelt_edit::Constraint,
        min_height: Option<crate::smelt_edit::Constraint>,
        max_height: Option<crate::smelt_edit::Constraint>,
        blocks_agent: bool,
        resizable: bool,
    ) -> Result<(crate::smelt_edit::ContainerId, crate::smelt_edit::ModalId), String> {
        self.app.open_docked_dialog(
            layout,
            height,
            min_height,
            max_height,
            blocks_agent,
            resizable,
        )
    }

    pub(crate) fn close_overlay(&mut self, overlay: crate::smelt_edit::OverlayId) {
        self.app.close_overlay(overlay);
    }

    pub(crate) fn set_overlay_keymap(
        &mut self,
        overlay: crate::smelt_edit::OverlayId,
        key: crate::smelt_edit::KeyBind,
        callback: crate::smelt_edit::Callback,
    ) {
        let previous = self.app.ui.overlay_set_keymap(overlay, key, callback);
        crate::lua::drop_displaced_lua_handle(self.app, previous);
    }

    pub(crate) fn clear_overlay_keymap(
        &mut self,
        overlay: crate::smelt_edit::OverlayId,
        key: crate::smelt_edit::KeyBind,
    ) -> bool {
        let previous = self.app.ui.overlay_clear_keymap(overlay, key);
        let removed = previous.is_some();
        crate::lua::drop_displaced_lua_handle(self.app, previous);
        removed
    }

    pub(crate) fn open_overlay(&mut self, options: mlua::Table) -> Result<u64, String> {
        crate::lua::ui_ops::open_overlay(self.app, options)
    }

    pub(crate) fn clear_overlay_callbacks(&mut self, overlay: crate::smelt_edit::OverlayId) {
        for callback_id in self.app.ui.overlay_clear_callbacks(overlay) {
            self.app.lua.remove_callback(callback_id);
        }
    }

    pub(crate) fn open_picker(
        &mut self,
        options: mlua::Table,
    ) -> Result<crate::smelt_edit::WinId, String> {
        crate::lua::ui_ops::open_picker(self.app, options)
    }

    pub(crate) fn set_picker_items(
        &mut self,
        window: crate::smelt_edit::WinId,
        values: &mlua::Table,
        selected: usize,
    ) -> Result<(), String> {
        let mut items = Vec::new();
        for value in values.clone().sequence_values::<mlua::Value>() {
            items.push(crate::lua::ui_ops::parse_picker_item(
                self.app,
                &value.map_err(|error| error.to_string())?,
            )?);
        }
        crate::picker::set_items(self.app, window, items, selected);
        Ok(())
    }

    pub(crate) fn set_picker_selected(
        &mut self,
        window: crate::smelt_edit::WinId,
        selected: usize,
    ) {
        crate::picker::set_selected(self.app, window, selected);
    }

    pub(crate) fn picker_selected(&mut self, window: crate::smelt_edit::WinId) -> Option<usize> {
        crate::picker::selected_index(self.app, window)
    }

    pub(crate) fn move_picker_selected(&mut self, window: crate::smelt_edit::WinId, delta: isize) {
        crate::picker::move_selected(self.app, window, delta);
    }

    pub(crate) fn window_scroll_snapshot(
        &self,
        window_id: crate::smelt_edit::WinId,
    ) -> Option<crate::app::transcript_scroll::WindowScrollSnapshot> {
        self.app.window_scroll_snapshot(window_id)
    }

    pub(crate) fn scroll_window(
        &mut self,
        window: crate::smelt_edit::WinId,
        command: crate::app::transcript_scroll::WindowScrollCommand,
    ) {
        self.app.scroll_window(window, command);
    }

    pub(crate) fn clear_search(&mut self) {
        self.app.clear_search();
    }
}

pub(crate) struct SessionLuaHost<'a> {
    app: &'a mut TuiApp,
}

impl SessionLuaHost<'_> {
    pub(crate) fn session_title(&self) -> Option<String> {
        self.app.conversation.session().title.clone()
    }

    pub(crate) fn set_session_title(
        &mut self,
        title: String,
        slug: String,
        history_len: Option<usize>,
    ) {
        self.app.set_session_title(title, slug, history_len);
    }

    pub(crate) fn session_slug(&self) -> Option<String> {
        self.app.conversation.session().slug.clone()
    }

    pub(crate) fn workspace_cwd(&self) -> String {
        self.app.workspace.cwd().to_owned()
    }

    pub(crate) fn enter_worktree(
        &mut self,
        name: &str,
        base: Option<&str>,
    ) -> Result<(smelt_core::worktree::WorktreeInfo, bool), String> {
        let cwd = self.app.workspace.cwd_path().to_owned();
        let worktree_root = std::path::PathBuf::from(&self.app.core.config.settings.worktree_root);
        let info = smelt_core::worktree::enter_or_create(
            &cwd,
            smelt_core::worktree::WorktreeSpec {
                name: Some(name),
                base,
                root: Some(&worktree_root),
            },
        )?;
        let (_, pending) = self.app.change_cwd(info.path.clone())?;
        Ok((info, pending))
    }

    pub(crate) fn managed_worktrees(
        &self,
    ) -> Result<Vec<smelt_core::worktree::ManagedWorktreeInfo>, String> {
        let root = std::path::Path::new(&self.app.core.config.settings.worktree_root);
        smelt_core::worktree::list_managed(self.app.workspace.cwd_path(), Some(root))
    }

    pub(crate) fn change_cwd(
        &mut self,
        path: std::path::PathBuf,
    ) -> Result<(String, bool), String> {
        self.app.change_cwd(path)
    }

    pub(crate) fn set_context_note(&mut self, name: String, text: Option<String>) {
        self.app.set_context_note(name, text);
    }

    pub(crate) fn system_prompt(&self) -> String {
        self.app.assemble_system_prompt()
    }

    pub(crate) fn session_cost(&self) -> f64 {
        self.app.conversation.session().session_cost_usd
    }

    pub(crate) fn session_usage(&self) -> protocol::TokenUsage {
        self.app.conversation.session().session_usage.clone()
    }

    pub(crate) fn set_fast_mode(&mut self, enabled: bool) {
        self.app.set_fast_mode(enabled);
    }

    fn context_recalculating(&self) -> bool {
        self.app.working.is_compacting() || self.app.busy_stack.context_recalculating()
    }

    pub(crate) fn session_status(&self) -> SessionStatusSnapshot {
        let session = self.app.conversation.session();
        let context_recalculating = self.context_recalculating();
        let context_tokens = session.display_context_tokens();
        SessionStatusSnapshot {
            active_model: self.app.core.config.active_model().cloned(),
            cost: session.session_cost_usd,
            mode: self.app.core.config.mode.as_str().to_owned(),
            mode_pending: self.app.mode_pending(),
            reasoning: self.app.core.config.reasoning_effort.label().to_owned(),
            reasoning_pending: self.app.reasoning_effort_pending(),
            fast_supported: self.app.fast_mode_supported(),
            fast_active: self.app.fast_mode_active(),
            context_state: if context_recalculating {
                "recalculating"
            } else {
                "ready"
            },
            context_tokens,
            context_window: self.app.core.config.context_window,
            context_stale: context_tokens.is_some()
                && session.display_context_tokens_stale(&self.app.active_context_token_identity()),
        }
    }

    pub(crate) fn session_context_tokens(&self) -> Option<u32> {
        if self.context_recalculating() {
            return None;
        }
        self.app.conversation.session().display_context_tokens()
    }

    pub(crate) fn session_created_at_ms(&self) -> u64 {
        self.app.conversation.session().created_at_ms
    }

    pub(crate) fn session_info(&mut self) -> Result<SessionInfoSnapshot, String> {
        let turn_count = self.rewind_turns()?.len();
        let session = self.app.conversation.session();
        let history_count = self.app.session_history_len();
        let has_live_session = self.app.conversation.has_live_session();
        let message_count = if has_live_session {
            history_count
        } else {
            protocol::history_to_messages(&session.history).len()
        };
        Ok(SessionInfoSnapshot {
            id: session.id.clone(),
            artifact_dir: self.app.current_artifact_dir(),
            ephemeral: self.app.ephemeral(),
            title: session.title.clone(),
            slug: session.slug.clone(),
            first_user_message: session.first_user_message.clone(),
            parent_id: session.parent_id.clone(),
            created_at_ms: session.created_at_ms,
            updated_at_ms: session.updated_at_ms,
            cwd: self.app.workspace.cwd().to_owned(),
            session_cwd: session.cwd.clone(),
            active_model: self.app.core.config.active_model().cloned(),
            mode: self.app.core.config.mode.as_str().to_owned(),
            reasoning: self.app.core.config.reasoning_effort.label().to_owned(),
            context_tokens: session.display_context_tokens(),
            context_tokens_stale: session
                .display_context_tokens_stale(&self.app.active_context_token_identity()),
            context_window: self.app.core.config.context_window,
            cost: session.session_cost_usd,
            history_count,
            message_count,
            message_count_approximate: has_live_session,
            turn_count,
            usage: session.session_usage.clone(),
            managed_worktree: self.app.workspace.is_managed_worktree(),
            project: self.app.workspace.project().to_owned(),
            branch: self.app.workspace.branch().to_owned(),
            worktree: self.app.workspace.worktree().to_owned(),
            worktree_path: self.app.workspace.worktree_path().to_owned(),
        })
    }

    pub(crate) fn session_id(&self) -> String {
        self.app.conversation.session().id.clone()
    }

    pub(crate) fn current_artifact_dir(&self) -> std::path::PathBuf {
        self.app.current_artifact_dir()
    }

    pub(crate) fn install_context_checkpoint(
        &mut self,
        kind: String,
        summary: String,
        first_live_message_index: usize,
        tokens_before: Option<u32>,
        guard: Option<(Option<u64>, Option<u64>)>,
    ) -> bool {
        if let Some((turn_id, cancel_generation)) = guard {
            if cancel_generation != Some(self.app.conversation.cancel_generation())
                || self.app.active_agent_turn_id() != turn_id
            {
                return false;
            }
        }
        self.app
            .install_context_checkpoint(kind, summary, first_live_message_index, tokens_before)
    }

    pub(crate) fn session_history_len(&self) -> usize {
        self.app.session_history_len()
    }

    pub(crate) fn session_history_range(
        &mut self,
        range: std::ops::Range<usize>,
    ) -> Result<Vec<protocol::HistoryItem>, String> {
        let result = self.app.session_history_range(range);
        if let Err(err) = &result {
            self.app.notify_session_error_sticky(format!(
                "failed to read canonical session history: {err}"
            ));
        }
        result
    }

    pub(crate) fn session_history_tail(
        &mut self,
        max_items: usize,
        max_bytes: Option<usize>,
    ) -> Result<Vec<protocol::HistoryItem>, String> {
        let result = self.app.session_history_tail(max_items, max_bytes);
        if let Err(err) = &result {
            self.app.notify_session_error_sticky(format!(
                "failed to read canonical session history: {err}"
            ));
        }
        result
    }

    pub(crate) fn model_history_messages(&self) -> Vec<protocol::Message> {
        self.app.model_history_messages()
    }

    pub(crate) fn rewind_turns(&mut self) -> Result<Vec<crate::app::history::RewindTurn>, String> {
        let result = self.app.rewind_turns();
        if let Err(err) = &result {
            self.app.notify_session_error_sticky(format!(
                "failed to enumerate rewindable session turns: {err}"
            ));
        }
        result
    }

    pub(crate) fn rewind_to_history(
        &mut self,
        history_idx: Option<usize>,
        restore_vim_insert: bool,
    ) {
        self.app
            .rewind_to_history_index(history_idx, restore_vim_insert);
    }

    pub(crate) fn rewind_active_user_turn_if_no_output(
        &mut self,
        restore_vim_insert: bool,
    ) -> bool {
        self.app
            .rewind_active_user_turn_if_no_output(restore_vim_insert)
    }

    pub(crate) fn retry_blocked_persistence(&mut self) -> bool {
        self.app.retry_blocked_persistence()
    }

    pub(crate) fn request_session_load(&mut self, id: &str) {
        self.app.request_session_load(id);
    }

    pub(crate) fn load_session_by_id_now(&mut self, id: &str) {
        self.app.load_session_by_id(id);
    }

    pub(crate) fn render_session_preview(
        &mut self,
        request: crate::app::session_preview::SessionPreviewRender,
    ) -> crate::app::session_preview::SessionPreviewRenderOutcome {
        self.app.render_session_preview(request)
    }

    pub(crate) fn list_session_entries(
        &self,
    ) -> smelt_core::session::SessionStoreResult<Vec<smelt_core::session::SessionListEntry>> {
        self.app
            .conversation
            .sessions()
            .list_session_entries_result()
    }

    pub(crate) fn list_session_page(
        &self,
        query: smelt_core::session::SessionListQuery,
    ) -> smelt_core::session::SessionStoreResult<smelt_core::session::SessionListPage> {
        self.app
            .conversation
            .sessions()
            .list_session_page_result(query)
    }

    pub(crate) fn session_search_blob(&self, id: &str) -> Option<String> {
        self.app.conversation.sessions().load_search_blob(id)
    }

    pub(crate) fn session_search_blobs(&self, ids: Vec<String>) -> Vec<(String, String)> {
        self.app.conversation.sessions().load_search_blobs(ids)
    }

    pub(crate) fn delete_session(&self, id: &str) -> Result<(), String> {
        let target = self
            .app
            .conversation
            .sessions()
            .resolve_prefix(id)
            .map_err(|error| error.to_string())?;
        if target.as_str() == self.app.conversation.session().id {
            return Err("cannot delete the active session".to_owned());
        }
        if self
            .app
            .conversation
            .delete_branch_through_persistence(&target)?
        {
            return Ok(());
        }
        self.app
            .conversation
            .sessions()
            .delete(target.as_str())
            .map_err(|error| error.to_string())
    }

    pub(crate) fn fork_session(&mut self) {
        self.app.fork_session();
    }

    pub(crate) fn reset_session(&mut self) {
        self.app.reset_session();
    }
}

fn drain_deferred_lua_operations(app: &mut TuiApp) {
    let operations =
        DEFERRED_LUA_OPERATIONS.with(|operations| std::mem::take(&mut *operations.borrow_mut()));
    let mut host = UiLuaHost { app };
    for operation in operations {
        match operation {
            DeferredLuaOperation::WindowKeymap { window, key } => {
                host.clear_window_keymap(window, key);
            }
            DeferredLuaOperation::WindowEvent {
                window,
                event,
                callback_id,
            } => {
                host.clear_window_event(window, event, callback_id);
            }
            DeferredLuaOperation::PaintEvent {
                paint,
                event,
                callback_id,
            } => {
                host.clear_paint_event(paint, event, callback_id);
            }
            DeferredLuaOperation::ModalKeymap { modal, key } => {
                host.clear_modal_keymap(modal, key);
            }
            DeferredLuaOperation::OverlayKeymap { overlay, key } => {
                host.clear_overlay_keymap(overlay, key);
            }
        }
    }
}

scoped_thread_local!(static mut TUI_APP: for<'a> &'a mut TuiApp);

struct TuiCoreBridge;

struct DeferredLuaDrain;

impl Drop for DeferredLuaDrain {
    fn drop(&mut self) {
        TUI_APP.with(drain_deferred_lua_operations);
        TUI_APP.with(crate::app::host_dispatch::drain_deferred_host_replies);
    }
}

impl smelt_core::host::LuaHost for TuiCoreBridge {
    fn with_core(&mut self, callback: &mut dyn FnMut(&mut smelt_core::Core)) {
        TUI_APP.with(|app| callback(&mut app.core));
    }
}

#[cfg(any(test, feature = "harness"))]
fn harness_async_runtime() -> &'static tokio::runtime::Handle {
    static HANDLE: std::sync::OnceLock<tokio::runtime::Handle> = std::sync::OnceLock::new();
    HANDLE.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("smelt-harness-async".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("build harness async runtime");
                tx.send(runtime.handle().clone())
                    .expect("publish harness async runtime");
                runtime.block_on(std::future::pending::<()>());
            })
            .expect("start harness async runtime");
        rx.recv().expect("receive harness async runtime")
    })
}

/// Lend `app` to Lua for the dynamic extent of one Lua entry.
pub(crate) fn scope_app<R>(app: &mut TuiApp, body: impl FnOnce() -> R) -> R {
    #[cfg(any(test, feature = "harness"))]
    let _runtime = tokio::runtime::Handle::try_current()
        .is_err()
        .then(|| harness_async_runtime().enter());
    TUI_APP.set(app, || {
        let mut core = TuiCoreBridge;
        smelt_core::host::scope_host(&mut core, || {
            let drain = DeferredLuaDrain;
            let output = body();
            drop(drain);
            output
        })
    })
}

pub(crate) fn ui_host_available() -> bool {
    TUI_APP.is_set()
}

fn with_scoped_app<R>(callback: impl FnOnce(&mut TuiApp) -> R) -> R {
    TUI_APP.with(|app| {
        drain_deferred_lua_operations(app);
        callback(app)
    })
}

pub(crate) fn with_runtime_host<R>(callback: impl FnOnce(&mut RuntimeLuaHost<'_>) -> R) -> R {
    with_scoped_app(|app| callback(&mut RuntimeLuaHost { app }))
}

pub(crate) fn try_with_runtime_host<R>(
    callback: impl FnOnce(&mut RuntimeLuaHost<'_>) -> R,
) -> Option<R> {
    TUI_APP.is_set().then(|| with_runtime_host(callback))
}

pub(crate) fn with_conversation_host<R>(
    callback: impl FnOnce(&mut ConversationLuaHost<'_>) -> R,
) -> R {
    with_scoped_app(|app| callback(&mut ConversationLuaHost { app }))
}

pub(crate) fn try_with_conversation_host<R>(
    callback: impl FnOnce(&mut ConversationLuaHost<'_>) -> R,
) -> Option<R> {
    TUI_APP.is_set().then(|| with_conversation_host(callback))
}

pub(crate) fn with_agent_host<R>(callback: impl FnOnce(&mut AgentLuaHost<'_>) -> R) -> R {
    with_scoped_app(|app| callback(&mut AgentLuaHost { app }))
}

pub(crate) fn try_with_agent_host<R>(
    callback: impl FnOnce(&mut AgentLuaHost<'_>) -> R,
) -> Option<R> {
    TUI_APP.is_set().then(|| with_agent_host(callback))
}

pub(crate) fn with_platform_host<R>(callback: impl FnOnce(&mut PlatformLuaHost<'_>) -> R) -> R {
    with_scoped_app(|app| callback(&mut PlatformLuaHost { app }))
}

pub(crate) fn try_with_platform_host<R>(
    callback: impl FnOnce(&mut PlatformLuaHost<'_>) -> R,
) -> Option<R> {
    TUI_APP.is_set().then(|| with_platform_host(callback))
}

pub(crate) fn with_ui_host<R>(callback: impl FnOnce(&mut UiLuaHost<'_>) -> R) -> R {
    with_scoped_app(|app| callback(&mut UiLuaHost { app }))
}

pub(crate) fn try_with_ui_host<R>(callback: impl FnOnce(&mut UiLuaHost<'_>) -> R) -> Option<R> {
    TUI_APP.is_set().then(|| with_ui_host(callback))
}

pub(crate) fn with_session_host<R>(callback: impl FnOnce(&mut SessionLuaHost<'_>) -> R) -> R {
    with_scoped_app(|app| callback(&mut SessionLuaHost { app }))
}

pub(crate) fn try_with_session_host<R>(
    callback: impl FnOnce(&mut SessionLuaHost<'_>) -> R,
) -> Option<R> {
    TUI_APP.is_set().then(|| with_session_host(callback))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_scope_restores_the_outer_app() {
        let mut outer = crate::app::test_harness::TestApp::builder().build();
        let mut nested = crate::app::test_harness::TestApp::builder().build();
        outer
            .app
            .conversation
            .set_session_id_for_harness("outer".into());
        outer.app.core.config.settings.fast_mode = false;
        nested
            .app
            .conversation
            .set_session_id_for_harness("nested".into());
        nested.app.core.config.settings.fast_mode = true;

        scope_app(&mut outer.app, || {
            assert_eq!(with_session_host(|host| host.session_id()), "outer");
            scope_app(&mut nested.app, || {
                assert_eq!(with_session_host(|host| host.session_id()), "nested");
                assert!(smelt_core::host::with_core(|core| core
                    .config
                    .settings
                    .fast_mode));
            });
            assert_eq!(with_session_host(|host| host.session_id()), "outer");
            assert!(!smelt_core::host::with_core(|core| core
                .config
                .settings
                .fast_mode));
        });
        assert!(try_with_session_host(|_| ()).is_none());
    }

    #[test]
    fn capability_borrow_cannot_alias_itself() {
        let mut app = crate::app::test_harness::TestApp::builder().build();

        scope_app(&mut app.app, || {
            with_session_host(|host| {
                assert!(!host.session_id().is_empty());
                assert!(try_with_session_host(|_| ()).is_none());
            });
            assert!(try_with_session_host(|host| host.session_id()).is_some());
        });
    }
}
