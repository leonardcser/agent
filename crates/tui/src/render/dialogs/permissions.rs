use crate::keymap::{hints, nav_lookup, NavAction};
use crate::render::draw_bar;
use crate::{theme, workspace_permissions};
use crossterm::event::{KeyCode, KeyModifiers};
use crossterm::style::Color;

use super::{end_dialog_draw, truncate_str, DialogResult, ListState, RenderOut};

/// A single permission rule — one tool + one pattern.
#[derive(Clone)]
pub struct PermissionEntry {
    pub tool: String,
    pub pattern: String,
}

/// A selectable row — one tool+pattern pair from either session or workspace.
#[derive(Clone)]
enum Item {
    Session(usize),          // index into session_entries
    Workspace(usize, usize), // (rule_index, pattern_index) into workspace_rules
}

pub struct PermissionsDialog {
    session_entries: Vec<PermissionEntry>,
    workspace_rules: Vec<workspace_permissions::Rule>,
    items: Vec<Item>,
    list: ListState,
    pending_d: bool,
    vim_enabled: bool,
}

impl PermissionsDialog {
    pub fn new(
        session_entries: Vec<PermissionEntry>,
        workspace_rules: Vec<workspace_permissions::Rule>,
        vim_enabled: bool,
    ) -> Self {
        let items = build_items(&session_entries, &workspace_rules);
        let total = display_row_count(&session_entries, &workspace_rules, &items);
        let list = ListState::new(total.max(1));
        Self {
            session_entries,
            workspace_rules,
            items,
            list,
            pending_d: false,
            vim_enabled,
        }
    }

    fn rebuild_items(&mut self) {
        self.items = build_items(&self.session_entries, &self.workspace_rules);
        let total = display_row_count(&self.session_entries, &self.workspace_rules, &self.items);
        self.list.set_items(total.max(1));
    }

    fn delete_selected(&mut self) {
        let Some(item) = self.items.get(self.list.selected).cloned() else {
            return;
        };
        match item {
            Item::Session(idx) => {
                self.session_entries.remove(idx);
            }
            Item::Workspace(rule_idx, pat_idx) => {
                let rule = &mut self.workspace_rules[rule_idx];
                if rule.patterns.is_empty() || rule.patterns.len() == 1 {
                    self.workspace_rules.remove(rule_idx);
                } else {
                    rule.patterns.remove(pat_idx);
                }
            }
        }
        self.rebuild_items();
    }

    fn close_result(&self) -> DialogResult {
        DialogResult::PermissionsClosed {
            session_remaining: self.session_entries.clone(),
            workspace_remaining: self.workspace_rules.clone(),
        }
    }
}

impl super::Dialog for PermissionsDialog {
    fn height(&self) -> u16 {
        let total = display_row_count(&self.session_entries, &self.workspace_rules, &self.items);
        self.list.height(total.max(1), 3)
    }

    fn constrain_height(&self) -> bool {
        true
    }

    fn mark_dirty(&mut self) {
        self.list.dirty = true;
    }

    fn handle_resize(&mut self) {
        self.list.handle_resize();
    }

    fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> Option<DialogResult> {
        if self.pending_d {
            self.pending_d = false;
            if code == KeyCode::Char('d') && mods == KeyModifiers::NONE {
                self.delete_selected();
                return None;
            }
        }

        // Permissions-specific: q to close, d to delete, backspace to delete.
        match (code, mods) {
            (KeyCode::Char('q'), KeyModifiers::NONE) => return Some(self.close_result()),
            (KeyCode::Char('d'), KeyModifiers::NONE) => {
                self.pending_d = true;
                self.list.dirty = true;
                return None;
            }
            (KeyCode::Backspace, _) => {
                self.delete_selected();
                return None;
            }
            _ => {}
        }

        let n = self.items.len();
        match nav_lookup(code, mods) {
            Some(NavAction::Dismiss) => Some(self.close_result()),
            Some(nav) => {
                self.list.handle_nav(nav, n);
                None
            }
            None => None,
        }
    }

    fn draw(&mut self, out: &mut RenderOut, start_row: u16, width: u16, granted_rows: u16) {
        let total = display_row_count(&self.session_entries, &self.workspace_rules, &self.items);
        let Some(w) = self
            .list
            .begin_draw(out, start_row, total.max(1), width, granted_rows, 3)
        else {
            return;
        };

        draw_bar(out, w, None, None, theme::accent());
        out.newline();

        if self.items.is_empty() {
            out.push_dim();
            out.print(" No permissions");
            out.pop_style();
            out.newline();
        } else {
            let mut printed_workspace = false;
            for (i, item) in self.items.iter().enumerate() {
                if matches!(item, Item::Session(_)) && i == 0 {
                    print_header(out, " Session");
                }
                if matches!(item, Item::Workspace(_, _)) && !printed_workspace {
                    printed_workspace = true;
                    if i > 0 {
                        out.newline();
                    }
                    print_header(out, " Workspace");
                }

                let label = match item {
                    Item::Session(idx) => format_permission_entry(&self.session_entries[*idx]),
                    Item::Workspace(ri, pi) => format_rule_entry(&self.workspace_rules[*ri], *pi),
                };
                render_entry_row(out, &label, i == self.list.selected, w, theme::accent());
            }
        }

        out.newline();
        out.push_dim();
        let hint = if self.pending_d {
            hints::join(&[hints::DD_PENDING, hints::CLOSE])
        } else {
            hints::join(&[hints::dd_delete(self.vim_enabled), hints::CLOSE])
        };
        out.print(&hint);
        out.pop_style();
        end_dialog_draw(out);
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

fn print_header(out: &mut crate::render::RenderOut, label: &str) {
    out.push_dim();
    out.print(label);
    out.pop_style();
    out.newline();
}

fn render_entry_row(
    out: &mut crate::render::RenderOut,
    label: &str,
    selected: bool,
    width: usize,
    accent: Color,
) {
    let label = truncate_str(label, width.saturating_sub(4));
    if selected {
        out.print("  ");
        out.push_fg(accent);
        out.print(&label);
        out.pop_style();
    } else {
        out.print("  ");
        out.print(&label);
    }
    out.newline();
}

fn build_items(
    session_entries: &[PermissionEntry],
    workspace_rules: &[workspace_permissions::Rule],
) -> Vec<Item> {
    let mut items = Vec::new();
    for i in 0..session_entries.len() {
        items.push(Item::Session(i));
    }
    for (ri, rule) in workspace_rules.iter().enumerate() {
        if rule.patterns.is_empty() {
            items.push(Item::Workspace(ri, 0));
        } else {
            for pi in 0..rule.patterns.len() {
                items.push(Item::Workspace(ri, pi));
            }
        }
    }
    items
}

/// Total display rows: items + one header per non-empty section.
fn display_row_count(
    session_entries: &[PermissionEntry],
    workspace_rules: &[workspace_permissions::Rule],
    items: &[Item],
) -> usize {
    let headers = !session_entries.is_empty() as usize + !workspace_rules.is_empty() as usize;
    let gap = if !session_entries.is_empty() && !workspace_rules.is_empty() {
        1
    } else {
        0
    };
    items.len() + headers + gap
}

fn format_permission_entry(entry: &PermissionEntry) -> String {
    format!("{}: {}", entry.tool, entry.pattern)
}

fn format_rule_entry(rule: &workspace_permissions::Rule, pat_idx: usize) -> String {
    if rule.patterns.is_empty() {
        format!("{}: *", rule.tool)
    } else {
        format!("{}: {}", rule.tool, rule.patterns[pat_idx])
    }
}
