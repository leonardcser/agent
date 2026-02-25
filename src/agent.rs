use crate::input::Mode;
use crate::permissions::{Decision, Permissions};
use crate::provider::{Message, Provider, Role, ToolDefinition};
use crate::tools::{self, ToolRegistry, ToolResult};
use serde_json::Value;
use std::collections::HashMap;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub enum AgentEvent {
    Text(String),
    ToolCall { name: String, args: HashMap<String, Value> },
    ToolOutputChunk(String),
    ToolResult { content: String, is_error: bool },
    Confirm { desc: String, reply: tokio::sync::oneshot::Sender<bool> },
    TokenUsage { prompt_tokens: u32 },
    Done,
    Error(String),
}

fn system_prompt(mode: Mode) -> String {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".into());

    let mut prompt = String::new();
    prompt.push_str("You are a coding assistant working in the user's terminal.\n");
    prompt.push_str("You help with software engineering tasks: reading code, finding bugs, explaining patterns, and implementing changes.\n\n");
    prompt.push_str(&format!("Working directory: {}\n\n", cwd));
    prompt.push_str("Guidelines:\n");
    prompt.push_str("- Read relevant files before making suggestions\n");
    prompt.push_str("- Be concise and direct\n");
    prompt.push_str("- Use grep and glob to search the codebase efficiently\n");
    if mode == Mode::Apply {
        prompt.push_str("- You have write access: use write_file and edit_file to implement changes\n");
        prompt.push_str("- Always read a file with read_file before editing it — edit_file will reject stale edits\n");
        prompt.push_str("- When modifying files, explain what you're changing and why\n");
    }
    prompt
}

pub async fn run_agent(
    provider: &Provider,
    model: &str,
    history: &[Message],
    registry: &ToolRegistry,
    mode: Mode,
    permissions: &Permissions,
    tx: &mpsc::UnboundedSender<AgentEvent>,
    cancel: CancellationToken,
) -> Vec<Message> {
    let mut messages = Vec::with_capacity(history.len() + 2);
    messages.push(Message {
        role: Role::System,
        content: Some(system_prompt(mode)),
        tool_calls: None,
        tool_call_id: None,
    });
    messages.extend_from_slice(history);

    let tool_defs: Vec<ToolDefinition> = registry.definitions(permissions, mode);

    loop {
        let resp = match provider.chat(&messages, &tool_defs, model, &cancel).await {
            Ok(r) => r,
            Err(e) => {
                let _ = tx.send(AgentEvent::Error(e));
                messages.remove(0);
                return messages;
            }
        };

        if let Some(tokens) = resp.prompt_tokens {
            let _ = tx.send(AgentEvent::TokenUsage { prompt_tokens: tokens });
        }

        if let Some(ref content) = resp.content {
            if !content.is_empty() {
                let _ = tx.send(AgentEvent::Text(content.clone()));
            }
        }

        if resp.tool_calls.is_empty() {
            messages.push(Message {
                role: Role::Assistant,
                content: resp.content,
                tool_calls: None,
                tool_call_id: None,
            });
            let _ = tx.send(AgentEvent::Done);
            messages.remove(0);
            return messages;
        }

        messages.push(Message {
            role: Role::Assistant,
            content: resp.content.clone(),
            tool_calls: Some(resp.tool_calls.clone()),
            tool_call_id: None,
        });

        for tc in &resp.tool_calls {
            let args: HashMap<String, Value> =
                serde_json::from_str(&tc.function.arguments).unwrap_or_default();

            let _ = tx.send(AgentEvent::ToolCall {
                name: tc.function.name.clone(),
                args: args.clone(),
            });

            let tool = match registry.get(&tc.function.name) {
                Some(t) => t,
                None => {
                    let result = format!("unknown tool: {}", tc.function.name);
                    messages.push(Message {
                        role: Role::Tool,
                        content: Some(result.clone()),
                        tool_calls: None,
                        tool_call_id: Some(tc.id.clone()),
                    });
                    let _ = tx.send(AgentEvent::ToolResult {
                        content: result,
                        is_error: true,
                    });
                    continue;
                }
            };

            // Check permissions: for bash, check the command pattern; for other tools, check by name
            let decision = if tc.function.name == "bash" {
                let cmd = tools::str_arg(&args, "command");
                let tool_decision = permissions.check_tool(mode, "bash");
                if tool_decision == Decision::Deny {
                    Decision::Deny
                } else {
                    let bash_decision = permissions.check_bash(mode, &cmd);
                    // Tool-level deny overrides bash-level allow
                    match (&tool_decision, &bash_decision) {
                        (_, Decision::Deny) => Decision::Deny,
                        (Decision::Allow, Decision::Ask) => Decision::Allow,
                        _ => bash_decision,
                    }
                }
            } else {
                permissions.check_tool(mode, &tc.function.name)
            };

            match decision {
                Decision::Deny => {
                    let result = "denied by permissions".to_string();
                    messages.push(Message {
                        role: Role::Tool,
                        content: Some(result.clone()),
                        tool_calls: None,
                        tool_call_id: Some(tc.id.clone()),
                    });
                    let _ = tx.send(AgentEvent::ToolResult {
                        content: result,
                        is_error: false,
                    });
                    continue;
                }
                Decision::Ask => {
                    let desc = tool.needs_confirm(&args)
                        .unwrap_or_else(|| tc.function.name.clone());
                    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                    let _ = tx.send(AgentEvent::Confirm {
                        desc,
                        reply: reply_tx,
                    });
                    let confirmed = reply_rx.await.unwrap_or(false);
                    if !confirmed {
                        let result = "denied by user".to_string();
                        messages.push(Message {
                            role: Role::Tool,
                            content: Some(result.clone()),
                            tool_calls: None,
                            tool_call_id: Some(tc.id.clone()),
                        });
                        let _ = tx.send(AgentEvent::ToolResult {
                            content: result,
                            is_error: false,
                        });
                        continue;
                    }
                }
                Decision::Allow => {}
            }

            let ToolResult { content, is_error } = if tc.function.name == "bash" {
                execute_bash_streaming(&args, tx).await
            } else {
                tool.execute(&args)
            };
            messages.push(Message {
                role: Role::Tool,
                content: Some(content.clone()),
                tool_calls: None,
                tool_call_id: Some(tc.id.clone()),
            });
            let _ = tx.send(AgentEvent::ToolResult {
                content,
                is_error,
            });
        }
    }
}

async fn execute_bash_streaming(
    args: &HashMap<String, Value>,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> ToolResult {
    let command = tools::str_arg(args, "command");
    let timeout = tools::timeout_arg(args, 120);

    let mut child = match tokio::process::Command::new("sh")
        .arg("-c")
        .arg(&command)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return ToolResult { content: e.to_string(), is_error: true },
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
                        let _ = tx.send(AgentEvent::ToolOutputChunk(line.clone()));
                        if !output.is_empty() { output.push('\n'); }
                        output.push_str(&line);
                    }
                    _ => stdout_done = true,
                }
            }
            line = stderr_reader.next_line(), if !stderr_done => {
                match line {
                    Ok(Some(line)) => {
                        let _ = tx.send(AgentEvent::ToolOutputChunk(line.clone()));
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
        }
    }

    let status = child.wait().await;
    let is_error = status.map(|s| !s.success()).unwrap_or(true);
    ToolResult { content: output, is_error }
}
