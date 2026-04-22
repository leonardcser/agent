use super::*;

use crate::keymap::{self, KeyAction};
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture, Event, MouseEvent, MouseEventKind},
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use std::time::{Duration, Instant};

impl App {
    fn apply_settings_result(&mut self, s: &crate::input::SettingsState) {
        self.set_settings(s.clone());
    }

    // ── Terminal event dispatch ───────────────────────────────────────────

    /// Handle a single terminal event, potentially starting/stopping agents.
    /// Returns `true` if the app should quit.
    pub(super) fn dispatch_terminal_event(&mut self, ev: Event, t: &mut Timers) -> bool {
        if matches!(ev, Event::FocusGained | Event::FocusLost) {
            let focused = matches!(ev, Event::FocusGained);
            if self.term_focused != focused {
                self.term_focused = focused;
            }
            return false;
        }

        // Global chord layer: these keys fire in every focus context
        // (prompt, content, cmdline, or any float). Intercepted before
        // focus-specific routing so no handler below can swallow them.
        if let Event::Key(KeyEvent {
            code, modifiers, ..
        }) = &ev
        {
            // Global shortcuts only fire when no float is focused —
            // otherwise the float's keymap (e.g. Confirm's BackTab
            // handler) gets first dibs.
            if self.ui.focused_float().is_none() {
                match (*code, *modifiers) {
                    (KeyCode::BackTab, _) => {
                        self.toggle_mode();
                        return false;
                    }
                    (KeyCode::Char('t'), m) if m.contains(KeyModifiers::CONTROL) => {
                        self.cycle_reasoning();
                        return false;
                    }
                    (KeyCode::Char('l'), m) if m.contains(KeyModifiers::CONTROL) => {
                        self.ui.force_redraw();
                        return false;
                    }
                    _ => {}
                }
            }
        }

        // Compositor float: when a float window is focused, route keys
        // through the compositor. The Callbacks registry handles
        // per-window keymaps, auto-dispatches widget actions into
        // `WinEvent::Submit/Dismiss`, and any Rust callbacks queue
        // `AppOp`s drained below.
        if self.ui.focused_float().is_some() {
            if let Event::Resize(w, h) = ev {
                self.handle_resize(w, h);
                return false;
            }
            if let Event::Key(KeyEvent {
                code, modifiers, ..
            }) = ev
            {
                let _ = self.ui.handle_key(code, modifiers);
                self.apply_lua_ops();
            }
            return false;
        }

        // Cmdline mode: when the `:` command line is active, route
        // all key events to it. Esc cancels, Enter executes.
        if self.cmdline.active {
            if let Event::Key(k) = ev {
                return self.handle_cmdline_key(k);
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

        let outcome = if self.agent.is_some() {
            self.handle_event_running(ev, t)
        } else {
            self.handle_event_idle(ev, t)
        };

        match outcome {
            EventOutcome::Noop | EventOutcome::Redraw => false,
            EventOutcome::Quit => {
                self.discard_turn(true);
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
                self.discard_turn(true);
                false
            }
            EventOutcome::InterruptWithQueued => {
                // Cancel the running turn and let the queued messages
                // auto-start via the main loop once TurnComplete arrives.
                // We must save the queued messages before finish_turn
                // because the cancel path dumps them into the input buffer.
                let remaining = std::mem::take(&mut self.queued_messages);
                self.discard_turn(true);
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
                self.agent = None;
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
                    }
                    MenuResult::ThemeSelect(value) => {
                        // Live preview already set the in-memory accent;
                        // mirror it explicitly so settings.json and the
                        // atomic stay in lockstep regardless of preview
                        // state.
                        self.apply_accent(value);
                    }
                    MenuResult::ColorSelect(_) => {
                    }
                    MenuResult::Stats | MenuResult::Cost | MenuResult::Dismissed => {}
                }
                let is_settings = matches!(&result, MenuResult::Settings(_));
                if !is_settings {
                    self.input.restore_stash();
                }
                false
            }
            EventOutcome::Exec(rx, kill) => {
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
                    }
                } else {
                    let text = content.text_content();
                    let has_images = content.image_count() > 0;
                    if !text.is_empty() || has_images {
                        let outcome = if has_images && text.trim().is_empty() {
                            InputOutcome::StartAgent
                        } else {
                            self.process_input(&text)
                        };
                        if self.apply_input_outcome(outcome, content, &display) {
                            return true;
                        }
                    } else if !self.queued_messages.is_empty() {
                        // Empty submit with queued messages: pop and send the
                        // oldest one immediately.
                        let queued = self.queued_messages.remove(0);
                        if let Some(cmd) =
                            crate::custom_commands::resolve(queued.trim(), self.multi_agent)
                        {
                            let turn = self.begin_custom_command_turn(cmd);
                            self.agent = Some(turn);
                        } else {
                            let outcome = self.process_input(&queued);
                            let content = Content::text(queued.clone());
                            if self.apply_input_outcome(outcome, content, &queued) {
                                return true;
                            }
                        }
                    }
                }
                // Restore stash unless a modal/dialog was opened (it will restore on close).
                if !self.input.has_modal() && self.ui.focused_float().is_none() {
                    self.input.restore_stash();
                }
                false
            }
        }
    }

    // ── Idle event handler ───────────────────────────────────────────────

    /// Shared event-routing preamble for both the idle and
    /// agent-running paths. Handles the routes that behave identically
    /// regardless of whether the agent is streaming: paste (drops the
    /// prompt prediction), resize, mouse (wheel, click, drag-select,
    /// scrollbar), `Ctrl-W` pane chord, transcript-window key
    /// routing when `Content` has focus, and dialog/overlay keys.
    ///
    /// Shared key/event preamble for both idle and running paths.
    ///
    /// Returns `Some(outcome)` when the event was fully handled;
    /// `None` when the caller should continue with path-specific
    /// logic (esc handling, keymap lookups, `InputState`).
    ///
    /// Dispatch priority (first match wins):
    ///  1. Resize / mouse — structural events, always handled
    ///  2. Lua keymaps    — user-registered chords (`vim.keymap.set`)
    ///  3. Pane chords    — Ctrl-W window management
    ///  4. Cmdline `:`    — opens nvim-style command line (normal mode only)
    ///  5. Content focus  — transcript navigation when content pane is focused
    ///  6. Overlay keys   — notifications, btw block dismiss
    fn dispatch_common(&mut self, ev: &Event, t: &mut Timers) -> Option<EventOutcome> {
        if matches!(ev, Event::Paste(_)) {
            self.input_prediction = None;
        }
        if let Event::Resize(w, h) = *ev {
            self.handle_resize(w, h);
            return Some(EventOutcome::Noop);
        }
        if let Event::Mouse(me) = *ev {
            return Some(self.handle_mouse(me));
        }
        // Lua-registered keymaps get first crack at key events, matching
        // nvim's `vim.keymap.set` priority. Unbound chords fall through
        // to the built-in keymap dispatcher.
        if let Event::Key(k) = *ev {
            if let Some(chord) = crate::lua::chord_string(k) {
                let (vim_mode, focused_window) = self.snapshot_lua_context();
                let handled = self.lua.run_keymap(&chord, vim_mode.as_deref());
                self.lua.clear_context();
                if handled {
                    let _ = focused_window;
                    self.apply_lua_ops();
                    return Some(EventOutcome::Noop);
                }
            }
        }
        if let Some(outcome) = self.handle_pane_chord(ev, t) {
            return Some(outcome);
        }
        // `:` opens the cmdline from any window, unless in insert mode.
        if let Event::Key(KeyEvent {
            code: KeyCode::Char(':'),
            modifiers: KeyModifiers::NONE | KeyModifiers::SHIFT,
            ..
        }) = ev
        {
            let in_insert = match self.app_focus {
                crate::app::AppFocus::Prompt => {
                    !self.input.vim_enabled() || self.input.vim_in_insert_mode()
                }
                crate::app::AppFocus::Content => false,
            };
            if !in_insert && !self.input.has_modal() {
                self.open_cmdline();
                return Some(EventOutcome::Noop);
            }
        }
        if self.app_focus == crate::app::AppFocus::Content && !self.input.has_modal() {
            return Some(self.handle_event_app_history(ev));
        }
        if let Some(outcome) = self.handle_overlay_keys(ev) {
            return Some(outcome);
        }
        None
    }

    fn handle_event_idle(&mut self, ev: Event, t: &mut Timers) -> EventOutcome {
        if let Some(outcome) = self.dispatch_common(&ev, t) {
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
                    if self.working.throbber == Some(render::Throbber::Compacting) {
                        self.compact_epoch += 1;
                        {
                            self.working.set_throbber(render::Throbber::Interrupted);
                        };
                        self.notify("compaction cancelled".into());
                        if restore_mode == Some(vim::ViMode::Insert) {
                            self.input.set_vim_mode(vim::ViMode::Insert);
                        }
                        return EventOutcome::Noop;
                    }

                    let turns = self.user_turns();
                    if turns.is_empty() {
                        return EventOutcome::Noop;
                    }
                    let restore_vim_insert = restore_mode == Some(vim::ViMode::Insert);
                    super::dialogs::rewind::open(self, turns, restore_vim_insert);
                    return EventOutcome::Redraw;
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
                        return EventOutcome::Redraw;
                    }
                    Some(
                        KeyAction::ToggleMode
                        | KeyAction::CycleReasoning
                        | KeyAction::Redraw
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
                                return EventOutcome::MenuResult(result);
                            }
                            if self.input.completer.is_some() {
                                self.input.completer = None;
                                return EventOutcome::Redraw;
                            }
                            t.last_ctrlc = Some(Instant::now());
                            self.input.clear();
                            return EventOutcome::Redraw;
                        }
                        KeyAction::OpenHelp => {
                            super::dialogs::help::open(self);
                            return EventOutcome::Redraw;
                        }
                        KeyAction::OpenHistorySearch => {
                            if self.input.history_search_query().is_none() {
                                self.input.open_history_search(&self.input_history);
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
        if let Some(outcome) = self.dispatch_common(&ev, t) {
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
                            return EventOutcome::MenuResult(result);
                        }
                        if self.input.completer.is_some() {
                            self.input.completer = None;
                            return EventOutcome::Noop;
                        }
                        self.queued_messages.clear();
                        return EventOutcome::CancelAgent;
                    }
                    KeyAction::ClearBuffer => {
                        // Dismiss menu/completer first, then clear.
                        if let Some(result) = self.input.dismiss_menu() {
                            return EventOutcome::MenuResult(result);
                        }
                        if self.input.completer.is_some() {
                            self.input.completer = None;
                            return EventOutcome::Noop;
                        }
                        t.last_ctrlc = Some(Instant::now());
                        self.input.clear();
                        self.queued_messages.clear();
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
                }
                EscAction::Unqueue => {
                    let mut combined = self.queued_messages.join("\n");
                    if !self.input.buf.is_empty() {
                        combined.push('\n');
                        combined.push_str(&self.input.buf);
                    }
                    crate::api::buf::replace(&mut self.input, combined, None);
                    self.queued_messages.clear();
                }
                EscAction::Cancel { restore_vim } => {
                    if let Some(mode) = restore_vim {
                        self.input.set_vim_mode(mode);
                    }
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
                let text = content.text_content();
                if let Some(outcome) = self.try_command_while_running(text.trim()) {
                    return outcome;
                }
                if !text.is_empty() {
                    self.queued_messages.push(text);
                }
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
            }
            Action::CycleReasoning => {
                self.cycle_reasoning();
            }
            Action::EditInEditor => {
                self.edit_in_editor();
            }
            Action::CenterScroll => {
                self.prompt_input_scroll = usize::MAX;
            }
            Action::NotifyError(msg) => {
                self.notify_error(msg);
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
                EventOutcome::Noop
            }
            Action::CenterScroll => {
                self.prompt_input_scroll = usize::MAX;
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
                EventOutcome::Redraw
            }
            Action::NotifyError(msg) => {
                self.notify_error(msg);
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
                self.notify_error(format!("tmpfile: {e}"));
                return;
            }
        };
        if let Err(e) = std::fs::write(tmp.path(), &self.input.buf) {
            self.notify_error(format!("write tmp: {e}"));
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
                Err(e) => self.notify_error(format!("read tmp: {e}")),
            },
            Ok(s) => {
                self.notify_error(format!("{editor} exited with {s}"));
            }
            Err(e) => {
                self.notify_error(format!("{editor}: {e}"));
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
        self.ui.set_terminal_size(w, h);
        if width_changed {
            self.invalidate_for_width(w);
        }
    }

    /// Handle overlay keys (notification dismiss).
    /// Returns `Some(EventOutcome)` if the event was consumed.
    fn handle_overlay_keys(&mut self, ev: &Event) -> Option<EventOutcome> {
        if matches!(ev, Event::Key(_)) && self.notification.is_some() {
            self.dismiss_notification();
        }

        None
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
        match super::commands::run_command(self, trimmed) {
            CommandAction::Quit => return InputOutcome::Quit,
            CommandAction::CancelAndClear => {
                return InputOutcome::CancelAndClear;
            }
            CommandAction::Compact { instructions } => {
                return InputOutcome::Compact { instructions }
            }
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

        // Fire InputSubmit event so Lua plugins can observe/log.
        let input_text = trimmed.to_string();
        self.lua
            .emit_data(crate::lua::AutocmdEvent::InputSubmit, |lua| {
                let t = lua.create_table()?;
                t.set("text", input_text)?;
                Ok(t)
            });
        self.apply_lua_ops();

        InputOutcome::StartAgent
    }

    // ── Tick ─────────────────────────────────────────────────────────────

    /// Build the status-bar position record for the focused window.
    /// Buffer-agnostic: reads the current buffer + byte offset from
    /// whichever window has focus, so every window shares one
    /// `<line>:<col> <pct>%` formatter and the numbers stay in sync
    /// with the actual cursor position regardless of which code path
    /// moved it (key, motion, mouse click, scroll).
    fn compute_status_position(&mut self) -> Option<render::StatusPosition> {
        match self.app_focus {
            crate::app::AppFocus::Prompt => {
                use crate::text_utils::byte_to_cell;
                let buf = &self.input.buf;
                let cpos = self.input.win.cpos.min(buf.len());
                let line_idx = buf[..cpos].bytes().filter(|&b| b == b'\n').count();
                let line_start = buf[..cpos].rfind('\n').map(|i| i + 1).unwrap_or(0);
                let col_cells = byte_to_cell(&buf[line_start..], cpos - line_start);
                let total_lines = buf.bytes().filter(|&b| b == b'\n').count() + 1;
                let pct = if total_lines <= 1 {
                    100
                } else {
                    ((line_idx as u64 * 100) / (total_lines.saturating_sub(1) as u64)) as u8
                };
                Some(render::StatusPosition {
                    line: (line_idx as u32) + 1,
                    col: col_cells as u32 + 1,
                    scroll_pct: pct.min(100),
                })
            }
            crate::app::AppFocus::Content => {
                let total_lines = self.full_transcript_display_text(self.settings.show_thinking)
                    .len();
                if total_lines == 0 {
                    return None;
                }
                let line_idx = self.transcript_window.cursor_abs_row();
                let pct = if total_lines <= 1 {
                    100
                } else {
                    ((line_idx as u64 * 100) / (total_lines.saturating_sub(1) as u64)) as u8
                };
                Some(render::StatusPosition {
                    line: (line_idx as u32) + 1,
                    col: self.transcript_window.cursor_col as u32 + 1,
                    scroll_pct: pct.min(100),
                })
            }
        }
    }

    fn refresh_status_bar(&mut self) {
        use crossterm::style::Color;
        use render::status::{spans_to_segments, StatusSpan};
        use ui::grid::Style;

        let (term_w, _) = self.ui.terminal_size();
        let width = term_w as usize;
        let status_bg = Color::AnsiValue(233);

        // Cmdline mode: the status row IS the `:` line when active.
        if self.cmdline.active {
            let _ = width;
            let (left, right) = render::status::build_cmdline_segments(&self.cmdline, status_bg);
            if let Some(bar) = self.ui.layer_mut::<ui::StatusBar>("status") {
                *bar = ui::StatusBar::new().with_bg(Style::bg(status_bg));
                bar.set_left(left);
                bar.set_right(right);
            }
            return;
        }

        // Custom status items (from Lua plugins) override everything.
        if let Some(items) = self.custom_status_items.as_ref() {
            let mut spans: Vec<StatusSpan> = items.iter().map(|i| i.to_span(status_bg)).collect();
            let (left, right) = spans_to_segments(&mut spans, width, status_bg);
            if let Some(bar) = self.ui.layer_mut::<ui::StatusBar>("status") {
                *bar = ui::StatusBar::new().with_bg(Style::bg(status_bg));
                bar.set_left(left);
                bar.set_right(right);
            }
            return;
        }

        let mut spans: Vec<StatusSpan> = Vec::with_capacity(16);

        // Slug pill: spinner + label.
        let is_compacting = self.working.throbber == Some(render::Throbber::Compacting);
        let pill_bg = if is_compacting {
            Color::White
        } else {
            crate::theme::slug_color()
        };
        let pill_style = render::StyleState {
            fg: Some(Color::Black),
            bg: Some(pill_bg),
            ..render::StyleState::default()
        };

        let spinner_char = self.working.spinner_char();
        if let Some(sp) = spinner_char {
            spans.push(StatusSpan {
                text: format!(" {sp} "),
                style: pill_style.clone(),
                priority: 0,
                ..StatusSpan::default()
            });
            let label = if is_compacting {
                "compacting ".into()
            } else if self.settings.show_slug {
                self.task_label
                    .as_deref()
                    .map(|l| format!("{l} "))
                    .unwrap_or_else(|| "working ".into())
            } else {
                "working ".into()
            };
            spans.push(StatusSpan {
                text: label,
                style: pill_style,
                priority: 5,
                truncatable: true,
                ..StatusSpan::default()
            });
        } else if self.settings.show_slug {
            if let Some(label) = self.task_label.as_deref() {
                spans.push(StatusSpan {
                    text: format!(" {label} "),
                    style: pill_style,
                    priority: 5,
                    truncatable: true,
                    ..StatusSpan::default()
                });
            }
        }

        // Vim mode.
        let (vim_enabled, vim_mode) = match self.app_focus {
            crate::app::AppFocus::Content => (
                self.transcript_window.vim.is_some(),
                self.transcript_window.vim.as_ref().map(|v| v.mode()),
            ),
            crate::app::AppFocus::Prompt => {
                let mut mode = self.input.vim_mode();
                if self.mouse_drag_active {
                    mode = Some(crate::vim::ViMode::Visual);
                }
                (self.input.vim_enabled() || self.mouse_drag_active, mode)
            }
        };
        if vim_enabled {
            let vim_label = render::status::vim_mode_label(vim_mode).unwrap_or("NORMAL");
            let vim_fg = match vim_mode {
                Some(crate::vim::ViMode::Insert) => Color::AnsiValue(78),
                Some(crate::vim::ViMode::Visual) | Some(crate::vim::ViMode::VisualLine) => {
                    Color::AnsiValue(176)
                }
                _ => Color::AnsiValue(74),
            };
            spans.push(StatusSpan {
                text: format!(" {vim_label} "),
                style: render::StyleState {
                    fg: Some(vim_fg),
                    bg: Some(Color::AnsiValue(236)),
                    ..render::StyleState::default()
                },
                priority: 3,
                ..StatusSpan::default()
            });
        }

        // Mode indicator.
        let mode = self.mode;
        let (mode_icon, mode_name, mode_fg) = match mode {
            protocol::Mode::Plan => ("◇ ", "plan", crate::theme::PLAN),
            protocol::Mode::Apply => ("→ ", "apply", crate::theme::APPLY),
            protocol::Mode::Yolo => ("⚡", "yolo", crate::theme::YOLO),
            protocol::Mode::Normal => ("○ ", "normal", crate::theme::muted()),
        };
        spans.push(StatusSpan {
            text: format!(" {mode_icon}{mode_name} "),
            style: render::StyleState {
                fg: Some(mode_fg),
                bg: Some(Color::AnsiValue(234)),
                ..render::StyleState::default()
            },
            priority: 1,
            ..StatusSpan::default()
        });

        // Throbber spans (timer, tok/s, etc.).
        let throbber_spans = self.working.throbber_spans(self.settings.show_tps);
        let is_active = matches!(
            self.working.throbber,
            Some(render::Throbber::Working)
                | Some(render::Throbber::Compacting)
                | Some(render::Throbber::Retrying { .. })
        );
        let skip = if is_active && !throbber_spans.is_empty() {
            1
        } else {
            0
        };
        for bar_span in throbber_spans.iter().skip(skip) {
            let priority = match bar_span.priority {
                0 => 4,
                3 => 6,
                p => p,
            };
            spans.push(StatusSpan {
                text: bar_span.text.clone(),
                style: render::StyleState {
                    fg: Some(bar_span.color),
                    bg: Some(status_bg),
                    bold: bar_span.bold,
                    dim: bar_span.dim,
                    ..render::StyleState::default()
                },
                priority,
                ..StatusSpan::default()
            });
        }

        // Permission pending (no Confirm float is showing yet).
        if self.pending_dialog && !self.focused_float_blocks_agent() {
            spans.push(StatusSpan {
                text: "permission pending".into(),
                style: render::StyleState {
                    fg: Some(crate::theme::accent()),
                    bg: Some(status_bg),
                    bold: true,
                    ..render::StyleState::default()
                },
                priority: 2,
                group: true,
                ..StatusSpan::default()
            });
        }

        // Running procs.
        let running_procs = self.engine.processes.running_count();
        if running_procs > 0 {
            let label = if running_procs == 1 {
                "1 proc".into()
            } else {
                format!("{running_procs} procs")
            };
            spans.push(StatusSpan {
                text: label,
                style: render::StyleState {
                    fg: Some(crate::theme::accent()),
                    bg: Some(status_bg),
                    ..render::StyleState::default()
                },
                priority: 2,
                group: true,
                ..StatusSpan::default()
            });
        }

        // Running agents.
        let running_agents = self.agents.len();
        if running_agents > 0 {
            let label = if running_agents == 1 {
                "1 agent".into()
            } else {
                format!("{running_agents} agents")
            };
            spans.push(StatusSpan {
                text: label,
                style: render::StyleState {
                    fg: Some(crate::theme::AGENT),
                    bg: Some(status_bg),
                    ..render::StyleState::default()
                },
                priority: 2,
                group: true,
                ..StatusSpan::default()
            });
        }

        // Right-aligned position.
        let position = self.compute_status_position();
        if let Some(p) = position {
            spans.push(StatusSpan {
                text: p.render(),
                style: render::StyleState {
                    fg: Some(crate::theme::muted()),
                    bg: Some(status_bg),
                    ..render::StyleState::default()
                },
                priority: 3,
                truncatable: true,
                align_right: true,
                ..StatusSpan::default()
            });
        }

        let (left, right) = spans_to_segments(&mut spans, width, status_bg);
        if let Some(bar) = self.ui.layer_mut::<ui::StatusBar>("status") {
            *bar = ui::StatusBar::new().with_bg(Style::bg(status_bg));
            bar.set_left(left);
            bar.set_right(right);
        }
    }

    /// Render a full-mode frame using the compositor pipeline.
    pub(super) fn render_normal(&mut self, agent_running: bool) {
        let _perf = crate::perf::begin("app:tick_compositor");
        self.update_spinner();

        let (term_w, term_h) = self.ui.terminal_size();
        let width = term_w as usize;
        let show_queued = agent_running || self.is_compacting();

        self.sync_transcript_pin();
        if self.transcript_window.is_pinned() {
            let (total, viewport) = self.transcript_dims();
            self.transcript_window.apply_pin(total, viewport);
        }
        let _visual = self.content_visual_range();

        let queued_owned: Vec<String> = if show_queued {
            self.queued_messages.clone()
        } else {
            Vec::new()
        };
        let prediction_owned: Option<String> = if show_queued {
            None
        } else {
            self.input_prediction.clone()
        };
        let queued: &[String] = &queued_owned;
        let prediction: Option<&str> = prediction_owned.as_deref();

        // Compute cursor ownership for this frame. Cmdline steals the
        // cursor; terminal-unfocused suppresses it; otherwise app_focus
        // decides prompt-vs-transcript.
        let has_prompt_cursor = !self.cmdline.active
            && self.term_focused
            && matches!(self.app_focus, crate::app::AppFocus::Prompt);
        let has_transcript_cursor = !self.cmdline.active
            && self.term_focused
            && matches!(self.app_focus, crate::app::AppFocus::Content);

        // ── Compute layout ──
        let natural_prompt_height = self.measure_prompt_height_pub(
            &self.input,
            width,
            queued,
            prediction,
            self.notification.is_some(),
        );
        self.layout = render::layout::LayoutState::compute(&render::layout::LayoutInput {
            term_width: term_w,
            term_height: term_h,
            prompt_height: natural_prompt_height,
        });
        let viewport_rows = self.layout.viewport_rows();
        let prompt_rect = self.layout.prompt;
        let prompt_height = prompt_rect.height;

        // ── Transcript ──
        let t_pad = self.transcript_gutters.pad_left;
        let transcript_rect = ui::Rect::new(0, t_pad, term_w.saturating_sub(t_pad), viewport_rows);
        let tdata = self.project_transcript_buffer(
            width,
            viewport_rows,
            self.transcript_window.scroll_top,
            self.settings.show_thinking,
        );
        self.transcript_window.scroll_top = tdata.clamped_scroll;

        // Compute transcript cursor.
        let tcursor = self.compute_transcript_cursor(
            width,
            viewport_rows,
            self.transcript_window.cursor_line,
            self.transcript_window.cursor_col,
            has_transcript_cursor,
            Some(&tdata.viewport),
        );
        self.transcript_window.cursor_line = tcursor.clamped_line;
        self.transcript_window.cursor_col = tcursor.clamped_col;

        let transcript_viewport = ui::WindowViewport::new(
            transcript_rect,
            self.transcript_gutters.content_width(term_w),
            tdata.total_rows,
            tdata.clamped_scroll,
            ui::ScrollbarState::new(tdata.scrollbar_col + t_pad, tdata.total_rows, viewport_rows),
        );
        self.transcript_viewport = Some(transcript_viewport);

        // Sync transcript WindowView from the projected buffer.
        if let Some(tv) = self
            .ui
            .layer_mut::<render::window_view::WindowView>("transcript")
        {
            tv.sync_from_buffer(
                self.transcript_projection.buf(),
                tdata.clamped_scroll as usize,
            );
            tv.set_soft_cursor(tcursor.soft_cursor);
            tv.set_viewport(Some(transcript_viewport));
        }

        // ── Prompt ──
        // Extract all immutable data first, then take the mutable btw borrow.
        let prev_input_scroll = self.prompt_input_scroll;
        let notification = self.notification.as_ref().cloned();
        let bar_info = render::prompt_data::BarInfo {
            model_label: Some(self.model.clone()),
            reasoning_effort: self.reasoning_effort,
            show_tokens: self.settings.show_tokens,
            context_tokens: self.context_tokens,
            context_window: self.context_window,
            show_cost: self.settings.show_cost,
            session_cost_usd: self.session_cost_usd,
        };

        let prompt_output = {
            let mut prompt_input = render::prompt_data::PromptInput {
                notification: notification.as_ref(),
                queued,
                stash: &self.input.stash,
                input: &self.input,
                prediction,
                width: term_w,
                height: prompt_height,
                has_prompt_cursor,
                prev_input_scroll,
                bar_info,
            };
            let input_buf = self
                .ui
                .buf_mut(self.input_display_buf)
                .expect("input_display_buf must be registered at startup");
            render::prompt_data::compute_prompt(&mut prompt_input, input_buf)
        };

        let chrome_rows = prompt_output.chrome_rows;
        let cursor = prompt_output.cursor;
        let cursor_style = prompt_output.cursor_style;
        let input_scroll = prompt_output.input_scroll;
        let input_viewport_data = prompt_output.input_viewport;

        self.prompt_input_scroll = input_scroll;

        let (prompt_input_rect, prompt_viewport) = if let Some(ref ivp) = input_viewport_data {
            let input_rect = ui::Rect::new(
                prompt_rect.top + ivp.top_row,
                0,
                prompt_rect.width,
                ivp.rows,
            );
            let viewport = ui::WindowViewport::new(
                input_rect,
                ivp.content_width,
                ivp.total_rows,
                ivp.scroll_top,
                ui::ScrollbarState::new(
                    prompt_rect.width.saturating_sub(1),
                    ivp.total_rows,
                    ivp.rows,
                ),
            );
            (input_rect, Some(viewport))
        } else {
            (
                ui::Rect::new(prompt_rect.bottom(), 0, prompt_rect.width, 0),
                None,
            )
        };
        self.prompt_viewport = prompt_viewport;

        if let Some(pv) = self
            .ui
            .layer_mut::<render::window_view::WindowView>("prompt")
        {
            pv.set_rows(chrome_rows);
            pv.set_viewport(None);
            pv.set_cursor(None, None);
        }
        {
            let scroll = self.prompt_input_scroll;
            let viewport = self.prompt_viewport;
            let input_buf_id = self.input_display_buf;
            let buf_snapshot = self.ui.buf(input_buf_id).cloned();
            if let (Some(pv), Some(buf)) = (
                self.ui
                    .layer_mut::<render::window_view::WindowView>("prompt_input"),
                buf_snapshot,
            ) {
                pv.sync_from_buffer(&buf, scroll);
                pv.set_viewport(viewport);
                pv.set_cursor(cursor, cursor_style);
            }
        }

        // ── Status bar ──
        self.refresh_status_bar();

        // ── Update layer rects and focus ──
        let status_rect = ui::Rect::new(term_h - 1, 0, term_w, 1);
        self.ui.set_layer_rect("transcript", transcript_rect);
        self.ui.set_layer_rect("prompt", prompt_rect);
        self.ui.set_layer_rect("prompt_input", prompt_input_rect);
        self.ui.set_layer_rect("status", status_rect);

        if self.ui.focused_float().is_none() {
            match self.app_focus {
                crate::app::AppFocus::Prompt => self.ui.focus_layer("prompt_input"),
                crate::app::AppFocus::Content => self.ui.focus_layer("transcript"),
            }
        }

        let mut stdout = std::io::stdout();
        let _ = self.ui.render(&mut stdout);

        // Clean up state.
    }

    // ── Content pane key handler — drives `Vim` over a readonly
    // transcript buffer so Normal / Visual / VisualLine motions and
    // yank behave exactly like they do in the prompt.
    fn handle_event_app_history(&mut self, ev: &Event) -> EventOutcome {
        let k = match ev {
            Event::Key(k) => *k,
            _ => return EventOutcome::Noop,
        };
        use crossterm::event::KeyModifiers as M;

        // Ctrl-C from a non-prompt pane returns focus to the prompt.
        if k.modifiers.contains(M::CONTROL) && matches!(k.code, KeyCode::Char('c')) {
            self.app_focus = crate::app::AppFocus::Prompt;
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

        // Shift+arrow / Shift+Home/End extends selection via the shared
        // keymap regardless of vim mode — the anchor logic lives in
        // one place (`ShiftSelection`). Vim's own v/V remain for users
        // who prefer them.
        if k.modifiers.contains(M::SHIFT)
            && matches!(
                k.code,
                KeyCode::Left
                    | KeyCode::Right
                    | KeyCode::Up
                    | KeyCode::Down
                    | KeyCode::Home
                    | KeyCode::End
            )
        {
            return self.handle_content_novim_key(k);
        }
        // Block-scoped bindings: the focused block gets first crack at
        // the key before buffer/window keymaps (nvim-style layering).
        if let Some(outcome) = self.dispatch_block_key(k) {
            return outcome;
        }

        if self.transcript_window.vim_enabled() {
            if self.handle_content_vim_key(k) {
                return EventOutcome::Redraw;
            }
            match (k.code, k.modifiers) {
                (KeyCode::Char('q'), M::NONE) => EventOutcome::Quit,
                _ => EventOutcome::Noop,
            }
        } else {
            self.handle_content_novim_key(k)
        }
    }

    /// Content-pane key handler when vim is disabled. Drives the same
    /// selection mechanism as the prompt: shift+movement extends via
    /// `ShiftSelection`; plain movement clears it; Ctrl-C / ⌘C copies.
    fn handle_content_novim_key(&mut self, k: KeyEvent) -> EventOutcome {
        use crate::keymap::{lookup, KeyAction, KeyContext};
        use crossterm::event::KeyModifiers as M;
        // Pull in the latest nav-only text (selectable chars) so cpos
        // stays valid across streaming updates.

        let rows = self.full_transcript_display_text(self.settings.show_thinking);
        let viewport = self.viewport_rows_estimate();
        self.transcript_window.resync(&rows, viewport);
        let ctx = KeyContext {
            buf_empty: self.transcript_window.edit_buf.buf.is_empty(),
            vim_non_insert: false,
            vim_enabled: false,
            agent_running: false,
            ghost_text_visible: false,
        };
        if let Some(action) = lookup(k.code, k.modifiers, &ctx) {
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
            match action {
                KeyAction::MoveLeft
                | KeyAction::MoveRight
                | KeyAction::MoveUp
                | KeyAction::MoveDown
                | KeyAction::MoveStartOfLine
                | KeyAction::MoveEndOfLine
                | KeyAction::MoveWordForward
                | KeyAction::MoveWordBackward => {
                    self.transcript_window.win_cursor.clear_anchor();
                }
                _ if extending => {
                    self.transcript_window
                        .win_cursor
                        .extend(self.transcript_window.cpos);
                }
                _ => {}
            }
            let delta: Option<isize> = match action {
                KeyAction::MoveUp | KeyAction::SelectUp => Some(-1),
                KeyAction::MoveDown | KeyAction::SelectDown => Some(1),
                _ => None,
            };
            if let Some(d) = delta {
                self.move_content_cursor_by_lines(d);
                self.sync_transcript_pin();
                return EventOutcome::Redraw;
            }
            let buf = self.transcript_window.edit_buf.buf.clone();
            let mv: Option<usize> = match action {
                KeyAction::MoveLeft | KeyAction::SelectLeft => Some(
                    crate::text_utils::prev_char_boundary(&buf, self.transcript_window.cpos),
                ),
                KeyAction::MoveRight | KeyAction::SelectRight => Some(
                    crate::text_utils::next_char_boundary(&buf, self.transcript_window.cpos),
                ),
                KeyAction::MoveStartOfLine | KeyAction::SelectStartOfLine => Some(
                    crate::text_utils::line_start(&buf, self.transcript_window.cpos),
                ),
                KeyAction::MoveEndOfLine | KeyAction::SelectEndOfLine => Some(
                    crate::text_utils::line_end(&buf, self.transcript_window.cpos),
                ),
                KeyAction::MoveWordForward | KeyAction::SelectWordForward => {
                    Some(crate::text_utils::word_forward_pos(
                        &buf,
                        self.transcript_window.cpos,
                        crate::text_utils::CharClass::Word,
                    ))
                }
                KeyAction::MoveWordBackward | KeyAction::SelectWordBackward => {
                    Some(crate::text_utils::word_backward_pos(
                        &buf,
                        self.transcript_window.cpos,
                        crate::text_utils::CharClass::Word,
                    ))
                }
                KeyAction::CopySelection => {
                    if let Some((s, e)) = self.transcript_window.selection_range(&rows) {
                        let s = crate::text_utils::snap(&buf, s);
                        let e = crate::text_utils::snap(&buf, e);
                        if s < e {
                            let copy =
                                self.copy_display_range(s, e, self.settings.show_thinking);
                            let _ = crate::app::commands::copy_to_clipboard(&copy);
                        }
                    }
                    return EventOutcome::Redraw;
                }
                _ => None,
            };
            if let Some(new_cpos) = mv {
                self.transcript_window.cpos = new_cpos;
                self.snap_transcript_cursor();

                let rows = self.full_transcript_display_text(self.settings.show_thinking);
                let viewport = self.viewport_rows_estimate();
                self.transcript_window.resync(&rows, viewport);
                self.sync_transcript_pin();
                return EventOutcome::Redraw;
            }
        }
        match (k.code, k.modifiers) {
            (KeyCode::Char('q'), M::NONE) => EventOutcome::Quit,
            _ => EventOutcome::Noop,
        }
    }

    /// Move the content-pane cursor by `delta` lines. Delegates to
    /// `TranscriptWindow::scroll_by_lines`, which reuses vim `j`/`k` so
    /// vertical motion shares one code path (with `curswant`) across
    /// mouse wheel, Ctrl-U/D, arrows and j/k.
    fn move_content_cursor_by_lines(&mut self, delta: isize) {
        let rows = self.full_transcript_display_text(self.settings.show_thinking);
        let viewport = self.viewport_rows_estimate();
        self.transcript_window
            .scroll_by_lines(delta, &rows, viewport);
        self.snap_transcript_cursor();
    }

    /// Build the transcript buffer, run `key` through the content-pane
    /// `Vim` instance, and mirror the resulting cursor / visual / yank
    /// state back onto our scroll + cursor. Returns `true` when vim
    /// consumed the key (caller should return `Redraw`).
    fn handle_content_vim_key(&mut self, k: KeyEvent) -> bool {
        let rows = self.full_transcript_display_text(self.settings.show_thinking);
        let viewport = self.viewport_rows_estimate();
        match self.transcript_window.handle_key(k, &rows, viewport) {
            None => false,
            Some(yanked) => {
                if let Some(raw) = yanked {
                    let copy = if let Some((s, e)) = self.transcript_window.kill_ring.source_range()
                    {
                        self.copy_display_range(s, e, self.settings.show_thinking)
                    } else {
                        raw
                    };
                    let _ = crate::app::commands::copy_to_clipboard(&copy);
                }
                self.snap_transcript_cursor();
                self.sync_transcript_pin();
                true
            }
        }
    }

    /// Compute the content pane's visual selection range (if any) in
    /// absolute transcript coordinates for the renderer to highlight.
    fn content_visual_range(&mut self) -> Option<render::ContentVisualRange> {
        if self.app_focus != crate::app::AppFocus::Content {
            return None;
        }
        let rows = self.full_transcript_display_text(self.settings.show_thinking);
        if rows.is_empty() {
            return None;
        }
        let buf = rows.join("\n");
        let cpos = self.transcript_window.compute_cpos(&rows);
        let (s, e, kind) = if let Some(vim) = self.transcript_window.vim.as_ref() {
            let kind = match vim.mode() {
                crate::vim::ViMode::Visual => render::ContentVisualKind::Char,
                crate::vim::ViMode::VisualLine => render::ContentVisualKind::Line,
                _ => return None,
            };
            let (s, e) = vim.visual_range(&buf, cpos)?;
            (s, e, kind)
        } else {
            let (s, e) = self.transcript_window.selection_range(&rows)?;
            (s, e, render::ContentVisualKind::Char)
        };
        // Byte offsets in display text → (line, display_col).
        let offset_to_line_col = |off: usize| -> (usize, usize) {
            let off = crate::text_utils::snap(&buf, off);
            let prefix = &buf[..off];
            let line = prefix.bytes().filter(|&b| b == b'\n').count();
            let line_start = prefix.rfind('\n').map(|p| p + 1).unwrap_or(0);
            (
                line,
                crate::text_utils::byte_to_cell(&buf[line_start..off], off - line_start),
            )
        };
        let (start_line_abs, start_col) = offset_to_line_col(s);
        let (end_line_abs, end_col) = offset_to_line_col(e);
        // Transcript-absolute line indices → viewport-relative so the
        // painter can walk `last_viewport_text` directly.
        let viewport = self.viewport_rows_estimate();
        let total = rows.len().min(u16::MAX as usize) as u16;
        let scroll = self.transcript_window.scroll_top;
        let geom = render::ViewportGeom::new(total, viewport, scroll);
        let view_top = geom.skip_from_top() as usize;
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
        self.layout.viewport_rows().max(1)
    }

    /// Single source of truth for whether the transcript viewport
    /// should be pinned. The viewport pins when the user has an
    /// active selection *or* is in the middle of a mouse drag — new
    /// agent output then flows into scrollback without shifting the
    /// rows the user is looking at. When the pin releases, scroll
    /// resumes its normal stuck-to-bottom behavior.
    fn sync_transcript_pin(&mut self) {
        let has_selection = self.transcript_window.win_cursor.anchor().is_some();
        let in_vim_visual = matches!(
            self.transcript_window.vim.as_ref().map(|v| v.mode()),
            Some(crate::vim::ViMode::Visual | crate::vim::ViMode::VisualLine)
        );
        let (total, viewport) = self.transcript_dims();
        let max_scroll = total.saturating_sub(viewport);
        let cursor_off_bottom = self.transcript_window.scroll_top < max_scroll
            || self.transcript_window.cursor_line + 1 < viewport;
        let want_pin =
            has_selection || in_vim_visual || self.mouse_drag_active || cursor_off_bottom;
        if want_pin {
            if !self.transcript_window.is_pinned() {
                self.transcript_window.pin(total);
            }
        } else {
            self.transcript_window.unpin();
        }
    }

    /// Snapshot app state into the Lua ops context and return the
    /// vim_mode + focused_window for callers that need them locally.
    pub(super) fn snapshot_lua_context(&mut self) -> (Option<String>, String) {
        let transcript_text = self.full_transcript_text(self.settings.show_thinking)
            .join("\n");
        let prompt_text = self.input.win.edit_buf.buf.clone();
        let focused_window = match self.app_focus {
            crate::app::AppFocus::Content => "transcript",
            crate::app::AppFocus::Prompt => "prompt",
        };
        let vim_mode = match self.app_focus {
            crate::app::AppFocus::Content => self
                .transcript_window
                .vim
                .as_ref()
                .map(|v| format!("{:?}", v.mode())),
            crate::app::AppFocus::Prompt => self.input.vim_mode().map(|m| format!("{m:?}")),
        };
        self.lua.set_context(
            Some(transcript_text),
            Some(prompt_text),
            Some(focused_window.to_string()),
            vim_mode.clone(),
        );
        self.snapshot_engine_context(false);
        (vim_mode, focused_window.to_string())
    }

    /// Populate engine-related Lua snapshot fields.
    pub(super) fn snapshot_engine_context(&self, is_busy: bool) {
        let session_dir = crate::session::dir_for(&self.session);
        self.lua.set_engine_context(crate::lua::EngineSnapshot {
            model: self.model.clone(),
            mode: self.mode.as_str().to_string(),
            reasoning_effort: self.reasoning_effort.label().to_string(),
            is_busy,
            session_cost: self.session_cost_usd,
            context_tokens: self.context_tokens,
            context_window: self.context_window,
            session_dir: session_dir.display().to_string(),
            session_id: self.session.id.clone(),
            session_title: self.session.title.clone(),
            session_cwd: self.cwd.clone(),
            session_created_at_ms: self.session.created_at_ms,
        });
        self.lua.set_history(self.history.clone());
    }

    /// Build the common `DialogConfig` used by all built-in dialogs
    /// (accent top rule, bar background, scrollbar colors, hints row).
    pub(super) fn builtin_dialog_config(
        &self,
        hint_text: Option<String>,
        dismiss_keys: Vec<(crossterm::event::KeyCode, crossterm::event::KeyModifiers)>,
    ) -> ui::DialogConfig {
        let accent = ui::grid::Style {
            fg: Some(crate::theme::accent()),
            ..Default::default()
        };
        let bg = ui::grid::Style {
            bg: Some(crossterm::style::Color::Black),
            ..Default::default()
        };
        let separator = ui::grid::Style {
            fg: Some(crate::theme::bar()),
            ..Default::default()
        };
        let hints = hint_text.map(|text| {
            let mut sb = ui::StatusBar::new().with_bg(bg);
            sb.set_left(vec![ui::StatusSegment::styled(
                text,
                ui::grid::Style { dim: true, ..bg },
            )]);
            sb
        });
        ui::DialogConfig {
            accent_style: accent,
            separator_style: separator,
            background_style: bg,
            scrollbar_track_style: ui::grid::Style {
                bg: Some(crate::theme::scrollbar_track()),
                ..Default::default()
            },
            scrollbar_thumb_style: ui::grid::Style {
                bg: Some(crate::theme::scrollbar_thumb()),
                ..Default::default()
            },
            dismiss_keys,
            hints,
        }
    }

    fn close_float(&mut self, win_id: ui::WinId) {
        self.ui.win_close(win_id);
    }

    /// Close the focused float if it doesn't block the agent (e.g. Ps,
    /// Permissions, Resume). Used before opening a blocking dialog so
    /// only one float is visible at a time. Fires `WinEvent::Dismiss`
    /// so the dialog's callbacks can flush any pending state (e.g.
    /// Permissions syncs its edits before close).
    pub(super) fn close_focused_non_blocking_float(&mut self) {
        let Some(win) = self.ui.focused_float() else {
            return;
        };
        if self.ui.float_config(win).is_some_and(|c| c.blocks_agent) {
            return;
        }
        let mut lua_invoke =
            |_handle: ui::LuaHandle, _payload: &ui::Payload| -> Vec<String> { Vec::new() };
        let _ = self.ui.dispatch_event(
            win,
            ui::WinEvent::Dismiss,
            ui::Payload::None,
            &mut lua_invoke,
        );
        self.apply_lua_ops();
    }

    /// True when the focused float pauses engine-event drain
    /// (Confirm / Question / Lua dialogs gate a pending tool call).
    pub(super) fn focused_float_blocks_agent(&self) -> bool {
        let Some(win) = self.ui.focused_float() else {
            return false;
        };
        self.ui.float_config(win).is_some_and(|c| c.blocks_agent)
    }

    /// Drain and apply all pending Lua ops (notifications, errors,
    /// commands, engine mutations). Call after any Lua handler dispatch.
    pub(super) fn apply_lua_ops(&mut self) {
        let ops = self.lua.drain_ops();
        self.apply_ops(ops);
    }

    /// Drive the `LuaTask` runtime and act on its outputs. Errors
    /// are queued via `NotifyError` internally; only
    /// `ToolComplete` (tool-as-task results) and `OpenDialog` (step
    /// iv) need app-side routing.
    pub(super) fn drive_lua_tasks(&mut self) {
        let outs = self.lua.drive_tasks();
        for out in outs {
            match out {
                crate::lua::TaskDriveOutput::ToolComplete {
                    request_id,
                    call_id,
                    content,
                    is_error,
                } => {
                    self.engine.send(protocol::UiCommand::PluginToolResult {
                        request_id,
                        call_id,
                        content,
                        is_error,
                    });
                }
                crate::lua::TaskDriveOutput::OpenDialog {
                    dialog_id, opts, ..
                } => {
                    if let Err(e) = super::dialogs::lua_dialog::open(self, dialog_id, opts) {
                        self.notify_error(format!("dialog.open: {e}"));
                        self.lua.resolve_dialog(dialog_id, mlua::Value::Nil);
                    }
                }
                crate::lua::TaskDriveOutput::Error(msg) => {
                    self.notify_error(msg);
                }
            }
        }
        self.apply_lua_ops();
    }

    pub(super) fn apply_ops(&mut self, ops: Vec<crate::app::ops::AppOp>) {
        for op in ops {
            match op {
                crate::app::ops::AppOp::Notify(msg) => self.notify(msg),
                crate::app::ops::AppOp::NotifyError(msg) => self.notify_error(msg),
                crate::app::ops::AppOp::RunCommand(line) => {
                    let _ = crate::api::cmd::run(self, &line);
                }
                crate::app::ops::AppOp::SetMode(mode_str) => {
                    if let Some(mode) = Mode::parse(&mode_str) {
                        self.set_mode(mode);
                    } else {
                        self.notify_error(format!("unknown mode: {mode_str}"));
                    }
                }
                crate::app::ops::AppOp::SetModel(model) => {
                    self.apply_model(&model);
                }
                crate::app::ops::AppOp::SetReasoningEffort(effort_str) => {
                    if let Some(effort) = ReasoningEffort::parse(&effort_str) {
                        self.set_reasoning_effort(effort);
                    } else {
                        self.notify_error(format!("unknown reasoning effort: {effort_str}"));
                    }
                }
                crate::app::ops::AppOp::Cancel => {
                    self.engine.send(UiCommand::Cancel);
                }
                crate::app::ops::AppOp::Compact(instructions) => {
                    if self.history.is_empty() {
                        self.notify_error("nothing to compact".into());
                    } else {
                        self.compact_history(instructions);
                    }
                }
                crate::app::ops::AppOp::Submit(text) => {
                    self.queued_messages.push(text);
                }
                crate::app::ops::AppOp::SetPromptSection(name, content) => {
                    self.prompt_sections.set(&name, content);
                }
                crate::app::ops::AppOp::RemovePromptSection(name) => {
                    self.prompt_sections.remove(&name);
                }
                crate::app::ops::AppOp::SetPermissionOverrides(_overrides) => {
                    // TODO: store overrides and include in StartTurn
                }
                crate::app::ops::AppOp::CloseFloat(win_id) => {
                    self.close_float(win_id);
                }
                crate::app::ops::AppOp::SyncPermissions {
                    session_entries,
                    workspace_rules,
                } => {
                    self.sync_permissions(session_entries, workspace_rules);
                }
                crate::app::ops::AppOp::AgentsBackToList {
                    detail_win,
                    initial_selected,
                } => {
                    self.close_float(detail_win);
                    super::dialogs::agents::open_list(self, initial_selected);
                }
                crate::app::ops::AppOp::AgentsOpenDetail {
                    list_win,
                    agent_id,
                    parent_selected,
                } => {
                    self.close_float(list_win);
                    super::dialogs::agents::open_detail(self, agent_id, parent_selected);
                }
                crate::app::ops::AppOp::AgentsListDismissed { win } => {
                    self.close_float(win);
                    self.refresh_agent_counts();
                }
                crate::app::ops::AppOp::ResolveConfirm {
                    choice,
                    message,
                    request_id,
                    call_id,
                    tool_name,
                } => {
                    let should_cancel =
                        self.resolve_confirm((choice, message), &call_id, request_id, &tool_name);
                    if should_cancel {
                        // Heavy cancel: flushes engine events, kills
                        // blocking subagents, emits TurnEnd, drops the
                        // active turn.
                        self.finish_turn(true);
                        self.agent = None;
                    }
                }
                crate::app::ops::AppOp::ConfirmBackTab {
                    win,
                    request_id,
                    call_id,
                    tool_name,
                    args,
                } => {
                    self.toggle_mode();
                    if self.permissions.decide(self.mode, &tool_name, &args, false)
                        == Decision::Allow
                    {
                        self.close_float(win);
                        self.set_active_status(&call_id, ToolStatus::Pending);
                        self.send_permission_decision(request_id, true, None);
                    }
                    // Otherwise: mode changed but dialog stays open so
                    // the user can still choose manually.
                }
                crate::app::ops::AppOp::LoadSession(id) => {
                    if let Some(loaded) = crate::session::load(&id) {
                        self.load_session(loaded);
                        self.restore_screen();
                        if let Some(tokens) = self.session.context_tokens {
                            self.context_tokens = Some(tokens);
                        }
                        self.finish_transcript_turn();
                        self.transcript_window.scroll_top = u16::MAX;
                    }
                }
                crate::app::ops::AppOp::RewindToBlock {
                    block_idx,
                    restore_vim_insert,
                } => {
                    if let Some(bidx) = block_idx {
                        self.cancel_agent();
                        self.agent = None;
                        if let Some((text, images)) = self.rewind_to(bidx) {
                            self.input.restore_from_rewind(text, images);
                        }
                        while self.engine.try_recv().is_ok() {}
                        self.save_session();
                    } else if restore_vim_insert {
                        self.input.set_vim_mode(crate::vim::ViMode::Insert);
                    }
                }
                crate::app::ops::AppOp::EngineAsk {
                    id,
                    system,
                    messages,
                    task,
                } => {
                    self.engine.send(UiCommand::EngineAsk {
                        id,
                        system,
                        messages,
                        task,
                    });
                }
                crate::app::ops::AppOp::SetGhostText(text) => {
                    self.input_prediction = Some(text);
                }
                crate::app::ops::AppOp::ClearGhostText => {
                    self.input_prediction = None;
                }
                crate::app::ops::AppOp::BufCreate { id } => {
                    self.ui.buf_create_with_id(
                        ui::BufId(id),
                        ui::buffer::BufCreateOpts {
                            buftype: ui::buffer::BufType::Scratch,
                            ..Default::default()
                        },
                    );
                }
                crate::app::ops::AppOp::BufSetLines { id, lines } => {
                    if let Some(buf) = self.ui.buf_mut(ui::BufId(id)) {
                        buf.set_all_lines(lines);
                    }
                }
                crate::app::ops::AppOp::WinOpenFloat {
                    buf_id,
                    title,
                    footer_items,
                    accent,
                } => {
                    let config = ui::FloatConfig {
                        title: Some(title),
                        border: ui::Border::Rounded,
                        ..Default::default()
                    };
                    if let Some(win_id) = self.ui.win_open_float(ui::BufId(buf_id), config) {
                        if let Some(fd) = self.ui.float_dialog_mut(win_id) {
                            fd.config_mut().accent_style = ui::Style {
                                fg: accent,
                                ..Default::default()
                            };
                            fd.config_mut().border_style = ui::Style {
                                fg: Some(crate::theme::accent()),
                                ..Default::default()
                            };
                            fd.config_mut().background_style = ui::Style {
                                bg: Some(crate::theme::bar()),
                                ..Default::default()
                            };
                            fd.config_mut().hint_left = Some("ESC close".into());
                            fd.config_mut().hint_right = if footer_items.is_empty() {
                                Some("j/k scroll".into())
                            } else {
                                Some("Enter select".into())
                            };
                            if !footer_items.is_empty() {
                                let items: Vec<ui::ListItem> = footer_items
                                    .iter()
                                    .enumerate()
                                    .map(|(i, label)| {
                                        ui::ListItem::plain(format!("{}. {}", i + 1, label))
                                    })
                                    .collect();
                                fd.set_footer_items(items);
                            }
                        }
                    }
                }
                crate::app::ops::AppOp::WinUpdate { id, title } => {
                    if let Some(win) = self.ui.win_mut(ui::WinId(id)) {
                        if let Some(t) = title {
                            win.set_title(Some(t));
                        }
                    }
                }
                crate::app::ops::AppOp::WinClose { id } => {
                    self.ui.win_close(ui::WinId(id));
                    self.ui.buf_delete(ui::BufId(id));
                }
                crate::app::ops::AppOp::ResolveToolResult {
                    request_id,
                    call_id,
                    content,
                    is_error,
                } => {
                    self.engine.send(protocol::UiCommand::PluginToolResult {
                        request_id,
                        call_id,
                        content,
                        is_error,
                    });
                }
                crate::app::ops::AppOp::ResolveLuaDialog {
                    dialog_id,
                    action,
                    option_index,
                    inputs,
                    on_select,
                } => {
                    if let Some(key) = on_select {
                        if let Ok(func) = self.lua.lua().registry_value::<mlua::Function>(&key) {
                            if let Err(e) = func.call::<()>(()) {
                                self.notify_error(format!("dialog on_select: {e}"));
                            }
                        }
                    }
                    let result = super::dialogs::lua_dialog::build_result(
                        self.lua.lua(),
                        &action,
                        option_index,
                        inputs,
                    );
                    match result {
                        Ok(v) => {
                            self.lua.resolve_dialog(dialog_id, v);
                        }
                        Err(e) => {
                            self.notify_error(format!("dialog resolve: {e}"));
                            self.lua.resolve_dialog(dialog_id, mlua::Value::Nil);
                        }
                    }
                }
            }
        }
    }

    fn transcript_dims(&mut self) -> (u16, u16) {
        let total = self.full_transcript_text(self.settings.show_thinking)
            .len() as u16;
        let viewport = self.viewport_rows_estimate();
        (total, viewport)
    }

    // ── Mouse event dispatch ─────────────────────────────────────────────
    fn handle_mouse(&mut self, me: MouseEvent) -> EventOutcome {
        use crossterm::event::MouseButton;
        if self.layout.hit_test(me.row, me.column) == render::HitRegion::Status {
            return EventOutcome::Noop;
        }
        // Drag + release drive tmux-style click-drag-copy. Works in
        // both the prompt and the content pane — each extends its own
        // buffer's selection anchor.
        match me.kind {
            MouseEventKind::Drag(MouseButton::Left) => {
                self.mouse_drag_active = true;
                self.extend_selection_to(me.row, me.column);
                self.sync_transcript_pin();
                return EventOutcome::Redraw;
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let dragged = self.mouse_drag_active && self.drag_on_scrollbar.is_none();
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
                self.drag_on_scrollbar = None;
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
                if let Some(vp) = self.prompt_viewport {
                    if vp.contains(me.row, me.column) {
                        self.app_focus = crate::app::AppFocus::Prompt;
                        if self.begin_scrollbar_drag_if_hit(
                            me.row,
                            me.column,
                            crate::app::AppFocus::Prompt,
                        ) {
                            return EventOutcome::Redraw;
                        }
                        self.drag_on_scrollbar = None;
                        if let Some(render::ViewportHit::Content { row, col }) =
                            vp.hit(me.row, me.column)
                        {
                            self.position_prompt_cursor_from_click(
                                row,
                                col,
                                vp.scroll_top as usize,
                                vp.content_width,
                            );
                        }
                        if double {
                            self.select_and_copy_word_in_prompt();
                        }
                        return EventOutcome::Redraw;
                    }
                }

                if matches!(
                    self.layout.hit_test(me.row, me.column),
                    render::HitRegion::Prompt | render::HitRegion::Status
                ) {
                    if self.app_focus != crate::app::AppFocus::Prompt {
                        self.app_focus = crate::app::AppFocus::Prompt;
                        return EventOutcome::Redraw;
                    }
                    return EventOutcome::Noop;
                }
                if !self.has_transcript_content(self.settings.show_thinking)
                {
                    return EventOutcome::Noop;
                }
                self.app_focus = crate::app::AppFocus::Content;
                // Route the event through the `Viewport`
                // recorded by the last paint: scrollbar clicks latch a
                // `ScrollbarDrag` so subsequent drag ticks keep
                // scrolling with the same thumb-relative offset;
                // content clicks position the cursor at already-clamped
                // (row, col).
                if self.begin_scrollbar_drag_if_hit(
                    me.row,
                    me.column,
                    crate::app::AppFocus::Content,
                ) {
                    return EventOutcome::Redraw;
                }
                self.drag_on_scrollbar = None;
                match self
                    .transcript_viewport
                    .and_then(|r| r.hit(me.row, me.column))
                {
                    Some(render::ViewportHit::Scrollbar) => {
                        // Unreachable: begin_scrollbar_drag_if_hit above
                        // handles Scrollbar hits. Kept for exhaustiveness.
                    }
                    Some(render::ViewportHit::Content { row, col }) => {
                        self.position_content_cursor_from_hit(row, col);
                    }
                    None => {
                    }
                }
                if double {
                    self.select_and_copy_word_in_content();
                    return EventOutcome::Redraw;
                }
                // Anchor the visual selection at the click position, not
                // wherever the cursor happened to be before — otherwise
                // a click selects everything between the previous
                // cursor and the click point.
                let anchor = self.transcript_window.cpos;
                if let Some(vim) = self.transcript_window.vim.as_mut() {
                    vim.begin_visual(crate::vim::ViMode::Visual, anchor);
                } else {
                    self.transcript_window.win_cursor.set_anchor(Some(anchor));
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
        if matches!(
            self.layout.hit_test(row, 0),
            render::HitRegion::Prompt
        ) {
            self.app_focus = crate::app::AppFocus::Prompt;
            self.scroll_prompt_by_lines(delta);
            return;
        }
        if !self.has_transcript_content(self.settings.show_thinking)
        {
            return;
        }
        self.app_focus = crate::app::AppFocus::Content;
        self.move_content_cursor_by_lines(delta);
    }

    fn scroll_prompt_by_lines(&mut self, delta: isize) {
        let buf = &self.input.win.edit_buf.buf;
        let new_pos = self
            .input
            .win
            .win_cursor
            .move_vertical(buf, self.input.win.cpos, delta);
        if new_pos != self.input.win.cpos {
            self.input.win.cpos = new_pos;
        }
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
        usable: u16,
    ) {
        let target_visual_row = rel_row as usize + scroll;
        let target_col = col as usize;
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
        self.input.win.cpos = cpos.min(buf.len());
        let want = col as usize;
        self.input.win.win_cursor.set_curswant(Some(want));
    }

    /// Extend the content-pane visual selection to the cell under the
    /// current drag position. Runs while the user holds mouse-1 and
    /// moves — each update moves the cursor inside vim Visual mode so
    /// the existing visual range widens or shrinks accordingly. Auto-
    /// scroll when the cursor is parked at an edge is handled by
    /// [`tick_drag_autoscroll`] on the frame tick, so holding the mouse
    /// still at the edge keeps extending the selection.
    fn extend_selection_to(&mut self, row: u16, col: u16) {
        if self.drag_on_scrollbar.is_some() {
            self.apply_scrollbar_drag(row);
            return;
        }
        match self.app_focus {
            crate::app::AppFocus::Content => {
                if let Some(region) = self.transcript_viewport {
                    let rel_row = row
                        .saturating_sub(region.rect.top)
                        .min(region.rect.height.saturating_sub(1));
                    let col = col.min(region.content_width.saturating_sub(1));
                    self.position_content_cursor_from_hit(rel_row, col);
                } else {
                    self.position_content_cursor_from_hit(row, col);
                }
            }
            crate::app::AppFocus::Prompt => {
                self.input.win.win_cursor.extend(self.input.win.cpos);
                if let Some(vp) = self.prompt_viewport {
                    if let Some(render::ViewportHit::Content { row: r, col: c }) = vp.hit(row, col)
                    {
                        self.position_prompt_cursor_from_click(
                            r,
                            c,
                            vp.scroll_top as usize,
                            vp.content_width,
                        );
                    }
                }
            }
        }
    }

    /// Frame-tick hook: if the user is mid-drag with the content cursor
    /// on the top or bottom row of the viewport, scroll a single line
    /// so the selection widens past the visible area. One-line-per-tick
    /// avoids the choppy feel of multi-line jumps; the main loop ramps
    /// its sleep interval down the longer the cursor stays at the edge,
    /// which is how acceleration happens.
    pub(super) fn tick_drag_autoscroll(&mut self) {
        if !self.mouse_drag_active
            || self.app_focus != crate::app::AppFocus::Content
            || self.drag_on_scrollbar.is_some()
        {
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
        let delta: isize = if self.transcript_window.cursor_line >= viewport.saturating_sub(1) {
            -1
        } else if self.transcript_window.cursor_line == 0 {
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

    /// Finalise a prompt drag-select: copy any non-empty selection to
    /// the clipboard and clear the anchor. A bare click (no drag) has
    /// anchor == cpos, so this is a no-op in that case.
    fn copy_prompt_selection_on_release(&mut self) {
        if let Some((s, e)) = self.input.selection_range() {
            let text: String = self.input.win.edit_buf.buf[s..e].to_string();
            let _ = crate::app::commands::copy_to_clipboard(&text);
        }
        self.input.win.win_cursor.clear_anchor();
    }

    /// Double-click on the prompt: select the word under the cursor
    /// (if any) via the shared `Buffer::select_word_at` helper, and
    /// copy it to the clipboard.
    fn select_and_copy_word_in_prompt(&mut self) {
        let cpos = self.input.win.cpos;
        if let Some((s, e)) = self.input.select_word_at(cpos) {
            let text = self.input.win.edit_buf.buf[s..e].to_string();
            let _ = crate::app::commands::copy_to_clipboard(&text);
        }
    }

    /// Double-click on the content pane: enter vim Visual over the
    /// word under the cursor and copy it.
    fn select_and_copy_word_in_content(&mut self) {
        let rows = self.full_transcript_display_text(self.settings.show_thinking);
        let cpos = self.transcript_window.compute_cpos(&rows);
        if let Some((s, e)) = self.transcript_window.select_word_at(&rows, cpos) {
            let text = self.transcript_window.edit_buf.buf[s..e].to_string();
            let _ = crate::app::commands::copy_to_clipboard(&text);
        }
        self.sync_transcript_pin();
    }

    /// Finalise a mouse interaction. Only copies when `dragged` is true —
    /// a bare click (no drag) exits Visual mode without copying, even
    /// though vim Visual selects the char under the cursor by default.
    fn copy_content_selection_and_clear(&mut self, dragged: bool) {
        if dragged {
            let rows = self.full_transcript_display_text(self.settings.show_thinking);
            let buf = rows.join("\n");
            let range = if let Some(vim) = self.transcript_window.vim.as_ref() {
                let cpos = self.transcript_window.compute_cpos(&rows);
                vim.visual_range(&buf, cpos)
            } else {
                self.transcript_window.selection_range(&rows)
            };
            if let Some((s, e)) = range {
                let s = crate::text_utils::snap(&buf, s);
                let e = crate::text_utils::snap(&buf, e);
                if s < e {
                    let copy = self.copy_display_range(s, e, self.settings.show_thinking);
                    let _ = crate::app::commands::copy_to_clipboard(&copy);
                }
            }
        }
        if let Some(vim) = self.transcript_window.vim.as_mut() {
            vim.set_mode(crate::vim::ViMode::Normal);
        } else {
            self.transcript_window.win_cursor.clear_anchor();
        }
        self.sync_transcript_pin();
    }

    /// Snap the viewport so the scrollbar thumb lands at screen row
    /// `screen_row`. Uses the `Viewport` recorded by the last
    /// paint — no re-measuring of the transcript on drag. Returns
    /// `true` when the region has a visible scrollbar and the jump was
    /// applied.
    /// If `(row, col)` lands on the scrollbar of `target`'s pane, latch
    /// a `ScrollbarDrag` that preserves the click's offset within the
    /// thumb, and snap the buffer's scroll so the thumb stays under the
    /// pointer. Returns `true` when the event was consumed.
    fn begin_scrollbar_drag_if_hit(
        &mut self,
        row: u16,
        col: u16,
        target: crate::app::AppFocus,
    ) -> bool {
        let Some(vp) = self.viewport_for(target) else {
            return false;
        };
        let Some(bar) = vp.scrollbar else {
            return false;
        };
        if !bar.contains(vp.rect, row, col) {
            return false;
        }
        self.drag_on_scrollbar = Some(target);
        self.apply_scrollbar_drag(row);
        true
    }

    /// Apply an in-flight `ScrollbarDrag` to the current pointer row:
    /// translate the thumb-relative anchor back into a thumb-top, then
    /// into a buffer scroll offset via the region's proportional map.
    fn apply_scrollbar_drag(&mut self, row: u16) {
        let Some(target) = self.drag_on_scrollbar else {
            return;
        };
        let Some(vp) = self.viewport_for(target) else {
            return;
        };
        let Some(bar) = vp.scrollbar else {
            return;
        };
        let max_thumb = bar.max_thumb_top();
        let rel_row = row.saturating_sub(vp.rect.top);
        let thumb_top = rel_row.min(max_thumb);
        let from_top = bar.scroll_from_top_for_thumb(thumb_top);
        match target {
            crate::app::AppFocus::Content => {
                self.transcript_window.scroll_top = from_top;
                let rows = self.full_transcript_display_text(self.settings.show_thinking);
                let viewport = self.viewport_rows_estimate();
                self.transcript_window
                    .reanchor_to_visible_row(&rows, viewport);
            }
            crate::app::AppFocus::Prompt => {
                self.prompt_input_scroll = from_top as usize;
            }
        }
    }

    /// Lookup the currently-painted viewport for a pane.
    fn viewport_for(&self, target: crate::app::AppFocus) -> Option<render::region::Viewport> {
        match target {
            crate::app::AppFocus::Content => self.transcript_viewport,
            crate::app::AppFocus::Prompt => self.prompt_viewport,
        }
    }

    /// Translate a click inside the transcript viewport into a
    /// (line, col) in the full transcript and jump the content cursor
    /// there. Reads geometry from the `Viewport` recorded at
    /// paint time so viewport rows, content width and scroll offset
    /// all match what the user is actually looking at. `rel_row` and
    /// `col` are already clamped against the region by the caller.
    fn position_content_cursor_from_hit(&mut self, rel_row: u16, abs_col: u16) {
        let rows = self.full_transcript_display_text(self.settings.show_thinking);
        if rows.is_empty() {
            return;
        }
        let Some(region) = self.transcript_viewport else {
            return;
        };
        let pad_left = self.transcript_gutters.pad_left;
        let display_col = abs_col.saturating_sub(pad_left) as usize;
        let viewport_rows = region.rect.height;
        let total = rows.len().min(u16::MAX as usize) as u16;
        let geom =
            render::ViewportGeom::new(total, viewport_rows, self.transcript_window.scroll_top);
        let line_idx = geom.line_of_row(rel_row).unwrap_or(total.saturating_sub(1)) as usize;
        let line_idx = line_idx.min(rows.len() - 1);
        let snapped =
            self.snap_col_to_selectable(line_idx, display_col, self.settings.show_thinking);
        self.transcript_window
            .jump_to_line_col(&rows, line_idx, snapped, viewport_rows);
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
        let target = match self.app_focus {
            crate::app::AppFocus::Prompt => crate::app::AppFocus::Content,
            crate::app::AppFocus::Content => crate::app::AppFocus::Prompt,
        };
        if target == crate::app::AppFocus::Content
            && !self.has_transcript_content(self.settings.show_thinking)
        {
            return;
        }
        self.app_focus = target;
        if self.app_focus == crate::app::AppFocus::Content {
            self.refocus_content();
        }
    }

    /// Warm up the content pane on focus switch: mount the transcript,
    /// clamp cpos into range, sync cursor line/col. Without this, a
    /// resumed session has stale/zero state and the first key press
    /// is a no-op until the user triggers a click-to-position.
    fn refocus_content(&mut self) {
        let rows = self.full_transcript_display_text(self.settings.show_thinking);
        let viewport = self.viewport_rows_estimate();
        self.transcript_window.refocus(&rows, viewport);
        self.snap_transcript_cursor();
    }

    /// Snap the transcript cursor to the nearest selectable cell.
    /// Called after every cursor motion to skip non-selectable gutters
    /// and padding now that the cursor operates in display-text space.
    fn snap_transcript_cursor(&mut self) {
        let rows = self.full_transcript_display_text(self.settings.show_thinking);
        let snapped = self.snap_cpos_to_selectable(
            &rows,
            self.transcript_window.cpos,
            self.settings.show_thinking,
        );
        if snapped != self.transcript_window.cpos {
            self.transcript_window.cpos = snapped;
            let viewport = self.viewport_rows_estimate();
            self.transcript_window.resync(&rows, viewport);
        }
    }

    /// Determine which block the content cursor is currently on, if any.
    /// Derives the absolute row from `cpos` (byte offset in the display
    /// buffer), then looks up the snapshot's `block_of_row`.
    fn focused_block_id(&mut self) -> Option<render::BlockId> {
        let tw = self.transcript_width() as u16;
        let snap = self.transcript.snapshot(tw, self.settings.show_thinking);
        if snap.rows.is_empty() {
            return None;
        }
        let row = self.transcript_window.cursor_abs_row();
        snap.block_of_row.get(row).copied().flatten()
    }

    /// Try to handle a key as a block-scoped binding. Returns `Some` if
    /// the key was consumed, `None` to fall through to buffer/window
    /// keymaps.
    fn dispatch_block_key(&mut self, k: KeyEvent) -> Option<EventOutcome> {
        use crossterm::event::KeyModifiers as M;
        if k.modifiers != M::NONE {
            return None;
        }
        let block_id = self.focused_block_id()?;
        let is_tool = matches!(
            self.transcript.block(block_id),
            Some(render::Block::ToolCall { .. })
        );
        if !is_tool {
            return None;
        }
        match k.code {
            KeyCode::Char('e') => {
                let vs = self.block_view_state(block_id);
                let next = match vs {
                    render::ViewState::Expanded => render::ViewState::Collapsed,
                    _ => render::ViewState::Expanded,
                };
                self.set_block_view_state(block_id, next);
                Some(EventOutcome::Redraw)
            }
            _ => None,
        }
    }
    // ── Cmdline (:) ───────────────────────────────────────────────────

    pub fn open_cmdline(&mut self) {
        self.cmdline.open();
    }

    fn handle_cmdline_key(&mut self, k: KeyEvent) -> bool {
        use crossterm::event::KeyModifiers as M;
        if !self.cmdline.active {
            return false;
        }
        let mut needs_completer_update = false;
        match (k.code, k.modifiers) {
            (KeyCode::Esc, _) | (KeyCode::Char('c'), M::CONTROL) => {
                self.cmdline.close();
            }
            (KeyCode::Enter, _) => {
                let line = self.cmdline.submit();
                if !line.is_empty() {
                    let action = super::commands::run_command(self, &format!(":{line}"));
                    match action {
                        CommandAction::Quit => return true,
                        CommandAction::CancelAndClear => {
                            self.reset_session();
                            self.agent = None;
                        }
                        CommandAction::Compact { instructions } => {
                            if self.history.is_empty() {
                                self.notify_error("nothing to compact".into());
                            } else {
                                self.compact_history(instructions);
                            }
                        }
                        CommandAction::Exec(rx, kill) => {
                            self.exec_rx = Some(rx);
                            self.exec_kill = Some(kill);
                        }
                        CommandAction::Continue => {}
                    }
                }
            }
            (KeyCode::Tab, _)
            | (KeyCode::Char('j'), M::CONTROL)
            | (KeyCode::Char('n'), M::CONTROL) => {
                if self.cmdline.completer.is_some() {
                    if let Some(ref mut comp) = self.cmdline.completer {
                        comp.move_up();
                    }
                } else {
                    let lua_cmds = self.lua.command_names();
                    self.cmdline.update_completer(&lua_cmds);
                }
                self.cmdline.apply_selected_completion();
            }
            (KeyCode::BackTab, _)
            | (KeyCode::Char('k'), M::CONTROL)
            | (KeyCode::Char('p'), M::CONTROL) => {
                if self.cmdline.completer.is_some() {
                    if let Some(ref mut comp) = self.cmdline.completer {
                        comp.move_down();
                    }
                } else {
                    let lua_cmds = self.lua.command_names();
                    self.cmdline.update_completer(&lua_cmds);
                }
                self.cmdline.apply_selected_completion();
            }
            (KeyCode::Backspace, _) => {
                self.cmdline.backspace();
                if self.cmdline.buf.is_empty() {
                    self.cmdline.close();
                } else {
                    needs_completer_update = true;
                }
            }
            (KeyCode::Delete, _) => {
                self.cmdline.delete();
                needs_completer_update = true;
            }
            (KeyCode::Left, _) => {
                self.cmdline.move_left();
            }
            (KeyCode::Right, _) => {
                self.cmdline.move_right();
            }
            (KeyCode::Up, _) => {
                self.cmdline.history_up();
            }
            (KeyCode::Down, _) => {
                self.cmdline.history_down();
            }
            (KeyCode::Home, _) | (KeyCode::Char('a'), M::CONTROL) => {
                self.cmdline.move_start();
            }
            (KeyCode::End, _) | (KeyCode::Char('e'), M::CONTROL) => {
                self.cmdline.move_end();
            }
            (KeyCode::Char('w'), M::CONTROL) => {
                self.cmdline.delete_word_back();
                if self.cmdline.buf.is_empty() {
                    self.cmdline.close();
                } else {
                    needs_completer_update = true;
                }
            }
            (KeyCode::Char('u'), M::CONTROL) => {
                self.cmdline.buf.clear();
                self.cmdline.cursor = 0;
                self.cmdline.completer = None;
            }
            (KeyCode::Char(ch), M::NONE | M::SHIFT) => {
                self.cmdline.insert_char(ch);
                needs_completer_update = true;
            }
            _ => {}
        }
        if needs_completer_update && self.cmdline.completer.is_some() {
            let lua_cmds = self.lua.command_names();
            self.cmdline.update_completer(&lua_cmds);
        }
        false
    }
}

/// Max inter-key gap between `Ctrl-W` and its follow-up key.
const PANE_CHORD_WINDOW: Duration = Duration::from_millis(750);
