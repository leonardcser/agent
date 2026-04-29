use super::{bool_arg, str_arg, Tool, ToolContext, ToolFuture, ToolResult};
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::broadcast::error::RecvError;

const BLOCKING_TIMEOUT: Duration = Duration::from_secs(600);
const CHILD_CHECK_INTERVAL: Duration = Duration::from_secs(5);

pub struct SpawnAgentTool {
    pub scope: String,
    pub my_pid: u32,
    pub depth: u8,
    pub max_agents: u8,
    pub api_base: String,
    pub api_key_env: String,
    pub model: String,
    pub provider_type: String,
    pub spawned_tx: Option<tokio::sync::mpsc::UnboundedSender<super::SpawnedChild>>,
    pub agent_msg_tx: Option<tokio::sync::broadcast::Sender<AgentMessageNotification>>,
}

/// Notification sent when an agent message arrives on the socket.
#[derive(Clone, Debug)]
pub struct AgentMessageNotification {
    pub from_id: String,
    pub from_slug: String,
    pub message: String,
}

impl Tool for SpawnAgentTool {
    fn name(&self) -> &str {
        "spawn_agent"
    }

    fn description(&self) -> &str {
        "Spawn a new subagent to work on a task. The subagent runs with full tool access. Give it a well-scoped task with all the context it needs — relevant files, constraints, and how its work fits into the larger picture. Set `wait` to true to block until the agent finishes and get its result directly. Subagents persist and build context — reuse them for related follow-ups via `message_agent`."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Detailed instructions for the subagent. Include task description, relevant files, constraints, and any context needed."
                },
                "wait": {
                    "type": "boolean",
                    "description": "If true, block until the subagent finishes and return its result. If false (default), spawn in the background and continue immediately."
                }
            },
            "required": ["prompt"]
        })
    }

    fn execute<'a>(&'a self, args: HashMap<String, Value>, ctx: &'a ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            let prompt = str_arg(&args, "prompt");
            let blocking = bool_arg(&args, "wait");

            // Check agent count limit.
            let current = crate::registry::discover(&self.scope);
            let count = current
                .iter()
                .filter(|e| e.parent_pid == Some(self.my_pid))
                .count();
            if count >= self.max_agents as usize {
                return ToolResult::err(format!(
                    "cannot spawn: already at max agents ({}) for this session",
                    self.max_agents
                ));
            }

            let child_depth = self.depth + 1;

            let exe = match std::env::current_exe() {
                Ok(p) => p,
                Err(e) => return ToolResult::err(format!("cannot find binary: {e}")),
            };

            let mut cmd = std::process::Command::new(&exe);
            cmd.args([
                "--subagent",
                "--multi-agent",
                "--parent-pid",
                &self.my_pid.to_string(),
                "--depth",
                &child_depth.to_string(),
                "--max-agents",
                &self.max_agents.to_string(),
                "--api-base",
                &self.api_base,
                "--api-key-env",
                &self.api_key_env,
                "--type",
                &self.provider_type,
                "-m",
                &self.model,
            ]);
            cmd.arg(&prompt);
            cmd.stdin(std::process::Stdio::null());
            cmd.env("FORCE_COLOR", "1");

            if self.provider_type == crate::ProviderKind::Codex.as_config_str() {
                if let Some(tokens) = crate::provider::codex::CodexTokens::load() {
                    cmd.env(
                        crate::provider::codex::CODEX_TOKENS_ENV,
                        tokens.to_env_json(),
                    );
                }
            }

            let agent_id = crate::registry::next_agent_id();

            cmd.stdout(std::process::Stdio::piped());
            let log_dir = ctx.session_dir.join("agent_logs");
            let _ = std::fs::create_dir_all(&log_dir);
            let log_file = std::fs::File::create(log_dir.join(format!("{agent_id}.log")));
            match log_file {
                Ok(f) => {
                    cmd.stderr(f);
                }
                Err(_) => {
                    cmd.stderr(std::process::Stdio::null());
                }
            }

            match cmd.spawn() {
                Ok(mut child) => {
                    let pid = child.id();

                    let _ = std::fs::rename(
                        log_dir.join(format!("{agent_id}.log")),
                        log_dir.join(format!("{pid}.log")),
                    );

                    let _ = crate::registry::register(&crate::registry::RegistryEntry {
                        agent_id: agent_id.clone(),
                        pid,
                        parent_pid: Some(self.my_pid),
                        git_root: Some(self.scope.clone()),
                        git_branch: None,
                        cwd: self.scope.clone(),
                        status: crate::registry::AgentStatus::Working,
                        task_slug: None,
                        session_id: String::new(),
                        socket_path: String::new(),
                        depth: self.depth + 1,
                        started_at: String::new(),
                    });

                    if let Some(ref tx) = self.spawned_tx {
                        if let Some(stdout) = child.stdout.take() {
                            let _ = tx.send(super::SpawnedChild {
                                agent_id: agent_id.clone(),
                                pid,
                                stdout,
                                prompt: prompt.clone(),
                                blocking,
                            });
                        }
                    }

                    // Drop the child handle. Rust's Child::drop closes pipes
                    // but does NOT kill the process — the subagent continues
                    // running independently.
                    drop(child);

                    if blocking {
                        self.wait_for_agent(&agent_id, ctx).await
                    } else {
                        ToolResult::ok(format!("agent {agent_id} is now working in the background"))
                            .with_metadata(serde_json::json!({
                                "agent_id": agent_id,
                                "blocking": false,
                            }))
                    }
                }
                Err(e) => ToolResult::err(format!("failed to spawn subagent: {e}")),
            }
        })
    }
}

impl SpawnAgentTool {
    /// Block until the named agent sends a message back via the socket.
    async fn wait_for_agent(&self, agent_id: &str, ctx: &ToolContext) -> ToolResult {
        let Some(ref tx) = self.agent_msg_tx else {
            return ToolResult::err("blocking spawn not available (no message channel)");
        };
        let mut rx = tx.subscribe();
        let deadline = tokio::time::Instant::now() + BLOCKING_TIMEOUT;
        let mut check_interval = tokio::time::interval(CHILD_CHECK_INTERVAL);
        check_interval.tick().await; // consume immediate tick

        loop {
            tokio::select! {
                result = rx.recv() => {
                    match result {
                        Ok(notif) if notif.from_id == agent_id => {
                            return ToolResult::ok(format!("agent {} finished:\n{}", notif.from_id, notif.message))
                                .with_metadata(serde_json::json!({
                                    "agent_id": agent_id,
                                    "blocking": true,
                                }));
                        }
                        Ok(_) => continue, // message from a different agent
                        Err(RecvError::Lagged(_)) => continue,
                        Err(RecvError::Closed) => {
                            return ToolResult::err(format!("agent {agent_id}: message channel closed"));
                        }
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    return ToolResult::err(format!("agent {agent_id}: timed out after {}s", BLOCKING_TIMEOUT.as_secs()));
                }
                _ = check_interval.tick() => {
                    let alive = crate::registry::children_of(self.my_pid)
                        .iter()
                        .any(|e| e.agent_id == agent_id && crate::registry::is_pid_alive(e.pid));
                    if !alive {
                        return ToolResult::err(format!("agent {agent_id} exited without sending a result"));
                    }
                }
                _ = ctx.cancel.cancelled() => {
                    return ToolResult::err("cancelled");
                }
            }
        }
    }
}
