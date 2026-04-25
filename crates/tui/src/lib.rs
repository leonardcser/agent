pub mod alloc;
pub mod api;
pub mod app;
pub mod attachment;
pub mod builtin_commands;
pub mod completer;
pub mod config;
pub mod content;
pub mod custom_commands;
pub mod format;
pub mod fuzzy;
pub mod input;
pub mod instructions;
pub mod keymap;
pub mod lua;
pub mod metrics;
pub mod perf;
pub mod persist;
pub mod prompt_sections;
pub mod session;
pub mod sleep_inhibit;
pub mod state;
pub mod theme;
pub mod utils;
pub mod vim;
pub mod window;
pub mod workspace_permissions;

/// Expand `@path` and `"@path with spaces"` references in user input:
/// if the path exists, append the file/directory contents.
pub fn expand_at_refs(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut refs: Vec<String> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if let Some((_, path, end)) = content::scan_at_token(&chars, i) {
            let resolved = path.trim_end_matches([',', '.']);
            if !resolved.is_empty() && std::path::Path::new(resolved).exists() {
                refs.push(resolved.to_string());
            }
            i = end;
        } else {
            i += 1;
        }
    }

    if refs.is_empty() {
        return input.to_string();
    }

    let mut result = input.to_string();
    for path in &refs {
        let p = std::path::Path::new(path);
        if p.is_dir() {
            if let Ok(output) = std::process::Command::new("ls")
                .arg("-1")
                .arg(path)
                .output()
            {
                let listing = String::from_utf8_lossy(&output.stdout);
                result.push_str(&format!(
                    "\n\nDirectory listing of {}:\n```\n{}\n```",
                    path,
                    listing.trim_end()
                ));
            }
        } else if let Ok(contents) = std::fs::read_to_string(path) {
            result.push_str(&format!(
                "\n\nContents of {}:\n```\n{}\n```",
                path, contents
            ));
        }
    }
    result
}
