use std::collections::HashSet;
use std::process::Command;

use super::{Completer, CompleterKind, CompletionItem};

impl Completer {
    pub fn files(anchor: usize) -> Self {
        let all_items: Vec<CompletionItem> = git_files()
            .into_iter()
            .map(|f| CompletionItem {
                label: f,
                ..Default::default()
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
            selected_key: None,
        }
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
