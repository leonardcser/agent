use super::{str_arg, Tool, ToolResult};
use serde_json::Value;
use std::collections::HashMap;

pub struct ExitPlanModeTool;

impl Tool for ExitPlanModeTool {
    fn name(&self) -> &str {
        "exit_plan_mode"
    }

    fn description(&self) -> &str {
        "Signal that planning is complete and ready for user approval. Call this when your plan is finalized."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "plan_summary": {
                    "type": "string",
                    "description": "A concise summary of the implementation plan for the user to approve."
                }
            },
            "required": ["plan_summary"]
        })
    }

    fn execute(&self, args: &HashMap<String, Value>) -> ToolResult {
        let summary = str_arg(args, "plan_summary");
        ToolResult {
            content: summary,
            is_error: false,
        }
    }
}
