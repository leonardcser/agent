use crate::keymap::{hints, nav_lookup, NavAction};
use crate::render::{draw_bar, ResumeEntry};
use crate::{session, theme};
use crossterm::event::{KeyCode, KeyModifiers};
use std::collections::HashMap;
use std::time::Instant;

use super::{end_dialog_draw, truncate_str, DialogResult, ListState, RenderOut};

pub struct ResumeDialog {
    entries: Vec<ResumeEntry>,
    /// Lowercased "title subtitle" strings, indexed in parallel with
    /// `entries`. Computed once so refilter doesn't realloc per keystroke.
    title_haystacks: Vec<String>,
    current_cwd: String,
    query: String,
    workspace_only: bool,
    /// Indices into `entries` for the currently visible rows. Storing
    /// indices avoids cloning entries (and their potentially large
    /// subtitles) on every keystroke.
    filtered: Vec<usize>,
    list: ListState,
    pending_d: bool,
    last_drawn: Instant,
    vim_enabled: bool,
    /// Lowercased search blobs keyed by session id. `None` until the user
    /// types the first query character, so opening the dialog is instant
    /// even with thousands of sessions.
    content_cache: Option<HashMap<String, String>>,
}

impl ResumeDialog {
    pub fn new(entries: Vec<ResumeEntry>, current_cwd: String, vim_enabled: bool) -> Self {
        let title_haystacks: Vec<String> = entries.iter().map(build_title_haystack).collect();
        let filtered =
            filter_resume_entries(&entries, &title_haystacks, "", true, &current_cwd, None);
        let list = ListState::new(filtered.len().max(1));
        Self {
            entries,
            title_haystacks,
            current_cwd,
            query: String::new(),
            workspace_only: true,
            filtered,
            list,
            pending_d: false,
            last_drawn: Instant::now(),
            vim_enabled,
            content_cache: None,
        }
    }

    fn ensure_content_loaded(&mut self) {
        if self.content_cache.is_some() {
            return;
        }
        let _perf = crate::perf::begin("resume:load_content");
        let ids: Vec<String> = self.entries.iter().map(|e| e.id.clone()).collect();
        let pairs = crate::utils::parallel_filter_map(ids, |id| {
            session::load_search_blob(&id).map(|b| (id, b.to_lowercase()))
        });
        let cache: HashMap<String, String> = pairs.into_iter().collect();
        let total_bytes: u64 = cache.values().map(|v| v.len() as u64).sum();
        crate::perf::record_value("resume:load_content:sessions", cache.len() as u64);
        crate::perf::record_value("resume:load_content:bytes", total_bytes);
        self.content_cache = Some(cache);
    }

    fn refilter(&mut self) {
        if !self.query.is_empty() {
            self.ensure_content_loaded();
        }
        let _perf = crate::perf::begin("resume:filter");
        self.filtered = filter_resume_entries(
            &self.entries,
            &self.title_haystacks,
            &self.query,
            self.workspace_only,
            &self.current_cwd,
            self.content_cache.as_ref(),
        );
        crate::perf::record_value("resume:filter:entries", self.entries.len() as u64);
        crate::perf::record_value("resume:filter:hits", self.filtered.len() as u64);
        crate::perf::record_value("resume:filter:query_len", self.query.len() as u64);
        self.list.set_items(self.filtered.len().max(1));
    }

    fn delete_selected(&mut self) {
        let Some(&idx) = self.filtered.get(self.list.selected) else {
            return;
        };
        let Some(entry) = self.entries.get(idx) else {
            return;
        };
        let id = entry.id.clone();
        session::delete(&id);
        self.entries.remove(idx);
        self.title_haystacks.remove(idx);
        if let Some(cache) = self.content_cache.as_mut() {
            cache.remove(&id);
        }
        self.refilter();
    }
}

impl super::Dialog for ResumeDialog {
    fn height(&self) -> u16 {
        self.list.height(self.filtered.len().max(1), 4)
    }

    fn constrain_height(&self) -> bool {
        true
    }

    fn mark_dirty(&mut self) {
        self.list.dirty = true;
    }

    fn handle_resize(&mut self) {
        self.list.handle_resize();
        self.refilter();
    }

    fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> Option<DialogResult> {
        // DD completion check.
        if self.pending_d {
            self.pending_d = false;
            if code == KeyCode::Char('d') && mods == KeyModifiers::NONE {
                self.delete_selected();
                return None;
            }
            self.query.push('d');
            // Fall through to handle the current key normally.
        }

        // Resume-specific keys (before shared dialog lookup).
        match (code, mods) {
            (KeyCode::Char('w'), m) if m.contains(KeyModifiers::CONTROL) => {
                self.workspace_only = !self.workspace_only;
                self.refilter();
                return None;
            }
            (KeyCode::Backspace, m)
                if m.contains(KeyModifiers::ALT) || m.contains(KeyModifiers::CONTROL) =>
            {
                if !self.query.is_empty() {
                    let len = self.query.len();
                    let target = crate::text_utils::word_backward_pos(
                        &self.query,
                        len,
                        crate::text_utils::CharClass::Word,
                    );
                    self.query.truncate(target);
                    self.refilter();
                }
                return None;
            }
            (KeyCode::Backspace, _) => {
                if !self.query.is_empty() {
                    self.query.pop();
                    self.refilter();
                }
                return None;
            }
            (KeyCode::Delete, _) => {
                self.delete_selected();
                return None;
            }
            (KeyCode::Char('d'), KeyModifiers::NONE) if self.query.is_empty() => {
                self.pending_d = true;
                self.list.dirty = true;
                return None;
            }
            _ => {}
        }

        // Shared dialog keys.
        let n = self.filtered.len();
        match nav_lookup(code, mods) {
            Some(NavAction::Confirm) => Some(DialogResult::Resume {
                session_id: self
                    .filtered
                    .get(self.list.selected)
                    .and_then(|&i| self.entries.get(i))
                    .map(|e| e.id.clone()),
            }),
            Some(NavAction::Dismiss) => Some(DialogResult::Resume { session_id: None }),
            Some(nav) if self.list.handle_nav(nav, n) => None,
            _ => {
                // Unhandled keys: insert as search query chars.
                if let KeyCode::Char(c) = code {
                    if mods.is_empty() || mods == KeyModifiers::SHIFT {
                        self.query.push(c);
                        self.refilter();
                    }
                }
                None
            }
        }
    }

    fn draw(&mut self, out: &mut RenderOut, start_row: u16, width: u16, granted_rows: u16) {
        if !self.list.dirty {
            let freshest = self
                .filtered
                .iter()
                .filter_map(|&i| self.entries.get(i))
                .map(resume_ts)
                .max()
                .unwrap_or(0);
            let age_s = session::now_ms().saturating_sub(freshest) / 1000;
            let interval = if age_s < 60 {
                1
            } else if age_s < 3600 {
                30
            } else {
                60
            };
            if self.last_drawn.elapsed().as_secs() >= interval {
                self.list.dirty = true;
            }
        }
        if !self.list.dirty {
            return;
        }
        self.last_drawn = Instant::now();

        let Some(w) = self.list.begin_draw(
            out,
            start_row,
            self.filtered.len().max(1),
            width,
            granted_rows,
            4,
        ) else {
            return;
        };

        let now_ms = session::now_ms();

        draw_bar(out, w, None, None, theme::accent());
        out.newline();

        out.push_dim();
        if self.workspace_only {
            out.print(" Resume (workspace):");
        } else {
            out.print(" Resume (all):");
        }
        out.pop_style();
        out.print(" ");
        out.print(&self.query);
        out.newline();

        if self.filtered.is_empty() {
            out.push_dim();
            out.print("  No matches");
            out.pop_style();
            out.newline();
        } else {
            let range = self.list.visible_range(self.filtered.len());
            for (i, entry) in self
                .filtered
                .iter()
                .filter_map(|&idx| self.entries.get(idx))
                .enumerate()
                .take(range.end)
                .skip(range.start)
            {
                const SIZE_COL: usize = 8;
                const TIME_COL: usize = 7;
                const GAP: usize = 2;
                const LEADING: usize = 2;

                let title = resume_title(entry);
                let time_ago = session::time_ago(resume_ts(entry), now_ms);
                let size_str = entry
                    .size_bytes
                    .map(session::format_size)
                    .unwrap_or_default();
                let tree_indent = entry.depth * 2;

                let size_pad = " ".repeat(SIZE_COL.saturating_sub(size_str.chars().count()));
                let time_pad = " ".repeat(TIME_COL.saturating_sub(time_ago.chars().count()));

                let title_col =
                    w.saturating_sub(LEADING + SIZE_COL + GAP + TIME_COL + GAP + tree_indent);
                let truncated = truncate_str(&title, title_col);

                out.push_dim();
                out.print(&" ".repeat(LEADING));
                out.print(&size_pad);
                out.print(&size_str);
                out.print(&" ".repeat(GAP));
                out.print(&time_ago);
                out.print(&time_pad);
                out.pop_style();
                out.print(&" ".repeat(GAP + tree_indent));
                if i == self.list.selected {
                    out.push_fg(theme::accent());
                    out.print(&truncated);
                    out.pop_style();
                } else {
                    out.print(&truncated);
                }
                out.newline();
            }
        }

        out.newline();
        out.push_dim();
        let toggle = if self.workspace_only {
            "ctrl+w: all sessions"
        } else {
            "ctrl+w: this workspace"
        };
        out.print(&hints::join(&[
            hints::SELECT,
            hints::del_delete(self.vim_enabled),
            hints::CANCEL,
            toggle,
        ]));
        out.pop_style();
        end_dialog_draw(out);
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

fn is_junk_title(s: &str) -> bool {
    let t = s.trim();
    t.is_empty()
        || t.eq_ignore_ascii_case("untitled")
        || t.starts_with('/')
        || t.starts_with('\x00')
}

fn resume_title(entry: &ResumeEntry) -> String {
    let raw = if !is_junk_title(&entry.title) {
        &entry.title
    } else if let Some(ref sub) = entry.subtitle {
        if !is_junk_title(sub) {
            sub
        } else {
            return "Untitled".into();
        }
    } else {
        return "Untitled".into();
    };
    raw.lines().next().unwrap_or("Untitled").trim().to_string()
}

fn resume_ts(entry: &ResumeEntry) -> u64 {
    if entry.updated_at_ms > 0 {
        entry.updated_at_ms
    } else {
        entry.created_at_ms
    }
}

fn filter_resume_entries(
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

fn build_title_haystack(entry: &ResumeEntry) -> String {
    let mut hay = resume_title(entry);
    if let Some(ref subtitle) = entry.subtitle {
        hay.push(' ');
        hay.push_str(subtitle);
    }
    hay.to_lowercase()
}
