use crate::input::{Mode, SharedMode};
use crate::log;
use crate::permissions::{Decision, Permissions};
use crate::provider::{Message, Provider, Role, ToolDefinition};
use crate::tools::{self, ToolRegistry, ToolResult};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub enum AgentEvent {
    Thinking(String),
    Text(String),
    Steered {
        text: String,
        count: usize,
    },
    ToolCall {
        name: String,
        args: HashMap<String, Value>,
    },
    ToolOutputChunk(String),
    ToolResult {
        content: String,
        is_error: bool,
    },
    Confirm {
        desc: String,
        args: HashMap<String, Value>,
        /// Glob pattern for "always allow this domain/pattern" session approval.
        approval_pattern: Option<String>,
        /// Short LLM-generated description of what the command does.
        summary: Option<String>,
        reply: tokio::sync::oneshot::Sender<(bool, Option<String>)>,
    },
    AskQuestion {
        args: HashMap<String, Value>,
        reply: tokio::sync::oneshot::Sender<String>,
    },
    TokenUsage {
        prompt_tokens: u32,
    },
    Retrying {
        delay: std::time::Duration,
        attempt: u32,
    },
    Done,
    Error(String),
}

/// Shared state the agent task needs to execute tool calls and talk to the LLM.
pub struct AgentContext {
    pub provider: Provider,
    pub model: String,
    pub registry: ToolRegistry,
    pub permissions: Permissions,
    pub shared_mode: SharedMode,
    pub cancel: CancellationToken,
    pub steering: Arc<Mutex<Vec<String>>>,
}

fn system_prompt(mode: Mode) -> String {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".into());

    let base = include_str!("prompts/system.txt");
    let overlay = match mode {
        Mode::Apply | Mode::Yolo => include_str!("prompts/system_apply.txt"),
        Mode::Plan => include_str!("prompts/system_plan.txt"),
        Mode::Normal => "",
    };

    let mut prompt = base.replace("{cwd}", &cwd);
    if !overlay.is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(overlay);
    }

    if let Some(instructions) = crate::instructions::load() {
        prompt.push_str("\n\n");
        prompt.push_str(&instructions);
    }

    prompt
}

pub async fn run_agent(
    ctx: &AgentContext,
    initial_mode: Mode,
    history: &[Message],
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> Vec<Message> {
    let mut messages = Vec::with_capacity(history.len() + 2);
    messages.push(Message {
        role: Role::System,
        content: Some(system_prompt(initial_mode)),
        tool_calls: None,
        tool_call_id: None,
    });
    messages.extend_from_slice(history);

    let tool_defs: Vec<ToolDefinition> = ctx.registry.definitions(&ctx.permissions, initial_mode);
    let mut first = true;

    loop {
        // Inject any user-steered messages queued while the agent was working,
        // but skip on the very first iteration (the triggering message is already in history).
        if !first {
            let pending: Vec<String> = ctx.steering.lock().unwrap().drain(..).collect();
            if !pending.is_empty() {
                let count = pending.len();
                let text = pending.join("\n");
                let _ = tx.send(AgentEvent::Steered {
                    text: text.clone(),
                    count,
                });
                messages.push(Message {
                    role: Role::User,
                    content: Some(text),
                    tool_calls: None,
                    tool_call_id: None,
                });
            }
        }
        first = false;

        let on_retry = |delay: std::time::Duration, attempt: u32| {
            let _ = tx.send(AgentEvent::Retrying { delay, attempt });
        };
        let resp = {
            let _perf = crate::perf::begin("llm_chat");
            match ctx
                .provider
                .chat(
                    &messages,
                    &tool_defs,
                    &ctx.model,
                    &ctx.cancel,
                    Some(&on_retry),
                )
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(AgentEvent::Error(e));
                    messages.remove(0);
                    return messages;
                }
            }
        };

        if let Some(tokens) = resp.prompt_tokens {
            let _ = tx.send(AgentEvent::TokenUsage {
                prompt_tokens: tokens,
            });
        }

        if let Some(ref reasoning) = resp.reasoning_content {
            if !reasoning.is_empty() {
                let _ = tx.send(AgentEvent::Thinking(reasoning.clone()));
            }
        }

        if let Some(ref content) = resp.content {
            if !content.is_empty() {
                let _ = tx.send(AgentEvent::Text(content.clone()));
            }
        }

        let content = resp.content;
        let tool_calls = resp.tool_calls;

        if tool_calls.is_empty() {
            messages.push(Message {
                role: Role::Assistant,
                content,
                tool_calls: None,
                tool_call_id: None,
            });
            let _ = tx.send(AgentEvent::Done);
            messages.remove(0);
            return messages;
        }

        messages.push(Message {
            role: Role::Assistant,
            content,
            tool_calls: Some(tool_calls.clone()),
            tool_call_id: None,
        });

        for tc in &tool_calls {
            let args: HashMap<String, Value> =
                serde_json::from_str(&tc.function.arguments).unwrap_or_default();

            let _ = tx.send(AgentEvent::ToolCall {
                name: tc.function.name.clone(),
                args: args.clone(),
            });

            let tool = match ctx.registry.get(&tc.function.name) {
                Some(t) => t,
                None => {
                    push_tool_reply(
                        &mut messages,
                        tx,
                        &tc.id,
                        &format!("unknown tool: {}", tc.function.name),
                        true,
                    );
                    continue;
                }
            };

            // Read current mode live — allows mid-run mode switches (e.g. toggling to yolo).
            let mode = ctx.shared_mode.load();
            let decision = decide_permission(&ctx.permissions, mode, &tc.function.name, &args);

            let mut confirm_msg: Option<String> = None;
            match decision {
                Decision::Deny => {
                    push_tool_reply(&mut messages, tx, &tc.id, "The user's permission settings blocked this tool call. Try a different approach or ask the user for guidance.", false);
                    continue;
                }
                Decision::Ask => {
                    let desc = tool
                        .needs_confirm(&args)
                        .unwrap_or_else(|| tc.function.name.clone());
                    let approval_pattern = tool.approval_pattern(&args);

                    let summary = if tc.function.name == "bash" {
                        let cmd = tools::str_arg(&args, "command");
                        match tokio::time::timeout(
                            std::time::Duration::from_secs(3),
                            ctx.provider.describe_command(&cmd, &ctx.model),
                        )
                        .await
                        {
                            Ok(Ok(s)) => Some(s),
                            _ => None,
                        }
                    } else {
                        None
                    };

                    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                    let _ = tx.send(AgentEvent::Confirm {
                        desc,
                        args: args.clone(),
                        approval_pattern,
                        summary,
                        reply: reply_tx,
                    });
                    let (approved, user_msg) = reply_rx.await.unwrap_or((false, None));
                    if !approved {
                        let denial = if let Some(ref msg) = user_msg {
                            format!("The user denied this tool call with message: {msg}")
                        } else {
                            "The user denied this tool call. Try a different approach or ask the user for guidance.".to_string()
                        };
                        push_tool_reply(&mut messages, tx, &tc.id, &denial, false);
                        continue;
                    }
                    confirm_msg = user_msg;
                }
                Decision::Allow => {}
            }

            let ToolResult { content, is_error } = if tc.function.name == "ask_user_question" {
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                let _ = tx.send(AgentEvent::AskQuestion {
                    args: args.clone(),
                    reply: reply_tx,
                });
                let answer = reply_rx.await.unwrap_or_else(|_| "no response".into());
                ToolResult {
                    content: answer,
                    is_error: false,
                }
            } else if tc.function.name == "bash" {
                let _perf = crate::perf::begin("tool_bash");
                execute_bash_streaming(&args, tx).await
            } else {
                let _perf = crate::perf::begin("tool_sync");
                tokio::task::block_in_place(|| tool.execute(&args))
            };
            log::entry(
                log::Level::Debug,
                "tool_result",
                &serde_json::json!({
                    "tool": tc.function.name,
                    "id": tc.id,
                    "is_error": is_error,
                    "content_len": content.len(),
                    "content_preview": &content[..content.len().min(500)],
                }),
            );
            let mut model_content = match tc.function.name.as_str() {
                "grep" | "glob" => trim_tool_output_for_model(&content, 200),
                _ => content.clone(),
            };
            if let Some(ref msg) = confirm_msg {
                model_content.push_str(&format!("\n\nUser message: {msg}"));
            }
            messages.push(Message {
                role: Role::Tool,
                content: Some(model_content),
                tool_calls: None,
                tool_call_id: Some(tc.id.clone()),
            });
            let _ = tx.send(AgentEvent::ToolResult { content, is_error });
        }
    }
}

fn decide_permission(
    permissions: &Permissions,
    mode: Mode,
    tool_name: &str,
    args: &HashMap<String, Value>,
) -> Decision {
    if tool_name == "bash" {
        let cmd = tools::str_arg(args, "command");
        let tool_decision = permissions.check_tool(mode, "bash");
        if tool_decision == Decision::Deny {
            return Decision::Deny;
        }
        let bash_decision = permissions.check_bash(mode, &cmd);
        match (&tool_decision, &bash_decision) {
            (_, Decision::Deny) => Decision::Deny,
            (Decision::Allow, Decision::Ask) => Decision::Allow,
            _ => bash_decision,
        }
    } else if tool_name == "web_fetch" {
        let url = tools::str_arg(args, "url");
        let tool_decision = permissions.check_tool(mode, "web_fetch");
        if tool_decision == Decision::Deny {
            return Decision::Deny;
        }
        let pattern_decision = permissions.check_tool_pattern(mode, "web_fetch", &url);
        match (&tool_decision, &pattern_decision) {
            (_, Decision::Deny) => Decision::Deny,
            (_, Decision::Allow) => Decision::Allow,
            (Decision::Allow, Decision::Ask) => Decision::Ask,
            _ => pattern_decision,
        }
    } else {
        permissions.check_tool(mode, tool_name)
    }
}

fn push_tool_reply(
    messages: &mut Vec<Message>,
    tx: &mpsc::UnboundedSender<AgentEvent>,
    tool_call_id: &str,
    content: &str,
    is_error: bool,
) {
    messages.push(Message {
        role: Role::Tool,
        content: Some(content.to_string()),
        tool_calls: None,
        tool_call_id: Some(tool_call_id.to_string()),
    });
    let _ = tx.send(AgentEvent::ToolResult {
        content: content.to_string(),
        is_error,
    });
}

fn trim_tool_output_for_model(content: &str, max_lines: usize) -> String {
    if content == "no matches found" {
        return content.to_string();
    }
    let lines: Vec<&str> = content.lines().collect();
    if lines.len() <= max_lines {
        return content.to_string();
    }
    let mut out = lines[..max_lines].join("\n");
    out.push_str(&format!("\n... (trimmed, {} lines total)", lines.len()));
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
    ToolResult {
        content: output,
        is_error,
    }
}
