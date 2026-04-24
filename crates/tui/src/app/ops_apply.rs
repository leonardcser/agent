//! AppOp reducer: applies queued UiOp / DomainOp effects emitted by
//! Rust/Lua callbacks. Runs once per tick after every handler drains.

use super::*;

impl App {
    pub(super) fn apply_ops(&mut self, ops: Vec<crate::app::ops::AppOp>) {
        use crate::app::ops::AppOp;
        for op in ops {
            match op {
                AppOp::Ui(u) => self.apply_ui_op(u),
                AppOp::Domain(d) => self.apply_domain_op(d),
            }
        }
    }

    fn apply_ui_op(&mut self, op: crate::app::ops::UiOp) {
        use crate::app::ops::UiOp;
        match op {
            UiOp::Notify(msg) => self.notify(msg),
            UiOp::NotifyError(msg) => self.notify_error(msg),
            UiOp::CloseFloat(win_id) => {
                self.close_float(win_id);
            }
            UiOp::SetGhostText(text) => {
                self.input_prediction = Some(text);
            }
            UiOp::ClearGhostText => {
                self.input_prediction = None;
            }
            UiOp::BufCreate { id } => {
                // A BufCreate for an id that already exists means the
                // allocator aliased into another buffer's id space.
                // Loud notify instead of silently stomping.
                if let Err(clash) = self.ui.buf_create_with_id(
                    ui::BufId(id),
                    ui::buffer::BufCreateOpts {
                        buftype: ui::buffer::BufType::Scratch,
                        ..Default::default()
                    },
                ) {
                    self.notify_error(format!("buf.create: id {} already in use", clash.0));
                }
            }
            UiOp::BufSetLines { id, lines } => {
                if let Some(buf) = self.ui.buf_mut(ui::BufId(id)) {
                    buf.set_all_lines(lines);
                }
            }
            UiOp::BufAddHighlight {
                id,
                line,
                col_start,
                col_end,
                fg,
                bold,
                italic,
                dim,
            } => {
                if let Some(buf) = self.ui.buf_mut(ui::BufId(id)) {
                    if line < buf.line_count() {
                        buf.add_highlight(
                            line,
                            col_start,
                            col_end,
                            ui::buffer::SpanStyle {
                                fg,
                                bg: None,
                                bold,
                                dim,
                                italic,
                            },
                        );
                    }
                }
            }
            UiOp::WinBindLuaKeymap {
                win,
                key,
                callback_id,
            } => {
                if let Some(ui::Callback::Lua(ui::LuaHandle(old))) =
                    self.ui
                        .win_set_keymap(win, key, ui::Callback::Lua(ui::LuaHandle(callback_id)))
                {
                    self.lua.remove_callback(old);
                }
            }
            UiOp::WinBindLuaEvent {
                win,
                event,
                callback_id,
            } => {
                self.ui
                    .win_on_event(win, event, ui::Callback::Lua(ui::LuaHandle(callback_id)));
            }
            UiOp::PickerSetSelected { win, index } => {
                if let Some(p) = self.ui.picker_mut(win) {
                    p.set_selected(index);
                }
            }
            UiOp::OpenLuaDialog { task_id, opts } => {
                let value = match crate::lua::ui_ops::open_dialog(self, opts) {
                    Ok(win_id) => self
                        .lua
                        .lua()
                        .create_table()
                        .and_then(|t| {
                            t.set("win_id", win_id.0)?;
                            Ok(mlua::Value::Table(t))
                        })
                        .unwrap_or(mlua::Value::Nil),
                    Err(e) => {
                        self.notify_error(format!("dialog.open: {e}"));
                        mlua::Value::Nil
                    }
                };
                self.lua.resolve_external(task_id, value);
            }
            UiOp::OpenLuaPicker { task_id, opts } => {
                let value = match crate::lua::ui_ops::open_picker(self, opts) {
                    Ok(win_id) => self
                        .lua
                        .lua()
                        .create_table()
                        .and_then(|t| {
                            t.set("win_id", win_id.0)?;
                            Ok(mlua::Value::Table(t))
                        })
                        .unwrap_or(mlua::Value::Nil),
                    Err(e) => {
                        self.notify_error(format!("picker.open: {e}"));
                        mlua::Value::Nil
                    }
                };
                self.lua.resolve_external(task_id, value);
            }
        }
    }

    fn apply_domain_op(&mut self, op: crate::app::ops::DomainOp) {
        use crate::app::ops::DomainOp;
        match op {
            DomainOp::RunCommand(line) => match crate::api::cmd::run(self, &line) {
                crate::app::CommandAction::Quit => {
                    self.pending_quit = true;
                }
                crate::app::CommandAction::CancelAndClear => {
                    self.reset_session();
                    self.agent = None;
                }
                crate::app::CommandAction::Compact { instructions } => {
                    if self.history.is_empty() {
                        self.notify_error("nothing to compact".into());
                    } else {
                        self.compact_history(instructions);
                    }
                }
                crate::app::CommandAction::Exec(rx, kill) => {
                    self.exec_rx = Some(rx);
                    self.exec_kill = Some(kill);
                }
                crate::app::CommandAction::Continue => {}
            },
            DomainOp::SetMode(mode_str) => {
                if let Some(mode) = Mode::parse(&mode_str) {
                    self.set_mode(mode);
                } else {
                    self.notify_error(format!("unknown mode: {mode_str}"));
                }
            }
            DomainOp::SetModel(model) => {
                self.apply_model(&model);
            }
            DomainOp::SetReasoningEffort(effort_str) => {
                if let Some(effort) = ReasoningEffort::parse(&effort_str) {
                    self.set_reasoning_effort(effort);
                } else {
                    self.notify_error(format!("unknown reasoning effort: {effort_str}"));
                }
            }
            DomainOp::Cancel => {
                self.engine.send(UiCommand::Cancel);
            }
            DomainOp::Compact(instructions) => {
                if self.history.is_empty() {
                    self.notify_error("nothing to compact".into());
                } else {
                    self.compact_history(instructions);
                }
            }
            DomainOp::Submit(text) => {
                self.queued_messages.push(text);
            }
            DomainOp::SetPromptSection(name, content) => {
                self.prompt_sections.set(&name, content);
            }
            DomainOp::RemovePromptSection(name) => {
                self.prompt_sections.remove(&name);
            }
            DomainOp::SyncPermissions {
                session_entries,
                workspace_rules,
            } => {
                self.sync_permissions(session_entries, workspace_rules);
            }
            DomainOp::ResolveConfirm {
                choice,
                message,
                request_id,
                call_id,
                tool_name,
            } => {
                let should_cancel =
                    self.resolve_confirm((choice, message), &call_id, request_id, &tool_name);
                if should_cancel {
                    // Heavy cancel: flushes engine events, kills blocking
                    // subagents, emits TurnEnd, drops the active turn.
                    self.finish_turn(true);
                    self.agent = None;
                }
            }
            DomainOp::ConfirmBackTab {
                win,
                request_id,
                call_id,
                tool_name,
                args,
            } => {
                self.toggle_mode();
                if self.permissions.decide(self.mode, &tool_name, &args, false) == Decision::Allow {
                    self.close_float(win);
                    self.set_active_status(&call_id, ToolStatus::Pending);
                    self.send_permission_decision(request_id, true, None);
                }
                // Otherwise: mode changed but dialog stays open so the
                // user can still choose manually.
            }
            DomainOp::LoadSession(id) => {
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
            DomainOp::DeleteSession(id) => {
                if id != self.session.id {
                    crate::session::delete(&id);
                }
            }
            DomainOp::KillAgent(pid) => {
                engine::registry::kill_agent(pid);
            }
            DomainOp::RewindToBlock {
                block_idx,
                restore_vim_insert,
            } => {
                if let Some(bidx) = block_idx {
                    if self.agent.is_some() {
                        self.cancel_agent();
                        self.agent = None;
                    }
                    if let Some((text, images)) = self.rewind_to(bidx) {
                        self.input.restore_from_rewind(text, images);
                    }
                    while self.engine.try_recv().is_ok() {}
                    self.save_session();
                } else if restore_vim_insert {
                    self.input.set_vim_mode(crate::vim::ViMode::Insert);
                }
            }
            DomainOp::EngineAsk {
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
            DomainOp::ResolveToolResult {
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
            DomainOp::KillProcess(id) => {
                let registry = self.engine.processes.clone();
                tokio::spawn(async move {
                    let _ = registry.stop(&id).await;
                });
            }
            DomainOp::YankBlockAtCursor => {
                let abs_row = self.transcript_window.cursor_abs_row();
                if let Some(text) = self.block_text_at_row(abs_row, self.settings.show_thinking) {
                    let _ = super::commands::copy_to_clipboard(&text);
                    self.notify("block copied".into());
                } else {
                    self.notify_error("no block at cursor".into());
                }
            }
        }
    }
}
