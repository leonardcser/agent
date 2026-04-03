use super::{
    display_path, hash_content, str_arg, FileHashes, Tool, ToolContext, ToolFuture, ToolResult,
};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

pub struct WriteFileTool {
    pub hashes: FileHashes,
}

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
        Some(display_path(&str_arg(args, "file_path")))
    }

    fn preflight(&self, args: &HashMap<String, Value>) -> Option<String> {
        let path = str_arg(args, "file_path");
        if Path::new(&path).exists() {
            let has_hash = self.hashes.lock().is_ok_and(|map| map.contains_key(&path));
            if !has_hash {
                return Some("File already exists. Use edit_file to modify existing files, or read_file then write_file to replace.".into());
            }
        }
        None
    }

    fn execute<'a>(
        &'a self,
        args: HashMap<String, Value>,
        ctx: &'a ToolContext<'a>,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            let path = str_arg(&args, "file_path");
            let _guard = ctx.file_locks.lock(&path).await;
            tokio::task::block_in_place(|| self.run(&args))
        })
    }
}

impl WriteFileTool {
    fn run(&self, args: &HashMap<String, Value>) -> ToolResult {
        let path = str_arg(args, "file_path");
        let content = str_arg(args, "content");

        // Acquire cross-process advisory lock if the file exists.
        let _flock = if Path::new(&path).exists() {
            let has_hash = self.hashes.lock().is_ok_and(|map| map.contains_key(&path));
            if !has_hash {
                return ToolResult::err("File already exists. Use edit_file to modify existing files, or read_file then write_file to replace.");
            }
            match super::try_flock(&path) {
                Ok(guard) => Some(guard),
                Err(e) => return ToolResult::err(e),
            }
        } else {
            None
        };

        if let Some(parent) = Path::new(&path).parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return ToolResult::err(e.to_string());
            }
        }

        match std::fs::write(&path, &content) {
            Ok(_) => {
                if let Ok(mut map) = self.hashes.lock() {
                    map.insert(path.clone(), hash_content(&content));
                }
                ToolResult::ok(format!(
                    "wrote {} bytes to {}",
                    content.len(),
                    display_path(&path)
                ))
            }
            Err(e) => ToolResult::err(e.to_string()),
        }
    }
}
