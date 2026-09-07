use super::*;
use crate::app::search::SearchDirection;
use crate::app::transcript_scroll_trace::{
    TranscriptProjectionTargetTrace, TranscriptRecordTraceRange, TranscriptScrollIntent,
    TranscriptScrollTraceFrame, TranscriptTraceAnchor,
};
use crate::app::TuiApp;
use crate::smelt_edit::RowIndex;

#[test]
fn notification_highlight_uses_terminal_width_for_unicode() {
    let summary = "besta\u{308}tigt 日本 👩\u{200d}💻";
    let (line, _, _, msg_start, msg_end) =
        TuiApp::notification_parts(smelt_core::messages::MessageKind::Info, summary, 80);

    assert!(line.trim_end().ends_with(summary));
    assert_eq!(
        msg_end - msg_start,
        smelt_buffer::cell_width::text_width_u16(summary)
    );
}

#[test]
fn starting_a_new_turn_keeps_long_tool_names_materialized_at_tail() {
    let mut app = TestApp::builder().build();
    app.start_turn(1);
    app.feed_one(SourceEvent::engine(EngineEvent::ToolStarted {
        invocation_id: protocol::InvocationId::new(1),
        call_id: "call-1".into(),
        tool_name: format!("\0\0%\0\0\0c{}", "\u{1c}".repeat(68)),
        args: std::collections::HashMap::new(),
        called_at_ms: 1,
    }));
    app.render_to_frame();

    app.start_turn(255);

    app.render_to_frame();
    app.assert_invariants();
}

#[test]
fn lua_paint_callback_uses_scoped_tui_host_after_ui_paint() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(40, 12);
    assert!(app.run_lua(
        r#"
        _G.__paint_host_seen = false
        _G.__paint = smelt.paint.register(function(slice)
            _G.__paint_host_seen = smelt.session.id() ~= ""
            _G.__paint_size = { slice:width(), slice:height() }
            _G.__paint_set_ok, _G.__paint_set_err = pcall(function()
                slice:set(0, 0, "X", { bold = true })
            end)
        end)
        _G.__paint_overlay = smelt.overlay.new({
            anchor = "screen_at",
            corner = "nw",
            row = 0,
            col = 0,
            width = 5,
            height = 3,
            layout = smelt.ui.layout.leaf(_G.__paint),
        })
        "#,
    ));

    let frame = app.render_to_frame();

    assert!(app.eval_lua::<bool>("return _G.__paint_host_seen").unwrap());
    let (set_ok, set_error) = app
        .eval_lua::<(bool, Option<String>)>("return _G.__paint_set_ok, _G.__paint_set_err")
        .unwrap();
    assert!(set_ok, "paint write failed: {set_error:?}");
    let (paint_width, paint_height) = app
        .eval_lua::<(u16, u16)>("return _G.__paint_size[1], _G.__paint_size[2]")
        .unwrap();
    assert!(
        frame.text().contains('X'),
        "paint size: {paint_width}x{paint_height}; frame: {}",
        frame.text()
    );
}

#[test]
fn statusline_explicit_invalidation_repaints_plugin_state() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(40, 12);
    assert!(app.run_lua(
        r#"
        local statusline = require("smelt.statusline")
        _G.__plugin_status_text = "retained status before"
        statusline.win:set_renderer(function(win)
          win:buf():lines({ _G.__plugin_status_text })
        end)
        "#,
    ));

    let initial = app.render_to_frame();
    assert!(initial.rows[11].contains("retained status before"));

    assert!(app.run_lua(r#"_G.__plugin_status_text = "retained status after""#));
    let retained = app.render_to_frame();
    assert!(retained.rows[11].contains("retained status before"));
    assert!(!retained.rows[11].contains("retained status after"));

    assert!(app.run_lua(r#"require("smelt.statusline").invalidate()"#));
    let invalidated = app.render_to_frame();
    assert!(invalidated.rows[11].contains("retained status after"));
}

#[test]
fn headerline_explicit_invalidation_recomposes_plugin_visibility() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(40, 12);
    assert!(app.run_lua(
        r#"
        local headerline = require("smelt.headerline")
        _G.__plugin_header_visible = false
        headerline.add("retained-visibility-probe", {
          visible = function() return _G.__plugin_header_visible end,
          render = function()
            return { text = "retained header visible", highlights = {} }
          end,
        })
        "#,
    ));

    let initial = app.render_to_frame();
    assert!(!initial.text().contains("retained header visible"));

    assert!(app.run_lua(r#"_G.__plugin_header_visible = true"#));
    let retained = app.render_to_frame();
    assert!(!retained.text().contains("retained header visible"));

    assert!(app.run_lua(r#"require("smelt.headerline").invalidate()"#));
    let invalidated = app.render_to_frame();
    assert!(invalidated.rows[0].contains("retained header visible"));
}

#[test]
fn fresh_session_render_does_not_probe_uncreated_transcript_store() {
    let mut app = TestApp::builder().build();
    let session_dir = app.app.conversation.current_artifact_dir();
    assert!(
        !session_dir.exists(),
        "fresh session should not be persisted before its first message"
    );

    for _ in 0..3 {
        app.render_silent();
    }

    let transcript = app.app.conversation.transcript();
    assert_eq!(
        transcript.projection_count_for_harness(),
        1,
        "unchanged frames must reuse the retained transcript projection"
    );
    assert_eq!(
        transcript.store_open_attempt_count_for_harness(),
        0,
        "rendering a fresh session must not probe a transcript store that cannot exist yet"
    );
}

#[test]
fn streaming_search_tool_summaries_keep_patterns_while_absolute_paths_collapse() {
    for (tool_name, pattern) in [
        (
            "grep",
            "FunctionTarget::Advanced.*true|Advanced,.*true|advanced_live_only",
        ),
        ("glob", "crates/**/*.rs"),
    ] {
        let mut app = TestApp::builder().build();
        app.set_terminal_size(100, 12);
        app.start_turn(1);
        let stream_id = format!("{tool_name}-stream");
        let call_id = format!("{tool_name}-call");

        app.feed_one(SourceEvent::engine(EngineEvent::ToolCallDraftStarted {
            stream_id: stream_id.clone(),
            call_id: Some(call_id.clone()),
            tool_name: Some(tool_name.into()),
        }));
        app.feed_one(SourceEvent::engine(EngineEvent::ToolCallDraftDelta {
            stream_id: stream_id.clone(),
            call_id: Some(call_id.clone()),
            tool_name: Some(tool_name.into()),
            delta: format!(r#"{{"pattern":{}"#, serde_json::to_string(pattern).unwrap()),
        }));
        let pattern_frame = app.render_to_frame().text();
        let expected_pattern = format!("* {tool_name} {pattern}");
        assert!(
            pattern_frame.contains(&expected_pattern),
            "pattern frame regressed for {tool_name}: {pattern_frame}"
        );

        let cwd = app.cwd_str().to_string();
        let serialized_cwd = serde_json::to_string(&cwd).unwrap();
        let partial_cwd = serialized_cwd
            .strip_suffix('"')
            .expect("serialized path ends with a quote");
        app.feed_one(SourceEvent::engine(EngineEvent::ToolCallDraftDelta {
            stream_id: stream_id.clone(),
            call_id: Some(call_id.clone()),
            tool_name: Some(tool_name.into()),
            delta: format!(r#","path":{}"#, partial_cwd),
        }));
        let hidden_path_frame = app.render_to_frame().text();
        assert!(
            hidden_path_frame.contains(&expected_pattern),
            "absolute workspace prefix blanked {tool_name}'s stable pattern: {hidden_path_frame}"
        );
        assert!(
            !hidden_path_frame.contains(&format!("{pattern} in ")),
            "hidden path left a dangling separator for {tool_name}: {hidden_path_frame}"
        );

        app.feed_one(SourceEvent::engine(EngineEvent::ToolCallDraftDelta {
            stream_id,
            call_id: Some(call_id),
            tool_name: Some(tool_name.into()),
            delta: "/crates/core/src/content".into(),
        }));
        app.feed_one(SourceEvent::Tick(50));
        let relative_path_frame = app.render_to_frame().text();
        assert!(
            relative_path_frame.contains(&format!(
                "* {tool_name} {pattern} in crates/core/src/content"
            )),
            "workspace-relative path did not replace the fallback for {tool_name}: {relative_path_frame}"
        );
    }
}

#[test]
fn empty_engine_output_is_a_transcript_noop() {
    let mut app = TestApp::builder().build();
    app.start_turn(42);
    let before = app.transcript_block_count();

    for event in [
        EngineEvent::Text {
            content: String::new(),
        },
        EngineEvent::Text {
            content: "  \n\t".into(),
        },
        EngineEvent::Reasoning {
            kind: protocol::ReasoningKind::Raw,
            title: None,
            content: "  \n\t".into(),
        },
        EngineEvent::ReasoningPartFinished {
            id: "summary".into(),
            kind: protocol::ReasoningKind::Summary,
            title: Some("  ".into()),
            content: "\n\t".into(),
        },
    ] {
        app.feed_one(SourceEvent::engine(event));
    }

    assert_eq!(app.transcript_block_count(), before);
    assert!(app.agent_running());
}

#[test]
fn main_transcript_paints_a_selected_delta_before_a_coalesced_turn_completion() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(70, 22);
    app.start_turn(42);
    app.render_to_frame();

    app.inject_engine(EngineEvent::TextDelta {
        delta: "coalesced live transcript marker".into(),
    })
    .expect("queue transcript delta");
    app.inject_engine(EngineEvent::TurnComplete {
        turn_id: 42,
        history: None,
        meta: None,
    })
    .expect("queue turn completion");

    let selected_delta = app
        .try_receive_engine_output()
        .expect("select loop should receive the first delta");
    app.dispatch_engine_output_in_render_loop_to(selected_delta, &mut std::io::sink(), |_| {});
    let mut streamed_frame = None;
    app.drain_ready_engine_outputs_for_frame_to(&mut std::io::sink(), |frame| {
        streamed_frame = Some(frame.text())
    });

    let streamed_frame = streamed_frame.expect("turn completion should paint the pending delta");
    assert!(
        streamed_frame.contains("coalesced live transcript marker"),
        "no frame painted the selected delta: {streamed_frame}"
    );

    app.render_to_frame();
    assert!(!app.agent_running(), "turn completion was not dispatched");
}

#[test]
fn main_transcript_streams_before_a_busy_engine_queue_completes_the_turn() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(70, 22);
    app.start_turn(42);
    app.render_to_frame();

    let queued_deltas = crate::app::READY_QUEUE_DRAIN_MAX_ITEMS_PER_FRAME * 2;
    for index in 0..queued_deltas {
        app.inject_engine(EngineEvent::TextDelta {
            delta: format!("live transcript marker {index}\n"),
        })
        .expect("queue transcript delta");
    }
    app.inject_engine(EngineEvent::TurnComplete {
        turn_id: 42,
        history: None,
        meta: None,
    })
    .expect("queue turn completion");

    app.drain_ready_engine_outputs_for_frame_to(&mut std::io::sink(), |_| {});
    let frame = app.render_to_frame().text();
    assert!(frame.contains("live transcript marker"), "frame: {frame}");
    assert!(
        app.agent_running(),
        "turn completed before a streaming frame"
    );
    assert!(
        app.streaming_state().text,
        "streaming text was flushed early"
    );

    for _ in 0..=queued_deltas {
        if !app.agent_running() {
            break;
        }
        app.drain_ready_engine_outputs_for_frame_to(&mut std::io::sink(), |_| {});
    }
    assert!(!app.agent_running(), "turn completion was not drained");
    assert!(
        !app.streaming_state().text,
        "streaming text was not finalized"
    );
}

#[test]
fn tool_output_paints_before_a_coalesced_tool_completion() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(70, 22);
    app.start_turn(42);
    app.render_to_frame();

    let invocation_id = protocol::InvocationId::new(1);
    let call_id = "streaming-tool".to_string();
    app.inject_engine(EngineEvent::ToolStarted {
        invocation_id,
        call_id: call_id.clone(),
        tool_name: "bash".into(),
        args: std::collections::HashMap::from([("command".into(), serde_json::json!("sleep 1"))]),
        called_at_ms: 0,
    })
    .expect("queue tool start");
    app.inject_engine(EngineEvent::ToolOutput {
        invocation_id,
        call_id: call_id.clone(),
        line: "coalesced live tool marker".into(),
    })
    .expect("queue tool output");
    app.inject_engine(EngineEvent::ToolFinished {
        invocation_id,
        call_id,
        result: protocol::ToolOutcome::new("done".into(), false, None),
        elapsed_ms: Some(10),
    })
    .expect("queue tool completion");

    let selected_start = app
        .try_receive_engine_output()
        .expect("select loop should receive the tool start");
    app.dispatch_engine_output_in_render_loop_to(selected_start, &mut std::io::sink(), |_| {});
    let mut streamed_frame = None;
    app.drain_ready_engine_outputs_for_frame_to(&mut std::io::sink(), |frame| {
        streamed_frame = Some(frame.text())
    });

    let streamed_frame = streamed_frame.expect("tool completion should paint pending output");
    assert!(
        streamed_frame.contains("coalesced live tool marker"),
        "no frame painted the tool output: {streamed_frame}"
    );
}

#[test]
fn provider_tool_completion_reuses_streamed_content_identity() {
    let mut app = TestApp::builder().build();
    app.start_turn(42);
    let invocation_id = protocol::InvocationId::new(1);
    let call_id = "streamed-identity".to_string();

    app.feed_one(SourceEvent::engine(EngineEvent::ToolStarted {
        invocation_id,
        call_id: call_id.clone(),
        tool_name: "bash".into(),
        args: std::collections::HashMap::new(),
        called_at_ms: 0,
    }));
    for line in ["first streamed line", "second streamed line"] {
        app.feed_one(SourceEvent::engine(EngineEvent::ToolOutput {
            invocation_id,
            call_id: call_id.clone(),
            line: line.into(),
        }));
    }
    app.render_silent();

    let block_id = app
        .conversation_probe()
        .transcript()
        .history()
        .last_block_id()
        .expect("tool block");
    let streamed_id = app
        .conversation_probe()
        .transcript()
        .history()
        .tool_state(block_id)
        .and_then(|state| state.output.as_ref())
        .map(|output| output.content.id())
        .expect("streamed output");

    app.feed_one(SourceEvent::engine(EngineEvent::ToolFinished {
        invocation_id,
        call_id,
        result: protocol::ToolOutcome::new(
            "provider final payload must not replace streamed content".into(),
            false,
            Some(serde_json::json!({ "exit_code": 0 })),
        )
        .with_display_content(vec![protocol::ToolDisplayContent::new(
            "summary",
            "display summary".into(),
        )]),
        elapsed_ms: Some(10),
    }));

    let output = app
        .conversation_probe()
        .transcript()
        .history()
        .tool_state(block_id)
        .and_then(|state| state.output.as_ref())
        .expect("finished output");
    assert_eq!(output.content.id(), streamed_id);
    assert_eq!(
        output.content.snapshot(),
        "first streamed line\nsecond streamed line"
    );
    assert_eq!(output.metadata, Some(serde_json::json!({ "exit_code": 0 })));
    assert_eq!(
        output
            .content_field("summary")
            .map(|content| content.snapshot()),
        Some("display summary".to_string())
    );
}

#[test]
fn oversized_provider_output_is_sliced_before_completion_and_turn_finalization() {
    const SLICE_BYTES: usize = 4 * 1024 * 1024;

    let mut app = TestApp::builder().build();
    app.start_turn(42);
    let invocation_id = protocol::InvocationId::new(1);
    let call_id = "bounded-provider-output".to_string();
    app.feed_one(SourceEvent::engine(EngineEvent::ToolStarted {
        invocation_id,
        call_id: call_id.clone(),
        tool_name: "bash".into(),
        args: std::collections::HashMap::new(),
        called_at_ms: 0,
    }));

    app.feed_one(SourceEvent::engine(EngineEvent::ToolOutput {
        invocation_id,
        call_id: call_id.clone(),
        line: "x".repeat(3 * SLICE_BYTES),
    }));
    app.render_silent();
    let block_id = app
        .conversation_probe()
        .transcript()
        .history()
        .last_block_id()
        .expect("tool block");
    let streamed_id = app
        .conversation_probe()
        .transcript()
        .history()
        .tool_state(block_id)
        .and_then(|state| state.output.as_ref())
        .map(|output| output.content.id())
        .expect("streamed output");
    assert_eq!(
        app.conversation_probe()
            .transcript()
            .history()
            .tool_state(block_id)
            .and_then(|state| state.output.as_ref())
            .map(|output| output.content.len()),
        Some(SLICE_BYTES),
        "provider dispatch must ingest only the first bounded slice"
    );

    app.feed_one(SourceEvent::engine(EngineEvent::ToolFinished {
        invocation_id,
        call_id,
        result: protocol::ToolOutcome::new(
            "provider final payload must not replace streamed content".into(),
            false,
            Some(serde_json::json!({ "exit_code": 0 })),
        ),
        elapsed_ms: Some(10),
    }));
    assert_eq!(
        app.conversation_probe()
            .transcript()
            .history()
            .tool_state(block_id)
            .map(|state| state.status),
        Some(smelt_core::transcript_model::ToolStatus::Pending),
        "tool completion must wait for queued output slices"
    );

    app.feed_one(SourceEvent::engine(EngineEvent::TurnComplete {
        turn_id: 42,
        history: None,
        meta: None,
    }));
    assert!(
        app.app.conversation.is_active(),
        "turn completion must wait for queued output slices"
    );
    assert_eq!(
        app.conversation_probe()
            .transcript()
            .history()
            .tool_state(block_id)
            .and_then(|state| state.output.as_ref())
            .map(|output| output.content.len()),
        Some(2 * SLICE_BYTES),
        "the pre-event transient frame must ingest at most one slice"
    );

    app.render_silent();
    let state = app
        .conversation_probe()
        .transcript()
        .history()
        .tool_state(block_id)
        .expect("streaming tool state");
    assert_eq!(
        state.status,
        smelt_core::transcript_model::ToolStatus::Pending
    );
    assert_eq!(
        state.output.as_ref().map(|output| output.content.len()),
        Some(3 * SLICE_BYTES)
    );

    app.render_silent();
    let state = app
        .conversation_probe()
        .transcript()
        .history()
        .tool_state(block_id)
        .expect("completed tool state");
    let output = state.output.as_ref().expect("completed output");
    assert_eq!(state.status, smelt_core::transcript_model::ToolStatus::Ok);
    assert_eq!(output.content.id(), streamed_id);
    assert_eq!(output.content.snapshot(), "x".repeat(3 * SLICE_BYTES));
    assert_eq!(output.metadata, Some(serde_json::json!({ "exit_code": 0 })));
    assert!(
        app.app.conversation.is_active(),
        "turn finalization must remain queued behind tool completion"
    );

    app.render_silent();
    assert!(
        !app.app.conversation.is_active(),
        "turn completes after retained output and tool lifecycle settle"
    );
}

#[test]
fn oversized_output_does_not_block_an_independent_tool() {
    const SLICE_BYTES: usize = 4 * 1024 * 1024;

    let mut app = TestApp::builder().build();
    app.start_turn(42);
    let first_invocation = protocol::InvocationId::new(1);
    let second_invocation = protocol::InvocationId::new(2);
    assert!(app.dispatch_engine_event(EngineEvent::ToolStarted {
        invocation_id: first_invocation,
        call_id: "large-tool".into(),
        tool_name: "bash".into(),
        args: std::collections::HashMap::new(),
        called_at_ms: 0,
    }));
    let first_block = app
        .conversation_probe()
        .transcript()
        .history()
        .last_block_id()
        .expect("large tool block");
    assert!(app.dispatch_engine_event(EngineEvent::ToolStarted {
        invocation_id: second_invocation,
        call_id: "small-tool".into(),
        tool_name: "bash".into(),
        args: std::collections::HashMap::new(),
        called_at_ms: 0,
    }));
    let second_block = app
        .conversation_probe()
        .transcript()
        .history()
        .last_block_id()
        .expect("small tool block");

    assert!(app.dispatch_engine_event(EngineEvent::ToolOutput {
        invocation_id: first_invocation,
        call_id: "large-tool".into(),
        line: "a".repeat(2 * SLICE_BYTES),
    }));
    assert!(app.dispatch_engine_event(EngineEvent::ToolOutput {
        invocation_id: second_invocation,
        call_id: "small-tool".into(),
        line: "small output".into(),
    }));
    assert!(app.dispatch_engine_event(EngineEvent::ToolFinished {
        invocation_id: second_invocation,
        call_id: "small-tool".into(),
        result: protocol::ToolOutcome::new("ignored final output".into(), false, None),
        elapsed_ms: Some(1),
    }));

    let history = app.conversation_probe().transcript().history();
    assert_eq!(
        history.tool_state(second_block).map(|state| state.status),
        Some(smelt_core::transcript_model::ToolStatus::Ok)
    );
    assert_eq!(
        history
            .tool_state(second_block)
            .and_then(|state| state.output.as_ref())
            .map(|output| output.content.snapshot()),
        Some("small output".to_string())
    );
    assert_eq!(
        history
            .tool_state(first_block)
            .and_then(|state| state.output.as_ref())
            .map(|output| output.content.len()),
        Some(SLICE_BYTES),
        "independent tool events must not synchronously drain the large output"
    );

    assert!(app.dispatch_engine_event(EngineEvent::ToolFinished {
        invocation_id: first_invocation,
        call_id: "large-tool".into(),
        result: protocol::ToolOutcome::new("ignored final output".into(), false, None),
        elapsed_ms: Some(2),
    }));
    app.render_silent();
    assert_eq!(
        app.conversation_probe()
            .transcript()
            .history()
            .tool_state(first_block)
            .and_then(|state| state.output.as_ref())
            .map(|output| output.content.len()),
        Some(SLICE_BYTES),
        "the first render boundary only schedules queued indexing"
    );
    app.render_silent();
    app.render_silent();
    assert_eq!(
        app.conversation_probe()
            .transcript()
            .history()
            .tool_state(first_block)
            .map(|state| state.status),
        Some(smelt_core::transcript_model::ToolStatus::Ok)
    );
}

#[test]
fn cancellation_discards_queued_provider_output_and_lifecycle_events() {
    const SLICE_BYTES: usize = 4 * 1024 * 1024;

    let mut app = TestApp::builder().build();
    app.start_turn(42);
    let invocation_id = protocol::InvocationId::new(1);
    assert!(app.dispatch_engine_event(EngineEvent::ToolStarted {
        invocation_id,
        call_id: "cancelled-large-tool".into(),
        tool_name: "bash".into(),
        args: std::collections::HashMap::new(),
        called_at_ms: 0,
    }));
    let block_id = app
        .conversation_probe()
        .transcript()
        .history()
        .last_block_id()
        .expect("tool block");
    assert!(app.dispatch_engine_event(EngineEvent::ToolOutput {
        invocation_id,
        call_id: "cancelled-large-tool".into(),
        line: "x".repeat(3 * SLICE_BYTES),
    }));
    assert!(app.dispatch_engine_event(EngineEvent::ToolFinished {
        invocation_id,
        call_id: "cancelled-large-tool".into(),
        result: protocol::ToolOutcome::new("ignored final output".into(), false, None),
        elapsed_ms: Some(1),
    }));
    assert!(app.dispatch_engine_event(EngineEvent::TurnComplete {
        turn_id: 42,
        history: None,
        meta: None,
    }));

    app.cancel();
    for _ in 0..4 {
        app.render_silent();
    }

    let state = app
        .conversation_probe()
        .transcript()
        .history()
        .tool_state(block_id)
        .expect("cancelled tool state");
    assert_eq!(
        state.output.as_ref().map(|output| output.content.len()),
        Some(SLICE_BYTES),
        "queued slices must not append after cancellation"
    );
    assert_ne!(
        state.status,
        smelt_core::transcript_model::ToolStatus::Ok,
        "deferred completion must not apply after cancellation"
    );
    assert!(!app.agent_running());
}

#[test]
fn streaming_tool_output_preserves_decomposed_unicode_across_frames() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(80, 22);
    app.start_turn(42);
    let invocation_id = protocol::InvocationId::new(1);
    let call_id = "unicode-stream".to_string();
    app.feed_one(SourceEvent::engine(EngineEvent::ToolStarted {
        invocation_id,
        call_id: call_id.clone(),
        tool_name: "bash".into(),
        args: std::collections::HashMap::from([(
            "command".into(),
            serde_json::json!("unicode output"),
        )]),
        called_at_ms: 0,
    }));

    app.feed_one(SourceEvent::engine(EngineEvent::ToolOutput {
        invocation_id,
        call_id: call_id.clone(),
        line: "[1/2] Bestellung 10500 besta".into(),
    }));
    app.render_silent();
    app.feed_one(SourceEvent::engine(EngineEvent::ToolOutput {
        invocation_id,
        call_id: call_id.clone(),
        line: "\u{308}tigt.pdf\n[2/2] Commande 10551 confirme".into(),
    }));
    app.render_silent();
    app.feed_one(SourceEvent::engine(EngineEvent::ToolOutput {
        invocation_id,
        call_id,
        line: "\u{301}e.pdf\n".into(),
    }));

    let frame = app.render_to_frame().text();
    assert!(
        frame.contains("[1/2] Bestellung 10500 besta\u{308}tigt.pdf"),
        "frame: {frame}"
    );
    assert!(
        frame.contains("[2/2] Commande 10551 confirme\u{301}e.pdf"),
        "frame: {frame}"
    );
}

#[test]
fn repeated_provider_call_ids_keep_distinct_live_tool_blocks() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(80, 24);
    app.start_turn(42);

    let invocations = [
        (
            protocol::InvocationId::new(1),
            "printf first",
            1_700_000_000_111,
            "first output",
        ),
        (
            protocol::InvocationId::new(2),
            "printf second",
            1_700_000_000_222,
            "second output",
        ),
    ];
    let mut block_ids = Vec::new();

    for (invocation_id, command, called_at_ms, _) in invocations {
        app.feed_one(SourceEvent::engine(EngineEvent::ToolStarted {
            invocation_id,
            call_id: "duplicate".into(),
            tool_name: "bash".into(),
            args: std::collections::HashMap::from([("command".into(), serde_json::json!(command))]),
            called_at_ms,
        }));
        block_ids.push(
            app.conversation_probe()
                .transcript()
                .history()
                .last_block_id()
                .expect("tool block"),
        );
    }

    assert_ne!(
        block_ids[0], block_ids[1],
        "each invocation needs its own block"
    );

    for ((invocation_id, _, _, output), block_id) in invocations.into_iter().zip(&block_ids) {
        app.feed_one(SourceEvent::engine(EngineEvent::ToolFinished {
            invocation_id,
            call_id: "duplicate".into(),
            result: protocol::ToolOutcome::new(output.into(), false, None),
            elapsed_ms: Some(10),
        }));
        assert_eq!(
            app.conversation_probe()
                .transcript()
                .history()
                .tool_state(*block_id)
                .and_then(|state| state.output.as_ref())
                .map(|output| output.content.snapshot()),
            Some(output.to_string())
        );
    }

    let records = app
        .conversation_probe()
        .transcript()
        .history()
        .block_records_with_ids();
    let mut restored =
        smelt_core::transcript_model::BlockHistory::from_block_records_with_ids(records.clone());
    for record in records {
        let stored = restored
            .stored_ref(record.block_id)
            .cloned()
            .expect("stored tool block");
        assert!(restored.install_hydrated_record(record.block_id, stored, record.record));
    }
    let restored_records = restored.block_records_with_ids();

    for ((_, _, called_at_ms, output), block_id) in invocations.into_iter().zip(block_ids) {
        let state = restored_records
            .iter()
            .find(|record| record.block_id == block_id)
            .and_then(|record| record.record.tool_state.as_ref())
            .expect("persisted tool state");
        assert_eq!(state.called_at_ms, Some(called_at_ms));
        assert_eq!(
            state
                .output
                .as_ref()
                .map(|output| output.content.snapshot()),
            Some(output.to_string())
        );
    }
}

#[test]
fn signal_api_reads_sets_and_subscribes_to_values() {
    let mut app = TestApp::builder().build();

    assert!(app.run_lua(
        r#"
        _G.initial_work_state = smelt.signal.get("work_state")
        smelt.signal.subscribe("work_state", function(value, previous)
            _G.signal_transition = previous .. "->" .. value
        end)
        smelt.signal.set("work_state", "testing")
        "#
    ));
    app.drain_signals_pending();

    let globals = app.lua_probe().lua.globals();
    assert_eq!(
        globals.get::<String>("initial_work_state").ok().as_deref(),
        Some("idle")
    );
    assert_eq!(
        globals.get::<String>("signal_transition").ok().as_deref(),
        Some("idle->testing")
    );
}

#[test]
fn events_on_subscribes_to_event_shaped_signals() {
    let mut app = TestApp::builder().build();

    assert!(app.run_lua(
        r#"
        smelt.events.on("turn_start", function(payload)
            _G.turn_start_seen = payload.kind
        end)
        smelt.events.emit("turn_start", { kind = "manual" })
        "#
    ));
    app.drain_signals_pending();

    let globals = app.lua_probe().lua.globals();
    assert_eq!(
        globals.get::<String>("turn_start_seen").ok().as_deref(),
        Some("manual")
    );
}

#[test]
fn custom_events_declare_and_emit_through_events_api() {
    let mut app = TestApp::builder().build();

    assert!(app.run_lua(
        r#"
        smelt.events.on("plugin:ready", function(payload)
            _G.custom_event_seen = payload.answer
        end)
        smelt.events.emit("plugin:ready", { answer = 42 })
        "#
    ));
    app.drain_signals_pending();

    let globals = app.lua_probe().lua.globals();
    assert_eq!(globals.get::<i64>("custom_event_seen").ok(), Some(42));
}

#[test]
fn events_emit_does_not_replace_signal_value() {
    let mut app = TestApp::builder().build();

    assert!(app.run_lua(
        r#"
        smelt.events.new("plugin:tick")
        _G.before_tick = smelt.signal.get("plugin:tick")
        smelt.events.on("plugin:tick", function(payload)
            _G.tick_payload = payload.value
        end)
        smelt.events.emit("plugin:tick", { value = 7 })
        _G.after_tick = smelt.signal.get("plugin:tick")
        "#
    ));
    app.drain_signals_pending();

    let globals = app.lua_probe().lua.globals();
    assert!(globals
        .get::<mlua::Value>("before_tick")
        .is_ok_and(|v| v.is_nil()));
    assert!(globals
        .get::<mlua::Value>("after_tick")
        .is_ok_and(|v| v.is_nil()));
    assert_eq!(globals.get::<i64>("tick_payload").ok(), Some(7));
}

#[test]
fn display_only_resume_sets_resume_hint_state() {
    let mut app = TestApp::builder().build();
    let session =
        smelt_core::session::Session::new(app.core_probe().env.pid(), app.core_probe().env.cwd());
    let session_id = session.id.clone();
    let mut transcript = smelt_core::content::transcript::Transcript::new();
    transcript.push(smelt_core::transcript_model::Block::Text {
        content: "restored transcript".into(),
    });

    app.load_store_backed_session(
        crate::app::session_document::StoreBackedSessionDocument::new(
            session,
            crate::app::transcript::LoadedTranscript::full(transcript),
            crate::app::history::live_session_for_test(session_id.clone(), 0, None),
            smelt_store::StoreHead::default(),
        ),
    );

    assert!(app.session_snapshot().history.is_empty());
    assert_eq!(
        app.conversation_probe()
            .live_session()
            .map(|live| live.id()),
        Some(session_id.as_str())
    );
    let shutdown = app.shutdown_context();
    assert_eq!(shutdown.session_id, session_id);
    assert!(shutdown.has_messages);
    let state = app
        .conversation_probe()
        .shared_state()
        .expect("shared session state");
    assert_eq!(state.id, session_id);
    assert!(state.has_messages);
}

fn assert_resume_preview_is_visible(
    preview: &MaterializedWindowSnapshot,
    stage: impl std::fmt::Display,
) {
    let materialized = preview.rows.materialized_range();
    let visible_end = preview
        .scroll_top
        .saturating_add(u64::from(preview.viewport.rect.height))
        .min(preview.rows.total_rows);
    assert!(
        materialized.start <= preview.scroll_top && materialized.end >= visible_end,
        "resume preview lost viewport coverage at {stage}: materialized={materialized:?}, viewport={}..{visible_end}, lines={:?}",
        preview.scroll_top,
        preview.lines,
    );
    assert!(
        preview.rows.materialized_rows
            <= u64::from(preview.viewport.rect.height.max(1)).saturating_mul(2),
        "resume preview materialized an unbounded row range at {stage}: {:?}",
        preview.rows,
    );
    assert!(
        preview
            .lines
            .iter()
            .any(|line| !line.trim().is_empty() && !line.contains("Loading session preview")),
        "resume preview became blank at {stage}: {:?}",
        preview.lines,
    );
}

#[test]
fn resume_overlay_scrolls_virtualized_preview_across_sparse_session() {
    let guard = test_home_guard();
    {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        app.push_transcript_block(smelt_core::transcript_model::Block::User {
            text: "resume preview regression".into(),
            image_labels: Vec::new(),
            command: false,
            sent_at_ms: None,
        });
        for index in 0..80 {
            let call_id = format!("resume-preview-tool-{index}");
            let invocation_id = app.start_tool(
                call_id,
                "bash".into(),
                protocol::StyledLines::from_plain(format!("printf tool-{index}")),
                std::collections::HashMap::new(),
            );
            app.finish_tool(
                invocation_id,
                smelt_core::transcript_model::ToolStatus::Ok,
                Some(Box::new(smelt_core::transcript_model::ToolOutput {
                    content: format!("tool-{index} output ").repeat(180).into(),
                    is_error: false,
                    metadata: None,
                    content_fields: Vec::new(),
                })),
                None,
            );
        }
        app.save_session_and_flush();
    }

    let mut app = TestApp::builder().build_without_test_home_reset(&guard);
    app.set_terminal_size(120, 32);
    app.reconcile_session_catalog()
        .expect("reconcile resume preview fixture");

    assert!(app.run_lua(r#"smelt.cmd.run("resume")"#));
    app.settle_lua();
    app.render_silent();
    app.feed_one(SourceEvent::Tick(50));
    app.app.tick_timers();
    app.settle_lua();
    app.render_silent();

    assert!(app.state().active_modal.is_some());
    let preview = app
        .materialized_window_containing("tool-79")
        .expect("resume preview should materialize the saved tail");
    assert!(
        preview
            .lines
            .iter()
            .all(|line| !line.contains("session preview unavailable")),
        "resume preview rendered the hydration failure placeholder"
    );
    assert_resume_preview_is_visible(&preview, "initial tail");

    let preview_win = preview.win;
    let pointer_column = preview.viewport.rect.left.saturating_add(2);
    let pointer_row = preview.viewport.rect.top.saturating_add(2);
    let initial_scroll_top = preview.scroll_top;
    assert!(
        app.scroll_at_with_transcript_intent(
            pointer_row,
            pointer_column,
            -3,
            "coalesced_resume_preview_wheel",
        ),
        "coalesced wheel input should be handled by the preview transcript",
    );
    app.render_silent();
    for step in 0..39 {
        app.feed_one(SourceEvent::Term(crossterm::event::Event::Mouse(
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::ScrollUp,
                column: pointer_column,
                row: pointer_row,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
        )));
        app.settle_lua();

        let scrolling = app
            .materialized_window(preview_win)
            .expect("resume preview window should remain materialized while scrolling");
        assert_resume_preview_is_visible(&scrolling, format_args!("wheel-up step {step}"));

        app.render_silent();
    }

    let scrolled = app
        .materialized_window(preview_win)
        .expect("resume preview should remain materialized after scrolling");
    assert!(
        scrolled.scroll_top < initial_scroll_top,
        "resume preview did not move upward: initial={initial_scroll_top}, final={}",
        scrolled.scroll_top,
    );

    for step in 0..60 {
        app.feed_one(SourceEvent::Term(crossterm::event::Event::Mouse(
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::ScrollDown,
                column: pointer_column,
                row: pointer_row,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
        )));
        app.settle_lua();
        let scrolling = app
            .materialized_window(preview_win)
            .expect("resume preview window should remain materialized while returning to tail");
        assert_resume_preview_is_visible(&scrolling, format_args!("wheel-down step {step}"));
        app.render_silent();
    }
    let returned_to_tail = app
        .materialized_window(preview_win)
        .expect("resume preview should remain materialized at the tail");
    assert!(
        returned_to_tail
            .scroll_top
            .saturating_add(u64::from(returned_to_tail.viewport.rect.height))
            >= returned_to_tail.rows.total_rows,
        "resume preview did not return to its tail: scroll={}, height={}, total={}",
        returned_to_tail.scroll_top,
        returned_to_tail.viewport.rect.height,
        returned_to_tail.rows.total_rows,
    );

    let scrollbar_column = returned_to_tail
        .viewport
        .rect
        .left
        .saturating_add(returned_to_tail.viewport.rect.width.saturating_sub(1));
    for kind in [
        crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
    ] {
        app.feed_one(SourceEvent::Term(crossterm::event::Event::Mouse(
            crossterm::event::MouseEvent {
                kind,
                column: scrollbar_column,
                row: returned_to_tail.viewport.rect.top,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
        )));
        app.settle_lua();
    }
    app.render_silent();
    let sought_to_top = app
        .materialized_window(preview_win)
        .expect("resume preview should materialize after a scrollbar seek");
    assert!(
        sought_to_top.scroll_top < scrolled.scroll_top,
        "resume preview scrollbar did not seek upward: wheel={}, scrollbar={}",
        scrolled.scroll_top,
        sought_to_top.scroll_top,
    );
    assert_resume_preview_is_visible(&sought_to_top, "scrollbar seek");
}

#[test]
fn resume_overlay_reports_unhydratable_preview_without_panicking() {
    let guard = test_home_guard();
    let session_id = {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        for index in 0..80 {
            app.push_transcript_block(smelt_core::transcript_model::Block::Text {
                content: format!("corrupt preview fixture {index}").into(),
            });
        }
        app.save_session_and_flush();
        app.session_snapshot().id.clone()
    };

    let sessions_root = smelt_core::session::sessions_dir();
    let lineage = smelt_store::LineageSessionReader::open_existing(&sessions_root, &session_id)
        .expect("open canonical preview fixture");
    let preview = crate::app::transcript::LoadedTranscript::tail_from_sqlite(
        smelt_core::session::SessionStoreAddress::new(
            sessions_root.clone(),
            session_id.clone(),
            lineage.lineage_id().to_string(),
        ),
        59,
        22,
    )
    .expect("load compact preview before corruption");
    let db = rusqlite::Connection::open(lineage.database_path())
        .expect("open canonical preview fixture database");
    db.execute(
        "UPDATE objects
             SET bytes = zeroblob(length(bytes))
             WHERE hash = (
                 SELECT object_hash
                 FROM lineage_payload_object_refs
                 WHERE payload_kind = 'transcript'
                 ORDER BY rowid DESC
                 LIMIT 1
             )",
        [],
    )
    .expect("corrupt canonical preview fixture body");
    drop(db);

    let mut app = TestApp::builder().build_without_test_home_reset(&guard);
    app.set_terminal_size(120, 32);
    app.reconcile_session_catalog()
        .expect("reconcile corrupt preview fixture");
    let updated_at_ms = app
        .app
        .conversation
        .sessions()
        .list_session_entries_result()
        .expect("list corrupt preview fixture")
        .into_iter()
        .find_map(|entry| {
            (entry.id == session_id)
                .then_some(entry.status)
                .and_then(|status| match status {
                    smelt_core::session::SessionListStatus::Available(meta) => {
                        Some(meta.updated_at_ms)
                    }
                    smelt_core::session::SessionListStatus::Unavailable(_) => None,
                })
        })
        .expect("corrupt preview fixture catalog metadata");
    app.app.conversation.store_resume_preview(
        format!("{session_id}:{updated_at_ms}"),
        crate::app::transcript::TranscriptDocument::from_loaded_transcript(preview),
    );
    assert!(app.run_lua(r#"smelt.cmd.run("resume")"#));
    app.settle_lua();
    app.render_silent();
    app.feed_one(SourceEvent::Tick(50));
    app.app.tick_timers();
    app.settle_lua();
    app.render_silent();

    let preview = app
        .window_lines_containing("session preview unavailable")
        .expect("resume overlay should report an unavailable preview");
    assert!(
        preview
            .iter()
            .any(|line| line.contains("persisted content could not be hydrated")),
        "unavailable preview should explain the hydration failure: {preview:?}"
    );
    assert!(
        preview.iter().all(|line| !line.contains("session missing")),
        "hydration failure was misreported as a missing session: {preview:?}"
    );
    assert!(
        app.materialized_window_containing("session preview unavailable")
            .is_none(),
        "an unavailable placeholder must not remain attached as a virtual transcript",
    );
    assert!(app.state().active_modal.is_some());
    app.press(KeyCode::Esc);
    app.settle_lua();
    assert!(
        app.state().active_modal.is_none(),
        "resume modal should remain closable after preview hydration fails"
    );
}

#[test]
fn shared_session_state_uses_resume_hint_message_state() {
    let mut app = TestApp::builder().build();
    app.install_live_session_for_harness(crate::app::history::live_session_for_test(
        "saved-session".into(),
        0,
        None,
    ));

    app.publish_shared_session_state();

    let state = app
        .conversation_probe()
        .shared_state()
        .expect("shared session state");
    assert_eq!(state.id, app.session_snapshot().id);
    assert!(state.has_messages);
}

#[test]
fn assembled_system_prompt_uses_engine_template() {
    let app = TestApp::builder().build();

    let prompt = app.assemble_system_prompt();

    assert!(prompt.contains("# Managed worktrees"));
    assert!(!prompt.contains("Working directory:"));
}

#[test]
fn system_prompt_override_replaces_tui_prompt() {
    let mut app = TestApp::builder().build();
    app.configure_prompt_inputs_for_harness(
        Some("custom prompt".into()),
        Some("ignored instructions".into()),
        Some("# Skills\nignored".into()),
    );

    assert_eq!(app.assemble_system_prompt(), "custom prompt");
}

#[test]
fn system_prompt_omits_tool_guidance_when_tool_calling_disabled() {
    let mut app = TestApp::builder().build();
    let mut model = app
        .core_probe()
        .config
        .active_model()
        .expect("test app has an active model")
        .clone();
    model.config.tool_calling = Some(false);
    app.replace_active_model_for_harness(model);

    let prompt = app.assemble_system_prompt();

    assert!(!prompt.contains("# Tools"));
    assert!(!prompt.contains("read_file"));
    assert!(prompt.contains("# Code"));
}

#[test]
fn edit_file_diff_survives_draft_promotion_until_tool_finish() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(120, 40);
    app.start_turn(7);

    let call_id = "edit-file-flicker-call".to_string();
    let stream_id = "edit-file-flicker-stream".to_string();
    let dir = tempfile::tempdir().expect("create edit preview fixture directory");
    let file = dir.path().join("preview.rs");
    let path = file.to_string_lossy().into_owned();
    let old_content = "fn main() {\n    println!(\"flicker-old\");\n}\n";
    let new_content = "fn main() {\n    println!(\"flicker-new\");\n}\n";
    let old_string = "println!(\"flicker-old\");";
    let new_string = "println!(\"flicker-new\");";
    std::fs::write(&file, old_content).expect("write edit preview fixture");

    assert!(app.run_lua(&format!(
        "smelt.fs.file_state.record_read({}, {}, 1, 1000)",
        serde_json::to_string(path.as_str()).unwrap(),
        serde_json::to_string(old_content).unwrap()
    )));
    assert!(app
        .eval_lua::<bool>(&format!(
            "return smelt.fs.file_state.has({})",
            serde_json::to_string(path.as_str()).unwrap()
        ))
        .unwrap());
    assert!(app
        .eval_lua::<bool>(
            "local p = smelt.transcript.get_tool_presentation('edit_file'); return p ~= nil and type(p.draft) == 'function'",
        )
        .unwrap());
    let args = std::collections::HashMap::from([
        ("file_path".to_string(), serde_json::json!(path.as_str())),
        ("old_string".to_string(), serde_json::json!(old_string)),
        ("new_string".to_string(), serde_json::json!(new_string)),
        ("replace_all".to_string(), serde_json::json!(false)),
    ]);
    let arguments = serde_json::to_string(&serde_json::json!({
        "file_path": path.as_str(),
        "old_string": old_string,
        "new_string": new_string,
        "replace_all": false,
    }))
    .unwrap();

    app.feed_one(SourceEvent::engine(EngineEvent::ToolCallDraftStarted {
        stream_id: stream_id.clone(),
        call_id: Some(call_id.clone()),
        tool_name: Some("edit_file".into()),
    }));
    app.feed_one(SourceEvent::engine(EngineEvent::ToolCallDraftFinished {
        stream_id,
        call_id: call_id.clone(),
        tool_name: "edit_file".into(),
        arguments,
    }));
    let draft_frame = app.render_to_frame().text();
    assert_edit_file_diff_visible(&draft_frame, "draft-finished");

    let invocation_id = protocol::InvocationId::new(1);
    app.feed_one(SourceEvent::engine(EngineEvent::ToolStarted {
        invocation_id,
        call_id: call_id.clone(),
        tool_name: "edit_file".into(),
        args,
        called_at_ms: 0,
    }));
    let block_id = app
        .conversation_probe()
        .transcript()
        .history()
        .last_block_id()
        .expect("tool block");
    let preview_output = app
        .conversation_probe()
        .transcript()
        .history()
        .tool_state(block_id)
        .and_then(|state| state.preview_output.as_deref())
        .expect("promoted finished draft should keep immutable preview output");
    assert_eq!(
        preview_output
            .content_field("old_content")
            .map(smelt_core::transcript_content::TranscriptContent::snapshot)
            .as_deref(),
        Some(old_content)
    );
    assert_eq!(
        preview_output
            .content_field("new_content")
            .map(smelt_core::transcript_content::TranscriptContent::snapshot)
            .as_deref(),
        Some(new_content)
    );
    let pending_frame = app.render_to_frame().text();
    assert_edit_file_diff_visible(&pending_frame, "pending-promoted");

    assert!(app.run_lua(&format!(
        "smelt.fs.file_state.record_write({}, {})",
        serde_json::to_string(path.as_str()).unwrap(),
        serde_json::to_string(new_content).unwrap()
    )));
    let after_write_frame = app.render_to_frame().text();
    assert_edit_file_diff_visible(&after_write_frame, "pending-after-file-state-write");

    app.feed_one(SourceEvent::engine(EngineEvent::ToolFinished {
        invocation_id,
        call_id: call_id.clone(),
        result: protocol::ToolOutcome::new(
            format!("edited {path}"),
            false,
            Some(serde_json::json!({ "path": path.as_str() })),
        )
        .with_display_content(vec![
            protocol::ToolDisplayContent::new("old_content", old_content.into()),
            protocol::ToolDisplayContent::new("new_content", new_content.into()),
        ]),
        elapsed_ms: Some(5),
    }));
    assert!(
        app.conversation_probe()
            .transcript()
            .history()
            .tool_state(block_id)
            .is_some_and(|state| state.preview_output.is_none()),
        "finished tool should rely on output metadata instead of preview output"
    );
    let finished_frame = app.render_to_frame().text();
    assert_edit_file_diff_visible(&finished_frame, "finished");
}

fn assert_edit_file_diff_visible(frame: &str, stage: &str) {
    assert!(
        frame.contains("flicker-old") && frame.contains("flicker-new"),
        "edit_file diff not visible at {stage}:\n{frame}"
    );
}

fn evaluate_tool_decision(
    app: &mut TestApp,
    request_id: u64,
    call_id: &str,
    tool_name: &str,
    args: &std::collections::HashMap<String, serde_json::Value>,
) -> protocol::Decision {
    app.feed_one(SourceEvent::engine(EngineEvent::ToolEvaluationRequest {
        request_id,
        invocation_id: protocol::InvocationId::new(request_id),
        call_id: call_id.into(),
        tool_name: tool_name.into(),
        args: args.clone(),
        mode: protocol::AgentMode::parse("apply").unwrap(),
    }));
    app.actions()
        .iter()
        .rev()
        .find_map(|action| match action {
            Action::EngineSend(command) => match command.as_ref() {
                protocol::UiCommand::ToolEvaluationResponse {
                    request_id: response_id,
                    evaluation,
                } if *response_id == request_id => Some(evaluation.decision.clone()),
                _ => None,
            },
            _ => None,
        })
        .expect("tool evaluation response")
}

#[derive(Clone, Copy)]
enum EditFileReadState {
    Unread,
    Fresh,
    Stale,
}

struct InvalidEditFileCase {
    name: &'static str,
    content: &'static str,
    old_string: &'static str,
    new_string: &'static str,
    read_state: EditFileReadState,
    error: &'static str,
}

#[test]
fn invalid_edit_files_are_rejected_without_a_speculative_diff() {
    let cases = [
        InvalidEditFileCase {
            name: "unread",
            content: "const BEFORE_UNREAD: i32 = 1;\n",
            old_string: "BEFORE_UNREAD",
            new_string: "AFTER_UNREAD",
            read_state: EditFileReadState::Unread,
            error: "read the file with read_file before editing",
        },
        InvalidEditFileCase {
            name: "stale",
            content: "const BEFORE_STALE: i32 = 1;\n",
            old_string: "BEFORE_STALE",
            new_string: "AFTER_STALE",
            read_state: EditFileReadState::Stale,
            error: "file has been modified since last read; use read_file to read the current contents before editing",
        },
        InvalidEditFileCase {
            name: "missing-string",
            content: "const PRESENT_ONLY: i32 = 1;\n",
            old_string: "MISSING_TARGET",
            new_string: "MISSING_REPLACEMENT",
            read_state: EditFileReadState::Fresh,
            error: "old_string not found in file",
        },
        InvalidEditFileCase {
            name: "duplicate-match",
            content: "DUPLICATE_TARGET\nDUPLICATE_TARGET\n",
            old_string: "DUPLICATE_TARGET",
            new_string: "DUPLICATE_REPLACEMENT",
            read_state: EditFileReadState::Fresh,
            error: "old_string matched 2 times; make it unique or set replace_all to true",
        },
        InvalidEditFileCase {
            name: "identical-replacement",
            content: "const IDENTICAL_TARGET: i32 = 1;\n",
            old_string: "IDENTICAL_TARGET",
            new_string: "IDENTICAL_TARGET",
            read_state: EditFileReadState::Fresh,
            error: "old_string and new_string are identical",
        },
    ];

    for (index, case) in cases.iter().enumerate() {
        assert_invalid_edit_file_lifecycle(case, 100 + index as u64);
    }
}

fn assert_invalid_edit_file_lifecycle(case: &InvalidEditFileCase, request_id: u64) {
    let dir = tempfile::tempdir().expect("create edit fixture directory");
    let file = dir.path().join(format!("{}.rs", case.name));
    let path = file.to_string_lossy();
    std::fs::write(&file, case.content).expect("write edit fixture");

    let mut app = TestApp::builder().build();
    app.set_terminal_size(100, 24);
    app.start_turn(7);
    match case.read_state {
        EditFileReadState::Unread => {}
        EditFileReadState::Fresh => assert!(app.run_lua(&format!(
            "smelt.fs.file_state.record_read({}, {}, 0, {})",
            serde_json::to_string(path.as_ref()).unwrap(),
            serde_json::to_string(case.content).unwrap(),
            case.content.len(),
        ))),
        EditFileReadState::Stale => assert!(app.run_lua(&format!(
            "smelt.fs.file_state.record_read_with_mtime({}, {}, 0, {}, 0)",
            serde_json::to_string(path.as_ref()).unwrap(),
            serde_json::to_string(case.content).unwrap(),
            case.content.len(),
        ))),
    }

    let call_id = format!("invalid-edit-file-{}", case.name);
    let stream_id = format!("{call_id}-stream");
    let args = std::collections::HashMap::from([
        ("file_path".to_string(), serde_json::json!(path.as_ref())),
        ("old_string".to_string(), serde_json::json!(case.old_string)),
        ("new_string".to_string(), serde_json::json!(case.new_string)),
        ("replace_all".to_string(), serde_json::json!(false)),
    ]);
    let arguments = serde_json::to_string(&args).unwrap();

    app.feed_one(SourceEvent::engine(EngineEvent::ToolCallDraftStarted {
        stream_id: stream_id.clone(),
        call_id: Some(call_id.clone()),
        tool_name: Some("edit_file".into()),
    }));
    app.feed_one(SourceEvent::engine(EngineEvent::ToolCallDraftFinished {
        stream_id,
        call_id: call_id.clone(),
        tool_name: "edit_file".into(),
        arguments,
    }));

    let draft_frame = app.render_to_frame().text();
    assert!(draft_frame.contains("* edit_file"), "frame: {draft_frame}");
    assert_no_speculative_edit(&draft_frame, case, "draft");

    let invocation_id = protocol::InvocationId::new(request_id);
    let decision = evaluate_tool_decision(&mut app, request_id, &call_id, "edit_file", &args);
    assert_eq!(
        decision,
        protocol::Decision::Error(case.error.into()),
        "case: {}",
        case.name
    );

    app.feed_one(SourceEvent::engine(EngineEvent::ToolRejected {
        invocation_id,
        call_id,
        tool_name: "edit_file".into(),
        args,
        summary: protocol::StyledLines::empty(),
        result: protocol::ToolOutcome::new(case.error.into(), true, None),
        elapsed_ms: Some(1),
        called_at_ms: 0,
    }));
    let error_frame = app.render_to_frame().text();
    assert!(
        error_frame.contains(case.error),
        "case: {}; frame: {error_frame}",
        case.name
    );
    assert_no_speculative_edit(&error_frame, case, "rejected");
    let block_id = app
        .conversation_probe()
        .transcript()
        .history()
        .last_block_id()
        .expect("rejected edit block");
    assert_eq!(
        app.conversation_probe()
            .transcript()
            .history()
            .tool_state(block_id)
            .map(|state| state.status),
        Some(smelt_core::ToolStatus::Err),
        "case: {}",
        case.name
    );
}

fn assert_no_speculative_edit(frame: &str, case: &InvalidEditFileCase, stage: &str) {
    assert!(
        !frame.contains(case.old_string) && !frame.contains(case.new_string),
        "{} edit rendered speculative content at {stage}:\n{frame}",
        case.name
    );
}

#[test]
fn stale_edit_notebook_is_rejected_in_preflight() {
    let dir = tempfile::tempdir().expect("create notebook fixture directory");
    let file = dir.path().join("stale.ipynb");
    let path = file.to_string_lossy();
    let content = r#"{"cells":[]}"#;
    std::fs::write(&file, content).expect("write notebook fixture");

    let mut app = TestApp::builder().build();
    app.start_turn(7);
    assert!(app.run_lua(&format!(
        "smelt.fs.file_state.record_read_with_mtime({}, {}, 0, {}, 0)",
        serde_json::to_string(path.as_ref()).unwrap(),
        serde_json::to_string(content).unwrap(),
        content.len(),
    )));

    let args = std::collections::HashMap::from([(
        "notebook_path".into(),
        serde_json::json!(path.as_ref()),
    )]);
    let decision =
        evaluate_tool_decision(&mut app, 92, "stale-edit-notebook", "edit_notebook", &args);
    assert_eq!(
        decision,
        protocol::Decision::Error(
            "notebook has been modified since last read; use read_file to read the current contents before editing".into()
        )
    );
}

#[test]
fn parallel_pending_tool_timers_refresh_live() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(80, 20);
    app.start_turn(7);
    for (id, call_id, path) in [(1, "read-a", "a.rs"), (2, "read-b", "b.rs")] {
        app.feed_one(SourceEvent::engine(EngineEvent::ToolStarted {
            invocation_id: protocol::InvocationId::new(id),
            call_id: call_id.into(),
            tool_name: "read_file".into(),
            args: std::collections::HashMap::from([("file_path".into(), serde_json::json!(path))]),
            called_at_ms: 0,
        }));
    }

    app.render_silent();
    assert!(app.run_lua("smelt.transcript.fold_all('open')"));
    let initial = app.render_to_frame().text();
    assert_eq!(initial.matches("2.0s").count(), 0);

    app.feed_one(SourceEvent::Tick(2_000));
    let live = app.render_to_frame().text();

    let live_tool_timers = live
        .lines()
        .filter(|line| line.contains("read_file") && line.contains("  2.0s"))
        .count();
    assert_eq!(
        live_tool_timers, 2,
        "both pending tool timers should repaint before either call finishes:\n{live}"
    );
}

#[test]
fn stale_title_response_after_reset_is_ignored() {
    let mut app = TestApp::builder().build();
    let original_session_id = app.session_snapshot().id.clone();

    publish_input_submit(&mut app, "Fix flaky integration tests");
    let ask_ids = engine_ask_ids(app.drain_engine_sends());
    assert_eq!(ask_ids.len(), 1, "title should issue one background ask");
    let title_id = ask_ids[0];

    {
        app.reset_session();
    }
    assert_ne!(app.session_snapshot().id, original_session_id);

    respond_ask_with_text(
        &mut app,
        title_id,
        r#"{"title":"Wrong session title","slug":"wrong-session"}"#,
    );

    assert_eq!(app.session_snapshot().title, None);
    assert_eq!(app.session_snapshot().slug, None);
}

#[test]
fn title_response_after_rewind_is_ignored() {
    let mut app = TestApp::builder().build();

    publish_input_submit(&mut app, "Add caching to parser");
    let ask_ids = engine_ask_ids(app.drain_engine_sends());
    assert_eq!(ask_ids.len(), 1, "title should issue one background ask");
    let title_id = ask_ids[0];

    publish_history_delta(&mut app, HistoryDeltaKind::Rewound);
    respond_ask_with_text(
        &mut app,
        title_id,
        r#"{"title":"Stale parser cache","slug":"stale-parser-cache"}"#,
    );

    assert_eq!(app.session_snapshot().title, None);
    assert_eq!(app.session_snapshot().slug, None);
}

#[test]
fn cancelled_title_request_does_not_fallback_to_submitted_text() {
    let mut app = TestApp::builder().build();

    publish_input_submit(&mut app, "Temporary request to cancel");
    let ask_ids = engine_ask_ids(app.drain_engine_sends());
    assert_eq!(ask_ids.len(), 1, "title should issue one background ask");

    respond_ask_with_error(
        &mut app,
        ask_ids[0],
        protocol::EngineAskErrorKind::Cancelled,
    );

    assert_eq!(app.session_snapshot().title, None);
    assert_eq!(app.session_snapshot().slug, None);
}

#[test]
fn rewind_restores_session_title_snapshot() {
    let mut app = TestApp::builder().build();
    app.replace_history_for_harness(vec![
        protocol::HistoryItem::user(protocol::Content::text("First task")),
        protocol::HistoryItem::Assistant(protocol::AssistantStep::terminal(
            Some(protocol::Content::text("done")),
            None,
            Vec::new(),
        )),
        protocol::HistoryItem::user(protocol::Content::text("Second task")),
        protocol::HistoryItem::Assistant(protocol::AssistantStep::terminal(
            Some(protocol::Content::text("done")),
            None,
            Vec::new(),
        )),
    ]);
    app.restore_screen();

    app.set_session_title("First task".into(), "first-task".into(), Some(1));
    app.set_session_title("Second task".into(), "second-task".into(), Some(3));

    let restored = app.rewind_to_history(2).expect("second user turn");

    assert_eq!(restored.0, "Second task");
    assert_eq!(app.session_snapshot().history.len(), 2);
    assert_eq!(app.session_snapshot().title.as_deref(), Some("First task"));
    assert_eq!(app.session_snapshot().slug.as_deref(), Some("first-task"));
    assert_eq!(app.task_label(), Some("first-task"));
}

#[test]
fn rewind_to_start_clears_session_title_snapshot() {
    let mut app = TestApp::builder().build();
    app.replace_history_for_harness(vec![protocol::HistoryItem::user(protocol::Content::text(
        "First task",
    ))]);
    app.set_session_title("First task".into(), "first-task".into(), Some(1));

    app.rewind_to_start();

    assert_eq!(app.session_snapshot().title, None);
    assert_eq!(app.session_snapshot().slug, None);
    assert_eq!(app.task_label(), None);
}

#[test]
fn second_title_request_supersedes_inflight_response() {
    let mut app = TestApp::builder().build();

    app.session_append_history(protocol::HistoryItem::user(protocol::Content::text(
        "Investigate parser panic",
    )));
    publish_input_submit(&mut app, "Investigate parser panic");
    let first_ids = engine_ask_ids(app.drain_engine_sends());
    assert_eq!(first_ids.len(), 1);
    let first_id = first_ids[0];

    app.session_append_history(protocol::HistoryItem::user(protocol::Content::text(
        "Fix renderer panic instead",
    )));
    publish_input_submit(&mut app, "Fix renderer panic instead");
    let second_ids = engine_ask_ids(app.drain_engine_sends());
    assert_eq!(second_ids.len(), 1);
    let second_id = second_ids[0];
    assert_ne!(first_id, second_id);

    respond_ask_with_text(
        &mut app,
        first_id,
        r#"{"title":"Old parser panic","slug":"old-parser"}"#,
    );
    assert_eq!(app.session_snapshot().title, None);

    respond_ask_with_text(
        &mut app,
        second_id,
        r#"{"title":"Fix renderer panic","slug":"fix-renderer"}"#,
    );
    assert_eq!(
        app.session_snapshot().title.as_deref(),
        Some("Fix renderer panic")
    );
    assert_eq!(app.session_snapshot().slug.as_deref(), Some("fix-renderer"));
}

#[test]
fn slow_double_escape_is_two_single_escapes() {
    let mut app = TestApp::builder().build();
    app.start_turn(1);

    app.press(KeyCode::Esc);
    app.feed_one(SourceEvent::Tick(crate::app::ESC_CHORD_TIMEOUT_MS + 1));
    app.press(KeyCode::Esc);

    assert!(app.agent_running(), "expired Esc prefix must not cancel");
}

#[test]
fn non_escape_key_breaks_pending_escape_sequence() {
    let mut app = TestApp::builder().build();
    app.start_turn(1);

    app.press(KeyCode::Esc);
    app.type_char('x');
    app.press(KeyCode::Esc);

    assert!(
        app.agent_running(),
        "Esc, other key, Esc is not a double Esc"
    );
}

#[test]
fn timed_notification_expires_after_ttl() {
    let mut app = TestApp::builder().build();

    app.notify_error("transient error".into());
    assert!(app.state().notification.is_some());

    app.feed_one(SourceEvent::Tick(crate::app::NOTIFICATION_TTL_MS + 1));

    assert!(app.state().notification.is_none());
}

#[test]
fn lua_tool_watchdog_timeout_is_reported_only_as_tool_result() {
    let mut app = TestApp::builder().build();
    app.start_turn(1);
    app.run_lua_result(
        r#"
        smelt.tools.register({
            name = "watchdog_timeout_probe",
            description = "wait past the tool watchdog",
            parameters = { type = "object", properties = {} },
            watchdog_timeout_ms = 100,
            execute = function()
                smelt.sleep(1000)
                return "unexpected completion"
            end,
        })
        "#,
    )
    .expect("register timeout probe");

    app.feed_one(SourceEvent::engine(EngineEvent::ToolDispatch {
        request_id: 41,
        invocation_id: protocol::InvocationId::new(41),
        call_id: "watchdog-timeout".into(),
        tool_name: "watchdog_timeout_probe".into(),
        args: std::collections::HashMap::new(),
    }));
    app.feed_one(SourceEvent::Tick(101));
    app.feed_one(SourceEvent::LuaWakeup);

    assert!(app.actions().iter().any(|action| matches!(
        action,
        Action::EngineSend(command)
            if matches!(
                command.as_ref(),
                protocol::UiCommand::ToolResult {
                    request_id: 41,
                    call_id,
                    content,
                    is_error: true,
                    ..
                } if call_id == "watchdog-timeout" && content.contains("timed out after 0.1s")
            )
    )));
    assert!(
        app.overlays_probe().notification().is_none(),
        "tool timeout also opened a notification: {:?}",
        app.overlays_probe()
            .notification()
            .map(|notification| &notification.summary)
    );
}

#[test]
fn sticky_notification_waits_for_escape() {
    let mut app = TestApp::builder().with_vim(false).build();

    app.notify_application_error_sticky("quota reached".into());
    let notification = app.state().notification;
    assert!(notification.is_some());

    app.feed_one(SourceEvent::Tick(crate::app::NOTIFICATION_TTL_MS + 1));
    assert_eq!(app.state().notification, notification);

    app.type_char('x');
    assert_eq!(app.state().notification, notification);

    app.press(KeyCode::Esc);
    assert!(app.state().notification.is_none());
}

fn first_transcript_action_position(
    app: &mut TestApp,
) -> (
    crate::smelt_edit::DocPosition,
    smelt_core::buffer::SpanAction,
) {
    let total_rows = app.transcript_total_rows();
    for row in 0..total_rows {
        let display_rows = app.transcript_rows_and_breaks_range(row, 1);
        let Some(display_row) = display_rows.rows.into_iter().next() else {
            continue;
        };
        let Some(action) = display_row.actions.into_iter().next() else {
            continue;
        };
        let byte_col = crate::smelt_edit::text::cell_to_byte(&display_row.text, action.cell_start);
        return (
            crate::smelt_edit::DocPosition { row, byte_col },
            action.action,
        );
    }
    panic!("transcript did not expose any display action");
}

#[test]
fn short_transcript_display_document_surfaces_actions() {
    let url = "https://example.test/short";
    let mut app = TestApp::builder().with_vim(true).build();
    app.set_terminal_size(80, 16);
    app.push_transcript_block(smelt_core::transcript_model::Block::Text {
        content: format!("short [link]({url})").into(),
    });
    app.render_silent();
    assert!(
        app.transcript_window().materialized_rows.is_none(),
        "short transcript should exercise the normal-buffer document path"
    );

    let (pos, action) = first_transcript_action_position(&mut app);
    assert_eq!(
        action,
        smelt_core::buffer::SpanAction::OpenUrl(url.to_string())
    );
    assert_eq!(
        app.document_action_at(crate::app::TRANSCRIPT_WIN, pos),
        Some(smelt_core::buffer::SpanAction::OpenUrl(url.to_string()))
    );
}

#[test]
fn row_backed_transcript_display_document_surfaces_actions() {
    let url = "https://example.test/row-backed";
    let mut app = TestApp::builder().with_vim(true).build();
    app.set_terminal_size(80, 16);
    app.push_transcript_block(smelt_core::transcript_model::Block::Text {
        content: format!("head [link]({url})").into(),
    });
    for i in 0..120 {
        app.push_transcript_block(smelt_core::transcript_model::Block::Text {
            content: format!("tail {i}").into(),
        });
    }
    app.render_silent();
    assert!(
        app.transcript_window().materialized_rows.is_some(),
        "large transcript should exercise the row-backed document path"
    );

    let (pos, action) = first_transcript_action_position(&mut app);
    assert_eq!(
        action,
        smelt_core::buffer::SpanAction::OpenUrl(url.to_string())
    );
    assert_eq!(
        app.document_action_at(crate::app::TRANSCRIPT_WIN, pos),
        Some(smelt_core::buffer::SpanAction::OpenUrl(url.to_string()))
    );
}

#[test]
fn transcript_vim_gg_g_and_count_g_use_row_document() {
    let mut app = row_document_transcript_app(100, true);
    let total_rows = transcript_total_rows(&app);
    assert!(total_rows > 40, "test transcript should be row-backed");

    app.type_char('g');
    app.type_char('g');
    assert_eq!(transcript_row_cursor_row(&app), 0);

    app.type_char('G');
    app.render_silent();
    assert_eq!(transcript_row_cursor_row(&app), total_rows - 1);
    assert!(
        app.transcript_window().following_tail,
        "vim G should enter transcript tail-follow mode"
    );

    app.type_char('2');
    app.type_char('5');
    app.type_char('G');
    assert_eq!(transcript_row_cursor_row(&app), 24);
}

#[test]
fn transcript_h_and_l_move_row_cursor_horizontally() {
    let mut app = row_document_transcript_app(100, true);
    let materialized_head = transcript_buffer_lines(&app, 1).pop().unwrap_or_default();

    app.type_char('g');
    app.type_char('g');

    let start = app.transcript_window().row_cursor.unwrap();
    assert_eq!(start.row, 0);
    let row = app
        .transcript_rows_and_breaks_range(start.row, 1)
        .into_text_rows()
        .pop()
        .expect("absolute transcript row");
    assert_ne!(row, materialized_head, "row 0 must be off-materialized");
    assert!(row.len() > 1, "row must allow horizontal motion: {row:?}");

    app.type_char('l');
    assert_eq!(
        app.transcript_window().row_cursor.unwrap().byte_col,
        crate::smelt_edit::text::next_grapheme_boundary(&row, start.byte_col)
    );

    app.type_char('h');
    assert_eq!(
        app.transcript_window().row_cursor.unwrap().byte_col,
        start.byte_col
    );
}

#[test]
fn transcript_line_end_uses_absolute_document_row() {
    let mut app = row_document_transcript_app(100, true);
    let materialized_head = transcript_buffer_lines(&app, 1).pop().unwrap_or_default();

    app.type_char('g');
    app.type_char('g');

    let start = app.transcript_window().row_cursor.unwrap();
    assert_eq!(start.row, 0);
    let row = app
        .transcript_rows_and_breaks_range(start.row, 1)
        .into_text_rows()
        .pop()
        .expect("absolute transcript row");
    assert_ne!(row, materialized_head, "row 0 must be off-materialized");
    assert!(!row.is_empty(), "row must have an end: {row:?}");

    app.type_char('$');

    assert_eq!(
        app.transcript_window().row_cursor.unwrap().byte_col,
        crate::smelt_edit::text::prev_grapheme_boundary(&row, row.len())
    );
}

#[test]
fn transcript_user_resize_keeps_viewport_top_content_stable() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(56, 20);
    let before = "before wrapping content ".repeat(6);
    app.push_transcript_block(smelt_core::transcript_model::Block::User {
        text: format!("{before}\nANCHOR stay at viewport top\nafter"),
        image_labels: vec![],
        command: false,
        sent_at_ms: None,
    });
    for i in 0..120 {
        app.push_transcript_block(smelt_core::transcript_model::Block::Text {
            content: format!("tail {i}").into(),
        });
    }
    app.render_silent();
    pin_transcript_top_to_line_containing(&mut app, "ANCHOR");

    app.set_terminal_size(96, 20);
    app.render_silent();

    assert!(
        transcript_viewport_top_line(&app).contains("ANCHOR"),
        "viewport top moved to {:?}",
        transcript_viewport_top_line(&app)
    );

    app.set_terminal_size(56, 20);
    app.render_silent();

    assert!(
        transcript_viewport_top_line(&app).contains("ANCHOR"),
        "viewport top moved to {:?}",
        transcript_viewport_top_line(&app)
    );
}

#[test]
fn transcript_resize_keeps_wrapped_line_anchor_visible() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(56, 20);
    let before = "before wrapping content ".repeat(10);
    let after = " after wrapping content".repeat(10);
    app.push_transcript_block(smelt_core::transcript_model::Block::Text {
        content: format!("{before} ANCHOR stay at viewport top {after}").into(),
    });
    for i in 0..120 {
        app.push_transcript_block(smelt_core::transcript_model::Block::Text {
            content: format!("tail {i}").into(),
        });
    }
    app.render_silent();
    pin_transcript_top_to_line_containing(&mut app, "ANCHOR");

    app.set_terminal_size(96, 20);
    app.render_silent();

    assert!(
        transcript_viewport_lines(&app)
            .iter()
            .any(|line| line.contains("ANCHOR")),
        "viewport moved to {:?}",
        transcript_viewport_lines(&app)
    );

    app.set_terminal_size(40, 20);
    app.render_silent();

    assert!(
        transcript_viewport_lines(&app)
            .iter()
            .any(|line| line.contains("ANCHOR")),
        "viewport moved to {:?}",
        transcript_viewport_lines(&app)
    );
}

#[test]
fn transcript_resize_keeps_markdown_anchor_visible() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(58, 20);
    app.push_transcript_block(smelt_core::transcript_model::Block::Text {
        content: format!(
            "# Heading\n\n{} ANCHOR markdown paragraph {}\n\n- {}\n- tail item",
            "paragraph with `inline code`, **bold text**, and wrap pressure".repeat(5),
            "after anchor content that continues wrapping".repeat(4),
            "list item with enough words to wrap around the viewport".repeat(5),
        )
        .into(),
    });
    for i in 0..120 {
        app.push_transcript_block(smelt_core::transcript_model::Block::Text {
            content: format!("tail {i}").into(),
        });
    }
    app.render_silent();
    pin_transcript_viewport_to_line_containing(&mut app, "ANCHOR");

    app.set_terminal_size(96, 20);
    app.render_silent();
    assert!(
        transcript_viewport_lines(&app)
            .iter()
            .any(|line| line.contains("ANCHOR")),
        "viewport moved to {:?}",
        transcript_viewport_lines(&app)
    );

    app.set_terminal_size(42, 20);
    app.render_silent();
    assert!(
        transcript_viewport_lines(&app)
            .iter()
            .any(|line| line.contains("ANCHOR")),
        "viewport moved to {:?}",
        transcript_viewport_lines(&app)
    );
}

#[test]
fn transcript_height_resize_keeps_top_when_cursor_is_lower_in_viewport() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(64, 28);
    app.push_transcript_block(smelt_core::transcript_model::Block::Text {
        content: "ANCHOR pinned top before height resize".into(),
    });
    for i in 0..160 {
        app.push_transcript_block(smelt_core::transcript_model::Block::Text {
            content: format!("tail {i}").into(),
        });
    }
    app.render_silent();
    let anchor_scroll = pin_transcript_top_to_line_containing(&mut app, "ANCHOR");
    move_transcript_cursor_to_row(&mut app, anchor_scroll + 14);
    assert!(
        transcript_viewport_top_line(&app).contains("ANCHOR"),
        "initial viewport top moved to {:?}; lines: {:?}",
        transcript_viewport_top_line(&app),
        transcript_viewport_lines(&app)
    );

    app.set_terminal_size(64, 12);
    app.render_silent();

    assert!(
        transcript_viewport_top_line(&app).contains("ANCHOR"),
        "height resize moved viewport top to {:?}; lines: {:?}",
        transcript_viewport_top_line(&app),
        transcript_viewport_lines(&app)
    );
}

#[test]
fn transcript_width_resize_keeps_top_when_cursor_is_lower_in_viewport() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(80, 28);
    app.push_transcript_block(smelt_core::transcript_model::Block::Text {
        content: format!(
            "{} ANCHOR pinned top before width resize {}",
            "leading wrapped text".repeat(4),
            "trailing wrapped text".repeat(8),
        )
        .into(),
    });
    for i in 0..160 {
        app.push_transcript_block(smelt_core::transcript_model::Block::Text {
            content: format!("tail {i}").into(),
        });
    }
    app.render_silent();
    let anchor_scroll = pin_transcript_top_to_line_containing(&mut app, "ANCHOR");
    move_transcript_cursor_to_row(&mut app, anchor_scroll + 16);
    assert!(
        transcript_viewport_top_line(&app).contains("ANCHOR"),
        "initial viewport top moved to {:?}; lines: {:?}",
        transcript_viewport_top_line(&app),
        transcript_viewport_lines(&app)
    );

    app.set_terminal_size(44, 28);
    app.render_silent();

    assert!(
        transcript_viewport_top_line(&app).contains("ANCHOR"),
        "width resize moved viewport top to {:?}; lines: {:?}",
        transcript_viewport_top_line(&app),
        transcript_viewport_lines(&app)
    );
}

#[test]
fn transcript_pinned_bottom_resize_click_selects_clicked_row_without_tail_snap() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let mut app = row_document_transcript_app(180, false);
    app.follow_transcript_tail();
    app.render_silent();
    assert!(app.transcript_window().following_tail);

    app.set_terminal_size(80, 12);
    app.render_silent();
    assert!(app.transcript_window().following_tail);

    let vp = app
        .transcript_window()
        .viewport
        .expect("transcript viewport after resize");
    let click_row = vp.rect.top.saturating_add(2);
    let click_col = vp
        .rect
        .left
        .saturating_add(vp.gutter_width)
        .saturating_add(3);
    let mouse = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        row: click_row,
        column: click_col,
        modifiers: KeyModifiers::empty(),
    };
    let expected = app
        .document_view_position_at_mouse_for_win(crate::app::TRANSCRIPT_WIN, mouse)
        .expect("clicked transcript row");
    let scroll_before = app.transcript_window().scroll_top;

    app.feed_one(SourceEvent::Term(Event::Mouse(mouse)));

    assert!(
        !app.transcript_window().following_tail,
        "click selection must break tail-follow before the next projection"
    );
    assert_eq!(
        app.transcript_window().scroll_top,
        scroll_before,
        "mouse down should not move the transcript before projection"
    );
    assert_eq!(transcript_row_cursor_row(&app), expected.row);

    app.render_silent();

    assert!(
        !app.transcript_window().following_tail,
        "selection must stay pinned after projection"
    );
    assert_eq!(
        app.transcript_window().scroll_top,
        scroll_before,
        "selection projection snapped away from the clicked viewport"
    );
    assert_eq!(transcript_row_cursor_row(&app), expected.row);
}

#[test]
fn transcript_scroll_state_does_not_request_tail_repin_when_pinned_at_bottom() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let mut app = row_document_transcript_app(180, false);
    app.follow_transcript_tail();
    app.render_silent();
    assert!(app.transcript_window().following_tail);

    let vp = app
        .transcript_window()
        .viewport
        .expect("transcript viewport");
    app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        row: vp.rect.bottom().saturating_sub(2),
        column: vp
            .rect
            .left
            .saturating_add(vp.gutter_width)
            .saturating_add(3),
        modifiers: KeyModifiers::empty(),
    })));
    assert!(
        !app.transcript_window().following_tail,
        "selection click should pin the transcript even at bottom"
    );

    assert!(app.run_lua(
        r#"
        local scroll = assert(smelt.win.transcript():scroll())
        _G.transcript_scroll_at_bottom = scroll.at_bottom
        _G.transcript_scroll_needs_tail_repin = scroll.needs_tail_repin
        _G.transcript_scroll_follow = scroll.follow
        "#
    ));
    let globals = app.lua_probe().lua.globals();
    assert!(
        globals
            .get::<bool>("transcript_scroll_at_bottom")
            .expect("transcript at_bottom global"),
        "clicked bottom-pinned transcript should still report at_bottom"
    );
    assert!(
        !globals
            .get::<bool>("transcript_scroll_follow")
            .expect("transcript follow global"),
        "test setup should exercise pinned-not-following state"
    );
    assert!(
        !globals
            .get::<bool>("transcript_scroll_needs_tail_repin")
            .expect("transcript needs_tail_repin global"),
        "bottom pill state should stay false when the viewport is already at bottom"
    );
}

#[test]
fn transcript_interaction_trace_records_click_and_retained_frame_events() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let mut app = row_document_transcript_app(80, false);
    app.set_transcript_scroll_trace_for_harness(true);
    app.take_transcript_interaction_trace_events_for_harness();
    app.render_silent();
    app.take_transcript_interaction_trace_events_for_harness();

    let vp = app
        .transcript_window()
        .viewport
        .expect("transcript viewport");
    app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        row: vp.rect.top.saturating_add(2),
        column: vp
            .rect
            .left
            .saturating_add(vp.gutter_width)
            .saturating_add(3),
        modifiers: KeyModifiers::empty(),
    })));
    app.render_silent();

    let events = app.take_transcript_interaction_trace_events_for_harness();
    let kinds = events
        .iter()
        .map(|event| event.kind.as_str())
        .collect::<Vec<_>>();
    assert!(
        kinds.contains(&"document_mouse_before"),
        "missing click pre-state trace in {kinds:?}"
    );
    assert!(
        kinds.contains(&"document_mouse_after"),
        "missing click post-state trace in {kinds:?}"
    );
    assert!(
        kinds.contains(&"retained_frame"),
        "missing retained frame trace in {kinds:?}"
    );
    assert!(
        !kinds.contains(&"projection_frame"),
        "cursor-only click should reuse the retained row tape: {kinds:?}"
    );
}

#[test]
fn resumed_sparse_tail_projection_covers_initial_viewport() {
    let (app, _dir) = resumed_heterogeneous_transcript_app(256, 40, 21);
    let window = app.transcript_window();
    let rows = window
        .materialized_rows
        .expect("resumed transcript should be row-materialized");
    let viewport_rows = window.viewport.expect("transcript viewport").rect.height;

    assert!(
        rows.clamped_scroll.saturating_add(viewport_rows.into())
            <= rows.row_base.saturating_add(rows.materialized_rows),
        "materialized transcript rows should cover the initial viewport: {rows:?}"
    );
}

#[test]
fn transcript_fast_scroll_jump_bottom_then_click_preserves_bottom_viewport() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let (mut app, _dir) = resumed_heterogeneous_transcript_app(320, 78, 18);
    app.set_transcript_scroll_trace_for_harness(true);
    app.take_transcript_interaction_trace_events_for_harness();

    for _ in 0..180 {
        wheel_transcript(&mut app, MouseEventKind::ScrollUp);
        app.render_silent();
    }

    app.type_char('G');
    app.render_silent();
    let bottom_scroll = app.transcript_window().scroll_top;
    let vp = app
        .transcript_window()
        .viewport
        .expect("transcript viewport");
    let mouse = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        row: vp.rect.bottom().saturating_sub(2),
        column: vp
            .rect
            .left
            .saturating_add(vp.gutter_width)
            .saturating_add(3),
        modifiers: KeyModifiers::empty(),
    };
    let expected = app
        .document_view_position_at_mouse_for_win(crate::app::TRANSCRIPT_WIN, mouse)
        .expect("clicked bottom transcript row");

    app.feed_one(SourceEvent::Term(Event::Mouse(mouse)));
    app.render_silent();

    let after_scroll = app.transcript_window().scroll_top;
    let after_cursor = transcript_row_cursor_row(&app);
    assert_eq!(
        after_cursor,
        expected.row,
        "bottom click cursor resolved to wrong row after fast sparse scroll; trace={:#?}",
        app.conversation_probe()
            .transcript()
            .scroll_trace_interaction_events()
    );
    assert_eq!(
        after_scroll,
        bottom_scroll,
        "bottom click teleported transcript after fast sparse scroll; trace={:#?}",
        app.conversation_probe()
            .transcript()
            .scroll_trace_interaction_events()
    );
}

#[test]
fn resumed_sparse_bottom_click_keeps_clicked_row_and_viewport() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let (mut app, _dir) = resumed_heterogeneous_transcript_app(120, 60, 14);
    app.configure_transcript_vim(true, crate::smelt_edit::VimMode::Normal);
    app.set_transcript_scroll_trace_for_harness(true);

    app.type_char('G');
    app.render_silent();
    app.take_transcript_interaction_trace_events_for_harness();

    let vp = app
        .transcript_window()
        .viewport
        .expect("transcript viewport");
    let click_row = vp.rect.bottom().saturating_sub(1);
    let click_col = vp
        .rect
        .left
        .saturating_add(vp.gutter_width)
        .saturating_add(3);
    let rel_row = click_row.saturating_sub(vp.rect.top) as crate::smelt_edit::RowIndex;
    let before_scroll = app.transcript_window().scroll_top;
    let total_rows = app
        .transcript_window()
        .materialized_rows()
        .expect("resumed transcript should be row-materialized")
        .total_rows;
    let expected_row = before_scroll
        .saturating_add(rel_row)
        .min(total_rows.saturating_sub(1));

    app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        row: click_row,
        column: click_col,
        modifiers: KeyModifiers::empty(),
    })));

    assert_eq!(
        app.transcript_window().scroll_top,
        before_scroll,
        "mouse down should not scroll the resumed transcript; trace={:#?}",
        app.conversation_probe()
            .transcript()
            .scroll_trace_interaction_events()
    );
    assert_eq!(
        transcript_row_cursor_row(&app),
        expected_row,
        "mouse down should put the cursor on the clicked transcript row; trace={:#?}",
        app.conversation_probe()
            .transcript()
            .scroll_trace_interaction_events()
    );
}

const SPARSE_SELECTION_RECORD_COUNT: usize = 900;
const SPARSE_SELECTION_WIDTH: u16 = 78;
const SPARSE_SELECTION_HEIGHT: u16 = 18;
const SPARSE_SELECTION_SCROLL_UP_STEPS: usize = 60;
const VISIBLE_RECORD_MARKER: &str = "record-";
const VISIBLE_RECORD_MARKER_WIDTH: usize = "record-0000".len();
const VISIBLE_RECORD_HIT_BYTE_OFFSET: usize = 2;

struct VisibleTranscriptWord {
    screen_row: u16,
    screen_col: u16,
    abs_row: RowIndex,
    byte_col: usize,
    text: String,
    visible_lines: Vec<String>,
    materialized_rows: crate::smelt_edit::MaterializedRows,
}

fn visible_transcript_record_word(app: &TestApp) -> VisibleTranscriptWord {
    let win = app.transcript_window();
    let vp = win.viewport.expect("transcript viewport after scroll");
    let materialized = win
        .materialized_rows()
        .expect("sparse transcript should be row-backed");
    let scroll_top = win.scroll_top();
    let pad_left = win.gutter_pad_left;
    let buf = app.ui_probe().buf(win.buf).expect("transcript buffer");
    let local_scroll = win.local_visual_row(scroll_top) as usize;
    let visible_end = local_scroll
        .saturating_add(vp.rect.height as usize)
        .min(buf.line_count());
    let (line_idx, byte_col, text) = (local_scroll..visible_end)
        .find_map(|idx| {
            let line = buf.get_line(idx)?;
            let byte_col = line.find(VISIBLE_RECORD_MARKER)?;
            let end = byte_col
                .saturating_add(VISIBLE_RECORD_MARKER_WIDTH)
                .min(line.len());
            Some((
                idx,
                byte_col,
                smelt_buffer::text::slice(line, byte_col..end).to_string(),
            ))
        })
        .expect("visible record marker");
    let line = buf.get_line(line_idx).expect("clicked line");
    let hit_col = smelt_buffer::text::byte_to_cell(
        line,
        byte_col.saturating_add(VISIBLE_RECORD_HIT_BYTE_OFFSET),
    ) as u16;
    let abs_row = materialized.absolute_row(line_idx as RowIndex);
    VisibleTranscriptWord {
        screen_row: vp.rect.top + abs_row.saturating_sub(scroll_top) as u16,
        screen_col: vp
            .rect
            .left
            .saturating_add(vp.gutter_width)
            .saturating_add(pad_left)
            .saturating_add(hit_col),
        abs_row,
        byte_col,
        text,
        visible_lines: transcript_viewport_lines(app),
        materialized_rows: materialized,
    }
}

fn assert_visible_word_direct_copy_round_trip(app: &mut TestApp, target: &VisibleTranscriptWord) {
    let direct = app
        .app
        .copy_document_rows(
            crate::app::TRANSCRIPT_WIN,
            crate::smelt_edit::DocRange {
                start: crate::smelt_edit::DocPosition {
                    row: target.abs_row,
                    byte_col: target.byte_col,
                },
                end: crate::smelt_edit::DocPosition {
                    row: target.abs_row,
                    byte_col: target.byte_col + target.text.len(),
                },
            },
        )
        .map(|out| out.kill_ring)
        .unwrap_or_default();
    assert_eq!(
        direct, target.text,
        "direct row copy failed before mouse: abs_row={}, byte_col={}, materialized_rows={:?}, visible_lines={:?}",
        target.abs_row, target.byte_col, target.materialized_rows, target.visible_lines
    );
}

fn click_visible_transcript_word(app: &mut TestApp, target: &VisibleTranscriptWord) {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
            kind,
            row: target.screen_row,
            column: target.screen_col,
            modifiers: KeyModifiers::empty(),
        })));
    }
}

fn scroll_sparse_transcript_away_from_tail(app: &mut TestApp) {
    for _ in 0..SPARSE_SELECTION_SCROLL_UP_STEPS {
        wheel_transcript(app, crossterm::event::MouseEventKind::ScrollUp);
        app.render_silent();
    }
}

fn expected_vim_inner_word(target: &VisibleTranscriptWord) -> &str {
    target
        .text
        .split('-')
        .next()
        .expect("record marker word before hyphen")
}

fn resumed_sparse_selection_app() -> (TestApp, tempfile::TempDir) {
    resumed_heterogeneous_transcript_app(
        SPARSE_SELECTION_RECORD_COUNT,
        SPARSE_SELECTION_WIDTH,
        SPARSE_SELECTION_HEIGHT,
    )
}

#[test]
fn resumed_sparse_transcript_double_click_copies_visible_word_after_scroll() {
    let (mut app, _dir) = resumed_sparse_selection_app();
    app.configure_transcript_vim(true, crate::smelt_edit::VimMode::Normal);
    scroll_sparse_transcript_away_from_tail(&mut app);

    let target = visible_transcript_record_word(&app);
    assert_visible_word_direct_copy_round_trip(&mut app, &target);

    for _ in 0..2 {
        click_visible_transcript_word(&mut app, &target);
    }

    let copied = app.core_probe().clipboard.kill_ring.current();
    assert_eq!(
        copied, target.text,
        "visible_lines={:?}",
        target.visible_lines
    );
}

#[test]
fn resumed_sparse_transcript_vim_iw_copies_visible_word_after_scroll() {
    let (mut app, _dir) = resumed_sparse_selection_app();
    app.configure_transcript_vim(true, crate::smelt_edit::VimMode::Normal);
    scroll_sparse_transcript_away_from_tail(&mut app);

    let target = visible_transcript_record_word(&app);
    assert_visible_word_direct_copy_round_trip(&mut app, &target);
    click_visible_transcript_word(&mut app, &target);
    assert_eq!(
        transcript_row_cursor_row(&app),
        target.abs_row,
        "single click placed cursor on wrong row before viw; visible_lines={:?}",
        target.visible_lines
    );

    for key in ['v', 'i', 'w', 'y'] {
        app.type_char(key);
    }

    let copied = app.core_probe().clipboard.kill_ring.current();
    assert_eq!(
        copied,
        expected_vim_inner_word(&target),
        "visible_lines={:?}",
        target.visible_lines
    );
}

#[test]
fn resumed_heterogeneous_sparse_wheel_scroll_up_keeps_visible_records_monotonic() {
    let count = 260;
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(count, 78, 18);
    let loaded = app
        .conversation_probe()
        .transcript()
        .history()
        .block_records()
        .len();
    assert!(
        loaded < count / 2,
        "resume test must stay sparse, loaded={loaded}, count={count}"
    );

    let initial = first_visible_record_index(&app).expect("initial visible record marker");
    let mut earliest = initial;
    for step in 0..140 {
        let before = transcript_viewport_lines(&app);
        wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollUp);
        app.render_silent();
        let after = transcript_viewport_lines(&app);
        assert_viewport_shifted_up_rows(step, &before, &after, 3);
        let current = first_visible_record_index(&app).unwrap_or_else(|| {
            panic!(
                "step {step} rendered no visible record marker: scroll={}, lines={after:?}",
                app.transcript_window().scroll_top
            )
        });
        earliest = earliest.min(current);
    }

    assert!(
        earliest < initial,
        "wheel scroll never reached an earlier record: initial={initial}, earliest={earliest}"
    );
}

#[test]
fn cold_transcript_wheel_moves_exact_visible_rows() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(78, 18);
    for index in 0..700 {
        app.push_transcript_block(smelt_core::transcript_model::Block::Text {
            content: format!(
                "record-{index:04} assistant paragraph\n\n```rust\nlet value = {index};\n```\n{}",
                "variable wrapped content ".repeat(index % 23),
            )
            .into(),
        });
    }
    app.focus_transcript();
    app.follow_transcript_tail();
    app.render_silent();

    let before = transcript_viewport_lines(&app);
    wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollUp);
    app.render_silent();
    let after = transcript_viewport_lines(&app);

    assert_viewport_shifted_up_rows(0, &before, &after, 3);
}

#[test]
fn first_resumed_wheel_after_tiny_initial_layout_moves_exact_rows() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(700, 78, 1);
    app.set_transcript_scroll_trace_for_harness(true);
    app.set_terminal_size(78, 18);
    app.render_silent();
    app.take_transcript_scroll_trace_frames_for_harness();

    let extent_reads = app
        .app
        .conversation
        .transcript_extent_store_read_count_for_harness();
    let before = transcript_viewport_lines(&app);
    wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollUp);
    app.render_silent();
    let after = transcript_viewport_lines(&app);

    assert_viewport_shifted_up_rows(0, &before, &after, 3);
    let frames = app.take_transcript_scroll_trace_frames_for_harness();
    let frame = frames.last().expect("first resumed wheel trace frame");
    assert_eq!(
        app.app
            .conversation
            .transcript_extent_store_read_count_for_harness(),
        extent_reads,
        "first resumed wheel after a tiny layout read the source-space extent index: {frame:?}"
    );
    assert_eq!(
        frame.scroll_intent,
        TranscriptScrollIntent::UserDelta { rows: -3 }
    );
    assert!(
        !frame.placeholder_rows_visible,
        "first wheel exposed sparse placeholders: {frame:?}"
    );
    assert!(
        matches!(
            frame.viewport_anchor_after,
            Some(TranscriptTraceAnchor::Content { .. })
        ),
        "first wheel did not finish on a semantic content anchor: {frame:?}"
    );

    app.set_terminal_size(78, 1);
    app.render_silent();
    app.set_terminal_size(78, 18);
    app.render_silent();
    app.take_transcript_scroll_trace_frames_for_harness();
    let extent_reads = app
        .app
        .conversation
        .transcript_extent_store_read_count_for_harness();
    let before = transcript_viewport_lines(&app);
    wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollUp);
    app.render_silent();
    let after = transcript_viewport_lines(&app);
    assert_viewport_shifted_up_rows(1, &before, &after, 3);
    assert_eq!(
        app.app
            .conversation
            .transcript_extent_store_read_count_for_harness(),
        extent_reads,
        "first wheel after a transient layout read the source-space extent index"
    );
    let frame = app
        .take_transcript_scroll_trace_frames_for_harness()
        .into_iter()
        .last()
        .expect("post-transient wheel trace frame");
    assert!(
        matches!(
            frame.viewport_anchor_after,
            Some(TranscriptTraceAnchor::Content { .. })
        ),
        "post-transient wheel did not finish on a semantic content anchor: {frame:?}"
    );
}

#[test]
fn resumed_sparse_width_resize_keeps_content_anchor_identity() {
    let count = 260;
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(count, 120, 32);
    let loaded = app
        .conversation_probe()
        .transcript()
        .history()
        .block_records()
        .len();
    assert!(loaded < count, "resize regression must start sparse");
    app.set_transcript_scroll_trace_for_harness(true);

    wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollUp);
    app.render_silent();
    app.take_transcript_scroll_trace_frames_for_harness();

    app.set_terminal_size(32, 32);
    app.render_silent();
    let frame = app
        .take_transcript_scroll_trace_frames_for_harness()
        .into_iter()
        .find(|frame| {
            matches!(
                frame.scroll_intent,
                TranscriptScrollIntent::ResizeReflow { .. }
            )
        })
        .expect("resize reflow trace frame");
    assert!(
        matches!(
            frame.projection_target,
            TranscriptProjectionTargetTrace::StableRowDelta { .. }
        ),
        "sparse resize must carry the semantic anchor directly: {frame:?}"
    );

    let Some(TranscriptTraceAnchor::Content {
        record_index: before_record,
        block_id: before_block,
        ..
    }) = frame.viewport_anchor_before
    else {
        panic!("resize setup did not start from a content anchor: {frame:?}");
    };
    let Some(TranscriptTraceAnchor::Content {
        record_index: after_record,
        block_id: after_block,
        ..
    }) = frame.viewport_anchor_after
    else {
        panic!("resize lost the content anchor: {frame:?}");
    };
    assert_eq!(
        (after_record, after_block),
        (before_record, before_block),
        "resize reflow moved to a different content identity: {frame:?}"
    );
}

#[test]
fn resumed_sparse_page_delta_uses_exact_tape_without_extent_reads() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(700, 78, 18);
    app.set_transcript_scroll_trace_for_harness(true);
    app.take_transcript_scroll_trace_frames_for_harness();

    let before_scroll = app.transcript_window().scroll_top;
    let rows = app
        .transcript_window()
        .materialized_rows()
        .expect("resumed transcript rows");
    let viewport_rows = app
        .transcript_window()
        .viewport
        .expect("resumed transcript viewport")
        .rect
        .height
        .max(1);
    let extent_reads = app
        .app
        .conversation
        .transcript_extent_store_read_count_for_harness();

    app.record_transcript_scroll_intent(
        "page_up",
        TranscriptScrollIntent::PageDelta { pages: -1 },
        before_scroll,
    );
    app.render_silent();
    let frame = app
        .take_transcript_scroll_trace_frames_for_harness()
        .into_iter()
        .last()
        .expect("page delta trace frame");

    assert_eq!(
        app.transcript_window().scroll_top,
        before_scroll.saturating_sub(u64::from(viewport_rows)),
        "page delta did not move by one exact viewport: {frame:?}"
    );
    assert_eq!(
        app.transcript_window()
            .materialized_rows()
            .expect("rows after page delta")
            .total_rows,
        rows.total_rows,
        "page delta changed the stable source-space extent: {frame:?}"
    );
    assert_eq!(
        app.app
            .conversation
            .transcript_extent_store_read_count_for_harness(),
        extent_reads,
        "local page delta read the source-space extent index"
    );
    assert_eq!(
        frame.scroll_intent,
        TranscriptScrollIntent::PageDelta { pages: -1 }
    );
    assert_eq!(
        frame.projection_target,
        TranscriptProjectionTargetTrace::StableRowDelta {
            row: before_scroll,
            delta: -(viewport_rows as isize),
        },
        "page delta did not target one exact viewport on the local tape"
    );
    assert!(
        !frame.placeholder_rows_visible,
        "page delta exposed sparse placeholders: {frame:?}"
    );
}

#[test]
fn resumed_session_wheel_moves_exact_visible_rows_end_to_end() {
    let guard = test_home_guard();
    let session_id = {
        let mut app = TestApp::builder().build_with_test_home_guard(&guard);
        for index in 0..700 {
            app.push_transcript_block(smelt_core::transcript_model::Block::Text {
                content: format!(
                    "record-{index:04} assistant paragraph\n\n```rust\nlet value = {index};\n```\n{}",
                    "variable wrapped content ".repeat(index % 23),
                )
                .into(),
            });
        }
        app.save_session_and_flush();
        app.session_snapshot().id.clone()
    };

    let mut app = TestApp::builder().build_without_test_home_reset(&guard);
    app.set_terminal_size(78, 18);
    app.load_session_by_id(&session_id);
    app.focus_transcript();
    app.render_silent();

    for step in 0..120 {
        let before = transcript_viewport_lines(&app);
        wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollUp);
        app.render_silent();
        let after = transcript_viewport_lines(&app);
        assert_viewport_shifted_up_rows(step, &before, &after, 3);
    }

    for step in 0..120 {
        let before = transcript_viewport_lines(&app);
        wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollDown);
        app.render_silent();
        let after = transcript_viewport_lines(&app);
        assert_viewport_shifted_down_rows(step, &before, &after, 3);
    }
}

#[test]
fn resumed_sparse_wheel_moves_exact_visible_rows_across_pages() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(900, 78, 18);
    app.set_transcript_scroll_trace_for_harness(true);
    app.take_transcript_scroll_trace_frames_for_harness();
    let extent_reads = app
        .app
        .conversation
        .transcript_extent_store_read_count_for_harness();
    let scrollbar_total_rows = app
        .transcript_window()
        .materialized_rows()
        .expect("resumed transcript rows")
        .total_rows;

    for step in 0..500 {
        let before = transcript_viewport_lines(&app);
        wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollUp);
        app.render_silent();
        let after = transcript_viewport_lines(&app);
        assert_viewport_shifted_up_rows(step, &before, &after, 3);
        assert_eq!(
            app.app
                .conversation
                .transcript_extent_store_read_count_for_harness(),
            extent_reads,
            "upward local wheel step {step} read the source-space extent index"
        );
        let frame = app
            .take_transcript_scroll_trace_frames_for_harness()
            .into_iter()
            .last()
            .expect("upward local wheel trace frame");
        assert_eq!(
            app.transcript_window()
                .materialized_rows()
                .expect("rows after upward local wheel step")
                .total_rows,
            scrollbar_total_rows,
            "upward local wheel step {step} changed the source-space scrollbar extent: {frame:?}"
        );
    }
    assert_eq!(
        app.app
            .conversation
            .transcript_extent_store_read_count_for_harness(),
        extent_reads,
        "upward local wheel movement read the source-space extent index"
    );
    assert_eq!(
        app.transcript_window()
            .materialized_rows()
            .expect("rows after upward local scrolling")
            .total_rows,
        scrollbar_total_rows,
        "upward local scrolling changed the source-space scrollbar extent"
    );

    for step in 0..500 {
        let before = transcript_viewport_lines(&app);
        wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollDown);
        app.render_silent();
        let after = transcript_viewport_lines(&app);
        assert_viewport_shifted_down_rows(step, &before, &after, 3);
        let frame = app
            .take_transcript_scroll_trace_frames_for_harness()
            .into_iter()
            .last()
            .expect("downward local wheel trace frame");
        assert_eq!(
            app.transcript_window()
                .materialized_rows()
                .expect("rows after downward local wheel step")
                .total_rows,
            scrollbar_total_rows,
            "downward local wheel step {step} changed the source-space scrollbar extent: {frame:?}"
        );
    }
    assert_eq!(
        app.app
            .conversation
            .transcript_extent_store_read_count_for_harness(),
        extent_reads,
        "downward local wheel movement read the source-space extent index"
    );
    assert_eq!(
        app.transcript_window()
            .materialized_rows()
            .expect("rows after downward local scrolling")
            .total_rows,
        scrollbar_total_rows,
        "downward local scrolling changed the source-space scrollbar extent"
    );
}

#[test]
fn resumed_sparse_scroll_up_reports_tail_repin_needed() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(260, 78, 18);
    let bottom_scroll = app.transcript_window().scroll_top;
    wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollUp);
    app.render_silent();
    assert!(app.run_lua(
        r#"
        local scroll = assert(smelt.win.transcript():scroll())
        _G.transcript_scroll_at_bottom = scroll.at_bottom
        _G.transcript_scroll_needs_tail_repin = scroll.needs_tail_repin
        _G.transcript_scroll_follow = scroll.follow
        _G.transcript_scroll_top = scroll.top
        _G.transcript_scroll_max = scroll.max
        "#
    ));
    let globals = app.lua_probe().lua.globals();
    assert!(
        !globals
            .get::<bool>("transcript_scroll_follow")
            .expect("transcript follow global"),
        "wheel up should leave tail-follow mode"
    );
    assert!(
        !globals
            .get::<bool>("transcript_scroll_at_bottom")
            .expect("transcript at_bottom global"),
        "wheel up should report off-bottom: bottom={bottom_scroll}, top={:?}, max={:?}, needs_tail_repin={:?}",
        globals.get::<u64>("transcript_scroll_top"),
        globals.get::<u64>("transcript_scroll_max"),
        globals.get::<bool>("transcript_scroll_needs_tail_repin")
    );
    assert!(
        globals
            .get::<bool>("transcript_scroll_needs_tail_repin")
            .expect("transcript needs_tail_repin global"),
        "wheel up should show the jump-to-bottom pill: top={:?}, max={:?}",
        globals.get::<u64>("transcript_scroll_top"),
        globals.get::<u64>("transcript_scroll_max")
    );
}

#[test]
fn resumed_sparse_jump_to_bottom_after_scroll_up_renders_tail() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(260, 78, 18);

    wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollUp);
    app.render_silent();
    let before_tail_command = app.transcript_window().scroll_top;
    assert!(
        app.run_lua("smelt.transcript.follow_tail()"),
        "jump-to-bottom lua command should succeed"
    );
    assert_eq!(
        app.transcript_window().scroll_top,
        before_tail_command,
        "transcript tail command should wait for projection instead of jumping to a sparse estimated row"
    );
    app.render_silent();

    let lines = transcript_viewport_lines(&app);
    assert!(
        lines.iter().any(|line| line.contains("record-025")),
        "jump-to-bottom should render tail records instead of an empty transcript: scroll={}, lines={lines:?}",
        app.transcript_window().scroll_top
    );
    assert!(
        app.transcript_window().following_tail,
        "jump-to-bottom should restore tail-follow"
    );
}

#[test]
fn resumed_sparse_transcript_reaches_exact_bottom_while_bash_output_streams() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(260, 78, 18);
    app.start_turn(42);
    let invocation_id = protocol::InvocationId::new(1);
    let call_id = "streaming-bash-scroll".to_string();
    app.feed_one(SourceEvent::engine(EngineEvent::ToolStarted {
        invocation_id,
        call_id: call_id.clone(),
        tool_name: "bash".into(),
        args: std::collections::HashMap::from([(
            "command".into(),
            serde_json::json!("cargo test --workspace"),
        )]),
        called_at_ms: 0,
    }));
    for line in 0..12 {
        app.feed_one(SourceEvent::engine(EngineEvent::ToolOutput {
            invocation_id,
            call_id: call_id.clone(),
            line: format!("initial streaming output line {line:02}"),
        }));
        app.render_silent();
    }

    for _ in 0..4 {
        wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollUp);
        app.render_silent();
    }
    assert!(!app.transcript_window().following_tail);

    for line in 12..96 {
        wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollDown);
        app.feed_one(SourceEvent::engine(EngineEvent::ToolOutput {
            invocation_id,
            call_id: call_id.clone(),
            line: format!("continued streaming output line {line:02}"),
        }));
        app.render_silent();
        if app.transcript_window().following_tail {
            break;
        }
    }

    assert_transcript_window_at_tail(&app, "streaming bash output");
    let before_append = app.transcript_window();
    let before_append_total_rows = before_append
        .viewport
        .expect("streaming transcript viewport before append")
        .total_rows;

    app.feed_one(SourceEvent::engine(EngineEvent::ToolOutput {
        invocation_id,
        call_id,
        line: "output appended after reaching the tail".into(),
    }));
    app.render_silent();
    assert_transcript_window_at_tail(&app, "new streaming bash output");

    let after_append = app.transcript_window();
    let viewport = after_append
        .viewport
        .expect("streaming transcript viewport after append");
    assert!(
        viewport.total_rows > before_append_total_rows,
        "streamed output should extend the transcript after tail-follow is restored: before={before_append:#?}, after={after_append:#?}"
    );
    assert!(
        after_append.scroll_top > before_append.scroll_top,
        "tail-follow should advance with newly streamed output: before={before_append:#?}, after={after_append:#?}"
    );
    let metrics = viewport
        .scrollbar
        .expect("streaming transcript scrollbar")
        .metrics(viewport.scroll_top);
    assert_eq!(
        metrics.thumb_top, metrics.max_thumb_top,
        "streaming transcript scrollbar thumb should reach the bottom"
    );
    assert!(
        app.ui_probe()
            .named_win("smelt.scroll_pills.bottom.win")
            .is_none(),
        "jump-to-bottom pill should hide at the streaming transcript tail"
    );
}

fn assert_transcript_window_at_tail(app: &TestApp, label: &str) {
    let win = app.transcript_window();
    let viewport = win.viewport.expect("transcript viewport");
    let max_scroll = viewport
        .total_rows
        .saturating_sub(RowIndex::from(viewport.rect.height.max(1)));
    assert!(
        win.following_tail,
        "{label} should pin the transcript at semantic tail: {win:#?}"
    );
    assert_eq!(
        win.scroll_top, max_scroll,
        "{label} should resolve to the exact bottom viewport: {win:#?}"
    );
}

fn transcript_vim_tail_is_settled(app: &TestApp) -> bool {
    let window = app.transcript_window();
    let Some(viewport) = window.viewport else {
        return false;
    };
    let max_scroll = viewport
        .total_rows
        .saturating_sub(RowIndex::from(viewport.rect.height.max(1)));
    let expected_cursor_row = window
        .scroll_top
        .saturating_add(RowIndex::from(viewport.rect.height.saturating_sub(1)))
        .min(viewport.total_rows.saturating_sub(1));
    window.following_tail
        && window.scroll_top == max_scroll
        && window.row_cursor.map(|cursor| cursor.row) == Some(expected_cursor_row)
}

fn settle_transcript_vim_tail(app: &mut TestApp) {
    const MAX_PROJECTION_FRAMES: usize = 16;

    for _ in 0..MAX_PROJECTION_FRAMES {
        app.render_silent();
        if transcript_vim_tail_is_settled(app) {
            return;
        }
    }
    panic!(
        "vim G did not settle at the exact transcript tail after {MAX_PROJECTION_FRAMES} frames: {:?}",
        app.transcript_window()
    );
}

fn assert_transcript_tail_follows_append(app: &mut TestApp, label: &str) {
    assert_transcript_window_at_tail(app, label);
    let append_marker = format!("tail-follow append via {label}");
    app.push_transcript_block(smelt_core::transcript_model::Block::Text {
        content: append_marker.clone().into(),
    });
    app.render_silent();
    let lines = transcript_viewport_lines(app);
    assert_transcript_window_at_tail(app, label);
    assert!(
        lines.iter().any(|line| line.contains(&append_marker)),
        "{label} did not follow appended tail content: {lines:?}"
    );
}

#[test]
fn resumed_sparse_vim_g_pins_semantic_tail() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(260, 78, 18);

    wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollUp);
    app.render_silent();
    assert!(
        !app.transcript_window().following_tail,
        "test setup should leave tail-follow mode"
    );

    app.configure_transcript_vim(true, VimMode::Normal);
    app.type_char('G');
    settle_transcript_vim_tail(&mut app);

    assert!(
        app.transcript_window().following_tail,
        "vim G should pin a resumed transcript to semantic tail"
    );
    let lines = transcript_viewport_lines(&app);
    assert!(
        lines.iter().any(|line| line.contains("record-025")),
        "vim G should render the resumed transcript tail: {lines:?}"
    );
    assert_transcript_tail_follows_append(&mut app, "vim G");
}

#[test]
fn resumed_sparse_vim_g_after_gg_pins_semantic_tail() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(900, 78, 18);
    prepare_transcript_burst_app(&mut app);

    jump_transcript_top(&mut app);
    app.type_char('G');
    let pending_tail_marker = "tail content appended while G is pending";
    app.push_transcript_block(smelt_core::transcript_model::Block::Text {
        content: pending_tail_marker.into(),
    });
    settle_transcript_vim_tail(&mut app);

    let lines = transcript_viewport_lines(&app);
    assert!(
        lines.iter().any(|line| line.contains(pending_tail_marker)),
        "vim G after gg did not include content appended before projection: {lines:?}"
    );
    assert_transcript_tail_follows_append(&mut app, "vim G after gg");
}

#[derive(Clone, Copy, Debug)]
enum TranscriptTailInput {
    MouseWheel,
    DownArrow,
    CtrlD,
    PageDown,
}

impl TranscriptTailInput {
    fn press(self, app: &mut TestApp) {
        match self {
            Self::MouseWheel => wheel_transcript(app, crossterm::event::MouseEventKind::ScrollDown),
            Self::DownArrow => app.press(KeyCode::Down),
            Self::CtrlD => app.press_mod(KeyCode::Char('d'), KeyModifiers::CONTROL),
            Self::PageDown => app.press(KeyCode::PageDown),
        }
    }
}

#[test]
fn resumed_sparse_downward_inputs_pin_and_follow_semantic_tail() {
    for input in [
        TranscriptTailInput::MouseWheel,
        TranscriptTailInput::DownArrow,
        TranscriptTailInput::CtrlD,
        TranscriptTailInput::PageDown,
    ] {
        let (mut app, _dir) = resumed_heterogeneous_transcript_app(260, 78, 18);
        app.configure_transcript_vim(true, VimMode::Normal);
        for _ in 0..8 {
            wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollUp);
            app.render_silent();
        }
        assert!(
            !app.transcript_window().following_tail,
            "{input:?} setup should leave tail-follow mode"
        );

        for _ in 0..160 {
            input.press(&mut app);
            app.render_silent();
            if app.transcript_window().following_tail {
                break;
            }
        }
        assert_transcript_tail_follows_append(&mut app, &format!("{input:?}"));
    }
}

#[test]
fn resumed_sparse_scroll_down_to_tail_hides_jump_to_bottom() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(260, 78, 18);

    for _ in 0..140 {
        wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollUp);
        app.render_silent();
    }
    assert!(
        !app.transcript_window().following_tail,
        "test setup should leave tail-follow"
    );

    for step in 0..200 {
        let before = transcript_viewport_lines(&app);
        wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollDown);
        app.render_silent();
        let after = transcript_viewport_lines(&app);
        if app.transcript_window().following_tail {
            assert_viewport_shifted_down_at_most_rows(step, &before, &after, 3);
            break;
        }
        assert_viewport_shifted_down_rows(step, &before, &after, 3);
    }
    assert!(
        app.transcript_window().following_tail,
        "wheel down should eventually reach semantic tail"
    );
    assert!(app.run_lua(
        r#"
        local scroll = assert(smelt.win.transcript():scroll())
        _G.transcript_scroll_at_bottom = scroll.at_bottom
        _G.transcript_scroll_needs_tail_repin = scroll.needs_tail_repin
        "#
    ));
    let globals = app.lua_probe().lua.globals();
    assert!(
        globals
            .get::<bool>("transcript_scroll_at_bottom")
            .expect("transcript at_bottom global"),
        "tail should report at_bottom"
    );
    assert!(
        !globals
            .get::<bool>("transcript_scroll_needs_tail_repin")
            .expect("transcript needs_tail_repin global"),
        "jump-to-bottom pill should hide at semantic tail"
    );
}

#[test]
fn committed_view_previous_user_includes_block_containing_viewport_top() {
    let mut app = TestApp::builder().with_ephemeral(true).build();
    app.set_terminal_size(80, 16);
    app.push_transcript_block(smelt_core::transcript_model::Block::User {
        text: "older user target".into(),
        image_labels: Vec::new(),
        command: false,
        sent_at_ms: None,
    });
    app.push_transcript_block(smelt_core::transcript_model::Block::Text {
        content: "older assistant response".into(),
    });
    app.push_transcript_block(smelt_core::transcript_model::Block::User {
        text: (0..24)
            .map(|line| format!("CURRENT USER line {line:02}"))
            .collect::<Vec<_>>()
            .join("\n"),
        image_labels: Vec::new(),
        command: false,
        sent_at_ms: None,
    });
    app.push_transcript_block(smelt_core::transcript_model::Block::Text {
        content: (0..40)
            .map(|line| format!("tail assistant line {line:02}"))
            .collect::<Vec<_>>()
            .join("\n")
            .into(),
    });
    app.render_silent();
    app.focus_prompt();
    app.reload_lua();
    app.render_silent();

    assert!(app.reveal_transcript_record_block(2, 0, false));
    app.render_silent();
    wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollDown);
    app.render_silent();

    let visible = transcript_viewport_lines(&app);
    assert!(
        visible
            .iter()
            .take(6)
            .any(|line| line.contains("CURRENT USER line 03")),
        "viewport top should be inside the current user block: {visible:?}"
    );
    assert!(
        visible
            .iter()
            .all(|line| !line.contains("CURRENT USER line 00")),
        "current user first line should be above the viewport: {visible:?}"
    );
    assert!(app.run_lua(
        r#"
        local view = assert(smelt.transcript.view())
        local target = assert(view:previous_block({ role = "user" }))
        _G.committed_previous_user = target.first_line
        "#
    ));
    assert_eq!(
        app.lua_probe()
            .lua
            .globals()
            .get::<String>("committed_previous_user")
            .expect("committed previous-user label"),
        "CURRENT USER line 00"
    );

    let pill_buf = app
        .ui_probe()
        .named_buf("smelt.scroll_pills.top.buf")
        .expect("visible top pill buffer");
    let label = app
        .ui_probe()
        .buf(pill_buf)
        .and_then(|buf| buf.get_line(0))
        .expect("top pill label");
    assert!(label.contains("CURRENT USER line 00"));
    assert!(!label.contains("older user target"));
}

#[test]
fn scroll_pill_clicks_jump_to_previous_user_then_back_to_tail() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    fn click_named_window(app: &mut TestApp, name: &str) {
        let win = app
            .ui_probe()
            .named_win(name)
            .unwrap_or_else(|| panic!("missing {name}"));
        let rect = app
            .ui_probe()
            .split_rect(win)
            .or_else(|| {
                app.ui_probe()
                    .win(win)
                    .and_then(|win| win.viewport.map(|viewport| viewport.rect))
            })
            .unwrap_or_else(|| panic!("missing rect for {name}"));
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
                kind,
                row: rect.top,
                column: rect.left,
                modifiers: KeyModifiers::empty(),
            })));
        }
    }

    let mut app = TestApp::builder()
        .with_ephemeral(true)
        .with_vim(true)
        .build();
    app.set_terminal_size(100, 24);
    for i in 0..120 {
        app.push_transcript_block(smelt_core::transcript_model::Block::User {
            text: format!("user target {i:03}"),
            image_labels: Vec::new(),
            command: false,
            sent_at_ms: None,
        });
        let content = if i == 119 {
            (0..48)
                .map(|line| format!("final assistant line {line:02}"))
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            format!("assistant response {i:03}")
        };
        app.push_transcript_block(smelt_core::transcript_model::Block::Text {
            content: content.into(),
        });
    }
    app.render_silent();
    app.focus_transcript();
    app.type_char('G');
    app.render_silent();
    app.render_silent();
    let tail_scroll = app.transcript_window().scroll_top;

    click_named_window(&mut app, "smelt.scroll_pills.top.win");
    app.render_silent();
    app.render_silent();
    assert!(app.transcript_window().scroll_top < tail_scroll);
    let visible = transcript_viewport_lines(&app);
    assert!(
        visible
            .iter()
            .take(3)
            .any(|line| line.contains("user target 119")),
        "previous-user pill did not align its target: {visible:?}"
    );

    click_named_window(&mut app, "smelt.scroll_pills.bottom.win");
    app.render_silent();
    assert!(app.transcript_window().following_tail);
    assert!(app.transcript_window().scroll_top >= tail_scroll);
}

#[test]
fn clear_command_resets_scrolled_transcript_view() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(260, 80, 16);
    for _ in 0..4 {
        wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollUp);
        app.render_silent();
    }
    app.focus_prompt();
    app.render_silent();

    let before = app.transcript_window();
    assert!(!before.following_tail);
    assert!(before
        .viewport
        .expect("transcript viewport")
        .scrollbar
        .is_some());
    assert!(app
        .ui_probe()
        .named_win("smelt.scroll_pills.top.win")
        .is_some());
    assert!(app
        .ui_probe()
        .named_win("smelt.scroll_pills.bottom.win")
        .is_some());

    app.type_text("/clear");
    app.press(KeyCode::Enter);
    app.render_silent();

    let after = app.transcript_window();
    assert_eq!(after.scroll_top, 0);
    assert!(after.following_tail, "cleared transcript: {after:#?}");
    let viewport = after.viewport.expect("cleared transcript viewport");
    assert!(viewport.total_rows <= viewport.rect.height.into());
    assert!(viewport.scrollbar.is_none());
    assert!(app
        .ui_probe()
        .named_win("smelt.scroll_pills.top.win")
        .is_none());
    assert!(app
        .ui_probe()
        .named_win("smelt.scroll_pills.bottom.win")
        .is_none());
}

#[test]
fn tall_write_file_expands_with_enter_and_scrolls_at_deep_offsets() {
    let mut app = TestApp::builder()
        .with_ephemeral(true)
        .with_vim(true)
        .build();
    app.set_terminal_size(100, 24);
    for i in 0..20 {
        app.push_transcript_block(smelt_core::transcript_model::Block::Text {
            content: format!("before write {i:02}").into(),
        });
    }
    let content = (0..600)
        .map(|i| format!("pub fn generated_{i:03}() -> usize {{ {i} }}"))
        .collect::<Vec<_>>()
        .join("\n");
    let invocation_id = app.start_tool(
        "tall-write-correctness".into(),
        "write_file".into(),
        protocol::StyledLines::from_plain("write generated/tall.rs"),
        std::collections::HashMap::from([
            ("file_path".into(), serde_json::json!("generated/tall.rs")),
            ("content".into(), serde_json::json!(content)),
        ]),
    );
    app.finish_tool(
        invocation_id,
        smelt_core::transcript_model::ToolStatus::Ok,
        None,
        Some(std::time::Duration::from_millis(50)),
    );
    for i in 0..20 {
        app.push_transcript_block(smelt_core::transcript_model::Block::Text {
            content: format!("after write {i:02}").into(),
        });
    }
    app.render_silent();
    app.focus_transcript();
    assert!(app.run_lua("smelt.transcript.fold_all('close')"));
    assert!(app.reveal_transcript_record_block(20, 0, true));
    app.render_silent();
    let collapsed_rows = transcript_total_rows(&app);

    app.press(KeyCode::Enter);
    app.render_silent();
    let expanded_rows = transcript_total_rows(&app);
    assert!(
        expanded_rows > collapsed_rows + 500,
        "write_file did not expand retained content: collapsed={collapsed_rows}, expanded={expanded_rows}"
    );
    let tool_top = app.transcript_window().scroll_top;
    let transcript_buf = app.transcript_window().buf;
    assert!(app
        .ui_probe()
        .buf(transcript_buf)
        .expect("expanded transcript buffer")
        .lines()
        .iter()
        .any(|line| line.contains("generated_")));

    app.type_text(&tool_top.saturating_add(450).to_string());
    app.type_char('G');
    app.render_silent();
    let deep_scroll = app.transcript_window().scroll_top;
    assert!(deep_scroll > tool_top + 300);
    wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollDown);
    app.render_silent();
    assert!(app.transcript_window().scroll_top > deep_scroll);
}

#[test]
fn committed_view_watcher_dispatches_once_per_revision() {
    let mut app = TestApp::builder().with_ephemeral(true).build();
    app.set_terminal_size(80, 16);
    app.push_transcript_block(smelt_core::transcript_model::Block::User {
        text: "anchored user".into(),
        image_labels: Vec::new(),
        command: false,
        sent_at_ms: None,
    });
    app.push_transcript_block(smelt_core::transcript_model::Block::Text {
        content: (0..40)
            .map(|line| format!("assistant line {line:02}"))
            .collect::<Vec<_>>()
            .join("\n")
            .into(),
    });
    app.render_silent();
    assert!(app.reveal_transcript_record_block(0, 0, false));
    app.render_silent();
    let scroll_top = app.transcript_window().scroll_top;

    assert!(app.run_lua(
        r#"
        _G.committed_view_calls = 0
        _G.committed_view_reg = smelt.transcript.watch_view(function(view)
          assert(smelt.transcript.view().revision == view.revision)
          _G.committed_view_calls = _G.committed_view_calls + 1
          _G.committed_view_revision = view.revision
        end)
        "#
    ));
    app.render_silent();
    assert_eq!(app.lua_int_global("committed_view_calls"), Some(1));
    let revision = app.lua_int_global("committed_view_revision");

    app.render_silent();
    assert_eq!(app.lua_int_global("committed_view_calls"), Some(1));
    assert_eq!(app.lua_int_global("committed_view_revision"), revision);

    assert!(app.run_lua(
        r#"
        _G.second_committed_view_calls = 0
        _G.second_committed_view_reg = smelt.transcript.watch_view(function()
          _G.second_committed_view_calls = _G.second_committed_view_calls + 1
        end)
        "#
    ));
    app.render_silent();
    assert_eq!(app.lua_int_global("committed_view_calls"), Some(1));
    assert_eq!(app.lua_int_global("second_committed_view_calls"), Some(1));

    app.push_transcript_block(smelt_core::transcript_model::Block::Text {
        content: "new assistant navigation target".into(),
    });
    app.render_silent();
    assert_eq!(app.transcript_window().scroll_top, scroll_top);
    assert_eq!(app.lua_int_global("committed_view_calls"), Some(2));
    assert_eq!(app.lua_int_global("second_committed_view_calls"), Some(2));
    assert_ne!(app.lua_int_global("committed_view_revision"), revision);
}

#[test]
fn committed_view_watcher_reuses_the_retained_sparse_reader() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(260, 80, 18);
    let opens_before = app
        .conversation_probe()
        .transcript()
        .store_open_attempt_count_for_harness();

    assert!(app.run_lua(
        r#"
        _G.sparse_committed_view_calls = 0
        _G.sparse_committed_view_reg = smelt.transcript.watch_view(function(view)
          local target = assert(view:previous_block({ role = "user" }))
          assert(target.first_line ~= nil)
          _G.sparse_committed_view_calls = _G.sparse_committed_view_calls + 1
        end)
        "#
    ));
    app.render_silent();

    assert_eq!(app.lua_int_global("sparse_committed_view_calls"), Some(1));
    assert_eq!(
        app.conversation_probe()
            .transcript()
            .store_open_attempt_count_for_harness(),
        opens_before,
        "committed-view dispatch and navigation must use the reader retained at resume"
    );
}

#[test]
fn stale_committed_views_and_cross_session_targets_are_rejected() {
    let mut app = TestApp::builder().with_ephemeral(true).build();
    app.set_terminal_size(80, 16);
    app.push_transcript_block(smelt_core::transcript_model::Block::User {
        text: "first user".into(),
        image_labels: Vec::new(),
        command: false,
        sent_at_ms: None,
    });
    app.push_transcript_block(smelt_core::transcript_model::Block::Text {
        content: "first response".into(),
    });
    app.push_transcript_block(smelt_core::transcript_model::Block::User {
        text: "second user".into(),
        image_labels: Vec::new(),
        command: false,
        sent_at_ms: None,
    });
    app.push_transcript_block(smelt_core::transcript_model::Block::Text {
        content: (0..30)
            .map(|line| format!("second response {line:02}"))
            .collect::<Vec<_>>()
            .join("\n")
            .into(),
    });
    app.render_silent();
    assert!(app.run_lua(
        r#"
        _G.stale_transcript_view = assert(smelt.transcript.view())
        _G.cross_session_target = assert(
          _G.stale_transcript_view:previous_block({ role = "user" })
        )
        assert(not pcall(function()
          _G.stale_transcript_view:previous_block({ role = "unknown" })
        end))
        assert(not pcall(function()
          smelt.transcript.reveal(_G.cross_session_target, { align = "center" })
        end))
        "#
    ));

    app.push_transcript_block(smelt_core::transcript_model::Block::Text {
        content: "third assistant response".into(),
    });
    assert!(app.run_lua(
        r#"assert(_G.stale_transcript_view:previous_block({ role = "assistant" }) == nil)"#
    ));

    let session_id = app.session_snapshot().id.clone();
    app.set_session_id_for_harness("different-session".into());
    assert!(app.run_lua("assert(smelt.transcript.reveal(_G.cross_session_target) == false)"));
    app.set_session_id_for_harness(session_id);
}

fn previous_user_record_index(app: &TestApp) -> usize {
    let target = app
        .conversation_probe()
        .transcript()
        .previous_navigation_block(Some("user"))
        .expect("previous user target");
    parse_record_index(&target.first_line).unwrap_or_else(|| {
        panic!(
            "previous user target did not include a record marker: {:?}",
            target.first_line
        )
    })
}

fn previous_heterogeneous_user_before(index: usize) -> usize {
    index.saturating_sub(1) / 10 * 10
}

fn previous_tail_consecutive_user_before(index: usize, count: usize) -> usize {
    let first_tail_user = count.saturating_sub(2);
    if index > first_tail_user {
        first_tail_user
    } else {
        previous_heterogeneous_user_before(index)
    }
}

fn wheel_and_render(app: &mut TestApp, kind: crossterm::event::MouseEventKind) {
    wheel_transcript(app, kind);
    app.render_silent();
}

#[test]
fn resumed_sparse_jump_to_bottom_anchors_previous_user_to_visible_tail() {
    let (mut app, _dir) =
        resumed_transcript_app_from_records(tail_consecutive_user_records(260), 78, 18);

    for _ in 0..8 {
        wheel_and_render(&mut app, crossterm::event::MouseEventKind::ScrollUp);
    }
    assert!(
        !app.transcript_window().following_tail,
        "test setup should leave tail-follow mode"
    );

    assert!(
        app.run_lua("smelt.transcript.follow_tail()"),
        "jump-to-bottom lua command should succeed"
    );
    app.render_silent();
    assert!(
        app.transcript_window().following_tail,
        "jump-to-bottom should restore tail-follow"
    );

    let first_visible = first_visible_record_index(&app).expect("visible tail record");
    assert!(
        first_visible < 258,
        "test must keep consecutive tail users below the top visible anchor; first_visible={first_visible}, lines={:?}",
        transcript_viewport_lines(&app)
    );
    let expected = previous_tail_consecutive_user_before(first_visible, 260);
    let target_after_jump = previous_user_record_index(&app);
    assert_eq!(
        target_after_jump,
        expected,
        "jump-to-bottom should anchor previous-user navigation to the visible tail, not the loaded sparse window start; first_visible={first_visible}, lines={:?}",
        transcript_viewport_lines(&app)
    );

    wheel_and_render(&mut app, crossterm::event::MouseEventKind::ScrollUp);
    let first_visible_after_scroll =
        first_visible_record_index(&app).expect("visible record after scroll");
    let expected_after_scroll =
        previous_tail_consecutive_user_before(first_visible_after_scroll, 260);
    assert_eq!(
        previous_user_record_index(&app),
        expected_after_scroll,
        "a small scroll should not be required to repair the previous-user target"
    );
}

#[test]
fn resumed_sparse_tail_scroll_down_keeps_previous_user_target_stable() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(260, 78, 18);
    assert!(
        app.transcript_window().following_tail,
        "resumed transcript test starts pinned to tail"
    );
    let bottom_scroll = app.transcript_window().scroll_top;
    let initial_target = previous_user_record_index(&app);

    for _ in 0..3 {
        wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollDown);
        app.render_silent();
        assert!(
            app.transcript_window().following_tail,
            "scrolling down at tail should remain tail-following"
        );
        assert_eq!(
            app.transcript_window().scroll_top,
            bottom_scroll,
            "scrolling down at tail should not move the viewport"
        );
        assert_eq!(
            previous_user_record_index(&app),
            initial_target,
            "top pill target changed after wheel-down input at tail"
        );

        app.render_silent();
        assert_eq!(
            previous_user_record_index(&app),
            initial_target,
            "top pill target changed on the idle render after wheel-down at tail"
        );
    }
}

#[test]
fn resumed_sparse_near_tail_scroll_down_stays_incremental() {
    let count = 260;
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(count, 78, 18);
    let loaded = app
        .conversation_probe()
        .transcript()
        .history()
        .block_records()
        .len();
    assert!(
        loaded < count / 2,
        "resume test must stay sparse, loaded={loaded}, count={count}"
    );

    assert!(
        app.transcript_window().following_tail,
        "resumed transcript test starts pinned to tail"
    );
    for step in 0..6 {
        let before = transcript_viewport_lines(&app);
        wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollUp);
        app.render_silent();
        let after = transcript_viewport_lines(&app);
        assert_viewport_shifted_up_rows(step, &before, &after, 3);
    }
    assert!(
        !app.transcript_window().following_tail,
        "wheel up should leave tail-follow mode"
    );

    let before = transcript_viewport_lines(&app);
    wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollDown);
    app.render_silent();
    let after = transcript_viewport_lines(&app);
    assert_viewport_shifted_down_rows(0, &before, &after, 3);
    assert!(
        !app.transcript_window().following_tail,
        "one near-tail wheel tick should not re-enter tail-follow after scrolling up six ticks"
    );
}

#[test]
fn resumed_sparse_first_wheel_after_tail_jumps_stays_incremental() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.install_sparse_transcript_scroll_fixture(111, 55, 21);

    for _ in 0..12 {
        app.type_char('G');
        app.render_silent();
    }

    app.take_transcript_scroll_trace_frames_for_harness();
    let before = transcript_viewport_lines(&app);
    wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollUp);
    app.render_silent();
    let after = transcript_viewport_lines(&app);
    let frames = app.take_transcript_scroll_trace_frames_for_harness();
    assert_eq!(
        &after[3..],
        &before[..before.len() - 3],
        "first wheel-up tick did not move by exactly three visible rows: before={before:?}, after={after:?}, frames={frames:#?}"
    );
}

#[test]
fn resumed_sparse_wheel_bursts_preserve_visible_movement_before_and_after_top() {
    let count = 320;
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(count, 78, 18);
    let loaded = app
        .conversation_probe()
        .transcript()
        .history()
        .block_records()
        .len();
    assert!(
        loaded < count / 2,
        "resume test must stay sparse, loaded={loaded}, count={count}"
    );

    for step in 0..70 {
        let before = transcript_viewport_lines(&app);
        for _ in 0..2 {
            wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollUp);
        }
        app.render_silent();
        let after = transcript_viewport_lines(&app);
        assert_viewport_shifted_up_rows(step, &before, &after, 6);
    }
    assert!(
        !app.transcript_window().following_tail,
        "wheel bursts should leave tail-follow mode"
    );

    let before_down = transcript_viewport_lines(&app);
    for _ in 0..2 {
        wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollDown);
    }
    app.render_silent();
    let after_down = transcript_viewport_lines(&app);
    assert_viewport_shifted_down_rows(0, &before_down, &after_down, 6);

    app.render_silent();
    assert_eq!(
        transcript_viewport_lines(&app),
        after_down,
        "idle render moved the transcript after a direction change"
    );

    for step in 70..400 {
        if app.transcript_window().scroll_top == 0 {
            break;
        }
        let before = transcript_viewport_lines(&app);
        for _ in 0..2 {
            wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollUp);
        }
        app.render_silent();
        let after = transcript_viewport_lines(&app);
        if before != after {
            assert_viewport_shifted_up_at_most_rows(step, &before, &after, 6);
        }
    }
    assert_eq!(
        app.transcript_window().scroll_top,
        0,
        "wheel bursts did not reach the top of the sparse transcript"
    );
    assert_eq!(
        first_visible_record_index(&app),
        Some(0),
        "the top viewport did not reveal the first sparse transcript record"
    );
    let at_top = transcript_viewport_lines(&app);

    for _ in 0..2 {
        wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollDown);
    }
    app.render_silent();
    let after_top = transcript_viewport_lines(&app);
    assert_viewport_shifted_down_rows(1, &at_top, &after_top, 6);
}

#[test]
fn resumed_sparse_scroll_down_after_scroll_up_does_not_snap_to_tail() {
    let count = 260;
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(count, 78, 18);
    let loaded = app
        .conversation_probe()
        .transcript()
        .history()
        .block_records()
        .len();
    assert!(
        loaded < count / 2,
        "resume test must stay sparse, loaded={loaded}, count={count}"
    );

    assert!(
        app.transcript_window().following_tail,
        "resumed transcript test starts pinned to tail"
    );
    let bottom_scroll = app.transcript_window().scroll_top;

    for _ in 0..140 {
        wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollUp);
        app.render_silent();
    }
    let after_up = app.transcript_window().scroll_top;
    assert!(
        after_up < bottom_scroll,
        "wheel up should move away from tail: bottom={bottom_scroll}, after_up={after_up}"
    );
    assert!(
        !app.transcript_window().following_tail,
        "wheel up should leave tail-follow mode"
    );

    wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollDown);
    app.render_silent();
    let after_down = app.transcript_window().scroll_top;
    assert_eq!(
        after_down,
        after_up.saturating_add(3),
        "first wheel down should move by one wheel tick, not snap to tail; bottom={bottom_scroll}, after_up={after_up}, after_down={after_down}, lines={:?}",
        transcript_viewport_lines(&app)
    );
    assert!(
        after_down < bottom_scroll,
        "first wheel down should remain off tail: bottom={bottom_scroll}, after_down={after_down}"
    );
}

#[test]
fn streaming_tool_and_compaction_updates_do_not_snap_scrolled_transcript_to_tail() {
    let mut app = row_document_transcript_app(180, false);
    app.follow_transcript_tail();
    app.render_silent();
    assert!(app.transcript_window().following_tail);

    for _ in 0..12 {
        wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollUp);
        app.render_silent();
    }
    let pinned_scroll = app.transcript_window().scroll_top;
    assert!(pinned_scroll > 0, "test must be scrolled away from top");
    assert!(
        !app.transcript_window().following_tail,
        "wheel scroll must pin transcript before streaming updates"
    );

    app.start_tool(
        "stream-write-file".into(),
        "write_file".into(),
        protocol::StyledLines::from_plain("STREAMING_WRITE_FILE should stay hidden"),
        std::collections::HashMap::new(),
    );
    app.render_silent();
    assert_eq!(
        app.transcript_window().scroll_top,
        pinned_scroll,
        "streaming tool start snapped the pinned transcript"
    );
    assert!(
        transcript_viewport_lines(&app)
            .iter()
            .all(|line| !line.contains("STREAMING_WRITE_FILE")),
        "streaming tool tail became visible: {:?}",
        transcript_viewport_lines(&app)
    );

    app.push_compaction_preview("STREAMING_COMPACTION should stay hidden");
    app.render_silent();
    assert_eq!(
        app.transcript_window().scroll_top,
        pinned_scroll,
        "streaming compaction preview snapped the pinned transcript"
    );
    assert!(
        !app.transcript_window().following_tail,
        "streaming updates must not re-enable tail-follow"
    );
    assert!(
        transcript_viewport_lines(&app)
            .iter()
            .all(|line| !line.contains("STREAMING_COMPACTION")),
        "streaming compaction tail became visible: {:?}",
        transcript_viewport_lines(&app)
    );
}

#[derive(Clone, Copy, Debug)]
enum TranscriptReplayStep {
    WheelUp { ticks: usize },
    WheelDown { ticks: usize },
    CoalescedWheel { rows: isize },
    DragAutoscrollTop { ticks: usize },
    DragAutoscrollBottom { ticks: usize },
    Resize { width: u16, height: u16 },
    StreamingAppend,
    ScrollbarFarSeek { rel_row: u16 },
    WheelUpUntilRecordRangeChanges { max_ticks: usize },
}

#[derive(Default)]
struct TranscriptScrollReplayReport {
    frames: Vec<TranscriptScrollTraceFrame>,
    record_range_changed: bool,
}

struct TranscriptScrollReplay {
    steps: Vec<TranscriptReplayStep>,
}

impl TranscriptScrollReplay {
    fn new(steps: Vec<TranscriptReplayStep>) -> Self {
        Self { steps }
    }

    fn run(self, app: &mut TestApp) -> TranscriptScrollReplayReport {
        app.set_transcript_scroll_trace_timings_for_harness(true);
        app.take_transcript_scroll_trace_frames_for_harness();
        let mut report = TranscriptScrollReplayReport::default();
        for step in self.steps {
            match step {
                TranscriptReplayStep::WheelUp { ticks } => {
                    for tick in 0..ticks {
                        let before = app.transcript_window().scroll_top;
                        wheel_transcript(app, crossterm::event::MouseEventKind::ScrollUp);
                        set_replay_trace_input(
                            app,
                            format!("wheel_up:{tick}"),
                            TranscriptScrollIntent::UserDelta { rows: -3 },
                            before,
                        );
                        render_replay_frame(app, &mut report);
                    }
                }
                TranscriptReplayStep::WheelDown { ticks } => {
                    for tick in 0..ticks {
                        let before = app.transcript_window().scroll_top;
                        wheel_transcript(app, crossterm::event::MouseEventKind::ScrollDown);
                        set_replay_trace_input(
                            app,
                            format!("wheel_down:{tick}"),
                            TranscriptScrollIntent::UserDelta { rows: 3 },
                            before,
                        );
                        render_replay_frame(app, &mut report);
                    }
                }
                TranscriptReplayStep::CoalescedWheel { rows } => {
                    let before = app.transcript_window().scroll_top;
                    let (row, col) = transcript_content_point(app, 1);
                    assert!(
                        app.scroll_at_with_transcript_intent(
                            row,
                            col,
                            rows,
                            &format!("coalesced_wheel:{rows}"),
                        ),
                        "coalesced wheel replay should pan the transcript"
                    );
                    set_replay_trace_input(
                        app,
                        format!("coalesced_wheel:{rows}"),
                        TranscriptScrollIntent::UserDelta { rows },
                        before,
                    );
                    render_replay_frame(app, &mut report);
                }
                TranscriptReplayStep::DragAutoscrollTop { ticks } => {
                    start_transcript_edge_drag(app, TranscriptDragEdge::Top);
                    for tick in 0..ticks {
                        let before = app.transcript_window().scroll_top;
                        if app.tick_drag_autoscroll_with_transcript_intent() {
                            set_replay_trace_input(
                                app,
                                format!("drag_autoscroll_top:{tick}"),
                                TranscriptScrollIntent::UserDelta { rows: -1 },
                                before,
                            );
                            render_replay_frame(app, &mut report);
                        }
                    }
                    finish_transcript_drag(app);
                }
                TranscriptReplayStep::DragAutoscrollBottom { ticks } => {
                    start_transcript_edge_drag(app, TranscriptDragEdge::Bottom);
                    for tick in 0..ticks {
                        let before = app.transcript_window().scroll_top;
                        if app.tick_drag_autoscroll_with_transcript_intent() {
                            set_replay_trace_input(
                                app,
                                format!("drag_autoscroll_bottom:{tick}"),
                                TranscriptScrollIntent::UserDelta { rows: 1 },
                                before,
                            );
                            render_replay_frame(app, &mut report);
                        }
                    }
                    finish_transcript_drag(app);
                }
                TranscriptReplayStep::Resize { width, height } => {
                    let before = app.transcript_window().scroll_top;
                    let previous_width = app
                        .transcript_window()
                        .viewport
                        .map(|viewport| viewport.content_width)
                        .unwrap_or(width);
                    app.set_terminal_size(width, height);
                    set_replay_trace_input(
                        app,
                        format!("resize:{width}x{height}"),
                        TranscriptScrollIntent::ResizeReflow { previous_width },
                        before,
                    );
                    render_replay_frame(app, &mut report);
                }
                TranscriptReplayStep::StreamingAppend => {
                    let before = app.transcript_window().scroll_top;
                    app.push_transcript_block(smelt_core::transcript_model::Block::Text {
                        content: "replay streaming append should not move pinned viewport".into(),
                    });
                    set_replay_trace_input(
                        app,
                        "streaming_append".to_string(),
                        TranscriptScrollIntent::PreserveViewport,
                        before,
                    );
                    render_replay_frame(app, &mut report);
                }
                TranscriptReplayStep::ScrollbarFarSeek { rel_row } => {
                    let before = app.transcript_window().scroll_top;
                    let (row, col, numerator, denominator, total_rows, viewport_rows) =
                        transcript_scrollbar_point(app, rel_row);
                    app.feed_one(SourceEvent::Term(Event::Mouse(
                        crossterm::event::MouseEvent {
                            kind: crossterm::event::MouseEventKind::Down(
                                crossterm::event::MouseButton::Left,
                            ),
                            row,
                            column: col,
                            modifiers: KeyModifiers::empty(),
                        },
                    )));
                    set_replay_trace_input(
                        app,
                        format!("scrollbar_far_seek:{rel_row}"),
                        TranscriptScrollIntent::ScrollbarFraction {
                            numerator,
                            denominator,
                            total_rows,
                            viewport_rows,
                        },
                        before,
                    );
                    render_replay_frame(app, &mut report);
                    app.feed_one(SourceEvent::Term(Event::Mouse(
                        crossterm::event::MouseEvent {
                            kind: crossterm::event::MouseEventKind::Up(
                                crossterm::event::MouseButton::Left,
                            ),
                            row,
                            column: col,
                            modifiers: KeyModifiers::empty(),
                        },
                    )));
                }
                TranscriptReplayStep::WheelUpUntilRecordRangeChanges { max_ticks } => {
                    for tick in 0..max_ticks {
                        if report.record_range_changed {
                            break;
                        }
                        let before = app.transcript_window().scroll_top;
                        wheel_transcript(app, crossterm::event::MouseEventKind::ScrollUp);
                        set_replay_trace_input(
                            app,
                            format!("wheel_up_window_probe:{tick}"),
                            TranscriptScrollIntent::UserDelta { rows: -3 },
                            before,
                        );
                        render_replay_frame(app, &mut report);
                    }
                }
            }
        }
        report
    }
}

#[derive(Clone, Copy)]
enum TranscriptDragEdge {
    Top,
    Bottom,
}

fn render_replay_frame(app: &mut TestApp, report: &mut TranscriptScrollReplayReport) {
    app.render_silent();
    let frames = app.take_transcript_scroll_trace_frames_for_harness();
    report.record_range_changed |= frames
        .iter()
        .any(|frame| frame.active_record_range_before != frame.active_record_range_after);
    report.frames.extend(frames);
}

fn set_replay_trace_input(
    app: &mut TestApp,
    input_event_or_tick: String,
    scroll_intent: TranscriptScrollIntent,
    window_scroll_before: crate::smelt_edit::RowIndex,
) {
    let window_scroll_after_input = app.transcript_window().scroll_top;
    app.set_next_transcript_scroll_trace_input(
        crate::app::transcript_scroll_trace::TranscriptScrollTraceRenderInput {
            input_event_or_tick,
            scroll_intent,
            window_scroll_before,
            window_scroll_after_input,
        },
    );
}

fn transcript_content_point(app: &TestApp, rel_row: u16) -> (u16, u16) {
    let vp = app
        .transcript_window()
        .viewport
        .expect("transcript viewport");
    (
        vp.rect
            .top
            .saturating_add(rel_row.min(vp.rect.height.saturating_sub(1))),
        vp.rect
            .left
            .saturating_add(vp.gutter_width)
            .saturating_add(1),
    )
}

fn transcript_scrollbar_point(
    app: &TestApp,
    rel_row: u16,
) -> (u16, u16, u64, u64, crate::smelt_edit::RowIndex, u16) {
    let vp = app
        .transcript_window()
        .viewport
        .expect("transcript viewport");
    let scrollbar = vp.scrollbar.expect("transcript scrollbar");
    let denominator = u64::from(vp.rect.height.saturating_sub(1).max(1));
    let numerator = u64::from(rel_row.min(vp.rect.height.saturating_sub(1)));
    (
        vp.rect
            .top
            .saturating_add(rel_row.min(vp.rect.height.saturating_sub(1))),
        scrollbar.col,
        numerator,
        denominator,
        scrollbar.total_rows,
        scrollbar.viewport_rows,
    )
}

fn start_transcript_edge_drag(app: &mut TestApp, edge: TranscriptDragEdge) {
    let vp = app
        .transcript_window()
        .viewport
        .expect("transcript viewport");
    let col = vp
        .rect
        .left
        .saturating_add(vp.gutter_width)
        .saturating_add(1);
    let down_row = vp.rect.top.saturating_add(vp.rect.height / 2);
    let edge_row = match edge {
        TranscriptDragEdge::Top => vp.rect.top,
        TranscriptDragEdge::Bottom => vp.rect.bottom().saturating_sub(1),
    };
    app.feed_one(SourceEvent::Term(Event::Mouse(
        crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            row: down_row,
            column: col,
            modifiers: KeyModifiers::empty(),
        },
    )));
    app.feed_one(SourceEvent::Term(Event::Mouse(
        crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            row: edge_row,
            column: col,
            modifiers: KeyModifiers::empty(),
        },
    )));
}

fn finish_transcript_drag(app: &mut TestApp) {
    let (row, col) = transcript_content_point(app, 1);
    app.feed_one(SourceEvent::Term(Event::Mouse(
        crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
            row,
            column: col,
            modifiers: KeyModifiers::empty(),
        },
    )));
}

#[derive(Clone, Debug)]
struct RevealedUserBlock {
    record_index: usize,
    block_id: u64,
    first_line: String,
}

fn reveal_user_block_via_lua(app: &mut TestApp, direction: &str) -> RevealedUserBlock {
    let view = app
        .conversation_probe()
        .committed_transcript_view()
        .expect("committed transcript view");
    let anchor = view.state.anchor.expect("committed navigation anchor");
    let expected = match direction {
        "previous" => app
            .conversation_probe()
            .transcript()
            .previous_navigation_block_from(anchor, Some("user")),
        "next" => app
            .conversation_probe()
            .transcript()
            .next_navigation_block_from(anchor, Some("user")),
        other => panic!("unsupported transcript reveal direction {other}"),
    }
    .expect("semantic user navigation target");
    let snippet = match direction {
        "previous" => {
            r#"
            local view = assert(smelt.transcript.view())
            local block = assert(view:previous_block({ role = "user" }))
            assert(block.role == "user")
            assert(smelt.transcript.reveal(block, { align = "top", move_cursor = true }))
            _G.transcript_revealed_block_id = block.block_id
            _G.transcript_revealed_first_line = block.first_line
            "#
        }
        "next" => {
            r#"
            local view = assert(smelt.transcript.view())
            local block = assert(view:next_block({ role = "user" }))
            assert(block.role == "user")
            assert(smelt.transcript.reveal(block, { align = "top", move_cursor = true }))
            _G.transcript_revealed_block_id = block.block_id
            _G.transcript_revealed_first_line = block.first_line
            "#
        }
        _ => unreachable!(),
    };
    assert!(app.run_lua(snippet));
    let globals = app.lua_probe().lua.globals();
    let block_id = globals
        .get::<u64>("transcript_revealed_block_id")
        .expect("revealed block id");
    let first_line = globals
        .get::<String>("transcript_revealed_first_line")
        .expect("revealed first line");
    assert_eq!(block_id, expected.block_id.get());
    assert_eq!(first_line, expected.first_line);
    RevealedUserBlock {
        record_index: expected.record_index,
        block_id,
        first_line,
    }
}

fn assert_reveal_block_frame(frame: &TranscriptScrollTraceFrame, block: &RevealedUserBlock) {
    assert_eq!(frame.input_event_or_tick, "reveal_block");
    assert!(
        !frame.placeholder_rows_visible,
        "semantic block reveal must not expose sparse placeholders: {frame:?}"
    );
    let first_visible = frame
        .first_visible_content_anchor
        .expect("semantic block reveal must resolve an exact content anchor");
    assert_eq!(
        first_visible.node_id,
        crate::content::transcript_scene::RenderNodeId::Block(
            smelt_core::transcript_model::BlockId::new(block.block_id),
        ),
        "semantic block reveal must align its target block to the viewport top: {frame:?}"
    );
    assert_eq!(
        first_visible.virtual_row, frame.resolved_scroll_top,
        "semantic block reveal must align its target content to the viewport top: {frame:?}"
    );
    match frame.scroll_intent {
        TranscriptScrollIntent::RevealBlock {
            record_index,
            block_id,
            row_offset,
            screen_padding_top,
            cursor_byte_col,
        } => {
            assert_eq!(record_index, block.record_index);
            assert_eq!(
                block_id,
                smelt_core::transcript_model::BlockId::new(block.block_id)
            );
            assert_eq!(row_offset, 0);
            assert_eq!(screen_padding_top, 0);
            assert_eq!(cursor_byte_col, Some(0));
        }
        ref intent => panic!("semantic user navigation collapsed to wrong intent: {intent:?}"),
    }
}

#[test]
fn resumed_sparse_top_scroll_pill_click_advances_without_extra_scroll() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    let (mut app, _dir) = resumed_heterogeneous_transcript_app(340, 78, 18);
    app.focus_prompt();
    app.render_silent();

    let target_before = previous_user_record_index(&app);
    let cursor_before = app.transcript_window().document_view_state().cursor;
    let top_win = app
        .ui_probe()
        .named_win("smelt.scroll_pills.top.win")
        .expect("visible sparse top scroll pill");
    let top_buf = app
        .ui_probe()
        .named_buf("smelt.scroll_pills.top.buf")
        .expect("sparse top scroll pill buffer");
    let label_before = app
        .ui_probe()
        .buf(top_buf)
        .and_then(|buf| buf.get_line(0))
        .expect("sparse top scroll pill label before click")
        .to_string();
    assert_eq!(parse_record_index(&label_before), Some(target_before));
    let pill_rect = app
        .ui_probe()
        .win(top_win)
        .and_then(|win| win.viewport)
        .expect("sparse top scroll pill viewport")
        .rect;

    app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        row: pill_rect.top,
        column: pill_rect.left.saturating_add(1),
        modifiers: KeyModifiers::empty(),
    })));
    app.render_silent();

    assert_eq!(app.state().app_focus, AppFocus::Prompt);
    assert_eq!(app.ui_probe().focus(), Some(crate::app::PROMPT_WIN));
    assert_eq!(
        app.transcript_window().document_view_state().cursor,
        cursor_before
    );
    let target_after = previous_user_record_index(&app);
    assert!(
        target_after < target_before,
        "top pill should advance after one click without a repairing scroll: before={target_before}, after={target_after}, lines={:?}",
        transcript_viewport_lines(&app)
    );
    let top_buf = app
        .ui_probe()
        .named_buf("smelt.scroll_pills.top.buf")
        .expect("top pill should remain available for the next previous-user jump");
    let label_after = app
        .ui_probe()
        .buf(top_buf)
        .and_then(|buf| buf.get_line(0))
        .expect("sparse top scroll pill label after click");
    assert_ne!(label_after, label_before);
    assert_eq!(parse_record_index(label_after), Some(target_after));
}

#[test]
fn transcript_previous_and_next_user_reveals_are_full_frame_semantic() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(340, 78, 18);
    app.set_transcript_scroll_trace_for_harness(true);
    app.take_transcript_scroll_trace_frames_for_harness();

    let cursor_before = app.transcript_window().document_view_state().cursor;
    let previous = reveal_user_block_via_lua(&mut app, "previous");
    assert_eq!(
        app.transcript_window().document_view_state().cursor.row,
        cursor_before.row,
        "pending semantic reveal exposed its cursor before projection"
    );
    app.render_silent();
    let frames = app.take_transcript_scroll_trace_frames_for_harness();
    let frame = frames.first().expect("previous user reveal frame");
    assert_reveal_block_frame(frame, &previous);
    let revealed_cursor = app.transcript_window().document_view_state().cursor;
    assert_eq!(revealed_cursor.row, frame.resolved_scroll_top);
    assert!(
        app.transcript_window()
            .materialized_rows()
            .is_some_and(|rows| rows.contains_abs_row(revealed_cursor.row)),
        "semantic reveal cursor escaped exact backing"
    );
    let lines = transcript_viewport_lines(&app);
    let previous_marker = previous
        .first_line
        .split_whitespace()
        .next()
        .expect("previous user marker");
    assert!(
        lines.iter().any(|line| line.contains(previous_marker)),
        "previous user target was not revealed in the viewport: target={:?}, lines={lines:?}, frame={frame:?}",
        previous.first_line
    );

    let older = reveal_user_block_via_lua(&mut app, "previous");
    app.render_silent();
    let frames = app.take_transcript_scroll_trace_frames_for_harness();
    let frame = frames.first().expect("older previous user reveal frame");
    assert_reveal_block_frame(frame, &older);
    let lines = transcript_viewport_lines(&app);
    let older_marker = older
        .first_line
        .split_whitespace()
        .next()
        .expect("older previous user marker");
    assert!(
        lines.iter().any(|line| line.contains(older_marker)),
        "second previous user target was not revealed in the viewport: target={:?}, lines={lines:?}",
        older.first_line
    );
    assert!(
        older.record_index < previous.record_index,
        "repeated previous-user reveal should walk backward by record identity: previous={previous:?}, older={older:?}"
    );

    app.submit_search(
        crate::app::TRANSCRIPT_WIN,
        SearchDirection::Forward,
        "record-0291".to_string(),
    );
    app.render_silent();
    app.take_transcript_scroll_trace_frames_for_harness();

    let next = reveal_user_block_via_lua(&mut app, "next");
    app.render_silent();
    let frames = app.take_transcript_scroll_trace_frames_for_harness();
    let frame = frames.first().expect("next user reveal frame");
    assert_reveal_block_frame(frame, &next);
    let lines = transcript_viewport_lines(&app);
    let next_marker = next
        .first_line
        .split_whitespace()
        .next()
        .expect("next user marker");
    assert!(
        lines.iter().any(|line| line.contains(next_marker)),
        "next user target was not revealed in the viewport: target={:?}, lines={lines:?}",
        next.first_line
    );
    assert_eq!(
        next_marker, "record-0300",
        "next user reveal should advance from the sparse search target: next={next:?}"
    );
}

fn semantic_viewport_anchor(
    frame: &TranscriptScrollTraceFrame,
) -> Option<(usize, smelt_core::transcript_model::BlockId, RowIndex)> {
    match frame.viewport_anchor_after {
        Some(TranscriptTraceAnchor::Content {
            record_index,
            block_id,
            row_offset,
            ..
        }) => Some((record_index, block_id, row_offset)),
        _ => None,
    }
}

fn assert_monotonic_visible_anchors(frames: &[TranscriptScrollTraceFrame], upward: bool) {
    let mut previous: Option<(usize, smelt_core::transcript_model::BlockId, RowIndex)> = None;
    let mut compared = 0;
    for frame in frames {
        let Some(current) = semantic_viewport_anchor(frame) else {
            continue;
        };
        if let Some(previous) = previous {
            let movement = if current.0 != previous.0 {
                Some(current.0.cmp(&previous.0))
            } else if current.1 == previous.1 {
                Some(current.2.cmp(&previous.2))
            } else {
                None
            };
            if let Some(movement) = movement {
                if upward {
                    assert!(
                        !movement.is_gt(),
                        "semantic viewport anchor moved down during upward replay: previous={previous:?}, current={current:?}, frame={frame:?}"
                    );
                } else {
                    assert!(
                        !movement.is_lt(),
                        "semantic viewport anchor moved up during downward replay: previous={previous:?}, current={current:?}, frame={frame:?}"
                    );
                }
                compared += 1;
            }
        }
        previous = Some(current);
    }
    assert!(
        compared > 0,
        "replay did not produce comparable semantic viewport anchors"
    );
}

fn assert_local_scroll_frames_are_exact_and_timed(frames: &[TranscriptScrollTraceFrame]) {
    assert!(!frames.is_empty(), "expected local scroll frames");
    for frame in frames {
        assert!(
            !frame.placeholder_rows_visible,
            "local scroll should not land in sparse placeholders: {frame:?}"
        );
        frame
            .render_or_projection_ms
            .expect("replay should enable projection timings");
        assert!(
            frame.first_visible_content_anchor.is_some(),
            "local scroll should resolve an exact visible content anchor: {frame:?}"
        );
    }
}

fn assert_user_delta_targets_exact_rows(frames: &[TranscriptScrollTraceFrame]) {
    assert!(!frames.is_empty(), "expected user delta frames");
    for frame in frames {
        let TranscriptScrollIntent::UserDelta { .. } = frame.scroll_intent else {
            continue;
        };
        assert!(
            frame.projection_target.exact_target_row().is_some(),
            "user delta should project through an exact local row: {frame:?}"
        );
    }
}

fn assert_user_delta_inputs_do_not_pre_scroll(frames: &[TranscriptScrollTraceFrame]) {
    assert!(!frames.is_empty(), "expected user delta frames");
    assert!(
        frames
            .iter()
            .filter(|frame| matches!(
                frame.scroll_intent,
                TranscriptScrollIntent::UserDelta { .. }
            ))
            .all(|frame| frame.window_scroll_after_input == frame.window_scroll_before),
        "local user delta mutated Window::scroll_top before transcript projection: {frames:?}"
    );
}

#[derive(Clone, Copy, Debug)]
enum TranscriptBurstKey {
    CtrlD,
    CtrlU,
    Down,
    Up,
}

impl TranscriptBurstKey {
    fn press(self, app: &mut TestApp) {
        match self {
            Self::CtrlD => app.press_mod(KeyCode::Char('d'), KeyModifiers::CONTROL),
            Self::CtrlU => app.press_mod(KeyCode::Char('u'), KeyModifiers::CONTROL),
            Self::Down => app.press(KeyCode::Down),
            Self::Up => app.press(KeyCode::Up),
        }
    }

    fn moves_down(self) -> bool {
        matches!(self, Self::CtrlD | Self::Down)
    }

    fn max_rows_per_event(self, viewport_rows: u16) -> RowIndex {
        match self {
            Self::CtrlD | Self::CtrlU => RowIndex::from(viewport_rows.max(1)),
            Self::Down | Self::Up => 1,
        }
    }
}

fn assert_burst_projection_delta_bounded(
    label: &str,
    frames: &[TranscriptScrollTraceFrame],
    before_scroll: RowIndex,
    max_delta: RowIndex,
) {
    let mut checked = 0;
    for frame in frames {
        let TranscriptScrollIntent::UserDelta { .. } = frame.scroll_intent else {
            continue;
        };
        let Some(target) = frame.projection_target.exact_target_row() else {
            panic!("{label} projected user delta through a non-exact target: {frame:?}");
        };
        let delta = target.abs_diff(before_scroll);
        assert!(
            delta <= max_delta,
            "{label} over-accumulated a no-render key burst: before_scroll={before_scroll}, target={target}, delta={delta}, max_delta={max_delta}, frame={frame:?}"
        );
        checked += 1;
    }
    assert!(
        checked > 0,
        "{label} produced no user-delta projection frames: {frames:?}"
    );
}

fn run_transcript_key_burst(app: &mut TestApp, label: &str, key: TranscriptBurstKey) {
    const BURST_EVENTS: usize = 80;

    let viewport_rows = app
        .transcript_window()
        .viewport
        .expect("transcript viewport")
        .rect
        .height
        .max(1);
    let before_scroll = app.transcript_window().scroll_top;
    app.set_transcript_scroll_trace_timings_for_harness(true);
    app.take_transcript_scroll_trace_frames_for_harness();

    for _ in 0..BURST_EVENTS {
        key.press(app);
    }

    let scroll_before_render = app.transcript_window().scroll_top;
    assert_eq!(
        scroll_before_render, before_scroll,
        "{label} mutated Window::scroll_top before projection"
    );

    app.render_silent();
    let frames = app.take_transcript_scroll_trace_frames_for_harness();
    assert_local_scroll_frames_are_exact_and_timed(&frames);
    assert_user_delta_targets_exact_rows(&frames);
    assert_user_delta_inputs_do_not_pre_scroll(&frames);
    let max_delta = key
        .max_rows_per_event(viewport_rows)
        .saturating_mul(BURST_EVENTS as RowIndex)
        .saturating_add(RowIndex::from(viewport_rows).saturating_mul(2));
    assert_burst_projection_delta_bounded(label, &frames, before_scroll, max_delta);

    let after_scroll = app.transcript_window().scroll_top;
    if key.moves_down() {
        assert!(
            after_scroll >= before_scroll,
            "{label} moved upward after a downward burst: before_scroll={before_scroll}, after_scroll={after_scroll}, frames={frames:?}"
        );
    } else {
        assert!(
            after_scroll <= before_scroll,
            "{label} moved downward after an upward burst: before_scroll={before_scroll}, after_scroll={after_scroll}, frames={frames:?}"
        );
    }
}

fn prepare_transcript_burst_app(app: &mut TestApp) {
    app.focus_transcript();
    app.configure_transcript_vim(true, VimMode::Normal);
}

fn jump_transcript_top(app: &mut TestApp) {
    app.type_char('g');
    app.type_char('g');
    app.render_silent();
    assert_eq!(
        app.transcript_window().scroll_top,
        0,
        "gg should reach the top before a key-repeat burst"
    );
}

fn jump_transcript_bottom(app: &mut TestApp) {
    app.type_char('G');
    settle_transcript_vim_tail(app);
}

fn jump_transcript_middle(app: &mut TestApp) {
    let record = app
        .conversation_probe()
        .transcript()
        .record_total_count()
        .expect("sparse record count")
        / 2;
    assert!(
        app.reveal_transcript_record_block(record, 1, true),
        "middle record reveal failed for record {record}"
    );
    app.render_silent();
    let scroll = app.transcript_window().scroll_top;
    assert!(
        scroll > 0,
        "middle reveal should leave the top: scroll={scroll}"
    );
}

fn assert_user_delta_record_coverage_moves_contiguously(frames: &[TranscriptScrollTraceFrame]) {
    let mut previous: Option<TranscriptRecordTraceRange> = None;
    for frame in frames {
        let TranscriptScrollIntent::UserDelta { .. } = frame.scroll_intent else {
            continue;
        };
        let Some(current) = frame.active_record_range_after else {
            continue;
        };
        if let Some(previous) = previous {
            assert!(
                current.start <= previous.end && previous.start <= current.end,
                "local user delta jumped to disjoint record coverage: previous={previous:?}, current={current:?}, frame={frame:?}"
            );
        }
        previous = Some(current);
    }
}

fn assert_preserve_frames_keep_semantic_anchor(frames: &[TranscriptScrollTraceFrame]) {
    let preserve_frames: Vec<_> = frames
        .iter()
        .filter(|frame| {
            matches!(
                frame.scroll_intent,
                TranscriptScrollIntent::PreserveViewport
                    | TranscriptScrollIntent::ResizeReflow { .. }
            )
        })
        .collect();
    assert!(
        !preserve_frames.is_empty(),
        "replay did not include preserve or resize frames"
    );
    let mut compared = 0;
    for frame in preserve_frames {
        let Some(TranscriptTraceAnchor::Content {
            record_index: before_record,
            block_id: before_block,
            ..
        }) = frame.viewport_anchor_before
        else {
            continue;
        };
        let Some(TranscriptTraceAnchor::Content {
            record_index: after_record,
            block_id: after_block,
            ..
        }) = frame.viewport_anchor_after
        else {
            panic!("preserve frame lost its semantic viewport anchor: {frame:?}");
        };
        assert_eq!(
            (after_record, after_block),
            (before_record, before_block),
            "preserve/resize frame moved to different visible block identity: {frame:?}"
        );
        compared += 1;
        frame
            .render_or_projection_ms
            .expect("replay should enable projection timings");
    }
    assert!(
        compared > 0,
        "replay preserve/resize frames did not expose comparable semantic anchors"
    );
}

#[test]
fn transcript_scroll_replay_covers_velocity_latency_and_sparse_scenarios() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(320, 78, 18);
    let replay = TranscriptScrollReplay::new(vec![
        TranscriptReplayStep::WheelUp { ticks: 8 },
        TranscriptReplayStep::CoalescedWheel { rows: -9 },
        TranscriptReplayStep::WheelDown { ticks: 4 },
        TranscriptReplayStep::DragAutoscrollTop { ticks: 3 },
        TranscriptReplayStep::WheelUpUntilRecordRangeChanges { max_ticks: 80 },
        TranscriptReplayStep::Resize {
            width: 70,
            height: 20,
        },
        TranscriptReplayStep::StreamingAppend,
        TranscriptReplayStep::DragAutoscrollBottom { ticks: 3 },
        TranscriptReplayStep::ScrollbarFarSeek { rel_row: 3 },
    ]);

    let report = replay.run(&mut app);
    assert!(!report.frames.is_empty(), "replay produced no trace frames");
    assert!(
        report.record_range_changed,
        "replay did not cover record-window replacement: {:?}",
        report.frames
    );
    assert!(
        report.frames.iter().any(|frame| matches!(
            frame.scroll_intent,
            TranscriptScrollIntent::ResizeReflow { .. }
        )),
        "replay did not cover resize/reflow"
    );
    assert!(
        report.frames.iter().any(|frame| matches!(
            frame.scroll_intent,
            TranscriptScrollIntent::ScrollbarFraction { .. }
        )),
        "replay did not cover scrollbar far seek"
    );
    assert!(
        report
            .frames
            .iter()
            .all(|frame| !frame.placeholder_rows_visible),
        "interactive replay exposed sparse transcript placeholders: {:?}",
        report.frames
    );

    let wheel_up_frames: Vec<_> = report
        .frames
        .iter()
        .filter(|frame| {
            frame.input_event_or_tick.starts_with("wheel_up:")
                || frame.input_event_or_tick.starts_with("coalesced_wheel")
        })
        .cloned()
        .collect();
    let wheel_probe_frames: Vec<_> = report
        .frames
        .iter()
        .filter(|frame| {
            frame
                .input_event_or_tick
                .starts_with("wheel_up_window_probe")
        })
        .cloned()
        .collect();
    let drag_top_frames: Vec<_> = report
        .frames
        .iter()
        .filter(|frame| frame.input_event_or_tick.starts_with("drag_autoscroll_top"))
        .cloned()
        .collect();
    let wheel_down_frames: Vec<_> = report
        .frames
        .iter()
        .filter(|frame| frame.input_event_or_tick.starts_with("wheel_down"))
        .cloned()
        .collect();
    let drag_bottom_frames: Vec<_> = report
        .frames
        .iter()
        .filter(|frame| {
            frame
                .input_event_or_tick
                .starts_with("drag_autoscroll_bottom")
        })
        .cloned()
        .collect();
    assert!(
        !wheel_up_frames.is_empty() && !drag_top_frames.is_empty() && !wheel_down_frames.is_empty() && !drag_bottom_frames.is_empty(),
        "replay missing local scroll frame groups: wheel_up={}, probe={}, drag_top={}, wheel_down={}, drag_bottom={}",
        wheel_up_frames.len(),
        wheel_probe_frames.len(),
        drag_top_frames.len(),
        wheel_down_frames.len(),
        drag_bottom_frames.len()
    );
    assert_local_scroll_frames_are_exact_and_timed(&wheel_up_frames);
    if !wheel_probe_frames.is_empty() {
        assert_local_scroll_frames_are_exact_and_timed(&wheel_probe_frames);
    }
    assert_local_scroll_frames_are_exact_and_timed(&drag_top_frames);
    assert_user_delta_record_coverage_moves_contiguously(&drag_top_frames);
    assert_local_scroll_frames_are_exact_and_timed(&wheel_down_frames);
    assert_local_scroll_frames_are_exact_and_timed(&drag_bottom_frames);
    assert_user_delta_record_coverage_moves_contiguously(&drag_bottom_frames);
    assert_user_delta_targets_exact_rows(&wheel_up_frames);
    assert_user_delta_record_coverage_moves_contiguously(&wheel_up_frames);
    if !wheel_probe_frames.is_empty() {
        assert_user_delta_targets_exact_rows(&wheel_probe_frames);
        assert_user_delta_record_coverage_moves_contiguously(&wheel_probe_frames);
    }
    assert_user_delta_targets_exact_rows(&wheel_down_frames);
    assert_user_delta_record_coverage_moves_contiguously(&wheel_down_frames);
    assert_preserve_frames_keep_semantic_anchor(&report.frames);
    assert_monotonic_visible_anchors(&wheel_up_frames, true);
    if !wheel_probe_frames.is_empty() {
        assert_monotonic_visible_anchors(&wheel_probe_frames, true);
    }
    assert_monotonic_visible_anchors(&drag_top_frames, true);
    assert_monotonic_visible_anchors(&wheel_down_frames, false);
    assert_monotonic_visible_anchors(&drag_bottom_frames, false);
}

#[test]
fn transcript_key_repeat_bursts_do_not_overaccumulate_sparse_local_motion() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(900, 78, 18);
    prepare_transcript_burst_app(&mut app);

    jump_transcript_top(&mut app);
    run_transcript_key_burst(&mut app, "top Ctrl-D burst", TranscriptBurstKey::CtrlD);
    jump_transcript_top(&mut app);
    run_transcript_key_burst(&mut app, "top Down-arrow burst", TranscriptBurstKey::Down);

    jump_transcript_bottom(&mut app);
    run_transcript_key_burst(&mut app, "bottom Ctrl-U burst", TranscriptBurstKey::CtrlU);
    jump_transcript_bottom(&mut app);
    run_transcript_key_burst(&mut app, "bottom Up-arrow burst", TranscriptBurstKey::Up);

    jump_transcript_middle(&mut app);
    run_transcript_key_burst(&mut app, "middle Ctrl-D burst", TranscriptBurstKey::CtrlD);
    jump_transcript_middle(&mut app);
    run_transcript_key_burst(
        &mut app,
        "middle Down-arrow burst",
        TranscriptBurstKey::Down,
    );
    jump_transcript_middle(&mut app);
    run_transcript_key_burst(&mut app, "middle Ctrl-U burst", TranscriptBurstKey::CtrlU);
    jump_transcript_middle(&mut app);
    run_transcript_key_burst(&mut app, "middle Up-arrow burst", TranscriptBurstKey::Up);
}

#[test]
fn transcript_jump_top_uses_first_record_anchor_from_sparse_tail() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(160, 78, 18);
    prepare_transcript_burst_app(&mut app);
    app.set_transcript_scroll_trace_for_harness(true);
    app.take_transcript_scroll_trace_frames_for_harness();

    app.type_char('g');
    app.type_char('g');
    app.render_silent();

    let frames = app.take_transcript_scroll_trace_frames_for_harness();
    assert!(
        frames.iter().any(|frame| matches!(
            frame.scroll_intent,
            TranscriptScrollIntent::RevealBlock {
                record_index: 0,
                row_offset: 0,
                screen_padding_top: 0,
                ..
            }
        )),
        "gg should project through the exact first transcript record: {frames:?}"
    );
    assert_eq!(
        first_visible_record_index(&app),
        Some(0),
        "gg did not reveal the first sparse transcript record"
    );
}

#[test]
fn transcript_jump_bottom_projects_sparse_semantic_tail() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.install_sparse_transcript_scroll_fixture(96, 40, 29);

    let mut frames = Vec::new();
    for _ in 0..12 {
        app.transcript_scroll_probe_command(TranscriptScrollProbeCommand::JumpBottom);
        app.render_silent();
        frames.extend(app.take_transcript_scroll_trace_frames_for_harness());
    }

    assert!(
        frames
            .iter()
            .any(|frame| matches!(frame.scroll_intent, TranscriptScrollIntent::Tail)),
        "G should project sparse transcript bottom jumps through the semantic tail: {frames:?}"
    );
    assert!(
        frames.iter().all(|frame| !matches!(
            frame.scroll_intent,
            TranscriptScrollIntent::ApproximateRowSeek(_)
                | TranscriptScrollIntent::ExactContentAnchor(TranscriptTraceAnchor::EstimatedRow(
                    _
                ))
        )),
        "G used an estimated row instead of the semantic transcript tail: {frames:?}"
    );
}

#[test]
fn transcript_drag_selection_command_does_not_pre_scroll_as_local_delta() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.install_sparse_transcript_scroll_fixture(96, 40, 26);

    app.transcript_scroll_probe_start_edge_drag(TranscriptScrollProbeEdge::Top);
    app.transcript_scroll_probe_render();
    app.transcript_scroll_probe_command(TranscriptScrollProbeCommand::MoveUp);
    app.transcript_scroll_probe_render();
}

#[test]
fn transcript_append_during_bottom_drag_preserves_sparse_anchor() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.install_sparse_transcript_scroll_fixture(512, 103, 13);

    app.transcript_scroll_probe_start_edge_drag(TranscriptScrollProbeEdge::Bottom);
    app.transcript_scroll_probe_render();
    app.transcript_scroll_probe_append(193);
    app.transcript_scroll_probe_render();
    app.transcript_scroll_probe_append(193);
    app.transcript_scroll_probe_render();
}

#[test]
fn transcript_resize_reflow_preserves_sparse_anchor_after_prefix_width_change() {
    let mut app = TestApp::builder().with_vim(true).build();
    app.install_sparse_transcript_scroll_fixture(256, 40, 36);

    app.transcript_scroll_probe_render();
    app.transcript_scroll_probe_render();
    app.transcript_scroll_probe_start_edge_drag(TranscriptScrollProbeEdge::Bottom);
    app.transcript_scroll_probe_render();

    app.set_terminal_size(95, 39);
    app.render_silent();

    let frames = app.take_transcript_scroll_trace_frames_for_harness();
    let frame = frames
        .iter()
        .find(|frame| {
            matches!(
                frame.scroll_intent,
                TranscriptScrollIntent::ResizeReflow { .. }
            )
        })
        .expect("resize/reflow trace frame");
    let Some(TranscriptTraceAnchor::Content {
        record_index: before_anchor_record,
        block_id: before_block,
        ..
    }) = frame.viewport_anchor_before
    else {
        panic!("resize should start from a content anchor: {frame:?}");
    };
    let Some(TranscriptTraceAnchor::Content {
        record_index: after_anchor_record,
        block_id: after_block,
        ..
    }) = frame.viewport_anchor_after
    else {
        panic!("resize should preserve a content anchor: {frame:?}");
    };
    assert_eq!(
        (after_anchor_record, after_block),
        (before_anchor_record, before_block),
        "resize/reflow moved to a different sparse transcript block: {frame:?}"
    );
}

#[test]
fn transcript_gg_then_key_repeat_burst_without_intermediate_render_uses_top_base() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(900, 78, 18);
    prepare_transcript_burst_app(&mut app);
    jump_transcript_bottom(&mut app);

    app.set_transcript_scroll_trace_timings_for_harness(true);
    app.take_transcript_scroll_trace_frames_for_harness();
    let committed_scroll_before = app.transcript_window().scroll_top;
    app.type_char('g');
    app.type_char('g');
    let extent_reads = app
        .app
        .conversation
        .transcript_extent_store_read_count_for_harness();
    for _ in 0..80 {
        TranscriptBurstKey::CtrlD.press(&mut app);
    }
    assert_eq!(
        app.transcript_window().scroll_top,
        committed_scroll_before,
        "pending gg and Ctrl-D inputs changed the exact committed viewport before rendering"
    );

    app.render_silent();
    assert_eq!(
        app.app
            .conversation
            .transcript_extent_store_read_count_for_harness(),
        extent_reads,
        "queued local motion after gg read the source-space extent index"
    );
    let frames = app.take_transcript_scroll_trace_frames_for_harness();
    assert_local_scroll_frames_are_exact_and_timed(&frames);
    assert_user_delta_targets_exact_rows(&frames);
    assert_user_delta_inputs_do_not_pre_scroll(&frames);
    let viewport_rows = app
        .transcript_window()
        .viewport
        .expect("transcript viewport")
        .rect
        .height
        .max(1);
    let max_delta = RowIndex::from(viewport_rows)
        .saturating_mul(80)
        .saturating_add(RowIndex::from(viewport_rows).saturating_mul(2));
    assert_burst_projection_delta_bounded("gg then Ctrl-D burst", &frames, 0, max_delta);
}

#[test]
fn transcript_drag_autoscroll_top_crosses_sparse_windows_without_teleport() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(900, 78, 18);
    app.set_transcript_scroll_trace_timings_for_harness(true);
    app.take_transcript_scroll_trace_frames_for_harness();

    let initial_record = first_visible_record_index(&app).expect("initial visible record");
    start_transcript_edge_drag(&mut app, TranscriptDragEdge::Top);
    let mut frames = Vec::new();
    for tick in 0..900 {
        let before = transcript_viewport_lines(&app);
        if app.tick_drag_autoscroll_with_transcript_intent() {
            app.render_silent();
            let after = transcript_viewport_lines(&app);
            assert_viewport_shifted_up_rows(tick, &before, &after, 1);
            frames.extend(app.take_transcript_scroll_trace_frames_for_harness());
        }
    }
    finish_transcript_drag(&mut app);

    let final_record = first_visible_record_index(&app).expect("final visible record");
    assert!(
        final_record.saturating_add(100) < initial_record,
        "drag autoscroll did not move through older sparse content: initial_record={initial_record}, final_record={final_record}, lines={:?}",
        transcript_viewport_lines(&app)
    );
    assert_local_scroll_frames_are_exact_and_timed(&frames);
    assert_monotonic_visible_anchors(&frames, true);
}

#[test]
fn transcript_drag_autoscroll_bottom_crosses_sparse_windows_without_locking() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(900, 78, 18);
    assert!(app.reveal_transcript_record_block(120, 1, true));
    app.render_silent();
    app.set_transcript_scroll_trace_timings_for_harness(true);
    app.take_transcript_scroll_trace_frames_for_harness();

    let initial_record = first_visible_record_index(&app).expect("initial visible record");
    assert!(
        initial_record < 200,
        "test must start high in sparse content, got record {initial_record}: {:?}",
        transcript_viewport_lines(&app)
    );
    start_transcript_edge_drag(&mut app, TranscriptDragEdge::Bottom);
    let mut frames = Vec::new();
    let mut latest = initial_record;
    for tick in 0..320 {
        let before = transcript_viewport_lines(&app);
        assert!(
            app.tick_drag_autoscroll_with_transcript_intent(),
            "bottom-edge drag tick {tick} stopped before reaching newer content: first={:?}, last={:?}, scroll_top={}, lines={:?}",
            first_visible_record_index(&app),
            last_visible_record_index(&app),
            app.transcript_window().scroll_top,
            transcript_viewport_lines(&app)
        );
        app.render_silent();
        let after = transcript_viewport_lines(&app);
        assert_viewport_shifted_down_rows(tick, &before, &after, 1);
        frames.extend(app.take_transcript_scroll_trace_frames_for_harness());
        let current = first_visible_record_index(&app).unwrap_or_else(|| {
            panic!(
                "bottom-edge drag tick {tick} rendered no visible record marker: lines={:?}",
                transcript_viewport_lines(&app)
            )
        });
        assert!(
            current >= latest,
            "bottom-edge drag moved backward: latest={latest}, current={current}, lines={:?}",
            transcript_viewport_lines(&app)
        );
        latest = current;
    }
    finish_transcript_drag(&mut app);

    let final_record = first_visible_record_index(&app).expect("final visible record");
    assert!(
        final_record > initial_record.saturating_add(40),
        "bottom-edge drag autoscroll did not advance through newer sparse content: initial_record={initial_record}, final_record={final_record}, lines={:?}",
        transcript_viewport_lines(&app)
    );
    assert!(
        frames.len() >= 300,
        "bottom-edge drag produced too few frames before locking: {} frames",
        frames.len()
    );
    assert_local_scroll_frames_are_exact_and_timed(&frames);
    assert_monotonic_visible_anchors(&frames, false);
    assert_user_delta_targets_exact_rows(&frames);
    assert_user_delta_inputs_do_not_pre_scroll(&frames);
    assert_user_delta_record_coverage_moves_contiguously(&frames);
}

#[test]
fn transcript_drag_autoscroll_bottom_stops_at_real_bottom() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(900, 78, 18);
    prepare_transcript_burst_app(&mut app);
    jump_transcript_bottom(&mut app);
    let viewport_rows = app
        .transcript_window()
        .viewport
        .expect("transcript viewport")
        .rect
        .height
        .max(1);
    let state = app.transcript_window().document_view_state();
    let max_scroll = state
        .materialized
        .total_rows
        .saturating_sub(RowIndex::from(viewport_rows));
    assert!(
        app.transcript_window().scroll_top >= max_scroll,
        "G should reach the real transcript bottom before testing drag boundary: scroll={}, max_scroll={max_scroll}, state={state:?}",
        app.transcript_window().scroll_top,
    );

    start_transcript_edge_drag(&mut app, TranscriptDragEdge::Bottom);
    assert!(
        !app.tick_drag_autoscroll_with_transcript_intent(),
        "bottom-edge drag autoscroll should stop when the resolved viewport is already at the real bottom"
    );
    finish_transcript_drag(&mut app);
}

#[test]
fn transcript_drag_autoscroll_bottom_no_input_renders_do_not_undo_ticks() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(900, 78, 18);
    assert!(app.reveal_transcript_record_block(120, 1, true));
    app.render_silent();
    app.set_transcript_scroll_trace_timings_for_harness(true);
    app.take_transcript_scroll_trace_frames_for_harness();

    start_transcript_edge_drag(&mut app, TranscriptDragEdge::Bottom);
    let mut frames = Vec::new();
    for tick in 0..80 {
        let before_scroll = app.transcript_window().scroll_top;
        assert!(
            app.tick_drag_autoscroll_with_transcript_intent(),
            "bottom-edge drag tick {tick} stopped before a real boundary"
        );
        app.render_silent();
        let after_tick_scroll = app.transcript_window().scroll_top;
        let after_tick_lines = transcript_viewport_lines(&app);
        frames.extend(app.take_transcript_scroll_trace_frames_for_harness());

        app.render_silent();
        let after_idle_scroll = app.transcript_window().scroll_top;
        let after_idle_lines = transcript_viewport_lines(&app);
        frames.extend(app.take_transcript_scroll_trace_frames_for_harness());
        assert_eq!(
            after_idle_scroll, after_tick_scroll,
            "no-input render after bottom-edge drag tick {tick} changed the resolved scroll row: before_scroll={before_scroll}, after_tick_scroll={after_tick_scroll}, after_idle_scroll={after_idle_scroll}, frames={frames:?}"
        );
        assert_eq!(
            after_idle_lines, after_tick_lines,
            "no-input render after bottom-edge drag tick {tick} changed visible content: before_scroll={before_scroll}, after_tick_scroll={after_tick_scroll}, after_idle_scroll={after_idle_scroll}, frames={frames:?}"
        );
    }
    finish_transcript_drag(&mut app);

    assert_local_scroll_frames_are_exact_and_timed(&frames);
    assert_user_delta_inputs_do_not_pre_scroll(&frames);
}

#[test]
fn transcript_cursor_down_inside_viewport_moves_one_row_without_scrolling() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(900, 78, 18);
    assert!(app.reveal_transcript_record_block(120, 1, true));
    app.render_silent();
    app.configure_transcript_vim(true, crate::smelt_edit::VimMode::Normal);

    let viewport_rows = app
        .transcript_window()
        .viewport
        .map(|viewport| viewport.rect.height)
        .expect("transcript viewport");
    let (click_row, click_col) = transcript_content_point(&app, 2);
    let mouse = crossterm::event::MouseEvent {
        kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        row: click_row,
        column: click_col,
        modifiers: KeyModifiers::empty(),
    };
    let pos = app
        .document_view_position_at_mouse_for_win(crate::app::TRANSCRIPT_WIN, mouse)
        .expect("transcript cursor position");
    let mut state = app.transcript_window().document_view_state();
    state.cursor = pos;
    state.drag_endpoint = None;
    state.selection_anchor = None;
    app.set_transcript_document_view(state);
    app.render_silent();

    for step in 0..4 {
        let before_lines = transcript_viewport_lines(&app);
        let before = app.transcript_window().document_view_state();
        let before_scroll = app.transcript_window().scroll_top;
        let now = app.core_probe().clock.instant_now();
        app.execute_document_view_command_for_win(
            crate::app::TRANSCRIPT_WIN,
            crate::smelt_edit::DocumentCommand::MoveRows(1),
            viewport_rows,
            now,
        );
        app.render_silent();
        let after_lines = transcript_viewport_lines(&app);
        let after = app.transcript_window().document_view_state();
        let after_scroll = app.transcript_window().scroll_top;
        assert_eq!(
            after_scroll, before_scroll,
            "cursor-down step {step} scrolled before the cursor reached the edge: before_lines={before_lines:?}, after_lines={after_lines:?}"
        );
        assert_eq!(
            after.cursor.row,
            before.cursor.row.saturating_add(1),
            "cursor-down step {step} did not move exactly one row inside the viewport: before={:?}, after={:?}, lines={after_lines:?}",
            before.cursor,
            after.cursor
        );
        assert_eq!(
            after_lines, before_lines,
            "cursor-down step {step} changed visible content before the cursor reached the edge"
        );
    }
}

#[test]
fn transcript_cursor_down_at_lower_edge_moves_one_visible_row_per_step() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(900, 78, 18);
    assert!(app.reveal_transcript_record_block(120, 1, true));
    app.render_silent();
    app.configure_transcript_vim(true, crate::smelt_edit::VimMode::Normal);

    let viewport_rows = app
        .transcript_window()
        .viewport
        .map(|viewport| viewport.rect.height)
        .expect("transcript viewport");
    let lower_edge = viewport_rows.saturating_sub(1);
    let (click_row, click_col) = transcript_content_point(&app, lower_edge);
    let mouse = crossterm::event::MouseEvent {
        kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        row: click_row,
        column: click_col,
        modifiers: KeyModifiers::empty(),
    };
    let pos = app
        .document_view_position_at_mouse_for_win(crate::app::TRANSCRIPT_WIN, mouse)
        .expect("transcript cursor position");
    let mut state = app.transcript_window().document_view_state();
    state.cursor = pos;
    state.drag_endpoint = None;
    state.selection_anchor = None;
    app.set_transcript_document_view(state);
    app.render_silent();
    app.set_transcript_scroll_trace_timings_for_harness(true);
    app.take_transcript_scroll_trace_frames_for_harness();

    let mut frames = Vec::new();
    for step in 0..160 {
        let before_lines = transcript_viewport_lines(&app);
        let before = app.transcript_window().document_view_state();
        let before_scroll = app.transcript_window().scroll_top;
        let before_screen_row = before.cursor.row.saturating_sub(before_scroll);
        assert_eq!(
            before_screen_row, lower_edge as u64,
            "cursor must stay parked at the lower edge before step {step}: cursor={:?}, scroll_top={before_scroll}, lines={before_lines:?}",
            before.cursor
        );

        let now = app.core_probe().clock.instant_now();
        app.execute_document_view_command_for_win(
            crate::app::TRANSCRIPT_WIN,
            crate::smelt_edit::DocumentCommand::MoveRows(1),
            viewport_rows,
            now,
        );
        app.render_silent();
        frames.extend(app.take_transcript_scroll_trace_frames_for_harness());

        let after_lines = transcript_viewport_lines(&app);
        let after = app.transcript_window().document_view_state();
        let after_scroll = app.transcript_window().scroll_top;
        let after_screen_row = after.cursor.row.saturating_sub(after_scroll);
        assert_eq!(
            after_screen_row, before_screen_row,
            "cursor-down step {step} moved the cursor away from the lower edge: before_cursor={:?}, after_cursor={:?}, before_scroll={before_scroll}, after_scroll={after_scroll}, after_lines={after_lines:?}",
            before.cursor, after.cursor
        );
        assert!(
            after.cursor.row >= after_scroll
                && after.cursor.row < after_scroll.saturating_add(u64::from(viewport_rows)),
            "cursor-down step {step} left the cursor outside the resolved viewport: after_cursor={:?}, after_scroll={after_scroll}, after_lines={after_lines:?}",
            after.cursor
        );
        assert_viewport_shifted_down_one_row(step, &before_lines, &after_lines);
    }

    assert_local_scroll_frames_are_exact_and_timed(&frames);
    assert_user_delta_inputs_do_not_pre_scroll(&frames);
    assert!(
        frames.iter().all(|frame| !matches!(
            frame.scroll_intent,
            TranscriptScrollIntent::ExactContentAnchor(TranscriptTraceAnchor::EstimatedRow(_))
        )),
        "cursor-down lower-edge movement fell back to estimated exact-row anchors: {frames:?}"
    );
    assert_user_delta_record_coverage_moves_contiguously(&frames);
}

#[test]
fn transcript_cursor_down_scroll_crosses_sparse_windows_without_locking() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(900, 78, 18);
    app.configure_transcript_vim(true, crate::smelt_edit::VimMode::Normal);
    app.set_transcript_scroll_trace_for_harness(true);
    for _ in 0..180 {
        wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollUp);
        app.render_silent();
        app.take_transcript_scroll_trace_frames_for_harness();
    }
    let start = first_visible_record_index(&app).expect("initial visible record");
    let start_row = app.transcript_window().scroll_top;
    let (click_row, click_col) = transcript_content_point(&app, 1);
    let mouse = crossterm::event::MouseEvent {
        kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        row: click_row,
        column: click_col,
        modifiers: KeyModifiers::empty(),
    };
    let pos = app
        .document_view_position_at_mouse_for_win(crate::app::TRANSCRIPT_WIN, mouse)
        .expect("transcript cursor position");
    let mut state = app.transcript_window().document_view_state();
    state.cursor = pos;
    state.drag_endpoint = None;
    state.selection_anchor = None;
    app.set_transcript_document_view(state);
    app.render_silent();
    app.take_transcript_scroll_trace_frames_for_harness();
    let mut latest = start;
    let mut frames = Vec::new();

    for step in 0..220 {
        let viewport_rows = app
            .transcript_window()
            .viewport
            .map(|viewport| viewport.rect.height)
            .expect("transcript viewport");
        let now = app.core_probe().clock.instant_now();
        app.execute_document_view_command_for_win(
            crate::app::TRANSCRIPT_WIN,
            crate::smelt_edit::DocumentCommand::MoveRows(1),
            viewport_rows,
            now,
        );
        app.render_silent();
        frames.extend(app.take_transcript_scroll_trace_frames_for_harness());
        let current = first_visible_record_index(&app).unwrap_or_else(|| {
            panic!(
                "cursor-down step {step} rendered no visible record marker: lines={:?}",
                transcript_viewport_lines(&app)
            )
        });
        assert!(
            current >= latest,
            "cursor-down scroll moved backward: latest={latest}, current={current}, lines={:?}",
            transcript_viewport_lines(&app)
        );
        latest = current;
    }

    let final_scroll = app.transcript_window().scroll_top;
    let final_cursor = app.transcript_window().document_view_state().cursor;
    assert!(
        final_scroll > start_row.saturating_add(40),
        "cursor-down scrolling locked before advancing the viewport: start_row={start_row}, final_scroll={final_scroll}, final_cursor={final_cursor:?}, lines={:?}",
        transcript_viewport_lines(&app)
    );
    assert!(
        frames.iter().any(|frame| matches!(
            frame.scroll_intent,
            TranscriptScrollIntent::UserDelta { rows } if rows > 0
        )),
        "cursor-down document scrolling did not produce semantic UserDelta frames: {frames:?}"
    );
    assert!(
        frames.iter().all(|frame| !matches!(
            frame.scroll_intent,
            TranscriptScrollIntent::ExactContentAnchor(TranscriptTraceAnchor::EstimatedRow(_))
        )),
        "cursor-down document scrolling fell back to estimated exact-row anchors: {frames:?}"
    );
    assert_monotonic_visible_anchors(&frames, false);
    assert_user_delta_record_coverage_moves_contiguously(&frames);
}

#[test]
fn transcript_scrollbar_click_preserves_fraction_intent() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(160, 78, 18);
    app.set_transcript_scroll_trace_for_harness(true);
    app.take_transcript_scroll_trace_frames_for_harness();

    let (row, column, _, _, _, _) = transcript_scrollbar_point(&app, 3);
    app.feed_one(SourceEvent::Term(Event::Mouse(
        crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            row,
            column,
            modifiers: KeyModifiers::empty(),
        },
    )));
    app.render_silent();
    let frames = app.take_transcript_scroll_trace_frames_for_harness();
    let frame = frames.first().expect("scrollbar render frame");

    assert!(
        matches!(
            frame.scroll_intent,
            TranscriptScrollIntent::ScrollbarFraction { .. }
        ),
        "scrollbar clicks must preserve a fraction intent instead of collapsing to {:?}",
        frame.scroll_intent
    );
}

#[test]
fn resumed_transcript_scrollbar_drag_is_continuous_through_exact_boundaries() {
    let mut app = TestApp::builder().build();
    app.install_sparse_transcript_scroll_fixture(900, 100, 40);
    app.set_terminal_size(100, 100);
    app.render_silent();
    app.take_transcript_scroll_trace_frames_for_harness();

    let viewport = app
        .transcript_window()
        .viewport
        .expect("transcript viewport");
    assert_eq!(viewport.rect.height, 95, "regression fixture viewport");
    let scrollbar = viewport.scrollbar.expect("transcript scrollbar");
    let initial_metrics = scrollbar.metrics(app.transcript_window().scroll_top);
    let start_rel_row = initial_metrics.thumb_top;
    let frozen_total_rows = scrollbar.total_rows;
    let mouse = |kind, rel_row: u16| {
        SourceEvent::Term(Event::Mouse(crossterm::event::MouseEvent {
            kind,
            row: viewport.rect.top.saturating_add(rel_row),
            column: scrollbar.col,
            modifiers: KeyModifiers::empty(),
        }))
    };

    app.feed_one(mouse(
        crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        start_rel_row,
    ));
    app.render_silent();
    app.take_transcript_scroll_trace_frames_for_harness();

    let mut previous_pointer = None;
    let mut previous_thumb = None;
    for pointer_row in [73, 72, 71, 70, 69] {
        app.feed_one(mouse(
            crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            pointer_row,
        ));
        app.render_silent();

        let frames = app.take_transcript_scroll_trace_frames_for_harness();
        let frame = frames.last().expect("scrollbar drag frame");
        let window = app.transcript_window();
        let current_viewport = window.viewport.expect("current transcript viewport");
        let current_scrollbar = current_viewport.scrollbar.expect("current scrollbar");
        let thumb_top = current_scrollbar.metrics(window.scroll_top).thumb_top;

        assert!(
            matches!(
                frame.scroll_intent,
                TranscriptScrollIntent::ScrollbarFraction { .. }
            ),
            "scrollbar drag collapsed frozen source geometry to a numeric row: {frame:?}"
        );
        assert!(
            matches!(
                frame.projection_target,
                crate::app::transcript_scroll_trace::TranscriptProjectionTargetTrace::StableRowDelta {
                    delta: 0,
                    ..
                }
            ),
            "scrollbar drag did not resolve through an exact semantic anchor: {frame:?}"
        );
        assert_eq!(
            current_scrollbar.total_rows, frozen_total_rows,
            "scrollbar extent changed during frozen drag geometry: {frame:?}"
        );
        assert!(
            thumb_top.abs_diff(pointer_row) <= 1,
            "painted thumb left the pointer at exact-tape boundary: pointer={pointer_row}, thumb={thumb_top}, frame={frame:?}"
        );
        if let (Some(previous_pointer), Some(previous_thumb)) = (previous_pointer, previous_thumb) {
            assert_eq!(previous_pointer - pointer_row, 1);
            assert!(
                thumb_top <= previous_thumb && previous_thumb.abs_diff(thumb_top) <= 1,
                "one-cell drag caused a discontinuous thumb jump: previous={previous_thumb}, current={thumb_top}, frame={frame:?}"
            );
        }
        assert!(
            !frame.placeholder_rows_visible,
            "committed scrollbar content must be exact: {frame:?}"
        );
        assert!(
            frame.first_visible_content_anchor.is_some()
                && frame.last_visible_content_anchor.is_some(),
            "committed scrollbar viewport lacks exact semantic anchors: {frame:?}"
        );
        assert!(
            transcript_viewport_lines(&app)
                .iter()
                .any(|line| line.contains("record-")),
            "committed scrollbar viewport exposed no semantic transcript content: {frame:?}"
        );
        let materialized_rows = frame
            .materialized_range
            .end
            .saturating_sub(frame.materialized_range.start);
        assert!(
            materialized_rows <= u64::from(viewport.rect.height).saturating_mul(3),
            "scrollbar seek materialized an unbounded row range: {frame:?}"
        );
        let active = frame
            .active_record_range_after
            .expect("semantic seek active record range");
        assert!(
            active.end.saturating_sub(active.start) <= 512,
            "scrollbar seek activated an unbounded source window: {frame:?}"
        );

        previous_pointer = Some(pointer_row);
        previous_thumb = Some(thumb_top);
    }

    let scroll_before_release = app.transcript_window().scroll_top;
    let thumb_before_release = previous_thumb.expect("dragged thumb");
    app.feed_one(mouse(
        crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
        69,
    ));
    app.render_silent();
    let window = app.transcript_window();
    let release_scrollbar = window
        .viewport
        .and_then(|viewport| viewport.scrollbar)
        .expect("release scrollbar");
    assert_eq!(
        window.scroll_top, scroll_before_release,
        "release snapped viewport"
    );
    assert_eq!(
        release_scrollbar.metrics(window.scroll_top).thumb_top,
        thumb_before_release,
        "release snapped scrollbar thumb"
    );
}

#[test]
fn scrollbar_drag_retains_exact_viewport_while_far_seek_hydrates() {
    let mut app = TestApp::builder().build();
    app.install_sparse_transcript_scroll_fixture(900, 100, 40);
    app.set_terminal_size(100, 100);
    app.render_silent();
    app.take_transcript_scroll_trace_frames_for_harness();

    let viewport = app
        .transcript_window()
        .viewport
        .expect("transcript viewport");
    let scrollbar = viewport.scrollbar.expect("transcript scrollbar");
    let start_rel_row = scrollbar
        .metrics(app.transcript_window().scroll_top)
        .thumb_top;
    let mouse = |kind, rel_row: u16| {
        SourceEvent::Term(Event::Mouse(crossterm::event::MouseEvent {
            kind,
            row: viewport.rect.top.saturating_add(rel_row),
            column: scrollbar.col,
            modifiers: KeyModifiers::empty(),
        }))
    };
    let assert_exact_coverage = |app: &TestApp| {
        let window = app.transcript_window();
        let materialized = window
            .materialized_rows()
            .expect("transcript materialized rows");
        assert!(
            materialized.covers_viewport(window.scroll_top, viewport.rect.height),
            "visible transcript range {:?} escaped exact backing {:?}",
            materialized.viewport_range(window.scroll_top, viewport.rect.height),
            materialized.materialized_range(),
        );
    };

    app.feed_one(mouse(
        crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        start_rel_row,
    ));
    app.render_silent();
    app.feed_one(mouse(
        crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left),
        50,
    ));
    app.render_silent();
    app.take_transcript_scroll_trace_frames_for_harness();

    let committed_scroll_top = app.transcript_window().scroll_top;
    let committed_lines = transcript_viewport_lines(&app);
    assert!(
        committed_lines.iter().any(|line| line.contains("record-")),
        "settled first seek must display exact transcript content"
    );
    assert_exact_coverage(&app);

    for pointer_row in [73, 72, 71] {
        app.feed_one(mouse(
            crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            pointer_row,
        ));
        let frame = app.render_unsettled_silent();
        let transcript_rows =
            &frame.rows[usize::from(viewport.rect.top)..usize::from(viewport.rect.bottom())];
        assert!(
            transcript_rows.iter().any(|line| line.contains("record-")),
            "pending far seek painted an empty transcript frame: {transcript_rows:?}"
        );

        assert!(
            app.app.conversation.transcript_hydration_is_pending(),
            "regression fixture must keep the far seek asynchronous"
        );
        let pending_window = app.transcript_window();
        let pending_scrollbar = pending_window
            .viewport
            .and_then(|viewport| viewport.scrollbar)
            .expect("pending transcript scrollbar");
        let pending_thumb = pending_scrollbar
            .display_metrics(pending_window.scroll_top)
            .thumb_top;
        assert!(
            pending_thumb.abs_diff(pointer_row) <= 1,
            "pending scrollbar thumb left the pointer: pointer={pointer_row}, thumb={pending_thumb}"
        );
        assert_eq!(
            app.transcript_window().scroll_top,
            committed_scroll_top,
            "pending far seek changed the committed viewport"
        );
        assert_eq!(
            transcript_viewport_lines(&app),
            committed_lines,
            "pending far seek exposed rows outside the committed exact viewport"
        );
        assert_exact_coverage(&app);
    }

    app.feed_one(mouse(
        crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
        71,
    ));
    let released_frame = app.render_unsettled_silent();
    let released_rows =
        &released_frame.rows[usize::from(viewport.rect.top)..usize::from(viewport.rect.bottom())];
    assert!(
        released_rows.iter().any(|line| line.contains("record-")),
        "releasing a pending seek exposed an empty transcript frame: {released_rows:?}"
    );
    assert_eq!(app.transcript_window().scroll_top, committed_scroll_top);
    assert_exact_coverage(&app);

    app.render_silent();
    let frames = app.take_transcript_scroll_trace_frames_for_harness();
    assert!(
        frames.iter().all(|frame| !frame.placeholder_rows_visible),
        "settled scrollbar seek exposed sparse placeholders: {frames:?}"
    );
    assert!(
        transcript_viewport_lines(&app)
            .iter()
            .any(|line| line.contains("record-")),
        "settled far seek must display exact transcript content"
    );
    let settled_window = app.transcript_window();
    let settled_thumb = settled_window
        .viewport
        .and_then(|viewport| viewport.scrollbar)
        .expect("settled transcript scrollbar")
        .display_metrics(settled_window.scroll_top)
        .thumb_top;
    assert!(
        settled_thumb.abs_diff(71) <= 1,
        "stale hydration replaced the latest drag target: thumb={settled_thumb}"
    );
    assert_exact_coverage(&app);
}

#[test]
fn transcript_scroll_search_jump_preserves_semantic_scroll_intent() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(160, 78, 18);
    app.set_transcript_scroll_trace_for_harness(true);
    app.take_transcript_scroll_trace_frames_for_harness();

    app.submit_search(
        crate::app::TRANSCRIPT_WIN,
        SearchDirection::Forward,
        "record-0150".to_string(),
    );
    app.render_silent();
    let frames = app.take_transcript_scroll_trace_frames_for_harness();
    let frame = frames.first().expect("search jump render frame");

    assert!(
        matches!(
            frame.scroll_intent,
            TranscriptScrollIntent::SearchJump {
                anchor: crate::app::transcript::TranscriptSearchAnchor::Content(_),
                ..
            }
        ),
        "search jumps must preserve a semantic content anchor instead of collapsing to {:?}",
        frame.scroll_intent
    );
    assert!(
        !frame.placeholder_rows_visible,
        "search reveal must not expose sparse placeholders: {frame:?}"
    );
    assert!(
        frame.first_visible_content_anchor.is_some(),
        "search reveal must resolve exact visible content: {frame:?}"
    );
    assert!(
        transcript_viewport_lines(&app)
            .iter()
            .any(|line| line.contains("record-0150")),
        "search reveal did not place the matched record in the viewport: {:?}",
        transcript_viewport_lines(&app)
    );
}

#[test]
fn transcript_search_jump_keeps_cursor_on_heterogeneous_match_start_after_render() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(180, 78, 18);

    app.submit_search(
        crate::app::TRANSCRIPT_WIN,
        SearchDirection::Forward,
        "assistant paragraph".to_string(),
    );
    app.render_silent();

    let range = app
        .overlays_probe()
        .search_session()
        .and_then(|session| session.current_range())
        .and_then(|range| range.rows())
        .expect("heterogeneous search match");
    let cursor = app
        .transcript_window()
        .row_cursor()
        .expect("transcript cursor");
    let cursor_row = app
        .transcript_rows_and_breaks_range(cursor.row, 1)
        .into_text_rows()
        .pop()
        .unwrap_or_default();
    assert_eq!(
        cursor,
        range.start,
        "cursor_row={cursor_row:?}, viewport={:?}",
        transcript_viewport_lines(&app)
    );
    assert_eq!(
        smelt_buffer::text::slice(&cursor_row, cursor.byte_col..range.end.byte_col),
        "assistant paragraph"
    );
}

#[test]
fn transcript_wheel_preserves_user_delta_intent() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(160, 78, 18);
    app.set_transcript_scroll_trace_for_harness(true);
    app.take_transcript_scroll_trace_frames_for_harness();

    wheel_transcript(&mut app, crossterm::event::MouseEventKind::ScrollUp);
    app.render_silent();
    let frames = app.take_transcript_scroll_trace_frames_for_harness();
    let frame = frames.first().expect("wheel render frame");

    assert_eq!(
        frame.scroll_intent,
        TranscriptScrollIntent::UserDelta { rows: -3 },
        "wheel replay must preserve the semantic user delta instead of collapsing to {:?}",
        frame.scroll_intent
    );
    assert_eq!(
        frame.window_scroll_after_input, frame.window_scroll_before,
        "transcript wheel input must not mutate Window::scroll_top before projection: {frame:?}"
    );
}

#[test]
fn transcript_drag_autoscroll_preserves_user_delta_intent() {
    let (mut app, _dir) = resumed_heterogeneous_transcript_app(160, 78, 18);
    app.set_transcript_scroll_trace_for_harness(true);
    app.take_transcript_scroll_trace_frames_for_harness();

    start_transcript_edge_drag(&mut app, TranscriptDragEdge::Top);
    assert!(
        app.tick_drag_autoscroll_with_transcript_intent(),
        "transcript edge drag should request a semantic scroll tick"
    );
    app.render_silent();
    let frames = app.take_transcript_scroll_trace_frames_for_harness();
    let frame = frames.first().expect("drag autoscroll render frame");
    let state = app.transcript_window().document_view_state();

    assert_eq!(
        frame.scroll_intent,
        TranscriptScrollIntent::UserDelta { rows: -1 },
        "drag autoscroll must preserve the semantic user delta instead of collapsing to {:?}",
        frame.scroll_intent
    );
    assert_eq!(
        frame.window_scroll_after_input, frame.window_scroll_before,
        "transcript drag autoscroll must not mutate Window::scroll_top before projection: {frame:?}"
    );
    assert_eq!(
        state.drag_endpoint.map(|pos| pos.row),
        Some(frame.resolved_scroll_top),
        "top-edge drag endpoint should advance to the projected leading row"
    );
}

#[test]
fn resumed_grouped_tail_keeps_warm_projection_viewport_bounded() {
    let viewport_rows = 24;
    let (mut app, _dir) =
        resumed_transcript_app_from_records(consecutive_tool_records(900), 80, viewport_rows);

    app.app
        .conversation
        .reset_transcript_projection_counters_for_harness();
    let frame = app.render_to_frame().text();

    assert!(frame.contains("explore"), "frame: {frame}");
    let counters = app
        .conversation_probe()
        .transcript_projection_counters_for_harness();
    assert_eq!(
        counters.full_layout_materializations, 0,
        "a warm grouped viewport must not materialize the exact loaded transcript layout"
    );
    assert!(
        counters.max_range_materialized_rows <= usize::from(viewport_rows) * 2,
        "warm projection materialized {} rows for a {viewport_rows}-row viewport",
        counters.max_range_materialized_rows
    );
}

fn resumed_heterogeneous_transcript_app(
    count: usize,
    width: u16,
    height: u16,
) -> (TestApp, tempfile::TempDir) {
    resumed_transcript_app_from_records(heterogeneous_resume_records(count), width, height)
}

fn resumed_transcript_app_from_records(
    records: Vec<smelt_core::TranscriptBlockRecord>,
    width: u16,
    height: u16,
) -> (TestApp, tempfile::TempDir) {
    static NEXT_SESSION_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

    let dir = tempfile::tempdir().expect("sessions root");
    let serial = NEXT_SESSION_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let session_id = format!("e{serial:063x}");
    let mut session = smelt_core::session::Session::new(1, dir.path().to_path_buf());
    session.id = session_id.clone();
    let initial = smelt_core::session::initial_store_commit_from_session(&session)
        .expect("build initial session fixture");
    let mut writer = smelt_store::OwnedLineageWriter::open(dir.path(), &session_id)
        .expect("open session fixture writer");
    writer
        .commit_session(&initial)
        .expect("commit initial session fixture");
    let lineage_id = writer.lineage_id().to_string();
    writer.release().expect("release session fixture writer");
    let store = smelt_core::session::SessionStoreAddress::new(
        dir.path().to_path_buf(),
        session_id,
        lineage_id,
    );
    crate::persist::write_transcript_record_suffix(&store, 0, &records)
        .expect("write transcript records");
    let loaded = crate::app::transcript::LoadedTranscript::tail_from_sqlite(store, width, height)
        .expect("tail transcript");
    let mut app = TestApp::builder().build();
    app.set_terminal_size(width, height);
    app.replace_transcript_document_for_harness(
        crate::app::transcript::TranscriptDocument::from_loaded_transcript(loaded),
    );
    app.focus_transcript();
    app.follow_transcript_tail();
    app.render_silent();
    (app, dir)
}

fn heterogeneous_resume_records(count: usize) -> Vec<smelt_core::TranscriptBlockRecord> {
    use smelt_core::transcript_model::Block;

    let mut source = smelt_core::content::transcript::Transcript::new();
    for idx in 0..count {
        let marker = format!("record-{idx:04}");
        match idx % 10 {
            0 => source.push(Block::User {
                text: format!(
                    "{marker} user prompt with image labels and wrapped text {}",
                    "u ".repeat(12)
                ),
                image_labels: vec![format!("image-{idx}")],
                command: false,
                sent_at_ms: None,
            }),
            1 => source.push(Block::Text {
                content: format!(
                    "{marker} assistant paragraph\n\n```diff\n- old {idx}\n+ new {idx}\n```\n{}",
                    "markdown wrap ".repeat(20)
                )
                .into(),
            }),
            2 => source.push(Block::Thinking {
                title: None,
                summary_titles: Vec::new(),
                kind: protocol::ReasoningKind::Raw,
                content: format!("{marker} thinking trace {}", "reasoning ".repeat(28)).into(),
            }),
            3 => source.push(Block::CodeLine {
                content: format!("{marker} let value_{idx} = compute({idx});"),
                lang: "rust".into(),
            }),
            4 => source.push(Block::Exec {
                command: format!("echo {marker}"),
                output: format!("{marker} stdout line\n{}", "exec output ".repeat(18)).into(),
            }),
            5 => source.push(Block::Compacted {
                summary: format!("{marker} compacted summary {}", "summary ".repeat(10)),
            }),
            6 => source.push(Block::CompactionPreview {
                summary: format!("{marker} streaming preview {}", "preview ".repeat(15)),
            }),
            7 => source.push(Block::ToolCall {
                call_id: format!("read-file-{idx}"),
                name: "read_file".into(),
                summary: protocol::StyledLines::from_plain(format!(
                    "{marker} read_file src/{idx}.rs"
                )),
                args: std::collections::HashMap::from([(
                    "file_path".to_string(),
                    serde_json::json!(format!("src/{idx}.rs")),
                )])
                .into(),
            }),
            8 => source.push(Block::ToolCall {
                call_id: format!("grep-{idx}"),
                name: "grep".into(),
                summary: protocol::StyledLines::from_plain(format!("{marker} grep needle")),
                args: std::collections::HashMap::from([(
                    "pattern".to_string(),
                    serde_json::json!(marker),
                )])
                .into(),
            }),
            _ => source.push(Block::ProcessStatus {
                text: format!("{marker} background process finished"),
                event: None,
            }),
        };
    }
    source.history.block_records()
}

fn consecutive_tool_records(count: usize) -> Vec<smelt_core::TranscriptBlockRecord> {
    use smelt_core::transcript_model::Block;

    let mut source = smelt_core::content::transcript::Transcript::new();
    for idx in 0..count {
        source.push(Block::ToolCall {
            call_id: format!("read-file-{idx}"),
            name: "read_file".into(),
            summary: protocol::StyledLines::from_plain(format!("read_file src/{idx}.rs")),
            args: std::collections::HashMap::from([(
                "file_path".to_string(),
                serde_json::json!(format!("src/{idx}.rs")),
            )])
            .into(),
        });
    }
    source.history.block_records()
}

fn tail_consecutive_user_records(count: usize) -> Vec<smelt_core::TranscriptBlockRecord> {
    use smelt_core::transcript_model::Block;

    let mut source = smelt_core::content::transcript::Transcript::new();
    for idx in 0..count {
        let marker = format!("record-{idx:04}");
        if idx % 10 == 0 || idx >= count.saturating_sub(2) {
            source.push(Block::User {
                text: format!("{marker} user prompt {}", "u ".repeat(8)),
                image_labels: Vec::new(),
                command: false,
                sent_at_ms: None,
            });
        } else {
            source.push(Block::Text {
                content: format!("{marker} assistant output\n{}", "tail context ".repeat(18))
                    .into(),
            });
        }
    }
    source.history.block_records()
}

fn wheel_transcript(app: &mut TestApp, kind: crossterm::event::MouseEventKind) {
    let vp = app
        .transcript_window()
        .viewport
        .expect("transcript viewport");
    app.feed_one(SourceEvent::Term(Event::Mouse(
        crossterm::event::MouseEvent {
            kind,
            row: vp.rect.top.saturating_add(1),
            column: vp
                .rect
                .left
                .saturating_add(vp.gutter_width)
                .saturating_add(1),
            modifiers: KeyModifiers::empty(),
        },
    )));
}

fn assert_viewport_shifted_up_rows(step: usize, before: &[String], after: &[String], rows: usize) {
    assert_eq!(
        before.len(),
        after.len(),
        "wheel-up step {step} changed viewport line count: before={before:?}, after={after:?}"
    );
    let overlap = before.len().saturating_sub(rows);
    assert!(
        overlap > 0,
        "wheel-up step {step} needs more viewport rows than movement: before={before:?}"
    );
    assert_eq!(
        &after[rows..rows + overlap],
        &before[..overlap],
        "wheel-up step {step} did not shift by exactly {rows} visible rows"
    );
}

fn assert_viewport_shifted_down_rows(
    step: usize,
    before: &[String],
    after: &[String],
    rows: usize,
) {
    assert_eq!(
        before.len(),
        after.len(),
        "wheel-down step {step} changed viewport line count: before={before:?}, after={after:?}"
    );
    let overlap = before.len().saturating_sub(rows);
    assert!(
        overlap > 0,
        "wheel-down step {step} needs more viewport rows than movement: before={before:?}"
    );
    assert!(
        after[..overlap] == before[rows..rows + overlap],
        "wheel-down step {step} did not shift by exactly {rows} visible rows: before={before:?}, after={after:?}, expected_overlap={:?}, actual_overlap={:?}",
        &before[rows..rows + overlap],
        &after[..overlap],
    );
}

fn assert_viewport_shifted_up_at_most_rows(
    step: usize,
    before: &[String],
    after: &[String],
    max_rows: usize,
) {
    assert_eq!(
        before.len(),
        after.len(),
        "wheel-up step {step} changed viewport line count: before={before:?}, after={after:?}"
    );
    let actual_rows = (1..=max_rows).find(|&rows| {
        let overlap = before.len().saturating_sub(rows);
        overlap > 0 && after[rows..rows + overlap] == before[..overlap]
    });
    assert!(
        actual_rows.is_some(),
        "wheel-up step {step} did not shift upward by 1..={max_rows} visible rows: before={before:?}, after={after:?}"
    );
}

fn assert_viewport_shifted_down_at_most_rows(
    step: usize,
    before: &[String],
    after: &[String],
    max_rows: usize,
) {
    assert_eq!(
        before.len(),
        after.len(),
        "wheel-down step {step} changed viewport line count: before={before:?}, after={after:?}"
    );
    let actual_rows = (1..=max_rows).find(|&rows| {
        let overlap = before.len().saturating_sub(rows);
        overlap > 0 && after[..overlap] == before[rows..rows + overlap]
    });
    assert!(
        actual_rows.is_some(),
        "wheel-down step {step} did not shift downward by 1..={max_rows} visible rows: before={before:?}, after={after:?}"
    );
}

fn assert_viewport_shifted_down_one_row(step: usize, before: &[String], after: &[String]) {
    assert_eq!(
        before.len(),
        after.len(),
        "cursor-down step {step} changed viewport line count: before={before:?}, after={after:?}"
    );
    let overlap = before.len().saturating_sub(1);
    assert!(
        overlap > 0,
        "cursor-down step {step} needs a non-empty viewport: before={before:?}, after={after:?}"
    );
    assert_eq!(
        &after[..overlap],
        &before[1..],
        "cursor-down step {step} did not shift the viewport by exactly one visible row"
    );
}

fn first_visible_record_index(app: &TestApp) -> Option<usize> {
    transcript_viewport_lines(app)
        .into_iter()
        .find_map(|line| parse_record_index(&line))
}

fn last_visible_record_index(app: &TestApp) -> Option<usize> {
    transcript_viewport_lines(app)
        .into_iter()
        .filter_map(|line| parse_record_index(&line))
        .next_back()
}

fn parse_record_index(line: &str) -> Option<usize> {
    let start = line.find("record-")? + "record-".len();
    let digits: String = line[start..]
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

#[test]
fn transcript_tail_follow_keeps_cursor_fixed_relative_to_viewport() {
    let mut app = row_document_transcript_app(100, true);

    app.type_char('G');
    app.render_silent();

    let win = app.transcript_window();
    let viewport_rows = win
        .viewport
        .map(|v| v.rect.height)
        .expect("transcript viewport after render");
    let total_rows = transcript_total_rows(&app);
    assert_eq!(transcript_row_cursor_row(&app), total_rows - 1);

    // Put the transcript cursor on a visible row above the bottom, then
    // re-enable sticky-bottom scrolling. New transcript rows should move
    // the cursor to the row now under the same screen cell.
    for _ in 0..3 {
        app.type_char('k');
    }
    app.render_silent();
    let screen_row_before = app
        .transcript_window()
        .cursor_screen_row(viewport_rows)
        .expect("cursor should be visible before append");
    let cursor_before = transcript_row_cursor_row(&app);
    assert!(cursor_before < total_rows - 1);
    assert!(!app.transcript_window().following_tail);

    app.follow_transcript_tail();
    app.render_silent();
    assert!(app.transcript_window().following_tail);
    assert_eq!(
        app.transcript_window().cursor_screen_row(viewport_rows),
        Some(screen_row_before)
    );

    for i in 0..10 {
        app.push_transcript_block(smelt_core::transcript_model::Block::Text {
            content: format!("new row {i:03} alpha beta").into(),
        });
    }
    app.render_silent();

    let win = app.transcript_window();
    assert!(
        transcript_total_rows(&app) > total_rows,
        "content was appended"
    );
    assert!(
        win.is_following_tail(),
        "should still be following tail after append"
    );
    assert_eq!(
        win.cursor_screen_row(viewport_rows),
        Some(screen_row_before),
        "cursor should stay fixed relative to the viewport"
    );
}

#[test]
fn transcript_vim_visual_yank_copies_document_range() {
    let mut app = row_document_transcript_app(100, true);

    app.type_char('g');
    app.type_char('g');
    app.type_char('V');
    app.type_char('6');
    app.type_char('0');
    app.type_char('G');
    app.type_char('y');

    let yank = app.core_probe().clipboard.kill_ring.current();
    assert!(yank.contains("row 000 alpha beta"), "yank was {yank:?}");
    assert!(yank.contains("row 029 alpha beta"), "yank was {yank:?}");
    let now = app.core_probe().clock.instant_now();
    assert!(app.transcript_has_row_selection(now));
    app.feed_one(SourceEvent::Tick(300));
    let now = app.core_probe().clock.instant_now();
    assert!(!app.transcript_has_row_selection(now));
}

#[test]
fn transcript_vim_visual_char_starts_at_cursor() {
    let mut app = row_document_transcript_app(100, true);

    // Move to row 003 (1-indexed in vim, so 0-indexed row 2)
    app.type_char('3');
    app.type_char('G');
    assert_eq!(transcript_row_cursor_row(&app), 2);

    // Enter visual mode
    app.type_char('v');

    // Render and inspect selection highlights
    app.render_silent();
    let win = app.transcript_window();
    let scroll_top = win.scroll_top();
    let row_base = 0;
    let highlights = app.transcript_selection_highlights(scroll_top, row_base, 16);
    // Visual mode includes the character under the cursor, matching nvim.
    let rows: Vec<usize> = highlights.iter().map(|(line, _, _)| *line).collect();
    assert!(
        rows.contains(&2),
        "visual selection should include cursor row 2, got {highlights:?}"
    );

    // Move down one row
    app.type_char('j');
    app.render_silent();
    let highlights = app.transcript_selection_highlights(scroll_top, row_base, 16);
    // Should highlight rows 2
    let rows: Vec<usize> = highlights.iter().map(|(line, _, _)| *line).collect();
    assert!(
        rows.contains(&2),
        "visual selection should include row 2, got rows {rows:?}"
    );
    assert!(
        !rows.contains(&0),
        "visual selection should not include row 0, got rows {rows:?}"
    );

    // Now simulate mouse down while in visual mode.
    // This clears selection_anchor but NOT vim_mode, which used to trigger
    // the fallback path and select the whole buffer.
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        row: 5,
        column: 10,
        modifiers: crossterm::event::KeyModifiers::empty(),
    })));
    app.render_silent();
    let highlights = app.transcript_selection_highlights(scroll_top, row_base, 16);
    assert!(
        highlights.is_empty(),
        "mouse down after visual should clear selection, got {highlights:?}"
    );
}

#[test]
fn transcript_jump_to_bottom_preserves_cursor_screen_row() {
    let mut app = row_document_transcript_app(100, true);
    let win = app.transcript_window();
    let viewport_rows = win
        .viewport
        .map(|v| v.rect.height)
        .expect("transcript viewport after render");

    // Scroll up so the viewport is no longer at the tail. Wheel-style panning
    // keeps the cursor on the same screen row.
    app.pan_transcript_by_lines(-(viewport_rows as isize * 3), viewport_rows);

    app.render_silent();
    let win = app.transcript_window();
    assert!(
        !win.is_following_tail(),
        "scrolled up should break tail-follow"
    );
    let screen_row_before = win
        .cursor_screen_row(viewport_rows)
        .expect("cursor should be visible");

    // Jump to bottom via the same API the bottom pill uses.
    app.jump_transcript_to_bottom();
    app.render_silent();

    let win = app.transcript_window();
    assert!(win.is_following_tail(), "jump_to_bottom should engage tail");
    assert_eq!(
        win.cursor_screen_row(viewport_rows),
        Some(screen_row_before),
        "cursor should stay on the same screen row after jumping to bottom"
    );
}
