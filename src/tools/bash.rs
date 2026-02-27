use super::{run_command_with_timeout, str_arg, timeout_arg, Tool, ToolResult};
use serde_json::Value;
use std::collections::HashMap;

pub struct BashTool;

impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Execute a shell command"
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Shell command to execute"},
                "timeout_ms": {"type": "integer", "description": "Timeout in milliseconds (default: 120000)"}
            },
            "required": ["command"]
        })
    }

    fn needs_confirm(&self, args: &HashMap<String, Value>) -> Option<String> {
        Some(str_arg(args, "command"))
    }

    fn execute(&self, args: &HashMap<String, Value>) -> ToolResult {
        let command = str_arg(args, "command");
        let timeout = timeout_arg(args, 120);

        let child = std::process::Command::new("sh")
            .arg("-c")
            .arg(&command)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();

        match child {
            Ok(child) => run_command_with_timeout(child, timeout),
            Err(e) => ToolResult {
                content: e.to_string(),
                is_error: true,
            },
        }
    }
}
