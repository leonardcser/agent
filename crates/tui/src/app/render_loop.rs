//! Per-frame render loop: projects transcript/prompt/status into the
//! compositor layers and syncs overlay floats (completer, notifications).

use super::*;

impl App {
    pub(super) fn render_normal(&mut self, agent_running: bool) {
        let _perf = crate::perf::begin("app:tick_compositor");
        self.update_spinner();

        let (term_w, term_h) = self.ui.terminal_size();
        let width = term_w as usize;
        let show_queued = agent_running || self.is_compacting();

        self.adjust_tail_scroll();

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

        let (has_prompt_cursor, has_transcript_cursor) = self.compute_cursor_ownership();

        // ── Layout ──
        let natural_prompt_height =
            self.measure_prompt_height(&self.input, width, queued, prediction);
        self.layout = content::layout::LayoutState::compute(&content::layout::LayoutInput {
            term_width: term_w,
            term_height: term_h,
            prompt_height: natural_prompt_height,
        });
        let viewport_rows = self.layout.viewport_rows();
        let prompt_rect = self.layout.prompt;
        let prompt_height = prompt_rect.height;

        let transcript_rect =
            self.sync_transcript_layer(term_w, width, viewport_rows, has_transcript_cursor);
        let prompt_input_rect = self.sync_prompt_layer(
            term_w,
            prompt_rect,
            prompt_height,
            queued,
            prediction,
            has_prompt_cursor,
        );
        // Freeze the live-turn timer + spinner whenever a blocking
        // dialog (Confirm, Question, …) is up so the user doesn't see
        // wall-clock seconds tick by while the agent is actually parked
        // waiting on input.
        self.working.set_paused(self.focused_float_blocks_agent());
        self.refresh_status_bar();

        self.finalize_layer_rects(
            transcript_rect,
            prompt_rect,
            prompt_input_rect,
            term_w,
            term_h,
        );

        self.sync_completer_float();
        self.sync_notification_float(prompt_rect);

        let mut stdout = std::io::stdout();
        let _ = self.ui.render(&mut stdout);
    }

    /// Freeze transcript tail-follow during an active selection / vim
    /// visual / mouse drag so streaming rows grow into scrollback
    /// rather than shifting the user's selection. Otherwise, when
    /// `follow_tail` is on, snap `scroll_top` to the bottom so content
    /// appended below stays visible across viewport resizes.
    fn adjust_tail_scroll(&mut self) {
        let has_selection = self.transcript_window.win_cursor.anchor().is_some();
        let in_vim_visual = matches!(
            self.transcript_window.vim.as_ref().map(|v| v.mode()),
            Some(crate::vim::ViMode::Visual | crate::vim::ViMode::VisualLine)
        );
        let freeze = has_selection || in_vim_visual || self.mouse_drag_active;
        if !freeze && self.transcript_window.follow_tail {
            self.transcript_window.scroll_top = u16::MAX;
        }
    }

    /// Cmdline steals the cursor (via its compositor layer);
    /// terminal-unfocused suppresses; otherwise `app_focus` decides
    /// prompt-vs-transcript ownership for this frame.
    fn compute_cursor_ownership(&self) -> (bool, bool) {
        let cmdline_active = self.cmdline_win.is_some();
        let has_prompt_cursor = !cmdline_active
            && self.term_focused
            && matches!(self.app_focus, crate::app::AppFocus::Prompt);
        let has_transcript_cursor = !cmdline_active
            && self.term_focused
            && matches!(self.app_focus, crate::app::AppFocus::Content);
        (has_prompt_cursor, has_transcript_cursor)
    }

    /// Project the transcript into its buffer, compute the soft cursor,
    /// and sync the `transcript` WindowView layer (rows + selection
    /// overlays + viewport). Returns the transcript layer's rect.
    fn sync_transcript_layer(
        &mut self,
        term_w: u16,
        width: usize,
        viewport_rows: u16,
        has_transcript_cursor: bool,
    ) -> ui::Rect {
        let t_pad = self.transcript_gutters.pad_left;
        let transcript_rect = ui::Rect::new(0, t_pad, term_w.saturating_sub(t_pad), viewport_rows);
        let tdata = self.project_transcript_buffer(
            width,
            viewport_rows,
            self.transcript_window.scroll_top,
            self.settings.show_thinking,
        );
        self.transcript_window.scroll_top = tdata.clamped_scroll;

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

        let transcript_selection =
            self.transcript_selection_highlights(tdata.clamped_scroll, viewport_rows);

        if let Some(tv) = self
            .ui
            .layer_mut::<content::window_view::WindowView>("transcript")
        {
            tv.sync_from_buffer(
                self.transcript_projection.buf(),
                tdata.clamped_scroll as usize,
            );
            for (line, col_start, col_end) in &transcript_selection {
                tv.add_highlight(
                    *line,
                    *col_start,
                    *col_end,
                    ui::grid::Style::bg(crate::theme::selection_bg()),
                );
            }
            tv.set_soft_cursor(tcursor.soft_cursor);
            tv.set_viewport(Some(transcript_viewport));
        }

        transcript_rect
    }

    /// Compute prompt chrome (queued / stash / bars) + input area, then
    /// sync both the `prompt` (chrome rows) and `prompt_input` (edit
    /// buffer) WindowView layers. Returns the `prompt_input` layer rect
    /// for later `set_layer_rect`.
    fn sync_prompt_layer(
        &mut self,
        term_w: u16,
        prompt_rect: ui::Rect,
        prompt_height: u16,
        queued: &[String],
        prediction: Option<&str>,
        has_prompt_cursor: bool,
    ) -> ui::Rect {
        let bar_info = content::prompt_data::BarInfo {
            model_label: Some(self.model.clone()),
            reasoning_effort: self.reasoning_effort,
            show_tokens: self.settings.show_tokens,
            context_tokens: self.context_tokens,
            context_window: self.context_window,
            show_cost: self.settings.show_cost,
            session_cost_usd: self.session_cost_usd,
        };

        let prompt_output = {
            let mut prompt_input = content::prompt_data::PromptInput {
                queued,
                stash: &self.input.stash,
                input: &self.input,
                prediction,
                width: term_w,
                height: prompt_height,
                has_prompt_cursor,
                bar_info,
            };
            let input_buf = self
                .ui
                .buf_mut(self.input_display_buf)
                .expect("input_display_buf must be registered at startup");
            content::prompt_data::compute_prompt(&mut prompt_input, input_buf)
        };

        let chrome_rows = prompt_output.chrome_rows;
        let cursor = prompt_output.cursor;
        let cursor_style = prompt_output.cursor_style;
        // Write the renderer's clamped scroll_top back into the Window
        // (handles the case where the prompt buffer shrank and the old
        // scroll_top is now beyond max_off, or vim `zz` requested a
        // recenter via `pending_recenter`).
        if let Some(ref ivp) = prompt_output.input_viewport {
            self.input.win.scroll_top = ivp.scroll_top;
        }
        self.input.win.pending_recenter = false;
        self.input.win.last_render_cpos = Some(self.input.win.cpos);

        let (prompt_input_rect, prompt_viewport) =
            if let Some(ref ivp) = prompt_output.input_viewport {
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
            .layer_mut::<content::window_view::WindowView>("prompt")
        {
            pv.set_rows(chrome_rows);
            pv.set_viewport(None);
            pv.set_cursor(None, None);
        }

        let viewport = self.prompt_viewport;
        let input_buf_id = self.input_display_buf;
        let buf_snapshot = self.ui.buf(input_buf_id).cloned();
        if let (Some(pv), Some(buf)) = (
            self.ui
                .layer_mut::<content::window_view::WindowView>("prompt_input"),
            buf_snapshot,
        ) {
            // `compute_input_area` already wrote only the visible slice
            // of wrapped lines into `buf`, with highlights indexed
            // 0..content_rows. Passing the absolute scroll here would
            // make `BufferView::draw_content` skip past the slice and
            // render empty rows — the buffer offset must be 0, while
            // the *viewport* above still carries the real scroll so
            // the scrollbar tracks correctly.
            pv.sync_from_buffer(&buf, 0);
            pv.set_viewport(viewport);
            pv.set_cursor(cursor, cursor_style);
        }

        prompt_input_rect
    }

    fn finalize_layer_rects(
        &mut self,
        transcript_rect: ui::Rect,
        prompt_rect: ui::Rect,
        prompt_input_rect: ui::Rect,
        term_w: u16,
        term_h: u16,
    ) {
        let status_rect = ui::Rect::new(term_h - 1, 0, term_w, 1);
        self.ui.set_layer_rect("transcript", transcript_rect);
        self.ui.set_layer_rect("prompt", prompt_rect);
        self.ui.set_layer_rect("prompt_input", prompt_input_rect);
        self.ui.set_layer_rect("status", status_rect);
        // Publish split-window rects so `Placement::DockedAbove(WinId)`
        // floats (prompt-docked pickers) can resolve their anchor.
        self.ui.set_window_rect(ui::PROMPT_WIN, prompt_rect);
        self.ui.set_window_rect(ui::TRANSCRIPT_WIN, transcript_rect);

        if self.ui.focused_float().is_none() {
            match self.app_focus {
                crate::app::AppFocus::Prompt => self.ui.focus_layer("prompt_input"),
                crate::app::AppFocus::Content => self.ui.focus_layer("transcript"),
            }
        }
    }

    // ── Completer float ────────────────────────────────────────────
    //
    // Mirrors the active `CompleterSession` into a `ui::Picker`
    // compositor float. The session (`PromptState.completer`) holds both
    // the matcher model *and* the `picker_win: Option<WinId>` — one
    // owner, one lifecycle. Matches the shape a future Lua completer
    // plugin would hold in its own local state.
    //
    // `focusable = false` ensures keys keep flowing to the prompt,
    // driving `completer_bridge::handle_completer_event`.
    fn sync_completer_float(&mut self) {
        // Drain any Picker floats that were orphaned when their session
        // ended (session held the WinId; when it dropped, it queued the
        // WinId here for out-of-band close).
        for win in std::mem::take(&mut self.input.pending_picker_close) {
            self.close_float(win);
        }

        let (max_rows, selected, items, existing_win) = match self.input.completer.as_ref() {
            Some(session) => {
                let prefix = match session.kind {
                    crate::completer::CompleterKind::Command => "/",
                    crate::completer::CompleterKind::File => "./",
                    crate::completer::CompleterKind::CommandArg => "",
                };
                let items: Vec<ui::PickerItem> = session
                    .results
                    .iter()
                    .map(|r| {
                        let item_prefix = if r.ansi_color.is_some() {
                            "● "
                        } else {
                            prefix
                        };
                        let mut it = ui::PickerItem::new(r.label.clone()).with_prefix(item_prefix);
                        if let Some(desc) = r.description.as_deref() {
                            it = it.with_description(desc);
                        }
                        if let Some(c) = r.ansi_color {
                            it = it.with_accent(crossterm::style::Color::AnsiValue(c));
                        }
                        it
                    })
                    .collect();
                (
                    session.max_visible_rows(),
                    session.selected,
                    items,
                    session.picker_win,
                )
            }
            None => return,
        };

        // Open once and reuse — `Placement::DockedAbove(PROMPT_WIN)` resizes
        // the picker's rect in-place each frame from the picker's
        // `natural_height()` (clamped by `max_height`). Closing and
        // reopening the float on every filter change forces a full-screen
        // redraw, which makes the cursor visibly jump around.
        let open_win = match existing_win {
            Some(win) => Some(win),
            None => {
                let config = ui::FloatConfig {
                    placement: ui::Placement::DockedAbove {
                        target: ui::PROMPT_WIN,
                        max_height: ui::Constraint::Fixed(max_rows as u16),
                    },
                    border: ui::Border::None,
                    title: None,
                    // Below the default float zindex (50) so dialogs (help,
                    // confirm, …) overlay the completer picker.
                    zindex: 30,
                    focusable: false,
                    blocks_agent: false,
                };
                let style = ui::PickerStyle {
                    selected_fg: ui::Style {
                        fg: Some(crate::theme::accent()),
                        ..Default::default()
                    },
                    unselected_fg: ui::Style::dim(),
                    description_fg: ui::Style::dim(),
                    background: ui::Style::default(),
                };
                self.ui
                    .picker_open(config, items.clone(), selected, style, true)
            }
        };

        if let Some(win) = open_win {
            if let Some(p) = self.ui.picker_mut(win) {
                p.set_items(items);
                p.set_selected(selected);
            }
        }

        if let Some(session) = self.input.completer.as_mut() {
            session.picker_win = open_win;
        }
    }

    // ── Notification float ─────────────────────────────────────────
    //
    // The `ui::Notification` float (if open) is positioned one row
    // above the prompt rect. Called each frame alongside
    // `sync_completer_float`. Open/close is driven by `notify` /
    // `notify_error` / `dismiss_notification`; this just keeps the
    // rect current across terminal resizes.
    fn sync_notification_float(&mut self, prompt_rect: ui::Rect) {
        let Some(win) = self.notification else {
            return;
        };
        let (tw, _th) = self.ui.terminal_size();
        let desired_top = prompt_rect.top.saturating_sub(1) as i32;

        let needs_update = match self.ui.float_config(win).map(|c| c.placement) {
            Some(ui::Placement::Manual { row, width: w, .. }) => {
                row != desired_top || !matches!(w, ui::Constraint::Fixed(v) if v == tw)
            }
            _ => true,
        };

        if needs_update {
            if let Some(cfg) = self.ui.float_config_mut(win) {
                cfg.placement = ui::Placement::Manual {
                    anchor: ui::Anchor::NW,
                    row: desired_top,
                    col: 0,
                    width: ui::Constraint::Fixed(tw),
                    height: ui::Constraint::Fixed(1),
                };
            }
        }
    }
}
