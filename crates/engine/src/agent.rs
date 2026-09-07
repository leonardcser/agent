use crate::log;
use crate::provider::EngineProvider;
use crate::tools::{ToolContext, ToolDispatcher, ToolResult};
use crate::EngineConfig;
use protocol::Decision;
#[cfg(test)]
use protocol::ModelConfig;
use protocol::{
    AgentMode, AssistantStep, Content, EngineAskError, EngineAskErrorKind, EngineEvent,
    HistoryItem, Message, ModelTarget, ReasoningEffort, ReasoningKind, RequestRuntimeConfig,
    ToolInvocation, ToolOutcome, TurnMeta, UiCommand,
};
use serde_json::Value;
use smelt_provider::{
    quota_exceeded_message, sort_tools_for_cache_stability, CancellationToken, ChatOptions,
    ChatRequestOptions, ChatResponse, FunctionSchema, ProviderError, ProviderStreamEvent,
    ReasoningStreamEvent, RequestAttemptInfo, ResponseFormat, ToolCallStreamEvent, ToolDefinition,
};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

use std::time::Instant;
use tokio::sync::mpsc;

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_INVOCATION_ID: AtomicU64 = AtomicU64::new(1);

fn next_request_id() -> u64 {
    NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed)
}

fn next_invocation_id() -> protocol::InvocationId {
    protocol::InvocationId::new(NEXT_INVOCATION_ID.fetch_add(1, Ordering::Relaxed))
}

fn dispatch_request_audit(
    host_tx: &crate::HostCallSender,
    persistence: protocol::PersistenceScope,
    ctx: crate::request_log::RequestContext,
    info: &RequestAttemptInfo<'_>,
    pricing: &smelt_provider::ResolvedPricing,
    mode: protocol::RequestAuditMode,
) {
    let Some((entry, payload_mode)) = crate::request_log::entry(ctx, info, pricing, mode) else {
        return;
    };
    let _ = host_tx.send(crate::host::HostCall::RequestAudit {
        persistence,
        entry: Box::new(entry),
        payload_mode,
    });
}

fn last_note_kind(items: &[HistoryItem]) -> Option<protocol::HistoryNoteKind> {
    items.last().and_then(HistoryItem::note_kind)
}

fn mode_append_returns_to_base(append: &protocol::HistoryAppend) -> bool {
    let protocol::HistoryAppendPolicy::ModeChange { base } = &append.policy else {
        return false;
    };
    append
        .item
        .as_note()
        .and_then(protocol::HistoryNote::mode)
        .is_some_and(|mode| mode == base.as_str())
}

fn queue_context_history_item(
    history: &[HistoryItem],
    pending: &mut Vec<HistoryItem>,
    append: &protocol::HistoryAppend,
) -> bool {
    let (name, valid) = match &append.policy {
        protocol::HistoryAppendPolicy::SetContext { name } => (
            name,
            append
                .item
                .as_note()
                .and_then(protocol::HistoryNote::context_name)
                == Some(name.as_str()),
        ),
        protocol::HistoryAppendPolicy::ClearContext { name } => (
            name,
            append.item.as_note().is_some_and(|note| {
                note.context_name() == Some(name.as_str()) && note.text().trim().is_empty()
            }),
        ),
        _ => return false,
    };
    if !valid {
        return true;
    }

    pending.retain(|item| {
        item.as_note().and_then(protocol::HistoryNote::context_name) != Some(name.as_str())
    });
    let plan = protocol::plan_history_append(history, append)
        .expect("in-memory context append planning is infallible");
    debug_assert!(matches!(
        plan,
        protocol::HistoryAppendPlan::Unchanged | protocol::HistoryAppendPlan::Push
    ));
    if plan == protocol::HistoryAppendPlan::Push {
        pending.push(append.item.clone());
    }
    true
}

struct DispatcherTurnGuard<'a> {
    dispatcher: &'a dyn ToolDispatcher,
    turn_id: u64,
}

impl<'a> DispatcherTurnGuard<'a> {
    fn new(dispatcher: &'a dyn ToolDispatcher, turn_id: u64) -> Self {
        dispatcher.begin_turn(turn_id);
        Self {
            dispatcher,
            turn_id,
        }
    }
}

impl Drop for DispatcherTurnGuard<'_> {
    fn drop(&mut self) {
        self.dispatcher.end_turn(self.turn_id);
    }
}

/// Main engine task. Runs in a tokio::spawn and processes commands/events.
pub(crate) async fn engine_task(
    mut config: EngineConfig,
    dispatcher: Box<dyn crate::tools::ToolDispatcher>,
    mut cmd_rx: mpsc::UnboundedReceiver<UiCommand>,
    event_tx: crate::EngineEventSender,
    host_tx: crate::HostCallSender,
) {
    // Some openai-compatible endpoints gate on User-Agent (e.g. api.kimi.com).
    // Per-request header() calls (Copilot, Codex) still override this.
    let client = reqwest::Client::builder()
        .user_agent(concat!("smelt/", env!("CARGO_PKG_VERSION")))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    smelt_provider::catalog::spawn_fetch(
        client.clone(),
        Some(crate::paths::cache_dir().join("web")),
    );

    let _ = event_tx.send(EngineEvent::Ready);

    let mut bg_cancel = CancellationToken::new();

    loop {
        if bg_cancel.is_cancelled() {
            bg_cancel = CancellationToken::new();
        }
        tokio::select! {
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    UiCommand::StartTurn(payload) => {
                        let protocol::StartTurnPayload {
                            turn_id,
                            input,
                            mode,
                            model_target,
                            request_config,
                            reasoning_effort,
                            fast_mode,
                            history,
                            session_id,
                            sessions_root,
                            persistence,
                            permission_overrides,
                            system_prompt: tui_system_prompt,
                            tools,
                        } = *payload;
                        let loaded_history =
                            match load_model_history(history, &sessions_root, &session_id) {
                            Ok(history) => history,
                            Err(message) => {
                                let _ = event_tx.send(EngineEvent::TurnError {
                                    message,
                                    kind: None,
                                    retry_at_ms: None,
                                });
                                continue;
                            }
                        };

                        let provider = build_provider(
                            &model_target,
                            &client,
                            std::sync::Arc::clone(&config.clock),
                        );
                        let system_prompt = tui_system_prompt
                            .or_else(|| config.system_prompt_override.clone())
                            .unwrap_or_else(|| {
                                crate::build_system_prompt(
                                    config.system_prompt_behavior,
                                    crate::SystemPromptCapabilities::from_tool_calling(
                                        provider.tool_calling(),
                                    ),
                                    config.instructions.as_deref(),
                                    config.skill_section.as_deref(),
                                )
                            });
                        let _dispatcher_turn =
                            DispatcherTurnGuard::new(dispatcher.as_ref(), turn_id);
                        let started_at = config.clock.instant_now();
                        let mut turn = Turn {
                            provider,
                            dispatcher: &*dispatcher,
                            cmd_rx: &mut cmd_rx,
                            event_tx: &event_tx,
                            host_tx: &host_tx,
                            config: &mut config,
                            http_client: &client,
                            cancel: CancellationToken::new(),
                            bg_cancel: bg_cancel.clone(),
                            history: Vec::new(),
                            history_coordinates: loaded_history.coordinates,
                            mode,
                            reasoning_effort,
                            fast_mode,
                            turn_id,
                            model_target,
                            target_revision: 0,
                            request_config,
                            system_prompt,
                            tools,
                            permission_overrides,
                            pending_history_items: Vec::new(),
                            next_history_changed_from: 0,
                            history_revision: 0,
                            session_id,
                            persistence,
                            started_at,
                            tps_samples: Vec::new(),
                        };
                        turn.run(input, loaded_history.items).await;
                    }
                    UiCommand::SetTurnModel { .. } => {}
                    UiCommand::UpdateAgentProjectContext(context) => {
                        config.install_project_context(&context);
                    }
                    UiCommand::SetMode { .. } => {}
                    UiCommand::SetFastMode { .. } => {}
                    UiCommand::Cancel => {
                        bg_cancel.cancel();
                    }
                    other => {
                        let ctx = BackgroundCtx {
                            config: &config,
                            http_client: &client,
                            dispatcher: &*dispatcher,
                            event_tx: &event_tx,
                            host_tx: &host_tx,
                            bg_cancel: bg_cancel.clone(),
                        };
                        let _ = dispatch_background_cmd(other, &ctx);
                    }
                }
            }
            else => break,
        }
    }

    let _ = event_tx.send(EngineEvent::Shutdown { reason: None });
}

struct LoadedModelHistory {
    items: Vec<HistoryItem>,
    coordinates: protocol::ModelHistoryCoordinates,
}

fn load_model_history(
    source: protocol::ModelHistorySource,
    sessions_root: &std::path::Path,
    session_id: &str,
) -> Result<LoadedModelHistory, String> {
    let _perf = smelt_perf::perf::begin("engine:model_history:load");
    let coordinates = source.coordinates();
    let items = match source {
        protocol::ModelHistorySource::Items { items, .. } => {
            smelt_perf::perf::record_value("engine:model_history:source_items", 1);
            smelt_perf::perf::record_value("engine:model_history:items", items.len() as u64);
            items
        }
        protocol::ModelHistorySource::Store {
            prefix,
            lineage_id,
            first_live_index,
            end_index,
            suffix,
            ..
        } => {
            smelt_perf::perf::record_value("engine:model_history:source_store", 1);
            smelt_perf::perf::record_value(
                "engine:model_history:first_live_index",
                first_live_index as u64,
            );
            smelt_perf::perf::record_value("engine:model_history:end_index", end_index as u64);
            smelt_perf::perf::record_value(
                "engine:model_history:suffix_items",
                suffix.len() as u64,
            );
            let mut history = prefix;
            if end_index > first_live_index {
                let reader = {
                    let _perf = smelt_perf::perf::begin("engine:model_history:open_store");
                    smelt_store::LineageSessionReader::open_existing_in_lineage(
                        sessions_root,
                        lineage_id,
                        session_id,
                    )
                    .map_err(|err| format!("open model history lineage: {err}"))?
                };
                let mut rows = reader
                    .history_range(
                        u64::try_from(first_live_index)
                            .map_err(|_| "model history start index exceeds u64".to_string())?,
                        u64::try_from(end_index)
                            .map_err(|_| "model history end index exceeds u64".to_string())?,
                    )
                    .map_err(|err| format!("read lineage model history rows: {err}"))?;
                smelt_perf::perf::record_value("engine:model_history:rows_read", rows.len() as u64);
                history.append(&mut rows);
            }
            history.extend(suffix);
            smelt_perf::perf::record_value("engine:model_history:items", history.len() as u64);
            history
        }
    };
    Ok(LoadedModelHistory { items, coordinates })
}

/// One pending out-of-band LLM call. Mirrors the fields plumbed through
/// `UiCommand::EngineAsk`; bundles them so the spawn surface stays a
/// two-arg function (config + task) instead of an ever-growing arg list.
pub(crate) struct AskTask {
    pub id: u64,
    pub system: String,
    pub messages: Vec<protocol::Message>,
    pub target: ModelTarget,
    pub request_config: RequestRuntimeConfig,
    pub response_format: Option<protocol::AskResponseFormat>,
    pub reasoning_effort: ReasoningEffort,
    pub fast_mode: bool,
    /// Optional tool list. When non-empty AND matching the main session's
    /// tools byte-for-byte, the request reuses the main session's
    /// Anthropic prefix cache.
    pub tools: Vec<protocol::ToolDef>,
    /// Session id forwarded as `prompt_cache_key` to OpenAI / Codex so
    /// the EngineAsk hits the same cache shard as the main turn.
    pub session_id: String,
    pub persistence: protocol::PersistenceScope,
    /// Whether text deltas for this auxiliary request should be forwarded
    /// as `EngineAskDelta` events.
    pub stream: bool,
    /// Whether provider retries for this auxiliary request should update the
    /// visible work state.
    pub visible_retries: bool,
}

/// Immutable refs out-of-band command dispatch needs. Bundling them lets
/// every site that drains `cmd_rx` (the outer engine loop, the
/// turn-control loop, `call_llm`, `execute_concurrent`,
/// `wait_for_tool_result`) route background commands through a single
/// function - and lets `execute_concurrent` in particular do so without
/// colliding with its long-held `&mut self.cmd_rx` borrow in the select.
pub(crate) struct BackgroundCtx<'a> {
    pub config: &'a EngineConfig,
    pub http_client: &'a reqwest::Client,
    pub dispatcher: &'a dyn ToolDispatcher,
    pub event_tx: &'a crate::EngineEventSender,
    pub host_tx: &'a crate::HostCallSender,
    pub bg_cancel: CancellationToken,
}

enum ActiveTurnCommand {
    Cancel,
    HistoryAppend(protocol::HistoryAppend),
    Injection(UiCommand),
    ConfigUpdate(UiCommand),
    Other(UiCommand),
}

impl ActiveTurnCommand {
    fn classify(cmd: UiCommand) -> Self {
        match cmd {
            UiCommand::Cancel => Self::Cancel,
            UiCommand::AppendHistoryItem { append } => Self::HistoryAppend(append),
            cmd @ (UiCommand::Steer { .. } | UiCommand::Unsteer { .. }) => Self::Injection(cmd),
            cmd @ (UiCommand::SetReasoningEffort { .. }
            | UiCommand::SetFastMode { .. }
            | UiCommand::SetMode { .. }
            | UiCommand::SetTurnModel { .. }
            | UiCommand::UpdateAgentProjectContext(_)) => Self::ConfigUpdate(cmd),
            other => Self::Other(other),
        }
    }

    fn into_command(self) -> UiCommand {
        match self {
            Self::Cancel => UiCommand::Cancel,
            Self::HistoryAppend(append) => UiCommand::AppendHistoryItem { append },
            Self::Injection(cmd) | Self::ConfigUpdate(cmd) | Self::Other(cmd) => cmd,
        }
    }
}

/// Dispatch a command that doesn't depend on turn state. Returns `None`
/// when the command was consumed; `Some(cmd)` when the caller should
/// handle it (turn-control, per-tool protocol, lifecycle, etc.). Today
/// only `EngineAsk` is handled here; new "anywhere" commands extend the
/// match and instantly become available at every call site.
pub(crate) fn dispatch_background_cmd(
    cmd: UiCommand,
    ctx: &BackgroundCtx<'_>,
) -> Option<UiCommand> {
    match cmd {
        UiCommand::EngineAsk {
            id,
            system,
            messages,
            target,
            request_config,
            response_format,
            reasoning_effort,
            fast_mode,
            tools,
            session_id,
            persistence,
            stream,
            visible_retries,
        } => {
            spawn_engine_ask(
                ctx.config,
                ctx.http_client,
                ctx.dispatcher,
                AskTask {
                    id,
                    system,
                    messages,
                    target: *target,
                    request_config,
                    response_format,
                    reasoning_effort,
                    fast_mode,
                    tools,
                    session_id,
                    persistence,
                    stream,
                    visible_retries,
                },
                ctx.event_tx,
                ctx.host_tx,
                ctx.bg_cancel.clone(),
            );
            None
        }
        other => Some(other),
    }
}

fn spawn_engine_ask(
    config: &EngineConfig,
    client: &reqwest::Client,
    dispatcher: &dyn ToolDispatcher,
    task: AskTask,
    event_tx: &crate::EngineEventSender,
    host_tx: &crate::HostCallSender,
    cancel: CancellationToken,
) {
    let AskTask {
        id,
        system,
        messages: supplied_messages,
        target,
        request_config,
        response_format,
        reasoning_effort,
        fast_mode,
        tools: supplied_tools,
        session_id,
        persistence,
        stream,
        visible_retries,
    } = task;

    // Inherit-session is signalled by a non-empty supplied tool list
    // (the Lua side fills it with `app.lua.tool_defs(...)` only on that
    // path). When present, merge the engine's MCP defs in too so the
    // tools section matches the main turn byte-for-byte. Plain callers
    // (predict, title) pass an empty list and get an empty `tools`
    // field - sending MCP defs to them would waste tokens and break
    // their own cache prefix.
    let mut messages = supplied_messages;
    if request_config.redact_secrets {
        for message in &mut messages {
            if matches!(message.role, protocol::Role::User | protocol::Role::Tool) {
                if let Some(content) = &mut message.content {
                    crate::redact::redact_content(content);
                }
            }
        }
    }
    let tools = supplied_tools;
    let mcp_defs = if tools.is_empty() {
        Vec::new()
    } else {
        dispatcher.definitions()
    };
    let provider = build_provider(&target, client, std::sync::Arc::clone(&config.clock));
    let model_name = target.model.clone();
    let pricing_target = target.clone();
    let tx = event_tx.clone();
    let cache_ttl_long = request_config.cache_ttl_long;
    let audit_mode = request_config.request_audit;
    let audit_host_tx = host_tx.clone();
    tokio::spawn(async move {
        messages.insert(0, protocol::Message::system(&system));
        let ask_id = id;

        let mut opts = ChatOptions::new(&cancel);
        let mut request_opts = ChatRequestOptions {
            cache: provider.default_cache_config(cache_ttl_long, Some(&session_id)),
            fast_mode,
            ..ChatRequestOptions::default()
        };
        if let Some(fmt) = response_format {
            request_opts.response_format = Some(ResponseFormat {
                name: fmt.name,
                schema: fmt.schema,
            });
        }

        // Reuse the main-turn tool format (sorted by name) so an EngineAsk
        // that inherits the session's tool list produces a byte-identical
        // tools section and hits the same Anthropic prefix cache slot.
        let mut tool_defs: Vec<ToolDefinition> = tools
            .into_iter()
            .map(|t| {
                ToolDefinition::new(FunctionSchema {
                    name: t.name,
                    description: t.description,
                    parameters: t.parameters,
                })
            })
            .chain(mcp_defs)
            .collect();
        sort_tools_for_cache_stability(&mut tool_defs);

        let result = {
            let on_retry = |delay: std::time::Duration, attempt: u32| {
                let _ = tx.send(EngineEvent::Retrying {
                    delay_ms: delay.as_millis() as u64,
                    attempt,
                });
            };
            if visible_retries {
                opts.on_retry = Some(&on_retry);
            }
            let on_delta = |delta: ProviderStreamEvent| {
                if let ProviderStreamEvent::TextDelta(text) = delta {
                    let _ = tx.send(EngineEvent::EngineAskDelta {
                        id,
                        delta: text.to_string(),
                    });
                }
            };
            if stream {
                opts.on_delta = Some(&on_delta);
            }
            let log_target = pricing_target.clone();
            let on_attempt = move |info: RequestAttemptInfo<'_>| {
                let resolved = smelt_provider::resolve_pricing(
                    info.model,
                    &log_target.provider_type,
                    &log_target.api_base,
                    &log_target.config,
                );
                let ctx = crate::request_log::RequestContext {
                    request_id: ask_id,
                    kind: "engine_ask".to_string(),
                    turn_id: None,
                    ask_id: Some(ask_id),
                    history_len: None,
                    background: true,
                };
                dispatch_request_audit(
                    &audit_host_tx,
                    persistence,
                    ctx,
                    &info,
                    &resolved,
                    audit_mode,
                );
            };
            opts.on_attempt = Some(&on_attempt);
            provider
                .chat(
                    &messages,
                    &tool_defs,
                    &model_name,
                    reasoning_effort,
                    &request_opts,
                    &opts,
                )
                .await
        };

        match result {
            Ok(resp) => {
                send_usage(&tx, &pricing_target, resp.usage, None, true);
                let message = protocol::Message::assistant_with_reasoning(
                    resp.content.map(protocol::Content::text),
                    resp.reasoning_content,
                    resp.reasoning_details,
                    (!resp.tool_calls.is_empty()).then_some(resp.tool_calls),
                );
                let _ = tx.send(EngineEvent::EngineAskResponse {
                    id,
                    message: Some(message),
                    error: None,
                });
            }
            Err(e) => {
                let kind = classify_provider_error(&e);
                let message = e.to_string().replace('\n', " ");
                let _ = tx.send(EngineEvent::EngineAskResponse {
                    id,
                    message: None,
                    error: Some(EngineAskError { kind, message }),
                });
            }
        }
    });
}

/// Map a provider error to a stable `EngineAskErrorKind` for Lua callers.
fn classify_provider_error(e: &ProviderError) -> EngineAskErrorKind {
    match e {
        ProviderError::Cancelled => EngineAskErrorKind::Cancelled,
        ProviderError::QuotaExceeded { .. } => EngineAskErrorKind::Quota,
        ProviderError::CyberPolicy { .. } => EngineAskErrorKind::CyberPolicy,
        ProviderError::Network(_) | ProviderError::Stream(_) => EngineAskErrorKind::Network,
        ProviderError::RateLimited { .. } => EngineAskErrorKind::RateLimited,
        ProviderError::Server { .. } => {
            if is_context_window_error(e) {
                EngineAskErrorKind::ContextWindow
            } else {
                EngineAskErrorKind::Network
            }
        }
        ProviderError::InvalidResponse(_) => {
            if is_context_window_error(e) {
                EngineAskErrorKind::ContextWindow
            } else {
                EngineAskErrorKind::InvalidResponse
            }
        }
        ProviderError::Auth(_) | ProviderError::NotFound(_) | ProviderError::MaxRetries => {
            EngineAskErrorKind::Other
        }
    }
}

fn provider_retry_at_ms(e: &ProviderError) -> Option<u64> {
    match e {
        ProviderError::RateLimited { resets_at } => {
            resets_at.map(|epoch| epoch.saturating_mul(1000))
        }
        ProviderError::QuotaExceeded { resets_at, .. } => {
            resets_at.map(|epoch| epoch.saturating_mul(1000))
        }
        _ => None,
    }
}

/// True when the error indicates the model's context window was exceeded.
/// Mirrors the body-substring check previously living in `compact.rs`.
fn is_context_window_error(e: &ProviderError) -> bool {
    let body = match e {
        ProviderError::InvalidResponse(b) => b.as_str(),
        ProviderError::Server { body, .. } => body.as_str(),
        _ => return false,
    };
    let lower = body.to_ascii_lowercase();
    lower.contains("context_length_exceeded")
        || lower.contains("context length")
        || lower.contains("context window")
        || lower.contains("maximum context")
        || lower.contains("prompt is too long")
        || lower.contains("prompt too long")
        || lower.contains("too many tokens")
        || lower.contains("model token limit")
}

fn build_provider(
    target: &ModelTarget,
    client: &reqwest::Client,
    clock: std::sync::Arc<dyn crate::clock::Clock>,
) -> EngineProvider {
    EngineProvider::new(
        target.api_base.clone(),
        target.api_key.clone(),
        &target.provider_type,
        client.clone(),
        clock,
    )
    .with_model_config(target.config.clone())
}

// ── Turn ────────────────────────────────────────────────────────────────────

struct ClassifiedToolCall<'a> {
    invocation_id: protocol::InvocationId,
    tc: &'a protocol::ToolCall,
    args: HashMap<String, Value>,
    start: Instant,
    called_at_ms: u64,
}

struct ToolSlot {
    call_index: usize,
    confirm_msg: Option<String>,
}

struct ToolExecutionPlan<'a> {
    calls: Vec<ClassifiedToolCall<'a>>,
    slots: Vec<ToolSlot>,
    ready: Vec<usize>,
    pending_perms: Vec<(usize, u64)>,
    pending_tools: Vec<(u64, usize)>,
    sequential_tools: Vec<usize>,
    pending_tool_hooks: Vec<(u64, PendingToolCall)>,
    pending_tool_perms: Vec<(u64, PendingToolCall)>,
    /// Synthetic outcomes produced inside `classify_tools` itself - unknown
    /// tools, denied dispatch decisions. Folded into the assistant step's
    /// `invocations` at commit time so the invariant ("every dispatched
    /// tool_call has a paired outcome") holds even when no execution
    /// happens for that call.
    inline_outcomes: Vec<(usize, ToolOutcome)>,
}

/// Fold every produced outcome into a `Vec<ToolInvocation>` in the order
/// the LLM emitted the calls. Any call without a recorded outcome gets a
/// synthetic `interrupted` outcome - that case should be unreachable in
/// production (all execution paths produce an outcome), but the safety net
/// makes the on-disk + on-wire invariant true *by construction*, not by
/// careful code review.
///
/// Precedence on classified-call collision: `slot` > `plugin` > `inline`.
/// The classify-then-execute pipeline routes each call to exactly one path,
/// so collisions shouldn't happen in practice - but if a path bug starts
/// double-writing, the explicit precedence keeps the result deterministic.
fn pair_invocations_in_order(
    calls: &[ClassifiedToolCall<'_>],
    slot_outcomes: Vec<(usize, ToolOutcome, Option<u64>)>,
    plugin_outcomes: Vec<(usize, ToolOutcome, Option<u64>)>,
    inline_outcomes: Vec<(usize, ToolOutcome)>,
) -> Vec<ToolInvocation> {
    let mut by_index: Vec<Option<(ToolOutcome, Option<u64>)>> =
        (0..calls.len()).map(|_| None).collect();
    for (index, outcome, elapsed_ms) in slot_outcomes {
        by_index[index] = Some((outcome, elapsed_ms));
    }
    for (index, outcome, elapsed_ms) in plugin_outcomes {
        if by_index[index].is_none() {
            by_index[index] = Some((outcome, elapsed_ms));
        }
    }
    for (index, outcome) in inline_outcomes {
        if by_index[index].is_none() {
            by_index[index] = Some((outcome, None));
        }
    }
    calls
        .iter()
        .zip(by_index)
        .map(|(call, outcome)| {
            let tc = call.tc;
            let (result, elapsed_ms) = outcome.unwrap_or_else(|| {
                // Safety net: should be unreachable because every execution
                // path (slot, plugin, sequential, inline-classify) writes
                // an outcome before pair_invocations_in_order runs. If it
                // fires, our reasoning was wrong - log so we can find out.
                crate::log::entry(
                    crate::log::Level::Warn,
                    "agent_invocation_missing",
                    &serde_json::json!({
                        "call_id": tc.id,
                        "tool": tc.function.name,
                    }),
                );
                (
                    ToolOutcome::new(
                        "interrupted: tool result missing at commit time".into(),
                        true,
                        None,
                    ),
                    None,
                )
            });
            ToolInvocation {
                call_id: tc.id.clone(),
                name: tc.function.name.clone(),
                arguments: tc.function.arguments.clone(),
                result,
                elapsed_ms,
                called_at_ms: Some(call.called_at_ms),
            }
        })
        .collect()
}

/// Log a warning when a host hook returns a `Vec<Message>` with assistant
/// `tool_calls` whose ids aren't satisfied by following `Role::Tool`
/// messages. The orphans are auto-repaired by `history_from_messages` -
/// this surface makes the repair visible so a misbehaving plugin can be
/// found instead of silently fixed.
fn warn_if_replacement_has_orphans(replacement: &[Message], site: &str) {
    use protocol::Role;
    let mut orphans: Vec<String> = Vec::new();
    let mut i = 0;
    while i < replacement.len() {
        if matches!(replacement[i].role, Role::Assistant) {
            if let Some(ref calls) = replacement[i].tool_calls {
                let mut satisfied: std::collections::HashSet<&str> =
                    std::collections::HashSet::new();
                let mut j = i + 1;
                while j < replacement.len() && matches!(replacement[j].role, Role::Tool) {
                    if let Some(ref id) = replacement[j].tool_call_id {
                        satisfied.insert(id.as_str());
                    }
                    j += 1;
                }
                for call in calls {
                    if !satisfied.contains(call.id.as_str()) {
                        orphans.push(call.id.clone());
                    }
                }
                i = j;
                continue;
            }
        }
        i += 1;
    }
    if !orphans.is_empty() {
        crate::log::entry(
            crate::log::Level::Warn,
            "host_hook_orphan_tool_calls",
            &serde_json::json!({
                "site": site,
                "orphan_call_ids": orphans,
                "note": "auto-repaired with synthetic 'interrupted' results",
            }),
        );
    }
}

fn estimate_prompt_tokens(
    system_prompt: &str,
    messages: &[Message],
    tool_defs: &[ToolDefinition],
) -> u32 {
    let _perf = smelt_perf::perf::begin("engine:request:estimate_prompt_tokens");
    let bytes = system_prompt
        .len()
        .saturating_add(crate::request_log::serialized_json_len(&messages))
        .saturating_add(crate::request_log::serialized_json_len(&tool_defs));
    let tokens = bytes.div_ceil(4);
    tokens.min(u32::MAX as usize) as u32
}

struct PendingToolCall {
    call_index: usize,
    is_sequential: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct ReplayedToolCall {
    call_id: String,
}

impl ReplayedToolCall {
    fn reason(&self) -> &'static str {
        "replayed_tool_call"
    }

    fn message(&self) -> String {
        let call_id = &self.call_id;
        format!(
            "Provider repeated tool call ID `{call_id}`. Stopped to prevent duplicate execution."
        )
    }
}

#[derive(Default)]
struct ToolReplayGuard {
    completed_call_ids: HashSet<String>,
}

impl ToolReplayGuard {
    fn begin_round(&mut self, calls: &[protocol::ToolCall]) -> Result<(), ReplayedToolCall> {
        let mut round_call_ids = HashSet::with_capacity(calls.len());
        for call in calls {
            if self.completed_call_ids.contains(&call.id)
                || !round_call_ids.insert(call.id.as_str())
            {
                return Err(ReplayedToolCall {
                    call_id: call.id.clone(),
                });
            }
        }

        Ok(())
    }

    fn finish_round(&mut self, calls: &[protocol::ToolCall]) {
        self.completed_call_ids
            .extend(calls.iter().map(|call| call.id.clone()));
    }
}

struct Turn<'a> {
    provider: EngineProvider,
    dispatcher: &'a dyn ToolDispatcher,
    cmd_rx: &'a mut mpsc::UnboundedReceiver<UiCommand>,
    event_tx: &'a crate::EngineEventSender,
    /// Host-callback sender for provider middleware and request preparation.
    host_tx: &'a crate::HostCallSender,
    config: &'a mut EngineConfig,
    http_client: &'a reqwest::Client,
    cancel: CancellationToken,
    bg_cancel: CancellationToken,
    /// Committed conversation history. Invariant: every `Assistant` step
    /// either has no `invocations` (terminal) or has a `ToolOutcome` paired
    /// with every `ToolCall` the LLM emitted. There is no in-flight tool
    /// state here - that lives on stack-locals during the run loop's
    /// dispatch phase and is folded into a single `HistoryItem::Assistant`
    /// at commit time.
    history: Vec<HistoryItem>,
    history_coordinates: protocol::ModelHistoryCoordinates,
    mode: AgentMode,
    reasoning_effort: ReasoningEffort,
    fast_mode: bool,
    turn_id: u64,
    model_target: ModelTarget,
    target_revision: u64,
    request_config: RequestRuntimeConfig,
    system_prompt: String,
    tools: Vec<protocol::ToolDef>,
    permission_overrides: Option<protocol::PermissionOverrides>,
    pending_history_items: Vec<HistoryItem>,
    next_history_changed_from: usize,
    history_revision: u64,
    /// Stable per-session identifier sent as OpenAI's `prompt_cache_key`
    /// to anchor cache routing across all turns in this session.
    session_id: String,
    persistence: protocol::PersistenceScope,
    started_at: Instant,
    tps_samples: Vec<f64>,
}

enum HostCallResult<T> {
    Replied(T, Vec<UiCommand>),
    Cancelled,
    Dropped(Vec<UiCommand>),
}

struct PreparedRequest {
    messages: crate::host::PreparedRequestMessages,
    history_revision: u64,
}

enum PrepareRequestOutcome {
    Continue(PreparedRequest),
    Restart,
    Abort(String),
    Cancelled,
}

impl<'a> Turn<'a> {
    fn emit(&self, event: EngineEvent) {
        let _ = self.event_tx.send(event);
    }

    fn public_history_len(&self) -> usize {
        self.history
            .len()
            .saturating_sub(self.system_history_offset())
    }

    fn system_history_offset(&self) -> usize {
        self.history
            .first()
            .is_some_and(|item| matches!(item, HistoryItem::System { .. })) as usize
    }

    fn raw_history_index(&self, public_idx: usize) -> usize {
        public_idx.saturating_add(self.system_history_offset())
    }

    fn mark_history_changed_from(&mut self, public_idx: usize) {
        self.next_history_changed_from = self.next_history_changed_from.min(public_idx);
        self.history_revision = self.history_revision.wrapping_add(1);
    }

    fn mark_append_history_changed(&mut self) {
        self.mark_history_changed_from(self.public_history_len());
    }

    /// Drain `deferred` back through `handle_turn_cmd`.
    fn apply_deferred_turn_cmds(&mut self, deferred: Vec<UiCommand>) {
        for cmd in deferred {
            self.handle_turn_cmd(cmd);
        }
    }

    /// Fire a `HostCall` and await its `oneshot::Sender<Reply>`.
    ///
    /// Commands received while waiting are handled immediately except for
    /// `Steer`/`Unsteer` injections. The caller applies those only after
    /// installing any replacement history and deciding to continue the turn.
    async fn host_call<Reply, F>(&mut self, build: F) -> HostCallResult<Reply>
    where
        F: FnOnce(u64, tokio::sync::oneshot::Sender<Reply>) -> crate::host::HostCall,
    {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self.host_tx.send(build(self.turn_id, tx)).is_err() {
            return HostCallResult::Dropped(Vec::new());
        }
        tokio::pin!(rx);
        let mut deferred: Vec<UiCommand> = Vec::new();
        loop {
            tokio::select! {
                res = &mut rx => {
                    return match res {
                        Ok(reply) => HostCallResult::Replied(reply, deferred),
                        Err(_) => HostCallResult::Dropped(deferred),
                    };
                }
                Some(cmd) = self.cmd_rx.recv() => {
                    match ActiveTurnCommand::classify(cmd) {
                        ActiveTurnCommand::Injection(cmd) => deferred.push(cmd),
                        other => {
                            self.handle_turn_cmd(other.into_command());
                        }
                    }
                    if self.cancel.is_cancelled() {
                        return HostCallResult::Cancelled;
                    }
                }
                else => return HostCallResult::Dropped(deferred),
            }
        }
    }

    /// Run `smelt.provider.middleware{on_response=...}` hooks against
    /// the assembled assistant `Message`. Returns the replacement when
    /// any hook produced one; otherwise the original.
    async fn apply_response_hooks(&mut self, message: Message) -> Message {
        match self
            .host_call(|turn_id, reply| crate::host::HostCall::ProviderResponse {
                turn_id,
                message: message.clone(),
                reply,
            })
            .await
        {
            HostCallResult::Replied(replacement, deferred) => {
                self.apply_deferred_turn_cmds(deferred);
                replacement.unwrap_or(message)
            }
            HostCallResult::Dropped(deferred) => {
                self.apply_deferred_turn_cmds(deferred);
                message
            }
            HostCallResult::Cancelled => message,
        }
    }

    /// Milliseconds since `start` measured against the injected clock.
    fn elapsed_ms_since(&self, start: Instant) -> u64 {
        self.config
            .clock
            .instant_now()
            .duration_since(start)
            .as_millis() as u64
    }

    /// Append current-turn content that may be a synthetic internal note.
    fn push_turn_content(&mut self, mut content: Content, display: Option<String>, command: bool) {
        let display = if self.request_config.redact_secrets {
            crate::redact::redact_content(&mut content);
            display.map(|text| crate::redact::redact(&text))
        } else {
            display
        };
        let mut item = protocol::history_item_from_user_content(content);
        if let HistoryItem::User {
            display: slot,
            command: is_command,
            ..
        } = &mut item
        {
            *slot = display;
            *is_command = command;
        }
        self.mark_append_history_changed();
        self.history.push(item);
    }

    /// Append an assistant step atomically. When `invocations` is non-empty,
    /// every entry already carries its `ToolOutcome` - the only way to
    /// satisfy `AssistantStep`'s shape - so the on-disk and on-wire
    /// representations can never carry an orphan tool_use.
    fn push_assistant_step(&mut self, mut step: AssistantStep) {
        if self.request_config.redact_secrets {
            for inv in &mut step.invocations {
                let redacted = crate::redact::redact(&inv.result.content);
                if redacted != inv.result.content {
                    inv.result.content = redacted;
                }
            }
        }
        self.mark_append_history_changed();
        self.history.push(HistoryItem::Assistant(step));
    }

    fn install_system_prompt(&mut self, system_prompt: String) {
        self.system_prompt = system_prompt;
        let replacement = HistoryItem::system(&self.system_prompt);
        let changed = self.history.first_mut().is_some_and(|first| {
            if matches!(first, HistoryItem::System { .. }) && first != &replacement {
                *first = replacement;
                true
            } else {
                false
            }
        });
        if changed {
            self.history_revision = self.history_revision.wrapping_add(1);
        }
    }

    fn install_project_context(&mut self, mut context: Box<protocol::AgentProjectContext>) {
        self.config.install_project_context(&context);
        self.tools = std::mem::take(&mut context.tools);
        self.install_system_prompt(std::mem::take(&mut context.system_prompt));
    }

    fn emit_history_appended_from(&mut self, first_index: usize) {
        let start = self.raw_history_index(first_index).min(self.history.len());
        let items = self.history[start..].to_vec();
        if items.is_empty() {
            return;
        }
        self.next_history_changed_from = self.public_history_len();
        self.emit(EngineEvent::HistoryAppended {
            turn_id: self.turn_id,
            delta: protocol::CanonicalHistoryDelta {
                first_index: self
                    .history_coordinates
                    .canonical_index(protocol::ModelHistoryIndex::new(first_index)),
                items,
            },
        });
    }

    fn replace_model_history(
        &mut self,
        history: Vec<HistoryItem>,
        coordinates: protocol::ModelHistoryCoordinates,
    ) {
        self.mark_history_changed_from(0);
        self.history_coordinates = coordinates;
        self.history.truncate(1);
        self.history.extend(history);
        self.emit_messages_snapshot();
    }

    /// Emit the canonical suffix affected by model-visible history changes.
    /// Synthetic checkpoint items are omitted from the payload.
    fn emit_messages_snapshot(&mut self) {
        let _perf = smelt_perf::perf::begin("engine:history:emit_snapshot");
        let public_len = self
            .history
            .iter()
            .filter(|item| !matches!(item, HistoryItem::System { .. }))
            .count();
        let first_changed_index = self
            .next_history_changed_from
            .max(self.history_coordinates.model_prefix_len())
            .min(public_len);
        let items = self
            .history
            .iter()
            .filter(|item| !matches!(item, HistoryItem::System { .. }))
            .skip(first_changed_index)
            .cloned()
            .collect();
        self.next_history_changed_from = public_len;
        self.emit(EngineEvent::HistoryUpdated {
            turn_id: self.turn_id,
            update: protocol::CanonicalHistoryDelta {
                first_index: self
                    .history_coordinates
                    .canonical_index(protocol::ModelHistoryIndex::new(first_changed_index)),
                items,
            },
        });
    }

    fn queue_history_item(&mut self, append: protocol::HistoryAppend) {
        if queue_context_history_item(&self.history, &mut self.pending_history_items, &append) {
            return;
        }

        let coalescing_note_kind = append.coalescing_note_kind();
        if coalescing_note_kind == Some(protocol::HistoryNoteKind::ModeChange) {
            if let Some(idx) = self
                .pending_history_items
                .iter()
                .position(|item| item.note_kind() == Some(protocol::HistoryNoteKind::ModeChange))
            {
                if mode_append_returns_to_base(&append) {
                    self.pending_history_items.remove(idx);
                } else {
                    self.pending_history_items[idx] = append.item;
                }
                return;
            }
            if last_note_kind(&self.history) == Some(protocol::HistoryNoteKind::ModeChange) {
                let changed_from = self.public_history_len().saturating_sub(1);
                let result = protocol::apply_history_append(&mut self.history, &append);
                if result != protocol::HistoryAppendResult::Unchanged {
                    self.mark_history_changed_from(changed_from);
                    self.emit_messages_snapshot();
                }
                return;
            }
        } else if let Some(kind) = coalescing_note_kind {
            if protocol::replace_last_note_kind(&mut self.pending_history_items, &append.item, kind)
            {
                return;
            }
            if protocol::replace_last_note_kind(&mut self.history, &append.item, kind) {
                self.mark_history_changed_from(0);
                self.emit_messages_snapshot();
                return;
            }
        }

        protocol::apply_history_append(&mut self.pending_history_items, &append);
    }

    fn apply_pending_history_items_for_request(&mut self) {
        if self.pending_history_items.is_empty() {
            return;
        }
        let items = std::mem::take(&mut self.pending_history_items);
        let first_index = self.public_history_len();
        self.mark_append_history_changed();
        self.history.extend(items);
        self.emit_history_appended_from(first_index);
    }

    /// Commit a streamed-but-cancelled assistant message. The model never
    /// asked for any tools, so this is a terminal step - `invocations` is
    /// empty by construction.
    fn commit_partial_assistant(&mut self, text: String, reasoning: String) -> Option<usize> {
        let content = if text.trim().is_empty() {
            None
        } else {
            Some(Content::text(text))
        };
        let reasoning = if reasoning.trim().is_empty() {
            None
        } else {
            Some(reasoning)
        };
        if content.is_some() || reasoning.is_some() {
            let first_index = self.public_history_len();
            self.push_assistant_step(AssistantStep::terminal(content, reasoning, Vec::new()));
            Some(first_index)
        } else {
            None
        }
    }

    fn emit_turn_complete(&mut self, interrupted: bool) {
        let meta = self.build_meta(interrupted);
        let history = if interrupted {
            let items: Vec<HistoryItem> = std::mem::take(&mut self.history)
                .into_iter()
                .filter(|i| !matches!(i, HistoryItem::System { .. }))
                .collect();
            let first_changed_index = self.next_history_changed_from.min(items.len());
            self.next_history_changed_from = items.len();
            Some(
                self.history_coordinates
                    .canonical_delta(protocol::ModelHistoryIndex::new(first_changed_index), items),
            )
        } else {
            self.history.clear();
            None
        };
        self.emit(EngineEvent::TurnComplete {
            turn_id: self.turn_id,
            history,
            meta: Some(meta),
        });
    }

    fn build_meta(&self, interrupted: bool) -> TurnMeta {
        let avg_tps = if self.tps_samples.is_empty() {
            None
        } else {
            let sum: f64 = self.tps_samples.iter().sum();
            Some(sum / self.tps_samples.len() as f64)
        };
        let elapsed = self
            .config
            .clock
            .instant_now()
            .duration_since(self.started_at);
        TurnMeta {
            elapsed_ms: elapsed.as_millis() as u64,
            avg_tps,
            display_tps: None,
            interrupted,
        }
    }

    fn apply_model_change(&mut self, target: ModelTarget, system_prompt: String) {
        self.provider = build_provider(
            &target,
            self.http_client,
            std::sync::Arc::clone(&self.config.clock),
        );
        self.model_target = target;
        self.target_revision = self.target_revision.wrapping_add(1);
        self.install_system_prompt(system_prompt);
    }

    async fn prepare_request_with_host(
        &mut self,
        tool_defs: &[ToolDefinition],
    ) -> PrepareRequestOutcome {
        let _perf = smelt_perf::perf::begin("engine:request:prepare_with_host");
        let messages = {
            let _perf = smelt_perf::perf::begin("engine:request:prepare_messages");
            crate::host::PreparedRequestMessages::new(
                protocol::history_to_messages(&self.history),
                self.system_history_offset(),
            )
        };
        let history_revision = self.history_revision;
        let estimated_tokens =
            estimate_prompt_tokens(&self.system_prompt, messages.model(), tool_defs);
        let host_messages = messages.clone();
        let decision = self
            .host_call(|turn_id, reply| crate::host::HostCall::PrepareRequest {
                turn_id,
                messages: host_messages,
                estimated_tokens,
                reply,
            })
            .await;
        let continue_request = || {
            PrepareRequestOutcome::Continue(PreparedRequest {
                messages,
                history_revision,
            })
        };
        let (replacement, coordinates, deferred) = match decision {
            HostCallResult::Replied(crate::host::HostRequestDecision::Continue, deferred)
            | HostCallResult::Dropped(deferred) => {
                self.apply_deferred_turn_cmds(deferred);
                return continue_request();
            }
            HostCallResult::Cancelled
            | HostCallResult::Replied(crate::host::HostRequestDecision::Stop, _) => {
                return PrepareRequestOutcome::Cancelled;
            }
            HostCallResult::Replied(crate::host::HostRequestDecision::Abort(message), _) => {
                return PrepareRequestOutcome::Abort(message);
            }
            HostCallResult::Replied(
                crate::host::HostRequestDecision::Replace {
                    messages,
                    coordinates,
                },
                deferred,
            ) => (messages, coordinates, deferred),
        };
        if replacement.is_empty() {
            log::entry(
                log::Level::Warn,
                "prepare_request_empty_replacement",
                &serde_json::json!({}),
            );
            self.apply_deferred_turn_cmds(deferred);
            return continue_request();
        }
        warn_if_replacement_has_orphans(&replacement, "prepare_request");
        let new = protocol::history_from_messages(replacement);
        log::entry(
            log::Level::Info,
            "prepare_request_replaced",
            &serde_json::json!({
                "estimated_tokens_before": estimated_tokens,
                "new_message_count": new.len() + 1,
            }),
        );
        self.replace_model_history(new, coordinates);
        self.apply_deferred_turn_cmds(deferred);
        PrepareRequestOutcome::Restart
    }

    /// Bundle of refs that out-of-band command dispatch needs. Built on
    /// demand so callsites that already hold a `&mut` borrow on
    /// `self.cmd_rx` (notably `execute_concurrent`) can bypass the
    /// borrow checker by pre-binding the ctx outside the select loop.
    fn bg_ctx(&self) -> BackgroundCtx<'_> {
        BackgroundCtx {
            config: self.config,
            http_client: self.http_client,
            dispatcher: self.dispatcher,
            event_tx: self.event_tx,
            host_tx: self.host_tx,
            bg_cancel: self.bg_cancel.clone(),
        }
    }

    fn apply_reasoning_effort(&mut self, effort: ReasoningEffort) {
        // Kimi's Anthropic-compatible endpoint rejects requests when reasoning
        // is toggled mid-turn because earlier assistant tool-call messages lack
        // the reasoning_content the backend now expects. The UI/session state
        // already records the new choice; keep this turn on its starting effort.
        if self.provider.supports_mid_turn_reasoning_changes() {
            self.reasoning_effort = effort;
        }
    }

    fn handle_turn_cmd(&mut self, cmd: UiCommand) -> bool {
        match cmd {
            UiCommand::Steer { input } => {
                self.emit(EngineEvent::Steered {
                    text: input.provider_content().text_content().into_owned(),
                    count: 1,
                });
                let first_index = self.public_history_len();
                self.push_current_turn_input(input);
                self.emit_history_appended_from(first_index);
                true
            }
            UiCommand::Unsteer { count } => {
                for _ in 0..count {
                    if let Some(pos) = self
                        .history
                        .iter()
                        .rposition(|i| matches!(i, HistoryItem::User { .. }))
                    {
                        self.mark_history_changed_from(pos.saturating_sub(1));
                        self.history.remove(pos);
                    }
                }
                self.emit_messages_snapshot();
                true
            }
            UiCommand::AppendHistoryItem { append } => {
                self.queue_history_item(append);
                true
            }
            UiCommand::SetReasoningEffort { effort } => {
                self.apply_reasoning_effort(effort);
                true
            }
            UiCommand::SetFastMode { enabled } => {
                self.fast_mode = enabled;
                true
            }
            UiCommand::SetMode { mode } => {
                self.mode = mode;
                true
            }
            UiCommand::SetTurnModel {
                target,
                system_prompt,
            } => {
                self.apply_model_change(*target, system_prompt);
                true
            }
            UiCommand::UpdateAgentProjectContext(context) => {
                self.install_project_context(context);
                true
            }
            UiCommand::Cancel => {
                self.cancel.cancel();
                self.bg_cancel.cancel();
                true
            }
            other => dispatch_background_cmd(other, &self.bg_ctx()).is_none(),
        }
    }

    fn push_current_turn_input(&mut self, input: protocol::StartTurnInput) {
        match input {
            protocol::StartTurnInput::User {
                content,
                display,
                command,
            } if !content.is_empty() => {
                self.push_turn_content(content, display, command);
            }
            protocol::StartTurnInput::Note { note } => {
                self.mark_append_history_changed();
                self.history.push(HistoryItem::note(note));
            }
            protocol::StartTurnInput::User { .. } => {}
        }
    }

    async fn run(&mut self, input: protocol::StartTurnInput, history: Vec<HistoryItem>) {
        self.provider.reset_turn_state();
        {
            let _perf = smelt_perf::perf::begin("engine:turn:install_history");
            self.history = Vec::with_capacity(history.len() + 2);
            self.history.push(HistoryItem::system(&self.system_prompt));
            self.history.extend(history);
            self.next_history_changed_from = self.public_history_len();
            self.push_current_turn_input(input);
        }
        self.emit_messages_snapshot();

        let mut first = true;
        let mut empty_retries: u8 = 0;
        let mut tool_replay_guard = ToolReplayGuard::default();
        const MAX_EMPTY_RETRIES: u8 = 2;

        loop {
            if !first {
                self.drain_commands();
            }
            first = false;

            let request_target_revision = self.target_revision;

            // Sorted by name so the request prefix stays byte-identical
            // across turns. Anything that reorders tools busts the cache.
            let tool_defs: Vec<ToolDefinition> = if self.provider.tool_calling() {
                let mut defs: Vec<ToolDefinition> = self
                    .dispatcher
                    .definitions()
                    .into_iter()
                    .filter(|d| {
                        self.dispatcher.is_visible(
                            self.turn_id,
                            d.function.name.as_str(),
                            self.mode.clone(),
                        )
                    })
                    .collect();
                // Plugin tools with `override_core` shadow the same-named core tool.
                let overridden: std::collections::HashSet<&str> = self
                    .tools
                    .iter()
                    .filter(|pt| pt.override_core)
                    .map(|pt| pt.name.as_str())
                    .collect();
                if !overridden.is_empty() {
                    defs.retain(|d| !overridden.contains(d.function.name.as_str()));
                }
                for pt in &self.tools {
                    if let Some(ref modes) = pt.modes {
                        if !modes.contains(&self.mode) {
                            continue;
                        }
                    }
                    defs.push(ToolDefinition::new(FunctionSchema {
                        name: pt.name.clone(),
                        description: pt.description.clone(),
                        parameters: pt.parameters.clone(),
                    }));
                }
                sort_tools_for_cache_stability(&mut defs);
                defs
            } else {
                Vec::new()
            };

            if self.cancel.is_cancelled() {
                self.emit_turn_complete(true);
                return;
            }

            self.apply_pending_history_items_for_request();

            let prepared = match self.prepare_request_with_host(&tool_defs).await {
                PrepareRequestOutcome::Restart => continue,
                PrepareRequestOutcome::Continue(prepared) => prepared,
                PrepareRequestOutcome::Cancelled => {
                    self.emit_turn_complete(true);
                    return;
                }
                PrepareRequestOutcome::Abort(message) => {
                    self.emit(EngineEvent::TurnError {
                        message,
                        kind: None,
                        retry_at_ms: None,
                    });
                    self.emit_turn_complete(false);
                    return;
                }
            };
            self.drain_commands();
            if self.target_revision != request_target_revision {
                continue;
            }
            if self.cancel.is_cancelled() {
                self.emit_turn_complete(true);
                return;
            }
            let prepared_messages =
                (self.history_revision == prepared.history_revision).then_some(prepared.messages);

            let LlmCallOutcome {
                result,
                partial_text,
                partial_reasoning,
                deferred_turn_cmds: mut provider_turn_cmds,
            } = self.call_llm(&tool_defs, prepared_messages).await;
            let resp = match result {
                Ok(r) => r,
                Err(ProviderError::Cancelled) => {
                    if let Some(first_index) =
                        self.commit_partial_assistant(partial_text, partial_reasoning)
                    {
                        self.emit_history_appended_from(first_index);
                    }
                    self.emit_turn_complete(true);
                    return;
                }
                Err(ProviderError::QuotaExceeded { body, resets_at }) => {
                    log::entry(
                        log::Level::Warn,
                        "agent_stop",
                        &serde_json::json!({"reason": "quota_exceeded", "error": body}),
                    );
                    self.emit(EngineEvent::TurnError {
                        message: quota_exceeded_message().to_string(),
                        kind: Some(EngineAskErrorKind::Quota),
                        retry_at_ms: resets_at.map(|epoch| epoch.saturating_mul(1000)),
                    });
                    self.emit_turn_complete(false);
                    return;
                }
                Err(e) => {
                    let error_msg = e.to_string().replace('\n', " ");
                    let is_ctx = is_context_window_error(&e);
                    log::entry(
                        log::Level::Warn,
                        if is_ctx {
                            "context_limit_reached"
                        } else {
                            "agent_stop"
                        },
                        &serde_json::json!({"reason": "llm_error", "error": error_msg.clone()}),
                    );
                    if is_ctx {
                        // Ask the host's recovery hook for a shorter
                        // conversation. On success we swap history
                        // (preserving the system prompt at index 0)
                        // and re-enter the loop transparently. The view
                        // sent to the host is the wire-shape; the
                        // returned `Vec<Message>` is folded back into
                        // `HistoryItem`s, which repairs any orphan
                        // tool_use the compaction plugin might emit.
                        let recovery_view: Vec<Message> = protocol::history_to_messages(
                            &self.history.iter().skip(1).cloned().collect::<Vec<_>>(),
                        );
                        let recovery_decision = self
                            .host_call(|turn_id, reply| {
                                crate::host::HostCall::RecoverFromContextLimit {
                                    turn_id,
                                    messages: recovery_view,
                                    reply,
                                }
                            })
                            .await;
                        match recovery_decision {
                            HostCallResult::Cancelled
                            | HostCallResult::Replied(crate::host::HostRequestDecision::Stop, _) => {
                                self.emit_turn_complete(true);
                                return;
                            }
                            HostCallResult::Replied(
                                crate::host::HostRequestDecision::Abort(message),
                                _,
                            ) => {
                                self.emit(EngineEvent::TurnError {
                                    message,
                                    kind: None,
                                    retry_at_ms: None,
                                });
                                self.emit_turn_complete(false);
                                return;
                            }
                            HostCallResult::Replied(
                                crate::host::HostRequestDecision::Replace {
                                    messages: shorter,
                                    coordinates,
                                },
                                deferred,
                            ) => {
                                warn_if_replacement_has_orphans(&shorter, "context_limit_recovery");
                                let new = protocol::history_from_messages(shorter);
                                log::entry(
                                    log::Level::Info,
                                    "context_limit_recovered",
                                    &serde_json::json!({"new_message_count": new.len() + 1}),
                                );
                                self.replace_model_history(new, coordinates);
                                provider_turn_cmds.extend(deferred);
                                self.apply_deferred_turn_cmds(provider_turn_cmds);
                                continue;
                            }
                            HostCallResult::Replied(
                                crate::host::HostRequestDecision::Continue,
                                _,
                            )
                            | HostCallResult::Dropped(_) => {}
                        }
                    }
                    let message = if is_ctx {
                        "Context limit reached. Run /compact and retry.".to_string()
                    } else {
                        error_msg
                    };
                    self.emit(EngineEvent::TurnError {
                        message,
                        kind: Some(classify_provider_error(&e)),
                        retry_at_ms: provider_retry_at_ms(&e),
                    });
                    self.emit_turn_complete(false);
                    return;
                }
            };

            if let Some(tps) = resp.tokens_per_sec {
                self.tps_samples.push(tps);
            }
            if resp.usage.has_any() {
                send_usage(
                    self.event_tx,
                    &self.model_target,
                    resp.usage,
                    resp.tokens_per_sec,
                    false,
                );
            }

            let content = resp.content.map(Content::text);
            let tool_calls = resp.tool_calls;
            let reasoning = resp.reasoning_content;
            let reasoning_parts = resp.reasoning_parts;
            let reasoning_details = resp.reasoning_details;

            let had_injected = provider_turn_cmds
                .iter()
                .any(|cmd| matches!(cmd, UiCommand::Steer { .. }));
            if tool_calls.is_empty() {
                self.apply_deferred_turn_cmds(std::mem::take(&mut provider_turn_cmds));
                // An injected message arrived during the LLM call. Discard this
                // response and loop so the model can respond to the new input.
                if had_injected {
                    continue;
                }
            }

            // Streaming already delivered deltas; only emit batch events for non-streaming.
            if partial_text.is_empty() && partial_reasoning.is_empty() {
                if reasoning_parts.is_empty() {
                    if let Some(ref reasoning) = reasoning {
                        let trimmed = smelt_buffer::text::trim_whitespace(reasoning);
                        if !trimmed.is_empty() {
                            self.emit(EngineEvent::Reasoning {
                                kind: ReasoningKind::Raw,
                                title: None,
                                content: trimmed.to_string(),
                            });
                        }
                    }
                } else {
                    for part in &reasoning_parts {
                        let (title, content) = normalize_reasoning_part(part.kind, &part.content);
                        self.emit(EngineEvent::Reasoning {
                            kind: part.kind,
                            title,
                            content,
                        });
                    }
                }

                if let Some(ref content) = content {
                    let content_text = content.as_text();
                    let trimmed = smelt_buffer::text::trim_whitespace(content_text);
                    if !trimmed.is_empty() {
                        self.emit(EngineEvent::Text {
                            content: trimmed.to_string(),
                        });
                    }
                }
            }

            if tool_calls.is_empty() {
                let is_empty = content.is_none()
                    && reasoning.is_none()
                    && matches!(
                        self.history.last(),
                        Some(HistoryItem::Assistant(t)) if !t.invocations.is_empty()
                    );

                if is_empty && empty_retries < MAX_EMPTY_RETRIES {
                    empty_retries += 1;
                    log::entry(
                        log::Level::Warn,
                        "empty_response_retry",
                        &serde_json::json!({ "attempt": empty_retries }),
                    );
                    continue;
                }

                let hooked = self
                    .apply_response_hooks(Message::assistant_with_reasoning(
                        content,
                        reasoning,
                        reasoning_details,
                        None,
                    ))
                    .await;
                let turn = AssistantStep::terminal(
                    hooked.content,
                    hooked.reasoning_content,
                    hooked.reasoning_details.unwrap_or_default(),
                );
                let first_index = self.public_history_len();
                self.push_assistant_step(turn);
                self.emit_history_appended_from(first_index);
                self.emit_turn_complete(false);
                return;
            }

            empty_retries = 0;
            let hooked = self
                .apply_response_hooks(Message::assistant_with_reasoning(
                    content,
                    reasoning,
                    reasoning_details,
                    Some(tool_calls.clone()),
                ))
                .await;
            // Capture the (possibly hook-mutated) in-flight assistant shape
            // for atomic commit. The `tool_calls` that classify_tools will
            // dispatch are read from the hook output too - plugins that
            // synthesize calls must do so via the hook return value.
            let post_hook_content = hooked.content;
            let post_hook_reasoning = hooked.reasoning_content;
            let post_hook_reasoning_blocks = hooked.reasoning_details.unwrap_or_default();
            let dispatched_calls = hooked.tool_calls.unwrap_or_default();

            if let Err(replayed) = tool_replay_guard.begin_round(&dispatched_calls) {
                let message = replayed.message();
                log::entry(
                    log::Level::Warn,
                    "agent_stop",
                    &serde_json::json!({
                        "reason": replayed.reason(),
                        "error": &message,
                    }),
                );
                self.emit(EngineEvent::TurnError {
                    message,
                    kind: None,
                    retry_at_ms: None,
                });
                self.emit_turn_complete(false);
                return;
            }

            let mut plan = self.classify_tools(&dispatched_calls);
            let mut completed: Vec<Option<ToolResult>> =
                (0..plan.slots.len()).map(|_| None).collect();
            let (mut cancelled, mut deferred_turn_cmds, mut plugin_outcomes) =
                self.execute_concurrent(&mut plan, &mut completed).await;
            let (sequential_cancelled, seq_outcomes) =
                self.run_sequential(&plan, &mut deferred_turn_cmds).await;
            cancelled |= sequential_cancelled;
            plugin_outcomes.extend(seq_outcomes);
            if cancelled {
                self.mark_unfinished_cancelled(&plan, &mut completed);
            }
            let slot_outcomes = self.gather_slot_results(&plan, completed);
            let inline_outcomes = std::mem::take(&mut plan.inline_outcomes);
            // All execution paths have written their outcome into one of
            // these three vecs. Pair-and-order folds them onto the classified
            // calls so the resulting `Vec<ToolInvocation>` has the same length
            // and order as the LLM's emitted tool_uses. The history can never
            // observe an unpaired tool_use or an invocation without its timestamp.
            let mut invocations = pair_invocations_in_order(
                &plan.calls,
                slot_outcomes,
                plugin_outcomes,
                inline_outcomes,
            );
            crate::result_dedup::apply_in_place(&mut invocations, &self.history);
            crate::trim::budget_tool_invocations(&mut invocations);
            let first_index = self.public_history_len();
            self.push_assistant_step(AssistantStep::with_invocations(
                post_hook_content,
                post_hook_reasoning,
                post_hook_reasoning_blocks,
                invocations,
            ));
            tool_replay_guard.finish_round(&dispatched_calls);
            self.emit_history_appended_from(first_index);
            if !cancelled {
                provider_turn_cmds.extend(deferred_turn_cmds);
                self.apply_deferred_turn_cmds(provider_turn_cmds);
            }
        }
    }

    fn send_tool_started_for_call(
        event_tx: &crate::EngineEventSender,
        call: &ClassifiedToolCall<'_>,
    ) {
        let _ = event_tx.send(EngineEvent::ToolStarted {
            invocation_id: call.invocation_id,
            call_id: call.tc.id.clone(),
            tool_name: call.tc.function.name.clone(),
            args: call.args.clone(),
            called_at_ms: call.called_at_ms,
        });
    }

    fn emit_tool_started_for_call(&self, call: &ClassifiedToolCall<'_>) {
        Self::send_tool_started_for_call(self.event_tx, call);
    }

    fn send_tool_dispatch_for_call(
        event_tx: &crate::EngineEventSender,
        request_id: u64,
        call: &ClassifiedToolCall<'_>,
    ) {
        Self::send_tool_started_for_call(event_tx, call);
        let _ = event_tx.send(EngineEvent::ToolDispatch {
            request_id,
            invocation_id: call.invocation_id,
            call_id: call.tc.id.clone(),
            tool_name: call.tc.function.name.clone(),
            args: call.args.clone(),
        });
    }

    fn send_tool_rejected_for_call(
        event_tx: &crate::EngineEventSender,
        call: &ClassifiedToolCall<'_>,
        summary: protocol::StyledLines,
        result: ToolOutcome,
        elapsed_ms: Option<u64>,
    ) {
        let _ = event_tx.send(EngineEvent::ToolRejected {
            invocation_id: call.invocation_id,
            call_id: call.tc.id.clone(),
            tool_name: call.tc.function.name.clone(),
            args: call.args.clone(),
            summary,
            result,
            elapsed_ms,
            called_at_ms: call.called_at_ms,
        });
    }

    fn emit_tool_rejected_for_call(
        &self,
        call: &ClassifiedToolCall<'_>,
        summary: protocol::StyledLines,
        result: ToolOutcome,
        elapsed_ms: Option<u64>,
    ) {
        Self::send_tool_rejected_for_call(self.event_tx, call, summary, result, elapsed_ms);
    }

    fn blocked_tool_outcome() -> ToolOutcome {
        ToolOutcome::new(
            "The user's permission settings blocked this tool call. \
             Try a different approach or ask the user for guidance."
                .to_string(),
            false,
            None,
        )
    }

    fn classify_tools<'b>(&mut self, tool_calls: &'b [protocol::ToolCall]) -> ToolExecutionPlan<'b>
    where
        'a: 'b,
    {
        let mut plan = ToolExecutionPlan {
            calls: Vec::with_capacity(tool_calls.len()),
            slots: Vec::new(),
            ready: Vec::new(),
            pending_perms: Vec::new(),
            pending_tools: Vec::new(),
            sequential_tools: Vec::new(),
            pending_tool_hooks: Vec::new(),
            pending_tool_perms: Vec::new(),
            inline_outcomes: Vec::new(),
        };

        // Cancellation isn't checked inside the per-tool loop: if it
        // were, the unprocessed tool_calls would never produce
        // outcomes, leaving the committed assistant step with orphan
        // calls. Letting classification finish is cheap, and
        // `execute_concurrent` short-circuits the actual work on
        // cancellation by draining its pending vecs with a synthetic
        // `cancelled` outcome (which the invariant requires).
        // Tool evaluation replies for plugin calls share `cmd_rx` with user commands.
        // Leave them queued until `execute_concurrent` has the full request-id plan.
        for tc in tool_calls {
            let args = serde_json::from_str(&tc.function.arguments).unwrap_or_default();
            let call_index = plan.calls.len();
            plan.calls.push(ClassifiedToolCall {
                invocation_id: next_invocation_id(),
                tc,
                args,
                start: self.config.clock.instant_now(),
                called_at_ms: crate::clock::unix_time_ms(self.config.clock.as_ref()),
            });
            let call = &plan.calls[call_index];

            let tool = self.tools.iter().find(|pt| {
                pt.name == call.tc.function.name
                    && (pt.override_core || !self.dispatcher.contains(&call.tc.function.name))
            });
            if let Some(pt) = tool {
                let is_sequential =
                    matches!(pt.execution_mode, protocol::ToolExecutionMode::Sequential);
                let request_id = next_request_id();
                self.emit(EngineEvent::ToolEvaluationRequest {
                    request_id,
                    invocation_id: call.invocation_id,
                    call_id: call.tc.id.clone(),
                    tool_name: call.tc.function.name.clone(),
                    args: call.args.clone(),
                    mode: self.mode.clone(),
                });
                plan.pending_tool_hooks.push((
                    request_id,
                    PendingToolCall {
                        call_index,
                        is_sequential,
                    },
                ));
                continue;
            }

            let evaluation = match self.dispatcher.evaluate_tool_call(
                self.turn_id,
                &call.tc.function.name,
                &call.args,
                self.mode.clone(),
                self.permission_overrides.as_ref(),
            ) {
                Some(evaluation) => evaluation,
                None => {
                    let outcome = ToolOutcome::new(
                        format!("unknown tool: {}", call.tc.function.name),
                        true,
                        None,
                    );
                    self.emit_tool_rejected_for_call(
                        call,
                        protocol::StyledLines::empty(),
                        outcome.clone(),
                        Some(self.elapsed_ms_since(call.start)),
                    );
                    plan.inline_outcomes.push((call_index, outcome));
                    continue;
                }
            };

            let protocol::ToolEvaluation { decision, metadata } = evaluation;
            let summary = metadata.summary.clone();
            let slot_index = plan.slots.len();
            match decision {
                Decision::Allow => {
                    self.emit_tool_started_for_call(call);
                    plan.slots.push(ToolSlot {
                        call_index,
                        confirm_msg: None,
                    });
                    plan.ready.push(slot_index);
                }
                Decision::Deny => {
                    let outcome = Self::blocked_tool_outcome();
                    self.emit_tool_rejected_for_call(call, summary, outcome.clone(), None);
                    plan.inline_outcomes.push((call_index, outcome));
                }
                Decision::Error(ref err) => {
                    let outcome = ToolOutcome::new(err.clone(), true, None);
                    self.emit_tool_rejected_for_call(call, summary, outcome.clone(), None);
                    plan.inline_outcomes.push((call_index, outcome));
                }
                Decision::Ask => {
                    let request_id = next_request_id();
                    self.emit(EngineEvent::RequestPermission {
                        request_id,
                        invocation_id: call.invocation_id,
                        call_id: call.tc.id.clone(),
                        tool_name: call.tc.function.name.clone(),
                        args: call.args.clone(),
                        approval_patterns: metadata.approval_patterns,
                        called_at_ms: call.called_at_ms,
                        summary,
                    });
                    plan.slots.push(ToolSlot {
                        call_index,
                        confirm_msg: None,
                    });
                    plan.pending_perms.push((slot_index, request_id));
                }
            }
        }

        plan
    }

    /// Run ready tools concurrently, processing permission decisions and steering mid-flight.
    /// Returns `(cancelled, deferred_commands, tool_results)`.
    async fn execute_concurrent<'b>(
        &mut self,
        plan: &mut ToolExecutionPlan<'b>,
        completed: &mut [Option<ToolResult>],
    ) -> (bool, Vec<UiCommand>, Vec<(usize, ToolOutcome, Option<u64>)>) {
        use futures_util::stream::StreamExt;

        type TaggedFut<'x> =
            std::pin::Pin<Box<dyn std::future::Future<Output = (usize, ToolResult)> + Send + 'x>>;

        let contexts: Vec<_> = plan.slots.iter().map(|_| ToolContext).collect();
        // Cloned once so the select! arms can compute elapsed without reborrowing `self`.
        let clock = std::sync::Arc::clone(&self.config.clock);
        let elapsed_ms_since = |start: Instant| -> u64 {
            clock.instant_now().duration_since(start).as_millis() as u64
        };

        let mut futs: futures_util::stream::FuturesUnordered<TaggedFut<'_>> =
            futures_util::stream::FuturesUnordered::new();

        // Side-call futures from `smelt.tools.call` - don't count against `outstanding`.
        type SideFut<'x> =
            std::pin::Pin<Box<dyn std::future::Future<Output = (u64, ToolResult)> + Send + 'x>>;
        let mut side_futs: futures_util::stream::FuturesUnordered<SideFut<'_>> =
            futures_util::stream::FuturesUnordered::new();

        let dispatcher = self.dispatcher;
        for &slot_index in &plan.ready {
            let call = &plan.calls[plan.slots[slot_index].call_index];
            let fut = dispatcher
                .dispatch(
                    &call.tc.function.name,
                    call.args.clone(),
                    &contexts[slot_index],
                )
                .expect("dispatcher resolved tool at slot-build time");
            futs.push(Box::pin(async move { (slot_index, fut.await) }));
        }

        let mut outstanding = plan.ready.len()
            + plan.pending_perms.len()
            + plan.pending_tools.len()
            + plan.pending_tool_hooks.len()
            + plan.pending_tool_perms.len();
        let cancel = &self.cancel;
        let cmd_rx = &mut self.cmd_rx;
        // Pre-bind disjoint immutable refs so the catch-all arm can route
        // background commands (today: EngineAsk) through
        // `dispatch_background_cmd` without colliding with `cmd_rx`'s
        // long-held `&mut self.cmd_rx` borrow. Without this, any tool
        // that parks on `smelt.engine.ask` (notably `web_fetch`) would
        // deadlock the turn: the command would land in cmd_rx, fall into
        // a silent `_ => {}`, and never produce an `EngineAskResponse`.
        let bg_ctx = BackgroundCtx {
            config: self.config,
            http_client: self.http_client,
            dispatcher,
            event_tx: self.event_tx,
            host_tx: self.host_tx,
            bg_cancel: self.bg_cancel.clone(),
        };
        let mut deferred: Vec<UiCommand> = Vec::new();
        let mut speculative: Vec<UiCommand> = Vec::new();
        let mut tool_results: Vec<(usize, ToolOutcome, Option<u64>)> = Vec::new();

        let cancelled = loop {
            if outstanding == 0 {
                break false;
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break true,
                Some(cmd) = cmd_rx.recv() => match cmd {
                    UiCommand::PermissionDecision { request_id, approved, message } => {
                        if let Some(pos) = plan
                            .pending_perms
                            .iter()
                            .position(|(_, rid)| *rid == request_id)
                        {
                            let (slot_index, _) = plan.pending_perms.swap_remove(pos);
                            if approved {
                                plan.slots[slot_index].confirm_msg = message;
                                let call = &plan.calls[plan.slots[slot_index].call_index];
                                Self::send_tool_started_for_call(self.event_tx, call);
                                let fut = dispatcher
                                    .dispatch(
                                        &call.tc.function.name,
                                        call.args.clone(),
                                        &contexts[slot_index],
                                    )
                                    .expect("dispatcher resolved tool at slot-build time");
                                futs.push(Box::pin(async move { (slot_index, fut.await) }));
                            } else {
                                let denial = match message {
                                    Some(msg) => format!(
                                        "The user denied this tool call with message: {msg}"
                                    ),
                                    None => "The user denied this tool call. Try a different \
                                             approach or ask the user for guidance."
                                        .to_string(),
                                };
                                completed[slot_index] = Some(ToolResult {
                                    content: denial,
                                    is_error: false,
                                    metadata: None,
                                });
                                outstanding -= 1;
                            }
                        } else if let Some(pos) = plan
                            .pending_tool_perms
                            .iter()
                            .position(|(rid, _)| *rid == request_id)
                        {
                            let (_, pending) = plan.pending_tool_perms.swap_remove(pos);
                            let call = &plan.calls[pending.call_index];
                            if approved {
                                if pending.is_sequential {
                                    plan.sequential_tools.push(pending.call_index);
                                    outstanding -= 1;
                                } else {
                                    let request_id = next_request_id();
                                    Self::send_tool_dispatch_for_call(
                                        self.event_tx,
                                        request_id,
                                        call,
                                    );
                                    plan.pending_tools.push((request_id, pending.call_index));
                                }
                            } else {
                                let denial = match message {
                                    Some(msg) => format!(
                                        "The user denied this tool call with message: {msg}"
                                    ),
                                    None => "The user denied this tool call. Try a different \
                                             approach or ask the user for guidance."
                                        .to_string(),
                                };
                                let elapsed_ms = Some(elapsed_ms_since(call.start));
                                let outcome = ToolOutcome::new(denial, false, None);
                                let _ = self.event_tx.send(EngineEvent::ToolFinished {
                                    invocation_id: call.invocation_id,
                                    call_id: call.tc.id.clone(),
                                    result: outcome.clone(),
                                    elapsed_ms,
                                });
                                tool_results.push((pending.call_index, outcome, elapsed_ms));
                                outstanding -= 1;
                            }
                        }
                    }
                    UiCommand::ToolEvaluationResponse { request_id, evaluation } => {
                        if let Some(pos) = plan
                            .pending_tool_hooks
                            .iter()
                            .position(|(rid, _)| *rid == request_id)
                        {
                            let (_, pending) = plan.pending_tool_hooks.swap_remove(pos);
                            let protocol::ToolEvaluation { decision, metadata } = evaluation;
                            match decision {
                                Decision::Allow => {
                                    let call = &plan.calls[pending.call_index];
                                    if pending.is_sequential {
                                        plan.sequential_tools.push(pending.call_index);
                                        outstanding -= 1;
                                    } else {
                                        let request_id = next_request_id();
                                        Self::send_tool_dispatch_for_call(
                                            self.event_tx,
                                            request_id,
                                            call,
                                        );
                                        plan.pending_tools.push((request_id, pending.call_index));
                                    }
                                }
                                Decision::Deny => {
                                    let call = &plan.calls[pending.call_index];
                                    let elapsed_ms = Some(elapsed_ms_since(call.start));
                                    let outcome = Self::blocked_tool_outcome();
                                    let summary = metadata.summary.clone();
                                    Self::send_tool_rejected_for_call(
                                        self.event_tx,
                                        call,
                                        summary,
                                        outcome.clone(),
                                        elapsed_ms,
                                    );
                                    tool_results.push((pending.call_index, outcome, elapsed_ms));
                                    outstanding -= 1;
                                }
                                Decision::Error(ref err) => {
                                    let call = &plan.calls[pending.call_index];
                                    let elapsed_ms = Some(elapsed_ms_since(call.start));
                                    let outcome = ToolOutcome::new(err.clone(), true, None);
                                    let summary = metadata.summary.clone();
                                    Self::send_tool_rejected_for_call(
                                        self.event_tx,
                                        call,
                                        summary,
                                        outcome.clone(),
                                        elapsed_ms,
                                    );
                                    tool_results.push((pending.call_index, outcome, elapsed_ms));
                                    outstanding -= 1;
                                }
                                Decision::Ask => {
                                    let call = &plan.calls[pending.call_index];
                                    let summary = metadata.summary.clone();
                                    let request_id = next_request_id();
                                    let _ = self
                                        .event_tx
                                        .send(EngineEvent::RequestPermission {
                                            request_id,
                                            invocation_id: call.invocation_id,
                                            call_id: call.tc.id.clone(),
                                            tool_name: call.tc.function.name.clone(),
                                            args: call.args.clone(),
                                            approval_patterns: metadata.approval_patterns,
                                            called_at_ms: call.called_at_ms,
                                            summary,
                                        });
                                    plan.pending_tool_perms.push((request_id, pending));
                                }
                            }
                        }
                    }
                    UiCommand::ToolResult {
                        request_id,
                        invocation_id: _,
                        call_id: _,
                        content,
                        is_error,
                        metadata,
                        display_content,
                        attachment,
                    } => {
                        if let Some(pos) = plan
                            .pending_tools
                            .iter()
                            .position(|(pending_id, _)| *pending_id == request_id)
                        {
                            let (_, call_index) = plan.pending_tools.swap_remove(pos);
                            let call = &plan.calls[call_index];
                            let elapsed_ms = Some(elapsed_ms_since(call.start));
                            let outcome = ToolOutcome::from_parts(
                                content,
                                is_error,
                                metadata,
                                display_content,
                                attachment,
                            );
                            let _ = self.event_tx.send(EngineEvent::ToolFinished {
                                invocation_id: call.invocation_id,
                                call_id: call.tc.id.clone(),
                                result: outcome.clone(),
                                elapsed_ms,
                            });
                            tool_results.push((call_index, outcome, elapsed_ms));
                            outstanding -= 1;
                        }
                    }
                    UiCommand::CallCoreTool {
                        request_id,
                        parent_invocation_id,
                        parent_call_id,
                        tool_name,
                        args,
                    } => {
                        if dispatcher.contains(&tool_name) {
                            let _ = (parent_invocation_id, parent_call_id);
                            let ctx = ToolContext;
                            side_futs.push(Box::pin(async move {
                                let r = dispatcher
                                    .dispatch(&tool_name, args, &ctx)
                                    .expect("dispatcher contains tool")
                                    .await;
                                (request_id, r)
                            }));
                        } else {
                            let _ = self.event_tx.send(EngineEvent::CoreToolResult {
                                request_id,
                                content: format!("tool not found: {tool_name}"),
                                is_error: true,
                                metadata: None,
                            });
                        }
                    }
                    cmd => match ActiveTurnCommand::classify(cmd) {
                        ActiveTurnCommand::Cancel => {
                            cancel.cancel();
                            self.bg_cancel.cancel();
                        }
                        ActiveTurnCommand::Injection(cmd) => speculative.push(cmd),
                        ActiveTurnCommand::HistoryAppend(append) => {
                            deferred.push(UiCommand::AppendHistoryItem { append });
                        }
                        ActiveTurnCommand::ConfigUpdate(cmd) => deferred.push(cmd),
                        ActiveTurnCommand::Other(other) => {
                            let _ = dispatch_background_cmd(other, &bg_ctx);
                        }
                    }
                },
                Some((idx, result)) = futs.next(), if !futs.is_empty() => {
                    completed[idx] = Some(result);
                    outstanding -= 1;
                }
                Some((req_id, result)) = side_futs.next(), if !side_futs.is_empty() => {
                    let _ = self.event_tx.send(EngineEvent::CoreToolResult {
                        request_id: req_id,
                        content: result.content,
                        is_error: result.is_error,
                        metadata: result.metadata,
                    });
                }
            }
        };

        if cancelled {
            let cancelled_outcome = || ToolOutcome::new("cancelled".to_string(), true, None);
            let calls = &plan.calls;
            for (_, call_index) in plan.pending_tools.drain(..) {
                let call = &calls[call_index];
                let elapsed_ms = Some(elapsed_ms_since(call.start));
                let outcome = cancelled_outcome();
                let _ = self.event_tx.send(EngineEvent::ToolFinished {
                    invocation_id: call.invocation_id,
                    call_id: call.tc.id.clone(),
                    result: outcome.clone(),
                    elapsed_ms,
                });
                tool_results.push((call_index, outcome, elapsed_ms));
            }
            for (_, pending) in plan.pending_tool_hooks.drain(..) {
                let call = &calls[pending.call_index];
                let elapsed_ms = Some(elapsed_ms_since(call.start));
                let outcome = cancelled_outcome();
                let _ = self.event_tx.send(EngineEvent::ToolFinished {
                    invocation_id: call.invocation_id,
                    call_id: call.tc.id.clone(),
                    result: outcome.clone(),
                    elapsed_ms,
                });
                tool_results.push((pending.call_index, outcome, elapsed_ms));
            }
            for (_, pending) in plan.pending_tool_perms.drain(..) {
                let call = &calls[pending.call_index];
                let elapsed_ms = Some(elapsed_ms_since(call.start));
                let outcome = cancelled_outcome();
                let _ = self.event_tx.send(EngineEvent::ToolFinished {
                    invocation_id: call.invocation_id,
                    call_id: call.tc.id.clone(),
                    result: outcome.clone(),
                    elapsed_ms,
                });
                tool_results.push((pending.call_index, outcome, elapsed_ms));
            }
        }

        self.apply_deferred_turn_cmds(deferred);

        let deferred_turn_cmds = if cancelled { Vec::new() } else { speculative };
        (cancelled, deferred_turn_cmds, tool_results)
    }

    /// Populate `completed[i]` with a synthetic `cancelled` outcome for any
    /// slot that never finished. Emits a `ToolFinished` event per slot. The
    /// resulting `Vec<Option<ToolResult>>` invariant - Some for every slot -
    /// is what `gather_slot_results` then folds into the assistant step.
    fn mark_unfinished_cancelled(
        &mut self,
        plan: &ToolExecutionPlan<'_>,
        completed: &mut [Option<ToolResult>],
    ) {
        for (slot_index, slot) in plan.slots.iter().enumerate() {
            if completed[slot_index].is_some() {
                continue;
            }
            let call = &plan.calls[slot.call_index];
            let outcome = ToolResult {
                content: "cancelled".to_string(),
                is_error: true,
                metadata: None,
            };
            self.emit(EngineEvent::ToolFinished {
                invocation_id: call.invocation_id,
                call_id: call.tc.id.clone(),
                result: ToolOutcome::new(
                    outcome.content.clone(),
                    outcome.is_error,
                    outcome.metadata.clone(),
                ),
                elapsed_ms: Some(self.elapsed_ms_since(call.start)),
            });
            completed[slot_index] = Some(outcome);
        }
    }

    /// Dispatch sequential tools one at a time (used by tools that await user interaction).
    async fn run_sequential(
        &mut self,
        plan: &ToolExecutionPlan<'_>,
        deferred_turn_cmds: &mut Vec<UiCommand>,
    ) -> (bool, Vec<(usize, ToolOutcome, Option<u64>)>) {
        let mut tool_results = Vec::new();
        let mut cancelled = self.cancel.is_cancelled();
        for &call_index in &plan.sequential_tools {
            let call = &plan.calls[call_index];
            let (content, is_error, metadata, display_content, attachment) = if cancelled {
                ("cancelled".to_string(), true, None, Vec::new(), None)
            } else {
                let request_id = next_request_id();
                Self::send_tool_dispatch_for_call(self.event_tx, request_id, call);
                match self
                    .wait_for_tool_result(request_id, deferred_turn_cmds)
                    .await
                {
                    Some((content, is_error, metadata, display_content, attachment)) => {
                        (content, is_error, metadata, display_content, attachment)
                    }
                    None => {
                        cancelled = true;
                        ("cancelled".to_string(), true, None, Vec::new(), None)
                    }
                }
            };
            let elapsed_ms = Some(self.elapsed_ms_since(call.start));
            let outcome =
                ToolOutcome::from_parts(content, is_error, metadata, display_content, attachment);
            let _ = self.event_tx.send(EngineEvent::ToolFinished {
                invocation_id: call.invocation_id,
                call_id: call.tc.id.clone(),
                result: outcome.clone(),
                elapsed_ms,
            });
            tool_results.push((call_index, outcome, elapsed_ms));
        }
        (cancelled, tool_results)
    }

    /// Fold built-in (dispatched-by-slot) tool results into classified-call
    /// indexes, outcomes, and elapsed times ready to be paired into the
    /// assistant turn. Emits `ToolFinished` along the way.
    ///
    /// Pure accumulator: never touches `self.history`. The atomic commit
    /// in `run()` is the only place that writes to history.
    fn gather_slot_results(
        &mut self,
        plan: &ToolExecutionPlan<'_>,
        mut completed: Vec<Option<ToolResult>>,
    ) -> Vec<(usize, ToolOutcome, Option<u64>)> {
        let mut out = Vec::with_capacity(plan.slots.len());
        for (slot_index, slot) in plan.slots.iter().enumerate() {
            let Some(result) = completed[slot_index].take() else {
                continue;
            };
            let call = &plan.calls[slot.call_index];
            let ToolResult {
                content,
                is_error,
                metadata,
            } = result;

            if log::Level::Debug.enabled() {
                let mut preview = smelt_buffer::text::slice(&content, 0..500).to_owned();
                if self.request_config.redact_secrets {
                    preview = crate::redact::redact(&preview);
                }
                log::entry(
                    log::Level::Debug,
                    "tool_result",
                    &serde_json::json!({
                        "tool": call.tc.function.name,
                        "id": call.tc.id,
                        "is_error": is_error,
                        "content_len": content.len(),
                        "content_preview": preview,
                    }),
                );
            }

            let elapsed_ms = self.elapsed_ms_since(call.start);
            let mut full_content = content.clone();
            if let Some(ref msg) = slot.confirm_msg {
                full_content.push_str(&format!("\n\nUser message: {msg}"));
            }
            self.emit(EngineEvent::ToolFinished {
                invocation_id: call.invocation_id,
                call_id: call.tc.id.clone(),
                result: ToolOutcome::new(content.clone(), is_error, metadata.clone()),
                elapsed_ms: Some(elapsed_ms),
            });
            out.push((
                slot.call_index,
                ToolOutcome::new(full_content, is_error, metadata),
                Some(elapsed_ms),
            ));
        }
        out
    }

    fn drain_commands(&mut self) {
        while let Ok(cmd) = self.cmd_rx.try_recv() {
            self.handle_turn_cmd(cmd);
        }
    }

    /// Call the LLM while retaining steer/unsteer commands for the response's
    /// canonical commit boundary.
    async fn call_llm(
        &mut self,
        tool_defs: &[ToolDefinition],
        prepared_messages: Option<crate::host::PreparedRequestMessages>,
    ) -> LlmCallOutcome {
        // Config updates received while the provider is borrowed are replayed
        // in channel order after the request resolves.
        let mut deferred_config_updates: Vec<UiCommand> = Vec::new();
        // Speculative steer/unsteer commands received while the request is
        // in flight. Applied only if the request succeeds so the app can
        // preserve its queue on failure.
        let mut deferred_turn_cmds: Vec<UiCommand> = Vec::new();
        // History appends (e.g. process status) are factual and are applied
        // regardless of whether the request succeeds or fails.
        let mut deferred_appends: Vec<protocol::HistoryAppend> = Vec::new();

        let provider_stream = ProviderStreamState::default();

        // Deferred config updates apply after the future resolves.
        let result = {
            let on_retry = |delay: std::time::Duration, attempt: u32| {
                let _ = self.event_tx.send(EngineEvent::Retrying {
                    delay_ms: delay.as_millis() as u64,
                    attempt,
                });
            };
            let on_delta =
                |delta: ProviderStreamEvent<'_>| provider_stream.apply(delta, self.event_tx);
            // Resolve pricing once so the request-log sidecar can record an
            // estimated cost alongside usage on success.
            let pricing = smelt_provider::resolve_pricing(
                &self.model_target.model,
                &self.model_target.provider_type,
                &self.model_target.api_base,
                &self.model_target.config,
            );
            let persistence = self.persistence;
            let audit_host_tx = self.host_tx.clone();
            let turn_id = self.turn_id;
            let history_len = self.history.len();
            let audit_mode = self.request_config.request_audit;
            // Commands may mutate history after the prepare hook. Reuse the
            // shared conversion in the common unchanged case and rebuild only
            // when its history revision is stale.
            let wire_messages = prepared_messages.unwrap_or_else(|| {
                let _perf = smelt_perf::perf::begin("engine:request:provider_messages");
                crate::host::PreparedRequestMessages::new(
                    protocol::history_to_messages(&self.history),
                    self.system_history_offset(),
                )
            });
            let on_attempt = move |info: RequestAttemptInfo<'_>| {
                let ctx = crate::request_log::RequestContext {
                    request_id: turn_id,
                    kind: "turn".to_string(),
                    turn_id: Some(turn_id),
                    ask_id: None,
                    history_len: Some(history_len),
                    background: false,
                };
                dispatch_request_audit(
                    &audit_host_tx,
                    persistence,
                    ctx,
                    &info,
                    &pricing,
                    audit_mode,
                );
            };
            let request_opts = ChatRequestOptions {
                cache: self.provider.default_cache_config(
                    self.request_config.cache_ttl_long,
                    Some(&self.session_id),
                ),
                fast_mode: self.fast_mode,
                ..ChatRequestOptions::default()
            };
            let opts = ChatOptions {
                cancel: &self.cancel,
                on_retry: Some(&on_retry),
                on_delta: Some(&on_delta),
                on_attempt: Some(&on_attempt),
            };
            let chat_future = self.provider.chat(
                wire_messages.wire(),
                tool_defs,
                &self.model_target.model,
                self.reasoning_effort.clone(),
                &request_opts,
                &opts,
            );
            tokio::pin!(chat_future);

            let mut cancel_received = false;
            loop {
                if cancel_received {
                    break (&mut chat_future).await;
                }
                tokio::select! {
                    biased;
                    result = &mut chat_future => break result,
                    Some(cmd) = self.cmd_rx.recv() => match ActiveTurnCommand::classify(cmd) {
                        ActiveTurnCommand::Cancel => {
                            self.cancel.cancel();
                            cancel_received = true;
                            self.bg_cancel.cancel();
                        }
                        ActiveTurnCommand::HistoryAppend(append) => {
                            deferred_appends.push(append);
                        }
                        ActiveTurnCommand::Injection(cmd) => deferred_turn_cmds.push(cmd),
                        ActiveTurnCommand::ConfigUpdate(cmd) => {
                            deferred_config_updates.push(cmd);
                        }
                        ActiveTurnCommand::Other(other) => {
                            let _ = dispatch_background_cmd(other, &self.bg_ctx());
                        }
                    },
                }
            }
        };

        let (pt, pr) = provider_stream.finish(self.event_tx);

        self.apply_deferred_turn_cmds(deferred_config_updates);
        for append in deferred_appends {
            self.queue_history_item(append);
        }
        LlmCallOutcome {
            result,
            partial_text: pt,
            partial_reasoning: pr,
            deferred_turn_cmds,
        }
    }

    async fn wait_for_tool_result(
        &mut self,
        request_id: u64,
        deferred_turn_cmds: &mut Vec<UiCommand>,
    ) -> Option<(
        String,
        bool,
        Option<serde_json::Value>,
        Vec<protocol::ToolDisplayContent>,
        Option<protocol::ToolAttachment>,
    )> {
        loop {
            match self.cmd_rx.recv().await {
                Some(UiCommand::ToolResult {
                    request_id: id,
                    invocation_id: _,
                    call_id: _,
                    content,
                    is_error,
                    metadata,
                    display_content,
                    attachment,
                }) if id == request_id => {
                    return Some((content, is_error, metadata, display_content, attachment));
                }
                Some(cmd) => match ActiveTurnCommand::classify(cmd) {
                    ActiveTurnCommand::Cancel => {
                        self.cancel.cancel();
                        self.bg_cancel.cancel();
                        return None;
                    }
                    ActiveTurnCommand::HistoryAppend(append) => {
                        self.queue_history_item(append);
                    }
                    ActiveTurnCommand::Injection(cmd) => deferred_turn_cmds.push(cmd),
                    ActiveTurnCommand::ConfigUpdate(cmd) => {
                        self.handle_turn_cmd(cmd);
                    }
                    ActiveTurnCommand::Other(other) => {
                        let _ = dispatch_background_cmd(other, &self.bg_ctx());
                    }
                },
                None => return None,
            }
        }
    }
}

struct LlmCallOutcome {
    result: Result<ChatResponse, ProviderError>,
    partial_text: String,
    partial_reasoning: String,
    deferred_turn_cmds: Vec<UiCommand>,
}

#[derive(Default)]
struct ProviderStreamState {
    text: std::sync::Mutex<String>,
    reasoning: std::sync::Mutex<String>,
    reasoning_parts: std::sync::Mutex<ReasoningStreamState>,
}

impl ProviderStreamState {
    fn apply(&self, event: ProviderStreamEvent<'_>, event_tx: &crate::EngineEventSender) {
        match event {
            ProviderStreamEvent::TextDelta(text) => {
                self.text.lock().unwrap().push_str(text);
                let _ = event_tx.send(EngineEvent::TextDelta {
                    delta: text.to_string(),
                });
            }
            ProviderStreamEvent::Reasoning(event) => {
                self.reasoning_parts
                    .lock()
                    .unwrap()
                    .apply(event, event_tx, &self.reasoning);
            }
            ProviderStreamEvent::ToolCall(tool_event) => match tool_event {
                ToolCallStreamEvent::Started {
                    stream_id,
                    call_id,
                    tool_name,
                } => {
                    let _ = event_tx.send(EngineEvent::ToolCallDraftStarted {
                        stream_id: stream_id.to_string(),
                        call_id: call_id.map(str::to_string),
                        tool_name: tool_name.map(str::to_string),
                    });
                }
                ToolCallStreamEvent::ArgsDelta {
                    stream_id,
                    call_id,
                    tool_name,
                    delta,
                } => {
                    let _ = event_tx.send(EngineEvent::ToolCallDraftDelta {
                        stream_id: stream_id.to_string(),
                        call_id: call_id.map(str::to_string),
                        tool_name: tool_name.map(str::to_string),
                        delta: delta.to_string(),
                    });
                }
                ToolCallStreamEvent::Finished {
                    stream_id,
                    call_id,
                    tool_name,
                    arguments,
                } => {
                    let _ = event_tx.send(EngineEvent::ToolCallDraftFinished {
                        stream_id: stream_id.to_string(),
                        call_id: call_id.to_string(),
                        tool_name: tool_name.to_string(),
                        arguments: arguments.to_string(),
                    });
                }
            },
        }
    }

    fn finish(self, event_tx: &crate::EngineEventSender) -> (String, String) {
        self.reasoning_parts
            .into_inner()
            .unwrap_or_default()
            .finish_all(event_tx);
        (
            self.text.into_inner().unwrap_or_default(),
            self.reasoning.into_inner().unwrap_or_default(),
        )
    }
}

#[derive(Default)]
struct ReasoningStreamState {
    parts: HashMap<String, ActiveReasoningPart>,
}

struct ActiveReasoningPart {
    item_id: String,
    kind: ReasoningKind,
    source: String,
}

impl ReasoningStreamState {
    fn apply(
        &mut self,
        event: ReasoningStreamEvent<'_>,
        tx: &crate::EngineEventSender,
        partial_reasoning: &std::sync::Mutex<String>,
    ) {
        match event {
            ReasoningStreamEvent::PartStarted {
                item_id,
                part_index,
                kind,
            } => {
                let id = reasoning_part_id(item_id, part_index, kind);
                self.parts
                    .entry(id.clone())
                    .or_insert_with(|| start_reasoning_part(tx, id, item_id, kind));
            }
            ReasoningStreamEvent::Delta {
                item_id,
                part_index,
                kind,
                delta,
            } => {
                let id = reasoning_part_id(item_id, part_index, kind);
                let part = self
                    .parts
                    .entry(id.clone())
                    .or_insert_with(|| start_reasoning_part(tx, id.clone(), item_id, kind));
                part.source.push_str(delta);
                partial_reasoning.lock().unwrap().push_str(delta);
                let title = (kind == ReasoningKind::Summary)
                    .then(|| streaming_summary_title(&part.source))
                    .flatten();
                let _ = tx.send(EngineEvent::ReasoningPartDelta {
                    id,
                    kind,
                    delta: delta.to_string(),
                    title,
                });
            }
            ReasoningStreamEvent::PartFinished {
                item_id,
                part_index,
                kind,
                content,
            } => {
                let id = reasoning_part_id(item_id, part_index, kind);
                let mut part = self
                    .parts
                    .remove(&id)
                    .unwrap_or_else(|| start_reasoning_part(tx, id.clone(), item_id, kind));
                if let Some(content) = content {
                    if part.source.is_empty() && !content.is_empty() {
                        partial_reasoning.lock().unwrap().push_str(content);
                        let title = (kind == ReasoningKind::Summary)
                            .then(|| streaming_summary_title(content))
                            .flatten();
                        let _ = tx.send(EngineEvent::ReasoningPartDelta {
                            id: id.clone(),
                            kind,
                            delta: content.to_string(),
                            title,
                        });
                    }
                    part.source.clear();
                    part.source.push_str(content);
                }
                send_reasoning_part_finished(tx, id, part);
            }
            ReasoningStreamEvent::ItemFinished { item_id } => {
                self.finish_item(item_id, tx);
            }
        }
    }

    fn finish_item(&mut self, item_id: &str, tx: &crate::EngineEventSender) {
        let mut ids: Vec<_> = self
            .parts
            .iter()
            .filter_map(|(id, part)| (part.item_id == item_id).then_some(id.clone()))
            .collect();
        ids.sort();
        for id in ids {
            if let Some(part) = self.parts.remove(&id) {
                send_reasoning_part_finished(tx, id, part);
            }
        }
    }

    fn finish_all(self, tx: &crate::EngineEventSender) {
        let mut parts: Vec<_> = self.parts.into_iter().collect();
        parts.sort_by(|a, b| a.0.cmp(&b.0));
        for (id, part) in parts {
            send_reasoning_part_finished(tx, id, part);
        }
    }
}

fn start_reasoning_part(
    tx: &crate::EngineEventSender,
    id: String,
    item_id: &str,
    kind: ReasoningKind,
) -> ActiveReasoningPart {
    let _ = tx.send(EngineEvent::ReasoningPartStarted { id, kind });
    ActiveReasoningPart {
        item_id: item_id.to_string(),
        kind,
        source: String::new(),
    }
}

fn reasoning_part_id(item_id: &str, part_index: u32, kind: ReasoningKind) -> String {
    let item_id = if item_id.is_empty() {
        "reasoning"
    } else {
        item_id
    };
    let kind = match kind {
        ReasoningKind::Summary => "summary",
        ReasoningKind::Raw => "raw",
    };
    format!("{item_id}:{kind}:{part_index}")
}

fn leading_bold_title(source: &str) -> Option<(&str, &str)> {
    let after_open = smelt_buffer::text::trim_start_whitespace(source).strip_prefix("**")?;
    let close = after_open.find("**")?;
    let title = smelt_buffer::text::trim_whitespace(&after_open[..close]);
    (!title.is_empty()).then_some((title, &after_open[close + 2..]))
}

fn streaming_summary_title(source: &str) -> Option<String> {
    leading_bold_title(source).map(|(title, _)| title.to_string())
}

fn normalize_reasoning_part(kind: ReasoningKind, source: &str) -> (Option<String>, String) {
    let source = smelt_buffer::text::trim_whitespace(source);
    if kind == ReasoningKind::Raw {
        return (None, source.to_string());
    }

    let Some((title, after_title)) = leading_bold_title(source) else {
        return (None, source.to_string());
    };
    if after_title.trim().is_empty() || !after_title.starts_with(char::is_whitespace) {
        return (None, source.to_string());
    }
    let content = after_title
        .lines()
        .filter(|line| line.trim() != "<!-- -->")
        .collect::<Vec<_>>()
        .join("\n");
    (
        Some(title.to_string()),
        smelt_buffer::text::trim_whitespace(&content).to_string(),
    )
}

fn send_reasoning_part_finished(
    tx: &crate::EngineEventSender,
    id: String,
    part: ActiveReasoningPart,
) {
    let (title, content) = normalize_reasoning_part(part.kind, &part.source);
    let _ = tx.send(EngineEvent::ReasoningPartFinished {
        id,
        kind: part.kind,
        title,
        content,
    });
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn send_usage(
    tx: &crate::EngineEventSender,
    target: &ModelTarget,
    usage: protocol::TokenUsage,
    tokens_per_sec: Option<f64>,
    background: bool,
) {
    let resolved = smelt_provider::resolve_pricing(
        &target.model,
        &target.provider_type,
        &target.api_base,
        &target.config,
    );
    let cost = resolved.pricing.cost(&usage);
    let _ = tx.send(EngineEvent::TokenUsage {
        usage,
        tokens_per_sec,
        cost_usd: if cost > 0.0 { Some(cost) } else { None },
        background,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestEventReceiver(tokio::sync::mpsc::UnboundedReceiver<crate::EngineOutput>);

    impl TestEventReceiver {
        fn try_recv(&mut self) -> Result<EngineEvent, tokio::sync::mpsc::error::TryRecvError> {
            loop {
                match self.0.try_recv()? {
                    crate::EngineOutput::Event(event) => return Ok(event),
                    crate::EngineOutput::HostCall(_) => {}
                }
            }
        }
    }

    fn test_event_channel() -> (crate::EngineEventSender, TestEventReceiver) {
        let (event_tx, _host_tx, output_rx) = crate::output_channel(crate::HostCallbacks::Enabled);
        (event_tx, TestEventReceiver(output_rx))
    }

    fn test_store_commit(id: &str, history: Vec<HistoryItem>) -> smelt_store::SessionCommit {
        smelt_store::SessionCommit {
            session_id: id.into(),
            expected: smelt_store::StoreHead::default(),
            identity: smelt_store::SessionIdentity {
                id: id.into(),
                created_at: 10,
                parent_id: None,
            },
            metadata: smelt_store::SessionMetadata {
                title: None,
                slug: None,
                first_user_message: None,
                cwd: None,
                mode: None,
                reasoning_effort: None,
                model: None,
                fast_mode: None,
                accounting_json: None,
                checkpoint_json: None,
                checkpoint_events_json: None,
                context_tokens: None,
                context_tokens_history_len: None,
                display_context_tokens: None,
                session_cost_usd: smelt_store::SessionCostUsd::new(0.0).unwrap(),
                updated_at: 20,
            },
            history: smelt_store::HistorySuffix {
                start: smelt_store::HistoryIndex::ZERO,
                final_len: smelt_store::HistoryLen::new(history.len() as u64),
                items: history,
            },
            side_tables: smelt_store::SideTableSuffixes::default(),
            transcript_records: None,
        }
    }

    #[test]
    fn completed_only_reasoning_part_emits_its_content_as_a_delta() {
        let (tx, mut rx) = test_event_channel();
        let partial_reasoning = std::sync::Mutex::new(String::new());
        let mut state = ReasoningStreamState::default();

        state.apply(
            ReasoningStreamEvent::PartFinished {
                item_id: "reasoning",
                part_index: 0,
                kind: ReasoningKind::Raw,
                content: Some("complete thought"),
            },
            &tx,
            &partial_reasoning,
        );

        assert!(matches!(
            rx.try_recv(),
            Ok(EngineEvent::ReasoningPartStarted { .. })
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(EngineEvent::ReasoningPartDelta { delta, .. }) if delta == "complete thought"
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(EngineEvent::ReasoningPartFinished { content, .. }) if content == "complete thought"
        ));
        assert_eq!(partial_reasoning.into_inner().unwrap(), "complete thought");
    }

    #[test]
    fn codex_reasoning_item_finishes_before_answer_engine_event() {
        let stream_events = [
            serde_json::json!({
                "type": "response.output_item.added",
                "item": {"type": "reasoning", "id": "rs_1"},
            }),
            serde_json::json!({
                "type": "response.reasoning_summary_part.added",
                "summary_index": 0,
            }),
            serde_json::json!({
                "type": "response.reasoning_summary_text.delta",
                "summary_index": 0,
                "delta": "**Checking accounting**",
            }),
            serde_json::json!({
                "type": "response.output_item.done",
                "item": {
                    "type": "reasoning",
                    "id": "rs_1",
                    "summary": [{
                        "type": "summary_text",
                        "text": "**Checking accounting**",
                    }],
                    "encrypted_content": "enc",
                },
            }),
            serde_json::json!({
                "type": "response.output_text.delta",
                "delta": "Final answer",
            }),
            serde_json::json!({
                "type": "response.completed",
                "response": {"usage": {}},
            }),
        ];
        let (tx, mut rx) = test_event_channel();
        let provider_stream = ProviderStreamState::default();

        smelt_provider::parse_openai_stream_events(&stream_events, &mut |event| {
            provider_stream.apply(event, &tx);
        })
        .expect("valid Codex response stream");
        let _ = provider_stream.finish(&tx);

        let mut emitted = Vec::new();
        while let Ok(event) = rx.try_recv() {
            emitted.push(event);
        }
        let finished_index = emitted
            .iter()
            .position(|event| matches!(event, EngineEvent::ReasoningPartFinished { .. }))
            .expect("reasoning completion event");
        let text_index = emitted
            .iter()
            .position(|event| matches!(event, EngineEvent::TextDelta { delta } if delta == "Final answer"))
            .expect("answer text event");
        assert!(finished_index < text_index, "events: {emitted:#?}");
        assert_eq!(
            emitted
                .iter()
                .filter(|event| matches!(event, EngineEvent::ReasoningPartFinished { .. }))
                .count(),
            1,
            "events: {emitted:#?}"
        );
    }

    #[test]
    fn reasoning_item_finished_closes_its_open_parts() {
        let (tx, mut rx) = test_event_channel();
        let partial_reasoning = std::sync::Mutex::new(String::new());
        let mut state = ReasoningStreamState::default();

        state.apply(
            ReasoningStreamEvent::Delta {
                item_id: "rs_1",
                part_index: 0,
                kind: ReasoningKind::Summary,
                delta: "**Checking accounting**",
            },
            &tx,
            &partial_reasoning,
        );
        state.apply(
            ReasoningStreamEvent::ItemFinished { item_id: "rs_1" },
            &tx,
            &partial_reasoning,
        );

        assert!(matches!(
            rx.try_recv(),
            Ok(EngineEvent::ReasoningPartStarted { .. })
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(EngineEvent::ReasoningPartDelta { .. })
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(EngineEvent::ReasoningPartFinished { content, .. })
                if content == "**Checking accounting**"
        ));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn reasoning_item_finished_does_not_repeat_a_finished_part() {
        let (tx, mut rx) = test_event_channel();
        let partial_reasoning = std::sync::Mutex::new(String::new());
        let mut state = ReasoningStreamState::default();

        state.apply(
            ReasoningStreamEvent::PartFinished {
                item_id: "rs_1",
                part_index: 0,
                kind: ReasoningKind::Summary,
                content: Some("complete thought"),
            },
            &tx,
            &partial_reasoning,
        );
        assert!(matches!(
            rx.try_recv(),
            Ok(EngineEvent::ReasoningPartStarted { .. })
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(EngineEvent::ReasoningPartDelta { .. })
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(EngineEvent::ReasoningPartFinished { .. })
        ));

        state.apply(
            ReasoningStreamEvent::ItemFinished { item_id: "rs_1" },
            &tx,
            &partial_reasoning,
        );

        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn summary_placeholder_normalizes_to_thinking_title_only() {
        assert_eq!(
            normalize_reasoning_part(
                ReasoningKind::Summary,
                "**Removing unused BlockIndex**\n\n<!-- -->",
            ),
            (Some("Removing unused BlockIndex".into()), String::new())
        );
    }

    #[test]
    fn summary_body_is_kept_separate_from_its_title() {
        assert_eq!(
            normalize_reasoning_part(
                ReasoningKind::Summary,
                "**Checking tests**\n\n<!-- -->\nAll tests passed.",
            ),
            (Some("Checking tests".into()), "All tests passed.".into(),)
        );
    }

    #[test]
    fn standalone_bold_summary_content_is_not_discarded() {
        assert_eq!(
            normalize_reasoning_part(ReasoningKind::Summary, "**Important conclusion**"),
            (None, "**Important conclusion**".into())
        );
    }

    #[test]
    fn load_model_history_reads_requested_lineage_range() {
        let root = tempfile::tempdir().unwrap();
        let session_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let old = HistoryItem::user(protocol::Content::text("old"));
        let recent = HistoryItem::user(protocol::Content::text("recent"));
        let reply = HistoryItem::Assistant(protocol::AssistantStep::terminal(
            Some(protocol::Content::text("reply")),
            None,
            Vec::new(),
        ));
        let command = test_store_commit(session_id, vec![old, recent.clone(), reply.clone()]);
        let mut writer = smelt_store::OwnedLineageWriter::open(root.path(), session_id).unwrap();
        writer.commit_session(&command).unwrap();
        let lineage_id = writer.lineage_id().to_string();
        writer.release().unwrap();

        let history = load_model_history(
            protocol::ModelHistorySource::store(
                vec![HistoryItem::user(protocol::Content::text(
                    "SUMMARY:\ncompact",
                ))],
                lineage_id,
                1,
                3,
            ),
            root.path(),
            session_id,
        )
        .unwrap();

        assert_eq!(history.items.len(), 3);
        assert!(
            matches!(&history.items[0], HistoryItem::User { content, .. } if content.text_content() == "SUMMARY:\ncompact")
        );
        assert_eq!(history.items[1], recent);
        assert_eq!(history.items[2], reply);
        assert_eq!(
            history.coordinates,
            protocol::ModelHistoryCoordinates::projected(1, 1)
        );
    }

    #[test]
    fn model_history_read_completes_while_writer_remains_owned() {
        let root = tempfile::tempdir().unwrap();
        let session_id = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let item = HistoryItem::user(protocol::Content::text("persisted"));
        let command = test_store_commit(session_id, vec![item.clone()]);
        let mut writer = smelt_store::OwnedLineageWriter::open(root.path(), session_id).unwrap();
        writer.commit_session(&command).unwrap();
        let lineage_id = writer.lineage_id().to_string();

        let history = load_model_history(
            protocol::ModelHistorySource::store(Vec::new(), lineage_id, 0, 1),
            root.path(),
            session_id,
        )
        .unwrap();
        writer.release().unwrap();

        assert_eq!(history.items, vec![item]);
    }

    #[test]
    fn classify_provider_error_preserves_cyber_policy() {
        let err = ProviderError::CyberPolicy {
            message: "flagged".into(),
        };

        assert_eq!(
            classify_provider_error(&err),
            EngineAskErrorKind::CyberPolicy
        );
    }

    #[test]
    fn classify_provider_error_detects_kimi_model_token_limit() {
        let err = ProviderError::InvalidResponse(
            r#"{"error":{"type":"invalid_request_error","message":"Invalid request: Your request exceeded model token limit: 262144 (requested: 264866)"},"type":"error"}"#.into(),
        );

        assert_eq!(
            classify_provider_error(&err),
            EngineAskErrorKind::ContextWindow
        );
    }

    // ---- pair_invocations_in_order ----

    fn tc(id: &str) -> protocol::ToolCall {
        protocol::ToolCall::new(
            id.into(),
            protocol::FunctionCall {
                name: "f".into(),
                arguments: "{}".into(),
            },
        )
    }

    #[test]
    fn tool_replay_guard_rejects_completed_call_ids() {
        let calls = vec![tc("call-1")];
        let mut guard = ToolReplayGuard::default();

        guard.begin_round(&calls).unwrap();
        guard.finish_round(&calls);

        assert_eq!(
            guard.begin_round(&calls),
            Err(ReplayedToolCall {
                call_id: "call-1".into(),
            })
        );
    }

    #[test]
    fn tool_replay_guard_rejects_duplicate_ids_before_execution() {
        let calls = vec![tc("call-1"), tc("call-1")];
        let mut guard = ToolReplayGuard::default();

        assert_eq!(
            guard.begin_round(&calls),
            Err(ReplayedToolCall {
                call_id: "call-1".into(),
            })
        );
    }

    #[test]
    fn tool_replay_guard_allows_many_fresh_call_ids() {
        let mut guard = ToolReplayGuard::default();
        for index in 0..2_048 {
            let calls = vec![tc(&format!("call-{index}"))];
            guard.begin_round(&calls).unwrap();
            guard.finish_round(&calls);
        }
    }

    fn classified_call(tc: &protocol::ToolCall, called_at_ms: u64) -> ClassifiedToolCall<'_> {
        ClassifiedToolCall {
            invocation_id: protocol::InvocationId::new(1),
            tc,
            args: HashMap::new(),
            start: Instant::now(),
            called_at_ms,
        }
    }

    fn outcome(content: &str) -> ToolOutcome {
        ToolOutcome::new(content.into(), false, None)
    }

    #[test]
    fn virtual_clock_captures_one_wall_timestamp_per_invocation() {
        let clock = crate::clock::VirtualClock::new(
            std::time::Instant::now(),
            std::time::UNIX_EPOCH + std::time::Duration::from_millis(1_700_000_000_123),
        );

        let captured = crate::clock::unix_time_ms(&clock);
        clock.advance(std::time::Duration::from_secs(5));

        assert_eq!(captured, 1_700_000_000_123);
        assert_eq!(crate::clock::unix_time_ms(&clock), 1_700_000_005_123);
    }

    #[test]
    fn tool_started_event_and_committed_invocation_share_called_at_timestamp() {
        let tc = tc("c1");
        let called_at_ms = 1_700_000_000_123;
        let call = classified_call(&tc, called_at_ms);
        let (event_tx, _host_tx, mut output_rx) =
            crate::output_channel(crate::HostCallbacks::Enabled);

        Turn::send_tool_started_for_call(&event_tx, &call);
        let event_called_at_ms = match output_rx.try_recv().unwrap() {
            crate::EngineOutput::Event(EngineEvent::ToolStarted { called_at_ms, .. }) => {
                called_at_ms
            }
            _ => panic!("unexpected engine output"),
        };
        let invocation = pair_invocations_in_order(
            std::slice::from_ref(&call),
            vec![(0, outcome("ok"), Some(42))],
            Vec::new(),
            Vec::new(),
        )
        .pop()
        .unwrap();

        assert_eq!(event_called_at_ms, called_at_ms);
        assert_eq!(invocation.called_at_ms, Some(event_called_at_ms));
    }

    #[test]
    fn pair_invocations_in_order_precedence_is_slot_then_plugin_then_inline() {
        // The same classified call appears in all three inputs. The classifier
        // guarantees this can't happen in practice - this test pins the
        // tiebreaker so a future refactor that does introduce a collision
        // behaves predictably instead of silently flipping which outcome wins.
        let tool_calls = [tc("c1"), tc("c2"), tc("c3")];
        let calls = vec![
            classified_call(&tool_calls[0], 101),
            classified_call(&tool_calls[1], 102),
            classified_call(&tool_calls[2], 103),
        ];
        let slot = vec![(0, outcome("slot-c1"), Some(11))];
        let plugin = vec![
            (0, outcome("plugin-c1"), Some(99)),
            (1, outcome("plugin-c2"), Some(22)),
        ];
        let inline = vec![
            (0, outcome("inline-c1")),
            (1, outcome("inline-c2")),
            (2, outcome("inline-c3")),
        ];
        let out = pair_invocations_in_order(&calls, slot, plugin, inline);
        assert_eq!(out[0].result.content, "slot-c1");
        assert_eq!(out[0].elapsed_ms, Some(11));
        assert_eq!(out[1].result.content, "plugin-c2");
        assert_eq!(out[1].elapsed_ms, Some(22));
        assert_eq!(out[2].result.content, "inline-c3");
        assert_eq!(out[2].elapsed_ms, None);
        assert_eq!(out[0].called_at_ms, Some(101));
        assert_eq!(out[1].called_at_ms, Some(102));
        assert_eq!(out[2].called_at_ms, Some(103));
    }

    #[test]
    fn pair_invocations_uses_classified_index_when_call_ids_repeat() {
        let tool_calls = [tc("duplicate"), tc("duplicate")];
        let calls = vec![
            classified_call(&tool_calls[0], 101),
            classified_call(&tool_calls[1], 102),
        ];

        let out = pair_invocations_in_order(
            &calls,
            Vec::new(),
            Vec::new(),
            vec![(0, outcome("first")), (1, outcome("second"))],
        );

        assert_eq!(out[0].result.content, "first");
        assert_eq!(out[0].called_at_ms, Some(101));
        assert_eq!(out[1].result.content, "second");
        assert_eq!(out[1].called_at_ms, Some(102));
    }

    fn model_target() -> ModelTarget {
        ModelTarget {
            model: "m".into(),
            api_base: "https://x/".into(),
            api_key: "k".into(),
            provider_type: "openai".into(),
            config: ModelConfig::default(),
        }
    }

    fn test_engine_config(clock: std::sync::Arc<dyn crate::clock::Clock>) -> EngineConfig {
        EngineConfig {
            system_prompt_override: Some("sys".into()),
            ..EngineConfig::new(std::path::PathBuf::from("/tmp"), clock)
        }
    }

    #[test]
    fn current_turn_input_classifies_notes_and_preserves_commands() {
        let client = reqwest::Client::new();
        let dispatcher = crate::tools::EmptyDispatcher::new();
        let (_cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let (event_tx, host_tx, _output_rx) = crate::output_channel(crate::HostCallbacks::Enabled);
        let clock: std::sync::Arc<dyn crate::clock::Clock> =
            std::sync::Arc::new(crate::clock::RealClock);
        let mut config = test_engine_config(clock.clone());
        let provider = EngineProvider::new(
            "https://x".into(),
            "k".into(),
            "openai",
            client.clone(),
            clock,
        );
        let mut turn = Turn {
            provider,
            dispatcher: &dispatcher,
            cmd_rx: &mut cmd_rx,
            event_tx: &event_tx,
            host_tx: &host_tx,
            config: &mut config,
            http_client: &client,
            cancel: CancellationToken::new(),
            bg_cancel: CancellationToken::new(),
            history: vec![HistoryItem::system("sys")],
            history_coordinates: protocol::ModelHistoryCoordinates::canonical(),
            mode: AgentMode::normal(),
            reasoning_effort: ReasoningEffort::Off,
            fast_mode: false,
            turn_id: 1,
            model_target: model_target(),
            target_revision: 0,
            request_config: RequestRuntimeConfig::default(),
            system_prompt: "sys".into(),
            tools: Vec::new(),
            permission_overrides: None,
            pending_history_items: Vec::new(),
            next_history_changed_from: 0,
            history_revision: 0,
            session_id: "s".into(),
            persistence: protocol::PersistenceScope::default(),
            started_at: Instant::now(),
            tps_samples: Vec::new(),
        };

        turn.push_turn_content(
            Content::text(protocol::process_status_note(
                "Background process 751225 exited with code 1.",
            )),
            None,
            false,
        );
        turn.push_turn_content(
            Content::text(protocol::mode_change_note("now in apply mode.")),
            None,
            false,
        );

        assert!(matches!(
            &turn.history[1],
            HistoryItem::Note(protocol::HistoryNote::ProcessStatus { text, .. })
                if text == "Background process 751225 exited with code 1."
        ));
        assert!(matches!(
            &turn.history[2],
            HistoryItem::Note(protocol::HistoryNote::ModeChange { text, .. })
                if text == "now in apply mode."
        ));
        let typed_note = protocol::HistoryNote::process_status_event(
            protocol::ProcessStatusEvent::background_process_completed(
                "751225",
                Some(1),
                protocol::JobTermination::Exited,
            ),
        );
        turn.push_current_turn_input(protocol::StartTurnInput::note(typed_note.clone()));

        assert!(matches!(
            &turn.history[3],
            HistoryItem::Note(note) if note == &typed_note
        ));

        turn.push_current_turn_input(protocol::StartTurnInput::user_command(
            Content::text("expanded command body"),
            "/reflect",
        ));
        assert!(matches!(
            &turn.history[4],
            HistoryItem::User {
                content,
                display: Some(display),
                command: true,
            } if content.text_content() == "expanded command body" && display == "/reflect"
        ));
    }

    #[test]
    fn queued_context_updates_coalesce_without_rewriting_committed_history() {
        let history = vec![HistoryItem::note(protocol::HistoryNote::named_context(
            "goal",
            "committed",
        ))];
        let mut pending = Vec::new();
        let first = protocol::HistoryAppend::set_context(
            HistoryItem::note(protocol::HistoryNote::named_context("goal", "first")),
            "goal",
        );
        let second = protocol::HistoryAppend::set_context(
            HistoryItem::note(protocol::HistoryNote::named_context("goal", "second")),
            "goal",
        );
        let clear = protocol::HistoryAppend::clear_context("goal");
        let restore = protocol::HistoryAppend::set_context(history[0].clone(), "goal");

        assert!(queue_context_history_item(&history, &mut pending, &first));
        assert!(queue_context_history_item(&history, &mut pending, &second));
        assert_eq!(pending, vec![second.item]);
        assert!(queue_context_history_item(&history, &mut pending, &restore));
        assert!(pending.is_empty());

        assert!(queue_context_history_item(&history, &mut pending, &clear));
        assert_eq!(pending, vec![clear.item.clone()]);
        assert!(queue_context_history_item(&history, &mut pending, &clear));
        assert_eq!(pending, vec![clear.item.clone()]);
        assert!(queue_context_history_item(&history, &mut pending, &restore));
        assert!(pending.is_empty());

        let mut never_committed = Vec::new();
        assert!(queue_context_history_item(
            &[],
            &mut never_committed,
            &first
        ));
        assert!(queue_context_history_item(
            &[],
            &mut never_committed,
            &protocol::HistoryAppend::clear_context("goal")
        ));
        assert!(never_committed.is_empty());
    }

    #[test]
    fn active_turn_command_classifier_covers_every_route() {
        let context = || {
            Box::new(protocol::AgentProjectContext {
                cwd: std::path::PathBuf::from("/target"),
                instructions: None,
                skill_section: None,
                system_prompt_override: None,
                system_prompt: "prompt".into(),
                tools: Vec::new(),
            })
        };
        for command in [
            UiCommand::SetReasoningEffort {
                effort: ReasoningEffort::Off,
            },
            UiCommand::SetFastMode { enabled: true },
            UiCommand::SetMode {
                mode: AgentMode::normal(),
            },
            UiCommand::SetTurnModel {
                target: Box::new(model_target()),
                system_prompt: "prompt".into(),
            },
            UiCommand::UpdateAgentProjectContext(context()),
        ] {
            assert!(matches!(
                ActiveTurnCommand::classify(command),
                ActiveTurnCommand::ConfigUpdate(_)
            ));
        }
        assert!(matches!(
            ActiveTurnCommand::classify(UiCommand::Cancel),
            ActiveTurnCommand::Cancel
        ));
        assert!(matches!(
            ActiveTurnCommand::classify(UiCommand::Unsteer { count: 1 }),
            ActiveTurnCommand::Injection(_)
        ));
        assert!(matches!(
            ActiveTurnCommand::classify(UiCommand::AppendHistoryItem {
                append: protocol::HistoryAppend::append(HistoryItem::user(Content::text("note"))),
            }),
            ActiveTurnCommand::HistoryAppend(_)
        ));
        assert!(matches!(
            ActiveTurnCommand::classify(UiCommand::PermissionDecision {
                request_id: 1,
                approved: true,
                message: None,
            }),
            ActiveTurnCommand::Other(_)
        ));
    }

    #[tokio::test]
    async fn sequential_tool_wait_replays_ordered_context_updates_before_result() {
        let client = reqwest::Client::new();
        let dispatcher = crate::tools::EmptyDispatcher::new();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let (event_tx, host_tx, _output_rx) = crate::output_channel(crate::HostCallbacks::Enabled);
        let clock: std::sync::Arc<dyn crate::clock::Clock> =
            std::sync::Arc::new(crate::clock::RealClock);
        let mut config = test_engine_config(clock.clone());
        let provider = EngineProvider::new(
            "https://x".into(),
            "k".into(),
            "openai",
            client.clone(),
            clock,
        );
        let target_tool = protocol::ToolDef {
            name: "target_tool".into(),
            description: "target".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
            modes: None,
            execution_mode: protocol::ToolExecutionMode::Sequential,
            override_core: false,
            hooks: protocol::ToolHookFlags::default(),
            headless: true,
        };
        let mut turn = Turn {
            provider,
            dispatcher: &dispatcher,
            cmd_rx: &mut cmd_rx,
            event_tx: &event_tx,
            host_tx: &host_tx,
            config: &mut config,
            http_client: &client,
            cancel: CancellationToken::new(),
            bg_cancel: CancellationToken::new(),
            history: vec![HistoryItem::system("old prompt")],
            history_coordinates: protocol::ModelHistoryCoordinates::canonical(),
            mode: AgentMode::normal(),
            reasoning_effort: ReasoningEffort::Off,
            fast_mode: false,
            turn_id: 1,
            model_target: model_target(),
            target_revision: 0,
            request_config: RequestRuntimeConfig::default(),
            system_prompt: "old prompt".into(),
            tools: Vec::new(),
            permission_overrides: None,
            pending_history_items: Vec::new(),
            next_history_changed_from: 0,
            history_revision: 0,
            session_id: "s".into(),
            persistence: protocol::PersistenceScope::default(),
            started_at: Instant::now(),
            tps_samples: Vec::new(),
        };

        cmd_tx
            .send(UiCommand::UpdateAgentProjectContext(Box::new(
                protocol::AgentProjectContext {
                    cwd: std::path::PathBuf::from("/first-target"),
                    instructions: Some("first instructions".into()),
                    skill_section: None,
                    system_prompt_override: None,
                    system_prompt: "first project prompt".into(),
                    tools: Vec::new(),
                },
            )))
            .unwrap();
        let mut switched_target = model_target();
        switched_target.model = "switched-model".into();
        cmd_tx
            .send(UiCommand::SetTurnModel {
                target: Box::new(switched_target),
                system_prompt: "model prompt with Lua fragment".into(),
            })
            .unwrap();
        cmd_tx
            .send(UiCommand::UpdateAgentProjectContext(Box::new(
                protocol::AgentProjectContext {
                    cwd: std::path::PathBuf::from("/target"),
                    instructions: Some("target instructions".into()),
                    skill_section: Some("target skills".into()),
                    system_prompt_override: None,
                    system_prompt: "target prompt with Lua fragment".into(),
                    tools: vec![target_tool],
                },
            )))
            .unwrap();
        cmd_tx
            .send(UiCommand::ToolResult {
                request_id: 7,
                invocation_id: protocol::InvocationId::new(1),
                call_id: "switch".into(),
                content: "cwd: /target".into(),
                is_error: false,
                metadata: None,
                display_content: Vec::new(),
                attachment: None,
            })
            .unwrap();

        let mut deferred_turn_cmds = Vec::new();
        let result = turn.wait_for_tool_result(7, &mut deferred_turn_cmds).await;

        assert_eq!(
            result,
            Some(("cwd: /target".into(), false, None, Vec::new(), None))
        );
        assert!(deferred_turn_cmds.is_empty());
        assert_eq!(turn.config.cwd, std::path::PathBuf::from("/target"));
        assert_eq!(
            turn.config.instructions.as_deref(),
            Some("target instructions")
        );
        assert_eq!(turn.model_target.model, "switched-model");
        assert_eq!(turn.system_prompt, "target prompt with Lua fragment");
        assert!(matches!(
            &turn.history[0],
            HistoryItem::System { content }
                if content.text_content() == "target prompt with Lua fragment"
        ));
        assert_eq!(turn.tools.len(), 1);
        assert_eq!(turn.tools[0].name, "target_tool");
    }

    #[tokio::test]
    async fn checkpointed_start_turn_emits_canonical_typed_note_update() {
        let note = protocol::HistoryNote::process_status_event(
            protocol::ProcessStatusEvent::background_process_completed(
                "751225",
                Some(1),
                protocol::JobTermination::Exited,
            ),
        );
        let mut config = test_engine_config(std::sync::Arc::new(crate::clock::RealClock));
        config.host_callbacks = crate::HostCallbacks::Disabled;
        let mut handle = crate::start(config, Box::new(crate::tools::EmptyDispatcher::new()));

        loop {
            match tokio::time::timeout(std::time::Duration::from_secs(1), handle.recv())
                .await
                .expect("engine ready")
            {
                Some(EngineEvent::Ready) => break,
                Some(_) => continue,
                None => panic!("engine closed before Ready"),
            }
        }

        let prior = HistoryItem::User {
            content: Content::text("prior"),
            display: None,
            command: false,
        };
        handle.send(UiCommand::StartTurn(Box::new(protocol::StartTurnPayload {
            turn_id: 1,
            input: protocol::StartTurnInput::note(note.clone()),
            mode: AgentMode::normal(),
            model_target: model_target(),
            request_config: RequestRuntimeConfig::default(),
            reasoning_effort: ReasoningEffort::Off,
            fast_mode: false,
            history: protocol::ModelHistorySource::projected_items(
                vec![
                    HistoryItem::user(Content::text("SUMMARY:\ncheckpoint")),
                    prior.clone(),
                ],
                protocol::ModelHistoryCoordinates::projected(1, 24),
            ),
            session_id: "s".into(),
            sessions_root: std::path::PathBuf::from("/tmp"),
            persistence: protocol::PersistenceScope::default(),
            permission_overrides: None,
            system_prompt: Some("sys".into()),
            tools: Vec::new(),
        })));

        loop {
            match tokio::time::timeout(std::time::Duration::from_secs(1), handle.recv())
                .await
                .expect("history snapshot")
            {
                Some(EngineEvent::HistoryUpdated { update, .. }) => {
                    assert_eq!(update.items, vec![HistoryItem::note(note)]);
                    assert_eq!(update.first_index.get(), 25);
                    break;
                }
                Some(_) => continue,
                None => panic!("engine closed before HistoryUpdated"),
            }
        }
        handle.send(UiCommand::Cancel);
    }

    // ---- next_request_id ----

    #[test]
    fn next_request_id_returns_monotonically_increasing_values() {
        let a = next_request_id();
        let b = next_request_id();
        let c = next_request_id();
        assert!(b > a);
        assert!(c > b);
    }

    // ---- build_provider ----

    #[test]
    fn build_provider_uses_the_complete_target() {
        let target = ModelTarget {
            api_base: "https://target.example/v1/".into(),
            api_key: "target-key".into(),
            config: ModelConfig {
                temperature: Some(0.9),
                top_k: Some(7),
                tool_calling: Some(false),
                ..Default::default()
            },
            ..model_target()
        };
        let provider = build_provider(
            &target,
            &reqwest::Client::new(),
            std::sync::Arc::new(crate::clock::RealClock),
        );

        assert_eq!(provider.api_base(), "https://target.example/v1");
        assert_eq!(provider.api_key(), "target-key");
        assert_eq!(provider.model_config(), &target.config);
        assert!(!provider.tool_calling());
    }

    // ---- send_usage ----

    #[test]
    fn send_usage_emits_token_usage_event_with_cost_when_pricing_resolves() {
        let (tx, mut rx) = test_event_channel();
        let target = ModelTarget {
            model: "model-x".into(),
            config: ModelConfig {
                input_cost: Some(5.0),
                output_cost: Some(10.0),
                ..Default::default()
            },
            ..model_target()
        };
        let usage = protocol::TokenUsage {
            context_tokens: Some(1_500_000),
            prompt_tokens: Some(1_000_000),
            completion_tokens: Some(500_000),
            cache_read_tokens: None,
            cache_write_tokens: None,
            reasoning_tokens: None,
        };
        send_usage(&tx, &target, usage, Some(50.0), false);
        match rx.try_recv().unwrap() {
            EngineEvent::TokenUsage {
                cost_usd,
                tokens_per_sec,
                background,
                ..
            } => {
                assert!(cost_usd.is_some());
                assert_eq!(tokens_per_sec, Some(50.0));
                assert!(!background);
            }
            _ => panic!("expected TokenUsage"),
        }
    }

    #[test]
    fn send_usage_emits_no_cost_when_pricing_zero() {
        let (tx, mut rx) = test_event_channel();
        let target = ModelTarget {
            provider_type: "openai-compatible".into(),
            config: ModelConfig::default(),
            ..model_target()
        };
        let usage = protocol::TokenUsage::default();
        send_usage(&tx, &target, usage, None, false);
        match rx.try_recv().unwrap() {
            EngineEvent::TokenUsage { cost_usd, .. } => assert!(cost_usd.is_none()),
            _ => panic!("expected TokenUsage"),
        }
    }

    #[test]
    fn send_usage_can_mark_background_requests() {
        let (tx, mut rx) = test_event_channel();
        send_usage(
            &tx,
            &model_target(),
            protocol::TokenUsage::default(),
            None,
            true,
        );
        match rx.try_recv().unwrap() {
            EngineEvent::TokenUsage {
                background,
                tokens_per_sec,
                ..
            } => {
                assert!(background);
                assert!(tokens_per_sec.is_none());
            }
            _ => panic!("expected TokenUsage"),
        }
    }
}
