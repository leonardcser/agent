use super::background::ProcessRegistry;
use super::{str_arg, Tool, ToolContext, ToolFuture, ToolResult};
use protocol::EngineEvent;
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;

pub fn format_read_result(output: String, running: bool, exit_code: Option<i32>) -> ToolResult {
    let status = if running {
        "running".to_string()
    } else {
        format!("exited (code {})", exit_code.unwrap_or(-1))
    };
    let content = if output.is_empty() {
        format!("[{status}]")
    } else {
        format!("{output}\n[{status}]")
    };
    ToolResult::ok(content)
}

pub struct ReadProcessOutputTool {
    pub registry: ProcessRegistry,
}

impl Tool for ReadProcessOutputTool {
    fn name(&self) -> &str {
        "read_process_output"
    }

    fn description(&self) -> &str {
        "Read output from a background bash process (proc_1, proc_2, etc). Blocks until the process finishes by default. Set block=false for a non-blocking check of current output."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {"type": "string", "description": "Bash process ID (e.g. proc_1)"},
                "block": {"type": "boolean", "description": "Wait for process to finish (default: true). Set to false for a non-blocking check."},
                "timeout_ms": {"type": "integer", "description": "Max wait time in ms when blocking (default: 30000)"}
            },
            "required": ["id"]
        })
    }

    fn execute<'a>(&'a self, args: HashMap<String, Value>, ctx: &'a ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            let id = str_arg(&args, "id");
            let block = args.get("block").and_then(|v| v.as_bool()).unwrap_or(true);

            if !block {
                return match self.registry.read(&id) {
                    Ok((output, running, exit_code)) => {
                        format_read_result(output, running, exit_code)
                    }
                    Err(e) => ToolResult::err(e),
                };
            }

            // Blocking poll loop with streaming output
            let timeout_ms = args
                .get("timeout_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(30000)
                .min(600_000);
            let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
            let mut accumulated = String::new();

            loop {
                match self.registry.read(&id) {
                    Ok((output, running, exit_code)) => {
                        if !output.is_empty() {
                            for line in output.lines() {
                                let _ = ctx.event_tx.send(EngineEvent::ToolOutput {
                                    call_id: ctx.call_id.to_string(),
                                    chunk: line.to_string(),
                                });
                            }
                            if !accumulated.is_empty() {
                                accumulated.push('\n');
                            }
                            accumulated.push_str(&output);
                        }
                        if !running {
                            break format_read_result(accumulated, false, exit_code);
                        }
                        if ctx.cancel.is_cancelled() {
                            let _ = self.registry.stop(&id).await;
                            break format_read_result(accumulated, false, None);
                        }
                        if tokio::time::Instant::now() >= deadline {
                            break format_read_result(accumulated, true, None);
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    Err(e) => {
                        break ToolResult::err(e);
                    }
                }
            }
        })
    }
}

pub struct StopProcessTool {
    pub registry: ProcessRegistry,
}

impl Tool for StopProcessTool {
    fn name(&self) -> &str {
        "stop_process"
    }

    fn description(&self) -> &str {
        "Stop a running background bash process and return its accumulated output."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {"type": "string", "description": "Bash process ID (e.g. proc_1)"}
            },
            "required": ["id"]
        })
    }

    fn execute<'a>(
        &'a self,
        args: HashMap<String, Value>,
        _ctx: &'a ToolContext,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            let id = str_arg(&args, "id");
            match self.registry.stop(&id).await {
                Ok(output) => ToolResult::ok(if output.is_empty() {
                    "process stopped (no output)".into()
                } else {
                    format!("process stopped\n{output}")
                }),
                Err(e) => ToolResult::err(e),
            }
        })
    }
}
