pub mod app;
pub mod completer;
pub mod config;
pub mod input;
pub mod instructions;
pub mod perf;
pub mod render;
pub mod session;
pub mod state;
pub mod theme;
pub mod utils;
pub mod vim;

/// Expand @path references in user input: if a token starts with @ and
/// the rest is an existing file path, append the file contents.
pub fn expand_at_refs(input: &str) -> String {
    let mut refs: Vec<String> = Vec::new();
    let mut chars = input.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c != '@' {
            continue;
        }
        let start = i + 1;
        let mut end = start;
        while let Some(&(j, nc)) = chars.peek() {
            if nc.is_whitespace() {
                break;
            }
            end = j + nc.len_utf8();
            chars.next();
        }
        if end > start {
            let path = &input[start..end];
            if std::path::Path::new(path).exists() {
                refs.push(path.to_string());
            }
        }
    }

    if refs.is_empty() {
        return input.to_string();
    }

    let mut result = input.to_string();
    for path in &refs {
        let p = std::path::Path::new(path);
        if p.is_dir() {
            if let Ok(output) = std::process::Command::new("ls").arg("-1").arg(path).output() {
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
