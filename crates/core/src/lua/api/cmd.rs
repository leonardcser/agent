//! `smelt.cmd` - register/list slash commands. `run` is added by the TUI after host-API init.

use crate::lua::doc::Tier;
use crate::lua::lua_type::LuaCallback;
use crate::lua::module::LuaMod;
use crate::lua::reg::LuaReg;
use crate::lua::{CommandBusyBehavior, LuaHandle, LuaShared, RegisteredCommand};
use lua_doc_derive::LuaOpts;
use mlua::prelude::*;
use std::sync::Arc;

/// Options accepted by `smelt.cmd.register`.
#[derive(Default, Debug, LuaOpts)]
#[lua(name = "smelt.cmd.RegisterOpts")]
pub struct LuaCmdRegisterOpts {
    /// Human-readable description shown in `/help` and the slash-command picker.
    pub desc: Option<String>,
    /// Positional argument labels used for help text and completion hints.
    #[lua(default)]
    pub args: Vec<String>,
    /// Return live argument labels for this command's hint. Overrides `args`;
    /// errors or invalid results fall back to `args`.
    pub args_fn: Option<LuaCallback<(), Vec<String>>>,
    /// Busy behavior while an agent turn is running: `run` (default), `reject`, `queue_request`, or `queue_command`.
    pub busy: Option<String>,
    /// If true, the command may run before the runtime has finished bootstrapping. Defaults to `false`.
    pub startup_ok: Option<bool>,
    /// If true, the command is hidden from `/help` and the picker (still callable). Defaults to `false`.
    pub hidden: Option<bool>,
    /// If true, replace an existing command with the same name. Defaults to `false`.
    #[lua(rename = "override", default)]
    pub override_existing: bool,
}

fn parse_busy_behavior(value: Option<String>) -> Result<CommandBusyBehavior, String> {
    match value.as_deref().unwrap_or("run") {
        "run" => Ok(CommandBusyBehavior::Run),
        "reject" => Ok(CommandBusyBehavior::Reject),
        "queue_request" => Ok(CommandBusyBehavior::QueueRequest),
        "queue_command" => Ok(CommandBusyBehavior::QueueCommand),
        other => Err(format!(
            "invalid busy behavior {other:?}; expected run, reject, queue_request, or queue_command"
        )),
    }
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let m = LuaMod::supported(
        lua,
        smelt,
        "cmd",
        "Register and list slash commands. `cmd.run` is injected by the TUI layer so it can access the live app state.",
        Tier::Host,
    )?;
    {
        let s = shared.clone();
        m.fn_(
            "register",
            "Register a slash command `name` whose `handler` is invoked when the user runs it. `opts` accepts `desc`, `args`, `args_fn` (live argument labels), `busy` (`run`, `reject`, `queue_request`, or `queue_command`; default `run`), `startup_ok` (default `false`), `hidden` (default `false`), and `override` (default `false`). Returns a `Reg` whose `:remove()` unregisters the command.",
            &["name", "handler", "opts"],
            move |lua,
                  (name, handler, opts): (
                String,
                LuaCallback<Option<String>, ()>,
                Option<LuaCmdRegisterOpts>,
            )|
                  -> LuaResult<LuaReg> {
                let opts = opts.unwrap_or_default();
                let busy = parse_busy_behavior(opts.busy).map_err(LuaError::RuntimeError)?;
                let handle = LuaHandle::from_func(lua, handler.into_inner())?;
                let token = s
                    .register_command(
                        name.clone(),
                        RegisteredCommand {
                            handle,
                            token: 0,
                            description: opts.desc,
                            args: opts.args,
                            args_fn: opts.args_fn
                                .map(|callback| LuaHandle::from_func(lua, callback.into_inner()))
                                .transpose()?,
                            busy,
                            startup_ok: opts.startup_ok.unwrap_or(false),
                            hidden: opts.hidden.unwrap_or(false),
                        },
                        opts.override_existing,
                    )
                    .map_err(LuaError::RuntimeError)?;
                let s_for_reg = s.clone();
                Ok(LuaReg::new(move || {
                    s_for_reg.unregister_command_token(&name, token)
                }))
            },
        )?;
    }
    {
        let s = shared.clone();
        m.fn_(
            "list",
            "Return every registered slash command as a Lua array of `{ name, desc, args, args_fn, busy, startup_ok, hidden }` rows. Sorted by name. Argument callbacks are returned without being invoked.",
            &[],
            move |lua, ()| -> LuaResult<mlua::Table> {
                struct Row {
                    name: String,
                    desc: Option<String>,
                    args: Vec<String>,
                    args_fn: Option<mlua::Function>,
                    busy: &'static str,
                    startup_ok: bool,
                    hidden: bool,
                }
                let rows: Vec<Row> = s
                    .commands
                    .lock()
                    .map(|m| -> LuaResult<Vec<Row>> {
                        let mut rows = m
                            .iter()
                            .map(|(name, cmd)| -> LuaResult<Row> {
                                Ok(Row {
                                    name: name.clone(),
                                    desc: cmd.description.clone(),
                                    args: cmd.args.clone(),
                                    args_fn: cmd.args_fn.as_ref()
                                        .map(|handle| lua.registry_value(&handle.key))
                                        .transpose()?,
                                    busy: cmd.busy.as_str(),
                                    startup_ok: cmd.startup_ok,
                                    hidden: cmd.hidden,
                                })
                            })
                            .collect::<LuaResult<Vec<_>>>()?;
                        rows.sort_by(|a, b| a.name.cmp(&b.name));
                        Ok(rows)
                    })
                    .unwrap_or_else(|_| Ok(Vec::new()))?;
                let table = lua.create_table()?;
                for (
                    i,
                    Row {
                        name,
                        desc,
                        args,
                        args_fn,
                        busy,
                        startup_ok,
                        hidden,
                    },
                ) in rows.into_iter().enumerate()
                {
                    let row = lua.create_table()?;
                    row.set("name", name)?;
                    if let Some(d) = desc {
                        row.set("desc", d)?;
                    }
                    row.set("args_fn", args_fn)?;
                    let args_tbl = lua.create_table()?;
                    for (j, a) in args.iter().enumerate() {
                        args_tbl.set(j + 1, a.as_str())?;
                    }
                    row.set("args", args_tbl)?;
                    row.set("busy", busy)?;
                    row.set("startup_ok", startup_ok)?;
                    row.set("hidden", hidden)?;
                    table.set(i + 1, row)?;
                }
                Ok(table)
            },
        )?;
    }
    Ok(())
}
