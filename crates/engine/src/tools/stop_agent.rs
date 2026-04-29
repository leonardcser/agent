use super::{str_arg, Tool, ToolContext, ToolFuture, ToolResult};
use serde_json::Value;
use std::collections::HashMap;

pub struct StopAgentTool {
    pub my_pid: u32,
}

impl Tool for StopAgentTool {
    fn name(&self) -> &str {
        "stop_agent"
    }

    fn description(&self) -> &str {
        "Stop a subagent and all its children. Only works on agents you own. Use to cancel work that is no longer needed or has been superseded."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "target": {
                    "type": "string",
                    "description": "Agent name to stop (e.g. \"cedar\")"
                }
            },
            "required": ["target"]
        })
    }

    fn execute<'a>(
        &'a self,
        args: HashMap<String, Value>,
        _ctx: &'a ToolContext,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            let target_id = str_arg(&args, "target");

            let entry = crate::registry::find_by_id(&target_id);
            match entry {
                Some(e) => {
                    if !crate::registry::is_in_tree(e.pid, self.my_pid) {
                        return ToolResult::err(format!("{target_id} is not owned by you"));
                    }
                    crate::registry::kill_agent(e.pid);
                    ToolResult::ok(format!("stopped {target_id}"))
                }
                None => ToolResult::err(format!("{target_id} not found")),
            }
        })
    }
}
