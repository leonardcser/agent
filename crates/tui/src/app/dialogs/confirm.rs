//! Confirm dialog — built-in tool approvals.
//!
//! The dialog itself lives in `runtime/lua/smelt/confirm.lua`. This
//! file holds the Rust-side primitives the Lua orchestrator calls:
//!
//! - [`render_title_into_buf`] — fills the title buffer with
//!   ` tool: desc Allow?` (bash-highlit when the tool is `bash`).
//! - [`build_options`] — yes / no + dynamic "always allow …" entries
//!   per approval scope. Returns the labels (for the OptionList
//!   widget) and the parallel `ConfirmChoice` array (looked up on
//!   resolve by index).
//!
//! Summary + preview buffers are composed in Lua via
//! `smelt.{diff,syntax,bash,notebook}.render`.
//!
//! Plugin tools drive their own dialogs through `smelt.ui.dialog.open`.

use super::super::App;
use crate::app::dialogs::confirm_preview::ConfirmPreview;
use crate::app::transcript_model::{ApprovalScope, ConfirmChoice, ConfirmRequest};
use crate::content::display::{ColorRole, ColorValue};
use crate::content::layout_out::SpanCollector;
use crate::theme;
use ui::BufId;

/// Live Confirm request held in `App::confirm_requests` while the
/// Lua dialog is open. The choices array is populated by
/// `build_options` so resolve can look up the user's pick by index.
pub(crate) struct ConfirmEntry {
    pub req: ConfirmRequest,
    pub choices: Vec<ConfirmChoice>,
}

/// Render the ` tool: desc Allow?` title into `buf_id`. The tool name
/// shows in the accent color; the desc is bash-highlit when the tool
/// is `bash` (or the preview is a bash body — multi-line commands
/// show only the first line, rest goes in the preview panel).
///
/// Lua creates the buffer via `smelt.buf.create` and asks Rust to
/// fill it; the inline bash highlight on the desc keeps title
/// composition Rust-side until we have a span-level Lua API.
pub(crate) fn render_title_into_buf(app: &mut App, buf_id: BufId, req: &ConfirmRequest) {
    let theme_snap = theme::snapshot();
    let width = crate::content::term_width() as u16;
    let preview = ConfirmPreview::from_tool(&req.tool_name, &req.desc, &req.args);
    let is_bash = matches!(preview, ConfirmPreview::BashBody { .. }) || req.tool_name == "bash";

    if let Some(buf) = app.ui.buf_mut(buf_id) {
        crate::content::to_buffer::render_into_buffer(buf, width, &theme_snap, |sink| {
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
}

fn render_title(
    sink: &mut SpanCollector,
    tool_name: &str,
    desc: &str,
    bash_body: bool,
    is_bash: bool,
) {
    use crate::content::highlight::BashHighlighter;
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
