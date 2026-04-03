use super::{display_path, str_arg, Tool, ToolContext, ToolFuture, ToolResult};
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

    fn needs_confirm(&self, args: &HashMap<String, Value>) -> Option<String> {
        let pattern = str_arg(args, "pattern");
        let path = str_arg(args, "path");
        if path.is_empty() {
            Some(pattern)
        } else {
            Some(format!("{} in {}", pattern, display_path(&path)))
        }
    }

    fn execute<'a>(
        &'a self,
        args: HashMap<String, Value>,
        _ctx: &'a ToolContext<'a>,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            tokio::task::block_in_place(|| {
                let pattern = str_arg(&args, "pattern");
                let root = str_arg(&args, "path");

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

                        entries.sort_by(|a, b| b.0.cmp(&a.0));
                        let matches: Vec<String> =
                            entries.into_iter().map(|(_, path)| path).collect();

                        if matches.is_empty() {
                            ToolResult::ok("no matches found")
                        } else {
                            ToolResult::ok(matches.join("\n"))
                        }
                    }
                    Err(e) => ToolResult::err(e.to_string()),
                }
            })
        })
    }
}
