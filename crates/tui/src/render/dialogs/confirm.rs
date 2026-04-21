use super::{
    begin_dialog_draw, finish_dialog_frame, render_inline_textarea, wrap_line, DialogResult,
    TextArea,
};
use crate::keymap::{hints, nav_lookup, NavAction};
use crate::render::highlight::{
    count_inline_diff_rows, print_inline_diff, print_syntax_file, BashHighlighter,
};
use crate::render::layout_out::LayoutSink;
use crate::render::{draw_bar, ConfirmChoice, RenderOut};
use crate::theme;
use crossterm::event::{KeyCode, KeyModifiers};
use crossterm::{cursor, terminal, QueueableCommand};
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

    pub(crate) fn total_rows(&self, _width: usize) -> u16 {
        match self {
            ConfirmPreview::None => 0,
            ConfirmPreview::Diff { old, new, path } => count_inline_diff_rows(old, new, path, old),
            ConfirmPreview::Notebook(data) => notebook_preview_rows(data),
            ConfirmPreview::FileContent { content, .. } => content.lines().count() as u16,
            ConfirmPreview::BashBody { full_command } => (full_command.lines().count() - 1) as u16,
        }
    }

    pub(crate) fn is_some(&self) -> bool {
        !matches!(self, ConfirmPreview::None)
    }

    /// Whether to show the top dashed separator before the preview.
    pub(crate) fn has_top_separator(&self) -> bool {
        // Bash preview flows directly from the title line — no separator needed.
        !matches!(self, ConfirmPreview::None | ConfirmPreview::BashBody { .. })
    }

    /// Emit the preview into `buf` via `SpanCollector` → `Buffer`
    /// projection. Uses the same LayoutSink-based renderers as the
    /// legacy `render` path, but captures styled spans into `buf`
    /// instead of ANSI bytes on stdout. Wired up by the upcoming
    /// Confirm dialog migration (step 11 of the plan).
    #[allow(dead_code)]
    pub(crate) fn render_into_buffer(
        &self,
        buf: &mut ui::buffer::Buffer,
        width: u16,
        theme: &crate::theme::Theme,
    ) {
        use crate::render::to_buffer::render_into_buffer;
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

    fn render(&self, out: &mut RenderOut, skip: u16, viewport: u16, _width: usize) {
        match self {
            ConfirmPreview::None => {}
            ConfirmPreview::Diff { old, new, path } => {
                print_inline_diff(out, old, new, path, old, skip, viewport);
            }
            ConfirmPreview::Notebook(data) => {
                render_notebook_preview(out, data, skip, viewport);
            }
            ConfirmPreview::FileContent { content, path } => {
                print_syntax_file(out, content, path, skip, viewport);
            }
            ConfirmPreview::BashBody { full_command } => {
                let mut bh = BashHighlighter::new();
                let mut lines = full_command.lines();
                // Advance past first line (rendered in title) to preserve highlighter state.
                if let Some(first) = lines.next() {
                    bh.advance(first);
                }
                let body_lines: Vec<&str> = lines.collect();
                let mut emitted = 0u16;
                for (i, line) in body_lines.iter().enumerate() {
                    if (i as u16) < skip {
                        bh.advance(line);
                        continue;
                    }
                    if emitted >= viewport {
                        break;
                    }
                    out.print(" ");
                    bh.print_line(out, line);
                    out.newline();
                    emitted += 1;
                }
            }
        }
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

fn notebook_preview_rows(data: &NotebookRenderData) -> u16 {
    let title = data.title();
    if data.edit_mode == "insert" {
        wrap_line(&title, crate::render::term_width().saturating_sub(4)).len() as u16
            + data.new_source.lines().count().max(1) as u16
    } else {
        wrap_line(&title, crate::render::term_width().saturating_sub(4)).len() as u16
            + count_inline_diff_rows(
                &data.old_source,
                &data.new_source,
                &data.path,
                &data.old_source,
            )
    }
}

fn render_notebook_preview<S: LayoutSink>(
    out: &mut S,
    data: &NotebookRenderData,
    skip: u16,
    viewport: u16,
) {
    let title = data.title();
    let title_lines = wrap_line(&title, crate::render::term_width().saturating_sub(4));
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

struct ConfirmLayout {
    title_rows: u16,
    summary_rows: u16,
    has_preview: bool,
    viewport_rows: u16,
    total_preview: u16,
    total_rows: u16,
}

/// Non-blocking confirm dialog state machine.
pub struct ConfirmDialog {
    tool_name: String,
    display_name: String,
    desc: String,
    summary: Option<String>,
    preview: ConfirmPreview,
    options: Vec<(String, ConfirmChoice)>,
    preview_scroll: usize,
    selected: usize,
    textarea: TextArea,
    kill_ring: String,
    editing: bool,
    dirty: bool,
    request_id: u64,
    /// Row where the options section begins (used for partial redraws).
    options_row: u16,
    vim_enabled: bool,
    /// Cached terminal size, updated each draw().
    term_size: (u16, u16),
}

impl ConfirmDialog {
    pub fn new(req: &crate::render::ConfirmRequest, vim_enabled: bool) -> Self {
        let mut options: Vec<(String, ConfirmChoice)> = vec![
            ("yes".into(), ConfirmChoice::Yes),
            ("no".into(), ConfirmChoice::No),
        ];
        let display_name = req.tool_name.clone();

        use crate::render::ApprovalScope::{Session, Workspace};

        let cwd_label = std::env::current_dir()
            .ok()
            .and_then(|p| {
                let home = engine::home_dir();
                if let Ok(rel) = p.strip_prefix(&home) {
                    return Some(format!("~/{}", rel.display()));
                }
                p.to_str().map(String::from)
            })
            .unwrap_or_default();

        let has_dir = req.outside_dir.is_some();
        let has_patterns = !req.approval_patterns.is_empty();

        if let Some(ref dir) = req.outside_dir {
            let dir_str = dir.to_string_lossy().into_owned();
            options.push((
                format!("allow {dir_str}"),
                ConfirmChoice::AlwaysDir(dir_str.clone(), Session),
            ));
            options.push((
                format!("allow {dir_str} in {cwd_label}"),
                ConfirmChoice::AlwaysDir(dir_str, Workspace),
            ));
        }
        if has_patterns {
            let display: Vec<&str> = req
                .approval_patterns
                .iter()
                .map(|p| {
                    let d = p.strip_suffix("/*").unwrap_or(p);
                    d.split("://").nth(1).unwrap_or(d)
                })
                .collect();
            let display_str = display.join(", ");
            options.push((
                format!("allow {display_str}"),
                ConfirmChoice::AlwaysPatterns(req.approval_patterns.clone(), Session),
            ));
            options.push((
                format!("allow {display_str} in {cwd_label}"),
                ConfirmChoice::AlwaysPatterns(req.approval_patterns.clone(), Workspace),
            ));
        }
        if !has_dir && !has_patterns {
            options.push(("always allow".into(), ConfirmChoice::Always(Session)));
            options.push((
                format!("always allow in {cwd_label}"),
                ConfirmChoice::Always(Workspace),
            ));
        }

        let preview = ConfirmPreview::from_tool(&req.tool_name, &req.desc, &req.args);

        Self {
            tool_name: req.tool_name.clone(),
            display_name,
            desc: req.desc.clone(),
            summary: req.summary.clone(),
            preview,
            options,
            preview_scroll: 0,
            selected: 0,
            textarea: TextArea::new(),
            kill_ring: String::new(),
            editing: false,
            options_row: 0,
            dirty: true,
            request_id: req.request_id,
            vim_enabled,
            term_size: terminal::size().unwrap_or((80, 24)),
        }
    }
}

impl ConfirmDialog {
    /// Override the cached terminal size (used by `height()` before the
    /// first `draw()` call).  In production `terminal::size()` matches the
    /// real screen, but test harnesses may need to inject a custom size.
    pub fn set_term_size(&mut self, width: u16, height: u16) {
        self.term_size = (width, height);
    }

    fn preview_total_rows(&self) -> usize {
        self.preview.total_rows(self.term_size.0 as usize) as usize
    }

    fn layout(&self, width: u16, height: u16) -> ConfirmLayout {
        let w = width as usize;
        let ta_visible = self.editing || !self.textarea.is_empty();
        let (selected_label, _) = &self.options[self.selected];
        let digits = format!("{}", self.selected + 1).len();
        let prefix_cols = 2 + digits + 2;
        let avail = w.saturating_sub(prefix_cols);
        let last_line_len = if avail > 0 {
            wrap_line(selected_label, avail)
                .last()
                .map(|l| l.len())
                .unwrap_or(0)
        } else {
            selected_label.len()
        };
        let text_indent = (prefix_cols + last_line_len + 2) as u16;
        let wrap_w = width.saturating_sub(text_indent) as usize;
        let ta_extra: u16 = if ta_visible {
            self.textarea.visual_row_count(wrap_w).saturating_sub(1)
        } else {
            0
        };

        let prefix_len = 1 + self.display_name.len() + 2;
        let title_rows = if matches!(self.preview, ConfirmPreview::BashBody { .. }) {
            // Only the first line goes in the title; the rest is scrollable preview.
            let first_line = self.desc.lines().next().unwrap_or("");
            wrap_line(first_line, w.saturating_sub(prefix_len)).len() as u16
        } else {
            wrap_line(&self.desc, w.saturating_sub(prefix_len)).len() as u16
        };
        let summary_rows: u16 = self
            .summary
            .as_ref()
            .map(|s| wrap_line(s, w.saturating_sub(1)).len() as u16)
            .unwrap_or(0);
        let has_preview = self.preview.is_some();
        // bar + title + summary + separators(if preview) +
        // "Allow?" + options + ta_extra + blank + hint
        let separator_rows = if has_preview {
            if self.preview.has_top_separator() {
                2
            } else {
                1
            }
        } else {
            1 // blank line
        };
        let option_rows: u16 = self
            .options
            .iter()
            .enumerate()
            .map(|(i, (label, _))| {
                let digits = format!("{}", i + 1).len();
                let prefix_cols = 2 + digits + 2; // "  N. "
                let avail = w.saturating_sub(prefix_cols);
                if avail == 0 {
                    1
                } else {
                    wrap_line(label, avail).len() as u16
                }
            })
            .sum();
        let fixed_rows: u16 =
            1 + title_rows + summary_rows + separator_rows + 1 + option_rows + ta_extra + 2;

        let total_preview = self.preview.total_rows(w);
        let viewport_rows: u16 = if has_preview {
            let space = height.saturating_sub(fixed_rows);
            space.max(1).min(total_preview)
        } else {
            0
        };

        ConfirmLayout {
            title_rows,
            summary_rows,
            has_preview,
            viewport_rows,
            total_preview,
            total_rows: fixed_rows + viewport_rows,
        }
    }
}

impl super::Dialog for ConfirmDialog {
    fn blocks_agent(&self) -> bool {
        true
    }

    fn height(&self) -> u16 {
        let (width, height) = self.term_size;
        self.layout(width, height).total_rows
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    fn handle_resize(&mut self) {
        self.term_size = terminal::size().unwrap_or(self.term_size);
        self.dirty = true;
    }

    fn set_kill_ring(&mut self, contents: String) {
        self.kill_ring = contents;
    }

    fn kill_ring(&self) -> Option<&str> {
        Some(&self.kill_ring)
    }

    fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> Option<DialogResult> {
        self.dirty = true;

        // ── Editing mode (textarea active) ──────────────────────────────
        if self.editing {
            match nav_lookup(code, modifiers) {
                Some(NavAction::Confirm) => {
                    let msg = if self.textarea.is_empty() {
                        None
                    } else {
                        Some(self.textarea.text())
                    };
                    return Some(DialogResult::Confirm {
                        choice: self.options[self.selected].1.clone(),
                        message: msg,
                        tool_name: self.tool_name.clone(),
                        request_id: self.request_id,
                    });
                }
                Some(NavAction::Dismiss) => {
                    if code == KeyCode::Esc {
                        // Esc exits editing; Ctrl+C rejects or clears.
                        self.editing = false;
                    } else if self.textarea.is_empty() {
                        return Some(DialogResult::Confirm {
                            choice: ConfirmChoice::No,
                            message: None,
                            tool_name: self.tool_name.clone(),
                            request_id: self.request_id,
                        });
                    } else {
                        self.textarea.clear();
                        self.editing = false;
                    }
                }
                _ => {
                    self.textarea
                        .handle_key_with_kill_ring(code, modifiers, &mut self.kill_ring);
                }
            }
            return None;
        }

        // ── Selection mode ──────────────────────────────────────────────
        let (width, height) = self.term_size;
        let viewport = self.layout(width, height).viewport_rows as usize;
        let scroll_step = (viewport / 2).max(1);
        match nav_lookup(code, modifiers) {
            Some(NavAction::Confirm) => {
                let msg = if self.textarea.is_empty() {
                    None
                } else {
                    Some(self.textarea.text())
                };
                Some(DialogResult::Confirm {
                    choice: self.options[self.selected].1.clone(),
                    message: msg,
                    tool_name: self.tool_name.clone(),
                    request_id: self.request_id,
                })
            }
            Some(NavAction::Dismiss) => Some(DialogResult::Confirm {
                choice: ConfirmChoice::No,
                message: None,
                tool_name: self.tool_name.clone(),
                request_id: self.request_id,
            }),
            Some(NavAction::Edit) => {
                self.editing = true;
                None
            }
            Some(NavAction::Up) => {
                self.selected = if self.selected == 0 {
                    self.options.len() - 1
                } else {
                    self.selected - 1
                };
                None
            }
            Some(NavAction::Down) => {
                self.selected = (self.selected + 1) % self.options.len();
                None
            }
            Some(NavAction::PageUp) => {
                self.preview_scroll = self.preview_scroll.saturating_sub(scroll_step);
                None
            }
            Some(NavAction::PageDown) => {
                let tp = self.preview_total_rows();
                if tp > 0 {
                    self.preview_scroll = (self.preview_scroll + scroll_step).min(tp);
                }
                None
            }
            _ => None,
        }
    }

    fn draw(&mut self, out: &mut RenderOut, start_row: u16, width: u16, granted_rows: u16) {
        if !self.dirty {
            return;
        }
        self.dirty = false;

        let w = width as usize;

        let ly = self.layout(width, granted_rows);
        let ta_visible = self.editing || !self.textarea.is_empty();

        // Clamp scroll
        let max_scroll = (ly.total_preview as usize).saturating_sub(ly.viewport_rows as usize);
        self.preview_scroll = self.preview_scroll.min(max_scroll);

        begin_dialog_draw(out, start_row);
        let bar_row = start_row;

        // Where the options section should begin in the current layout.
        let preview_section = if ly.has_preview {
            let seps = if self.preview.has_top_separator() {
                2
            } else {
                1
            };
            seps + ly.viewport_rows
        } else {
            1 // blank line
        };
        let expected_options_row =
            bar_row + 1 + ly.title_rows + ly.summary_rows + preview_section + 1;

        // Partial redraw: when editing and the layout above the options
        // hasn't shifted, skip re-rendering bar/title/preview/"Allow?" and
        // only redraw from options_row down.
        let partial = self.editing
            && self.options_row == expected_options_row
            && self.options_row > 0
            && self.options_row >= bar_row;

        let mut row;
        if partial {
            row = self.options_row;
            out.row = Some(row);
            let _ = out.queue(cursor::MoveTo(0, row));
        } else {
            row = bar_row;

            let title_color = theme::accent();
            draw_bar(out, w, None, None, title_color);
            out.newline();
            row += 1;

            // Title
            let prefix_len = 1 + self.display_name.len() + 2;
            let title_desc = if matches!(self.preview, ConfirmPreview::BashBody { .. }) {
                self.desc.lines().next().unwrap_or("")
            } else {
                &self.desc
            };
            let segments = wrap_line(title_desc, w.saturating_sub(prefix_len));
            let is_bash =
                matches!(self.preview, ConfirmPreview::BashBody { .. }) || self.tool_name == "bash";
            let mut bh = if is_bash {
                Some(BashHighlighter::new())
            } else {
                None
            };
            for (i, seg) in segments.iter().enumerate() {
                if i == 0 {
                    out.print(" ");
                    out.push_fg(title_color);
                    out.print(&self.display_name);
                    out.pop_style();
                    out.print(": ");
                } else {
                    out.print(" ");
                }
                if let Some(ref mut h) = bh {
                    h.print_line(out, seg);
                } else {
                    out.print(seg);
                }
                out.newline();
                row += 1;
            }

            // summary
            if let Some(ref summary) = self.summary {
                let max_cols = w.saturating_sub(1);
                let segments = wrap_line(summary, max_cols);
                for seg in &segments {
                    out.print(" ");
                    out.push_fg(theme::muted());
                    out.print(seg);
                    out.pop_style();
                    out.newline();
                    row += 1;
                }
            }

            if ly.has_preview {
                let separator: String = "\u{254c}".repeat(w);
                // Top separator (only for tools that request it)
                if self.preview.has_top_separator() {
                    out.push_fg(theme::bar());
                    out.print(&separator);
                    out.pop_style();
                    out.newline();
                    row += 1;
                }
                self.preview
                    .render(out, self.preview_scroll as u16, ly.viewport_rows, w);
                row += ly.viewport_rows;
                // Bottom separator -- show scroll indicator when content is clipped
                out.push_fg(theme::bar());
                if ly.total_preview > ly.viewport_rows {
                    let pos = format!(
                        " [{}/{}]",
                        self.preview_scroll + ly.viewport_rows as usize,
                        ly.total_preview
                    );
                    let sep_len = w.saturating_sub(pos.len());
                    out.print(&"\u{254c}".repeat(sep_len));
                    out.pop_style();
                    out.push_fg(theme::muted());
                    out.print(&pos);
                    out.pop_style();
                } else {
                    out.print(&separator);
                    out.pop_style();
                }
                out.newline();
                row += 1;
            }

            if !ly.has_preview {
                out.newline();
                row += 1;
            }
            // Action prompt
            out.push_dim();
            out.print(" Allow?");
            out.pop_style();
            out.newline();
            row += 1;
        }

        self.options_row = row;

        let mut cursor_pos: Option<(u16, u16)> = None;

        for (i, (label, _)) in self.options.iter().enumerate() {
            let digits = format!("{}", i + 1).len();
            let prefix_cols = 2 + digits + 2; // "  N. "
            let avail = w.saturating_sub(prefix_cols);
            let lines = if avail > 0 {
                wrap_line(label, avail)
            } else {
                vec![label.clone()]
            };
            let highlighted = i == self.selected;

            for (li, line) in lines.iter().enumerate() {
                if li == 0 {
                    out.print("  ");
                    if highlighted {
                        out.push_dim();
                        out.print(&format!("{}.", i + 1));
                        out.pop_style();
                        out.print(" ");
                    } else {
                        out.push_dim();
                        out.print(&format!("{}. ", i + 1));
                        out.pop_style();
                    }
                } else {
                    out.print(&" ".repeat(prefix_cols));
                }
                if highlighted {
                    out.push_fg(theme::accent());
                    out.print(line);
                    out.pop_style();
                } else {
                    out.print(line);
                }
                if li < lines.len() - 1 {
                    out.newline();
                    row += 1;
                }
            }

            if i == self.selected && ta_visible {
                let last_line_len = lines.last().map(|l| l.len()).unwrap_or(0);
                let text_col = (prefix_cols + last_line_len + 2) as u16;
                let wrap_w = (w as u16).saturating_sub(text_col) as usize;
                let (new_row, cpos) = render_inline_textarea(
                    out,
                    &self.textarea,
                    self.editing,
                    text_col,
                    wrap_w,
                    row,
                );
                row = new_row;
                cursor_pos = cpos;
            } else {
                out.newline();
                row += 1;
            }
        }

        // footer: blank + hint (queue_dialog_gap provides the gap below)
        out.newline();
        out.push_dim();
        let hint = if self.editing {
            hints::join(&[hints::SEND, hints::CANCEL])
        } else if !self.textarea.is_empty() {
            if ly.total_preview > 0 {
                hints::join(&[
                    hints::CONFIRM_WITH_MSG,
                    hints::EDIT_MSG,
                    hints::scroll(self.vim_enabled),
                ])
            } else {
                hints::join(&[hints::CONFIRM_WITH_MSG, hints::EDIT_MSG])
            }
        } else if ly.total_preview > 0 {
            hints::join(&[
                hints::CONFIRM,
                hints::ADD_MSG,
                hints::scroll(self.vim_enabled),
                hints::CANCEL,
            ])
        } else {
            hints::join(&[hints::CONFIRM, hints::ADD_MSG, hints::CANCEL])
        };
        out.print(&hint);
        out.pop_style();
        // Only clear below the dialog if there's actually viewport space left.
        // When the dialog fills the full terminal, the cursor is at or past
        // the bottom row -- clearing there wipes the last visible option.
        if out.row.is_some_and(|r| r < start_row + granted_rows) {
            let _ = out.queue(terminal::Clear(terminal::ClearType::FromCursorDown));
        }

        finish_dialog_frame(out, cursor_pos, self.editing);
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
