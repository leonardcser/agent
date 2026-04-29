use std::collections::HashSet;

use super::{Completer, CompleterKind, CompletionItem};

impl Completer {
    pub fn is_command(s: &str) -> bool {
        let base = s.split_whitespace().next().unwrap_or(s);
        let slash_name = base.strip_prefix('/').unwrap_or("");
        crate::app::commands::is_rust_command(slash_name)
            || crate::custom_commands::is_custom_command(s)
            || crate::lua::is_lua_command(s)
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

    pub fn commands(anchor: usize) -> Self {
        let mut all_items: Vec<CompletionItem> = crate::app::commands::rust_command_items()
            .map(|(label, desc)| CompletionItem {
                label: label.into(),
                description: Some(desc.into()),
                ..Default::default()
            })
            .collect();
        let custom = crate::custom_commands::list();
        let custom_names: HashSet<&str> = custom.iter().map(|(n, _)| n.as_str()).collect();
        for (name, desc) in crate::builtin_commands::list() {
            if custom_names.contains(name.as_str()) {
                continue;
            }
            all_items.push(CompletionItem {
                label: name,
                description: if desc.is_empty() { None } else { Some(desc) },
                ..Default::default()
            });
        }
        for (name, desc) in custom {
            all_items.push(CompletionItem {
                label: name,
                description: if desc.is_empty() { None } else { Some(desc) },
                ..Default::default()
            });
        }
        let mut seen: HashSet<String> = all_items.iter().map(|i| i.label.clone()).collect();
        for (name, desc) in crate::lua::list_commands() {
            if !seen.insert(name.clone()) {
                continue;
            }
            all_items.push(CompletionItem {
                label: name,
                description: desc,
                ..Default::default()
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
            selected_key: None,
        }
    }

    pub fn command_args(anchor: usize, items: &[String]) -> Self {
        let all_items: Vec<CompletionItem> = items
            .iter()
            .map(|s| CompletionItem {
                label: s.clone(),
                ..Default::default()
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
            selected_key: None,
        }
    }
}
