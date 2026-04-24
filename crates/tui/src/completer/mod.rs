use score::{query_match_score, split_words};

pub mod command;
pub mod file;
pub mod history;
mod score;

#[derive(Clone, Default)]
pub struct CompletionItem {
    pub label: String,
    pub description: Option<String>,
    /// Optional ANSI color. When set, the row's pill + label + description
    /// are all painted in this color.
    pub ansi_color: Option<u8>,
    /// Extra terms to match against when filtering — label and this
    /// field are both scanned. Lets a plugin match on hidden fields
    /// (provider + key for the model picker) without displaying them.
    pub search_terms: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum CompleterKind {
    File,
    Command,
    CommandArg,
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

    /// Maximum rows to display for the inline completer popup.
    pub fn max_visible_rows(&self) -> usize {
        5
    }

    fn item_key(item: &CompletionItem) -> &str {
        &item.label
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
        if let Some(t) = item.search_terms.as_deref() {
            fields.push(t.to_lowercase());
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
                    let fields = self.search_fields(item);
                    let score = query_match_score(&query, &query_words, &fields)?;
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
/// One owner, one lifecycle: created when a completer opens, destroyed
/// when it closes.
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
