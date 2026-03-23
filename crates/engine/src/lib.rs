mod agent;
pub mod cancel;
pub mod config;
pub mod image;
pub mod log;
pub mod paths;
pub mod permissions;
pub mod plan;
pub mod provider;
pub mod tools;

use protocol::{EngineEvent, UiCommand};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

pub use config::ModelConfig;
pub use paths::{cache_dir, config_dir, home_dir, state_dir};
pub use permissions::Permissions;
pub use provider::{Provider, ProviderKind};

/// Assemble the system prompt from the base template, mode overlay, cwd, and
/// optional extra instructions (e.g. from AGENTS.md files).
pub fn build_system_prompt(
    mode: protocol::Mode,
    cwd: &std::path::Path,
    extra_instructions: Option<&str>,
) -> String {
    let base = include_str!("prompts/system.txt");
    let overlay = match mode {
        protocol::Mode::Apply | protocol::Mode::Yolo => include_str!("prompts/system_apply.txt"),
        protocol::Mode::Plan => include_str!("prompts/system_plan.txt"),
        protocol::Mode::Normal => "",
    };

    let mut prompt = format!("{base}\n\nYou are working in: {cwd}", cwd = cwd.display());

    if !overlay.is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(overlay);
    }

    if let Some(instructions) = extra_instructions {
        prompt.push_str("\n\n");
        prompt.push_str(instructions);
    }

    prompt
}

/// Configuration for the engine. Constructed once by the binary.
pub struct EngineConfig {
    pub api_base: String,
    pub api_key: String,
    /// Provider type: "openai", "anthropic", or "openai-compatible".
    pub provider_type: String,
    pub model_config: ModelConfig,
    pub instructions: Option<String>,
    /// When set, replaces the entire system prompt (skips the built-in
    /// template, mode overlays, and AGENTS.md instructions).
    pub system_prompt_override: Option<String>,
    pub cwd: PathBuf,
    pub permissions: Arc<Permissions>,
}

/// Handle to a running engine. Send commands, receive events.
pub struct EngineHandle {
    pub cmd_tx: mpsc::UnboundedSender<UiCommand>,
    pub event_rx: mpsc::UnboundedReceiver<EngineEvent>,
    pub processes: tools::ProcessRegistry,
    pub permissions: Arc<Permissions>,
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
}

/// Start the engine. Returns a handle for bidirectional communication.
pub fn start(config: EngineConfig) -> EngineHandle {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();

    let processes = tools::ProcessRegistry::new();
    let registry = tools::build_tools(processes.clone());

    let permissions = Arc::clone(&config.permissions);
    let processes_clone = processes.clone();
    tokio::spawn(agent::engine_task(
        config,
        registry,
        processes_clone,
        cmd_rx,
        event_tx,
    ));

    EngineHandle {
        cmd_tx,
        event_rx,
        processes,
        permissions,
    }
}
