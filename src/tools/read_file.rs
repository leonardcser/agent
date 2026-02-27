use super::{hash_content, int_arg, str_arg, FileHashes, Tool, ToolResult};
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
            if l > 0 { l } else { 2000 }
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
                let truncated = if line.len() > 2000 { &line[..2000] } else { line };
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
