//! Rule-set types, compilation, mode construction, and pattern matching.
//!
//! This module owns the static permission rules loaded from config:
//! - `Decision`, `RuleSet`, `ModePerms`
//! - Raw deserialization types and merge helpers
//! - `DEFAULT_BASH_ALLOW` safe-read-only-command list
//! - `build_mode` (materializes a `ModePerms` for one Mode)
//! - `check_ruleset` (the core pattern-matching decision)

use protocol::Mode;
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct RawRuleSet {
    pub(super) allow: Vec<String>,
    pub(super) ask: Vec<String>,
    pub(super) deny: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct RawModePerms {
    pub(super) tools: RawRuleSet,
    pub(super) bash: RawRuleSet,
    pub(super) web_fetch: RawRuleSet,
    pub(super) mcp: RawRuleSet,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct RawPerms {
    pub(super) default: RawModePerms,
    pub(super) normal: RawModePerms,
    pub(super) plan: RawModePerms,
    pub(super) apply: RawModePerms,
    pub(super) yolo: RawModePerms,
}

fn merge_ruleset(default: &RawRuleSet, mode: &RawRuleSet) -> RawRuleSet {
    RawRuleSet {
        allow: default.allow.iter().chain(&mode.allow).cloned().collect(),
        ask: default.ask.iter().chain(&mode.ask).cloned().collect(),
        deny: default.deny.iter().chain(&mode.deny).cloned().collect(),
    }
}

pub(super) fn merge_mode(default: &RawModePerms, mode: &RawModePerms) -> RawModePerms {
    RawModePerms {
        tools: merge_ruleset(&default.tools, &mode.tools),
        bash: merge_ruleset(&default.bash, &mode.bash),
        web_fetch: merge_ruleset(&default.web_fetch, &mode.web_fetch),
        mcp: merge_ruleset(&default.mcp, &mode.mcp),
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct RawConfig {
    pub(super) permissions: RawPerms,
}

#[derive(Debug, Clone)]
pub struct RuleSet {
    pub(super) allow: Vec<glob::Pattern>,
    pub(super) ask: Vec<glob::Pattern>,
    pub(super) deny: Vec<glob::Pattern>,
}

#[derive(Debug, Clone)]
pub(super) struct ModePerms {
    pub(super) tools: HashMap<String, Decision>,
    pub(super) bash: RuleSet,
    pub(super) web_fetch: RuleSet,
    pub(super) mcp: RuleSet,
}

pub(super) fn compile_patterns(raw: &[String]) -> Vec<glob::Pattern> {
    raw.iter()
        .filter_map(|s| glob::Pattern::new(s).ok())
        .collect()
}

fn build_tool_map(raw: &RawRuleSet) -> HashMap<String, Decision> {
    let mut map = HashMap::new();
    for name in &raw.allow {
        map.insert(name.clone(), Decision::Allow);
    }
    for name in &raw.ask {
        map.insert(name.clone(), Decision::Ask);
    }
    // Deny wins — inserted last so it overwrites allow/ask
    for name in &raw.deny {
        map.insert(name.clone(), Decision::Deny);
    }
    map
}

/// Default bash patterns that are allowed without explicit approval.
/// Used by both permissions checking and approval pattern suggestions.
pub const DEFAULT_BASH_ALLOW: &[&str] = &[
    // Directory listing & file search
    "ls *",
    "find *",
    "tree *",
    // Text viewing
    "cat *",
    "head *",
    "tail *",
    "less *",
    // Text search & processing (read-only)
    "grep *",
    "sort *",
    "uniq *",
    "wc *",
    "diff *",
    "tr *",
    "cut *",
    "jq *",
    // Path & file info
    "echo *",
    "pwd *",
    "which *",
    "dirname *",
    "basename *",
    "realpath *",
    "stat *",
    "file *",
    "test *",
    // Disk & system info
    "du *",
    "df *",
    "date *",
    "whoami *",
    // Binary inspection
    "sha256sum *",
    "md5sum *",
    "xxd *",
    "hexdump *",
    "strings *",
];

pub(super) fn build_mode(raw: &RawModePerms, mode: Mode) -> ModePerms {
    let mut tools = build_tool_map(&raw.tools);

    if mode == Mode::Yolo {
        // Yolo defaults: everything allowed unless explicitly overridden.
        // Any tool not in the map will also default to Allow via check_tool().
        for name in [
            "read_file",
            "edit_file",
            "write_file",
            "glob",
            "grep",
            "ask_user_question",
            "bash",
            "web_fetch",
            "web_search",
            "read_process_output",
            "stop_process",
        ] {
            tools.entry(name.to_string()).or_insert(Decision::Allow);
        }
    } else {
        // read_file: allow in all non-yolo modes
        tools
            .entry("read_file".to_string())
            .or_insert(Decision::Allow);

        // edit_file: ask in normal/plan, allow in apply
        let default_edit = if mode == Mode::Apply {
            Decision::Allow
        } else {
            Decision::Ask
        };
        tools.entry("edit_file".to_string()).or_insert(default_edit);

        // write_file: ask in normal/plan, allow in apply
        let default_write = if mode == Mode::Apply {
            Decision::Allow
        } else {
            Decision::Ask
        };
        tools
            .entry("write_file".to_string())
            .or_insert(default_write);

        tools.entry("glob".to_string()).or_insert(Decision::Allow);
        tools.entry("grep".to_string()).or_insert(Decision::Allow);
        tools
            .entry("ask_user_question".to_string())
            .or_insert(Decision::Allow);
    }

    // Multi-agent tools: always allowed in all modes (they're coordination
    // primitives, not filesystem operations).
    for name in [
        "spawn_agent",
        "stop_agent",
        "list_agents",
        "message_agent",
        "peek_agent",
    ] {
        tools.entry(name.to_string()).or_insert(Decision::Allow);
    }

    let mut bash_allow = compile_patterns(&raw.bash.allow);
    if bash_allow.is_empty() {
        if mode == Mode::Yolo {
            bash_allow = vec![glob::Pattern::new("*").unwrap()];
        } else {
            bash_allow = compile_patterns(
                &DEFAULT_BASH_ALLOW
                    .iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>(),
            );
        }
    }

    let mut web_allow = compile_patterns(&raw.web_fetch.allow);
    if mode == Mode::Yolo && web_allow.is_empty() {
        web_allow = vec![glob::Pattern::new("*").unwrap()];
    }

    let mut mcp_allow = compile_patterns(&raw.mcp.allow);
    if mode == Mode::Yolo && mcp_allow.is_empty() {
        mcp_allow = vec![glob::Pattern::new("*").unwrap()];
    }

    ModePerms {
        tools,
        bash: RuleSet {
            allow: bash_allow,
            ask: compile_patterns(&raw.bash.ask),
            deny: compile_patterns(&raw.bash.deny),
        },
        web_fetch: RuleSet {
            allow: web_allow,
            ask: compile_patterns(&raw.web_fetch.ask),
            deny: compile_patterns(&raw.web_fetch.deny),
        },
        mcp: RuleSet {
            allow: mcp_allow,
            ask: compile_patterns(&raw.mcp.ask),
            deny: compile_patterns(&raw.mcp.deny),
        },
    }
}

fn matches_rule(pat: &glob::Pattern, value: &str) -> bool {
    // Match both the value as-is and with a trailing space to handle
    // patterns like "ls *" matching bare "ls" (no arguments).
    pat.matches(value) || pat.matches(&format!("{value} "))
}

pub(super) fn check_ruleset(ruleset: &RuleSet, value: &str) -> Decision {
    // Deny always wins — checked first regardless of specificity.
    for pat in &ruleset.deny {
        if matches_rule(pat, value) {
            return Decision::Deny;
        }
    }

    // Among allow and ask, the most specific (longest pattern) wins.
    // On tie, ask wins (safer default). Pattern length is a heuristic for
    // specificity — works well for typical patterns like "git *" vs "git push *".
    let mut best_allow: Option<usize> = None;
    let mut best_ask: Option<usize> = None;

    for pat in &ruleset.allow {
        if matches_rule(pat, value) {
            let len = pat.as_str().len();
            if best_allow.is_none_or(|prev| len > prev) {
                best_allow = Some(len);
            }
        }
    }
    for pat in &ruleset.ask {
        if matches_rule(pat, value) {
            let len = pat.as_str().len();
            if best_ask.is_none_or(|prev| len > prev) {
                best_ask = Some(len);
            }
        }
    }

    match (best_allow, best_ask) {
        (Some(a), Some(k)) => {
            if k >= a {
                Decision::Ask
            } else {
                Decision::Allow
            }
        }
        (Some(_), None) => Decision::Allow,
        (None, Some(_)) => Decision::Ask,
        (None, None) => Decision::Ask,
    }
}
