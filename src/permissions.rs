use crate::input::Mode;
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
struct RawRuleSet {
    allow: Vec<String>,
    ask: Vec<String>,
    deny: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawModePerms {
    tools: RawRuleSet,
    bash: RawRuleSet,
    web_fetch: RawRuleSet,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawPerms {
    normal: RawModePerms,
    apply: RawModePerms,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawConfig {
    permissions: RawPerms,
}

#[derive(Debug, Clone)]
struct RuleSet {
    allow: Vec<glob::Pattern>,
    ask: Vec<glob::Pattern>,
    deny: Vec<glob::Pattern>,
}

#[derive(Debug, Clone)]
struct ModePerms {
    tools: HashMap<String, Decision>,
    bash: RuleSet,
    web_fetch: RuleSet,
}

#[derive(Debug, Clone)]
pub struct Permissions {
    normal: ModePerms,
    plan: ModePerms,
    apply: ModePerms,
}

fn compile_patterns(raw: &[String]) -> Vec<glob::Pattern> {
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

fn build_mode(raw: &RawModePerms, mode: Mode) -> ModePerms {
    let mut tools = build_tool_map(&raw.tools);

    // Set default permissions for tools if not explicitly configured
    // read_file: allow in both modes by default
    tools
        .entry("read_file".to_string())
        .or_insert(Decision::Allow);

    // edit_file: ask in normal mode, allow in apply mode
    let default_edit_file = if mode == Mode::Apply {
        Decision::Allow
    } else {
        Decision::Ask
    };
    tools
        .entry("edit_file".to_string())
        .or_insert(default_edit_file);

    // write_file: ask in normal mode, allow in apply mode
    let default_write_file = if mode == Mode::Apply {
        Decision::Allow
    } else {
        Decision::Ask
    };
    tools
        .entry("write_file".to_string())
        .or_insert(default_write_file);

    // glob: always allow by default in both modes
    tools.entry("glob".to_string()).or_insert(Decision::Allow);

    // grep: always allow by default in both modes
    tools.entry("grep".to_string()).or_insert(Decision::Allow);

    // ask_user_question: always allow
    tools
        .entry("ask_user_question".to_string())
        .or_insert(Decision::Allow);

    // exit_plan_mode: only in plan mode
    let default_exit_plan = if mode == Mode::Plan {
        Decision::Allow
    } else {
        Decision::Deny
    };
    tools
        .entry("exit_plan_mode".to_string())
        .or_insert(default_exit_plan);

    const DEFAULT_BASH_ALLOW: &[&str] = &["ls *", "grep *", "find *"];
    let mut bash_allow = compile_patterns(&raw.bash.allow);
    if bash_allow.is_empty() {
        bash_allow = compile_patterns(
            &DEFAULT_BASH_ALLOW
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
        );
    }

    ModePerms {
        tools,
        bash: RuleSet {
            allow: bash_allow,
            ask: compile_patterns(&raw.bash.ask),
            deny: compile_patterns(&raw.bash.deny),
        },
        web_fetch: RuleSet {
            allow: compile_patterns(&raw.web_fetch.allow),
            ask: compile_patterns(&raw.web_fetch.ask),
            deny: compile_patterns(&raw.web_fetch.deny),
        },
    }
}

impl Permissions {
    pub fn load() -> Self {
        let path = crate::config::config_dir().join("config.yaml");
        let contents = std::fs::read_to_string(&path).unwrap_or_default();
        let raw: RawConfig = serde_yml::from_str(&contents).unwrap_or_default();
        Self {
            normal: build_mode(&raw.permissions.normal, Mode::Normal),
            plan: build_mode(&raw.permissions.normal, Mode::Plan),
            apply: build_mode(&raw.permissions.apply, Mode::Apply),
        }
    }

    pub fn check_tool(&self, mode: Mode, tool_name: &str) -> Decision {
        if mode == Mode::Yolo {
            return Decision::Allow;
        }
        let perms = match mode {
            Mode::Normal => &self.normal,
            Mode::Plan => &self.plan,
            Mode::Apply => &self.apply,
            Mode::Yolo => unreachable!(),
        };
        perms.tools.get(tool_name).cloned().unwrap_or(Decision::Ask)
    }

    pub fn check_tool_pattern(&self, mode: Mode, tool_name: &str, pattern: &str) -> Decision {
        if mode == Mode::Yolo {
            return Decision::Allow;
        }
        let perms = match mode {
            Mode::Normal => &self.normal,
            Mode::Plan => &self.plan,
            Mode::Apply => &self.apply,
            Mode::Yolo => unreachable!(),
        };
        let ruleset = match tool_name {
            "web_fetch" => &perms.web_fetch,
            _ => return Decision::Ask,
        };
        check_ruleset(ruleset, pattern)
    }

    pub fn check_bash(&self, mode: Mode, command: &str) -> Decision {
        if mode == Mode::Yolo {
            return Decision::Allow;
        }
        let perms = match mode {
            Mode::Normal => &self.normal,
            Mode::Plan => &self.plan,
            Mode::Apply => &self.apply,
            Mode::Yolo => unreachable!(),
        };
        // Split on shell operators and check each sub-command independently.
        // The most restrictive result wins (Deny > Ask > Allow).
        let subcmds = split_shell_commands(command);
        if subcmds.len() <= 1 {
            return check_ruleset(&perms.bash, command);
        }
        let mut worst = Decision::Allow;
        for subcmd in subcmds {
            let d = check_ruleset(&perms.bash, subcmd);
            match d {
                Decision::Deny => return Decision::Deny,
                Decision::Ask if worst == Decision::Allow => worst = Decision::Ask,
                _ => {}
            }
        }
        worst
    }
}

/// Split a command string on shell operators (&&, ||, ;, |) and return
/// the individual sub-commands (trimmed).
fn split_shell_commands(cmd: &str) -> Vec<&str> {
    let mut commands = Vec::new();
    let mut rest = cmd;
    while !rest.is_empty() {
        let trimmed = rest.trim_start();
        if trimmed.is_empty() {
            break;
        }
        let bytes = trimmed.as_bytes();
        let mut i = 0;
        let split_pos = loop {
            if i >= bytes.len() {
                break None;
            }
            match bytes[i] {
                b'&' if i + 1 < bytes.len() && bytes[i + 1] == b'&' => break Some((i, 2)),
                b'|' if i + 1 < bytes.len() && bytes[i + 1] == b'|' => break Some((i, 2)),
                b';' => break Some((i, 1)),
                b'|' => break Some((i, 1)),
                _ => i += 1,
            }
        };
        match split_pos {
            Some((pos, len)) => {
                let part = trimmed[..pos].trim();
                if !part.is_empty() {
                    commands.push(part);
                }
                rest = &trimmed[pos + len..];
            }
            None => {
                let part = trimmed.trim();
                if !part.is_empty() {
                    commands.push(part);
                }
                break;
            }
        }
    }
    commands
}

fn matches_rule(pat: &glob::Pattern, value: &str) -> bool {
    // Match both the value as-is and with a trailing space to handle
    // patterns like "ls *" matching bare "ls" (no arguments).
    pat.matches(value) || pat.matches(&format!("{value} "))
}

fn check_ruleset(ruleset: &RuleSet, value: &str) -> Decision {
    for pat in &ruleset.deny {
        if matches_rule(pat, value) {
            return Decision::Deny;
        }
    }
    for pat in &ruleset.allow {
        if matches_rule(pat, value) {
            return Decision::Allow;
        }
    }
    for pat in &ruleset.ask {
        if matches_rule(pat, value) {
            return Decision::Ask;
        }
    }
    Decision::Ask
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ruleset(allow: &[&str], ask: &[&str], deny: &[&str]) -> RuleSet {
        RuleSet {
            allow: compile_patterns(&allow.iter().map(|s| s.to_string()).collect::<Vec<_>>()),
            ask: compile_patterns(&ask.iter().map(|s| s.to_string()).collect::<Vec<_>>()),
            deny: compile_patterns(&deny.iter().map(|s| s.to_string()).collect::<Vec<_>>()),
        }
    }

    fn perms_with_bash(allow: &[&str], ask: &[&str], deny: &[&str]) -> Permissions {
        let mode = ModePerms {
            tools: HashMap::new(),
            bash: ruleset(allow, ask, deny),
            web_fetch: RuleSet {
                allow: vec![],
                ask: vec![],
                deny: vec![],
            },
        };
        Permissions {
            normal: mode.clone(),
            plan: mode.clone(),
            apply: mode,
        }
    }

    // --- simple commands ---

    #[test]
    fn simple_allowed() {
        let p = perms_with_bash(&["ls *"], &[], &[]);
        assert_eq!(p.check_bash(Mode::Normal, "ls -la"), Decision::Allow);
    }

    #[test]
    fn simple_denied() {
        let p = perms_with_bash(&[], &[], &["rm *"]);
        assert_eq!(p.check_bash(Mode::Normal, "rm -rf /"), Decision::Deny);
    }

    #[test]
    fn simple_ask() {
        let p = perms_with_bash(&[], &["rm *"], &[]);
        assert_eq!(p.check_bash(Mode::Normal, "rm -rf /"), Decision::Ask);
    }

    // --- deny rules with chained commands ---

    #[test]
    fn deny_rm_simple() {
        let p = perms_with_bash(&[], &[], &["rm *"]);
        assert_eq!(p.check_bash(Mode::Normal, "rm -rf /"), Decision::Deny);
    }

    #[test]
    fn deny_rm_after_ls() {
        let p = perms_with_bash(&["ls *"], &[], &["rm *"]);
        assert_eq!(
            p.check_bash(Mode::Normal, "ls && rm -rf /"),
            Decision::Deny
        );
    }

    #[test]
    fn deny_rm_before_ls() {
        let p = perms_with_bash(&["ls *"], &[], &["rm *"]);
        assert_eq!(
            p.check_bash(Mode::Normal, "rm -rf / && ls"),
            Decision::Deny
        );
    }

    // --- ask rules with chained commands ---

    #[test]
    fn ask_rm_simple() {
        let p = perms_with_bash(&[], &["rm *"], &[]);
        assert_eq!(p.check_bash(Mode::Normal, "rm -rf /"), Decision::Ask);
    }

    #[test]
    fn ask_rm_after_ls() {
        let p = perms_with_bash(&["ls *"], &["rm *"], &[]);
        assert_eq!(
            p.check_bash(Mode::Normal, "ls && rm -rf /"),
            Decision::Ask
        );
    }

    #[test]
    fn ask_rm_before_ls() {
        let p = perms_with_bash(&["ls *"], &["rm *"], &[]);
        assert_eq!(
            p.check_bash(Mode::Normal, "rm -rf / && ls"),
            Decision::Ask
        );
    }

    // --- allow rule should not match chained commands ---

    #[test]
    fn allow_ls_does_not_allow_chained_rm() {
        let p = perms_with_bash(&["ls *"], &[], &[]);
        assert_eq!(
            p.check_bash(Mode::Normal, "ls && rm README.md"),
            Decision::Ask
        );
    }

    // --- both sub-commands allowed ---

    #[test]
    fn chained_both_allowed() {
        let p = perms_with_bash(&["ls *", "rm *"], &[], &[]);
        assert_eq!(
            p.check_bash(Mode::Normal, "ls && rm README.md"),
            Decision::Allow
        );
    }

    // --- pipes ---

    #[test]
    fn pipe_both_allowed() {
        let p = perms_with_bash(&["cat *", "grep *"], &[], &[]);
        assert_eq!(
            p.check_bash(Mode::Normal, "cat file.txt | grep foo"),
            Decision::Allow
        );
    }

    #[test]
    fn pipe_second_not_allowed() {
        let p = perms_with_bash(&["cat *"], &[], &[]);
        assert_eq!(
            p.check_bash(Mode::Normal, "cat file.txt | rm foo"),
            Decision::Ask
        );
    }

    // --- semicolon ---

    #[test]
    fn semicolon_second_denied() {
        let p = perms_with_bash(&["echo *"], &[], &["rm *"]);
        assert_eq!(
            p.check_bash(Mode::Normal, "echo hi; rm -rf /"),
            Decision::Deny
        );
    }

    // --- or chain ---

    #[test]
    fn or_chain_both_allowed() {
        let p = perms_with_bash(&["make *"], &[], &[]);
        assert_eq!(
            p.check_bash(Mode::Normal, "make || make install"),
            Decision::Allow
        );
    }

    // --- deny wins over allow ---

    #[test]
    fn deny_wins_over_allow() {
        let p = perms_with_bash(&["rm *"], &[], &["rm *"]);
        assert_eq!(p.check_bash(Mode::Normal, "rm foo"), Decision::Deny);
    }

    // --- split helper ---

    #[test]
    fn split_shell_commands_basic() {
        assert_eq!(split_shell_commands("ls"), vec!["ls"]);
        assert_eq!(
            split_shell_commands("ls && rm foo"),
            vec!["ls", "rm foo"]
        );
        assert_eq!(
            split_shell_commands("a | b || c; d && e"),
            vec!["a", "b", "c", "d", "e"]
        );
    }
}
