use super::{bool_arg, hash_content, str_arg, FileHashes, Tool, ToolResult};
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
