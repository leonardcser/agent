use super::{str_arg, Tool, ToolContext, ToolFuture, ToolResult};
use serde_json::Value;
use std::collections::HashMap;

pub struct MessageAgentTool {
    pub my_id: String,
    pub my_slug: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

impl Tool for MessageAgentTool {
    fn name(&self) -> &str {
        "message_agent"
    }

    fn description(&self) -> &str {
        "Send a message to one or more agents. Use the agent name from `list_agents` or from the <agent-message from=\"name\"> tag. The recipient may be busy and reply later. Use this to steer subagents, provide information, or coordinate work."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "targets": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "List of agent names (slugs) to send the message to. Use the name from the <agent-message from=\"name\"> tag. Example: [\"reed\", \"plum\"]"
                },
                "message": {
                    "type": "string",
                    "description": "The message to send"
                }
            },
            "required": ["targets", "message"]
        })
    }

    fn execute<'a>(
        &'a self,
        args: HashMap<String, Value>,
        _ctx: &'a ToolContext,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            let message = str_arg(&args, "message");
            let targets: Vec<String> = args
                .get("targets")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();

            if targets.is_empty() {
                return ToolResult::err("no targets specified");
            }

            let slug = self
                .my_slug
                .lock()
                .ok()
                .and_then(|guard| guard.clone())
                .unwrap_or_default();

            let mut delivered = vec![];
            let mut errors = vec![];

            for id in &targets {
                let entry = crate::registry::find_by_id(id);
                let socket_path = match entry {
                    Some(e) => std::path::PathBuf::from(&e.socket_path),
                    None => {
                        errors.push(format!("{id}: not found"));
                        continue;
                    }
                };

                match crate::socket::send_message(&socket_path, &self.my_id, &slug, &message).await
                {
                    Ok(()) => delivered.push(id.clone()),
                    Err(e) => errors.push(format!("{id}: {e}")),
                }
            }

            if errors.is_empty() {
                ToolResult::ok("delivered")
            } else if delivered.is_empty() {
                ToolResult::err(format!("failed: {}", errors.join("; ")))
            } else {
                ToolResult::ok(format!(
                    "partial: delivered to {:?}, failed: {}",
                    delivered,
                    errors.join("; ")
                ))
            }
        })
    }
}
