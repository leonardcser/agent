use super::*;

fn cache_sensitive_history() -> Vec<protocol::HistoryItem> {
    protocol::history_from_messages(vec![
        protocol::Message::user(protocol::Content::with_images(
            "inspect this image".into(),
            vec![(
                "diagram.png".into(),
                "data:image/png;base64,dGVzdA==".into(),
            )],
        )),
        protocol::Message::assistant_with_reasoning(
            None,
            Some("inspect the file".into()),
            Some(vec![protocol::ReasoningBlock {
                provider: protocol::ReasoningBlock::OPENAI_RESPONSES.into(),
                data: serde_json::json!({
                    "type": "reasoning",
                    "encrypted_content": "synthetic-encrypted-reasoning",
                    "content": null,
                    "summary": [],
                }),
            }]),
            Some(vec![protocol::ToolCall::new(
                "call_read".into(),
                protocol::FunctionCall {
                    name: "read_file".into(),
                    arguments: r#"{ "file_path": "diagram.png" }"#.into(),
                },
            )]),
        ),
        protocol::Message::tool_with_metadata(
            "call_read".into(),
            "image attachment",
            false,
            Some(serde_json::json!({
                "kind": "file_attachment",
                "modality": "image",
                "mime": "image/png",
                "data_url": "data:image/png;base64,dGVzdA==",
                "label": "diagram.png",
            })),
        ),
        assistant_message("a1"),
        user_message("u2"),
    ])
}

#[test]
fn identity_provider_middleware_preserves_model_message() {
    let mut app = TestApp::builder().build();
    app.start_turn(42);
    assert!(app.run_lua(
        r#"
        smelt.provider.middleware({
            on_response = function(message)
                return message
            end,
        })
        "#,
    ));
    let original = protocol::history_to_messages(&cache_sensitive_history())
        .into_iter()
        .find(|message| message.reasoning_details.is_some())
        .expect("assistant message with provider reasoning");
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    app.dispatch_host_call(engine::HostCall::ProviderResponse {
        turn_id: app.current_turn_id().expect("active response turn"),
        message: original.clone(),
        reply: tx,
    });

    assert_eq!(rx.try_recv().expect("middleware reply"), Some(original));
}

#[test]
fn compaction_preserves_full_model_message_prefix() {
    for trigger in ["auto", "manual", "context_limit"] {
        let mut app = TestApp::builder().build();
        app.set_context_window(Some(100));
        for item in cache_sensitive_history() {
            app.session_append_history(item);
        }
        assert!(app.run_lua(
            r#"
            local transcript = smelt.session.messages.list({ roles = { "user" } })
            assert(transcript[1].content == "inspect this image")
            "#,
        ));
        if trigger != "manual" {
            app.start_turn(42);
        }
        let full_history = protocol::history_to_messages(&app.model_history());
        let expected_prefix = &full_history[..full_history.len() - 1];
        let (tx, _rx) = tokio::sync::oneshot::channel();
        match trigger {
            "auto" => app.dispatch_host_call(engine::HostCall::PrepareRequest {
                turn_id: app.current_turn_id().expect("active request turn"),
                messages: engine::PreparedRequestMessages::model_only(full_history.clone()),
                estimated_tokens: 200,
                reply: tx,
            }),
            "manual" => {
                app.set_context_token_baseline_for_harness(Some(200));
                assert!(app.run_lua(r#"smelt.cmd.run("compact")"#));
            }
            "context_limit" => app.dispatch_host_call(engine::HostCall::RecoverFromContextLimit {
                turn_id: app.current_turn_id().expect("active request turn"),
                messages: full_history.clone(),
                reply: tx,
            }),
            _ => unreachable!(),
        }

        let asks = ask_messages(app.drain_engine_sends());
        assert_eq!(
            asks.len(),
            1,
            "{trigger} compaction should issue one EngineAsk"
        );
        let (system, messages) = &asks[0];
        assert_eq!(system, &app.assemble_system_prompt());
        assert_eq!(messages.len(), expected_prefix.len() + 1);
        assert_eq!(&messages[..expected_prefix.len()], expected_prefix);
        assert!(messages
            .last()
            .unwrap()
            .content
            .as_ref()
            .unwrap()
            .as_text()
            .contains("CONTEXT CHECKPOINT COMPACTION"));

        let mut response = expected_prefix[1].clone();
        response.tool_calls.as_mut().unwrap()[0].id = "call_denied".into();
        app.dispatch_engine_event(protocol::EngineEvent::EngineAskResponse {
            id: app.pending_ask_id().expect("pending compaction ask"),
            message: Some(response.clone()),
            error: None,
        });
        app.drive_lua_tasks();
        let retries = ask_messages(app.drain_engine_sends());
        assert_eq!(retries.len(), 1, "tool denial should retry compaction");
        let (retry_system, retry_messages) = &retries[0];
        assert_eq!(retry_system, system);
        assert_eq!(retry_messages.len(), messages.len() + 2);
        assert_eq!(&retry_messages[..messages.len()], messages);
        assert_eq!(retry_messages[messages.len()], response);
        let denial = retry_messages.last().unwrap();
        assert_eq!(denial.tool_call_id.as_deref(), Some("call_denied"));
        assert!(denial.is_error);
    }
}

async fn read_json_request(stream: &mut tokio::net::TcpStream) -> serde_json::Value {
    use tokio::io::AsyncReadExt;

    let mut request = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut chunk).await.expect("read request headers");
        assert!(read > 0, "request ended before headers");
        request.extend_from_slice(&chunk[..read]);
        if let Some(index) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = String::from_utf8_lossy(&request[..header_end]);
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().expect("content length"))
        })
        .expect("request content-length");
    while request.len() < header_end + content_length {
        let read = stream.read(&mut chunk).await.expect("read request body");
        assert!(read > 0, "request ended before body");
        request.extend_from_slice(&chunk[..read]);
    }
    serde_json::from_slice(&request[header_end..header_end + content_length]).expect("JSON request")
}

#[test]
fn compact_command_reports_when_history_is_too_recent() {
    let mut app = TestApp::builder().build();
    app.session_append_history(protocol::HistoryItem::user(protocol::Content::text("u1")));

    assert!(app.run_lua(r#"smelt.cmd.run("compact")"#));

    assert!(app.lua_messages_contain("nothing old enough to compact"));
}

#[test]
fn compact_command_shows_preview_before_first_delta() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(80, 24);
    app.session_append_history(protocol::HistoryItem::user(protocol::Content::text("u1")));
    app.push_assistant_text("a1");
    app.set_context_token_baseline_for_harness(Some(500));

    assert!(app.run_lua(r#"smelt.cmd.run("compact")"#));

    let preview_id = app
        .conversation_probe()
        .transcript_compaction_preview_id()
        .expect("/compact should create a preview before the provider responds");
    assert!(matches!(
        app.conversation_probe().transcript().history().block(preview_id),
        Some(smelt_core::transcript_model::Block::CompactionPreview { summary })
            if summary.is_empty()
    ));
    assert!(app.run_lua(
        r#"
        assert(smelt.session.context_tokens() == nil)
        local context = smelt.session.status().context
        assert(context.state == "recalculating")
        assert(context.tokens == 500)
        "#,
    ));
    let frame = app.render_to_frame().text();
    assert!(frame.contains("compacting"), "frame: {frame}");
}

#[test]
fn compact_command_streams_preview_into_rendered_transcript() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(80, 24);
    app.session_append_history(protocol::HistoryItem::user(protocol::Content::text("u1")));
    app.push_assistant_text("a1");
    app.set_context_token_baseline_for_harness(Some(500));

    assert!(app.run_lua(r#"smelt.cmd.run("compact")"#));
    let ask_id = app
        .pending_ask_id()
        .expect("/compact registered ask callback");

    app.dispatch_engine_event(protocol::EngineEvent::EngineAskDelta {
        id: ask_id,
        delta: "# Goal\nstreamed via slash command".into(),
    });

    let frame = app.render_to_frame().text();
    assert!(frame.contains("compacting"), "frame: {frame}");
    assert!(
        frame.contains("streamed via slash command"),
        "frame: {frame}"
    );
}

#[test]
fn streaming_compaction_preview_keeps_sparse_projection_viewport_bounded() {
    let mut app = TestApp::builder().with_vim(true).build();
    let viewport_rows = 24;
    app.install_sparse_transcript_scroll_fixture(900, 80, viewport_rows);
    app.session_append_history(protocol::HistoryItem::user(protocol::Content::text("u1")));
    app.push_assistant_text("a1");
    app.set_context_token_baseline_for_harness(Some(500));

    assert!(app.run_lua(r#"smelt.cmd.run("compact")"#));
    let ask_id = app
        .pending_ask_id()
        .expect("/compact registered ask callback");
    let marker = "sparse compaction projection marker";

    app.dispatch_engine_event(protocol::EngineEvent::EngineAskDelta {
        id: ask_id,
        delta: format!("# Goal\n{}\n{marker}", "streaming summary ".repeat(1_000)),
    });
    app.app
        .conversation
        .reset_transcript_projection_counters_for_harness();
    let frame = app.render_to_frame().text();

    assert!(frame.contains("compacting"), "frame: {frame}");
    assert!(frame.contains(marker), "frame: {frame}");
    let counters = app
        .conversation_probe()
        .transcript_projection_counters_for_harness();
    assert_eq!(
        counters.full_layout_materializations, 0,
        "streaming a transient tail block must not materialize the full loaded transcript"
    );
    assert!(
        counters.max_range_materialized_rows <= usize::from(viewport_rows) * 2,
        "streaming projection materialized {} rows for a {viewport_rows}-row viewport",
        counters.max_range_materialized_rows
    );
}

#[test]
fn compact_command_keeps_completed_block_at_compaction_position() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(60, 12);
    app.commit_request_history_item(
        protocol::HistoryItem::user(protocol::Content::text("old user")),
        Some(smelt_core::transcript_model::Block::User {
            text: "old user".into(),
            image_labels: Vec::new(),
            command: false,
            sent_at_ms: None,
        }),
    );
    app.commit_request_history_item(
        protocol::HistoryItem::Assistant(protocol::AssistantStep::terminal(
            Some(protocol::Content::text("old assistant")),
            None,
            Vec::new(),
        )),
        Some(smelt_core::transcript_model::Block::Text {
            content: "old assistant".into(),
        }),
    );
    let retained = "retained user line\n".repeat(20);
    app.commit_request_history_item(
        protocol::HistoryItem::user(protocol::Content::text(retained.clone())),
        Some(smelt_core::transcript_model::Block::User {
            text: retained,
            image_labels: Vec::new(),
            command: false,
            sent_at_ms: None,
        }),
    );
    app.set_context_token_baseline_for_harness(Some(500));
    app.follow_transcript_tail();

    assert!(app.run_lua(r#"smelt.cmd.run("compact")"#));
    let ask_id = app
        .pending_ask_id()
        .expect("/compact registered ask callback");
    app.dispatch_engine_event(protocol::EngineEvent::EngineAskResponse {
        id: ask_id,
        message: Some(protocol::Message::assistant(
            Some(protocol::Content::text("# Goal\ncompleted marker")),
            None,
            None,
        )),
        error: None,
    });
    app.drive_lua_tasks();

    let history = app.conversation_probe().transcript().history();
    let marker_id = *history
        .order
        .last()
        .expect("completed marker at transcript tail");
    assert!(matches!(
        history.block(marker_id),
        Some(smelt_core::transcript_model::Block::Compacted { summary })
            if summary == "# Goal\ncompleted marker"
    ));
    let frame = app.render_to_frame().text();
    assert!(frame.contains("compacted"), "frame: {frame}");
}

#[test]
fn auto_compaction_requests_frame_before_coalesced_response_clears_preview() {
    let mut app = TestApp::builder().build();
    app.set_terminal_size(80, 24);
    app.set_context_window(Some(100));
    app.session_append_history(protocol::HistoryItem::user(protocol::Content::text("u1")));
    app.push_assistant_text("a1");
    app.session_append_history(protocol::HistoryItem::user(protocol::Content::text("u2")));
    app.start_turn(42);

    let messages = protocol::history_to_messages(&app.model_history());
    let (tx, _rx) = tokio::sync::oneshot::channel();
    {
        app.dispatch_host_call(engine::HostCall::PrepareRequest {
            turn_id: app.current_turn_id().expect("active request turn"),
            messages: engine::PreparedRequestMessages::model_only(messages),
            estimated_tokens: 200,
            reply: tx,
        });
    }
    let ask_id = app
        .drain_engine_sends()
        .into_iter()
        .filter_map(|cmd| match cmd {
            protocol::UiCommand::EngineAsk { id, stream, .. } => {
                assert!(stream, "auto-compaction EngineAsk should stream");
                Some(id)
            }
            _ => None,
        })
        .next_back()
        .expect("prepare-request compaction should issue EngineAsk");

    app.dispatch_engine_event(protocol::EngineEvent::EngineAskDelta {
        id: ask_id,
        delta: "# Goal\nstreamed before response".into(),
    });
    let response = protocol::EngineEvent::EngineAskResponse {
        id: ask_id,
        message: Some(protocol::Message::assistant(
            Some(protocol::Content::text("# Goal\nfinal summary")),
            None,
            None,
        )),
        error: None,
    };
    let mut transient_frame = None;
    {
        let mut sink = std::io::sink();
        app.dispatch_engine_event_in_render_loop_to(response, &mut sink, |frame| {
            transient_frame = Some(frame)
        });
    }

    let streamed_frame = transient_frame
        .expect("response should render the requested transient frame")
        .text();
    assert!(
        streamed_frame.contains("compacting"),
        "frame: {streamed_frame}"
    );
    assert!(
        streamed_frame.contains("streamed before response"),
        "frame: {streamed_frame}"
    );

    assert!(app
        .conversation_probe()
        .transcript_compaction_preview_id()
        .is_none());
}

#[test]
fn ordered_prepare_request_paints_transient_streaming_state() {
    let mut app = TestApp::builder().build();
    app.start_turn(42);
    app.set_terminal_size(80, 24);
    assert!(app.run_bundled_lua(
        r#"
        smelt.engine.ask_inherited({
            messages = { { role = "user", content = "summarize" } },
            on_delta = function(delta)
                __smelt_internal.transcript._set_compaction_preview(delta)
            end,
            on_response = function()
                __smelt_internal.transcript._set_compaction_preview(nil)
            end,
        })
        "#
    ));
    let ask_id = app.pending_ask_id().expect("pending ask id");
    app.render_to_frame();

    let marker = "ordered prepare streaming marker";
    app.inject_engine(protocol::EngineEvent::EngineAskDelta {
        id: ask_id,
        delta: format!("# Goal\n{marker}"),
    })
    .expect("queue compaction preview delta");
    app.inject_engine(protocol::EngineEvent::EngineAskResponse {
        id: ask_id,
        message: Some(protocol::Message::assistant(
            Some(protocol::Content::text("# Goal\nfinal summary")),
            None,
            None,
        )),
        error: None,
    })
    .expect("queue compaction preview response");
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    assert!(
        app.inject_host_call(engine::HostCall::PrepareRequest {
            turn_id: app.current_turn_id().expect("active request turn"),
            messages: engine::PreparedRequestMessages::new(Vec::new(), 0),
            estimated_tokens: 0,
            reply: tx,
        })
        .is_ok(),
        "queue prepare request"
    );

    let mut streamed_frames = Vec::new();
    loop {
        let outcome = app.drain_ready_engine_outputs_for_frame_to(&mut std::io::sink(), |frame| {
            streamed_frames.push(frame.text())
        });
        if outcome == crate::app::render_loop::EngineOutputDrainOutcome::Drained {
            break;
        }
        app.render_frame_to(&mut std::io::sink());
        streamed_frames.push(app.ui_snapshot().text());
    }

    assert!(
        streamed_frames.iter().any(|frame| frame.contains(marker)),
        "ordered prepare request skipped the transient frame: {streamed_frames:#?}"
    );
    assert!(
        app.conversation_probe()
            .transcript_compaction_preview_id()
            .is_none(),
        "final response should clear the transient preview"
    );
    assert!(matches!(
        rx.try_recv().expect("prepare request reply"),
        engine::HostRequestDecision::Continue
    ));
}

#[test]
fn auto_compaction_does_not_recompact_checkpoint_summary_without_new_old_groups() {
    let mut app = TestApp::builder().build();
    app.start_turn(42);
    let mut settings = app.core_probe().config.settings.clone();
    settings.auto_compact = true;
    settings.compact_threshold = 0.8;
    settings.compact_keep_recent_groups = 1.0;
    app.set_settings_for_harness(settings);
    app.set_context_window(Some(100));

    app.session_append_history(protocol::HistoryItem::user(protocol::Content::text("u1")));
    app.push_assistant_text("a1");
    app.session_append_history(protocol::HistoryItem::user(protocol::Content::text("u2")));

    let messages = protocol::history_to_messages(&app.model_history());
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    {
        app.dispatch_host_call(engine::HostCall::PrepareRequest {
            turn_id: app.current_turn_id().expect("active request turn"),
            messages: engine::PreparedRequestMessages::model_only(messages),
            estimated_tokens: 200,
            reply: tx,
        });
    }
    let ask_id = app
        .drain_engine_sends()
        .into_iter()
        .filter_map(|cmd| match cmd {
            protocol::UiCommand::EngineAsk { id, .. } => Some(id),
            _ => None,
        })
        .next_back()
        .expect("first prepare request should compact");
    {
        app.dispatch_engine_event(protocol::EngineEvent::EngineAskResponse {
            id: ask_id,
            message: Some(protocol::Message::assistant(
                Some(protocol::Content::text("# Goal\nsummary")),
                None,
                None,
            )),
            error: None,
        });
        app.drive_lua_tasks();
    }
    assert!(matches!(
        rx.try_recv().expect("first prepare reply"),
        engine::HostRequestDecision::ReplaceModelHistory { .. }
    ));
    let checkpoint = app
        .conversation_probe()
        .session()
        .checkpoint
        .as_ref()
        .expect("checkpoint installed");
    assert!(checkpoint.tokens_after_estimate.is_some());
    assert_eq!(
        checkpoint.tokens_after_estimate_history_len,
        Some(app.session_message_count())
    );

    let messages = protocol::history_to_messages(&app.model_history());
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    {
        app.dispatch_host_call(engine::HostCall::PrepareRequest {
            turn_id: app.current_turn_id().expect("active request turn"),
            messages: engine::PreparedRequestMessages::model_only(messages),
            estimated_tokens: 200,
            reply: tx,
        });
    }

    let sends = app.drain_engine_sends();
    assert!(
        sends
            .iter()
            .all(|cmd| !matches!(cmd, protocol::UiCommand::EngineAsk { .. })),
        "second prepare re-entered compaction: {sends:?}"
    );
    assert!(matches!(
        rx.try_recv().expect("second prepare reply"),
        engine::HostRequestDecision::Continue
    ));
}

#[test]
fn engine_ask_delta_callbacks_can_update_compaction_preview_from_dispatch() {
    let mut app = TestApp::builder().build();
    assert!(app.run_bundled_lua(
        r#"
        smelt.engine.ask_inherited({
            messages = { { role = "user", content = "summarize" } },
            on_delta = function(delta)
                __smelt_internal.transcript._set_compaction_preview(delta)
            end,
            on_response = function() end,
        })
        "#
    ));
    let ask_id = app.pending_ask_id().expect("pending ask id");

    app.dispatch_engine_event(protocol::EngineEvent::EngineAskDelta {
        id: ask_id,
        delta: "# Goal\nstream the summary".into(),
    });

    let preview_id = app
        .conversation_probe()
        .transcript_compaction_preview_id()
        .expect("compaction preview id");
    assert!(matches!(
        app.conversation_probe().transcript().history().block(preview_id),
        Some(smelt_core::transcript_model::Block::CompactionPreview { summary })
            if summary == "# Goal\nstream the summary"
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn timed_render_loop_shows_compaction_preview_only_while_following_tail() {
    use crossterm::event::{MouseEvent, MouseEventKind};

    for scrolled_up in [false, true] {
        let mut app = TestApp::builder().build();
        app.set_terminal_size(80, 24);
        app.session_append_history(protocol::HistoryItem::user(protocol::Content::text("u1")));
        app.push_assistant_text("a1");
        for index in 0..40 {
            app.push_user_block(&format!("transcript row {index}: {}", "content ".repeat(8)));
            app.push_transcript_block(smelt_core::transcript_model::Block::Text {
                content: format!("assistant row {index}: {}", "response ".repeat(8)).into(),
            });
        }
        app.set_context_token_baseline_for_harness(Some(500));
        app.follow_transcript_tail();
        app.render_to_frame();

        assert!(app.run_lua(r#"smelt.cmd.run("compact")"#));
        let ask_id = app
            .pending_ask_id()
            .expect("/compact registered ask callback");

        if scrolled_up {
            let viewport = app
                .transcript_window()
                .viewport
                .expect("rendered transcript viewport");
            for _ in 0..6 {
                app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
                    kind: MouseEventKind::ScrollUp,
                    column: viewport.rect.left.saturating_add(2),
                    row: viewport.rect.top.saturating_add(2),
                    modifiers: KeyModifiers::NONE,
                })));
                app.render_to_frame();
            }
            assert!(
                !app.transcript_window().following_tail,
                "wheel input should pin the transcript before streaming"
            );
        } else {
            assert!(app.transcript_window().following_tail);
        }

        let marker = "render-loop preview marker";
        let mut source = crate::event_source::ScriptedSource::new([
            SourceEvent::Tick(20),
            SourceEvent::engine(protocol::EngineEvent::EngineAskDelta {
                id: ask_id,
                delta: "# Goal\n".into(),
            }),
            SourceEvent::Tick(20),
            SourceEvent::engine(protocol::EngineEvent::EngineAskDelta {
                id: ask_id,
                delta: marker.into(),
            }),
            SourceEvent::Tick(20),
            SourceEvent::engine(protocol::EngineEvent::EngineAskResponse {
                id: ask_id,
                message: Some(protocol::Message::assistant(
                    Some(protocol::Content::text("# Goal\nfinal checkpoint")),
                    None,
                    None,
                )),
                error: None,
            }),
        ]);
        let frames = app.run_scripted_render_loop(&mut source).await;
        let preview_frames: Vec<_> = frames
            .iter()
            .map(|frame| (frame.kind, frame.snapshot.text()))
            .filter(|(_, frame)| frame.contains(marker))
            .collect();

        if scrolled_up {
            assert!(
                preview_frames.is_empty(),
                "pinned transcript should not jump to the preview: {preview_frames:?}"
            );
            assert!(
                !app.transcript_window().following_tail,
                "streaming preview should preserve the pinned viewport"
            );
        } else {
            assert!(
                preview_frames.iter().any(|(kind, frame)| {
                    *kind == RenderLoopFrameKind::Normal && frame.contains("compacting")
                }),
                "no normal frame rendered the streaming preview body: {frames:#?}"
            );
        }
        assert!(
            app.conversation_probe()
                .transcript_compaction_preview_id()
                .is_none(),
            "response should replace the transient preview"
        );
        assert!(app.run_lua(r#"assert(smelt.session.status().context.state == "ready")"#));
    }
}

#[test]
fn wheel_scroll_moves_while_compaction_preview_streams() {
    use crossterm::event::{MouseEvent, MouseEventKind};

    let mut app = TestApp::builder().build();
    app.set_terminal_size(80, 24);
    app.session_append_history(protocol::HistoryItem::user(protocol::Content::text("u1")));
    app.push_assistant_text("a1");
    for index in 0..40 {
        app.push_user_block(&format!("transcript row {index}: {}", "content ".repeat(8)));
        app.push_transcript_block(smelt_core::transcript_model::Block::Text {
            content: format!("assistant row {index}: {}", "response ".repeat(8)).into(),
        });
    }
    app.set_context_token_baseline_for_harness(Some(500));
    app.follow_transcript_tail();
    app.render_to_frame();

    assert!(app.run_lua(r#"smelt.cmd.run("compact")"#));
    let ask_id = app
        .pending_ask_id()
        .expect("/compact registered ask callback");
    app.dispatch_engine_event(protocol::EngineEvent::EngineAskDelta {
        id: ask_id,
        delta: (1..=20)
            .map(|line| format!("summary line {line}"))
            .collect::<Vec<_>>()
            .join("\n"),
    });
    app.render_to_frame();

    let viewport = app
        .transcript_window()
        .viewport
        .expect("rendered transcript viewport");
    for step in 0..6 {
        let before = app.app.transcript_scroll_top();
        app.feed_one(SourceEvent::Term(Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: viewport.rect.left.saturating_add(2),
            row: viewport.rect.top.saturating_add(2),
            modifiers: KeyModifiers::NONE,
        })));
        app.dispatch_engine_event(protocol::EngineEvent::EngineAskDelta {
            id: ask_id,
            delta: format!(" streaming-{step}"),
        });
        app.render_to_frame();

        let after = app.app.transcript_scroll_top();
        assert!(
            after < before,
            "wheel step {step} was lost while the preview streamed: {before} -> {after}"
        );
        assert!(!app.transcript_window().following_tail);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn real_engine_responses_compaction_streams_preview_before_response() {
    use std::sync::Arc;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    fn sse(events: &[&str]) -> String {
        events
            .iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect()
    }

    // Codex and OpenAI share the OpenAI Responses wire parser. Use OpenAI
    // identity here so the test does not depend on developer OAuth credentials.
    let first_events = sse(&[
        r#"{"type":"response.created","response":{"id":"resp_compaction","status":"in_progress","output":[]}}"#,
        r#"{"type":"response.output_item.added","output_index":0,"item":{"id":"msg_compaction","type":"message","status":"in_progress","role":"assistant","content":[]}}"#,
        r##"{"type":"response.output_text.delta","item_id":"msg_compaction","output_index":0,"content_index":0,"delta":"# Goal\nlive compaction marker"}"##,
    ]);
    let remaining_events = sse(&[
        r#"{"type":"response.output_text.delta","item_id":"msg_compaction","output_index":0,"content_index":0,"delta":" completed"}"#,
        r##"{"type":"response.output_text.done","item_id":"msg_compaction","output_index":0,"content_index":0,"text":"# Goal\nlive compaction marker completed"}"##,
        r##"{"type":"response.output_item.done","output_index":0,"item":{"id":"msg_compaction","type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","text":"# Goal\nlive compaction marker completed","annotations":[]}]}}"##,
        r##"{"type":"response.completed","response":{"id":"resp_compaction","status":"completed","output":[{"id":"msg_compaction","type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","text":"# Goal\nlive compaction marker completed","annotations":[]}]}],"usage":{"input_tokens":100,"output_tokens":5,"total_tokens":105}}}"##,
    ]);
    let full_len = first_events.len() + remaining_events.len();

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock provider");
    let address = listener.local_addr().expect("mock provider address");
    let (first_chunk_tx, first_chunk_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept provider request");
        let request = read_json_request(&mut stream).await;
        assert_eq!(request["stream"], true, "compaction request must stream");
        let headers = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {full_len}\r\nConnection: close\r\n\r\n"
        );
        stream
            .write_all(headers.as_bytes())
            .await
            .expect("write response headers");
        stream
            .write_all(first_events.as_bytes())
            .await
            .expect("write first stream chunk");
        stream.flush().await.expect("flush first stream chunk");
        let _ = first_chunk_tx.send(request);
        let _ = release_rx.await;
        stream
            .write_all(remaining_events.as_bytes())
            .await
            .expect("write final stream chunk");
        stream.flush().await.expect("flush final stream chunk");
    });

    let engine_cwd = tempfile::tempdir().expect("create engine cwd");
    let engine = engine::start(
        engine::EngineConfig::new(
            engine_cwd.path().to_path_buf(),
            Arc::new(engine::clock::RealClock),
        ),
        Box::new(engine::tools::EmptyDispatcher),
    );
    let mut app = TestApp::builder()
        .with_cwd(engine_cwd.path())
        .with_engine(engine)
        .build();
    app.set_terminal_size(80, 24);
    app.use_model(smelt_core::config::ResolvedModel {
        key: "mock/compact".into(),
        provider_name: "mock".into(),
        model_name: "compact".into(),
        display_name: None,
        api_base: format!("http://{address}"),
        api_key_env: String::new(),
        provider_type: "openai".into(),
        config: protocol::ModelConfig::default(),
        catalog: protocol::ModelCatalogMetadata {
            default_reasoning_effort: Some(protocol::ReasoningEffort::Medium),
            supported_reasoning_efforts: vec![
                protocol::ReasoningEffort::Medium,
                protocol::ReasoningEffort::Max,
            ],
            ..Default::default()
        },
    });
    assert!(app.run_lua(r#"smelt.reasoning.set("max")"#));
    for item in cache_sensitive_history() {
        app.session_append_history(item);
    }
    app.set_context_token_baseline_for_harness(Some(500));
    app.follow_transcript_tail();
    app.render_to_frame();
    assert!(app.run_lua(r#"smelt.cmd.run("compact")"#));

    let waiting_frame = app.render_to_frame().text();
    assert!(
        waiting_frame.contains("compacting"),
        "frame: {waiting_frame}"
    );
    assert!(
        !waiting_frame.contains("live compaction marker"),
        "provider delta arrived before the waiting frame: {waiting_frame}"
    );

    let request = tokio::time::timeout(std::time::Duration::from_secs(5), first_chunk_rx)
        .await
        .expect("provider did not receive compaction request")
        .expect("mock provider stopped before first chunk");
    assert_eq!(request["reasoning"]["effort"], "max");
    assert_eq!(
        request["input"][1],
        serde_json::json!({
            "type": "reasoning",
            "encrypted_content": "synthetic-encrypted-reasoning",
            "content": null,
            "summary": [],
        })
    );
    assert_eq!(request["input"][2]["type"], "function_call");
    assert_eq!(request["input"][3]["type"], "function_call_output");

    let mut terminal_output = Vec::new();
    let mut streamed_frame = None;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while streamed_frame.is_none() {
            let output = app
                .app
                .core
                .engine
                .recv_output()
                .await
                .expect("engine stopped while compaction streamed");
            let is_delta = matches!(
                output,
                engine::EngineOutput::Event(protocol::EngineEvent::EngineAskDelta { .. })
            );
            let output_len_before = terminal_output.len();
            app.app
                .dispatch_selected_engine_output_in_render_loop_to(output, &mut terminal_output);
            if is_delta {
                assert!(
                    terminal_output.len() > output_len_before,
                    "selected compaction delta did not paint the terminal"
                );
                streamed_frame = Some(app.ui_snapshot().text());
            }
        }
    })
    .await
    .expect("engine did not emit compaction delta");

    let frame = streamed_frame.expect("streaming frame");
    assert!(frame.contains("compacting"), "frame: {frame}");
    assert!(frame.contains("live compaction marker"), "frame: {frame}");

    let next_iteration_frame = app.render_to_frame().text();
    assert!(
        next_iteration_frame.contains("live compaction marker"),
        "next event-loop frame: {next_iteration_frame}"
    );

    let _ = release_tx.send(());
    server.await.expect("mock provider server");
}

fn one_shot_response(id: &str, text: &str, input_tokens: u32) -> String {
    let message_id = format!("{id}_message");
    let events = [
        serde_json::json!({
            "type": "response.created",
            "response": { "id": id, "status": "in_progress", "output": [] }
        }),
        serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {
                "id": message_id,
                "type": "message",
                "status": "in_progress",
                "role": "assistant",
                "content": []
            }
        }),
        serde_json::json!({
            "type": "response.output_text.delta",
            "item_id": message_id,
            "output_index": 0,
            "content_index": 0,
            "delta": text
        }),
        serde_json::json!({
            "type": "response.output_text.done",
            "item_id": message_id,
            "output_index": 0,
            "content_index": 0,
            "text": text
        }),
        serde_json::json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "id": message_id,
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": text, "annotations": [] }]
            }
        }),
        serde_json::json!({
            "type": "response.completed",
            "response": {
                "id": id,
                "status": "completed",
                "output": [{
                    "id": message_id,
                    "type": "message",
                    "status": "completed",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": text, "annotations": [] }]
                }],
                "usage": {
                    "input_tokens": input_tokens,
                    "output_tokens": 3,
                    "total_tokens": input_tokens + 3
                }
            }
        }),
    ];
    events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect()
}

async fn write_response(stream: &mut tokio::net::TcpStream, body: &str) {
    use tokio::io::AsyncWriteExt;
    let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
    stream
        .write_all(response.as_bytes())
        .await
        .expect("write one-shot response");
    stream.shutdown().await.expect("shutdown response");
}

#[tokio::test(flavor = "current_thread")]
async fn real_engine_one_shot_auto_compaction_preserves_lifecycle() {
    use std::sync::Arc;
    use tokio::net::TcpListener;

    fn compacted_block_count(app: &TestApp) -> usize {
        let history = app.conversation_probe().transcript().history();
        (0..history.len())
            .filter(|index| {
                history
                    .block_id_at(*index)
                    .and_then(|id| history.block_kind(id))
                    == Some("compacted")
            })
            .count()
    }

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock provider");
    let address = listener.local_addr().expect("mock provider address");
    let summary = [
        "SUMMARY_HEAD_MUST_BE_CAPPED",
        "summary line 02",
        "summary line 03",
        "summary line 04",
        "summary line 05",
        "summary line 06",
        "summary line 07",
        "summary line 08",
        "summary line 09",
        "summary line 10",
        "summary line 11",
        "SUMMARY_TAIL_MUST_BE_VISIBLE",
    ]
    .join("\n");
    let server_summary = summary.clone();
    let server = tokio::spawn(async move {
        for request_index in 0..2 {
            let (mut stream, _) = listener.accept().await.expect("accept provider request");
            let request = read_json_request(&mut stream).await;
            let request_text = request.to_string();
            let (id, content, input_tokens) = if request_index == 0 {
                assert!(
                    request_text.contains("CONTEXT CHECKPOINT COMPACTION"),
                    "first request should compact context: {request_text}"
                );
                ("resp_compaction", server_summary.as_str(), 120)
            } else {
                assert!(
                    request_text.contains("SUMMARY_TAIL_MUST_BE_VISIBLE"),
                    "foreground request should use checkpointed model history: {request_text}"
                );
                assert!(
                    !request_text.contains("old user 1"),
                    "foreground request should omit the compacted prefix: {request_text}"
                );
                ("resp_foreground", "foreground complete", 55)
            };
            write_response(&mut stream, &one_shot_response(id, content, input_tokens)).await;
        }
        if let Ok(Ok((mut stream, _))) =
            tokio::time::timeout(std::time::Duration::from_millis(500), listener.accept()).await
        {
            let request = read_json_request(&mut stream).await;
            panic!("unexpected third provider request: {request}");
        }
    });

    let engine_cwd = tempfile::tempdir().expect("create engine cwd");
    let config_dir = engine_cwd.path().join("config");
    std::fs::create_dir_all(&config_dir).expect("create test config");
    std::fs::write(
        config_dir.join("early.lua"),
        r#"smelt.builtins.disable({ plugins = { "title", "predict" } })"#,
    )
    .expect("write early init");
    let engine = engine::start(
        engine::EngineConfig::new(
            engine_cwd.path().to_path_buf(),
            Arc::new(engine::clock::RealClock),
        ),
        Box::new(engine::tools::EmptyDispatcher),
    );
    let mut app = TestApp::builder()
        .with_cwd(engine_cwd.path())
        .with_lua_load_paths(&config_dir, None)
        .with_engine(engine)
        .build();
    app.set_terminal_size(80, 24);
    app.use_model(smelt_core::config::ResolvedModel {
        key: "mock/compact".into(),
        provider_name: "mock".into(),
        model_name: "compact".into(),
        display_name: None,
        api_base: format!("http://{address}"),
        api_key_env: String::new(),
        provider_type: "openai".into(),
        config: protocol::ModelConfig::default(),
        catalog: protocol::ModelCatalogMetadata::default(),
    });
    app.set_context_window(Some(200));
    let mut settings = app.core_probe().config.settings.clone();
    settings.auto_compact = true;
    settings.compact_threshold = 0.5;
    settings.compact_keep_recent_groups = 2.0;
    app.set_settings_for_harness(settings);
    for index in 1..=3 {
        let user = format!("old user {index}");
        app.commit_request_history_item(
            protocol::HistoryItem::user(protocol::Content::text(user.clone())),
            Some(smelt_core::transcript_model::Block::User {
                text: user,
                image_labels: Vec::new(),
                command: false,
                sent_at_ms: None,
            }),
        );
        let assistant = format!("old assistant {index}");
        app.commit_request_history_item(
            protocol::HistoryItem::Assistant(protocol::AssistantStep::terminal(
                Some(protocol::Content::text(assistant.clone())),
                None,
                Vec::new(),
            )),
            Some(smelt_core::transcript_model::Block::Text {
                content: assistant.into(),
            }),
        );
    }
    app.set_context_token_baseline_for_harness(Some(20));
    app.follow_transcript_tail();
    app.render_to_frame();
    let submitted = "new unaccounted context ".repeat(30);
    app.start_submitted_turn(&submitted);

    let mut terminal_output = Vec::new();
    let mut waiting_frame = None;
    let mut preview_frame = None;
    let mut marker_count_after_response = None;
    let mut marker_count_after_history_update = None;
    let mut final_frame = None;
    let mut saw_turn_complete = false;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !saw_turn_complete {
            let output = app
                .app
                .core
                .engine
                .recv_output()
                .await
                .expect("engine stopped during one-shot compaction");
            let is_prepare_request = matches!(
                &output,
                engine::EngineOutput::HostCall(engine::HostCall::PrepareRequest { .. })
            );
            let is_compaction_response = matches!(
                &output,
                engine::EngineOutput::Event(protocol::EngineEvent::EngineAskResponse { .. })
            );
            let is_history_update = matches!(
                &output,
                engine::EngineOutput::Event(protocol::EngineEvent::HistoryUpdated { .. })
            );
            saw_turn_complete = matches!(
                &output,
                engine::EngineOutput::Event(protocol::EngineEvent::TurnComplete { .. })
            );

            app.app
                .dispatch_selected_engine_output_in_render_loop_to(output, &mut terminal_output);
            let frame = app.render_to_frame().text();
            if is_prepare_request && waiting_frame.is_none() && frame.contains("compacting") {
                waiting_frame = Some(frame.clone());
            }
            if frame.contains("SUMMARY_TAIL_MUST_BE_VISIBLE") {
                preview_frame = Some(frame.clone());
            }
            if is_compaction_response {
                marker_count_after_response = Some(compacted_block_count(&app));
            }
            if is_history_update {
                marker_count_after_history_update = Some(compacted_block_count(&app));
            }
            final_frame = Some(frame);
        }
    })
    .await
    .expect("one-shot compaction turn timed out");

    server.await.expect("mock provider server");
    let waiting_frame = waiting_frame.expect("waiting compaction frame");
    assert!(waiting_frame.contains("compacting"));
    let preview_frame = preview_frame.expect("one-shot compaction preview frame");
    for expected in [
        "summary line 09",
        "summary line 10",
        "summary line 11",
        "SUMMARY_TAIL_MUST_BE_VISIBLE",
    ] {
        assert!(
            preview_frame.contains(expected),
            "one-shot preview omitted tail line {expected:?}:\n{preview_frame}"
        );
    }
    assert!(
        !preview_frame.contains("summary line 08")
            && !preview_frame.contains("SUMMARY_HEAD_MUST_BE_CAPPED"),
        "one-shot preview should retain exactly four summary tail lines:\n{preview_frame}"
    );
    assert_eq!(marker_count_after_response, Some(1));
    assert_eq!(marker_count_after_history_update, Some(1));
    assert_eq!(compacted_block_count(&app), 1);
    assert!(app
        .conversation_probe()
        .transcript_compaction_preview_id()
        .is_none());
    let final_frame = final_frame.expect("final foreground frame");
    assert!(final_frame.contains("foreground complete"));
    assert!(
        final_frame.contains("58 (28%)"),
        "foreground usage should restore authoritative context:\n{final_frame}"
    );
    assert!(
        waiting_frame.contains("20 (10%)"),
        "context usage disappeared while compacting:\n{waiting_frame}"
    );
}

#[test]
fn delayed_host_calls_cannot_claim_a_replacement_turn() {
    for replace_turn in [false, true] {
        for kind in ["prepare", "recover", "response"] {
            let mut app = TestApp::builder().build();
            app.start_turn(42);
            app.app.lua.core_shared().hooks.prepare_request.clear();
            app.app.lua.core_shared().hooks.context_limit.clear();
            app.app.lua.core_shared().hooks.provider_response.clear();
            assert!(app.run_bundled_lua(
                r#"
                _G.hook_calls = 0
                local function pending(_, reply)
                    _G.hook_calls = _G.hook_calls + 1
                    _G.pending_reply = reply
                    _G.compaction = __smelt_internal.work._context_recalculation("compacting")
                    __smelt_internal.transcript._set_compaction_preview("CURRENT_PREVIEW")
                end
                smelt.engine.on_prepare_request(pending)
                smelt.engine.on_context_limit(pending)
                smelt.provider.middleware({ on_response = function(message)
                    _G.hook_calls = _G.hook_calls + 1
                    __smelt_internal.transcript._set_compaction_preview("STALE_PREVIEW")
                    return message
                end })
            "#
            ));
            let (reply, mut response) = tokio::sync::oneshot::channel();
            let (provider_reply, mut provider_response) = tokio::sync::oneshot::channel();
            let delayed = match kind {
                "prepare" => engine::HostCall::PrepareRequest {
                    turn_id: 42,
                    messages: engine::PreparedRequestMessages::model_only(Vec::new()),
                    estimated_tokens: 200,
                    reply,
                },
                "recover" => engine::HostCall::RecoverFromContextLimit {
                    turn_id: 42,
                    messages: Vec::new(),
                    reply,
                },
                "response" => engine::HostCall::ProviderResponse {
                    turn_id: 42,
                    message: assistant_message("STALE_RESPONSE"),
                    reply: provider_reply,
                },
                _ => unreachable!(),
            };
            let mut current_response = None;
            if replace_turn {
                app.type_text("NEXT_TASK");
                app.press(KeyCode::Enter);
                app.press(KeyCode::Enter);
                app.press(KeyCode::Enter);
                assert_ne!(app.current_turn_id(), Some(42));
                let (reply, response) = tokio::sync::oneshot::channel();
                current_response = Some(response);
                app.dispatch_host_call(engine::HostCall::PrepareRequest {
                    turn_id: app.current_turn_id().expect("replacement turn"),
                    messages: engine::PreparedRequestMessages::model_only(Vec::new()),
                    estimated_tokens: 0,
                    reply,
                });
            } else {
                app.press(KeyCode::Esc);
                app.press(KeyCode::Esc);
                assert!(!app.agent_running());
            }
            let turn_id = app.current_turn_id();
            let history = app.model_history();
            app.app.dispatch_selected_engine_output_in_render_loop_to(
                engine::EngineOutput::HostCall(delayed),
                &mut std::io::sink(),
            );
            assert!(app.run_lua(&format!(
                "assert(hook_calls == {})",
                usize::from(replace_turn)
            )));
            assert_eq!(app.current_turn_id(), turn_id);
            assert_eq!(app.model_history(), history);
            if kind == "response" {
                assert!(provider_response.try_recv().unwrap().is_none());
            } else {
                assert!(matches!(
                    response.try_recv().unwrap(),
                    engine::HostRequestDecision::Stop
                ));
            }
            if let Some(mut current_response) = current_response {
                assert!(matches!(
                    current_response.try_recv(),
                    Err(tokio::sync::oneshot::error::TryRecvError::Empty)
                ));
                assert_eq!(app.working_probe().phase_label(), Some("compacting"));
                assert!(app.render_to_frame().text().contains("CURRENT_PREVIEW"));
            }
        }
    }
}

#[test]
fn request_hook_drop_clears_pending_work() {
    let mut app = TestApp::builder().build();
    app.start_turn(42);
    app.app.lua.core_shared().hooks.prepare_request.clear();
    assert!(app.run_lua(
        r#"smelt.engine.on_prepare_request(function(_, reply) _G.pending_reply = reply end)"#
    ));
    let (reply, mut response) = tokio::sync::oneshot::channel();
    app.dispatch_host_call(engine::HostCall::PrepareRequest {
        turn_id: app.current_turn_id().expect("active request turn"),
        messages: engine::PreparedRequestMessages::model_only(Vec::new()),
        estimated_tokens: 0,
        reply,
    });
    assert!(app.run_lua("pending_reply = nil; collectgarbage('collect')"));
    assert!(matches!(
        response.try_recv(),
        Ok(engine::HostRequestDecision::Continue)
    ));
    assert_eq!(app.working_probe().phase_label(), Some("working"));
}

#[test]
fn non_compaction_request_hook_does_not_defer_force_pop() {
    let mut app = TestApp::builder().build();
    app.start_turn(42);
    app.app.lua.core_shared().hooks.prepare_request.clear();
    assert!(app.run_lua(
        r#"smelt.engine.on_prepare_request(function(_, reply) _G.pending_reply = reply end)"#
    ));
    let (reply, _response) = tokio::sync::oneshot::channel();
    app.dispatch_host_call(engine::HostCall::PrepareRequest {
        turn_id: app.current_turn_id().expect("active request turn"),
        messages: engine::PreparedRequestMessages::model_only(Vec::new()),
        estimated_tokens: 0,
        reply,
    });
    app.type_text("NEXT_TASK");
    app.press(KeyCode::Enter);
    app.press(KeyCode::Enter);
    app.press(KeyCode::Enter);
    assert_ne!(app.current_turn_id(), Some(42));
    assert_eq!(app.queued_message_count(), 0);
    assert_eq!(app.working_probe().phase_label(), Some("working"));
}

#[test]
fn cancelled_turn_without_usage_preserves_context_token_baseline() {
    let mut app = TestApp::builder().build();
    app.session_append_history(protocol::HistoryItem::user(protocol::Content::text("u1")));
    app.push_assistant_text("a1");
    app.set_context_token_baseline_for_harness(Some(500));
    app.start_turn(7);

    app.discard_turn(crate::app::TurnEnd::Cancelled);

    assert_eq!(app.session_snapshot().context_tokens, Some(500));
    assert_eq!(app.session_snapshot().context_tokens_history_len, Some(2));
}

#[test]
fn late_compaction_callbacks_preserve_new_preview() {
    fn prepare(app: &mut TestApp) -> u64 {
        let messages = protocol::history_to_messages(&app.model_history());
        let (tx, _rx) = tokio::sync::oneshot::channel();
        app.dispatch_host_call(engine::HostCall::PrepareRequest {
            turn_id: app.current_turn_id().expect("active request turn"),
            messages: engine::PreparedRequestMessages::model_only(messages),
            estimated_tokens: 200,
            reply: tx,
        });
        app.drain_engine_sends()
            .into_iter()
            .filter_map(|command| match command {
                protocol::UiCommand::EngineAsk { id, messages, .. }
                    if messages.last().is_some_and(|message| {
                        message.content.as_ref().is_some_and(|content| {
                            content
                                .text_content()
                                .contains("CONTEXT CHECKPOINT COMPACTION")
                        })
                    }) =>
                {
                    Some(id)
                }
                _ => None,
            })
            .next_back()
            .expect("compaction asks for a summary")
    }

    let mut failures = Vec::new();
    for (replace_turn, late_response) in
        [(false, false), (false, true), (true, false), (true, true)]
    {
        let mut app = TestApp::builder().build();
        app.set_terminal_size(80, 24);
        app.set_context_window(Some(100));
        app.session_append_history(protocol::HistoryItem::user(protocol::Content::text("u1")));
        app.push_assistant_text("a1");
        app.session_append_history(protocol::HistoryItem::user(protocol::Content::text("u2")));
        app.start_turn(42);
        let old_ask = prepare(&mut app);
        app.dispatch_engine_event(protocol::EngineEvent::EngineAskDelta {
            id: old_ask,
            delta: "OLD_SUMMARY".into(),
        });
        if replace_turn {
            app.discard_turn(crate::app::TurnEnd::Cancelled);
            app.start_turn(43);
        }

        let new_ask = prepare(&mut app);
        assert_ne!(old_ask, new_ask);
        app.dispatch_engine_event(protocol::EngineEvent::EngineAskDelta {
            id: new_ask,
            delta: "NEW_SUMMARY".into(),
        });
        assert!(app.render_to_frame().text().contains("NEW_SUMMARY"));
        assert_eq!(app.working_probe().phase_label(), Some("compacting"));

        if late_response {
            app.dispatch_engine_event(protocol::EngineEvent::EngineAskResponse {
                id: old_ask,
                message: None,
                error: Some(protocol::EngineAskError {
                    kind: protocol::EngineAskErrorKind::Cancelled,
                    message: "cancelled".into(),
                }),
            });
        } else {
            app.dispatch_engine_event(protocol::EngineEvent::EngineAskDelta {
                id: old_ask,
                delta: "_LATE_DELTA".into(),
            });
        }
        app.drive_lua_tasks();
        let frame = app.render_to_frame().text();
        let phase = app.working_probe().phase_label();
        if !frame.contains("NEW_SUMMARY") || phase != Some("compacting") {
            failures.push(format!(
                "late_response={late_response}, phase={phase:?}, frame:\n{frame}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[tokio::test(flavor = "current_thread")]
async fn real_engine_compaction_preserves_queued_inputs() {
    use std::sync::Arc;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    const SENT_AT_MS: u64 = 1_742_567_823_000;
    for recovery in [false, true] {
        for action in ["promote", "steer", "pop", "multi", "withdraw"] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let (release_tx, release_rx) = tokio::sync::oneshot::channel();
            let foreground_count = if action == "multi" { 3 } else { 1 };
            let server = tokio::spawn(async move {
                if recovery {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let request = read_json_request(&mut stream).await;
                    assert!(!request
                        .to_string()
                        .contains("CONTEXT CHECKPOINT COMPACTION"));
                    let body = r#"{"error":{"message":"maximum context length exceeded","type":"invalid_request_error","code":"context_length_exceeded"}}"#;
                    stream.write_all(format!(
                        "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()
                    ).as_bytes()).await.unwrap();
                    stream.shutdown().await.unwrap();
                }
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_json_request(&mut stream).await;
                assert!(request
                    .to_string()
                    .contains("CONTEXT CHECKPOINT COMPACTION"));
                let body = one_shot_response("summary", "CHECKPOINT_READY", 40);
                let prefix_len: usize = body.split_inclusive("\n\n").take(3).map(str::len).sum();
                stream.write_all(format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()
                ).as_bytes()).await.unwrap();
                stream
                    .write_all(&body.as_bytes()[..prefix_len])
                    .await
                    .unwrap();
                release_rx.await.unwrap();
                stream
                    .write_all(&body.as_bytes()[prefix_len..])
                    .await
                    .unwrap();
                stream.shutdown().await.unwrap();

                let mut requests = Vec::new();
                for index in 0..foreground_count {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let request = read_json_request(&mut stream).await;
                    assert!(
                        request.to_string().contains("CHECKPOINT_READY"),
                        "{request}"
                    );
                    assert!(
                        !request
                            .to_string()
                            .contains("CONTEXT CHECKPOINT COMPACTION"),
                        "compaction restarted: {request}"
                    );
                    requests.push(request);
                    write_response(
                        &mut stream,
                        &one_shot_response(
                            &format!("answer_{index}"),
                            &format!("ANSWER_{index}"),
                            55,
                        ),
                    )
                    .await;
                }
                requests
            });

            let cwd = tempfile::tempdir().unwrap();
            let config_dir = cwd.path().join("config");
            std::fs::create_dir_all(&config_dir).unwrap();
            std::fs::write(
                config_dir.join("early.lua"),
                r#"smelt.builtins.disable({ plugins = { "title", "predict" } })"#,
            )
            .unwrap();
            let engine = engine::start(
                engine::EngineConfig::new(
                    cwd.path().to_path_buf(),
                    Arc::new(engine::clock::RealClock),
                ),
                Box::new(engine::tools::EmptyDispatcher),
            );
            let mut app = TestApp::builder()
                .with_cwd(cwd.path())
                .with_lua_load_paths(&config_dir, None)
                .with_engine(engine)
                .with_wall_time(std::time::UNIX_EPOCH + Duration::from_millis(SENT_AT_MS))
                .build();
            app.set_terminal_size(80, 24);
            app.use_model(smelt_core::config::ResolvedModel {
                key: "mock/compact".into(),
                provider_name: "mock".into(),
                model_name: "compact".into(),
                display_name: None,
                api_base: format!("http://{address}"),
                api_key_env: String::new(),
                provider_type: "openai".into(),
                config: protocol::ModelConfig::default(),
                catalog: protocol::ModelCatalogMetadata::default(),
            });
            app.set_context_window(Some(1_000_000));
            let mut settings = app.core_probe().config.settings.clone();
            settings.auto_compact = true;
            settings.compact_threshold = 0.8;
            settings.compact_keep_recent_groups = 1.0;
            app.set_settings_for_harness(settings);
            app.commit_request_history_item(
                protocol::HistoryItem::user(protocol::Content::text("OLD_USER")),
                None,
            );
            app.commit_request_history_item(
                protocol::HistoryItem::Assistant(protocol::AssistantStep::terminal(
                    Some(protocol::Content::text("OLD_ANSWER")),
                    None,
                    Vec::new(),
                )),
                None,
            );
            app.set_context_token_baseline_for_harness(Some(if recovery { 20 } else { 900_000 }));
            app.start_submitted_turn("CURRENT_TASK");
            let original_turn = app.current_turn_id();
            let mut release = Some(release_tx);
            let mut completed = 0;
            let expected_turns = foreground_count + usize::from(matches!(action, "pop" | "multi"));
            let mut terminal_output = Vec::new();
            tokio::time::timeout(std::time::Duration::from_secs(15), async {
                while completed < expected_turns {
                    let output = app
                        .app
                        .core
                        .engine
                        .recv_output()
                        .await
                        .expect("engine output");
                    let summary_delta = matches!(&output, engine::EngineOutput::Event(
                        protocol::EngineEvent::EngineAskDelta { delta, .. }
                    ) if delta.contains("CHECKPOINT_READY"));
                    if matches!(
                        &output,
                        engine::EngineOutput::Event(protocol::EngineEvent::TurnComplete { .. })
                    ) {
                        completed += 1;
                    }
                    app.app.dispatch_selected_engine_output_in_render_loop_to(
                        output,
                        &mut terminal_output,
                    );
                    if summary_delta {
                        assert_eq!(app.working_probe().phase_label(), Some("compacting"));
                        app.clock.advance(Duration::from_secs(1));
                        app.type_text("QUEUED_FIRST");
                        if action == "steer" {
                            app.press_mod(KeyCode::Char('q'), KeyModifiers::CONTROL);
                        } else {
                            app.press(KeyCode::Enter);
                            if action == "multi" {
                                for text in ["QUEUED_SECOND", "QUEUED_THIRD"] {
                                    app.type_text(text);
                                    app.press(KeyCode::Enter);
                                }
                            }
                            app.press(KeyCode::Enter);
                        }
                        if matches!(action, "pop" | "multi") {
                            app.press(KeyCode::Enter);
                            app.press(KeyCode::Enter);
                        } else if action == "withdraw" {
                            app.press(KeyCode::Esc);
                            app.press(KeyCode::Esc);
                            assert_eq!(app.queued_message_count(), 0);
                        }
                        assert_eq!(
                            app.current_turn_id(),
                            original_turn,
                            "recovery={recovery}, action={action}"
                        );
                        assert_eq!(app.working_probe().phase_label(), Some("compacting"));
                        app.clock.advance(Duration::from_secs(60));
                        release.take().expect("one compaction").send(()).unwrap();
                    }
                    app.render_to_frame();
                }
            })
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "compaction timed out: recovery={recovery}, action={action}, frame:\n{}",
                    app.render_to_frame().text()
                )
            });
            let requests = server.await.unwrap();
            for (index, request) in requests.iter().enumerate() {
                let text = request["input"].to_string();
                assert_eq!(
                    text.matches("QUEUED_FIRST").count(),
                    usize::from(action != "withdraw"),
                    "recovery={recovery}, action={action}: {text}"
                );
                assert!(text.contains("CURRENT_TASK"), "{text}");
                assert!(!text.contains("OLD_USER"), "{text}");
                if action == "multi" {
                    assert_eq!(
                        text.matches("QUEUED_SECOND").count(),
                        usize::from(index >= 1)
                    );
                    assert_eq!(
                        text.matches("QUEUED_THIRD").count(),
                        usize::from(index >= 2)
                    );
                }
            }
            assert!(!app.agent_running());
            assert_eq!(app.queued_message_count(), 0);
            app.save_session_and_flush();
            let session_id = app.session_snapshot().id;
            let history = crate::app::history::materialize_full_session(
                &app.core_probe().sessions,
                &session_id,
                crate::app::history::FullSessionMaterializationReason::TestSavedSessionAssertion,
            )
            .expect("saved canonical history")
            .history;
            for (content, expected_time) in [
                ("CURRENT_TASK", SENT_AT_MS),
                ("QUEUED_FIRST", SENT_AT_MS + 1_000),
                ("QUEUED_SECOND", SENT_AT_MS + 1_000),
                ("QUEUED_THIRD", SENT_AT_MS + 1_000),
            ] {
                if (content == "QUEUED_FIRST" && action == "withdraw")
                    || (matches!(content, "QUEUED_SECOND" | "QUEUED_THIRD") && action != "multi")
                {
                    continue;
                }
                assert!(
                    history.iter().any(|item| matches!(item,
                        protocol::HistoryItem::User { content: actual, sent_at_ms: Some(time), .. }
                            if actual.text_content() == content && *time == expected_time
                    )),
                    "submission time lost: recovery={recovery}, action={action}, content={content}, history={history:?}"
                );
            }
            let text = serde_json::to_string(&history).unwrap();
            assert_eq!(
                text.matches("QUEUED_FIRST").count(),
                usize::from(action != "withdraw"),
                "persisted history, recovery={recovery}, action={action}: {text}"
            );
            assert!(text.contains("CURRENT_TASK"), "{text}");
            assert!(
                text.contains("OLD_USER"),
                "canonical prefix was lost: {text}"
            );
            if action == "multi" {
                assert_eq!(text.matches("QUEUED_SECOND").count(), 1);
                assert_eq!(text.matches("QUEUED_THIRD").count(), 1);
            }
        }
    }
}
