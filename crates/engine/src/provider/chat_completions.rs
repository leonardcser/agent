use super::extract::extract_tool_calls_from_text;
use super::{collect_indexed_tool_calls, non_empty, sse};
use super::{ParsedResponse, ProviderError, StreamDelta, ToolDefinition};
use crate::cancel::CancellationToken;
use crate::config::ModelConfig;
use crate::tools::{trim_tool_output, MAX_TOOL_OUTPUT_LINES};
use protocol::{Message, ReasoningEffort, Role, TokenUsage, ToolCall};
use std::collections::HashMap;

pub(super) fn build_body(
    messages: &[Message],
    tools: &[ToolDefinition],
    model: &str,
    effort: ReasoningEffort,
    config: &ModelConfig,
) -> serde_json::Value {
    let api_messages: Vec<serde_json::Value> = messages
        .iter()
        .map(|m| {
            let mut v = serde_json::to_value(m).unwrap();
            if m.role == Role::Agent {
                super::fixup_agent_message(m, &mut v);
            }
            if let Some(obj) = v.as_object_mut() {
                obj.remove("is_error");
                if m.role == Role::Tool {
                    if let Some(s) = obj.get("content").and_then(|c| c.as_str()) {
                        let trimmed = trim_tool_output(s, MAX_TOOL_OUTPUT_LINES);
                        obj.insert("content".into(), serde_json::json!(trimmed));
                    }
                }
                super::sanitize_tool_call_arguments(obj);
            }
            v
        })
        .collect();

    let mut body = serde_json::json!({ "model": model, "messages": api_messages });

    if !tools.is_empty() {
        body["tools"] = serde_json::to_value(tools).unwrap();
    }
    if let Some(v) = config.temperature {
        body["temperature"] = serde_json::json!(v);
    }
    if let Some(v) = config.top_p {
        body["top_p"] = serde_json::json!(v);
    }
    if let Some(v) = config.top_k {
        body["top_k"] = serde_json::json!(v);
    }
    if let Some(v) = config.min_p {
        body["min_p"] = serde_json::json!(v);
    }
    if let Some(v) = config.repeat_penalty {
        body["repeat_penalty"] = serde_json::json!(v);
    }

    let label = effort.label();
    if effort != ReasoningEffort::Off {
        body["reasoning_effort"] = serde_json::json!(label);
        body["chat_template_kwargs"] = serde_json::json!({
            "enable_thinking": true,
            "reasoning_effort": label,
        });
    } else {
        body["chat_template_kwargs"] = serde_json::json!({"enable_thinking": false});
    }

    body
}

pub(super) fn parse_response(data: &serde_json::Value) -> Result<ParsedResponse, ProviderError> {
    let choice = data["choices"]
        .get(0)
        .ok_or_else(|| ProviderError::InvalidResponse("no choices in response".into()))?;
    let msg = &choice["message"];

    let mut content = msg["content"].as_str().map(|s| s.to_string());
    let mut reasoning = msg["reasoning_content"]
        .as_str()
        .or_else(|| msg["reasoning"].as_str())
        .map(|s| s.to_string());

    let mut tool_calls: Vec<ToolCall> = if let Some(tcs) = msg.get("tool_calls") {
        serde_json::from_value(tcs.clone()).unwrap_or_default()
    } else {
        vec![]
    };

    // Fallback: some backends (vLLM with reasoning+tool calling) may
    // place <tool_call> markup inside `content` or `reasoning_content`.
    if tool_calls.is_empty() {
        let (from_content, cleaned_content) = extract_tool_calls_from_text(content.as_deref());
        let (from_reasoning, cleaned_reasoning) =
            extract_tool_calls_from_text(reasoning.as_deref());
        if !from_content.is_empty() || !from_reasoning.is_empty() {
            tool_calls = from_content.into_iter().chain(from_reasoning).collect();
            content = cleaned_content;
            reasoning = cleaned_reasoning;
        }
    }

    let u = &data["usage"];
    let usage = TokenUsage {
        prompt_tokens: u["prompt_tokens"].as_u64().map(|n| n as u32),
        completion_tokens: u["completion_tokens"].as_u64().map(|n| n as u32),
        cache_read_tokens: u["prompt_tokens_details"]["cached_tokens"]
            .as_u64()
            .map(|n| n as u32),
        cache_write_tokens: None,
        reasoning_tokens: u["completion_tokens_details"]["reasoning_tokens"]
            .as_u64()
            .map(|n| n as u32),
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
        // Usage from the final chunk
        if let Some(u) = ev.get("usage") {
            usage.prompt_tokens = u["prompt_tokens"].as_u64().map(|n| n as u32);
            usage.completion_tokens = usage
                .completion_tokens
                .or(u["completion_tokens"].as_u64().map(|n| n as u32));
            usage.cache_read_tokens = u["prompt_tokens_details"]["cached_tokens"]
                .as_u64()
                .map(|n| n as u32);
            usage.reasoning_tokens = u["completion_tokens_details"]["reasoning_tokens"]
                .as_u64()
                .map(|n| n as u32);
        }

        let Some(delta) = ev["choices"].get(0).and_then(|c| c.get("delta")) else {
            return;
        };

        if let Some(text) = delta["content"].as_str() {
            if !text.is_empty() {
                content.push_str(text);
                on_delta(StreamDelta::Text(text));
            }
        }

        if let Some(text) = delta
            .get("reasoning_content")
            .or_else(|| delta.get("reasoning"))
            .and_then(|v| v.as_str())
        {
            if !text.is_empty() {
                reasoning.push_str(text);
                on_delta(StreamDelta::Thinking(text));
            }
        }

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
    })
    .await?;

    let content = non_empty(content);
    let reasoning = non_empty(reasoning);
    let tool_calls = collect_indexed_tool_calls(tool_calls);

    // Fallback: extract tool calls from text (vLLM etc.)
    if tool_calls.is_empty() {
        let (from_content, cleaned_content) = extract_tool_calls_from_text(content.as_deref());
        let (from_reasoning, cleaned_reasoning) =
            extract_tool_calls_from_text(reasoning.as_deref());
        if !from_content.is_empty() || !from_reasoning.is_empty() {
            let tool_calls: Vec<ToolCall> =
                from_content.into_iter().chain(from_reasoning).collect();
            return Ok(ParsedResponse {
                content: cleaned_content,
                reasoning: cleaned_reasoning,
                tool_calls,
                usage,
            });
        }
    }

    Ok(ParsedResponse {
        content,
        reasoning,
        tool_calls,
        usage,
    })
}
