use super::{
    bool_arg, display_path, hash_content, str_arg, FileHashes, Tool, ToolContext, ToolFuture,
    ToolResult,
};
use serde_json::Value;
use std::collections::HashMap;

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
        Some(display_path(&str_arg(args, "file_path")))
    }

    fn preflight(&self, args: &HashMap<String, Value>) -> Option<String> {
        let path = str_arg(args, "file_path");
        if let Ok(map) = self.hashes.lock() {
            match map.get(&path) {
                None => {
                    return Some(
                        "You must use read_file before editing. Read the file first.".into(),
                    );
                }
                Some(&stored_hash) => {
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        if stored_hash != hash_content(&content) {
                            return Some("File has been modified since last read. You must use read_file to read the current contents before editing.".into());
                        }
                    }
                }
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

impl EditFileTool {
    fn run(&self, args: &HashMap<String, Value>) -> ToolResult {
        let path = str_arg(args, "file_path");
        let old_string = str_arg(args, "old_string");
        let new_string = str_arg(args, "new_string");
        let replace_all = bool_arg(args, "replace_all");

        // Acquire cross-process advisory lock (non-blocking).
        let _flock = match super::try_flock(&path) {
            Ok(guard) => Some(guard),
            Err(e) => return ToolResult::err(e),
        };

        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => return ToolResult::err(e.to_string()),
        };

        if let Ok(map) = self.hashes.lock() {
            match map.get(&path) {
                None => {
                    return ToolResult::err(
                        "You must use read_file before editing. Read the file first.",
                    );
                }
                Some(&stored_hash) => {
                    let current_hash = hash_content(&content);
                    if stored_hash != current_hash {
                        return ToolResult::err("File has been modified since last read. You must use read_file to read the current contents before editing.");
                    }
                }
            }
        }

        if old_string == new_string {
            return ToolResult::err("old_string and new_string are identical");
        }

        let count = content.matches(&old_string).count();
        if count == 0 {
            return ToolResult::err("old_string not found in file");
        }
        if count > 1 && !replace_all {
            return ToolResult::err(format!(
                "old_string found {} times — must be unique, or set replace_all to true",
                count
            ));
        }

        let new_content = if replace_all {
            content.replace(&old_string, &new_string)
        } else {
            content.replacen(&old_string, &new_string, 1)
        };

        match std::fs::write(&path, &new_content) {
            Ok(_) => {
                if let Ok(mut map) = self.hashes.lock() {
                    map.insert(path.clone(), hash_content(&new_content));
                }
                ToolResult::ok(format!("edited {}", display_path(&path)))
            }
            Err(e) => ToolResult::err(e.to_string()),
        }
    }
}
