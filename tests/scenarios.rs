//! Integration scenarios driving the `smelt` binary against a wiremock'd
//! provider. Each `#[tokio::test]` is one flow: prompt + canned LLM
//! response → assertions on the JSONL event stream.

mod common;

use common::harness::Harness;

/// Smoke: harness compiles, wiremock spins up, tempdir resolves.
/// Doesn't drive the binary.
#[tokio::test]
async fn smoke_harness_starts() {
    let h = Harness::new().await;
    assert!(h.mock.uri().starts_with("http://"));
    h.write_config("anthropic", "claude-test");
    h.write_init_lua("");
    let cfg = h.config_dir.path().join("smelt").join("init.lua");
    assert!(cfg.exists());
}

/// Shell escapes bypass the model and preserve the child command's status.
#[tokio::test]
async fn shell_escape_propagates_child_status() {
    let h = Harness::new().await;
    h.write_config("anthropic", "claude-test");
    h.write_init_lua("");

    let out = h.run("!exit 7", "test/claude-test");

    assert_eq!(out.status, 7, "stderr: {}", out.stderr);
    assert!(out.events.is_empty());
}

/// Plain turn: user types a prompt; provider returns a single text
/// content block; engine emits the streaming + completion events.
#[tokio::test]
async fn plain_turn() {
    let h = Harness::new().await;
    h.write_config("anthropic", "claude-test");
    h.write_init_lua("");
    h.mount_anthropic_sse(&[
        serde_json::json!({
            "type": "message_start",
            "message": {
                "id": "msg_test",
                "type": "message",
                "role": "assistant",
                "model": "claude-test",
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": { "input_tokens": 10, "output_tokens": 0 }
            }
        }),
        serde_json::json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": { "type": "text", "text": "" }
        }),
        serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "text_delta", "text": "hello" }
        }),
        serde_json::json!({
            "type": "content_block_stop",
            "index": 0
        }),
        serde_json::json!({
            "type": "message_delta",
            "delta": { "stop_reason": "end_turn", "stop_sequence": null },
            "usage": { "output_tokens": 1 }
        }),
        serde_json::json!({ "type": "message_stop" }),
    ])
    .await;

    let out = h.run("hi", "test/claude-test");
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    insta::assert_json_snapshot!(out.events, {
        "[].HistoryUpdated.update.items[].sent_at_ms" => "[sent_at_ms]",
        "[].TurnComplete.meta.elapsed_ms" => "[elapsed_ms]",
        "[].TurnComplete.meta.avg_tps" => "[avg_tps]",
        "[].TokenUsage.tokens_per_sec" => "[tps]",
    });
}

/// Kimi-style SSE: provider omits the space after `data:`.
/// The SSE parser must still correctly parse each event.
#[tokio::test]
async fn anthropic_compatible_sse_no_space() {
    let h = Harness::new().await;
    h.write_config("anthropic-compatible", "kimi-test");
    h.write_init_lua("");
    h.mount_anthropic_sse_no_space(&[
        serde_json::json!({
            "type": "message_start",
            "message": {
                "id": "msg_test",
                "type": "message",
                "role": "assistant",
                "model": "kimi-test",
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": { "input_tokens": 10, "output_tokens": 0 }
            }
        }),
        serde_json::json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": { "type": "text", "text": "" }
        }),
        serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "text_delta", "text": "hello" }
        }),
        serde_json::json!({
            "type": "content_block_stop",
            "index": 0
        }),
        serde_json::json!({
            "type": "message_delta",
            "delta": { "stop_reason": "end_turn", "stop_sequence": null },
            "usage": { "output_tokens": 1 }
        }),
        serde_json::json!({ "type": "message_stop" }),
    ])
    .await;

    let out = h.run("hi", "test/kimi-test");
    let has_text = out
        .events
        .iter()
        .any(|ev| ev.get("TextDelta").is_some() || ev.get("Text").is_some());
    assert!(
        has_text,
        "expected TextDelta or Text event, got: {:?}",
        out.events
    );
}

/// Provider streams the same response across two text deltas (split
/// mid-word). Engine must concatenate them into a single assistant
/// content string. Pins the SSE buffer-and-split logic.
#[tokio::test]
async fn streaming_concat_across_deltas() {
    let h = Harness::new().await;
    h.write_config("anthropic", "claude-test");
    h.write_init_lua("");
    h.mount_anthropic_sse(&[
        serde_json::json!({
            "type": "message_start",
            "message": {
                "id": "msg_test",
                "type": "message",
                "role": "assistant",
                "model": "claude-test",
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": { "input_tokens": 4, "output_tokens": 0 }
            }
        }),
        serde_json::json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": { "type": "text", "text": "" }
        }),
        serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "text_delta", "text": "hel" }
        }),
        serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "text_delta", "text": "lo wor" }
        }),
        serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "text_delta", "text": "ld" }
        }),
        serde_json::json!({
            "type": "content_block_stop",
            "index": 0
        }),
        serde_json::json!({
            "type": "message_delta",
            "delta": { "stop_reason": "end_turn", "stop_sequence": null },
            "usage": { "output_tokens": 2 }
        }),
        serde_json::json!({ "type": "message_stop" }),
    ])
    .await;

    let out = h.run("hi", "test/claude-test");
    insta::assert_json_snapshot!(out.events, {
        "[].HistoryUpdated.update.items[].sent_at_ms" => "[sent_at_ms]",
        "[].TurnComplete.meta.elapsed_ms" => "[elapsed_ms]",
        "[].TurnComplete.meta.avg_tps" => "[avg_tps]",
        "[].TokenUsage.tokens_per_sec" => "[tps]",
    });
}

/// Provider returns 401 Unauthorized. The engine emits `TurnError`, and the
/// headless process reports a dispatch failure.
#[tokio::test]
async fn provider_auth_error() {
    let h = Harness::new().await;
    h.write_config("anthropic", "claude-test");
    h.write_init_lua("");
    h.mount_http_error(
        401,
        serde_json::json!({
            "error": { "type": "authentication_error", "message": "invalid api key" }
        }),
    )
    .await;

    let out = h.run("hi", "test/claude-test");
    assert_eq!(out.status, 3, "stderr: {}", out.stderr);
    insta::assert_json_snapshot!(out.events, {
        "[].HistoryUpdated.update.items[].sent_at_ms" => "[sent_at_ms]",
        "[].TurnComplete.meta.elapsed_ms" => "[elapsed_ms]",
        "[].TurnComplete.meta.avg_tps" => "[avg_tps]",
        "[].TokenUsage.tokens_per_sec" => "[tps]",
    });
}

/// Incomplete stream: provider sends a `text_delta` then closes the
/// connection without `content_block_stop` / `message_delta` /
/// `message_stop`. Engine detects the premature EOF and emits a
/// `TurnError` instead of a silent `TurnComplete`.
#[tokio::test]
async fn incomplete_stream() {
    let h = Harness::new().await;
    h.write_config("anthropic", "claude-test");
    h.write_init_lua("");
    h.mount_anthropic_sse(&[
        serde_json::json!({
            "type": "message_start",
            "message": {
                "id": "msg_test",
                "type": "message",
                "role": "assistant",
                "model": "claude-test",
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": { "input_tokens": 4, "output_tokens": 0 }
            }
        }),
        serde_json::json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": { "type": "text", "text": "" }
        }),
        serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "text_delta", "text": "partial" }
        }),
    ])
    .await;

    let out = h.run("hi", "test/claude-test");
    assert_eq!(out.status, 3, "stderr: {}", out.stderr);
    insta::assert_json_snapshot!(out.events, {
        "[].HistoryUpdated.update.items[].sent_at_ms" => "[sent_at_ms]",
        "[].TurnComplete.meta.elapsed_ms" => "[elapsed_ms]",
        "[].TurnComplete.meta.avg_tps" => "[avg_tps]",
        "[].TokenUsage.tokens_per_sec" => "[tps]",
    });
}

/// Thinking + text: provider streams a `thinking_delta` then a
/// `text_delta`. Engine emits a raw reasoning-part lifecycle, then TextDelta,
/// then Messages with the assistant content (thinking is dropped from the
/// persisted message tail when reasoning effort is off).
#[tokio::test]
async fn thinking_then_text() {
    let h = Harness::new().await;
    h.write_config("anthropic", "claude-test");
    h.write_init_lua("");
    h.mount_anthropic_sse(&[
        serde_json::json!({
            "type": "message_start",
            "message": {
                "id": "msg_test",
                "type": "message",
                "role": "assistant",
                "model": "claude-test",
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": { "input_tokens": 5, "output_tokens": 0 }
            }
        }),
        serde_json::json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": { "type": "thinking", "thinking": "" }
        }),
        serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "thinking_delta", "thinking": "let me think" }
        }),
        serde_json::json!({
            "type": "content_block_stop",
            "index": 0
        }),
        serde_json::json!({
            "type": "content_block_start",
            "index": 1,
            "content_block": { "type": "text", "text": "" }
        }),
        serde_json::json!({
            "type": "content_block_delta",
            "index": 1,
            "delta": { "type": "text_delta", "text": "answer" }
        }),
        serde_json::json!({
            "type": "content_block_stop",
            "index": 1
        }),
        serde_json::json!({
            "type": "message_delta",
            "delta": { "stop_reason": "end_turn", "stop_sequence": null },
            "usage": { "output_tokens": 3 }
        }),
        serde_json::json!({ "type": "message_stop" }),
    ])
    .await;

    let out = h.run("solve it", "test/claude-test");
    insta::assert_json_snapshot!(out.events, {
        "[].HistoryUpdated.update.items[].sent_at_ms" => "[sent_at_ms]",
        "[].TurnComplete.meta.elapsed_ms" => "[elapsed_ms]",
        "[].TurnComplete.meta.avg_tps" => "[avg_tps]",
        "[].TokenUsage.tokens_per_sec" => "[tps]",
    });
}

/// Anthropic cache_control: a plain turn stamps `cache_control` on the
/// last block of the system prompt and on the last user message. Tools
/// would also get a marker but headless mode disables tools entirely.
#[tokio::test]
async fn anthropic_emits_cache_control_markers() {
    let h = Harness::new().await;
    h.write_config("anthropic", "claude-test");
    h.write_init_lua("");
    h.mount_anthropic_sse(&[
        serde_json::json!({
            "type": "message_start",
            "message": {
                "id": "msg_test",
                "type": "message",
                "role": "assistant",
                "model": "claude-test",
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": { "input_tokens": 10, "output_tokens": 0 }
            }
        }),
        serde_json::json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": { "type": "text", "text": "" }
        }),
        serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "text_delta", "text": "ok" }
        }),
        serde_json::json!({ "type": "content_block_stop", "index": 0 }),
        serde_json::json!({
            "type": "message_delta",
            "delta": { "stop_reason": "end_turn", "stop_sequence": null },
            "usage": { "output_tokens": 1 }
        }),
        serde_json::json!({ "type": "message_stop" }),
    ])
    .await;

    let _ = h.run("hello", "test/claude-test");
    let bodies = h.captured_request_bodies().await;
    let body = bodies.first().expect("captured at least one request body");

    let expected = serde_json::json!({"type": "ephemeral"});
    let system_blocks = body["system"]
        .as_array()
        .expect("system encoded as block array");
    let last_sys = system_blocks.last().expect("system has at least one block");
    assert_eq!(
        last_sys["cache_control"], expected,
        "system prompt's last block must carry cache_control"
    );

    let messages = body["messages"]
        .as_array()
        .expect("messages encoded as array");
    let last_user = messages
        .iter()
        .rev()
        .find(|m| m["role"] == "user")
        .expect("found at least one user message");
    let user_blocks = last_user["content"]
        .as_array()
        .expect("user content encoded as block array");
    let last_block = user_blocks.last().expect("user has at least one block");
    assert_eq!(
        last_block["cache_control"], expected,
        "last user message's last content block must carry cache_control"
    );
}

/// `prompt_cache_key` is gated to `openai` and `codex` provider kinds.
/// Generic `openai-compatible` endpoints (vllm, llama.cpp, etc.) often
/// don't recognize the field - sending it would be telemetry without
/// any caching benefit.
#[tokio::test]
async fn openai_compatible_omits_prompt_cache_key() {
    let h = Harness::new().await;
    h.write_config("openai-compatible", "gpt-test");
    h.write_init_lua("");
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};
    let mut body = String::new();
    for ev in [
        serde_json::json!({
            "id": "chatcmpl-1",
            "object": "chat.completion.chunk",
            "choices": [{
                "index": 0,
                "delta": { "role": "assistant", "content": "ok" }
            }]
        }),
        serde_json::json!({
            "id": "chatcmpl-1",
            "object": "chat.completion.chunk",
            "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 1, "total_tokens": 11 }
        }),
    ] {
        body.push_str("data: ");
        body.push_str(&serde_json::to_string(&ev).unwrap());
        body.push_str("\n\n");
    }
    body.push_str("data: [DONE]\n\n");
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&h.mock)
        .await;

    let _ = h.run("hi", "test/gpt-test");
    let bodies = h.captured_request_bodies().await;
    let body = bodies.first().expect("captured at least one request body");
    assert!(
        body.get("prompt_cache_key").is_none(),
        "smelt must not send prompt_cache_key to the provider"
    );
}
