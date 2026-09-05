//! Provider chat integrations for smelt.
//!
//! This crate owns API request/response shaping, streaming parsers, provider
//! error classification, and provider-specific protocol helpers. It does not
//! own smelt credential persistence, transcript state, UI policy, or tool
//! execution.

mod anthropic;
mod anthropic_models;
mod cache;
pub mod catalog;
mod chat_completions;
mod client;
pub mod codex;
mod config;
pub mod copilot;
mod endpoint;
mod error;
mod event;
mod extract;
mod format;
pub mod kimi_code;
mod kind;
mod openai;
mod pricing;
mod response;
mod sse;
mod tool;

pub use anthropic_models::{parse_claude_model_version, ClaudeModelFamily, ClaudeModelVersion};
pub use cache::{clamp_prompt_cache_key, sort_tools_for_cache_stability, CacheConfig};
#[cfg(feature = "test-support")]
pub use client::{context_window_from_models_entry, emit_retry, models_entry_matches};
pub use client::{
    ChatOptions, ChatProvider, ChatRequest, ChatRequestOptions, CopilotInitiator, ProviderClient,
    RequestAttemptInfo,
};
pub use config::{effective_reasoning_effort, reasoning_catalog};
#[cfg(not(feature = "test-support"))]
pub(crate) use endpoint::endpoint_url;
#[cfg(feature = "test-support")]
pub use endpoint::endpoint_url;
pub use endpoint::{api_base_normalization_hint, normalize_api_base, ApiBaseNormalizationHint};
#[cfg(feature = "test-support")]
pub use error::{
    backoff_delay, format_epoch_local, format_rate_limit, json_as_u64, parse_resets_at,
    parse_retry_after, parse_retry_from_body, rate_limit_error, retry_delay_for, unix_secs,
};
#[cfg(not(feature = "test-support"))]
pub(crate) use error::{parse_retry_after, retry_delay_for};
pub use error::{quota_exceeded_message, unix_now, ProviderError};
pub use event::{ProviderStreamEvent, ReasoningStreamEvent, ToolCallStreamEvent};
#[cfg(not(feature = "test-support"))]
pub(crate) use format::apply_response_format;
pub use format::ResponseFormat;
#[cfg(feature = "test-support")]
pub use format::{anthropic_supports_structured_output, apply_response_format};
#[cfg(not(feature = "test-support"))]
pub(crate) use kind::api_key_auth;
#[cfg(feature = "test-support")]
pub use kind::api_key_auth;
pub use kind::{
    is_kimi_code_api_base, ApiKeyAuth, AuthKind, ProviderDescriptor, ProviderKind, WireApi,
};
#[cfg(feature = "test-support")]
pub use openai::parse_stream_events as parse_openai_stream_events;
pub use pricing::{resolve as resolve_pricing, ModelPricing, PricingSource, ResolvedPricing};
pub(crate) use protocol::ModelConfig;
#[cfg(not(feature = "test-support"))]
pub(crate) use response::{
    collect_indexed_tool_calls, non_empty, non_empty_blocks, sanitize_tool_call_arguments,
};
#[cfg(feature = "test-support")]
pub use response::{
    collect_indexed_tool_calls, non_empty, non_empty_blocks, sanitize_tool_call_arguments,
};
pub use response::{ChatResponse, ChatResponseMetadata, CompletedReasoningPart, ParsedResponse};
pub use tokio_util::sync::CancellationToken;
pub use tool::{FunctionSchema, ToolDefinition};

#[cfg(any(test, feature = "fuzz"))]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FuzzProviderSummary {
    pub ok: bool,
    pub content_len: usize,
    pub reasoning_len: usize,
    pub reasoning_blocks: usize,
    pub tool_calls: usize,
    pub text_deltas: usize,
    pub thinking_deltas: usize,
    pub tool_arg_deltas: usize,
    pub usage_fields: usize,
    pub error: Option<String>,
}

#[cfg(any(test, feature = "fuzz"))]
fn fuzz_summary(
    result: Result<ParsedResponse, ProviderError>,
    deltas: FuzzProviderSummary,
) -> FuzzProviderSummary {
    match result {
        Ok(parsed) => FuzzProviderSummary {
            ok: true,
            content_len: parsed.content.as_deref().map(str::len).unwrap_or(0),
            reasoning_len: parsed.reasoning.as_deref().map(str::len).unwrap_or(0),
            reasoning_blocks: parsed.reasoning_blocks.as_ref().map(Vec::len).unwrap_or(0),
            tool_calls: parsed.tool_calls.len(),
            usage_fields: usize::from(parsed.usage.context_tokens.is_some())
                + usize::from(parsed.usage.prompt_tokens.is_some())
                + usize::from(parsed.usage.completion_tokens.is_some())
                + usize::from(parsed.usage.cache_read_tokens.is_some())
                + usize::from(parsed.usage.cache_write_tokens.is_some())
                + usize::from(parsed.usage.reasoning_tokens.is_some()),
            ..deltas
        },
        Err(err) => FuzzProviderSummary {
            ok: false,
            error: Some(err.to_string()),
            ..deltas
        },
    }
}

#[cfg(any(test, feature = "fuzz"))]
fn count_delta(summary: &mut FuzzProviderSummary, event: ProviderStreamEvent<'_>) {
    match event {
        ProviderStreamEvent::TextDelta(_) => summary.text_deltas += 1,
        ProviderStreamEvent::Reasoning(event) => {
            if matches!(event, ReasoningStreamEvent::Delta { .. }) {
                summary.thinking_deltas += 1;
            }
        }
        ProviderStreamEvent::ToolCall(ToolCallStreamEvent::ArgsDelta { .. }) => {
            summary.tool_arg_deltas += 1
        }
        ProviderStreamEvent::ToolCall(
            ToolCallStreamEvent::Started { .. } | ToolCallStreamEvent::Finished { .. },
        ) => {}
    }
}

#[cfg(any(test, feature = "fuzz"))]
pub fn fuzz_drain_sse_bytes(buf: &mut Vec<u8>) -> Vec<serde_json::Value> {
    sse::drain_sse_bytes(buf)
}

#[cfg(any(test, feature = "fuzz"))]
pub fn fuzz_parse_provider_response(wire: u8, data: &serde_json::Value) -> FuzzProviderSummary {
    let result = match wire % 3 {
        0 => chat_completions::parse_response(data),
        1 => openai::parse_response(data),
        _ => anthropic::parse_response(data),
    };
    fuzz_summary(result, FuzzProviderSummary::default())
}

#[cfg(any(test, feature = "fuzz"))]
pub fn fuzz_parse_provider_stream(wire: u8, events: &[serde_json::Value]) -> FuzzProviderSummary {
    let mut deltas = FuzzProviderSummary::default();
    let result = match wire % 3 {
        0 => chat_completions::parse_stream_events(events, &mut |d| count_delta(&mut deltas, d)),
        1 => openai::parse_stream_events(events, &mut |d| count_delta(&mut deltas, d)),
        _ => anthropic::parse_stream_events(events, &mut |d| count_delta(&mut deltas, d)),
    };
    fuzz_summary(result, deltas)
}

#[cfg(any(test, feature = "fuzz"))]
pub fn fuzz_build_anthropic_body(
    messages: &[protocol::Message],
    tools: &[ToolDefinition],
    model: &str,
    effort: protocol::ReasoningEffort,
    config: &ModelConfig,
    cache: &CacheConfig,
) -> serde_json::Value {
    anthropic::build_body(messages, tools, model, effort, config, cache)
}

#[cfg(any(test, feature = "fuzz"))]
pub fn fuzz_build_chat_completions_body(
    messages: &[protocol::Message],
    tools: &[ToolDefinition],
    model: &str,
    effort: protocol::ReasoningEffort,
    config: &ModelConfig,
) -> serde_json::Value {
    chat_completions::build_body(messages, tools, model, effort, config)
}

#[cfg(any(test, feature = "fuzz"))]
pub fn fuzz_build_openai_body(
    messages: &[protocol::Message],
    tools: &[ToolDefinition],
    model: &str,
    effort: protocol::ReasoningEffort,
    config: &ModelConfig,
    cache: &CacheConfig,
) -> serde_json::Value {
    let mut body = openai::build_body(messages, tools, model, effort, config);
    if let Some(ref key) = cache.prompt_cache_key {
        body["prompt_cache_key"] = serde_json::json!(clamp_prompt_cache_key(key));
    }
    body
}

#[cfg(any(test, feature = "fuzz"))]
pub fn fuzz_parse_catalog(json: &str) -> Option<usize> {
    catalog::fuzz_parse_len(json)
}

#[cfg(any(test, feature = "fuzz"))]
pub fn fuzz_extract_tool_calls(text: &str) -> (usize, Option<String>) {
    let (calls, cleaned) = extract::extract_tool_calls_from_text(Some(text));
    (calls.len(), cleaned)
}

#[cfg(any(test, feature = "fuzz"))]
pub fn fuzz_api_key_auth(kind: ProviderKind, api_key: &str) -> Option<ApiKeyAuth> {
    kind::api_key_auth(kind, api_key)
}
