use crate::mcp::{McpManager, McpToolDef};
use crate::tools::{Tool, ToolContext, ToolFuture, ToolResult};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

/// Wraps an MCP tool definition as a native Tool trait object.
pub struct McpTool {
    def: McpToolDef,
    qualified_name: String,
    manager: Arc<McpManager>,
}

impl McpTool {
    pub fn new(def: McpToolDef, manager: Arc<McpManager>) -> Self {
        let qualified_name = def.qualified_name();
        Self {
            def,
            qualified_name,
            manager,
        }
    }
}

impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.qualified_name
    }

    fn description(&self) -> &str {
        &self.def.description
    }

    fn parameters(&self) -> Value {
        self.def.input_schema.clone()
    }

    fn needs_confirm(&self, _args: &HashMap<String, Value>) -> Option<String> {
        Some(format!(
            "MCP {}_{}",
            self.def.server_name, self.def.tool_name
        ))
    }

    fn is_mcp(&self) -> bool {
        true
    }

    fn execute<'a>(
        &'a self,
        args: HashMap<String, Value>,
        _ctx: &'a ToolContext,
    ) -> ToolFuture<'a> {
        let args_value = serde_json::to_value(&args).unwrap_or(Value::Object(Default::default()));
        Box::pin(async move {
            match self
                .manager
                .call_tool(
                    &self.def.server_name,
                    &self.def.tool_name,
                    args_value,
                    self.def.timeout,
                )
                .await
            {
                Ok(output) => ToolResult::ok(output),
                Err(e) => ToolResult::err(e),
            }
        })
    }
}
