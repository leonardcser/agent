use super::*;

fn next_synthetic_invocation_id() -> protocol::InvocationId {
    static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    protocol::InvocationId::new(NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

impl TestApp {
    pub fn push_context_recalculation(&mut self, label: impl Into<String>) {
        let _token = self
            .app
            .busy_stack
            .push_context_recalculation_token(label.into());
    }

    pub(crate) fn push_transcript_block(&mut self, block: smelt_core::transcript_model::Block) {
        self.app.push_block(block);
    }

    pub(crate) fn start_tool(
        &mut self,
        call_id: String,
        name: String,
        summary: protocol::StyledLines,
        args: std::collections::HashMap<String, serde_json::Value>,
    ) -> protocol::InvocationId {
        let invocation_id = next_synthetic_invocation_id();
        let called_at_ms = self.tool_called_at_ms();
        self.app
            .start_tool_at(invocation_id, call_id, name, summary, args, called_at_ms);
        invocation_id
    }

    pub(crate) fn finish_tool(
        &mut self,
        invocation_id: protocol::InvocationId,
        status: smelt_core::transcript_model::ToolStatus,
        output: Option<smelt_core::transcript_model::ToolOutputRef>,
        engine_elapsed: Option<std::time::Duration>,
    ) {
        self.app
            .finish_tool(invocation_id, status, output, engine_elapsed);
    }

    pub(crate) fn dispatch_host_call(&mut self, call: engine::HostCall) {
        self.app.dispatch_host_call(call);
    }

    pub(crate) fn dispatch_confirm_request(
        &mut self,
        request: smelt_core::ConfirmRequest,
        pending: &mut Vec<crate::app::PendingTool>,
    ) -> crate::app::SessionControl {
        let mut turn = self
            .app
            .conversation
            .clear_active()
            .expect("test turn is active");
        turn.pending = std::mem::take(pending);
        let control = self.app.dispatch_control(
            crate::app::SessionControl::NeedsConfirm(Box::new(request)),
            &mut turn,
        );
        *pending = std::mem::take(&mut turn.pending);
        self.app.conversation.set_active(Some(turn));
        control
    }

    pub(crate) fn start_custom_command_turn(
        &mut self,
        command: smelt_core::custom_commands::CustomCommand,
    ) -> bool {
        let sent_at_ms = engine::clock::unix_time_ms(self.app.core.clock.as_ref());
        let Some(turn) = self.app.begin_custom_command_turn(command, sent_at_ms) else {
            return false;
        };
        self.app.conversation.set_active(Some(turn));
        true
    }

    pub(crate) fn start_command_request_turn(
        &mut self,
        display: String,
        evaluated: String,
        overrides: smelt_core::custom_commands::CommandOverrides,
        start: crate::app::CommandTurnStart,
    ) -> bool {
        let Some(turn) = self.app.begin_command_request_turn(
            display,
            evaluated,
            overrides,
            start,
            engine::clock::unix_time_ms(self.app.core.clock.as_ref()),
        ) else {
            return false;
        };
        self.app.conversation.set_active(Some(turn));
        true
    }

    pub(crate) fn open_picker(
        &mut self,
        items: Vec<crate::picker::PickerItem>,
        selected: usize,
        placement: crate::picker::PickerPlacement,
        focusable: bool,
        close_on_select: bool,
        max_rows: u16,
    ) -> Option<WinId> {
        crate::picker::open(
            &mut self.app,
            items,
            selected,
            placement,
            focusable,
            close_on_select,
            max_rows,
        )
    }

    pub(crate) fn set_picker_items(
        &mut self,
        leaf: WinId,
        items: Vec<crate::picker::PickerItem>,
        selected: usize,
    ) {
        crate::picker::set_items(&mut self.app, leaf, items, selected);
    }

    pub(crate) fn set_picker_selected(&mut self, leaf: WinId, selected: usize) {
        crate::picker::set_selected(&mut self.app, leaf, selected);
    }

    pub(crate) fn forget_picker(&mut self, leaf: WinId) {
        crate::picker::forget(&mut self.app, leaf);
    }

    pub(crate) fn scroll_at(&mut self, row: u16, col: u16, rows: isize) -> bool {
        self.app.ui.scroll_at(row, col, rows)
    }

    pub(crate) fn open_readonly_overlay_fixture(
        &mut self,
        lines: Vec<String>,
        unselectable_prefix_len: Option<usize>,
    ) -> WinId {
        let buf = self
            .app
            .ui
            .buf_create(crate::smelt_edit::BufCreateOpts::default());
        {
            let buffer = self.app.ui.buf_mut(buf).expect("overlay buffer");
            buffer.readonly = true;
            buffer.set_all_lines(lines);
            if let Some(end) = unselectable_prefix_len {
                buffer.add_highlight_group_with_meta(
                    0,
                    0,
                    end.min(u16::MAX as usize) as u16,
                    smelt_buffer::theme::intern("Normal"),
                    crate::smelt_edit::SpanMeta::unselectable(),
                );
            }
        }
        let leaf = self
            .app
            .ui
            .win_open_split(
                buf,
                crate::smelt_edit::SplitConfig {
                    region: "dialog".into(),
                    gutters: Default::default(),
                },
            )
            .expect("overlay leaf");
        if let Some(win) = self.app.ui.win_mut(leaf) {
            win.set_surface(crate::smelt_edit::WindowSurface::readonly_text());
            win.set_vim_enabled(true);
        }
        self.app.ui.overlay_open(
            crate::smelt_edit::Overlay::new(
                crate::smelt_edit::LayoutTree::leaf(leaf),
                crate::smelt_edit::layout::Anchor::ScreenCenter,
            )
            .with_size((40, 5))
            .modal(true),
        );
        leaf
    }

    pub(crate) fn set_window_lines(&mut self, win: WinId, lines: Vec<String>) {
        let buf = self.app.ui.win(win).expect("window").buf;
        self.app
            .ui
            .buf_mut(buf)
            .expect("window buffer")
            .set_all_lines(lines);
    }

    pub(crate) fn set_window_cursor(&mut self, win: WinId, cpos: usize) {
        self.app.ui.win_mut(win).expect("window").set_cpos(cpos);
    }

    pub(crate) fn set_overlay_size_override(
        &mut self,
        overlay: crate::smelt_edit::OverlayId,
        size: (u16, u16),
    ) {
        if let Some(overlay) = self.app.ui.overlay_mut(overlay) {
            overlay.size_override = Some(size);
        }
    }

    /// Side-channel: insert a synthetic image attachment at the prompt
    /// cursor. Mirrors clipboard-image paste / `:image` paths without
    /// needing a real terminal clipboard. Exercises the
    /// attachment_ids ↔ marker invariant under interleaved mutations.
    pub fn insert_attachment(&mut self, label: String) {
        self.insert_image_attachment(label, "data:image/png;base64,FUZZ-0".to_string());
    }

    pub(crate) fn insert_image_attachment(&mut self, label: String, data_url: String) {
        let mut ctx = crate::input::prompt_ctx_mut(&mut self.app.ui);
        self.app
            .prompt
            .insert_image_for_harness(&mut ctx, label, data_url);
    }

    /// Side-channel: flip pane focus between Prompt and Content. In
    /// production this requires a Ctrl-W chord inside `PANE_CHORD_WINDOW`;
    /// the harness bypasses the timing gate so coverage doesn't depend on
    /// random key collisions.
    pub fn toggle_pane_focus(&mut self) {
        self.app.toggle_pane_focus();
    }

    /// Side-channel: install a placeholder on the prompt window with given
    /// accept / dismiss chords. Mirrors what Lua's `Win:placeholder(text, opts)`
    /// does; the dispatch path then runs on the next matching key. Without a
    /// side channel the placeholder is reachable only through Lua, which
    /// limits coverage of the accept/dismiss key-routing branches.
    pub fn install_prompt_placeholder(
        &mut self,
        text: String,
        accept: Vec<crate::smelt_edit::KeyBind>,
        dismiss: Vec<crate::smelt_edit::KeyBind>,
    ) {
        let win = self.app.well_known.prompt;
        if text.is_empty() {
            self.app.clear_placeholder(win);
            return;
        }
        self.app.set_placeholder(win, text);
        self.app.set_placeholder_options(
            win,
            crate::app::PlaceholderOpts {
                accept_keys: accept,
                dismiss_keys: dismiss,
            },
        );
    }

    /// Side-channel: clear the prompt placeholder (both extmark and opts).
    pub fn clear_prompt_placeholder(&mut self) {
        let win = self.app.well_known.prompt;
        self.app.clear_placeholder(win);
    }

    /// Side-channel: open a synthetic overlay via `smelt.overlay.new`.
    /// `variant % N` picks from a small fixed set spanning the new
    /// surface area (leaf, vbox, with static measure, with keymap,
    /// named vs anonymous). Same-variant repeats land on the same
    /// NamedSlot name so the dedup path runs; different variants
    /// allocate fresh slots. Best-effort: a Lua failure is swallowed
    /// (the next op still runs against a consistent app).
    pub fn open_synthetic_overlay(&mut self, variant: u8) {
        const VARIANTS: &[&str] = &[
            // 0: named leaf
            r#"
            local b = smelt.buf.new({ name = "fuzz.ov.0.buf" })
            local w = smelt.win.new(b, { name = "fuzz.ov.0.win" })
            smelt.overlay.new({
                name = "fuzz.ov.0", anchor = "screen_at", corner = "nw",
                row = 0, col = 0, width = 20, height = 5,
                layout = smelt.ui.layout.leaf(w),
            })
            "#,
            // 1: anonymous leaf - reaped on reload
            r#"
            local b = smelt.buf.new()
            local w = smelt.win.new(b, {})
            smelt.overlay.new({
                anchor = "screen_at", corner = "ne",
                row = 0, col = 0, width = 15, height = 4,
                layout = smelt.ui.layout.leaf(w),
            })
            "#,
            // 2: leaf with static measure (drives the per-leaf measure hook)
            r#"
            local b = smelt.buf.new({ name = "fuzz.ov.2.buf" })
            local w = smelt.win.new(b, { name = "fuzz.ov.2.win" })
            smelt.overlay.new({
                name = "fuzz.ov.2", anchor = "screen_at", corner = "sw",
                row = 0, col = 0, width = 25, height = 6,
                layout = smelt.ui.layout.leaf(w, { measure = { w = 18, h = 4 } }),
            })
            "#,
            // 3: vbox of two leaves
            r#"
            local b1 = smelt.buf.new({ name = "fuzz.ov.3.buf1" })
            local w1 = smelt.win.new(b1, { name = "fuzz.ov.3.win1" })
            local b2 = smelt.buf.new({ name = "fuzz.ov.3.buf2" })
            local w2 = smelt.win.new(b2, { name = "fuzz.ov.3.win2" })
            smelt.overlay.new({
                name = "fuzz.ov.3", anchor = "screen_at", corner = "se",
                row = 0, col = 0, width = 22, height = 8,
                layout = smelt.ui.layout.vbox({
                    { node = smelt.ui.layout.leaf(w1), height = 3 },
                    { node = smelt.ui.layout.leaf(w2), height = 3 },
                }),
            })
            "#,
            // 4: leaf with overlay-level keymap (deferred-safe path)
            r#"
            local b = smelt.buf.new({ name = "fuzz.ov.4.buf" })
            local w = smelt.win.new(b, { name = "fuzz.ov.4.win" })
            smelt.overlay.new({
                name = "fuzz.ov.4", anchor = "screen_at", corner = "nw",
                row = 1, col = 1, width = 18, height = 5,
                layout = smelt.ui.layout.leaf(w),
                keymaps = {
                    { key = "<C-x>", on_press = function() end },
                },
            })
            "#,
        ];
        let snippet = VARIANTS[(variant as usize) % VARIANTS.len()];
        let lua = self.app.lua.lua().clone();
        let _ = crate::lua::scope_app(&mut self.app, || lua.load(snippet).exec());
    }

    /// Append a canonical user turn and its transcript block without a real
    /// engine roundtrip.
    pub fn push_user_block(&mut self, text: &str) {
        let sent_at_ms = engine::clock::unix_time_ms(self.clock.as_ref());
        self.app.stage_request_history_item(
            protocol::HistoryItem::user(protocol::Content::text(text)).with_sent_at_ms(sent_at_ms),
            Some(smelt_core::transcript_model::Block::User {
                text: text.to_string(),
                image_labels: Vec::new(),
                command: false,
                sent_at_ms: Some(sent_at_ms),
            }),
        );
    }

    /// Append a command-marked user block to the transcript history.
    pub fn push_command_block(&mut self, text: &str) {
        let sent_at_ms = engine::clock::unix_time_ms(self.clock.as_ref());
        self.app
            .push_block(smelt_core::transcript_model::Block::User {
                text: text.to_string(),
                image_labels: Vec::new(),
                command: true,
                sent_at_ms: Some(sent_at_ms),
            });
    }

    /// Push a `Block::Compacted` summary block into the transcript -
    /// the same committed marker installed after a successful compaction
    /// checkpoint. Stories use this to snapshot the final compaction chrome
    /// without running a real `engine.ask` round-trip.
    pub fn push_compacted(&mut self, summary: &str) {
        self.app
            .push_block(smelt_core::transcript_model::Block::Compacted {
                summary: summary.to_string(),
            });
    }

    /// Push or rewrite the transient compaction preview block shown while
    /// the compact plugin streams a checkpoint summary.
    pub fn push_compaction_preview(&mut self, summary: &str) {
        self.app.update_compaction_preview(summary.to_string());
    }

    /// Push a typed process-status block into the transcript. Mirrors the
    /// display block produced when a background process completion is
    /// committed to history, without needing to spawn a real process.
    pub fn push_process_status(&mut self, text: &str, event: Option<protocol::ProcessStatusEvent>) {
        self.app
            .push_block(smelt_core::transcript_model::Block::ProcessStatus {
                text: text.to_string(),
                event,
            });
    }

    /// Push a `Block::Mode` into the transcript without restoring a fixture.
    pub fn push_mode_block(&mut self, text: &str, icon: &str, hl_group: &str) {
        self.app
            .push_block(smelt_core::transcript_model::Block::Mode {
                text: text.to_string(),
                icon: icon.to_string(),
                hl_group: hl_group.to_string(),
            });
    }

    /// Push a `Block::CodeLine` into the transcript.
    pub fn push_code_line(&mut self, content: &str, lang: &str) {
        self.app
            .push_block(smelt_core::transcript_model::Block::CodeLine {
                content: content.to_string(),
                lang: lang.to_string(),
            });
    }

    /// Push an untyped process-status block into the transcript.
    pub fn push_process_status_text(&mut self, text: &str) {
        self.push_process_status(text, None);
    }

    /// Open a `Block::Exec` shell-escape block in the transcript with
    /// `command` as the header. Pair with
    /// `SourceEvent::ExecOutput`/`ExecDone` to stream output and close
    /// the block. The production path is `start_shell_escape`, which
    /// also spawns a real `sh -c`; stories don't want a subprocess, so
    /// the harness invokes the transcript hook and input-submit event
    /// directly.
    pub fn start_exec(&mut self, command: &str) {
        self.app.start_exec(command.to_string());
        self.app.publish_input_submit(format!("!{command}"));
        self.tick_signals();
    }

    /// Stream one line into the active synthetic shell-escape block.
    pub fn append_exec_output(&mut self, line: impl Into<String>) {
        self.feed_one(SourceEvent::ExecOutput(line.into()));
    }

    /// Complete the active synthetic shell-escape block.
    pub fn finish_exec(&mut self, exit_code: Option<i32>) {
        self.feed_one(SourceEvent::ExecDone(exit_code));
    }

    /// Feed one engine event through the active or idle dispatch path.
    pub fn engine_event(&mut self, event: EngineEvent) {
        self.feed_one(SourceEvent::engine(event));
    }

    fn tool_called_at_ms(&self) -> u64 {
        engine::clock::unix_time_ms(self.clock.as_ref())
    }

    pub fn tool_started(
        &mut self,
        call_id: impl Into<String>,
        tool_name: impl Into<String>,
        args: std::collections::HashMap<String, serde_json::Value>,
    ) -> protocol::InvocationId {
        let invocation_id = next_synthetic_invocation_id();
        let called_at_ms = self.tool_called_at_ms();
        self.engine_event(EngineEvent::ToolStarted {
            invocation_id,
            call_id: call_id.into(),
            tool_name: tool_name.into(),
            args,
            called_at_ms,
        });
        invocation_id
    }

    pub fn tool_finished(
        &mut self,
        invocation_id: protocol::InvocationId,
        call_id: impl Into<String>,
        result: protocol::ToolOutcome,
        elapsed_ms: Option<u64>,
    ) {
        self.engine_event(EngineEvent::ToolFinished {
            invocation_id,
            call_id: call_id.into(),
            result,
            elapsed_ms,
        });
    }

    pub fn tool_rejected(
        &mut self,
        call_id: impl Into<String>,
        tool_name: impl Into<String>,
        args: std::collections::HashMap<String, serde_json::Value>,
        summary: protocol::StyledLines,
        result: protocol::ToolOutcome,
        elapsed_ms: Option<u64>,
    ) {
        let called_at_ms = self.tool_called_at_ms();
        self.engine_event(EngineEvent::ToolRejected {
            invocation_id: next_synthetic_invocation_id(),
            call_id: call_id.into(),
            tool_name: tool_name.into(),
            args,
            summary,
            result,
            elapsed_ms,
            called_at_ms,
        });
    }

    pub fn request_permission(
        &mut self,
        request_id: u64,
        call_id: impl Into<String>,
        tool_name: impl Into<String>,
        args: std::collections::HashMap<String, serde_json::Value>,
        approval_patterns: Vec<String>,
        summary: protocol::StyledLines,
    ) {
        let called_at_ms = self.tool_called_at_ms();
        self.engine_event(EngineEvent::RequestPermission {
            request_id,
            invocation_id: next_synthetic_invocation_id(),
            call_id: call_id.into(),
            tool_name: tool_name.into(),
            args,
            approval_patterns,
            called_at_ms,
            summary,
        });
    }

    /// Cancel the active turn (or idle background tasks). Mirrors
    /// `EventOutcome::CancelAgent` → `discard_turn(true)`.
    pub fn cancel(&mut self) {
        self.app.discard_turn(crate::app::TurnEnd::Cancelled);
        self.drain_cmd();
    }

    /// Push text that has already been promoted into the active turn's request queue.
    pub fn steer(&mut self, text: &str) {
        if !text.is_empty() {
            self.app
                .prompt
                .try_queue_request(crate::app::QueuedInput::request_from_text(
                    text.to_string(),
                    text.to_string(),
                    engine::clock::unix_time_ms(self.app.core.clock.as_ref()),
                ));
        }
    }

    /// Remove up to `count` request-queued messages from the front.
    pub fn unsteer(&mut self, count: usize) {
        self.app.prompt.acknowledge_requests(count);
    }

    /// Send a `CallCoreTool` UiCommand to the engine channel.
    pub fn call_core_tool(
        &mut self,
        tool_name: &str,
        args: std::collections::HashMap<String, serde_json::Value>,
    ) {
        self.app
            .core
            .engine
            .send(protocol::UiCommand::CallCoreTool {
                request_id: 1,
                parent_invocation_id: next_synthetic_invocation_id(),
                parent_call_id: String::new(),
                tool_name: tool_name.to_string(),
                args,
            });
        self.drain_cmd();
    }

    /// Change the active agent mode.
    pub fn set_agent_mode(&mut self, mode: AgentMode) {
        self.app.core.config.mode = mode;
    }

    /// Push an `assistant` text block onto the transcript history so
    /// flows that read message history see a multi-turn conversation.
    pub fn push_assistant_text(&mut self, text: &str) {
        self.app
            .conversation
            .append_history_item(protocol::HistoryItem::Assistant(
                protocol::AssistantStep::terminal(
                    Some(protocol::Content::Text(text.to_string())),
                    None,
                    Vec::new(),
                ),
            ));
    }

    /// Seed one canonical database-backed session and publish its catalog overlay.
    pub fn seed_session_meta(&self, meta: &smelt_core::session::SessionMeta) {
        let mut writer =
            smelt_store::OwnedLineageWriter::open(self.app.core.sessions.sessions_dir(), &meta.id)
                .expect("create session fixture database");
        let history = if let Some(text_bytes) = meta.text_bytes.filter(|bytes| *bytes > 0) {
            let mut remaining = usize::try_from(text_bytes).expect("fixture text size fits usize");
            let mut history = Vec::new();
            while remaining > 0 {
                let chunk = remaining.min(60_000);
                history.push(protocol::HistoryItem::user(protocol::Content::text(
                    "x".repeat(chunk),
                )));
                remaining -= chunk;
            }
            history
        } else {
            (0..meta.history_len.unwrap_or_default())
                .map(|index| {
                    protocol::HistoryItem::user(protocol::Content::text(format!(
                        "test session fixture row {index}"
                    )))
                })
                .collect::<Vec<_>>()
        };
        let history_len = history.len();
        let command = smelt_store::SessionCommit {
            session_id: meta.id.clone(),
            expected: smelt_store::StoreHead::default(),
            identity: smelt_store::SessionIdentity {
                id: meta.id.clone(),
                created_at: i64::try_from(meta.created_at_ms).expect("fixture created_at fits i64"),
                parent_id: meta.parent_id.clone(),
            },
            metadata: smelt_store::SessionMetadata {
                title: meta.title.clone(),
                slug: meta.slug.clone(),
                first_user_message: meta.first_user_message.clone(),
                cwd: meta.cwd.clone(),
                mode: meta.mode.clone(),
                reasoning_effort: meta
                    .reasoning_effort
                    .as_ref()
                    .map(|effort| effort.label().to_string()),
                model: meta.model.clone(),
                fast_mode: meta.fast_mode,
                accounting_json: Some(serde_json::json!({
                    "session_usage": {},
                    "context_token_identity": meta
                        .authoritative_context_tokens
                        .as_ref()
                        .map(|context| &context.identity),
                    "display_context_token_identity": meta
                        .display_context_tokens
                        .as_ref()
                        .and_then(|context| context.identity.as_ref()),
                })),
                checkpoint_json: meta
                    .checkpoint
                    .as_ref()
                    .map(|checkpoint| serde_json::to_value(checkpoint).expect("valid checkpoint")),
                checkpoint_events_json: (!meta.checkpoint_events.is_empty()).then(|| {
                    serde_json::to_value(&meta.checkpoint_events).expect("valid checkpoint events")
                }),
                context_tokens: meta
                    .authoritative_context_tokens
                    .as_ref()
                    .map(|context| u64::from(context.tokens)),
                context_tokens_history_len: meta.authoritative_context_tokens.as_ref().map(
                    |context| {
                        u64::try_from(context.history_len)
                            .expect("fixture history coordinate fits u64")
                    },
                ),
                display_context_tokens: meta
                    .display_context_tokens
                    .as_ref()
                    .map(|context| u64::from(context.tokens)),
                session_cost_usd: smelt_store::SessionCostUsd::new(0.0)
                    .expect("valid fixture cost"),
                updated_at: i64::try_from(meta.updated_at_ms).expect("fixture updated_at fits i64"),
            },
            history: smelt_store::HistorySuffix {
                start: smelt_store::HistoryIndex::ZERO,
                final_len: smelt_store::HistoryLen::new(
                    u64::try_from(history_len).expect("fixture history length fits u64"),
                ),
                items: history,
            },
            side_tables: smelt_store::SideTableSuffixes::default(),
            transcript_records: None,
        };
        let receipt = writer
            .commit_session(&command)
            .expect("write canonical session fixture");
        writer.release().expect("release session fixture database");
        self.publish_session_catalog_commit(&command, &receipt);
    }
}
