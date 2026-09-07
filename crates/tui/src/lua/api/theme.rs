//! `smelt.theme` bindings - apply a colorscheme spec, read/write
//! individual groups, snapshot the resolved theme.
//!
//! A colorscheme is a `ThemeSpec` table with optional metadata and a required
//! `groups` map. Pass the table to `apply()` or have
//! `runtime/lua/smelt/colorschemes/<name>.lua` return one and call
//! `theme.use("<name>")`.

use crate::theme::{StyleDecl, ThemeSpec};
use mlua::prelude::*;
use smelt_core::content::highlight::syntax_theme_names;
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;
use smelt_core::style::{Color, Style};

#[derive(Clone)]
struct ThemeMetadata {
    is_light: bool,
    syntax_theme: Option<String>,
}

/// Host-tier render callbacks can read theme metadata without borrowing the UI.
pub(crate) fn sync_metadata(lua: &Lua, theme: &smelt_core::theme::Theme) {
    lua.set_app_data(ThemeMetadata {
        is_light: theme.is_light(),
        syntax_theme: theme.syntax_theme().map(str::to_owned),
    });
}

pub(crate) fn inherit_metadata(source: &Lua, target: &Lua) {
    if let Some(metadata) = source.app_data_ref::<ThemeMetadata>() {
        target.set_app_data(metadata.clone());
    }
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    sync_metadata(lua, crate::theme::default_baked());
    let m = LuaMod::supported(
        lua,
        smelt,
        "theme",
        "List bundled colorschemes, or apply, read, and override the active one. Highlight \
groups follow nvim's PascalCase convention (`Comment`, `SmeltAccent`, …). \
A `ThemeSpec` has optional `name`, `syntax`, and `light` metadata plus a \
required `groups` map. Catalog and active-theme metadata reads are Host-tier; \
highlight-group access and mutations require UiHost.",
        Tier::UiHost,
    )?;

    m.fn_(
        "apply",
        "Compile `spec` against the current light/dark setting and \
install it as the active theme. String-valued group entries are resolved \
at compile time; cycles and dangling references raise a runtime error.",
        &["spec"],
        |lua, spec: ThemeSpec| -> LuaResult<()> {
            crate::lua::with_runtime_host(|host| host.apply_theme(lua, &spec))
                .map_err(|error| LuaError::RuntimeError(format!("theme.apply: {error}")))
        },
    )?;

    m.fn_(
        "set",
        "Override a single highlight group's style. `style` is a \
`StyleDecl` table (`{ fg = { ansi = 244 }, bold = true }`). The override \
sticks until the next `apply()` or `use()` call.",
        &["group", "style"],
        |lua, (group, style): (String, StyleDecl)| -> LuaResult<()> {
            crate::lua::with_runtime_host(|host| {
                let style = style_decl_to_style(&style, host.theme_is_light());
                host.set_theme_group(lua, group, style);
            });
            Ok(())
        },
    )?;

    m.fn_(
        "get",
        "Read the resolved `StyleDecl` for `group`. Unknown groups \
resolve to an empty table.",
        &["group"],
        |lua, group: String| -> LuaResult<mlua::Table> {
            let style = crate::lua::with_runtime_host(|host| host.theme_group(&group));
            style_to_lua(lua, style)
        },
    )?;

    m.fn_(
        "snapshot",
        "Snapshot every group currently set on the active theme into a \
`{ group = StyleDecl }` table.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let pairs = crate::lua::with_runtime_host(|host| host.theme_snapshot());
            let out = lua.create_table()?;
            for (name, style) in pairs {
                out.set(name, style_to_lua(lua, style)?)?;
            }
            Ok(out)
        },
    )?;

    let host = LuaMod::extend_supported(lua, m.tbl.clone(), "smelt.theme", Tier::Host);
    host.fn_(
        "is_light",
        "Return `true` if the active theme is a light theme. Lets \
plugins flip glyphs or contrast levels based on the current palette.",
        &[],
        |lua, ()| {
            Ok(lua
                .app_data_ref::<ThemeMetadata>()
                .expect("theme metadata is initialized")
                .is_light)
        },
    )?;

    host.fn_(
        "syntax_theme",
        "Return the active bundled syntect/two-face syntax theme name, if the colorscheme set one.",
        &[],
        |lua, ()| {
            Ok(lua
                .app_data_ref::<ThemeMetadata>()
                .expect("theme metadata is initialized")
                .syntax_theme
                .clone())
        },
    )?;

    host.fn_(
        "syntax_theme_names",
        "Return all bundled syntect/two-face syntax theme names accepted by `ThemeSpec.syntax`.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let out = lua.create_table()?;
            for name in syntax_theme_names() {
                out.push(name)?;
            }
            Ok(out)
        },
    )?;

    Ok(())
}

/// Project a `Style` to a Lua `StyleDecl` table. Unset fields are
/// omitted so `snapshot()` round-trips cleanly back through `apply()`.
fn style_to_lua(lua: &Lua, style: Style) -> LuaResult<mlua::Table> {
    let t = lua.create_table()?;
    if let Some(fg) = style.fg {
        t.set("fg", color_to_lua(lua, fg)?)?;
    }
    if let Some(bg) = style.bg {
        t.set("bg", color_to_lua(lua, bg)?)?;
    }
    if style.bold {
        t.set("bold", true)?;
    }
    if style.italic {
        t.set("italic", true)?;
    }
    if style.dim {
        t.set("dim", true)?;
    }
    if style.underline {
        t.set("underline", true)?;
    }
    if style.crossedout {
        t.set("crossedout", true)?;
    }
    if style.reverse {
        t.set("reverse", true)?;
    }
    Ok(t)
}

fn color_to_lua(lua: &Lua, color: Color) -> LuaResult<mlua::Table> {
    let t = lua.create_table()?;
    match color {
        Color::AnsiValue(v) => t.set("ansi", v)?,
        Color::Rgb { r, g, b } => t.set("rgb", [r, g, b])?,
        other => {
            // Project the named colors back to their canonical ANSI slot.
            if let Some(idx) = super::color_to_ansi(other) {
                t.set("ansi", idx)?;
            }
        }
    }
    Ok(t)
}

fn style_decl_to_style(decl: &StyleDecl, is_light: bool) -> Style {
    Style {
        fg: decl.fg.as_ref().and_then(|c| c.to_color(is_light)),
        bg: decl.bg.as_ref().and_then(|c| c.to_color(is_light)),
        bold: decl.bold.unwrap_or(false),
        italic: decl.italic.unwrap_or(false),
        dim: decl.dim.unwrap_or(false),
        underline: decl.underline.unwrap_or(false),
        crossedout: decl.crossedout.unwrap_or(false),
        reverse: decl.reverse.unwrap_or(false),
    }
}
