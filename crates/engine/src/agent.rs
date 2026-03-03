use crate::log;
use crate::permissions::{Decision, Permissions};
use crate::provider::{Provider, ToolDefinition};
use crate::tools::{self, ToolRegistry, ToolResult};
use crate::EngineConfig;
use protocol::{EngineEvent, Message, Mode, ReasoningEffort, Role, ToolOutcome, UiCommand};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

fn next_request_id() -> u64 {
    NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed)
}

/// Main engine task. Runs in a tokio::spawn and processes commands/events.
pub async fn engine_task(
    config: EngineConfig,
    registry: ToolRegistry,
    processes: tools::ProcessRegistry,
    mut cmd_rx: mpsc::UnboundedReceiver<UiCommand>,
    event_tx: mpsc::UnboundedSender<EngineEvent>,
) {
    let client = reqwest::Client::new();

    let _ = event_tx.send(EngineEvent::Ready);

    // Process completion channel for background processes
    let (proc_done_tx, mut proc_done_rx) = mpsc::unbounded_channel::<(String, Option<i32>)>();
    let mut last_model = String::new();

    loop {
        tokio::select! {
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    UiCommand::StartTurn { input, mode, model, reasoning_effort, history, api_base, api_key } => {
                        last_model = model.clone();
                        run_turn(
                            &config, &client, &registry, &config.permissions,
                            &processes, &proc_done_tx, &mut cmd_rx, &event_tx,
                            input, mode, &model, reasoning_effort, history,
                            api_base, api_key,
                        ).await;
                    }
                    UiCommand::Compact { keep_turns, history } => {
                        let provider = build_provider(&config, &client);
                        let cancel = tokio_util::sync::CancellationToken::new();
                        match compact_history(&provider, &history, keep_turns, &last_model, &cancel).await {
                            Ok(messages) => {
                                let _ = event_tx.send(EngineEvent::CompactionComplete { messages });
                            }
                            Err(e) => {
                                let _ = event_tx.send(EngineEvent::TurnError { message: e });
                            }
                        }
                    }
                    UiCommand::GenerateTitle { first_message } => {
                        let provider = build_provider(&config, &client);
                        match provider.complete_title(&first_message, &last_model).await {
                            Ok(title) => {
                                let _ = event_tx.send(EngineEvent::TitleGenerated { title });
                            }
                            Err(_) => {
                                let fallback = first_message.lines().next().unwrap_or("Untitled");
                                let mut title = fallback.to_string();
                                if title.len() > 48 { title.truncate(48); }
                                let _ = event_tx.send(EngineEvent::TitleGenerated {
                                    title: title.trim().to_string(),
                                });
                            }
                        }
                    }
                    _ => {} // Steer, Cancel, etc. only relevant during a turn
                }
            }
            Some((id, exit_code)) = proc_done_rx.recv() => {
                let _ = event_tx.send(EngineEvent::ProcessCompleted { id, exit_code });
            }
            else => break,
        }
    }

    let _ = event_tx.send(EngineEvent::Shutdown { reason: None });
}

fn build_provider(config: &EngineConfig, client: &reqwest::Client) -> Provider {
    Provider::new(
        config.api_base.clone(),
        config.api_key.clone(),
        client.clone(),
    )
    .with_model_config(config.model_config.clone())
}

fn build_provider_with_overrides(
    config: &EngineConfig,
    client: &reqwest::Client,
    api_base: Option<&str>,
    api_key: Option<&str>,
) -> Provider {
    Provider::new(
        api_base.unwrap_or(&config.api_base).to_string(),
        api_key.unwrap_or(&config.api_key).to_string(),
        client.clone(),
    )
    .with_model_config(config.model_config.clone())
}

#[allow(clippy::too_many_arguments)]
async fn run_turn(
    config: &EngineConfig,
    client: &reqwest::Client,
    registry: &ToolRegistry,
    permissions: &Permissions,
    processes: &tools::ProcessRegistry,
    proc_done_tx: &mpsc::UnboundedSender<(String, Option<i32>)>,
    cmd_rx: &mut mpsc::UnboundedReceiver<UiCommand>,
    event_tx: &mpsc::UnboundedSender<EngineEvent>,
    input: String,
    mut mode: Mode,
    model: &str,
    mut reasoning_effort: ReasoningEffort,
    history: Vec<Message>,
    api_base_override: Option<String>,
    api_key_override: Option<String>,
) {
    let provider = build_provider_with_overrides(
        config,
        client,
        api_base_override.as_deref(),
        api_key_override.as_deref(),
    );
    let cancel = tokio_util::sync::CancellationToken::new();

    let mut messages = Vec::with_capacity(history.len() + 2);
    messages.push(Message {
        role: Role::System,
        content: Some(config.system_prompt.clone()),
        reasoning_content: None,
        tool_calls: None,
        tool_call_id: None,
    });
    messages.extend(history);

    if !input.is_empty() {
        messages.push(Message {
            role: Role::User,
            content: Some(input),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
        });
    }

    let tool_defs: Vec<ToolDefinition> = registry.definitions(permissions, mode);
    let mut first = true;

    loop {
        // Drain pending commands (steering, mode changes, cancel, process list)
        if !first {
            loop {
                match cmd_rx.try_recv() {
                    Ok(UiCommand::Steer { text }) => {
                        let _ = event_tx.send(EngineEvent::Steered {
                            text: text.clone(),
                            count: 1,
                        });
                        messages.push(Message {
                            role: Role::User,
                            content: Some(text),
                            reasoning_content: None,
                            tool_calls: None,
                            tool_call_id: None,
                        });
                    }
                    Ok(UiCommand::SetMode { mode: new_mode }) => {
                        mode = new_mode;
                    }
                    Ok(UiCommand::SetReasoningEffort { effort }) => {
                        reasoning_effort = effort;
                    }
                    Ok(UiCommand::Cancel) => {
                        cancel.cancel();
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        }
        first = false;

        if cancel.is_cancelled() {
            messages.remove(0);
            let _ = event_tx.send(EngineEvent::TurnComplete { messages });
            return;
        }

        let on_retry = |delay: std::time::Duration, attempt: u32| {
            let _ = event_tx.send(EngineEvent::Retrying {
                delay_ms: delay.as_millis() as u64,
                attempt,
            });
        };

        let resp = match provider
            .chat(&messages, &tool_defs, model, reasoning_effort, &cancel, Some(&on_retry))
            .await
        {
            Ok(r) => r,
            Err(e) => {
                if e == "cancelled" {
                    messages.remove(0);
                    let _ = event_tx.send(EngineEvent::TurnComplete { messages });
                } else {
                    log::entry(
                        log::Level::Warn,
                        "agent_stop",
                        &serde_json::json!({"reason": "llm_error", "error": e}),
                    );
                    let _ = event_tx.send(EngineEvent::TurnError { message: e });
                }
                return;
            }
        };

        if let Some(tokens) = resp.prompt_tokens {
            let _ = event_tx.send(EngineEvent::TokenUsage {
                prompt_tokens: tokens,
            });
        }

        if let Some(ref reasoning) = resp.reasoning_content {
            if !reasoning.is_empty() {
                let _ = event_tx.send(EngineEvent::Thinking {
                    content: reasoning.clone(),
                });
            }
        }

        if let Some(ref content) = resp.content {
            if !content.is_empty() {
                let _ = event_tx.send(EngineEvent::Text {
                    content: content.clone(),
                });
            }
        }

        let content = resp.content;
        let tool_calls = resp.tool_calls;

        let reasoning = resp.reasoning_content;

        if tool_calls.is_empty() {
            messages.push(Message {
                role: Role::Assistant,
                content,
                reasoning_content: reasoning,
                tool_calls: None,
                tool_call_id: None,
            });
            messages.remove(0);
            let _ = event_tx.send(EngineEvent::TurnComplete { messages });
            return;
        }

        messages.push(Message {
            role: Role::Assistant,
            content,
            reasoning_content: reasoning,
            tool_calls: Some(tool_calls.clone()),
            tool_call_id: None,
        });

        for tc in &tool_calls {
            let args: HashMap<String, Value> =
                serde_json::from_str(&tc.function.arguments).unwrap_or_default();

            let summary = tools::tool_arg_summary(&tc.function.name, &args);
            let _ = event_tx.send(EngineEvent::ToolStarted {
                call_id: tc.id.clone(),
                tool_name: tc.function.name.clone(),
                args: args.clone(),
                summary,
            });

            let tool = match registry.get(&tc.function.name) {
                Some(t) => t,
                None => {
                    push_tool_result(
                        &mut messages,
                        event_tx,
                        &tc.id,
                        &format!("unknown tool: {}", tc.function.name),
                        true,
                    );
                    continue;
                }
            };

            let decision = decide_permission(permissions, mode, &tc.function.name, &args);

            let mut confirm_msg: Option<String> = None;
            match decision {
                Decision::Deny => {
                    push_tool_result(
                        &mut messages, event_tx, &tc.id,
                        "The user's permission settings blocked this tool call. Try a different approach or ask the user for guidance.",
                        false,
                    );
                    continue;
                }
                Decision::Ask => {
                    let desc = tool
                        .needs_confirm(&args)
                        .unwrap_or_else(|| tc.function.name.clone());
                    let approval_pattern = tool.approval_pattern(&args);

                    let cmd_summary = if tc.function.name == "bash" {
                        let cmd = tools::str_arg(&args, "command");
                        match tokio::time::timeout(
                            std::time::Duration::from_secs(3),
                            provider.describe_command(&cmd, model),
                        )
                        .await
                        {
                            Ok(Ok(s)) => Some(s),
                            _ => None,
                        }
                    } else {
                        None
                    };

                    let request_id = next_request_id();
                    let _ = event_tx.send(EngineEvent::RequestPermission {
                        request_id,
                        call_id: tc.id.clone(),
                        tool_name: tc.function.name.clone(),
                        args: args.clone(),
                        confirm_message: desc,
                        approval_pattern,
                        summary: cmd_summary,
                    });

                    let (approved, user_msg) =
                        wait_for_permission(cmd_rx, request_id, &mut mode, &mut reasoning_effort).await;
                    if !approved {
                        let denial = if let Some(ref msg) = user_msg {
                            format!("The user denied this tool call with message: {msg}")
                        } else {
                            "The user denied this tool call. Try a different approach or ask the user for guidance.".to_string()
                        };
                        push_tool_result(&mut messages, event_tx, &tc.id, &denial, false);
                        continue;
                    }
                    confirm_msg = user_msg;
                }
                Decision::Allow => {}
            }

            let ToolResult { content, is_error } = if tc.function.name == "ask_user_question" {
                let request_id = next_request_id();
                let _ = event_tx.send(EngineEvent::RequestAnswer {
                    request_id,
                    args: args.clone(),
                });
                let answer = wait_for_answer(cmd_rx, request_id, &mut mode, &mut reasoning_effort).await;
                ToolResult {
                    content: answer.unwrap_or_else(|| "no response".into()),
                    is_error: false,
                }
            } else if tc.function.name == "bash" && tools::bool_arg(&args, "run_in_background") {
                let command = tools::str_arg(&args, "command");
                match tokio::process::Command::new("sh")
                    .arg("-c")
                    .arg(&command)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                {
                    Ok(child) => {
                        let id = processes.next_id();
                        processes.spawn(id.clone(), &command, child, proc_done_tx.clone());
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
            } else if tc.function.name == "read_process_output"
                && args.get("block").and_then(|v| v.as_bool()).unwrap_or(true)
            {
                let id = tools::str_arg(&args, "id");
                let timeout_ms = args
                    .get("timeout_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(30000)
                    .min(600_000);
                let deadline =
                    tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
                let mut accumulated = String::new();
                loop {
                    match processes.read(&id) {
                        Ok((output, running, exit_code)) => {
                            if !output.is_empty() {
                                for line in output.lines() {
                                    let _ = event_tx.send(EngineEvent::ToolOutput {
                                        call_id: tc.id.clone(),
                                        chunk: line.to_string(),
                                    });
                                }
                                if !accumulated.is_empty() {
                                    accumulated.push('\n');
                                }
                                accumulated.push_str(&output);
                            }
                            if !running {
                                break tools::format_read_result(accumulated, false, exit_code);
                            }
                            if cancel.is_cancelled() {
                                let _ = processes.stop(&id);
                                break tools::format_read_result(accumulated, false, None);
                            }
                            if tokio::time::Instant::now() >= deadline {
                                break tools::format_read_result(accumulated, true, None);
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        }
                        Err(e) => {
                            break tools::ToolResult {
                                content: e,
                                is_error: true,
                            };
                        }
                    }
                }
            } else if tc.function.name == "bash" {
                execute_bash_streaming(&args, &tc.id, event_tx).await
            } else if tc.function.name == "web_fetch" {
                let raw = tokio::task::block_in_place(|| tool.execute(&args));
                if raw.is_error {
                    raw
                } else {
                    let prompt = tools::str_arg(&args, "prompt");
                    match provider
                        .extract_web_content(&raw.content, &prompt, model)
                        .await
                    {
                        Ok(extracted) => ToolResult {
                            content: extracted,
                            is_error: false,
                        },
                        Err(_) => raw,
                    }
                }
            } else {
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
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: Some(tc.id.clone()),
            });
            let _ = event_tx.send(EngineEvent::ToolFinished {
                call_id: tc.id.clone(),
                result: ToolOutcome { content, is_error },
            });
        }
    }
}

/// Wait for a PermissionDecision matching the given request_id.
async fn wait_for_permission(
    cmd_rx: &mut mpsc::UnboundedReceiver<UiCommand>,
    request_id: u64,
    mode: &mut Mode,
    reasoning_effort: &mut ReasoningEffort,
) -> (bool, Option<String>) {
    loop {
        match cmd_rx.recv().await {
            Some(UiCommand::PermissionDecision {
                request_id: id,
                approved,
                message,
            }) if id == request_id => {
                return (approved, message);
            }
            Some(UiCommand::SetMode { mode: new_mode }) => *mode = new_mode,
            Some(UiCommand::SetReasoningEffort { effort }) => *reasoning_effort = effort,
            Some(UiCommand::Cancel) => return (false, None),
            None => return (false, None),
            _ => {}
        }
    }
}

/// Wait for a QuestionAnswer matching the given request_id.
async fn wait_for_answer(
    cmd_rx: &mut mpsc::UnboundedReceiver<UiCommand>,
    request_id: u64,
    mode: &mut Mode,
    reasoning_effort: &mut ReasoningEffort,
) -> Option<String> {
    loop {
        match cmd_rx.recv().await {
            Some(UiCommand::QuestionAnswer {
                request_id: id,
                answer,
            }) if id == request_id => return answer,
            Some(UiCommand::SetMode { mode: new_mode }) => *mode = new_mode,
            Some(UiCommand::SetReasoningEffort { effort }) => *reasoning_effort = effort,
            Some(UiCommand::Cancel) => return None,
            None => return None,
            _ => {}
        }
    }
}

async fn compact_history(
    provider: &Provider,
    messages: &[Message],
    keep_turns: usize,
    model: &str,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Vec<Message>, String> {
    let mut user_count = 0;
    let mut cut = messages.len();
    for (i, m) in messages.iter().enumerate().rev() {
        if m.role == Role::User {
            user_count += 1;
            if user_count >= keep_turns {
                cut = i;
                break;
            }
        }
    }
    if cut == 0 || cut >= messages.len() {
        return Err("not enough history to compact".into());
    }

    let to_summarize = &messages[..cut];
    let summary = provider.compact(to_summarize, model, cancel).await?;

    let mut new_messages = vec![Message {
        role: Role::System,
        content: Some(format!("Summary of prior conversation:\n\n{summary}")),
        reasoning_content: None,
        tool_calls: None,
        tool_call_id: None,
    }];
    new_messages.extend_from_slice(&messages[cut..]);
    Ok(new_messages)
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

fn push_tool_result(
    messages: &mut Vec<Message>,
    event_tx: &mpsc::UnboundedSender<EngineEvent>,
    tool_call_id: &str,
    content: &str,
    is_error: bool,
) {
    messages.push(Message {
        role: Role::Tool,
        content: Some(content.to_string()),
        reasoning_content: None,
        tool_calls: None,
        tool_call_id: Some(tool_call_id.to_string()),
    });
    let _ = event_tx.send(EngineEvent::ToolFinished {
        call_id: tool_call_id.to_string(),
        result: ToolOutcome {
            content: content.to_string(),
            is_error,
        },
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
    call_id: &str,
    event_tx: &mpsc::UnboundedSender<EngineEvent>,
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
                        let _ = event_tx.send(EngineEvent::ToolOutput {
                            call_id: call_id.to_string(),
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
                        let _ = event_tx.send(EngineEvent::ToolOutput {
                            call_id: call_id.to_string(),
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
        }
    }

    let status = child.wait().await;
    let is_error = status.map(|s| !s.success()).unwrap_or(true);
    ToolResult {
        content: output,
        is_error,
    }
}
