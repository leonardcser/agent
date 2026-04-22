use score::{query_match_score, split_words};

pub mod command;
pub mod file;
pub mod history;
pub mod pickers;
mod score;

#[derive(Clone, Default)]
pub struct CompletionItem {
    pub label: String,
    pub description: Option<String>,
    pub search_terms: Option<String>,
    /// ANSI terminal color for theme/color picker swatches.
    pub ansi_color: Option<u8>,
    /// Secondary value (e.g. model key when label is the display name).
    pub extra: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum CompleterKind {
    File,
    Command,
    CommandArg,
    History,
    Model,
    Theme,
    Color,
    Settings,
}

pub struct Completer {
    /// Byte offset in the buffer where the trigger char starts.
    pub anchor: usize,
    pub kind: CompleterKind,
    /// Current query (text after trigger).
    pub query: String,
    /// Filtered results.
    pub results: Vec<CompletionItem>,
    /// Selected index in results.
    pub selected: usize,
    /// Full item list (cached on activation).
    pub(super) all_items: Vec<CompletionItem>,
    /// Stable identity of the selected item across filter updates.
    pub(super) selected_key: Option<String>,
    /// Original value to restore on dismiss (Theme = accent, Color = slug color).
    pub original_value: Option<u8>,
}

impl Completer {
    /// Replace the item list and re-filter, preserving the current selection.
    pub fn refresh_items(&mut self, items: Vec<CompletionItem>) {
        self.all_items = items;
        self.filter_inner(true);
    }

    pub fn all_items(&self) -> &[CompletionItem] {
        &self.all_items
    }

    /// Returns the selected item, if any.
    pub fn selected_item(&self) -> Option<&CompletionItem> {
        self.results.get(self.selected)
    }

    /// Returns the `extra` field of the selected item if present, otherwise `label`.
    pub fn accept_extra(&self) -> Option<&str> {
        self.selected_item()
            .map(|i| i.extra.as_deref().unwrap_or(i.label.as_str()))
    }

    /// True for pickers that should always stay visible (even with no matches).
    pub fn is_picker(&self) -> bool {
        matches!(
            self.kind,
            CompleterKind::Model
                | CompleterKind::Theme
                | CompleterKind::Color
                | CompleterKind::Settings
        )
    }

    /// Maximum rows to display for this completer kind.
    pub fn max_visible_rows(&self) -> usize {
        match self.kind {
            CompleterKind::Theme | CompleterKind::Color => 14,
            CompleterKind::Model => 7,
            CompleterKind::Settings => 9,
            _ => 5,
        }
    }

    fn item_key(item: &CompletionItem) -> &str {
        item.extra.as_deref().unwrap_or(item.label.as_str())
    }

    fn remember_selected_key(&mut self) {
        self.selected_key = self
            .results
            .get(self.selected)
            .map(|item| Self::item_key(item).to_string());
    }

    fn restore_selected_key(&mut self) {
        if let Some(ref key) = self.selected_key {
            if let Some(idx) = self
                .results
                .iter()
                .position(|item| Self::item_key(item) == key)
            {
                self.selected = idx;
                return;
            }
        }
        if self.selected >= self.results.len() {
            self.selected = 0;
        }
    }

    fn search_fields(&self, item: &CompletionItem) -> Vec<String> {
        let mut fields = vec![item.label.to_lowercase()];
        if let Some(terms) = item.search_terms.as_deref() {
            if !terms.is_empty() {
                fields.push(terms.to_lowercase());
            }
        }
        if self.kind == CompleterKind::Settings {
            if let Some(extra) = item.extra.as_deref() {
                if !extra.is_empty() {
                    fields.push(extra.to_lowercase());
                }
            }
        }
        fields
    }

    pub fn update_query(&mut self, query: String) {
        self.query = query;
        self.selected = 0;
        self.selected_key = None;
        self.filter();
    }

    fn filter(&mut self) {
        self.filter_inner(false);
    }

    fn filter_inner(&mut self, preserve_selection: bool) {
        let _perf = crate::perf::begin("completer:filter");
        if preserve_selection {
            self.remember_selected_key();
        }
        if self.query.is_empty() {
            self.results = self.all_items.clone();
        } else {
            let query = self.query.to_lowercase();
            let query_words = split_words(&query);
            let mut scored: Vec<_> = self
                .all_items
                .iter()
                .enumerate()
                .filter_map(|(i, item)| {
                    let score = if self.kind == CompleterKind::History {
                        history::history_score(&item.label, &self.query, i)
                    } else {
                        let fields = self.search_fields(item);
                        query_match_score(&query, &query_words, &fields)
                    }?;
                    Some((score, i, item.clone()))
                })
                .collect();
            scored.sort_by_key(|(s, i, _)| (*s, *i));
            self.results = scored.into_iter().map(|(_, _, item)| item).collect();
        }
        if preserve_selection {
            self.restore_selected_key();
        } else {
            self.selected = 0;
        }
    }

    pub fn move_up(&mut self) {
        if !self.results.is_empty() {
            self.selected = if self.selected == 0 {
                self.results.len() - 1
            } else {
                self.selected - 1
            };
            self.remember_selected_key();
        }
    }

    pub fn move_down(&mut self) {
        if !self.results.is_empty() {
            self.selected = (self.selected + 1) % self.results.len();
            self.remember_selected_key();
        }
    }

    pub fn accept(&self) -> Option<&str> {
        self.results.get(self.selected).map(|i| i.label.as_str())
    }
}

/// Couples a `Completer` model with its `ui::Picker` float WinId.
/// The two share a lifecycle — created when the user opens a
/// completer, destroyed together when it closes — so they live in
/// one owner. Matches the shape a future Lua completer plugin would
/// hold locally (`{ completer, picker_win }`).
pub struct CompleterSession {
    pub completer: Completer,
    pub picker_win: Option<ui::WinId>,
}

impl CompleterSession {
    pub fn new(completer: Completer) -> Self {
        Self {
            completer,
            picker_win: None,
        }
    }
}

impl std::ops::Deref for CompleterSession {
    type Target = Completer;
    fn deref(&self) -> &Completer {
        &self.completer
    }
}

impl std::ops::DerefMut for CompleterSession {
    fn deref_mut(&mut self) -> &mut Completer {
        &mut self.completer
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history_labels(entries: &[&str], query: &str) -> Vec<String> {
        let history: Vec<String> = entries.iter().map(|entry| (*entry).to_string()).collect();
        let mut completer = Completer::history(&history);
        completer.update_query(query.to_string());
        completer
            .results
            .iter()
            .map(|item| item.label.clone())
            .collect()
    }

    #[test]
    fn reverse_search_prefers_exact_single_word_prompt() {
        let labels = history_labels(&["hot dog bun", "bundle assets", "bun"], "bun");
        assert_eq!(labels.first().map(String::as_str), Some("bun"));
    }

    #[test]
    fn reverse_search_prefers_whole_word_over_embedded_match() {
        let labels = history_labels(&["bundle assets", "hot dog bun"], "bun");
        let bun_pos = labels
            .iter()
            .position(|label| label == "hot dog bun")
            .unwrap();
        let bundle_pos = labels
            .iter()
            .position(|label| label == "bundle assets")
            .unwrap();
        assert!(bun_pos < bundle_pos, "whole-word bun should beat bundle");
    }

    #[test]
    fn reverse_search_prefers_more_recent_history_for_similar_matches() {
        let labels = history_labels(&["older bun prompt", "newer bun prompt"], "bun");
        assert_eq!(labels.first().map(String::as_str), Some("newer bun prompt"));
    }

    #[test]
    fn reverse_search_prefers_real_word_match_over_fuzzy_letters() {
        let labels = history_labels(
            &[
                "use the gh cli search for issue in the llama.cpp repo",
                "don't cat into a file, just tell me here",
                "create a full stack application fully with bun and typscript for recepies. work with subagents",
                "add them with default allow",
                "full",
            ],
            "full",
        );
        let exact_pos = labels.iter().position(|label| label == "full").unwrap();
        let word_pos = labels
            .iter()
            .position(|label| {
                label == "create a full stack application fully with bun and typscript for recepies. work with subagents"
            })
            .unwrap();
        let fuzzy_pos = labels
            .iter()
            .position(|label| label == "add them with default allow")
            .unwrap();

        assert!(
            exact_pos < word_pos,
            "exact match should beat longer word hit"
        );
        assert!(
            word_pos < fuzzy_pos,
            "word hit should beat fuzzy-only subsequence"
        );
    }
}

#[cfg(test)]
mod settings_tests {
    use super::*;
    use crate::input::SettingsState;

    fn test_state(vim: bool) -> SettingsState {
        SettingsState {
            vim,
            auto_compact: false,
            show_tps: true,
            show_tokens: true,
            show_cost: true,
            show_prediction: true,
            show_slug: true,
            show_thinking: true,
            restrict_to_workspace: false,
            redact_secrets: true,
        }
    }

    #[test]
    fn filter_auto_shows_auto_compact() {
        let mut comp = Completer::settings(&test_state(false));
        comp.update_query("auto".into());
        assert_eq!(comp.results[0].extra.as_deref(), Some("auto_compact"));
        assert_eq!(comp.results[0].description.as_deref(), Some("off"));
    }

    #[test]
    fn filter_vim_shows_vim_mode() {
        let mut comp = Completer::settings(&test_state(true));
        comp.update_query("vim".into());
        assert_eq!(
            comp.results.len(),
            1,
            "results: {:?}",
            comp.results.iter().map(|i| &i.label).collect::<Vec<_>>()
        );
        assert_eq!(comp.results[0].extra.as_deref(), Some("vim"));
    }

    #[test]
    fn filter_vim_prefers_vim_mode() {
        let mut comp = Completer::settings(&test_state(true));
        comp.update_query("vim".into());
        assert_eq!(comp.results[0].extra.as_deref(), Some("vim"));
    }

    #[test]
    fn filter_speed_shows_tps() {
        let mut comp = Completer::settings(&test_state(false));
        comp.update_query("speed".into());
        assert_eq!(comp.results[0].extra.as_deref(), Some("show_tps"));
    }

    #[test]
    fn filter_thinking_shows_show_thinking() {
        let mut comp = Completer::settings(&test_state(false));
        comp.update_query("thinking".into());
        assert!(
            comp.results
                .iter()
                .any(|item| item.extra.as_deref() == Some("show_thinking")),
            "results: {:?}",
            comp.results.iter().map(|i| &i.label).collect::<Vec<_>>()
        );
    }

    #[test]
    fn filter_thinking_prefers_show_thinking() {
        let mut comp = Completer::settings(&test_state(false));
        comp.update_query("thinking".into());
        assert_eq!(comp.results[0].extra.as_deref(), Some("show_thinking"));
    }

    #[test]
    fn filter_query_change_from_selected_item_selects_first_visible_result() {
        let mut comp = Completer::settings(&test_state(false));
        comp.update_query("th".into());
        let show_tps_idx = comp
            .results
            .iter()
            .position(|item| item.extra.as_deref() == Some("show_tps"))
            .unwrap();
        comp.selected = show_tps_idx;

        comp.update_query("thi".into());
        assert_eq!(comp.selected, 0);
        assert_eq!(comp.results[0].extra.as_deref(), Some("show_thinking"));
    }

    #[test]
    fn update_query_resets_selection_to_first_result() {
        let mut comp = Completer::settings(&test_state(false));
        comp.move_down();
        comp.move_down();
        assert_ne!(comp.selected, 0);

        comp.update_query("thinking".into());
        assert_eq!(comp.selected, 0);
        assert_eq!(comp.results[0].extra.as_deref(), Some("show_thinking"));
    }

    #[test]
    fn toggle_preserves_selected_after_refresh() {
        let mut comp = Completer::settings(&test_state(false));
        // Navigate down to "auto compact" (index 1)
        comp.move_down();
        assert_eq!(comp.accept_extra(), Some("auto_compact"));

        // Refresh with auto_compact toggled
        let mut toggled = test_state(false);
        toggled.auto_compact = true;
        comp.refresh_items(Completer::settings_items(&toggled));
        assert_eq!(
            comp.accept_extra(),
            Some("auto_compact"),
            "selection should stay on auto_compact"
        );
        assert_eq!(
            comp.selected_item().unwrap().description.as_deref(),
            Some("on")
        );
    }

    #[test]
    fn accept_extra_on_filtered_single_result() {
        let mut comp = Completer::settings(&test_state(false));
        comp.update_query("auto".into());
        assert_eq!(comp.selected, 0);
        let key = comp.accept_extra();
        assert_eq!(key, Some("auto_compact"));
    }
}
