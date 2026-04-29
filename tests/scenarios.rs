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
    let cfg = h.config_dir.path().join("smelt").join("config.yaml");
    assert!(cfg.exists());
}

/// Plain turn: user types a prompt; provider returns a single text
/// content block; engine emits TextDelta + TurnComplete.
///
/// TODO: mount Anthropic SSE stub on `/v1/messages` returning
///   - message_start
///   - content_block_start (type=text)
///   - content_block_delta (delta="hello")
///   - content_block_stop
///   - message_delta (stop_reason=end_turn)
///   - message_stop
/// Then snapshot the resulting JSONL events via insta.
#[tokio::test]
#[ignore = "TODO: mount Anthropic SSE cassette + snapshot events"]
async fn plain_turn() {
    let h = Harness::new().await;
    h.write_config("anthropic", "claude-test");
    h.write_init_lua("");

    let out = h.run("hi", "test/claude-test");
    insta::assert_json_snapshot!(out.events);
}
