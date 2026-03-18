use crate::log;
use crate::permissions::{Decision, Permissions};
use crate::provider::{Provider, ProviderError, ToolDefinition};
use crate::tools::{self, ToolContext, ToolRegistry, ToolResult};
use crate::EngineConfig;
use protocol::{
    Content, EngineEvent, Message, Mode, ReasoningEffort, Role, ToolOutcome, UiCommand,
};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::mpsc;

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

fn next_request_id() -> u64 {
    NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed)
}

/// Main engine task. Runs in a tokio::spawn and processes commands/events.
pub async fn engine_task(
    config: EngineConfig,
    registry: ToolRegistry,
    processes: tools::ProcessRegistry,
    mut cmd_rx: mpsc::UnboundedReceiver<UiCommand>,
    event_tx: mpsc::UnboundedSender<EngineEvent>,
) {
    let client = reqwest::Client::new();

    let _ = event_tx.send(EngineEvent::Ready);

    // Process completion channel for background processes
    let (proc_done_tx, mut proc_done_rx) = mpsc::unbounded_channel::<(String, Option<i32>)>();
    let mut last_model = String::new();

    loop {
        tokio::select! {
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    UiCommand::StartTurn { turn_id, input, mode, model, reasoning_effort, history, api_base, api_key, session_id, model_config_overrides, permission_overrides } => {
                        last_model = model.clone();
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
                            cancel: tokio_util::sync::CancellationToken::new(),
                            messages: Vec::new(),
                            mode,
                            reasoning_effort,
                            turn_id,
                            model,
                            system_prompt: &config.system_prompt,
                            session_id,
                        };
                        turn.run(input, history).await;
                    }
                    UiCommand::Compact { keep_turns, history } => {
                        let provider = build_provider(&config, &client);
                        let cancel = tokio_util::sync::CancellationToken::new();
                        match compact_history(&provider, &history, keep_turns, &last_model, &cancel).await {
                            Ok(messages) => {
                                let _ = event_tx.send(EngineEvent::CompactionComplete { messages });
                            }
                            Err(e) => {
                                let _ = event_tx.send(EngineEvent::TurnError { message: e });
                            }
                        }
                    }
                    UiCommand::GenerateTitle { user_messages } => {
                        spawn_title_generation(&config, &client, &last_model, user_messages, &event_tx);
                    }
                    UiCommand::Btw { question, history, model, reasoning_effort, api_base, api_key } => {
                        spawn_btw_request(&config, &client, &model, reasoning_effort, question, history, api_base, api_key, &event_tx);
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
    event_tx: &mpsc::UnboundedSender<EngineEvent>,
) {
    let provider = build_provider(config, client);
    let model = model.to_string();
    let tx = event_tx.clone();
    tokio::spawn(async move {
        log::entry(
            log::Level::Info,
            "title_request",
            &serde_json::json!({"message_count": user_messages.len(), "model": &model}),
        );
        match provider.complete_title(&user_messages, &model).await {
            Ok(ref title) => {
                log::entry(
                    log::Level::Info,
                    "title_result",
                    &serde_json::json!({"title": title}),
                );
                let _ = tx.send(EngineEvent::TitleGenerated {
                    title: title.clone(),
                });
            }
            Err(ref e) => {
                log::entry(
                    log::Level::Warn,
                    "title_error",
                    &serde_json::json!({"error": e}),
                );
                let fallback = user_messages
                    .last()
                    .and_then(|m| m.lines().next())
                    .unwrap_or("Untitled");
                let mut title = fallback.to_string();
                if title.len() > 48 {
                    title.truncate(title.floor_char_boundary(48));
                }
                let _ = tx.send(EngineEvent::TitleGenerated {
                    title: title.trim().to_string(),
                });
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
        let cancel = tokio_util::sync::CancellationToken::new();

        let mut messages = Vec::with_capacity(history.len() + 2);
        messages.push(protocol::Message {
            role: protocol::Role::System,
            content: Some(protocol::Content::text(
                "You are a helpful assistant. The user is asking a quick side question \
                 while working on something else. Answer concisely and directly. \
                 You have the conversation history for context.",
            )),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
        });
        messages.extend(history);
        messages.push(protocol::Message {
            role: protocol::Role::User,
            content: Some(protocol::Content::text(&question)),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
        });

        let content = match provider
            .chat(&messages, &[], &model, reasoning_effort, &cancel, None)
            .await
        {
            Ok(resp) => resp.content.unwrap_or_default(),
            Err(e) => format!("error: {e}"),
        };
        let _ = tx.send(EngineEvent::BtwResponse { content });
    });
}

fn build_provider(config: &EngineConfig, client: &reqwest::Client) -> Provider {
    Provider::new(
        config.api_base.clone(),
        config.api_key.clone(),
        client.clone(),
    )
    .with_model_config(config.model_config.clone())
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
    cancel: tokio_util::sync::CancellationToken,
    messages: Vec<Message>,
    mode: Mode,
    reasoning_effort: ReasoningEffort,
    turn_id: u64,
    model: String,
    system_prompt: &'a str,
    session_id: String,
}

impl<'a> Turn<'a> {
    fn emit(&self, event: EngineEvent) {
        let _ = self.event_tx.send(event);
    }

    fn apply_model_change(&mut self, model: String, api_base: String, api_key: String) {
        self.model = model;
        self.provider = Provider::new(api_base, api_key, self.http_client.clone())
            .with_model_config(self.config.model_config.clone());
    }

    /// Handle a command that arrived during a turn but isn't turn-specific.
    /// Returns true if the command was handled (caller should not fall through).
    fn handle_background_cmd(&self, cmd: UiCommand) -> bool {
        match cmd {
            UiCommand::GenerateTitle { user_messages } => {
                spawn_title_generation(
                    self.config,
                    self.http_client,
                    &self.model,
                    user_messages,
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

    fn send_snapshot(&self) {
        let _ = self.event_tx.send(EngineEvent::Messages {
            turn_id: self.turn_id,
            messages: self.messages[1..].to_vec(),
        });
    }

    /// Main agentic loop for a single turn.
    async fn run(&mut self, input: String, history: Vec<Message>) {
        self.messages = Vec::with_capacity(history.len() + 2);
        self.messages.push(Message {
            role: Role::System,
            content: Some(Content::text(self.system_prompt)),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
        });
        self.messages.extend(history);

        if !input.is_empty() {
            self.messages.push(Message {
                role: Role::User,
                content: Some(Content::text(input)),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: None,
            });
        }

        let tool_defs: Vec<ToolDefinition> = self.registry.definitions(self.permissions, self.mode);
        let mut first = true;
        let mut empty_retries: u8 = 0;
        const MAX_EMPTY_RETRIES: u8 = 2;

        loop {
            if !first {
                self.drain_commands();
            }
            first = false;

            if self.cancel.is_cancelled() {
                self.messages.remove(0);
                let msgs = std::mem::take(&mut self.messages);
                self.emit(EngineEvent::TurnComplete {
                    turn_id: self.turn_id,
                    messages: msgs,
                });
                return;
            }

            // Call LLM with cancel monitoring
            let resp = match self.call_llm(&tool_defs).await {
                Ok(r) => r,
                Err(ProviderError::Cancelled) => {
                    self.messages.remove(0);
                    let msgs = std::mem::take(&mut self.messages);
                    self.emit(EngineEvent::TurnComplete {
                        turn_id: self.turn_id,
                        messages: msgs,
                    });
                    return;
                }
                Err(e) => {
                    log::entry(
                        log::Level::Warn,
                        "agent_stop",
                        &serde_json::json!({"reason": "llm_error", "error": e.to_string()}),
                    );
                    self.send_snapshot();
                    self.emit(EngineEvent::TurnError {
                        message: e.to_string(),
                    });
                    return;
                }
            };

            if let Some(tokens) = resp.prompt_tokens {
                let tokens_per_sec = resp.tokens_per_sec;
                self.emit(EngineEvent::TokenUsage {
                    prompt_tokens: tokens,
                    completion_tokens: resp.completion_tokens,
                    tokens_per_sec,
                });
            }

            if let Some(ref reasoning) = resp.reasoning_content {
                let trimmed = reasoning.trim();
                if !trimmed.is_empty() {
                    self.emit(EngineEvent::Thinking {
                        content: trimmed.to_string(),
                    });
                }
            }

            if let Some(ref content) = resp.content {
                let trimmed = content.trim();
                if !trimmed.is_empty() {
                    self.emit(EngineEvent::Text {
                        content: trimmed.to_string(),
                    });
                }
            }

            let content = resp.content.map(Content::text);
            let tool_calls = resp.tool_calls;
            let reasoning = resp.reasoning_content;

            // No tool calls — turn is done
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

                self.messages.push(Message {
                    role: Role::Assistant,
                    content,
                    reasoning_content: reasoning,
                    tool_calls: None,
                    tool_call_id: None,
                });
                self.send_snapshot();
                self.messages.remove(0);
                let msgs = std::mem::take(&mut self.messages);
                self.emit(EngineEvent::TurnComplete {
                    turn_id: self.turn_id,
                    messages: msgs,
                });
                return;
            }

            // Has tool calls — execute them
            empty_retries = 0;
            self.messages.push(Message {
                role: Role::Assistant,
                content,
                reasoning_content: reasoning,
                tool_calls: Some(tool_calls.clone()),
                tool_call_id: None,
            });
            self.send_snapshot();

            for tc in &tool_calls {
                self.drain_commands();
                if self.cancel.is_cancelled() {
                    break;
                }

                let args: HashMap<String, Value> =
                    serde_json::from_str(&tc.function.arguments).unwrap_or_default();

                let summary = tools::tool_arg_summary(&tc.function.name, &args);
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
                        );
                        continue;
                    }
                };

                // Permission check
                let confirm_msg = match self.check_permission(tool, &tc.function.name, &args).await
                {
                    PermissionResult::Allow(msg) => msg,
                    PermissionResult::Deny(denial) => {
                        self.push_tool_result(&tc.id, &denial, false);
                        continue;
                    }
                };

                // Execute tool
                let ToolResult { content, is_error } = if tc.function.name == "ask_user_question" {
                    self.ask_user(&args).await
                } else {
                    let ctx = ToolContext {
                        event_tx: self.event_tx,
                        call_id: &tc.id,
                        cancel: &self.cancel,
                        processes: self.processes,
                        proc_done_tx: self.proc_done_tx,
                        provider: &self.provider,
                        model: &self.model,
                        session_id: &self.session_id,
                    };
                    tool.execute(args.clone(), &ctx).await
                };

                log::entry(
                    log::Level::Debug,
                    "tool_result",
                    &serde_json::json!({
                        "tool": tc.function.name,
                        "id": tc.id,
                        "is_error": is_error,
                        "content_len": content.len(),
                        "content_preview": &content[..content.floor_char_boundary(500)],
                    }),
                );

                let mut model_content = match tc.function.name.as_str() {
                    "grep" | "glob" => trim_tool_output(&content, 200),
                    _ => content.clone(),
                };
                if let Some(ref msg) = confirm_msg {
                    model_content.push_str(&format!("\n\nUser message: {msg}"));
                }
                self.messages.push(Message {
                    role: Role::Tool,
                    content: Some(Content::text(model_content)),
                    reasoning_content: None,
                    tool_calls: None,
                    tool_call_id: Some(tc.id.clone()),
                });
                self.emit(EngineEvent::ToolFinished {
                    call_id: tc.id.clone(),
                    result: ToolOutcome { content, is_error },
                });
                self.send_snapshot();
            }
        }
    }

    /// Drain pending commands (steering, mode changes, cancel).
    fn drain_commands(&mut self) {
        loop {
            match self.cmd_rx.try_recv() {
                Ok(UiCommand::Steer { text }) => {
                    self.emit(EngineEvent::Steered {
                        text: text.clone(),
                        count: 1,
                    });
                    self.messages.push(Message {
                        role: Role::User,
                        content: Some(Content::text(text)),
                        reasoning_content: None,
                        tool_calls: None,
                        tool_call_id: None,
                    });
                }
                Ok(UiCommand::Unsteer { count }) => {
                    // Remove the last `count` steered user messages.
                    for _ in 0..count {
                        if let Some(pos) = self.messages.iter().rposition(|m| m.role == Role::User)
                        {
                            self.messages.remove(pos);
                        }
                    }
                }
                Ok(UiCommand::SetMode { mode }) => {
                    self.mode = mode;
                }
                Ok(UiCommand::SetReasoningEffort { effort }) => {
                    self.reasoning_effort = effort;
                }
                Ok(UiCommand::SetModel {
                    model,
                    api_base,
                    api_key,
                }) => {
                    self.apply_model_change(model, api_base, api_key);
                }
                Ok(UiCommand::Cancel) => {
                    self.cancel.cancel();
                }
                Ok(other) => {
                    self.handle_background_cmd(other);
                }
                Err(_) => break,
            }
        }
    }

    /// Call the LLM, monitoring cmd_rx for Cancel during the request.
    async fn call_llm(
        &mut self,
        tool_defs: &[ToolDefinition],
    ) -> Result<crate::provider::LLMResponse, ProviderError> {
        // The chat future borrows self.provider and self.model, so model
        // changes received mid-request are deferred until the future resolves.
        let mut pending_model: Option<(String, String, String)> = None;

        let result = {
            let on_retry = |delay: std::time::Duration, attempt: u32| {
                let _ = self.event_tx.send(EngineEvent::Retrying {
                    delay_ms: delay.as_millis() as u64,
                    attempt,
                });
            };
            let chat_future = self.provider.chat(
                &self.messages,
                tool_defs,
                &self.model,
                self.reasoning_effort,
                &self.cancel,
                Some(&on_retry),
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
                        UiCommand::SetModel { model, api_base, api_key } => {
                            pending_model = Some((model, api_base, api_key));
                        }
                        other => { self.handle_background_cmd(other); }
                    },
                }
            }
        };

        if let Some((model, api_base, api_key)) = pending_model {
            self.apply_model_change(model, api_base, api_key);
        }
        result
    }

    /// Check permission and handle the Ask flow.
    async fn check_permission(
        &mut self,
        tool: &dyn tools::Tool,
        tool_name: &str,
        args: &HashMap<String, Value>,
    ) -> PermissionResult {
        // Auto-allow edit_file on plan files in Plan mode.
        if self.mode == Mode::Plan && tool_name == "edit_file" {
            if let Some(path) = args.get("file_path").and_then(|v| v.as_str()) {
                if crate::plan::is_plan_file(&self.session_id, path) {
                    return PermissionResult::Allow(None);
                }
            }
        }

        let decision = self.permissions.decide(self.mode, tool_name, args);

        match decision {
            Decision::Deny => PermissionResult::Deny(
                "The user's permission settings blocked this tool call. Try a different approach or ask the user for guidance.".into()
            ),
            Decision::Allow => PermissionResult::Allow(None),
            Decision::Ask => {
                let desc = tool
                    .needs_confirm(args)
                    .unwrap_or_else(|| tool_name.to_string());
                let approval_pattern = tool.approval_pattern(args);

                let cmd_summary = if tool_name == "bash" {
                    let cmd = tools::str_arg(args, "command");
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(3),
                        self.provider.describe_command(&cmd, &self.model),
                    )
                    .await
                    {
                        Ok(Ok(s)) => Some(s),
                        _ => None,
                    }
                } else {
                    None
                };

                let request_id = next_request_id();
                self.emit(EngineEvent::RequestPermission {
                    request_id,
                    call_id: String::new(),
                    tool_name: tool_name.to_string(),
                    args: args.clone(),
                    confirm_message: desc,
                    approval_pattern,
                    summary: cmd_summary,
                });

                let (approved, user_msg) = self.wait_for_permission(request_id).await;
                if !approved {
                    let denial = if let Some(ref msg) = user_msg {
                        format!("The user denied this tool call with message: {msg}")
                    } else {
                        "The user denied this tool call. Try a different approach or ask the user for guidance.".to_string()
                    };
                    PermissionResult::Deny(denial)
                } else {
                    PermissionResult::Allow(user_msg)
                }
            }
        }
    }

    /// Handle the ask_user_question tool by requesting an answer from the TUI.
    async fn ask_user(&mut self, args: &HashMap<String, Value>) -> ToolResult {
        let request_id = next_request_id();
        self.emit(EngineEvent::RequestAnswer {
            request_id,
            args: args.clone(),
        });
        let answer = self.wait_for_answer(request_id).await;
        ToolResult {
            content: answer.unwrap_or_else(|| "no response".into()),
            is_error: false,
        }
    }

    /// Wait for a PermissionDecision matching the given request_id.
    async fn wait_for_permission(&mut self, request_id: u64) -> (bool, Option<String>) {
        loop {
            match self.cmd_rx.recv().await {
                Some(UiCommand::PermissionDecision {
                    request_id: id,
                    approved,
                    message,
                }) if id == request_id => {
                    return (approved, message);
                }
                Some(UiCommand::SetMode { mode }) => self.mode = mode,
                Some(UiCommand::SetReasoningEffort { effort }) => self.reasoning_effort = effort,
                Some(UiCommand::SetModel {
                    model,
                    api_base,
                    api_key,
                }) => self.apply_model_change(model, api_base, api_key),
                Some(UiCommand::Cancel) => {
                    self.cancel.cancel();
                    return (false, None);
                }
                None => return (false, None),
                Some(other) => {
                    self.handle_background_cmd(other);
                }
            }
        }
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
                }) => self.apply_model_change(model, api_base, api_key),
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

    fn push_tool_result(&mut self, tool_call_id: &str, content: &str, is_error: bool) {
        self.messages.push(Message {
            role: Role::Tool,
            content: Some(Content::text(content)),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: Some(tool_call_id.to_string()),
        });
        self.emit(EngineEvent::ToolFinished {
            call_id: tool_call_id.to_string(),
            result: ToolOutcome {
                content: content.to_string(),
                is_error,
            },
        });
    }
}

enum PermissionResult {
    Allow(Option<String>),
    Deny(String),
}

// ── Helpers ─────────────────────────────────────────────────────────────────

async fn compact_history(
    provider: &Provider,
    messages: &[Message],
    keep_turns: usize,
    model: &str,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Vec<Message>, String> {
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
        return Err("not enough history to compact".into());
    }

    let to_summarize = &messages[..cut];
    let summary = provider.compact(to_summarize, model, cancel).await?;

    let mut new_messages = vec![Message {
        role: Role::System,
        content: Some(Content::text(format!(
            "Summary of prior conversation:\n\n{summary}"
        ))),
        reasoning_content: None,
        tool_calls: None,
        tool_call_id: None,
    }];
    new_messages.extend_from_slice(&messages[cut..]);
    Ok(new_messages)
}

fn trim_tool_output(content: &str, max_lines: usize) -> String {
    if content == "no matches found" {
        return content.to_string();
    }
    let lines: Vec<&str> = content.lines().collect();
    if lines.len() <= max_lines {
        return content.to_string();
    }
    let mut out = lines[..max_lines].join("\n");
    out.push_str(&format!("\n... (trimmed, {} lines total)", lines.len()));
    out
}
