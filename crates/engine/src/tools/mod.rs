mod ask_user_question;
pub(crate) mod background;
mod bash;
mod bash_background;
mod edit_file;
mod exit_plan_mode;
mod glob;
mod grep;
mod notebook;
mod read_file;
mod web_cache;
mod web_fetch;
mod web_search;
mod web_shared;
mod write_file;

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

pub use ask_user_question::AskUserQuestionTool;
pub use background::{ProcessInfo, ProcessRegistry};
pub use bash::BashTool;
pub use bash_background::{format_read_result, ReadProcessOutputTool, StopProcessTool};
pub use edit_file::EditFileTool;
pub use exit_plan_mode::ExitPlanModeTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use notebook::NotebookEditTool;
pub use read_file::ReadFileTool;
pub use web_fetch::WebFetchTool;
pub use web_search::WebSearchTool;
pub use write_file::WriteFileTool;

pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}

/// Context provided to tools during execution, giving them access to
/// engine facilities (event streaming, cancellation, background processes,
/// and the LLM provider for tools that need secondary LLM calls).
pub struct ToolContext<'a> {
    pub event_tx: &'a mpsc::UnboundedSender<EngineEvent>,
    pub call_id: &'a str,
    pub cancel: &'a CancellationToken,
    pub processes: &'a ProcessRegistry,
    pub proc_done_tx: &'a mpsc::UnboundedSender<(String, Option<i32>)>,
    pub provider: &'a Provider,
    pub model: &'a str,
    pub session_id: &'a str,
    pub session_dir: &'a std::path::Path,
}

pub type ToolFuture<'a> = Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>>;

pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> Value;
    fn execute<'a>(
        &'a self,
        args: HashMap<String, Value>,
        ctx: &'a ToolContext<'a>,
    ) -> ToolFuture<'a>;
    fn needs_confirm(&self, _args: &HashMap<String, Value>) -> Option<String> {
        None
    }

    /// Returns glob patterns for session-level "always allow" approval.
    /// Each pattern is matched independently against individual sub-commands.
    fn approval_patterns(&self, _args: &HashMap<String, Value>) -> Vec<String> {
        vec![]
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

    pub fn definitions(&self, permissions: &Permissions, mode: Mode) -> Vec<ToolDefinition> {
        self.tools
            .iter()
            .filter(|t| permissions.check_tool(mode, t.name()) != Decision::Deny)
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
        "bash" => str_arg(args, "command")
            .lines()
            .next()
            .unwrap_or("")
            .to_string(),
        "read_file" | "write_file" | "edit_file" => display_path(&str_arg(args, "file_path")),
        "notebook_edit" => display_path(&str_arg(args, "notebook_path")),
        "glob" => str_arg(args, "pattern"),
        "grep" => {
            let pattern = str_arg(args, "pattern");
            let path = str_arg(args, "path");
            if path.is_empty() {
                pattern
            } else {
                format!("{} in {}", pattern, display_path(&path))
            }
        }
        "web_fetch" => str_arg(args, "url"),
        "web_search" => str_arg(args, "query"),
        "exit_plan_mode" => "plan ready".to_string(),
        "read_process_output" | "stop_process" => str_arg(args, "id"),
        "ask_user_question" => {
            let count = args
                .get("questions")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            format!("{} question{}", count, if count == 1 { "" } else { "s" })
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
                };
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return ToolResult {
                        content: format!("timed out after {:.0}s", timeout.as_secs_f64()),
                        is_error: true,
                    };
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                return ToolResult {
                    content: e.to_string(),
                    is_error: true,
                };
            }
        }
    }
}

/// Computes a simple hash of file contents for staleness detection.
pub(crate) fn hash_content(content: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut hasher);
    hasher.finish()
}

/// Shared map of file_path -> content hash, updated on read and edit.
pub type FileHashes = Arc<Mutex<HashMap<String, u64>>>;

pub fn new_file_hashes() -> FileHashes {
    Arc::new(Mutex::new(HashMap::new()))
}

pub fn build_tools(processes: ProcessRegistry) -> ToolRegistry {
    let hashes = new_file_hashes();
    let mut r = ToolRegistry::new();
    r.register(Box::new(ReadFileTool {
        hashes: hashes.clone(),
    }));
    r.register(Box::new(WriteFileTool {
        hashes: hashes.clone(),
    }));
    r.register(Box::new(EditFileTool {
        hashes: hashes.clone(),
    }));
    r.register(Box::new(BashTool));
    r.register(Box::new(GlobTool));
    r.register(Box::new(GrepTool));
    r.register(Box::new(ExitPlanModeTool));
    r.register(Box::new(AskUserQuestionTool));
    r.register(Box::new(WebFetchTool));
    r.register(Box::new(WebSearchTool));
    r.register(Box::new(NotebookEditTool {
        hashes: hashes.clone(),
    }));
    r.register(Box::new(ReadProcessOutputTool {
        registry: processes.clone(),
    }));
    r.register(Box::new(StopProcessTool {
        registry: processes,
    }));
    r
}
