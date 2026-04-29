//! Wire protocol between the engine and the UI.

use crate::content::Content;
use crate::message::{Message, ToolOutcome};
use crate::mode::{Mode, ReasoningEffort};
use crate::usage::{ModelConfigOverrides, PermissionOverrides, TokenUsage, TurnMeta};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Auxiliary LLM task — routed to a dedicated auxiliary model when the
/// engine config has one, otherwise falls back to the primary model.
/// `Btw` is the generic escape hatch (plain `/btw` prompts, plugin
/// `engine.ask` calls without a specific task tag).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuxiliaryTask {
    Title,
    Prediction,
    Compaction,
    #[default]
    Btw,
}

/// How a plugin tool interacts with concurrent tool execution.
///
/// `Concurrent` (default): runs alongside other tools via the engine's
/// `pending_plugins` channel — good for pure data fetches with no UI.
///
/// `Sequential`: deferred until after every concurrent tool has
/// finished, then dispatched one at a time. Used by tools that open a
/// dialog and await a user reply — the user should see all other tool
/// output before the prompt. `ask_user_question` is the canonical
/// example.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolExecutionMode {
    #[default]
    Concurrent,
    Sequential,
}

/// A tool defined by a Lua plugin. Sent from TUI to engine so the engine
/// can include it in LLM tool definitions and proxy execution back.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginToolDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    /// When set, the tool is only available in these modes.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub modes: Option<Vec<Mode>>,
    #[serde(default)]
    pub execution_mode: ToolExecutionMode,
    /// When `true`, this plugin tool replaces the core Rust tool of the
    /// same name. The engine drops the core definition from the LLM
    /// schema and dispatches calls to the plugin instead. When `false`
    /// (default), registering a name that collides with a core tool
    /// is an error reported back to the user.
    #[serde(default)]
    pub override_core: bool,
    /// Hook signals declared by the plugin. Each `true` flag tells the
    /// engine to round-trip through `EvaluatePluginToolHooks` before
    /// dispatching the tool — to ask the user for permission, run a
    /// preflight check, etc. When all flags are false the engine
    /// dispatches the tool directly (today's behavior, no permission
    /// gate). Plugin tools that touch security-sensitive surfaces
    /// (bash, file mutation) MUST opt in.
    #[serde(default)]
    pub hooks: PluginToolHookFlags,
}

/// Which permission hooks a plugin tool has registered. Sent with
/// `PluginToolDef` so the engine knows whether to ask the TUI to
/// evaluate them per-call.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginToolHookFlags {
    #[serde(default)]
    pub needs_confirm: bool,
    #[serde(default)]
    pub approval_patterns: bool,
    #[serde(default)]
    pub preflight: bool,
}

impl PluginToolHookFlags {
    /// True when at least one hook is registered — i.e. the engine must
    /// round-trip through `EvaluatePluginToolHooks` before dispatch.
    pub fn any(&self) -> bool {
        self.needs_confirm || self.approval_patterns || self.preflight
    }
}

/// Result of evaluating a plugin tool's permission hooks for a specific
/// invocation. Returned by the TUI in response to
/// `EngineEvent::EvaluatePluginToolHooks`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginToolHooks {
    /// Confirm dialog message; `None` falls back to the tool name.
    #[serde(default)]
    pub needs_confirm: Option<String>,
    /// Approval patterns to offer "always allow" for.
    #[serde(default)]
    pub approval_patterns: Vec<String>,
    /// Pre-flight error; `Some` fails the call immediately without
    /// asking the user.
    #[serde(default)]
    pub preflight_error: Option<String>,
}

/// Events emitted by the engine. The UI consumes these to update its display.
///
/// Most variants are fire-and-forget. The exception is `RequestPermission`,
/// which carries a `request_id` that the UI must eventually reply to via
/// `UiCommand::PermissionDecision`.
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

    /// Response to a `UiCommand::EngineAsk` request.
    EngineAskResponse { id: u64, content: String },

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

    /// Engine needs the TUI to execute a plugin-defined tool.
    ExecutePluginTool {
        request_id: u64,
        call_id: String,
        tool_name: String,
        args: HashMap<String, serde_json::Value>,
    },

    /// Engine asks the TUI to evaluate a plugin tool's permission
    /// hooks (`needs_confirm`, `approval_patterns`, `preflight`) for a
    /// specific invocation. The TUI replies with
    /// `UiCommand::PluginToolHooksResult`, after which the engine
    /// resumes the standard Allow / Deny / Ask flow.
    EvaluatePluginToolHooks {
        request_id: u64,
        call_id: String,
        tool_name: String,
        args: HashMap<String, serde_json::Value>,
        mode: Mode,
    },

    /// Result of a core-tool side call requested by a Lua plugin via
    /// `smelt.tools.call`. Streamed back so the suspended Lua coroutine
    /// can resume with the tool's output.
    CoreToolResult {
        request_id: u64,
        content: String,
        is_error: bool,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        metadata: Option<serde_json::Value>,
    },
}

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
        /// Per-turn permission overrides (from custom commands or Lua plugins).
        #[serde(skip_serializing_if = "Option::is_none", default)]
        permission_overrides: Option<PermissionOverrides>,
        /// Full system prompt assembled by the TUI (from prompt sections).
        /// When present the engine uses this verbatim instead of rendering
        /// its built-in template.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        system_prompt: Option<String>,
        /// Plugin-defined tools registered by Lua plugins. The engine
        /// includes these in the LLM tool definitions and proxies execution
        /// back to the TUI via `ExecutePluginTool`.
        #[serde(skip_serializing_if = "Vec::is_empty", default)]
        plugin_tools: Vec<PluginToolDef>,
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

    /// Change the active mode while the engine is running.
    SetMode {
        mode: Mode,
        /// Updated system prompt for the new mode (if managed by TUI).
        #[serde(skip_serializing_if = "Option::is_none", default)]
        system_prompt: Option<String>,
        /// Updated plugin tools for the new mode.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        plugin_tools: Option<Vec<PluginToolDef>>,
    },

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

    /// One-shot LLM call initiated by a Lua plugin. The engine spawns
    /// a fire-and-forget request routed through `task`'s auxiliary
    /// model (or the primary model when the routing slot is empty) and
    /// returns the response as `EngineAskResponse`.
    EngineAsk {
        id: u64,
        system: String,
        messages: Vec<Message>,
        #[serde(default)]
        task: AuxiliaryTask,
    },

    /// Result of a plugin tool execution (response to `ExecutePluginTool`).
    PluginToolResult {
        request_id: u64,
        call_id: String,
        content: String,
        is_error: bool,
    },

    /// Result of evaluating a plugin tool's permission hooks (response
    /// to `EngineEvent::EvaluatePluginToolHooks`).
    PluginToolHooksResult {
        request_id: u64,
        hooks: PluginToolHooks,
    },

    /// Side-call from a Lua plugin to a core (or another plugin) tool.
    /// The engine runs the named tool and replies with
    /// `EngineEvent::CoreToolResult`. The plugin's parent `call_id` is
    /// reused so streamed output (e.g. `ToolOutput`) is grouped under
    /// the visible plugin invocation.
    CallCoreTool {
        request_id: u64,
        parent_call_id: String,
        tool_name: String,
        args: HashMap<String, serde_json::Value>,
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
