mod ask_user_question;
mod bash;
mod edit_file;
mod exit_plan_mode;
mod glob;
mod grep;
mod read_file;
mod web_cache;
mod web_fetch;
mod web_search;
mod web_shared;
mod write_file;

use crate::input::Mode;
use crate::permissions::{Decision, Permissions};
use crate::provider::{FunctionSchema, ToolDefinition};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub use ask_user_question::AskUserQuestionTool;
pub use bash::BashTool;
pub use edit_file::EditFileTool;
pub use exit_plan_mode::ExitPlanModeTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use read_file::ReadFileTool;
pub use web_fetch::WebFetchTool;
pub use web_search::WebSearchTool;
pub use write_file::WriteFileTool;

pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}

pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> Value;
    fn execute(&self, args: &HashMap<String, Value>) -> ToolResult;
    fn needs_confirm(&self, _args: &HashMap<String, Value>) -> Option<String> {
        None
    }

    /// Returns a glob pattern for session-level "always allow" approval.
    /// For web tools this is a domain pattern like "*.github.com".
    fn approval_pattern(&self, _args: &HashMap<String, Value>) -> Option<String> {
        None
    }
}

pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self { tools: vec![] }
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

pub(crate) fn int_arg(args: &HashMap<String, Value>, key: &str) -> usize {
    args.get(key).and_then(|v| v.as_u64()).unwrap_or(0) as usize
}

pub(crate) fn bool_arg(args: &HashMap<String, Value>, key: &str) -> bool {
    args.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}

pub fn timeout_arg(args: &HashMap<String, Value>, default_secs: u64) -> Duration {
    let ms = args
        .get("timeout_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(default_secs * 1000);
    Duration::from_millis(ms)
}

pub(crate) fn run_command_with_timeout(
    mut child: std::process::Child,
    timeout: Duration,
) -> ToolResult {
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let out = child
                    .wait_with_output()
                    .unwrap_or_else(|e| std::process::Output {
                        status,
                        stdout: Vec::new(),
                        stderr: e.to_string().into_bytes(),
                    });
                let mut result = String::from_utf8_lossy(&out.stdout).to_string();
                let stderr = String::from_utf8_lossy(&out.stderr);
                if !stderr.is_empty() {
                    if !result.is_empty() {
                        result.push('\n');
                    }
                    result.push_str(&stderr);
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

pub fn build_tools() -> ToolRegistry {
    let hashes = new_file_hashes();
    let mut r = ToolRegistry::new();
    r.register(Box::new(ReadFileTool {
        hashes: hashes.clone(),
    }));
    r.register(Box::new(WriteFileTool));
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
    r
}
