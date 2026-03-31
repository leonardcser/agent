use super::non_empty;
use super::sse;
use super::{ParsedResponse, ProviderError, StreamDelta, ToolDefinition};
use crate::cancel::CancellationToken;
use crate::config::ModelConfig;
use crate::log;
use crate::tools::{trim_tool_output, MAX_TOOL_OUTPUT_LINES};
use protocol::{FunctionCall, Message, ReasoningEffort, Role, TokenUsage, ToolCall};
use std::collections::HashMap;

/// Parse OpenAI usage fields into a `TokenUsage`. OpenAI reports total input
/// tokens (including cached), so we subtract cached to match Anthropic semantics.
fn parse_usage(u: &serde_json::Value) -> TokenUsage {
    let total_input = u["input_tokens"].as_u64().map(|n| n as u32);
    let cached = u["input_tokens_details"]["cached_tokens"]
        .as_u64()
        .map(|n| n as u32);
    TokenUsage {
        prompt_tokens: match (total_input, cached) {
            (Some(t), Some(c)) => Some(t.saturating_sub(c)),
            (t, _) => t,
        },
        completion_tokens: u["output_tokens"].as_u64().map(|n| n as u32),
        cache_read_tokens: cached,
        cache_write_tokens: None,
        reasoning_tokens: u["output_tokens_details"]["reasoning_tokens"]
            .as_u64()
            .map(|n| n as u32),
    }
}

fn effort_label(effort: ReasoningEffort) -> String {
    if effort == ReasoningEffort::Max {
        "xhigh".to_string()
    } else {
        effort.label().to_string()
    }
}

pub(super) fn build_body(
    messages: &[Message],
    tools: &[ToolDefinition],
    model: &str,
    effort: ReasoningEffort,
    config: &ModelConfig,
) -> serde_json::Value {
    let mut instructions = String::new();
    let mut input = Vec::new();
    for m in messages {
        match m.role {
            Role::System => {
                let text = m.content.as_ref().map(|c| c.as_text()).unwrap_or_default();
                if !instructions.is_empty() {
                    instructions.push('\n');
                }
                instructions.push_str(text);
            }
            Role::User => {
                let content_val = match &m.content {
                    Some(protocol::Content::Text(t)) => serde_json::json!(t),
                    Some(protocol::Content::Parts(parts)) => {
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
                if let Some(content) = &m.content {
                    input.push(serde_json::json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": content.as_text()}],
                    }));
                }
                if let Some(tcs) = &m.tool_calls {
                    for tc in tcs {
                        let args = if serde_json::from_str::<serde_json::Value>(
                            &tc.function.arguments,
                        )
                        .is_ok()
                        {
                            &tc.function.arguments
                        } else {
                            "{}"
                        };
                        input.push(serde_json::json!({
                            "type": "function_call",
                            "call_id": tc.id,
                            "name": tc.function.name,
                            "arguments": args,
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
                let trimmed = trim_tool_output(output, MAX_TOOL_OUTPUT_LINES);
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

    let mut body =
        serde_json::json!({ "model": model, "instructions": instructions, "input": input });

    if !api_tools.is_empty() {
        body["tools"] = serde_json::json!(api_tools);
    }
    if let Some(v) = config.temperature {
        body["temperature"] = serde_json::json!(v);
    }
    if let Some(v) = config.top_p {
        body["top_p"] = serde_json::json!(v);
    }
    if effort != ReasoningEffort::Off {
        body["reasoning"] = serde_json::json!({
            "effort": effort_label(effort),
            "summary": "auto",
        });
    }

    body
}

pub(super) fn parse_response(data: &serde_json::Value) -> Result<ParsedResponse, ProviderError> {
    let output = data["output"]
        .as_array()
        .ok_or_else(|| ProviderError::InvalidResponse("no output in response".into()))?;

    let mut content: Option<String> = None;
    let mut reasoning: Option<String> = None;
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
                let mut texts: Vec<&str> = Vec::new();
                if let Some(summaries) = item["summary"].as_array() {
                    texts.extend(summaries.iter().filter_map(|s| s["text"].as_str()));
                }
                if texts.is_empty() {
                    if let Some(parts) = item["content"].as_array() {
                        texts.extend(parts.iter().filter_map(|p| p["text"].as_str()));
                    }
                }
                if !texts.is_empty() {
                    reasoning = Some(texts.join("\n"));
                }
            }
            _ => {}
        }
    }

    let usage = parse_usage(&data["usage"]);

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
    // Map from item_id to (call_id, name, args)
    let mut tool_calls: HashMap<String, (String, String, String)> = HashMap::new();
    let mut usage = TokenUsage::default();

    sse::read_events(resp, cancel, |ev| {
        let ev_type = ev["type"].as_str().unwrap_or("");

        match ev_type {
            "response.output_item.added" => {
                if ev["item"]["type"].as_str() == Some("function_call") {
                    let item = &ev["item"];
                    let id = item["id"].as_str().unwrap_or("").to_string();
                    let call_id = item["call_id"].as_str().unwrap_or("").to_string();
                    let name = item["name"].as_str().unwrap_or("").to_string();
                    if !id.is_empty() && !name.is_empty() {
                        tool_calls.insert(id, (call_id, name, String::new()));
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
                if let Some(item_id) = ev["item_id"].as_str() {
                    if let Some(entry) = tool_calls.get_mut(item_id) {
                        if let Some(args) = ev["delta"].as_str() {
                            entry.2.push_str(args);
                        }
                    }
                }
            }
            "response.function_call_arguments.done" => {
                if let Some(item_id) = ev["item_id"].as_str() {
                    if let Some(entry) = tool_calls.get_mut(item_id) {
                        entry.2 = ev["arguments"].as_str().unwrap_or("{}").to_string();
                    }
                }
            }
            "response.reasoning.delta" | "response.reasoning_summary_text.delta" => {
                if let Some(text) = ev["delta"].as_str() {
                    if !text.is_empty() {
                        reasoning.push_str(text);
                        on_delta(StreamDelta::Thinking(text));
                    }
                }
            }
            "response.completed" | "response.done" => {
                if let Some(u) = ev.get("response").and_then(|r| r.get("usage")) {
                    usage = parse_usage(u);
                }
            }
            _ => {}
        }
    })
    .await?;

    let tool_calls: Vec<ToolCall> = tool_calls
        .into_values()
        .filter(|(call_id, name, _)| !call_id.is_empty() && !name.is_empty())
        .map(|(call_id, name, args)| {
            ToolCall::new(
                call_id,
                FunctionCall {
                    name,
                    arguments: args,
                },
            )
        })
        .collect();

    Ok(ParsedResponse {
        content: non_empty(content),
        reasoning: non_empty(reasoning),
        tool_calls,
        usage,
    })
}
