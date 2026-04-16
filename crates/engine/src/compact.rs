//! Conversation history compaction.
//!
//! Replaces older history with a model-generated handoff summary so the
//! conversation can keep growing without overflowing the context window.
//!
//! Resilience notes:
//! - The summarization call itself can overflow the model's context window
//!   when the thread is huge; the retry loop drops the oldest item and tries
//!   again rather than surfacing the failure.
//! - A stable marker on summary messages prevents repeat compactions from
//!   feeding a prior summary back in as if it were user input.

use crate::cancel::CancellationToken;
use crate::log;
use crate::provider::{ChatOptions, Provider, ProviderError, TokenUsage};
use protocol::{Content, Message, ReasoningEffort, Role};

/// Handoff instructions handed to the summarizing model.
pub const SUMMARIZATION_PROMPT: &str = include_str!("prompts/compact.md");

/// Lead-in text the _next_ model sees at the top of the handoff summary.
/// Used both as a marker to detect "already summarized" messages on repeat
/// compaction and as framing so the next model treats the summary as
/// reference material rather than a user instruction.
pub const SUMMARY_PREFIX: &str = include_str!("prompts/compact_summary_prefix.md");

/// Soft cap on user-message text preserved verbatim after compaction, so the
/// replacement history leaves room for the next turn in the context window.
pub const COMPACT_USER_MESSAGE_MAX_TOKENS: usize = 20_000;

/// Per-message cap when flattening history for the summarizer: stops a single
/// oversized tool output from eating the compact prompt's budget.
const MAX_STRINGIFIED_MESSAGE_BYTES: usize = 8_000;

/// How many times `run_compact` will drop the oldest history message and
/// retry when the summarization call itself hits the model's context window.
const MAX_CONTEXT_TRIMS: usize = 20;

/// How many times `run_compact` will retry when the model returns an empty
/// summary.
const MAX_EMPTY_RETRIES: u8 = 2;

/// Controls how much context the replacement history preserves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitialContextInjection {
    /// Drop everything except the summary. Fits a user-initiated `/compact`
    /// where a fresh user message is expected to arrive next.
    DoNotInject,
    /// Carry recent user-authored messages forward so a mid-turn compaction
    /// leaves the in-flight request visible to the model.
    BeforeLastUserMessage,
}

/// Why compaction is running. Surfaced to logs so flakiness is diagnosable.
#[derive(Debug, Clone, Copy)]
pub enum CompactReason {
    /// The prompt crossed the configured context-window percentage.
    ContextLimit,
    /// The user invoked `/compact`.
    UserRequested,
}

impl CompactReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::ContextLimit => "context_limit",
            Self::UserRequested => "user_requested",
        }
    }
}

/// Which point in the turn lifecycle triggered compaction.
#[derive(Debug, Clone, Copy)]
pub enum CompactPhase {
    MidTurn,
    Manual,
}

impl CompactPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::MidTurn => "mid_turn",
            Self::Manual => "manual",
        }
    }
}

/// Caller-declared metadata for a compaction pass: what to keep, why, and
/// when. Bundled into a single options struct so `run_compact` doesn't grow
/// an unwieldy positional argument list.
#[derive(Debug, Clone, Copy)]
pub struct CompactOptions {
    pub injection: InitialContextInjection,
    pub phase: CompactPhase,
    pub reason: CompactReason,
}

/// Run a compaction pass against `history` (which must NOT contain the
/// system prompt — the caller owns that) and return the replacement
/// history plus token usage.
///
/// Resilience:
/// - Retries with exponential backoff are already handled inside
///   `Provider::chat`, so network/server/rate-limit failures are covered.
/// - If the summarization call itself overflows the model's context window,
///   drops the oldest history entry and retries up to
///   [`MAX_CONTEXT_TRIMS`] times.
/// - If the model returns an empty summary, retries up to
///   [`MAX_EMPTY_RETRIES`] times before giving up.
pub async fn run_compact(
    provider: &Provider,
    history: &[Message],
    model: &str,
    instructions: Option<&str>,
    cancel: &CancellationToken,
    options: CompactOptions,
) -> Result<(Vec<Message>, TokenUsage), ProviderError> {
    if history.is_empty() {
        return Err(ProviderError::InvalidResponse(
            "not enough history to compact".into(),
        ));
    }

    let CompactOptions {
        injection,
        phase,
        reason,
    } = options;

    log::entry(
        log::Level::Info,
        "compaction_started",
        &serde_json::json!({
            "phase": phase.as_str(),
            "reason": reason.as_str(),
            "history_len": history.len(),
            "injection": match injection {
                InitialContextInjection::DoNotInject => "do_not_inject",
                InitialContextInjection::BeforeLastUserMessage => "before_last_user_message",
            },
        }),
    );

    let mut window_start = 0usize;
    let mut empty_retries = 0u8;
    let mut context_trims = 0usize;

    let (summary_text, usage) = loop {
        if cancel.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }

        let request_messages = build_summarize_request(
            &history[window_start..],
            instructions,
            SUMMARIZATION_PROMPT,
        );

        let opts = ChatOptions::new(cancel);
        match provider
            .chat(&request_messages, &[], model, ReasoningEffort::Off, &opts)
            .await
        {
            Ok(resp) => {
                let text = resp.content.unwrap_or_default().trim().to_string();
                if text.is_empty() {
                    if empty_retries < MAX_EMPTY_RETRIES {
                        empty_retries += 1;
                        log::entry(
                            log::Level::Warn,
                            "compaction_empty_retry",
                            &serde_json::json!({ "attempt": empty_retries }),
                        );
                        continue;
                    }
                    return Err(ProviderError::InvalidResponse(
                        "compaction returned empty summary after retries".into(),
                    ));
                }
                break (text, resp.usage);
            }
            Err(e) if is_context_window_error(&e) => {
                if window_start + 1 < history.len() && context_trims < MAX_CONTEXT_TRIMS {
                    window_start += 1;
                    context_trims += 1;
                    log::entry(
                        log::Level::Warn,
                        "compaction_trim_oldest",
                        &serde_json::json!({
                            "trimmed": context_trims,
                            "remaining_items": history.len() - window_start,
                            "error": e.to_string(),
                        }),
                    );
                    continue;
                }
                log::entry(
                    log::Level::Warn,
                    "compaction_error",
                    &serde_json::json!({
                        "stage": "context_window_exhausted",
                        "error": e.to_string(),
                    }),
                );
                return Err(e);
            }
            Err(e) => {
                log::entry(
                    log::Level::Warn,
                    "compaction_error",
                    &serde_json::json!({
                        "stage": "chat",
                        "error": e.to_string(),
                    }),
                );
                return Err(e);
            }
        }
    };

    let user_messages = collect_user_messages(history);
    let replacement = build_compacted_history(user_messages, &summary_text, injection);

    log::entry(
        log::Level::Info,
        "compaction_complete",
        &serde_json::json!({
            "phase": phase.as_str(),
            "reason": reason.as_str(),
            "context_trims": context_trims,
            "empty_retries": empty_retries,
            "user_messages_kept": replacement.len().saturating_sub(1),
            "prompt_tokens": usage.prompt_tokens,
            "completion_tokens": usage.completion_tokens,
        }),
    );

    Ok((replacement, usage))
}

/// Build the messages that get sent to the summarizer.
fn build_summarize_request(
    history: &[Message],
    instructions: Option<&str>,
    prompt: &str,
) -> Vec<Message> {
    let conversation = stringify_conversation(history);

    let mut system_text = prompt.trim().to_string();
    if let Some(extra) = instructions {
        let extra = extra.trim();
        if !extra.is_empty() {
            system_text.push_str(
                "\n\nThe user has asked you to pay special attention to the following \
                 when summarizing:\n",
            );
            system_text.push_str(extra);
        }
    }

    vec![
        Message::system(system_text),
        Message::user(Content::text(format!(
            "Conversation to summarize:\n\n{conversation}"
        ))),
    ]
}

/// Flatten history into a role-tagged transcript for the summarizer.
/// Each message is capped at [`MAX_STRINGIFIED_MESSAGE_BYTES`].
fn stringify_conversation(messages: &[Message]) -> String {
    let mut out = String::new();
    for m in messages {
        let (role_label, text): (&str, String) = match m.role {
            Role::System => ("System", message_text(m)),
            Role::User => ("User", message_text(m)),
            Role::Assistant => ("Assistant", assistant_text(m)),
            Role::Tool => ("ToolResult", message_text(m)),
            Role::Agent => ("Agent", m.agent_api_text()),
        };

        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let truncated = truncate_bytes_floor(text, MAX_STRINGIFIED_MESSAGE_BYTES);
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(role_label);
        out.push_str(": ");
        out.push_str(&truncated);
    }
    out
}

fn message_text(m: &Message) -> String {
    m.content
        .as_ref()
        .map(|c| c.as_text().to_string())
        .unwrap_or_default()
}

fn assistant_text(m: &Message) -> String {
    let mut text = String::new();
    if let Some(r) = m.reasoning_content.as_deref() {
        let r = r.trim();
        if !r.is_empty() {
            text.push_str("[thinking]\n");
            text.push_str(r);
            text.push_str("\n\n");
        }
    }
    if let Some(c) = m.content.as_ref() {
        text.push_str(c.as_text());
    }
    if let Some(calls) = m.tool_calls.as_ref() {
        for call in calls {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str("[tool_call] ");
            text.push_str(&call.function.name);
            text.push('(');
            text.push_str(&call.function.arguments);
            text.push(')');
        }
    }
    text
}

/// Truncate to at most `max_bytes`, snapping down to the nearest char
/// boundary so we never split a multi-byte sequence.
fn truncate_bytes_floor(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let end = text.floor_char_boundary(max_bytes);
    let mut out = String::with_capacity(end + 32);
    out.push_str(&text[..end]);
    out.push_str("\n…[truncated for compaction]");
    out
}

/// Collect user and inter-agent messages as plain text, skipping anything
/// that was itself produced by a prior compaction.
pub fn collect_user_messages(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|m| {
            let text = match m.role {
                Role::User => m.content.as_ref()?.as_text().to_string(),
                Role::Agent => m.agent_api_text(),
                _ => return None,
            };
            let trimmed = text.trim();
            if trimmed.is_empty() || is_summary_text(trimmed) {
                None
            } else {
                Some(text)
            }
        })
        .collect()
}

/// True if `message` is a handoff summary produced by a prior compaction.
/// Used on re-compaction so a prior summary doesn't get re-ingested as user
/// input and nested under a new summary.
pub fn is_summary_message(message: &Message) -> bool {
    if !matches!(message.role, Role::User) {
        return false;
    }
    message
        .content
        .as_ref()
        .map(|c| is_summary_text(c.as_text().trim()))
        .unwrap_or(false)
}

fn is_summary_text(text: &str) -> bool {
    text.starts_with(SUMMARY_PREFIX.trim_end())
}

/// Assemble the replacement history: recent user-authored messages
/// (token-budgeted, most recent kept) followed by the handoff summary as the
/// final user message. Caller prepends the system prompt.
pub fn build_compacted_history(
    user_messages: Vec<String>,
    summary_text: &str,
    injection: InitialContextInjection,
) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::new();

    if matches!(injection, InitialContextInjection::BeforeLastUserMessage) {
        let selected = select_recent_user_messages(user_messages, COMPACT_USER_MESSAGE_MAX_TOKENS);
        for text in selected {
            out.push(Message::user(Content::text(text)));
        }
    }

    let summary_body = if summary_text.trim().is_empty() {
        "(no summary available)".to_string()
    } else {
        summary_text.trim().to_string()
    };
    let prefixed = format!("{}\n{}", SUMMARY_PREFIX.trim_end(), summary_body);
    out.push(Message::user(Content::text(prefixed)));

    out
}

/// Select the most recent user messages (in chronological order) that fit
/// within `max_tokens` total, truncating the oldest-kept message if needed.
fn select_recent_user_messages(user_messages: Vec<String>, max_tokens: usize) -> Vec<String> {
    if max_tokens == 0 || user_messages.is_empty() {
        return Vec::new();
    }
    let mut remaining = max_tokens;
    let mut selected: Vec<String> = Vec::new();
    for message in user_messages.into_iter().rev() {
        if remaining == 0 {
            break;
        }
        let tokens = approx_token_count(&message);
        if tokens <= remaining {
            selected.push(message);
            remaining = remaining.saturating_sub(tokens);
        } else {
            let max_bytes = remaining.saturating_mul(4);
            selected.push(truncate_bytes_floor(&message, max_bytes));
            break;
        }
    }
    selected.reverse();
    selected
}

/// Very rough token estimator: ~4 bytes per token. Only used to budget user-
/// message carryover, so exactness against a real tokenizer isn't required.
pub fn approx_token_count(text: &str) -> usize {
    text.len().div_ceil(4)
}

/// Detect context-window-exceeded via error text. The provider layer funnels
/// `context_length_exceeded` and several 400 bodies into `InvalidResponse`,
/// so the retry loop has to pattern-match the body. Substring list is kept
/// narrow so an unrelated 400 (e.g. schema error) doesn't trip retries.
fn is_context_window_error(e: &ProviderError) -> bool {
    let body = match e {
        ProviderError::InvalidResponse(b) => b.as_str(),
        ProviderError::Server { body, .. } => body.as_str(),
        _ => return false,
    };
    let lower = body.to_ascii_lowercase();
    lower.contains("context_length_exceeded")
        || lower.contains("context length")
        || lower.contains("context window")
        || lower.contains("maximum context")
        || lower.contains("prompt is too long")
        || lower.contains("prompt too long")
        || lower.contains("too many tokens")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_recent_respects_budget() {
        let msgs = vec![
            "a".repeat(4000), // ~1000 tokens
            "b".repeat(4000),
            "c".repeat(4000),
        ];
        let kept = select_recent_user_messages(msgs, 2000);
        assert_eq!(kept.len(), 2, "should keep the two most recent");
        assert!(kept[0].starts_with('b'));
        assert!(kept[1].starts_with('c'));
    }

    #[test]
    fn select_recent_truncates_oldest_kept() {
        let original = vec!["a".repeat(12_000), "b".repeat(400)];
        let kept = select_recent_user_messages(original.clone(), 500);
        assert_eq!(kept.len(), 2);
        assert!(
            kept[0].len() < original[0].len(),
            "oldest kept got truncated"
        );
        assert!(kept[0].contains("truncated"));
    }

    #[test]
    fn summary_roundtrip_detected() {
        let replacement = build_compacted_history(
            vec!["earlier user ask".to_string()],
            "work in progress",
            InitialContextInjection::BeforeLastUserMessage,
        );
        let summary_msg = replacement.last().unwrap();
        assert!(is_summary_message(summary_msg));
        let kept_user = &replacement[0];
        assert!(!is_summary_message(kept_user));
    }

    #[test]
    fn do_not_inject_drops_user_messages() {
        let replacement = build_compacted_history(
            vec!["a".into(), "b".into()],
            "summary",
            InitialContextInjection::DoNotInject,
        );
        assert_eq!(replacement.len(), 1);
        assert!(is_summary_message(&replacement[0]));
    }

    #[test]
    fn summary_detection_ignores_non_user_roles() {
        let as_user = Message::user(Content::text(format!(
            "{}\nbody",
            SUMMARY_PREFIX.trim_end()
        )));
        let as_assistant =
            Message::assistant(Some(as_user.content.as_ref().unwrap().clone()), None, None);
        assert!(is_summary_message(&as_user));
        assert!(!is_summary_message(&as_assistant));
    }

    #[test]
    fn collect_skips_summary_messages() {
        let history = vec![
            Message::user(Content::text("hello")),
            Message::user(Content::text(format!(
                "{}\nprior summary",
                SUMMARY_PREFIX.trim_end()
            ))),
            Message::user(Content::text("hi again")),
        ];
        let collected = collect_user_messages(&history);
        assert_eq!(collected, vec!["hello".to_string(), "hi again".to_string()]);
    }

    #[test]
    fn context_error_detection() {
        assert!(is_context_window_error(&ProviderError::InvalidResponse(
            "context_length_exceeded: prompt too long".into()
        )));
        assert!(is_context_window_error(&ProviderError::InvalidResponse(
            "The prompt is too long for the model".into()
        )));
        assert!(!is_context_window_error(&ProviderError::InvalidResponse(
            "invalid json schema".into()
        )));
        assert!(!is_context_window_error(&ProviderError::Cancelled));
    }
}
