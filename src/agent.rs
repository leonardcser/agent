use crate::input::Mode;
use crate::log;
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
    Confirm { desc: String, args: HashMap<String, Value>, reply: tokio::sync::oneshot::Sender<bool> },
    TokenUsage { prompt_tokens: u32 },
    Retrying(std::time::Duration),
    Done,
    Error(String),
}

fn system_prompt(mode: Mode) -> String {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".into());

    let template = match mode {
        Mode::Apply => include_str!("prompts/system_apply.txt"),
        _ => include_str!("prompts/system.txt"),
    };

    template.replace("{cwd}", &cwd)
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
        let on_retry = |delay: std::time::Duration| { let _ = tx.send(AgentEvent::Retrying(delay)); };
        let resp = match provider.chat(&messages, &tool_defs, model, &cancel, Some(&on_retry)).await {
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
                        args: args.clone(),
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
            log::entry("tool_result", &serde_json::json!({
                "tool": tc.function.name,
                "id": tc.id,
                "is_error": is_error,
                "content_len": content.len(),
                "content_preview": &content[..content.len().min(500)],
            }));
            let model_content = match tc.function.name.as_str() {
                "grep" => trim_tool_output_for_model(&content, 200),
                "glob" => trim_tool_output_for_model(&content, 200),
                _ => content.clone(),
            };
            messages.push(Message {
                role: Role::Tool,
                content: Some(model_content),
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

fn trim_tool_output_for_model(content: &str, max_lines: usize) -> String {
    if content == "no matches found" {
        return content.to_string();
    }
    let mut lines = content.lines();
    let total = content.lines().count();
    if total <= max_lines {
        return content.to_string();
    }
    let mut out = String::new();
    for (i, line) in lines.by_ref().take(max_lines).enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(line);
    }
    out.push_str(&format!("\n... (trimmed, {} lines total)", total));
    out
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
