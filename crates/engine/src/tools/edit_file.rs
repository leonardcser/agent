use super::{
    bool_arg, display_path, notebook, staleness_error, str_arg, FileStateCache, Tool, ToolContext,
    ToolFuture, ToolResult,
};
use serde_json::Value;
use std::collections::HashMap;

pub struct EditFileTool {
    pub files: FileStateCache,
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
        staleness_error(&self.files, &path, "file")
    }

    fn execute<'a>(&'a self, args: HashMap<String, Value>, ctx: &'a ToolContext) -> ToolFuture<'a> {
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

        if notebook::is_notebook(&path) {
            return ToolResult::err(
                "Cannot use edit_file on a Jupyter notebook. Use edit_notebook instead.",
            );
        }

        let old_string = str_arg(args, "old_string");
        let new_string = str_arg(args, "new_string");
        let replace_all = bool_arg(args, "replace_all");

        if let Some(err) = staleness_error(&self.files, &path, "file") {
            return ToolResult::err(err);
        }

        let _flock = match super::try_flock(&path) {
            Ok(guard) => Some(guard),
            Err(e) => return ToolResult::err(e),
        };

        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => return ToolResult::err(e.to_string()),
        };

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
                self.files.record_write(&path, new_content);
                ToolResult::ok(format!("edited {}", display_path(&path)))
            }
            Err(e) => ToolResult::err(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    fn mk_tool() -> (EditFileTool, FileStateCache) {
        let files = FileStateCache::new();
        (
            EditFileTool {
                files: files.clone(),
            },
            files,
        )
    }

    fn args(path: &str, old: &str, new: &str, all: bool) -> HashMap<String, Value> {
        let mut m = HashMap::new();
        m.insert("file_path".into(), Value::String(path.into()));
        m.insert("old_string".into(), Value::String(old.into()));
        m.insert("new_string".into(), Value::String(new.into()));
        if all {
            m.insert("replace_all".into(), Value::Bool(true));
        }
        m
    }

    fn cached_read(cache: &FileStateCache, path: &str, content: &str) {
        cache.record_read(path, content.into(), (1, 2000));
    }

    #[test]
    fn preflight_errors_when_file_not_read() {
        let (tool, _cache) = mk_tool();
        let tmp = NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "hello\n").unwrap();
        let path = tmp.path().to_string_lossy().into_owned();
        let err = tool
            .preflight(&args(&path, "hello", "world", false))
            .expect("preflight error");
        assert!(err.contains("must use read_file"));
        assert!(err.contains("file"));
    }

    #[test]
    fn preflight_errors_on_stale_mtime() {
        use std::thread::sleep;
        use std::time::Duration;
        let (tool, cache) = mk_tool();
        let tmp = NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "hello\n").unwrap();
        let path = tmp.path().to_string_lossy().into_owned();
        cached_read(&cache, &path, "hello\n");
        sleep(Duration::from_millis(1100));
        std::fs::write(tmp.path(), "modified\n").unwrap();
        let err = tool
            .preflight(&args(&path, "modified", "changed", false))
            .expect("preflight error");
        assert!(err.contains("modified since last read"));
    }

    #[test]
    fn preflight_passes_when_fresh() {
        let (tool, cache) = mk_tool();
        let tmp = NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "hello world\n").unwrap();
        let path = tmp.path().to_string_lossy().into_owned();
        cached_read(&cache, &path, "hello world\n");
        assert!(tool.preflight(&args(&path, "hello", "hi", false)).is_none());
    }

    #[test]
    fn run_updates_cache_with_new_content_and_no_range() {
        let (tool, cache) = mk_tool();
        let tmp = NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "aaa\nbbb\n").unwrap();
        let path = tmp.path().to_string_lossy().into_owned();
        cached_read(&cache, &path, "aaa\nbbb\n");
        let r = tool.run(&args(&path, "aaa", "xxx", false));
        assert!(!r.is_error, "edit failed: {}", r.content);
        let cached = cache.get(&path).unwrap();
        assert_eq!(cached.content, "xxx\nbbb\n");
        assert_eq!(cached.read_range, None);
    }

    #[test]
    fn run_rejects_edit_without_prior_read() {
        let (tool, _cache) = mk_tool();
        let tmp = NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "hi\n").unwrap();
        let path = tmp.path().to_string_lossy().into_owned();
        let r = tool.run(&args(&path, "hi", "yo", false));
        assert!(r.is_error);
        assert!(r.content.contains("must use read_file"));
    }

    #[test]
    fn run_detects_mid_flight_mtime_change() {
        use std::thread::sleep;
        use std::time::Duration;
        let (tool, cache) = mk_tool();
        let tmp = NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "original\n").unwrap();
        let path = tmp.path().to_string_lossy().into_owned();
        cached_read(&cache, &path, "original\n");
        sleep(Duration::from_millis(1100));
        std::fs::write(tmp.path(), "different\n").unwrap();
        let r = tool.run(&args(&path, "different", "changed", false));
        assert!(r.is_error);
        assert!(r.content.contains("modified since last read"));
    }
}
