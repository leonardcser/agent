mod agent;
pub mod cancel;
pub mod config;
pub mod config_file;
pub mod image;
pub mod log;
pub mod mcp;
pub mod paths;
pub mod permissions;
pub mod plan;
pub mod pricing;
pub mod provider;
pub mod registry;
pub mod skills;
pub mod socket;
pub mod tools;

use protocol::{EngineEvent, UiCommand};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Trigger auto-compaction when prompt tokens reach this percentage of the
/// context window.
pub const COMPACT_THRESHOLD_PERCENT: u64 = 80;

pub use config::ModelConfig;
pub use mcp::McpServerConfig;
pub use paths::{cache_dir, config_dir, home_dir, state_dir};
pub use permissions::Permissions;
pub use provider::{Provider, ProviderKind};
pub use skills::SkillLoader;

/// Context for rendering the system prompt template.
pub struct PromptContext<'a> {
    pub cwd: &'a std::path::Path,
    pub interactive: bool,
    pub write_access: bool,
    pub plan_mode: bool,
    pub multi_agent: Option<&'a AgentPromptConfig>,
    pub skills_section: Option<&'a str>,
    pub extra_instructions: Option<&'a str>,
}

/// Assemble the system prompt from the base template, mode overlay, cwd, and
/// optional extra instructions (e.g. from AGENTS.md files).
pub fn build_system_prompt(
    mode: protocol::Mode,
    cwd: &std::path::Path,
    extra_instructions: Option<&str>,
) -> String {
    build_system_prompt_full(mode, cwd, extra_instructions, None, None, true)
}

pub fn build_system_prompt_full(
    mode: protocol::Mode,
    cwd: &std::path::Path,
    extra_instructions: Option<&str>,
    agent_config: Option<&AgentPromptConfig>,
    skill_section: Option<&str>,
    interactive: bool,
) -> String {
    let ctx = PromptContext {
        cwd,
        interactive,
        write_access: matches!(mode, protocol::Mode::Apply | protocol::Mode::Yolo),
        plan_mode: matches!(mode, protocol::Mode::Plan),
        multi_agent: agent_config,
        skills_section: skill_section,
        extra_instructions,
    };
    render_system_prompt(&ctx)
}

/// Render the system prompt template with the given context.
pub fn render_system_prompt(ctx: &PromptContext<'_>) -> String {
    let template_src = include_str!("prompts/system.txt");
    let env = minijinja::Environment::new();
    let template = env
        .template_from_str(template_src)
        .expect("system prompt template should parse");

    let is_child = ctx.multi_agent.map(|m| m.depth > 0).unwrap_or(false);
    let agent_id = ctx.multi_agent.map(|m| m.agent_id.as_str()).unwrap_or("");
    let parent_id = ctx
        .multi_agent
        .and_then(|m| m.parent_id.as_deref())
        .unwrap_or("unknown");
    let siblings = ctx
        .multi_agent
        .map(|m| m.siblings.join(", "))
        .unwrap_or_default();

    let rendered = template
        .render(minijinja::context! {
            cwd => ctx.cwd.display().to_string(),
            interactive => ctx.interactive,
            write_access => ctx.write_access,
            plan_mode => ctx.plan_mode,
            multi_agent => ctx.multi_agent.is_some(),
            is_child => is_child,
            agent_id => agent_id,
            parent_id => parent_id,
            siblings => siblings,
            skills_section => ctx.skills_section.unwrap_or(""),
            extra_instructions => ctx.extra_instructions.unwrap_or(""),
        })
        .expect("system prompt template should render");

    // Collapse runs of 3+ blank lines into 2 (section separators).
    let mut result = String::with_capacity(rendered.len());
    let mut blank_count = 0u32;
    for line in rendered.lines() {
        if line.trim().is_empty() {
            blank_count += 1;
            if blank_count <= 2 {
                result.push('\n');
            }
        } else {
            blank_count = 0;
            result.push_str(line);
            result.push('\n');
        }
    }
    result.trim().to_string()
}

/// Configuration for the multi-agent section of the system prompt.
#[derive(Clone)]
pub struct AgentPromptConfig {
    pub agent_id: String,
    pub depth: u8,
    pub parent_id: Option<String>,
    /// Sibling agent names (other children of the same parent).
    pub siblings: Vec<String>,
}

/// Multi-agent configuration. Present when multi-agent mode is enabled.
pub struct MultiAgentConfig {
    pub depth: u8,
    pub max_depth: u8,
    pub max_agents: u8,
    pub parent_pid: Option<u32>,
    /// Optional preselected agent ID for interactive root agents.
    /// When provided, engine tools use this exact identity.
    pub agent_id: Option<String>,
}

/// API connection and model configuration, grouped for clarity.
pub struct ApiConfig {
    pub base: String,
    pub key: String,
    pub key_env: String,
    pub provider_type: String,
    pub model_config: ModelConfig,
}

/// Configuration for the engine. Constructed once by the binary.
pub struct EngineConfig {
    pub api: ApiConfig,
    pub instructions: Option<String>,
    /// When set, replaces the entire system prompt (skips the built-in
    /// template, mode overlays, and AGENTS.md instructions).
    pub system_prompt_override: Option<String>,
    pub cwd: PathBuf,
    pub permissions: Arc<Permissions>,
    /// Multi-agent settings. `None` when multi-agent is disabled.
    pub multi_agent: Option<MultiAgentConfig>,
    /// True when a human is present (TUI mode). False for headless/subagent.
    pub interactive: bool,
    /// MCP server configurations.
    pub mcp_servers: HashMap<String, McpServerConfig>,
    /// Pre-loaded skill loader.
    pub skills: Option<Arc<SkillLoader>>,
    /// Auto-compact when context usage crosses the threshold.
    pub auto_compact: bool,
    /// Context window size (tokens). When `None`, the engine fetches it from
    /// the provider API at startup. User config can override via
    /// `context_window`.
    pub context_window: Option<u32>,
}

/// Handle to a running engine. Send commands, receive events.
pub struct EngineHandle {
    cmd_tx: mpsc::UnboundedSender<UiCommand>,
    event_tx: mpsc::UnboundedSender<EngineEvent>,
    event_rx: mpsc::UnboundedReceiver<EngineEvent>,
    pub processes: tools::ProcessRegistry,
    pub permissions: Arc<Permissions>,
    agent_msg_tx: Option<tokio::sync::broadcast::Sender<tools::AgentMessageNotification>>,
    spawned_rx: Option<mpsc::UnboundedReceiver<tools::SpawnedChild>>,
}

impl EngineHandle {
    pub fn send(&self, cmd: UiCommand) {
        let _ = self.cmd_tx.send(cmd);
    }

    pub async fn recv(&mut self) -> Option<EngineEvent> {
        self.event_rx.recv().await
    }

    pub fn try_recv(&mut self) -> Result<EngineEvent, mpsc::error::TryRecvError> {
        self.event_rx.try_recv()
    }

    /// Inject an inter-agent message event into the engine's event stream.
    pub fn inject_agent_message(&self, from_id: String, from_slug: String, message: String) {
        let _ = self.event_tx.send(EngineEvent::AgentMessage {
            from_id,
            from_slug,
            message,
        });
    }

    /// Inject an agent-exited event into the engine's event stream.
    pub fn inject_agent_exited(&self, agent_id: String, exit_code: Option<i32>) {
        let _ = self.event_tx.send(EngineEvent::AgentExited {
            agent_id,
            exit_code,
        });
    }

    /// Notify blocking `spawn_agent` calls that an agent message arrived.
    pub fn notify_agent_message(&self, notif: tools::AgentMessageNotification) {
        if let Some(ref tx) = self.agent_msg_tx {
            let _ = tx.send(notif);
        }
    }

    /// Drain spawned child handles (stdout pipes for subagent streaming).
    pub fn drain_spawned(&mut self) -> Vec<tools::SpawnedChild> {
        let Some(ref mut rx) = self.spawned_rx else {
            return vec![];
        };
        let mut children = vec![];
        while let Ok(child) = rx.try_recv() {
            children.push(child);
        }
        children
    }

    /// Create a cloneable injector for external tasks (socket bridge, watchers)
    /// that need to inject events into the engine's event stream.
    pub fn injector(&self) -> EventInjector {
        EventInjector {
            event_tx: self.event_tx.clone(),
            agent_msg_tx: self.agent_msg_tx.clone(),
        }
    }
}

/// Cloneable handle for injecting events from external async tasks.
#[derive(Clone)]
pub struct EventInjector {
    event_tx: mpsc::UnboundedSender<EngineEvent>,
    agent_msg_tx: Option<tokio::sync::broadcast::Sender<tools::AgentMessageNotification>>,
}

impl EventInjector {
    pub fn inject_agent_message(&self, from_id: String, from_slug: String, message: String) {
        if let Some(ref tx) = self.agent_msg_tx {
            let _ = tx.send(tools::AgentMessageNotification {
                from_id: from_id.clone(),
                from_slug: from_slug.clone(),
                message: message.clone(),
            });
        }
        let _ = self.event_tx.send(EngineEvent::AgentMessage {
            from_id,
            from_slug,
            message,
        });
    }

    pub fn inject_agent_exited(&self, agent_id: String, exit_code: Option<i32>) {
        let _ = self.event_tx.send(EngineEvent::AgentExited {
            agent_id,
            exit_code,
        });
    }
}

/// Start the engine. Returns a handle for bidirectional communication.
///
/// MCP servers are connected asynchronously — this must be called from
/// within a tokio runtime.
pub fn start(config: EngineConfig) -> EngineHandle {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();

    let processes = tools::ProcessRegistry::new();

    // Broadcast channel for agent message notifications (blocking spawn_agent).
    // Only created for interactive agents (depth == 0) that can spawn children.
    let agent_msg_tx = if config.multi_agent.as_ref().is_some_and(|ma| ma.depth == 0) {
        let (tx, _) = tokio::sync::broadcast::channel(16);
        Some(tx)
    } else {
        None
    };

    // Channel for spawned child stdout handles (used for streaming).
    let (spawned_tx, spawned_rx) = mpsc::unbounded_channel();

    let ma_config = if let Some(ref ma) = config.multi_agent {
        let scope = config.cwd.to_string_lossy().into_owned();
        let my_pid = std::process::id();
        // Subagents: read the pre-registered agent_id from the registry.
        // Interactive sessions: generate a unique ID.
        let agent_id = if ma.depth > 0 {
            registry::read_entry(my_pid)
                .ok()
                .map(|e| e.agent_id)
                .unwrap_or_else(registry::next_agent_id)
        } else {
            ma.agent_id.clone().unwrap_or_else(registry::next_agent_id)
        };
        Some(tools::MultiAgentToolConfig {
            scope,
            pid: my_pid,
            agent_id,
            depth: ma.depth,
            max_depth: ma.max_depth,
            max_agents: ma.max_agents,
            parent_pid: ma.parent_pid,
            slug: std::sync::Arc::new(std::sync::Mutex::new(None)),
            api_base: config.api.base.clone(),
            api_key_env: config.api.key_env.clone(),
            model: config.api.model_config.name.clone().unwrap_or_default(),
            provider_type: config.api.provider_type.clone(),
            agent_msg_tx: agent_msg_tx.clone(),
            spawned_tx: Some(spawned_tx),
        })
    } else {
        None
    };

    let registry = tools::build_tools(processes.clone(), ma_config, config.skills.clone());

    let permissions = Arc::clone(&config.permissions);
    let has_multi_agent = config.multi_agent.is_some();
    let processes_clone = processes.clone();
    let event_tx_clone = event_tx.clone();
    tokio::spawn(agent::engine_task(
        config,
        registry,
        processes_clone,
        cmd_rx,
        event_tx,
    ));

    EngineHandle {
        cmd_tx,
        event_tx: event_tx_clone,
        event_rx,
        processes,
        permissions,
        agent_msg_tx,
        spawned_rx: if has_multi_agent {
            Some(spawned_rx)
        } else {
            None
        },
    }
}
