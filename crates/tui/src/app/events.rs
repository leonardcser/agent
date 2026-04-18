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
                        self.input.buf = full.lines().next().unwrap_or(&full).to_string();
                        self.input.cpos = self.input.buf.len();
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
                    self.input.buf = combined;
                    self.input.cpos = self.input.buf.len();
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
                    self.input.save_undo();
                    self.input.buf = new;
                    self.input.cpos = self.input.buf.len();
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
            self.history_scroll_offset,
            self.history_cursor_line,
            self.history_cursor_col,
            visual,
        );
        self.history_scroll_offset = clamped_scroll;
        self.history_cursor_line = clamped_line;
        self.history_cursor_col = clamped_col;
    }

    // ── Content pane key handler — drives `Vim` over a readonly
    // transcript buffer so Normal / Visual / VisualLine motions and
    // yank behave exactly like they do in the prompt.
    fn handle_event_app_history(&mut self, ev: Event) -> EventOutcome {
        let Event::Key(k) = ev else {
            return EventOutcome::Noop;
        };
        use crossterm::event::KeyModifiers as M;

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

    /// Move the content-pane cursor by `delta` lines (positive = down,
    /// negative = up) by synthesizing vim `j`/`k` keys and feeding them
    /// through `handle_content_vim_key`. This reuses vim's own vertical
    /// motion — including `curswant` (desired column) tracking — so
    /// mouse-wheel scroll, Ctrl-U/Ctrl-D, arrow keys, and j/k all take
    /// the same code path.
    fn move_content_cursor_by_lines(&mut self, delta: isize) {
        let (code, count) = if delta >= 0 {
            (KeyCode::Char('j'), delta as usize)
        } else {
            (KeyCode::Char('k'), (-delta) as usize)
        };
        let k = KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::empty(),
        };
        for _ in 0..count {
            if !self.handle_content_vim_key(k) {
                break;
            }
        }
    }

    /// Place the content cursor at `(line_idx, col)` (col measured in
    /// visual columns, clamped to the line) and adjust the viewport
    /// scroll so the cursor stays onscreen.
    fn set_content_cursor_to_line_col(&mut self, rows: &[String], line_idx: usize, col: usize) {
        if rows.is_empty() {
            return;
        }
        let line_idx = line_idx.min(rows.len() - 1);
        let mut acc = 0usize;
        for r in rows.iter().take(line_idx) {
            acc += r.len() + 1;
        }
        let line = &rows[line_idx];
        let max_cols = line.chars().count();
        let col = col.min(max_cols);
        let byte_off = line
            .char_indices()
            .nth(col)
            .map(|(b, _)| b)
            .unwrap_or(line.len());
        self.content_cpos = acc + byte_off;

        let total = rows.len();
        let line_from_bottom = (total - 1).saturating_sub(line_idx) as u16;
        self.history_cursor_line = line_from_bottom;
        self.history_cursor_col = col as u16;

        let viewport = self.viewport_rows_estimate();
        if line_from_bottom >= self.history_scroll_offset + viewport {
            self.history_scroll_offset = line_from_bottom + 1 - viewport;
        } else if line_from_bottom < self.history_scroll_offset {
            self.history_scroll_offset = line_from_bottom;
        }
        self.screen.mark_dirty();
    }

    /// Build the transcript buffer, run `key` through the content-pane
    /// `Vim` instance, and mirror the resulting cursor / visual / yank
    /// state back onto our scroll + cursor. Returns `true` when vim
    /// consumed the key (caller should return `Redraw`).
    fn handle_content_vim_key(&mut self, k: KeyEvent) -> bool {
        let w = render::term_width();
        let rows = self.screen.full_transcript_text(w);
        if rows.is_empty() {
            return false;
        }
        let mut buf = rows.join("\n");
        let total_lines = rows.len();

        // Start-of-bottom-line offset (what cpos maps to when scroll=0).
        let line_start_offsets: Vec<usize> = {
            let mut v = Vec::with_capacity(total_lines);
            let mut acc = 0usize;
            for r in &rows {
                v.push(acc);
                acc += r.len() + 1; // include '\n'
            }
            v
        };

        // Clamp cpos into the current buffer.
        if self.content_cpos > buf.len() {
            self.content_cpos = buf.len();
        }
        let mut cpos = self.content_cpos;
        let mut attachments: Vec<crate::attachment::AttachmentId> = Vec::new();
        let mut ctx = crate::vim::VimContext {
            buf: &mut buf,
            cpos: &mut cpos,
            attachments: &mut attachments,
            kill_ring: &mut self.content_kill,
            history: &mut self.content_undo,
        };
        let action = self.content_vim.handle_key(k, &mut ctx);
        // Insert mode in the content pane would expose editing — snap
        // back to Normal so the pane stays readonly.
        if self.content_vim.mode() == crate::vim::ViMode::Insert {
            self.content_vim.set_mode(crate::vim::ViMode::Normal);
        }
        if matches!(action, crate::vim::Action::Passthrough) {
            return false;
        }

        // On yank (kill_ring updated), push to the system clipboard.
        let yanked = self.content_kill.current().to_string();
        if !yanked.is_empty() {
            let _ = crate::app::commands::copy_to_clipboard(&yanked);
            self.screen
                .notify(format!("yanked {} chars", yanked.chars().count()));
            self.content_kill.set_with_linewise(String::new(), false);
        }

        // Map cpos back to (line_idx, col) and sync scroll/cursor.
        self.content_cpos = cpos.min(
            line_start_offsets.last().copied().unwrap_or(0) + rows.last().map_or(0, |r| r.len()),
        );
        let line_idx = match line_start_offsets.binary_search(&self.content_cpos) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        };
        let col = self.content_cpos - line_start_offsets[line_idx];
        let line_from_bottom = (total_lines - 1).saturating_sub(line_idx) as u16;
        self.history_cursor_line = line_from_bottom;
        self.history_cursor_col = col as u16;

        // Scroll so the cursor line is visible. Viewport height is
        // estimated — the render pass clamps the final values.
        let viewport = self.viewport_rows_estimate();
        if line_from_bottom >= viewport {
            self.history_scroll_offset = line_from_bottom - (viewport - 1);
        } else if line_from_bottom < self.history_scroll_offset.saturating_sub(viewport - 1) {
            self.history_scroll_offset = line_from_bottom;
        }

        self.screen.mark_dirty();
        true
    }

    /// Compute the content pane's visual selection range (if any) in
    /// absolute transcript coordinates for the renderer to highlight.
    fn content_visual_range(&mut self, width: usize) -> Option<render::ContentVisualRange> {
        if self.app_focus != crate::app::AppFocus::Content {
            return None;
        }
        let mode = self.content_vim.mode();
        let kind = match mode {
            crate::vim::ViMode::Visual => render::ContentVisualKind::Char,
            crate::vim::ViMode::VisualLine => render::ContentVisualKind::Line,
            _ => return None,
        };
        let rows = self.screen.full_transcript_text(width);
        if rows.is_empty() {
            return None;
        }
        let buf = rows.join("\n");
        let (s, e) = self.content_vim.visual_range(&buf, self.content_cpos)?;
        let offset_to_line_col = |off: usize| -> (usize, usize) {
            let off = off.min(buf.len());
            let prefix = &buf[..off];
            let line = prefix.bytes().filter(|&b| b == b'\n').count();
            let line_start = prefix.rfind('\n').map(|p| p + 1).unwrap_or(0);
            let col = buf[line_start..off].chars().count();
            (line, col)
        };
        let (start_line, start_col) = offset_to_line_col(s);
        let (end_line, end_col) = offset_to_line_col(e);
        Some(render::ContentVisualRange {
            start_line,
            start_col,
            end_line,
            end_col,
            kind,
        })
    }

    /// Rough viewport-height estimate for cursor clamping at event time.
    /// Prompt height isn't known outside the render pass, so we reserve
    /// a conservative constant and let the render pass clamp the real
    /// cursor row.
    fn viewport_rows_estimate(&self) -> u16 {
        const PROMPT_RESERVE: u16 = 6;
        self.last_height.saturating_sub(PROMPT_RESERVE).max(1)
    }

    // ── Mouse event dispatch ─────────────────────────────────────────────
    fn handle_mouse(&mut self, me: MouseEvent) -> EventOutcome {
        match me.kind {
            MouseEventKind::ScrollUp => {
                // Scroll wheel moves the content cursor like j/k — the
                // viewport only scrolls when the cursor is pushed past
                // an edge. This matches the vim-buffer paradigm.
                self.app_focus = crate::app::AppFocus::Content;
                self.move_content_cursor_by_lines(-3);
                EventOutcome::Redraw
            }
            MouseEventKind::ScrollDown => {
                self.app_focus = crate::app::AppFocus::Content;
                self.move_content_cursor_by_lines(3);
                EventOutcome::Redraw
            }
            MouseEventKind::Down(_) => {
                // Clicks focus whichever pane the click lands in, and
                // position the cursor at the clicked cell.
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
                // Content pane click: focus + move cursor.
                self.app_focus = crate::app::AppFocus::Content;
                self.position_content_cursor_from_click(me.row, me.column);
                EventOutcome::Redraw
            }
            _ => EventOutcome::Noop,
        }
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
        let scroll = (self.history_scroll_offset as usize).min(max_scroll);
        let skip = total
            .saturating_sub(viewport_rows as usize)
            .saturating_sub(scroll);
        let line_idx = (skip + row as usize).min(total - 1);
        self.set_content_cursor_to_line_col(&rows, line_idx, col as usize);
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
    }
}

/// Max inter-key gap between `Ctrl-W` and its follow-up key.
const PANE_CHORD_WINDOW: Duration = Duration::from_millis(750);
