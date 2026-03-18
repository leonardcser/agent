use super::*;

use crossterm::{event::Event, terminal};

impl App {
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
        // Route events to the active dialog if one is showing.
        if active_dialog.is_some() {
            // Terminal resize: full clear + redraw screen + redraw dialog.
            if let Event::Resize(w, h) = ev {
                if w != self.last_width || h != self.last_height {
                    self.last_width = w;
                    self.last_height = h;
                    self.screen.redraw(true);
                }
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
                        .decide(self.mode, &ctx.tool_name, &ctx.args)
                        == Decision::Allow
                    {
                        let d = active_dialog.take().unwrap();
                        self.screen.clear_dialog_area(d.anchor_row());
                        self.screen.set_active_status(ToolStatus::Pending);
                        self.engine.send(UiCommand::PermissionDecision {
                            request_id: ctx.request_id,
                            approved: true,
                            message: None,
                        });
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
                let mut d = active_dialog.take().unwrap();
                if let Some(result) = d.handle_key(code, modifiers) {
                    let anchor = d.anchor_row();
                    self.handle_dialog_result(result, anchor, agent);
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
            EventOutcome::CancelAndClear => {
                engine::log::entry(
                    engine::log::Level::Info,
                    "agent_stop",
                    &serde_json::json!({
                        "reason": "user_cancel_and_clear",
                    }),
                );
                if agent.is_some() {
                    self.cancel_agent();
                    *agent = None;
                }
                self.reset_session();
                false
            }
            EventOutcome::MenuResult(result) => {
                match result {
                    MenuResult::Settings {
                        vim,
                        auto_compact,
                        show_speed,
                        show_prediction,
                        show_slug,
                        restrict_to_workspace,
                    } => {
                        self.input.set_vim_enabled(vim);
                        state::set_vim_enabled(vim);
                        self.auto_compact = auto_compact;
                        self.show_speed = show_speed;
                        self.screen.set_show_speed(show_speed);
                        self.show_prediction = show_prediction;
                        self.show_slug = show_slug;
                        self.screen.set_show_slug(show_slug);
                        self.restrict_to_workspace = restrict_to_workspace;
                    }
                    MenuResult::ModelSelect(key) => {
                        if let Some(resolved) = self.available_models.iter().find(|m| m.key == key)
                        {
                            self.model = resolved.model_name.clone();
                            self.api_base = resolved.api_base.clone();
                            self.api_key_env = resolved.api_key_env.clone();
                            self.screen.set_model_label(resolved.model_name.clone());
                            state::set_selected_model(key.clone());
                            self.engine.send(UiCommand::SetModel {
                                model: resolved.model_name.clone(),
                                api_base: resolved.api_base.clone(),
                                api_key: std::env::var(&resolved.api_key_env).unwrap_or_default(),
                            });
                        }
                        self.screen.erase_prompt();
                    }
                    MenuResult::ThemeSelect(value) => {
                        state::set_accent(value);
                        self.screen.redraw(true);
                    }
                    MenuResult::ColorSelect(_) => {
                        // Session-only, no persistence needed
                        self.screen.mark_dirty();
                    }
                    MenuResult::Stats | MenuResult::Dismissed => {}
                }
                self.input.restore_stash();
                self.screen.mark_dirty();
                false
            }
            EventOutcome::OpenDialog(dlg) => {
                self.screen.erase_prompt();
                *active_dialog = Some(dlg);
                false
            }
            EventOutcome::Exec(rx, kill) => {
                self.screen.erase_prompt();
                self.exec_rx = Some(rx);
                self.exec_kill = Some(kill);
                false
            }
            EventOutcome::Submit { content, display } => {
                if self.try_btw_submit(&content, &display) {
                    // handled
                } else {
                    let text = content.text_content();
                    let has_images = content.image_count() > 0;
                    if !text.is_empty() || has_images {
                        self.screen.erase_prompt();
                        let outcome = if has_images && text.trim().is_empty() {
                            InputOutcome::StartAgent
                        } else {
                            self.process_input(&text)
                        };
                        match outcome {
                            InputOutcome::StartAgent => {
                                *agent = Some(self.begin_agent_turn(&display, content));
                            }
                            InputOutcome::CustomCommand(cmd) => {
                                *agent = Some(self.begin_custom_command_turn(*cmd));
                            }
                            InputOutcome::Compact { focus } => {
                                if self.history.is_empty() {
                                    self.screen.notify_error("nothing to compact".into());
                                } else {
                                    self.compact_history(focus);
                                }
                            }
                            InputOutcome::Exec(rx, kill) => {
                                self.exec_rx = Some(rx);
                                self.exec_kill = Some(kill);
                            }
                            InputOutcome::Continue => {}
                            InputOutcome::Quit => return true,
                            InputOutcome::OpenDialog(dlg) => {
                                *active_dialog = Some(dlg);
                            }
                        }
                    }
                }
                // Restore stash unless a menu was opened (it will restore on menu close).
                if self.input.menu_rows() == 0 {
                    self.input.restore_stash();
                }
                false
            }
        }
    }

    // ── Idle event handler ───────────────────────────────────────────────

    fn handle_event_idle(&mut self, ev: Event, t: &mut Timers) -> EventOutcome {
        // Resize
        if let Event::Resize(w, h) = ev {
            if w != self.last_width || h != self.last_height {
                self.last_width = w;
                self.last_height = h;
                self.screen.redraw(true);
            }
            return EventOutcome::Noop;
        }

        if let Some(outcome) = self.handle_overlay_keys(&ev) {
            return outcome;
        }

        // Ctrl+R: open history fuzzy search (not in vim normal mode).
        if matches!(
            ev,
            Event::Key(KeyEvent {
                code: KeyCode::Char('r'),
                modifiers: KeyModifiers::CONTROL,
                ..
            })
        ) && self.input.history_search_query().is_none()
            && !self
                .input
                .vim_mode()
                .is_some_and(|m| m == vim::ViMode::Normal)
        {
            self.input.open_history_search(&self.input_history);
            self.screen.mark_dirty();
            return EventOutcome::Redraw;
        }

        // Ctrl+C: dismiss the topmost UI element, or quit if nothing is open.
        if matches!(
            ev,
            Event::Key(KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                ..
            })
        ) {
            // Menu open → dismiss it.
            if let Some(result) = self.input.dismiss_menu() {
                self.screen.mark_dirty();
                return EventOutcome::MenuResult(result);
            }
            // Completer open → close it.
            if self.input.completer.is_some() {
                self.input.completer = None;
                self.screen.mark_dirty();
                return EventOutcome::Redraw;
            }
            // Non-empty prompt → clear it.
            if !self.input.buf.is_empty() {
                t.last_ctrlc = Some(Instant::now());
                self.input.clear();
                self.screen.mark_dirty();
                return EventOutcome::Redraw;
            }
            // Nothing open, empty prompt → quit.
            let double_tap = t
                .last_ctrlc
                .is_some_and(|prev| prev.elapsed() < Duration::from_millis(500));
            if double_tap {
                return EventOutcome::Quit;
            }
            t.last_ctrlc = Some(Instant::now());
            return EventOutcome::Quit;
        }

        // ?: open help dialog (only when input is empty so it doesn't interfere with typing).
        if self.input.buf.is_empty()
            && matches!(
                ev,
                Event::Key(KeyEvent {
                    code: KeyCode::Char('?'),
                    modifiers: KeyModifiers::NONE,
                    ..
                })
            )
        {
            return EventOutcome::OpenDialog(Box::new(render::HelpDialog::new()));
        }

        // Ctrl+S: toggle stash.
        if matches!(
            ev,
            Event::Key(KeyEvent {
                code: KeyCode::Char('s'),
                modifiers: KeyModifiers::CONTROL,
                ..
            })
        ) {
            self.input.toggle_stash();
            self.screen.mark_dirty();
            return EventOutcome::Redraw;
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

                    // Cancel in-flight compaction instead of opening rewind.
                    if self.screen.working_throbber() == Some(render::Throbber::Compacting) {
                        self.compact_epoch += 1;
                        self.screen.set_throbber(render::Throbber::Done);
                        self.screen.notify("compaction cancelled".into());
                        if restore_mode == Some(vim::ViMode::Insert) {
                            self.input.set_vim_mode(vim::ViMode::Insert);
                        }
                        return EventOutcome::Noop;
                    }

                    let turns = self.screen.user_turns();
                    if turns.is_empty() {
                        return EventOutcome::Noop;
                    }
                    self.screen.erase_prompt();
                    let restore_vim_insert = restore_mode == Some(vim::ViMode::Insert);
                    return EventOutcome::OpenDialog(Box::new(render::RewindDialog::new(
                        turns,
                        restore_vim_insert,
                        Some(terminal::size().map(|(_, h)| h / 2).unwrap_or(12)),
                    )));
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

        // Ghost-text prediction: Tab accepts, any other key dismisses.
        if self.input_prediction.is_some() && self.input.buf.is_empty() {
            if matches!(
                ev,
                Event::Key(KeyEvent {
                    code: KeyCode::Tab,
                    ..
                })
            ) {
                self.input.buf = self.input_prediction.take().unwrap();
                self.input.cpos = self.input.buf.len();
                self.screen.mark_dirty();
                return EventOutcome::Redraw;
            }
            self.input_prediction = None;
        }

        // Delegate to InputState::handle_event
        match self.input.handle_event(ev, Some(&mut self.input_history)) {
            Action::Submit { content, display } => EventOutcome::Submit { content, display },
            Action::MenuResult(result) => EventOutcome::MenuResult(result),
            Action::ToggleMode => {
                self.toggle_mode();
                EventOutcome::Redraw
            }
            Action::CycleReasoning => {
                self.set_reasoning_effort(self.reasoning_effort.cycle());
                EventOutcome::Redraw
            }
            Action::Resize {
                width: w,
                height: h,
            } => {
                let (w16, h16) = (w as u16, h as u16);
                if w16 != self.last_width || h16 != self.last_height {
                    self.last_width = w16;
                    self.last_height = h16;
                    self.screen.redraw(true);
                }
                EventOutcome::Noop
            }
            Action::Redraw => {
                // Live-preview settings toggles.
                if let Some(ref ms) = self.input.menu {
                    if let MenuKind::Settings {
                        show_speed,
                        show_slug,
                        ..
                    } = ms.kind
                    {
                        self.show_speed = show_speed;
                        self.screen.set_show_speed(show_speed);
                        self.show_slug = show_slug;
                        self.screen.set_show_slug(show_slug);
                    }
                }
                self.screen.mark_dirty();
                EventOutcome::Redraw
            }
            Action::NotifyError(msg) => {
                self.screen.notify_error(msg);
                EventOutcome::Redraw
            }
            Action::Noop => EventOutcome::Noop,
        }
    }

    // ── Running event handler ────────────────────────────────────────────

    fn handle_event_running(&mut self, ev: Event, t: &mut Timers) -> EventOutcome {
        // Resize
        if let Event::Resize(w, h) = ev {
            if w != self.last_width || h != self.last_height {
                self.last_width = w;
                self.last_height = h;
                self.screen.redraw(true);
            }
            return EventOutcome::Noop;
        }

        if let Some(outcome) = self.handle_overlay_keys(&ev) {
            return outcome;
        }

        // Track last keypress for deferring permission dialogs.
        if matches!(ev, Event::Key(_)) {
            t.last_keypress = Some(Instant::now());
        }

        // Ctrl+C: dismiss UI elements first, then cancel agent.
        if matches!(
            ev,
            Event::Key(KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                ..
            })
        ) {
            // Menu open → dismiss it.
            if let Some(result) = self.input.dismiss_menu() {
                self.screen.mark_dirty();
                return EventOutcome::MenuResult(result);
            }
            // Completer open → close it.
            if self.input.completer.is_some() {
                self.input.completer = None;
                self.screen.mark_dirty();
                return EventOutcome::Noop;
            }
            // Non-empty prompt → clear it + queued messages.
            if !self.input.buf.is_empty() {
                t.last_ctrlc = Some(Instant::now());
                self.input.clear();
                let count = self.steered_message_count();
                if count > 0 {
                    self.engine.send(UiCommand::Unsteer { count });
                }
                self.queued_messages.clear();
                self.screen.mark_dirty();
                return EventOutcome::Noop;
            }
            // Nothing open → cancel agent and clear queued messages.
            self.queued_messages.clear();
            self.screen.mark_dirty();
            return EventOutcome::CancelAgent;
        }

        // Esc: use resolve_agent_esc for the running-mode logic.
        if matches!(
            ev,
            Event::Key(KeyEvent {
                code: KeyCode::Esc,
                ..
            })
        ) {
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
                    // Tell the engine to remove already-steered messages.
                    let count = self.steered_message_count();
                    if count > 0 {
                        self.engine.send(UiCommand::Unsteer { count });
                    }
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
            Action::Submit { content, display } => {
                if self.try_btw_submit(&content, &display) {
                    self.screen.mark_dirty();
                    return EventOutcome::Noop;
                }
                let text = content.text_content();
                if let Some(outcome) = self.try_command_while_running(text.trim()) {
                    return outcome;
                }
                // Not a command — queue as a user message.
                if !text.is_empty() {
                    self.queued_messages.push(text);
                }
                self.screen.mark_dirty();
            }
            Action::ToggleMode => {
                self.toggle_mode();
            }
            Action::Redraw => {
                self.screen.mark_dirty();
            }
            Action::CycleReasoning => {
                self.set_reasoning_effort(self.reasoning_effort.cycle());
            }
            Action::NotifyError(msg) => {
                self.screen.notify_error(msg);
                self.screen.mark_dirty();
            }
            Action::MenuResult(_) | Action::Noop | Action::Resize { .. } => {}
        }
        EventOutcome::Noop
    }

    // ── Shared helpers ────────────────────────────────────────────────────

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
                match (*code, *modifiers) {
                    (KeyCode::Char('d'), KeyModifiers::CONTROL) | (KeyCode::PageDown, _) => {
                        self.screen.btw_scroll(10);
                        return Some(EventOutcome::Noop);
                    }
                    (KeyCode::Char('u'), KeyModifiers::CONTROL) | (KeyCode::PageUp, _) => {
                        self.screen.btw_scroll(-10);
                        return Some(EventOutcome::Noop);
                    }
                    _ => {
                        self.screen.dismiss_btw();
                        return Some(EventOutcome::Noop);
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
        state::set_mode(self.mode);

        // Skip shell escape for pasted content
        let is_from_paste = self.input.skip_shell_escape();
        match self.handle_command(trimmed) {
            CommandAction::Quit => return InputOutcome::Quit,
            CommandAction::CancelAndClear => {
                self.reset_session();
                return InputOutcome::Continue;
            }
            CommandAction::Compact { focus } => return InputOutcome::Compact { focus },
            CommandAction::OpenDialog(dlg) => return InputOutcome::OpenDialog(dlg),
            CommandAction::Exec(rx, kill) => return InputOutcome::Exec(rx, kill),
            CommandAction::Continue => {}
        }
        if trimmed.starts_with('/') {
            if let Some(cmd) = crate::custom_commands::resolve(trimmed) {
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

    /// Returns true if a dialog overlay needs to be re-dirtied (because
    /// `draw_frame` cleared the area underneath it).
    pub(super) fn tick(&mut self, agent_running: bool, has_dialog: bool) -> bool {
        let w = render::term_width();
        if has_dialog {
            // Render blocks + active tool but skip the prompt — the dialog
            // covers the bottom and must stay at the highest z-index.
            return self.screen.draw_frame(w, None);
        }
        if agent_running {
            self.screen.draw_frame(
                w,
                Some(FramePrompt {
                    state: &self.input,
                    mode: self.mode,
                    queued: &self.queued_messages,
                    prediction: None,
                }),
            );
        } else {
            self.screen.draw_frame(
                w,
                Some(FramePrompt {
                    state: &self.input,
                    mode: self.mode,
                    queued: &[],
                    prediction: self.input_prediction.as_deref(),
                }),
            );
        }
        false
    }
}
