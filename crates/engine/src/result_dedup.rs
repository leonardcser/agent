//! Append-only deduplication of tool invocation results within a conversation.
//! Walks committed `HistoryItem::Assistant` steps and replaces a freshly-
//! produced `ToolOutcome.content` with a short pointer when an identical
//! result already exists earlier in the conversation. Cache-safe: the
//! pointer lives on the *new* invocation, never on prior history bytes.

use protocol::{HistoryItem, ToolInvocation};

/// Minimum body length for dedup to fire.
const MIN_DEDUP_LEN: usize = 500;

/// Return the call id of the most recent prior tool invocation matching
/// `new_content` and `new_is_error`, or `None`.
pub(crate) fn duplicate_of<'a>(
    new_content: &str,
    new_is_error: bool,
    history: &'a [HistoryItem],
) -> Option<&'a str> {
    if new_content.len() < MIN_DEDUP_LEN {
        return None;
    }
    for item in history.iter().rev() {
        let HistoryItem::Assistant(turn) = item else {
            continue;
        };
        for inv in turn.invocations.iter().rev() {
            if inv.result.is_error != new_is_error {
                continue;
            }
            if inv.result.content == new_content {
                return Some(inv.call_id.as_str());
            }
        }
    }
    None
}

/// Replacement body for a deduplicated tool_result.
pub(crate) fn dedup_stub(prior_call_id: &str) -> String {
    format!(
        "Output identical to a prior tool_result (call {prior_call_id}). \
         Refer to that earlier result."
    )
}

/// Mutate every `ToolInvocation.result.content` whose body duplicates an
/// earlier entry, replacing it with a `dedup_stub`. Checks two sources:
/// earlier invocations in the *current* turn (e.g. parallel tool_calls
/// returning identical payloads), then committed `history`. The current
/// turn is checked first so the stub points at the nearest prior call.
pub(crate) fn apply_in_place(invocations: &mut [ToolInvocation], history: &[HistoryItem]) {
    for i in 0..invocations.len() {
        let (earlier, rest) = invocations.split_at_mut(i);
        let inv = &mut rest[0];
        if inv.result.content.len() < MIN_DEDUP_LEN {
            continue;
        }
        let same_turn = earlier.iter().rev().find(|p| {
            p.result.is_error == inv.result.is_error
                && p.result.content.len() >= MIN_DEDUP_LEN
                && p.result.content == inv.result.content
        });
        if let Some(prior) = same_turn {
            inv.result.content = dedup_stub(&prior.call_id);
            continue;
        }
        if let Some(prior_id) = duplicate_of(&inv.result.content, inv.result.is_error, history) {
            inv.result.content = dedup_stub(prior_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{AssistantStep, Content, HistoryItem, ToolInvocation, ToolOutcome};

    fn big(prefix: &str) -> String {
        format!("{prefix}{}", "x".repeat(MIN_DEDUP_LEN))
    }

    fn inv(call_id: &str, content: &str, is_error: bool) -> ToolInvocation {
        ToolInvocation {
            call_id: call_id.into(),
            name: "f".into(),
            arguments: "{}".into(),
            result: ToolOutcome::new(content.into(), is_error, None),
            elapsed_ms: None,
            called_at_ms: None,
        }
    }

    fn assistant_with(invocations: Vec<ToolInvocation>) -> HistoryItem {
        HistoryItem::Assistant(AssistantStep::with_invocations(
            None,
            None,
            Vec::new(),
            invocations,
        ))
    }

    #[test]
    fn short_output_is_not_deduped() {
        let history = vec![assistant_with(vec![inv("a", "ok", false)])];
        assert!(duplicate_of("ok", false, &history).is_none());
    }

    #[test]
    fn identical_long_output_is_deduped() {
        let body = big("same ");
        let history = vec![assistant_with(vec![inv("call_1", &body, false)])];
        assert_eq!(duplicate_of(&body, false, &history), Some("call_1"));
    }

    #[test]
    fn different_long_outputs_are_not_deduped() {
        let a = big("a ");
        let b = big("b ");
        let history = vec![assistant_with(vec![inv("call_1", &a, false)])];
        assert!(duplicate_of(&b, false, &history).is_none());
    }

    #[test]
    fn non_assistant_items_are_ignored() {
        let body = big("same ");
        let history = vec![HistoryItem::User {
            content: Content::text(body.clone()),
            display: None,
            command: false,
            sent_at_ms: None,
        }];
        assert!(duplicate_of(&body, false, &history).is_none());
    }

    #[test]
    fn multiple_matches_return_most_recent() {
        let body = big("same ");
        let history = vec![
            assistant_with(vec![inv("call_1", &body, false)]),
            assistant_with(vec![inv("call_2", &body, false)]),
        ];
        assert_eq!(duplicate_of(&body, false, &history), Some("call_2"));
    }

    #[test]
    fn error_result_does_not_match_success_result() {
        let body = big("same ");
        let history = vec![assistant_with(vec![inv("call_1", &body, false)])];
        assert!(duplicate_of(&body, true, &history).is_none());
    }

    #[test]
    fn apply_in_place_rewrites_duplicates_in_current_invocations() {
        let body = big("dup ");
        let history = vec![assistant_with(vec![inv("prior", &body, false)])];
        let mut current = vec![inv("fresh", &body, false), inv("uniq", "small", false)];
        apply_in_place(&mut current, &history);
        assert!(current[0].result.content.contains("prior"));
        assert_eq!(current[1].result.content, "small");
    }

    #[test]
    fn apply_in_place_dedups_within_same_turn() {
        // Two parallel tool_calls returning identical large bodies. The
        // second should stub to the first even though the body is not in
        // committed history yet.
        let body = big("same ");
        let mut current = vec![inv("first", &body, false), inv("second", &body, false)];
        apply_in_place(&mut current, &[]);
        assert_eq!(current[0].result.content, body, "first kept verbatim");
        assert!(
            current[1].result.content.contains("first"),
            "second should stub to first; got {:?}",
            current[1].result.content
        );
    }
}
