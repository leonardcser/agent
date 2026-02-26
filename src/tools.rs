use crate::input::Mode;
use crate::permissions::{Decision, Permissions};
use crate::provider::{FunctionSchema, ToolDefinition};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

fn int_arg(args: &HashMap<String, Value>, key: &str) -> usize {
    args.get(key).and_then(|v| v.as_u64()).unwrap_or(0) as usize
}

fn bool_arg(args: &HashMap<String, Value>, key: &str) -> bool {
    args.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}

pub fn timeout_arg(args: &HashMap<String, Value>, default_secs: u64) -> Duration {
    let ms = args
        .get("timeout_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(default_secs * 1000);
    Duration::from_millis(ms)
}

fn run_command_with_timeout(mut child: std::process::Child, timeout: Duration) -> ToolResult {
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
fn hash_content(content: &str) -> u64 {
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

// --- read_file ---

pub struct ReadFileTool {
    pub hashes: FileHashes,
}

impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Reads a file from the local filesystem. You can access any file directly by using this tool."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "The absolute path to the file to read"
                },
                "offset": {
                    "type": "integer",
                    "description": "The line number to start reading from (1-based). Only provide if the file is too large to read at once."
                },
                "limit": {
                    "type": "integer",
                    "description": "The number of lines to read. Only provide if the file is too large to read at once."
                }
            },
            "required": ["file_path"]
        })
    }

    fn execute(&self, args: &HashMap<String, Value>) -> ToolResult {
        let path = str_arg(args, "file_path");
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                return ToolResult {
                    content: e.to_string(),
                    is_error: true,
                }
            }
        };

        // Store hash for staleness detection
        if let Ok(mut map) = self.hashes.lock() {
            map.insert(path.clone(), hash_content(&content));
        }

        let lines: Vec<&str> = content.lines().collect();
        let offset = int_arg(args, "offset").max(1);
        let limit = {
            let l = int_arg(args, "limit");
            if l > 0 {
                l
            } else {
                2000
            }
        };

        let start = offset - 1;
        if start >= lines.len() {
            return ToolResult {
                content: "offset beyond end of file".into(),
                is_error: false,
            };
        }

        let end = (start + limit).min(lines.len());
        let result: String = lines[start..end]
            .iter()
            .enumerate()
            .map(|(i, line)| {
                let truncated = if line.len() > 2000 {
                    &line[..2000]
                } else {
                    line
                };
                format!("{:4}\t{}", start + i + 1, truncated)
            })
            .collect::<Vec<_>>()
            .join("\n");

        ToolResult {
            content: result,
            is_error: false,
        }
    }
}

// --- write_file ---

pub struct WriteFileTool;

impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Writes a file to the local filesystem. This tool will overwrite the existing file if there is one at the provided path."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "The absolute path to the file to write (must be absolute, not relative)"
                },
                "content": {
                    "type": "string",
                    "description": "The content to write to the file"
                }
            },
            "required": ["file_path", "content"]
        })
    }

    fn needs_confirm(&self, args: &HashMap<String, Value>) -> Option<String> {
        Some(str_arg(args, "file_path"))
    }

    fn execute(&self, args: &HashMap<String, Value>) -> ToolResult {
        let path = str_arg(args, "file_path");
        let content = str_arg(args, "content");

        if let Some(parent) = Path::new(&path).parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return ToolResult {
                    content: e.to_string(),
                    is_error: true,
                };
            }
        }

        match std::fs::write(&path, &content) {
            Ok(_) => ToolResult {
                content: format!("wrote {} bytes to {}", content.len(), path),
                is_error: false,
            },
            Err(e) => ToolResult {
                content: e.to_string(),
                is_error: true,
            },
        }
    }
}

// --- edit_file ---

pub struct EditFileTool {
    pub hashes: FileHashes,
}

impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "Performs exact string replacements in files. The old_string must be unique in the file unless replace_all is true."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "The absolute path to the file to modify"
                },
                "old_string": {
                    "type": "string",
                    "description": "The text to replace"
                },
                "new_string": {
                    "type": "string",
                    "description": "The text to replace it with (must be different from old_string)"
                },
                "replace_all": {
                    "type": "boolean",
                    "description": "Replace all occurrences of old_string (default false)"
                }
            },
            "required": ["file_path", "old_string", "new_string"]
        })
    }

    fn needs_confirm(&self, args: &HashMap<String, Value>) -> Option<String> {
        Some(str_arg(args, "file_path"))
    }

    fn execute(&self, args: &HashMap<String, Value>) -> ToolResult {
        let path = str_arg(args, "file_path");
        let old_string = str_arg(args, "old_string");
        let new_string = str_arg(args, "new_string");
        let replace_all = bool_arg(args, "replace_all");

        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                return ToolResult {
                    content: e.to_string(),
                    is_error: true,
                }
            }
        };

        // Check staleness: if we have a stored hash and it doesn't match, the file changed
        if let Ok(map) = self.hashes.lock() {
            if let Some(&stored_hash) = map.get(&path) {
                let current_hash = hash_content(&content);
                if stored_hash != current_hash {
                    return ToolResult {
                        content: "File has been modified since last read. You must use read_file to read the current contents before editing.".into(),
                        is_error: true,
                    };
                }
            }
        }

        if old_string == new_string {
            return ToolResult {
                content: "old_string and new_string are identical".into(),
                is_error: true,
            };
        }

        let count = content.matches(&old_string).count();
        if count == 0 {
            return ToolResult {
                content: "old_string not found in file".into(),
                is_error: true,
            };
        }
        if count > 1 && !replace_all {
            return ToolResult {
                content: format!(
                    "old_string found {} times — must be unique, or set replace_all to true",
                    count
                ),
                is_error: true,
            };
        }

        let new_content = if replace_all {
            content.replace(&old_string, &new_string)
        } else {
            content.replacen(&old_string, &new_string, 1)
        };

        match std::fs::write(&path, &new_content) {
            Ok(_) => {
                // Update the stored hash to the new content
                if let Ok(mut map) = self.hashes.lock() {
                    map.insert(path.clone(), hash_content(&new_content));
                }
                ToolResult {
                    content: format!("edited {}", path),
                    is_error: false,
                }
            }
            Err(e) => ToolResult {
                content: e.to_string(),
                is_error: true,
            },
        }
    }
}

// --- bash ---

pub struct BashTool;

impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        "Execute a shell command"
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Shell command to execute"},
                "timeout_ms": {"type": "integer", "description": "Timeout in milliseconds (default: 120000)"}
            },
            "required": ["command"]
        })
    }

    fn needs_confirm(&self, args: &HashMap<String, Value>) -> Option<String> {
        Some(str_arg(args, "command"))
    }

    fn execute(&self, args: &HashMap<String, Value>) -> ToolResult {
        let command = str_arg(args, "command");
        let timeout = timeout_arg(args, 120);

        let child = std::process::Command::new("sh")
            .arg("-c")
            .arg(&command)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();

        match child {
            Ok(child) => run_command_with_timeout(child, timeout),
            Err(e) => ToolResult {
                content: e.to_string(),
                is_error: true,
            },
        }
    }
}

// --- glob ---

pub struct GlobTool;

impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        "Fast file pattern matching tool that works with any codebase size. Returns matching file paths sorted by modification time."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "The glob pattern to match files against (supports **), e.g. **/*.rs"
                },
                "path": {
                    "type": "string",
                    "description": "The directory to search in. If not specified, the current working directory will be used."
                }
            },
            "required": ["pattern"]
        })
    }

    fn execute(&self, args: &HashMap<String, Value>) -> ToolResult {
        let pattern = str_arg(args, "pattern");
        let root = str_arg(args, "path");

        let full_pattern = if root.is_empty() {
            pattern
        } else {
            format!("{}/{}", root.trim_end_matches('/'), pattern)
        };

        match glob::glob(&full_pattern) {
            Ok(paths) => {
                let mut entries: Vec<(std::time::SystemTime, String)> = paths
                    .filter_map(|p| p.ok())
                    .take(200)
                    .filter_map(|p| {
                        let mtime = p.metadata().ok()?.modified().ok()?;
                        Some((mtime, p.display().to_string()))
                    })
                    .collect();

                // Sort by modification time, most recent first
                entries.sort_by(|a, b| b.0.cmp(&a.0));

                let matches: Vec<String> = entries.into_iter().map(|(_, path)| path).collect();

                if matches.is_empty() {
                    ToolResult {
                        content: "no matches found".into(),
                        is_error: false,
                    }
                } else {
                    ToolResult {
                        content: matches.join("\n"),
                        is_error: false,
                    }
                }
            }
            Err(e) => ToolResult {
                content: e.to_string(),
                is_error: true,
            },
        }
    }
}

// --- grep ---

pub struct GrepTool;

impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "A powerful search tool built on ripgrep. Supports full regex syntax, file type filtering, glob filtering, and multiple output modes."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "The regular expression pattern to search for in file contents"
                },
                "path": {
                    "type": "string",
                    "description": "File or directory to search in. Defaults to current working directory."
                },
                "glob": {
                    "type": "string",
                    "description": "Glob pattern to filter files (e.g. \"*.js\", \"*.{ts,tsx}\")"
                },
                "type": {
                    "type": "string",
                    "description": "File type to search (rg --type). Common types: js, py, rust, go, java."
                },
                "output_mode": {
                    "type": "string",
                    "enum": ["content", "files_with_matches", "count"],
                    "description": "Output mode: \"content\" shows matching lines, \"files_with_matches\" shows file paths (default), \"count\" shows match counts."
                },
                "case_insensitive": {
                    "type": "boolean",
                    "description": "Case insensitive search"
                },
                "line_numbers": {
                    "type": "boolean",
                    "description": "Show line numbers in output (default true). Only applies to output_mode \"content\"."
                },
                "after_context": {
                    "type": "integer",
                    "description": "Number of lines to show after each match. Only applies to output_mode \"content\"."
                },
                "before_context": {
                    "type": "integer",
                    "description": "Number of lines to show before each match. Only applies to output_mode \"content\"."
                },
                "context": {
                    "type": "integer",
                    "description": "Number of lines to show before and after each match. Only applies to output_mode \"content\"."
                },
                "multiline": {
                    "type": "boolean",
                    "description": "Enable multiline mode where . matches newlines and patterns can span lines."
                },
                "head_limit": {
                    "type": "integer",
                    "description": "Limit output to first N lines/entries. 0 means unlimited (default)."
                },
                "offset": {
                    "type": "integer",
                    "description": "Skip first N lines/entries before applying head_limit."
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Timeout in milliseconds (default: 30000)"
                }
            },
            "required": ["pattern"]
        })
    }

    fn execute(&self, args: &HashMap<String, Value>) -> ToolResult {
        let pattern = str_arg(args, "pattern");
        let path = str_arg(args, "path");
        let glob_filter = str_arg(args, "glob");
        let file_type = str_arg(args, "type");
        let output_mode = str_arg(args, "output_mode");
        let case_insensitive = bool_arg(args, "case_insensitive");
        let multiline = bool_arg(args, "multiline");
        let after_ctx = int_arg(args, "after_context");
        let before_ctx = int_arg(args, "before_context");
        let context = int_arg(args, "context");
        let line_numbers = args
            .get("line_numbers")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let timeout = timeout_arg(args, 30);

        let search_path = if path.is_empty() { ".".into() } else { path };

        let mut cmd_args: Vec<String> = Vec::new();

        // Output mode
        match output_mode.as_str() {
            "files_with_matches" | "" => cmd_args.push("--files-with-matches".into()),
            "count" => cmd_args.push("--count".into()),
            "content" => {
                if line_numbers {
                    cmd_args.push("--line-number".into());
                }
                if after_ctx > 0 {
                    cmd_args.push(format!("--after-context={}", after_ctx));
                }
                if before_ctx > 0 {
                    cmd_args.push(format!("--before-context={}", before_ctx));
                }
                if context > 0 {
                    cmd_args.push(format!("--context={}", context));
                }
            }
            _ => cmd_args.push("--files-with-matches".into()),
        }

        if case_insensitive {
            cmd_args.push("--ignore-case".into());
        }

        if multiline {
            cmd_args.push("--multiline".into());
            cmd_args.push("--multiline-dotall".into());
        }

        if !glob_filter.is_empty() {
            cmd_args.push(format!("--glob={}", glob_filter));
        }

        if !file_type.is_empty() {
            cmd_args.push(format!("--type={}", file_type));
        }

        cmd_args.push("--".into());
        cmd_args.push(pattern.clone());
        cmd_args.push(search_path.clone());

        // Try rg first, fall back to system grep
        let child = std::process::Command::new("rg")
            .args(&cmd_args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();

        match child {
            Ok(child) => {
                let result = run_command_with_timeout(child, timeout);
                if result.is_error {
                    // rg exits 1 for no matches — treat as not-error
                    if result.content.is_empty() {
                        return ToolResult {
                            content: "no matches found".into(),
                            is_error: false,
                        };
                    }
                    return result;
                }

                ToolResult {
                    content: result.content,
                    is_error: false,
                }
            }
            Err(_) => {
                // rg not found — fall back to system grep
                grep_fallback(
                    &pattern,
                    &search_path,
                    &glob_filter,
                    case_insensitive,
                    timeout,
                )
            }
        }
    }
}

fn grep_fallback(
    pattern: &str,
    search_path: &str,
    glob_filter: &str,
    case_insensitive: bool,
    timeout: Duration,
) -> ToolResult {
    let mut cmd_args = vec!["-rn".to_string(), "--max-count=200".to_string()];
    if case_insensitive {
        cmd_args.push("-i".into());
    }
    if !glob_filter.is_empty() {
        cmd_args.push(format!("--include={}", glob_filter));
    }
    cmd_args.push("--".into());
    cmd_args.push(pattern.to_string());
    cmd_args.push(search_path.to_string());

    let child = std::process::Command::new("grep")
        .args(&cmd_args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn();

    match child {
        Ok(child) => {
            let result = run_command_with_timeout(child, timeout);
            if !result.is_error && result.content.is_empty() {
                ToolResult {
                    content: "no matches found".into(),
                    is_error: false,
                }
            } else {
                ToolResult {
                    content: result.content,
                    is_error: result.is_error,
                }
            }
        }
        Err(e) => ToolResult {
            content: e.to_string(),
            is_error: true,
        },
    }
}

// --- Registry builders ---

pub fn normal_tools() -> ToolRegistry {
    let hashes = new_file_hashes();
    let mut r = ToolRegistry::new();
    r.register(Box::new(ReadFileTool {
        hashes: hashes.clone(),
    }));
    r.register(Box::new(BashTool));
    r.register(Box::new(GlobTool));
    r.register(Box::new(GrepTool));
    r
}

pub fn apply_tools() -> ToolRegistry {
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
    r
}
