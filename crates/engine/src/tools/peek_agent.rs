use super::{str_arg, Tool, ToolContext, ToolFuture, ToolResult};
use serde_json::Value;
use std::collections::HashMap;

pub struct PeekAgentTool {
    pub my_id: String,
}

impl Tool for PeekAgentTool {
    fn name(&self) -> &str {
        "peek_agent"
    }

    fn description(&self) -> &str {
        "Non-intrusively inspect another agent's knowledge by running a question against their conversation context. The target agent is unaware of the query. Returns an answer synthesized from their context. Use this to understand what another agent knows or has done without interrupting their work."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "target": {
                    "type": "string",
                    "description": "Agent name to query (e.g. \"cedar\")"
                },
                "question": {
                    "type": "string",
                    "description": "The question to answer from the target's context"
                }
            },
            "required": ["target", "question"]
        })
    }

    fn execute<'a>(
        &'a self,
        args: HashMap<String, Value>,
        _ctx: &'a ToolContext,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            let target_id = str_arg(&args, "target");
            let question = str_arg(&args, "question");

            let entry = crate::registry::find_by_id(&target_id);
            let socket_path = match entry {
                Some(e) => std::path::PathBuf::from(&e.socket_path),
                None => {
                    return ToolResult::err(format!("{target_id}: not found"));
                }
            };

            let framed = format!(
                "Another agent is inspecting this agent's context. \
                 Answer the following question factually based on what this agent \
                 has done and knows. Answer in third person (\"the agent has...\"), \
                 not as the agent itself. Report only what has been done and \
                 what is known.\n\n{question}"
            );

            match crate::socket::send_query(&socket_path, &self.my_id, &framed).await {
                Ok(answer) => ToolResult::ok(answer),
                Err(e) => ToolResult::err(format!("{target_id}: {e}")),
            }
        })
    }
}
