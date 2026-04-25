//! Lua → ui translators for `smelt.ui.dialog.open` and
//! `smelt.ui.picker.open`. These parse the plugin-supplied `opts`
//! tables into the typed `PanelSpec` / `PickerItem` shapes that the
//! `ui` crate consumes, create the compositor float, and hand the new
//! `WinId` back to the parked Lua task. Everything else — keymap
//! callbacks, submit / dismiss routing, selection tracking —
//! lives in `runtime/lua/smelt/{dialog,picker}.lua`.

use crate::app::App;
use crate::format::BufFormat;
use ui::buffer::BufCreateOpts;
use ui::text_input::TextInput;
use ui::{
    BufId, BufferList, Constraint, FitMax, FloatConfig, OptionItem, OptionList, PanelHeight,
    PanelSpec, Placement, SeparatorStyle, WinId,
};

// ── Dialog ───────────────────────────────────────────────────────────
//
// Supported panel kinds from `opts.panels[]`:
// - `{ kind = "content", buf = <id> }`                       — existing buffer (plain or formatter-backed)
// - `{ kind = "content", text = "..." }`                     — soft-wrapped plain text
// - `{ kind = "content", text = "...", mode = "markdown" }`  — formatter-rendered text
//                                                              (also accepts "bash", "file", "diff")
// - `{ kind = "markdown", text = "..." }`                    — sugar for the line above with mode = "markdown"
// - `{ kind = "options",  items = [{label, shortcut?}], selected? = <1-based index> }`
// - `{ kind = "input",    placeholder? = "..." }`
// - `{ kind = "list",     buf = <id> }`

pub fn open_dialog(app: &mut App, opts: mlua::Table) -> Result<WinId, String> {
    let title: Option<String> = opts.get("title").ok();
    let panels_tbl: mlua::Table = opts
        .get("panels")
        .map_err(|e| format!("dialog panels: {e}"))?;

    let mut panel_specs: Vec<PanelSpec> = Vec::new();
    for pair in panels_tbl.sequence_values::<mlua::Table>() {
        let panel = pair.map_err(|e| format!("dialog panel entry: {e}"))?;
        let kind: String = panel.get("kind").map_err(|e| format!("panel.kind: {e}"))?;
        let height = parse_panel_height(&panel)?;
        let initial_focus: bool = panel.get("focus").unwrap_or(false);
        match kind.as_str() {
            "content" => {
                let buf = if let Ok(id) = panel.get::<u64>("buf") {
                    BufId(id)
                } else {
                    let text: String = panel.get("text").unwrap_or_default();
                    // Inline text panels default to the plain formatter
                    // so help dialogs / help-style content wrap at the
                    // current panel width instead of being clipped.
                    // Callers who want raw unwrapped lines can supply a
                    // pre-built buffer via `buf = ...`.
                    let format = parse_panel_mode(&panel)?.or(Some(BufFormat::Plain));
                    make_content_buffer(app, &text, format)
                };
                let focusable: bool = panel.get("focusable").unwrap_or(false);
                let interactive: bool = panel.get("interactive").unwrap_or(false);
                let pad_left: u16 = panel.get("pad_left").unwrap_or(0);
                // `interactive` upgrades the panel to a transcript-style
                // window: click + double/triple click + drag select all
                // ride on the same `ui::Window` primitive as the
                // transcript pane. Implies focusable.
                let mut spec = if interactive {
                    PanelSpec::interactive_content(buf, height.unwrap_or(PanelHeight::Fit))
                } else {
                    PanelSpec::content(buf, height.unwrap_or(PanelHeight::Fit))
                };
                if pad_left > 0 {
                    spec = spec.with_pad_left(pad_left);
                }
                spec = spec
                    .focusable(focusable || interactive)
                    .with_initial_focus(initial_focus);
                if let Some(sep) = parse_separator(&panel)? {
                    spec = spec.with_separator(sep);
                }
                if panel.get::<bool>("collapse_when_empty").unwrap_or(false) {
                    spec.collapse_when_empty = true;
                }
                panel_specs.push(spec);
            }
            "markdown" => {
                let text: String = panel.get("text").unwrap_or_default();
                let buf = make_content_buffer(app, &text, Some(BufFormat::Markdown));
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
                let mut option_list = OptionList::new(list_items)
                    .multi(multi)
                    .with_cursor_style(accent_style())
                    .with_shortcut_style(accent_style());
                if let Ok(selected) = panel.get::<i64>("selected") {
                    if selected >= 1 {
                        option_list = option_list.with_cursor((selected - 1) as usize);
                    }
                }
                let widget = Box::new(option_list);
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
                let mut spec =
                    PanelSpec::widget(widget, PanelHeight::Fit).with_initial_focus(initial_focus);
                if panel.get::<bool>("collapse_when_empty").unwrap_or(false) {
                    spec.collapse_when_empty = true;
                }
                panel_specs.push(spec);
            }
            "list" => {
                let buf_id: u64 = panel.get("buf").map_err(|e| format!("list.buf: {e}"))?;
                let bg = ui::grid::Style {
                    bg: Some(crossterm::style::Color::Black),
                    ..Default::default()
                };
                let track = ui::grid::Style {
                    bg: Some(crate::theme::scrollbar_track()),
                    ..Default::default()
                };
                let thumb = ui::grid::Style {
                    bg: Some(crate::theme::scrollbar_thumb()),
                    ..Default::default()
                };
                let widget = Box::new(
                    BufferList::new(BufId(buf_id))
                        .with_cursor_style(accent_style())
                        .with_bg_style(bg)
                        .with_scrollbar_styles(track, thumb),
                );
                panel_specs.push(
                    PanelSpec::widget(widget, height.unwrap_or(PanelHeight::Fill))
                        .with_pad_left(2)
                        .with_initial_focus(initial_focus),
                );
            }
            other => return Err(format!("unknown panel kind: {other}")),
        }
    }

    if panel_specs.is_empty() {
        return Err("dialog must have at least one panel".into());
    }

    // Collect footer hints from top-level `opts.keymaps[].hint`. The Lua
    // side handles the actual keymap registration; Rust only needs the
    // hint strings to build the footer.
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

    // `blocks_agent` gates engine-event drain + queues new user input. Only
    // dialogs that gate an agent decision (permission prompts,
    // `ask_user_question`, `exit_plan_mode`) should opt in — passive viewers
    // like `/help`, `/btw`, `/ps` must let engine responses flow through.
    let blocks_agent: bool = opts.get("blocks_agent").unwrap_or(false);
    let placement = parse_dialog_placement(&opts);

    let vim_enabled = app.input.vim_enabled();
    let win_id = app
        .ui
        .dialog_open(
            FloatConfig {
                title,
                border: ui::Border::None,
                placement,
                blocks_agent,
                ..Default::default()
            },
            dialog_config,
            panel_specs,
        )
        .ok_or_else(|| "failed to open dialog window".to_string())?;
    // Mirror the transcript's selection model on interactive buffer
    // panels — vim Visual gives inclusive selection so dragging
    // "hello" yanks all five chars, not "hell".
    if let Some(dialog) = app.ui.dialog_mut(win_id) {
        dialog.set_vim_enabled_on_interactive(vim_enabled);
    }
    Ok(win_id)
}

fn parse_separator(panel: &mlua::Table) -> Result<Option<SeparatorStyle>, String> {
    match panel.get::<String>("separator").ok().as_deref() {
        Some("dashed") => Ok(Some(SeparatorStyle::Dashed)),
        Some("solid") => Ok(Some(SeparatorStyle::Solid)),
        Some(other) => Err(format!(
            "panel.separator: unknown style {other:?} (expected \"dashed\" or \"solid\")"
        )),
        None => Ok(None),
    }
}

/// Top-level `placement` option on `smelt.ui.dialog._open`. Defaults
/// to `fit_content(HalfScreen)` (compact center-floating dialog).
/// `"dock_bottom"` docks full-width at the bottom; an optional
/// `placement_height = <pct>` caps it (e.g. `60` → `Pct(60)`).
fn parse_dialog_placement(opts: &mlua::Table) -> Placement {
    match opts.get::<String>("placement").ok().as_deref() {
        Some("dock_bottom") => {
            let pct: u16 = opts.get("placement_height").unwrap_or(60);
            Placement::dock_bottom_full_width(Constraint::Pct(pct))
        }
        _ => Placement::fit_content(FitMax::HalfScreen),
    }
}

// ── Picker ───────────────────────────────────────────────────────────

pub fn open_picker(app: &mut App, opts: mlua::Table) -> Result<WinId, String> {
    let items_tbl: mlua::Table = opts
        .get("items")
        .map_err(|e| format!("picker items: {e}"))?;
    let mut items: Vec<ui::picker::PickerItem> = Vec::new();
    for pair in items_tbl.sequence_values::<mlua::Value>() {
        let v = pair.map_err(|e| format!("picker item: {e}"))?;
        items.push(parse_picker_item(&v)?);
    }
    if items.is_empty() {
        return Err("picker.open: items must be non-empty".into());
    }

    let placement_str: String = opts
        .get("placement")
        .ok()
        .unwrap_or_else(|| "center".to_string());
    let prompt_docked = placement_str == "prompt_docked";
    let placement = parse_picker_placement(&opts);
    let title: Option<String> = opts.get("title").ok();

    // `prompt_docked`: no border, non-focusable, reversed (best match
    // closest to the prompt). Placement::DockedAbove handles rect +
    // natural-height resolution in the ui crate — no TUI sync loop.
    let (border, focusable, reversed) = if prompt_docked {
        (ui::Border::None, false, true)
    } else {
        (ui::Border::Rounded, true, false)
    };
    let zindex = if prompt_docked { 60 } else { 50 };
    let float_config = FloatConfig {
        title,
        border,
        placement,
        focusable,
        blocks_agent: false,
        zindex,
    };

    let style = if prompt_docked {
        ui::PickerStyle {
            selected_fg: ui::grid::Style {
                fg: Some(crate::theme::accent()),
                ..Default::default()
            },
            unselected_fg: ui::grid::Style::dim(),
            description_fg: ui::grid::Style::dim(),
            background: ui::grid::Style::default(),
        }
    } else {
        Default::default()
    };

    app.ui
        .picker_open(float_config, items, 0, style, reversed)
        .ok_or_else(|| "picker.open: failed to create float".to_string())
}

// ── Helpers ──────────────────────────────────────────────────────────

/// Parse a per-panel `height` option. `"fit"` → Fit (auto-shrink),
/// `"fill"` → Fill (stretch + scroll), integer → Fixed rows. Absent →
/// Ok(None) so the caller can pick a kind-appropriate default.
fn parse_panel_height(panel: &mlua::Table) -> Result<Option<PanelHeight>, String> {
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

pub fn parse_picker_item(v: &mlua::Value) -> Result<ui::picker::PickerItem, String> {
    match v {
        mlua::Value::String(s) => Ok(ui::picker::PickerItem::new(s.to_string_lossy().to_string())),
        mlua::Value::Table(t) => {
            let label: String = t
                .get("label")
                .map_err(|e| format!("picker item.label: {e}"))?;
            let mut item = ui::picker::PickerItem::new(label);
            if let Ok(desc) = t.get::<String>("description") {
                item = item.with_description(desc);
            }
            if let Ok(prefix) = t.get::<String>("prefix") {
                item = item.with_prefix(prefix);
            }
            if let Ok(Some(ansi)) = t.get::<Option<u64>>("ansi_color") {
                item = item.with_accent(crossterm::style::Color::AnsiValue(ansi as u8));
            }
            Ok(item)
        }
        other => Err(format!(
            "picker item: expected string or table, got {}",
            other.type_name()
        )),
    }
}

fn parse_picker_placement(opts: &mlua::Table) -> Placement {
    let mode: String = opts
        .get("placement")
        .ok()
        .unwrap_or_else(|| "center".to_string());
    match mode.as_str() {
        "bottom" => Placement::dock_bottom_full_width(Constraint::Pct(40)),
        "cursor" => Placement::AnchorCursor {
            row_offset: 1,
            col_offset: 0,
            width: Constraint::Fixed(48),
            height: Constraint::Pct(40),
        },
        "prompt_docked" => Placement::docked_above(ui::PROMPT_WIN, Constraint::Fixed(7)),
        _ => Placement::centered(Constraint::Pct(60), Constraint::Pct(50)),
    }
}

fn accent_style() -> ui::grid::Style {
    ui::grid::Style {
        fg: Some(crate::theme::accent()),
        ..Default::default()
    }
}

/// Build a buffer for a `content` / `markdown` panel. `format = None`
/// produces a plain buffer with raw lines (no wrapping, no
/// formatter) — matches the pre-formatter behaviour. `format =
/// Some(mode)` installs the matching formatter and seeds its source
/// with `text`; the dialog drives re-rendering at the panel's
/// content width, so the baked-at-open width=80 trap of the old
/// markdown path is gone.
fn make_content_buffer(app: &mut App, text: &str, format: Option<BufFormat>) -> BufId {
    let id = app.ui.buf_create(BufCreateOpts::default());
    if let Some(buf) = app.ui.buf_mut(id) {
        match format {
            Some(fmt) => {
                buf.set_formatter(fmt.into_formatter());
                buf.set_source(text.to_string());
            }
            None => {
                let lines: Vec<String> = if text.is_empty() {
                    vec![String::new()]
                } else {
                    text.lines().map(|s| s.to_string()).collect()
                };
                buf.set_all_lines(lines);
            }
        }
    }
    id
}

/// Parse an optional `mode = "..."` field off a panel table. Returns
/// `Ok(None)` when absent, `Ok(Some(fmt))` when a valid mode is
/// specified, and `Err(msg)` on unknown modes or missing payload
/// fields (e.g. `mode = "file"` without `path`).
fn parse_panel_mode(panel: &mlua::Table) -> Result<Option<BufFormat>, String> {
    match panel
        .get::<Option<String>>("mode")
        .map_err(|e| e.to_string())?
    {
        Some(mode) => BufFormat::from_lua_spec(&mode, panel).map(Some),
        None => Ok(None),
    }
}
