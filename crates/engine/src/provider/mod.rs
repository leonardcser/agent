mod anthropic;
mod chat_completions;
pub mod codex;
mod extract;
mod openai;
mod sse;

use crate::cancel::CancellationToken;
use crate::log;
pub use protocol::TokenUsage;
use protocol::{Content, Message, ReasoningEffort, Role, ToolCall};
use reqwest::Client;
use serde::Serialize;
use std::time::{Duration, Instant};

// ── Tool definitions ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct ToolDefinition {
    #[serde(rename = "type")]
    def_type: AlwaysFunctionDef,
    pub function: FunctionSchema,
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionSchema {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Copy)]
struct AlwaysFunctionDef;

impl Serialize for AlwaysFunctionDef {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("function")
    }
}

impl ToolDefinition {
    pub fn new(function: FunctionSchema) -> Self {
        Self {
            def_type: AlwaysFunctionDef,
            function,
        }
    }
}

// ── Response types ──────────────────────────────────────────────────────────

/// Internal parsed fields from an API response. Shared across backends.
pub(crate) struct ParsedResponse {
    pub content: Option<String>,
    pub reasoning: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub usage: TokenUsage,
}

impl ParsedResponse {
    pub fn into_response(self, tokens_per_sec: Option<f64>) -> LLMResponse {
        LLMResponse {
            content: self.content,
            reasoning_content: self.reasoning,
            tool_calls: self.tool_calls,
            usage: self.usage,
            tokens_per_sec,
        }
    }
}

/// Convert an accumulated String to Option, returning None if empty.
pub(crate) fn non_empty(s: String) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Collect indexed tool calls from a HashMap<usize, (id, name, args)>,
/// sorted by index. Used by Anthropic and Chat Completions backends.
pub(crate) fn collect_indexed_tool_calls(
    map: std::collections::HashMap<usize, (String, String, String)>,
) -> Vec<ToolCall> {
    let mut vec: Vec<(usize, ToolCall)> = map
        .into_iter()
        .map(|(idx, (id, name, args))| {
            (
                idx,
                ToolCall::new(
                    id,
                    protocol::FunctionCall {
                        name,
                        arguments: args,
                    },
                ),
            )
        })
        .collect();
    vec.sort_by_key(|(idx, _)| *idx);
    vec.into_iter().map(|(_, tc)| tc).collect()
}

/// A streaming delta from the LLM.
pub enum StreamDelta<'a> {
    Text(&'a str),
    Thinking(&'a str),
}

pub struct LLMResponse {
    pub content: Option<String>,
    pub reasoning_content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub usage: TokenUsage,
    pub tokens_per_sec: Option<f64>,
}

// ── Errors ──────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("cancelled")]
    Cancelled,
    #[error("rate limited (attempt {attempt})")]
    RateLimited { attempt: u32 },
    #[error("quota exceeded: {0}")]
    QuotaExceeded(String),
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("server error {status}: {body}")]
    Server { status: u16, body: String },
    #[error("network error: {0}")]
    Network(String),
    #[error("invalid response: {0}")]
    InvalidResponse(String),
    #[error("max retries exceeded")]
    MaxRetries,
}

impl ProviderError {
    fn is_retryable(&self) -> bool {
        matches!(
            self,
            ProviderError::RateLimited { .. }
                | ProviderError::Server { .. }
                | ProviderError::Network(_)
        )
    }

    fn from_http(code: u16, body: String) -> Self {
        let is_quota = body.contains("insufficient_quota")
            || body.contains("billing_not_active")
            || body.contains("credit balance is too low")
            || (code == 429 && body.contains("exceeded"));

        match code {
            _ if is_quota => ProviderError::QuotaExceeded(body),
            400 => ProviderError::InvalidResponse(body),
            401 | 403 => ProviderError::Auth(body),
            404 => ProviderError::NotFound(body),
            429 => ProviderError::RateLimited { attempt: 0 },
            _ => ProviderError::Server { status: code, body },
        }
    }
}

fn backoff_delay(attempt: usize) -> Duration {
    Duration::from_millis(500 * 2u64.pow(attempt as u32))
}

/// Parse the `retry-after` header from an HTTP response (seconds).
fn parse_retry_after(resp: &reqwest::Response) -> Option<Duration> {
    let val = resp.headers().get("retry-after")?.to_str().ok()?;
    val.parse::<f64>()
        .ok()
        .filter(|&s| s > 0.0)
        .map(Duration::from_secs_f64)
}

// ── Provider kind ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    OpenAi,
    Codex,
    Anthropic,
    Local,
}

impl ProviderKind {
    pub fn default_reasoning_cycle(self) -> &'static [ReasoningEffort] {
        match self {
            Self::OpenAi | Self::Codex | Self::Anthropic => &[
                ReasoningEffort::Off,
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
                ReasoningEffort::Max,
            ],
            Self::Local => &[
                ReasoningEffort::Off,
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
            ],
        }
    }

    pub fn from_config(provider_type: &str) -> Self {
        match provider_type {
            "openai" => Self::OpenAi,
            "codex" => Self::Codex,
            "anthropic" => Self::Anthropic,
            _ => Self::Local,
        }
    }

    pub fn detect_from_url(api_base: &str) -> Self {
        if api_base.contains("api.openai.com") {
            Self::OpenAi
        } else if api_base.contains("chatgpt.com") {
            Self::Codex
        } else if api_base.contains("api.anthropic.com") {
            Self::Anthropic
        } else {
            Self::Local
        }
    }

    pub fn as_config_str(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Codex => "codex",
            Self::Anthropic => "anthropic",
            Self::Local => "openai-compatible",
        }
    }
}

// ── Chat options ────────────────────────────────────────────────────────────

/// Execution-time options for a `Provider::chat()` call.
pub struct ChatOptions<'a> {
    pub cancel: &'a CancellationToken,
    pub on_retry: Option<&'a (dyn Fn(Duration, u32) + Send + Sync)>,
    pub on_delta: Option<&'a (dyn Fn(StreamDelta<'_>) + Send + Sync)>,
}

impl<'a> ChatOptions<'a> {
    pub fn new(cancel: &'a CancellationToken) -> Self {
        Self {
            cancel,
            on_retry: None,
            on_delta: None,
        }
    }
}

// ── Provider ────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct Provider {
    api_base: String,
    api_key: String,
    client: Client,
    kind: ProviderKind,
    model_config: crate::config::ModelConfig,
    /// Sticky routing token for Codex — set from the first response in a turn,
    /// echoed back on subsequent requests within the same turn.
    turn_state: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

/// Ensure that `arguments` in any `tool_calls[].function` is valid JSON.
/// Some models produce malformed argument strings (e.g. `"{"`); sending these
/// back in conversation history causes 400 errors from strict backends.
pub(crate) fn sanitize_tool_call_arguments(obj: &mut serde_json::Map<String, serde_json::Value>) {
    if let Some(tcs) = obj.get_mut("tool_calls").and_then(|v| v.as_array_mut()) {
        for tc in tcs {
            if let Some(args) = tc
                .get_mut("function")
                .and_then(|f| f.get_mut("arguments"))
            {
                if let Some(s) = args.as_str() {
                    if serde_json::from_str::<serde_json::Value>(s).is_err() {
                        *args = serde_json::json!("{}");
                    }
                }
            }
        }
    }
}

/// Rewrite an Agent-role message as a user message for API serialization.
pub(crate) fn fixup_agent_message(m: &Message, v: &mut serde_json::Value) {
    if let Some(obj) = v.as_object_mut() {
        obj.insert("role".into(), serde_json::json!("user"));
        obj.remove("agent_from_id");
        obj.remove("agent_from_slug");
        obj.insert("content".into(), serde_json::json!(m.agent_api_text()));
    }
}

impl Provider {
    pub fn new(api_base: String, api_key: String, provider_type: &str, client: Client) -> Self {
        let api_base = api_base.trim_end_matches('/').to_string();
        let kind = ProviderKind::from_config(provider_type);
        Self {
            api_base,
            api_key,
            client,
            kind,
            model_config: Default::default(),
            turn_state: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// Reset the sticky routing state. Call this at the start of each new turn.
    pub fn reset_turn_state(&self) {
        *self.turn_state.lock().unwrap() = None;
    }

    pub fn with_model_config(mut self, config: crate::config::ModelConfig) -> Self {
        self.model_config = config;
        self
    }

    pub fn tool_calling(&self) -> bool {
        self.model_config.tool_calling()
    }

    pub fn apply_model_overrides(&mut self, overrides: &protocol::ModelConfigOverrides) {
        if let Some(v) = overrides.temperature {
            self.model_config.temperature = Some(v);
        }
        if let Some(v) = overrides.top_p {
            self.model_config.top_p = Some(v);
        }
        if let Some(v) = overrides.top_k {
            self.model_config.top_k = Some(v);
        }
        if let Some(v) = overrides.min_p {
            self.model_config.min_p = Some(v);
        }
        if let Some(v) = overrides.repeat_penalty {
            self.model_config.repeat_penalty = Some(v);
        }
    }

    // ── Main chat method ────────────────────────────────────────────────

    pub async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        model: &str,
        effort: ReasoningEffort,
        opts: &ChatOptions<'_>,
    ) -> Result<LLMResponse, ProviderError> {
        let is_anthropic = self.kind == ProviderKind::Anthropic;
        let is_codex = self.kind == ProviderKind::Codex;

        // Codex: resolve OAuth access token (refreshing if needed).
        let mut codex_auth = if is_codex {
            Some(
                codex::ensure_access_token_full(&self.client)
                    .await
                    .map_err(ProviderError::Auth)?,
            )
        } else {
            None
        };
        let mut codex_401_retried = false;

        let (url, mut body) = match self.kind {
            ProviderKind::OpenAi => {
                let url = format!("{}/responses", self.api_base);
                let body = openai::build_body(messages, tools, model, effort, &self.model_config);
                (url, body)
            }
            ProviderKind::Codex => {
                let url = codex::CODEX_API_ENDPOINT.to_string();
                let mut body =
                    openai::build_body(messages, tools, model, effort, &self.model_config);
                body["store"] = serde_json::json!(false);
                (url, body)
            }
            ProviderKind::Anthropic => {
                let url = format!("{}/messages", self.api_base);
                let body =
                    anthropic::build_body(messages, tools, model, effort, &self.model_config);
                (url, body)
            }
            ProviderKind::Local => {
                let url = format!("{}/chat/completions", self.api_base);
                let body = chat_completions::build_body(
                    messages,
                    tools,
                    model,
                    effort,
                    &self.model_config,
                );
                (url, body)
            }
        };

        let use_stream = opts.on_delta.is_some() || is_codex;
        if use_stream {
            body["stream"] = serde_json::json!(true);
            // Request usage data in the final streaming chunk.
            // Anthropic and OpenAI Responses API don't use stream_options.
            if self.kind == ProviderKind::Local {
                body["stream_options"] = serde_json::json!({"include_usage": true});
            }
        }

        log::entry(
            log::Level::Debug,
            "request",
            &serde_json::json!({
                "url": url,
                "provider_kind": format!("{:?}", self.kind),
                "body": body,
            }),
        );

        let max_retries = 9;

        for attempt in 0..=max_retries {
            let request_start = Instant::now();

            let mut req = self.client.post(&url).json(&body);
            if is_codex {
                if let Some(ref tokens) = codex_auth {
                    req = req.bearer_auth(&tokens.access_token);
                    if let Some(id) = &tokens.account_id {
                        req = req.header("ChatGPT-Account-Id", id);
                    }
                    req = req.header("originator", "smelt");
                    if let Some(ref ts) = *self.turn_state.lock().unwrap() {
                        req = req.header("x-codex-turn-state", ts.as_str());
                    }
                }
            } else if !self.api_key.is_empty() {
                if is_anthropic {
                    req = req.header("x-api-key", &self.api_key);
                } else {
                    req = req.bearer_auth(&self.api_key);
                }
            }
            if is_anthropic {
                req = req.header("anthropic-version", "2023-06-01");
            }

            let resp = tokio::select! {
                _ = opts.cancel.cancelled() => {
                    return Err(ProviderError::Cancelled);
                }
                result = req.send() => match result {
                    Ok(r) => r,
                    Err(e) => {
                        let err = ProviderError::Network(e.to_string());
                        log::entry(log::Level::Warn, "request_error", &serde_json::json!({
                            "attempt": attempt,
                            "error": format!("{e:?}"),
                        }));
                        if attempt < max_retries {
                            let delay = backoff_delay(attempt);
                            if attempt > 0 {
                                if let Some(f) = opts.on_retry { f(delay, attempt as u32); }
                            }
                            tokio::time::sleep(delay).await;
                            continue;
                        }
                        return Err(err);
                    }
                }
            };

            if !resp.status().is_success() {
                let code = resp.status().as_u16();
                let retry_after = parse_retry_after(&resp);
                let text = resp.text().await.unwrap_or_default();

                let mut err = ProviderError::from_http(code, text);
                if let ProviderError::RateLimited { attempt: ref mut a } = err {
                    *a = attempt as u32;
                }

                log::entry(
                    log::Level::Warn,
                    "request_error",
                    &serde_json::json!({
                        "attempt": attempt,
                        "status": code,
                        "retry_after_secs": retry_after.map(|d| d.as_secs_f64()),
                        "error": err.to_string(),
                    }),
                );

                // Codex 401 recovery: refresh tokens and retry once.
                if is_codex && matches!(err, ProviderError::Auth(_)) && !codex_401_retried {
                    codex_401_retried = true;
                    if let Some(ref stale) = codex_auth {
                        if let Ok(refreshed) =
                            codex::refresh_tokens(&self.client, &stale.refresh_token).await
                        {
                            log::entry(
                                log::Level::Info,
                                "codex_401_recovery",
                                &serde_json::json!({ "account_id": refreshed.account_id }),
                            );
                            codex_auth = Some(refreshed);
                            continue;
                        }
                    }
                }

                if err.is_retryable() && attempt < max_retries {
                    let backoff = backoff_delay(attempt);
                    let delay = retry_after.map_or(backoff, |ra| ra.max(backoff));
                    if attempt > 0 {
                        if let Some(f) = opts.on_retry {
                            f(delay, attempt as u32);
                        }
                    }
                    tokio::time::sleep(delay).await;
                    continue;
                }
                return Err(err);
            }

            if is_codex && self.turn_state.lock().unwrap().is_none() {
                if let Some(val) = resp.headers().get("x-codex-turn-state") {
                    if let Ok(s) = val.to_str() {
                        *self.turn_state.lock().unwrap() = Some(s.to_string());
                    }
                }
            }

            let noop_delta: &(dyn Fn(StreamDelta<'_>) + Send + Sync) = &|_| {};
            let on_delta = opts.on_delta.unwrap_or(noop_delta);

            let parsed = if use_stream {
                match self.kind {
                    ProviderKind::OpenAi | ProviderKind::Codex => {
                        openai::read_stream(resp, opts.cancel, on_delta).await
                    }
                    ProviderKind::Anthropic => {
                        anthropic::read_stream(resp, opts.cancel, on_delta).await
                    }
                    ProviderKind::Local => {
                        chat_completions::read_stream(resp, opts.cancel, on_delta).await
                    }
                }?
            } else {
                let data: serde_json::Value = resp
                    .json()
                    .await
                    .map_err(|e| ProviderError::InvalidResponse(e.to_string()))?;

                log::entry(
                    log::Level::Debug,
                    "raw_response",
                    &serde_json::json!({
                        "url": url,
                        "provider_kind": format!("{:?}", self.kind),
                        "data": data,
                    }),
                );

                match self.kind {
                    ProviderKind::OpenAi | ProviderKind::Codex => openai::parse_response(&data)?,
                    ProviderKind::Anthropic => anthropic::parse_response(&data)?,
                    ProviderKind::Local => chat_completions::parse_response(&data)?,
                }
            };

            let elapsed = request_start.elapsed();
            let tokens_per_sec = parsed.usage.completion_tokens.and_then(|c| {
                if c > 0 && elapsed.as_secs_f64() >= 0.001 {
                    Some(c as f64 / elapsed.as_secs_f64())
                } else {
                    None
                }
            });

            log::entry(
                log::Level::Debug,
                "response",
                &serde_json::json!({
                    "content": parsed.content,
                    "reasoning_content": parsed.reasoning,
                    "tool_calls": parsed.tool_calls,
                    "prompt_tokens": parsed.usage.prompt_tokens,
                }),
            );

            return Ok(parsed.into_response(tokens_per_sec));
        }

        Err(ProviderError::MaxRetries)
    }

    // ── Utility methods ─────────────────────────────────────────────────

    /// Fetch the context window size (in tokens) from the provider's API.
    ///
    /// - **Anthropic**: `GET /v1/models/{model}` → `max_input_tokens`
    /// - **Local** (llama.cpp): `GET /models` → parse `--ctx-size` from args
    /// - **OpenAI / Codex**: the standard API does not expose this, returns `None`.
    pub async fn fetch_context_window(&self, model: &str) -> Option<u32> {
        let result = match self.kind {
            ProviderKind::Anthropic => self.fetch_context_window_anthropic(model).await,
            ProviderKind::Local => self.fetch_context_window_local(model).await,
            ProviderKind::OpenAi | ProviderKind::Codex => None,
        };
        crate::log::entry(
            crate::log::Level::Info,
            "fetch_context_window",
            &serde_json::json!({
                "model": model,
                "provider": format!("{:?}", self.kind),
                "result": result,
            }),
        );
        result
    }

    async fn fetch_context_window_anthropic(&self, model: &str) -> Option<u32> {
        let url = format!("{}/models/{}", self.api_base, model);
        let resp = self
            .client
            .get(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let data: serde_json::Value = resp.json().await.ok()?;
        data["max_input_tokens"].as_u64().map(|v| v as u32)
    }

    /// Fetch context window from a local OpenAI-compatible server.
    /// Supports vLLM/SGLang (`max_model_len`) and llama.cpp (`--ctx-size`).
    async fn fetch_context_window_local(&self, model: &str) -> Option<u32> {
        let url = format!("{}/models", self.api_base);
        let mut req = self.client.get(&url);
        if !self.api_key.is_empty() {
            req = req.bearer_auth(&self.api_key);
        }
        let resp = req.send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let data: serde_json::Value = resp.json().await.ok()?;
        let models = data["data"].as_array()?;
        let entry = models.iter().find(|m| m["id"].as_str() == Some(model))?;

        // vLLM / SGLang: top-level `max_model_len` field.
        if let Some(v) = entry["max_model_len"].as_u64() {
            return Some(v as u32);
        }

        // llama.cpp: `--ctx-size` in status args.
        if let Some(args) = entry["status"]["args"].as_array() {
            for i in 0..args.len().saturating_sub(1) {
                if args[i].as_str() == Some("--ctx-size") {
                    return args[i + 1].as_str()?.parse::<u32>().ok();
                }
            }
        }

        None
    }

    pub async fn compact(
        &self,
        messages: &[Message],
        model: &str,
        instructions: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<String, ProviderError> {
        const COMPACT_PROMPT: &str = include_str!("../prompts/compact.txt");

        let conversation = messages
            .iter()
            .filter_map(|m| {
                let role = match m.role {
                    Role::User => "User",
                    Role::Assistant => "Assistant",
                    Role::System => "System",
                    Role::Agent => "Agent",
                    Role::Tool => return None,
                };
                let text = m.content.as_ref().map(|c| c.as_text()).unwrap_or("");
                let text = text.trim();
                if text.is_empty() {
                    None
                } else {
                    Some(format!("{}: {}", role, text))
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n");

        let mut system_text = COMPACT_PROMPT.trim().to_string();
        if let Some(instructions) = instructions {
            system_text.push_str(&format!(
                "\n\nThe user has asked you to pay special attention to the following when summarizing:\n{}",
                instructions
            ));
        }

        let system = Message::system(system_text);
        let user = Message::user(Content::text(format!(
            "Conversation to summarize:\n\n{}",
            conversation
        )));
        let resp = self
            .chat(
                &[system, user],
                &[],
                model,
                ReasoningEffort::Off,
                &ChatOptions::new(cancel),
            )
            .await?;
        let summary = resp.content.unwrap_or_default();
        if summary.trim().is_empty() {
            return Err(ProviderError::InvalidResponse("empty summary".into()));
        }
        Ok(summary)
    }

    /// Simple helper: run a system+user message pair through `chat()` and return the text.
    async fn complete_simple(
        &self,
        messages: &[Message],
        model: &str,
    ) -> Result<String, ProviderError> {
        let cancel = CancellationToken::new();
        let resp = self
            .chat(
                messages,
                &[],
                model,
                ReasoningEffort::Off,
                &ChatOptions::new(&cancel),
            )
            .await?;
        let text = resp.content.unwrap_or_default().trim().to_string();
        if text.is_empty() {
            Err(ProviderError::InvalidResponse("empty response".into()))
        } else {
            Ok(text)
        }
    }

    pub async fn complete_predict(
        &self,
        messages: &[protocol::Message],
        model: &str,
    ) -> Result<String, ProviderError> {
        self.complete_simple(messages, model).await
    }

    async fn complete_short(&self, prompt: &str, model: &str) -> Result<String, ProviderError> {
        let messages = vec![
            Message::system("Reasoning: low".to_string()),
            Message::user(Content::text(prompt)),
        ];
        self.complete_simple(&messages, model).await
    }

    pub async fn extract_web_content(
        &self,
        content: &str,
        prompt: &str,
        model: &str,
    ) -> Result<String, ProviderError> {
        let messages = vec![
            Message::system(
                "Answer the user's question based solely on the provided web page content. Be concise and direct.".to_string(),
            ),
            Message::user(Content::text(format!(
                "<content>\n{content}\n</content>\n\n{prompt}"
            ))),
        ];
        self.complete_simple(&messages, model).await
    }

    pub async fn complete_title(
        &self,
        user_messages: &[String],
        model: &str,
    ) -> Result<(String, String), ProviderError> {
        let numbered: Vec<String> = user_messages
            .iter()
            .enumerate()
            .map(|(i, m)| format!("{}. {}", i + 1, m.replace('\n', " ")))
            .collect();
        let prompt = format!(
            "Generate a short session title and slug for a coding session. \
             Focus on the most recent topic/task, not earlier ones. \
             Reply with exactly two lines, no quotes:\n\
             title: <3-6 word title>\n\
             slug: <1-5 lowercase words separated by dashes, like a git branch name>\n\n\
             User messages (oldest to newest):\n{}",
            numbered.join("\n")
        );

        let raw = self.complete_short(&prompt, model).await?;
        let (title, slug) = parse_title_and_slug(&raw);

        Ok((title, slug))
    }
}

// ── Free functions ──────────────────────────────────────────────────────────

fn parse_title_and_slug(raw: &str) -> (String, String) {
    let mut title = String::new();
    let mut slug = String::new();

    for line in raw.lines() {
        let line = line.trim();
        if let Some(rest) = line
            .strip_prefix("title:")
            .or_else(|| line.strip_prefix("Title:"))
        {
            title = rest.trim().trim_matches('"').trim_matches('\'').to_string();
        } else if let Some(rest) = line
            .strip_prefix("slug:")
            .or_else(|| line.strip_prefix("Slug:"))
        {
            slug = rest.trim().trim_matches('"').trim_matches('\'').to_string();
        }
    }

    if title.is_empty() {
        title = normalize_short(raw);
    }
    if slug.is_empty() {
        slug = slugify(&title);
    }

    slug = slug
        .split('-')
        .filter(|w| !w.is_empty())
        .take(5)
        .collect::<Vec<_>>()
        .join("-");

    if title.len() > 64 {
        title.truncate(title.floor_char_boundary(64));
        title = title.trim().to_string();
    }

    (title, slug)
}

pub fn slugify(title: &str) -> String {
    title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

fn normalize_short(raw: &str) -> String {
    let mut t = raw.trim().trim_matches('"').trim_matches('\'').to_string();
    t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.len() > 64 {
        t.truncate(t.floor_char_boundary(64));
        t = t.trim().to_string();
    }
    t
}
