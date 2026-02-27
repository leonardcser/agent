use std::process::Command;

pub struct CompletionItem {
    pub label: String,
    pub description: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum CompleterKind {
    File,
    Command,
    History,
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
    all_items: Vec<CompletionItem>,
}

impl Completer {
    pub fn files(anchor: usize) -> Self {
        let all_items: Vec<CompletionItem> = git_files()
            .into_iter()
            .map(|f| CompletionItem {
                label: f,
                description: None,
            })
            .collect();
        let results = all_items.clone();
        Self {
            anchor,
            kind: CompleterKind::File,
            query: String::new(),
            results,
            selected: 0,
            all_items,
        }
    }

    pub fn is_command(s: &str) -> bool {
        Self::command_items()
            .iter()
            .any(|(label, _)| s == format!("/{}", label))
    }

    fn command_items() -> Vec<(&'static str, &'static str)> {
        vec![
            ("clear", "clear conversation"),
            ("new", "start new conversation"),
            ("resume", "resume saved session"),
            ("vim", "toggle vim mode"),
            ("settings", "open settings menu"),
            ("exit", "exit the app"),
            ("quit", "exit the app"),
        ]
    }

    pub fn commands(anchor: usize) -> Self {
        let all_items = Self::command_items()
            .into_iter()
            .map(|(label, desc)| CompletionItem {
                label: label.into(),
                description: Some(desc.into()),
            })
            .collect::<Vec<_>>();
        let results = all_items.clone();
        Self {
            anchor,
            kind: CompleterKind::Command,
            query: String::new(),
            results,
            selected: 0,
            all_items,
        }
    }

    pub fn history(entries: &[String]) -> Self {
        let all_items: Vec<CompletionItem> = entries
            .iter()
            .rev()
            .map(|text| {
                let label = text
                    .trim_start()
                    .lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty())
                    .unwrap_or("")
                    .to_string();
                CompletionItem {
                    label,
                    description: None,
                }
            })
            .collect();
        let results = all_items.clone();
        Self {
            anchor: 0,
            kind: CompleterKind::History,
            query: String::new(),
            results,
            selected: 0,
            all_items,
        }
    }

    pub fn update_query(&mut self, query: String) {
        self.query = query;
        self.filter();
    }

    fn filter(&mut self) {
        if self.query.is_empty() {
            self.results = self.all_items.clone();
        } else {
            let q = self.query.to_lowercase();
            self.results = self
                .all_items
                .iter()
                .filter(|item| fuzzy_match(&item.label, &q))
                .cloned()
                .collect();
        }
        if self.selected >= self.results.len() {
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
        }
    }

    pub fn move_down(&mut self) {
        if !self.results.is_empty() {
            self.selected = (self.selected + 1) % self.results.len();
        }
    }

    pub fn accept(&self) -> Option<&str> {
        self.results.get(self.selected).map(|i| i.label.as_str())
    }
}

impl Clone for CompletionItem {
    fn clone(&self) -> Self {
        Self {
            label: self.label.clone(),
            description: self.description.clone(),
        }
    }
}

/// Fuzzy match: all query chars appear in order in the path (case-insensitive).
fn fuzzy_match(path: &str, query: &str) -> bool {
    let lower = path.to_lowercase();
    let mut hay = lower.chars().peekable();
    for qc in query.chars() {
        loop {
            match hay.next() {
                Some(pc) if pc == qc => break,
                Some(_) => continue,
                None => return false,
            }
        }
    }
    true
}

/// Get tracked + untracked (but not ignored) files via git.
fn git_files() -> Vec<String> {
    let output = Command::new("git")
        .args(["ls-files", "--cached", "--others", "--exclude-standard"])
        .output();
    match output {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            let mut files: Vec<String> = s
                .lines()
                .filter(|l| !l.is_empty())
                .map(String::from)
                .collect();
            files.sort();
            files
        }
        Err(_) => Vec::new(),
    }
}
