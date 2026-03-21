use super::{str_arg, Tool, ToolContext, ToolFuture, ToolResult};
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

    fn needs_confirm(&self, _args: &HashMap<String, Value>) -> Option<String> {
        Some("Implement this plan?".to_string())
    }

    fn execute<'a>(
        &'a self,
        args: HashMap<String, Value>,
        ctx: &'a ToolContext<'a>,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            let summary = str_arg(&args, "plan_summary");

            match crate::plan::save(ctx.session_dir, ctx.session_id, &summary) {
                Ok(path) => {
                    let display_path = path.display().to_string();
                    ToolResult {
                        content: format!(
                            "Plan saved to {display_path}\n\n{summary}\n\n\
                             The user approved this plan. Proceed with the implementation now."
                        ),
                        is_error: false,
                    }
                }
                Err(e) => ToolResult {
                    content: format!("Failed to save plan: {e}\n\n{summary}"),
                    is_error: true,
                },
            }
        })
    }
}
