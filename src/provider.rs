use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

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
            Err(serde::de::Error::custom(format!("expected \"function\", got \"{}\"", v)))
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
        Self { def_type: AlwaysFunction, function }
    }
}

pub struct LLMResponse {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub prompt_tokens: Option<u32>,
}

pub struct Provider {
    api_base: String,
    api_key: String,
    client: Client,
}

impl Provider {
    pub fn new(api_base: &str, api_key: &str) -> Self {
        Self {
            api_base: api_base.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            client: Client::new(),
        }
    }

    pub async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        model: &str,
        cancel: &CancellationToken,
    ) -> Result<LLMResponse, String> {
        let mut body: HashMap<&str, serde_json::Value> = HashMap::new();
        body.insert("model", serde_json::json!(model));
        body.insert("messages", serde_json::to_value(messages).unwrap());
        if !tools.is_empty() {
            body.insert("tools", serde_json::to_value(tools).unwrap());
        }

        let url = format!("{}/chat/completions", self.api_base);
        let max_retries = 3;

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
                    tokio::time::sleep(delay).await;
                    continue;
                }
                return Err(format!("API error {}: {}", status, text));
            }

            let data: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;

            let choice = data["choices"]
                .get(0)
                .ok_or("no choices in response")?;
            let msg = &choice["message"];

            let content = msg["content"].as_str().map(|s| s.to_string());

            let tool_calls: Vec<ToolCall> = if let Some(tcs) = msg.get("tool_calls") {
                serde_json::from_value(tcs.clone()).unwrap_or_default()
            } else {
                vec![]
            };

            let prompt_tokens = data["usage"]["prompt_tokens"].as_u64().map(|n| n as u32);

            return Ok(LLMResponse {
                content,
                tool_calls,
                prompt_tokens,
            });
        }

        Err("max retries exceeded".into())
    }
}
