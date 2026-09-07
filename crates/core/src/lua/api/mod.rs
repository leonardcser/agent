//! Host-tier Lua API bindings (TUI and headless).

#[macro_export]
macro_rules! host_read {
    ($lua:expr, |$host:ident| $body:expr) => {{
        $lua.create_function(|_, ()| {
            Ok($crate::host::try_with_core(|$host| $body).unwrap_or_default())
        })?
    }};
}

pub(crate) mod agent;
mod auth;
mod builtins;
mod cli;
mod clipboard;
mod cmd;
mod defaults;
mod diff;
pub mod document;
mod events;
mod files;
mod frontend;
mod fs;
mod fuzzy;
mod grep;
mod html;
mod http;
mod image;
mod json;
pub mod layout;
mod lifecycle;
mod log;
mod lsp;
mod mcp;
mod messages;
pub mod mode;
mod os;
mod parse;
mod path;
mod perf;
mod phase;
mod process;
mod provider;
pub mod reasoning;
mod reg;
mod remember;
mod shell;
mod signal;
mod skills;
mod spawn;
mod state;
mod task;
mod time;
mod timer;
mod tools;
mod transcript;
mod transcript_groups;
mod trust;

use mlua::prelude::*;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

const JSON_ARRAY_REGISTRY_KEY: &str = "__smelt_json_arrays";

fn json_array_registry(lua: &Lua) -> LuaResult<mlua::Table> {
    if let Ok(registry) = lua.named_registry_value::<mlua::Table>(JSON_ARRAY_REGISTRY_KEY) {
        return Ok(registry);
    }

    let registry = lua.create_table()?;
    let metatable = lua.create_table()?;
    metatable.raw_set("__mode", "k")?;
    registry.set_metatable(Some(metatable))?;
    lua.set_named_registry_value(JSON_ARRAY_REGISTRY_KEY, registry.clone())?;
    Ok(registry)
}

pub(super) fn new_json_array(lua: &Lua) -> LuaResult<mlua::Table> {
    let table = lua.create_table()?;
    mark_json_array(lua, &table)?;
    Ok(table)
}

pub(super) fn mark_json_array(lua: &Lua, table: &mlua::Table) -> LuaResult<()> {
    json_array_registry(lua)?.raw_set(table.clone(), true)
}

fn is_json_array(lua: &Lua, table: &mlua::Table) -> bool {
    json_array_registry(lua)
        .and_then(|registry| registry.raw_get::<bool>(table.clone()))
        .unwrap_or(false)
}

/// Convert a Lua table to a `serde_json::Value`. Tables with contiguous
/// 1..N integer keys become JSON arrays; tagged empty arrays stay arrays;
/// anything else becomes an object.
pub fn lua_table_to_json(lua: &Lua, table: &mlua::Table) -> serde_json::Value {
    let mut pairs: Vec<(mlua::Value, mlua::Value)> = Vec::new();
    for pair in table.pairs::<mlua::Value, mlua::Value>() {
        let Ok(kv) = pair else { continue };
        pairs.push(kv);
    }

    let is_array = (pairs.is_empty() && is_json_array(lua, table))
        || (!pairs.is_empty()
            && pairs
                .iter()
                .all(|(k, _)| matches!(k, mlua::Value::Integer(_)))
            && {
                let mut ints: Vec<i64> = pairs
                    .iter()
                    .filter_map(|(k, _)| match k {
                        mlua::Value::Integer(i) => Some(*i),
                        _ => None,
                    })
                    .collect();
                ints.sort_unstable();
                ints.first().copied() == Some(1) && ints.windows(2).all(|w| w[1] == w[0] + 1)
            });

    if is_array {
        let len = table.raw_len();
        let mut arr = Vec::with_capacity(len);
        for i in 1..=len {
            let val: mlua::Value = table.raw_get(i).unwrap_or(mlua::Value::Nil);
            arr.push(lua_value_to_json(lua, &val));
        }
        serde_json::Value::Array(arr)
    } else {
        let mut map = serde_json::Map::new();
        for (key, val) in pairs {
            let key_str = match &key {
                mlua::Value::String(s) => s.to_string_lossy().to_string(),
                mlua::Value::Integer(i) => i.to_string(),
                _ => continue,
            };
            map.insert(key_str, lua_value_to_json(lua, &val));
        }
        serde_json::Value::Object(map)
    }
}

pub fn lua_value_to_json(lua: &Lua, val: &mlua::Value) -> serde_json::Value {
    match val {
        mlua::Value::Nil => serde_json::Value::Null,
        mlua::Value::Boolean(b) => serde_json::Value::Bool(*b),
        mlua::Value::Integer(i) => serde_json::json!(*i),
        mlua::Value::Number(n) => serde_json::json!(*n),
        mlua::Value::String(s) => serde_json::Value::String(s.to_string_lossy().to_string()),
        mlua::Value::Table(t) => lua_table_to_json(lua, t),
        _ => serde_json::Value::Null,
    }
}

struct BoundedToolMetadataConverter<'lua> {
    lua: &'lua Lua,
    active_tables: HashSet<usize>,
    nodes: usize,
    bytes: usize,
}

impl<'lua> BoundedToolMetadataConverter<'lua> {
    fn new(lua: &'lua Lua) -> Self {
        Self {
            lua,
            active_tables: HashSet::new(),
            nodes: 0,
            bytes: 0,
        }
    }

    fn convert(
        &mut self,
        value: &mlua::Value,
        depth: usize,
        ignored_root_keys: &[&str],
    ) -> Result<serde_json::Value, String> {
        if depth > protocol::TOOL_METADATA_MAX_DEPTH {
            return Err(protocol::ToolResultValidationError::MetadataTooDeep.to_string());
        }
        self.nodes = self.nodes.saturating_add(1);
        if self.nodes > protocol::TOOL_METADATA_MAX_NODES {
            return Err(protocol::ToolResultValidationError::MetadataTooManyNodes.to_string());
        }
        match value {
            mlua::Value::Nil => Ok(serde_json::Value::Null),
            mlua::Value::Boolean(value) => {
                self.add_bytes(1)?;
                Ok(serde_json::Value::Bool(*value))
            }
            mlua::Value::Integer(value) => {
                self.add_bytes(std::mem::size_of::<i64>())?;
                Ok(serde_json::json!(*value))
            }
            mlua::Value::Number(value) => {
                if !value.is_finite() {
                    return Err("tool metadata contains a non-finite number".into());
                }
                self.add_bytes(std::mem::size_of::<f64>())?;
                Ok(serde_json::json!(*value))
            }
            mlua::Value::String(value) => {
                let value = value.to_string_lossy();
                self.add_bytes(value.len())?;
                Ok(serde_json::Value::String(value))
            }
            mlua::Value::Table(table) => self.convert_table(table, depth, ignored_root_keys),
            other => Err(format!(
                "tool metadata contains unsupported Lua {} value",
                other.type_name()
            )),
        }
    }

    fn convert_table(
        &mut self,
        table: &mlua::Table,
        depth: usize,
        ignored_root_keys: &[&str],
    ) -> Result<serde_json::Value, String> {
        let pointer = table.to_pointer() as usize;
        if !self.active_tables.insert(pointer) {
            return Err("tool metadata contains a table cycle".into());
        }
        let result = self.convert_table_inner(table, depth, ignored_root_keys);
        self.active_tables.remove(&pointer);
        result
    }

    fn convert_table_inner(
        &mut self,
        table: &mlua::Table,
        depth: usize,
        ignored_root_keys: &[&str],
    ) -> Result<serde_json::Value, String> {
        let mut pairs = Vec::new();
        for pair in table.clone().pairs::<mlua::Value, mlua::Value>() {
            let pair = pair.map_err(|error| format!("tool metadata table: {error}"))?;
            if pairs.len() >= protocol::TOOL_METADATA_MAX_NODES {
                return Err(protocol::ToolResultValidationError::MetadataTooManyNodes.to_string());
            }
            pairs.push(pair);
        }

        let is_array = (pairs.is_empty() && is_json_array(self.lua, table))
            || (!pairs.is_empty()
                && pairs
                    .iter()
                    .all(|(key, _)| matches!(key, mlua::Value::Integer(_)))
                && {
                    let mut indices = pairs
                        .iter()
                        .filter_map(|(key, _)| match key {
                            mlua::Value::Integer(index) => Some(*index),
                            _ => None,
                        })
                        .collect::<Vec<_>>();
                    indices.sort_unstable();
                    indices.first().copied() == Some(1)
                        && indices.windows(2).all(|pair| pair[1] == pair[0] + 1)
                });

        if is_array {
            let mut array = Vec::with_capacity(pairs.len());
            for index in 1..=pairs.len() {
                let value = table
                    .raw_get::<mlua::Value>(index)
                    .map_err(|error| format!("tool metadata array: {error}"))?;
                array.push(self.convert(&value, depth.saturating_add(1), &[])?);
            }
            return Ok(serde_json::Value::Array(array));
        }

        let mut object = serde_json::Map::with_capacity(pairs.len());
        for (key, value) in pairs {
            let key = match key {
                mlua::Value::String(key) => key.to_string_lossy(),
                mlua::Value::Integer(key) => key.to_string(),
                other => {
                    return Err(format!(
                        "tool metadata object contains unsupported Lua {} key",
                        other.type_name()
                    ));
                }
            };
            if depth == 0 && ignored_root_keys.contains(&key.as_str()) {
                continue;
            }
            if key.len() > protocol::TOOL_METADATA_MAX_KEY_BYTES {
                return Err(protocol::ToolResultValidationError::MetadataKeyTooLong.to_string());
            }
            self.add_bytes(key.len())?;
            if object.contains_key(&key) {
                return Err(format!("tool metadata contains duplicate key `{key}`"));
            }
            let value = self.convert(&value, depth.saturating_add(1), &[])?;
            object.insert(key, value);
        }
        Ok(serde_json::Value::Object(object))
    }

    fn add_bytes(&mut self, bytes: usize) -> Result<(), String> {
        self.bytes = self.bytes.saturating_add(bytes);
        if self.bytes > protocol::TOOL_METADATA_MAX_BYTES {
            return Err(protocol::ToolResultValidationError::MetadataTooLarge.to_string());
        }
        Ok(())
    }
}

pub(super) fn bounded_tool_metadata_from_lua(
    lua: &Lua,
    value: &mlua::Value,
    ignored_root_keys: &[&str],
) -> Result<serde_json::Value, String> {
    let metadata = BoundedToolMetadataConverter::new(lua).convert(value, 0, ignored_root_keys)?;
    protocol::validate_tool_metadata(&metadata).map_err(|error| error.to_string())?;
    Ok(metadata)
}

/// Convert a Lua table into a `HashMap<String, serde_json::Value>`
/// suitable for tool arguments.
pub fn lua_table_to_args(lua: &Lua, table: &mlua::Table) -> HashMap<String, serde_json::Value> {
    match lua_table_to_json(lua, table) {
        serde_json::Value::Object(map) => map.into_iter().collect(),
        _ => HashMap::new(),
    }
}

/// Register all Host-tier namespaces on the `smelt` table.
pub fn register_host_api(
    lua: &Lua,
    smelt: &mlua::Table,
    _smelt_keymap: &mlua::Table,
    shared: &Arc<crate::lua::LuaShared>,
    state_root: &Path,
    cache_root: &Path,
) -> LuaResult<()> {
    crate::lua::reg::register_class_doc();
    agent::register(lua, smelt)?;
    auth::register(lua, smelt, shared)?;
    builtins::register(lua, smelt, shared)?;
    time::register(lua, smelt)?;
    cli::register(lua, smelt, shared)?;
    clipboard::register(lua, smelt)?;
    cmd::register(lua, smelt, shared)?;
    defaults::register(lua, smelt, shared)?;
    document::register(lua, smelt)?;
    diff::register(lua, smelt, shared)?;
    events::register(lua, smelt, shared)?;
    files::register(lua, smelt, shared)?;
    frontend::register(lua, smelt)?;
    fs::register(lua, smelt, shared)?;
    fuzzy::register(lua, smelt)?;
    grep::register(lua, smelt, shared)?;
    html::register(lua, smelt)?;
    http::register(lua, smelt, shared, cache_root)?;
    image::register(lua, smelt, shared)?;
    json::register(lua, smelt)?;
    layout::register(lua, smelt, shared)?;
    lifecycle::register(lua, smelt, shared)?;
    log::register(lua, smelt, shared)?;
    lsp::register(lua, smelt, shared)?;
    mcp::register(lua, smelt, shared)?;
    messages::register(lua, smelt, shared)?;
    mode::register(lua, smelt)?;
    os::register(lua, smelt, shared)?;
    reasoning::register(lua, smelt)?;
    reg::register(lua, smelt)?;
    remember::register(lua, smelt, shared)?;
    parse::register(lua, smelt)?;
    path::register(lua, smelt, shared)?;
    perf::register(lua, smelt)?;
    phase::register(lua, smelt, shared)?;
    process::register(lua, smelt, shared)?;
    provider::register(lua, smelt, shared)?;
    shell::register(lua, smelt)?;
    signal::register(lua, smelt, shared)?;
    skills::register(lua, smelt, shared)?;
    spawn::register(lua, smelt, shared)?;
    state::register(lua, smelt, state_root)?;
    task::register(lua, smelt, shared)?;
    timer::register(lua, smelt, shared)?;
    tools::register(lua, smelt, shared)?;
    transcript::register(lua, smelt, shared)?;
    trust::register(lua, smelt, shared, state_root)?;
    Ok(())
}
