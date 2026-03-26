use crate::log;
use crate::permissions::{Decision, Permissions};
use crate::provider::{self, Provider, ProviderError, ToolDefinition};
use crate::tools::{self, ToolContext, ToolRegistry, ToolResult};
use crate::EngineConfig;
use protocol::{
    Content, EngineEvent, Message, Mode, ReasoningEffort, Role, ToolOutcome, TurnMeta, UiCommand,
};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use tokio::sync::mpsc;

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

fn next_request_id() -> u64 {
    NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed)
}

/// Main engine task. Runs in a tokio::spawn and processes commands/events.
pub async fn engine_task(
    mut config: EngineConfig,
    registry: ToolRegistry,
    processes: tools::ProcessRegistry,
    mut cmd_rx: mpsc::UnboundedReceiver<UiCommand>,
    event_tx: mpsc::UnboundedSender<EngineEvent>,
) {
    let client = reqwest::Client::new();
    let file_locks = tools::FileLocks::default();

    let _ = event_tx.send(EngineEvent::Ready);

    // Process completion channel for background processes
    let (proc_done_tx, mut proc_done_rx) = mpsc::unbounded_channel::<(String, Option<i32>)>();

    loop {
        tokio::select! {
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    UiCommand::StartTurn { turn_id, content: input_content, mode, model, reasoning_effort, history, api_base, api_key, session_id, session_dir, model_config_overrides, permission_overrides } => {

                        let mut provider = build_provider_with_overrides(
                            &config, &client,
                            api_base.as_deref(), api_key.as_deref(),
                        );
                        if let Some(overrides) = model_config_overrides {
                            provider.apply_model_overrides(&overrides);
                        }
                        let turn_permissions: Permissions;
                        let perm_ref: &Permissions = if let Some(ref perm_ovr) = permission_overrides {
                            turn_permissions = config.permissions.with_overrides(perm_ovr);
                            &turn_permissions
                        } else {
                            &config.permissions
                        };
                        let system_prompt = config.system_prompt_override.clone().unwrap_or_else(|| {
                            let agent_config = if let Some(ref ma) = config.multi_agent {
                                let scope = config.cwd.to_string_lossy();
                                let my_pid = std::process::id();
                                let my_entry = crate::registry::read_entry(my_pid).ok();
                                let agent_id = my_entry
                                    .as_ref()
                                    .map(|e| e.agent_id.clone())
                                    .unwrap_or_default();
                                let parent_id = ma.parent_pid.and_then(|ppid| {
                                    crate::registry::read_entry(ppid)
                                        .ok()
                                        .map(|e| e.agent_id)
                                });
                                let siblings = if ma.depth > 0 {
                                    let entries = crate::registry::discover(&scope);
                                    entries
                                        .iter()
                                        .filter(|e| e.pid != my_pid && e.parent_pid == ma.parent_pid)
                                        .map(|e| e.agent_id.clone())
                                        .collect()
                                } else {
                                    vec![]
                                };
                                Some(crate::AgentPromptConfig {
                                    agent_id,
                                    depth: ma.depth,
                                    parent_id,
                                    siblings,
                                })
                            } else {
                                None
                            };
                            crate::build_system_prompt_full(
                                mode,
                                &config.cwd,
                                config.instructions.as_deref(),
                                agent_config.as_ref(),
                            )
                        });
                        let mut turn = Turn {
                            provider,
                            registry: &registry,
                            permissions: perm_ref,
                            processes: &processes,
                            proc_done_tx: &proc_done_tx,
                            cmd_rx: &mut cmd_rx,
                            event_tx: &event_tx,
                            config: &config,
                            http_client: &client,
                            cancel: crate::cancel::CancellationToken::new(),
                            messages: Vec::new(),
                            mode,
                            reasoning_effort,
                            turn_id,
                            model,
                            system_prompt,
                            session_id,
                            session_dir,
                            started_at: Instant::now(),
                            tps_samples: Vec::new(),
                            tool_elapsed: HashMap::new(),
                            file_locks: &file_locks,
                        };
                        turn.run(input_content, history).await;
                    }
                    UiCommand::Compact { keep_turns, history, model, focus } => {
                        let provider = build_provider(&config, &client);
                        let cancel = crate::cancel::CancellationToken::new();
                        match compact_history(&provider, &history, keep_turns, &model, focus.as_deref(), &cancel).await {
                            Ok(messages) => {
                                let _ = event_tx.send(EngineEvent::CompactionComplete { messages });
                            }
                            Err(e) => {
                                let msg = match e {
                                    ProviderError::QuotaExceeded(_) => {
                                        "API quota exceeded — check your plan and billing details".to_string()
                                    }
                                    _ => format!("compaction failed: {}", e.to_string().replace('\n', " ")),
                                };
                                let _ = event_tx.send(EngineEvent::TurnError { message: msg });
                            }
                        }
                    }
                    UiCommand::GenerateTitle { user_messages, model, api_base, api_key } => {
                        spawn_title_generation(&config, &client, &model, user_messages, api_base, api_key, &event_tx);
                    }
                    UiCommand::Btw { question, history, model, reasoning_effort, api_base, api_key } => {
                        spawn_btw_request(&config, &client, &model, reasoning_effort, question, history, api_base, api_key, &event_tx);
                    }
                    UiCommand::PredictInput { history, model, api_base, api_key, generation } => {
                        spawn_predict_request(&config, &client, &model, history, api_base, api_key, &event_tx, generation);
                    }
                    UiCommand::SetModel { provider_type, .. } => {
                        config.provider_type = provider_type;
                    }
                    _ => {} // Steer, Cancel, etc. only relevant during a turn
                }
            }
            Some((id, exit_code)) = proc_done_rx.recv() => {
                let _ = event_tx.send(EngineEvent::ProcessCompleted { id, exit_code });
            }
            else => break,
        }
    }

    let _ = event_tx.send(EngineEvent::Shutdown { reason: None });
}

/// Spawn title generation as a background task so it doesn't block the engine
/// loop or get swallowed by a running turn.
fn spawn_title_generation(
    config: &EngineConfig,
    client: &reqwest::Client,
    model: &str,
    user_messages: Vec<String>,
    api_base: Option<String>,
    api_key: Option<String>,
    event_tx: &mpsc::UnboundedSender<EngineEvent>,
) {
    let provider =
        build_provider_with_overrides(config, client, api_base.as_deref(), api_key.as_deref());
    let model = model.to_string();
    let tx = event_tx.clone();
    tokio::spawn(async move {
        log::entry(
            log::Level::Info,
            "title_request",
            &serde_json::json!({"message_count": user_messages.len(), "model": &model}),
        );
        match provider.complete_title(&user_messages, &model).await {
            Ok((ref title, ref slug)) => {
                log::entry(
                    log::Level::Info,
                    "title_result",
                    &serde_json::json!({"title": title, "slug": slug}),
                );
                let _ = tx.send(EngineEvent::TitleGenerated {
                    title: title.clone(),
                    slug: slug.clone(),
                });
            }
            Err(ref e) => {
                log::entry(
                    log::Level::Warn,
                    "title_error",
                    &serde_json::json!({"error": e.to_string()}),
                );
                if matches!(e, ProviderError::QuotaExceeded(_)) {
                    let _ = tx.send(EngineEvent::TurnError {
                        message: "API quota exceeded — check your plan and billing details"
                            .to_string(),
                    });
                    return;
                }
                let fallback = user_messages
                    .last()
                    .and_then(|m| m.lines().next())
                    .unwrap_or("Untitled");
                let mut title = fallback.to_string();
                if title.len() > 48 {
                    title.truncate(title.floor_char_boundary(48));
                }
                let title = title.trim().to_string();
                let slug = provider::slugify(&title);
                let _ = tx.send(EngineEvent::TitleGenerated { title, slug });
            }
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn spawn_btw_request(
    config: &EngineConfig,
    client: &reqwest::Client,
    model: &str,
    reasoning_effort: protocol::ReasoningEffort,
    question: String,
    history: Vec<protocol::Message>,
    api_base: Option<String>,
    api_key: Option<String>,
    event_tx: &mpsc::UnboundedSender<EngineEvent>,
) {
    let provider =
        build_provider_with_overrides(config, client, api_base.as_deref(), api_key.as_deref());
    let model = model.to_string();
    let tx = event_tx.clone();
    tokio::spawn(async move {
        let cancel = crate::cancel::CancellationToken::new();

        let mut messages = Vec::with_capacity(history.len() + 2);
        messages.push(protocol::Message::system(
            "You are a helpful assistant. The user is asking a quick side question \
             while working on something else. Answer concisely and directly. \
             You have the conversation history for context.",
        ));
        messages.extend(history);
        messages.push(protocol::Message::user(protocol::Content::text(&question)));

        let content = match provider
            .chat(
                &messages,
                &[],
                &model,
                reasoning_effort,
                &cancel,
                None,
                None,
            )
            .await
        {
            Ok(resp) => resp.content.unwrap_or_default(),
            Err(e) => format!("error: {e}"),
        };
        let _ = tx.send(EngineEvent::BtwResponse { content });
    });
}

#[allow(clippy::too_many_arguments)]
fn spawn_predict_request(
    config: &EngineConfig,
    client: &reqwest::Client,
    model: &str,
    history: Vec<protocol::Message>,
    api_base: Option<String>,
    api_key: Option<String>,
    event_tx: &mpsc::UnboundedSender<EngineEvent>,
    generation: u64,
) {
    let provider =
        build_provider_with_overrides(config, client, api_base.as_deref(), api_key.as_deref());
    let model = model.to_string();
    let tx = event_tx.clone();
    tokio::spawn(async move {
        let system = "You predict what a user will type next in a coding assistant conversation. \
                      Reply with ONLY the predicted message — no quotes, no explanation, \
                      no preamble. Keep it short (one sentence max). If you cannot predict, \
                      reply with an empty string.";

        // Build context from recent user messages + last assistant response.
        let mut context_parts = Vec::new();
        for msg in &history {
            let text = msg
                .content
                .as_ref()
                .map(|c| c.text_content())
                .unwrap_or_default();
            if text.is_empty() {
                continue;
            }
            // Truncate each message to keep the request small.
            let truncated = if text.len() > 500 {
                &text[text.floor_char_boundary(text.len() - 500)..]
            } else {
                &text
            };
            let label = if msg.role == protocol::Role::User {
                "User"
            } else {
                "Assistant"
            };
            context_parts.push(format!("{label}: {truncated}"));
        }

        let user_msg = format!(
            "Recent conversation:\n\n{}\n\nPredict the user's next message.",
            context_parts.join("\n\n")
        );

        let messages = vec![
            protocol::Message::system(system),
            protocol::Message::user(protocol::Content::text(&user_msg)),
        ];

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            provider.complete_predict(&messages, &model),
        )
        .await;
        if let Ok(Ok(text)) = result {
            let text = text.trim().to_string();
            if !text.is_empty() {
                let _ = tx.send(EngineEvent::InputPrediction { text, generation });
            }
        }
    });
}

fn build_provider(config: &EngineConfig, client: &reqwest::Client) -> Provider {
    build_provider_with_overrides(config, client, None, None)
}

fn build_provider_with_overrides(
    config: &EngineConfig,
    client: &reqwest::Client,
    api_base: Option<&str>,
    api_key: Option<&str>,
) -> Provider {
    Provider::new(
        api_base.unwrap_or(&config.api_base).to_string(),
        api_key.unwrap_or(&config.api_key).to_string(),
        &config.provider_type,
        client.clone(),
    )
    .with_model_config(config.model_config.clone())
}

// ── Turn ────────────────────────────────────────────────────────────────────

/// Encapsulates the state of a single agent turn.
struct Turn<'a> {
    provider: Provider,
    registry: &'a ToolRegistry,
    permissions: &'a Permissions,
    processes: &'a tools::ProcessRegistry,
    proc_done_tx: &'a mpsc::UnboundedSender<(String, Option<i32>)>,
    cmd_rx: &'a mut mpsc::UnboundedReceiver<UiCommand>,
    event_tx: &'a mpsc::UnboundedSender<EngineEvent>,
    config: &'a EngineConfig,
    http_client: &'a reqwest::Client,
    cancel: crate::cancel::CancellationToken,
    file_locks: &'a tools::FileLocks,
    messages: Vec<Message>,
    mode: Mode,
    reasoning_effort: ReasoningEffort,
    turn_id: u64,
    model: String,
    system_prompt: String,
    session_id: String,
    session_dir: PathBuf,
    started_at: Instant,
    tps_samples: Vec<f64>,
    tool_elapsed: HashMap<String, u64>,
}

impl<'a> Turn<'a> {
    fn emit(&self, event: EngineEvent) {
        let _ = self.event_tx.send(event);
    }

    fn emit_messages_snapshot(&self) {
        // Only subagents consume Messages snapshots. Interactive mode ignores
        // them, so skip the expensive clone of the entire conversation history.
        if self.config.interactive {
            return;
        }
        let mut messages = self.messages.clone();
        if messages
            .first()
            .is_some_and(|m| matches!(m.role, Role::System))
        {
            messages.remove(0);
        }
        self.emit(EngineEvent::Messages {
            turn_id: self.turn_id,
            messages,
        });
    }

    fn commit_partial_assistant(&mut self, text: String, reasoning: String) {
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
            self.messages
                .push(Message::assistant(content, reasoning, None));
        }
    }

    fn emit_turn_complete(&mut self, interrupted: bool) {
        let meta = self.build_meta(interrupted);
        self.messages.remove(0);
        let msgs = std::mem::take(&mut self.messages);
        self.emit(EngineEvent::TurnComplete {
            turn_id: self.turn_id,
            messages: msgs,
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
        TurnMeta {
            elapsed_ms: self.started_at.elapsed().as_millis() as u64,
            avg_tps,
            interrupted,
            tool_elapsed: self.tool_elapsed.clone(),
        }
    }

    fn apply_model_change(
        &mut self,
        model: String,
        api_base: String,
        api_key: String,
        provider_type: String,
    ) {
        self.model = model;
        self.provider = Provider::new(api_base, api_key, &provider_type, self.http_client.clone())
            .with_model_config(self.config.model_config.clone());
    }

    /// Handle a command that arrived during a turn but isn't turn-specific.
    /// Returns true if the command was handled (caller should not fall through).
    fn handle_background_cmd(&self, cmd: UiCommand) -> bool {
        match cmd {
            UiCommand::GenerateTitle {
                user_messages,
                model,
                api_base,
                api_key,
            } => {
                spawn_title_generation(
                    self.config,
                    self.http_client,
                    &model,
                    user_messages,
                    api_base,
                    api_key,
                    self.event_tx,
                );
                true
            }
            UiCommand::Btw {
                question,
                history,
                model,
                reasoning_effort,
                api_base,
                api_key,
            } => {
                spawn_btw_request(
                    self.config,
                    self.http_client,
                    &model,
                    reasoning_effort,
                    question,
                    history,
                    api_base,
                    api_key,
                    self.event_tx,
                );
                true
            }
            _ => false,
        }
    }

    /// Apply a turn-local command immediately to in-memory state.
    /// Returns true if the command was consumed here.
    fn handle_turn_cmd(&mut self, cmd: UiCommand) -> bool {
        match cmd {
            UiCommand::Steer { text } => {
                self.emit(EngineEvent::Steered {
                    text: text.clone(),
                    count: 1,
                });
                self.messages.push(Message::user(Content::text(text)));
                self.emit_messages_snapshot();
                true
            }
            UiCommand::Unsteer { count } => {
                for _ in 0..count {
                    if let Some(pos) = self.messages.iter().rposition(|m| m.role == Role::User) {
                        self.messages.remove(pos);
                    }
                }
                self.emit_messages_snapshot();
                true
            }
            UiCommand::SetMode { mode } => {
                self.mode = mode;
                true
            }
            UiCommand::SetReasoningEffort { effort } => {
                self.reasoning_effort = effort;
                true
            }
            UiCommand::SetModel {
                model,
                api_base,
                api_key,
                provider_type,
            } => {
                self.apply_model_change(model, api_base, api_key, provider_type);
                true
            }
            UiCommand::Cancel => {
                self.cancel.cancel();
                true
            }
            UiCommand::AgentMessage {
                from_id,
                from_slug,
                message,
            } => {
                // Don't re-emit EngineEvent::AgentMessage here — the TUI
                // already rendered the block when the socket bridge first
                // delivered the event. Just inject into conversation history
                // so the LLM sees it on the next API call.
                self.messages
                    .push(Message::agent(&from_id, &from_slug, &message));
                self.emit_messages_snapshot();
                true
            }
            other => self.handle_background_cmd(other),
        }
    }

    /// Main agentic loop for a single turn.
    async fn run(&mut self, content: Content, history: Vec<Message>) {
        self.messages = Vec::with_capacity(history.len() + 2);
        self.messages.push(Message::system(&self.system_prompt));
        self.messages.extend(history);

        if !content.is_empty() {
            self.messages.push(Message::user(content));
        }
        self.emit_messages_snapshot();

        let mut first = true;
        let mut empty_retries: u8 = 0;
        const MAX_EMPTY_RETRIES: u8 = 2;

        loop {
            if !first {
                self.drain_commands();
            }
            first = false;

            // Recompute tool definitions each iteration — mode may have
            // changed (e.g. Plan → Apply after plan approval).
            let tool_defs: Vec<ToolDefinition> = if self.provider.tool_calling() {
                self.registry
                    .definitions(self.permissions, self.mode, self.config.interactive)
            } else {
                Vec::new()
            };

            if self.cancel.is_cancelled() {
                self.emit_turn_complete(true);
                return;
            }

            // Call LLM with cancel monitoring
            let (result, partial_text, partial_reasoning) = self.call_llm(&tool_defs).await;
            let (resp, had_injected) = match result {
                Ok(r) => r,
                Err(ProviderError::Cancelled) => {
                    self.commit_partial_assistant(partial_text, partial_reasoning);
                    self.emit_turn_complete(true);
                    return;
                }
                Err(ProviderError::QuotaExceeded(ref body)) => {
                    log::entry(
                        log::Level::Warn,
                        "agent_stop",
                        &serde_json::json!({"reason": "quota_exceeded", "error": body}),
                    );
                    self.emit_turn_complete(false);
                    self.emit(EngineEvent::TurnError {
                        message: "API quota exceeded — check your plan and billing details"
                            .to_string(),
                    });
                    return;
                }
                Err(e) => {
                    let error_msg = e.to_string().replace('\n', " ");
                    log::entry(
                        log::Level::Warn,
                        "agent_stop",
                        &serde_json::json!({"reason": "llm_error", "error": error_msg.clone()}),
                    );
                    // Send final history so the TUI can persist tool results
                    // accumulated before the error.
                    self.emit_turn_complete(false);
                    self.emit(EngineEvent::TurnError { message: error_msg });
                    return;
                }
            };

            if let Some(tokens) = resp.prompt_tokens {
                let tokens_per_sec = resp.tokens_per_sec;
                if let Some(tps) = tokens_per_sec {
                    self.tps_samples.push(tps);
                }
                self.emit(EngineEvent::TokenUsage {
                    prompt_tokens: tokens,
                    completion_tokens: resp.completion_tokens,
                    tokens_per_sec,
                });
            }

            let content = resp.content.map(Content::text);
            let tool_calls = resp.tool_calls;
            let reasoning = resp.reasoning_content;

            // If a message was injected during the LLM call and the LLM
            // produced only text (no tool calls), discard this response —
            // the LLM never saw the injected message. Loop immediately so
            // it gets a chance to respond to the new context.
            if had_injected && tool_calls.is_empty() {
                continue;
            }

            if let Some(ref reasoning) = reasoning {
                let trimmed = reasoning.trim();
                if !trimmed.is_empty() {
                    self.emit(EngineEvent::Thinking {
                        content: trimmed.to_string(),
                    });
                }
            }

            if let Some(ref content) = content {
                let trimmed = content.as_text().trim();
                if !trimmed.is_empty() {
                    self.emit(EngineEvent::Text {
                        content: trimmed.to_string(),
                    });
                }
            }

            // No tool calls — turn is done.
            if tool_calls.is_empty() {
                let is_empty = content.is_none()
                    && reasoning.is_none()
                    && self
                        .messages
                        .last()
                        .map(|m| m.role == Role::Tool)
                        .unwrap_or(false);

                if is_empty && empty_retries < MAX_EMPTY_RETRIES {
                    empty_retries += 1;
                    log::entry(
                        log::Level::Warn,
                        "empty_response_retry",
                        &serde_json::json!({ "attempt": empty_retries }),
                    );
                    continue;
                }

                self.messages
                    .push(Message::assistant(content, reasoning, None));
                self.emit_messages_snapshot();
                self.emit_turn_complete(false);
                return;
            }

            // Has tool calls — execute them
            empty_retries = 0;
            self.messages.push(Message::assistant(
                content,
                reasoning,
                Some(tool_calls.clone()),
            ));
            self.emit_messages_snapshot();

            // Phase 1: Classify tools (sync permission checks).
            // Allow/Deny are instant; Ask tools are deferred so that
            // already-approved tools can start executing immediately.
            struct ToolSlot<'b> {
                tc: &'b protocol::ToolCall,
                args: HashMap<String, Value>,
                tool: &'b dyn tools::Tool,
                confirm_msg: Option<String>,
                start: Instant,
            }

            let mut slots: Vec<ToolSlot<'_>> = Vec::new();
            let mut ready_indices: Vec<usize> = Vec::new();
            // (slot index, request_id) for tools that need user permission
            let mut pending_perms: Vec<(usize, u64)> = Vec::new();
            let mut sequential_indices: Vec<usize> = Vec::new();

            for tc in &tool_calls {
                self.drain_commands();
                if self.cancel.is_cancelled() {
                    break;
                }

                let args: HashMap<String, Value> =
                    serde_json::from_str(&tc.function.arguments).unwrap_or_default();

                let summary = tools::tool_arg_summary(&tc.function.name, &args);
                let tool_start = Instant::now();
                self.emit(EngineEvent::ToolStarted {
                    call_id: tc.id.clone(),
                    tool_name: tc.function.name.clone(),
                    args: args.clone(),
                    summary,
                });

                let tool = match self.registry.get(&tc.function.name) {
                    Some(t) => t,
                    None => {
                        self.push_tool_result(
                            &tc.id,
                            &format!("unknown tool: {}", tc.function.name),
                            true,
                            Some(tool_start),
                        );
                        continue;
                    }
                };

                // Plan-mode auto-allow for plan files.
                let plan_auto = self.mode == Mode::Plan
                    && tc.function.name == "edit_file"
                    && args
                        .get("file_path")
                        .and_then(|v| v.as_str())
                        .is_some_and(|p| crate::plan::is_plan_file(&self.session_dir, p));

                let decision = if plan_auto {
                    Decision::Allow
                } else {
                    self.permissions.decide(self.mode, &tc.function.name, &args)
                };

                let idx = slots.len();
                match decision {
                    Decision::Allow => {
                        slots.push(ToolSlot {
                            tc,
                            args,
                            tool,
                            confirm_msg: None,
                            start: tool_start,
                        });
                        if tc.function.name == "ask_user_question" {
                            sequential_indices.push(idx);
                        } else {
                            ready_indices.push(idx);
                        }
                    }
                    Decision::Deny => {
                        self.push_tool_result(
                            &tc.id,
                            "The user's permission settings blocked this tool call. \
                             Try a different approach or ask the user for guidance.",
                            false,
                            None,
                        );
                    }
                    Decision::Ask => {
                        let desc = tool
                            .needs_confirm(&args)
                            .unwrap_or_else(|| tc.function.name.clone());
                        let approval_patterns = tool.approval_patterns(&args);
                        let cmd_summary = if tc.function.name == "bash" {
                            let d = tools::str_arg(&args, "description");
                            if d.is_empty() {
                                None
                            } else {
                                Some(d)
                            }
                        } else {
                            None
                        };
                        let request_id = next_request_id();
                        self.emit(EngineEvent::RequestPermission {
                            request_id,
                            call_id: tc.id.clone(),
                            tool_name: tc.function.name.clone(),
                            args: args.clone(),
                            confirm_message: desc,
                            approval_patterns,
                            summary: cmd_summary,
                        });
                        slots.push(ToolSlot {
                            tc,
                            args,
                            tool,
                            confirm_msg: None,
                            start: tool_start,
                        });
                        pending_perms.push((idx, request_id));
                    }
                }
            }

            // Phase 2: Execute approved tools concurrently while resolving
            // pending permissions. Tools that get approved mid-flight are
            // launched immediately alongside already-running tools.
            //
            // contexts/futs are scoped so they drop before we need &mut self
            // again (for handle_turn_cmd, push_tool_result, ask_user).
            let mut completed: Vec<Option<ToolResult>> = (0..slots.len()).map(|_| None).collect();

            let (cancelled, deferred_tool_cmds) = {
                use futures_util::stream::StreamExt;

                type TaggedFut<'x> = std::pin::Pin<
                    Box<dyn std::future::Future<Output = (usize, ToolResult)> + Send + 'x>,
                >;

                let contexts: Vec<_> = slots
                    .iter()
                    .map(|s| ToolContext {
                        event_tx: self.event_tx,
                        call_id: &s.tc.id,
                        cancel: &self.cancel,
                        processes: self.processes,
                        proc_done_tx: self.proc_done_tx,
                        provider: &self.provider,
                        model: &self.model,
                        session_id: &self.session_id,
                        session_dir: &self.session_dir,
                        file_locks: self.file_locks,
                    })
                    .collect();

                let mut futs: futures_util::stream::FuturesUnordered<TaggedFut<'_>> =
                    futures_util::stream::FuturesUnordered::new();

                for &i in &ready_indices {
                    let fut = slots[i].tool.execute(slots[i].args.clone(), &contexts[i]);
                    futs.push(Box::pin(async move { (i, fut.await) }));
                }

                let mut outstanding = ready_indices.len() + pending_perms.len();
                let cancel = &self.cancel;
                let cmd_rx = &mut self.cmd_rx;
                let mut deferred: Vec<UiCommand> = Vec::new();

                let cancelled = loop {
                    if outstanding == 0 {
                        break false;
                    }
                    tokio::select! {
                        Some((idx, result)) = futs.next(), if !futs.is_empty() => {
                            completed[idx] = Some(result);
                            outstanding -= 1;
                        }
                        _ = cancel.cancelled() => break true,
                        Some(cmd) = cmd_rx.recv() => match cmd {
                            UiCommand::Cancel => cancel.cancel(),
                            UiCommand::PermissionDecision {
                                request_id,
                                approved,
                                message,
                            } => {
                                if let Some(pos) = pending_perms
                                    .iter()
                                    .position(|(_, rid)| *rid == request_id)
                                {
                                    let (idx, _) = pending_perms.swap_remove(pos);
                                    if approved {
                                        slots[idx].confirm_msg = message;
                                        let fut = slots[idx]
                                            .tool
                                            .execute(slots[idx].args.clone(), &contexts[idx]);
                                        futs.push(Box::pin(async move { (idx, fut.await) }));
                                    } else {
                                        let denial = if let Some(ref msg) = message {
                                            format!(
                                                "The user denied this tool call with message: {msg}"
                                            )
                                        } else {
                                            "The user denied this tool call. Try a different \
                                             approach or ask the user for guidance."
                                                .to_string()
                                        };
                                        completed[idx] = Some(ToolResult {
                                            content: denial,
                                            is_error: false,
                                            metadata: None,
                                        });
                                        outstanding -= 1;
                                    }
                                }
                            }
                            UiCommand::AgentMessage { .. }
                            | UiCommand::Steer { .. }
                            | UiCommand::Unsteer { .. }
                            | UiCommand::SetMode { .. }
                            | UiCommand::SetReasoningEffort { .. }
                            | UiCommand::SetModel { .. } => deferred.push(cmd),
                            _ => {}
                        },
                    }
                };

                (cancelled, deferred)
            };
            // contexts and futs are now dropped — safe to use &mut self.

            if cancelled {
                for (i, slot) in slots.iter().enumerate() {
                    if completed[i].is_none() {
                        self.push_tool_result(&slot.tc.id, "cancelled", true, Some(slot.start));
                    }
                }
            }

            // Phase 2b: Execute sequential tools (ask_user_question).
            if !cancelled {
                for &i in &sequential_indices {
                    let result = self.ask_user(&slots[i].args).await;
                    completed[i] = Some(result);
                }
            }

            // Phase 3: Collect results.
            for (i, slot) in slots.iter().enumerate() {
                let result = match completed[i].take() {
                    Some(r) => r,
                    None => continue,
                };
                let ToolResult {
                    content,
                    is_error,
                    metadata,
                } = result;

                log::entry(
                    log::Level::Debug,
                    "tool_result",
                    &serde_json::json!({
                        "tool": slot.tc.function.name,
                        "id": slot.tc.id,
                        "is_error": is_error,
                        "content_len": content.len(),
                        "content_preview": &content[..content.floor_char_boundary(500)],
                    }),
                );

                let elapsed_ms = slot.start.elapsed().as_millis() as u64;
                self.tool_elapsed.insert(slot.tc.id.clone(), elapsed_ms);
                let mut tool_content = content.clone();
                if let Some(ref msg) = slot.confirm_msg {
                    tool_content.push_str(&format!("\n\nUser message: {msg}"));
                }
                self.messages
                    .push(Message::tool(slot.tc.id.clone(), tool_content, is_error));
                self.emit_messages_snapshot();
                self.emit(EngineEvent::ToolFinished {
                    call_id: slot.tc.id.clone(),
                    result: ToolOutcome {
                        content,
                        is_error,
                        metadata,
                    },
                    elapsed_ms: Some(elapsed_ms),
                });
            }

            // Process deferred Steer/AgentMessage commands AFTER tool results
            // are in history, so steered user messages appear in the correct
            // chronological position.
            for cmd in deferred_tool_cmds {
                self.handle_turn_cmd(cmd);
            }
        }
    }

    /// Drain pending commands (steering, mode changes, cancel).
    fn drain_commands(&mut self) {
        while let Ok(cmd) = self.cmd_rx.try_recv() {
            self.handle_turn_cmd(cmd);
        }
    }

    /// Call the LLM, monitoring cmd_rx for Cancel during the request.
    /// Returns (response, had_injected_messages). The bool is true when
    /// Steer or AgentMessage commands arrived during the LLM call and were
    /// injected into conversation history — the caller should continue the
    /// loop instead of ending the turn.
    async fn call_llm(
        &mut self,
        tool_defs: &[ToolDefinition],
    ) -> (Result<(crate::provider::LLMResponse, bool), ProviderError>, String, String) {
        // The chat future borrows self.provider and self.model, so model
        // changes received mid-request are deferred until the future resolves.
        let mut pending_model: Option<(String, String, String, String)> = None;
        let mut deferred_turn_cmds: Vec<UiCommand> = Vec::new();

        let partial_text = std::sync::Mutex::new(String::new());
        let partial_reasoning = std::sync::Mutex::new(String::new());

        let result = {
            let on_retry = |delay: std::time::Duration, attempt: u32| {
                let _ = self.event_tx.send(EngineEvent::Retrying {
                    delay_ms: delay.as_millis() as u64,
                    attempt,
                });
            };
            let on_delta = |delta: provider::StreamDelta| match delta {
                provider::StreamDelta::Text(text) => {
                    partial_text.lock().unwrap().push_str(text);
                    let _ = self.event_tx.send(EngineEvent::TextDelta {
                        delta: text.to_string(),
                    });
                }
                provider::StreamDelta::Thinking(text) => {
                    partial_reasoning.lock().unwrap().push_str(text);
                    let _ = self.event_tx.send(EngineEvent::ThinkingDelta {
                        delta: text.to_string(),
                    });
                }
            };
            let chat_future = self.provider.chat(
                &self.messages,
                tool_defs,
                &self.model,
                self.reasoning_effort,
                &self.cancel,
                Some(&on_retry),
                Some(&on_delta),
            );
            tokio::pin!(chat_future);

            let mut cancel_received = false;
            loop {
                if cancel_received {
                    break (&mut chat_future).await;
                }
                tokio::select! {
                    result = &mut chat_future => break result,
                    Some(cmd) = self.cmd_rx.recv() => match cmd {
                        UiCommand::Cancel => {
                            self.cancel.cancel();
                            cancel_received = true;
                        }
                        UiCommand::SetMode { mode } => self.mode = mode,
                        UiCommand::SetReasoningEffort { effort } => self.reasoning_effort = effort,
                        UiCommand::SetModel { model, api_base, api_key, provider_type } => {
                            pending_model = Some((model, api_base, api_key, provider_type));
                        }
                        UiCommand::Steer { .. }
                        | UiCommand::Unsteer { .. }
                        | UiCommand::AgentMessage { .. } => deferred_turn_cmds.push(cmd),
                        other => {
                            self.handle_background_cmd(other);
                        }
                    },
                }
            }
        };

        let pt = partial_text.into_inner().unwrap_or_default();
        let pr = partial_reasoning.into_inner().unwrap_or_default();

        if let Some((model, api_base, api_key, provider_type)) = pending_model {
            self.apply_model_change(model, api_base, api_key, provider_type);
        }
        let had_injected = deferred_turn_cmds
            .iter()
            .any(|c| matches!(c, UiCommand::Steer { .. } | UiCommand::AgentMessage { .. }));
        for cmd in deferred_turn_cmds {
            self.handle_turn_cmd(cmd);
        }
        (result.map(|r| (r, had_injected)), pt, pr)
    }

    /// Handle the ask_user_question tool by requesting an answer from the TUI.
    async fn ask_user(&mut self, args: &HashMap<String, Value>) -> ToolResult {
        let request_id = next_request_id();
        self.emit(EngineEvent::RequestAnswer {
            request_id,
            args: args.clone(),
        });
        let answer = self.wait_for_answer(request_id).await;
        ToolResult::ok(answer.unwrap_or_else(|| "no response".to_string()))
    }

    /// Wait for a QuestionAnswer matching the given request_id.
    async fn wait_for_answer(&mut self, request_id: u64) -> Option<String> {
        loop {
            match self.cmd_rx.recv().await {
                Some(UiCommand::QuestionAnswer {
                    request_id: id,
                    answer,
                }) if id == request_id => return answer,
                Some(UiCommand::SetMode { mode }) => self.mode = mode,
                Some(UiCommand::SetReasoningEffort { effort }) => self.reasoning_effort = effort,
                Some(UiCommand::SetModel {
                    model,
                    api_base,
                    api_key,
                    provider_type,
                }) => self.apply_model_change(model, api_base, api_key, provider_type),
                Some(UiCommand::Cancel) => {
                    self.cancel.cancel();
                    return None;
                }
                None => return None,
                Some(other) => {
                    self.handle_background_cmd(other);
                }
            }
        }
    }

    fn push_tool_result(
        &mut self,
        tool_call_id: &str,
        content: &str,
        is_error: bool,
        started_at: Option<Instant>,
    ) {
        self.messages
            .push(Message::tool(tool_call_id.to_string(), content, is_error));
        self.emit(EngineEvent::ToolFinished {
            call_id: tool_call_id.to_string(),
            result: ToolOutcome {
                content: content.to_string(),
                is_error,
                metadata: None,
            },
            elapsed_ms: started_at.map(|t| t.elapsed().as_millis() as u64),
        });
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

async fn compact_history(
    provider: &Provider,
    messages: &[Message],
    keep_turns: usize,
    model: &str,
    focus: Option<&str>,
    cancel: &crate::cancel::CancellationToken,
) -> Result<Vec<Message>, ProviderError> {
    let mut user_count = 0;
    let mut cut = messages.len();
    for (i, m) in messages.iter().enumerate().rev() {
        if m.role == Role::User {
            user_count += 1;
            if user_count >= keep_turns {
                cut = i;
                break;
            }
        }
    }
    if cut == 0 || cut >= messages.len() {
        return Err(ProviderError::InvalidResponse(
            "not enough history to compact".into(),
        ));
    }

    let to_summarize = &messages[..cut];
    let summary = provider.compact(to_summarize, model, focus, cancel).await?;

    let mut new_messages = vec![Message::user(Content::text(format!(
        "Summary of prior conversation:\n\n{summary}"
    )))];
    new_messages.extend_from_slice(&messages[cut..]);
    Ok(new_messages)
}
