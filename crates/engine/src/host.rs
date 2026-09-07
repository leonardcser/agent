//! Typed callbacks from the engine task to the frontend host. Request/reply
//! variants carry their own `oneshot::Sender<Reply>` so correlation is the
//! channel handle instead of an integer id and lookup table. Host callbacks
//! share the ordered [`crate::EngineOutput`] queue with protocol events.
//!
//! Why not put these on [`protocol::EngineEvent`] / [`protocol::UiCommand`]?
//! Those types are `Serialize` for JSON output and persistence; an embedded
//! `oneshot::Sender` is neither serializable nor meaningful across process
//! boundaries. Keeping host RPCs outside the protocol surface also avoids a
//! pending map and paired `*Response` variants on `UiCommand`.

use protocol::Message;
use std::sync::Arc;
use tokio::sync::oneshot;

#[derive(Debug)]
pub enum HostRequestDecision {
    Continue,
    /// End the turn without sending another request or acknowledging pending
    /// steering inputs. The host retains those inputs for a subsequent turn.
    Stop,
    Replace {
        messages: Vec<Message>,
        coordinates: protocol::ModelHistoryCoordinates,
    },
    Abort(String),
}

impl HostRequestDecision {
    pub fn replace_canonical_history(messages: Vec<Message>) -> Self {
        Self::Replace {
            messages,
            coordinates: protocol::ModelHistoryCoordinates::canonical(),
        }
    }

    pub fn replace_model_history(
        messages: Vec<Message>,
        coordinates: protocol::ModelHistoryCoordinates,
    ) -> Self {
        Self::Replace {
            messages,
            coordinates,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PreparedRequestMessages {
    messages: Arc<Vec<Message>>,
    model_start: usize,
}

impl PreparedRequestMessages {
    pub fn new(messages: Vec<Message>, model_start: usize) -> Self {
        let model_start = model_start.min(messages.len());
        Self {
            messages: Arc::new(messages),
            model_start,
        }
    }

    pub fn model_only(messages: Vec<Message>) -> Self {
        Self::new(messages, 0)
    }

    pub fn wire(&self) -> &[Message] {
        self.messages.as_slice()
    }

    pub fn model(&self) -> &[Message] {
        &self.messages[self.model_start..]
    }
}

/// One callback from the engine to the frontend host. Request/reply variants
/// carry their originating turn, which the host validates before running hooks.
/// They fall back to their default when the host drops `reply` without sending.
pub enum HostCall {
    /// Run `smelt.provider.middleware{on_response=...}` hooks against
    /// the assembled assistant message. `Some(msg)` replaces it before
    /// it's pushed to history; `None` keeps the original.
    ProviderResponse {
        turn_id: u64,
        message: Message,
        reply: oneshot::Sender<Option<Message>>,
    },

    /// Engine hit a context-window error mid-turn. The host's registered
    /// recovery hook (`smelt.engine.on_context_limit`) is invoked with
    /// the conversation up to that point and returns a shorter
    /// conversation to retry with. `Replace { .. }` swaps the engine's
    /// `messages` (excluding the system prompt at index 0) and re-runs
    /// the loop; `Continue` (no hook registered, hook returned nil, or hook
    /// failed) aborts the turn with the existing `TurnError`; `Abort(message)`
    /// aborts with a host-provided terminal error.
    RecoverFromContextLimit {
        turn_id: u64,
        messages: Vec<Message>,
        reply: oneshot::Sender<HostRequestDecision>,
    },

    /// Append a provider request audit row through the host's fixed-session
    /// persistence actor after the required document generation is durable.
    RequestAudit {
        persistence: protocol::PersistenceScope,
        entry: Box<protocol::request_log::RequestLogEntry>,
        payload_mode: smelt_store::RequestAuditPayloadMode,
    },

    /// Engine is about to send a model request. The host may replace
    /// the conversation before the request is sent. `messages.model()`
    /// excludes the system prompt, while `messages.wire()` is shared with
    /// the provider call when the hook leaves history unchanged.
    PrepareRequest {
        turn_id: u64,
        messages: PreparedRequestMessages,
        estimated_tokens: u32,
        reply: oneshot::Sender<HostRequestDecision>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepared_request_exposes_wire_and_model_views() {
        let wire = vec![
            Message::system("system"),
            Message::user(protocol::Content::text("hello")),
        ];
        let messages = PreparedRequestMessages::new(wire.clone(), 1);

        assert_eq!(messages.wire(), wire.as_slice());
        assert_eq!(messages.model(), &wire[1..]);
    }

    #[test]
    fn prepared_request_clamps_model_start() {
        let wire = vec![Message::system("system")];
        let messages = PreparedRequestMessages::new(wire.clone(), usize::MAX);

        assert_eq!(messages.wire(), wire.as_slice());
        assert!(messages.model().is_empty());
    }
}
