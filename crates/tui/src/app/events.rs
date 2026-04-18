use super::*;

use crate::keymap::{self, KeyAction};
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture, Event, MouseEvent, MouseEventKind},
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use std::time::{Duration, Instant};

/// Coalesce-window for repeated `Action::PurgeRedraw` (Ctrl+L) presses.
/// A second press inside this window is dropped — the first press has
/// already done a full purge+repaint and the screen state hasn't had
/// time to drift.
const PURGE_REDRAW_DEBOUNCE: Duration = Duration::from_millis(10);

impl App {
    /// Run a Ctrl+L purge+redraw, suppressing repeats inside the
    /// debounce window so a held key or rapid double-press only fires
    /// the expensive full-screen repaint once.
    pub(super) fn purge_redraw_debounced(&mut self) {
        let now = Instant::now();
        if let Some(last) = self.last_purge_redraw {
            if now.duration_since(last) < PURGE_REDRAW_DEBOUNCE {
                return;
            }
        }
        self.last_purge_redraw = Some(now);
        self.screen.redraw();
    }

    fn apply_settings_result(&mut self, s: &crate::input::SettingsState) {
        self.set_settings(s.clone());
    }

    // ── Terminal event dispatch ───────────────────────────────────────────

    /// Handle a single terminal event, potentially starting/stopping agents.
    /// Returns `true` if the app should quit.
    pub(super) fn dispatch_terminal_event(
        &mut self,
        ev: Event,
        agent: &mut Option<TurnState>,
        t: &mut Timers,
        active_dialog: &mut Option<Box<dyn render::Dialog>>,
    ) -> bool {
        if matches!(ev, Event::FocusGained | Event::FocusLost) {
            self.screen.set_focused(matches!(ev, Event::FocusGained));
            return false;
        }

        // Route events to the active dialog if one is showing.
        if active_dialog.is_some() {
            // Terminal resize: full clear + redraw screen + redraw dialog.
            if let Event::Resize(w, h) = ev {
                self.handle_resize(w, h);
                active_dialog.as_mut().unwrap().handle_resize();
                return false;
            }
            // BackTab (shift-tab): toggle mode. If the new mode auto-allows
            // the pending tool call, accept the dialog automatically.
            if matches!(
                ev,
                Event::Key(KeyEvent {
                    code: KeyCode::BackTab,
                    ..
                })
            ) {
                self.toggle_mode();
                if let Some(ctx) = self.confirm_context.take() {
                    if self
                        .permissions
                        .decide(self.mode, &ctx.tool_name, &ctx.args, false)
                        == Decision::Allow
                    {
                        active_dialog.take();
                        self.finalize_dialog_close();
                        self.screen
                            .set_active_status(&ctx.call_id, ToolStatus::Pending);
                        self.send_permission_decision(ctx.request_id, true, None);
                    } else {
                        // Mode changed but still needs confirmation — keep dialog open.
                        self.confirm_context = Some(ctx);
                    }
                }
                return false;
            }
            if let Event::Key(KeyEvent {
                code, modifiers, ..
            }) = ev
            {
                // Ctrl+L: full redraw (same as outside a dialog).
                if code == KeyCode::Char('l') && modifiers.contains(KeyModifiers::CONTROL) {
                    self.purge_redraw_debounced();
                    active_dialog.as_mut().unwrap().mark_dirty();
                    return false;
                }
                let mut d = active_dialog.take().unwrap();
                if let Some(result) = d.handle_key(code, modifiers) {
                    // Sync kill ring back from dialog.
                    if let Some(kr) = d.kill_ring() {
                        self.input.set_kill_ring(kr.to_string());
                    }
                    self.handle_dialog_result(result, agent);
                    self.input.restore_stash();
                } else {
                    *active_dialog = Some(d);
                }
            }
            return false;
        }

        // Ctrl+C while exec is running → kill it.
        if self.exec_kill.is_some()
            && matches!(
                ev,
                Event::Key(KeyEvent {
                    code: KeyCode::Char('c'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                })
            )
        {
            if let Some(kill) = self.exec_kill.take() {
                kill.notify_one();
            }
            return false;
        }

        let outcome = if agent.is_some() {
            self.handle_event_running(ev, t)
        } else {
            self.handle_event_idle(ev, t)
        };

        match outcome {
            EventOutcome::Noop | EventOutcome::Redraw => false,
            EventOutcome::Quit => {
                if agent.is_some() {
                    self.finish_turn(true);
                    *agent = None;
                }
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
                if agent.is_some() {
                    self.finish_turn(true);
                    *agent = None;
                }
                false
            }
            EventOutcome::InterruptWithQueued => {
                // Cancel the running turn and let the queued messages
                // auto-start via the main loop once TurnComplete arrives.
                // We must save the queued messages before finish_turn
                // because the cancel path dumps them into the input buffer.
                let remaining = std::mem::take(&mut self.queued_messages);
                if agent.is_some() {
                    self.finish_turn(true);
                    *agent = None;
                }
                self.queued_messages = remaining;
                false
            }
            EventOutcome::CancelAndClear => {
                engine::log::entry(
                    engine::log::Level::Info,
                    "agent_stop",
                    &serde_json::json!({
                        "reason": "user_cancel_and_clear",
                    }),
                );
                self.reset_session();
                *agent = None;
                false
            }
            EventOutcome::MenuResult(result) => {
                match result {
                    MenuResult::Settings(ref s) => {
                        self.apply_settings_result(s);
                        let items = crate::completer::Completer::settings_items(s);
                        if let Some(comp) = self.input.completer.as_mut() {
                            if comp.kind == crate::completer::CompleterKind::Settings {
                                comp.refresh_items(items);
                            }
                        }
                    }
                    MenuResult::ModelSelect(ref key) => {
                        self.apply_model(key);
                        self.screen.erase_prompt();
                    }
                    MenuResult::ThemeSelect(value) => {
                        // Live preview already set the in-memory accent;
                        // mirror it explicitly so settings.json and the
                        // atomic stay in lockstep regardless of preview
                        // state.
                        self.apply_accent(value);
                    }
                    MenuResult::ColorSelect(_) => {
                        self.screen.redraw();
                    }
                    MenuResult::Stats | MenuResult::Cost | MenuResult::Dismissed => {}
                }
                let is_settings = matches!(&result, MenuResult::Settings(_));
                if !is_settings {
                    self.input.restore_stash();
                }
                self.screen.mark_dirty();
                false
            }
            EventOutcome::OpenDialog(dlg) => {
                self.open_dialog(dlg, active_dialog);
                false
            }
            EventOutcome::Exec(rx, kill) => {
                self.screen.erase_prompt();
                self.exec_rx = Some(rx);
                self.exec_kill = Some(kill);
                false
            }
            EventOutcome::Submit {
                mut content,
                mut display,
            } => {
                // Ingress: scrub secrets from user submissions before they
                // reach the screen, the queue, or the engine.
                self.redact_user_submission(&mut content, &mut display);
                // Queue messages while compaction is in progress so they
                // are sent against the compacted history, not the old one.
                if self.is_compacting() {
                    let text = content.text_content();
                    if !text.is_empty() {
                        self.queued_messages.push(text);
                        self.screen.erase_prompt();
                        self.screen.mark_dirty();
                    }
                } else if self.try_btw_submit(&content, &display) {
                    // handled
                } else {
                    let text = content.text_content();
                    let has_images = content.image_count() > 0;
                    if !text.is_empty() || has_images {
                        let outcome = if has_images && text.trim().is_empty() {
                            InputOutcome::StartAgent
                        } else {
                            self.process_input(&text)
                        };
                        if self.apply_input_outcome(
                            outcome,
                            content,
                            &display,
                            agent,
                            active_dialog,
                        ) {
                            return true;
                        }
                    } else if !self.queued_messages.is_empty() {
                        // Empty submit with queued messages: pop and send the
                        // oldest one immediately.
                        let queued = self.queued_messages.remove(0);
                        if let Some(cmd) =
                            crate::custom_commands::resolve(queued.trim(), self.multi_agent)
                        {
                            self.screen.erase_prompt();
                            *agent = Some(self.begin_custom_command_turn(cmd));
                        } else {
                            let outcome = self.process_input(&queued);
                            let content = Content::text(queued.clone());
                            if self.apply_input_outcome(
                                outcome,
                                content,
                                &queued,
                                agent,
                                active_dialog,
                            ) {
                                return true;
                            }
                        }
                    }
                }
                // Restore stash unless a modal/dialog was opened (it will restore on close).
                if !self.input.has_modal() && active_dialog.is_none() {
                    self.input.restore_stash();
                }
                false
            }
        }
    }

    // ── Idle event handler ───────────────────────────────────────────────

    fn handle_event_idle(&mut self, ev: Event, t: &mut Timers) -> EventOutcome {
        if matches!(ev, Event::Paste(_)) {
            self.input_prediction = None;
        }
        if let Event::Resize(w, h) = ev {
            self.handle_resize(w, h);
            return EventOutcome::Noop;
        }

        // ── Mouse events (wheel + click → pane focus) ─────────────────────
        if let Event::Mouse(me) = ev {
            return self.handle_mouse(me);
        }

        // ── Ctrl-W pane chord ─────────────────────────────────────────────
        // First press primes the chord; the next keypress within
        // `PANE_CHORD_WINDOW` is consumed as the navigation command.
        if let Some(outcome) = self.handle_pane_chord(&ev, t) {
            return outcome;
        }

        // ── App NORMAL (History focus): intercept keys before anything else.
        // Dialogs/completers still take precedence — handled inside.
        if self.app_focus == crate::app::AppFocus::Content && !self.input.has_modal() {
            return self.handle_event_app_history(ev);
        }

        if let Some(outcome) = self.handle_overlay_keys(&ev) {
            return outcome;
        }

        // Esc / double-Esc (skip when a modal menu is open — let it handle Esc)
        if !self.input.has_modal()
            && matches!(
                ev,
                Event::Key(KeyEvent {
                    code: KeyCode::Esc,
                    ..
                })
            )
        {
            let in_normal = !self.input.vim_enabled() || !self.input.vim_in_insert_mode();
            if in_normal {
                let double = t
                    .last_esc
                    .is_some_and(|prev| prev.elapsed() < Duration::from_millis(500));
                if double {
                    t.last_esc = None;
                    let restore_mode = t.esc_vim_mode.take();

                    // Cancel in-flight compaction on double-Esc.
                    if self.screen.working_throbber() == Some(render::Throbber::Compacting) {
                        self.compact_epoch += 1;
                        self.screen.set_throbber(render::Throbber::Interrupted);
                        self.screen.notify("compaction cancelled".into());
                        if restore_mode == Some(vim::ViMode::Insert) {
                            self.input.set_vim_mode(vim::ViMode::Insert);
                        }
                        return EventOutcome::Noop;
                    }
                    return EventOutcome::Noop;
                }
                // Single Esc in normal mode — start timer.
                t.last_esc = Some(Instant::now());
                t.esc_vim_mode = self.input.vim_mode();
                if !self.input.vim_enabled() {
                    return EventOutcome::Noop;
                }
                // Vim normal mode — fall through to handle_event (resets pending op).
            } else {
                // Vim insert mode — start double-Esc timer, fall through so
                // handle_event processes the Esc and switches vim to normal.
                t.esc_vim_mode = Some(vim::ViMode::Insert);
                t.last_esc = Some(Instant::now());
            }
        } else {
            t.last_esc = None;
        }

        // Keymap lookup for app-level actions (before delegating to InputState).
        if let Event::Key(KeyEvent {
            code, modifiers, ..
        }) = ev
        {
            let ghost = self.input_prediction.is_some() && self.input.buf.is_empty();
            let ctx = self.input.key_context(false, ghost);

            // Dismiss ghost text on keys that affect input content.
            // Transparent actions (mode toggles, redraw, etc.) preserve it.
            if ghost {
                match keymap::lookup(code, modifiers, &ctx) {
                    Some(KeyAction::AcceptGhostText) => {
                        let full = self.input_prediction.take().unwrap();
                        let line = full.lines().next().unwrap_or(&full).to_string();
                        crate::api::buf::replace(&mut self.input, line, None);
                        self.screen.mark_dirty();
                        return EventOutcome::Redraw;
                    }
                    Some(
                        KeyAction::ToggleMode
                        | KeyAction::CycleReasoning
                        | KeyAction::PurgeRedraw
                        | KeyAction::ToggleStash,
                    ) => {}
                    _ => {
                        self.input_prediction = None;
                    }
                }
            }

            if !self.input.has_modal() {
                if let Some(action) = keymap::lookup(code, modifiers, &ctx) {
                    // Handle actions that need app-level context.
                    match action {
                        KeyAction::Quit => {
                            return EventOutcome::Quit;
                        }
                        KeyAction::ClearBuffer => {
                            // Dismiss menu/completer first, then clear buffer.
                            if let Some(result) = self.input.dismiss_menu() {
                                self.screen.mark_dirty();
                                return EventOutcome::MenuResult(result);
                            }
                            if self.input.completer.is_some() {
                                self.input.completer = None;
                                self.screen.mark_dirty();
                                return EventOutcome::Redraw;
                            }
                            t.last_ctrlc = Some(Instant::now());
                            self.input.clear();
                            self.screen.mark_dirty();
                            return EventOutcome::Redraw;
                        }
                        KeyAction::OpenHelp => {
                            return EventOutcome::OpenDialog(Box::new(render::HelpDialog::new(
                                self.input.vim_enabled(),
                            )));
                        }
                        KeyAction::OpenHistorySearch => {
                            if self.input.history_search_query().is_none() {
                                self.input.open_history_search(&self.input_history);
                                self.screen.mark_dirty();
                            }
                            return EventOutcome::Redraw;
                        }
                        _ => {
                            // Delegate to InputState for editing/navigation actions.
                        }
                    }
                }
            }
        }

        // Delegate to InputState::handle_event (menu, completer, vim, editing).
        let action = self.input.handle_event(ev, Some(&mut self.input_history));
        self.dispatch_input_action(action)
    }

    // ── Running event handler ────────────────────────────────────────────

    fn handle_event_running(&mut self, ev: Event, t: &mut Timers) -> EventOutcome {
        if matches!(ev, Event::Paste(_)) {
            self.input_prediction = None;
        }
        if let Event::Resize(w, h) = ev {
            self.handle_resize(w, h);
            return EventOutcome::Noop;
        }

        if let Some(outcome) = self.handle_overlay_keys(&ev) {
            return outcome;
        }

        // Track last keypress for deferring permission dialogs.
        if matches!(ev, Event::Key(_)) {
            t.last_keypress = Some(Instant::now());
        }

        // Keymap lookup for Ctrl+C (agent-running variant).
        if let Event::Key(KeyEvent {
            code, modifiers, ..
        }) = ev
        {
            let ctx = self.input.key_context(true, false);
            if let Some(action) = keymap::lookup(code, modifiers, &ctx) {
                match action {
                    KeyAction::CancelAgent => {
                        // Dismiss menu/completer first, then cancel.
                        if let Some(result) = self.input.dismiss_menu() {
                            self.screen.mark_dirty();
                            return EventOutcome::MenuResult(result);
                        }
                        if self.input.completer.is_some() {
                            self.input.completer = None;
                            self.screen.mark_dirty();
                            return EventOutcome::Noop;
                        }
                        self.queued_messages.clear();
                        self.screen.mark_dirty();
                        return EventOutcome::CancelAgent;
                    }
                    KeyAction::ClearBuffer => {
                        // Dismiss menu/completer first, then clear.
                        if let Some(result) = self.input.dismiss_menu() {
                            self.screen.mark_dirty();
                            return EventOutcome::MenuResult(result);
                        }
                        if self.input.completer.is_some() {
                            self.input.completer = None;
                            self.screen.mark_dirty();
                            return EventOutcome::Noop;
                        }
                        t.last_ctrlc = Some(Instant::now());
                        self.input.clear();
                        self.queued_messages.clear();
                        self.screen.mark_dirty();
                        return EventOutcome::Noop;
                    }
                    _ => {
                        // Other keymap actions — continue to Esc / input handling.
                    }
                }
            }
        }

        // Esc: dismiss any open picker completer first, then run agent-mode logic.
        if matches!(
            ev,
            Event::Key(KeyEvent {
                code: KeyCode::Esc,
                ..
            })
        ) {
            if self.input.has_modal() {
                let action = self.input.handle_event(ev, None);
                self.screen.mark_dirty();
                return self.dispatch_input_action(action);
            }
            match resolve_agent_esc(
                self.input.vim_mode(),
                !self.queued_messages.is_empty(),
                &mut t.last_esc,
                &mut t.esc_vim_mode,
            ) {
                EscAction::VimToNormal => {
                    self.input.handle_event(ev, None);
                    self.screen.mark_dirty();
                }
                EscAction::Unqueue => {
                    let mut combined = self.queued_messages.join("\n");
                    if !self.input.buf.is_empty() {
                        combined.push('\n');
                        combined.push_str(&self.input.buf);
                    }
                    crate::api::buf::replace(&mut self.input, combined, None);
                    self.queued_messages.clear();
                    self.screen.mark_dirty();
                }
                EscAction::Cancel { restore_vim } => {
                    if let Some(mode) = restore_vim {
                        self.input.set_vim_mode(mode);
                    }
                    self.screen.mark_dirty();
                    return EventOutcome::CancelAgent;
                }
                EscAction::StartTimer => {}
            }
            return EventOutcome::Noop;
        }

        // Everything else → InputState::handle_event (type-ahead with history).
        match self.input.handle_event(ev, Some(&mut self.input_history)) {
            Action::Submit {
                mut content,
                mut display,
            } => {
                // Ingress: scrub secrets before queueing or running commands.
                self.redact_user_submission(&mut content, &mut display);
                if self.try_btw_submit(&content, &display) {
                    self.screen.mark_dirty();
                    return EventOutcome::Noop;
                }
                let text = content.text_content();
                if let Some(outcome) = self.try_command_while_running(text.trim()) {
                    return outcome;
                }
                if !text.is_empty() {
                    self.queued_messages.push(text);
                }
                self.screen.mark_dirty();
            }
            Action::SubmitEmpty => {
                if !self.queued_messages.is_empty() {
                    return EventOutcome::InterruptWithQueued;
                }
            }
            Action::ToggleMode => {
                self.toggle_mode();
            }
            Action::Redraw => {
                self.screen.mark_dirty();
            }
            Action::CycleReasoning => {
                self.cycle_reasoning();
            }
            Action::EditInEditor => {
                self.edit_in_editor();
                self.screen.redraw();
            }
            Action::CenterScroll => {
                self.screen.center_input_scroll();
            }
            Action::NotifyError(msg) => {
                self.screen.notify_error(msg);
                self.screen.mark_dirty();
            }
            Action::PurgeRedraw => {
                self.purge_redraw_debounced();
            }
            Action::MenuResult(result) => return EventOutcome::MenuResult(result),
            Action::Noop | Action::Resize { .. } => {}
        }
        EventOutcome::Noop
    }

    // ── Shared helpers ────────────────────────────────────────────────────

    /// Map an `input::Action` into an `EventOutcome`.
    fn dispatch_input_action(&mut self, action: Action) -> EventOutcome {
        match action {
            Action::Submit { content, display } => EventOutcome::Submit { content, display },
            Action::SubmitEmpty => EventOutcome::Noop,
            Action::MenuResult(result) => EventOutcome::MenuResult(result),
            Action::ToggleMode => {
                self.toggle_mode();
                EventOutcome::Redraw
            }
            Action::CycleReasoning => {
                self.cycle_reasoning();
                EventOutcome::Redraw
            }
            Action::EditInEditor => {
                self.edit_in_editor();
                self.screen.redraw();
                EventOutcome::Noop
            }
            Action::CenterScroll => {
                self.screen.center_input_scroll();
                EventOutcome::Noop
            }
            Action::Resize {
                width: w,
                height: h,
            } => {
                self.handle_resize(w as u16, h as u16);
                EventOutcome::Noop
            }
            Action::Redraw => {
                self.screen.mark_dirty();
                EventOutcome::Redraw
            }
            Action::PurgeRedraw => {
                self.purge_redraw_debounced();
                EventOutcome::Noop
            }
            Action::NotifyError(msg) => {
                self.screen.notify_error(msg);
                EventOutcome::Redraw
            }
            Action::Noop => EventOutcome::Noop,
        }
    }

    fn edit_in_editor(&mut self) {
        let editor = std::env::var("VISUAL")
            .or_else(|_| std::env::var("EDITOR"))
            .unwrap_or_else(|_| "vi".into());

        let tmp = match tempfile::Builder::new().suffix(".md").tempfile() {
            Ok(f) => f,
            Err(e) => {
                self.screen.notify_error(format!("tmpfile: {e}"));
                return;
            }
        };
        if let Err(e) = std::fs::write(tmp.path(), &self.input.buf) {
            self.screen.notify_error(format!("write tmp: {e}"));
            return;
        }

        // Suspend TUI so the editor gets a normal terminal.
        let _ = io::stdout().execute(DisableMouseCapture);
        let _ = io::stdout().execute(LeaveAlternateScreen);
        terminal::disable_raw_mode().ok();

        let status = std::process::Command::new(&editor).arg(tmp.path()).status();

        // Resume TUI.
        terminal::enable_raw_mode().ok();
        let _ = io::stdout().execute(EnterAlternateScreen);
        let _ = io::stdout().execute(EnableMouseCapture);

        match status {
            Ok(s) if s.success() => match std::fs::read_to_string(tmp.path()) {
                Ok(new) => {
                    crate::api::buf::replace(&mut self.input, new, None);
                }
                Err(e) => self.screen.notify_error(format!("read tmp: {e}")),
            },
            Ok(s) => {
                self.screen
                    .notify_error(format!("{editor} exited with {s}"));
            }
            Err(e) => {
                self.screen.notify_error(format!("{editor}: {e}"));
            }
        }
    }

    fn handle_resize(&mut self, w: u16, h: u16) {
        if w == self.last_width && h == self.last_height {
            return;
        }
        let width_changed = w != self.last_width;
        self.last_width = w;
        self.last_height = h;
        if width_changed {
            self.screen.redraw();
        } else {
            // Height-only: block layouts are still valid at this width.
            // Soft redraw skips Clear::Purge and reuses cached layouts.
            self.screen.redraw();
        }
    }

    /// Handle overlay keys (notification dismiss + btw scroll/dismiss).
    /// Returns `Some(EventOutcome)` if the event was consumed.
    fn handle_overlay_keys(&mut self, ev: &Event) -> Option<EventOutcome> {
        if matches!(ev, Event::Key(_)) && self.screen.has_notification() {
            self.screen.dismiss_notification();
        }

        if self.screen.has_btw() {
            if let Event::Key(KeyEvent {
                code, modifiers, ..
            }) = ev
            {
                use crate::keymap::{nav_lookup, NavAction};
                match nav_lookup(*code, *modifiers) {
                    Some(NavAction::Down) => {
                        self.screen.btw_scroll(1);
                        return Some(EventOutcome::Noop);
                    }
                    Some(NavAction::Up) => {
                        self.screen.btw_scroll(-1);
                        return Some(EventOutcome::Noop);
                    }
                    Some(NavAction::PageDown) => {
                        let half = (render::term_height() / 2).max(1) as isize;
                        self.screen.btw_scroll(half);
                        return Some(EventOutcome::Noop);
                    }
                    Some(NavAction::PageUp) => {
                        let half = (render::term_height() / 2).max(1) as isize;
                        self.screen.btw_scroll(-half);
                        return Some(EventOutcome::Noop);
                    }
                    Some(NavAction::Dismiss) => {
                        self.screen.dismiss_btw();
                        return Some(EventOutcome::Noop);
                    }
                    _ => {
                        // Let transparent actions (mode toggles, redraw)
                        // pass through without dismissing.
                        let ctx = self.input.key_context(false, false);
                        match keymap::lookup(*code, *modifiers, &ctx) {
                            Some(
                                KeyAction::ToggleMode
                                | KeyAction::CycleReasoning
                                | KeyAction::PurgeRedraw
                                | KeyAction::ToggleStash,
                            ) => return None,
                            _ => {
                                self.screen.dismiss_btw();
                                return Some(EventOutcome::Noop);
                            }
                        }
                    }
                }
            }
        }

        None
    }

    /// Try to handle a submitted input as a `/btw` command.
    /// Returns `true` if it was handled.
    fn try_btw_submit(&mut self, content: &Content, display: &str) -> bool {
        let text = content.text_content();
        let trimmed = text.trim();
        if !trimmed.starts_with("/btw ") {
            return false;
        }
        let question_full = trimmed[5..].trim().to_string();
        if question_full.is_empty() {
            return true; // handled (as no-op)
        }
        let display_q = display
            .strip_prefix("/btw ")
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| question_full.clone());
        let labels = content.image_labels();
        self.input_history.push(text);
        self.start_btw(question_full, display_q, labels);
        true
    }

    // ── Input processing (commands, settings, rewind, shell) ─────────────

    pub(super) fn process_input(&mut self, input: &str) -> InputOutcome {
        if input.is_empty() {
            return InputOutcome::Continue;
        }

        let trimmed = input.trim();
        self.input_history.push(input.to_string());

        // Skip shell escape for pasted content
        let is_from_paste = self.input.skip_shell_escape();
        match self.handle_command(trimmed) {
            CommandAction::Quit => return InputOutcome::Quit,
            CommandAction::CancelAndClear => {
                return InputOutcome::CancelAndClear;
            }
            CommandAction::Compact { instructions } => {
                return InputOutcome::Compact { instructions }
            }
            CommandAction::OpenDialog(dlg) => return InputOutcome::OpenDialog(dlg),
            CommandAction::Exec(rx, kill) => return InputOutcome::Exec(rx, kill),
            CommandAction::Continue => {}
        }
        if trimmed.starts_with('/') {
            if let Some(cmd) = crate::custom_commands::resolve(trimmed, self.multi_agent) {
                return InputOutcome::CustomCommand(Box::new(cmd));
            }
            if crate::completer::Completer::is_command(trimmed) {
                return InputOutcome::Continue;
            }
        }
        // Skip starting agent for shell escapes, but NOT for pasted content
        if trimmed.starts_with('!') && !is_from_paste {
            return InputOutcome::Continue;
        }

        // Regular user message → start agent
        InputOutcome::StartAgent
    }

    // ── Tick ─────────────────────────────────────────────────────────────

    /// Render a dialog-mode frame into the provided output buffer.
    /// Returns `(redirtied, placement)` — the bool indicates whether
    /// content was drawn (caller should re-dirty the dialog), and the
    /// placement carries the row budget for the dialog.
    pub(super) fn tick_dialog(
        &mut self,
        out: &mut render::RenderOut,
        dialog_height: u16,
        constrain: bool,
    ) -> (bool, Option<render::DialogPlacement>) {
        let _perf = crate::perf::begin("app:tick");
        let w = render::term_width();
        self.screen.set_dialog_open(true);
        self.screen.set_constrain_dialog(constrain);
        self.screen.draw_frame(out, w, None, Some(dialog_height))
    }

    /// Render a full-mode frame (content + prompt) in its own sync frame.
    pub(super) fn tick_prompt(&mut self, agent_running: bool) {
        let _perf = crate::perf::begin("app:tick");
        self.screen.update_spinner();
        if !self.screen.needs_draw(false) {
            return;
        }
        let w = render::term_width();
        let show_queued = agent_running || self.is_compacting();
        self.screen.set_dialog_open(false);

        let visual = self.content_visual_range(w);
        // If the viewport is pinned (active selection / drag), adjust
        // `scroll_offset` against the current transcript row count so
        // new rows appearing at the bottom push into scrollback
        // instead of shifting the visible slice. Compute the dims
        // before the `&self` borrows below.
        if self.content_pane.is_pinned() {
            let (total, viewport) = self.transcript_dims();
            self.content_pane.apply_pin(total, viewport);
        }
        // Status bar shows the *focused* window's vim mode. Without
        // this, the status bar caches the prompt's mode even when the
        // transcript window has focus.
        let (status_vim_enabled, status_vim_mode) = match self.app_focus {
            crate::app::AppFocus::Content => (
                self.content_pane.vim.is_some(),
                self.content_pane.vim.as_ref().map(|v| v.mode()),
            ),
            crate::app::AppFocus::Prompt => (self.input.vim_enabled(), self.input.vim_mode()),
        };
        self.screen
            .set_status_vim(status_vim_enabled, status_vim_mode);
        let (queued, prediction): (&[String], Option<&str>) = if show_queued {
            (&self.queued_messages, None)
        } else {
            (&[], self.input_prediction.as_deref())
        };
        let mut frame = render::Frame::begin(self.screen.backend());
        let (clamped_scroll, clamped_line, clamped_col) = self.screen.draw_viewport_frame(
            &mut frame,
            w,
            FramePrompt {
                state: &self.input,
                mode: self.mode,
                queued,
                prediction,
            },
            self.content_pane.scroll_offset,
            self.content_pane.cursor_line,
            self.content_pane.cursor_col,
            visual,
        );
        self.content_pane.scroll_offset = clamped_scroll;
        self.content_pane.cursor_line = clamped_line;
        self.content_pane.cursor_col = clamped_col;
    }

    // ── Content pane key handler — drives `Vim` over a readonly
    // transcript buffer so Normal / Visual / VisualLine motions and
    // yank behave exactly like they do in the prompt.
    fn handle_event_app_history(&mut self, ev: Event) -> EventOutcome {
        let Event::Key(k) = ev else {
            return EventOutcome::Noop;
        };
        use crossterm::event::KeyModifiers as M;

        // Ctrl-C from a non-prompt pane returns focus to the prompt.
        if k.modifiers.contains(M::CONTROL) && matches!(k.code, KeyCode::Char('c')) {
            self.app_focus = crate::app::AppFocus::Prompt;
            self.screen.mark_dirty();
            return EventOutcome::Redraw;
        }

        // Readonly-buffer scrolling keybinds: Ctrl-U / Ctrl-D (half-page),
        // Ctrl-B / Ctrl-F (full-page), Ctrl-Y / Ctrl-E (one line). These
        // mirror Vim's scroll commands. Since Vim in the prompt reuses
        // InputState for these, we implement them here by driving the
        // content cursor directly — which in turn pulls the viewport via
        // the normal scroll-follows-cursor logic.
        if k.modifiers.contains(M::CONTROL) {
            let half = (self.viewport_rows_estimate() / 2).max(1) as isize;
            let full = (self.viewport_rows_estimate() as isize).max(1);
            let delta: Option<isize> = match k.code {
                KeyCode::Char('u') => Some(-half),
                KeyCode::Char('d') => Some(half),
                KeyCode::Char('b') => Some(-full),
                KeyCode::Char('f') => Some(full),
                KeyCode::Char('y') => Some(-1),
                KeyCode::Char('e') => Some(1),
                _ => None,
            };
            if let Some(dn) = delta {
                self.move_content_cursor_by_lines(dn);
                return EventOutcome::Redraw;
            }
        }

        // Route the key through vim. If it's consumed, sync
        // scroll/cursor state from the new cpos and return.
        if self.handle_content_vim_key(k) {
            return EventOutcome::Redraw;
        }
        match (k.code, k.modifiers) {
            (KeyCode::Char('q'), M::NONE) => EventOutcome::Quit,
            _ => EventOutcome::Noop,
        }
    }

    /// Move the content-pane cursor by `delta` lines. Delegates to
    /// `ContentPane::scroll_by_lines`, which reuses vim `j`/`k` so
    /// vertical motion shares one code path (with `curswant`) across
    /// mouse wheel, Ctrl-U/D, arrows and j/k.
    fn move_content_cursor_by_lines(&mut self, delta: isize) {
        let w = render::term_width();
        let rows = self.screen.full_transcript_text(w);
        let viewport = self.viewport_rows_estimate();
        self.content_pane.scroll_by_lines(delta, &rows, viewport);
        self.screen.mark_dirty();
    }

    /// Build the transcript buffer, run `key` through the content-pane
    /// `Vim` instance, and mirror the resulting cursor / visual / yank
    /// state back onto our scroll + cursor. Returns `true` when vim
    /// consumed the key (caller should return `Redraw`).
    fn handle_content_vim_key(&mut self, k: KeyEvent) -> bool {
        let w = render::term_width();
        let rows = self.screen.full_transcript_text(w);
        let viewport = self.viewport_rows_estimate();
        match self.content_pane.handle_key(k, &rows, viewport) {
            None => false,
            Some(yanked) => {
                if let Some(text) = yanked {
                    let _ = crate::app::commands::copy_to_clipboard(&text);
                    self.screen
                        .notify(format!("yanked {} chars", text.chars().count()));
                }
                // Live-streaming updates can't shift the transcript
                // under the user while a visual selection is active.
                self.sync_transcript_pin();
                self.screen.mark_dirty();
                true
            }
        }
    }

    /// Compute the content pane's visual selection range (if any) in
    /// absolute transcript coordinates for the renderer to highlight.
    fn content_visual_range(&mut self, width: usize) -> Option<render::ContentVisualRange> {
        if self.app_focus != crate::app::AppFocus::Content {
            return None;
        }
        let vim = self.content_pane.vim.as_ref()?;
        let kind = match vim.mode() {
            crate::vim::ViMode::Visual => render::ContentVisualKind::Char,
            crate::vim::ViMode::VisualLine => render::ContentVisualKind::Line,
            _ => return None,
        };
        let rows = self.screen.full_transcript_text(width);
        if rows.is_empty() {
            return None;
        }
        let buf = rows.join("\n");
        let (s, e) = vim.visual_range(&buf, self.content_pane.cpos)?;
        let offset_to_line_col = |off: usize| -> (usize, usize) {
            let off = crate::text_utils::snap(&buf, off);
            let prefix = &buf[..off];
            let line = prefix.bytes().filter(|&b| b == b'\n').count();
            let line_start = prefix.rfind('\n').map(|p| p + 1).unwrap_or(0);
            (line, crate::text_utils::byte_to_cell(&buf[line_start..off], off - line_start))
        };
        let (start_line_abs, start_col) = offset_to_line_col(s);
        let (end_line_abs, end_col) = offset_to_line_col(e);
        // Transcript-absolute line indices → viewport-relative so the
        // painter can walk `last_viewport_text` directly.
        let viewport = self.viewport_rows_estimate() as usize;
        let total = rows.len();
        let scroll = self.content_pane.scroll_offset as usize;
        let view_top = total.saturating_sub(viewport).saturating_sub(scroll);
        let to_view = |abs: usize| abs.saturating_sub(view_top);
        Some(render::ContentVisualRange {
            start_line: to_view(start_line_abs),
            start_col,
            end_line: to_view(end_line_abs),
            end_col,
            kind,
        })
    }

    /// Viewport rows available for the content pane. Uses the prompt's
    /// actual rendered height from the previous frame plus the 1-row
    /// gap, so multi-line prompts (and completion menus) don't cause
    /// the scroll math to overshoot.
    fn viewport_rows_estimate(&self) -> u16 {
        let prompt_rows = self.screen.prev_prompt_rows().max(1);
        let gap: u16 = 1;
        self.last_height.saturating_sub(prompt_rows + gap).max(1)
    }

    /// Single source of truth for whether the transcript viewport
    /// should be pinned. The viewport pins when the user has an
    /// active selection *or* is in the middle of a mouse drag — new
    /// agent output then flows into scrollback without shifting the
    /// rows the user is looking at. When the pin releases, scroll
    /// resumes its normal stuck-to-bottom behavior.
    fn sync_transcript_pin(&mut self) {
        let has_selection = self.content_pane.selection_range().is_some();
        let in_vim_visual = matches!(
            self.content_pane.vim.as_ref().map(|v| v.mode()),
            Some(crate::vim::ViMode::Visual | crate::vim::ViMode::VisualLine)
        );
        let want_pin = has_selection || in_vim_visual || self.mouse_drag_active;
        if want_pin {
            if !self.content_pane.is_pinned() {
                let (total, viewport) = self.transcript_dims();
                self.content_pane.pin(total, viewport);
            }
        } else {
            self.content_pane.unpin();
        }
    }

    /// Current (total_transcript_rows, viewport_rows) — needed by the
    /// pin math. Reads the width from the renderer and measures the
    /// transcript against it.
    fn transcript_dims(&mut self) -> (u16, u16) {
        let w = render::term_width();
        let total = self.screen.full_transcript_text(w).len() as u16;
        let viewport = self.viewport_rows_estimate();
        (total, viewport)
    }

    // ── Mouse event dispatch ─────────────────────────────────────────────
    fn handle_mouse(&mut self, me: MouseEvent) -> EventOutcome {
        use crossterm::event::MouseButton;
        // Drag + release drive tmux-style click-drag-copy. Works in
        // both the prompt and the content pane — each extends its own
        // buffer's selection anchor.
        match me.kind {
            MouseEventKind::Drag(MouseButton::Left) => {
                self.mouse_drag_active = true;
                match self.app_focus {
                    crate::app::AppFocus::Content => {
                        self.extend_content_selection_to(me.row, me.column);
                    }
                    crate::app::AppFocus::Prompt => {
                        self.extend_prompt_selection_to(me.row, me.column);
                    }
                }
                self.sync_transcript_pin();
                return EventOutcome::Redraw;
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let dragged = self.mouse_drag_active;
                match self.app_focus {
                    crate::app::AppFocus::Content => {
                        self.copy_content_selection_and_clear(dragged);
                    }
                    crate::app::AppFocus::Prompt => {
                        self.copy_prompt_selection_on_release();
                    }
                }
                self.mouse_drag_active = false;
                self.drag_autoscroll_since = None;
                self.sync_transcript_pin();
                return EventOutcome::Redraw;
            }
            _ => {}
        }

        match me.kind {
            MouseEventKind::ScrollUp => {
                self.scroll_under_mouse(me.row, -3);
                EventOutcome::Redraw
            }
            MouseEventKind::ScrollDown => {
                self.scroll_under_mouse(me.row, 3);
                EventOutcome::Redraw
            }
            MouseEventKind::Down(_) => {
                // Double-click detection: two primary-button Downs on
                // the same cell within 400ms → word-select + copy.
                let now = Instant::now();
                let double = self.last_click.is_some_and(|(t, r, c)| {
                    now.duration_since(t) < Duration::from_millis(400)
                        && r == me.row
                        && c == me.column
                });
                self.last_click = Some((now, me.row, me.column));

                // First, check if the click lands inside the input text
                // region — that's the only prompt-area hit we position
                // for. Clicks on queued messages, bars, status line etc.
                // only change focus.
                if let Some((top, rows, scroll, gutter, usable)) = self.screen.input_region() {
                    if me.row >= top && me.row < top + rows {
                        self.app_focus = crate::app::AppFocus::Prompt;
                        self.position_prompt_cursor_from_click(
                            me.row - top,
                            me.column,
                            scroll,
                            gutter,
                            usable,
                        );
                        if double {
                            self.select_and_copy_word_in_prompt();
                        }
                        return EventOutcome::Redraw;
                    }
                }

                let (_, height) = self.screen.size();
                let prompt_rows = self.screen.prev_prompt_rows();
                let prompt_top = height.saturating_sub(prompt_rows);
                if me.row >= prompt_top {
                    if self.app_focus != crate::app::AppFocus::Prompt {
                        self.app_focus = crate::app::AppFocus::Prompt;
                        self.screen.mark_dirty();
                        return EventOutcome::Redraw;
                    }
                    return EventOutcome::Noop;
                }
                // Content pane click: focus + move cursor + anchor a
                // potential drag-selection in vim Visual mode. If the
                // user just clicks without dragging, the subsequent Up
                // clears visual so nothing gets selected.
                self.app_focus = crate::app::AppFocus::Content;
                self.position_content_cursor_from_click(me.row, me.column);
                if double {
                    self.select_and_copy_word_in_content();
                    return EventOutcome::Redraw;
                }
                // Anchor the visual selection at the click position, not
                // wherever the cursor happened to be before — otherwise
                // a click selects everything between the previous
                // cursor and the click point.
                let anchor = self.content_pane.cpos;
                if let Some(vim) = self.content_pane.vim.as_mut() {
                    vim.begin_visual(crate::vim::ViMode::Visual, anchor);
                }
                EventOutcome::Redraw
            }
            _ => EventOutcome::Noop,
        }
    }

    /// Scroll the pane under the mouse cursor by `delta` lines (positive
    /// = down). Scrolling over the prompt drives vim j/k on the input
    /// buffer; scrolling anywhere else drives the content pane. This
    /// keeps wheel behaviour consistent with the "buffer scroll is
    /// cursor motion" model used by keyboard navigation.
    pub(super) fn scroll_under_mouse(&mut self, row: u16, delta: isize) {
        if let Some((top, rows, _, _, _)) = self.screen.input_region() {
            if row >= top && row < top + rows {
                self.app_focus = crate::app::AppFocus::Prompt;
                self.scroll_prompt_by_lines(delta);
                return;
            }
        }
        self.app_focus = crate::app::AppFocus::Content;
        self.move_content_cursor_by_lines(delta);
    }

    /// Move the prompt cursor by `delta` logical lines. Uses `Up`/`Down`
    /// keys — NOT `j`/`k` — because the prompt may be in vim insert
    /// mode (or vim disabled), where literal `j`/`k` would be typed
    /// into the buffer. Arrow keys are interpreted as cursor motion in
    /// every prompt mode.
    fn scroll_prompt_by_lines(&mut self, delta: isize) {
        let (code, count) = if delta >= 0 {
            (KeyCode::Down, delta as usize)
        } else {
            (KeyCode::Up, (-delta) as usize)
        };
        let k = KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::empty(),
        };
        for _ in 0..count {
            self.input.handle_event(Event::Key(k), None);
        }
        self.screen.mark_dirty();
    }

    /// Translate a click inside the prompt input region into a char
    /// offset in `state.buf` and move `state.cpos` there. `rel_row` is
    /// rows below the top of the input region; `col` is the screen
    /// column. Takes current wrap metrics from the last-drawn frame.
    fn position_prompt_cursor_from_click(
        &mut self,
        rel_row: u16,
        col: u16,
        scroll: usize,
        gutter: u16,
        usable: u16,
    ) {
        let target_visual_row = rel_row as usize + scroll;
        let target_col = col.saturating_sub(gutter) as usize;
        let usable = usable as usize;
        let buf = &self.input.buf;

        // Simple char-wrap walk that mirrors `wrap_and_locate_cursor`'s
        // behaviour for the common case of plain text input.
        let mut visual_row = 0usize;
        let mut col_in_line = 0usize;
        let mut target_byte: Option<usize> = None;
        let mut last_byte_on_target_row: Option<usize> = None;
        for (byte_off, ch) in buf.char_indices() {
            if visual_row == target_visual_row {
                last_byte_on_target_row = Some(byte_off);
                if col_in_line == target_col {
                    target_byte = Some(byte_off);
                    break;
                }
            }
            if ch == '\n' {
                if visual_row == target_visual_row && target_byte.is_none() {
                    target_byte = Some(byte_off);
                    break;
                }
                visual_row += 1;
                col_in_line = 0;
                continue;
            }
            col_in_line += 1;
            if col_in_line >= usable {
                if visual_row == target_visual_row && target_byte.is_none() {
                    // Past the end of target row without hitting target
                    // col — clamp to end of line.
                    target_byte = Some(byte_off + ch.len_utf8());
                    break;
                }
                visual_row += 1;
                col_in_line = 0;
            }
        }
        let cpos = target_byte
            .or_else(|| {
                last_byte_on_target_row
                    .map(|b| b + buf[b..].chars().next().map_or(0, |c| c.len_utf8()))
            })
            .unwrap_or(buf.len());
        self.input.cpos = cpos.min(buf.len());
        self.screen.mark_dirty();
    }

    /// Extend the content-pane visual selection to the cell under the
    /// current drag position. Runs while the user holds mouse-1 and
    /// moves — each update moves the cursor inside vim Visual mode so
    /// the existing visual range widens or shrinks accordingly. Auto-
    /// scroll when the cursor is parked at an edge is handled by
    /// [`tick_drag_autoscroll`] on the frame tick, so holding the mouse
    /// still at the edge keeps extending the selection.
    fn extend_content_selection_to(&mut self, row: u16, col: u16) {
        self.position_content_cursor_from_click(row, col);
    }

    /// Frame-tick hook: if the user is mid-drag with the content cursor
    /// on the top or bottom row of the viewport, scroll a single line
    /// so the selection widens past the visible area. One-line-per-tick
    /// avoids the choppy feel of multi-line jumps; the main loop ramps
    /// its sleep interval down the longer the cursor stays at the edge,
    /// which is how acceleration happens.
    pub(super) fn tick_drag_autoscroll(&mut self) {
        if !self.mouse_drag_active || self.app_focus != crate::app::AppFocus::Content {
            self.drag_autoscroll_since = None;
            return;
        }
        let viewport = self.viewport_rows_estimate();
        if viewport == 0 {
            self.drag_autoscroll_since = None;
            return;
        }
        // `cursor_line` is measured from the bottom of the viewport:
        // 0 = bottom row, viewport-1 = top row.
        let delta: isize = if self.content_pane.cursor_line >= viewport.saturating_sub(1) {
            -1
        } else if self.content_pane.cursor_line == 0 {
            1
        } else {
            self.drag_autoscroll_since = None;
            return;
        };
        self.drag_autoscroll_since
            .get_or_insert_with(std::time::Instant::now);
        self.move_content_cursor_by_lines(delta);
        self.sync_transcript_pin();
    }

    /// Extend the prompt's shift-selection anchor to the click position.
    /// On the first drag tick the anchor is set at the current cursor
    /// position (where the drag started); subsequent drags only move
    /// the cursor so the selection widens or shrinks.
    fn extend_prompt_selection_to(&mut self, row: u16, col: u16) {
        if self.input.selection_anchor.is_none() {
            self.input.selection_anchor = Some(self.input.cpos);
        }
        if let Some((top, rows, scroll, gutter, usable)) = self.screen.input_region() {
            if row >= top && row < top + rows {
                self.position_prompt_cursor_from_click(row - top, col, scroll, gutter, usable);
                return;
            }
        }
        self.screen.mark_dirty();
    }

    /// Finalise a prompt drag-select: copy any non-empty selection to
    /// the clipboard and clear the anchor. A bare click (no drag) has
    /// anchor == cpos, so this is a no-op in that case.
    fn copy_prompt_selection_on_release(&mut self) {
        if let Some((s, e)) = self.input.selection_range() {
            let text: String = self.input.buffer.buf[s..e].to_string();
            let chars = text.chars().count();
            let _ = crate::app::commands::copy_to_clipboard(&text);
            self.screen.notify(format!("copied {} chars", chars));
        }
        self.input.selection_anchor = None;
        self.screen.mark_dirty();
    }

    /// Double-click on the prompt: select the word under the cursor
    /// (if any) via the shared `Buffer::select_word_at` helper, and
    /// copy it to the clipboard.
    fn select_and_copy_word_in_prompt(&mut self) {
        let cpos = self.input.cpos;
        if let Some((s, e)) = self.input.select_word_at(cpos) {
            let text = self.input.buffer.buf[s..e].to_string();
            let chars = text.chars().count();
            let _ = crate::app::commands::copy_to_clipboard(&text);
            self.screen.notify(format!("copied {} chars", chars));
        }
        self.screen.mark_dirty();
    }

    /// Double-click on the content pane: enter vim Visual over the
    /// word under the cursor and copy it.
    fn select_and_copy_word_in_content(&mut self) {
        let cpos = self.content_pane.cpos;
        if let Some((s, e)) = self.content_pane.select_word_at(cpos) {
            let text = self.content_pane.buffer.buf[s..e].to_string();
            let chars = text.chars().count();
            let _ = crate::app::commands::copy_to_clipboard(&text);
            self.screen.notify(format!("copied {} chars", chars));
        }
        self.sync_transcript_pin();
        self.screen.mark_dirty();
    }

    /// Finalise a mouse interaction. Only copies when `dragged` is true —
    /// a bare click (no drag) exits Visual mode without copying, even
    /// though vim Visual selects the char under the cursor by default.
    fn copy_content_selection_and_clear(&mut self, dragged: bool) {
        let mut copied_len: Option<usize> = None;
        if dragged {
            let width = render::term_width();
            let rows = self.screen.full_transcript_text(width);
            let buf = rows.join("\n");
            if let Some(vim) = self.content_pane.vim.as_ref() {
                if let Some((s, e)) = vim.visual_range(&buf, self.content_pane.cpos) {
                    let s = crate::text_utils::snap(&buf, s);
                    let e = crate::text_utils::snap(&buf, e);
                    if s < e {
                        let selection = buf[s..e].to_string();
                        let chars = selection.chars().count();
                        let _ = crate::app::commands::copy_to_clipboard(&selection);
                        copied_len = Some(chars);
                    }
                }
            }
        }
        if let Some(vim) = self.content_pane.vim.as_mut() {
            vim.set_mode(crate::vim::ViMode::Normal);
        }
        if let Some(n) = copied_len {
            self.screen.notify(format!("copied {} chars", n));
        }
        self.sync_transcript_pin();
        self.screen.mark_dirty();
    }

    /// Translate a click at screen (row, col) within the content pane
    /// into a transcript (line, col) and jump the content cursor there.
    fn position_content_cursor_from_click(&mut self, row: u16, col: u16) {
        let w = render::term_width();
        let rows = self.screen.full_transcript_text(w);
        if rows.is_empty() {
            self.screen.mark_dirty();
            return;
        }
        let (_, height) = self.screen.size();
        let prompt_rows = self.screen.prev_prompt_rows();
        let prompt_top = height.saturating_sub(prompt_rows);
        let gap_rows: u16 = 1;
        let viewport_rows = prompt_top.saturating_sub(gap_rows);
        if row >= viewport_rows {
            // Click on the gap row — ignore.
            self.screen.mark_dirty();
            return;
        }
        let total = rows.len();
        let max_scroll = total.saturating_sub(viewport_rows as usize);
        let scroll = (self.content_pane.scroll_offset as usize).min(max_scroll);
        let skip = total
            .saturating_sub(viewport_rows as usize)
            .saturating_sub(scroll);
        let line_idx = (skip + row as usize).min(total - 1);
        self.content_pane
            .jump_to_line_col(&rows, line_idx, col as usize, viewport_rows);
        self.screen.mark_dirty();
    }

    /// Ctrl-W pane chord. Returns `Some` when the event was consumed by
    /// the chord state machine (either priming or dispatching).
    fn handle_pane_chord(&mut self, ev: &Event, t: &mut Timers) -> Option<EventOutcome> {
        use crossterm::event::KeyModifiers as M;
        let Event::Key(k) = ev else { return None };

        // In-flight chord: consume the follow-up key.
        if let Some(started) = t.pending_pane_chord {
            if started.elapsed() < PANE_CHORD_WINDOW {
                let navigated = matches!(
                    (k.code, k.modifiers),
                    (KeyCode::Char('w'), _) | (KeyCode::Char('j' | 'k' | 'h' | 'l' | 'p'), M::NONE)
                );
                t.pending_pane_chord = None;
                if navigated {
                    self.toggle_pane_focus();
                    self.screen.mark_dirty();
                    return Some(EventOutcome::Redraw);
                }
                // Non-navigation follow-up — fall through so the key is
                // processed normally.
                return None;
            }
            t.pending_pane_chord = None;
        }

        // Prime the chord.
        if k.code == KeyCode::Char('w') && k.modifiers.contains(M::CONTROL) {
            t.pending_pane_chord = Some(Instant::now());
            return Some(EventOutcome::Noop);
        }
        None
    }

    fn toggle_pane_focus(&mut self) {
        self.app_focus = match self.app_focus {
            crate::app::AppFocus::Prompt => crate::app::AppFocus::Content,
            crate::app::AppFocus::Content => crate::app::AppFocus::Prompt,
        };
        if self.app_focus == crate::app::AppFocus::Content {
            self.refocus_content();
        }
    }

    /// Warm up the content pane on focus switch: mount the transcript,
    /// clamp cpos into range, sync cursor line/col. Without this, a
    /// resumed session has stale/zero state and the first key press
    /// is a no-op until the user triggers a click-to-position.
    fn refocus_content(&mut self) {
        let w = render::term_width();
        let rows = self.screen.full_transcript_text(w);
        let viewport = self.viewport_rows_estimate();
        self.content_pane.refocus(&rows, viewport);
        self.screen.mark_dirty();
    }
}

/// Max inter-key gap between `Ctrl-W` and its follow-up key.
const PANE_CHORD_WINDOW: Duration = Duration::from_millis(750);
