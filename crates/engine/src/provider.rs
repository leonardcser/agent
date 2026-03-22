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

#[derive(Clone)]
pub struct Provider {
    api_base: String,
    api_key: String,
    client: Client,
    model_config: crate::config::ModelConfig,
}

impl Provider {
    pub fn new(api_base: String, api_key: String, client: Client) -> Self {
        Self {
            api_base: api_base.trim_end_matches('/').to_string(),
            api_key,
            client,
            model_config: Default::default(),
        }
    }

    pub fn with_model_config(mut self, config: crate::config::ModelConfig) -> Self {
        self.model_config = config;
        self
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
        let mut body: HashMap<&str, serde_json::Value> = HashMap::new();
        body.insert("model", serde_json::json!(model));
        let api_messages: Vec<serde_json::Value> = messages
            .iter()
            .map(|m| {
                let mut v = serde_json::to_value(m).unwrap();
                if let Some(obj) = v.as_object_mut() {
                    // Strip is_error — not part of the OpenAI API spec.
                    obj.remove("is_error");
                    // Trim large tool outputs to keep API payloads lean.
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
        if let Some(v) = self.model_config.top_k {
            body.insert("top_k", serde_json::json!(v));
        }
        if let Some(v) = self.model_config.min_p {
            body.insert("min_p", serde_json::json!(v));
        }
        if let Some(v) = self.model_config.repeat_penalty {
            body.insert("repeat_penalty", serde_json::json!(v));
        }
        if reasoning_effort != ReasoningEffort::Off {
            let effort = reasoning_effort.label();
            body.insert("reasoning_effort", serde_json::json!(effort));
            body.insert(
                "chat_template_kwargs",
                serde_json::json!({
                    "enable_thinking": true,
                    "reasoning_effort": effort,
                }),
            );
        } else {
            body.insert(
                "chat_template_kwargs",
                serde_json::json!({ "enable_thinking": false }),
            );
        }

        log::entry(
            log::Level::Debug,
            "request",
            &serde_json::json!({
                "model": model,
                "messages": messages,
                "tool_count": tools.len(),
            }),
        );

        let url = format!("{}/chat/completions", self.api_base);
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
                let (from_content, cleaned_content) =
                    extract_tool_calls_from_text(content.as_deref());
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
            "max_tokens": max_tokens,
            "temperature": temperature,
            "chat_template_kwargs": {"enable_thinking": false},
        });
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
        self.complete_raw(serde_json::json!({
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
            "max_tokens": 4096,
            "temperature": 0.0,
            "chat_template_kwargs": {"enable_thinking": false},
        }))
        .await
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

/// Parse a JSON tool call body: `{"name": "...", "arguments": {...}}`
fn parse_tool_call_json(raw: &str, idx: &mut usize) -> Option<ToolCall> {
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
}
