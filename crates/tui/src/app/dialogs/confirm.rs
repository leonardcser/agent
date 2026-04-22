//! Confirm dialog — migrated to the panel framework. Handles built-in
//! tool approvals only; plugin tools drive their own dialogs through
//! `smelt.api.dialog.open`.
//!
//! Panels (top to bottom):
//! - 0 `Title` (Content, Fit) — ` tool: desc` (bash-syntax-highlighted
//!   for `bash`; first line only when the command is multi-line).
//! - 1 `Summary` (Content, Fit, hidden when absent).
//! - 2 `Preview` (Content, Fill, hidden when the tool has no preview)
//!   — inline diff, notebook diff, syntax-highlit file content, or
//!   the bash command body. Scrolls on PageUp/PageDown.
//! - 3 `Options` (`OptionList` widget, Fit, default focus) — yes / no
//!   + dynamic "always allow …" entries per approval scope.
//! - 4 `Reason` (`TextInput` widget, Fit, hidden until the user types
//!   `e` or starts typing) — message attached to the decision.
//!
//! Keys (when focus is on Options):
//! - `1`..`9`, Enter → resolve with `options[N]`
//! - PageUp / PageDown → scroll Preview panel
//! - `e` → focus Reason panel (edit reason message)
//! - Esc / Ctrl+C → resolve as `ConfirmChoice::No`
//!
//! When focus is on Reason:
//! - Enter → resolve with the currently-selected option + the reason text
//! - Esc → clear reason and re-focus Options

use super::super::{App, TurnState};
use super::{ActionResult, DialogState};
use crate::keymap::hints;
use crate::render::dialogs::confirm::ConfirmPreview;
use crate::render::display::{ColorRole, ColorValue};
use crate::render::layout_out::{LayoutSink, SpanCollector};
use crate::render::{ApprovalScope, ConfirmChoice, ConfirmRequest};
use crate::theme;
use crossterm::event::{KeyCode, KeyModifiers};
use ui::buffer::BufCreateOpts;
use ui::text_input::TextInput;
use ui::{BufId, OptionItem, OptionList, PanelHeight, PanelSpec, SeparatorStyle, WinId};

const PANEL_PREVIEW: usize = 2;
const PANEL_OPTIONS: usize = 3;
const PANEL_REASON: usize = 4;

pub struct Confirm {
    request_id: u64,
    tool_name: String,
    /// Option index → ConfirmChoice mapping. Mirrors legacy.
    choices: Vec<ConfirmChoice>,
}

pub fn open(app: &mut App, req: &ConfirmRequest) {
    let (title_buf, summary_buf, preview_buf) = build_text_buffers(app, req);
    let (option_list, choices) = build_options(req);
    let reason = TextInput::new().with_placeholder("reason (optional)…");

    let hint_text = hints::join(&[
        hints::CONFIRM,
        hints::ADD_MSG,
        hints::scroll(app.input.vim_enabled()),
        hints::CANCEL,
    ]);
    let dialog_config = app.builtin_dialog_config(Some(hint_text), vec![]);

    let panels = vec![
        PanelSpec::content(title_buf, PanelHeight::Fit).focusable(false),
        {
            let mut p = PanelSpec::content(summary_buf, PanelHeight::Fit).focusable(false);
            p.collapse_when_empty = true;
            p
        },
        {
            let mut p = PanelSpec::content(preview_buf, PanelHeight::Fill)
                .focusable(false)
                .with_separator(SeparatorStyle::Dashed);
            p.collapse_when_empty = true;
            p
        },
        PanelSpec::widget(Box::new(option_list), PanelHeight::Fit),
        {
            let mut p = PanelSpec::widget(Box::new(reason), PanelHeight::Fit);
            p.collapse_when_empty = true;
            p
        },
    ];

    let win_id = app.ui.dialog_open(
        ui::FloatConfig {
            title: None,
            border: ui::Border::None,
            placement: ui::Placement::dock_bottom_full_width(ui::Constraint::Pct(60)),
            ..Default::default()
        },
        dialog_config,
        panels,
    );
    let Some(win_id) = win_id else { return };

    app.float_states.insert(
        win_id,
        Box::new(Confirm {
            request_id: req.request_id,
            tool_name: req.tool_name.clone(),
            choices,
        }),
    );
}

impl DialogState for Confirm {
    fn blocks_agent(&self) -> bool {
        true
    }

    fn handle_key(
        &mut self,
        app: &mut App,
        win: WinId,
        code: KeyCode,
        mods: KeyModifiers,
    ) -> Option<ui::KeyResult> {
        // PageUp / PageDown scroll the preview regardless of focus.
        match (code, mods) {
            (KeyCode::PageUp, _) => {
                self.scroll_preview(app, win, -1);
                return Some(ui::KeyResult::Consumed);
            }
            (KeyCode::PageDown, _) => {
                self.scroll_preview(app, win, 1);
                return Some(ui::KeyResult::Consumed);
            }
            _ => {}
        }

        // When focus is on Options, 'e' jumps to the Reason input.
        if mods == KeyModifiers::NONE && code == KeyCode::Char('e') {
            let dialog = app.ui.dialog_mut(win)?;
            if dialog.focused_panel() == PANEL_OPTIONS {
                dialog.focus_panel(PANEL_REASON);
                return Some(ui::KeyResult::Consumed);
            }
        }

        None
    }

    fn on_action(
        &mut self,
        app: &mut App,
        win: WinId,
        action: &str,
        agent: &mut Option<TurnState>,
    ) -> ActionResult {
        if let Some(idx_str) = action.strip_prefix("select:") {
            if let Ok(idx) = idx_str.parse::<usize>() {
                if idx < self.choices.len() {
                    self.resolve(app, win, idx, agent);
                    return ActionResult::Close;
                }
            }
        }
        if action == "submit" {
            // TextInput Enter: use the current option selection + reason text.
            let idx = self.selected_option(app, win).unwrap_or(0);
            self.resolve(app, win, idx, agent);
            return ActionResult::Close;
        }
        if action == "dismiss" {
            self.resolve_denied(app, agent);
            return ActionResult::Close;
        }
        ActionResult::Pass
    }

    fn on_dismiss(&mut self, app: &mut App, _win: WinId) {
        let mut agent = None;
        self.resolve_denied(app, &mut agent);
    }
}

impl Confirm {
    fn scroll_preview(&self, app: &mut App, win: WinId, direction: isize) {
        if let Some(dialog) = app.ui.dialog_mut(win) {
            let page = (dialog.panel_rect_height(PANEL_PREVIEW).max(1) as isize) / 2;
            dialog.scroll_panel(PANEL_PREVIEW, direction * page);
        }
    }

    fn selected_option(&self, app: &mut App, win: WinId) -> Option<usize> {
        let dialog = app.ui.dialog_mut(win)?;
        let widget = dialog.panel_widget_mut::<OptionList>(PANEL_OPTIONS)?;
        Some(widget.cursor())
    }

    fn reason_text(&self, app: &mut App, win: WinId) -> Option<String> {
        let dialog = app.ui.dialog_mut(win)?;
        let widget = dialog.panel_widget_mut::<TextInput>(PANEL_REASON)?;
        let text = widget.text().to_string();
        if text.is_empty() {
            None
        } else {
            Some(text)
        }
    }

    fn resolve(&self, app: &mut App, win: WinId, idx: usize, agent: &mut Option<TurnState>) {
        let choice = self.choices.get(idx).cloned().unwrap_or(ConfirmChoice::No);
        let message = self.reason_text(app, win);
        // Match the legacy App::handle_dialog_result Confirm arm.
        let ctx = app.confirm_context.take();
        let call_id = ctx.as_ref().map(|c| c.call_id.clone()).unwrap_or_default();
        let should_cancel = app.resolve_confirm(
            (choice, message),
            &call_id,
            self.request_id,
            &self.tool_name,
            agent,
        );
        if should_cancel && agent.is_some() {
            app.finish_turn(true);
            *agent = None;
        }
    }

    fn resolve_denied(&self, app: &mut App, agent: &mut Option<TurnState>) {
        let ctx = app.confirm_context.take();
        let call_id = ctx.as_ref().map(|c| c.call_id.clone()).unwrap_or_default();
        let should_cancel = app.resolve_confirm(
            (ConfirmChoice::No, None),
            &call_id,
            self.request_id,
            &self.tool_name,
            agent,
        );
        if should_cancel && agent.is_some() {
            app.finish_turn(true);
            *agent = None;
        }
    }
}

// ── Buffer construction ────────────────────────────────────────────────

fn build_text_buffers(app: &mut App, req: &ConfirmRequest) -> (BufId, BufId, BufId) {
    let theme_snap = theme::snapshot();
    let width = crate::render::term_width() as u16;
    let preview = ConfirmPreview::from_tool(&req.tool_name, &req.desc, &req.args);
    let is_bash = matches!(preview, ConfirmPreview::BashBody { .. }) || req.tool_name == "bash";

    let title_buf = app.ui.buf_create(BufCreateOpts {
        buftype: ui::buffer::BufType::Scratch,
        modifiable: false,
    });
    if let Some(buf) = app.ui.buf_mut(title_buf) {
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

    let summary_buf = app.ui.buf_create(BufCreateOpts {
        buftype: ui::buffer::BufType::Scratch,
        modifiable: false,
    });
    if let Some(ref summary) = req.summary {
        if let Some(buf) = app.ui.buf_mut(summary_buf) {
            crate::render::to_buffer::render_into_buffer(buf, width, &theme_snap, |sink| {
                sink.print(" ");
                sink.push_fg(ColorValue::Role(ColorRole::Muted));
                sink.print(summary);
                sink.pop_style();
                sink.newline();
            });
        }
    }

    let preview_buf = app.ui.buf_create(BufCreateOpts {
        buftype: ui::buffer::BufType::Scratch,
        modifiable: false,
    });
    if preview.is_some() {
        if let Some(buf) = app.ui.buf_mut(preview_buf) {
            preview.render_into_buffer(buf, width, &theme_snap);
        }
    }

    (title_buf, summary_buf, preview_buf)
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

// ── Options ────────────────────────────────────────────────────────────

fn build_options(req: &ConfirmRequest) -> (OptionList, Vec<ConfirmChoice>) {
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

    let items: Vec<OptionItem> = labels.into_iter().map(OptionItem::new).collect();
    let accent = ui::grid::Style {
        fg: Some(theme::accent()),
        ..Default::default()
    };
    let list = OptionList::new(items).with_cursor_style(accent);
    (list, choices)
}
