use super::{str_arg, Tool, ToolResult};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

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
