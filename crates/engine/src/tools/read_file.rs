use super::{
    display_path, file_mtime_ms, int_arg, notebook, str_arg, FileStateCache, Tool, ToolContext,
    ToolFuture, ToolResult,
};
use crate::image;
use serde_json::Value;
use std::collections::HashMap;

/// Default line cap when `limit` is omitted by the caller.
pub const DEFAULT_LINE_LIMIT: usize = 2000;

/// Returned instead of file content when the same file + range is re-read and
/// the file is unchanged on disk. Keeps the earlier `read_file` tool_result in
/// the prompt cache intact (append-only), saving cache_creation tokens.
pub const FILE_UNCHANGED_STUB: &str =
    "File unchanged since last read. The content from the earlier read_file \
     tool_result in this conversation is still current — refer to that \
     instead of re-reading.";

pub struct ReadFileTool {
    pub files: FileStateCache,
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
        _ctx: &'a ToolContext,
    ) -> ToolFuture<'a> {
        Box::pin(async move { tokio::task::block_in_place(|| self.run(&args)) })
    }
}

/// Resolve `offset`/`limit` from tool args, applying the same defaults used at
/// read time. Returned values are the "effective" range actually used — this
/// is what we compare against the cache.
fn effective_range(args: &HashMap<String, Value>) -> (usize, usize) {
    let offset = int_arg(args, "offset").max(1);
    let raw_limit = int_arg(args, "limit");
    let limit = if raw_limit > 0 {
        raw_limit
    } else {
        DEFAULT_LINE_LIMIT
    };
    (offset, limit)
}

/// Check the cache for an exact-range match with unchanged mtime. Returns
/// `Some(stub)` when a dedup hit allows us to skip re-reading; `None` when we
/// must go to disk. Only entries populated by a prior `read_file` call are
/// eligible — entries from edit/write carry `read_range: None` and falling
/// back to them would hand the model pre-edit content.
fn dedup_stub(
    cache: &FileStateCache,
    path: &str,
    requested: (usize, usize),
) -> Option<&'static str> {
    let cached = cache.get(path)?;
    let (co, cl) = cached.read_range?;
    if co != requested.0 || cl != requested.1 {
        return None;
    }
    let current_mtime = file_mtime_ms(path).ok()?;
    if current_mtime == cached.mtime_ms {
        Some(FILE_UNCHANGED_STUB)
    } else {
        None
    }
}

impl ReadFileTool {
    fn run(&self, args: &HashMap<String, Value>) -> ToolResult {
        let path = str_arg(args, "file_path");

        if image::is_image_file(&path) {
            // Images aren't cached: the base64 payload blows the byte cap and
            // isn't a candidate for text dedup anyway.
            return match image::read_image_as_data_url(&path) {
                Ok(data_url) => ToolResult::ok(format!("![image]({data_url})")),
                Err(e) => ToolResult::err(e),
            };
        }

        let requested = effective_range(args);

        if let Some(stub) = dedup_stub(&self.files, &path, requested) {
            return ToolResult::ok(stub);
        }

        if notebook::is_notebook(&path) {
            let raw = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) => return ToolResult::err(e.to_string()),
            };
            self.files.record_read(&path, raw, requested);
            return notebook::read_notebook(&path, requested.0, requested.1);
        }

        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => return ToolResult::err(e.to_string()),
        };

        let (offset, limit) = requested;
        let start = offset - 1;

        let result = {
            let lines: Vec<&str> = content.lines().collect();
            if start >= lines.len() {
                self.files.record_read(&path, content, requested);
                return ToolResult::ok("offset beyond end of file");
            }
            let end = (start + limit).min(lines.len());
            lines[start..end]
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
                .join("\n")
        };

        self.files.record_read(&path, content, requested);
        ToolResult::ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::super::FileState;
    use super::*;
    use tempfile::NamedTempFile;

    fn mk_tool() -> (ReadFileTool, FileStateCache) {
        let files = FileStateCache::new();
        (
            ReadFileTool {
                files: files.clone(),
            },
            files,
        )
    }

    fn args(path: &str, offset: Option<u64>, limit: Option<u64>) -> HashMap<String, Value> {
        let mut m = HashMap::new();
        m.insert("file_path".into(), Value::String(path.into()));
        if let Some(o) = offset {
            m.insert("offset".into(), Value::Number(o.into()));
        }
        if let Some(l) = limit {
            m.insert("limit".into(), Value::Number(l.into()));
        }
        m
    }

    #[test]
    fn effective_range_applies_defaults() {
        let a = args("/x", None, None);
        assert_eq!(effective_range(&a), (1, DEFAULT_LINE_LIMIT));
    }

    #[test]
    fn effective_range_clamps_offset_to_one() {
        let a = args("/x", Some(0), None);
        assert_eq!(effective_range(&a), (1, DEFAULT_LINE_LIMIT));
    }

    #[test]
    fn effective_range_uses_caller_values() {
        let a = args("/x", Some(5), Some(100));
        assert_eq!(effective_range(&a), (5, 100));
    }

    #[test]
    fn first_read_returns_content_and_caches() {
        let (tool, cache) = mk_tool();
        let tmp = NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "alpha\nbeta\ngamma\n").unwrap();
        let path = tmp.path().to_string_lossy().into_owned();

        let r = tool.run(&args(&path, None, None));
        assert!(!r.is_error);
        assert!(r.content.contains("alpha"));
        assert!(r.content.contains("gamma"));
        let cached = cache.get(&path).expect("cache populated");
        assert_eq!(cached.content, "alpha\nbeta\ngamma\n");
        assert_eq!(cached.read_range, Some((1, DEFAULT_LINE_LIMIT)));
    }

    #[test]
    fn repeat_read_same_range_returns_stub() {
        let (tool, _cache) = mk_tool();
        let tmp = NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "a\nb\nc\n").unwrap();
        let path = tmp.path().to_string_lossy().into_owned();

        let first = tool.run(&args(&path, None, None));
        assert!(first.content.contains("a"));
        let second = tool.run(&args(&path, None, None));
        assert_eq!(second.content, FILE_UNCHANGED_STUB);
        assert!(!second.is_error);
    }

    #[test]
    fn repeat_read_different_offset_returns_content() {
        let (tool, _cache) = mk_tool();
        let tmp = NamedTempFile::new().unwrap();
        let body = (1..=10)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(tmp.path(), &body).unwrap();
        let path = tmp.path().to_string_lossy().into_owned();

        let _ = tool.run(&args(&path, None, None));
        let second = tool.run(&args(&path, Some(3), Some(2)));
        assert_ne!(second.content, FILE_UNCHANGED_STUB);
        assert!(second.content.contains("line3"));
        assert!(second.content.contains("line4"));
    }

    #[test]
    fn repeat_read_different_limit_returns_content() {
        let (tool, _cache) = mk_tool();
        let tmp = NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "one\ntwo\nthree\n").unwrap();
        let path = tmp.path().to_string_lossy().into_owned();

        let _ = tool.run(&args(&path, None, None));
        let second = tool.run(&args(&path, Some(1), Some(2)));
        assert_ne!(second.content, FILE_UNCHANGED_STUB);
    }

    #[test]
    fn read_after_external_modification_returns_content() {
        use std::thread::sleep;
        use std::time::Duration;
        let (tool, _cache) = mk_tool();
        let tmp = NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "old\n").unwrap();
        let path = tmp.path().to_string_lossy().into_owned();

        let _ = tool.run(&args(&path, None, None));
        // Sleep past the 1s mtime granularity on HFS+/ext4.
        sleep(Duration::from_millis(1100));
        std::fs::write(tmp.path(), "new content line\n").unwrap();
        let second = tool.run(&args(&path, None, None));
        assert_ne!(second.content, FILE_UNCHANGED_STUB);
        assert!(second.content.contains("new content line"));
    }

    #[test]
    fn cache_entry_without_read_range_is_not_dedup_eligible() {
        let (tool, cache) = mk_tool();
        let tmp = NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "x\n").unwrap();
        let path = tmp.path().to_string_lossy().into_owned();

        let mtime = file_mtime_ms(&path).unwrap();
        cache.set(
            &path,
            FileState {
                content: "x\n".into(),
                mtime_ms: mtime,
                read_range: None,
            },
        );

        let r = tool.run(&args(&path, None, None));
        assert_ne!(r.content, FILE_UNCHANGED_STUB);
        assert!(r.content.contains("x"));
    }

    #[test]
    fn missing_file_is_an_error_not_a_stub() {
        let (tool, _cache) = mk_tool();
        let r = tool.run(&args("/definitely/not/here.txt", None, None));
        assert!(r.is_error);
        assert_ne!(r.content, FILE_UNCHANGED_STUB);
    }
}
