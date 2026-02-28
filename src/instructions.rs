use crate::config;
use std::path::{Path, PathBuf};

const FILENAME: &str = "AGENTS.md";

/// Discover AGENTS.md files from root (config dir) and workspace (cwd) levels.
fn paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();

    // Root level: ~/.config/agent/AGENTS.md
    let root = config::config_dir().join(FILENAME);
    if root.exists() {
        paths.push(root);
    }

    // Workspace level: walk up from cwd to find the nearest AGENTS.md
    if let Ok(cwd) = std::env::current_dir() {
        let mut dir: Option<&Path> = Some(cwd.as_path());
        while let Some(d) = dir {
            let candidate = d.join(FILENAME);
            if candidate.exists() {
                // Avoid duplicating the root-level file
                if paths.first().is_none_or(|r| *r != candidate) {
                    paths.push(candidate);
                }
                break;
            }
            dir = d.parent();
        }
    }

    paths
}

/// Load all discovered AGENTS.md files into a single string suitable for
/// appending to the system prompt. Returns `None` if no files are found.
pub fn load() -> Option<String> {
    let files = paths();
    if files.is_empty() {
        return None;
    }

    let mut sections = Vec::new();
    for path in &files {
        if let Ok(content) = std::fs::read_to_string(path) {
            if !content.trim().is_empty() {
                sections.push(format!(
                    "Instructions from {}:\n{}",
                    path.display(),
                    content.trim()
                ));
            }
        }
    }

    if sections.is_empty() {
        return None;
    }

    Some(sections.join("\n\n"))
}
