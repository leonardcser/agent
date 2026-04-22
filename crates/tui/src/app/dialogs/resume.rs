use super::super::App;
use crate::app::ops::AppOp;
use crate::render::ResumeEntry;
use crossterm::event::{KeyCode, KeyModifiers};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use ui::{Callback, CallbackResult, KeyBind, Payload, WinEvent};

const LEADING: usize = 2;
const SIZE_COL: usize = 8;
const TIME_COL: usize = 7;
const GAP: usize = 2;

struct ResumeState {
    entries: Vec<ResumeEntry>,
    title_haystacks: Vec<String>,
    current_cwd: String,
    query: String,
    workspace_only: bool,
    filtered: Vec<usize>,
    pending_d: bool,
    content_cache: Option<HashMap<String, String>>,
    list_buf: ui::BufId,
    title_buf: ui::BufId,
}

impl ResumeState {
    fn recompute_filter(&mut self) {
        self.filtered = filter_entries(
            &self.entries,
            &self.title_haystacks,
            &self.query,
            self.workspace_only,
            &self.current_cwd,
            self.content_cache.as_ref(),
        );
    }

    fn refresh_title(&self, ui: &mut ui::Ui) {
        refresh_title(ui, self.title_buf, self.workspace_only, &self.query);
    }

    fn refresh_list(&self, ui: &mut ui::Ui) {
        refresh_list(ui, self.list_buf, &self.entries, &self.filtered);
    }

    fn delete_selected(&mut self, ui: &mut ui::Ui, win: ui::WinId) {
        let sel = ui.dialog_mut(win).and_then(|d| d.selected_index());
        let Some(sel) = sel else { return };
        let Some(&idx) = self.filtered.get(sel) else {
            return;
        };
        let Some(entry) = self.entries.get(idx) else {
            return;
        };
        let id = entry.id.clone();
        crate::session::delete(&id);
        self.entries.remove(idx);
        self.title_haystacks.remove(idx);
        if let Some(cache) = self.content_cache.as_mut() {
            cache.remove(&id);
        }
        self.recompute_filter();
        self.refresh_list(ui);
    }
}

pub(in crate::app) fn open(app: &mut App, entries: Vec<ResumeEntry>) {
    use crate::keymap::hints;

    let current_cwd = app.cwd.clone();
    let vim_enabled = app.input.vim_enabled();
    let title_haystacks: Vec<String> = entries.iter().map(build_title_haystack).collect();
    let filtered = filter_entries(&entries, &title_haystacks, "", true, &current_cwd, None);

    let title_buf = app.ui.buf_create(ui::buffer::BufCreateOpts {
        buftype: ui::buffer::BufType::Scratch,
        modifiable: false,
    });
    refresh_title(&mut app.ui, title_buf, true, "");

    let list_buf = app.ui.buf_create(ui::buffer::BufCreateOpts {
        buftype: ui::buffer::BufType::Scratch,
        modifiable: false,
    });
    refresh_list(&mut app.ui, list_buf, &entries, &filtered);

    let toggle = "ctrl+w: this workspace";
    let hint_text = hints::join(&[
        hints::SELECT,
        hints::del_delete(vim_enabled),
        hints::CANCEL,
        toggle,
    ]);
    let dialog_config = app.builtin_dialog_config(Some(hint_text), vec![]);

    let Some(win_id) = app.ui.dialog_open(
        ui::FloatConfig {
            title: None,
            border: ui::Border::None,
            placement: ui::Placement::dock_bottom_full_width(ui::Constraint::Pct(60)),
            ..Default::default()
        },
        dialog_config,
        vec![
            ui::PanelSpec::content(title_buf, ui::PanelHeight::Fixed(1)).focusable(false),
            ui::PanelSpec::list(list_buf, ui::PanelHeight::Fill),
        ],
    ) else {
        return;
    };

    let state = Rc::new(RefCell::new(ResumeState {
        entries,
        title_haystacks,
        current_cwd,
        query: String::new(),
        workspace_only: true,
        filtered,
        pending_d: false,
        content_cache: None,
        list_buf,
        title_buf,
    }));

    let ops = app.lua.ops_handle();

    // Submit (Enter): load selected session.
    let state_submit = state.clone();
    let ops_submit = ops.clone();
    app.ui.win_on_event(
        win_id,
        WinEvent::Submit,
        Callback::Rust(Box::new(move |ctx| {
            if let Payload::Selection { index } = ctx.payload {
                let s = state_submit.borrow();
                if let Some(&entry_idx) = s.filtered.get(index) {
                    if let Some(entry) = s.entries.get(entry_idx) {
                        ops_submit.push(AppOp::LoadSession(entry.id.clone()));
                    }
                }
            }
            ops_submit.push(AppOp::CloseFloat(ctx.win));
            CallbackResult::Consumed
        })),
    );

    // Dismiss: just close.
    let ops_dismiss = ops.clone();
    app.ui.win_on_event(
        win_id,
        WinEvent::Dismiss,
        Callback::Rust(Box::new(move |ctx| {
            ops_dismiss.push(AppOp::CloseFloat(ctx.win));
            CallbackResult::Consumed
        })),
    );

    // Ctrl+W: toggle workspace-only filter.
    let state_w = state.clone();
    app.ui.win_set_keymap(
        win_id,
        KeyBind::ctrl('w'),
        Callback::Rust(Box::new(move |ctx| {
            let mut s = state_w.borrow_mut();
            s.pending_d = false;
            s.workspace_only = !s.workspace_only;
            s.recompute_filter();
            s.refresh_title(ctx.ui);
            s.refresh_list(ctx.ui);
            CallbackResult::Consumed
        })),
    );

    // Delete: drop selected session.
    let state_del = state.clone();
    app.ui.win_set_keymap(
        win_id,
        KeyBind::plain(KeyCode::Delete),
        Callback::Rust(Box::new(move |ctx| {
            let mut s = state_del.borrow_mut();
            s.pending_d = false;
            s.delete_selected(ctx.ui, ctx.win);
            CallbackResult::Consumed
        })),
    );

    // Backspace: plain -> pop char from query; Alt/Ctrl -> delete word.
    let state_bs = state.clone();
    app.ui.win_set_keymap(
        win_id,
        KeyBind::plain(KeyCode::Backspace),
        Callback::Rust(Box::new(move |ctx| {
            let mut s = state_bs.borrow_mut();
            s.pending_d = false;
            if !s.query.is_empty() {
                s.query.pop();
                s.recompute_filter();
                s.refresh_title(ctx.ui);
                s.refresh_list(ctx.ui);
            }
            CallbackResult::Consumed
        })),
    );
    for mods in [KeyModifiers::ALT, KeyModifiers::CONTROL] {
        let state_wbs = state.clone();
        app.ui.win_set_keymap(
            win_id,
            KeyBind::new(KeyCode::Backspace, mods),
            Callback::Rust(Box::new(move |ctx| {
                let mut s = state_wbs.borrow_mut();
                s.pending_d = false;
                if !s.query.is_empty() {
                    let len = s.query.len();
                    let target = crate::text_utils::word_backward_pos(
                        &s.query,
                        len,
                        crate::text_utils::CharClass::Word,
                    );
                    s.query.truncate(target);
                    if !s.query.is_empty() {
                        let ResumeState {
                            ref entries,
                            ref mut content_cache,
                            ..
                        } = *s;
                        ensure_content_loaded(entries, content_cache);
                    }
                    s.recompute_filter();
                    s.refresh_title(ctx.ui);
                    s.refresh_list(ctx.ui);
                }
                CallbackResult::Consumed
            })),
        );
    }

    // Fallback: any other key. Handles typing into the filter query
    // (printable chars + optional shift) and the `dd` delete chord
    // when the query is empty.
    let state_fb = state;
    let ops_fb = ops;
    app.ui.win_set_key_fallback(
        win_id,
        Callback::Rust(Box::new(move |ctx| {
            let Payload::Key { code, mods } = ctx.payload else {
                return CallbackResult::Pass;
            };
            let mut s = state_fb.borrow_mut();

            // dd: two-press delete when query is empty.
            if s.pending_d {
                s.pending_d = false;
                if code == KeyCode::Char('d') && mods == KeyModifiers::NONE {
                    s.delete_selected(ctx.ui, ctx.win);
                    return CallbackResult::Consumed;
                }
                s.query.push('d');
            }

            if let KeyCode::Char(c) = code {
                if mods.is_empty() || mods == KeyModifiers::SHIFT {
                    if c == 'd' && mods == KeyModifiers::NONE && s.query.is_empty() {
                        s.pending_d = true;
                        return CallbackResult::Consumed;
                    }
                    s.query.push(c);
                    if !s.query.is_empty() {
                        let ResumeState {
                            ref entries,
                            ref mut content_cache,
                            ..
                        } = *s;
                        ensure_content_loaded(entries, content_cache);
                    }
                    s.recompute_filter();
                    s.refresh_title(ctx.ui);
                    s.refresh_list(ctx.ui);
                    return CallbackResult::Consumed;
                }
            }

            // Any non-char key we didn't recognize — let the dialog's
            // default handler process it (nav arrows, Enter, Esc, …).
            // Also ensure query/pending_d aren't left in an inconsistent
            // state by falling through.
            let _ = &ops_fb; // keep capture live
            CallbackResult::Pass
        })),
    );
}

fn title(entry: &ResumeEntry) -> String {
    fn is_junk(s: &str) -> bool {
        let t = s.trim();
        t.is_empty()
            || t.eq_ignore_ascii_case("untitled")
            || t.starts_with('/')
            || t.starts_with('\x00')
    }
    let raw = if !is_junk(&entry.title) {
        &entry.title
    } else if let Some(ref sub) = entry.subtitle {
        if !is_junk(sub) {
            sub
        } else {
            return "Untitled".into();
        }
    } else {
        return "Untitled".into();
    };
    raw.lines().next().unwrap_or("Untitled").trim().to_string()
}

fn build_title_haystack(entry: &ResumeEntry) -> String {
    let mut hay = title(entry);
    if let Some(ref subtitle) = entry.subtitle {
        hay.push(' ');
        hay.push_str(subtitle);
    }
    hay.to_lowercase()
}

fn ts(entry: &ResumeEntry) -> u64 {
    if entry.updated_at_ms > 0 {
        entry.updated_at_ms
    } else {
        entry.created_at_ms
    }
}

fn filter_entries(
    entries: &[ResumeEntry],
    title_haystacks: &[String],
    query: &str,
    workspace_only: bool,
    current_cwd: &str,
    content_cache: Option<&HashMap<String, String>>,
) -> Vec<usize> {
    let in_workspace = |e: &ResumeEntry| -> bool {
        if !workspace_only {
            return true;
        }
        matches!(e.cwd, Some(ref cwd) if cwd == current_cwd)
    };

    if query.is_empty() {
        return entries
            .iter()
            .enumerate()
            .filter(|(_, e)| in_workspace(e))
            .map(|(i, _)| i)
            .collect();
    }

    let q = query.to_lowercase();
    let mut title_hits: Vec<usize> = Vec::new();
    let mut content_hits: Vec<usize> = Vec::new();
    for (i, entry) in entries.iter().enumerate() {
        if !in_workspace(entry) {
            continue;
        }
        if crate::fuzzy::fuzzy_match_lower(&title_haystacks[i], &q) {
            title_hits.push(i);
            continue;
        }
        if let Some(cache) = content_cache {
            if cache.get(&entry.id).is_some_and(|blob| blob.contains(&q)) {
                content_hits.push(i);
            }
        }
    }
    title_hits.extend(content_hits);
    title_hits
}

fn ensure_content_loaded(
    entries: &[ResumeEntry],
    content_cache: &mut Option<HashMap<String, String>>,
) {
    if content_cache.is_some() {
        return;
    }
    let ids: Vec<String> = entries.iter().map(|e| e.id.clone()).collect();
    let pairs = crate::utils::parallel_filter_map(ids, |id| {
        crate::session::load_search_blob(&id).map(|b| (id, b.to_lowercase()))
    });
    *content_cache = Some(pairs.into_iter().collect());
}

fn refresh_list(ui: &mut ui::Ui, list_buf: ui::BufId, entries: &[ResumeEntry], filtered: &[usize]) {
    let now_ms = crate::session::now_ms();
    let mut lines: Vec<String> = Vec::with_capacity(filtered.len());
    let mut dim_ranges: Vec<(u16, u16)> = Vec::with_capacity(filtered.len());
    for &idx in filtered {
        let Some(e) = entries.get(idx) else {
            continue;
        };
        let title_text = title(e);
        let time_ago = crate::session::time_ago(ts(e), now_ms);
        let size_str = e
            .size_bytes
            .map(crate::session::format_size)
            .unwrap_or_default();
        let indent = " ".repeat(e.depth * 2);
        let line = format!(
            "{leading}{size:>size_w$}{gap}{time:>time_w$}{gap}{indent}{title_text}",
            leading = " ".repeat(LEADING),
            size = size_str,
            time = time_ago,
            size_w = SIZE_COL,
            time_w = TIME_COL,
            gap = " ".repeat(GAP),
        );
        let dim_end = (LEADING + SIZE_COL + GAP + TIME_COL) as u16;
        dim_ranges.push((0, dim_end));
        lines.push(line);
    }
    let Some(buf) = ui.buf_mut(list_buf) else {
        return;
    };
    buf.set_all_lines(lines);
    for (i, (start, end)) in dim_ranges.iter().enumerate() {
        buf.add_highlight(i, *start, *end, ui::buffer::SpanStyle::dim());
    }
}

fn refresh_title(ui: &mut ui::Ui, title_buf: ui::BufId, workspace_only: bool, query: &str) {
    let Some(buf) = ui.buf_mut(title_buf) else {
        return;
    };
    let label = if workspace_only {
        " resume (workspace):"
    } else {
        " resume (all):"
    };
    let line = format!("{label} {query}");
    buf.set_all_lines(vec![line]);
    buf.add_highlight(0, 0, label.len() as u16, ui::buffer::SpanStyle::dim());
}

