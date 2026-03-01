use crate::log;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    #[default]
    Off,
    Low,
    Medium,
    High,
}

impl ReasoningEffort {
    pub fn cycle(self) -> Self {
        match self {
            Self::Off => Self::Low,
            Self::Low => Self::Medium,
            Self::Medium => Self::High,
            Self::High => Self::Off,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    pub fn color(self) -> crossterm::style::Color {
        use crate::theme;
        match self {
            Self::Off => theme::REASON_OFF,
            Self::Low => theme::REASON_LOW,
            Self::Medium => theme::REASON_MED,
            Self::High => theme::REASON_HIGH,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    call_type: AlwaysFunction,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

/// Serde helper: always serializes as "function", accepts "function" on deserialize.
#[derive(Debug, Clone, Copy)]
struct AlwaysFunction;

impl Serialize for AlwaysFunction {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("function")
    }
}

impl<'de> Deserialize<'de> for AlwaysFunction {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = String::deserialize(d)?;
        if v == "function" {
            Ok(AlwaysFunction)
        } else {
            Err(serde::de::Error::custom(format!(
                "expected \"function\", got \"{}\"",
                v
            )))
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolDefinition {
    #[serde(rename = "type")]
    def_type: AlwaysFunction,
    pub function: FunctionSchema,
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionSchema {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

impl ToolDefinition {
    pub fn new(function: FunctionSchema) -> Self {
        Self {
            def_type: AlwaysFunction,
            function,
        }
    }
}

pub struct LLMResponse {
    pub content: Option<String>,
    pub reasoning_content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub prompt_tokens: Option<u32>,
}

pub struct Provider {
    api_base: String,
    api_key: String,
    client: Client,
    model_config: crate::config::ModelConfig,
    reasoning_effort: ReasoningEffort,
}

impl Provider {
    pub fn new(api_base: String, api_key: String, client: Client) -> Self {
        Self {
            api_base: api_base.trim_end_matches('/').to_string(),
            api_key,
            client,
            model_config: Default::default(),
            reasoning_effort: ReasoningEffort::Off,
        }
    }

    pub fn with_model_config(mut self, config: crate::config::ModelConfig) -> Self {
        self.model_config = config;
        self
    }

    pub fn with_reasoning_effort(mut self, effort: ReasoningEffort) -> Self {
        self.reasoning_effort = effort;
        self
    }

    pub async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        model: &str,
        cancel: &CancellationToken,
        on_retry: Option<&(dyn Fn(Duration, u32) + Send + Sync)>,
    ) -> Result<LLMResponse, String> {
        let mut body: HashMap<&str, serde_json::Value> = HashMap::new();
        body.insert("model", serde_json::json!(model));
        body.insert("messages", serde_json::to_value(messages).unwrap());
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
        if self.reasoning_effort != ReasoningEffort::Off {
            let effort = self.reasoning_effort.label();
            body.insert("reasoning_effort", serde_json::json!(effort));
            body.insert(
                "chat_template_kwargs",
                serde_json::json!({
                    "enable_thinking": true,
                    "reasoning_effort": effort,
                }),
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
            let mut req = self.client.post(&url).json(&body);
            if !self.api_key.is_empty() {
                req = req.bearer_auth(&self.api_key);
            }

            let resp = tokio::select! {
                _ = cancel.cancelled() => {
                    return Err("cancelled".into());
                }
                result = req.send() => match result {
                    Ok(r) => r,
                    Err(e) => {
                        if attempt < max_retries {
                            let delay = Duration::from_millis(500 * 2u64.pow(attempt as u32));
                            // Only show retrying after at least one retry has occurred
                            if attempt > 0 {
                                if let Some(f) = on_retry { f(delay, attempt as u32); }
                            }
                            tokio::time::sleep(delay).await;
                            continue;
                        }
                        return Err(e.to_string());
                    }
                }
            };

            if !resp.status().is_success() {
                let status = resp.status();
                let code = status.as_u16();
                let text = resp.text().await.unwrap_or_default();
                if (code == 429 || code >= 500) && attempt < max_retries {
                    let delay = Duration::from_millis(500 * 2u64.pow(attempt as u32));
                    // Only show retrying after at least one retry has occurred
                    if attempt > 0 {
                        if let Some(f) = on_retry {
                            f(delay, attempt as u32);
                        }
                    }
                    tokio::time::sleep(delay).await;
                    continue;
                }
                return Err(format!("API error {}: {}", status, text));
            }

            let data: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;

            let choice = data["choices"].get(0).ok_or("no choices in response")?;
            let msg = &choice["message"];

            let content = msg["content"].as_str().map(|s| s.to_string());
            let reasoning_content = msg["reasoning_content"].as_str().map(|s| s.to_string());

            let tool_calls: Vec<ToolCall> = if let Some(tcs) = msg.get("tool_calls") {
                serde_json::from_value(tcs.clone()).unwrap_or_default()
            } else {
                vec![]
            };

            let prompt_tokens = data["usage"]["prompt_tokens"].as_u64().map(|n| n as u32);

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
            });
        }

        Err("max retries exceeded".into())
    }

    /// Fetch the context window size for `model` from the /v1/models endpoint.
    /// Parses --ctx-size from the model's args list.
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
                let content = m.content.as_deref().unwrap_or("").trim();
                if content.is_empty() {
                    None
                } else {
                    Some(format!("{}: {}", role, content))
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n");

        let system = Message {
            role: Role::System,
            content: Some(COMPACT_PROMPT.trim().to_string()),
            tool_calls: None,
            tool_call_id: None,
        };
        let user = Message {
            role: Role::User,
            content: Some(format!("Conversation to summarize:\n\n{}", conversation)),
            tool_calls: None,
            tool_call_id: None,
        };
        let resp = self.chat(&[system, user], &[], model, cancel, None).await?;
        let summary = resp.content.unwrap_or_default();
        if summary.trim().is_empty() {
            return Err("empty summary".into());
        }
        Ok(summary)
    }

    pub async fn complete_title(
        &self,
        first_user_message: &str,
        model: &str,
    ) -> Result<String, String> {
        let prompt = format!(
            "Generate a short session title (3-6 words) for: \"{}\". Reply with only the title.",
            first_user_message.replace('\n', " ")
        );

        let body = serde_json::json!({
            "model": model,
            "messages": [
                {"role": "system", "content": "Reasoning: low"},
                {"role": "user", "content": prompt},
            ],
            "max_tokens": 512,
            "temperature": 0.2,
            "stop": ["\n"],
            "chat_template_kwargs": {"enable_thinking": false},
        });

        log::entry(
            log::Level::Debug,
            "title_request",
            &serde_json::json!({
                "model": model,
                "prompt_len": first_user_message.len(),
            }),
        );

        let url = format!("{}/chat/completions", self.api_base);
        let mut req = self.client.post(&url).json(&body);
        if !self.api_key.is_empty() {
            req = req.bearer_auth(&self.api_key);
        }

        let resp = req.send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("API error {}: {}", status, text));
        }

        let data: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        let text = data["choices"]
            .get(0)
            .and_then(|c| c["message"]["content"].as_str())
            .unwrap_or("")
            .to_string();

        let title = normalize_title(&text);
        log::entry(
            log::Level::Debug,
            "title_response",
            &serde_json::json!({
                "title": title,
            }),
        );

        if title.is_empty() {
            Err("empty title".into())
        } else {
            Ok(title)
        }
    }
}

fn normalize_title(raw: &str) -> String {
    let mut t = raw.trim().trim_matches('"').trim_matches('\'').to_string();
    t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.len() > 64 {
        t.truncate(64);
        t = t.trim().to_string();
    }
    t
}
