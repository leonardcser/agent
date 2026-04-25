//! Confirm dialog — built-in tool approvals.
//!
//! The dialog itself lives in `runtime/lua/smelt/confirm.lua`. This
//! file is just the Rust-side primitives the Lua orchestrator calls
//! through `smelt.confirm._*`:
//!
//! - [`build_title_buf`] — bash-syntax-highlit ` tool: desc` line.
//! - [`build_summary_buf`] — optional one-line muted summary.
//! - [`build_preview_buf`] — diff / notebook / file / bash-body
//!   preview, depending on the tool.
//! - [`build_options`] — yes / no + dynamic "always allow …" entries
//!   per approval scope. Returns the labels (for the OptionList
//!   widget) and the parallel `ConfirmChoice` array (looked up on
//!   resolve by index).
//!
//! Plugin tools drive their own dialogs through `smelt.ui.dialog.open`.

use super::super::App;
use crate::app::dialogs::confirm_preview::ConfirmPreview;
use crate::app::transcript_model::{ApprovalScope, ConfirmChoice, ConfirmRequest};
use crate::render::display::{ColorRole, ColorValue};
use crate::render::layout_out::{LayoutSink, SpanCollector};
use crate::theme;
use ui::buffer::BufCreateOpts;
use ui::BufId;

/// Live Confirm request held in `App::confirm_requests` while the
/// Lua dialog is open. The choices array is populated by
/// `build_options` so resolve can look up the user's pick by index.
pub(crate) struct ConfirmEntry {
    pub req: ConfirmRequest,
    pub choices: Vec<ConfirmChoice>,
}

/// Title buffer: ` tool: desc Allow?` with the tool name in the accent
/// color and the desc bash-highlit when the tool is `bash` (or the
/// preview is a bash body — multi-line commands show only the first
/// line in the title; the rest goes in the preview panel).
pub(crate) fn build_title_buf(app: &mut App, req: &ConfirmRequest) -> BufId {
    let theme_snap = theme::snapshot();
    let width = crate::render::term_width() as u16;
    let preview = ConfirmPreview::from_tool(&req.tool_name, &req.desc, &req.args);
    let is_bash = matches!(preview, ConfirmPreview::BashBody { .. }) || req.tool_name == "bash";

    let buf_id = app.ui.buf_create(BufCreateOpts {
        buftype: ui::buffer::BufType::Scratch,
        modifiable: false,
    });
    if let Some(buf) = app.ui.buf_mut(buf_id) {
        crate::render::to_buffer::render_into_buffer(buf, width, &theme_snap, |sink| {
            render_title(
                sink,
                &req.tool_name,
                &req.desc,
                matches!(preview, ConfirmPreview::BashBody { .. }),
                is_bash,
            );
            sink.print(" Allow?");
            sink.newline();
        });
    }
    buf_id
}

/// Summary buffer: ` <muted summary>` or empty when the request has no
/// summary. The Lua dialog hides the panel via `collapse_when_empty`.
pub(crate) fn build_summary_buf(app: &mut App, req: &ConfirmRequest) -> BufId {
    let theme_snap = theme::snapshot();
    let width = crate::render::term_width() as u16;
    let buf_id = app.ui.buf_create(BufCreateOpts {
        buftype: ui::buffer::BufType::Scratch,
        modifiable: false,
    });
    if let Some(ref summary) = req.summary {
        if let Some(buf) = app.ui.buf_mut(buf_id) {
            crate::render::to_buffer::render_into_buffer(buf, width, &theme_snap, |sink| {
                sink.print(" ");
                sink.push_fg(ColorValue::Role(ColorRole::Muted));
                sink.print(summary);
                sink.pop_style();
                sink.newline();
            });
        }
    }
    buf_id
}

/// Preview buffer: tool-specific syntax-highlit content (diff,
/// notebook diff, file content, bash body). Empty when the tool has
/// no preview; the Lua dialog hides the panel via `collapse_when_empty`.
pub(crate) fn build_preview_buf(app: &mut App, req: &ConfirmRequest) -> BufId {
    let theme_snap = theme::snapshot();
    let width = crate::render::term_width() as u16;
    let preview = ConfirmPreview::from_tool(&req.tool_name, &req.desc, &req.args);
    let buf_id = app.ui.buf_create(BufCreateOpts {
        buftype: ui::buffer::BufType::Scratch,
        modifiable: false,
    });
    if preview.is_some() {
        if let Some(buf) = app.ui.buf_mut(buf_id) {
            preview.render_into_buffer(buf, width, &theme_snap);
        }
    }
    buf_id
}

fn render_title(
    sink: &mut SpanCollector,
    tool_name: &str,
    desc: &str,
    bash_body: bool,
    is_bash: bool,
) {
    use crate::render::highlight::BashHighlighter;
    let shown = if bash_body {
        desc.lines().next().unwrap_or("")
    } else {
        desc
    };
    sink.print(" ");
    sink.push_fg(ColorValue::Role(ColorRole::Accent));
    sink.print(tool_name);
    sink.pop_style();
    sink.print(": ");
    if is_bash {
        let mut bh = BashHighlighter::new();
        bh.print_line(sink, shown);
    } else {
        sink.print(shown);
    }
    sink.newline();
}

/// `(labels, choices)` for the OptionList widget. The two arrays are
/// parallel — index into `labels` matches the same `ConfirmChoice`
/// entry. Yes / No are always first; "always allow …" variants vary
/// by whether the request has an outside-cwd directory or
/// approval-pattern globs.
pub(crate) fn build_options(req: &ConfirmRequest) -> (Vec<String>, Vec<ConfirmChoice>) {
    let mut labels: Vec<String> = Vec::new();
    let mut choices: Vec<ConfirmChoice> = Vec::new();

    labels.push("yes".into());
    choices.push(ConfirmChoice::Yes);
    labels.push("no".into());
    choices.push(ConfirmChoice::No);

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
        labels.push(format!("allow {dir_str}"));
        choices.push(ConfirmChoice::AlwaysDir(
            dir_str.clone(),
            ApprovalScope::Session,
        ));
        labels.push(format!("allow {dir_str} in {cwd_label}"));
        choices.push(ConfirmChoice::AlwaysDir(dir_str, ApprovalScope::Workspace));
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
        labels.push(format!("allow {display_str}"));
        choices.push(ConfirmChoice::AlwaysPatterns(
            req.approval_patterns.clone(),
            ApprovalScope::Session,
        ));
        labels.push(format!("allow {display_str} in {cwd_label}"));
        choices.push(ConfirmChoice::AlwaysPatterns(
            req.approval_patterns.clone(),
            ApprovalScope::Workspace,
        ));
    }
    if !has_dir && !has_patterns {
        labels.push("always allow".into());
        choices.push(ConfirmChoice::Always(ApprovalScope::Session));
        labels.push(format!("always allow in {cwd_label}"));
        choices.push(ConfirmChoice::Always(ApprovalScope::Workspace));
    }

    (labels, choices)
}
