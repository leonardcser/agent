use crate::app::{
    EventOutcome, InputOutcome, PendingChordPolicy, PromptWorkState, QueueStage, QueuedInput,
    TuiApp,
};

use crate::input::Action;
use crate::keymap::{self, KeyAction};
use crate::smelt_edit::UiHost;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

const ESC_TOKEN: &str = "<Esc>";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GlobalLuaKeymapRoute {
    Dispatch,
    ObservePrefix,
    Skip,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StatefulViewerKeyDispatch {
    NotApplicable,
    Consumed,
    Passthrough {
        vim_dispatched_for: crate::smelt_edit::WinId,
        global_keymap_checked: bool,
    },
}

impl TuiApp {
    // ── Terminal event dispatch ───────────────────────────────────────────

    /// Returns `true` if the app should quit.
    pub(crate) fn dispatch_terminal_event(&mut self, ev: Event) -> bool {
        if matches!(&ev, Event::Key(key) if key.kind == KeyEventKind::Release) {
            return false;
        }

        if matches!(ev, Event::FocusGained | Event::FocusLost) {
            let focused = matches!(ev, Event::FocusGained);
            self.platform.set_terminal_focus(focused);
            if !focused {
                self.ui.cancel_pointer_interaction();
            }
            return false;
        }

        if matches!(ev, Event::Key(_) | Event::Paste(_)) {
            self.ui.finish_pointer_interaction_for_keyboard();
        }

        // Global chords fire before focus-specific routing so no handler can swallow them.
        if let Event::Key(KeyEvent {
            code, modifiers, ..
        }) = &ev
        {
            // Skip when transient modal UI or cmdline is focused - it gets first dibs.
            if self.ui.focused_overlay().is_none()
                && self.ui.focused_modal().is_none()
                && self.well_known.cmdline.is_none()
            {
                let pctx = crate::input::prompt_ctx_ref(&self.ui);
                let ctx = self.prompt.key_context(pctx, self.turn_input_is_active());
                match keymap::lookup(*code, *modifiers, &ctx) {
                    Some(KeyAction::ToggleMode) => {
                        let lua = self.lua.execution();
                        crate::lua::scope_app(self, move || lua.cycle_mode());
                        return false;
                    }
                    Some(KeyAction::CycleReasoning) => {
                        let lua = self.lua.execution();
                        crate::lua::scope_app(self, move || lua.cycle_reasoning());
                        return false;
                    }
                    _ => {}
                }
            }
        }

        // Ctrl+C kills a focused shell-output command before modal dismiss sees it.
        if self.shell_panel_is_focused()
            && self
                .overlays
                .execution_uses_sink(crate::commands::ShellSink::Overlay)
            && matches!(
                ev,
                Event::Key(KeyEvent {
                    code: KeyCode::Char('c'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                })
            )
        {
            self.overlays.cancel_execution();
            return false;
        }

        if self.shell_panel_is_focused()
            && matches!(
                ev,
                Event::Key(KeyEvent {
                    code: KeyCode::Esc | KeyCode::Char('q'),
                    modifiers: KeyModifiers::NONE,
                    ..
                })
            )
        {
            self.close_shell_panel_and_stop_job();
            return false;
        }

        // Overlay/modal focus: route keys through the focused leaf's keymap registry.
        // Mouse events fall through so wheel/scrollbar logic runs over the overlay rect.
        if self.ui.focused_overlay().is_some() || self.ui.active_modal().is_some() {
            if let Event::Resize(w, h) = ev {
                self.handle_resize(w, h);
                return false;
            }
            if matches!(&ev, Event::Key(_) | Event::Paste(_)) {
                // Cmdline owns its input end-to-end: text edit,
                // history nav, completer cycling, and command exec
                // all need `&mut TuiApp`, so the overlay leaf has no
                // recipe and `cmdline_handle_event` runs every key/paste
                // before the generic compositor dispatch. Returns
                // `Some(true)` only when the run command resolved to
                // Quit (propagated as the loop's quit signal).
                if self.cmdline_is_focused() {
                    if let Some(quit) = self.cmdline_handle_event(ev) {
                        return quit;
                    }
                    // Swallow unclaimed input so split keymaps don't fire over an open cmdline.
                    return false;
                }
                if let Event::Paste(data) = ev {
                    if matches!(
                        self.run_paste_fallback(data),
                        crate::smelt_edit::Status::Consumed
                    ) {
                        self.flush_lua_callbacks();
                    }
                    return false;
                }
                let Event::Key(k) = ev else { unreachable!() };
                if self.handle_focused_search_key(k) {
                    return false;
                }
                if self.handle_search_open_before_window_dispatch(k) {
                    return false;
                }
                if self.try_open_cmdline_for_key(k) {
                    return false;
                }
                if matches!(self.run_key_cascade(k), crate::smelt_edit::Status::Consumed) {
                    self.flush_lua_callbacks();
                    return false;
                }
                self.flush_lua_callbacks();
                return false;
            }
            if !matches!(ev, Event::Mouse(_)) {
                return false;
            }
            // Mouse events fall through so wheel + scrollbar drag work on overlays.
        }

        // Ctrl+C kills a running exec process.
        if self.overlays.execution_is_running()
            && matches!(
                ev,
                Event::Key(KeyEvent {
                    code: KeyCode::Char('c'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                })
            )
        {
            self.overlays.cancel_execution();
            return false;
        }

        let outcome = if self.prompt_work_state() != PromptWorkState::Idle {
            self.handle_event_running(ev)
        } else {
            self.handle_event_idle(ev)
        };

        // Notify Lua subscribers if the prompt buffer changed (drives filter-as-you-type pickers).
        self.emit_prompt_text_changed_if_dirty();

        let redraw = self.apply_event_outcome(outcome);
        self.save_deferred_session_batch_if_ready();
        redraw
    }

    fn apply_event_outcome(&mut self, outcome: EventOutcome) -> bool {
        match outcome {
            EventOutcome::Noop | EventOutcome::Redraw => false,
            EventOutcome::Quit => {
                self.discard_turn(crate::app::TurnEnd::Cancelled);
                true
            }
            EventOutcome::CancelAgent => {
                engine::log::entry(
                    engine::log::Level::Info,
                    "agent_stop",
                    &serde_json::json!({
                        "reason": "user_cancel",
                    }),
                );
                self.discard_turn(crate::app::TurnEnd::Cancelled);
                false
            }
            EventOutcome::InterruptWithQueued => {
                self.interrupt_with_next_queued();
                false
            }
            EventOutcome::ContinueTurn => {
                let turn = self.begin_agent_turn("", protocol::Content::text(""));
                self.conversation.set_active(turn);
                false
            }
            EventOutcome::Exec(handle) => {
                self.overlays.install_execution(handle);
                false
            }
            EventOutcome::Submit {
                mut content,
                mut display,
                edit,
            } => {
                self.clear_prompt_prediction();
                self.redact_user_submission(&mut content, &mut display);
                let mut edit = Some(edit);
                let accepted = match self.prompt_work_state() {
                    PromptWorkState::TurnActive
                    | PromptWorkState::BackgroundBusy
                    | PromptWorkState::Paused => {
                        // Queue while an active turn or background plugin owns the
                        // input lifecycle so messages run against the next stable state.
                        if content.is_empty() {
                            false
                        } else {
                            self.commit_prompt_submission(edit.take().expect("submit edit"));
                            self.prompt
                                .try_queue_turn(QueuedInput::request(display.clone(), content));
                            true
                        }
                    }
                    PromptWorkState::Idle => {
                        let text = content.text_content().into_owned();
                        let has_images = content.image_count() > 0;
                        if !text.is_empty() || has_images {
                            let outcome = if has_images && text.trim().is_empty() {
                                InputOutcome::StartAgent
                            } else {
                                self.process_input(&text)
                            };
                            let accepted = match outcome {
                                InputOutcome::StartAgent => {
                                    match self.begin_agent_turn(&display, content) {
                                        Some(turn) => {
                                            self.commit_prompt_submission(
                                                edit.take().expect("submit edit"),
                                            );
                                            self.conversation.set_active(Some(turn));
                                            true
                                        }
                                        None if self.turn_submission_is_pending() => {
                                            self.commit_prompt_submission(
                                                edit.take().expect("submit edit"),
                                            );
                                            true
                                        }
                                        None => false,
                                    }
                                }
                                outcome => {
                                    self.commit_prompt_submission(
                                        edit.take().expect("submit edit"),
                                    );
                                    self.apply_input_outcome(outcome, content, &display);
                                    true
                                }
                            };
                            if self.pending_quit {
                                return true;
                            }
                            accepted
                        } else {
                            let outcome = self.handle_empty_submit();
                            return self.apply_event_outcome(outcome);
                        }
                    }
                };
                // Don't restore stash if a dialog opened - it restores on close.
                if accepted && self.ui.active_modal().is_none() {
                    let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
                    self.prompt.restore_stash(&mut pctx);
                }
                false
            }
        }
    }

    fn commit_prompt_submission(&mut self, edit: crate::input::SubmitEdit) {
        let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
        self.prompt.apply_submit_edit(&mut pctx, edit);
    }

    fn handle_focused_search_key(&mut self, k: KeyEvent) -> bool {
        let Some(win) = self.ui.focus() else {
            return false;
        };
        self.handle_search_key_for_target(win, k)
    }

    fn handle_search_open_before_window_dispatch(&mut self, k: KeyEvent) -> bool {
        self.try_open_search_for_key(k)
    }

    fn focused_transient_owns_printable_keys(&self) -> bool {
        if self.ui.focused_overlay().is_none() && self.ui.focused_modal().is_none() {
            return false;
        }
        let Some(win) = self.ui.focus().and_then(|win| self.ui.win(win)) else {
            return true;
        };
        !win.surface().is_readonly_text()
            || win.vim_enabled() && win.vim_mode() == crate::smelt_edit::VimMode::Insert
    }

    pub(crate) fn try_open_cmdline_for_key(&mut self, k: KeyEvent) -> bool {
        if !matches!(
            (k.code, k.modifiers),
            (KeyCode::Char(':'), KeyModifiers::NONE | KeyModifiers::SHIFT)
        ) || self.focused_transient_owns_printable_keys()
        {
            return false;
        }

        self.open_cmdline();
        true
    }

    pub(crate) fn try_open_search_for_key(&mut self, k: KeyEvent) -> bool {
        let direction = match (k.code, k.modifiers) {
            (KeyCode::Char('/'), KeyModifiers::NONE) => {
                crate::app::search::SearchDirection::Forward
            }
            (KeyCode::Char('?'), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                crate::app::search::SearchDirection::Backward
            }
            _ => return false,
        };

        if self.focused_transient_owns_printable_keys() {
            return false;
        }

        self.open_search_input(direction)
    }

    fn dispatch_window_lua_keymap(&mut self, key: KeyEvent) -> Option<EventOutcome> {
        if self.ui.focused_overlay().is_some() {
            return None;
        }

        let lua = &self.lua;
        let mut lua_invoke = |handle: crate::smelt_edit::LuaHandle,
                              win: crate::smelt_edit::WinId,
                              payload: &crate::smelt_edit::Payload| {
            lua.queue_invocation(handle, win, payload);
        };
        let result = self
            .ui
            .dispatch_event(crate::smelt_edit::Event::Key(key), &mut lua_invoke);
        if matches!(result, crate::smelt_edit::Status::Consumed) {
            self.flush_lua_callbacks();
            return Some(EventOutcome::Noop);
        }
        None
    }

    fn global_lua_keymap_route(&self, key: KeyEvent) -> GlobalLuaKeymapRoute {
        if self.timers.pending_chord.is_some() {
            return GlobalLuaKeymapRoute::Dispatch;
        }

        let plain_esc = matches!(key.code, KeyCode::Esc) && key.modifiers == KeyModifiers::NONE;
        if !plain_esc {
            return GlobalLuaKeymapRoute::Dispatch;
        }

        if self.app_focus == crate::app::AppFocus::Prompt
            && self.ui.focused_overlay().is_none()
            && self.prompt_escape_owned_by_vim()
        {
            return GlobalLuaKeymapRoute::ObservePrefix;
        }

        if self.prompt_escape_owned_by_vim()
            || matches!(
                self.focused_vim_mode(),
                Some(
                    crate::smelt_edit::VimMode::Insert
                        | crate::smelt_edit::VimMode::Visual
                        | crate::smelt_edit::VimMode::VisualLine
                )
            )
        {
            return GlobalLuaKeymapRoute::Skip;
        }

        GlobalLuaKeymapRoute::Dispatch
    }

    fn pending_lua_keymap_cancelled_by(
        &self,
        key: KeyEvent,
        token: &str,
        vim_mode: Option<&str>,
    ) -> bool {
        let Some(pending) = &self.timers.pending_chord else {
            return false;
        };
        let cancel_key = matches!(key.code, KeyCode::Esc) && key.modifiers == KeyModifiers::NONE
            || matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
                && key.modifiers == KeyModifiers::CONTROL;
        if !cancel_key {
            return false;
        }

        let mut candidate = pending.tokens.concat();
        candidate.push_str(token);
        !self.lua.chord_has_binding(&candidate, vim_mode)
            && !self.lua.chord_has_longer(&candidate, vim_mode)
    }

    fn dispatch_single_global_lua_keymap(&mut self, key: KeyEvent) -> bool {
        if self.global_lua_keymap_route(key) != GlobalLuaKeymapRoute::Dispatch {
            return false;
        }
        let Some(token) = crate::lua::chord_string(key) else {
            return false;
        };
        let vim_mode = self.current_vim_mode_label();
        let lua = self.lua.execution();
        use smelt_core::lua::runtime::KeymapResult;
        let result =
            crate::lua::scope_app(self, || lua.run_keymap(&token, vim_mode.as_deref(), None));
        match result {
            KeymapResult::Consumed => {
                self.flush_lua_callbacks();
                true
            }
            KeymapResult::PassThrough | KeymapResult::NoBinding => {
                self.flush_lua_callbacks();
                false
            }
        }
    }

    fn pending_lua_chord_policy(
        &self,
        first_token: &str,
        now: std::time::Instant,
    ) -> PendingChordPolicy {
        match first_token {
            ESC_TOKEN => PendingChordPolicy::Timed {
                expires_at: now
                    + std::time::Duration::from_millis(crate::app::ESC_CHORD_TIMEOUT_MS),
            },
            _ => PendingChordPolicy::Sticky,
        }
    }

    fn dispatch_global_lua_keymap(&mut self, key: KeyEvent) -> Option<EventOutcome> {
        self.expire_pending_keymap_chord();
        let route = self.global_lua_keymap_route(key);
        if route == GlobalLuaKeymapRoute::Skip {
            return None;
        }
        let observe_prefix = route == GlobalLuaKeymapRoute::ObservePrefix;

        let Some(token) = crate::lua::chord_string(key) else {
            return self.timers.pending_chord.take().map(|_| EventOutcome::Noop);
        };

        let had_pending = self.timers.pending_chord.is_some();
        let vim_mode = self.current_vim_mode_label();
        if self.pending_lua_keymap_cancelled_by(key, &token, vim_mode.as_deref()) {
            self.timers.pending_chord = None;
            return Some(EventOutcome::Noop);
        }
        use smelt_core::lua::runtime::KeymapResult;

        // If no sequence is pending, try exact single-key bindings first. Once a
        // prefix is pending, the next key belongs to that sequence before any
        // single-key binding gets a chance, matching Vim's modal mapping feel.
        // Observe-only keys seed prefixes without stealing the local key action.
        if !had_pending && !observe_prefix {
            let lua = self.lua.execution();
            let result =
                crate::lua::scope_app(self, || lua.run_keymap(&token, vim_mode.as_deref(), None));
            match result {
                KeymapResult::Consumed => {
                    self.timers.pending_chord = None;
                    self.flush_lua_callbacks();
                    return Some(EventOutcome::Noop);
                }
                KeymapResult::PassThrough | KeymapResult::NoBinding => {
                    self.flush_lua_callbacks();
                }
            }
        }

        let now = self.core.clock.instant_now();
        if self.timers.pending_chord.is_none() {
            self.timers.pending_chord = Some(crate::app::PendingChord {
                tokens: Vec::new(),
                vim_mode_at_start: if self.prompt.vim_enabled(self.prompt_win()) {
                    Some(self.prompt_win().vim_mode())
                } else {
                    None
                },
                policy: self.pending_lua_chord_policy(&token, now),
            });
        }
        let (mut tokens, vim_mode_at_start, policy, first_token_at_start) = {
            let p = self.timers.pending_chord.take().unwrap();
            let first = p.tokens.first().cloned();
            (p.tokens, p.vim_mode_at_start, p.policy, first)
        };
        tokens.push(token);

        let lua = self.lua.execution();
        let mut oracle = LuaChordOracle {
            lua: &lua,
            vim_mode: vim_mode.as_deref(),
            vim_mode_at_start,
        };
        let outcome = crate::lua::scope_app(self, || {
            smelt_core::keymap::match_chord(tokens, &mut oracle)
        });
        self.flush_lua_callbacks();
        match outcome {
            smelt_core::keymap::ChordOutcome::Consumed => Some(EventOutcome::Noop),
            smelt_core::keymap::ChordOutcome::Pending { tokens } => {
                if tokens.is_empty() {
                    self.timers.pending_chord = None;
                } else {
                    let next_policy = if tokens.first() == first_token_at_start.as_ref() {
                        policy
                    } else {
                        tokens
                            .first()
                            .map(|first| self.pending_lua_chord_policy(first, now))
                            .unwrap_or(PendingChordPolicy::Sticky)
                    };
                    self.timers.pending_chord = Some(crate::app::PendingChord {
                        tokens,
                        vim_mode_at_start,
                        policy: next_policy,
                    });
                    if !observe_prefix {
                        return Some(EventOutcome::Noop);
                    }
                }
                None
            }
        }
    }

    // ── Idle event handler ───────────────────────────────────────────────

    /// Shared preamble for idle and agent-running paths.
    ///
    /// Returns `Some(outcome)` when consumed; `None` to continue with path-specific logic.
    ///
    /// Dispatch priority: resize/mouse → notification dismissal → pending prompt Vim input
    /// → content search opener → window-local Lua keymaps → global Lua keymaps
    /// → pane chords → cmdline `:` → content focus.
    fn dispatch_common(&mut self, ev: &Event, running: bool) -> Option<EventOutcome> {
        if let Event::Resize(w, h) = *ev {
            self.handle_resize(w, h);
            return Some(EventOutcome::Noop);
        }
        if let Event::Mouse(me) = *ev {
            return Some(self.handle_mouse(me));
        }
        if let Event::Key(k) = *ev {
            if let Some(outcome) = self.handle_notification_key(k) {
                return Some(outcome);
            }
            if self.prompt_vim_pending_input_owns_key(k) {
                return Some(self.dispatch_prompt_event_to_input(Event::Key(k), running));
            }
            if matches!(self.app_focus, crate::app::AppFocus::Content)
                && self.handle_focused_search_key(k)
            {
                return Some(EventOutcome::Noop);
            }
            if matches!(self.app_focus, crate::app::AppFocus::Content)
                && self.handle_search_open_before_window_dispatch(k)
            {
                return Some(EventOutcome::Noop);
            }
            if let Some(outcome) = self.dispatch_window_lua_keymap(k) {
                return Some(outcome);
            }
            if let Some(outcome) = self.dispatch_global_lua_keymap(k) {
                return Some(outcome);
            }
        }
        if let Some(outcome) = self.handle_pane_chord(ev) {
            return Some(outcome);
        }
        // `:` opens the cmdline unless in insert mode.
        if let Event::Key(KeyEvent {
            code: KeyCode::Char(':'),
            modifiers: KeyModifiers::NONE | KeyModifiers::SHIFT,
            ..
        }) = ev
        {
            let in_insert = match self.app_focus {
                crate::app::AppFocus::Prompt => {
                    !self.prompt.vim_enabled(self.prompt_win())
                        || self.prompt_win().vim_mode() == crate::smelt_edit::VimMode::Insert
                }
                crate::app::AppFocus::Content => false,
            };
            if !in_insert {
                self.open_cmdline();
                return Some(EventOutcome::Noop);
            }
        }
        if self.app_focus == crate::app::AppFocus::Content {
            return Some(self.handle_event_app_history(ev));
        }
        None
    }

    fn handle_event_idle(&mut self, ev: Event) -> EventOutcome {
        if let Some(outcome) = self.dispatch_common(&ev, false) {
            return outcome;
        }

        if let Event::Key(KeyEvent {
            code, modifiers, ..
        }) = ev
        {
            if matches!(code, KeyCode::Esc) {
                return self.handle_idle_prompt_esc(ev, modifiers);
            }

            // Placeholder routing: when the prompt is empty and a placeholder is set,
            // matching `accept_keys` accept the text into the buffer; matching
            // `dismiss_keys` clear it. Both fire the corresponding win event.
            // Typing past those chords leaves the placeholder intact (the buffer
            // becoming non-empty just hides it visually - undoing back to empty
            // restores it).
            if let Some(outcome) =
                self.dispatch_placeholder_key(self.well_known.prompt, code, modifiers)
            {
                return outcome;
            }

            let pctx_ref = crate::input::prompt_ctx_ref(&self.ui);
            let ctx = self.prompt.key_context(pctx_ref, false);

            if let Some(action) = keymap::lookup(code, modifiers, &ctx) {
                match action {
                    KeyAction::Quit => {
                        return EventOutcome::Quit;
                    }
                    KeyAction::ClearBuffer => {
                        self.timers.last_ctrlc = Some(self.core.clock.instant_now());
                        let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
                        self.prompt.clear_with_undo(&mut pctx);
                        return EventOutcome::Redraw;
                    }
                    _ => {}
                }
            }
        }

        let now = self.core.clock.instant_now();
        let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
        let action = self
            .prompt
            .handle_event(&mut pctx, ev, true, &mut self.core.clipboard, now);
        self.dispatch_input_action(action)
    }

    // ── Running event handler ────────────────────────────────────────────

    fn handle_event_running(&mut self, ev: Event) -> EventOutcome {
        if let Some(outcome) = self.dispatch_common(&ev, true) {
            return outcome;
        }

        // Record last keypress for deferred permission dialogs.
        if matches!(ev, Event::Key(_)) {
            self.timers.last_keypress = Some(self.core.clock.instant_now());
        }

        if let Event::Key(KeyEvent {
            code, modifiers, ..
        }) = ev
        {
            let pctx_ref = crate::input::prompt_ctx_ref(&self.ui);
            let ctx = self.prompt.key_context(pctx_ref, true);
            if let Some(action) = keymap::lookup(code, modifiers, &ctx) {
                match action {
                    KeyAction::CancelAgent => {
                        return EventOutcome::CancelAgent;
                    }
                    KeyAction::ClearBuffer => {
                        self.timers.last_ctrlc = Some(self.core.clock.instant_now());
                        let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
                        self.prompt.clear_with_undo(&mut pctx);
                        return EventOutcome::Noop;
                    }
                    _ => {}
                }
            }
        }

        if matches!(
            ev,
            Event::Key(KeyEvent {
                code: KeyCode::Esc,
                ..
            })
        ) {
            return self.handle_running_prompt_esc(ev);
        }

        let now = self.core.clock.instant_now();
        let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
        let input_action =
            self.prompt
                .handle_event(&mut pctx, ev, true, &mut self.core.clipboard, now);
        self.dispatch_running_input_action(input_action)
    }

    fn dispatch_running_input_action(&mut self, input_action: Action) -> EventOutcome {
        match input_action {
            Action::Submit {
                content,
                display,
                edit,
            } => {
                return self.handle_running_submit(content, display, edit, QueueStage::Turn);
            }
            Action::SubmitToRequestQueue {
                content,
                display,
                edit,
            } => {
                return self.handle_running_submit(content, display, edit, QueueStage::Request);
            }
            Action::SubmitEmpty => {
                return self.handle_empty_submit();
            }
            Action::Redraw => {}
            Action::EditInEditor => {
                self.edit_in_editor();
            }
            Action::CenterScroll => {
                self.prompt_win_mut().pending_recenter = true;
            }
            Action::PanColumns(d) => {
                self.pan_prompt_columns(d);
            }
            Action::NotifyError(msg) => {
                self.notify_error(msg);
            }
            Action::Noop => {}
        }
        EventOutcome::Noop
    }

    fn handle_running_submit(
        &mut self,
        mut content: protocol::Content,
        mut display: String,
        edit: crate::input::SubmitEdit,
        target: QueueStage,
    ) -> EventOutcome {
        self.clear_prompt_prediction();
        self.redact_user_submission(&mut content, &mut display);
        let text = content.text_content().into_owned();
        if content.image_count() == 0 {
            if let Some(outcome) =
                self.try_command_while_running(smelt_buffer::text::trim_whitespace(&text), target)
            {
                self.commit_prompt_submission(edit);
                return outcome;
            }
        }
        if content.is_empty() {
            return EventOutcome::Noop;
        }
        if target == QueueStage::Request && content.image_count() > 0 {
            self.notify_error(
                "cannot use image attachments to steer the current response; prompt left unchanged"
                    .into(),
            );
            return EventOutcome::Noop;
        }
        self.commit_prompt_submission(edit);
        let queued = QueuedInput::request(display, content);
        match target {
            QueueStage::Turn => {
                self.prompt.try_queue_turn(queued);
            }
            QueueStage::Request => {
                self.queue_input_for_request(queued);
            }
        }
        EventOutcome::Noop
    }

    fn handle_empty_submit(&mut self) -> EventOutcome {
        match self.prompt_work_state() {
            PromptWorkState::TurnActive => {
                self.clear_prompt_prediction();
                if self.prompt.has_queued_request() {
                    return EventOutcome::InterruptWithQueued;
                }
                if self.prompt.front_turn_can_be_request() {
                    self.promote_next_queued_turn_to_request();
                }
                return EventOutcome::Noop;
            }
            PromptWorkState::BackgroundBusy => {
                self.clear_prompt_prediction();
                return EventOutcome::Noop;
            }
            PromptWorkState::Paused => {
                self.resume_paused_turn(None);
                return EventOutcome::Noop;
            }
            PromptWorkState::Idle => {}
        }

        if !self.prompt.queue_is_empty() {
            self.start_next_queued_input_if_idle();
            return EventOutcome::Noop;
        }

        if self.can_continue_turn() {
            EventOutcome::ContinueTurn
        } else {
            EventOutcome::Noop
        }
    }

    fn promote_next_queued_turn_to_request(&mut self) {
        let Some(queued) = self.prompt.promote_turn_to_request() else {
            return;
        };
        let Some(input) = queued.steer_input() else {
            return;
        };
        if !input.provider_content().is_empty() {
            self.core.engine.send(protocol::UiCommand::Steer { input });
        }
    }

    pub(super) fn interrupt_with_next_queued(&mut self) {
        if self.defer_queued_compaction_handoff() {
            return;
        }
        let interrupted = self.prompt.suspend_for_interrupt();
        if interrupted.unsteer_count() > 0 {
            self.core.engine.send(protocol::UiCommand::Unsteer {
                count: interrupted.unsteer_count(),
            });
        }
        let terminal_commit = self.discard_turn(crate::app::TurnEnd::Cancelled);
        if let Some(queued) = self.prompt.restore_after_interrupt(interrupted) {
            // Interrupt-and-send authorizes the replacement, even if persistence is still pending.
            self.conversation.set_turn_pause(None);
            if terminal_commit.is_durable() {
                if let Err(queued) = self.start_queued_input(queued) {
                    self.prompt.queue_front(QueueStage::Turn, queued);
                }
            } else {
                self.prompt.queue_front(QueueStage::Turn, queued);
            }
        }
    }

    // ── Shared helpers ────────────────────────────────────────────────────

    fn handle_idle_prompt_esc(&mut self, ev: Event, modifiers: KeyModifiers) -> EventOutcome {
        if self.prompt_escape_owned_by_vim() {
            return self.dispatch_prompt_event_to_input(ev, false);
        }

        if let Some(outcome) =
            self.dispatch_placeholder_key(self.well_known.prompt, KeyCode::Esc, modifiers)
        {
            return outcome;
        }

        EventOutcome::Noop
    }

    fn handle_running_prompt_esc(&mut self, ev: Event) -> EventOutcome {
        let now = self.core.clock.instant_now();

        if self.prompt_escape_owned_by_vim() {
            self.apply_prompt_escape_to_input(ev, now);
            return EventOutcome::Noop;
        }

        if let Some(outcome) = self.dispatch_placeholder_key(
            self.well_known.prompt,
            KeyCode::Esc,
            KeyModifiers::empty(),
        ) {
            return outcome;
        }

        if self.conversation.turn_pause().is_some() {
            self.conversation.clear_continuation();
            return EventOutcome::Noop;
        }
        if !self.prompt.queue_is_empty() {
            self.drain_queued_inputs_into_prompt();
            return EventOutcome::Noop;
        }

        EventOutcome::Noop
    }

    fn prompt_vim_pending_input_owns_key(&self, key: KeyEvent) -> bool {
        if self.app_focus != crate::app::AppFocus::Prompt {
            return false;
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
        {
            return false;
        }
        self.prompt_vim_has_pending_input()
    }

    fn prompt_vim_has_pending_input(&self) -> bool {
        self.prompt_win().vim_has_pending_input()
    }

    fn dispatch_prompt_event_to_input(&mut self, ev: Event, running: bool) -> EventOutcome {
        let now = self.core.clock.instant_now();
        let action = {
            let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
            self.prompt
                .handle_event(&mut pctx, ev, true, &mut self.core.clipboard, now)
        };
        if running {
            self.dispatch_running_input_action(action)
        } else {
            self.dispatch_input_action(action)
        }
    }

    fn prompt_escape_owned_by_vim(&self) -> bool {
        if !self.prompt.vim_enabled(self.prompt_win()) {
            return false;
        }
        match self.prompt_win().vim_mode() {
            crate::smelt_edit::VimMode::Insert
            | crate::smelt_edit::VimMode::Visual
            | crate::smelt_edit::VimMode::VisualLine => true,
            crate::smelt_edit::VimMode::Normal => self.prompt_vim_has_pending_input(),
        }
    }

    fn apply_prompt_escape_to_input(&mut self, ev: Event, now: std::time::Instant) {
        let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
        self.prompt
            .handle_event(&mut pctx, ev, false, &mut self.core.clipboard, now);
    }

    fn handle_notification_key(&mut self, key: KeyEvent) -> Option<EventOutcome> {
        let sticky = self.overlays.notification_is_sticky()?;
        if key.code == KeyCode::Esc && key.modifiers == KeyModifiers::NONE {
            if matches!(self.app_focus, crate::app::AppFocus::Prompt)
                && self.prompt_escape_owned_by_vim()
            {
                return None;
            }
            self.dismiss_notification();
            return Some(EventOutcome::Redraw);
        }
        if !sticky {
            self.dismiss_notification();
        }
        None
    }

    fn dispatch_input_action(&mut self, action: Action) -> EventOutcome {
        match action {
            Action::Submit {
                content,
                display,
                edit,
            }
            | Action::SubmitToRequestQueue {
                content,
                display,
                edit,
            } => EventOutcome::Submit {
                content,
                display,
                edit,
            },
            Action::SubmitEmpty => self.handle_empty_submit(),
            Action::EditInEditor => {
                self.edit_in_editor();
                EventOutcome::Noop
            }
            Action::CenterScroll => {
                self.prompt_win_mut().pending_recenter = true;
                EventOutcome::Noop
            }
            Action::PanColumns(d) => {
                self.pan_prompt_columns(d);
                EventOutcome::Redraw
            }
            Action::Redraw => EventOutcome::Redraw,
            Action::NotifyError(msg) => {
                self.notify_error(msg);
                EventOutcome::Redraw
            }
            Action::Noop => EventOutcome::Noop,
        }
    }

    fn pan_prompt_columns(&mut self, delta: isize) {
        let win_id = crate::app::PROMPT_WIN;
        if let Some(win) = self.ui.win_mut(win_id) {
            let viewport_cols = win.viewport.map(|v| v.content_width).unwrap_or(0);
            win.pan_by_columns(delta, viewport_cols);
        }
    }

    fn edit_in_editor(&mut self) {
        let req = match crate::input::editor::prepare(
            self.prompt_buf().source(),
            self.core.env.runtime_dir(),
        ) {
            Ok(req) => req,
            Err(e) => {
                self.notify_error(format!("editor: {e}"));
                return;
            }
        };
        let cwd = self.core.env.cwd();
        let spawn = || {
            std::process::Command::new(&req.program)
                .args(&req.args)
                .current_dir(cwd)
                .status()
        };
        let Some(status) = self.platform.suspend_terminal(spawn) else {
            self.notify_error("editor: unavailable without an attached terminal".to_string());
            self.ui.force_redraw();
            return;
        };
        // Vim et al re-show the hardware cursor and scribble over the alt
        // screen; force a full repaint so the diff baseline is rebuilt.
        self.ui.force_redraw();
        match crate::input::editor::finalize(req, status) {
            Ok(text) => {
                let mut pctx = crate::input::prompt_ctx_mut(&mut self.ui);
                self.prompt.replace_text(&mut pctx, text);
            }
            Err(msg) => self.notify_error(msg),
        }
    }

    pub(crate) fn handle_resize(&mut self, w: u16, h: u16) {
        let width_changed = w != self.last_width;
        self.last_width = w;
        self.last_height = h;
        let _ = self
            .ui
            .dispatch_event(crate::smelt_edit::Event::Resize(w, h), &mut |_, _, _| {});
        self.refresh_main_layout();
        if width_changed {
            self.ui.cancel_pointer_interaction();
        }
    }

    // ── Input processing (commands, settings, rewind, shell) ─────────────

    pub(crate) fn process_input(&mut self, input: &str) -> InputOutcome {
        if input.is_empty() {
            return InputOutcome::Continue;
        }

        let trimmed = smelt_buffer::text::trim_whitespace(input);
        self.prompt.push_history(input.to_string());

        let is_from_paste = self.prompt.skip_shell_escape();
        let parsed = crate::commands::parse_command_line(trimmed);

        if let crate::commands::ParsedCommand::Slash { name, .. } = &parsed {
            return if self.has_command_name(name) {
                InputOutcome::Command(trimmed.to_string())
            } else {
                InputOutcome::StartAgent
            };
        }
        if crate::commands::prompt_quit_alias(trimmed)
            || (trimmed.starts_with('!') && !is_from_paste)
        {
            return InputOutcome::Command(trimmed.to_string());
        }

        InputOutcome::StartAgent
    }

    // ── Tick ─────────────────────────────────────────────────────────────

    /// Viewport rows for the content pane. Uses the prompt's previous-frame rendered height
    /// so multi-line prompts don't cause scroll math to overshoot.
    pub(crate) fn viewport_rows_estimate(&self) -> u16 {
        self.layout.viewport_rows().max(1)
    }

    /// Close an overlay leaf and clean up picker/Lua-callback registrations.
    /// `Ui::win_close` cascades to overlay close when the leaf belongs to one.
    pub(crate) fn close_overlay_leaf(&mut self, win_id: crate::smelt_edit::WinId) {
        crate::picker::forget(self, win_id);
        self.prompt.clear_placeholder(win_id);
        for id in self.win_close(win_id) {
            self.lua.remove_callback(id);
        }
    }

    /// Close an overlay by id without assuming its first layout leaf is a window.
    pub(crate) fn close_overlay(&mut self, overlay_id: crate::smelt_edit::OverlayId) {
        for id in self.ui.overlay_close_tree(overlay_id) {
            self.lua.remove_callback(id);
        }
    }

    /// Close a window-owned decoration and clean up leaf Lua callbacks.
    pub(crate) fn close_decoration(&mut self, decoration_id: crate::smelt_edit::DecorationId) {
        for id in self.ui.decoration_close_tree(decoration_id) {
            self.lua.remove_callback(id);
        }
    }

    /// Close the focused overlay if it doesn't block the agent.
    /// Fires `WinEvent::Dismiss` so callbacks can flush pending state before close.
    pub(crate) fn close_focused_non_blocking_overlay(&mut self) {
        let Some(overlay_id) = self.ui.focused_overlay() else {
            return;
        };
        let Some(overlay) = self.ui.overlay(overlay_id) else {
            return;
        };
        if overlay.blocks_agent {
            return;
        }
        let Some(root) = overlay
            .layout
            .leaves_in_order()
            .into_iter()
            .next()
            .map(|p| crate::smelt_edit::WinId(p.0))
        else {
            return;
        };
        let lua = &self.lua;
        let mut lua_invoke = |handle: crate::smelt_edit::LuaHandle,
                              win: crate::smelt_edit::WinId,
                              payload: &crate::smelt_edit::Payload| {
            lua.queue_invocation(handle, win, payload);
        };
        self.ui.fire_win_event(
            root,
            crate::smelt_edit::WinEvent::Dismiss,
            crate::smelt_edit::Payload::None,
            &mut lua_invoke,
        );
        self.flush_lua_callbacks();
    }

    /// True when the active modal blocks engine-event drain.
    pub(crate) fn modal_blocks_agent(&self) -> bool {
        self.ui.active_modal_blocks_agent()
    }

    /// Snap the transcript cursor to the nearest selectable cell, skipping gutters and padding.
    pub(crate) fn snap_transcript_cursor(&mut self) {
        let win_id = self.well_known.transcript;
        let buf_id = self.transcript_win().buf;
        // Row-backed windows manage their own cursor; snapping the local cpos would
        // overwrite the document cursor via sync_from_cpos in resync.
        if self
            .ui
            .win(win_id)
            .is_some_and(|w| w.has_materialized_rows())
        {
            return;
        }
        let cpos = self.transcript_win().cpos();
        let rows: Vec<String> = self
            .ui
            .buf(buf_id)
            .map(|b| b.lines().to_vec())
            .unwrap_or_default();
        let snapped = self.snap_cpos_to_selectable(&rows, cpos);
        if snapped != cpos {
            self.transcript_win_mut().set_cpos(snapped);
            let viewport = self.viewport_rows_estimate();
            let (win, buf) = self.ui.win_and_buf_mut(win_id, buf_id);
            win.expect("transcript window")
                .resync(buf.expect("transcript buffer"), viewport);
        }
    }

    /// Orchestrates the per-key dispatch cascade when an overlay or modal is
    /// focused. Tiers fire in order; the first to return `Consumed` wins:
    ///
    /// - Tier 1: specific keymap on the focused leaf (`win_set_keymap`).
    /// - Tier 1b: stateful Vim input on the focused transient viewer.
    /// - Tier 1c: modal- or overlay-scoped keymap on the focused container.
    /// - Tier 2: global Lua keymap (`smelt.keymap.set("", chord, fn)`).
    /// - Tier 3: vim viewer keys on the focused leaf.
    /// - Tier 4: per-window catch-all fallback (`win_set_key_fallback`).
    /// - Tier 5: modal dismiss for bare Esc / Ctrl-C.
    ///
    /// Putting global keymaps between tiers 1c and 3 lets a site-wide chord
    /// like `?` -> /help win over a dialog input's blanket printable-char
    /// fallback, without each leaf needing a bespoke carve-out. Overlay-scoped
    /// keymaps sit above global so an open dialog/picker's local intent
    /// (e.g. `Tab` cycles items) beats a site-wide rebinding of the same chord.
    pub(crate) fn run_key_cascade(&mut self, k: KeyEvent) -> crate::smelt_edit::Status {
        use crate::smelt_edit::Status;

        // Tier 1: specific keymap on the focused leaf.
        {
            let lua = &self.lua;
            let mut lua_invoke =
                |handle: crate::smelt_edit::LuaHandle,
                 win: crate::smelt_edit::WinId,
                 payload: &crate::smelt_edit::Payload| {
                    lua.queue_invocation(handle, win, payload);
                };
            if matches!(
                self.ui.dispatch_key(k.code, k.modifiers, &mut lua_invoke),
                Status::Consumed
            ) {
                return Status::Consumed;
            }
        }

        // Tier 1b: explicit Visual mappings can override viewer defaults;
        // otherwise Visual/VisualLine mode and pending Normal-mode sequences
        // belong to Vim before container shortcuts.
        let stateful_dispatch = self.dispatch_stateful_transient_viewer_key(k);
        let (vim_dispatched_for, global_keymap_checked) = match stateful_dispatch {
            StatefulViewerKeyDispatch::Consumed => return Status::Consumed,
            StatefulViewerKeyDispatch::Passthrough {
                vim_dispatched_for,
                global_keymap_checked,
            } => (Some(vim_dispatched_for), global_keymap_checked),
            StatefulViewerKeyDispatch::NotApplicable => (None, false),
        };

        // Tier 1c: modal- and overlay-scoped keymaps shared by every leaf in
        // their container.
        {
            let lua = &self.lua;
            let mut lua_invoke =
                |handle: crate::smelt_edit::LuaHandle,
                 win: crate::smelt_edit::WinId,
                 payload: &crate::smelt_edit::Payload| {
                    lua.queue_invocation(handle, win, payload);
                };
            if matches!(
                self.ui
                    .dispatch_modal_key(k.code, k.modifiers, &mut lua_invoke),
                Status::Consumed
            ) || matches!(
                self.ui
                    .dispatch_overlay_key(k.code, k.modifiers, &mut lua_invoke),
                Status::Consumed
            ) {
                return Status::Consumed;
            }
        }

        // Tier 2: global Lua keymap (single-chord lookup only - overlays
        // don't participate in the chord-buffering path). Visual mode checked
        // this tier before Vim so explicit remaps can override viewer defaults.
        if !global_keymap_checked && self.dispatch_single_global_lua_keymap(k) {
            return Status::Consumed;
        }

        // Tier 3: viewer fallbacks for the focused transient leaf. A key that
        // already passed through stateful Vim skips the Vim engine here so each
        // terminal key is evaluated by Vim at most once.
        let viewer_consumed = if vim_dispatched_for.is_some_and(|win| self.ui.focus() == Some(win))
        {
            self.dispatch_transient_viewer_fallback_key(k)
        } else {
            self.dispatch_transient_viewer_key(k)
        };
        if viewer_consumed {
            return Status::Consumed;
        }

        // Tier 4: per-window catch-all fallback (dialog inputs, etc.).
        {
            let lua = &self.lua;
            let mut lua_invoke =
                |handle: crate::smelt_edit::LuaHandle,
                 win: crate::smelt_edit::WinId,
                 payload: &crate::smelt_edit::Payload| {
                    lua.queue_invocation(handle, win, payload);
                };
            if matches!(
                self.ui
                    .dispatch_key_fallback(k.code, k.modifiers, &mut lua_invoke),
                Status::Consumed
            ) {
                return Status::Consumed;
            }
        }

        // Tier 5: bare Esc / Ctrl-C dismisses the active modal.
        let lua = &self.lua;
        let mut lua_invoke = |handle: crate::smelt_edit::LuaHandle,
                              win: crate::smelt_edit::WinId,
                              payload: &crate::smelt_edit::Payload| {
            lua.queue_invocation(handle, win, payload);
        };
        self.ui
            .try_dismiss_modal_for_chord(k.code, k.modifiers, &mut lua_invoke)
    }

    pub(crate) fn run_paste_fallback(&mut self, content: String) -> crate::smelt_edit::Status {
        let lua = &self.lua;
        let mut lua_invoke = |handle: crate::smelt_edit::LuaHandle,
                              win: crate::smelt_edit::WinId,
                              payload: &crate::smelt_edit::Payload| {
            lua.queue_invocation(handle, win, payload);
        };
        self.ui.dispatch_paste_fallback(content, &mut lua_invoke)
    }

    fn dispatch_stateful_transient_viewer_key(&mut self, k: KeyEvent) -> StatefulViewerKeyDispatch {
        let Some(win_id) = self.ui.focus() else {
            return StatefulViewerKeyDispatch::NotApplicable;
        };
        if self.ui.overlay_for_leaf(win_id).is_none() && self.ui.focused_modal().is_none() {
            return StatefulViewerKeyDispatch::NotApplicable;
        }
        let Some(win) = self.ui.win(win_id) else {
            return StatefulViewerKeyDispatch::NotApplicable;
        };
        if !win.vim_enabled() || !win.surface().is_readonly_text() {
            return StatefulViewerKeyDispatch::NotApplicable;
        }
        let visual = match win.vim_mode() {
            crate::smelt_edit::VimMode::Visual | crate::smelt_edit::VimMode::VisualLine => true,
            crate::smelt_edit::VimMode::Normal if !win.vim_state().is_idle() => false,
            crate::smelt_edit::VimMode::Normal | crate::smelt_edit::VimMode::Insert => {
                return StatefulViewerKeyDispatch::NotApplicable;
            }
        };

        if visual && self.dispatch_single_global_lua_keymap(k) {
            return StatefulViewerKeyDispatch::Consumed;
        }
        if matches!(
            self.dispatch_viewer_vim_key(win_id, k),
            crate::smelt_edit::Status::Consumed
        ) {
            StatefulViewerKeyDispatch::Consumed
        } else {
            StatefulViewerKeyDispatch::Passthrough {
                vim_dispatched_for: win_id,
                global_keymap_checked: visual,
            }
        }
    }

    /// Transient-focus key cascade tier 3. Wraps the shared
    /// [`Self::dispatch_window_viewer_key`] with two gates:
    ///   * Insert-mode skip - typing inside an editable leaf must not bubble
    ///     navigation keys here.
    ///   * Esc-in-idle-Normal falls through so the modal-dismiss tier (5)
    ///     can close the surface; Visual / pending-sequence Esc stays with vim.
    pub(crate) fn dispatch_transient_viewer_key(&mut self, k: KeyEvent) -> bool {
        let win = match self.ui.focus() {
            Some(w) => w,
            None => return false,
        };
        let (vim_enabled, vim_mode, vim_idle) = match self.ui.win(win) {
            Some(w) => (w.vim_enabled(), w.vim_mode(), w.vim_state().is_idle()),
            None => return false,
        };
        let in_insert = vim_enabled && vim_mode == crate::smelt_edit::VimMode::Insert;
        let insert_escape =
            in_insert && k.code == KeyCode::Esc && k.modifiers == KeyModifiers::NONE;
        if in_insert && !insert_escape {
            return false;
        }
        if k.code == KeyCode::Esc
            && vim_enabled
            && vim_mode == crate::smelt_edit::VimMode::Normal
            && vim_idle
        {
            return false;
        }
        matches!(
            self.dispatch_window_viewer_key(win, k),
            crate::smelt_edit::Status::Consumed
        )
    }

    fn dispatch_transient_viewer_fallback_key(&mut self, k: KeyEvent) -> bool {
        let Some(win) = self.ui.focus() else {
            return false;
        };
        matches!(
            self.dispatch_window_viewer_fallback_key(win, k),
            crate::smelt_edit::Status::Consumed
        )
    }

    fn viewer_key_context(
        &self,
        win_id: crate::smelt_edit::WinId,
    ) -> Option<(crate::smelt_edit::BufId, u16, bool)> {
        let win = self.ui.win(win_id)?;
        let viewport_rows = win.viewport.map(|v| v.rect.height).unwrap_or(0);
        if viewport_rows == 0 {
            return None;
        }
        let buf_id = win.buf;
        if self.ui.buf(buf_id)?.lines().is_empty() {
            return None;
        }
        Some((
            buf_id,
            viewport_rows,
            win.vim_enabled() && win.surface().is_readonly_text(),
        ))
    }

    fn dispatch_viewer_vim_key(
        &mut self,
        win_id: crate::smelt_edit::WinId,
        k: KeyEvent,
    ) -> crate::smelt_edit::Status {
        use crate::smelt_edit::Status;
        let Some((buf_id, viewport_rows, uses_vim)) = self.viewer_key_context(win_id) else {
            return Status::Ignored;
        };
        if !uses_vim {
            return Status::Ignored;
        }

        match self
            .ui
            .win_mut(win_id)
            .expect("viewer window")
            .handle_viewer_key(k)
        {
            crate::smelt_edit::DocumentKeyResult::Command(command) => {
                self.execute_viewer_command(win_id, buf_id, command, viewport_rows)
            }
            crate::smelt_edit::DocumentKeyResult::Consumed => Status::Consumed,
            crate::smelt_edit::DocumentKeyResult::Passthrough => Status::Ignored,
        }
    }

    fn dispatch_viewer_buffer_key(
        &mut self,
        win_id: crate::smelt_edit::WinId,
        k: KeyEvent,
    ) -> crate::smelt_edit::Status {
        let Some((buf_id, viewport_rows, _)) = self.viewer_key_context(win_id) else {
            return crate::smelt_edit::Status::Ignored;
        };
        self.dispatch_buffer_action(win_id, buf_id, k, viewport_rows)
    }

    fn dispatch_window_viewer_fallback_key(
        &mut self,
        win_id: crate::smelt_edit::WinId,
        k: KeyEvent,
    ) -> crate::smelt_edit::Status {
        if self.handle_search_key_for_target(win_id, k) {
            return crate::smelt_edit::Status::Consumed;
        }
        self.dispatch_viewer_buffer_key(win_id, k)
    }

    /// Unified viewer-key dispatcher shared between transcript, overlay leaves,
    /// and any future scrollable read-only window. Resolution order:
    ///   1. Viewer Vim engine for `readonly_text` surfaces - handles motions,
    ///      counts, visual selection, and yanks as viewer commands. Prompt/editor
    ///      actions such as history never run for read-only viewers.
    ///   2. Shared keymap dispatch via [`Self::dispatch_buffer_action`].
    ///      The keymap uses the window's actual `vim_enabled`/`vim_mode`
    ///      context so vim-Normal-only chords (Ctrl-U/D, Ctrl-B/F page
    ///      motion, Ctrl-Y/E line scroll) and emacs chords (Ctrl-V/Alt-V,
    ///      Alt-</>, Ctrl-P/N) route correctly.
    ///
    /// Editing actions (kill, delete, yank, etc.) are silently dropped on
    /// `buf.readonly` buffers - the same dispatcher serves the read-only
    /// transcript and a future read-write Lua-created buffer without
    /// branching the call site.
    pub(crate) fn dispatch_window_viewer_key(
        &mut self,
        win_id: crate::smelt_edit::WinId,
        k: KeyEvent,
    ) -> crate::smelt_edit::Status {
        use crate::smelt_edit::Status;
        if self.handle_search_key_for_target(win_id, k) {
            return Status::Consumed;
        }
        if matches!(self.dispatch_viewer_vim_key(win_id, k), Status::Consumed) {
            return Status::Consumed;
        }
        self.dispatch_viewer_buffer_key(win_id, k)
    }

    fn execute_viewer_command(
        &mut self,
        win_id: crate::smelt_edit::WinId,
        buf_id: crate::smelt_edit::BufId,
        command: crate::smelt_edit::DocumentCommand,
        viewport_rows: u16,
    ) -> crate::smelt_edit::Status {
        use crate::smelt_edit::{DocumentCommand, Status};

        if matches!(command, DocumentCommand::OpenAction) {
            let pos = {
                let Some(win) = self.ui.win(win_id) else {
                    return Status::Ignored;
                };
                let Some(buf) = self.ui.buf(buf_id) else {
                    return Status::Ignored;
                };
                win.viewer_doc_cursor(buf)
            };
            let action = pos.and_then(|pos| self.document_action_at(win_id, pos));
            if let Some(action) = action {
                self.dispatch_span_action(action);
            } else {
                self.record_notice(
                    smelt_core::messages::MessageKind::Info,
                    "actions".into(),
                    "no action under cursor".into(),
                );
            }
            return Status::Consumed;
        }

        let now = self.core.clock.instant_now();
        let copied = if self.win_uses_document_view(win_id) {
            self.execute_document_view_command_for_win(win_id, command, viewport_rows, now)
                .map(crate::smelt_edit::DocumentCopy::Rows)
        } else {
            let (win, buf) = self.ui.win_and_buf_mut(win_id, buf_id);
            let win = win.expect("window");
            let buf = buf.expect("buffer");
            win.execute_viewer_command(buf, command, viewport_rows, now)
        };
        self.copy_viewer_selection(win_id, buf_id, copied, now);
        Status::Consumed
    }

    fn copy_viewer_selection(
        &mut self,
        win_id: crate::smelt_edit::WinId,
        buf_id: crate::smelt_edit::BufId,
        copied: Option<crate::smelt_edit::DocumentCopy>,
        now: std::time::Instant,
    ) {
        match copied {
            Some(crate::smelt_edit::DocumentCopy::Rows(range)) => {
                if let Some(out) = self.copy_document_rows(win_id, range) {
                    self.yank_to_clipboard(out);
                }
            }
            Some(crate::smelt_edit::DocumentCopy::Bytes(range)) => {
                let copied = self.ui.buf(buf_id).map(|buf| buf.copy_range(range.clone()));
                if let Some(win) = self.ui.win_mut(win_id) {
                    win.set_byte_yank_flash(range, now);
                }
                if let Some(out) = copied {
                    self.yank_to_clipboard(out);
                }
            }
            None => {}
        }
    }

    pub(crate) fn dispatch_span_action(&mut self, action: smelt_core::buffer::SpanAction) {
        use smelt_core::buffer::SpanAction;
        use smelt_core::messages::MessageKind;

        let (result, fallback_text) = match action {
            SpanAction::OpenUrl(url) => {
                let result = engine::opener::open_url_if_available_in(&url, &self.core.env.cwd());
                (result, url)
            }
            SpanAction::OpenFile { path, line, col } => {
                let target = engine::opener::FileOpenTarget::new(path, line, col);
                let fallback_text = target.display_location();
                let result =
                    engine::opener::open_file_if_available_in(&target, &self.core.env.cwd());
                if result.opened() {
                    self.record_notice(
                        MessageKind::Info,
                        "actions".into(),
                        format!("opened {fallback_text}"),
                    );
                }
                (result, fallback_text)
            }
        };

        match result {
            engine::opener::OpenResult::Opened => {}
            engine::opener::OpenResult::Unavailable(reason) => {
                self.record_action_open_fallback(reason, &fallback_text);
            }
            engine::opener::OpenResult::Failed(err) => {
                self.record_action_open_fallback(&err, &fallback_text);
            }
        }
    }

    fn record_action_open_fallback(&mut self, reason: &str, text: &str) {
        use smelt_core::messages::MessageKind;

        let body = match self.core.clipboard.write(text) {
            Ok(()) => {
                self.core
                    .clipboard
                    .kill_ring
                    .record_clipboard_write(text.to_string());
                format!("{reason}: copied {text} to clipboard")
            }
            Err(err) => format!("{reason}: could not copy ({err}); open {text} manually"),
        };
        self.record_notice(MessageKind::Warning, "actions".into(), body);
    }

    /// Keymap-driven dispatcher: looks up the binding under the window's
    /// real vim context, executes the resolved [`KeyAction`] against the
    /// (win, buf) pair, and honours `buf.readonly` for editing actions.
    /// Mirrors the prompt's `PromptState::execute_key_action` for the
    /// motion + selection + copy + page-motion subset; non-motion actions
    /// (editing, history) are handled elsewhere (the prompt) or dropped
    /// (read-only buffers).
    fn dispatch_buffer_action(
        &mut self,
        win_id: crate::smelt_edit::WinId,
        buf_id: crate::smelt_edit::BufId,
        k: KeyEvent,
        viewport_rows: u16,
    ) -> crate::smelt_edit::Status {
        use crate::keymap::{lookup, KeyAction, KeyContext};
        use crate::smelt_edit::{Status, VimMode};

        let (vim_enabled, vim_mode, readonly, readonly_text, buf_empty) =
            match (self.ui.win(win_id), self.ui.buf(buf_id)) {
                (Some(w), Some(b)) => (
                    w.vim_enabled(),
                    w.vim_mode(),
                    b.readonly,
                    w.surface().is_readonly_text(),
                    b.text().is_empty(),
                ),
                _ => return Status::Ignored,
            };
        let ctx = KeyContext {
            buf_empty,
            vim_non_insert: vim_enabled
                && matches!(
                    vim_mode,
                    VimMode::Normal | VimMode::Visual | VimMode::VisualLine
                ),
            vim_enabled,
            agent_running: false,
        };
        let Some(action) = lookup(k.code, k.modifiers, &ctx) else {
            return Status::Ignored;
        };

        if readonly_text
            || self
                .ui
                .win(win_id)
                .is_some_and(|win| win.has_materialized_rows())
        {
            let extending = matches!(
                action,
                KeyAction::SelectUp
                    | KeyAction::SelectDown
                    | KeyAction::SelectStartOfLine
                    | KeyAction::SelectEndOfLine
                    | KeyAction::SelectWordForward
                    | KeyAction::SelectWordBackward
            );
            let command = match action {
                KeyAction::MoveUp | KeyAction::SelectUp => {
                    Some(crate::smelt_edit::DocumentCommand::MoveRows(-1))
                }
                KeyAction::MoveDown | KeyAction::SelectDown => {
                    Some(crate::smelt_edit::DocumentCommand::MoveRows(1))
                }
                KeyAction::MoveWordForward | KeyAction::SelectWordForward => {
                    Some(crate::smelt_edit::DocumentCommand::WordForward(1))
                }
                KeyAction::MoveWordBackward | KeyAction::SelectWordBackward => {
                    Some(crate::smelt_edit::DocumentCommand::WordBackward(1))
                }
                KeyAction::PageUp => Some(crate::smelt_edit::DocumentCommand::PageRows(-1)),
                KeyAction::PageDown => Some(crate::smelt_edit::DocumentCommand::PageRows(1)),
                KeyAction::HalfPageUp => Some(crate::smelt_edit::DocumentCommand::MoveRows(
                    -((viewport_rows as isize) / 2).max(1),
                )),
                KeyAction::HalfPageDown => Some(crate::smelt_edit::DocumentCommand::MoveRows(
                    ((viewport_rows as isize) / 2).max(1),
                )),
                KeyAction::ScrollLineUp => Some(crate::smelt_edit::DocumentCommand::ScrollRows(-1)),
                KeyAction::ScrollLineDown => {
                    Some(crate::smelt_edit::DocumentCommand::ScrollRows(1))
                }
                KeyAction::MoveStartOfBuffer => {
                    Some(crate::smelt_edit::DocumentCommand::BufferStart)
                }
                KeyAction::MoveEndOfBuffer => Some(crate::smelt_edit::DocumentCommand::BufferEnd),
                KeyAction::MoveStartOfLine | KeyAction::SelectStartOfLine => {
                    Some(crate::smelt_edit::DocumentCommand::LineStart)
                }
                KeyAction::MoveEndOfLine | KeyAction::SelectEndOfLine => {
                    Some(crate::smelt_edit::DocumentCommand::LineEnd)
                }
                KeyAction::CopySelection => Some(crate::smelt_edit::DocumentCommand::YankSelection),
                _ => None,
            };
            if let Some(command) = command {
                let now = self.core.clock.instant_now();
                let anchor_active = self
                    .ui
                    .win(win_id)
                    .is_some_and(|win| win.row_selection_anchor_active());
                if extending && !anchor_active {
                    self.execute_document_view_command_for_win(
                        win_id,
                        crate::smelt_edit::DocumentCommand::StartVisual,
                        viewport_rows,
                        now,
                    );
                } else if !extending
                    && !matches!(command, crate::smelt_edit::DocumentCommand::YankSelection)
                {
                    self.execute_document_view_command_for_win(
                        win_id,
                        crate::smelt_edit::DocumentCommand::ClearSelection,
                        viewport_rows,
                        now,
                    );
                }
                let copied =
                    self.execute_document_view_command_for_win(win_id, command, viewport_rows, now);
                if let Some(range) = copied {
                    if let Some(out) = self.copy_document_rows(win_id, range) {
                        self.yank_to_clipboard(out);
                    }
                }
                return Status::Consumed;
            }
        }

        let extending = matches!(
            action,
            KeyAction::SelectLeft
                | KeyAction::SelectRight
                | KeyAction::SelectUp
                | KeyAction::SelectDown
                | KeyAction::SelectWordForward
                | KeyAction::SelectWordBackward
                | KeyAction::SelectStartOfLine
                | KeyAction::SelectEndOfLine
        );
        let is_motion = matches!(
            action,
            KeyAction::MoveLeft
                | KeyAction::MoveRight
                | KeyAction::MoveUp
                | KeyAction::MoveDown
                | KeyAction::MoveStartOfLine
                | KeyAction::MoveEndOfLine
                | KeyAction::MoveWordForward
                | KeyAction::MoveWordBackward
                | KeyAction::MoveStartOfBuffer
                | KeyAction::MoveEndOfBuffer
                | KeyAction::PageUp
                | KeyAction::PageDown
                | KeyAction::HalfPageUp
                | KeyAction::HalfPageDown
                | KeyAction::ScrollLineUp
                | KeyAction::ScrollLineDown
        );

        let cpos_before = self.ui.win(win_id).expect("window").cpos();
        let win = self.ui.win_mut(win_id).expect("window");
        if is_motion {
            win.clear_selection_anchor();
        } else if extending {
            win.extend_selection(cpos_before);
        }

        // Line-delta motions resolve through the window's display-line API,
        // which respects soft-wrap and gutter math.
        let line_delta: Option<isize> = match action {
            KeyAction::MoveUp | KeyAction::SelectUp => Some(-1),
            KeyAction::MoveDown | KeyAction::SelectDown => Some(1),
            KeyAction::PageUp => Some(-(viewport_rows as isize)),
            KeyAction::PageDown => Some(viewport_rows as isize),
            KeyAction::HalfPageUp => Some(-((viewport_rows as isize) / 2).max(1)),
            KeyAction::HalfPageDown => Some(((viewport_rows as isize) / 2).max(1)),
            KeyAction::ScrollLineUp => Some(-1),
            KeyAction::ScrollLineDown => Some(1),
            _ => None,
        };
        if let Some(d) = line_delta {
            let (win, buf) = self.ui.win_and_buf_mut(win_id, buf_id);
            let win = win.expect("window");
            let buf = buf.expect("buffer");
            win.move_cursor_by_lines(buf, d, viewport_rows);
            win.update_tail_state(buf, viewport_rows);
            return Status::Consumed;
        }

        let buf = match self.ui.buf(buf_id) {
            Some(b) => b,
            None => return Status::Ignored,
        };
        let text = buf.text().to_string();
        let cpos = self.ui.win(win_id).expect("window").cpos();
        let mv: Option<usize> = match action {
            KeyAction::MoveLeft | KeyAction::SelectLeft => {
                Some(crate::smelt_edit::text::prev_grapheme_boundary(&text, cpos))
            }
            KeyAction::MoveRight | KeyAction::SelectRight => {
                Some(crate::smelt_edit::text::next_grapheme_boundary(&text, cpos))
            }
            KeyAction::MoveStartOfLine | KeyAction::SelectStartOfLine => {
                Some(crate::smelt_edit::text::line_start(&text, cpos))
            }
            KeyAction::MoveEndOfLine | KeyAction::SelectEndOfLine => {
                Some(crate::smelt_edit::text::line_end(&text, cpos))
            }
            KeyAction::MoveWordForward | KeyAction::SelectWordForward => {
                Some(crate::smelt_edit::text::word_forward_pos(
                    &text,
                    cpos,
                    crate::smelt_edit::text::CharClass::Word,
                ))
            }
            KeyAction::MoveWordBackward | KeyAction::SelectWordBackward => {
                Some(crate::smelt_edit::text::word_backward_pos(
                    &text,
                    cpos,
                    crate::smelt_edit::text::CharClass::Word,
                ))
            }
            KeyAction::MoveStartOfBuffer => Some(0),
            KeyAction::MoveEndOfBuffer => Some(text.len()),
            KeyAction::CopySelection => {
                let win = self.ui.win(win_id).expect("window");
                if let Some((s, e)) = win.selection_range(buf) {
                    let s = crate::smelt_edit::text::snap_grapheme(&text, s);
                    let e = crate::smelt_edit::text::snap_grapheme(&text, e);
                    if s < e {
                        let out = buf.copy_range(s..e);
                        if !out.clipboard.is_empty() {
                            let _ = self.core.clipboard.write(&out.clipboard);
                        }
                    }
                }
                return Status::Consumed;
            }
            // Read-only buffers silently drop editing actions; the prompt
            // path is the only consumer for these.
            _ if readonly => return Status::Ignored,
            _ => None,
        };
        drop(text);
        if let Some(new_cpos) = mv {
            let (win, buf) = self.ui.win_and_buf_mut(win_id, buf_id);
            let win = win.expect("window");
            let buf = buf.expect("buffer");
            win.set_cpos(new_cpos);
            win.set_curswant(None);
            // A shift-motion that resolved to no movement (e.g. Shift+End at
            // EOL) leaves `selection_anchor == cpos` - a degenerate, empty
            // selection that downstream code treats as "no selection" but
            // whose anchor still points into the buffer. Clear it so a
            // follow-up source-shrinking edit can't orphan it.
            if win.selection_anchor() == Some(win.cpos()) {
                win.clear_selection_anchor();
            }
            win.resync(buf, viewport_rows);
            return Status::Consumed;
        }
        Status::Ignored
    }
}

/// Adapter that lets the pure [`smelt_core::keymap::match_chord`] loop call
/// into the live Lua keymap registry. Carries the `vim_mode_at_chord_start`
/// context pair that handlers see on multi-key matches.
struct LuaChordOracle<'a> {
    lua: &'a crate::lua::LuaExecution,
    vim_mode: Option<&'a str>,
    vim_mode_at_start: Option<crate::smelt_edit::VimMode>,
}

impl smelt_core::keymap::ChordOracle for LuaChordOracle<'_> {
    fn has_longer(&self, seq: &str) -> bool {
        self.lua.chord_has_longer(seq, self.vim_mode)
    }
    fn try_keymap(&mut self, seq: &str) -> smelt_core::lua::runtime::KeymapResult {
        let ctx: Vec<(&str, String)> = vec![(
            "vim_mode_at_chord_start",
            self.vim_mode_at_start
                .map(|m| crate::lua::LuaVimMode::from(m).label().to_string())
                .unwrap_or_default(),
        )];
        self.lua
            .run_keymap(seq, self.vim_mode, Some(ctx.as_slice()))
    }
}

#[cfg(test)]
mod tests {
    use crate::app::test_harness::{Action, TestApp};
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    use protocol::AgentMode;
    use smelt_core::working::TurnPhase;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    #[test]
    fn key_releases_are_ignored_without_disabling_key_repeat() {
        let mut app = TestApp::builder().build();
        for kind in [
            KeyEventKind::Press,
            KeyEventKind::Repeat,
            KeyEventKind::Release,
        ] {
            app.app
                .dispatch_terminal_event(Event::Key(KeyEvent::new_with_kind(
                    KeyCode::Char('a'),
                    KeyModifiers::NONE,
                    kind,
                )));
        }

        assert_eq!(app.state().prompt_text, "aa");
    }

    #[test]
    fn running_esc_esc_cancels_but_single_esc_does_not() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);

        app.press(KeyCode::Esc);
        assert!(
            app.agent_running(),
            "first Esc should only arm the Lua Esc Esc sequence"
        );

        app.press(KeyCode::Esc);
        assert!(
            !app.agent_running(),
            "second Esc should resolve the Lua cancel sequence"
        );
    }

    #[test]
    fn running_esc_esc_drains_queued_input_before_canceling() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        app.push_queued_message("queued steer".to_string());

        app.press(KeyCode::Esc);
        app.press(KeyCode::Esc);

        let state = app.state();
        assert!(
            state.agent_running,
            "Esc Esc with queued input should preserve the active turn"
        );
        assert!(
            state.queued_inputs.is_empty(),
            "queued input should be drained instead of left pending"
        );
        assert_eq!(state.prompt_text, "queued steer");
    }

    fn queue_stages(app: &TestApp) -> Vec<String> {
        app.app.prompt.queued_kinds()
    }

    fn insert_prompt_image(app: &mut TestApp, label: &str) {
        let mut pctx = crate::input::prompt_ctx_mut(&mut app.app.ui);
        app.app.prompt.insert_image_for_harness(
            &mut pctx,
            label.to_string(),
            "data:image/png;base64,AAAA".to_string(),
        );
    }

    fn lua_command_queue_guard() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn ctrl_c_clear_is_undoable_and_redoable() {
        let mut app = TestApp::builder().build();
        app.type_text("hello");

        app.press_mod(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(app.state().prompt_text, "");

        app.press_mod(KeyCode::Char('_'), KeyModifiers::CONTROL);
        assert_eq!(app.state().prompt_text, "hello");

        app.press_mod(KeyCode::Char('_'), KeyModifiers::ALT);
        assert_eq!(app.state().prompt_text, "");
    }

    #[test]
    fn cmd_z_and_cmd_shift_z_undo_redo_prompt_edits() {
        let mut app = TestApp::builder().build();
        app.type_text("hello");
        app.press_mod(KeyCode::Char('c'), KeyModifiers::CONTROL);

        app.press_mod(KeyCode::Char('z'), KeyModifiers::SUPER);
        assert_eq!(app.state().prompt_text, "hello");

        app.press_mod(
            KeyCode::Char('z'),
            KeyModifiers::SUPER.union(KeyModifiers::SHIFT),
        );
        assert_eq!(app.state().prompt_text, "");
    }

    #[test]
    fn empty_enter_promotes_turn_queue_to_request_queue() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        app.type_text("check this first");
        app.press(KeyCode::Enter);
        assert_eq!(
            app.state().queued_inputs,
            vec!["check this first".to_string()]
        );
        assert_eq!(queue_stages(&app), vec!["turn".to_string()]);

        app.clear_actions();
        app.press(KeyCode::Enter);

        assert!(app.agent_running());
        assert_eq!(
            app.state().queued_inputs,
            vec!["check this first".to_string()]
        );
        assert_eq!(queue_stages(&app), vec!["request".to_string()]);
        assert!(app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::Steer { input }
                    if input.provider_content().text_content() == "check this first")
        )));
    }

    #[test]
    fn empty_enter_promotes_turn_queue_during_compacting() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        let _dispatch = app
            .app
            .conversation
            .begin_dispatch()
            .expect("test turn enters dispatch");
        app.app.working.begin(TurnPhase::Compacting);

        app.type_text("check this first");
        app.press(KeyCode::Enter);
        assert_eq!(
            app.state().queued_inputs,
            vec!["check this first".to_string()]
        );
        assert_eq!(queue_stages(&app), vec!["turn".to_string()]);

        app.clear_actions();
        app.press(KeyCode::Enter);

        assert!(app.agent_running());
        assert_eq!(
            app.state().queued_inputs,
            vec!["check this first".to_string()]
        );
        assert_eq!(queue_stages(&app), vec!["request".to_string()]);
        assert!(app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::Steer { input }
                    if input.provider_content().text_content() == "check this first")
        )));
        assert!(!app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::StartTurn(_))
        )));
    }

    #[test]
    fn enter_queues_image_prompt_for_next_turn_without_dropping_attachment() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        app.type_text("see ");
        insert_prompt_image(&mut app, "pic.png");

        app.press(KeyCode::Enter);

        assert_eq!(app.state().queued_inputs, vec!["see [pic.png]".to_string()]);
        assert_eq!(queue_stages(&app), vec!["turn".to_string()]);

        app.clear_actions();
        app.press(KeyCode::Enter);

        assert_eq!(queue_stages(&app), vec!["turn".to_string()]);
        assert!(!app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::Steer { .. })
        )));
    }

    #[test]
    fn request_queue_with_image_restores_prompt_instead_of_dropping_attachment() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        app.type_text("see ");
        insert_prompt_image(&mut app, "pic.png");

        app.press_mod(KeyCode::Enter, KeyModifiers::CONTROL);

        let state = app.state();
        assert!(state.queued_inputs.is_empty());
        assert_eq!(
            state.prompt_text,
            format!("see {}", crate::input::ATTACHMENT_MARKER)
        );
        assert!(app.app.overlays.notification().is_some());
        assert!(!app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::Steer { .. })
        )));
    }

    #[test]
    fn ctrl_enter_queues_prompt_to_current_request() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        app.push_queued_message("next turn".to_string());
        app.type_text("right now");

        app.clear_actions();
        app.press_mod(KeyCode::Enter, KeyModifiers::CONTROL);

        assert!(app.agent_running());
        assert_eq!(
            app.state().queued_inputs,
            vec!["right now".to_string(), "next turn".to_string()]
        );
        assert_eq!(
            queue_stages(&app),
            vec!["request".to_string(), "turn".to_string()]
        );
        assert!(app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::Steer { input }
                    if input.provider_content().text_content() == "right now")
        )));
    }

    #[test]
    fn ctrl_enter_slash_command_queues_expansion_to_current_request() {
        let _guard = lua_command_queue_guard();
        let mut app = TestApp::builder().build();
        assert!(app.run_lua(
            r#"
                smelt.cmd.register("request-body", function(arg)
                    smelt.engine.submit_command(
                        "request-body",
                        "expanded " .. (arg or ""),
                        nil,
                        "request-body " .. (arg or "")
                    )
                end, { busy = "queue_request" })
            "#
        ));
        app.start_turn(1);
        app.push_queued_message("next turn".to_string());
        app.type_text("/request-body now");

        app.clear_actions();
        app.press_mod(KeyCode::Enter, KeyModifiers::CONTROL);

        assert_eq!(
            app.state().queued_inputs,
            vec!["/request-body now".to_string(), "next turn".to_string()]
        );
        assert_eq!(
            queue_stages(&app),
            vec!["request".to_string(), "turn".to_string()]
        );
        assert!(app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::Steer { input }
                    if input.provider_content().text_content() == "expanded now"
                        && matches!(input, protocol::StartTurnInput::User { command: true, .. }))
        )));
    }

    #[test]
    fn empty_enter_interrupts_promoted_request_queue() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        app.type_text("run now");
        app.press(KeyCode::Enter);
        app.press(KeyCode::Enter);
        assert_eq!(queue_stages(&app), vec!["request".to_string()]);

        app.clear_actions();
        app.press(KeyCode::Enter);

        assert!(app.agent_running());
        assert!(app.state().queued_inputs.is_empty());
        assert!(app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::Unsteer { count } if *count == 1)
        )));
        assert!(app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::Cancel)
        )));
        assert!(app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::StartTurn(payload) if payload.input.provider_content().text_content() == "run now")
        )));
    }

    #[test]
    fn queued_turn_submit_timeout_resumes_without_making_session_read_only() {
        let mut app = TestApp::builder().build();
        app.type_text("baseline");
        app.press(KeyCode::Enter);
        assert!(app.finish_turn());
        let _ = app.app.flush_persist();
        assert!(!app.app.conversation.canonical_operations_are_pending());

        let release_persistence = app.app.conversation.pause_persistence();
        app.type_text("queued submit");
        app.press(KeyCode::Enter);

        assert!(app.app.turn_submission_is_pending());
        assert!(app.app.turn_lifecycle_is_active());
        assert!(!app.agent_running());
        assert!(!app.session_is_read_only());
        assert_eq!(app.state().prompt_text, "");
        assert!(!app.app.fail_pending_turn_submission(
            Some(crate::persist::CanonicalCommandId::new(u64::MAX)),
            &crate::persist::PersistenceCause::invariant("unrelated command failed"),
        ));
        assert!(app.app.turn_submission_is_pending());

        release_persistence
            .send(())
            .expect("release turn submission");
        let flush = app.app.flush_persist();
        assert!(matches!(
            flush,
            crate::persist::PersistenceFlushOutcome::Durable { .. }
        ));
        assert!(app.agent_running());
        assert!(!app.app.turn_submission_is_pending());
        assert!(!app.session_is_read_only());
    }

    #[test]
    fn queued_process_status_accepts_persistence_pending_turn() {
        let mut app = TestApp::builder().build();
        app.type_text("baseline");
        app.press(KeyCode::Enter);
        assert!(app.finish_turn());
        let _ = app.app.flush_persist();

        let release_persistence = app.app.conversation.pause_persistence();
        let queued = crate::app::QueuedInput::ProcessStatus(protocol::HistoryNote::process_status(
            "background process finished",
        ));
        assert!(app.app.start_queued_input(queued).is_ok());
        assert!(app.app.turn_submission_is_pending());
        assert!(!app.agent_running());

        release_persistence
            .send(())
            .expect("release process-status turn submission");
        let flush = app.app.flush_persist();
        assert!(matches!(
            flush,
            crate::persist::PersistenceFlushOutcome::Durable { .. }
        ));
        assert!(app.agent_running());
        assert!(!app.app.turn_submission_is_pending());
    }

    #[test]
    fn cancelling_pending_turn_submit_does_not_dispatch_after_receipt() {
        let mut app = TestApp::builder().build();
        app.type_text("baseline");
        app.press(KeyCode::Enter);
        assert!(app.finish_turn());
        let _ = app.app.flush_persist();
        let baseline_turn_id = app
            .app
            .conversation
            .last_terminal_turn_id()
            .expect("baseline turn is terminal");
        app.clear_actions();

        let release_persistence = app.app.conversation.pause_persistence();
        app.type_text("cancel before dispatch");
        app.press(KeyCode::Enter);
        assert!(app.app.turn_submission_is_pending());
        assert!(!app.agent_running());

        app.press(KeyCode::Esc);
        app.press(KeyCode::Esc);
        assert!(app.app.turn_submission_is_pending());
        assert!(!app.agent_running());

        release_persistence
            .send(())
            .expect("release cancelled turn submission");
        let flush = app.app.flush_persist();
        assert!(matches!(
            flush,
            crate::persist::PersistenceFlushOutcome::Durable { .. }
        ));
        assert!(!app.app.turn_submission_is_pending());
        assert!(!app.agent_running());
        assert!(!app.session_is_read_only());
        let terminal_flush = app.app.flush_persist();
        assert!(matches!(
            terminal_flush,
            crate::persist::PersistenceFlushOutcome::Durable { .. }
        ));
        assert_eq!(
            app.app.conversation.last_terminal_turn_id(),
            Some(baseline_turn_id + 1)
        );
        assert!(app.actions().iter().all(|action| !matches!(
            action,
            Action::EngineSend(command)
                if matches!(command.as_ref(), protocol::UiCommand::StartTurn(payload)
                    if payload.input.provider_content().text_content() == "cancel before dispatch")
        )));
    }

    #[test]
    fn replacing_session_resolves_pending_turn_without_dispatching_it() {
        let mut app = TestApp::builder().build();
        app.type_text("baseline");
        app.press(KeyCode::Enter);
        assert!(app.finish_turn());
        let _ = app.app.flush_persist();

        let release_persistence = app.app.conversation.pause_persistence();
        app.type_text("replace before dispatch");
        app.press(KeyCode::Enter);
        assert!(app.app.turn_submission_is_pending());
        assert!(!app.agent_running());

        let replacement =
            smelt_core::session::Session::new(app.app.core.env.pid(), app.app.core.env.cwd());
        let replacement_id = replacement.id.clone();
        let release_thread = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            release_persistence
                .send(())
                .expect("release replaced turn submission");
        });
        app.app.load_session(replacement);
        release_thread.join().expect("release persistence thread");

        assert_eq!(app.session_snapshot().id, replacement_id);
        assert!(!app.app.turn_submission_is_pending());
        assert!(!app.agent_running());
        assert!(!app.session_is_read_only());
        assert!(app.drain_engine_sends().iter().all(|command| !matches!(
            command,
            protocol::UiCommand::StartTurn(payload)
                if payload.input.provider_content().text_content() == "replace before dispatch"
        )));
    }

    #[test]
    fn interrupt_waits_for_terminal_turn_before_submitting_queued_request() {
        let mut app = TestApp::builder().build();
        app.type_text("first turn");
        app.press(KeyCode::Enter);
        assert!(app.agent_running());
        let _ = app.app.flush_persist();
        app.feed_one(crate::app::test_harness::SourceEvent::engine(
            protocol::EngineEvent::ToolStarted {
                invocation_id: protocol::InvocationId::new(1),
                call_id: "call-1".into(),
                tool_name: "read_file".into(),
                args: std::collections::HashMap::new(),
                called_at_ms: 1,
            },
        ));

        app.type_text("run now");
        app.press(KeyCode::Enter);
        app.press(KeyCode::Enter);
        assert_eq!(queue_stages(&app), vec!["request".to_string()]);

        let release_persistence = app.app.conversation.pause_persistence();

        app.press(KeyCode::Enter);
        assert!(
            !app.session_is_read_only(),
            "interrupt submitted the queued request before its terminal transition was durable: {}",
            app.app.conversation.read_only_reason()
        );
        assert!(!app.agent_running());
        assert_eq!(app.state().queued_inputs, vec!["run now"]);

        release_persistence
            .send(())
            .expect("release terminal transition");
        let flush = app.app.flush_persist();
        assert!(
            matches!(
                flush,
                crate::persist::PersistenceFlushOutcome::Durable { .. }
            ),
            "terminal transition did not become durable: {flush:?}"
        );

        assert!(
            !app.session_is_read_only(),
            "queued request timeout made the session read-only: {}",
            app.app.conversation.read_only_reason()
        );
        assert!(app.agent_running(), "queued request did not start");
        assert!(app.state().queued_inputs.is_empty());
        assert!(app.session_snapshot().history.iter().any(|item| {
            matches!(item, protocol::HistoryItem::User { content, .. }
                if content.text_content() == "run now")
        }));

        app.set_session_title(
            "Writable after interrupt".into(),
            "writable-after-interrupt".into(),
            None,
        );
        let _ = app.app.flush_persist();
        assert_eq!(
            app.session_snapshot().title.as_deref(),
            Some("Writable after interrupt")
        );
    }

    #[test]
    fn empty_enter_promotes_queued_custom_command_expansion() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        assert!(app.run_lua(
            r#"smelt.engine.submit_command("queued-custom", "custom body", nil, "queued-custom")"#
        ));
        assert_eq!(
            app.state().queued_inputs,
            vec!["/queued-custom".to_string()]
        );
        assert_eq!(queue_stages(&app), vec!["turn".to_string()]);

        app.clear_actions();
        app.press(KeyCode::Enter);

        assert!(app.agent_running());
        assert_eq!(
            app.state().queued_inputs,
            vec!["/queued-custom".to_string()]
        );
        assert_eq!(queue_stages(&app), vec!["request".to_string()]);
        assert!(app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::Steer { input }
                    if input.provider_content().text_content() == "custom body"
                        && matches!(input, protocol::StartTurnInput::User { command: true, .. }))
        )));
    }

    #[test]
    fn queue_request_busy_command_enqueues_expanded_request() {
        let _guard = lua_command_queue_guard();
        let mut app = TestApp::builder().build();
        assert!(app.run_lua(
            r#"
                smelt.cmd.register("qbody", function(arg)
                    smelt.engine.submit_command(
                        "qbody",
                        "expanded " .. (arg or ""),
                        nil,
                        "qbody " .. (arg or "")
                    )
                end, { busy = "queue_request" })
            "#
        ));
        app.start_turn(1);
        app.type_text("/qbody arg");
        app.press(KeyCode::Enter);

        assert_eq!(app.state().queued_inputs, vec!["/qbody arg".to_string()]);
        assert_eq!(queue_stages(&app), vec!["turn".to_string()]);

        app.clear_actions();
        app.press(KeyCode::Enter);

        assert_eq!(queue_stages(&app), vec!["request".to_string()]);
        assert!(app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::Steer { input }
                    if input.provider_content().text_content() == "expanded arg"
                        && matches!(input, protocol::StartTurnInput::User { command: true, .. }))
        )));
    }

    #[test]
    fn queue_request_busy_command_overrides_survive_interrupt_start() {
        let _guard = lua_command_queue_guard();
        let mut app = TestApp::builder().build();
        assert!(app.run_lua(
            r#"
                smelt.cmd.register("qoverride", function(_)
                    smelt.engine.submit_command(
                        "qoverride",
                        "override body",
                        { reasoning_effort = "high" },
                        "qoverride"
                    )
                end, { busy = "queue_request" })
            "#
        ));
        app.start_turn(1);
        app.type_text("/qoverride");
        app.press(KeyCode::Enter);
        app.press(KeyCode::Enter);
        assert_eq!(queue_stages(&app), vec!["request".to_string()]);

        app.clear_actions();
        app.press(KeyCode::Enter);

        assert!(app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::StartTurn(payload)
                if payload.input.provider_content().text_content() == "override body"
                    && payload.reasoning_effort == protocol::ReasoningEffort::High)
        )));
    }

    #[test]
    fn blocked_slash_command_does_not_enqueue_while_running() {
        let mut app = TestApp::builder().build();
        assert!(app.run_lua(
            r#"
                smelt.cmd.register("blocked", function(_)
                    _G.blocked_ran = 1
                    smelt.engine.submit_command("blocked", "should not run", nil, "blocked")
                end, { busy = "reject" })
            "#
        ));
        app.start_turn(1);
        app.type_text("/blocked arg");
        app.press(KeyCode::Enter);

        assert!(app.state().queued_inputs.is_empty());
        assert_eq!(app.lua_int_global("blocked_ran"), None);
    }

    #[test]
    fn empty_enter_does_not_interrupt_for_turn_only_status_item() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        let note = protocol::HistoryNote::process_status("Background process 42 exited.");
        app.app
            .prompt
            .try_queue_turn(crate::app::QueuedInput::ProcessStatus(note));

        app.clear_actions();
        app.press(KeyCode::Enter);

        assert!(app.agent_running());
        assert_eq!(
            app.state().queued_inputs,
            vec!["Background process 42 exited.".to_string()]
        );
        assert_eq!(queue_stages(&app), vec!["turn".to_string()]);
        assert!(!app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::Cancel | protocol::UiCommand::Steer { .. } | protocol::UiCommand::StartTurn(_))
        )));
    }

    #[test]
    fn empty_enter_busy_without_agent_does_not_promote_to_request() {
        let mut app = TestApp::builder().build();
        assert!(app.run_lua("_G._busy_handle = smelt.work.busy('busy')"));
        app.type_text("wait for busy");
        app.press(KeyCode::Enter);
        assert_eq!(queue_stages(&app), vec!["turn".to_string()]);

        app.clear_actions();
        app.press(KeyCode::Enter);

        assert!(!app.agent_running());
        assert_eq!(app.state().queued_inputs, vec!["wait for busy".to_string()]);
        assert_eq!(queue_stages(&app), vec!["turn".to_string()]);
        assert!(!app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::Steer { .. })
        )));
    }

    #[test]
    fn steered_ack_drains_only_request_queue() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        app.push_queued_message("request now".to_string());
        app.push_queued_message("next turn".to_string());
        app.press(KeyCode::Enter);
        assert_eq!(
            queue_stages(&app),
            vec!["request".to_string(), "turn".to_string()]
        );

        app.feed_one(crate::app::test_harness::SourceEvent::engine(
            protocol::EngineEvent::Steered {
                text: "request now".to_string(),
                count: 1,
            },
        ));

        assert_eq!(app.state().queued_inputs, vec!["next turn".to_string()]);
        assert_eq!(queue_stages(&app), vec!["turn".to_string()]);

        app.clear_actions();
        app.press(KeyCode::Enter);

        assert_eq!(app.state().queued_inputs, vec!["next turn".to_string()]);
        assert_eq!(queue_stages(&app), vec!["request".to_string()]);
        assert!(app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::Steer { input }
                    if input.provider_content().text_content() == "next turn")
        )));
    }

    #[test]
    fn interrupt_starts_request_queue_before_turn_queue() {
        let mut app = TestApp::builder().build();
        app.start_turn(1);
        app.push_queued_message("run first".to_string());
        app.push_queued_message("run second".to_string());
        app.press(KeyCode::Enter);
        assert_eq!(
            queue_stages(&app),
            vec!["request".to_string(), "turn".to_string()]
        );

        app.clear_actions();
        app.press(KeyCode::Enter);

        assert!(app.agent_running());
        assert_eq!(app.state().queued_inputs, vec!["run second".to_string()]);
        assert_eq!(queue_stages(&app), vec!["turn".to_string()]);
        assert!(app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::StartTurn(payload) if payload.input.provider_content().text_content() == "run first")
        )));
    }

    #[test]
    fn empty_enter_continue_does_not_add_user_block() {
        let mut app = TestApp::builder().build();
        app.type_text("before");
        app.press(KeyCode::Enter);
        let first_turn_id = app.current_turn_id().expect("first turn starts");
        app.push_assistant_text("done");
        app.feed_one(crate::app::test_harness::SourceEvent::engine(
            protocol::EngineEvent::TurnComplete {
                turn_id: first_turn_id,
                history: None,
                meta: None,
            },
        ));
        let user_blocks_before = app
            .app
            .conversation
            .transcript()
            .history()
            .order
            .iter()
            .filter_map(|id| app.app.conversation.transcript().history().block(*id))
            .filter(|block| matches!(block, smelt_core::Block::User { .. }))
            .count();
        app.clear_actions();

        app.press(KeyCode::Enter);

        assert!(app.agent_running());
        let history = app.app.conversation.transcript().history();
        let user_blocks_after = history
            .order
            .iter()
            .filter_map(|id| history.block(*id))
            .filter(|block| matches!(block, smelt_core::Block::User { .. }))
            .count();
        assert_eq!(user_blocks_after, user_blocks_before);
        assert!(app.actions().iter().any(|action| matches!(
            action,
            Action::EngineSend(cmd) if matches!(cmd.as_ref(), protocol::UiCommand::StartTurn(payload) if payload.input.provider_content().is_empty())
        )));
    }

    #[test]
    fn vim_insert_esc_bypasses_global_esc_prefix_on_focused_surface() {
        let mut app = TestApp::builder().with_vim(true).build();
        assert!(app.run_lua(
            r#"
                smelt.keymap.set("", "<Esc><Esc>", function()
                    _G.esc_esc_hit = (_G.esc_esc_hit or 0) + 1
                end)
            "#
        ));
        app.app.handle_resize(80, 16);
        app.app
            .push_block(smelt_core::transcript_model::Block::Text {
                content: "transcript row".to_string().into(),
            });
        app.render_silent();
        app.app.app_focus = crate::app::AppFocus::Content;
        app.app.ui.set_focus(crate::app::TRANSCRIPT_WIN);
        app.app.transcript_win_mut().set_vim_enabled(true);
        app.app
            .transcript_win_mut()
            .set_vim_mode(crate::smelt_edit::VimMode::Insert);

        app.press(KeyCode::Esc);

        assert_eq!(
            app.app.transcript_win().vim_mode(),
            crate::smelt_edit::VimMode::Normal
        );
        assert!(app.app.timers.pending_chord.is_none());
        assert_eq!(app.lua_int_global("esc_esc_hit"), None);
    }

    #[test]
    fn lua_keymap_prefix_enters_pending_state_until_match() {
        let mut app = TestApp::builder().with_vim(true).build();
        assert!(app.run_lua(
            r#"
                smelt.keymap.set_leader("<space>")
                smelt.keymap.set("n", "<leader>r", function()
                    _G.leader_hit = (_G.leader_hit or 0) + 1
                end)
            "#
        ));
        app.app
            .prompt_win_mut()
            .set_vim_mode(crate::smelt_edit::VimMode::Normal);

        app.press(KeyCode::Char(' '));

        assert_eq!(app.state().prompt_text, "");
        assert!(app.app.timers.pending_chord.is_some());
        app.app.publish_diff_signals();
        assert_eq!(
            app.app
                .core
                .signals
                .get::<String>("keymap_pending")
                .as_deref(),
            Some("<space>")
        );

        app.press(KeyCode::Char('r'));

        assert_eq!(app.lua_int_global("leader_hit"), Some(1));
        assert!(app.app.timers.pending_chord.is_none());
    }

    #[test]
    fn lua_keymap_prefix_clears_on_escape_cancel() {
        let mut app = TestApp::builder().with_vim(true).build();
        assert!(app.run_lua(
            r#"
                smelt.keymap.set_leader("<space>")
                smelt.keymap.set("n", "<leader>r", function()
                    _G.leader_hit = (_G.leader_hit or 0) + 1
                end)
            "#
        ));
        app.app
            .prompt_win_mut()
            .set_vim_mode(crate::smelt_edit::VimMode::Normal);

        app.press(KeyCode::Char(' '));
        app.press(KeyCode::Esc);
        app.app.publish_diff_signals();

        assert!(app.app.timers.pending_chord.is_none());
        assert_eq!(
            app.app
                .core
                .signals
                .get::<String>("keymap_pending")
                .as_deref(),
            Some("")
        );

        app.press(KeyCode::Char('r'));

        assert_eq!(app.lua_int_global("leader_hit"), None);
    }

    #[test]
    fn lua_keymap_prefix_mismatch_passes_current_prompt_key() {
        let mut app = TestApp::builder().build();

        app.press(KeyCode::Esc);

        assert!(app.app.timers.pending_chord.is_some());

        app.press(KeyCode::Char('a'));

        assert!(app.app.timers.pending_chord.is_none());
        assert_eq!(app.state().prompt_text, "a");
        assert_eq!(app.prompt_cpos(), 1);
    }

    #[test]
    fn vim_insert_plain_less_than_inserts_despite_named_key_prefixes() {
        let mut app = TestApp::builder()
            .with_vim(true)
            .with_mode(AgentMode::parse("apply").unwrap())
            .build();

        app.press(KeyCode::Char('<'));

        assert!(app.app.timers.pending_chord.is_none());
        assert_eq!(app.state().prompt_text, "<");
        assert_eq!(app.prompt_cpos(), 1);
    }

    #[test]
    fn transient_escape_prefix_expires() {
        let mut app = TestApp::builder().build();
        assert!(app.run_lua(
            r#"
                smelt.keymap.set("", "<Esc><Esc>", function()
                    _G.esc_esc_hit = (_G.esc_esc_hit or 0) + 1
                end)
            "#
        ));

        app.press(KeyCode::Esc);
        assert!(app.app.timers.pending_chord.is_some());

        app.feed_one(crate::app::test_harness::SourceEvent::Tick(
            crate::app::ESC_CHORD_TIMEOUT_MS + 1,
        ));
        app.app.publish_diff_signals();

        assert!(app.app.timers.pending_chord.is_none());
        assert_eq!(
            app.app
                .core
                .signals
                .get::<String>("keymap_pending")
                .as_deref(),
            Some("")
        );

        app.press(KeyCode::Esc);

        assert_eq!(app.lua_int_global("esc_esc_hit"), None);
        assert!(app.app.timers.pending_chord.is_some());
    }

    #[test]
    fn global_mode_toggle_wins_before_prompt_placeholder_accept_key() {
        let mut app = TestApp::builder().build();
        app.install_prompt_placeholder(
            "draft".to_string(),
            vec![crate::smelt_edit::KeyBind {
                code: KeyCode::BackTab,
                mods: KeyModifiers::NONE,
            }],
            Vec::new(),
        );
        assert_eq!(app.app.core.config.mode, AgentMode::normal());

        app.press(KeyCode::BackTab);

        assert_eq!(app.app.core.config.mode, AgentMode::parse("plan").unwrap());
        assert_eq!(
            app.state().prompt_text,
            "",
            "prompt placeholder must not accept a globally-handled chord"
        );
    }

    #[test]
    fn modal_overlay_swallows_cmdline_open_key() {
        let mut app = TestApp::builder().build();
        let buf = app
            .app
            .ui
            .buf_create(crate::smelt_edit::BufCreateOpts::default());
        let win = app
            .app
            .ui
            .win_open_split(
                buf,
                crate::smelt_edit::SplitConfig {
                    region: "event-test-overlay".into(),
                    gutters: crate::smelt_edit::Gutters::default(),
                },
            )
            .expect("overlay test window opens");
        app.app.ui.overlay_open(
            crate::smelt_edit::Overlay::new(
                crate::smelt_edit::LayoutTree::leaf(win),
                crate::smelt_edit::layout::Anchor::ScreenCenter,
            )
            .modal(true),
        );
        assert!(app.state().active_modal.is_some());

        app.press(KeyCode::Char(':'));

        let state = app.state();
        assert!(state.active_modal.is_some());
        assert!(
            !state.cmdline_open,
            "':' must stay inside overlay routing while a modal overlay is active"
        );
        assert_eq!(state.prompt_text, "");
    }

    #[test]
    fn busy_submit_queues_without_starting_agent_turn() {
        let mut app = TestApp::builder().build();
        app.app.busy_stack.push("background".to_string());
        app.type_text("run later");

        app.press(KeyCode::Enter);

        let state = app.state();
        assert!(
            !state.agent_running,
            "busy-only submit should not start an agent turn immediately"
        );
        assert_eq!(state.queued_inputs, vec!["run later".to_string()]);
        assert_eq!(state.prompt_text, "");
    }
}
