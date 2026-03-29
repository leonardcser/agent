use std::collections::HashSet;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

static MULTI_AGENT_ENABLED: AtomicBool = AtomicBool::new(false);

pub fn set_multi_agent(enabled: bool) {
    MULTI_AGENT_ENABLED.store(enabled, Ordering::Relaxed);
}

#[derive(Clone)]
pub struct CompletionItem {
    pub label: String,
    pub description: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum CompleterKind {
    File,
    Command,
    CommandArg,
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
        let base = s.split_whitespace().next().unwrap_or(s);
        let slash_name = base.strip_prefix('/').unwrap_or("");
        Self::command_items()
            .iter()
            .any(|(label, _)| *label == slash_name)
            || crate::custom_commands::is_custom_command(s)
    }

    /// Returns the argument hint for a command that accepts arguments.
    /// The result is `(prefix, hint)` where prefix is the `/cmd` part
    /// and hint is displayed dimmed after the prefix (e.g. preset names
    /// joined with ` | ` or a `<placeholder>`).
    ///
    /// `arg_sources` provides the dynamic completion labels for commands
    /// like `/model`, `/theme`, `/color`.
    pub fn command_hint(
        buf: &str,
        arg_sources: &[(String, Vec<String>)],
    ) -> Option<(String, String)> {
        let cmd = buf.split_whitespace().next()?;
        match cmd {
            "/btw" => Some(("/btw".into(), "<question>".into())),
            "/compact" => Some(("/compact".into(), "<instructions>".into())),
            _ => {
                for (prefix, items) in arg_sources {
                    if cmd == prefix {
                        let hint = format!("<{}>", items.join("|"));
                        return Some((prefix.clone(), hint));
                    }
                }
                if crate::custom_commands::is_custom_command(cmd) {
                    return Some((cmd.into(), "<instructions>".into()));
                }
                None
            }
        }
    }

    fn command_items() -> &'static [(&'static str, &'static str)] {
        &[
            ("clear", "start new conversation"),
            ("new", "start new conversation"),
            ("resume", "resume saved session"),
            ("rewind", "rewind to a previous turn"),
            ("vim", "toggle vim mode"),
            ("model", "switch model"),
            ("settings", "open settings menu"),
            ("compact", "compact conversation history"),
            ("export", "copy conversation to clipboard"),
            ("fork", "fork current session"),
            ("branch", "fork current session"),
            ("stats", "show token usage statistics"),
            ("theme", "change accent color"),
            ("color", "set task slug color"),
            ("btw", "ask a side question"),
            ("permissions", "manage session permissions"),
            ("ps", "manage background processes"),
            ("agents", "manage running agents"),
            ("exit", "exit the app"),
            ("quit", "exit the app"),
        ]
    }

    pub fn commands(anchor: usize) -> Self {
        let multi_agent = MULTI_AGENT_ENABLED.load(Ordering::Relaxed);
        let mut all_items: Vec<CompletionItem> = Self::command_items()
            .iter()
            .filter(|&&(label, _)| label != "agents" || multi_agent)
            .map(|&(label, desc)| CompletionItem {
                label: label.into(),
                description: Some(desc.into()),
            })
            .collect();
        for (name, desc) in crate::custom_commands::list() {
            all_items.push(CompletionItem {
                label: name,
                description: if desc.is_empty() { None } else { Some(desc) },
            });
        }
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

    pub fn command_args(anchor: usize, items: &[String]) -> Self {
        let all_items: Vec<CompletionItem> = items
            .iter()
            .map(|s| CompletionItem {
                label: s.clone(),
                description: None,
            })
            .collect();
        let results = all_items.clone();
        Self {
            anchor,
            kind: CompleterKind::CommandArg,
            query: String::new(),
            results,
            selected: 0,
            all_items,
        }
    }

    pub fn history(entries: &[String]) -> Self {
        let mut seen = HashSet::new();
        let all_items: Vec<CompletionItem> = entries
            .iter()
            .rev()
            .filter(|text| seen.insert(text.as_str()))
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
        let _perf = crate::perf::begin("completer_filter");
        if self.query.is_empty() {
            self.results = self.all_items.clone();
        } else {
            let mut scored: Vec<_> = self
                .all_items
                .iter()
                .enumerate()
                .filter_map(|(i, item)| {
                    let score = if self.kind == CompleterKind::History {
                        history_score(&item.label, &self.query, i)
                    } else {
                        crate::fuzzy::fuzzy_score(&item.label, &self.query)
                    }?;
                    Some((score, i, item.clone()))
                })
                .collect();
            scored.sort_by_key(|(s, i, _)| (*s, *i));
            self.results = scored.into_iter().map(|(_, _, item)| item).collect();
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

fn history_score(text: &str, query: &str, recency_rank: usize) -> Option<u32> {
    let base = crate::fuzzy::fuzzy_score(text, query)? as i64;
    let text_norm = text.trim().to_lowercase();
    let query_norm = query.trim().to_lowercase();

    if query_norm.is_empty() {
        return Some(recency_rank as u32);
    }

    let text_words = split_words(&text_norm);
    let query_words = split_words(&query_norm);
    let query_has_multiple_words = query_words.len() > 1;

    let mut score = base * 10;

    if text_norm == query_norm {
        score -= 2_000;
    } else if text_norm.starts_with(&query_norm) {
        score -= 200;
    }

    if !query_has_multiple_words && text_words.len() > 1 {
        score += ((text_words.len() - 1) as i64) * 60;
    }

    let mut saw_exact_word_match = false;
    let mut saw_prefix_word_match = false;
    let mut saw_substring_match = false;

    for word in &query_words {
        if text_words.iter().any(|candidate| candidate == word) {
            saw_exact_word_match = true;
            score -= 400;
        } else if text_words
            .iter()
            .any(|candidate| candidate.starts_with(word))
        {
            saw_prefix_word_match = true;
            score -= 140;
        } else if text_norm.contains(word) {
            saw_substring_match = true;
            score -= 40;
        }
    }

    if !query_has_multiple_words {
        if let Some(first_word) = query_words.first() {
            let boundary_prefix_matches = text_words
                .iter()
                .filter(|candidate| candidate.starts_with(first_word))
                .count();
            if boundary_prefix_matches > 0 {
                score -= 80;
            }
        }
    }

    if !query_has_multiple_words {
        // For single-word reverse search, plain fuzzy subsequence matches like
        // "default allow" for "full" should come well after true word hits.
        if !saw_exact_word_match && !saw_prefix_word_match && !saw_substring_match {
            score += 900;
        }
    }

    score -= recency_bonus(recency_rank);

    Some(score.max(0) as u32)
}

fn split_words(text: &str) -> Vec<&str> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect()
}

fn recency_bonus(recency_rank: usize) -> i64 {
    // History items are stored newest-first. Give recent entries a material
    // advantage without overpowering exact or whole-word matches.
    match recency_rank {
        0..=4 => 180 - (recency_rank as i64 * 20),
        5..=14 => 90 - ((recency_rank as i64 - 5) * 6),
        _ => 0,
    }
}

/// Get tracked + untracked (but not ignored) files and directories via git.
/// Falls back to a filesystem walk when not inside a git repository.
fn git_files() -> Vec<String> {
    let output = Command::new("git")
        .args(["ls-files", "--cached", "--others", "--exclude-standard"])
        .output();
    let lines: Vec<String> = match output {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout);
            s.lines()
                .filter(|l| !l.is_empty())
                .map(|l| l.to_string())
                .collect()
        }
        _ => return walk_cwd_files(),
    };
    let mut dirs = HashSet::new();
    let mut entries: Vec<String> = lines
        .iter()
        .flat_map(|l| {
            let mut parts = Vec::new();
            let mut prefix = String::new();
            for component in std::path::Path::new(l)
                .parent()
                .into_iter()
                .flat_map(|p| p.components())
            {
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(&component.as_os_str().to_string_lossy());
                if dirs.insert(prefix.clone()) {
                    parts.push(prefix.clone());
                }
            }
            parts.push(l.to_string());
            parts
        })
        .collect();
    entries.sort();
    entries
}

/// Recursively walk the cwd collecting files and directories (non-git fallback).
fn walk_cwd_files() -> Vec<String> {
    use std::fs;
    use std::path::Path;

    const IGNORED: &[&str] = &[
        ".git",
        "node_modules",
        "target",
        "__pycache__",
        ".venv",
        "venv",
        ".tox",
        "dist",
        "build",
        ".next",
    ];
    const MAX_DEPTH: usize = 6;
    const MAX_ENTRIES: usize = 5000;

    let mut entries = Vec::new();
    let mut dirs = HashSet::new();
    let mut stack: Vec<(String, usize)> = vec![(String::new(), 0)];

    while let Some((prefix, depth)) = stack.pop() {
        if entries.len() >= MAX_ENTRIES {
            break;
        }
        let dir_path = if prefix.is_empty() {
            ".".to_string()
        } else {
            prefix.clone()
        };
        let read = match fs::read_dir(&dir_path) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for entry in read.flatten() {
            if entries.len() >= MAX_ENTRIES {
                break;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || IGNORED.contains(&name.as_str()) {
                continue;
            }
            let rel = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{}/{}", prefix, name)
            };
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if is_dir {
                if dirs.insert(rel.clone()) {
                    entries.push(rel.clone());
                }
                if depth < MAX_DEPTH {
                    stack.push((rel, depth + 1));
                }
            } else {
                // Also collect parent dirs.
                let mut dir_prefix = String::new();
                for component in Path::new(&rel)
                    .parent()
                    .into_iter()
                    .flat_map(|p| p.components())
                {
                    if !dir_prefix.is_empty() {
                        dir_prefix.push('/');
                    }
                    dir_prefix.push_str(&component.as_os_str().to_string_lossy());
                    if dirs.insert(dir_prefix.clone()) {
                        entries.push(dir_prefix.clone());
                    }
                }
                entries.push(rel);
            }
        }
    }
    entries.sort();
    entries
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
