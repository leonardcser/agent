pub(crate) mod background;
mod bash;
mod edit_file;

mod file_state;
mod glob;
mod grep;
mod list_agents;
mod load_skill;
mod message_agent;
mod notebook;
mod peek_agent;
mod read_file;
pub mod result_dedup;
mod spawn_agent;
mod stop_agent;
pub(crate) mod web_cache;
mod web_fetch;
mod web_search;
mod web_shared;
mod write_file;

pub use file_state::{file_mtime_ms, normalize_path, staleness_error, FileState, FileStateCache};

use crate::cancel::CancellationToken;
use crate::permissions::{Decision, Permissions};
use crate::provider::{FunctionSchema, Provider, ToolDefinition};
use protocol::{EngineEvent, Mode};
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

/// Kill the entire process group spawned by a child.
/// The child must have been spawned with `.process_group(0)` so it leads its
/// own group. We send SIGKILL to the negative PID (i.e. the group).
pub(crate) fn kill_process_group(child: &tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // SAFETY: pid is a valid process group ID (we set process_group(0) at spawn).
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child;
    }
}

pub use background::{ProcessInfo, ProcessRegistry};
pub use bash::{check_interactive, check_shell_background_operator, BashTool};
pub use edit_file::EditFileTool;

pub use glob::GlobTool;
pub use grep::GrepTool;
pub use notebook::{NotebookEditTool, NotebookRenderData};
pub use read_file::ReadFileTool;
pub use spawn_agent::AgentMessageNotification;
pub use web_fetch::WebFetchTool;
pub use web_search::WebSearchTool;
pub use write_file::WriteFileTool;

pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
    /// Structured metadata passed through to ToolOutcome for machine-readable data.
    pub metadata: Option<serde_json::Value>,
}

impl ToolResult {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            metadata: None,
        }
    }

    pub fn err(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            metadata: None,
        }
    }

    pub fn with_metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = Some(metadata);
        self
    }
}

/// Context provided to tools during execution, giving them access to
/// engine facilities (event streaming, cancellation, background processes,
/// and the LLM provider for tools that need secondary LLM calls).
///
/// All fields are owned (Arc-backed where shared) so a fresh context can be
/// constructed per call without lifetime gymnastics — this enables side calls
/// like `smelt.tools.call("bash", args)` from Lua plugin tools.
pub struct ToolContext {
    pub event_tx: mpsc::UnboundedSender<EngineEvent>,
    pub call_id: String,
    pub cancel: CancellationToken,
    pub processes: ProcessRegistry,
    pub proc_done_tx: mpsc::UnboundedSender<(String, Option<i32>)>,
    pub provider: Provider,
    pub model: String,
    pub session_id: String,
    pub session_dir: std::path::PathBuf,
    pub file_locks: FileLocks,
    pub api: crate::ApiConfig,
}

pub type ToolFuture<'a> = Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>>;

pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> Value;
    fn execute<'a>(&'a self, args: HashMap<String, Value>, ctx: &'a ToolContext) -> ToolFuture<'a>;
    fn needs_confirm(&self, _args: &HashMap<String, Value>) -> Option<String> {
        None
    }

    /// Returns glob patterns for session-level "always allow" approval.
    /// Each pattern is matched independently against individual sub-commands.
    fn approval_patterns(&self, _args: &HashMap<String, Value>) -> Vec<String> {
        vec![]
    }

    /// Whether this tool requires a human in the loop.
    fn interactive_only(&self) -> bool {
        false
    }

    /// Which modes this tool is available in. None means all modes.
    fn modes(&self) -> Option<&[Mode]> {
        None
    }

    /// Whether this tool is an MCP tool (uses `mcp` permission ruleset).
    fn is_mcp(&self) -> bool {
        false
    }

    /// Pre-flight validation run before showing the permission dialog.
    /// Return `Some(error)` to skip the dialog and fail the tool immediately.
    fn preflight(&self, _args: &HashMap<String, Value>) -> Option<String> {
        None
    }

    /// Optional decision override, consulted before the config rule-set.
    /// Used by tools with dynamic scope (e.g. `edit_file` auto-allowed on
    /// plan files in Plan mode). Returning `None` defers to the rules.
    fn decide_override(
        &self,
        _args: &HashMap<String, Value>,
        _mode: Mode,
        _session_dir: &std::path::Path,
    ) -> Option<Decision> {
        None
    }
}

#[derive(Default)]
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(tool);
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|t| t.name() == name)
            .map(|t| t.as_ref())
    }

    pub fn definitions(
        &self,
        permissions: &Permissions,
        mode: Mode,
        interactive: bool,
    ) -> Vec<ToolDefinition> {
        self.tools
            .iter()
            .filter(|t| {
                if t.interactive_only() && !interactive {
                    return false;
                }
                if let Some(modes) = t.modes() {
                    if !modes.contains(&mode) {
                        return false;
                    }
                }
                if t.is_mcp() {
                    permissions.check_mcp(mode, t.name()) != Decision::Deny
                } else {
                    permissions.check_tool(mode, t.name()) != Decision::Deny
                }
            })
            .map(|t| {
                ToolDefinition::new(FunctionSchema {
                    name: t.name().into(),
                    description: t.description().into(),
                    parameters: t.parameters(),
                })
            })
            .collect()
    }
}

pub fn str_arg(args: &HashMap<String, Value>, key: &str) -> String {
    args.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

pub fn tool_arg_summary(tool_name: &str, args: &HashMap<String, Value>) -> String {
    match tool_name {
        "bash" => str_arg(args, "command"),
        "read_file" | "write_file" | "edit_file" => display_path(&str_arg(args, "file_path")),
        "edit_notebook" => display_path(&str_arg(args, "notebook_path")),
        "glob" | "grep" => {
            confirm_with_optional_path(str_arg(args, "pattern"), &str_arg(args, "path"))
                .unwrap_or_default()
        }
        "web_fetch" => str_arg(args, "url"),
        "web_search" => str_arg(args, "query"),
        "exit_plan_mode" => "plan ready".into(),
        "read_process_output" | "stop_process" => str_arg(args, "id"),
        "ask_user_question" => {
            let count = args
                .get("questions")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            format!("{} question{}", count, if count == 1 { "" } else { "s" })
        }
        "spawn_agent" => {
            let prompt = str_arg(args, "prompt");
            prompt.lines().next().unwrap_or("").trim().to_string()
        }
        "message_agent" => {
            let targets: Vec<String> = args
                .get("targets")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let msg = str_arg(args, "message");
            let first_line = msg.lines().next().unwrap_or("").trim().to_string();
            format!("{} {first_line}", targets.join(", "))
        }
        "stop_agent" => str_arg(args, "target"),
        "load_skill" => str_arg(args, "name"),
        "list_agents" => String::new(),
        "peek_agent" => {
            let target = str_arg(args, "target");
            let question = str_arg(args, "question");
            format!("{target} {question}")
        }
        _ => String::new(),
    }
}

/// Convert an absolute path to a relative one if it's inside the cwd.
pub fn display_path(path: &str) -> String {
    if let Ok(cwd) = std::env::current_dir() {
        let prefix = cwd.to_string_lossy();
        if let Some(rest) = path.strip_prefix(prefix.as_ref()) {
            let rest = rest.strip_prefix('/').unwrap_or(rest);
            if rest.is_empty() {
                return ".".into();
            }
            return rest.into();
        }
    }
    path.into()
}

/// Build a confirm label like `"pattern"` or `"pattern in dir"`, omitting the
/// path when it is the cwd.
pub fn confirm_with_optional_path(label: String, path: &str) -> Option<String> {
    if path.is_empty() || path == "." {
        Some(label)
    } else {
        Some(format!("{} in {}", label, display_path(path)))
    }
}

/// Maximum lines of tool output sent to the LLM. Individual tools may
/// enforce their own (often larger) limits before this; this is the final
/// trim applied when building the API request.
pub const MAX_TOOL_OUTPUT_LINES: usize = 2000;

/// Trim tool output to `max_lines` for LLM context. Appends a note with
/// the total line count when truncated.
pub fn trim_tool_output(content: &str, max_lines: usize) -> String {
    if content == "no matches found" {
        return content.to_string();
    }
    let total = content.lines().count();
    if total <= max_lines {
        return content.to_string();
    }
    let mut out: String = content
        .lines()
        .take(max_lines)
        .collect::<Vec<_>>()
        .join("\n");
    out.push_str(&format!("\n... (trimmed, {} lines total)", total));
    out
}

pub(crate) fn int_arg(args: &HashMap<String, Value>, key: &str) -> usize {
    args.get(key).and_then(|v| v.as_u64()).unwrap_or(0) as usize
}

pub(crate) fn bool_arg(args: &HashMap<String, Value>, key: &str) -> bool {
    args.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}

const MAX_TIMEOUT_MS: u64 = 600_000;

pub fn timeout_arg(args: &HashMap<String, Value>, default_secs: u64) -> Duration {
    let ms = args
        .get("timeout_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(default_secs * 1000)
        .min(MAX_TIMEOUT_MS);
    Duration::from_millis(ms)
}

pub(crate) fn run_command_with_timeout(
    mut child: std::process::Child,
    timeout: Duration,
) -> ToolResult {
    // Drain stdout/stderr in background threads to avoid pipe buffer deadlocks.
    // If the child produces more output than the OS pipe buffer (~64KB on macOS),
    // it will block on write and never exit unless we actively read.
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let stdout_handle = std::thread::spawn(move || {
        stdout.map(|mut r| {
            let mut buf = Vec::new();
            std::io::Read::read_to_end(&mut r, &mut buf).ok();
            buf
        })
    });
    let stderr_handle = std::thread::spawn(move || {
        stderr.map(|mut r| {
            let mut buf = Vec::new();
            std::io::Read::read_to_end(&mut r, &mut buf).ok();
            buf
        })
    });

    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stdout_bytes = stdout_handle.join().ok().flatten().unwrap_or_default();
                let stderr_bytes = stderr_handle.join().ok().flatten().unwrap_or_default();
                let mut result = String::from_utf8_lossy(&stdout_bytes).into_owned();
                let stderr_str = String::from_utf8_lossy(&stderr_bytes);
                if !stderr_str.is_empty() {
                    if !result.is_empty() {
                        result.push('\n');
                    }
                    result.push_str(&stderr_str);
                }
                return ToolResult {
                    content: result,
                    is_error: !status.success(),
                    metadata: None,
                };
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return ToolResult::err(format!(
                        "timed out after {:.0}s",
                        timeout.as_secs_f64()
                    ));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                return ToolResult::err(e.to_string());
            }
        }
    }
}

/// Acquire an exclusive, non-blocking advisory lock on the given file path.
/// Returns `Ok(guard)` on success. Returns `Err(message)` if the file is
/// locked by another process (EWOULDBLOCK) or on any other I/O error.
/// The lock is released when the guard is dropped.
#[cfg(unix)]
pub(crate) fn try_flock(path: &str) -> Result<FlockGuard, String> {
    use std::os::unix::io::AsRawFd;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    let fd = file.as_raw_fd();
    let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::WouldBlock {
            return Err("File is currently being edited by another agent, try again later.".into());
        }
        return Err(format!("flock error: {err}"));
    }
    Ok(FlockGuard { _file: file })
}

#[cfg(not(unix))]
pub(crate) fn try_flock(_path: &str) -> Result<FlockGuard, String> {
    Ok(FlockGuard { _file: None })
}

pub(crate) struct FlockGuard {
    #[cfg(unix)]
    _file: std::fs::File,
    #[cfg(not(unix))]
    _file: Option<()>,
}

/// Per-path locks that serialize concurrent file-mutating operations.
/// Concurrent tool calls (edit_file, write_file, edit_notebook) targeting
/// the same file will execute sequentially, while different files remain
/// parallel. Entries are pruned when no one else holds a reference.
#[derive(Clone, Default)]
pub struct FileLocks(Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>);

impl FileLocks {
    pub async fn lock(&self, path: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let mutex = {
            let mut map = self.0.lock().unwrap();
            // Prune idle entries (strong_count == 1 means only the map holds it).
            if map.len() > 32 {
                map.retain(|_, v| Arc::strong_count(v) > 1);
            }
            map.entry(path.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        mutex.lock_owned().await
    }
}

/// A handle to a spawned child process, carrying the piped stdout.
pub struct SpawnedChild {
    pub agent_id: String,
    pub pid: u32,
    pub stdout: std::process::ChildStdout,
    /// The prompt given to the subagent (displayed as the initial user message).
    pub prompt: String,
    /// Whether the parent is waiting for this agent to finish (blocking spawn).
    pub blocking: bool,
}

/// Configuration for multi-agent tool registration.
pub struct MultiAgentToolConfig {
    pub scope: String,
    pub pid: u32,
    pub agent_id: String,
    pub depth: u8,
    pub max_depth: u8,
    pub max_agents: u8,
    pub parent_pid: Option<u32>,
    /// Shared mutable slug — updated by title generation, read by message_agent.
    pub slug: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    /// API config for spawned subagents.
    pub api_base: String,
    pub api_key_env: String,
    pub model: String,
    pub provider_type: String,
    /// Broadcast channel for agent message notifications (used by blocking spawn).
    pub agent_msg_tx: Option<tokio::sync::broadcast::Sender<AgentMessageNotification>>,
    /// Channel for sending spawned child handles (stdout pipes) to the parent.
    pub spawned_tx: Option<mpsc::UnboundedSender<SpawnedChild>>,
}

pub fn build_tools(
    _processes: ProcessRegistry,
    ma: Option<MultiAgentToolConfig>,
    skills: Option<std::sync::Arc<crate::skills::SkillLoader>>,
) -> ToolRegistry {
    let files = FileStateCache::new();
    let mut r = ToolRegistry::new();
    r.register(Box::new(ReadFileTool {
        files: files.clone(),
    }));
    r.register(Box::new(WriteFileTool {
        files: files.clone(),
    }));
    r.register(Box::new(EditFileTool {
        files: files.clone(),
    }));
    r.register(Box::new(BashTool));
    r.register(Box::new(GlobTool));
    r.register(Box::new(GrepTool));
    r.register(Box::new(WebFetchTool));
    r.register(Box::new(WebSearchTool));
    r.register(Box::new(NotebookEditTool {
        files: files.clone(),
    }));

    // Skill loader tool (conditionally registered).
    if let Some(loader) = skills {
        r.register(Box::new(load_skill::LoadSkillTool { loader }));
    }

    // Multi-agent tools (conditionally registered).
    if let Some(ma) = ma {
        r.register(Box::new(list_agents::ListAgentsTool {
            scope: ma.scope.clone(),
            my_pid: ma.pid,
        }));
        r.register(Box::new(message_agent::MessageAgentTool {
            my_id: ma.agent_id.clone(),
            my_slug: ma.slug,
        }));
        r.register(Box::new(peek_agent::PeekAgentTool {
            my_id: ma.agent_id.clone(),
        }));
        if ma.depth < ma.max_depth {
            r.register(Box::new(spawn_agent::SpawnAgentTool {
                scope: ma.scope.clone(),
                my_pid: ma.pid,
                depth: ma.depth,
                max_agents: ma.max_agents,
                api_base: ma.api_base.clone(),
                api_key_env: ma.api_key_env.clone(),
                model: ma.model.clone(),
                provider_type: ma.provider_type.clone(),
                spawned_tx: ma.spawned_tx.clone(),
                agent_msg_tx: ma.agent_msg_tx.clone(),
            }));
        }
        // stop_agent: any agent can stop its children.
        r.register(Box::new(stop_agent::StopAgentTool { my_pid: ma.pid }));
    }

    r
}
