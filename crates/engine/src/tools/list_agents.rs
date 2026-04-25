use super::{Tool, ToolContext, ToolFuture, ToolResult};
use serde_json::Value;
use std::collections::HashMap;

pub struct ListAgentsTool {
    pub scope: String,
    pub my_pid: u32,
}

impl Tool for ListAgentsTool {
    fn name(&self) -> &str {
        "list_agents"
    }

    fn description(&self) -> &str {
        "List agents in the current workspace with their name, status, task slug, and whether they are owned (your subagents) or peers. Use to discover agent names before calling `message_agent` or `stop_agent`."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
        })
    }

    fn execute<'a>(
        &'a self,
        _args: HashMap<String, Value>,
        _ctx: &'a ToolContext,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            let entries = crate::registry::discover(&self.scope);

            let others: Vec<_> = entries.iter().filter(|e| e.pid != self.my_pid).collect();

            if others.is_empty() {
                return ToolResult::ok("No other agents found.");
            }

            // Build parent_pid -> children map for in-memory tree walk.
            let parent_map: HashMap<u32, Vec<u32>> = {
                let mut m: HashMap<u32, Vec<u32>> = HashMap::new();
                for e in &entries {
                    if let Some(ppid) = e.parent_pid {
                        m.entry(ppid).or_default().push(e.pid);
                    }
                }
                m
            };
            let is_descendant = |pid: u32, root: u32| -> bool {
                let mut stack = vec![root];
                while let Some(current) = stack.pop() {
                    if let Some(children) = parent_map.get(&current) {
                        for &child in children {
                            if child == pid {
                                return true;
                            }
                            stack.push(child);
                        }
                    }
                }
                false
            };

            // Compute column widths for alignment.
            let name_w = others.iter().map(|e| e.agent_id.len()).max().unwrap_or(0);
            let status_w = 7; // "working" is the longest

            let mut lines = Vec::new();
            for e in &others {
                let agent_type = if is_descendant(e.pid, self.my_pid) {
                    "owned"
                } else {
                    "peer "
                };
                let status = match e.status {
                    crate::registry::AgentStatus::Working => "working",
                    crate::registry::AgentStatus::Idle => "idle",
                };
                let slug = e.task_slug.as_deref().unwrap_or("");
                lines.push(format!(
                    "{:<name_w$}  {:<5}  {:<status_w$}  {slug}",
                    e.agent_id, agent_type, status
                ));
            }

            ToolResult::ok(lines.join("\n"))
        })
    }
}
