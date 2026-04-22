//! Dialog built from a Lua `opts` table and driven by a parked
//! `LuaTask`. Opened when a plugin tool / `smelt.task` coroutine yields
//! `smelt.api.dialog.open({...})`. When the user resolves the dialog
//! (selects an option, dismisses), the task is resumed with a result
//! table and continues executing.
//!
//! Supported panel kinds (from `opts.panels[]`):
//! - `{ kind = "content",  text = "..." }`      plain lines
//! - `{ kind = "markdown", text = "..." }`      rendered via `render_markdown_inner`
//! - `{ kind = "options",  items = [{label, action?, shortcut?, on_select?}] }`
//! - `{ kind = "input",    name = "x", placeholder? = "..." }`

use super::super::App;
use crate::app::ops::AppOp;
use crate::keymap::hints;
use mlua::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;
use ui::buffer::BufCreateOpts;
use ui::text_input::TextInput;
use ui::{
    BufId, Callback, CallbackResult, OptionItem, OptionList, PanelHeight, PanelSpec, Payload,
    WinEvent, WinId,
};

/// Per-option data consumed when the task resumes. Drained out of
/// `LuaDialogState` into the [`AppOp::ResolveLuaDialog`] payload when
/// the user selects an option, so the RegistryKey (which isn't Clone)
/// moves cleanly from the dialog's state to the reducer.
struct OptionEntry {
    /// Action string reported back to the task (`result.action`).
    /// Defaults to `"select"` when the plugin didn't specify one.
    action: String,
    /// Optional `on_select` callback fired *before* the task resumes.
    on_select: Option<mlua::RegistryKey>,
}

/// Per-input panel, to gather final text on resolution.
struct InputEntry {
    name: String,
    panel_index: usize,
}

/// In-closure state for a Lua dialog. Held by `Rc<RefCell>` so the
/// Submit and Dismiss callbacks share the same options / inputs.
struct LuaDialogState {
    dialog_id: u64,
    options: Vec<OptionEntry>,
    inputs: Vec<InputEntry>,
}

/// Open the dialog described by the Lua `opts_key` table. On success,
/// registers Submit/Dismiss callbacks that resume the parked task.
/// Returns `Err` when the table is malformed — the caller should
/// resolve the parked task with an error string.
pub fn open(app: &mut App, dialog_id: u64, opts_key: mlua::RegistryKey) -> Result<(), String> {
    let opts: mlua::Table = app
        .lua
        .lua()
        .registry_value(&opts_key)
        .map_err(|e| format!("dialog opts: {e}"))?;
    let title: Option<String> = opts.get("title").ok();
    let panels_tbl: mlua::Table = opts
        .get("panels")
        .map_err(|e| format!("dialog panels: {e}"))?;

    let mut panel_specs: Vec<PanelSpec> = Vec::new();
    let mut options: Vec<OptionEntry> = Vec::new();
    let mut inputs: Vec<InputEntry> = Vec::new();

    for pair in panels_tbl.sequence_values::<mlua::Table>() {
        let panel = pair.map_err(|e| format!("dialog panel entry: {e}"))?;
        let kind: String = panel.get("kind").map_err(|e| format!("panel.kind: {e}"))?;
        let panel_index = panel_specs.len();
        match kind.as_str() {
            "content" => {
                let text: String = panel.get("text").unwrap_or_default();
                let buf = make_text_buffer(app, &text);
                panel_specs.push(PanelSpec::content(buf, PanelHeight::Fit));
            }
            "markdown" => {
                let text: String = panel.get("text").unwrap_or_default();
                let buf = make_markdown_buffer(app, &text);
                panel_specs.push(PanelSpec::content(buf, PanelHeight::Fit));
            }
            "options" => {
                let items_tbl: mlua::Table = panel
                    .get("items")
                    .map_err(|e| format!("options.items: {e}"))?;
                let mut list_items = Vec::new();
                for it_pair in items_tbl.sequence_values::<mlua::Table>() {
                    let it = it_pair.map_err(|e| format!("option item: {e}"))?;
                    let label: String = it.get("label").unwrap_or_default();
                    let shortcut: Option<char> = it
                        .get::<String>("shortcut")
                        .ok()
                        .and_then(|s| s.chars().next());
                    let mut item = OptionItem::new(label);
                    if let Some(c) = shortcut {
                        item = item.with_shortcut(c);
                    }
                    let action: String = it.get("action").unwrap_or_else(|_| "select".into());
                    let on_select = it
                        .get::<mlua::Function>("on_select")
                        .ok()
                        .and_then(|f| app.lua.lua().create_registry_value(f).ok());
                    list_items.push(item);
                    options.push(OptionEntry { action, on_select });
                }
                let multi: bool = panel.get("multi").unwrap_or(false);
                let widget = Box::new(
                    OptionList::new(list_items)
                        .multi(multi)
                        .with_cursor_style(accent_style())
                        .with_shortcut_style(accent_style()),
                );
                panel_specs.push(PanelSpec::widget(widget, PanelHeight::Fit));
            }
            "input" => {
                let name: String = panel.get("name").unwrap_or_default();
                let placeholder: Option<String> = panel.get("placeholder").ok();
                let mut ti = TextInput::new();
                if let Some(p) = placeholder {
                    ti = ti.with_placeholder(p);
                }
                let widget = Box::new(ti);
                panel_specs.push(PanelSpec::widget(widget, PanelHeight::Fit));
                inputs.push(InputEntry { name, panel_index });
            }
            other => return Err(format!("unknown panel kind: {other}")),
        }
    }

    if panel_specs.is_empty() {
        return Err("dialog must have at least one panel".into());
    }

    let dialog_config =
        app.builtin_dialog_config(Some(hints::join(&[hints::CONFIRM, hints::CANCEL])), vec![]);
    // Lua dialogs block the agent event drain until the task resumes.
    let win_id = app
        .ui
        .dialog_open(
            ui::FloatConfig {
                title,
                border: ui::Border::None,
                placement: ui::Placement::dock_bottom_full_width(ui::Constraint::Pct(60)),
                blocks_agent: true,
                ..Default::default()
            },
            dialog_config,
            panel_specs,
        )
        .ok_or_else(|| "failed to open dialog window".to_string())?;

    let state = Rc::new(RefCell::new(LuaDialogState {
        dialog_id,
        options,
        inputs,
    }));

    let ops = app.lua.ops_handle();

    let state_submit = state.clone();
    let ops_submit = ops.clone();
    app.ui.win_on_event(
        win_id,
        WinEvent::Submit,
        Callback::Rust(Box::new(move |ctx| {
            let idx = match ctx.payload {
                Payload::Selection { index } => index,
                _ => 0,
            };
            let mut s = state_submit.borrow_mut();
            let (action, on_select) = match s.options.get_mut(idx) {
                Some(entry) => (entry.action.clone(), entry.on_select.take()),
                None => ("select".to_string(), None),
            };
            let inputs = collect_inputs(ctx.ui, ctx.win, &s.inputs);
            ops_submit.push(AppOp::ResolveLuaDialog {
                dialog_id: s.dialog_id,
                action,
                option_index: Some(idx + 1),
                inputs,
                on_select,
            });
            ops_submit.push(AppOp::CloseFloat(ctx.win));
            CallbackResult::Consumed
        })),
    );

    let state_dismiss = state;
    app.ui.win_on_event(
        win_id,
        WinEvent::Dismiss,
        Callback::Rust(Box::new(move |ctx| {
            let s = state_dismiss.borrow();
            ops.push(AppOp::ResolveLuaDialog {
                dialog_id: s.dialog_id,
                action: "dismiss".into(),
                option_index: None,
                inputs: Vec::new(),
                on_select: None,
            });
            ops.push(AppOp::CloseFloat(ctx.win));
            CallbackResult::Consumed
        })),
    );

    Ok(())
}

fn collect_inputs(ui: &mut ui::Ui, win: WinId, entries: &[InputEntry]) -> Vec<(String, String)> {
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        let text = ui
            .dialog_mut(win)
            .and_then(|d| d.panel_widget_mut::<TextInput>(entry.panel_index))
            .map(|w| w.text().to_string())
            .unwrap_or_default();
        out.push((entry.name.clone(), text));
    }
    out
}

/// Build the result table the Lua task resumes with. Called by
/// `apply_ops` when handling [`AppOp::ResolveLuaDialog`].
pub(crate) fn build_result(
    lua: &Lua,
    action: &str,
    option_index: Option<usize>,
    inputs: Vec<(String, String)>,
) -> LuaResult<mlua::Value> {
    let t = lua.create_table()?;
    t.set("action", action)?;
    if let Some(i) = option_index {
        t.set("option_index", i)?;
    }
    let inputs_tbl = lua.create_table()?;
    for (k, v) in inputs {
        inputs_tbl.set(k, v)?;
    }
    t.set("inputs", inputs_tbl)?;
    Ok(mlua::Value::Table(t))
}

fn accent_style() -> ui::grid::Style {
    ui::grid::Style {
        fg: Some(crate::theme::accent()),
        ..Default::default()
    }
}

fn make_text_buffer(app: &mut App, text: &str) -> BufId {
    let id = app.ui.buf_create(BufCreateOpts::default());
    if let Some(buf) = app.ui.buf_mut(id) {
        let lines: Vec<String> = if text.is_empty() {
            vec![String::new()]
        } else {
            text.lines().map(|s| s.to_string()).collect()
        };
        buf.set_all_lines(lines);
    }
    id
}

fn make_markdown_buffer(app: &mut App, text: &str) -> BufId {
    let id = app.ui.buf_create(BufCreateOpts::default());
    let theme = crate::theme::snapshot();
    // Width is unknown at buffer-build time (the float hasn't been
    // placed yet). Use a conservative default; markdown wrapping will
    // reflow when the dialog picks its rect on first draw.
    let width: u16 = 80;
    if let Some(buf) = app.ui.buf_mut(id) {
        crate::render::to_buffer::render_into_buffer(buf, width, &theme, |sink| {
            crate::render::blocks::render_markdown_inner(
                sink,
                text,
                width as usize,
                " ",
                false,
                None,
            );
        });
    }
    id
}
