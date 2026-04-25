use super::{confirm_with_optional_path, str_arg, Tool, ToolContext, ToolFuture, ToolResult};
use globset::Glob;
use ignore::WalkBuilder;
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
        confirm_with_optional_path(str_arg(args, "pattern"), &str_arg(args, "path"))
    }

    fn execute<'a>(
        &'a self,
        args: HashMap<String, Value>,
        _ctx: &'a ToolContext,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            tokio::task::block_in_place(|| {
                let pattern = str_arg(&args, "pattern");
                let root = str_arg(&args, "path");
                let search_dir = if root.is_empty() { "." } else { root.as_str() };

                let matcher = match Glob::new(&pattern) {
                    Ok(g) => g.compile_matcher(),
                    Err(e) => return ToolResult::err(format!("invalid glob pattern: {}", e)),
                };

                let walker = WalkBuilder::new(search_dir)
                    .hidden(false)
                    .git_ignore(true)
                    .build();

                let mut entries: Vec<(std::time::SystemTime, String)> = Vec::new();

                for entry in walker {
                    let entry = match entry {
                        Ok(e) => e,
                        Err(_) => continue,
                    };

                    if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                        continue;
                    }

                    let path = entry.path();
                    let relative = path.strip_prefix(search_dir).unwrap_or(path);

                    if !matcher.is_match(relative) {
                        continue;
                    }

                    if let Ok(meta) = entry.metadata() {
                        if let Ok(mtime) = meta.modified() {
                            entries.push((mtime, path.display().to_string()));
                        }
                    }

                    if entries.len() >= 200 {
                        break;
                    }
                }

                entries.sort_by_key(|e| std::cmp::Reverse(e.0));
                let matches: Vec<String> = entries.into_iter().map(|(_, p)| p).collect();

                if matches.is_empty() {
                    ToolResult::ok("no matches found")
                } else {
                    ToolResult::ok(matches.join("\n"))
                }
            })
        })
    }
}
