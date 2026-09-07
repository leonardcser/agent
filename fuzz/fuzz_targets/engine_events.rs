#![no_main]

//! Focused engine-event lifecycle state machine. It generates complete reasoning,
//! draft-tool, tool, permission, ask, and canonical-history transitions while an
//! independent model checks active-turn identity and canonical suffix semantics.

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use protocol::{
    AgentMode, CanonicalHistoryDelta, Content, EngineAskError, EngineAskErrorKind, EngineEvent,
    HistoryItem, ReasoningKind, TokenUsage, ToolOutcome, UiCommand,
};
use smelt_fuzz::{runtime::with_current_thread_runtime, TestApp};
use std::collections::HashMap;
use tui::app::test_harness::{Action, SourceEvent};

const MAX_OPS: usize = 96;
const MAX_HISTORY_ITEMS: usize = 8;
const MAX_TEXT_CHARS: usize = 128;

#[derive(Arbitrary, Debug)]
struct Input {
    ops: Vec<Op>,
}

#[derive(Arbitrary, Debug)]
enum Op {
    StartTurn {
        id: u8,
    },
    Ready,
    TextRoundTrip {
        deltas: Vec<String>,
        final_text: String,
    },
    Steered {
        text: String,
        count: u8,
    },
    Retrying {
        delay_ms: u16,
        attempt: u8,
    },
    TokenUsage {
        prompt: u16,
        completion: u16,
        cost_cents: u16,
        background: bool,
    },
    ToolEvaluation {
        name: String,
    },
    ToolDispatch {
        name: String,
    },
    CoreToolResult {
        content: String,
        is_error: bool,
    },
    Shutdown {
        reason: Option<String>,
    },
    ReasoningRoundTrip {
        kind: u8,
        title: Option<String>,
        deltas: Vec<String>,
        final_text: String,
    },
    ToolDraftRoundTrip {
        name: String,
        deltas: Vec<String>,
        arguments: String,
    },
    ToolRoundTrip {
        name: String,
        output: String,
        is_error: bool,
    },
    ToolRejected {
        name: String,
        message: String,
        is_error: bool,
    },
    Permission {
        name: String,
        approve: bool,
    },
    AskRoundTrip {
        question: String,
        delta: String,
        response: String,
        fail: bool,
    },
    AppendHistory {
        items: Vec<HistoryEntry>,
    },
    ReplaceHistory {
        first: u8,
        items: Vec<HistoryEntry>,
    },
    Complete {
        first: u8,
        items: Option<Vec<HistoryEntry>>,
    },
    Error {
        message: String,
    },
    AuditError {
        message: String,
    },
    MismatchedHistory {
        id: u8,
        items: Vec<HistoryEntry>,
    },
    Render,
}

#[derive(Arbitrary, Debug)]
enum HistoryEntry {
    User(String),
    Assistant(String),
}

#[derive(Default)]
struct Model {
    active_turn: Option<u64>,
    history: Vec<HistoryItem>,
    sequence: u64,
}

fuzz_target!(|input: Input| {
    with_current_thread_runtime("engine_events", || run(input));
});

fn run(input: Input) {
    let mut app = TestApp::builder().build();
    app.run_lua_result(
        r#"
        smelt.tools.register({
          name = "fuzz_engine_dispatch",
          description = "engine lifecycle fuzz tool",
          parameters = { type = "object", properties = { value = { type = "string" } } },
          execute = function(args) return args.value or "" end,
        })
        "#,
    )
    .expect("register engine lifecycle fuzz tool");
    let mut model = Model::default();

    for op in input.ops.into_iter().take(MAX_OPS) {
        model.sequence = model.sequence.saturating_add(1);
        apply(&mut app, &mut model, op);
        app.render_silent();
        app.assert_invariants();
        assert_eq!(app.current_turn_id(), model.active_turn);
        assert_eq!(app.session_history(), model.history.as_slice());
    }
}

fn apply(app: &mut TestApp, model: &mut Model, op: Op) {
    match op {
        Op::StartTurn { id } => {
            if model.active_turn.is_none() {
                let id = u64::from(id);
                app.start_turn(id);
                model.active_turn = Some(id);
            }
        }
        Op::Ready => feed(app, EngineEvent::Ready),
        Op::TextRoundTrip { deltas, final_text } => {
            ensure_turn(app, model);
            for delta in deltas.into_iter().take(8) {
                feed(
                    app,
                    EngineEvent::TextDelta {
                        delta: small_text(delta),
                    },
                );
            }
            feed(
                app,
                EngineEvent::Text {
                    content: small_text(final_text),
                },
            );
            assert!(!app.streaming_state().text);
        }
        Op::Steered { text, count } => {
            ensure_turn(app, model);
            feed(
                app,
                EngineEvent::TextDelta {
                    delta: "partial".to_string(),
                },
            );
            feed(
                app,
                EngineEvent::Steered {
                    text: small_text(text),
                    count: usize::from(count),
                    sent_at_ms: 1_742_567_823_000,
                },
            );
            let streaming = app.streaming_state();
            assert!(!streaming.text && !streaming.thinking);
        }
        Op::Retrying { delay_ms, attempt } => {
            ensure_turn(app, model);
            feed(
                app,
                EngineEvent::TextDelta {
                    delta: "partial".to_string(),
                },
            );
            feed(
                app,
                EngineEvent::Retrying {
                    delay_ms: u64::from(delay_ms),
                    attempt: u32::from(attempt),
                },
            );
            let streaming = app.streaming_state();
            assert!(!streaming.text && !streaming.thinking);
            assert!(app.working_state().animating);
        }
        Op::TokenUsage {
            prompt,
            completion,
            cost_cents,
            background,
        } => {
            ensure_turn(app, model);
            let before_cost = app.session_cost_usd();
            let prompt = u32::from(prompt);
            let cost_usd = f64::from(cost_cents) / 100.0;
            feed(
                app,
                EngineEvent::TokenUsage {
                    usage: TokenUsage {
                        context_tokens: None,
                        prompt_tokens: Some(prompt),
                        completion_tokens: Some(u32::from(completion)),
                        cache_read_tokens: None,
                        cache_write_tokens: None,
                        reasoning_tokens: None,
                    },
                    tokens_per_sec: None,
                    cost_usd: Some(cost_usd),
                    background,
                },
            );
            assert!((app.session_cost_usd() - (before_cost + cost_usd)).abs() < 1e-6);
            if !background && prompt > 0 {
                assert_eq!(app.context_tokens(), Some(prompt));
            }
        }
        Op::ToolEvaluation { name } => {
            ensure_turn(app, model);
            let action_index = app.actions().len();
            feed(
                app,
                EngineEvent::ToolEvaluationRequest {
                    invocation_id: protocol::InvocationId::new(model.sequence),
                    request_id: model.sequence,
                    call_id: format!("evaluation-{}", model.sequence),
                    tool_name: small_text(name),
                    args: HashMap::new(),
                    mode: AgentMode::normal(),
                },
            );
            assert_eq!(
                count_commands_since(app, action_index, |command| matches!(
                    command,
                    UiCommand::ToolEvaluationResponse { .. }
                )),
                1
            );
        }
        Op::ToolDispatch { name } => {
            ensure_turn(app, model);
            let request_id = model.sequence;
            let call_id = format!("dispatch-{request_id}");
            let value = small_text(name);
            let mut args = HashMap::new();
            args.insert("value".to_string(), serde_json::json!(value));
            let action_index = app.actions().len();
            feed(
                app,
                EngineEvent::ToolDispatch {
                    invocation_id: protocol::InvocationId::new(request_id),
                    request_id,
                    call_id: call_id.clone(),
                    tool_name: "fuzz_engine_dispatch".to_string(),
                    args,
                },
            );
            let results: Vec<_> = app
                .actions_since(action_index)
                .iter()
                .filter_map(|action| match action {
                    Action::EngineSend(command) => match command.as_ref() {
                        UiCommand::ToolResult {
                            request_id,
                            call_id,
                            content,
                            is_error,
                            ..
                        } => Some((*request_id, call_id.as_str(), content.as_str(), *is_error)),
                        _ => None,
                    },
                    Action::Quit => None,
                })
                .collect();
            assert_eq!(
                results,
                vec![(request_id, call_id.as_str(), value.as_str(), false)]
            );
        }
        Op::CoreToolResult { content, is_error } => feed(
            app,
            EngineEvent::CoreToolResult {
                request_id: model.sequence,
                content: small_text(content),
                is_error,
                metadata: None,
            },
        ),
        Op::Shutdown { reason } => {
            ensure_turn(app, model);
            feed(
                app,
                EngineEvent::Shutdown {
                    reason: reason.map(small_text),
                },
            );
            model.active_turn = None;
        }
        Op::ReasoningRoundTrip {
            kind,
            title,
            deltas,
            final_text,
        } => {
            ensure_turn(app, model);
            let id = format!("reasoning-{}", model.sequence);
            let kind = reasoning_kind(kind);
            feed(
                app,
                EngineEvent::ReasoningPartStarted {
                    id: id.clone(),
                    kind,
                },
            );
            for delta in deltas.into_iter().take(8) {
                feed(
                    app,
                    EngineEvent::ReasoningPartDelta {
                        id: id.clone(),
                        kind,
                        delta: small_text(delta),
                        title: title.clone().map(small_text),
                    },
                );
            }
            feed(
                app,
                EngineEvent::ReasoningPartFinished {
                    id,
                    kind,
                    title: title.map(small_text),
                    content: small_text(final_text),
                },
            );
        }
        Op::ToolDraftRoundTrip {
            name,
            deltas,
            arguments,
        } => {
            ensure_turn(app, model);
            let stream_id = format!("stream-{}", model.sequence);
            let call_id = format!("draft-call-{}", model.sequence);
            let name = small_text(name);
            feed(
                app,
                EngineEvent::ToolCallDraftStarted {
                    stream_id: stream_id.clone(),
                    call_id: Some(call_id.clone()),
                    tool_name: Some(name.clone()),
                },
            );
            for delta in deltas.into_iter().take(8) {
                feed(
                    app,
                    EngineEvent::ToolCallDraftDelta {
                        stream_id: stream_id.clone(),
                        call_id: Some(call_id.clone()),
                        tool_name: Some(name.clone()),
                        delta: small_text(delta),
                    },
                );
            }
            feed(
                app,
                EngineEvent::ToolCallDraftFinished {
                    stream_id,
                    call_id,
                    tool_name: name,
                    arguments: small_text(arguments),
                },
            );
        }
        Op::ToolRoundTrip {
            name,
            output,
            is_error,
        } => {
            ensure_turn(app, model);
            let invocation_id = protocol::InvocationId::new(model.sequence);
            let call_id = format!("call-{}", model.sequence);
            feed(
                app,
                EngineEvent::ToolStarted {
                    invocation_id,
                    call_id: call_id.clone(),
                    tool_name: small_text(name),
                    args: HashMap::new(),
                    called_at_ms: model.sequence,
                },
            );
            assert!(app.pending_tool_invocation_ids().contains(&invocation_id));
            feed(
                app,
                EngineEvent::ToolOutput {
                    invocation_id,
                    call_id: call_id.clone(),
                    line: small_text(output.clone()),
                },
            );
            feed(
                app,
                EngineEvent::ToolFinished {
                    invocation_id,
                    call_id: call_id.clone(),
                    result: ToolOutcome::new(small_text(output), is_error, None),
                    elapsed_ms: Some(model.sequence),
                },
            );
            assert!(!app.pending_tool_invocation_ids().contains(&invocation_id));
        }
        Op::ToolRejected {
            name,
            message,
            is_error,
        } => {
            ensure_turn(app, model);
            let name = small_text(name);
            let message = small_text(message);
            feed(
                app,
                EngineEvent::ToolRejected {
                    invocation_id: protocol::InvocationId::new(model.sequence),
                    call_id: format!("rejected-{}", model.sequence),
                    tool_name: name,
                    args: HashMap::new(),
                    summary: protocol::style::StyledLines::from_plain(message.clone()),
                    result: ToolOutcome::new(message, is_error, None),
                    elapsed_ms: Some(model.sequence),
                    called_at_ms: model.sequence,
                },
            );
        }
        Op::Permission { name, approve } => {
            ensure_turn(app, model);
            let request_id = model.sequence;
            let action_index = app.actions().len();
            feed(
                app,
                EngineEvent::RequestPermission {
                    invocation_id: protocol::InvocationId::new(request_id),
                    request_id,
                    call_id: format!("permission-{request_id}"),
                    tool_name: small_text(name),
                    args: HashMap::new(),
                    approval_patterns: Vec::new(),
                    summary: protocol::style::StyledLines::from_plain("permission".to_string()),
                    called_at_ms: model.sequence,
                },
            );
            assert_eq!(app.pending_confirm_count(), 1);
            assert!(app.resolve_first_confirm(approve, None));
            assert_eq!(app.pending_confirm_count(), 0);
            let decisions: Vec<_> = app
                .actions_since(action_index)
                .iter()
                .filter_map(|action| match action {
                    Action::EngineSend(command) => match command.as_ref() {
                        UiCommand::PermissionDecision {
                            request_id,
                            approved,
                            ..
                        } => Some((*request_id, *approved)),
                        _ => None,
                    },
                    Action::Quit => None,
                })
                .collect();
            assert_eq!(decisions, vec![(request_id, approve)]);
        }
        Op::AskRoundTrip {
            question,
            delta,
            response,
            fail,
        } => ask_round_trip(app, question, delta, response, fail),
        Op::AppendHistory { items } => {
            ensure_turn(app, model);
            let items = history_items(items);
            let first = model.history.len();
            feed(
                app,
                EngineEvent::HistoryAppended {
                    turn_id: model.active_turn.unwrap(),
                    delta: CanonicalHistoryDelta::new(first, items.clone()),
                },
            );
            model.history.extend(items);
        }
        Op::ReplaceHistory { first, items } => {
            ensure_turn(app, model);
            let first = usize::from(first) % (model.history.len() + 1);
            let items = history_items(items);
            feed(
                app,
                EngineEvent::HistoryUpdated {
                    turn_id: model.active_turn.unwrap(),
                    update: CanonicalHistoryDelta::new(first, items.clone()),
                },
            );
            model.history.truncate(first);
            model.history.extend(items);
        }
        Op::Complete { first, items } => {
            ensure_turn(app, model);
            let history = items.map(|items| {
                let first = usize::from(first) % (model.history.len() + 1);
                let items = history_items(items);
                model.history.truncate(first);
                model.history.extend(items.clone());
                CanonicalHistoryDelta::new(first, items)
            });
            feed(
                app,
                EngineEvent::TurnComplete {
                    turn_id: model.active_turn.unwrap(),
                    history,
                    meta: None,
                },
            );
            model.active_turn = None;
        }
        Op::Error { message } => {
            ensure_turn(app, model);
            feed(
                app,
                EngineEvent::TurnError {
                    message: small_text(message),
                    kind: None,
                    retry_at_ms: None,
                },
            );
            model.active_turn = None;
        }
        Op::AuditError { message } => feed(
            app,
            EngineEvent::RequestAuditError {
                message: small_text(message),
            },
        ),
        Op::MismatchedHistory { id, items } => {
            ensure_turn(app, model);
            let mut wrong = u64::from(id);
            if Some(wrong) == model.active_turn {
                wrong = wrong.wrapping_add(1);
            }
            feed(
                app,
                EngineEvent::HistoryUpdated {
                    turn_id: wrong,
                    update: CanonicalHistoryDelta::new(0, history_items(items)),
                },
            );
        }
        Op::Render => {}
    }
}

fn ensure_turn(app: &mut TestApp, model: &mut Model) {
    if model.active_turn.is_none() {
        let id = model.sequence;
        app.start_turn(id);
        model.active_turn = Some(id);
    }
}

fn ask_round_trip(
    app: &mut TestApp,
    question: String,
    delta: String,
    response: String,
    fail: bool,
) {
    app.start_engine_ask_probe(&small_text(question));
    let id = app
        .drain_engine_sends()
        .into_iter()
        .find_map(|command| match command {
            UiCommand::EngineAsk { id, .. } => Some(id),
            _ => None,
        })
        .expect("engine ask probe did not send a request");
    feed(
        app,
        EngineEvent::EngineAskDelta {
            id,
            delta: small_text(delta),
        },
    );
    let (message, error) = if fail {
        (
            None,
            Some(EngineAskError {
                kind: EngineAskErrorKind::Other,
                message: small_text(response),
            }),
        )
    } else {
        (
            Some(protocol::Message::assistant(
                Some(Content::text(small_text(response))),
                None,
                None,
            )),
            None,
        )
    };
    feed(app, EngineEvent::EngineAskResponse { id, message, error });
    assert!(
        app.pending_ask_id().is_none(),
        "engine ask callback survived its terminal response"
    );
}

fn count_commands_since(
    app: &TestApp,
    action_index: usize,
    predicate: impl Fn(&UiCommand) -> bool,
) -> usize {
    app.actions_since(action_index)
        .iter()
        .filter(
            |action| matches!(action, Action::EngineSend(command) if predicate(command.as_ref())),
        )
        .count()
}

fn feed(app: &mut TestApp, event: EngineEvent) {
    app.feed_one(SourceEvent::engine(event));
}

fn reasoning_kind(value: u8) -> ReasoningKind {
    if value & 1 == 0 {
        ReasoningKind::Raw
    } else {
        ReasoningKind::Summary
    }
}

fn history_items(items: Vec<HistoryEntry>) -> Vec<HistoryItem> {
    items
        .into_iter()
        .take(MAX_HISTORY_ITEMS)
        .map(|item| match item {
            HistoryEntry::User(text) => HistoryItem::user(Content::text(small_text(text))),
            HistoryEntry::Assistant(text) => {
                HistoryItem::Assistant(protocol::AssistantStep::terminal(
                    Some(Content::text(small_text(text))),
                    None,
                    Vec::new(),
                ))
            }
        })
        .collect()
}

fn small_text(value: String) -> String {
    value
        .chars()
        .filter(|ch| !ch.is_control() || *ch == '\n' || *ch == '\t')
        .take(MAX_TEXT_CHARS)
        .collect()
}
