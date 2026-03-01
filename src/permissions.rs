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
        let command = command.trim();
        let subcmds = split_shell_commands(command);
        if subcmds.len() <= 1 {
            return check_ruleset(&perms.bash, command);
        }
        let mut worst = Decision::Allow;
        for subcmd in subcmds {
            let d = check_ruleset(&perms.bash, &subcmd);
            match d {
                Decision::Deny => return Decision::Deny,
                Decision::Ask if worst == Decision::Allow => worst = Decision::Ask,
                _ => {}
            }
        }
        worst
    }
}

const SHELL_OPERATORS: &[(&str, usize)] = &[
    ("&&", 2),
    ("||", 2),
    (";", 1),
    ("|", 1),
    ("&", 1),
    ("\n", 1),
];

/// Split a command string on shell operators, returning each sub-command
/// paired with the operator that follows it (None for the last command).
pub fn split_shell_commands_with_ops(cmd: &str) -> Vec<(String, Option<String>)> {
    let (commands, operators) = split_impl(cmd);
    commands
        .into_iter()
        .enumerate()
        .map(|(i, c)| (c, operators.get(i).cloned()))
        .collect()
}

/// Split a command string on shell operators (&&, ||, ;, |, &, newline).
/// Quote-aware: operators inside single or double quotes are ignored.
/// Also extracts commands embedded in $(...), backticks, and (...) subshells.
fn split_shell_commands(cmd: &str) -> Vec<String> {
    let mut result = split_impl(cmd).0;
    // Post-process: extract embedded commands from subshells and substitutions.
    let mut i = 0;
    while i < result.len() {
        let extracted = extract_embedded_commands(&result[i]);
        if !extracted.is_empty() {
            result.extend(extracted);
        }
        i += 1;
    }
    result
}

fn split_impl(cmd: &str) -> (Vec<String>, Vec<String>) {
    let bytes = cmd.as_bytes();
    let len = bytes.len();
    let mut commands = Vec::new();
    let mut operators = Vec::new();
    let mut start = 0;
    let mut i = 0;

    while i < len {
        match bytes[i] {
            b'\'' => {
                i += 1;
                while i < len && bytes[i] != b'\'' {
                    i += 1;
                }
                if i < len {
                    i += 1;
                }
            }
            b'"' => {
                i += 1;
                while i < len && bytes[i] != b'"' {
                    if bytes[i] == b'\\' && i + 1 < len {
                        i += 1;
                    }
                    i += 1;
                }
                if i < len {
                    i += 1;
                }
            }
            b'\\' if i + 1 < len => {
                i += 2;
            }
            _ => {
                let rest = &cmd[i..];
                if let Some(&(op, op_len)) =
                    SHELL_OPERATORS.iter().find(|(op, _)| rest.starts_with(op))
                {
                    let part = cmd[start..i].trim();
                    if !part.is_empty() {
                        commands.push(part.to_string());
                        operators.push(op.to_string());
                    }
                    i += op_len;
                    start = i;
                } else {
                    i += 1;
                }
            }
        }
    }

    let part = cmd[start..].trim();
    if !part.is_empty() {
        commands.push(part.to_string());
    }
    (commands, operators)
}

/// Extract commands embedded in $(...), `...`, and (...) subshells.
/// Returns additional commands found inside these constructs.
/// The original command is kept as-is (for pattern matching); these are extras
/// that also need permission checks.
fn extract_embedded_commands(cmd: &str) -> Vec<String> {
    let mut extra = Vec::new();
    let bytes = cmd.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        match bytes[i] {
            // Single quotes are fully opaque — no expansions inside
            b'\'' => {
                i += 1;
                while i < len && bytes[i] != b'\'' {
                    i += 1;
                }
                if i < len {
                    i += 1;
                }
            }
            // Double quotes: bash expands $() and backticks inside them,
            // so we scan through without skipping — fall through to default
            b'\\' if i + 1 < len => {
                i += 2;
            }
            // $( ... )
            b'$' if i + 1 < len && bytes[i + 1] == b'(' => {
                i += 2;
                if let Some((inner, end)) = find_matching_paren(cmd, i) {
                    // Recursively split the inner command
                    for sub in split_shell_commands(inner) {
                        extra.push(sub);
                    }
                    i = end + 1;
                }
            }
            // backtick substitution
            b'`' => {
                i += 1;
                let start = i;
                while i < len && bytes[i] != b'`' {
                    if bytes[i] == b'\\' && i + 1 < len {
                        i += 1;
                    }
                    i += 1;
                }
                if i < len {
                    let inner = &cmd[start..i];
                    for sub in split_shell_commands(inner) {
                        extra.push(sub);
                    }
                    i += 1;
                }
            }
            // ( ... ) subshell — but not preceded by $
            b'(' => {
                i += 1;
                if let Some((inner, end)) = find_matching_paren(cmd, i) {
                    for sub in split_shell_commands(inner) {
                        extra.push(sub);
                    }
                    i = end + 1;
                }
            }
            _ => {
                i += 1;
            }
        }
    }
    extra
}

/// Find the matching `)` for an already-opened `(`, respecting nesting and quotes.
/// `start` is the index right after the opening `(`.
/// Returns the inner slice and the index of the closing `)`.
fn find_matching_paren(cmd: &str, start: usize) -> Option<(&str, usize)> {
    let bytes = cmd.as_bytes();
    let len = bytes.len();
    let mut depth = 1;
    let mut i = start;

    while i < len && depth > 0 {
        match bytes[i] {
            b'\'' => {
                i += 1;
                while i < len && bytes[i] != b'\'' {
                    i += 1;
                }
                if i < len {
                    i += 1;
                }
            }
            b'"' => {
                i += 1;
                while i < len && bytes[i] != b'"' {
                    if bytes[i] == b'\\' && i + 1 < len {
                        i += 1;
                    }
                    i += 1;
                }
                if i < len {
                    i += 1;
                }
            }
            b'\\' if i + 1 < len => {
                i += 2;
            }
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some((&cmd[start..i], i));
                }
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }
    None
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
        assert_eq!(p.check_bash(Mode::Normal, "ls && rm -rf /"), Decision::Deny);
    }

    #[test]
    fn deny_rm_before_ls() {
        let p = perms_with_bash(&["ls *"], &[], &["rm *"]);
        assert_eq!(p.check_bash(Mode::Normal, "rm -rf / && ls"), Decision::Deny);
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
        assert_eq!(p.check_bash(Mode::Normal, "ls && rm -rf /"), Decision::Ask);
    }

    #[test]
    fn ask_rm_before_ls() {
        let p = perms_with_bash(&["ls *"], &["rm *"], &[]);
        assert_eq!(p.check_bash(Mode::Normal, "rm -rf / && ls"), Decision::Ask);
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
        assert_eq!(split_shell_commands("ls && rm foo"), vec!["ls", "rm foo"]);
        assert_eq!(
            split_shell_commands("a | b || c; d && e"),
            vec!["a", "b", "c", "d", "e"]
        );
    }

    // --- edge cases ---

    // Empty / whitespace-only commands
    #[test]
    fn empty_command() {
        let p = perms_with_bash(&["ls *"], &[], &[]);
        assert_eq!(p.check_bash(Mode::Normal, ""), Decision::Ask);
    }

    #[test]
    fn whitespace_only_command() {
        let p = perms_with_bash(&["ls *"], &[], &[]);
        assert_eq!(p.check_bash(Mode::Normal, "   "), Decision::Ask);
    }

    // --- quote-aware splitting (shlex) ---

    // Operators inside quotes are NOT treated as operators
    #[test]
    fn operator_in_quoted_argument() {
        let p = perms_with_bash(&["grep *"], &[], &[]);
        // && inside quotes is not an operator — stays as single command
        assert_eq!(
            p.check_bash(Mode::Normal, r#"grep "&&" file.txt"#),
            Decision::Allow
        );
    }

    #[test]
    fn semicolon_in_echo() {
        let p = perms_with_bash(&["echo *"], &[], &["rm *"]);
        // shlex sees: ["echo", "hello; world"] — semicolon inside quotes
        assert_eq!(
            p.check_bash(Mode::Normal, r#"echo "hello; world""#),
            Decision::Allow
        );
    }

    #[test]
    fn pipe_in_quoted_filename() {
        let p = perms_with_bash(&["cat *"], &[], &["rm *"]);
        // shlex sees: ["cat", "file|name"] — pipe inside quotes
        assert_eq!(
            p.check_bash(Mode::Normal, r#"cat "file|name""#),
            Decision::Allow
        );
    }

    // --- single & (background operator) now handled ---

    #[test]
    fn single_ampersand_background() {
        let p = perms_with_bash(&["sleep *"], &[], &["rm *"]);
        // shlex sees: ["sleep", "5", "&", "rm", "foo"]
        // splits to ["sleep 5", "rm foo"] — rm is denied
        assert_eq!(
            p.check_bash(Mode::Normal, "sleep 5 & rm foo"),
            Decision::Deny
        );
    }

    // --- subshell / substitution (still not caught) ---

    #[test]
    fn command_substitution() {
        let p = perms_with_bash(&["echo *"], &[], &["rm *"]);
        // rm inside $() is now extracted and checked
        assert_eq!(
            p.check_bash(Mode::Normal, "echo $(rm -rf /)"),
            Decision::Deny
        );
    }

    #[test]
    fn backtick_substitution() {
        let p = perms_with_bash(&["echo *"], &[], &["rm *"]);
        // rm inside backticks is now extracted and checked
        assert_eq!(
            p.check_bash(Mode::Normal, "echo `rm -rf /`"),
            Decision::Deny
        );
    }

    // --- newline separator ---

    #[test]
    fn newline_separator() {
        let p = perms_with_bash(&["ls *"], &[], &["rm *"]);
        // Newline is now treated as a command separator
        assert_eq!(p.check_bash(Mode::Normal, "ls\nrm -rf /"), Decision::Deny);
    }

    // --- trailing / leading operators ---

    #[test]
    fn trailing_operator() {
        let p = perms_with_bash(&["ls *"], &[], &[]);
        assert_eq!(p.check_bash(Mode::Normal, "ls &&"), Decision::Allow);
    }

    #[test]
    fn split_trailing_operator() {
        assert_eq!(split_shell_commands("ls &&"), vec!["ls"]);
    }

    #[test]
    fn leading_operator() {
        let p = perms_with_bash(&["rm *"], &[], &[]);
        // shlex sees: ["&&", "rm", "foo"] → splits to ["rm foo"]
        // single-command path uses original "&& rm foo" which won't match
        assert_eq!(p.check_bash(Mode::Normal, "&& rm foo"), Decision::Ask);
    }

    #[test]
    fn split_leading_operator() {
        assert_eq!(split_shell_commands("&& rm foo"), vec!["rm foo"]);
    }

    // --- triple &&& ---

    #[test]
    fn triple_ampersand() {
        // "ls &&&rm foo" — && consumes first two, & consumes third → ["ls", "rm foo"]
        assert_eq!(split_shell_commands("ls &&&rm foo"), vec!["ls", "rm foo"]);
    }

    #[test]
    fn triple_ampersand_spaced() {
        // "ls &&& rm foo" → shlex: ["ls", "&&", "&", "rm", "foo"]
        // splits on && and &: ["ls", "rm foo"]
        assert_eq!(split_shell_commands("ls &&& rm foo"), vec!["ls", "rm foo"]);
    }

    // --- bare commands ---

    #[test]
    fn bare_command_matches_star_pattern() {
        let p = perms_with_bash(&["ls *"], &[], &[]);
        assert_eq!(p.check_bash(Mode::Normal, "ls"), Decision::Allow);
    }

    #[test]
    fn trailing_space_no_false_positive() {
        let p = perms_with_bash(&["ls *"], &[], &[]);
        assert_eq!(p.check_bash(Mode::Normal, "lsof"), Decision::Ask);
    }

    // --- unclosed quotes ---

    #[test]
    fn unclosed_quote() {
        let p = perms_with_bash(&["echo *"], &[], &["rm *"]);
        // shlex returns None for unclosed quotes — treated as single command
        assert_eq!(
            p.check_bash(Mode::Normal, r#"echo "hello && rm foo"#),
            Decision::Allow
        );
    }

    // --- escaped operators outside quotes ---

    #[test]
    fn escaped_ampersand_not_split() {
        // \&\& is two literal & chars in bash, not an operator
        assert_eq!(
            split_shell_commands(r"ls \&\& rm foo"),
            vec![r"ls \&\& rm foo"]
        );
    }

    #[test]
    fn escaped_semicolon_not_split() {
        assert_eq!(
            split_shell_commands(r"echo hello\; world"),
            vec![r"echo hello\; world"]
        );
    }

    #[test]
    fn escaped_pipe_not_split() {
        assert_eq!(
            split_shell_commands(r"echo hello\|world"),
            vec![r"echo hello\|world"]
        );
    }

    // --- mixed quote types ---

    #[test]
    fn single_quotes_inside_double() {
        // echo "it's fine" && rm foo → two commands
        let p = perms_with_bash(&["echo *"], &[], &["rm *"]);
        assert_eq!(
            p.check_bash(Mode::Normal, r#"echo "it's fine" && rm foo"#),
            Decision::Deny
        );
    }

    #[test]
    fn double_quotes_inside_single() {
        // echo '"hello"' && rm foo → two commands
        let p = perms_with_bash(&["echo *"], &[], &["rm *"]);
        assert_eq!(
            p.check_bash(Mode::Normal, r#"echo '"hello"' && rm foo"#),
            Decision::Deny
        );
    }

    // --- escaped quote inside double quotes ---

    #[test]
    fn escaped_quote_inside_double_quotes() {
        // echo "he said \"hi\" && rm" is all one quoted string — single command
        let p = perms_with_bash(&["echo *"], &[], &["rm *"]);
        assert_eq!(
            p.check_bash(Mode::Normal, r#"echo "he said \"hi\" && rm""#),
            Decision::Allow
        );
    }

    // --- consecutive operators ---

    #[test]
    fn double_semicolons() {
        // ls ;; rm → empty command between ;; is dropped, both ls and rm checked
        assert_eq!(split_shell_commands("ls ;; rm"), vec!["ls", "rm"]);
    }

    #[test]
    fn double_semicolons_deny() {
        let p = perms_with_bash(&["ls *"], &[], &["rm *"]);
        assert_eq!(p.check_bash(Mode::Normal, "ls ;; rm foo"), Decision::Deny);
    }

    // --- operator-only input ---

    #[test]
    fn only_operators() {
        // No actual commands, just operators
        assert_eq!(split_shell_commands("&& || ;"), Vec::<String>::new());
    }

    // --- whitespace around operators ---

    #[test]
    fn extra_whitespace_around_operators() {
        assert_eq!(
            split_shell_commands("  ls   &&   rm foo  "),
            vec!["ls", "rm foo"]
        );
    }

    // --- single-command path inconsistency (pre-existing bug) ---

    #[test]
    fn leading_whitespace_single_command() {
        let p = perms_with_bash(&["ls *"], &[], &[]);
        // Input is trimmed before matching, so leading whitespace is fine
        assert_eq!(p.check_bash(Mode::Normal, "  ls -la"), Decision::Allow);
    }

    #[test]
    fn leading_whitespace_chained_command() {
        let p = perms_with_bash(&["ls *", "echo *"], &[], &[]);
        // Multi-command path trims each part, so "ls -la" matches "ls *".
        assert_eq!(
            p.check_bash(Mode::Normal, "  ls -la && echo hi"),
            Decision::Allow
        );
    }

    // --- subshells / parentheses (known limitation) ---

    #[test]
    fn subshell_not_parsed() {
        let p = perms_with_bash(&["echo *"], &[], &["rm *"]);
        // rm inside (...) subshell is now extracted and checked
        assert_eq!(
            p.check_bash(Mode::Normal, "echo hi && (rm -rf /)"),
            Decision::Deny
        );
    }

    #[test]
    fn subshell_hides_denied_command() {
        let p = perms_with_bash(&["echo *"], &[], &["rm *"]);
        // $() inside quotes: quotes prevent extraction in split_impl,
        // but extract_embedded_commands scans the full command including quotes.
        // The $() is found and rm is extracted → Deny.
        assert_eq!(
            p.check_bash(Mode::Normal, r#"echo "$(rm -rf /)""#),
            Decision::Deny
        );
    }

    // --- approval_pattern with background operator ---

    #[test]
    fn split_with_ops_background() {
        let result = split_shell_commands_with_ops("sleep 5 & echo done");
        assert_eq!(
            result,
            vec![
                ("sleep 5".to_string(), Some("&".to_string())),
                ("echo done".to_string(), None),
            ]
        );
    }

    #[test]
    fn split_with_ops_preserves_operators() {
        let result = split_shell_commands_with_ops("ls && rm foo | grep err; echo done");
        assert_eq!(
            result,
            vec![
                ("ls".to_string(), Some("&&".to_string())),
                ("rm foo".to_string(), Some("|".to_string())),
                ("grep err".to_string(), Some(";".to_string())),
                ("echo done".to_string(), None),
            ]
        );
    }

    // --- backslash at end of string ---

    #[test]
    fn trailing_backslash() {
        // Trailing backslash with nothing after — should not panic
        assert_eq!(split_shell_commands("ls \\"), vec!["ls \\"]);
    }

    // --- here-string / redirection ---

    #[test]
    fn redirection_not_split() {
        // << is not a shell operator we handle, so it stays as one command
        assert_eq!(split_shell_commands("cat << EOF"), vec!["cat << EOF"]);
    }

    // --- newline as separator ---

    #[test]
    fn newline_treated_as_separator() {
        assert_eq!(split_shell_commands("ls\nrm -rf /"), vec!["ls", "rm -rf /"]);
    }
}
