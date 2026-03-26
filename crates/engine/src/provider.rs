use crate::cancel::CancellationToken;
use crate::log;
use crate::tools::trim_tool_output;
use futures_util::StreamExt;
use protocol::{Content, FunctionCall, Message, ReasoningEffort, Role, ToolCall};
use reqwest::Client;
use serde::Serialize;
use std::collections::HashMap;
use std::time::{Duration, Instant};

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

/// Serde helper: always serializes as "function" for tool definition type field.
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

/// Parsed fields from an API response: (content, reasoning, tool_calls, prompt_tokens, completion_tokens).
type ParsedResponse = (
    Option<String>,
    Option<String>,
    Vec<ToolCall>,
    Option<u32>,
    Option<u32>,
);

/// A streaming delta from the LLM.
pub enum StreamDelta<'a> {
    Text(&'a str),
    Thinking(&'a str),
}

pub struct LLMResponse {
    pub content: Option<String>,
    pub reasoning_content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    pub tokens_per_sec: Option<f64>,
}

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

    /// Classify an HTTP error response into a `ProviderError`.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    OpenAi,
    Anthropic,
    Local,
}

impl ProviderKind {
    /// Default reasoning effort levels available for cycling in the TUI.
    pub fn default_reasoning_cycle(self) -> &'static [ReasoningEffort] {
        match self {
            Self::OpenAi | Self::Anthropic => &[
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

    /// Resolve from the config `type` string.
    pub fn from_config(provider_type: &str) -> Self {
        match provider_type {
            "openai" => Self::OpenAi,
            "anthropic" => Self::Anthropic,
            _ => Self::Local,
        }
    }

    /// Auto-detect from an API base URL. Used as a CLI convenience when
    /// no explicit `--type` is provided.
    pub fn detect_from_url(api_base: &str) -> Self {
        if api_base.contains("api.openai.com") {
            Self::OpenAi
        } else if api_base.contains("api.anthropic.com") {
            Self::Anthropic
        } else {
            Self::Local
        }
    }

    /// Return the config type string for this kind.
    pub fn as_config_str(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
            Self::Local => "openai-compatible",
        }
    }
}

#[derive(Clone)]
pub struct Provider {
    api_base: String,
    api_key: String,
    client: Client,
    kind: ProviderKind,
    model_config: crate::config::ModelConfig,
}

/// Rewrite an Agent-role message as a user message for API serialization.
/// Agent messages are an internal concept; the LLM API sees them as user turns.
fn fixup_agent_message(m: &Message, v: &mut serde_json::Value) {
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
        }
    }

    pub fn with_model_config(mut self, config: crate::config::ModelConfig) -> Self {
        self.model_config = config;
        self
    }

    pub fn tool_calling(&self) -> bool {
        self.model_config.tool_calling()
    }

    /// Cloud APIs reject unknown parameters with 400 errors. These getters
    /// return `None` for params unsupported by the target provider.
    fn top_k(&self) -> Option<u32> {
        match self.kind {
            ProviderKind::OpenAi => None,
            _ => self.model_config.top_k,
        }
    }

    fn min_p(&self) -> Option<f64> {
        match self.kind {
            ProviderKind::Local => self.model_config.min_p,
            _ => None,
        }
    }

    fn repeat_penalty(&self) -> Option<f64> {
        match self.kind {
            ProviderKind::Local => self.model_config.repeat_penalty,
            _ => None,
        }
    }

    /// OpenAI uses `max_completion_tokens`; other providers use `max_tokens`.
    fn max_tokens_key(&self) -> &'static str {
        match self.kind {
            ProviderKind::OpenAi => "max_completion_tokens",
            _ => "max_tokens",
        }
    }

    /// Get the effort label to send to the API.
    /// Maps `Max` → `xhigh` for OpenAI.
    fn effort_label(&self, effort: ReasoningEffort) -> String {
        if effort == ReasoningEffort::Max {
            match self.kind {
                ProviderKind::OpenAi => "xhigh".to_string(),
                _ => "max".to_string(),
            }
        } else {
            effort.label().to_string()
        }
    }

    /// Check if the model supports adaptive thinking.
    ///
    /// Only Claude Opus 4.6 and Sonnet 4.6 support adaptive thinking.
    /// All other models (including Haiku 4.5, Sonnet 4.5, etc.) do not.
    fn supports_adaptive_thinking(&self, model: &str) -> bool {
        model.contains("opus-4-6") || model.contains("sonnet-4-6")
    }

    /// Insert provider-specific reasoning/thinking parameters into the body.
    ///
    /// - OpenAI: `reasoning_effort` (via Responses API)
    /// - Anthropic: handled in build_anthropic_messages_body
    /// - Local servers: `reasoning_effort` + `chat_template_kwargs`
    fn insert_reasoning(
        &self,
        body: &mut HashMap<&str, serde_json::Value>,
        effort: ReasoningEffort,
    ) {
        let label = self.effort_label(effort);
        match self.kind {
            ProviderKind::Anthropic => {
                // Anthropic uses native Messages API, handled separately.
            }
            ProviderKind::OpenAi => {
                // Reasoning is handled via Responses API body, not here.
            }
            ProviderKind::Local => {
                if effort != ReasoningEffort::Off {
                    body.insert("reasoning_effort", serde_json::json!(label));
                    body.insert(
                        "chat_template_kwargs",
                        serde_json::json!({
                            "enable_thinking": true,
                            "reasoning_effort": label,
                        }),
                    );
                } else {
                    body.insert(
                        "chat_template_kwargs",
                        serde_json::json!({"enable_thinking": false}),
                    );
                }
            }
        }
    }

    /// Insert `chat_template_kwargs` with thinking disabled for utility
    /// requests (title generation, web extraction, etc.).
    fn insert_no_thinking(&self, body: &mut serde_json::Value) {
        if self.kind == ProviderKind::Local {
            body["chat_template_kwargs"] = serde_json::json!({"enable_thinking": false});
        }
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

    #[allow(clippy::too_many_arguments)]
    pub async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        model: &str,
        reasoning_effort: ReasoningEffort,
        cancel: &CancellationToken,
        on_retry: Option<&(dyn Fn(Duration, u32) + Send + Sync)>,
        on_delta: Option<&(dyn Fn(StreamDelta) + Send + Sync)>,
    ) -> Result<LLMResponse, ProviderError> {
        let (url, mut body, is_anthropic_messages) = match self.kind {
            ProviderKind::OpenAi => {
                let url = format!("{}/responses", self.api_base);
                let body = self.build_responses_body(messages, tools, model, reasoning_effort);
                (url, body, false)
            }
            ProviderKind::Anthropic => {
                let url = format!("{}/messages", self.api_base);
                let body =
                    self.build_anthropic_messages_body(messages, tools, model, reasoning_effort);
                (url, body, true)
            }
            _ => {
                let url = format!("{}/chat/completions", self.api_base);
                let body = self.build_chat_body(messages, tools, model, reasoning_effort);
                (url, serde_json::to_value(body).unwrap(), false)
            }
        };

        let use_stream = on_delta.is_some();
        if use_stream {
            body["stream"] = serde_json::json!(true);
            // Request usage data in the final streaming chunk so we can
            // compute tokens_per_sec.
            // Note: Anthropic Messages API doesn't support stream_options.
            if !matches!(self.kind, ProviderKind::OpenAi | ProviderKind::Anthropic) {
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
            if !self.api_key.is_empty() {
                if is_anthropic_messages {
                    // Anthropic native API uses x-api-key header, not Bearer auth.
                    req = req.header("x-api-key", &self.api_key);
                } else {
                    req = req.bearer_auth(&self.api_key);
                }
            }
            if is_anthropic_messages {
                req = req.header("anthropic-version", "2023-06-01");
            }

            let resp = tokio::select! {
                _ = cancel.cancelled() => {
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
                            let delay = Duration::from_millis(500 * 2u64.pow(attempt as u32));
                            if attempt > 0 {
                                if let Some(f) = on_retry { f(delay, attempt as u32); }
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
                        "error": err.to_string(),
                    }),
                );

                if err.is_retryable() && attempt < max_retries {
                    let delay = Duration::from_millis(500 * 2u64.pow(attempt as u32));
                    if attempt > 0 {
                        if let Some(f) = on_retry {
                            f(delay, attempt as u32);
                        }
                    }
                    tokio::time::sleep(delay).await;
                    continue;
                }
                return Err(err);
            }

            let parsed = if let Some(on_delta) = on_delta {
                match self.kind {
                    ProviderKind::OpenAi => {
                        self.read_responses_stream(resp, cancel, on_delta).await
                    }
                    ProviderKind::Anthropic => {
                        self.read_anthropic_messages_stream(resp, cancel, on_delta)
                            .await
                    }
                    _ => self.read_chat_stream(resp, cancel, on_delta).await,
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
                    ProviderKind::OpenAi => Self::parse_responses_response(&data)?,
                    ProviderKind::Anthropic => Self::parse_anthropic_messages_response(&data)?,
                    _ => Self::parse_chat_response(&data)?,
                }
            };

            let (content, reasoning_content, tool_calls, prompt_tokens, completion_tokens) = parsed;

            let elapsed = request_start.elapsed();
            let tokens_per_sec = completion_tokens.and_then(|c| {
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
                    "content": content,
                    "reasoning_content": reasoning_content,
                    "tool_calls": tool_calls,
                    "prompt_tokens": prompt_tokens,
                }),
            );

            return Ok(LLMResponse {
                content,
                reasoning_content,
                tool_calls,
                prompt_tokens,
                completion_tokens,
                tokens_per_sec,
            });
        }

        Err(ProviderError::MaxRetries)
    }

    // ── SSE streaming helpers ─────────────────────────────────────────

    /// Read an SSE stream from the Chat Completions API and accumulate the response.
    async fn read_chat_stream(
        &self,
        resp: reqwest::Response,
        cancel: &CancellationToken,
        on_delta: &(dyn Fn(StreamDelta) + Send + Sync),
    ) -> Result<ParsedResponse, ProviderError> {
        let mut content = String::new();
        let mut reasoning_content = String::new();
        let mut tool_calls: HashMap<usize, (String, String, String)> = HashMap::new(); // idx -> (id, name, args)
        let mut prompt_tokens: Option<u32> = None;
        let mut completion_tokens: Option<u32> = None;
        let mut sse_buf = String::new();

        let mut stream = resp.bytes_stream();

        loop {
            let chunk = tokio::select! {
                _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
                chunk = stream.next() => chunk,
            };
            let chunk = match chunk {
                Some(Ok(bytes)) => bytes,
                Some(Err(e)) => return Err(ProviderError::Network(e.to_string())),
                None => break,
            };
            sse_buf.push_str(&String::from_utf8_lossy(&chunk));

            // Process complete SSE lines
            while let Some(pos) = sse_buf.find('\n') {
                let raw: String = sse_buf.drain(..pos + 1).collect();
                let line = raw.trim_end_matches('\n').trim_end_matches('\r');

                if !line.starts_with("data: ") {
                    continue;
                }
                let data = &line[6..];
                if data == "[DONE]" {
                    continue;
                }

                let ev: serde_json::Value = match serde_json::from_str(data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                // Extract usage from the final chunk
                if let Some(usage) = ev.get("usage") {
                    prompt_tokens = usage["prompt_tokens"].as_u64().map(|n| n as u32);
                    completion_tokens =
                        completion_tokens.or(usage["completion_tokens"].as_u64().map(|n| n as u32));
                }

                let delta = match ev["choices"].get(0).and_then(|c| c.get("delta")) {
                    Some(d) => d,
                    None => continue,
                };

                // Text content delta
                if let Some(text) = delta["content"].as_str() {
                    if !text.is_empty() {
                        content.push_str(text);
                        on_delta(StreamDelta::Text(text));
                    }
                }

                // Reasoning content delta
                if let Some(text) = delta
                    .get("reasoning_content")
                    .or_else(|| delta.get("reasoning"))
                    .and_then(|v| v.as_str())
                {
                    if !text.is_empty() {
                        reasoning_content.push_str(text);
                        on_delta(StreamDelta::Thinking(text));
                    }
                }

                // Tool call deltas
                if let Some(tcs) = delta["tool_calls"].as_array() {
                    for tc in tcs {
                        let idx = tc["index"].as_u64().unwrap_or(0) as usize;
                        let entry = tool_calls.entry(idx).or_insert_with(|| {
                            let id = tc["id"].as_str().unwrap_or("").to_string();
                            let name = tc["function"]["name"].as_str().unwrap_or("").to_string();
                            (id, name, String::new())
                        });
                        if let Some(args) = tc["function"]["arguments"].as_str() {
                            entry.2.push_str(args);
                        }
                    }
                }
            }
        }

        let content = if content.is_empty() {
            None
        } else {
            Some(content)
        };
        let reasoning = if reasoning_content.is_empty() {
            None
        } else {
            Some(reasoning_content)
        };

        let mut tc_vec: Vec<(usize, ToolCall)> = tool_calls
            .into_iter()
            .map(|(idx, (id, name, args))| {
                (
                    idx,
                    ToolCall::new(
                        id,
                        FunctionCall {
                            name,
                            arguments: args,
                        },
                    ),
                )
            })
            .collect();
        tc_vec.sort_by_key(|(idx, _)| *idx);
        let tool_calls: Vec<ToolCall> = tc_vec.into_iter().map(|(_, tc)| tc).collect();

        // Fallback: extract tool calls from text (vLLM etc.)
        if tool_calls.is_empty() {
            let (from_content, cleaned_content) = extract_tool_calls_from_text(content.as_deref());
            let (from_reasoning, cleaned_reasoning) =
                extract_tool_calls_from_text(reasoning.as_deref());
            if !from_content.is_empty() || !from_reasoning.is_empty() {
                let tool_calls: Vec<ToolCall> =
                    from_content.into_iter().chain(from_reasoning).collect();
                return Ok((
                    cleaned_content,
                    cleaned_reasoning,
                    tool_calls,
                    prompt_tokens,
                    completion_tokens,
                ));
            }
        }

        Ok((
            content,
            reasoning,
            tool_calls,
            prompt_tokens,
            completion_tokens,
        ))
    }

    /// Read an SSE stream from the OpenAI Responses API and accumulate the response.
    async fn read_responses_stream(
        &self,
        resp: reqwest::Response,
        cancel: &CancellationToken,
        on_delta: &(dyn Fn(StreamDelta) + Send + Sync),
    ) -> Result<ParsedResponse, ProviderError> {
        let mut content = String::new();
        let mut reasoning_content = String::new();
        // Map from item_id to (id, call_id, name, args)
        let mut tool_calls: HashMap<String, (String, String, String, String)> = HashMap::new();
        let mut prompt_tokens: Option<u32> = None;
        let mut completion_tokens: Option<u32> = None;
        let mut sse_buf = String::new();

        let mut stream = resp.bytes_stream();

        loop {
            let chunk = tokio::select! {
                _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
                chunk = stream.next() => chunk,
            };
            let chunk = match chunk {
                Some(Ok(bytes)) => bytes,
                Some(Err(e)) => return Err(ProviderError::Network(e.to_string())),
                None => break,
            };
            sse_buf.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(pos) = sse_buf.find('\n') {
                let raw: String = sse_buf.drain(..pos + 1).collect();
                let line = raw.trim_end_matches('\n').trim_end_matches('\r');

                // Responses API uses "event:" + "data:" lines
                if !line.starts_with("data: ") {
                    continue;
                }
                let data = &line[6..];
                if data == "[DONE]" {
                    continue;
                }

                let ev: serde_json::Value = match serde_json::from_str(data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                let ev_type = ev["type"].as_str().unwrap_or("");

                match ev_type {
                    "response.output_item.added" => {
                        // Initialize tool call with id, call_id, and name from the added event
                        if ev["item"]["type"].as_str() == Some("function_call") {
                            let item = &ev["item"];
                            let id = item["id"].as_str().unwrap_or("").to_string();
                            let call_id = item["call_id"].as_str().unwrap_or("").to_string();
                            let name = item["name"].as_str().unwrap_or("").to_string();
                            // Use id as the key for tracking across events
                            if !id.is_empty() && !name.is_empty() {
                                tool_calls.insert(id.clone(), (id, call_id, name, String::new()));
                            }
                        }
                    }
                    "response.output_text.delta" => {
                        if let Some(text) = ev["delta"].as_str() {
                            if !text.is_empty() {
                                content.push_str(text);
                                on_delta(StreamDelta::Text(text));
                            }
                        }
                    }
                    "response.function_call_arguments.delta" => {
                        // Use item_id to track the tool call
                        let item_id = ev["item_id"].as_str().unwrap_or("").to_string();
                        if let Some(entry) = tool_calls.get_mut(&item_id) {
                            if let Some(args) = ev["delta"].as_str() {
                                entry.3.push_str(args);
                            }
                        }
                    }
                    "response.function_call_arguments.done" => {
                        // Use item_id to find the tool call and set the final arguments
                        let item_id = ev["item_id"].as_str().unwrap_or("").to_string();
                        let args = ev["arguments"].as_str().unwrap_or("{}").to_string();
                        if let Some(entry) = tool_calls.get_mut(&item_id) {
                            entry.3 = args;
                        }
                    }
                    "response.reasoning.delta" | "response.reasoning_summary_text.delta" => {
                        if let Some(text) = ev["delta"].as_str() {
                            if !text.is_empty() {
                                reasoning_content.push_str(text);
                                on_delta(StreamDelta::Thinking(text));
                            }
                        }
                    }
                    "response.completed" | "response.done" => {
                        if let Some(usage) = ev.get("response").and_then(|r| r.get("usage")) {
                            prompt_tokens = usage["input_tokens"].as_u64().map(|n| n as u32);
                            completion_tokens = usage["output_tokens"].as_u64().map(|n| n as u32);
                        }
                    }
                    _ => {}
                }
            }
        }

        let content = if content.is_empty() {
            None
        } else {
            Some(content)
        };
        let reasoning = if reasoning_content.is_empty() {
            None
        } else {
            Some(reasoning_content)
        };

        // Convert to Vec<ToolCall>, using call_id as the ToolCall id
        let tool_calls: Vec<ToolCall> = tool_calls
            .into_values()
            .filter(|(_, call_id, name, _)| !call_id.is_empty() && !name.is_empty())
            .map(|(_id, call_id, name, args)| {
                ToolCall::new(
                    call_id,
                    FunctionCall {
                        name,
                        arguments: args,
                    },
                )
            })
            .collect();

        Ok((
            content,
            reasoning,
            tool_calls,
            prompt_tokens,
            completion_tokens,
        ))
    }

    /// Read an SSE stream from the Anthropic Messages API and accumulate the response.
    async fn read_anthropic_messages_stream(
        &self,
        resp: reqwest::Response,
        cancel: &CancellationToken,
        on_delta: &(dyn Fn(StreamDelta) + Send + Sync),
    ) -> Result<ParsedResponse, ProviderError> {
        let mut content = String::new();
        let mut reasoning_content = String::new();
        let mut tool_calls: HashMap<usize, (String, String, String)> = HashMap::new(); // idx -> (id, name, args)
        let mut prompt_tokens: Option<u32> = None;
        let mut completion_tokens: Option<u32> = None;
        let mut sse_buf = String::new();

        let mut stream = resp.bytes_stream();

        loop {
            let chunk = tokio::select! {
                _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
                chunk = stream.next() => chunk,
            };
            let chunk = match chunk {
                Some(Ok(bytes)) => bytes,
                Some(Err(e)) => return Err(ProviderError::Network(e.to_string())),
                None => break,
            };
            sse_buf.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(pos) = sse_buf.find('\n') {
                let raw: String = sse_buf.drain(..pos + 1).collect();
                let line = raw.trim_end_matches('\n').trim_end_matches('\r');

                if !line.starts_with("data: ") {
                    continue;
                }
                let data = &line[6..];
                if data == "[DONE]" {
                    continue;
                }

                let ev: serde_json::Value = match serde_json::from_str(data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                let event_type = ev["type"].as_str().unwrap_or("");

                match event_type {
                    "content_block_start" => {
                        if let Some(idx) = ev["index"].as_u64() {
                            if let Some(cb) = ev.get("content_block") {
                                if cb["type"].as_str() == Some("tool_use") {
                                    let id = cb["id"].as_str().unwrap_or_default().to_string();
                                    let name = cb["name"].as_str().unwrap_or_default().to_string();
                                    tool_calls.insert(idx as usize, (id, name, String::new()));
                                }
                            }
                        }
                    }
                    "content_block_delta" => {
                        if let Some(delta) = ev.get("delta") {
                            if delta["type"].as_str() == Some("text_delta") {
                                if let Some(text) = delta["text"].as_str() {
                                    if !text.is_empty() {
                                        content.push_str(text);
                                        on_delta(StreamDelta::Text(text));
                                    }
                                }
                            } else if delta["type"].as_str() == Some("thinking_delta") {
                                if let Some(text) = delta["thinking"].as_str() {
                                    if !text.is_empty() {
                                        reasoning_content.push_str(text);
                                        on_delta(StreamDelta::Thinking(text));
                                    }
                                }
                            } else if delta["type"].as_str() == Some("input_json_delta") {
                                if let Some(partial_json) = delta["partial_json"].as_str() {
                                    if let Some(idx) = ev["index"].as_u64() {
                                        let idx = idx as usize;
                                        if let Some(entry) = tool_calls.get_mut(&idx) {
                                            entry.2.push_str(partial_json);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    "message_delta" => {
                        if let Some(usage) = ev.get("usage") {
                            prompt_tokens = usage["input_tokens"].as_u64().map(|n| n as u32);
                            completion_tokens = usage["output_tokens"].as_u64().map(|n| n as u32);
                        }
                    }
                    _ => {}
                }
            }
        }

        let content = if content.is_empty() {
            None
        } else {
            Some(content)
        };
        let reasoning = if reasoning_content.is_empty() {
            None
        } else {
            Some(reasoning_content)
        };

        let mut tc_vec: Vec<(usize, ToolCall)> = tool_calls
            .into_iter()
            .map(|(idx, (id, name, args))| {
                (
                    idx,
                    ToolCall::new(
                        id,
                        FunctionCall {
                            name,
                            arguments: args,
                        },
                    ),
                )
            })
            .collect();
        tc_vec.sort_by_key(|(idx, _)| *idx);
        let tool_calls: Vec<ToolCall> = tc_vec.into_iter().map(|(_, tc)| tc).collect();

        Ok((
            content,
            reasoning,
            tool_calls,
            prompt_tokens,
            completion_tokens,
        ))
    }

    // ── Request body builders ───────────────────────────────────────────

    /// Build a Chat Completions API request body (Anthropic compat, local servers).
    fn build_chat_body(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        model: &str,
        reasoning_effort: ReasoningEffort,
    ) -> HashMap<&str, serde_json::Value> {
        let mut body: HashMap<&str, serde_json::Value> = HashMap::new();
        body.insert("model", serde_json::json!(model));
        let api_messages: Vec<serde_json::Value> = messages
            .iter()
            .map(|m| {
                let mut v = serde_json::to_value(m).unwrap();
                if m.role == Role::Agent {
                    fixup_agent_message(m, &mut v);
                }
                if let Some(obj) = v.as_object_mut() {
                    obj.remove("is_error");
                    if m.role == Role::Tool {
                        if let Some(s) = obj.get("content").and_then(|c| c.as_str()) {
                            let trimmed = trim_tool_output(s, 200);
                            obj.insert("content".into(), serde_json::json!(trimmed));
                        }
                    }
                }
                v
            })
            .collect();
        body.insert("messages", serde_json::json!(api_messages));
        if !tools.is_empty() {
            body.insert("tools", serde_json::to_value(tools).unwrap());
        }
        if let Some(v) = self.model_config.temperature {
            body.insert("temperature", serde_json::json!(v));
        }
        if let Some(v) = self.model_config.top_p {
            body.insert("top_p", serde_json::json!(v));
        }
        if let Some(v) = self.top_k() {
            body.insert("top_k", serde_json::json!(v));
        }
        if let Some(v) = self.min_p() {
            body.insert("min_p", serde_json::json!(v));
        }
        if let Some(v) = self.repeat_penalty() {
            body.insert("repeat_penalty", serde_json::json!(v));
        }
        self.insert_reasoning(&mut body, reasoning_effort);
        body
    }

    /// Build an OpenAI Responses API request body.
    fn build_responses_body(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        model: &str,
        reasoning_effort: ReasoningEffort,
    ) -> serde_json::Value {
        // Convert messages to Responses API input items.
        let mut input = Vec::new();
        for m in messages {
            match m.role {
                Role::System => {
                    // System messages become instructions; handled below via
                    // the first system message. If there are multiple, append
                    // them as easy_input_message items with "developer" role.
                    input.push(serde_json::json!({
                        "role": "developer",
                        "content": m.content.as_ref().map(|c| c.as_text()).unwrap_or_default(),
                    }));
                }
                Role::User => {
                    let content_val = match &m.content {
                        Some(Content::Text(t)) => serde_json::json!(t),
                        Some(Content::Parts(parts)) => {
                            let items: Vec<serde_json::Value> = parts
                                .iter()
                                .map(|p| match p {
                                    protocol::ContentPart::Text { text } => {
                                        serde_json::json!({"type": "input_text", "text": text})
                                    }
                                    protocol::ContentPart::ImageUrl { url, .. } => {
                                        serde_json::json!({"type": "input_image", "image_url": url})
                                    }
                                })
                                .collect();
                            serde_json::json!(items)
                        }
                        None => serde_json::json!(""),
                    };
                    input.push(serde_json::json!({
                        "role": "user",
                        "content": content_val,
                    }));
                }
                Role::Assistant => {
                    // Assistant messages may contain text and/or tool calls.
                    if let Some(content) = &m.content {
                        input.push(serde_json::json!({
                            "type": "message",
                            "role": "assistant",
                            "content": [{"type": "output_text", "text": content.as_text()}],
                        }));
                    }
                    if let Some(tcs) = &m.tool_calls {
                        for tc in tcs {
                            input.push(serde_json::json!({
                                "type": "function_call",
                                "call_id": tc.id,
                                "name": tc.function.name,
                                "arguments": tc.function.arguments,
                            }));
                        }
                    }
                }
                Role::Agent => {
                    input.push(serde_json::json!({
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": m.agent_api_text()}],
                    }));
                }
                Role::Tool => {
                    let output = m.content.as_ref().map(|c| c.as_text()).unwrap_or_default();
                    let trimmed = trim_tool_output(output, 200);
                    // OpenAI Responses API requires call_id to be non-empty for function_call_output.
                    // Message::tool() always sets tool_call_id, so this should never be None.
                    let call_id = match &m.tool_call_id {
                        Some(id) if !id.is_empty() => id.as_str(),
                        _ => {
                            log::entry(
                                log::Level::Error,
                                "tool_message_missing_call_id",
                                &serde_json::json!({
                                    "content": output,
                                    "tool_call_id": m.tool_call_id.clone(),
                                }),
                            );
                            // Use a placeholder if somehow call_id is missing/empty
                            "missing_call_id"
                        }
                    };
                    input.push(serde_json::json!({
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": trimmed,
                    }));
                }
            }
        }

        // Convert tools to Responses API format (flattened).
        let api_tools: Vec<serde_json::Value> = tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "name": t.function.name,
                    "description": t.function.description,
                    "parameters": t.function.parameters,
                })
            })
            .collect();

        let mut body = serde_json::json!({
            "model": model,
            "input": input,
        });

        if !api_tools.is_empty() {
            body["tools"] = serde_json::json!(api_tools);
        }
        if let Some(v) = self.model_config.temperature {
            body["temperature"] = serde_json::json!(v);
        }
        if let Some(v) = self.model_config.top_p {
            body["top_p"] = serde_json::json!(v);
        }
        if reasoning_effort != ReasoningEffort::Off {
            body["reasoning"] = serde_json::json!({
                "effort": self.effort_label(reasoning_effort),
                "summary": "auto",
            });
        }

        body
    }

    /// Build an Anthropic Messages API request body.
    fn build_anthropic_messages_body(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        model: &str,
        reasoning_effort: ReasoningEffort,
    ) -> serde_json::Value {
        // Convert messages to Anthropic format.
        let mut system_content: Option<String> = None;
        let mut content: Vec<serde_json::Value> = Vec::new();

        for m in messages {
            match m.role {
                Role::System => {
                    // Collect system messages into a single system prompt.
                    let text = m.content.as_ref().map(|c| c.as_text()).unwrap_or_default();
                    match &mut system_content {
                        Some(s) => s.push_str(&format!("\n\n{}", text)),
                        None => system_content = Some(text.to_string()),
                    }
                }
                Role::User => {
                    content.push(serde_json::json!({
                        "role": "user",
                        "content": m.content.as_ref().map(|c| c.as_text()).unwrap_or_default(),
                    }));
                }
                Role::Assistant => {
                    let mut message_content = Vec::new();

                    // Add text content if present.
                    if let Some(c) = &m.content {
                        message_content.push(serde_json::json!({
                            "type": "text",
                            "text": c.as_text(),
                        }));
                    }

                    // Add tool use blocks.
                    if let Some(tcs) = &m.tool_calls {
                        for tc in tcs {
                            message_content.push(serde_json::json!({
                                "type": "tool_use",
                                "id": tc.id,
                                "name": tc.function.name,
                                "input": serde_json::from_str(&tc.function.arguments).unwrap_or_else(|_| serde_json::json!({})),
                            }));
                        }
                    }

                    content.push(serde_json::json!({
                        "role": "assistant",
                        "content": message_content,
                    }));
                }
                Role::Agent => {
                    // Agent messages are treated as user messages.
                    content.push(serde_json::json!({
                        "role": "user",
                        "content": m.agent_api_text(),
                    }));
                }
                Role::Tool => {
                    let output = m.content.as_ref().map(|c| c.as_text()).unwrap_or_default();
                    let trimmed = trim_tool_output(output, 200);
                    // tool_result must be in an array, and must come first.
                    content.push(serde_json::json!({
                        "role": "user",
                        "content": [
                            {
                                "type": "tool_result",
                                "tool_use_id": m.tool_call_id.as_deref().unwrap_or(""),
                                "content": trimmed,
                            }
                        ],
                    }));
                }
            }
        }

        // Convert tools to Anthropic format.
        let api_tools: Vec<serde_json::Value> = tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.function.name,
                    "description": t.function.description,
                    "input_schema": t.function.parameters,
                })
            })
            .collect();

        let mut body = serde_json::json!({
            "model": model,
            "messages": content,
            "max_tokens": 4096,
        });

        if let Some(sys) = system_content {
            body["system"] = serde_json::json!([{"type": "text", "text": sys}]);
        }

        if !api_tools.is_empty() {
            body["tools"] = serde_json::json!(api_tools);
        }

        if let Some(v) = self.model_config.temperature {
            body["temperature"] = serde_json::json!(v);
        }
        if let Some(v) = self.model_config.top_p {
            body["top_p"] = serde_json::json!(v);
        }

        // Add thinking config for adaptive thinking (only supported on Opus 4.6 and Sonnet 4.6).
        if reasoning_effort != ReasoningEffort::Off && self.supports_adaptive_thinking(model) {
            body["thinking"] = serde_json::json!({
                "type": "adaptive",
                "display": "summarized",
            });
            body["output_config"] = serde_json::json!({
                "effort": self.effort_label(reasoning_effort),
            });
        }

        body
    }

    // ── Response parsers ────────────────────────────────────────────────

    /// Parse an Anthropic Messages API response.
    fn parse_anthropic_messages_response(
        data: &serde_json::Value,
    ) -> Result<ParsedResponse, ProviderError> {
        let mut content: Option<String> = None;
        let mut reasoning_content: Option<String> = None;
        let mut tool_calls: Vec<ToolCall> = Vec::new();

        // Extract content blocks.
        if let Some(content_blocks) = data["content"].as_array() {
            for block in content_blocks {
                match block["type"].as_str() {
                    Some("text") => {
                        let text = block["text"].as_str().unwrap_or_default();
                        match &mut content {
                            Some(c) => c.push_str(text),
                            None => content = Some(text.to_string()),
                        }
                    }
                    Some("thinking") => {
                        // Thinking block with text content.
                        if let Some(text) = block["thinking"].as_str() {
                            match &mut reasoning_content {
                                Some(r) => r.push_str(text),
                                None => reasoning_content = Some(text.to_string()),
                            }
                        }
                    }
                    Some("tool_use") => {
                        let id = block["id"].as_str().unwrap_or_default().to_string();
                        let name = block["name"].as_str().unwrap_or_default().to_string();
                        // input is already a JSON object, serialize it back to string.
                        let input = block["input"].clone();
                        let arguments = input.to_string();
                        tool_calls.push(ToolCall::new(id, FunctionCall { name, arguments }));
                    }
                    _ => {}
                }
            }
        }

        // Also check for thinking in the top-level thinking field (summary mode).
        if reasoning_content.is_none() {
            if let Some(thinking) = data["thinking"].as_array() {
                for block in thinking {
                    if let Some(text) = block["text"].as_str() {
                        match &mut reasoning_content {
                            Some(r) => r.push_str(text),
                            None => reasoning_content = Some(text.to_string()),
                        }
                    }
                }
            }
        }

        // Extract usage.
        let prompt_tokens = data["usage"]["input_tokens"].as_u64().map(|n| n as u32);
        let completion_tokens = data["usage"]["output_tokens"].as_u64().map(|n| n as u32);

        Ok((
            content,
            reasoning_content,
            tool_calls,
            prompt_tokens,
            completion_tokens,
        ))
    }

    /// Parse a Chat Completions API response.
    fn parse_chat_response(data: &serde_json::Value) -> Result<ParsedResponse, ProviderError> {
        let choice = data["choices"]
            .get(0)
            .ok_or_else(|| ProviderError::InvalidResponse("no choices in response".into()))?;
        let msg = &choice["message"];

        let mut content = msg["content"].as_str().map(|s| s.to_string());
        let mut reasoning_content = msg["reasoning_content"]
            .as_str()
            .or_else(|| msg["reasoning"].as_str())
            .map(|s| s.to_string());

        let mut tool_calls: Vec<ToolCall> = if let Some(tcs) = msg.get("tool_calls") {
            serde_json::from_value(tcs.clone()).unwrap_or_default()
        } else {
            vec![]
        };

        // Fallback: some backends (vLLM with reasoning+tool calling) may
        // place <tool_call> markup inside `content` or `reasoning_content`
        // instead of populating `tool_calls`. Extract them client-side.
        if tool_calls.is_empty() {
            let (from_content, cleaned_content) = extract_tool_calls_from_text(content.as_deref());
            let (from_reasoning, cleaned_reasoning) =
                extract_tool_calls_from_text(reasoning_content.as_deref());
            if !from_content.is_empty() || !from_reasoning.is_empty() {
                tool_calls = from_content.into_iter().chain(from_reasoning).collect();
                content = cleaned_content;
                reasoning_content = cleaned_reasoning;
            }
        }

        let prompt_tokens = data["usage"]["prompt_tokens"].as_u64().map(|n| n as u32);
        let completion_tokens = data["usage"]["completion_tokens"]
            .as_u64()
            .map(|n| n as u32);

        Ok((
            content,
            reasoning_content,
            tool_calls,
            prompt_tokens,
            completion_tokens,
        ))
    }

    /// Parse an OpenAI Responses API response.
    fn parse_responses_response(data: &serde_json::Value) -> Result<ParsedResponse, ProviderError> {
        let output = data["output"]
            .as_array()
            .ok_or_else(|| ProviderError::InvalidResponse("no output in response".into()))?;

        let mut content: Option<String> = None;
        let mut reasoning_content: Option<String> = None;
        let mut tool_calls: Vec<ToolCall> = Vec::new();

        for item in output {
            match item["type"].as_str() {
                Some("message") => {
                    if let Some(parts) = item["content"].as_array() {
                        for part in parts {
                            if part["type"].as_str() == Some("output_text") {
                                let text = part["text"].as_str().unwrap_or_default();
                                match &mut content {
                                    Some(c) => c.push_str(text),
                                    None => content = Some(text.to_string()),
                                }
                            }
                        }
                    }
                }
                Some("function_call") => {
                    let call_id = item["call_id"].as_str().unwrap_or_default().to_string();
                    let name = item["name"].as_str().unwrap_or_default().to_string();
                    let arguments = item["arguments"].as_str().unwrap_or("{}").to_string();
                    tool_calls.push(ToolCall::new(call_id, FunctionCall { name, arguments }));
                }
                Some("reasoning") => {
                    // Try summary first (requires reasoning.summary in request).
                    let mut texts: Vec<&str> = Vec::new();
                    if let Some(summaries) = item["summary"].as_array() {
                        texts.extend(summaries.iter().filter_map(|s| s["text"].as_str()));
                    }
                    // Fall back to content (reasoning_text items).
                    if texts.is_empty() {
                        if let Some(parts) = item["content"].as_array() {
                            texts.extend(parts.iter().filter_map(|p| p["text"].as_str()));
                        }
                    }
                    if !texts.is_empty() {
                        reasoning_content = Some(texts.join("\n"));
                    }
                }
                _ => {}
            }
        }

        let prompt_tokens = data["usage"]["input_tokens"].as_u64().map(|n| n as u32);
        let completion_tokens = data["usage"]["output_tokens"].as_u64().map(|n| n as u32);

        Ok((
            content,
            reasoning_content,
            tool_calls,
            prompt_tokens,
            completion_tokens,
        ))
    }

    /// Fetch the context window size for `model` from the /v1/models endpoint.
    pub async fn fetch_context_window(&self, model: &str) -> Option<u32> {
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
        let args = entry["status"]["args"].as_array()?;
        for i in 0..args.len().saturating_sub(1) {
            if args[i].as_str() == Some("--ctx-size") {
                return args[i + 1].as_str()?.parse::<u32>().ok();
            }
        }
        None
    }

    /// Summarize `messages` into a compact string using the model.
    pub async fn compact(
        &self,
        messages: &[Message],
        model: &str,
        focus: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<String, ProviderError> {
        const COMPACT_PROMPT: &str = include_str!("prompts/compact.txt");

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
        if let Some(focus) = focus {
            system_text.push_str(&format!(
                "\n\nThe user has asked you to pay special attention to the following when summarizing:\n{}",
                focus
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
                cancel,
                None,
                None,
            )
            .await?;
        let summary = resp.content.unwrap_or_default();
        if summary.trim().is_empty() {
            return Err(ProviderError::InvalidResponse("empty summary".into()));
        }
        Ok(summary)
    }

    /// Fire-and-forget short completion.
    async fn complete_raw(&self, body: serde_json::Value) -> Result<String, ProviderError> {
        let url = format!("{}/chat/completions", self.api_base);
        let mut req = self.client.post(&url).json(&body);
        if !self.api_key.is_empty() {
            req = req.bearer_auth(&self.api_key);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| ProviderError::Network(e.to_string()))?;
        if !resp.status().is_success() {
            let code = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            return Err(ProviderError::from_http(code, text));
        }

        let data: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| ProviderError::InvalidResponse(e.to_string()))?;
        let text = data["choices"]
            .get(0)
            .and_then(|c| c["message"]["content"].as_str())
            .unwrap_or("")
            .trim()
            .to_string();

        if text.is_empty() {
            Err(ProviderError::InvalidResponse("empty response".into()))
        } else {
            Ok(text)
        }
    }

    /// Single-line completion for input prediction. Adds a `\n` stop sequence
    /// so the model returns at most one line.
    pub async fn complete_predict(
        &self,
        messages: &[protocol::Message],
        model: &str,
    ) -> Result<String, ProviderError> {
        let api_messages: Vec<serde_json::Value> = messages
            .iter()
            .map(|m| {
                let mut v = serde_json::to_value(m).unwrap();
                if m.role == Role::Agent {
                    fixup_agent_message(m, &mut v);
                }
                v
            })
            .collect();
        let mut body = serde_json::json!({
            "model": model,
            "messages": api_messages,
            "stop": ["\n"],
        });
        body[self.max_tokens_key()] = serde_json::json!(128);
        self.insert_no_thinking(&mut body);
        self.complete_raw(body).await
    }

    async fn complete_short(
        &self,
        prompt: &str,
        model: &str,
        max_tokens: u32,
        temperature: f32,
        multiline: bool,
    ) -> Result<String, ProviderError> {
        let mut body = serde_json::json!({
            "model": model,
            "messages": [
                {"role": "system", "content": "Reasoning: low"},
                {"role": "user", "content": prompt},
            ],
            "temperature": temperature,
        });
        body[self.max_tokens_key()] = serde_json::json!(max_tokens);
        self.insert_no_thinking(&mut body);
        if !multiline {
            body["stop"] = serde_json::json!(["\n"]);
        }
        let text = self.complete_raw(body).await?;
        if multiline {
            Ok(text.trim().to_string())
        } else {
            Ok(normalize_short(&text))
        }
    }

    pub async fn extract_web_content(
        &self,
        content: &str,
        prompt: &str,
        model: &str,
    ) -> Result<String, ProviderError> {
        let mut body = serde_json::json!({
            "model": model,
            "messages": [
                {
                    "role": "system",
                    "content": "Answer the user's question based solely on the provided web page content. Be concise and direct."
                },
                {
                    "role": "user",
                    "content": format!("<content>\n{content}\n</content>\n\n{prompt}"),
                },
            ],
            "temperature": 0.0,
        });
        body[self.max_tokens_key()] = serde_json::json!(4096);
        self.insert_no_thinking(&mut body);
        self.complete_raw(body).await
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

        let raw = self.complete_short(&prompt, model, 64, 0.2, true).await?;
        let (title, slug) = parse_title_and_slug(&raw);

        Ok((title, slug))
    }
}

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

    // Fallback: if no structured parse, treat entire response as title
    if title.is_empty() {
        title = normalize_short(raw);
    }

    // Fallback: slugify the title
    if slug.is_empty() {
        slug = slugify(&title);
    }

    // Enforce slug constraints
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

/// Extract `<tool_call>...</tool_call>` blocks from raw text.
///
/// Some backends (vLLM with reasoning + tool calling) place tool call markup
/// inside `content` or `reasoning_content` instead of the `tool_calls` field.
/// Following Ollama's approach (PR #14477), we treat `<tool_call>` as an
/// implicit end of any thinking block and parse the tool calls ourselves.
///
/// Returns the parsed tool calls and the cleaned text (with tool call blocks
/// removed). If the cleaned text is empty/whitespace, returns `None`.
fn extract_tool_calls_from_text(text: Option<&str>) -> (Vec<ToolCall>, Option<String>) {
    let Some(text) = text else {
        return (vec![], None);
    };

    let mut calls = Vec::new();
    let mut cleaned = String::with_capacity(text.len());
    let mut rest = text;
    let mut idx = 0;

    while let Some(open) = rest.find("<tool_call>") {
        cleaned.push_str(&rest[..open]);
        let after_open = &rest[open + "<tool_call>".len()..];

        if let Some(close) = after_open.find("</tool_call>") {
            let raw = after_open[..close].trim();
            if let Some(tc) = parse_tool_call_json(raw, &mut idx) {
                calls.push(tc);
            }
            rest = &after_open[close + "</tool_call>".len()..];
        } else {
            // Unclosed tag — try to parse the remainder as a tool call anyway
            let raw = after_open.trim();
            if let Some(tc) = parse_tool_call_json(raw, &mut idx) {
                calls.push(tc);
            }
            rest = "";
            break;
        }
    }
    cleaned.push_str(rest);

    // Also strip any orphaned </think> that may follow extracted tool calls
    let cleaned = cleaned.trim().to_string();
    let cleaned = if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    };

    (calls, cleaned)
}

/// Parse a tool call body in either JSON or XML-attribute format.
///
/// JSON format: `{"name": "...", "arguments": {...}}`
/// XML format:
/// ```text
/// <function=tool_name>
/// <parameter=key>value</parameter>
/// ...
/// </function>
/// ```
fn parse_tool_call_json(raw: &str, idx: &mut usize) -> Option<ToolCall> {
    // Try JSON first
    if let Some(tc) = parse_tool_call_json_inner(raw, idx) {
        return Some(tc);
    }
    // Fall back to XML-attribute format (<function=name><parameter=k>v</parameter>)
    if let Some(tc) = parse_tool_call_xml(raw, idx) {
        return Some(tc);
    }
    // Fall back to arg_key/arg_value format (<function>name</function><arg_key>k</arg_key><arg_value>v</arg_value>)
    parse_tool_call_arg_kv(raw, idx)
}

fn parse_tool_call_json_inner(raw: &str, idx: &mut usize) -> Option<ToolCall> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    let name = v["name"].as_str()?;
    let arguments = match &v["arguments"] {
        serde_json::Value::Null => return None,
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let id = format!("fallback-{idx}");
    *idx += 1;
    Some(ToolCall::new(
        id,
        FunctionCall {
            name: name.to_string(),
            arguments,
        },
    ))
}

/// Parse `<function=name><parameter=k>v</parameter>...</function>` format.
fn parse_tool_call_xml(raw: &str, idx: &mut usize) -> Option<ToolCall> {
    // Extract function name: <function=tool_name>
    let func_start = raw.find("<function=")?;
    let after_eq = &raw[func_start + "<function=".len()..];
    let name_end = after_eq.find('>')?;
    let name = after_eq[..name_end].trim().to_string();
    if name.is_empty() {
        return None;
    }

    // Extract parameters: <parameter=key>value</parameter>
    let mut params = serde_json::Map::new();
    let mut rest = &after_eq[name_end + 1..];
    while let Some(param_start) = rest.find("<parameter=") {
        let after_param_eq = &rest[param_start + "<parameter=".len()..];
        let key_end = after_param_eq.find('>')?;
        let key = after_param_eq[..key_end].trim();
        let value_start = &after_param_eq[key_end + 1..];
        let value_end = value_start.find("</parameter>")?;
        let value = value_start[..value_end].to_string();
        // Trim a single leading newline — the XML format often has one after `>`
        let value = value.strip_prefix('\n').unwrap_or(&value).to_string();
        let value = value.strip_suffix('\n').unwrap_or(&value).to_string();
        params.insert(key.to_string(), serde_json::Value::String(value));
        rest = &value_start[value_end + "</parameter>".len()..];
    }

    if params.is_empty() {
        return None;
    }

    let arguments = serde_json::Value::Object(params).to_string();
    let id = format!("fallback-{idx}");
    *idx += 1;
    Some(ToolCall::new(id, FunctionCall { name, arguments }))
}

/// Parse `<function>name</function><arg_key>k</arg_key><arg_value>v</arg_value>` format.
///
/// Some models (e.g. certain vLLM/reasoning backends) emit tool calls in this
/// format instead of the `<function=name><parameter=k>v</parameter>` variant.
/// The `thought` key is model reasoning and is stripped from the arguments.
fn parse_tool_call_arg_kv(raw: &str, idx: &mut usize) -> Option<ToolCall> {
    // Extract function name: <function>tool_name</function>
    let func_start = raw.find("<function>")?;
    let after_tag = &raw[func_start + "<function>".len()..];
    let func_end = after_tag.find("</function>")?;
    let name = after_tag[..func_end].trim().to_string();
    if name.is_empty() {
        return None;
    }

    // Extract arg_key/arg_value pairs
    let mut params = serde_json::Map::new();
    let mut rest = &after_tag[func_end + "</function>".len()..];
    while let Some(key_start) = rest.find("<arg_key>") {
        let after_key_tag = &rest[key_start + "<arg_key>".len()..];
        let key_end = after_key_tag.find("</arg_key>")?;
        let key = after_key_tag[..key_end].trim().to_string();
        rest = &after_key_tag[key_end + "</arg_key>".len()..];

        let val_start = rest.find("<arg_value>")?;
        let after_val_tag = &rest[val_start + "<arg_value>".len()..];
        let val_end = after_val_tag.find("</arg_value>")?;
        let value = after_val_tag[..val_end].to_string();
        let value = value.strip_prefix('\n').unwrap_or(&value).to_string();
        let value = value.strip_suffix('\n').unwrap_or(&value).to_string();
        rest = &after_val_tag[val_end + "</arg_value>".len()..];

        // "thought" is model reasoning, not an actual argument
        if key != "thought" {
            params.insert(key, serde_json::Value::String(value));
        }
    }

    if params.is_empty() {
        return None;
    }

    let arguments = serde_json::Value::Object(params).to_string();
    let id = format!("fallback-{idx}");
    *idx += 1;
    Some(ToolCall::new(id, FunctionCall { name, arguments }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_tool_calls_from_content() {
        let text = r#"Let me search for that.
<tool_call>
{"name": "search", "arguments": {"query": "rust async"}}
</tool_call>"#;

        let (calls, cleaned) = extract_tool_calls_from_text(Some(text));
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "search");
        assert_eq!(calls[0].function.arguments, r#"{"query":"rust async"}"#);
        assert_eq!(cleaned.unwrap(), "Let me search for that.");
    }

    #[test]
    fn extract_tool_calls_from_reasoning() {
        let text = r#"I need to call the tool
<tool_call>
{"name": "bash", "arguments": {"command": "ls"}}
</tool_call>
</think>"#;

        let (calls, cleaned) = extract_tool_calls_from_text(Some(text));
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "bash");
        // Remaining </think> is kept as cleaned text
        assert_eq!(cleaned.unwrap(), "I need to call the tool\n\n</think>");
    }

    #[test]
    fn extract_multiple_tool_calls() {
        let text = r#"<tool_call>
{"name": "read", "arguments": {"path": "a.rs"}}
</tool_call>
<tool_call>
{"name": "read", "arguments": {"path": "b.rs"}}
</tool_call>"#;

        let (calls, cleaned) = extract_tool_calls_from_text(Some(text));
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].function.name, "read");
        assert_eq!(calls[1].function.name, "read");
        assert_eq!(calls[0].id, "fallback-0");
        assert_eq!(calls[1].id, "fallback-1");
        // Only whitespace remains
        assert!(cleaned.is_none() || cleaned.as_deref() == Some(""));
    }

    #[test]
    fn no_tool_calls_passthrough() {
        let text = "Just regular content";
        let (calls, cleaned) = extract_tool_calls_from_text(Some(text));
        assert!(calls.is_empty());
        assert_eq!(cleaned.unwrap(), "Just regular content");
    }

    #[test]
    fn none_input() {
        let (calls, cleaned) = extract_tool_calls_from_text(None);
        assert!(calls.is_empty());
        assert!(cleaned.is_none());
    }

    #[test]
    fn extract_xml_format_tool_call() {
        let text = r#"
<tool_call>
<function=write_file>
<parameter=file_path>/testbed/test.py</parameter>
<parameter=content>
print("hello")
</parameter>
</function>
</tool_call>"#;

        let (calls, cleaned) = extract_tool_calls_from_text(Some(text));
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "write_file");
        let args: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["file_path"], "/testbed/test.py");
        assert_eq!(args["content"], "print(\"hello\")");
        assert!(cleaned.is_none() || cleaned.as_deref().unwrap().trim().is_empty());
    }

    #[test]
    fn extract_xml_format_with_multiline_content() {
        let text = r#"Let me try this:

<tool_call>
<function=write_file>
<parameter=content>
import sys

class Base:
    pass
</parameter>
<parameter=file_path>
/testbed/test_clear.py
</parameter>
</function>
</tool_call>"#;

        let (calls, cleaned) = extract_tool_calls_from_text(Some(text));
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "write_file");
        let args: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert!(args["content"].as_str().unwrap().contains("class Base:"));
        assert!(args["file_path"]
            .as_str()
            .unwrap()
            .contains("test_clear.py"));
        assert_eq!(cleaned.unwrap(), "Let me try this:");
    }

    #[test]
    fn extract_arg_kv_format_tool_call() {
        let text = r#"
<tool_call>
<function>bash</function>
<arg_key>thought</arg_key>
<arg_value>Let me check what files are here.</arg_value>
<arg_key>command</arg_key>
<arg_value>ls -la</arg_value>
</tool_call>"#;

        let (calls, cleaned) = extract_tool_calls_from_text(Some(text));
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "bash");
        let args: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        // "thought" should be stripped — it's model reasoning, not a tool argument
        assert!(args.get("thought").is_none());
        assert_eq!(args["command"], "ls -la");
        assert!(cleaned.is_none() || cleaned.as_deref().unwrap().trim().is_empty());
    }

    #[test]
    fn extract_arg_kv_format_multiple_args() {
        let text = r#"<tool_call>
<function>write_file</function>
<arg_key>file_path</arg_key>
<arg_value>/tmp/test.py</arg_value>
<arg_key>content</arg_key>
<arg_value>
print("hello")
</arg_value>
</tool_call>"#;

        let (calls, cleaned) = extract_tool_calls_from_text(Some(text));
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "write_file");
        let args: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["file_path"], "/tmp/test.py");
        assert_eq!(args["content"], "print(\"hello\")");
        assert!(cleaned.is_none() || cleaned.as_deref().unwrap().trim().is_empty());
    }
}
