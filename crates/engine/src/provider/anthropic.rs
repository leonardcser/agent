use super::{collect_indexed_tool_calls, non_empty, sse};
use super::{ParsedResponse, ProviderError, StreamDelta, ToolDefinition};
use crate::cancel::CancellationToken;
use crate::config::ModelConfig;
use crate::tools::{trim_tool_output, MAX_TOOL_OUTPUT_LINES};
use protocol::{FunctionCall, Message, ReasoningEffort, Role, TokenUsage, ToolCall};
use std::collections::HashMap;

fn supports_adaptive_thinking(model: &str) -> bool {
    model.contains("opus-4-6") || model.contains("sonnet-4-6")
}

fn parse_cache_write_tokens(u: &serde_json::Value) -> Option<u32> {
    u["cache_creation_input_tokens"]
        .as_u64()
        .map(|n| n as u32)
        .or_else(|| {
            let cc = u.get("cache_creation")?;
            let a = cc["ephemeral_5m_input_tokens"].as_u64().unwrap_or(0);
            let b = cc["ephemeral_1h_input_tokens"].as_u64().unwrap_or(0);
            if a + b > 0 {
                Some((a + b) as u32)
            } else {
                None
            }
        })
}

pub(super) fn build_body(
    messages: &[Message],
    tools: &[ToolDefinition],
    model: &str,
    effort: ReasoningEffort,
    config: &ModelConfig,
) -> serde_json::Value {
    let mut system_content: Option<String> = None;
    let mut content: Vec<serde_json::Value> = Vec::new();

    for m in messages {
        match m.role {
            Role::System => {
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
                if let Some(c) = &m.content {
                    message_content.push(serde_json::json!({
                        "type": "text",
                        "text": c.as_text(),
                    }));
                }
                if let Some(tcs) = &m.tool_calls {
                    for tc in tcs {
                        message_content.push(serde_json::json!({
                            "type": "tool_use",
                            "id": tc.id,
                            "name": tc.function.name,
                            "input": serde_json::from_str::<serde_json::Value>(&tc.function.arguments)
                                .unwrap_or_else(|_| serde_json::json!({})),
                        }));
                    }
                }
                content.push(serde_json::json!({
                    "role": "assistant",
                    "content": message_content,
                }));
            }
            Role::Agent => {
                content.push(serde_json::json!({
                    "role": "user",
                    "content": m.agent_api_text(),
                }));
            }
            Role::Tool => {
                let output = m.content.as_ref().map(|c| c.as_text()).unwrap_or_default();
                let trimmed = trim_tool_output(output, MAX_TOOL_OUTPUT_LINES);
                content.push(serde_json::json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": m.tool_call_id.as_deref().unwrap_or(""),
                        "content": trimmed,
                    }],
                }));
            }
        }
    }

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
    if let Some(v) = config.temperature {
        body["temperature"] = serde_json::json!(v);
    }
    if let Some(v) = config.top_p {
        body["top_p"] = serde_json::json!(v);
    }

    if effort != ReasoningEffort::Off && supports_adaptive_thinking(model) {
        body["thinking"] = serde_json::json!({
            "type": "adaptive",
            "display": "summarized",
        });
        body["output_config"] = serde_json::json!({
            "effort": effort.label(),
        });
    }

    body
}

pub(super) fn parse_response(data: &serde_json::Value) -> Result<ParsedResponse, ProviderError> {
    let mut content: Option<String> = None;
    let mut reasoning: Option<String> = None;
    let mut tool_calls: Vec<ToolCall> = Vec::new();

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
                    if let Some(text) = block["thinking"].as_str() {
                        match &mut reasoning {
                            Some(r) => r.push_str(text),
                            None => reasoning = Some(text.to_string()),
                        }
                    }
                }
                Some("tool_use") => {
                    let id = block["id"].as_str().unwrap_or_default().to_string();
                    let name = block["name"].as_str().unwrap_or_default().to_string();
                    let arguments = block["input"].clone().to_string();
                    tool_calls.push(ToolCall::new(id, FunctionCall { name, arguments }));
                }
                _ => {}
            }
        }
    }

    // Check for thinking in the top-level thinking field (summary mode).
    if reasoning.is_none() {
        if let Some(thinking) = data["thinking"].as_array() {
            for block in thinking {
                if let Some(text) = block["text"].as_str() {
                    match &mut reasoning {
                        Some(r) => r.push_str(text),
                        None => reasoning = Some(text.to_string()),
                    }
                }
            }
        }
    }

    let u = &data["usage"];
    let usage = TokenUsage {
        prompt_tokens: u["input_tokens"].as_u64().map(|n| n as u32),
        completion_tokens: u["output_tokens"].as_u64().map(|n| n as u32),
        cache_read_tokens: u["cache_read_input_tokens"].as_u64().map(|n| n as u32),
        cache_write_tokens: parse_cache_write_tokens(u),
        reasoning_tokens: None,
    };

    Ok(ParsedResponse {
        content,
        reasoning,
        tool_calls,
        usage,
    })
}

pub(super) async fn read_stream(
    resp: reqwest::Response,
    cancel: &CancellationToken,
    on_delta: &(dyn Fn(StreamDelta) + Send + Sync),
) -> Result<ParsedResponse, ProviderError> {
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut tool_calls: HashMap<usize, (String, String, String)> = HashMap::new();
    let mut usage = TokenUsage::default();

    sse::read_events(resp, cancel, |ev| {
        let event_type = ev["type"].as_str().unwrap_or("");

        match event_type {
            "message_start" => {
                if let Some(u) = ev.get("message").and_then(|m| m.get("usage")) {
                    usage.prompt_tokens = u["input_tokens"].as_u64().map(|n| n as u32);
                    usage.cache_read_tokens =
                        u["cache_read_input_tokens"].as_u64().map(|n| n as u32);
                    usage.cache_write_tokens = parse_cache_write_tokens(u);
                }
            }
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
                    match delta["type"].as_str() {
                        Some("text_delta") => {
                            if let Some(text) = delta["text"].as_str() {
                                if !text.is_empty() {
                                    content.push_str(text);
                                    on_delta(StreamDelta::Text(text));
                                }
                            }
                        }
                        Some("thinking_delta") => {
                            if let Some(text) = delta["thinking"].as_str() {
                                if !text.is_empty() {
                                    reasoning.push_str(text);
                                    on_delta(StreamDelta::Thinking(text));
                                }
                            }
                        }
                        Some("input_json_delta") => {
                            if let Some(partial_json) = delta["partial_json"].as_str() {
                                if let Some(idx) = ev["index"].as_u64() {
                                    if let Some(entry) = tool_calls.get_mut(&(idx as usize)) {
                                        entry.2.push_str(partial_json);
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            "message_delta" => {
                if let Some(u) = ev.get("usage") {
                    usage.completion_tokens = u["output_tokens"].as_u64().map(|n| n as u32);
                    if usage.prompt_tokens.is_none() {
                        usage.prompt_tokens = u["input_tokens"].as_u64().map(|n| n as u32);
                    }
                }
            }
            _ => {}
        }
    })
    .await?;

    Ok(ParsedResponse {
        content: non_empty(content),
        reasoning: non_empty(reasoning),
        tool_calls: collect_indexed_tool_calls(tool_calls),
        usage,
    })
}
