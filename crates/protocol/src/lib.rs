//! Wire-protocol and shared domain types between the engine and the UI.
//!
//! Layout:
//! - [`content`]: multipart message content (text + images)
//! - [`message`]: `Message`, `Role`, tool calls, tool outcomes
//! - [`mode`]: agent modes and reasoning effort levels
//! - [`usage`]: token usage, turn metadata, per-turn overrides
//! - [`event`]: the wire contract — `EngineEvent` and `UiCommand`

pub mod content;
pub mod event;
pub mod message;
pub mod mode;
pub mod usage;

pub use content::{Content, ContentPart};
pub use event::{
    AuxiliaryTask, EngineEvent, PluginToolDef, PluginToolHookFlags, PluginToolHooks,
    ToolExecutionMode, UiCommand,
};
pub use message::{FunctionCall, Message, Role, ToolCall, ToolOutcome};
pub use mode::{Mode, ReasoningEffort};
pub use usage::{
    AgentBlockData, AgentToolData, ModelConfigOverrides, PermissionOverrides, RuleSetOverride,
    TokenUsage, TurnMeta,
};
