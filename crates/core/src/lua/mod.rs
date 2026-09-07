//! Lua runtime types for `smelt-core`.

pub mod api;
pub mod doc;
pub mod hooks;
pub mod lua_type;
pub mod module;
pub mod reg;
pub mod runtime;
mod shared;
mod task;
pub mod watchers;

pub use reg::LuaReg;

pub use hooks::{HookEntry, HookRegistry};
pub use runtime::{
    autoload_modules, autoload_modules_filtered, ensure_builtins_extracted, init_lua_path,
    load_bootstrap_chunks, LuaLoadFailureLocation, LuaRuntime, ShutdownHookContext, ToolVisibility,
    ToolWorkspacePaths, OPTIONAL_PLUGINS,
};
pub use shared::{
    AskCallbacks, CliFlagKind, CliFlagSpec, CliFlagValue, CommandBusyBehavior, DefaultShell, Hooks,
    LuaHostServices, LuaResumeSink, LuaShared, Phase, RegisteredCommand, RegisteredKeymap,
    RegisteredWinRenderer, ToolHandles, TranscriptGroupBucket, TranscriptGroupFieldMatch,
    TranscriptGroupSelector, TranscriptGroupSpec, LUA_BUF_ID_BASE,
};
pub(crate) use task::step_task_owned;
pub use task::{
    current_command_queue_target, current_command_sent_at_ms, current_task_cancel,
    current_task_scope, current_tool_invocation, with_task_cancel, CommandQueueTarget,
    LuaTaskRuntime, TaskCompletion, TaskDriveOutput, TaskEvent, TaskScope, ToolEnv,
    ToolInvocationContext,
};

/// Identifiers carried together through one plugin tool execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolCallIds<'a> {
    pub invocation_id: protocol::InvocationId,
    pub request_id: u64,
    pub call_id: &'a str,
}

/// Outcome of invoking a plugin tool handler.
pub enum ToolExecResult {
    /// Handler returned synchronously; forward content to the engine immediately.
    Immediate {
        content: String,
        is_error: bool,
        metadata: Option<serde_json::Value>,
        display_content: Vec<protocol::ToolDisplayContent>,
        attachment: Option<Box<protocol::ToolAttachment>>,
    },
    /// Handler yielded; result arrives later via `drive_tasks() -> TaskDriveOutput::ToolComplete`.
    Pending,
}

pub(crate) struct LuaToolResultParts {
    pub(crate) content: String,
    pub(crate) is_error: bool,
    pub(crate) metadata: Option<serde_json::Value>,
    pub(crate) display_content: Vec<protocol::ToolDisplayContent>,
    pub(crate) attachment: Option<protocol::ToolAttachment>,
}

pub(crate) fn tool_result_from_lua_table(
    lua: &mlua::Lua,
    result: &mlua::Table,
) -> mlua::Result<LuaToolResultParts> {
    let content = result.get("content").unwrap_or_default();
    let is_error = result.get("is_error").unwrap_or(false);
    let mut display_content = tool_display_content_from_lua(result)?;
    let metadata_value = result
        .get::<mlua::Value>("metadata")
        .unwrap_or(mlua::Value::Nil);
    let (metadata, attachment) =
        tool_metadata_from_lua(lua, &metadata_value, &mut display_content)?;
    protocol::validate_tool_display_content(&display_content).map_err(mlua::Error::external)?;
    Ok(LuaToolResultParts {
        content,
        is_error,
        metadata,
        display_content,
        attachment,
    })
}

fn tool_display_content_from_lua(
    result: &mlua::Table,
) -> mlua::Result<Vec<protocol::ToolDisplayContent>> {
    let Some(fields) = result.get::<Option<mlua::Table>>("display_content")? else {
        return Ok(Vec::new());
    };
    let mut display_content = Vec::new();
    for pair in fields.pairs::<mlua::Value, mlua::Value>() {
        if display_content.len() >= protocol::TOOL_DISPLAY_CONTENT_MAX_FIELDS {
            return Err(mlua::Error::external(
                protocol::ToolResultValidationError::TooManyDisplayFields,
            ));
        }
        let (name, content) = pair?;
        let mlua::Value::String(name) = name else {
            return Err(mlua::Error::external(
                "tool display content field names must be strings",
            ));
        };
        let mlua::Value::String(content) = content else {
            return Err(mlua::Error::external(
                "tool display content field values must be strings",
            ));
        };
        display_content.push(protocol::ToolDisplayContent::new(
            name.to_string_lossy(),
            content.to_string_lossy(),
        ));
    }
    display_content.sort_unstable_by(|left, right| left.name.cmp(&right.name));
    protocol::validate_tool_display_content(&display_content).map_err(mlua::Error::external)?;
    Ok(display_content)
}

fn tool_metadata_from_lua(
    lua: &mlua::Lua,
    value: &mlua::Value,
    display_content: &mut Vec<protocol::ToolDisplayContent>,
) -> mlua::Result<(Option<serde_json::Value>, Option<protocol::ToolAttachment>)> {
    if matches!(value, mlua::Value::Nil) {
        return Ok((None, None));
    }
    let mlua::Value::Table(table) = value else {
        let metadata =
            api::bounded_tool_metadata_from_lua(lua, value, &[]).map_err(mlua::Error::external)?;
        return Ok((Some(metadata), None));
    };

    let mut ignored_keys = Vec::with_capacity(protocol::TOOL_DISPLAY_METADATA_FIELDS.len() + 1);
    for name in protocol::TOOL_DISPLAY_METADATA_FIELDS {
        let value = table
            .raw_get::<mlua::Value>(name)
            .unwrap_or(mlua::Value::Nil);
        let mlua::Value::String(content) = value else {
            continue;
        };
        ignored_keys.push(name);
        if display_content.iter().any(|field| field.name == name) {
            continue;
        }
        display_content.push(protocol::ToolDisplayContent::new(
            name,
            content.to_string_lossy(),
        ));
    }

    let attachment = attachment_from_lua_metadata(table)?;
    if attachment.is_some() {
        ignored_keys.push("data_url");
    }
    let metadata = api::bounded_tool_metadata_from_lua(lua, value, &ignored_keys)
        .map_err(mlua::Error::external)?;
    Ok((Some(metadata), attachment))
}

fn attachment_from_lua_metadata(
    metadata: &mlua::Table,
) -> mlua::Result<Option<protocol::ToolAttachment>> {
    if metadata.get::<Option<String>>("kind")?.as_deref() != Some("file_attachment") {
        return Ok(None);
    }
    let modality = match metadata.get::<String>("modality")?.as_str() {
        "image" => protocol::ToolAttachmentModality::Image,
        "pdf" => protocol::ToolAttachmentModality::Pdf,
        modality => {
            return Err(mlua::Error::external(format!(
                "tool attachment has unsupported modality `{modality}`"
            )));
        }
    };
    let mime = metadata.get::<String>("mime")?;
    let data_url = metadata.get::<String>("data_url")?;
    let expected_prefix = format!("data:{mime};base64,");
    if !data_url.starts_with(&expected_prefix) {
        return Err(mlua::Error::external(format!(
            "tool attachment data URL must start with `{expected_prefix}`"
        )));
    }
    Ok(Some(protocol::ToolAttachment {
        modality,
        mime,
        data_url,
        label: metadata.get::<Option<String>>("label")?,
    }))
}

use mlua::prelude::*;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Session-scoped counters of `LuaHandle` lifecycle events. The pair is a
/// **drop-counter ledger**: every `from_func` increments `created`, every
/// `Drop` increments `dropped`. The difference is the net live count of
/// registry-backed callables.
///
/// The ledger is shared by every Lua generation in one session, keeping the
/// fuzz harness leak oracle isolated from other sessions running in parallel.
#[derive(Default)]
pub(crate) struct LuaHandleLedger {
    created: AtomicU64,
    dropped: AtomicU64,
}

impl LuaHandleLedger {
    fn live(&self) -> u64 {
        self.created
            .load(Ordering::Relaxed)
            .saturating_sub(self.dropped.load(Ordering::Relaxed))
    }
}

/// A Lua callable parked in the registry so it survives GC.
pub struct LuaHandle {
    pub key: mlua::RegistryKey,
    ledger: Arc<LuaHandleLedger>,
}

impl LuaHandle {
    pub fn from_func(lua: &Lua, func: mlua::Function) -> LuaResult<Self> {
        let key = lua.create_registry_value(func)?;
        let ledger = lua
            .app_data_ref::<Arc<LuaHandleLedger>>()
            .map(|ledger| Arc::clone(&ledger))
            .unwrap_or_else(|| {
                let ledger = Arc::new(LuaHandleLedger::default());
                lua.set_app_data(Arc::clone(&ledger));
                ledger
            });
        ledger.created.fetch_add(1, Ordering::Relaxed);
        Ok(Self { key, ledger })
    }
}

impl Drop for LuaHandle {
    fn drop(&mut self) {
        self.ledger.dropped.fetch_add(1, Ordering::Relaxed);
    }
}

/// Serialize a value for Lua-facing views, mapping JSON nulls to nil.
/// Use [`serde_to_lua_preserving_nulls`] for protocol data that must round-trip.
pub fn serde_to_lua<T: serde::Serialize>(lua: &Lua, value: &T) -> LuaResult<mlua::Value> {
    let json = serde_json::to_value(value).map_err(mlua::Error::external)?;
    json_to_lua(lua, &json)
}

/// Serialize protocol data without dropping JSON null fields or array entries.
/// Nulls use mlua's non-nil sentinel so opaque provider payloads round-trip
/// through Lua tables; ordinary Lua-facing APIs can use [`serde_to_lua`].
pub fn serde_to_lua_preserving_nulls<T: serde::Serialize>(
    lua: &Lua,
    value: &T,
) -> LuaResult<mlua::Value> {
    let json = serde_json::to_value(value).map_err(mlua::Error::external)?;
    json_to_lua_with_null(lua, &json, &mlua::Value::NULL)
}

/// Deserialize a Lua value into a `DeserializeOwned` Rust type via JSON.
/// Inverse of [`serde_to_lua_preserving_nulls`]. Returns `None` if either the Lua→JSON
/// conversion drops fields the deserializer requires, or the JSON
/// doesn't match the target shape. Callers treat `None` as "no
/// mutation" (the original payload stays in flight).
pub fn lua_to_serde<T: serde::de::DeserializeOwned>(lua: &Lua, value: &mlua::Value) -> Option<T> {
    let json = match value {
        mlua::Value::Table(t) => api::lua_table_to_json(lua, t),
        mlua::Value::Nil => serde_json::Value::Null,
        mlua::Value::LightUserData(ptr) if ptr.0.is_null() => serde_json::Value::Null,
        mlua::Value::Boolean(b) => serde_json::Value::Bool(*b),
        mlua::Value::Integer(i) => serde_json::json!(*i),
        mlua::Value::Number(n) => serde_json::json!(*n),
        mlua::Value::String(s) => serde_json::Value::String(s.to_string_lossy().to_string()),
        _ => return None,
    };
    serde_json::from_value(json).ok()
}

/// Decode a Lua value into one styled line.
pub(crate) fn styled_line_from_lua(
    value: mlua::Value,
    label: &str,
) -> LuaResult<Vec<protocol::StyledSpan>> {
    match value {
        mlua::Value::Nil => Ok(Vec::new()),
        mlua::Value::String(s) => Ok(vec![protocol::StyledSpan {
            text: s.to_string_lossy(),
            ..Default::default()
        }]),
        mlua::Value::Table(line_table) => {
            let mut spans = Vec::new();
            for span in line_table.sequence_values::<mlua::Value>() {
                spans.push(styled_span_from_lua(span?, label)?);
            }
            Ok(spans)
        }
        other => Err(mlua::Error::external(format!(
            "{label}: expected styled line, got {}",
            other.type_name()
        ))),
    }
}

/// Decode a Lua value into the styled-lines shape shared by tool summaries,
/// transcript blocks, and `smelt.layout.runs`.
pub(crate) fn styled_lines_from_lua(
    value: mlua::Value,
    label: &str,
) -> LuaResult<protocol::StyledLines> {
    use protocol::{StyledLines, StyledSpan};

    match value {
        mlua::Value::Nil => Ok(StyledLines::empty()),
        mlua::Value::String(s) => Ok(StyledLines::from_plain(s.to_string_lossy())),
        mlua::Value::Table(lines_table) => {
            let mut lines = Vec::new();
            for line in lines_table.sequence_values::<mlua::Value>() {
                let line = line?;
                match line {
                    mlua::Value::Nil => lines.push(Vec::new()),
                    mlua::Value::String(s) => lines.push(vec![StyledSpan {
                        text: s.to_string_lossy(),
                        ..Default::default()
                    }]),
                    mlua::Value::Table(t) => {
                        let mut spans = Vec::new();
                        for span in t.sequence_values::<mlua::Value>() {
                            spans.push(styled_span_from_lua(span?, label)?);
                        }
                        lines.push(spans);
                    }
                    other => {
                        return Err(mlua::Error::external(format!(
                            "{label}: expected line table or string, got {}",
                            other.type_name()
                        )));
                    }
                }
            }
            Ok(StyledLines(lines))
        }
        other => Err(mlua::Error::external(format!(
            "{label}: expected styled lines, got {}",
            other.type_name()
        ))),
    }
}

fn styled_span_from_lua(span: mlua::Value, label: &str) -> LuaResult<protocol::StyledSpan> {
    use protocol::StyledSpan;

    let span_table = match span {
        mlua::Value::String(s) => {
            return Ok(StyledSpan {
                text: s.to_string_lossy(),
                ..Default::default()
            })
        }
        mlua::Value::Table(t) => t,
        other => {
            return Err(mlua::Error::external(format!(
                "{label}: expected span table or string, got {}",
                other.type_name()
            )))
        }
    };
    let style = span_table.get::<Option<mlua::Table>>("style")?;
    let mut out = StyledSpan {
        text: span_table
            .get::<Option<String>>("text")?
            .or_else(|| span_table.get::<Option<String>>(1).ok().flatten())
            .unwrap_or_default(),
        syntax: span_table.get::<Option<String>>("syntax")?,
        hl: span_table.get::<Option<String>>("hl")?,
        fg: span_table.get::<Option<String>>("fg")?,
        bg: span_table.get::<Option<String>>("bg")?,
        dim: span_table.get::<Option<bool>>("dim")?.unwrap_or(false),
        bold: span_table.get::<Option<bool>>("bold")?.unwrap_or(false),
        italic: span_table.get::<Option<bool>>("italic")?.unwrap_or(false),
        selectable: span_table
            .get::<Option<bool>>("selectable")?
            .unwrap_or(true),
        title_suffix: span_table
            .get::<Option<bool>>("title_suffix")?
            .unwrap_or(false),
    };
    if let Some(style) = style {
        out.hl = out.hl.or(style.get::<Option<String>>("hl")?);
        out.fg = out.fg.or(style.get::<Option<String>>("fg")?);
        out.bg = out.bg.or(style.get::<Option<String>>("bg")?);
        out.dim |= style.get::<Option<bool>>("dim")?.unwrap_or(false);
        out.bold |= style.get::<Option<bool>>("bold")?.unwrap_or(false);
        out.italic |= style.get::<Option<bool>>("italic")?.unwrap_or(false);
    }
    Ok(out)
}

pub fn json_to_lua(lua: &Lua, v: &serde_json::Value) -> LuaResult<mlua::Value> {
    json_to_lua_with_null(lua, v, &mlua::Value::Nil)
}

fn json_to_lua_with_null(
    lua: &Lua,
    v: &serde_json::Value,
    null: &mlua::Value,
) -> LuaResult<mlua::Value> {
    match v {
        serde_json::Value::Null => Ok(null.clone()),
        serde_json::Value::Bool(b) => Ok(mlua::Value::Boolean(*b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(mlua::Value::Integer(i))
            } else {
                Ok(mlua::Value::Number(n.as_f64().unwrap_or(0.0)))
            }
        }
        serde_json::Value::String(s) => Ok(mlua::Value::String(lua.create_string(s)?)),
        serde_json::Value::Array(arr) => {
            let t = lua.create_table()?;
            crate::lua::api::mark_json_array(lua, &t)?;
            for (i, elem) in arr.iter().enumerate() {
                t.set(i + 1, json_to_lua_with_null(lua, elem, null)?)?;
            }
            Ok(mlua::Value::Table(t))
        }
        serde_json::Value::Object(map) => {
            let t = lua.create_table()?;
            for (k, val) in map {
                t.set(k.as_str(), json_to_lua_with_null(lua, val, null)?)?;
            }
            Ok(mlua::Value::Table(t))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_json_round_trip_preserves_nulls_and_empty_containers() {
        let lua = Lua::new();
        for json in [
            serde_json::Value::Null,
            serde_json::json!([null, {}, [], null]),
            serde_json::json!({
                "encrypted_content": "synthetic-encrypted-reasoning",
                "content": null,
                "summary": [],
                "metadata": {"items": [null, {}, [], [null, true]], "empty": {}},
            }),
        ] {
            let value = serde_to_lua_preserving_nulls(&lua, &json).unwrap();
            assert_eq!(lua_to_serde::<serde_json::Value>(&lua, &value), Some(json));
        }
    }

    #[test]
    fn ordinary_json_conversion_uses_nil_for_null() {
        let lua = Lua::new();
        assert_eq!(
            serde_to_lua(&lua, &serde_json::Value::Null).unwrap(),
            mlua::Value::Nil
        );
        let value = serde_to_lua(&lua, &serde_json::json!({"content": null})).unwrap();
        let table = value.as_table().unwrap();
        assert_eq!(
            table.get::<mlua::Value>("content").unwrap(),
            mlua::Value::Nil
        );
    }
}
