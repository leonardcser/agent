//! Native row-source handles shared by Lua data providers and UI windows.

use crate::lua::doc::{record_class, Tier};
use crate::lua::lua_type::{LuaClassDecl, LuaClassField, LuaType};
use crate::lua::module::LuaMod;
use mlua::prelude::*;
use smelt_buffer::document::{RowSource, TextDocument};
use std::sync::Arc;

#[derive(Clone)]
pub struct LuaDocument(pub Arc<dyn RowSource>);

impl LuaType for LuaDocument {
    fn lua_type() -> String {
        "smelt.document.Document".into()
    }
}

impl FromLua for LuaDocument {
    fn from_lua(value: LuaValue, _: &Lua) -> LuaResult<Self> {
        match value {
            LuaValue::UserData(value) => Ok(value.borrow::<Self>()?.clone()),
            _ => Err(LuaError::external("expected a smelt.document.Document")),
        }
    }
}

impl mlua::UserData for LuaDocument {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("row_count", |_, this, ()| Ok(this.0.snapshot().total_rows));
    }
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::advanced(
        lua,
        smelt,
        "document",
        "Indexed read-only documents shared by native data providers and Lua-created windows.",
        Tier::Host,
    )?;
    record_class(LuaClassDecl {
        name: "smelt.document.Document",
        classification: crate::lua::doc::classification_for_type("smelt.document.Document"),
        doc: "Native random-access row source. Attach with `win:document(source)`; each window owns its viewport, scratch storage and highlighting subscription. Sharing a document shares its row projection, including folds; use `diff:view()` for independent diff folds. Only requested rows are materialized. Copy/search access does not request background highlighting. Row coordinates are zero-based.",
        fields: vec![LuaClassField { name: "row_count", ty: "fun(self: smelt.document.Document): integer".into(), optional: false, doc: "Current visible row count, including fold markers." }],
    });
    m.fn_("text", "Index plain text once without creating a Lua table or buffer line per row. Attach the result with `win:document(source)` for viewport-only rendering and Vim navigation.", &["text"],
        |_, text: String| -> LuaResult<LuaDocument> { Ok(LuaDocument(Arc::new(TextDocument::new(text)))) })?;
    Ok(())
}
