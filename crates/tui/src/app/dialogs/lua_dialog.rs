//! Dialog opener for `smelt.api.dialog.open` yields. Parses the
//! plugin-supplied `opts` table into a `PanelSpec` list (the part Lua
//! can't own without userdata constructors), creates the dialog float,
//! and hands the new `WinId` back to the parked task. All callback
//! wiring — Submit / Dismiss / custom keymaps / `on_change` /
//! `on_tick` — lives in `runtime/lua/smelt/dialog.lua`; the Lua side
//! uses `ctx.panels` pull-reads to build the final result table and
//! resumes the parked task via `smelt.api.task.resume`.
//!
//! Supported panel kinds (from `opts.panels[]`):
//! - `{ kind = "content",  text = "..." }`      plain lines
//! - `{ kind = "markdown", text = "..." }`      rendered via `render_markdown_inner`
//! - `{ kind = "options",  items = [{label, shortcut?}] }`
//! - `{ kind = "input",    placeholder? = "..." }`
//! - `{ kind = "list",     buf = <id> }`

use super::super::App;
use ui::buffer::BufCreateOpts;
use ui::text_input::TextInput;
use ui::{BufId, FitMax, OptionItem, OptionList, PanelHeight, PanelSpec, WinId};

/// Open the dialog described by the Lua `opts_key` table. On success,
/// returns the new `WinId` so the caller can resolve the parked task
/// with it. Returns `Err` when the table is malformed.
pub fn open(app: &mut App, opts_key: mlua::RegistryKey) -> Result<WinId, String> {
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
    for pair in panels_tbl.sequence_values::<mlua::Table>() {
        let panel = pair.map_err(|e| format!("dialog panel entry: {e}"))?;
        let kind: String = panel.get("kind").map_err(|e| format!("panel.kind: {e}"))?;
        let height = parse_height(&panel)?;
        let initial_focus: bool = panel.get("focus").unwrap_or(false);
        match kind.as_str() {
            "content" => {
                let buf = if let Ok(id) = panel.get::<u64>("buf") {
                    BufId(id)
                } else {
                    let text: String = panel.get("text").unwrap_or_default();
                    make_text_buffer(app, &text)
                };
                let focusable: bool = panel.get("focusable").unwrap_or(false);
                let pad_left: u16 = panel.get("pad_left").unwrap_or(0);
                let mut spec = PanelSpec::content(buf, height.unwrap_or(PanelHeight::Fit));
                if pad_left > 0 {
                    spec = spec.with_pad_left(pad_left);
                }
                spec = spec.focusable(focusable).with_initial_focus(initial_focus);
                panel_specs.push(spec);
            }
            "markdown" => {
                let text: String = panel.get("text").unwrap_or_default();
                let buf = make_markdown_buffer(app, &text);
                panel_specs.push(
                    PanelSpec::content(buf, height.unwrap_or(PanelHeight::Fit))
                        .with_initial_focus(initial_focus),
                );
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
                    list_items.push(item);
                }
                let multi: bool = panel.get("multi").unwrap_or(false);
                let widget = Box::new(
                    OptionList::new(list_items)
                        .multi(multi)
                        .with_cursor_style(accent_style())
                        .with_shortcut_style(accent_style()),
                );
                panel_specs.push(
                    PanelSpec::widget(widget, PanelHeight::Fit).with_initial_focus(initial_focus),
                );
            }
            "input" => {
                let placeholder: Option<String> = panel.get("placeholder").ok();
                let mut ti = TextInput::new();
                if let Some(p) = placeholder {
                    ti = ti.with_placeholder(p);
                }
                let widget = Box::new(ti);
                panel_specs.push(
                    PanelSpec::widget(widget, PanelHeight::Fit).with_initial_focus(initial_focus),
                );
            }
            "list" => {
                let buf_id: u64 = panel.get("buf").map_err(|e| format!("list.buf: {e}"))?;
                panel_specs.push(
                    PanelSpec::list(BufId(buf_id), height.unwrap_or(PanelHeight::Fill))
                        .with_initial_focus(initial_focus),
                );
            }
            other => return Err(format!("unknown panel kind: {other}")),
        }
    }

    if panel_specs.is_empty() {
        return Err("dialog must have at least one panel".into());
    }

    // Collect footer hints from top-level `opts.keymaps[].hint`. The
    // Lua side handles the actual keymap registration; Rust just needs
    // the hint strings to build the footer.
    let mut extra_hints: Vec<String> = Vec::new();
    if let Ok(km_tbl) = opts.get::<mlua::Table>("keymaps") {
        for entry_res in km_tbl.sequence_values::<mlua::Table>() {
            let entry = entry_res.map_err(|e| format!("keymap entry: {e}"))?;
            if let Ok(hint) = entry.get::<String>("hint") {
                if !hint.is_empty() {
                    extra_hints.push(hint);
                }
            }
        }
    }

    let mut hint_parts: Vec<&str> =
        vec![crate::keymap::hints::CONFIRM, crate::keymap::hints::CANCEL];
    for h in &extra_hints {
        hint_parts.push(h.as_str());
    }
    let dialog_config =
        app.builtin_dialog_config(Some(crate::keymap::hints::join(&hint_parts)), vec![]);

    let win_id = app
        .ui
        .dialog_open(
            ui::FloatConfig {
                title,
                border: ui::Border::None,
                placement: ui::Placement::fit_content(FitMax::HalfScreen),
                blocks_agent: true,
                ..Default::default()
            },
            dialog_config,
            panel_specs,
        )
        .ok_or_else(|| "failed to open dialog window".to_string())?;

    Ok(win_id)
}

/// Parse the per-panel `height` option. `"fit"` → Fit (auto-shrink),
/// `"fill"` → Fill (stretch + scroll), an integer → Fixed rows. Absent
/// → Ok(None) so the caller can pick a kind-appropriate default.
fn parse_height(panel: &mlua::Table) -> Result<Option<PanelHeight>, String> {
    match panel.get::<mlua::Value>("height").ok() {
        None | Some(mlua::Value::Nil) => Ok(None),
        Some(mlua::Value::String(s)) => match s.to_str().map_err(|e| e.to_string())?.as_ref() {
            "fit" => Ok(Some(PanelHeight::Fit)),
            "fill" => Ok(Some(PanelHeight::Fill)),
            other => Err(format!("panel.height: unknown value '{other}'")),
        },
        Some(mlua::Value::Integer(n)) if n > 0 => Ok(Some(PanelHeight::Fixed(n as u16))),
        Some(other) => Err(format!(
            "panel.height: expected 'fit' | 'fill' | int, got {other:?}"
        )),
    }
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
