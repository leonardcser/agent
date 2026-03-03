use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ── Multipart content ────────────────────────────────────────────────────────

/// A single part of a multipart message content block.
#[derive(Debug, Clone)]
pub enum ContentPart {
    Text { text: String },
    ImageUrl { url: String, label: Option<String> },
}

impl Serialize for ContentPart {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            ContentPart::Text { text } => {
                let mut map = s.serialize_map(Some(2))?;
                map.serialize_entry("type", "text")?;
                map.serialize_entry("text", text)?;
                map.end()
            }
            ContentPart::ImageUrl { url, label } => {
                let entries = 2 + usize::from(label.is_some());
                let mut map = s.serialize_map(Some(entries))?;
                map.serialize_entry("type", "image_url")?;
                map.serialize_entry("image_url", &serde_json::json!({"url": url}))?;
                if let Some(label) = label {
                    map.serialize_entry("label", label)?;
                }
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for ContentPart {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v: serde_json::Value = Deserialize::deserialize(d)?;
        match v.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                let text = v["text"].as_str().unwrap_or("").to_string();
                Ok(ContentPart::Text { text })
            }
            Some("image_url") => {
                let url = v["image_url"]["url"].as_str().unwrap_or("").to_string();
                let label = v.get("label").and_then(|l| l.as_str()).map(String::from);
                Ok(ContentPart::ImageUrl { url, label })
            }
            _ => Err(serde::de::Error::custom("unknown content part type")),
        }
    }
}

/// Message content: either a plain string or an array of typed parts.
///
/// Serializes as a JSON string when `Text`, or a JSON array when `Parts`.
#[derive(Debug, Clone)]
pub enum Content {
    Text(String),
    Parts(Vec<ContentPart>),
}

impl Content {
    pub fn text(s: impl Into<String>) -> Self {
        Content::Text(s.into())
    }

    /// Construct multipart content from text + labelled image data URLs.
    pub fn with_images(text: String, images: Vec<(String, String)>) -> Self {
        if images.is_empty() {
            return Content::Text(text);
        }
        let mut parts = vec![ContentPart::Text { text }];
        for (label, url) in images {
            parts.push(ContentPart::ImageUrl {
                url,
                label: Some(label),
            });
        }
        Content::Parts(parts)
    }

    /// Return the first text part, or the full string for `Text`.
    pub fn as_text(&self) -> &str {
        match self {
            Content::Text(s) => s,
            Content::Parts(parts) => parts
                .iter()
                .find_map(|p| match p {
                    ContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .unwrap_or(""),
        }
    }

    /// Concatenate all text parts (ignoring images).
    pub fn text_content(&self) -> String {
        match self {
            Content::Text(s) => s.clone(),
            Content::Parts(parts) => parts
                .iter()
                .filter_map(|p| match p {
                    ContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }

    pub fn image_count(&self) -> usize {
        match self {
            Content::Text(_) => 0,
            Content::Parts(parts) => parts
                .iter()
                .filter(|p| matches!(p, ContentPart::ImageUrl { .. }))
                .count(),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Content::Text(s) => s.is_empty(),
            Content::Parts(parts) => parts.is_empty(),
        }
    }
}

impl Serialize for Content {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Content::Text(text) => s.serialize_str(text),
            Content::Parts(parts) => parts.serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for Content {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v: serde_json::Value = Deserialize::deserialize(d)?;
        match v {
            serde_json::Value::String(s) => Ok(Content::Text(s)),
            serde_json::Value::Array(arr) => {
                let parts: Vec<ContentPart> = arr
                    .into_iter()
                    .map(|v| serde_json::from_value(v).map_err(serde::de::Error::custom))
                    .collect::<Result<_, _>>()?;
                Ok(Content::Parts(parts))
            }
            _ => Err(serde::de::Error::custom(
                "expected string or array for content",
            )),
        }
    }
}

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
    TokenUsage {
        prompt_tokens: u32,
        completion_tokens: Option<u32>,
        tokens_per_sec: Option<f64>,
    },

    /// LLM call failed, engine is retrying.
    Retrying { delay_ms: u64, attempt: u32 },

    /// A background process has finished.
    ProcessCompleted { id: String, exit_code: Option<i32> },

    /// Response to `UiCommand::Compact`.
    CompactionComplete { messages: Vec<Message> },

    /// Response to `UiCommand::GenerateTitle`.
    TitleGenerated { title: String },

    /// Snapshot of the engine's message list, sent after each significant step.
    Messages { messages: Vec<Message> },

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
    pub content: Option<Content>,
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
