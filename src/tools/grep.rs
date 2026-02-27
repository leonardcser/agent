use super::{bool_arg, int_arg, run_command_with_timeout, str_arg, timeout_arg, Tool, ToolResult};
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;

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
                    "description": "Output mode: \"content\" shows matching lines (default), \"files_with_matches\" shows file paths, \"count\" shows match counts."
                },
                "-i": {
                    "type": "boolean",
                    "description": "Case insensitive search (rg -i)"
                },
                "-n": {
                    "type": "boolean",
                    "description": "Show line numbers in output (rg -n). Requires output_mode: \"content\", ignored otherwise. Defaults to true."
                },
                "-A": {
                    "type": "integer",
                    "description": "Number of lines to show after each match (rg -A). Requires output_mode: \"content\", ignored otherwise."
                },
                "-B": {
                    "type": "integer",
                    "description": "Number of lines to show before each match (rg -B). Requires output_mode: \"content\", ignored otherwise."
                },
                "-C": {
                    "type": "integer",
                    "description": "Alias for context."
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
        let case_insensitive = bool_arg(args, "-i");
        let multiline = bool_arg(args, "multiline");
        let after_ctx = int_arg(args, "-A");
        let before_ctx = int_arg(args, "-B");
        let context = {
            let c = int_arg(args, "context");
            if c > 0 { c } else { int_arg(args, "-C") }
        };
        let head_limit = int_arg(args, "head_limit");
        let offset = int_arg(args, "offset");
        let line_numbers = args
            .get("-n")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let timeout = timeout_arg(args, 30);

        let search_path = if path.is_empty() { ".".into() } else { path };

        let mut cmd_args: Vec<String> = Vec::new();

        // Output mode
        match output_mode.as_str() {
            "files_with_matches" => cmd_args.push("--files-with-matches".into()),
            "count" => cmd_args.push("--count".into()),
            "content" | "" => {
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

                let content = if result.content.is_empty() {
                    "no matches found".into()
                } else {
                    apply_offset_and_limit(&result.content, offset, head_limit)
                };

                ToolResult {
                    content,
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
                    offset,
                    head_limit,
                )
            }
        }
    }
}

fn apply_offset_and_limit(content: &str, offset: usize, head_limit: usize) -> String {
    if offset == 0 && head_limit == 0 {
        return content.to_string();
    }

    let lines: Vec<&str> = content.lines().collect();
    let start = offset.min(lines.len());
    let end = if head_limit > 0 {
        (start + head_limit).min(lines.len())
    } else {
        lines.len()
    };

    lines[start..end].join("\n")
}

fn grep_fallback(
    pattern: &str,
    search_path: &str,
    glob_filter: &str,
    case_insensitive: bool,
    timeout: Duration,
    offset: usize,
    head_limit: usize,
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
                let content = apply_offset_and_limit(&result.content, offset, head_limit);
                ToolResult {
                    content,
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
