use super::{
    display_path, notebook, staleness_error, str_arg, FileStateCache, Tool, ToolContext,
    ToolFuture, ToolResult,
};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

const UNREAD_OVERWRITE_ERR: &str = "File already exists. Use edit_file to modify existing files, or read_file then write_file to replace.";

pub struct WriteFileTool {
    pub files: FileStateCache,
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
        if !Path::new(&path).exists() {
            return None;
        }
        if !self.files.has(&path) {
            return Some(UNREAD_OVERWRITE_ERR.into());
        }
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

impl WriteFileTool {
    fn run(&self, args: &HashMap<String, Value>) -> ToolResult {
        let path = str_arg(args, "file_path");
        let content = str_arg(args, "content");

        if notebook::is_notebook(&path) {
            return ToolResult::err(
                "Cannot use write_file on a Jupyter notebook. Use edit_notebook instead.",
            );
        }

        let exists = Path::new(&path).exists();
        let _flock = if exists {
            if !self.files.has(&path) {
                return ToolResult::err(UNREAD_OVERWRITE_ERR);
            }
            if let Some(err) = staleness_error(&self.files, &path, "file") {
                return ToolResult::err(err);
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

        let len = content.len();
        match std::fs::write(&path, &content) {
            Ok(_) => {
                self.files.record_write(&path, content);
                ToolResult::ok(format!("wrote {} bytes to {}", len, display_path(&path)))
            }
            Err(e) => ToolResult::err(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    fn mk_tool() -> (WriteFileTool, FileStateCache) {
        let files = FileStateCache::new();
        (
            WriteFileTool {
                files: files.clone(),
            },
            files,
        )
    }

    fn args(path: &str, content: &str) -> HashMap<String, Value> {
        let mut m = HashMap::new();
        m.insert("file_path".into(), Value::String(path.into()));
        m.insert("content".into(), Value::String(content.into()));
        m
    }

    fn cached_read(cache: &FileStateCache, path: &str, content: &str) {
        cache.record_read(path, content.into(), (1, 2000));
    }

    #[test]
    fn write_new_file_does_not_require_prior_read() {
        let (tool, cache) = mk_tool();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.txt").to_string_lossy().into_owned();
        assert!(tool.preflight(&args(&path, "hello")).is_none());
        let r = tool.run(&args(&path, "hello"));
        assert!(!r.is_error, "{}", r.content);
        assert_eq!(cache.get(&path).unwrap().content, "hello");
        assert_eq!(cache.get(&path).unwrap().read_range, None);
    }

    #[test]
    fn overwrite_without_prior_read_is_rejected() {
        let (tool, _cache) = mk_tool();
        let tmp = NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "existing\n").unwrap();
        let path = tmp.path().to_string_lossy().into_owned();
        let err = tool.preflight(&args(&path, "new")).expect("preflight err");
        assert_eq!(err, UNREAD_OVERWRITE_ERR);
        let r = tool.run(&args(&path, "new"));
        assert!(r.is_error);
        assert_eq!(r.content, UNREAD_OVERWRITE_ERR);
    }

    #[test]
    fn overwrite_after_prior_read_succeeds() {
        let (tool, cache) = mk_tool();
        let tmp = NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "old\n").unwrap();
        let path = tmp.path().to_string_lossy().into_owned();
        cached_read(&cache, &path, "old\n");
        assert!(tool.preflight(&args(&path, "new content")).is_none());
        let r = tool.run(&args(&path, "new content"));
        assert!(!r.is_error, "{}", r.content);
        let cached = cache.get(&path).unwrap();
        assert_eq!(cached.content, "new content");
        assert_eq!(cached.read_range, None);
    }

    #[test]
    fn overwrite_detects_external_modification() {
        use std::thread::sleep;
        use std::time::Duration;
        let (tool, cache) = mk_tool();
        let tmp = NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "original\n").unwrap();
        let path = tmp.path().to_string_lossy().into_owned();
        cached_read(&cache, &path, "original\n");
        sleep(Duration::from_millis(1100));
        std::fs::write(tmp.path(), "touched\n").unwrap();
        let r = tool.run(&args(&path, "new content"));
        assert!(r.is_error);
        assert!(r.content.contains("modified since last read"));
    }
}
