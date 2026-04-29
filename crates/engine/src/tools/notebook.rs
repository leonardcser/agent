use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use super::{
    display_path, staleness_error, str_arg, FileStateCache, Tool, ToolContext, ToolFuture,
    ToolResult,
};

/// Check whether a file path looks like a Jupyter notebook.
pub fn is_notebook(path: &str) -> bool {
    path.to_lowercase().ends_with(".ipynb")
}

#[derive(Debug, Clone)]
struct NotebookCellSnapshot {
    index: usize,
    cell_type: String,
    cell_id: Option<String>,
    source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotebookRenderData {
    pub edit_mode: String,
    pub path: String,
    pub index: usize,
    pub old_type: Option<String>,
    pub new_type: Option<String>,
    pub cell_id: Option<String>,
    pub old_source: String,
    pub new_source: String,
}

impl NotebookRenderData {
    /// Return the file extension for syntax highlighting based on the cell type.
    pub fn syntax_ext(&self) -> &str {
        let cell_type = self.new_type.as_deref().or(self.old_type.as_deref());
        match cell_type {
            Some("markdown") => "md",
            _ => "py",
        }
    }

    pub fn title(&self) -> String {
        let kind = match (self.old_type.as_deref(), self.new_type.as_deref()) {
            (Some(old), Some(new)) if old != new => format!("{old} → {new}"),
            (_, Some(new)) => new.to_string(),
            (Some(old), None) => old.to_string(),
            _ => "cell".into(),
        };
        let mut title = format!("{} cell {} [{}]", self.edit_mode, self.index, kind);
        if let Some(id) = self.cell_id.as_deref() {
            title.push_str(&format!(" id={id}"));
        }
        title
    }
}

/// Render a notebook's cells as human-readable text with line numbers.
pub fn read_notebook(path: &str, offset: usize, limit: usize) -> ToolResult {
    let raw = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => return ToolResult::err(e.to_string()),
    };

    let nb: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => return ToolResult::err(format!("failed to parse notebook JSON: {e}")),
    };

    let cells = match nb.get("cells").and_then(|c| c.as_array()) {
        Some(c) => c,
        None => return ToolResult::ok("notebook has no cells array"),
    };

    if cells.is_empty() {
        return ToolResult::ok("notebook is empty (0 cells)");
    }

    let mut lines: Vec<String> = Vec::new();

    for (i, cell) in cells.iter().enumerate() {
        let cell_type = cell
            .get("cell_type")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let cell_id = cell.get("id").and_then(|v| v.as_str()).unwrap_or("");

        let id_display = if cell_id.is_empty() {
            String::new()
        } else {
            format!(" id={cell_id}")
        };

        lines.push(format!("--- Cell {i} [{cell_type}]{id_display} ---"));

        // Source
        let source = join_string_or_array(cell.get("source"));
        for line in source.lines() {
            lines.push(line.to_string());
        }
        if source.is_empty() {
            lines.push(String::new());
        }

        // Outputs (code cells only)
        if cell_type == "code" {
            if let Some(outputs) = cell.get("outputs").and_then(|o| o.as_array()) {
                for output in outputs {
                    render_output(output, &mut lines);
                }
            }
        }

        lines.push(String::new()); // blank separator
    }

    // Apply offset/limit (1-based offset like read_file)
    let start = (offset.max(1)) - 1;
    if start >= lines.len() {
        return ToolResult::ok("offset beyond end of notebook");
    }
    let end = (start + limit).min(lines.len());

    let result: String = lines[start..end]
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{:4}\t{}", start + i + 1, line))
        .collect::<Vec<_>>()
        .join("\n");

    ToolResult::ok(result)
}

fn render_output(output: &Value, lines: &mut Vec<String>) {
    let output_type = output
        .get("output_type")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    match output_type {
        "stream" => {
            let text = join_string_or_array(output.get("text"));
            if !text.is_empty() {
                lines.push("[output]".into());
                for line in text.lines() {
                    lines.push(line.to_string());
                }
            }
        }
        "execute_result" | "display_data" => {
            if let Some(data) = output.get("data") {
                // Prefer text/plain, note image presence
                if let Some(text) = data.get("text/plain") {
                    let t = join_string_or_array(Some(text));
                    if !t.is_empty() {
                        lines.push("[output]".into());
                        for line in t.lines() {
                            lines.push(line.to_string());
                        }
                    }
                }
                if data.get("image/png").is_some() || data.get("image/jpeg").is_some() {
                    lines.push("[image output]".into());
                }
                if let Some(html) = data.get("text/html") {
                    let h = join_string_or_array(Some(html));
                    if !h.is_empty() && data.get("text/plain").is_none() {
                        lines.push("[html output]".into());
                        for line in h.lines() {
                            lines.push(line.to_string());
                        }
                    }
                }
            }
        }
        "error" => {
            let ename = output
                .get("ename")
                .and_then(|v| v.as_str())
                .unwrap_or("Error");
            let evalue = output.get("evalue").and_then(|v| v.as_str()).unwrap_or("");
            lines.push(format!("[error: {ename}: {evalue}]"));
            if let Some(tb) = output.get("traceback").and_then(|v| v.as_array()) {
                for frame in tb {
                    if let Some(s) = frame.as_str() {
                        // Strip ANSI escape codes from traceback
                        let clean = strip_ansi(s);
                        for line in clean.lines() {
                            lines.push(line.to_string());
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

/// Notebook source can be a string or an array of strings.
fn join_string_or_array(val: Option<&Value>) -> String {
    match val {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek() {
                Some('[') => {
                    while let Some(&next) = chars.peek() {
                        chars.next();
                        if next.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(&next) = chars.peek() {
                        if next == '\x07' {
                            chars.next();
                            break;
                        }
                        if next == '\x1b' {
                            chars.next();
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                        chars.next();
                    }
                }
                _ => {
                    while let Some(&next) = chars.peek() {
                        chars.next();
                        if next.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn render_data_metadata(data: &NotebookRenderData) -> serde_json::Value {
    serde_json::json!({
        "kind": "notebook_cell_edit",
        "edit_mode": data.edit_mode,
        "path": data.path,
        "index": data.index,
        "old_type": data.old_type,
        "new_type": data.new_type,
        "cell_id": data.cell_id,
        "old_source": data.old_source,
        "new_source": data.new_source,
    })
}

fn cell_snapshot(cell: &Value, index: usize) -> NotebookCellSnapshot {
    NotebookCellSnapshot {
        index,
        cell_type: cell
            .get("cell_type")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string(),
        cell_id: cell.get("id").and_then(|v| v.as_str()).map(str::to_string),
        source: join_string_or_array(cell.get("source")),
    }
}

fn render_data_from_snapshots(
    edit_mode: &str,
    path: &str,
    old: Option<&NotebookCellSnapshot>,
    new: Option<&NotebookCellSnapshot>,
) -> NotebookRenderData {
    let index = new
        .map(|c| c.index)
        .or_else(|| old.map(|c| c.index))
        .unwrap_or(0);
    NotebookRenderData {
        edit_mode: edit_mode.to_string(),
        path: path.to_string(),
        index,
        old_type: old.map(|c| c.cell_type.clone()),
        new_type: new.map(|c| c.cell_type.clone()),
        cell_id: new
            .and_then(|c| c.cell_id.clone())
            .or_else(|| old.and_then(|c| c.cell_id.clone())),
        old_source: old.map(|c| c.source.clone()).unwrap_or_default(),
        new_source: new.map(|c| c.source.clone()).unwrap_or_default(),
    }
}

// ---------------------------------------------------------------------------
// Editing
// ---------------------------------------------------------------------------

pub struct NotebookEditTool {
    pub files: FileStateCache,
}

impl Tool for NotebookEditTool {
    fn name(&self) -> &str {
        "edit_notebook"
    }

    fn description(&self) -> &str {
        "Edit a Jupyter notebook (.ipynb) cell. Supports replacing, inserting, and deleting cells. Identify cells by cell_id or cell_number (0-indexed)."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "notebook_path": {
                    "type": "string",
                    "description": "The absolute path to the Jupyter notebook file"
                },
                "cell_number": {
                    "type": "integer",
                    "description": "The 0-indexed cell number to edit. Used when cell_id is not provided."
                },
                "cell_id": {
                    "type": "string",
                    "description": "The ID of the cell to edit. Takes precedence over cell_number. When inserting, the new cell is placed after this cell (omit to insert at the beginning)."
                },
                "new_source": {
                    "type": "string",
                    "description": "The new source content for the cell. Required for replace and insert."
                },
                "cell_type": {
                    "type": "string",
                    "enum": ["code", "markdown"],
                    "description": "The cell type. Required for insert, defaults to current type for replace."
                },
                "edit_mode": {
                    "type": "string",
                    "enum": ["replace", "insert", "delete"],
                    "description": "The edit operation. Defaults to replace."
                }
            },
            "required": ["notebook_path"]
        })
    }

    fn needs_confirm(&self, args: &HashMap<String, Value>) -> Option<String> {
        Some(display_path(&str_arg(args, "notebook_path")))
    }

    fn preflight(&self, args: &HashMap<String, Value>) -> Option<String> {
        let path = str_arg(args, "notebook_path");
        staleness_error(&self.files, &path, "notebook")
    }

    fn execute<'a>(&'a self, args: HashMap<String, Value>, ctx: &'a ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            let path = str_arg(&args, "notebook_path");
            let _guard = ctx.file_locks.lock(&path).await;
            tokio::task::block_in_place(|| run_edit(&args, &self.files))
        })
    }
}

fn run_edit(args: &HashMap<String, Value>, files: &FileStateCache) -> ToolResult {
    let path = str_arg(args, "notebook_path");

    if path.is_empty() {
        return ToolResult::err("notebook_path is required");
    }

    if !Path::new(&path).exists() {
        return ToolResult::err(format!("file not found: {}", display_path(&path)));
    }

    // Acquire cross-process advisory lock (non-blocking).
    let _flock = match super::try_flock(&path) {
        Ok(guard) => Some(guard),
        Err(e) => return ToolResult::err(e),
    };

    let edit_mode = {
        let m = str_arg(args, "edit_mode");
        if m.is_empty() {
            "replace".to_string()
        } else {
            m
        }
    };
    let new_source = str_arg(args, "new_source");
    let cell_id = str_arg(args, "cell_id");
    let cell_type = str_arg(args, "cell_type");
    let cell_number = args.get("cell_number").and_then(|v| v.as_i64());

    // Validate edit_mode
    if !matches!(edit_mode.as_str(), "replace" | "insert" | "delete") {
        return ToolResult::err(format!(
            "invalid edit_mode: {edit_mode} (expected replace, insert, or delete)"
        ));
    }

    // new_source required for replace and insert
    if edit_mode != "delete" && new_source.is_empty() {
        return ToolResult::err(format!("new_source is required for {edit_mode}"));
    }

    // cell_type required for insert
    if edit_mode == "insert" && cell_type.is_empty() {
        return ToolResult::err("cell_type is required when inserting a new cell");
    }

    if let Some(err) = staleness_error(files, &path, "notebook") {
        return ToolResult::err(err);
    }

    let raw = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => return ToolResult::err(e.to_string()),
    };

    let mut nb: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => return ToolResult::err(format!("failed to parse notebook JSON: {e}")),
    };

    let cells = match nb.get_mut("cells").and_then(|c| c.as_array_mut()) {
        Some(c) => c,
        None => return ToolResult::err("notebook has no cells array"),
    };

    // Resolve target cell index
    let target_idx = resolve_cell_index(cells, &cell_id, cell_number);

    match edit_mode.as_str() {
        "replace" => {
            let idx = match target_idx {
                Some(i) => i,
                None => {
                    return ToolResult::err(cell_not_found_msg(&cell_id, cell_number, cells.len()))
                }
            };
            if idx >= cells.len() {
                return ToolResult::err(format!(
                    "cell_number {idx} out of range (notebook has {} cells)",
                    cells.len()
                ));
            }

            let old_cell = cell_snapshot(&cells[idx], idx);

            // Convert source to array of lines (notebook convention)
            let source_value = source_to_json(&new_source);
            cells[idx]["source"] = source_value;

            if !cell_type.is_empty() {
                cells[idx]["cell_type"] = Value::String(cell_type.clone());
                // If switching to markdown, remove outputs and execution_count
                if cell_type == "markdown" {
                    if let Some(o) = cells[idx].as_object_mut() {
                        o.remove("outputs");
                        o.remove("execution_count");
                    }
                }
                // If switching to code, ensure outputs/execution_count exist
                if cell_type == "code" {
                    let obj = cells[idx].as_object_mut().unwrap();
                    obj.entry("outputs").or_insert(Value::Array(vec![]));
                    obj.entry("execution_count").or_insert(Value::Null);
                }
            }

            // Clear outputs on replace (stale)
            if cells[idx].get("cell_type").and_then(|v| v.as_str()) == Some("code") {
                cells[idx]["outputs"] = Value::Array(vec![]);
                cells[idx]["execution_count"] = Value::Null;
            }

            let new_cell = cell_snapshot(&cells[idx], idx);
            let render =
                render_data_from_snapshots("replace", &path, Some(&old_cell), Some(&new_cell));
            write_notebook(
                &path,
                &nb,
                &format!("replaced cell {idx}"),
                files,
                Some(render),
            )
        }
        "insert" => {
            // Insert after target_idx, or at beginning if no target specified
            let insert_at = if cell_id.is_empty() && cell_number.is_none() {
                0
            } else {
                match target_idx {
                    Some(i) => {
                        if i >= cells.len() {
                            return ToolResult::err(format!(
                                "cell_number {i} out of range (notebook has {} cells)",
                                cells.len()
                            ));
                        }
                        i + 1
                    }
                    None => {
                        return ToolResult::err(cell_not_found_msg(
                            &cell_id,
                            cell_number,
                            cells.len(),
                        ))
                    }
                }
            };

            let new_cell = make_cell(&cell_type, &new_source);
            cells.insert(insert_at, new_cell);
            let inserted = cell_snapshot(&cells[insert_at], insert_at);
            let render = render_data_from_snapshots("insert", &path, None, Some(&inserted));

            write_notebook(
                &path,
                &nb,
                &format!("inserted {cell_type} cell at position {insert_at}"),
                files,
                Some(render),
            )
        }
        "delete" => {
            let idx = match target_idx {
                Some(i) => i,
                None => {
                    return ToolResult::err(cell_not_found_msg(&cell_id, cell_number, cells.len()))
                }
            };
            if idx >= cells.len() {
                return ToolResult::err(format!(
                    "cell_number {idx} out of range (notebook has {} cells)",
                    cells.len()
                ));
            }

            let deleted = cell_snapshot(&cells[idx], idx);
            cells.remove(idx);
            let render = render_data_from_snapshots("delete", &path, Some(&deleted), None);

            write_notebook(
                &path,
                &nb,
                &format!("deleted cell {idx}"),
                files,
                Some(render),
            )
        }
        _ => unreachable!(),
    }
}

fn resolve_cell_index(cells: &[Value], cell_id: &str, cell_number: Option<i64>) -> Option<usize> {
    // cell_id takes precedence
    if !cell_id.is_empty() {
        return cells
            .iter()
            .position(|c| c.get("id").and_then(|v| v.as_str()) == Some(cell_id));
    }
    cell_number.and_then(|n| if n < 0 { None } else { Some(n as usize) })
}

fn cell_not_found_msg(cell_id: &str, cell_number: Option<i64>, total: usize) -> String {
    if !cell_id.is_empty() {
        format!("cell with id '{cell_id}' not found")
    } else if let Some(n) = cell_number {
        format!("cell_number {n} out of range (notebook has {total} cells)")
    } else {
        "either cell_id or cell_number must be provided".into()
    }
}

/// Convert a source string into the notebook JSON array-of-lines format.
fn source_to_json(source: &str) -> Value {
    let lines: Vec<&str> = source.split('\n').collect();
    let arr: Vec<Value> = lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            if i < lines.len() - 1 {
                Value::String(format!("{line}\n"))
            } else if line.is_empty() {
                // Last line empty means trailing newline was already captured
                Value::String(String::new())
            } else {
                Value::String((*line).to_string())
            }
        })
        .collect();
    Value::Array(arr)
}

fn make_cell(cell_type: &str, source: &str) -> Value {
    let id = generate_cell_id();
    let source_value = source_to_json(source);

    let mut cell = serde_json::json!({
        "cell_type": cell_type,
        "id": id,
        "metadata": {},
        "source": source_value
    });

    if cell_type == "code" {
        cell["execution_count"] = Value::Null;
        cell["outputs"] = Value::Array(vec![]);
    }

    cell
}

static NEXT_CELL_ID: AtomicU64 = AtomicU64::new(1);

fn generate_cell_id() -> String {
    let id = NEXT_CELL_ID.fetch_add(1, Ordering::Relaxed);
    format!("{:016x}", id)
}

fn write_notebook(
    path: &str,
    nb: &Value,
    action: &str,
    files: &FileStateCache,
    render: Option<NotebookRenderData>,
) -> ToolResult {
    // 1-space indent matches Jupyter/JupyterLab convention
    let mut buf = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b" ");
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, formatter);
    if let Err(e) = nb.serialize(&mut ser) {
        return ToolResult::err(format!("failed to serialize notebook: {e}"));
    }
    let mut json = match String::from_utf8(buf) {
        Ok(s) => s,
        Err(e) => return ToolResult::err(format!("failed to serialize notebook: {e}")),
    };

    if !json.ends_with('\n') {
        json.push('\n');
    }

    match std::fs::write(path, &json) {
        Ok(_) => {
            files.record_write(path, json);
            if let Some(render) = render {
                ToolResult::ok(format!("{action} in {}", display_path(path)))
                    .with_metadata(render_data_metadata(&render))
            } else {
                ToolResult::ok(format!("{action} in {}", display_path(path)))
            }
        }
        Err(e) => ToolResult::err(e.to_string()),
    }
}
