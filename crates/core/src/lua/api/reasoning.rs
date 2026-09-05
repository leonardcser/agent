//! `smelt.reasoning` - reasoning-effort selector. Mirrors `smelt.mode`.
//! `smelt.reasoning.current()` reads, `smelt.reasoning.set(v)` sets in a TUI
//! session, and `smelt.reasoning.cycle_list()` returns the configured cycle.

use crate::lua::doc::Tier;
use crate::lua::module::LuaMod;
use mlua::prelude::*;

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::supported(
        lua,
        smelt,
        "reasoning",
        "Reasoning-effort selector. `smelt.reasoning.current()` reads the active effort, `smelt.reasoning.set(v)` sets it in a TUI session, and `smelt.reasoning.cycle_list()` lists the configured cycle.",
        Tier::Host,
    )?;
    m.fn_(
        "cycle_list",
        "Return the configured reasoning-effort cycle.",
        &[],
        |_, ()| -> LuaResult<Vec<String>> {
            Ok(crate::host::try_with_core(|core| {
                core.config
                    .reasoning_cycle
                    .iter()
                    .map(|effort| effort.label().to_string())
                    .collect()
            })
            .unwrap_or_default())
        },
    )?;

    m.fn_(
        "options",
        "Return `{ efforts, default }` for the active model. `efforts` contains supported native labels; an empty list means the supported levels are unknown. `default` is the model's default effort when known.",
        &[],
        |lua, ()| -> LuaResult<mlua::Table> {
            let catalog = crate::host::try_with_core(|core| {
                core.config.active_model().map(|model| model.reasoning_catalog())
            })
            .flatten()
            .unwrap_or_default();
            let efforts = catalog.supported_reasoning_efforts.iter()
                .map(|effort| effort.label().to_string())
                .collect::<Vec<_>>();
            let default = catalog.default_reasoning_effort
                .map(|effort| effort.label().to_string());
            let out = lua.create_table()?;
            out.set("efforts", efforts)?;
            out.set("default", default)?;
            Ok(out)
        },
    )?;

    m.fn_(
        "known_list",
        "Return the reasoning-effort labels known by this smelt version. Models may advertise additional labels.",
        &[],
        |_, ()| -> LuaResult<Vec<String>> {
            Ok(protocol::ReasoningEffort::KNOWN
                .iter()
                .map(|effort| effort.label().to_string())
                .collect())
        },
    )?;

    m.fn_(
        "current",
        "Return the active reasoning effort.",
        &[],
        |_, ()| -> LuaResult<String> {
            Ok(
                crate::host::try_with_core(|core| core.config.reasoning_effort.label().to_string())
                    .unwrap_or_else(|| "off".to_string()),
            )
        },
    )?;

    Ok(())
}
