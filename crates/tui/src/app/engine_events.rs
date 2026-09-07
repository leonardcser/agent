use crate::app::{PendingTool, SessionControl, TuiApp};
use protocol::EngineEvent;
use smelt_core::content::stream_parser::ToolStart;
use smelt_core::transcript_model::{Block, ToolOutput, ToolStatus};
use smelt_core::working::{TurnOutcome, TurnPhase};
use smelt_core::ConfirmRequest;
use std::time::Duration;

struct EngineEventResult {
    control: SessionControl,
    assistant_output_started: bool,
}

fn waits_for_pending_transcript_work(event: &EngineEvent) -> bool {
    matches!(
        event,
        EngineEvent::HistoryAppended { .. }
            | EngineEvent::HistoryUpdated { .. }
            | EngineEvent::TurnComplete { .. }
            | EngineEvent::TurnError { .. }
    )
}

fn is_reasoning_summary(event: &EngineEvent) -> bool {
    matches!(
        event,
        EngineEvent::ReasoningPartFinished {
            kind: protocol::ReasoningKind::Summary,
            ..
        } | EngineEvent::Reasoning {
            kind: protocol::ReasoningKind::Summary,
            ..
        }
    )
}

impl TuiApp {
    pub(crate) fn publish_visible_token_usage(&mut self, usage: protocol::TokenUsage) {
        self.core
            .signals
            .set_dyn("tokens_used", std::rc::Rc::new(usage));
    }

    pub(crate) fn reset_visible_context_tokens(&mut self) {
        self.publish_visible_token_usage(protocol::TokenUsage {
            context_tokens: Some(0),
            prompt_tokens: Some(0),
            ..Default::default()
        });
    }

    pub(crate) fn record_visible_token_usage(&mut self, usage: protocol::TokenUsage) {
        if !self.session_is_read_only() {
            let identity = self.active_context_token_identity();
            if self
                .conversation
                .record_context_tokens(usage.clone(), identity)
            {
                self.conversation.mark_context_tokens_updated();
                if self.active_provider_supports_mid_turn_reasoning_changes() {
                    self.sync_reasoning_effort_applied();
                }
            }
        }
        self.publish_visible_token_usage(usage);
    }

    fn resolve_tool_summary_for_engine_event(
        &mut self,
        tool_name: &str,
        args: &std::collections::HashMap<String, serde_json::Value>,
        summary: protocol::StyledLines,
    ) -> protocol::StyledLines {
        if !summary.is_empty() {
            return summary;
        }
        let lua = self.lua.execution();
        let summary = crate::lua::scope_app(self, || {
            lua.tool_summary_with_context(tool_name, args, true)
        });
        if summary.is_empty() && !lua.has_tool(tool_name) {
            let summary = smelt_core::mcp::args_summary(args);
            if !summary.is_empty() {
                return summary;
            }
        }
        if summary.is_empty() {
            protocol::StyledLines::from_plain(tool_name)
        } else {
            summary
        }
    }

    fn begin_tool_block_for_engine_event(
        &mut self,
        pending: &mut Vec<PendingTool>,
        start: ToolStart,
    ) {
        let ToolStart {
            invocation_id,
            call_id,
            name: tool_name,
            summary,
            args,
            called_at_ms,
            ..
        } = start;
        if pending
            .iter()
            .any(|pending| pending.invocation_id == invocation_id)
        {
            return;
        }
        self.flush_streaming_thinking();
        self.flush_streaming_text();
        if !self.promote_tool_draft(
            invocation_id,
            call_id.clone(),
            tool_name.clone(),
            summary.clone(),
            args.clone(),
            called_at_ms,
        ) {
            self.start_tool_at(
                invocation_id,
                call_id.clone(),
                tool_name.clone(),
                summary,
                args.clone(),
                called_at_ms,
            );
        }
        self.core.signals.emit_dyn(
            "tool_start",
            std::rc::Rc::new(smelt_core::signals::ToolStart {
                tool: tool_name.clone(),
                args: args.clone(),
            }),
        );
        self.pump_lua();
        pending.push(PendingTool {
            invocation_id,
            name: tool_name,
        });
    }

    fn finish_tool_for_engine_event(
        &mut self,
        pending: &mut Vec<PendingTool>,
        invocation_id: protocol::InvocationId,
        result: protocol::ToolOutcome,
        elapsed_ms: Option<u64>,
        status: ToolStatus,
    ) {
        self.finish_tool_now_for_engine_event(pending, invocation_id, result, elapsed_ms, status);
    }

    fn finish_tool_now_for_engine_event(
        &mut self,
        pending: &mut Vec<PendingTool>,
        invocation_id: protocol::InvocationId,
        result: protocol::ToolOutcome,
        elapsed_ms: Option<u64>,
        status: ToolStatus,
    ) {
        let mut finished_tool_name: Option<String> = None;
        let mut finished_is_error = false;
        if let Some(idx) = pending
            .iter()
            .position(|pending| pending.invocation_id == invocation_id)
        {
            let removed = pending.remove(idx);
            finished_tool_name = Some(removed.name.clone());
            finished_is_error = result.is_error;
            let content = self
                .conversation
                .active_tool_output_content(invocation_id)
                .unwrap_or_else(|| result.content.into());
            let output = Some(Box::new(ToolOutput::from_display_content(
                content,
                result.is_error,
                result.metadata,
                result.display_content,
            )));
            let elapsed = elapsed_ms.map(Duration::from_millis);
            self.finish_tool(invocation_id, status, output, elapsed);
        }
        if let Some(tool_name) = finished_tool_name {
            self.core.signals.emit_dyn(
                "tool_end",
                std::rc::Rc::new(smelt_core::signals::ToolEnd {
                    tool: tool_name,
                    is_error: finished_is_error,
                    elapsed_ms,
                }),
            );
            self.pump_lua();
        }
    }

    fn complete_reasoning_summary_part(&mut self, title: Option<String>, content: String) -> bool {
        let title = title
            .map(|title| smelt_buffer::text::trim_whitespace(&title).to_string())
            .filter(|title| !title.is_empty());
        let content = smelt_buffer::text::trim_whitespace(&content).to_string();
        if title.is_none() && content.is_empty() {
            return false;
        }

        self.flush_streaming_thinking();
        let Some(previous) = self.conversation.promote_last_reasoning_summary() else {
            self.push_block(Block::Thinking {
                summary_titles: title.iter().cloned().collect(),
                title,
                content: content.into(),
                kind: protocol::ReasoningKind::Summary,
            });
            return true;
        };
        let id = previous.id;
        let previous_title = previous.title;
        let mut summary_titles = previous.summary_titles;
        let previous_content = previous.content;

        if let Some(title) = title.as_ref() {
            if summary_titles.last() != Some(title) {
                summary_titles.push(title.clone());
            }
        }
        let mut combined_content = previous_content;
        if !content.is_empty() {
            if !combined_content.is_empty() {
                combined_content.push_owned("\n".to_string());
            }
            combined_content.push_owned(content);
        }
        self.conversation.rewrite_block(
            id,
            Block::Thinking {
                title: title.or(previous_title),
                summary_titles,
                content: combined_content,
                kind: protocol::ReasoningKind::Summary,
            },
        );
        true
    }

    /// Route an `EngineEvent` through the correct branch of the agent
    /// state machine. When a turn is active, delegates to
    /// `handle_engine_event` + `dispatch_control` and discards the turn on
    /// a stop signal; when idle, falls through to `handle_idle_engine_event`.
    /// Shared by the production main loop, the test harness, and the
    /// scenario replay binary so all three drive identical state.
    pub fn dispatch_engine_event(&mut self, ev: EngineEvent) -> bool {
        let _perf = smelt_perf::perf::begin("tui:dispatch_engine_event");
        self.dispatch_engine_event_inner(ev)
    }

    pub(crate) fn queue_engine_continuation(&mut self, event: EngineEvent) {
        if matches!(event, EngineEvent::EngineAskDelta { .. }) {
            self.transcript_work.push_auxiliary_continuation(event);
        } else {
            self.transcript_work.push_provider_continuation(event);
        }
    }

    #[cfg(all(test, feature = "transcript-bench"))]
    pub(crate) fn transcript_work_pending_for_harness(&self) -> bool {
        !self.transcript_work.is_empty()
    }

    pub(crate) fn discard_pending_transcript_work(&mut self) {
        self.transcript_work.discard_main_turn_work();
        self.conversation.release_all_deferred_engine_blocks();
    }

    pub(crate) fn apply_pending_transcript_work(&mut self) {
        use crate::app::transcript_work::TranscriptWork;

        let _perf = smelt_perf::perf::begin("transcript:apply_pending_work");
        let mut applied = 0u64;
        while let Some(work) = self.transcript_work.pop_front() {
            let stop_at_frame_boundary = match work {
                TranscriptWork::ProviderContinuation(event) => {
                    let keep_streaming = self.dispatch_engine_event_now(event);
                    debug_assert!(keep_streaming, "a continuation cannot terminate its turn");
                    self.transcript_work.front_is_tool_output_append()
                }
                TranscriptWork::AuxiliaryContinuation(event) => {
                    self.dispatch_engine_event_now(event);
                    false
                }
                TranscriptWork::ToolOutputAppend(mut pending) => {
                    if pending.defer_until_frame {
                        pending.defer_until_frame = false;
                        self.transcript_work.push_front_tool_output(pending);
                    } else if let Some(pending) =
                        self.conversation.advance_tool_output_append(pending)
                    {
                        self.transcript_work.push_front_tool_output(pending);
                    }
                    true
                }
                TranscriptWork::DeferredReasoningSummary { event, block_id } => {
                    if self.conversation.deferred_engine_block_is_ready(block_id) {
                        self.dispatch_engine_event_now(event);
                        self.conversation.release_deferred_engine_block(block_id);
                    } else if self.conversation.deferred_engine_block_failed(block_id) {
                        self.conversation.release_deferred_engine_block(block_id);
                        self.dispatch_engine_event_now(event);
                    } else {
                        self.transcript_work
                            .push_front_deferred_reasoning_summary(event, block_id);
                    }
                    true
                }
                TranscriptWork::OrderedEngineEvent(event) => {
                    self.dispatch_engine_event_now(event);
                    true
                }
                TranscriptWork::AppendExecOutput(chunk) => {
                    self.conversation.append_exec_output(chunk);
                    false
                }
                TranscriptWork::FinishExec(final_output) => {
                    self.conversation.finish_exec(final_output);
                    false
                }
            };
            applied = applied.saturating_add(1);
            if stop_at_frame_boundary {
                break;
            }
        }
        smelt_perf::perf::record_value("transcript:pending_work:applied", applied);
    }

    fn dispatch_engine_event_inner(&mut self, ev: EngineEvent) -> bool {
        if is_reasoning_summary(&ev) {
            if let Some(block_id) = self.conversation.defer_last_reasoning_summary_hydration() {
                self.transcript_work
                    .push_deferred_reasoning_summary(ev, block_id);
                self.request_continuation_render();
                return true;
            }
        }
        let wait_for_work = match &ev {
            EngineEvent::ToolFinished { invocation_id, .. } => {
                self.transcript_work.has_pending_tool_output(*invocation_id)
            }
            _ if waits_for_pending_transcript_work(&ev) => {
                self.transcript_work.has_main_turn_work()
            }
            _ => false,
        };
        if wait_for_work {
            self.transcript_work.push_ordered_engine_event(ev);
            self.request_continuation_render();
            return true;
        }
        self.dispatch_engine_event_now(ev)
    }

    fn dispatch_engine_event_now(&mut self, ev: EngineEvent) -> bool {
        if !self.conversation.is_active() {
            self.handle_idle_engine_event(ev);
            return true;
        }

        let end = self
            .with_dispatched_turn(move |app, turn| {
                let result = app.handle_engine_event(
                    ev,
                    turn.turn_id,
                    turn.submitted_history_idx,
                    &mut turn.pending,
                );
                turn.assistant_output_started |= result.assistant_output_started;
                app.dispatch_control(result.control, turn)
            })
            .expect("active turn enters engine event dispatch");
        match end {
            SessionControl::Continue | SessionControl::NeedsConfirm(_) => true,
            SessionControl::Done => {
                if self.take_queued_compaction_handoff() {
                    self.interrupt_with_next_queued();
                } else {
                    self.discard_turn(crate::app::TurnEnd::Complete);
                }
                false
            }
            SessionControl::Error { kind, retry_at_ms } => {
                self.discard_turn(crate::app::TurnEnd::Errored { kind, retry_at_ms });
                false
            }
        }
    }

    fn preserve_submitted_item_in_history_update(
        &mut self,
        first_index: usize,
        mut items: Vec<protocol::HistoryItem>,
        submitted_history_idx: usize,
    ) -> Vec<protocol::HistoryItem> {
        let Some(relative_idx) = submitted_history_idx.checked_sub(first_index) else {
            return items;
        };
        let Some(incoming) = items.get(relative_idx) else {
            return items;
        };
        let existing =
            match self.session_history_range(submitted_history_idx..submitted_history_idx + 1) {
                Ok(existing) => existing,
                Err(err) => {
                    self.notify_session_error_sticky(format!(
                        "failed to read canonical session history: {err}"
                    ));
                    return items;
                }
            };
        if let Some(submitted) = existing.first() {
            if submitted != incoming {
                items[relative_idx] = submitted.clone();
            }
        }
        items
    }

    fn handle_engine_event(
        &mut self,
        ev: EngineEvent,
        turn_id: u64,
        submitted_history_idx: usize,
        pending: &mut Vec<PendingTool>,
    ) -> EngineEventResult {
        let mut assistant_output_started = false;
        let control = match ev {
            EngineEvent::Ready => SessionControl::Continue,
            EngineEvent::TokenUsage {
                usage,
                tokens_per_sec,
                cost_usd,
                background,
            } => {
                if !background {
                    self.record_visible_token_usage(usage.clone());
                    if let Some(tps) = tokens_per_sec {
                        self.working.record_tokens_per_sec(tps);
                    }
                    {
                        self.working.begin(TurnPhase::Working);
                    };
                }
                let cost = cost_usd.unwrap_or(0.0);
                if !self.session_is_read_only() {
                    self.conversation.accumulate_usage(usage.clone(), cost);
                }
                crate::metrics::append(
                    self.core.sessions.state_root(),
                    &crate::metrics::MetricsEntry {
                        timestamp_ms: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as u64,
                        prompt_tokens: usage.prompt_tokens.unwrap_or(0),
                        completion_tokens: usage.completion_tokens.unwrap_or(0),
                        model: self
                            .core
                            .config
                            .active_model()
                            .map_or_else(|| "unknown".into(), |model| model.model_name.clone()),
                        cost_usd,
                        cache_read_tokens: usage.cache_read_tokens,
                        cache_write_tokens: usage.cache_write_tokens,
                        reasoning_tokens: usage.reasoning_tokens,
                    },
                );
                SessionControl::Continue
            }
            EngineEvent::ToolOutput {
                invocation_id,
                line,
                ..
            } => {
                assistant_output_started = true;
                self.append_active_output_line(invocation_id, line);
                SessionControl::Continue
            }
            EngineEvent::Steered {
                text,
                count,
                sent_at_ms,
            } => {
                self.flush_streaming_thinking();
                self.flush_streaming_text();
                let drained = self.prompt.acknowledge_requests(count);
                if !drained.is_empty() {
                    let display = drained
                        .iter()
                        .map(crate::app::QueuedInput::display)
                        .collect::<Vec<_>>()
                        .join("\n");
                    let text = if display.is_empty() { text } else { display };
                    let command = drained
                        .first()
                        .is_some_and(crate::app::QueuedInput::is_command);
                    self.push_block(Block::User {
                        text,
                        image_labels: vec![],
                        command,
                        sent_at_ms: Some(sent_at_ms),
                    });
                    for queued in &drained {
                        if let crate::app::QueuedInput::Command {
                            line, sent_at_ms, ..
                        } = queued
                        {
                            self.run_queued_command_line(line, *sent_at_ms);
                        }
                    }
                }
                SessionControl::Continue
            }
            EngineEvent::ReasoningPartStarted { kind, .. } => {
                if kind == protocol::ReasoningKind::Raw {
                    self.flush_streaming_thinking();
                }
                SessionControl::Continue
            }
            EngineEvent::ReasoningPartDelta { kind, delta, .. } => {
                assistant_output_started = !delta.is_empty();
                let bytes = delta.len();
                if kind == protocol::ReasoningKind::Raw {
                    self.append_streaming_thinking(&delta);
                }
                self.core.signals.emit_dyn(
                    "stream_delta",
                    std::rc::Rc::new(smelt_core::signals::StreamDelta {
                        kind: "thinking".to_string(),
                        bytes,
                        text: delta,
                        call_id: None,
                        tool_name: None,
                    }),
                );
                SessionControl::Continue
            }
            EngineEvent::ReasoningPartFinished {
                kind,
                title,
                content,
                ..
            } => {
                if kind == protocol::ReasoningKind::Raw {
                    self.flush_streaming_thinking();
                } else {
                    assistant_output_started = self.complete_reasoning_summary_part(title, content);
                }
                SessionControl::Continue
            }
            EngineEvent::Reasoning {
                kind,
                title,
                content,
            } => {
                if kind == protocol::ReasoningKind::Summary {
                    assistant_output_started = self.complete_reasoning_summary_part(title, content);
                } else {
                    self.flush_streaming_thinking();
                    assistant_output_started = self.try_push_block(Block::Thinking {
                        title,
                        summary_titles: Vec::new(),
                        content: content.into(),
                        kind,
                    });
                }
                SessionControl::Continue
            }
            EngineEvent::TextDelta { delta } => {
                assistant_output_started = !delta.is_empty();
                let bytes = delta.len();
                self.append_streaming_text(&delta);
                self.core.signals.emit_dyn(
                    "stream_delta",
                    std::rc::Rc::new(smelt_core::signals::StreamDelta {
                        kind: "text".to_string(),
                        bytes,
                        text: delta,
                        call_id: None,
                        tool_name: None,
                    }),
                );
                SessionControl::Continue
            }
            EngineEvent::ToolCallDraftStarted {
                stream_id,
                call_id,
                tool_name,
            } => {
                assistant_output_started = true;
                self.handle_tool_draft_started(stream_id, call_id, tool_name);
                SessionControl::Continue
            }
            EngineEvent::ToolCallDraftDelta {
                stream_id,
                call_id,
                tool_name,
                delta,
            } => {
                assistant_output_started = !delta.is_empty();
                self.handle_tool_draft_delta(stream_id, call_id, tool_name, delta);
                SessionControl::Continue
            }
            EngineEvent::ToolCallDraftFinished {
                stream_id,
                call_id,
                tool_name,
                arguments,
            } => {
                assistant_output_started = true;
                self.handle_tool_draft_finished(stream_id, call_id, tool_name, arguments);
                SessionControl::Continue
            }
            EngineEvent::Text { content } => {
                self.flush_streaming_text();
                assistant_output_started = self.try_push_block(Block::Text {
                    content: content.into(),
                });
                SessionControl::Continue
            }
            EngineEvent::ToolStarted {
                invocation_id,
                call_id,
                tool_name,
                args,
                called_at_ms,
            } => {
                assistant_output_started = true;
                let summary = self.resolve_tool_summary_for_engine_event(
                    &tool_name,
                    &args,
                    protocol::StyledLines::empty(),
                );
                self.begin_tool_block_for_engine_event(
                    pending,
                    ToolStart {
                        invocation_id,
                        call_id,
                        name: tool_name,
                        summary,
                        args,
                        preview_output: None,
                        called_at_ms,
                    },
                );
                SessionControl::Continue
            }
            EngineEvent::ToolFinished {
                invocation_id,
                result,
                elapsed_ms,
                ..
            } => {
                assistant_output_started = true;
                let status = if result.is_error {
                    ToolStatus::Err
                } else {
                    ToolStatus::Ok
                };
                self.finish_tool_for_engine_event(
                    pending,
                    invocation_id,
                    result,
                    elapsed_ms,
                    status,
                );
                SessionControl::Continue
            }
            EngineEvent::ToolRejected {
                invocation_id,
                call_id,
                tool_name,
                args,
                summary,
                result,
                elapsed_ms,
                called_at_ms,
            } => {
                assistant_output_started = true;
                let summary =
                    self.resolve_tool_summary_for_engine_event(&tool_name, &args, summary);
                self.begin_tool_block_for_engine_event(
                    pending,
                    ToolStart {
                        invocation_id,
                        call_id: call_id.clone(),
                        name: tool_name,
                        summary,
                        args,
                        preview_output: None,
                        called_at_ms,
                    },
                );
                let status = if result.is_error {
                    ToolStatus::Err
                } else {
                    ToolStatus::Denied
                };
                self.finish_tool_for_engine_event(
                    pending,
                    invocation_id,
                    result,
                    elapsed_ms,
                    status,
                );
                SessionControl::Continue
            }
            EngineEvent::RequestPermission {
                request_id,
                invocation_id,
                call_id,
                tool_name,
                args,
                approval_patterns,
                called_at_ms,
                summary,
            } => {
                assistant_output_started = true;
                let summary =
                    self.resolve_tool_summary_for_engine_event(&tool_name, &args, summary);
                let lua = self.lua.execution();
                let (tool_paths, tool_paths_error) = match crate::lua::scope_app(self, || {
                    lua.tool_paths_for_workspace(&tool_name, &args)
                }) {
                    Ok(paths) => (paths.into_paths(), None),
                    Err(error) => (Vec::new(), Some(error)),
                };
                self.begin_tool_block_for_engine_event(
                    pending,
                    ToolStart {
                        invocation_id,
                        call_id: call_id.clone(),
                        name: tool_name.clone(),
                        summary: summary.clone(),
                        args: args.clone(),
                        preview_output: None,
                        called_at_ms,
                    },
                );
                SessionControl::NeedsConfirm(Box::new(ConfirmRequest {
                    invocation_id,
                    call_id,
                    tool_name,
                    args,
                    tool_paths,
                    tool_paths_error,
                    approval_candidates: approval_patterns,
                    grant_options: Vec::new(),
                    summary,
                    request_id,
                }))
            }
            EngineEvent::Retrying { delay_ms, attempt } => {
                // The retry restarts the turn from the last committed
                // message - any partial streaming text/thinking captured
                // before the failure is obsolete and must not bleed into
                // the next attempt's stream.
                self.flush_streaming_thinking();
                self.flush_streaming_text();
                self.clear_tool_drafts();
                self.working.begin(TurnPhase::Retrying {
                    delay: Duration::from_millis(delay_ms),
                    attempt,
                });
                SessionControl::Continue
            }
            EngineEvent::RequestAuditError { message } => {
                self.notify_warn(message);
                SessionControl::Continue
            }
            EngineEvent::EngineAskDelta { id, delta } => {
                self.continuing_engine_ask_ids.insert(id);
                let lua = self.lua.execution();
                crate::lua::scope_app(self, || lua.fire_ask_delta_callback(id, &delta));
                SessionControl::Continue
            }
            EngineEvent::EngineAskResponse { id, message, error } => {
                self.continuing_engine_ask_ids.remove(&id);
                let lua = self.lua.execution();
                crate::lua::scope_app(self, || lua.fire_ask_callback(id, message.as_ref(), error));
                SessionControl::Continue
            }
            EngineEvent::HistoryAppended { turn_id: id, delta } => {
                if id == turn_id {
                    assistant_output_started = delta
                        .items
                        .iter()
                        .any(|item| matches!(item, protocol::HistoryItem::Assistant(_)));
                    self.append_engine_history_items(delta.first_index.get(), delta.items);
                    self.save_session();
                }
                SessionControl::Continue
            }
            EngineEvent::HistoryUpdated {
                turn_id: id,
                update,
            } => {
                if id == turn_id {
                    assistant_output_started = update
                        .items
                        .iter()
                        .any(|item| matches!(item, protocol::HistoryItem::Assistant(_)));
                    self.set_history_from(update.first_index.get(), update.items);
                    self.save_session();
                }
                SessionControl::Continue
            }
            EngineEvent::TurnComplete {
                turn_id: id,
                history,
                meta,
            } => {
                if id != turn_id {
                    SessionControl::Continue
                } else {
                    if let Some(history) = history {
                        let first_index = history.first_index.get();
                        let items = self.preserve_submitted_item_in_history_update(
                            first_index,
                            history.items,
                            submitted_history_idx,
                        );
                        self.set_history_from(first_index, items);
                    }
                    let payload = meta.clone().unwrap_or(protocol::TurnMeta {
                        elapsed_ms: 0,
                        avg_tps: None,
                        display_tps: None,
                        interrupted: false,
                    });
                    self.core
                        .signals
                        .emit_dyn("turn_complete", std::rc::Rc::new(payload));
                    self.conversation.set_pending_meta(meta);
                    SessionControl::Done
                }
            }
            EngineEvent::TurnError {
                message,
                kind,
                retry_at_ms,
            } => {
                {
                    self.working.finish(TurnOutcome::Errored);
                };
                self.core.signals.emit_dyn(
                    "turn_error",
                    std::rc::Rc::new(smelt_core::signals::TurnError {
                        message: message.clone(),
                    }),
                );
                if matches!(
                    kind,
                    Some(
                        protocol::EngineAskErrorKind::Quota
                            | protocol::EngineAskErrorKind::RateLimited
                    )
                ) {
                    if let Ok(mut messages) = self.lua.core_shared().messages.lock() {
                        messages.append(
                            smelt_core::messages::MessageKind::Error,
                            "smelt".into(),
                            message,
                        );
                    }
                } else {
                    self.notify_turn_error_sticky(message);
                }
                SessionControl::Error { kind, retry_at_ms }
            }
            EngineEvent::Shutdown { .. } => SessionControl::Error {
                kind: None,
                retry_at_ms: None,
            },
            EngineEvent::ToolDispatch {
                request_id,
                invocation_id,
                call_id,
                tool_name,
                args,
            } => {
                // Plugins open their own confirm dialogs via `smelt.dialog.open` inside `execute`.
                self.handle_tool_call(request_id, invocation_id, call_id, tool_name, args);
                SessionControl::Continue
            }
            EngineEvent::ToolEvaluationRequest {
                request_id,
                invocation_id: _,
                call_id: _,
                tool_name,
                args,
                mode,
            } => {
                let lua = self.lua.execution();
                let metadata =
                    crate::lua::scope_app(self, || lua.evaluate_tool_metadata(&tool_name, &args));
                let decision = if let Some(err) = metadata.preflight_error.clone() {
                    protocol::Decision::Error(err)
                } else {
                    let lua = self.lua.execution();
                    match crate::lua::scope_app(self, || {
                        lua.tool_paths_for_workspace(&tool_name, &args)
                    }) {
                        Ok(tool_paths) => {
                            self.active_permissions()
                                .evaluate_tool_with_paths_and_approvals(
                                    mode,
                                    smelt_core::permissions::ToolOrigin::Lua,
                                    &tool_name,
                                    &args,
                                    tool_paths.as_slice(),
                                )
                                .decision
                        }
                        Err(error) => protocol::Decision::Error(error),
                    }
                };
                let evaluation = protocol::ToolEvaluation { decision, metadata };
                self.core
                    .engine
                    .send(protocol::UiCommand::ToolEvaluationResponse {
                        request_id,
                        evaluation,
                    });
                SessionControl::Continue
            }
            EngineEvent::CoreToolResult {
                request_id,
                content,
                is_error,
                metadata,
            } => {
                let lua = self.lua.execution();
                crate::lua::scope_app(self, move || {
                    lua.resolve_core_tool_call(request_id, content, is_error, metadata)
                });
                SessionControl::Continue
            }
        };
        EngineEventResult {
            control,
            assistant_output_started,
        }
    }
    /// Handle engine events that arrive when no turn is active.
    pub(crate) fn handle_idle_engine_event(&mut self, ev: EngineEvent) {
        match ev {
            // Stale history snapshots from cancelled/completed turns would overwrite a freshly cleared history.
            EngineEvent::HistoryUpdated { .. } => {}
            EngineEvent::TurnComplete {
                history: Some(history),
                ..
            } if !history.items.is_empty() => {
                // Persist final messages from a cancelled turn without rebuilding the screen.
                self.set_history_from(history.first_index.get(), history.items);
                self.save_session();
            }
            EngineEvent::EngineAskDelta { id, delta } => {
                self.continuing_engine_ask_ids.insert(id);
                let lua = self.lua.execution();
                crate::lua::scope_app(self, || lua.fire_ask_delta_callback(id, &delta));
            }
            EngineEvent::EngineAskResponse { id, message, error } => {
                self.continuing_engine_ask_ids.remove(&id);
                let lua = self.lua.execution();
                crate::lua::scope_app(self, || lua.fire_ask_callback(id, message.as_ref(), error));
            }
            EngineEvent::TurnError { message, .. } => {
                self.working.finish(TurnOutcome::Errored);
                self.notify_turn_error_sticky(message);
            }
            EngineEvent::RequestAuditError { message } => {
                self.notify_warn(message);
            }
            _ => {}
        }
    }
}
