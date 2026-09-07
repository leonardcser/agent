//! App-level story context.
//!
//! `StoryCtx` (in `mod.rs`) drives the bare `Ui`; `AppStoryCtx` drives
//! the full `TuiApp` through `TestApp`, so stories can feed engine
//! events and snapshot the composed transcript / prompt / status frame
//! the user actually sees.
//!
//! Background: `TestApp::render_normal` writes ANSI to real stdout as a
//! side effect, but it also composes the layer tree and updates the
//! `Ui` snapshot buffer in the same pass. Snapshotting via
//! `Ui::snapshot` after `render_normal` therefore returns the rendered
//! frame; the stdout noise is captured (and discarded) by nextest.
//!
//! Snapshot files use the same `.snap` / `.styles.snap` shape and
//! `.step-N` sequence convention as `StoryCtx`, so the existing viewer
//! at `examples/stories.rs` lists them without changes.
//!
//! Naming: every app-level story file lives under `stories/app_*.rs`
//! so its snapshots are namespaced (`app_transcript::…`) and can't
//! collide with the Ui-level snapshot space.
#![allow(dead_code)]

use insta::{assert_snapshot, with_settings};
use protocol::{EngineEvent, TokenUsage, ToolOutcome};
use tui::app::test_harness::{SourceEvent, TestApp};
use tui::smelt_edit::SnapshotFrame;

pub struct AppStoryCtx {
    app: TestApp,
    name: String,
    snapshot_index: u32,
    turn_id: u64,
    call_counter: u64,
}

impl AppStoryCtx {
    pub fn new(name: &str) -> Self {
        let mut app = TestApp::builder()
            .without_model()
            .with_wall_time(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_742_567_823))
            .build();
        app.run_lua_result("smelt.time.format = smelt.time.format_utc; smelt.theme.use('default')")
            .expect("pin storybook time formatting and theme");
        app.allow_permissions_outside_cwd();
        Self {
            app,
            name: name.to_string(),
            snapshot_index: 0,
            turn_id: 0,
            call_counter: 0,
        }
    }

    pub fn set_viewport(&mut self, w: u16, h: u16) {
        self.app.set_terminal_size(w, h);
    }

    pub fn wait_for_document(&mut self, name: &str) {
        self.app
            .wait_for_document(name, std::time::Duration::from_secs(5));
    }

    pub fn advance_time(&mut self, ms: u64) {
        self.app.feed_one(SourceEvent::Tick(ms));
    }

    pub fn write_workspace_file(&self, path: &str, content: &str) {
        let path = std::path::Path::new(self.app.cwd_str()).join(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create story workspace directory");
        }
        std::fs::write(path, content).expect("write story workspace file");
    }

    pub fn restrict_permissions_to_cwd(&mut self) {
        assert_eq!(
            self.turn_id, 0,
            "configure permissions before starting a turn"
        );
        self.app.restrict_permissions_to_cwd();
    }

    pub fn approve_tool_for_session(&mut self, tool: &str) {
        self.app.approve_tool_for_session(tool);
    }

    /// Expand the active root-docked dialog while retaining a small transcript
    /// viewport above it.
    pub fn expand_active_dialog_to_max_height(&mut self) {
        self.run_lua(
            "assert(smelt.dialog.current(), 'active docked dialog exists').toggle_expanded()",
        )
    }

    /// Set the configured context window so prompt-bar token percentages
    /// render in stories.
    pub fn set_context_window(&mut self, context_window: Option<u32>) {
        self.app.set_context_window(context_window);
    }

    /// Install a usable synthetic model for stories that exercise model-backed
    /// APIs. Other stories intentionally retain the no-model baseline.
    pub fn use_test_model(&mut self) {
        let model = smelt_core::config::ResolvedModel {
            key: "test/test-model".into(),
            provider_name: "test".into(),
            model_name: "test-model".into(),
            display_name: None,
            api_base: "https://example.invalid/v1".into(),
            api_key_env: String::new(),
            provider_type: "openai-compatible".into(),
            config: protocol::ModelConfig {
                name: Some("test-model".into()),
                ..Default::default()
            },
            catalog: protocol::ModelCatalogMetadata::default(),
        };
        self.app.use_model(model);
    }

    /// Begin an agent turn. Required before `engine(...)` events route
    /// through the active-turn dispatch path (transcript writes, tool
    /// tracking). Idempotent.
    pub fn start_turn(&mut self) {
        if self.turn_id == 0 {
            self.turn_id = 1;
            self.app.start_turn(self.turn_id);
        }
    }

    /// Feed one engine event. Auto-starts a turn so callers don't have
    /// to remember.
    pub fn engine(&mut self, ev: EngineEvent) {
        self.start_turn();
        self.app.engine_event(ev);
    }

    /// Push a `Block::Compacted` summary block - the same committed marker
    /// inserted after a successful compaction checkpoint. Use this to
    /// snapshot the final compaction chrome without driving a real
    /// `engine.ask` round-trip.
    pub fn push_compacted(&mut self, summary: &str) {
        self.app.push_compacted(summary);
    }

    /// Push the transient block shown while the compact plugin streams a
    /// checkpoint summary. The real plugin rewrites this block on each delta
    /// and `smelt.session.checkpoint` replaces it with a compacted marker.
    pub fn push_compaction_preview(&mut self, summary: &str) {
        self.app.push_compaction_preview(summary);
    }

    pub fn push_context_recalculation(&mut self, label: &str) {
        self.app.push_context_recalculation(label);
    }

    /// Push a typed background-process completion status block. This drives the
    /// same transcript block shape produced by the live job supervisor without
    /// spawning a subprocess in the story.
    pub fn push_background_process_completed(&mut self, id: &str, exit_code: Option<i32>) {
        let event = protocol::ProcessStatusEvent::background_process_completed(
            id,
            exit_code,
            protocol::JobTermination::Exited,
        );
        let text = event.display_text();
        self.app.push_process_status(&text, Some(event));
    }

    pub fn push_process_status_text(&mut self, text: &str) {
        self.app.push_process_status_text(text);
    }

    pub fn push_mode_block(&mut self, text: &str, icon: &str, hl_group: &str) {
        self.app.push_mode_block(text, icon, hl_group);
    }

    pub fn push_code_line(&mut self, content: &str, lang: &str) {
        self.app.push_code_line(content, lang);
    }

    /// Run a shell-escape (`Block::Exec`) lifecycle: open the block,
    /// stream `output` line by line, then replace the preview with the
    /// authoritative final output. This uses the same pipeline as the live
    /// `!cmd` flow, so the rendered block is byte-identical to what users see.
    pub fn exec_with_output(&mut self, command: &str, output: &str) {
        self.app.start_exec(command);
        for line in output.lines() {
            self.app.append_exec_output(line.to_owned());
        }
        self.app.finish_exec(None);
    }

    /// Drive a tool call lifecycle: `ToolStarted(args)` immediately
    /// followed by `ToolFinished(result)`. Hides the boilerplate around
    /// allocating the args map, picking a `call_id`, and assembling the
    /// `ToolOutcome`. Stories that need the pending-only state should
    /// use [`tool_started`] (and never call `tool_finished`); stories
    /// that need a custom `metadata` payload should use
    /// [`tool_call_with_metadata`].
    pub fn tool_call(
        &mut self,
        tool_name: &str,
        args: &[(&str, serde_json::Value)],
        content: &str,
        elapsed_ms: Option<u64>,
    ) {
        self.tool_call_full(
            tool_name,
            args,
            ToolOutcome::new(content.into(), false, None),
            elapsed_ms,
        );
    }

    /// Tool call lifecycle with an `is_error = true` result. Same shape
    /// as [`tool_call`] but flips the error flag so tool render paths
    /// that branch on `output.is_error` get exercised.
    pub fn tool_call_error(
        &mut self,
        tool_name: &str,
        args: &[(&str, serde_json::Value)],
        content: &str,
        elapsed_ms: Option<u64>,
    ) {
        self.tool_call_full(
            tool_name,
            args,
            ToolOutcome::new(content.into(), true, None),
            elapsed_ms,
        );
    }

    /// Tool call lifecycle with bounded structured `metadata`.
    pub fn tool_call_with_metadata(
        &mut self,
        tool_name: &str,
        args: &[(&str, serde_json::Value)],
        content: &str,
        metadata: serde_json::Value,
        elapsed_ms: Option<u64>,
    ) {
        self.tool_call_full(
            tool_name,
            args,
            ToolOutcome::new(content.into(), false, Some(metadata)),
            elapsed_ms,
        );
    }

    /// Tool call lifecycle with bounded metadata and retained display content.
    pub fn tool_call_with_display_content(
        &mut self,
        tool_name: &str,
        args: &[(&str, serde_json::Value)],
        content: &str,
        metadata: serde_json::Value,
        display_content: &[(&str, &str)],
        elapsed_ms: Option<u64>,
    ) {
        let outcome = ToolOutcome::new(content.into(), false, Some(metadata)).with_display_content(
            display_content
                .iter()
                .map(|(name, content)| {
                    protocol::ToolDisplayContent::new(*name, (*content).to_owned())
                })
                .collect(),
        );
        self.tool_call_full(tool_name, args, outcome, elapsed_ms);
    }

    /// Emit only `ToolStarted` - the pending state. Use this when the
    /// snapshot should capture the streaming/spinner chrome before any
    /// result arrives.
    pub fn tool_started(&mut self, tool_name: &str, args: &[(&str, serde_json::Value)]) {
        let call_id = self.next_call_id(tool_name);
        self.start_turn();
        self.app.tool_started(
            call_id,
            tool_name,
            args.iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
        );
    }

    /// Emit a display-only streaming tool-call draft. The final tool
    /// execution path is still driven by `tool_started` / `tool_finished`.
    pub fn tool_draft(&mut self, tool_name: &str, args_json: &str) {
        self.call_counter += 1;
        let stream_id = format!("draft-{}", self.call_counter);
        let call_id = format!("{tool_name}-draft-{}", self.call_counter);
        self.engine(EngineEvent::ToolCallDraftStarted {
            stream_id: stream_id.clone(),
            call_id: Some(call_id.clone()),
            tool_name: Some(tool_name.into()),
        });
        self.engine(EngineEvent::ToolCallDraftDelta {
            stream_id: stream_id.clone(),
            call_id: Some(call_id.clone()),
            tool_name: Some(tool_name.into()),
            delta: args_json.into(),
        });
        self.engine(EngineEvent::ToolCallDraftFinished {
            stream_id,
            call_id,
            tool_name: tool_name.into(),
            arguments: args_json.into(),
        });
    }

    /// Emit one in-flight streaming tool-call draft delta without the finished
    /// event. This captures the state users see while arguments are still
    /// arriving from the provider.
    pub fn tool_draft_delta(&mut self, tool_name: &str, args_json_delta: &str) {
        self.call_counter += 1;
        let stream_id = format!("draft-{}", self.call_counter);
        let call_id = format!("{tool_name}-draft-{}", self.call_counter);
        self.engine(EngineEvent::ToolCallDraftStarted {
            stream_id: stream_id.clone(),
            call_id: Some(call_id.clone()),
            tool_name: Some(tool_name.into()),
        });
        self.engine(EngineEvent::ToolCallDraftDelta {
            stream_id,
            call_id: Some(call_id),
            tool_name: Some(tool_name.into()),
            delta: args_json_delta.into(),
        });
    }

    fn tool_call_full(
        &mut self,
        tool_name: &str,
        args: &[(&str, serde_json::Value)],
        outcome: ToolOutcome,
        elapsed_ms: Option<u64>,
    ) {
        let call_id = self.next_call_id(tool_name);
        self.start_turn();
        let invocation_id = self.app.tool_started(
            call_id.clone(),
            tool_name,
            args.iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
        );
        self.app
            .tool_finished(invocation_id, call_id, outcome, elapsed_ms);
    }

    pub fn tool_rejected(
        &mut self,
        tool_name: &str,
        args: &[(&str, serde_json::Value)],
        content: &str,
        is_error: bool,
        summary: protocol::StyledLines,
        elapsed_ms: Option<u64>,
    ) {
        let call_id = self.next_call_id(tool_name);
        self.start_turn();
        self.app.tool_rejected(
            call_id,
            tool_name,
            args.iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
            summary,
            ToolOutcome::new(content.into(), is_error, None),
            elapsed_ms,
        );
    }

    fn next_call_id(&mut self, tool_name: &str) -> String {
        self.call_counter += 1;
        format!("{tool_name}-{}", self.call_counter)
    }

    /// Drive a real permission flow. Invokes the tool's `summary(args)`
    /// Lua callback (the same path the engine uses in production) so
    /// the dialog header carries the tool's syntax-highlighted summary
    /// instead of a hand-rolled plain string. The dialog then renders
    /// the tool's `preview(args)` body through the real Lua → buffer
    /// pipeline.
    pub fn request_permission(
        &mut self,
        tool_name: &str,
        args: std::collections::HashMap<String, serde_json::Value>,
        approval_patterns: Vec<String>,
    ) {
        let summary = self.app.tool_summary(tool_name, &args);
        self.app
            .require_tool_confirmation(tool_name, &approval_patterns);
        self.start_turn();
        self.app
            .request_permission(1, "call-1", tool_name, args, approval_patterns, summary);
    }

    /// Write `contents` to `<cwd>/<rel_path>` and return the relative
    /// path (`./<rel_path>` form), suitable for direct use as a tool
    /// argument. Use this from dialog stories that exercise tool
    /// previews that read the file (notebook_edit, edit_file with
    /// file-based staleness checks). The fixture lives under the test
    /// cwd (`<HOME>/cwd`) so `smelt.path.display` collapses it to a
    /// clean relative path in the dialog header and snapshots stay
    /// byte-stable across machines.
    pub fn write_fixture(&self, rel_path: &str, contents: &str) -> String {
        let cwd = std::path::PathBuf::from(self.app.cwd_str());
        let path = cwd.join(rel_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dir");
        }
        std::fs::write(&path, contents).expect("write story fixture");
        path.to_string_lossy().into_owned()
    }

    /// Record a fixture as read so edit previews exercise the same cache gate as
    /// a real read_file followed by edit_file.
    pub fn cache_file_read(&mut self, path: &str, contents: &str) {
        self.run_lua(&format!(
            "smelt.fs.file_state.record_read({}, {}, 0, {})",
            serde_json::to_string(path).expect("serialize fixture path"),
            serde_json::to_string(contents).expect("serialize fixture content"),
            contents.len(),
        ));
    }

    /// Working directory the live `TuiApp` captured at construction.
    /// Stories that seed persisted-session fixtures must match this
    /// into catalog metadata so the resume dialog's default workspace filter
    /// keeps the seeded entries visible.
    pub fn app_cwd(&self) -> &str {
        self.app.cwd_str()
    }

    /// Write a canonical database-backed session fixture and publish its catalog overlay.
    pub fn write_session_meta(&self, meta: &smelt_core::session::SessionMeta) {
        self.app.seed_session_meta(meta);
    }

    /// Reconcile canonical session fixtures before a story opens the resume list.
    pub fn wait_for_session_catalog(&self) {
        self.app
            .reconcile_session_catalog()
            .expect("reconcile storybook session catalog");
    }

    /// Commit a fresh Lua generation through the production reload pipeline.
    pub fn reload_lua(&mut self) {
        self.app.reload_lua();
        self.run_lua("smelt.time.format = smelt.time.format_utc");
    }

    /// Type a string into the prompt as individual key events.
    pub fn type_prompt(&mut self, s: &str) {
        self.app.type_text(s);
    }

    pub fn preview_transcript_search(&mut self, query: &str) {
        self.app.focus_transcript();
        self.app
            .configure_transcript_vim(true, tui::smelt_edit::VimMode::Normal);
        self.app.type_char('/');
        self.app.type_text(query);
        self.app.render_silent();
    }

    /// Press `ctrl+s` to toggle the prompt stash. Use this to drive the
    /// stash chrome (the `◌ Stashed` row) from a story.
    pub fn stash_prompt(&mut self) {
        self.app.press_mod(
            crossterm::event::KeyCode::Char('s'),
            crossterm::event::KeyModifiers::CONTROL,
        );
    }

    /// Press Enter and pump Lua tasks so dialog submit handlers that resolve a
    /// coroutine can open their follow-up UI before the next snapshot.
    pub fn press_enter(&mut self) {
        self.app.press(crossterm::event::KeyCode::Enter);
        self.app.settle_lua();
    }

    /// Press a plain character key.
    pub fn press_char(&mut self, ch: char) {
        self.press_key(
            crossterm::event::KeyCode::Char(ch),
            crossterm::event::KeyModifiers::NONE,
        );
    }

    pub fn press_key(
        &mut self,
        key: crossterm::event::KeyCode,
        modifiers: crossterm::event::KeyModifiers,
    ) {
        self.app.press_mod(key, modifiers);
        self.app.settle_lua();
    }

    /// Press Tab and pump Lua callbacks so focus-changing dialog keymaps settle
    /// before the next synthetic input.
    pub fn press_tab(&mut self) {
        self.app.press(crossterm::event::KeyCode::Tab);
        self.pump_lua();
    }

    /// Promote the oldest queued next-turn message into the next-request queue.
    pub fn promote_next_queued_message(&mut self) {
        self.app.press(crossterm::event::KeyCode::Enter);
    }

    /// Push a synthetic queued user message. In production these arrive
    /// by pressing Enter on the prompt while a turn is active; the
    /// harness side-channels them straight onto `app.queued_inputs`.
    /// Auto-starts a turn so the top bar's `prompt.queued()` accessor
    /// (which gates on `agent.is_some() || busy`) surfaces the entries.
    pub fn push_queued_message(&mut self, text: &str) {
        self.start_turn();
        self.app.push_queued_message(text.to_string());
    }

    /// Seed a prompt-history entry so reverse-history picker stories can open
    /// without submitting a real engine turn.
    pub fn push_history_entry(&mut self, text: &str) {
        self.app.push_history_entry(text.to_string());
    }

    /// Seed model picker entries in the live config. Each tuple is
    /// `(provider, model_name, provider_type)`.
    pub fn seed_models(&mut self, models: &[(&str, &str, &str)]) {
        self.app.set_available_models(
            models
                .iter()
                .map(
                    |(provider, model, provider_type)| smelt_core::config::ResolvedModel {
                        key: format!("{provider}/{model}"),
                        provider_name: (*provider).to_string(),
                        model_name: (*model).to_string(),
                        display_name: None,
                        api_base: format!("https://{provider}.example/v1"),
                        api_key_env: format!("{}_API_KEY", provider.to_ascii_uppercase()),
                        provider_type: (*provider_type).to_string(),
                        config: smelt_core::config::ModelConfig {
                            name: Some((*model).to_string()),
                            ..Default::default()
                        },
                        catalog: protocol::ModelCatalogMetadata::default(),
                    },
                )
                .collect(),
        );
    }

    /// Execute a Lua snippet against the embedded runtime. Used by
    /// dialog stories that need to seed state or invoke a primitive
    /// directly.
    pub fn run_lua(&mut self, snippet: &str) {
        self.app
            .run_lua_result(snippet)
            .expect("story Lua snippet failed");
    }

    /// Pump spawned Lua coroutines to their next yield point.
    pub fn pump_lua(&mut self) {
        self.app.settle_lua();
    }

    /// Seed the latest provider-reported context token count through the
    /// normal engine event path so prompt-bar stories exercise the live
    /// bookkeeping.
    pub fn set_context_tokens(&mut self, context_tokens: u32) {
        self.engine(EngineEvent::TokenUsage {
            usage: TokenUsage {
                context_tokens: Some(context_tokens),
                ..TokenUsage::default()
            },
            tokens_per_sec: None,
            cost_usd: None,
            background: false,
        });
    }

    /// Seed a user turn on the transcript. Required by `/rewind` (and
    /// any other flow that reads `smelt.session.turns()`).
    pub fn push_user_turn(&mut self, text: &str) {
        self.app.push_user_block(text);
    }

    /// Seed a command-marked user turn on the transcript.
    pub fn push_command_turn(&mut self, text: &str) {
        self.app.push_command_block(text);
    }

    /// Seed an assistant message on `session.messages`. Required by
    /// flows that read history (e.g. `/btw`).
    pub fn push_assistant_text(&mut self, text: &str) {
        self.app.push_assistant_text(text);
    }

    /// Smallest pending `smelt.engine.ask` callback id, if any. Used
    /// by stories that drive `/btw` (or any other ask-callback flow)
    /// to feed a matching `EngineAskResponse` once the spawned
    /// coroutine has yielded and the callback is registered.
    pub fn pending_ask_id(&self) -> Option<u64> {
        self.app.pending_ask_id()
    }

    /// Show a notification toast via `smelt.notify` and pump the render
    /// so the toast overlay is created before the next snapshot.
    pub fn notify(&mut self, body: &str, source: Option<&str>) {
        let source = source.unwrap_or("story");
        let snippet = format!("smelt.notify.info({body:?}, {source:?})");
        self.run_lua(&snippet);
        self.app.settle_lua();
    }

    /// Run a slash command as if the user typed it in the cmdline.
    /// Goes through the real `smelt.cmd.run` path → command handler →
    /// `smelt.spawn` → `smelt.dialog.open` (yielding coroutine). After
    /// dispatch we pump the task runtime so the spawned coroutine
    /// progresses to its first yield (the dialog open), which is where
    /// stories want to snapshot.
    pub fn run_command(&mut self, line: &str) {
        // mlua's lua-to-string round-trip: `string.format("%q", line)`
        // would also work, but ASCII command lines don't need escaping
        // beyond `\"`. Stories pass plain ASCII so a debug-format quote
        // matches Lua's string literal syntax.
        let snippet = format!("smelt.cmd.run({line:?})");
        self.run_lua(&snippet);
        // The handler ran synchronously and called `smelt.spawn`, which
        // enqueued a coroutine on the task runtime. Pump the runtime to
        // run it until it yields at `smelt.dialog.open`.
        self.app.settle_lua();
    }

    fn frame(&mut self) -> SnapshotFrame {
        self.app.render_to_frame()
    }

    pub fn frame_text(&mut self) -> String {
        self.frame().text()
    }

    pub fn assert_snapshot(&mut self) {
        let suffix = if self.snapshot_index == 0 {
            String::new()
        } else {
            format!(".step-{}", self.snapshot_index)
        };
        self.snapshot_index += 1;
        self.assert_snapshot_with_suffix(&suffix);
    }

    pub fn assert_snapshot_named(&mut self, name: &str) {
        self.assert_snapshot_with_suffix(&format!(".{name}"));
    }

    fn assert_snapshot_with_suffix(&mut self, suffix: &str) {
        let frame = self.frame();
        let text_name = format!("{}{}", self.name, suffix);
        let style_name = format!("{}{}.styles", self.name, suffix);
        with_settings!({
            prepend_module_to_snapshot => false,
            snapshot_path => "snapshots",
        }, {
            assert_snapshot!(text_name, frame.text());
            assert_snapshot!(style_name, frame.styles_text());
        });
    }
}

#[macro_export]
macro_rules! app_story {
    ($name:ident, |$ctx:ident| $body:block) => {
        #[tokio::test]
        async fn $name() {
            let snapshot_id = format!(
                "{}__{}",
                module_path!().rsplit("::").next().unwrap_or("app_story"),
                stringify!($name),
            );
            let mut __sb_appctx = $crate::storybook::app_ctx::AppStoryCtx::new(&snapshot_id);
            let $ctx: &mut $crate::storybook::app_ctx::AppStoryCtx = &mut __sb_appctx;
            $body
        }
    };
}
