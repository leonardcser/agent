//! Confirm-dialog preview rendering. `ConfirmPreview` owns the
//! tool-specific preview payload and renders into a `ui::Buffer` via
//! the `SpanCollector` projection pipeline. `app/dialogs/confirm.rs`
//! consumes `from_tool` + `render_into_buffer`.

use crate::content::highlight::{print_inline_diff, print_syntax_file, BashHighlighter};
use crate::content::layout_out::SpanCollector;
use crate::content::wrap_line;
use crate::theme;
use engine::tools::NotebookRenderData;
use std::collections::HashMap;

/// Tool-specific scrollable preview content for the confirm dialog.
pub(crate) enum ConfirmPreview {
    /// No preview — simple tool calls.
    None,
    /// Inline diff preview for edit_file.
    Diff {
        old: String,
        new: String,
        path: String,
    },
    /// Notebook cell preview/diff for edit_notebook.
    Notebook(NotebookRenderData),
    /// Syntax-highlighted file content for write_file.
    FileContent { content: String, path: String },
    /// Remaining lines of a multiline bash command (after the first line).
    BashBody {
        /// The full command — first line is rendered in the title, rest here.
        full_command: String,
    },
}

impl ConfirmPreview {
    pub(crate) fn from_tool(
        tool_name: &str,
        desc: &str,
        args: &HashMap<String, serde_json::Value>,
    ) -> Self {
        match tool_name {
            "edit_file" => {
                let old = args
                    .get("old_string")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let new = args
                    .get("new_string")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let path = args
                    .get("file_path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                ConfirmPreview::Diff { old, new, path }
            }
            "edit_notebook" => build_notebook_preview(args),
            "write_file" => {
                let content = args
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let path = args
                    .get("file_path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                ConfirmPreview::FileContent { content, path }
            }
            "bash" if desc.lines().count() > 1 => ConfirmPreview::BashBody {
                full_command: desc.to_string(),
            },
            _ => ConfirmPreview::None,
        }
    }

    pub(crate) fn is_some(&self) -> bool {
        !matches!(self, ConfirmPreview::None)
    }

    /// Emit the preview into `buf` via `SpanCollector` → `Buffer`
    /// projection. Used by the panel-based Confirm dialog.
    pub(crate) fn render_into_buffer(
        &self,
        buf: &mut ui::buffer::Buffer,
        width: u16,
        theme: &crate::theme::Theme,
    ) {
        use crate::content::to_buffer::render_into_buffer;
        render_into_buffer(buf, width, theme, |sink| match self {
            ConfirmPreview::None => {}
            ConfirmPreview::Diff { old, new, path } => {
                print_inline_diff(sink, old, new, path, old, 0, u16::MAX);
            }
            ConfirmPreview::Notebook(data) => {
                render_notebook_preview(sink, data, 0, u16::MAX);
            }
            ConfirmPreview::FileContent { content, path } => {
                print_syntax_file(sink, content, path, 0, u16::MAX);
            }
            ConfirmPreview::BashBody { full_command } => {
                let mut bh = BashHighlighter::new();
                let mut lines = full_command.lines();
                if let Some(first) = lines.next() {
                    bh.advance(first);
                }
                for line in lines {
                    sink.print(" ");
                    bh.print_line(sink, line);
                    sink.newline();
                }
            }
        });
    }
}

fn build_notebook_preview(args: &HashMap<String, serde_json::Value>) -> ConfirmPreview {
    let path = args
        .get("notebook_path")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(_) => return ConfirmPreview::None,
    };
    let parsed: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return ConfirmPreview::None,
    };
    let Some(cells) = parsed.get("cells").and_then(|c| c.as_array()) else {
        return ConfirmPreview::None;
    };

    let edit_mode = args
        .get("edit_mode")
        .and_then(|v| v.as_str())
        .unwrap_or("replace");
    let cell_id = args.get("cell_id").and_then(|v| v.as_str()).unwrap_or("");
    let cell_number = args.get("cell_number").and_then(|v| v.as_i64());
    let new_source = args
        .get("new_source")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let requested_type = args
        .get("cell_type")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let target_idx = if !cell_id.is_empty() {
        cells
            .iter()
            .position(|c| c.get("id").and_then(|v| v.as_str()) == Some(cell_id))
    } else {
        cell_number.and_then(|n| if n < 0 { None } else { Some(n as usize) })
    };

    let preview = match edit_mode {
        "insert" => {
            let insert_at = if cell_id.is_empty() && cell_number.is_none() {
                0
            } else {
                match target_idx {
                    Some(i) if i < cells.len() => i + 1,
                    _ => return ConfirmPreview::None,
                }
            };
            NotebookRenderData {
                edit_mode: "insert".into(),
                path: path.into(),
                index: insert_at,
                old_type: None,
                new_type: requested_type,
                cell_id: None,
                old_source: String::new(),
                new_source,
            }
        }
        "delete" => {
            let idx = match target_idx {
                Some(i) if i < cells.len() => i,
                _ => return ConfirmPreview::None,
            };
            let cell = &cells[idx];
            NotebookRenderData {
                edit_mode: "delete".into(),
                path: path.into(),
                index: idx,
                old_type: cell
                    .get("cell_type")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                new_type: None,
                cell_id: cell.get("id").and_then(|v| v.as_str()).map(str::to_string),
                old_source: cell
                    .get("source")
                    .and_then(join_string_or_array)
                    .unwrap_or_default(),
                new_source: String::new(),
            }
        }
        _ => {
            let idx = match target_idx {
                Some(i) if i < cells.len() => i,
                _ => return ConfirmPreview::None,
            };
            let cell = &cells[idx];
            NotebookRenderData {
                edit_mode: "replace".into(),
                path: path.into(),
                index: idx,
                old_type: cell
                    .get("cell_type")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                new_type: requested_type.or_else(|| {
                    cell.get("cell_type")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                }),
                cell_id: cell.get("id").and_then(|v| v.as_str()).map(str::to_string),
                old_source: cell
                    .get("source")
                    .and_then(join_string_or_array)
                    .unwrap_or_default(),
                new_source,
            }
        }
    };
    ConfirmPreview::Notebook(preview)
}

fn join_string_or_array(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(arr) => Some(
            arr.iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(""),
        ),
        _ => None,
    }
}

fn render_notebook_preview(
    out: &mut SpanCollector,
    data: &NotebookRenderData,
    skip: u16,
    viewport: u16,
) {
    let title = data.title();
    let title_lines = wrap_line(&title, crate::content::term_width().saturating_sub(4));
    let mut skipped = skip;
    let mut emitted = 0u16;

    for line in &title_lines {
        if skipped > 0 {
            skipped -= 1;
            continue;
        }
        if viewport > 0 && emitted >= viewport {
            return;
        }
        out.print(" ");
        out.push_fg(theme::muted().into());
        out.print(line);
        out.pop_style();
        out.newline();
        emitted += 1;
    }

    let remaining = if viewport == 0 {
        0
    } else {
        viewport.saturating_sub(emitted)
    };
    if data.edit_mode == "insert" {
        if remaining == 0 && viewport > 0 {
            return;
        }
        print_syntax_file(out, &data.new_source, &data.path, skipped, remaining);
    } else {
        print_inline_diff(
            out,
            &data.old_source,
            &data.new_source,
            &data.path,
            &data.old_source,
            skipped,
            remaining,
        );
    }
}
