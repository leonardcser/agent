use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ── Engine → UI ─────────────────────────────────────────────────────────────

/// Events emitted by the engine. The UI consumes these to update its display.
///
/// Most variants are fire-and-forget. The exceptions are `RequestPermission`
/// and `RequestAnswer`, which carry a `request_id` that the UI must eventually
/// reply to via `UiCommand`.
///
/// Event ordering within a turn:
///   Ready → (Thinking* → Text* → ToolStarted → ToolOutput* → ToolFinished)*
///         → TurnComplete | TurnError
///
/// ProcessCompleted can arrive at any time (including between turns).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EngineEvent {
    /// Engine has initialized and is ready to accept commands.
    Ready,

    /// Extended thinking / chain-of-thought text.
    Thinking { content: String },

    /// Streamed assistant text (may arrive in chunks).
    Text { content: String },

    /// A queued user message was consumed by the engine.
    Steered { text: String, count: usize },

    /// A tool call has started.
    ToolStarted {
        call_id: String,
        tool_name: String,
        args: HashMap<String, serde_json::Value>,
        summary: String,
    },

    /// Incremental output from a running tool (stdout/stderr lines).
    ToolOutput { call_id: String, chunk: String },

    /// A tool call has finished.
    ToolFinished {
        call_id: String,
        result: ToolOutcome,
    },

    /// Engine needs user permission before executing a tool.
    RequestPermission {
        request_id: u64,
        call_id: String,
        tool_name: String,
        args: HashMap<String, serde_json::Value>,
        confirm_message: String,
        approval_pattern: Option<String>,
        summary: Option<String>,
    },

    /// Engine needs the user to answer a question (ask_user_question tool).
    RequestAnswer {
        request_id: u64,
        args: HashMap<String, serde_json::Value>,
    },

    /// Token usage update after an LLM call.
    TokenUsage { prompt_tokens: u32 },

    /// LLM call failed, engine is retrying.
    Retrying { delay_ms: u64, attempt: u32 },

    /// A background process has finished.
    ProcessCompleted { id: String, exit_code: Option<i32> },

    /// Response to `UiCommand::Compact`.
    CompactionComplete { messages: Vec<Message> },

    /// Response to `UiCommand::GenerateTitle`.
    TitleGenerated { title: String },

    /// The agent turn completed successfully.
    TurnComplete { messages: Vec<Message> },

    /// The agent turn ended with an error.
    TurnError { message: String },

    /// Engine is shutting down.
    Shutdown { reason: Option<String> },
}

// ── UI → Engine ─────────────────────────────────────────────────────────────

/// Commands sent from the UI to the engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UiCommand {
    /// Start a new agent turn.
    StartTurn {
        input: String,
        mode: Mode,
        model: String,
        reasoning_effort: ReasoningEffort,
        history: Vec<Message>,
        /// Override API base URL for this turn (uses engine default if None).
        api_base: Option<String>,
        /// Override API key for this turn (uses engine default if None).
        api_key: Option<String>,
    },

    /// Inject a message mid-turn (steering / type-ahead).
    Steer { text: String },

    /// Reply to a `RequestPermission` event.
    PermissionDecision {
        request_id: u64,
        approved: bool,
        message: Option<String>,
    },

    /// Reply to a `RequestAnswer` event.
    QuestionAnswer {
        request_id: u64,
        answer: Option<String>,
    },

    /// Change the active mode while the engine is running.
    SetMode { mode: Mode },

    /// Change reasoning effort while the engine is running.
    SetReasoningEffort { effort: ReasoningEffort },

    /// Compact conversation history.
    Compact {
        keep_turns: usize,
        history: Vec<Message>,
    },

    /// Generate a title for the session.
    GenerateTitle { first_message: String },

    /// Cancel the current turn.
    Cancel,
}

// ── Shared Domain Types ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Normal,
    Plan,
    Apply,
    Yolo,
}

impl Mode {
    pub fn toggle(self) -> Self {
        match self {
            Mode::Normal => Mode::Plan,
            Mode::Plan => Mode::Apply,
            Mode::Apply => Mode::Yolo,
            Mode::Yolo => Mode::Normal,
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "normal" => Some(Mode::Normal),
            "plan" => Some(Mode::Plan),
            "apply" => Some(Mode::Apply),
            "yolo" => Some(Mode::Yolo),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Normal => "normal",
            Mode::Plan => "plan",
            Mode::Apply => "apply",
            Mode::Yolo => "yolo",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
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

/// Serde helper: always serializes as "function".
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
            Err(serde::de::Error::custom(format!(
                "expected \"function\", got \"{v}\""
            )))
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolOutcome {
    pub content: String,
    pub is_error: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    #[default]
    Off,
    Low,
    Medium,
    High,
}

impl ReasoningEffort {
    pub fn cycle(self) -> Self {
        match self {
            Self::Off => Self::Low,
            Self::Low => Self::Medium,
            Self::Medium => Self::High,
            Self::High => Self::Off,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// Metadata for a saved session (used by resume dialog).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub first_user_message: Option<String>,
    #[serde(default)]
    pub created_at_ms: u64,
    #[serde(default)]
    pub updated_at_ms: u64,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<ReasoningEffort>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
}
