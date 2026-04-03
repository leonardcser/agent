use super::{
    display_path, hash_content, int_arg, notebook, str_arg, FileHashes, Tool, ToolContext,
    ToolFuture, ToolResult,
};
use crate::image;
use serde_json::Value;
use std::collections::HashMap;

pub struct ReadFileTool {
    pub hashes: FileHashes,
}

impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Reads a file from the local filesystem. Supports text files and image files (png, jpg, gif, webp, bmp, tiff, svg)."
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

    fn needs_confirm(&self, args: &HashMap<String, Value>) -> Option<String> {
        Some(display_path(&str_arg(args, "file_path")))
    }

    fn execute<'a>(
        &'a self,
        args: HashMap<String, Value>,
        _ctx: &'a ToolContext<'a>,
    ) -> ToolFuture<'a> {
        Box::pin(async move { tokio::task::block_in_place(|| self.run(&args)) })
    }
}

impl ReadFileTool {
    fn run(&self, args: &HashMap<String, Value>) -> ToolResult {
        let path = str_arg(args, "file_path");

        if image::is_image_file(&path) {
            return match image::read_image_as_data_url(&path) {
                Ok(data_url) => ToolResult::ok(format!("![image]({data_url})")),
                Err(e) => ToolResult::err(e),
            };
        }

        if notebook::is_notebook(&path) {
            // Store hash for staleness detection before delegating
            if let Ok(raw) = std::fs::read_to_string(&path) {
                if let Ok(mut map) = self.hashes.lock() {
                    map.insert(path.clone(), hash_content(&raw));
                }
            }
            let offset = int_arg(args, "offset").max(1);
            let limit = {
                let l = int_arg(args, "limit");
                if l > 0 {
                    l
                } else {
                    2000
                }
            };
            return notebook::read_notebook(&path, offset, limit);
        }

        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => return ToolResult::err(e.to_string()),
        };

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
            return ToolResult::ok("offset beyond end of file");
        }

        let end = (start + limit).min(lines.len());
        let result: String = lines[start..end]
            .iter()
            .enumerate()
            .map(|(i, line)| {
                let truncated = if line.len() > 2000 {
                    &line[..line.floor_char_boundary(2000)]
                } else {
                    line
                };
                format!("{:4}\t{}", start + i + 1, truncated)
            })
            .collect::<Vec<_>>()
            .join("\n");

        ToolResult::ok(result)
    }
}
