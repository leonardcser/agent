use super::{str_arg, Tool, ToolResult};
use serde_json::Value;
use std::collections::HashMap;

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
