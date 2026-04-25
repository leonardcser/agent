use super::{str_arg, Tool, ToolContext, ToolFuture, ToolResult};
use crate::skills::SkillLoader;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

pub struct LoadSkillTool {
    pub loader: Arc<SkillLoader>,
}

impl Tool for LoadSkillTool {
    fn name(&self) -> &str {
        "load_skill"
    }

    fn description(&self) -> &str {
        "Load a skill by name to get specialized instructions and knowledge. Use this when a task matches one of the available skills listed in the system prompt."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "The name of the skill to load"
                }
            },
            "required": ["name"]
        })
    }

    fn needs_confirm(&self, args: &HashMap<String, Value>) -> Option<String> {
        Some(str_arg(args, "name"))
    }

    fn execute<'a>(
        &'a self,
        args: HashMap<String, Value>,
        _ctx: &'a ToolContext,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            let name = str_arg(&args, "name");
            if name.is_empty() {
                return ToolResult::err("Missing required parameter: name");
            }
            match self.loader.content(&name) {
                Ok(content) => ToolResult::ok(content),
                Err(msg) => ToolResult::err(msg),
            }
        })
    }
}
