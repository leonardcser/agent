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

    fn approval_pattern(&self, args: &HashMap<String, Value>) -> Option<String> {
        let cmd = str_arg(args, "command");
        let subcmds = crate::permissions::split_shell_commands_with_ops(&cmd);
        let mut result = String::new();
        for (subcmd, op) in &subcmds {
            let bin = subcmd.split_whitespace().next().unwrap_or("");
            if !bin.is_empty() {
                if !result.is_empty() {
                    result.push(' ');
                }
                result.push_str(bin);
                result.push_str(" *");
            }
            if let Some(op) = op {
                result.push_str(&format!(" {op}"));
            }
        }
        Some(result)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern(cmd: &str) -> String {
        let tool = BashTool;
        let mut args = HashMap::new();
        args.insert("command".into(), Value::String(cmd.into()));
        tool.approval_pattern(&args).unwrap()
    }

    #[test]
    fn simple_command() {
        assert_eq!(pattern("cargo build"), "cargo *");
    }

    #[test]
    fn chain_and() {
        assert_eq!(pattern("cargo fmt && cargo clippy"), "cargo * && cargo *");
    }

    #[test]
    fn chain_or() {
        assert_eq!(pattern("make || make install"), "make * || make *");
    }

    #[test]
    fn chain_semicolon() {
        assert_eq!(pattern("cd /tmp; rm -rf foo"), "cd * ; rm *");
    }

    #[test]
    fn pipe() {
        assert_eq!(pattern("cat file.txt | grep foo"), "cat * | grep *");
    }

    #[test]
    fn ls_and_rm() {
        assert_eq!(pattern("ls && rm README.md"), "ls * && rm *");
    }

    #[test]
    fn mixed() {
        assert_eq!(
            pattern("cd /tmp && rm -rf * | grep err; echo done"),
            "cd * && rm * | grep * ; echo *"
        );
    }

    #[test]
    fn background_operator() {
        assert_eq!(pattern("sleep 5 & echo done"), "sleep * & echo *");
    }

    #[test]
    fn quoted_operator_not_split() {
        // && inside quotes is not an operator — single command
        assert_eq!(pattern(r#"grep "&&" file.txt"#), "grep *");
    }

    #[test]
    fn empty_command() {
        assert_eq!(pattern(""), "");
    }

    #[test]
    fn only_whitespace() {
        assert_eq!(pattern("   "), "");
    }
}
