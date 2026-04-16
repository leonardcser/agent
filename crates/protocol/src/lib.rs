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

    pub fn image_labels(&self) -> Vec<String> {
        match self {
            Content::Text(_) => vec![],
            Content::Parts(parts) => parts
                .iter()
                .filter_map(|p| match p {
                    ContentPart::ImageUrl { label, .. } => {
                        Some(format!("[{}]", label.as_deref().unwrap_or("image")))
                    }
                    _ => None,
                })
                .collect(),
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

// ── Token usage ─────────────────────────────────────────────────────────────

/// Parsed token usage from an API response.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u32>,
}

impl TokenUsage {
    /// Add another usage report into this accumulator.
    pub fn accumulate(&mut self, other: &TokenUsage) {
        fn add(a: &mut Option<u32>, b: Option<u32>) {
            if let Some(v) = b {
                *a = Some(a.unwrap_or(0) + v);
            }
        }
        add(&mut self.prompt_tokens, other.prompt_tokens);
        add(&mut self.completion_tokens, other.completion_tokens);
        add(&mut self.cache_read_tokens, other.cache_read_tokens);
        add(&mut self.cache_write_tokens, other.cache_write_tokens);
        add(&mut self.reasoning_tokens, other.reasoning_tokens);
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

    /// Incremental thinking token from the LLM (streaming delta).
    ThinkingDelta { delta: String },

    /// Streamed assistant text (may arrive in chunks).
    Text { content: String },

    /// Incremental text token from the LLM (streaming delta).
    TextDelta { delta: String },

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
        elapsed_ms: Option<u64>,
    },

    /// Engine needs user permission before executing a tool.
    RequestPermission {
        request_id: u64,
        call_id: String,
        tool_name: String,
        args: HashMap<String, serde_json::Value>,
        confirm_message: String,
        approval_patterns: Vec<String>,
        summary: Option<String>,
    },

    /// Engine needs the user to answer a question (ask_user_question tool).
    RequestAnswer {
        request_id: u64,
        args: HashMap<String, serde_json::Value>,
    },

    /// Token usage update after an LLM call.
    TokenUsage {
        usage: TokenUsage,
        tokens_per_sec: Option<f64>,
        cost_usd: Option<f64>,
        /// True for background requests (title, compaction, btw, predict)
        /// whose prompt_tokens should not update the displayed context usage.
        #[serde(default)]
        background: bool,
    },

    /// LLM call failed, engine is retrying.
    Retrying { delay_ms: u64, attempt: u32 },

    /// A background process has finished.
    ProcessCompleted { id: String, exit_code: Option<i32> },

    /// Response to `UiCommand::Compact`.
    CompactionComplete { messages: Vec<Message> },

    /// Response to `UiCommand::GenerateTitle`.
    TitleGenerated { title: String, slug: String },

    /// Response to `UiCommand::Btw`.
    BtwResponse { content: String },

    /// Predicted next user input (ghost text autocomplete).
    InputPrediction { text: String, generation: u64 },

    /// Snapshot of the engine's message list, sent after each significant step.
    Messages {
        turn_id: u64,
        messages: Vec<Message>,
    },

    /// The agent turn completed successfully.
    TurnComplete {
        turn_id: u64,
        messages: Vec<Message>,
        meta: Option<TurnMeta>,
    },

    /// The agent turn ended with an error.
    TurnError { message: String },

    /// Engine is shutting down.
    Shutdown { reason: Option<String> },

    /// A subagent exited (expected or unexpected).
    AgentExited {
        agent_id: String,
        exit_code: Option<i32>,
    },

    /// An inter-agent message arrived via the socket.
    AgentMessage {
        from_id: String,
        from_slug: String,
        message: String,
    },
}

// ── UI → Engine ─────────────────────────────────────────────────────────────

/// Commands sent from the UI to the engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)]
pub enum UiCommand {
    /// Start a new agent turn.
    StartTurn {
        turn_id: u64,
        content: Content,
        mode: Mode,
        model: String,
        reasoning_effort: ReasoningEffort,
        history: Vec<Message>,
        /// Override API base URL for this turn (uses engine default if None).
        api_base: Option<String>,
        /// Override API key for this turn (uses engine default if None).
        api_key: Option<String>,
        /// Session ID for plan file storage.
        session_id: String,
        /// On-disk directory for this session (date-bucketed).
        session_dir: std::path::PathBuf,
        /// Per-turn model parameter overrides (from custom commands).
        #[serde(skip_serializing_if = "Option::is_none", default)]
        model_config_overrides: Option<ModelConfigOverrides>,
        /// Per-turn permission overrides (from custom commands).
        #[serde(skip_serializing_if = "Option::is_none", default)]
        permission_overrides: Option<PermissionOverrides>,
    },

    /// Inject a message mid-turn (steering / type-ahead).
    Steer { text: String },

    /// Remove the last `count` steered messages (user unqueued them).
    Unsteer { count: usize },

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

    /// Change the model/provider while the engine is running.
    SetModel {
        model: String,
        api_base: String,
        api_key: String,
        provider_type: String,
    },

    /// Compact conversation history.
    Compact {
        keep_turns: usize,
        history: Vec<Message>,
        instructions: Option<String>,
    },

    /// Generate a title for the session based on the latest user message and
    /// the tail of the assistant's response.
    GenerateTitle {
        last_user_message: String,
        assistant_tail: String,
    },

    /// Ask an ephemeral side question (no tools, not added to history).
    Btw {
        question: String,
        history: Vec<Message>,
        reasoning_effort: ReasoningEffort,
    },

    /// Predict the user's next input based on conversation history.
    PredictInput {
        history: Vec<Message>,
        generation: u64,
    },

    /// Cancel the current turn.
    Cancel,

    /// Inject an inter-agent message as a steer message.
    AgentMessage {
        from_id: String,
        from_slug: String,
        message: String,
    },
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

    /// Cycle to the next mode within the given allowed list.
    pub fn cycle_within(self, allowed: &[Self]) -> Self {
        let list = if allowed.is_empty() {
            Self::ALL
        } else {
            allowed
        };
        let pos = list.iter().position(|&m| m == self);
        match pos {
            Some(i) => list[(i + 1) % list.len()],
            None => list[0],
        }
    }

    /// Parse a list of mode labels, skipping unknown ones.
    pub fn parse_list(items: &[String]) -> Vec<Self> {
        items.iter().filter_map(|s| Self::parse(s)).collect()
    }

    /// The full default cycle order.
    pub const ALL: &[Self] = &[Self::Normal, Self::Plan, Self::Apply, Self::Yolo];
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
    /// Whether this tool result is an error. Only meaningful for `Role::Tool`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_error: bool,
    /// Agent identity fields. Only meaningful for `Role::Agent`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_from_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub agent_from_slug: Option<String>,
}

impl Message {
    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: Some(Content::text(text)),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            is_error: false,
            agent_from_id: None,
            agent_from_slug: None,
        }
    }

    pub fn user(content: Content) -> Self {
        Self {
            role: Role::User,
            content: Some(content),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            is_error: false,
            agent_from_id: None,
            agent_from_slug: None,
        }
    }

    pub fn assistant(
        content: Option<Content>,
        reasoning: Option<String>,
        tool_calls: Option<Vec<ToolCall>>,
    ) -> Self {
        Self {
            role: Role::Assistant,
            content,
            reasoning_content: reasoning,
            tool_calls,
            tool_call_id: None,
            is_error: false,
            agent_from_id: None,
            agent_from_slug: None,
        }
    }

    pub fn tool(call_id: String, content: impl Into<String>, is_error: bool) -> Self {
        Self {
            role: Role::Tool,
            content: Some(Content::text(content)),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: Some(call_id),
            is_error,
            agent_from_id: None,
            agent_from_slug: None,
        }
    }

    pub fn agent(from_id: &str, from_slug: &str, message: impl Into<String>) -> Self {
        Self {
            role: Role::Agent,
            content: Some(Content::text(message)),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            is_error: false,
            agent_from_id: Some(from_id.to_string()),
            agent_from_slug: Some(from_slug.to_string()),
        }
    }

    /// Format an Agent message's content for the LLM API (which only knows
    /// system/user/assistant/tool). Wraps in XML tags to clearly distinguish
    /// from actual user messages.
    pub fn agent_api_text(&self) -> String {
        let raw = self
            .content
            .as_ref()
            .map(|c| c.as_text())
            .unwrap_or_default();
        let id = self.agent_from_id.as_deref().unwrap_or("");
        let slug = self.agent_from_slug.as_deref().unwrap_or("");
        if slug.is_empty() {
            format!("<agent-message from=\"{id}\">\n{raw}\n</agent-message>")
        } else {
            format!("<agent-message from=\"{id}\" task=\"{slug}\">\n{raw}\n</agent-message>")
        }
    }
}

fn is_false(v: &bool) -> bool {
    !v
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
    /// Inter-agent message. Serialized as "user" for API calls (providers only
    /// support system/user/assistant/tool), but stored distinctly in our protocol
    /// so the TUI can render it differently.
    Agent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    call_type: AlwaysFunction,
    pub function: FunctionCall,
}

impl ToolCall {
    pub fn new(id: String, function: FunctionCall) -> Self {
        Self {
            id,
            call_type: AlwaysFunction,
            function,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    #[serde(deserialize_with = "deserialize_arguments")]
    pub arguments: String,
}

/// Accept `arguments` as either a JSON string or a JSON object.
/// OpenAI returns a stringified JSON object, but llama.cpp and some other
/// backends return a raw JSON object. Normalize to a string in both cases.
fn deserialize_arguments<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    match v {
        serde_json::Value::String(s) => Ok(s),
        other => Ok(other.to_string()),
    }
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
    /// Structured metadata for tools that need to communicate machine-readable
    /// data alongside the human-readable content string.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub metadata: Option<serde_json::Value>,
}

/// Per-turn metadata emitted by the engine at turn completion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnMeta {
    pub elapsed_ms: u64,
    pub avg_tps: Option<f64>,
    pub interrupted: bool,
    /// Per-tool-call elapsed times, keyed by call_id.
    #[serde(default)]
    pub tool_elapsed: HashMap<String, u64>,
    /// Subagent block data, keyed by spawn_agent call_id.
    #[serde(default)]
    pub agent_blocks: HashMap<String, AgentBlockData>,
}

/// Persisted subagent block state for session resume.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentBlockData {
    pub slug: Option<String>,
    pub tool_calls: Vec<AgentToolData>,
}

/// A single tool call from a subagent's execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentToolData {
    pub tool_name: String,
    pub summary: String,
    pub elapsed_ms: Option<u64>,
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
    Max,
}

impl ReasoningEffort {
    /// Parse from a string label.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "off" => Some(Self::Off),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "max" => Some(Self::Max),
            _ => None,
        }
    }

    /// Cycle to the next effort level within the given allowed list.
    pub fn cycle_within(self, allowed: &[Self]) -> Self {
        if allowed.is_empty() {
            return self;
        }
        let pos = allowed.iter().position(|&e| e == self);
        match pos {
            Some(i) => allowed[(i + 1) % allowed.len()],
            None => allowed[0],
        }
    }

    /// Parse a list of effort labels into enum values, skipping unknown ones.
    pub fn parse_list(items: &[String]) -> Vec<Self> {
        items.iter().filter_map(|s| Self::parse(s)).collect()
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Max => "max",
        }
    }
}

// ── Per-turn overrides (for custom commands) ────────────────────────────────

/// Model-parameter overrides applied to a single turn.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelConfigOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repeat_penalty: Option<f64>,
}

/// Permission rule-set override (allow / ask / deny glob patterns).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RuleSetOverride {
    pub allow: Vec<String>,
    pub ask: Vec<String>,
    pub deny: Vec<String>,
}

/// Per-turn permission overrides for tools, bash, and web_fetch.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PermissionOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<RuleSetOverride>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bash: Option<RuleSetOverride>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub web_fetch: Option<RuleSetOverride>,
}
