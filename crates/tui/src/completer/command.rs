use std::collections::HashSet;
use std::sync::atomic::Ordering;

use super::{Completer, CompleterKind, CompletionItem, MULTI_AGENT_ENABLED};

impl Completer {
    pub fn is_command(s: &str) -> bool {
        let base = s.split_whitespace().next().unwrap_or(s);
        let slash_name = base.strip_prefix('/').unwrap_or("");
        Self::command_items()
            .iter()
            .any(|(label, _)| *label == slash_name)
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

    fn command_items() -> &'static [(&'static str, &'static str)] {
        &[
            ("clear", "start new conversation"),
            ("new", "start new conversation"),
            ("resume", "resume saved session"),
            ("rewind", "rewind to a previous turn"),
            ("vim", "toggle vim mode"),
            ("thinking", "toggle thinking blocks"),
            ("model", "switch model"),
            ("settings", "open settings menu"),
            ("compact", "compact conversation history"),
            ("export", "copy conversation to clipboard"),
            ("fork", "fork current session"),
            ("branch", "fork current session"),
            ("stats", "show token usage statistics"),
            ("cost", "show session cost"),
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
            original_value: None,
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
            original_value: None,
        }
    }
}
