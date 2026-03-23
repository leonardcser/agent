use crate::cancel::CancellationToken;
use crate::log;
use crate::tools::trim_tool_output;
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    OpenAi,
    Anthropic,
    Local,
}

impl ProviderKind {
    /// Default reasoning effort levels available for cycling in the TUI.
    pub fn default_reasoning_efforts(self) -> &'static [ReasoningEffort] {
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

    /// Insert provider-specific reasoning/thinking parameters into the body.
    ///
    /// - OpenAI: `reasoning_effort` (via Responses API)
    /// - Anthropic (OpenAI compat): `thinking` (adaptive) + `output_config.effort`
    /// - Local servers: `reasoning_effort` + `chat_template_kwargs`
    fn insert_reasoning(
        &self,
        body: &mut HashMap<&str, serde_json::Value>,
        effort: ReasoningEffort,
    ) {
        let label = self.effort_label(effort);
        match self.kind {
            ProviderKind::Anthropic => {
                if effort != ReasoningEffort::Off {
                    body.insert("thinking", serde_json::json!({"type": "adaptive"}));
                    body.insert("output_config", serde_json::json!({"effort": label}));
                }
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

    pub async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        model: &str,
        reasoning_effort: ReasoningEffort,
        cancel: &CancellationToken,
        on_retry: Option<&(dyn Fn(Duration, u32) + Send + Sync)>,
    ) -> Result<LLMResponse, ProviderError> {
        let (url, body) = match self.kind {
            ProviderKind::OpenAi => {
                let url = format!("{}/responses", self.api_base);
                let body = self.build_responses_body(messages, tools, model, reasoning_effort);
                (url, body)
            }
            _ => {
                let url = format!("{}/chat/completions", self.api_base);
                let body = self.build_chat_body(messages, tools, model, reasoning_effort);
                (url, serde_json::to_value(body).unwrap())
            }
        };

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
                req = req.bearer_auth(&self.api_key);
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
                let status = resp.status();
                let code = status.as_u16();
                let text = resp.text().await.unwrap_or_default();

                let err = match code {
                    400 => ProviderError::InvalidResponse(text),
                    401 | 403 => ProviderError::Auth(text),
                    404 => ProviderError::NotFound(text),
                    429 if text.contains("insufficient_quota")
                        || text.contains("billing_not_active")
                        || text.contains("exceeded") =>
                    {
                        ProviderError::QuotaExceeded(text)
                    }
                    429 => ProviderError::RateLimited {
                        attempt: attempt as u32,
                    },
                    _ => ProviderError::Server {
                        status: code,
                        body: text,
                    },
                };

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

            let (content, reasoning_content, tool_calls, prompt_tokens, completion_tokens) =
                match self.kind {
                    ProviderKind::OpenAi => Self::parse_responses_response(&data)?,
                    _ => Self::parse_chat_response(&data)?,
                };

            let elapsed = request_start.elapsed();
            let tokens_per_sec = if let Some(completed) = completion_tokens {
                if completed > 0 && elapsed.as_secs_f64() >= 0.001 {
                    Some(completed as f64 / elapsed.as_secs_f64())
                } else {
                    None
                }
            } else {
                None
            };

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
                Role::Tool => {
                    let output = m.content.as_ref().map(|c| c.as_text()).unwrap_or_default();
                    let trimmed = trim_tool_output(output, 200);
                    input.push(serde_json::json!({
                        "type": "function_call_output",
                        "call_id": m.tool_call_id.as_deref().unwrap_or(""),
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

    // ── Response parsers ────────────────────────────────────────────────

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
    ) -> Result<String, String> {
        const COMPACT_PROMPT: &str = include_str!("prompts/compact.txt");

        let conversation = messages
            .iter()
            .filter_map(|m| {
                let role = match m.role {
                    Role::User => "User",
                    Role::Assistant => "Assistant",
                    Role::System => "System",
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
            )
            .await
            .map_err(|e| e.to_string())?;
        let summary = resp.content.unwrap_or_default();
        if summary.trim().is_empty() {
            return Err("empty summary".into());
        }
        Ok(summary)
    }

    /// Fire-and-forget short completion.
    async fn complete_raw(&self, body: serde_json::Value) -> Result<String, String> {
        let url = format!("{}/chat/completions", self.api_base);
        let mut req = self.client.post(&url).json(&body);
        if !self.api_key.is_empty() {
            req = req.bearer_auth(&self.api_key);
        }

        let resp = req.send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            let code = status.as_u16();
            if code == 429
                && (text.contains("insufficient_quota")
                    || text.contains("billing_not_active")
                    || text.contains("exceeded"))
            {
                return Err(format!("quota exceeded: {text}"));
            }
            return Err(format!("API error {}: {}", status, text));
        }

        let data: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        let text = data["choices"]
            .get(0)
            .and_then(|c| c["message"]["content"].as_str())
            .unwrap_or("")
            .trim()
            .to_string();

        if text.is_empty() {
            Err("empty response".into())
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
    ) -> Result<String, String> {
        let api_messages: Vec<serde_json::Value> = messages
            .iter()
            .map(|m| serde_json::to_value(m).unwrap())
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
    ) -> Result<String, String> {
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
    ) -> Result<String, String> {
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
    ) -> Result<(String, String), String> {
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
    // Fall back to XML-attribute format
    parse_tool_call_xml(raw, idx)
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
}
