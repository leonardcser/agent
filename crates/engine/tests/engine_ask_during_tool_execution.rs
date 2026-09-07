//! Integration coverage for auxiliary engine asks and explicit model switches
//! across in-flight provider requests and concurrent or sequential tool waits.

use engine::EngineConfig;
use protocol::{
    AgentMode, Content, EngineEvent, Message, ModelConfig, ModelTarget, ReasoningEffort,
    RequestRuntimeConfig, StartTurnPayload, ToolDef, ToolExecutionMode, ToolHookFlags,
    ToolMetadata, UiCommand,
};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

const TOOL_NAME: &str = "needs_engine_ask";
const TOOL_CALL_ID: &str = "needs_engine_ask:1";
const PARALLEL_TOOL_A: &str = "parallel_a";
const PARALLEL_TOOL_B: &str = "parallel_b";
const PARALLEL_CALL_A: &str = "parallel_a:1";
const PARALLEL_CALL_B: &str = "parallel_b:1";
const TEST_DEADLINE: Duration = Duration::from_secs(30);

fn primary_turn_sse() -> String {
    let block_start = format!(
        r#"{{"type":"content_block_start","index":0,"content_block":{{"type":"tool_use","id":"{TOOL_CALL_ID}","name":"{TOOL_NAME}","input":{{}}}}}}"#
    );
    let events: Vec<&str> = vec![
        r#"{"type":"message_start","message":{"id":"m","type":"message","role":"assistant","content":[],"model":"x","stop_reason":null,"usage":{"input_tokens":5,"output_tokens":1}}}"#,
        &block_start,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":12}}"#,
        r#"{"type":"message_stop"}"#,
    ];
    sse(&events)
}

fn parallel_tools_sse() -> String {
    let block_a = format!(
        r#"{{"type":"content_block_start","index":0,"content_block":{{"type":"tool_use","id":"{PARALLEL_CALL_A}","name":"{PARALLEL_TOOL_A}","input":{{}}}}}}"#
    );
    let block_b = format!(
        r#"{{"type":"content_block_start","index":1,"content_block":{{"type":"tool_use","id":"{PARALLEL_CALL_B}","name":"{PARALLEL_TOOL_B}","input":{{}}}}}}"#
    );
    let events: Vec<&str> = vec![
        r#"{"type":"message_start","message":{"id":"m-parallel","type":"message","role":"assistant","content":[],"model":"x","stop_reason":null,"usage":{"input_tokens":5,"output_tokens":1}}}"#,
        &block_a,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        &block_b,
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
        r#"{"type":"content_block_stop","index":1}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":12}}"#,
        r#"{"type":"message_stop"}"#,
    ];
    sse(&events)
}

fn aux_text_sse() -> String {
    let events: Vec<&str> = vec![
        r#"{"type":"message_start","message":{"id":"m2","type":"message","role":"assistant","content":[],"model":"x","stop_reason":null,"usage":{"input_tokens":5,"output_tokens":1}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"sub-answer"}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":3}}"#,
        r#"{"type":"message_stop"}"#,
    ];
    sse(&events)
}

fn sse(events: &[&str]) -> String {
    let mut body = String::new();
    for ev in events {
        body.push_str("data: ");
        body.push_str(ev);
        body.push_str("\n\n");
    }
    body
}

async fn read_json_request(sock: &mut tokio::net::TcpStream) -> serde_json::Value {
    let mut buf = [0u8; 8192];
    let mut accumulated = Vec::new();
    let header_end = loop {
        let read = sock.read(&mut buf).await.unwrap();
        assert!(read > 0, "connection closed before request headers");
        accumulated.extend_from_slice(&buf[..read]);
        if let Some(end) = accumulated
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|index| index + 4)
        {
            break end;
        }
    };
    let headers = String::from_utf8_lossy(&accumulated[..header_end]);
    let content_len = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .expect("request content-length");
    while accumulated.len().saturating_sub(header_end) < content_len {
        let read = sock.read(&mut buf).await.unwrap();
        assert!(read > 0, "connection closed before request body");
        accumulated.extend_from_slice(&buf[..read]);
    }
    serde_json::from_slice(&accumulated[header_end..header_end + content_len]).unwrap()
}

async fn write_sse_response(sock: &mut tokio::net::TcpStream, body: &str) {
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    sock.write_all(header.as_bytes()).await.unwrap();
    sock.write_all(body.as_bytes()).await.unwrap();
    sock.flush().await.unwrap();
}

/// Accepts connections in a loop. Request #0 is the main turn (returns
/// a tool_use); every subsequent request is treated as an EngineAsk and
/// gets a plain text response.
async fn run_server(
    listener: TcpListener,
    counter: Arc<AtomicUsize>,
    request_tx: tokio::sync::mpsc::UnboundedSender<serde_json::Value>,
) {
    loop {
        let (mut sock, _) = match listener.accept().await {
            Ok(s) => s,
            Err(_) => return,
        };
        let counter = counter.clone();
        let request_tx = request_tx.clone();
        tokio::spawn(async move {
            let _ = request_tx.send(read_json_request(&mut sock).await);
            let request_index = counter.fetch_add(1, Ordering::SeqCst);
            let body = if request_index == 0 {
                primary_turn_sse()
            } else {
                aux_text_sse()
            };
            write_sse_response(&mut sock, &body).await;
        });
    }
}

struct BlockingDispatcher {
    releases: HashMap<String, Arc<Semaphore>>,
}

impl engine::tools::ToolDispatcher for BlockingDispatcher {
    fn definitions(&self) -> Vec<smelt_provider::ToolDefinition> {
        Vec::new()
    }

    fn contains(&self, name: &str) -> bool {
        self.releases.contains_key(name)
    }

    fn evaluate_tool_call(
        &self,
        _turn_id: u64,
        name: &str,
        _args: &HashMap<String, serde_json::Value>,
        _mode: AgentMode,
        _permission_overrides: Option<&protocol::PermissionOverrides>,
    ) -> Option<protocol::ToolEvaluation> {
        self.contains(name).then(|| protocol::ToolEvaluation {
            decision: protocol::Decision::Allow,
            metadata: ToolMetadata::default(),
        })
    }

    fn dispatch<'a>(
        &'a self,
        name: &str,
        _args: HashMap<String, serde_json::Value>,
        _ctx: &'a engine::tools::ToolContext,
    ) -> Option<engine::tools::ToolFuture<'a>> {
        let release = Arc::clone(self.releases.get(name)?);
        let name = name.to_string();
        Some(Box::pin(async move {
            let _permit = release.acquire().await.expect("tool release semaphore");
            engine::tools::ToolResult::ok(format!("{name} finished"))
        }))
    }
}

#[tokio::test(flavor = "current_thread")]
async fn engine_ask_during_tool_execution_is_not_silently_dropped() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let req_counter = Arc::new(AtomicUsize::new(0));
    let (request_tx, mut request_rx) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(run_server(listener, req_counter.clone(), request_tx));

    let config = EngineConfig {
        system_prompt_override: Some("test system".into()),
        host_callbacks: engine::HostCallbacks::Disabled,
        ..EngineConfig::new(PathBuf::from("/tmp"), Arc::new(engine::clock::RealClock))
    };
    let target = ModelTarget {
        model: "test-model".into(),
        api_base: format!("http://{addr}"),
        api_key: "test-key".into(),
        provider_type: "anthropic-compatible".into(),
        config: ModelConfig {
            max_tokens: Some(4096),
            ..ModelConfig::default()
        },
    };

    let mut handle = engine::start(config, Box::new(engine::tools::EmptyDispatcher));

    loop {
        match handle.recv().await {
            Some(EngineEvent::Ready) => break,
            Some(_) => continue,
            None => panic!("engine closed before Ready"),
        }
    }

    // Register the tool as a Lua-defined tool with no hooks - classify
    // pushes it straight to `pending_tools` and emits ToolDispatch.
    let tool = ToolDef {
        name: TOOL_NAME.into(),
        description: "calls smelt.engine.ask while executing".into(),
        parameters: serde_json::json!({"type":"object","properties":{}}),
        modes: None,
        execution_mode: ToolExecutionMode::Concurrent,
        override_core: false,
        hooks: ToolHookFlags::default(),
        headless: true,
    };

    handle.send(UiCommand::StartTurn(Box::new(StartTurnPayload {
        turn_id: 1,
        input: protocol::StartTurnInput::user(Content::text("go")),
        mode: AgentMode::normal(),
        model_target: target.clone(),
        request_config: RequestRuntimeConfig::default(),
        reasoning_effort: ReasoningEffort::Off,
        fast_mode: false,
        history: protocol::ModelHistorySource::items(Vec::new()),
        session_id: "sess".into(),
        sessions_root: PathBuf::from("/tmp"),
        persistence: protocol::PersistenceScope::default(),
        permission_overrides: Some(protocol::PermissionOverrides {
            tools: Some(protocol::RuleSetOverride {
                allow: vec![TOOL_NAME.into()],
                ask: Vec::new(),
                deny: Vec::new(),
            }),
            subcommands: Default::default(),
        }),
        system_prompt: Some("test system".into()),
        tools: vec![tool],
    })));

    // Pose as the Lua host: on ToolDispatch, simulate a Lua tool that sends
    // EngineAsk and waits for EngineAskResponse before returning ToolResult.
    let ask_id: u64 = 0xDEAD_BEEF;
    let ask_target = ModelTarget {
        model: "ask-model".into(),
        config: ModelConfig {
            max_tokens: Some(333),
            input_cost: Some(4.5),
            ..Default::default()
        },
        ..target.clone()
    };
    let switched_target = ModelTarget {
        model: "switched-model".into(),
        config: ModelConfig {
            max_tokens: Some(777),
            tool_calling: Some(false),
            ..Default::default()
        },
        ..target.clone()
    };
    let mut sent_engine_ask = false;
    let mut got_response = false;
    let mut turn_completed = false;
    let mut tool_dispatch: Option<(u64, protocol::InvocationId)> = None;

    tokio::time::timeout(TEST_DEADLINE, async {
        loop {
            match handle.recv().await {
                Some(EngineEvent::ToolEvaluationRequest { request_id, .. }) => {
                    handle.send(UiCommand::ToolEvaluationResponse {
                        request_id,
                        evaluation: protocol::ToolEvaluation {
                            decision: protocol::Decision::Allow,
                            metadata: ToolMetadata::default(),
                        },
                    });
                }
                Some(EngineEvent::ToolDispatch {
                    request_id,
                    invocation_id,
                    call_id,
                    ..
                }) => {
                    assert_eq!(call_id, TOOL_CALL_ID);
                    tool_dispatch = Some((request_id, invocation_id));
                    if !sent_engine_ask {
                        sent_engine_ask = true;
                        handle.send(UiCommand::SetTurnModel {
                            target: Box::new(switched_target.clone()),
                            system_prompt: "switched system".into(),
                        });
                        handle.send(UiCommand::EngineAsk {
                            id: ask_id,
                            system: "sub-system".into(),
                            messages: vec![Message::user(Content::text("sub-q"))],
                            target: Box::new(ask_target.clone()),
                            request_config: RequestRuntimeConfig::default(),
                            response_format: None,
                            reasoning_effort: ReasoningEffort::Off,
                            fast_mode: false,
                            tools: Vec::new(),
                            session_id: "sess".into(),
                            persistence: protocol::PersistenceScope::default(),
                            stream: false,
                            visible_retries: false,
                        });
                    }
                }
                Some(EngineEvent::EngineAskResponse { id, .. }) if id == ask_id => {
                    got_response = true;
                    // Unblock the tool so the engine can complete its turn.
                    if let Some((request_id, invocation_id)) = tool_dispatch {
                        handle.send(UiCommand::ToolResult {
                            request_id,
                            invocation_id,
                            call_id: TOOL_CALL_ID.into(),
                            content: "tool finished".into(),
                            is_error: false,
                            metadata: None,
                            display_content: Vec::new(),
                            attachment: None,
                        });
                    }
                }
                Some(EngineEvent::TurnComplete { .. }) => {
                    turn_completed = true;
                    break;
                }
                Some(EngineEvent::TurnError { message, .. }) => panic!("turn failed: {message}"),
                Some(_) => {}
                None => panic!("engine stopped before turn completion"),
            }
        }
    })
    .await
    .expect("concurrent tool turn completion");

    server.abort();

    assert!(
        sent_engine_ask,
        "test never received ToolDispatch - the test plumbing is broken \
         before the bug under test is exercised"
    );
    assert!(
        got_response,
        "EngineAsk sent during tool execution must produce EngineAskResponse"
    );
    assert!(
        turn_completed,
        "turn did not complete after the tool result"
    );
    let requests: Vec<_> = std::iter::from_fn(|| request_rx.try_recv().ok()).collect();
    let ask_request = requests
        .iter()
        .find(|request| request["model"] == "ask-model")
        .expect("EngineAsk should dispatch its own complete target");
    assert_eq!(ask_request["max_tokens"], 333);
    let switched_request = requests
        .iter()
        .find(|request| request["model"] == "switched-model")
        .expect("the request after concurrent tool execution should use the switched target");
    assert_eq!(switched_request["max_tokens"], 777);
    assert!(
        switched_request
            .get("tools")
            .is_none_or(|tools| tools.as_array().is_some_and(Vec::is_empty)),
        "tool-calling capability must come from the switched target: {switched_request}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn queued_message_during_streamed_tool_call_is_acknowledged_after_tool_finishes() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (draft_streamed_tx, draft_streamed_rx) = tokio::sync::oneshot::channel();
    let (finish_response_tx, finish_response_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        let _ = read_json_request(&mut first).await;
        let body = primary_turn_sse();
        let split = body
            .find("data: {\"type\":\"message_delta\"")
            .expect("primary response message delta");
        first
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        let prefix = &body[..split];
        first
            .write_all(format!("{:X}\r\n{prefix}\r\n", prefix.len()).as_bytes())
            .await
            .unwrap();
        first.flush().await.unwrap();
        draft_streamed_tx.send(()).unwrap();

        let (mut barrier, _) = listener.accept().await.unwrap();
        let _ = read_json_request(&mut barrier).await;
        write_sse_response(&mut barrier, &aux_text_sse()).await;

        finish_response_rx.await.unwrap();
        let suffix = &body[split..];
        first
            .write_all(format!("{:X}\r\n{suffix}\r\n0\r\n\r\n", suffix.len()).as_bytes())
            .await
            .unwrap();
        first.flush().await.unwrap();

        let (mut second, _) = listener.accept().await.unwrap();
        let _ = read_json_request(&mut second).await;
        write_sse_response(&mut second, &aux_text_sse()).await;
    });

    let release_tool = Arc::new(Semaphore::new(0));
    let dispatcher = BlockingDispatcher {
        releases: HashMap::from([(TOOL_NAME.to_string(), Arc::clone(&release_tool))]),
    };
    let config = EngineConfig {
        system_prompt_override: Some("test system".into()),
        host_callbacks: engine::HostCallbacks::Disabled,
        ..EngineConfig::new(PathBuf::from("/tmp"), Arc::new(engine::clock::RealClock))
    };
    let target = ModelTarget {
        model: "test-model".into(),
        api_base: format!("http://{addr}"),
        api_key: "test-key".into(),
        provider_type: "anthropic-compatible".into(),
        config: ModelConfig::default(),
    };
    let mut handle = engine::start(config, Box::new(dispatcher));
    while !matches!(handle.recv().await, Some(EngineEvent::Ready)) {}

    handle.send(UiCommand::StartTurn(Box::new(StartTurnPayload {
        turn_id: 1,
        input: protocol::StartTurnInput::user(Content::text("read it")),
        mode: AgentMode::normal(),
        model_target: target.clone(),
        request_config: RequestRuntimeConfig::default(),
        reasoning_effort: ReasoningEffort::Off,
        fast_mode: false,
        history: protocol::ModelHistorySource::items(Vec::new()),
        session_id: "sess".into(),
        sessions_root: PathBuf::from("/tmp"),
        persistence: protocol::PersistenceScope::default(),
        permission_overrides: None,
        system_prompt: Some("test system".into()),
        tools: Vec::new(),
    })));

    draft_streamed_rx.await.unwrap();
    handle.send(UiCommand::Steer {
        input: protocol::StartTurnInput::user(Content::text("queued follow-up"))
            .with_sent_at_ms(1_742_567_823_000),
    });
    let barrier_id = 0xA11CE;
    handle.send(UiCommand::EngineAsk {
        id: barrier_id,
        system: "command barrier".into(),
        messages: vec![Message::user(Content::text("ack"))],
        target: Box::new(target),
        request_config: RequestRuntimeConfig::default(),
        response_format: None,
        reasoning_effort: ReasoningEffort::Off,
        fast_mode: false,
        tools: Vec::new(),
        session_id: "sess".into(),
        persistence: protocol::PersistenceScope::default(),
        stream: false,
        visible_retries: false,
    });
    tokio::time::timeout(TEST_DEADLINE, async {
        loop {
            match handle.recv().await {
                Some(EngineEvent::EngineAskResponse { id, .. }) if id == barrier_id => break,
                Some(EngineEvent::TurnError { message, .. }) => panic!("turn failed: {message}"),
                Some(_) => {}
                None => panic!("engine stopped before processing the command barrier"),
            }
        }
    })
    .await
    .expect("engine should consume steering before the provider response completes");
    finish_response_tx.send(()).unwrap();

    let mut lifecycle = Vec::new();
    tokio::time::timeout(TEST_DEADLINE, async {
        while lifecycle.len() < 2 {
            match handle.recv().await {
                Some(EngineEvent::ToolStarted { call_id, .. }) if call_id == TOOL_CALL_ID => {
                    lifecycle.push("tool_started");
                    release_tool.add_permits(1);
                }
                Some(EngineEvent::ToolFinished { call_id, .. }) if call_id == TOOL_CALL_ID => {
                    lifecycle.push("tool_finished");
                }
                Some(EngineEvent::Steered { text, count, .. }) => {
                    assert_eq!(text, "queued follow-up");
                    assert_eq!(count, 1);
                    lifecycle.push("steered");
                }
                Some(EngineEvent::TurnError { message, .. }) => panic!("turn failed: {message}"),
                Some(_) => {}
                None => panic!("engine stopped before queued message was acknowledged"),
            }
        }
    })
    .await
    .expect("streamed tool and queued-message acknowledgement");

    assert_eq!(
        lifecycle,
        ["tool_started", "tool_finished"],
        "queued input must not be acknowledged between a streamed tool draft and its execution"
    );

    tokio::time::timeout(TEST_DEADLINE, async {
        let mut acknowledged = false;
        loop {
            match handle.recv().await {
                Some(EngineEvent::Steered { sent_at_ms, .. }) => {
                    assert_eq!(sent_at_ms, 1_742_567_823_000);
                    acknowledged = true;
                }
                Some(EngineEvent::HistoryAppended { delta, .. }) if acknowledged => {
                    assert!(matches!(
                        delta.items.as_slice(),
                        [protocol::HistoryItem::User {
                            sent_at_ms: Some(1_742_567_823_000),
                            ..
                        }]
                    ));
                    break;
                }
                Some(EngineEvent::TurnError { message, .. }) => panic!("turn failed: {message}"),
                Some(_) => {}
                None => panic!("engine stopped before queued message was acknowledged"),
            }
        }
    })
    .await
    .expect("queued message acknowledgement after tool completion");
    server.await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn queued_message_is_acknowledged_after_all_parallel_tools_finish() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        let _ = read_json_request(&mut first).await;
        write_sse_response(&mut first, &parallel_tools_sse()).await;

        let (mut second, _) = listener.accept().await.unwrap();
        let _ = read_json_request(&mut second).await;
        write_sse_response(&mut second, &aux_text_sse()).await;
    });

    let release_a = Arc::new(Semaphore::new(0));
    let release_b = Arc::new(Semaphore::new(0));
    let dispatcher = BlockingDispatcher {
        releases: HashMap::from([
            (PARALLEL_TOOL_A.to_string(), Arc::clone(&release_a)),
            (PARALLEL_TOOL_B.to_string(), Arc::clone(&release_b)),
        ]),
    };
    let config = EngineConfig {
        system_prompt_override: Some("test system".into()),
        host_callbacks: engine::HostCallbacks::Disabled,
        ..EngineConfig::new(PathBuf::from("/tmp"), Arc::new(engine::clock::RealClock))
    };
    let target = ModelTarget {
        model: "test-model".into(),
        api_base: format!("http://{addr}"),
        api_key: "test-key".into(),
        provider_type: "anthropic-compatible".into(),
        config: ModelConfig::default(),
    };
    let mut handle = engine::start(config, Box::new(dispatcher));
    while !matches!(handle.recv().await, Some(EngineEvent::Ready)) {}

    handle.send(UiCommand::StartTurn(Box::new(StartTurnPayload {
        turn_id: 1,
        input: protocol::StartTurnInput::user(Content::text("run both")),
        mode: AgentMode::normal(),
        model_target: target,
        request_config: RequestRuntimeConfig::default(),
        reasoning_effort: ReasoningEffort::Off,
        fast_mode: false,
        history: protocol::ModelHistorySource::items(Vec::new()),
        session_id: "sess".into(),
        sessions_root: PathBuf::from("/tmp"),
        persistence: protocol::PersistenceScope::default(),
        permission_overrides: None,
        system_prompt: Some("test system".into()),
        tools: Vec::new(),
    })));

    let expected_calls = HashSet::from([PARALLEL_CALL_A, PARALLEL_CALL_B]);
    let mut started = HashSet::new();
    tokio::time::timeout(TEST_DEADLINE, async {
        while started.len() < expected_calls.len() {
            match handle.recv().await {
                Some(EngineEvent::ToolStarted { call_id, .. })
                    if expected_calls.contains(call_id.as_str()) =>
                {
                    started.insert(call_id);
                }
                Some(EngineEvent::TurnError { message, .. }) => panic!("turn failed: {message}"),
                Some(_) => {}
                None => panic!("engine stopped before tools started"),
            }
        }
    })
    .await
    .expect("parallel tools should start");

    handle.send(UiCommand::Steer {
        input: protocol::StartTurnInput::user(Content::text("queued follow-up")),
    });
    release_a.add_permits(1);
    release_b.add_permits(1);

    let mut lifecycle = Vec::new();
    let mut finished = HashSet::new();
    tokio::time::timeout(TEST_DEADLINE, async {
        while lifecycle.len() < 3 {
            match handle.recv().await {
                Some(EngineEvent::ToolFinished { call_id, .. })
                    if expected_calls.contains(call_id.as_str()) =>
                {
                    assert!(finished.insert(call_id), "tool finished more than once");
                    lifecycle.push("tool_finished");
                }
                Some(EngineEvent::Steered { text, count, .. }) => {
                    assert_eq!(text, "queued follow-up");
                    assert_eq!(count, 1);
                    lifecycle.push("steered");
                }
                Some(EngineEvent::TurnError { message, .. }) => panic!("turn failed: {message}"),
                Some(_) => {}
                None => panic!("engine stopped before queued message was acknowledged"),
            }
        }
    })
    .await
    .expect("tool completion and queued-message acknowledgement");

    assert_eq!(
        lifecycle,
        ["tool_finished", "tool_finished", "steered"],
        "queued input must remain queued until every parallel tool is visibly finished"
    );

    tokio::time::timeout(TEST_DEADLINE, async {
        loop {
            match handle.recv().await {
                Some(EngineEvent::TurnComplete { .. }) => break,
                Some(EngineEvent::TurnError { message, .. }) => panic!("turn failed: {message}"),
                Some(_) => {}
                None => panic!("engine stopped before turn completion"),
            }
        }
    })
    .await
    .expect("turn should complete");
    server.await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn model_switch_during_in_flight_request_applies_at_next_request_boundary() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (first_request_tx, first_request_rx) = tokio::sync::oneshot::channel();
    let (release_first_tx, release_first_rx) = tokio::sync::oneshot::channel();
    let (second_request_tx, second_request_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        first_request_tx
            .send(read_json_request(&mut first).await)
            .unwrap();
        release_first_rx.await.unwrap();
        write_sse_response(&mut first, &aux_text_sse()).await;

        let (mut second, _) = listener.accept().await.unwrap();
        second_request_tx
            .send(read_json_request(&mut second).await)
            .unwrap();
        write_sse_response(&mut second, &aux_text_sse()).await;
    });

    let config = EngineConfig {
        system_prompt_override: Some("test system".into()),
        host_callbacks: engine::HostCallbacks::Disabled,
        ..EngineConfig::new(PathBuf::from("/tmp"), Arc::new(engine::clock::RealClock))
    };
    let original_target = ModelTarget {
        model: "original-model".into(),
        api_base: format!("http://{addr}"),
        api_key: "original-key".into(),
        provider_type: "anthropic-compatible".into(),
        config: ModelConfig {
            max_tokens: Some(111),
            ..Default::default()
        },
    };
    let switched_target = ModelTarget {
        model: "next-model".into(),
        api_key: "next-key".into(),
        config: ModelConfig {
            max_tokens: Some(222),
            supports_reasoning: Some(true),
            ..Default::default()
        },
        ..original_target.clone()
    };
    let mut handle = engine::start(config, Box::new(engine::tools::EmptyDispatcher));
    while !matches!(handle.recv().await, Some(EngineEvent::Ready)) {}

    handle.send(UiCommand::StartTurn(Box::new(StartTurnPayload {
        turn_id: 1,
        input: protocol::StartTurnInput::user(Content::text("first")),
        mode: AgentMode::normal(),
        model_target: original_target,
        request_config: RequestRuntimeConfig {
            request_audit: protocol::RequestAuditMode::Off,
            ..Default::default()
        },
        reasoning_effort: ReasoningEffort::Off,
        fast_mode: false,
        history: protocol::ModelHistorySource::items(Vec::new()),
        session_id: "sess".into(),
        sessions_root: PathBuf::from("/tmp"),
        persistence: protocol::PersistenceScope::default(),
        permission_overrides: None,
        system_prompt: Some("test system".into()),
        tools: Vec::new(),
    })));

    let first_request = first_request_rx.await.unwrap();
    assert_eq!(first_request["model"], "original-model");
    assert_eq!(first_request["max_tokens"], 111);
    handle.send(UiCommand::SetTurnModel {
        target: Box::new(switched_target),
        system_prompt: "switched system with Lua fragment".into(),
    });
    handle.send(UiCommand::Steer {
        input: protocol::StartTurnInput::user(Content::text("use the new target")),
    });
    release_first_tx.send(()).unwrap();

    let second_request = second_request_rx.await.unwrap();
    assert_eq!(second_request["model"], "next-model");
    assert_eq!(second_request["max_tokens"], 222);
    assert_eq!(
        second_request["system"][0]["text"],
        "switched system with Lua fragment"
    );

    loop {
        match handle.recv().await {
            Some(EngineEvent::TurnComplete { .. }) => break,
            Some(EngineEvent::TurnError { message, .. }) => panic!("turn failed: {message}"),
            Some(_) => {}
            None => panic!("engine stopped before turn completion"),
        }
    }
    server.await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn updates_during_sequential_tool_wait_apply_after_tool_finishes() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let counter = Arc::new(AtomicUsize::new(0));
    let (request_tx, mut request_rx) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(run_server(listener, counter, request_tx));
    let config = EngineConfig {
        system_prompt_override: Some("test system".into()),
        host_callbacks: engine::HostCallbacks::Disabled,
        ..EngineConfig::new(PathBuf::from("/tmp"), Arc::new(engine::clock::RealClock))
    };
    let original_target = ModelTarget {
        model: "sequential-original".into(),
        api_base: format!("http://{addr}"),
        api_key: "key".into(),
        provider_type: "anthropic-compatible".into(),
        config: ModelConfig::default(),
    };
    let switched_target = ModelTarget {
        model: "sequential-next".into(),
        config: ModelConfig {
            max_tokens: Some(654),
            ..Default::default()
        },
        ..original_target.clone()
    };
    let mut handle = engine::start(config, Box::new(engine::tools::EmptyDispatcher));
    while !matches!(handle.recv().await, Some(EngineEvent::Ready)) {}
    handle.send(UiCommand::StartTurn(Box::new(StartTurnPayload {
        turn_id: 1,
        input: protocol::StartTurnInput::user(Content::text("go")),
        mode: AgentMode::normal(),
        model_target: original_target,
        request_config: RequestRuntimeConfig::default(),
        reasoning_effort: ReasoningEffort::Off,
        fast_mode: false,
        history: protocol::ModelHistorySource::items(Vec::new()),
        session_id: "sess".into(),
        sessions_root: PathBuf::from("/tmp"),
        persistence: protocol::PersistenceScope::default(),
        permission_overrides: Some(protocol::PermissionOverrides {
            tools: Some(protocol::RuleSetOverride {
                allow: vec![TOOL_NAME.into()],
                ask: Vec::new(),
                deny: Vec::new(),
            }),
            subcommands: Default::default(),
        }),
        system_prompt: Some("test system".into()),
        tools: vec![ToolDef {
            name: TOOL_NAME.into(),
            description: "sequential target switch".into(),
            parameters: serde_json::json!({"type":"object","properties":{}}),
            modes: None,
            execution_mode: ToolExecutionMode::Sequential,
            override_core: false,
            hooks: ToolHookFlags::default(),
            headless: true,
        }],
    })));

    let mut lifecycle = Vec::new();
    loop {
        match tokio::time::timeout(TEST_DEADLINE, handle.recv())
            .await
            .expect("sequential turn event")
        {
            Some(EngineEvent::ToolEvaluationRequest { request_id, .. }) => {
                handle.send(UiCommand::ToolEvaluationResponse {
                    request_id,
                    evaluation: protocol::ToolEvaluation {
                        decision: protocol::Decision::Allow,
                        metadata: ToolMetadata::default(),
                    },
                });
            }
            Some(EngineEvent::ToolDispatch {
                request_id,
                invocation_id,
                call_id,
                ..
            }) => {
                handle.send(UiCommand::SetTurnModel {
                    target: Box::new(switched_target.clone()),
                    system_prompt: "sequential switched system".into(),
                });
                handle.send(UiCommand::Steer {
                    input: protocol::StartTurnInput::user(Content::text("queued during tool")),
                });
                handle.send(UiCommand::ToolResult {
                    request_id,
                    invocation_id,
                    call_id,
                    content: "done".into(),
                    is_error: false,
                    metadata: None,
                    display_content: Vec::new(),
                    attachment: None,
                });
            }
            Some(EngineEvent::ToolFinished { call_id, .. }) if call_id == TOOL_CALL_ID => {
                lifecycle.push("tool_finished");
            }
            Some(EngineEvent::Steered { text, count, .. }) => {
                assert_eq!(text, "queued during tool");
                assert_eq!(count, 1);
                lifecycle.push("steered");
            }
            Some(EngineEvent::TurnComplete { .. }) => break,
            Some(EngineEvent::TurnError { message, .. }) => panic!("turn failed: {message}"),
            Some(_) => {}
            None => panic!("engine stopped before turn completion"),
        }
    }
    server.abort();

    assert_eq!(
        lifecycle,
        ["tool_finished", "steered"],
        "queued input must remain queued during a sequential tool wait"
    );
    let requests: Vec<_> = std::iter::from_fn(|| request_rx.try_recv().ok()).collect();
    let next_request = requests
        .iter()
        .find(|request| request["model"] == "sequential-next")
        .expect("follow-up request should use switched sequential target");
    assert_eq!(next_request["max_tokens"], 654);
    assert_eq!(
        next_request["system"][0]["text"],
        "sequential switched system"
    );
}
