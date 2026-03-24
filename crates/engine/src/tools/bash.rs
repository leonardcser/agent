use super::{bool_arg, str_arg, timeout_arg, Tool, ToolContext, ToolFuture, ToolResult};
use protocol::EngineEvent;
use serde_json::Value;
use std::collections::HashMap;
use tokio::io::{AsyncBufReadExt, BufReader};

pub struct BashTool;

impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Execute a non-interactive bash command and return its output. The working directory persists between calls. Commands time out after 2 minutes by default (configurable up to 10 minutes). Use run_in_background for long-running processes. Do not run interactive commands (editors, pagers, interactive rebases, etc.) — they will hang. If there is no non-interactive alternative, ask the user to run it themselves."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Shell command to execute"},
                "description": {"type": "string", "description": "Short (max 10 words) description of what this command does"},
                "timeout_ms": {"type": "integer", "description": "Timeout in milliseconds (default: 120000, max: 600000)"},
                "run_in_background": {"type": "boolean", "description": "Run the command in the background and return a process ID. Use read_process_output to check output and stop_process to kill it."}
            },
            "required": ["command"]
        })
    }

    fn needs_confirm(&self, args: &HashMap<String, Value>) -> Option<String> {
        Some(str_arg(args, "command"))
    }

    fn approval_patterns(&self, args: &HashMap<String, Value>) -> Vec<String> {
        let cmd = str_arg(args, "command");
        let subcmds = crate::permissions::split_shell_commands(&cmd);
        let mut patterns = Vec::new();
        for subcmd in &subcmds {
            let bin = subcmd.split_whitespace().next().unwrap_or("");
            if !bin.is_empty() {
                let pat = format!("{bin} *");
                if !patterns.contains(&pat) {
                    patterns.push(pat);
                }
            }
        }
        patterns
    }

    fn execute<'a>(
        &'a self,
        args: HashMap<String, Value>,
        ctx: &'a ToolContext<'a>,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            let command = str_arg(&args, "command");

            if let Some(msg) = check_interactive(&command) {
                return ToolResult {
                    content: msg.to_string(),
                    is_error: true,
                };
            }

            if bool_arg(&args, "run_in_background") {
                return execute_background(&command, ctx).await;
            }

            execute_streaming(&command, &args, ctx).await
        })
    }
}

/// Known interactive binaries that require a TTY.
const INTERACTIVE_BINS: &[&str] = &[
    "vim", "nvim", "vi", "nano", "emacs", "pico", "less", "more", "top", "htop", "btop", "nmon",
    "irb", "ghci",
];

/// Git subcommands whose `-i`/`--interactive` flag requires a TTY.
const GIT_INTERACTIVE_SUBCMDS: &[&str] = &["rebase", "add", "checkout", "clean", "stash"];

fn check_interactive(command: &str) -> Option<&'static str> {
    let cmds = crate::permissions::split_shell_commands(command);
    for subcmd in &cmds {
        let parts: Vec<&str> = subcmd.split_whitespace().collect();
        let bin = match parts.first() {
            Some(b) => *b,
            None => continue,
        };
        // Check known interactive binaries (handle paths like /usr/bin/vim)
        let base = bin.rsplit('/').next().unwrap_or(bin);
        if INTERACTIVE_BINS.contains(&base) {
            return Some("Interactive commands (editors, REPLs, pagers) cannot run here — they require a terminal. If there is no non-interactive alternative, ask the user to run it themselves.");
        }
        // Check git interactive flags
        if base == "git" {
            let has_interactive_flag = parts.iter().any(|p| *p == "-i" || *p == "--interactive");
            if has_interactive_flag {
                let has_interactive_subcmd =
                    parts.iter().any(|p| GIT_INTERACTIVE_SUBCMDS.contains(p));
                if has_interactive_subcmd {
                    return Some("Interactive git commands (rebase -i, add -i, etc.) cannot run here — they require a terminal. If there is no non-interactive alternative, ask the user to run it themselves.");
                }
            }
        }
    }
    None
}

async fn execute_background(command: &str, ctx: &ToolContext<'_>) -> ToolResult {
    match tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => {
            let id = ctx.processes.next_id();
            ctx.processes
                .spawn(id.clone(), command, child, ctx.proc_done_tx.clone());
            ToolResult {
                content: format!("background process started with id: {id}"),
                is_error: false,
            }
        }
        Err(e) => ToolResult {
            content: e.to_string(),
            is_error: true,
        },
    }
}

async fn execute_streaming(
    command: &str,
    args: &HashMap<String, Value>,
    ctx: &ToolContext<'_>,
) -> ToolResult {
    let timeout = timeout_arg(args, 120);

    let mut child = match tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return ToolResult {
                content: e.to_string(),
                is_error: true,
            }
        }
    };

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let mut stdout_reader = BufReader::new(stdout).lines();
    let mut stderr_reader = BufReader::new(stderr).lines();
    let mut output = String::new();
    let mut stdout_done = false;
    let mut stderr_done = false;

    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);

    loop {
        if stdout_done && stderr_done {
            break;
        }
        tokio::select! {
            line = stdout_reader.next_line(), if !stdout_done => {
                match line {
                    Ok(Some(line)) => {
                        let _ = ctx.event_tx.send(EngineEvent::ToolOutput {
                            call_id: ctx.call_id.to_string(),
                            chunk: line.clone(),
                        });
                        if !output.is_empty() { output.push('\n'); }
                        output.push_str(&line);
                    }
                    _ => stdout_done = true,
                }
            }
            line = stderr_reader.next_line(), if !stderr_done => {
                match line {
                    Ok(Some(line)) => {
                        let _ = ctx.event_tx.send(EngineEvent::ToolOutput {
                            call_id: ctx.call_id.to_string(),
                            chunk: line.clone(),
                        });
                        if !output.is_empty() { output.push('\n'); }
                        output.push_str(&line);
                    }
                    _ => stderr_done = true,
                }
            }
            _ = &mut deadline => {
                let _ = child.kill().await;
                return ToolResult {
                    content: format!("timed out after {:.0}s", timeout.as_secs_f64()),
                    is_error: true,
                };
            }
            _ = ctx.cancel.cancelled() => {
                let _ = child.kill().await;
                return ToolResult {
                    content: "cancelled".into(),
                    is_error: true,
                };
            }
        }
    }

    let status = child.wait().await;
    let is_error = status.map(|s| !s.success()).unwrap_or(true);
    ToolResult {
        content: output,
        is_error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patterns(cmd: &str) -> Vec<String> {
        let tool = BashTool;
        let mut args = HashMap::new();
        args.insert("command".into(), Value::String(cmd.into()));
        tool.approval_patterns(&args)
    }

    #[test]
    fn simple_command() {
        assert_eq!(patterns("cargo build"), vec!["cargo *"]);
    }

    #[test]
    fn chain_same_binary() {
        // Deduplicated: both sub-commands use "cargo"
        assert_eq!(patterns("cargo fmt && cargo clippy"), vec!["cargo *"]);
    }

    #[test]
    fn chain_different_binaries() {
        assert_eq!(patterns("cd /tmp; rm -rf foo"), vec!["cd *", "rm *"]);
    }

    #[test]
    fn pipe() {
        assert_eq!(patterns("cat file.txt | grep foo"), vec!["cat *", "grep *"]);
    }

    #[test]
    fn mixed() {
        assert_eq!(
            patterns("cd /tmp && rm -rf * | grep err; echo done"),
            vec!["cd *", "rm *", "grep *", "echo *"]
        );
    }

    #[test]
    fn background_operator() {
        assert_eq!(patterns("sleep 5 & echo done"), vec!["sleep *", "echo *"]);
    }

    #[test]
    fn quoted_operator_not_split() {
        assert_eq!(patterns(r#"grep "&&" file.txt"#), vec!["grep *"]);
    }

    #[test]
    fn empty_command() {
        assert!(patterns("").is_empty());
    }

    #[test]
    fn only_whitespace() {
        assert!(patterns("   ").is_empty());
    }

    #[test]
    fn parens_inside_double_quotes_not_extracted() {
        // "fix(tui): ..." should NOT extract "tui" as a subshell command
        assert_eq!(
            patterns(r#"git commit -m "fix(tui): keep lists sized""#),
            vec!["git *"]
        );
    }

    #[test]
    fn interactive_vim_blocked() {
        assert!(check_interactive("vim file.txt").is_some());
    }

    #[test]
    fn interactive_nvim_blocked() {
        assert!(check_interactive("nvim").is_some());
    }

    #[test]
    fn interactive_less_blocked() {
        assert!(check_interactive("less /var/log/syslog").is_some());
    }

    #[test]
    fn interactive_git_rebase_i_blocked() {
        assert!(check_interactive("git rebase -i HEAD~3").is_some());
    }

    #[test]
    fn interactive_git_add_i_blocked() {
        assert!(check_interactive("git add --interactive").is_some());
    }

    #[test]
    fn non_interactive_git_rebase_allowed() {
        assert!(check_interactive("git rebase main").is_none());
    }

    #[test]
    fn non_interactive_cargo_allowed() {
        assert!(check_interactive("cargo build").is_none());
    }

    #[test]
    fn interactive_in_chain_blocked() {
        assert!(check_interactive("echo hello && vim file.txt").is_some());
    }

    #[test]
    fn interactive_with_path_blocked() {
        assert!(check_interactive("/usr/bin/vim file.txt").is_some());
    }
}
