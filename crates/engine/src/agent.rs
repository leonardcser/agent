use crate::compact::{self, CompactOptions, CompactPhase, CompactReason, InitialContextInjection};
use crate::log;
use crate::permissions::{Decision, Permissions, RuntimeApprovals};
use crate::provider::{self, ChatOptions, FunctionSchema, Provider, ProviderError, ToolDefinition};
use crate::tools::{self, ToolContext, ToolRegistry, ToolResult};
use crate::{ApiConfig, AuxiliaryTask, EngineConfig, ModelConfig};
use protocol::{
    Content, EngineEvent, Message, Mode, ReasoningEffort, Role, ToolOutcome, TurnMeta, UiCommand,
};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;
use tokio::sync::mpsc;

use crate::compact_threshold_percent;

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

fn next_request_id() -> u64 {
    NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed)
}

/// Main engine task. Runs in a tokio::spawn and processes commands/events.
pub async fn engine_task(
    mut config: EngineConfig,
    mut registry: ToolRegistry,
    processes: tools::ProcessRegistry,
    mut cmd_rx: mpsc::UnboundedReceiver<UiCommand>,
    event_tx: mpsc::UnboundedSender<EngineEvent>,
) {
    let client = reqwest::Client::new();
    crate::pricing::spawn_catalog_fetch(client.clone());
    let file_locks = tools::FileLocks::default();

    // Connect MCP servers and register their tools.
    let _mcp_manager = if !config.mcp_servers.is_empty() {
        let mgr = crate::mcp::McpManager::start(&config.mcp_servers).await;
        let tool_defs = mgr.tool_defs().await;
        for def in tool_defs {
            registry.register(Box::new(crate::mcp::McpTool::new(
                def,
                std::sync::Arc::clone(&mgr),
            )));
        }
        Some(mgr)
    } else {
        None
    };

    let _ = event_tx.send(EngineEvent::Ready);

    // Process completion channel for background processes
    let (proc_done_tx, mut proc_done_rx) = mpsc::unbounded_channel::<(String, Option<i32>)>();

    // Context window size — set from config or lazily fetched from the
    // provider API on the first turn.
    let mut context_window: Option<u32> = config.context_window;

    loop {
        tokio::select! {
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    UiCommand::StartTurn { turn_id, content: input_content, mode, model, reasoning_effort, history, api_base, api_key, session_id, session_dir, model_config_overrides, permission_overrides, system_prompt: tui_system_prompt, plugin_tools } => {

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
                        let skill_section = config.skills.as_ref().and_then(|s| s.prompt_section());
                        let system_prompt = tui_system_prompt
                            .or_else(|| config.system_prompt_override.clone())
                            .unwrap_or_else(|| {
                                crate::build_system_prompt_full(
                                    mode,
                                    &config.cwd,
                                    config.instructions.as_deref(),
                                    agent_config.as_ref(),
                                    skill_section,
                                    config.interactive,
                                )
                            });
                        let mut turn = Turn {
                            provider,
                            registry: &registry,
                            permissions: perm_ref,
                            runtime_approvals: &config.runtime_approvals,
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
                            agent_config,
                            plugin_tools,
                            session_id,
                            session_dir,
                            started_at: Instant::now(),
                            tps_samples: Vec::new(),
                            tool_elapsed: HashMap::new(),
                            file_locks: &file_locks,
                            context_window,
                            compacted_this_turn: false,
                        };
                        turn.run(input_content, history).await;
                        // Cache the (possibly fetched) context window for future turns.
                        context_window = turn.context_window;
                    }
                    UiCommand::Compact { history, instructions } => {
                        let request = config.aux_or_primary(AuxiliaryTask::Compaction);
                        let provider = build_provider_from_api(&request.api, &client);
                        let cancel = crate::cancel::CancellationToken::new();
                        match compact::run_compact(
                            &provider,
                            &history,
                            &request.model,
                            instructions.as_deref(),
                            &cancel,
                            CompactOptions {
                                injection: InitialContextInjection::DoNotInject,
                                phase: CompactPhase::Manual,
                                reason: CompactReason::UserRequested,
                            },
                        )
                        .await
                        {
                            Ok((messages, usage)) => {
                                emit_usage_background(&event_tx, &request.api, &request.model, usage);
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
                    UiCommand::GenerateTitle {
                        last_user_message,
                        assistant_tail,
                    } => {
                        spawn_title_generation(
                            &config,
                            &client,
                            last_user_message,
                            assistant_tail,
                            &event_tx,
                        );
                    }
                    UiCommand::Btw {
                        question,
                        history,
                        reasoning_effort,
                    } => {
                        spawn_btw_request(
                            &config,
                            &client,
                            reasoning_effort,
                            question,
                            history,
                            &event_tx,
                        );
                    }
                    UiCommand::PredictInput {
                        history,
                        generation,
                    } => {
                        spawn_predict_request(&config, &client, history, &event_tx, generation);
                    }
                    UiCommand::EngineAsk {
                        id,
                        system,
                        messages,
                        task,
                    } => {
                        spawn_engine_ask(
                            &config, &client, id, system, messages, task, &event_tx,
                        );
                    }
                    UiCommand::SetModel { model, api_base, api_key, provider_type } => {
                        config.api.base = api_base;
                        config.api.key = api_key;
                        config.api.provider_type = provider_type;
                        config.model = model;
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
    last_user_message: String,
    assistant_tail: String,
    event_tx: &mpsc::UnboundedSender<EngineEvent>,
) {
    let request = config.aux_or_primary(AuxiliaryTask::Title);
    let provider = build_provider_from_api(&request.api, client);
    let pricing = PricingContext::from_api(&request.api);
    let model = request.model;
    let tx = event_tx.clone();
    tokio::spawn(async move {
        // History (source of last_user_message) is already redacted at ingress;
        // assistant_tail is model-generated and left unredacted by policy.
        log::entry(
            log::Level::Info,
            "title_request",
            &serde_json::json!({
                "user_chars": last_user_message.len(),
                "assistant_chars": assistant_tail.len(),
                "model": &model,
            }),
        );
        match provider
            .complete_title(&last_user_message, &assistant_tail, &model)
            .await
        {
            Ok(((ref title, ref slug), usage)) => {
                pricing.emit(&tx, &model, usage);
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
                let fallback = last_user_message
                    .lines()
                    .next()
                    .filter(|l| !l.is_empty())
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

fn spawn_btw_request(
    config: &EngineConfig,
    client: &reqwest::Client,
    reasoning_effort: protocol::ReasoningEffort,
    question: String,
    history: Vec<protocol::Message>,
    event_tx: &mpsc::UnboundedSender<EngineEvent>,
) {
    let request = config.aux_or_primary(AuxiliaryTask::Btw);
    let provider = build_provider_from_api(&request.api, client);
    let pricing = PricingContext::from_api(&request.api);
    let model = request.model;
    let tx = event_tx.clone();
    let redact = config.redact_secrets;
    tokio::spawn(async move {
        let cancel = crate::cancel::CancellationToken::new();

        // Btw questions can originate from the TUI (already redacted at
        // submit) or from a peer agent over the socket (not yet scrubbed).
        // Redact here so both paths land in history the same way.
        let question = if redact {
            crate::redact::redact(&question)
        } else {
            question
        };

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
                &ChatOptions::new(&cancel),
            )
            .await
        {
            Ok(resp) => {
                pricing.emit(&tx, &model, resp.usage);
                resp.content.unwrap_or_default()
            }
            Err(e) => format!("error: {e}"),
        };
        let _ = tx.send(EngineEvent::BtwResponse { content });
    });
}

fn spawn_predict_request(
    config: &EngineConfig,
    client: &reqwest::Client,
    history: Vec<protocol::Message>,
    event_tx: &mpsc::UnboundedSender<EngineEvent>,
    generation: u64,
) {
    let request = config.aux_or_primary(AuxiliaryTask::Prediction);
    let provider = build_provider_from_api(&request.api, client);
    let pricing = PricingContext::from_api(&request.api);
    let model = request.model;
    let tx = event_tx.clone();
    tokio::spawn(async move {
        let system = "You predict what a user will type next in a coding assistant conversation. \
                      Reply with ONLY the predicted message — no quotes, no explanation, \
                      no preamble. Keep it short (one sentence max). If you cannot predict, \
                      reply with an empty string.";

        // Build context from recent user messages + last assistant response.
        // History content is already redacted at ingress.
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
        if let Ok(Ok((text, usage))) = result {
            pricing.emit(&tx, &model, usage);
            let text = text.trim().to_string();
            if !text.is_empty() {
                let _ = tx.send(EngineEvent::InputPrediction { text, generation });
            }
        }
    });
}

fn spawn_engine_ask(
    config: &EngineConfig,
    client: &reqwest::Client,
    id: u64,
    system: String,
    mut messages: Vec<protocol::Message>,
    task: AuxiliaryTask,
    event_tx: &mpsc::UnboundedSender<EngineEvent>,
) {
    let request = config.aux_or_primary(task);
    let provider = build_provider_from_api(&request.api, client);
    let pricing = PricingContext::from_api(&request.api);
    let model = request.model;
    let tx = event_tx.clone();
    tokio::spawn(async move {
        let cancel = crate::cancel::CancellationToken::new();
        messages.insert(0, protocol::Message::system(&system));

        let content = match provider
            .chat(
                &messages,
                &[],
                &model,
                protocol::ReasoningEffort::default(),
                &ChatOptions::new(&cancel),
            )
            .await
        {
            Ok(resp) => {
                pricing.emit(&tx, &model, resp.usage);
                resp.content.unwrap_or_default()
            }
            Err(e) => format!("error: {e}"),
        };
        let _ = tx.send(EngineEvent::EngineAskResponse { id, content });
    });
}

fn build_provider_from_api(api: &ApiConfig, client: &reqwest::Client) -> Provider {
    build_provider(
        &api.base,
        &api.key,
        &api.provider_type,
        &api.model_config,
        client,
    )
}

fn build_provider_with_overrides(
    config: &EngineConfig,
    client: &reqwest::Client,
    api_base: Option<&str>,
    api_key: Option<&str>,
) -> Provider {
    build_provider(
        api_base.unwrap_or(&config.api.base),
        api_key.unwrap_or(&config.api.key),
        &config.api.provider_type,
        &config.api.model_config,
        client,
    )
}

fn build_provider(
    api_base: &str,
    api_key: &str,
    provider_type: &str,
    model_config: &ModelConfig,
    client: &reqwest::Client,
) -> Provider {
    Provider::new(
        api_base.to_string(),
        api_key.to_string(),
        provider_type,
        client.clone(),
    )
    .with_model_config(model_config.clone())
}

// ── Turn ────────────────────────────────────────────────────────────────────

/// A tool call awaiting execution. Borrows the `ToolCall` from the parent
/// assistant message and the tool impl from the registry; lifetime `'a`
/// ties both to the turn iteration that built the plan.
struct ToolSlot<'a> {
    tc: &'a protocol::ToolCall,
    args: HashMap<String, Value>,
    tool: &'a dyn tools::Tool,
    confirm_msg: Option<String>,
    start: Instant,
}

/// Tool calls from one LLM response, sorted by how they execute.
///
/// Tools whose base decision is `Allow` go into `ready` (run concurrently)
/// or `sequential_plugins` (plugin tools that blocked the LLM turn).
/// Tools whose decision is `Ask` go into `pending_perms`; they sit in
/// `slots` waiting for a `PermissionDecision` to either launch or cancel.
/// `Deny` decisions don't land here at all — they produce a synthetic
/// tool-result message inside [`Turn::classify_tools`] and are dropped.
struct ToolExecutionPlan<'a> {
    slots: Vec<ToolSlot<'a>>,
    ready: Vec<usize>,
    pending_perms: Vec<(usize, u64)>,
    /// Plugin tools awaiting execution by the TUI (request_id, call_id, start).
    pending_plugins: Vec<(u64, String, Instant)>,
    /// Plugin tools that opted into sequential execution — deferred
    /// until after the concurrent phase, then dispatched one at a time.
    /// (tool_call, args, start)
    sequential_plugins: Vec<(&'a protocol::ToolCall, HashMap<String, Value>, Instant)>,
    /// Plugin tools whose permission hooks are being evaluated by the
    /// TUI. Resolved by `UiCommand::PluginToolHooksResult`, after which
    /// the call transitions into `pending_plugins` (Allow), into
    /// `pending_plugin_perms` (Ask), or into a synthetic deny result.
    pending_plugin_hooks: Vec<(u64, PendingPluginCall<'a>)>,
    /// Plugin tools awaiting a user permission decision after their
    /// hooks evaluated to `Ask`. Resolved by
    /// `UiCommand::PermissionDecision`.
    pending_plugin_perms: Vec<(u64, PendingPluginCall<'a>)>,
}

/// In-flight plugin tool call carried through the hooks→permission
/// pipeline. Mirrors the data `ToolSlot` carries for core tools.
struct PendingPluginCall<'a> {
    tc: &'a protocol::ToolCall,
    args: HashMap<String, Value>,
    tool_start: Instant,
    is_sequential: bool,
}

/// Encapsulates the state of a single agent turn.
struct Turn<'a> {
    provider: Provider,
    registry: &'a ToolRegistry,
    permissions: &'a Permissions,
    runtime_approvals: &'a Arc<RwLock<RuntimeApprovals>>,
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
    agent_config: Option<crate::AgentPromptConfig>,
    plugin_tools: Vec<protocol::PluginToolDef>,
    session_id: String,
    session_dir: PathBuf,
    started_at: Instant,
    tps_samples: Vec<f64>,
    tool_elapsed: HashMap<String, u64>,
    /// Cached context window size. Lazily fetched from the provider API
    /// on the first turn if not set via config.
    context_window: Option<u32>,
    compacted_this_turn: bool,
}

impl<'a> Turn<'a> {
    fn emit(&self, event: EngineEvent) {
        let _ = self.event_tx.send(event);
    }

    /// Push a message into history. When `redact_secrets` is enabled, content
    /// from user/tool/agent roles — the only roles that carry data entering
    /// from outside the model — is scrubbed at this boundary. Model-generated
    /// messages (assistant, system) are passed through untouched.
    fn push_message(&mut self, mut msg: Message) {
        if self.config.redact_secrets && matches!(msg.role, Role::User | Role::Tool | Role::Agent) {
            crate::redact::redact_message(&mut msg);
        }
        self.messages.push(msg);
    }

    /// Rebuild the system prompt after a mid-turn mode change so the LLM sees
    /// the correct mode instructions on the next API call.
    fn regenerate_system_prompt(&mut self) {
        let skill_section = self.config.skills.as_ref().and_then(|s| s.prompt_section());
        let new = self
            .config
            .system_prompt_override
            .clone()
            .unwrap_or_else(|| {
                crate::build_system_prompt_full(
                    self.mode,
                    &self.config.cwd,
                    self.config.instructions.as_deref(),
                    self.agent_config.as_ref(),
                    skill_section,
                    self.config.interactive,
                )
            });
        self.system_prompt = new;
        if let Some(first) = self.messages.first_mut() {
            if matches!(first.role, Role::System) {
                *first = Message::system(&self.system_prompt);
            }
        }
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

    /// Compact conversation history mid-turn when context usage crosses the
    /// threshold. Returns `true` if compaction happened. Only fires once per
    /// turn to avoid wasting an LLM call when compaction can't reduce further.
    async fn maybe_compact(&mut self, prompt_tokens: u32) -> bool {
        if !self.config.auto_compact || self.compacted_this_turn {
            return false;
        }
        // Lazily fetch context window on first check.
        if self.context_window.is_none() {
            self.context_window = self.provider.fetch_context_window(&self.model).await;
        }
        let Some(ctx) = self.context_window else {
            return false;
        };
        let threshold = compact_threshold_percent();
        if (prompt_tokens as u64) * 100 < (ctx as u64) * threshold {
            return false;
        }
        self.compacted_this_turn = true;
        debug_assert!(
            matches!(self.messages[0].role, Role::System),
            "first message should be the system prompt"
        );
        let request = self.config.aux_or_primary(AuxiliaryTask::Compaction);
        let provider = build_provider_from_api(&request.api, self.http_client);
        let result = compact::run_compact(
            &provider,
            &self.messages[1..],
            &request.model,
            self.config.instructions.as_deref(),
            &self.cancel,
            CompactOptions {
                injection: InitialContextInjection::BeforeLastUserMessage,
                phase: CompactPhase::MidTurn,
                reason: CompactReason::ContextLimit,
            },
        )
        .await;
        match result {
            Ok((compacted, usage)) => {
                emit_usage_background(self.event_tx, &request.api, &request.model, usage);
                self.messages.truncate(1);
                self.messages.extend(compacted);
                self.emit_messages_snapshot();
                log::entry(
                    log::Level::Info,
                    "mid_turn_compact",
                    &serde_json::json!({
                        "prompt_tokens": prompt_tokens,
                        "context_window": ctx,
                        "threshold_percent": threshold,
                        "new_message_count": self.messages.len(),
                    }),
                );
                true
            }
            Err(e) => {
                log::entry(
                    log::Level::Warn,
                    "mid_turn_compact_error",
                    &serde_json::json!({"error": e.to_string()}),
                );
                false
            }
        }
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
            agent_blocks: std::collections::HashMap::new(),
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
            .with_model_config(self.config.api.model_config.clone());
    }

    /// Handle a command that arrived during a turn but isn't turn-specific.
    /// Returns true if the command was handled (caller should not fall through).
    fn handle_background_cmd(&self, cmd: UiCommand) -> bool {
        match cmd {
            UiCommand::GenerateTitle {
                last_user_message,
                assistant_tail,
            } => {
                spawn_title_generation(
                    self.config,
                    self.http_client,
                    last_user_message,
                    assistant_tail,
                    self.event_tx,
                );
                true
            }
            UiCommand::Btw {
                question,
                history,
                reasoning_effort,
            } => {
                spawn_btw_request(
                    self.config,
                    self.http_client,
                    reasoning_effort,
                    question,
                    history,
                    self.event_tx,
                );
                true
            }
            UiCommand::EngineAsk {
                id,
                system,
                messages,
                task,
            } => {
                spawn_engine_ask(
                    self.config,
                    self.http_client,
                    id,
                    system,
                    messages,
                    task,
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
                self.push_message(Message::user(Content::text(text)));
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
            UiCommand::SetMode {
                mode,
                system_prompt,
                plugin_tools,
            } => {
                self.mode = mode;
                if let Some(prompt) = system_prompt {
                    self.system_prompt = prompt;
                    if let Some(first) = self.messages.first_mut() {
                        if matches!(first.role, Role::System) {
                            *first = Message::system(&self.system_prompt);
                        }
                    }
                } else {
                    self.regenerate_system_prompt();
                }
                if let Some(tools) = plugin_tools {
                    self.plugin_tools = tools;
                }
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
                self.push_message(Message::agent(&from_id, &from_slug, &message));
                self.emit_messages_snapshot();
                true
            }
            other => self.handle_background_cmd(other),
        }
    }

    /// Main agentic loop for a single turn.
    async fn run(&mut self, content: Content, history: Vec<Message>) {
        self.provider.reset_turn_state();
        self.messages = Vec::with_capacity(history.len() + 2);
        self.messages.push(Message::system(&self.system_prompt));
        self.messages.extend(history);

        if !content.is_empty() {
            self.push_message(Message::user(content));
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

            // Ensure the system prompt reflects the current mode — a mid-turn
            // mode change (via SetMode) updates self.mode but the prompt may
            // still describe the old mode.
            self.regenerate_system_prompt();

            // Recompute tool definitions each iteration — mode may have
            // changed (e.g. Plan → Apply after plan approval).
            let tool_defs: Vec<ToolDefinition> = if self.provider.tool_calling() {
                let mut defs =
                    self.registry
                        .definitions(self.permissions, self.mode, self.config.interactive);
                // Plugin tools with `override_core` shadow the core
                // definition of the same name — drop the core one so
                // the LLM only sees a single schema for that tool name.
                let overridden: std::collections::HashSet<&str> = self
                    .plugin_tools
                    .iter()
                    .filter(|pt| pt.override_core)
                    .map(|pt| pt.name.as_str())
                    .collect();
                if !overridden.is_empty() {
                    defs.retain(|d| !overridden.contains(d.function.name.as_str()));
                }
                for pt in &self.plugin_tools {
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
                defs
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

            let prompt_tokens = resp.usage.prompt_tokens;
            if prompt_tokens.is_some() {
                let tokens_per_sec = resp.tokens_per_sec;
                if let Some(tps) = tokens_per_sec {
                    self.tps_samples.push(tps);
                }
                send_usage(
                    self.event_tx,
                    &self.config.api.provider_type,
                    &self.config.api.model_config,
                    &self.model,
                    resp.usage,
                    tokens_per_sec,
                    false,
                );
            }

            // Mid-turn auto-compaction: if context usage crossed the 80%
            // threshold, summarize older messages and continue the loop
            // with a smaller context.
            if let Some(tokens) = prompt_tokens {
                if self.maybe_compact(tokens).await {
                    continue;
                }
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

            // Only emit batch Thinking/Text when streaming wasn't active.
            // When streaming, ThinkingDelta/TextDelta already delivered the content.
            if partial_text.is_empty() && partial_reasoning.is_empty() {
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

            // Has tool calls — commit the assistant message (with the
            // tool_calls attached) then classify → execute → collect.
            // Deferred commands are replayed after results land in history
            // so steered user messages appear in the right position.
            empty_retries = 0;
            self.messages.push(Message::assistant(
                content,
                reasoning,
                Some(tool_calls.clone()),
            ));
            self.emit_messages_snapshot();

            let mut plan = self.classify_tools(&tool_calls);
            let mut completed: Vec<Option<ToolResult>> =
                (0..plan.slots.len()).map(|_| None).collect();
            let (cancelled, deferred, mut plugin_results) =
                self.execute_concurrent(&mut plan, &mut completed).await;
            let seq_plugin_results = self.run_sequential(&plan, &mut completed).await;
            plugin_results.extend(seq_plugin_results);
            if cancelled {
                self.mark_unfinished_cancelled(&plan, &completed);
            }
            self.collect_results(&plan, completed);
            for (call_id, content, is_error) in plugin_results {
                self.push_message(Message::tool(call_id, content, is_error));
            }
            for cmd in deferred {
                self.handle_turn_cmd(cmd);
            }
        }
    }

    /// Phase 1: turn LLM-emitted tool calls into a plan of slots with their
    /// permission decisions resolved. Allow/Deny are terminal here (Deny
    /// emits a synthetic tool result); Ask lands in `pending_perms` and
    /// waits for a `PermissionDecision` during [`execute_concurrent`].
    fn classify_tools<'b>(&mut self, tool_calls: &'b [protocol::ToolCall]) -> ToolExecutionPlan<'b>
    where
        'a: 'b,
    {
        let mut plan = ToolExecutionPlan {
            slots: Vec::new(),
            ready: Vec::new(),
            pending_perms: Vec::new(),
            pending_plugins: Vec::new(),
            sequential_plugins: Vec::new(),
            pending_plugin_hooks: Vec::new(),
            pending_plugin_perms: Vec::new(),
        };

        for tc in tool_calls {
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

            // Plugin-tool dispatch. A plugin tool with `override_core`
            // shadows the same-named core tool here AND in `definitions()`
            // — the LLM only sees the plugin's schema and we never reach
            // the core-tool path for that name. Without the flag, the
            // core tool wins on dispatch (and the engine errors at
            // schema-emit time for collisions, see `definitions()`).
            let plugin_tool = self.plugin_tools.iter().find(|pt| {
                pt.name == tc.function.name
                    && (pt.override_core || self.registry.get(&tc.function.name).is_none())
            });
            if let Some(pt) = plugin_tool {
                let is_sequential =
                    matches!(pt.execution_mode, protocol::ToolExecutionMode::Sequential);
                if pt.hooks.any() {
                    // Round-trip through the TUI for permission hooks.
                    let request_id = next_request_id();
                    self.emit(EngineEvent::EvaluatePluginToolHooks {
                        request_id,
                        call_id: tc.id.clone(),
                        tool_name: tc.function.name.clone(),
                        args: args.clone(),
                        mode: self.mode,
                    });
                    plan.pending_plugin_hooks.push((
                        request_id,
                        PendingPluginCall {
                            tc,
                            args: args.clone(),
                            tool_start,
                            is_sequential,
                        },
                    ));
                } else if is_sequential {
                    plan.sequential_plugins.push((tc, args.clone(), tool_start));
                } else {
                    let request_id = next_request_id();
                    self.emit(EngineEvent::ExecutePluginTool {
                        request_id,
                        call_id: tc.id.clone(),
                        tool_name: tc.function.name.clone(),
                        args: args.clone(),
                    });
                    plan.pending_plugins
                        .push((request_id, tc.id.clone(), tool_start));
                }
                continue;
            }

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

            let mut decision = tool
                .decide_override(&args, self.mode, &self.session_dir)
                .unwrap_or_else(|| {
                    self.permissions
                        .decide(self.mode, &tc.function.name, &args, tool.is_mcp())
                });

            // Runtime approvals (session + workspace) can turn Ask → Allow.
            if decision == Decision::Ask {
                let rt = self.runtime_approvals.read().unwrap();
                let desc = tool
                    .needs_confirm(&args)
                    .unwrap_or_else(|| tc.function.name.clone());
                if rt.is_auto_approved(self.permissions, self.mode, &tc.function.name, &args, &desc)
                {
                    decision = Decision::Allow;
                }
            }

            // Pre-flight validation: catch errors before prompting (e.g. stale file hash).
            if let Some(err) = tool.preflight(&args) {
                self.push_tool_result(&tc.id, &err, true, None);
                continue;
            }

            let idx = plan.slots.len();
            match decision {
                Decision::Allow => {
                    plan.slots.push(ToolSlot {
                        tc,
                        args,
                        tool,
                        confirm_msg: None,
                        start: tool_start,
                    });
                    plan.ready.push(idx);
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
                        (!d.is_empty()).then_some(d)
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
                    plan.slots.push(ToolSlot {
                        tc,
                        args,
                        tool,
                        confirm_msg: None,
                        start: tool_start,
                    });
                    plan.pending_perms.push((idx, request_id));
                }
            }
        }

        plan
    }

    /// Phase 2: run ready tools concurrently while draining the command
    /// channel. `PermissionDecision` for any pending tool launches it
    /// mid-flight (approved) or records a denial (rejected). Mid-turn
    /// steering / mode / model commands are collected into `deferred` and
    /// replayed after results are committed to history.
    ///
    /// Returns `(cancelled, deferred_commands, plugin_results)`.
    async fn execute_concurrent<'b>(
        &mut self,
        plan: &mut ToolExecutionPlan<'b>,
        completed: &mut [Option<ToolResult>],
    ) -> (bool, Vec<UiCommand>, Vec<(String, String, bool)>) {
        use futures_util::stream::StreamExt;

        type TaggedFut<'x> =
            std::pin::Pin<Box<dyn std::future::Future<Output = (usize, ToolResult)> + Send + 'x>>;

        let contexts: Vec<_> = plan
            .slots
            .iter()
            .map(|s| ToolContext {
                event_tx: self.event_tx.clone(),
                call_id: s.tc.id.clone(),
                cancel: self.cancel.clone(),
                processes: self.processes.clone(),
                proc_done_tx: self.proc_done_tx.clone(),
                provider: self.provider.clone(),
                model: self.model.clone(),
                session_id: self.session_id.clone(),
                session_dir: self.session_dir.clone(),
                file_locks: self.file_locks.clone(),
                api: self.config.api.clone(),
            })
            .collect();

        let mut futs: futures_util::stream::FuturesUnordered<TaggedFut<'_>> =
            futures_util::stream::FuturesUnordered::new();

        // Side-call futures from `smelt.tools.call` invocations.
        // Tracked separately from `outstanding` since they don't fill a tool
        // slot — they belong to a parent plugin invocation that's already
        // counted via `pending_plugins`.
        type SideFut<'x> =
            std::pin::Pin<Box<dyn std::future::Future<Output = (u64, ToolResult)> + Send + 'x>>;
        let mut side_futs: futures_util::stream::FuturesUnordered<SideFut<'_>> =
            futures_util::stream::FuturesUnordered::new();

        for &i in &plan.ready {
            let fut = plan.slots[i]
                .tool
                .execute(plan.slots[i].args.clone(), &contexts[i]);
            futs.push(Box::pin(async move { (i, fut.await) }));
        }

        let mut outstanding = plan.ready.len()
            + plan.pending_perms.len()
            + plan.pending_plugins.len()
            + plan.pending_plugin_hooks.len()
            + plan.pending_plugin_perms.len();
        let cancel = &self.cancel;
        let cmd_rx = &mut self.cmd_rx;
        let mut deferred: Vec<UiCommand> = Vec::new();
        let mut plugin_results: Vec<(String, String, bool)> = Vec::new();

        let cancelled = loop {
            if outstanding == 0 {
                break false;
            }
            tokio::select! {
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
                _ = cancel.cancelled() => break true,
                Some(cmd) = cmd_rx.recv() => match cmd {
                    UiCommand::Cancel => cancel.cancel(),
                    UiCommand::PermissionDecision { request_id, approved, message } => {
                        if let Some(pos) = plan
                            .pending_perms
                            .iter()
                            .position(|(_, rid)| *rid == request_id)
                        {
                            let (idx, _) = plan.pending_perms.swap_remove(pos);
                            if approved {
                                plan.slots[idx].confirm_msg = message;
                                let fut = plan.slots[idx]
                                    .tool
                                    .execute(plan.slots[idx].args.clone(), &contexts[idx]);
                                futs.push(Box::pin(async move { (idx, fut.await) }));
                            } else {
                                let denial = match message {
                                    Some(msg) => format!(
                                        "The user denied this tool call with message: {msg}"
                                    ),
                                    None => "The user denied this tool call. Try a different \
                                             approach or ask the user for guidance."
                                        .to_string(),
                                };
                                completed[idx] = Some(ToolResult {
                                    content: denial,
                                    is_error: false,
                                    metadata: None,
                                });
                                outstanding -= 1;
                            }
                        } else if let Some(pos) = plan
                            .pending_plugin_perms
                            .iter()
                            .position(|(rid, _)| *rid == request_id)
                        {
                            // Plugin tool whose hooks evaluated to Ask
                            // and is now hearing back from the user.
                            let (_, pending) = plan.pending_plugin_perms.swap_remove(pos);
                            if approved {
                                if pending.is_sequential {
                                    plan.sequential_plugins.push((
                                        pending.tc,
                                        pending.args,
                                        pending.tool_start,
                                    ));
                                } else {
                                    let rid = next_request_id();
                                    let _ = self.event_tx.send(EngineEvent::ExecutePluginTool {
                                        request_id: rid,
                                        call_id: pending.tc.id.clone(),
                                        tool_name: pending.tc.function.name.clone(),
                                        args: pending.args.clone(),
                                    });
                                    plan.pending_plugins.push((
                                        rid,
                                        pending.tc.id.clone(),
                                        pending.tool_start,
                                    ));
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
                                let elapsed_ms =
                                    Some(pending.tool_start.elapsed().as_millis() as u64);
                                let _ = self.event_tx.send(EngineEvent::ToolFinished {
                                    call_id: pending.tc.id.clone(),
                                    result: ToolOutcome {
                                        content: denial.clone(),
                                        is_error: false,
                                        metadata: None,
                                    },
                                    elapsed_ms,
                                });
                                plugin_results.push((pending.tc.id.clone(), denial, false));
                                outstanding -= 1;
                            }
                        }
                    }
                    UiCommand::PluginToolHooksResult { request_id, hooks } => {
                        if let Some(pos) = plan
                            .pending_plugin_hooks
                            .iter()
                            .position(|(rid, _)| *rid == request_id)
                        {
                            let (_, pending) = plan.pending_plugin_hooks.swap_remove(pos);
                            // Preflight: bail immediately on hook error.
                            if let Some(err) = hooks.preflight_error {
                                let elapsed_ms =
                                    Some(pending.tool_start.elapsed().as_millis() as u64);
                                let _ = self.event_tx.send(EngineEvent::ToolFinished {
                                    call_id: pending.tc.id.clone(),
                                    result: ToolOutcome {
                                        content: err.clone(),
                                        is_error: true,
                                        metadata: None,
                                    },
                                    elapsed_ms,
                                });
                                plugin_results.push((pending.tc.id.clone(), err, true));
                                outstanding -= 1;
                            } else {
                                // Same Decision flow core tools use, keyed on tool
                                // name. Plugin tools that shadow a core name
                                // (override_core) inherit that name's permission
                                // ruleset by design — e.g. a plugin "bash" goes
                                // through the bash subcommand-split machinery in
                                // permissions::decide_base.
                                let mut decision = self.permissions.decide(
                                    self.mode,
                                    &pending.tc.function.name,
                                    &pending.args,
                                    false,
                                );
                                if decision == Decision::Ask {
                                    let rt = self.runtime_approvals.read().unwrap();
                                    let desc = hooks
                                        .needs_confirm
                                        .clone()
                                        .unwrap_or_else(|| pending.tc.function.name.clone());
                                    if rt.is_auto_approved(
                                        self.permissions,
                                        self.mode,
                                        &pending.tc.function.name,
                                        &pending.args,
                                        &desc,
                                    ) {
                                        decision = Decision::Allow;
                                    }
                                }
                                match decision {
                                    Decision::Allow => {
                                        if pending.is_sequential {
                                            plan.sequential_plugins.push((
                                                pending.tc,
                                                pending.args,
                                                pending.tool_start,
                                            ));
                                        } else {
                                            let rid = next_request_id();
                                            let _ = self
                                                .event_tx
                                                .send(EngineEvent::ExecutePluginTool {
                                                    request_id: rid,
                                                    call_id: pending.tc.id.clone(),
                                                    tool_name: pending.tc.function.name.clone(),
                                                    args: pending.args.clone(),
                                                });
                                            plan.pending_plugins.push((
                                                rid,
                                                pending.tc.id.clone(),
                                                pending.tool_start,
                                            ));
                                        }
                                    }
                                    Decision::Deny => {
                                        let denial = "The user's permission settings blocked \
                                                      this tool call. Try a different approach \
                                                      or ask the user for guidance."
                                            .to_string();
                                        let elapsed_ms = Some(
                                            pending.tool_start.elapsed().as_millis() as u64,
                                        );
                                        let _ = self.event_tx.send(EngineEvent::ToolFinished {
                                            call_id: pending.tc.id.clone(),
                                            result: ToolOutcome {
                                                content: denial.clone(),
                                                is_error: false,
                                                metadata: None,
                                            },
                                            elapsed_ms,
                                        });
                                        plugin_results.push((pending.tc.id.clone(), denial, false));
                                        outstanding -= 1;
                                    }
                                    Decision::Ask => {
                                        let cmd_summary =
                                            if pending.tc.function.name == "bash" {
                                                let d = tools::str_arg(&pending.args, "description");
                                                (!d.is_empty()).then_some(d)
                                            } else {
                                                None
                                            };
                                        let confirm_msg = hooks
                                            .needs_confirm
                                            .clone()
                                            .unwrap_or_else(|| pending.tc.function.name.clone());
                                        let rid = next_request_id();
                                        let _ = self
                                            .event_tx
                                            .send(EngineEvent::RequestPermission {
                                                request_id: rid,
                                                call_id: pending.tc.id.clone(),
                                                tool_name: pending.tc.function.name.clone(),
                                                args: pending.args.clone(),
                                                confirm_message: confirm_msg,
                                                approval_patterns: hooks.approval_patterns,
                                                summary: cmd_summary,
                                            });
                                        plan.pending_plugin_perms.push((rid, pending));
                                    }
                                }
                            }
                        }
                    }
                    UiCommand::PluginToolResult { request_id, call_id, content, is_error } => {
                        if let Some(pos) = plan
                            .pending_plugins
                            .iter()
                            .position(|(rid, _, _)| *rid == request_id)
                        {
                            let (_, _, start) = plan.pending_plugins.swap_remove(pos);
                            let elapsed_ms = Some(start.elapsed().as_millis() as u64);
                            let _ = self.event_tx.send(EngineEvent::ToolFinished {
                                call_id: call_id.clone(),
                                result: ToolOutcome {
                                    content: content.clone(),
                                    is_error,
                                    metadata: None,
                                },
                                elapsed_ms,
                            });
                            plugin_results.push((call_id, content, is_error));
                            outstanding -= 1;
                        }
                    }
                    UiCommand::CallCoreTool { request_id, parent_call_id, tool_name, args } => {
                        if let Some(tool) = self.registry.get(&tool_name) {
                            let ctx = ToolContext {
                                event_tx: self.event_tx.clone(),
                                call_id: parent_call_id,
                                cancel: self.cancel.clone(),
                                processes: self.processes.clone(),
                                proc_done_tx: self.proc_done_tx.clone(),
                                provider: self.provider.clone(),
                                model: self.model.clone(),
                                session_id: self.session_id.clone(),
                                session_dir: self.session_dir.clone(),
                                file_locks: self.file_locks.clone(),
                                api: self.config.api.clone(),
                            };
                            side_futs.push(Box::pin(async move {
                                let r = tool.execute(args, &ctx).await;
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

        if cancelled {
            for (_, call_id, start) in plan.pending_plugins.drain(..) {
                let elapsed_ms = Some(start.elapsed().as_millis() as u64);
                let _ = self.event_tx.send(EngineEvent::ToolFinished {
                    call_id: call_id.clone(),
                    result: ToolOutcome {
                        content: "cancelled".to_string(),
                        is_error: true,
                        metadata: None,
                    },
                    elapsed_ms,
                });
                plugin_results.push((call_id, "cancelled".to_string(), true));
            }
            for (_, pending) in plan.pending_plugin_hooks.drain(..) {
                let elapsed_ms = Some(pending.tool_start.elapsed().as_millis() as u64);
                let _ = self.event_tx.send(EngineEvent::ToolFinished {
                    call_id: pending.tc.id.clone(),
                    result: ToolOutcome {
                        content: "cancelled".to_string(),
                        is_error: true,
                        metadata: None,
                    },
                    elapsed_ms,
                });
                plugin_results.push((pending.tc.id.clone(), "cancelled".to_string(), true));
            }
            for (_, pending) in plan.pending_plugin_perms.drain(..) {
                let elapsed_ms = Some(pending.tool_start.elapsed().as_millis() as u64);
                let _ = self.event_tx.send(EngineEvent::ToolFinished {
                    call_id: pending.tc.id.clone(),
                    result: ToolOutcome {
                        content: "cancelled".to_string(),
                        is_error: true,
                        metadata: None,
                    },
                    elapsed_ms,
                });
                plugin_results.push((pending.tc.id.clone(), "cancelled".to_string(), true));
            }
        }

        (cancelled, deferred, plugin_results)
    }

    /// When the turn was cancelled mid-flight, emit a cancelled-result
    /// message for every slot that never completed. Permissions that were
    /// still pending (no approval ever arrived) also land here.
    fn mark_unfinished_cancelled(
        &mut self,
        plan: &ToolExecutionPlan<'_>,
        completed: &[Option<ToolResult>],
    ) {
        let starts: Vec<_> = plan
            .slots
            .iter()
            .enumerate()
            .filter(|(i, _)| completed[*i].is_none())
            .map(|(_, slot)| (slot.tc.id.clone(), slot.start))
            .collect();
        for (call_id, start) in starts {
            self.push_tool_result(&call_id, "cancelled", true, Some(start));
        }
    }

    /// Phase 2b: sequential plugin tools — deferred past the concurrent
    /// phase and dispatched one at a time. Used by plugin tools that
    /// open a dialog and await a user reply.
    async fn run_sequential(
        &mut self,
        plan: &ToolExecutionPlan<'_>,
        _completed: &mut [Option<ToolResult>],
    ) -> Vec<(String, String, bool)> {
        let mut plugin_results = Vec::new();
        let mut cancelled = false;
        for (tc, args, start) in &plan.sequential_plugins {
            let (content, is_error) = if cancelled || self.cancel.is_cancelled() {
                cancelled = true;
                ("cancelled".to_string(), true)
            } else {
                let request_id = next_request_id();
                let _ = self.event_tx.send(EngineEvent::ExecutePluginTool {
                    request_id,
                    call_id: tc.id.clone(),
                    tool_name: tc.function.name.clone(),
                    args: args.clone(),
                });
                match self.wait_for_plugin_result(request_id).await {
                    Some(result) => result,
                    None => {
                        cancelled = true;
                        ("cancelled".to_string(), true)
                    }
                }
            };
            let elapsed_ms = Some(start.elapsed().as_millis() as u64);
            let _ = self.event_tx.send(EngineEvent::ToolFinished {
                call_id: tc.id.clone(),
                result: ToolOutcome {
                    content: content.clone(),
                    is_error,
                    metadata: None,
                },
                elapsed_ms,
            });
            plugin_results.push((tc.id.clone(), content, is_error));
        }
        plugin_results
    }

    /// Phase 3: commit each tool's result to history, emit `ToolFinished`,
    /// and record elapsed times. Duplicate detection replaces identical
    /// prior results with a dedup stub to keep context small.
    fn collect_results(
        &mut self,
        plan: &ToolExecutionPlan<'_>,
        mut completed: Vec<Option<ToolResult>>,
    ) {
        for (i, slot) in plan.slots.iter().enumerate() {
            let Some(result) = completed[i].take() else {
                continue;
            };
            let ToolResult {
                content,
                is_error,
                metadata,
            } = result;

            if log::Level::Debug.enabled() {
                let mut preview = content[..content.floor_char_boundary(500)].to_string();
                if self.config.redact_secrets {
                    preview = crate::redact::redact(&preview);
                }
                log::entry(
                    log::Level::Debug,
                    "tool_result",
                    &serde_json::json!({
                        "tool": slot.tc.function.name,
                        "id": slot.tc.id,
                        "is_error": is_error,
                        "content_len": content.len(),
                        "content_preview": preview,
                    }),
                );
            }

            let elapsed_ms = slot.start.elapsed().as_millis() as u64;
            self.tool_elapsed.insert(slot.tc.id.clone(), elapsed_ms);
            let mut tool_content = content.clone();
            if let Some(ref msg) = slot.confirm_msg {
                tool_content.push_str(&format!("\n\nUser message: {msg}"));
            }
            let history_content =
                match tools::result_dedup::duplicate_of(&tool_content, is_error, &self.messages) {
                    Some(prior_id) => tools::result_dedup::dedup_stub(prior_id),
                    None => tool_content,
                };
            self.push_message(Message::tool(slot.tc.id.clone(), history_content, is_error));
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
    ) -> (
        Result<(crate::provider::LLMResponse, bool), ProviderError>,
        String,
        String,
    ) {
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
            let opts = ChatOptions {
                cancel: &self.cancel,
                on_retry: Some(&on_retry),
                on_delta: Some(&on_delta),
                response_format: None,
            };
            // History is redacted at ingress (see Turn::push_message), so we
            // can pass self.messages straight through — no per-turn clone.
            let chat_future = self.provider.chat(
                &self.messages,
                tool_defs,
                &self.model,
                self.reasoning_effort,
                &opts,
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
                        UiCommand::SetMode { mode, system_prompt, plugin_tools } => {
                            self.mode = mode;
                            if let Some(p) = system_prompt { self.system_prompt = p; }
                            if let Some(t) = plugin_tools { self.plugin_tools = t; }
                        }
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

    /// Wait for a PluginToolResult matching the given request_id.
    /// Applies mid-wait mode/model/reasoning changes.
    async fn wait_for_plugin_result(&mut self, request_id: u64) -> Option<(String, bool)> {
        loop {
            match self.cmd_rx.recv().await {
                Some(UiCommand::PluginToolResult {
                    request_id: id,
                    content,
                    is_error,
                    ..
                }) if id == request_id => return Some((content, is_error)),
                Some(UiCommand::SetMode {
                    mode,
                    system_prompt,
                    plugin_tools,
                }) => {
                    self.mode = mode;
                    if let Some(p) = system_prompt {
                        self.system_prompt = p;
                    } else {
                        self.regenerate_system_prompt();
                    }
                    if let Some(t) = plugin_tools {
                        self.plugin_tools = t;
                    }
                }
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
        let history_content =
            match tools::result_dedup::duplicate_of(content, is_error, &self.messages) {
                Some(prior_id) => tools::result_dedup::dedup_stub(prior_id),
                None => content.to_string(),
            };
        self.push_message(Message::tool(
            tool_call_id.to_string(),
            history_content,
            is_error,
        ));
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

fn send_usage(
    tx: &mpsc::UnboundedSender<EngineEvent>,
    provider_type: &str,
    model_config: &crate::ModelConfig,
    model: &str,
    usage: protocol::TokenUsage,
    tokens_per_sec: Option<f64>,
    background: bool,
) {
    let resolved = crate::pricing::resolve(model, provider_type, model_config);
    let cost = resolved.pricing.cost(&usage);
    let _ = tx.send(EngineEvent::TokenUsage {
        usage,
        tokens_per_sec,
        cost_usd: if cost > 0.0 { Some(cost) } else { None },
        background,
    });
}

/// Calculate cost from token usage and emit a `TokenUsage` event.
pub fn emit_usage(
    tx: &mpsc::UnboundedSender<EngineEvent>,
    api: &crate::ApiConfig,
    model: &str,
    usage: protocol::TokenUsage,
) {
    send_usage(
        tx,
        &api.provider_type,
        &api.model_config,
        model,
        usage,
        None,
        false,
    );
}

/// Emit a background `TokenUsage` event (compaction, title, btw, predict).
/// Cost is tracked but prompt_tokens won't update displayed context usage.
fn emit_usage_background(
    tx: &mpsc::UnboundedSender<EngineEvent>,
    api: &crate::ApiConfig,
    model: &str,
    usage: protocol::TokenUsage,
) {
    send_usage(
        tx,
        &api.provider_type,
        &api.model_config,
        model,
        usage,
        None,
        true,
    );
}

/// Lightweight pricing context for spawned background tasks.
#[derive(Clone)]
struct PricingContext {
    provider_type: String,
    model_config: crate::ModelConfig,
}

impl PricingContext {
    fn from_api(api: &crate::ApiConfig) -> Self {
        Self {
            provider_type: api.provider_type.clone(),
            model_config: api.model_config.clone(),
        }
    }

    fn emit(
        &self,
        tx: &mpsc::UnboundedSender<EngineEvent>,
        model: &str,
        usage: protocol::TokenUsage,
    ) {
        send_usage(
            tx,
            &self.provider_type,
            &self.model_config,
            model,
            usage,
            None,
            true,
        );
    }
}
