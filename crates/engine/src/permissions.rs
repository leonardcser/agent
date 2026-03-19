use protocol::Mode;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

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
    plan: RawModePerms,
    apply: RawModePerms,
    yolo: RawModePerms,
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
    yolo: ModePerms,
    restrict_to_workspace: bool,
    workspace: PathBuf,
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

const DEFAULT_BASH_ALLOW: &[&str] = &["ls *", "grep *", "find *", "cat *", "tail *", "head *"];

fn build_mode(raw: &RawModePerms, mode: Mode) -> ModePerms {
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
            "bash_background",
            "read_process_output",
            "stop_process",
        ] {
            tools.entry(name.to_string()).or_insert(Decision::Allow);
        }
        tools
            .entry("exit_plan_mode".to_string())
            .or_insert(Decision::Deny);
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

        let default_exit_plan = if mode == Mode::Plan {
            Decision::Ask
        } else {
            Decision::Deny
        };
        tools
            .entry("exit_plan_mode".to_string())
            .or_insert(default_exit_plan);
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
    }
}

impl Permissions {
    pub fn load() -> Self {
        let path = crate::paths::config_dir().join("config.yaml");
        let contents = std::fs::read_to_string(&path).unwrap_or_default();
        let raw: RawConfig = serde_yml::from_str(&contents).unwrap_or_default();
        Self {
            normal: build_mode(&raw.permissions.normal, Mode::Normal),
            plan: build_mode(&raw.permissions.plan, Mode::Plan),
            apply: build_mode(&raw.permissions.apply, Mode::Apply),
            yolo: build_mode(&raw.permissions.yolo, Mode::Yolo),
            restrict_to_workspace: true,
            workspace: PathBuf::new(),
        }
    }

    /// Create a clone with per-turn permission overrides layered on top.
    /// Override rules are prepended (checked first) to the existing rules
    /// for every mode.
    pub fn with_overrides(&self, overrides: &protocol::PermissionOverrides) -> Self {
        let mut cloned = self.clone();
        fn apply_to_mode(mode: &mut ModePerms, overrides: &protocol::PermissionOverrides) {
            if let Some(ref tools) = overrides.tools {
                for name in &tools.deny {
                    mode.tools.insert(name.clone(), Decision::Deny);
                }
                for name in &tools.ask {
                    mode.tools.entry(name.clone()).or_insert(Decision::Ask);
                }
                for name in &tools.allow {
                    mode.tools.entry(name.clone()).or_insert(Decision::Allow);
                }
            }
            if let Some(ref bash) = overrides.bash {
                let mut allow = compile_patterns(&bash.allow);
                allow.append(&mut mode.bash.allow);
                mode.bash.allow = allow;
                let mut ask = compile_patterns(&bash.ask);
                ask.append(&mut mode.bash.ask);
                mode.bash.ask = ask;
                let mut deny = compile_patterns(&bash.deny);
                deny.append(&mut mode.bash.deny);
                mode.bash.deny = deny;
            }
            if let Some(ref wf) = overrides.web_fetch {
                let mut allow = compile_patterns(&wf.allow);
                allow.append(&mut mode.web_fetch.allow);
                mode.web_fetch.allow = allow;
                let mut ask = compile_patterns(&wf.ask);
                ask.append(&mut mode.web_fetch.ask);
                mode.web_fetch.ask = ask;
                let mut deny = compile_patterns(&wf.deny);
                deny.append(&mut mode.web_fetch.deny);
                mode.web_fetch.deny = deny;
            }
        }
        apply_to_mode(&mut cloned.normal, overrides);
        apply_to_mode(&mut cloned.plan, overrides);
        apply_to_mode(&mut cloned.apply, overrides);
        apply_to_mode(&mut cloned.yolo, overrides);
        cloned
    }

    pub fn set_workspace(&mut self, path: PathBuf) {
        self.workspace = path;
    }

    pub fn restrict_to_workspace(&self) -> bool {
        self.restrict_to_workspace
    }

    pub fn set_restrict_to_workspace(&mut self, val: bool) {
        self.restrict_to_workspace = val;
    }

    fn mode_perms(&self, mode: Mode) -> &ModePerms {
        match mode {
            Mode::Normal => &self.normal,
            Mode::Plan => &self.plan,
            Mode::Apply => &self.apply,
            Mode::Yolo => &self.yolo,
        }
    }

    pub fn check_tool(&self, mode: Mode, tool_name: &str) -> Decision {
        let perms = self.mode_perms(mode);
        let default = if mode == Mode::Yolo {
            Decision::Allow
        } else {
            Decision::Ask
        };
        perms.tools.get(tool_name).cloned().unwrap_or(default)
    }

    pub fn check_tool_pattern(&self, mode: Mode, tool_name: &str, pattern: &str) -> Decision {
        let perms = self.mode_perms(mode);
        let ruleset = match tool_name {
            "web_fetch" => &perms.web_fetch,
            _ => return Decision::Ask,
        };
        check_ruleset(ruleset, pattern)
    }

    pub fn check_bash(&self, mode: Mode, command: &str) -> Decision {
        let perms = self.mode_perms(mode);
        let command = command.trim();
        // Escalate output redirection only in Normal/Plan modes.
        let escalate_redirect = matches!(mode, Mode::Normal | Mode::Plan);
        let subcmds = split_shell_commands(command);
        if subcmds.len() <= 1 {
            let d = check_ruleset(&perms.bash, command);
            if escalate_redirect && d == Decision::Allow && has_output_redirection(command) {
                return Decision::Ask;
            }
            return d;
        }
        let mut worst = Decision::Allow;
        for subcmd in subcmds {
            let d = check_ruleset(&perms.bash, &subcmd);
            let d = if escalate_redirect && d == Decision::Allow && has_output_redirection(&subcmd)
            {
                Decision::Ask
            } else {
                d
            };
            match d {
                Decision::Deny => return Decision::Deny,
                Decision::Ask if worst == Decision::Allow => worst = Decision::Ask,
                _ => {}
            }
        }
        worst
    }

    /// Full permission decision for a tool call, including workspace restriction.
    /// This is the single entry point used by both the engine and TUI.
    pub fn decide(&self, mode: Mode, tool_name: &str, args: &HashMap<String, Value>) -> Decision {
        let base = decide_base(self, mode, tool_name, args);
        if base == Decision::Allow
            && self.restrict_to_workspace
            && !self.workspace.as_os_str().is_empty()
            && has_paths_outside_workspace(tool_name, args, &self.workspace)
        {
            return Decision::Ask;
        }
        base
    }

    /// Whether this tool call's base permission is Allow but was downgraded
    /// to Ask solely because of paths outside the workspace.
    pub fn was_downgraded(
        &self,
        mode: Mode,
        tool_name: &str,
        args: &HashMap<String, Value>,
    ) -> bool {
        let base = decide_base(self, mode, tool_name, args);
        base == Decision::Allow
            && self.restrict_to_workspace
            && !self.workspace.as_os_str().is_empty()
            && has_paths_outside_workspace(tool_name, args, &self.workspace)
    }

    /// Return paths from a tool call that fall outside the workspace.
    /// Empty if `restrict_to_workspace` is off or no paths escape.
    pub fn outside_workspace_paths(
        &self,
        tool_name: &str,
        args: &HashMap<String, Value>,
    ) -> Vec<String> {
        if !self.restrict_to_workspace || self.workspace.as_os_str().is_empty() {
            return vec![];
        }
        extract_tool_paths(tool_name, args)
            .into_iter()
            .filter(|p| !is_in_workspace(p, &self.workspace))
            .collect()
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
pub fn split_shell_commands(cmd: &str) -> Vec<String> {
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

                // Handle heredoc: << or <<- followed by a delimiter word.
                // Skip everything until the delimiter appears on its own line.
                if rest.starts_with("<<") {
                    let mut hi = 2;
                    if hi < rest.len() && rest.as_bytes()[hi] == b'-' {
                        hi += 1;
                    }
                    // Skip whitespace before delimiter
                    while hi < rest.len() && rest.as_bytes()[hi] == b' ' {
                        hi += 1;
                    }
                    // Read the delimiter (strip quotes)
                    let mut delim_start = hi;
                    let mut strip_quotes = false;
                    if hi < rest.len()
                        && (rest.as_bytes()[hi] == b'\'' || rest.as_bytes()[hi] == b'"')
                    {
                        let q = rest.as_bytes()[hi];
                        strip_quotes = true;
                        hi += 1;
                        delim_start = hi;
                        while hi < rest.len() && rest.as_bytes()[hi] != q {
                            hi += 1;
                        }
                    } else {
                        while hi < rest.len()
                            && !rest.as_bytes()[hi].is_ascii_whitespace()
                            && rest.as_bytes()[hi] != b';'
                            && rest.as_bytes()[hi] != b'&'
                            && rest.as_bytes()[hi] != b'|'
                        {
                            hi += 1;
                        }
                    }
                    let delim = &rest[delim_start..hi];
                    if strip_quotes && hi < rest.len() {
                        hi += 1; // skip closing quote
                    }
                    if !delim.is_empty() {
                        // Skip past the heredoc body: find \n<delim>\n or \n<delim>EOF
                        let search_from = i + hi;
                        let mut found = false;
                        let mut si = search_from;
                        while si < len {
                            if bytes[si] == b'\n' {
                                let line_start = si + 1;
                                let line_end = cmd[line_start..]
                                    .find('\n')
                                    .map(|p| line_start + p)
                                    .unwrap_or(len);
                                let line = cmd[line_start..line_end].trim();
                                if line == delim {
                                    i = line_end;
                                    found = true;
                                    break;
                                }
                            }
                            si += 1;
                        }
                        if !found {
                            // No closing delimiter — consume rest
                            i = len;
                        }
                        continue;
                    }
                }

                // Handle redirections containing & (e.g. 2>&1, >&2, &>, &>>)
                // Don't treat & as an operator in these contexts.
                if rest.starts_with("&>") {
                    // &> or &>> redirection
                    i += if rest.starts_with("&>>") { 3 } else { 2 };
                    continue;
                }
                if bytes[i] == b'&' && i > 0 && bytes[i - 1] == b'>' {
                    // >& redirection (e.g. 2>&1)
                    i += 1;
                    // skip the fd number after
                    while i < len && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                    continue;
                }

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

/// Check whether a command contains an output redirection (`>`, `>>`, `&>`, `&>>`).
/// Quote-aware: ignores redirection operators inside single or double quotes.
fn has_output_redirection(cmd: &str) -> bool {
    let bytes = cmd.as_bytes();
    let len = bytes.len();
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
            b'<' => {
                // Skip << (heredoc) — not an output redirection by itself.
                if i + 1 < len && bytes[i + 1] == b'<' {
                    i += 2;
                    // Skip past delimiter and body (already handled by split_impl,
                    // but for standalone use, just skip the << token).
                } else {
                    // Input redirection <, not output.
                    i += 1;
                }
            }
            b'&' => {
                // &> or &>> is output redirection
                if i + 1 < len && bytes[i + 1] == b'>' {
                    return true;
                }
                i += 1;
            }
            b'>' => {
                // Check if this is >&N style fd duplication (e.g., 2>&1).
                // These don't write to files, they just duplicate file descriptors.
                // A standalone > or >> or N> is output redirection to a file.
                if i + 1 < len && bytes[i + 1] == b'&' {
                    // >& followed by a digit is fd duplication (e.g., 2>&1)
                    let j = i + 2;
                    if j < len && bytes[j].is_ascii_digit() {
                        i += 1;
                        continue;
                    }
                    // >& without a following digit, or >&N where N isn't a digit
                    // could still be problematic, but let's be conservative.
                }
                return true;
            }
            _ => {
                i += 1;
            }
        }
    }
    false
}

// ── Base decision (without workspace restriction) ────────────────────────────

use crate::tools::str_arg;

fn decide_base(
    permissions: &Permissions,
    mode: Mode,
    tool_name: &str,
    args: &HashMap<String, Value>,
) -> Decision {
    if tool_name == "bash" {
        let cmd = str_arg(args, "command");
        let tool_decision = permissions.check_tool(mode, "bash");
        if tool_decision == Decision::Deny {
            return Decision::Deny;
        }
        let bash_decision = permissions.check_bash(mode, &cmd);
        match (&tool_decision, &bash_decision) {
            (_, Decision::Deny) => Decision::Deny,
            (Decision::Allow, Decision::Ask) => Decision::Allow,
            _ => bash_decision,
        }
    } else if tool_name == "web_fetch" {
        let url = str_arg(args, "url");
        let tool_decision = permissions.check_tool(mode, "web_fetch");
        if tool_decision == Decision::Deny {
            return Decision::Deny;
        }
        let pattern_decision = permissions.check_tool_pattern(mode, "web_fetch", &url);
        match (&tool_decision, &pattern_decision) {
            (_, Decision::Deny) => Decision::Deny,
            (_, Decision::Allow) => Decision::Allow,
            (Decision::Allow, Decision::Ask) => Decision::Ask,
            _ => pattern_decision,
        }
    } else {
        permissions.check_tool(mode, tool_name)
    }
}

// ── Workspace path restriction ───────────────────────────────────────────────

fn extract_tool_paths(tool_name: &str, args: &HashMap<String, Value>) -> Vec<String> {
    match tool_name {
        "read_file" | "write_file" | "edit_file" => {
            let p = str_arg(args, "file_path");
            if p.is_empty() {
                vec![]
            } else {
                vec![p]
            }
        }
        "glob" | "grep" => {
            let p = str_arg(args, "path");
            if p.is_empty() {
                vec![]
            } else {
                vec![p]
            }
        }
        "bash" => extract_paths_from_command(&str_arg(args, "command")),
        _ => vec![],
    }
}

/// Extract tokens that look like absolute paths from a shell command.
/// Relative paths are fine (they resolve within the workspace).
fn extract_paths_from_command(cmd: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for token in cmd.split_whitespace() {
        let clean = token.trim_matches(|c: char| c == '\'' || c == '"' || c == ';');
        if clean.starts_with('/') || clean.starts_with("~/") {
            paths.push(clean.to_string());
        }
    }
    paths
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                components.pop();
            }
            std::path::Component::CurDir => {}
            c => components.push(c),
        }
    }
    components.iter().collect()
}

fn resolve_path(path_str: &str, workspace: &Path) -> PathBuf {
    if let Some(rest) = path_str.strip_prefix("~/") {
        let resolved = crate::paths::home_dir().join(rest);
        resolved
            .canonicalize()
            .unwrap_or_else(|_| normalize_path(&resolved))
    } else if path_str.starts_with('/') {
        let p = PathBuf::from(path_str);
        p.canonicalize().unwrap_or_else(|_| normalize_path(&p))
    } else {
        let resolved = workspace.join(path_str);
        resolved
            .canonicalize()
            .unwrap_or_else(|_| normalize_path(&resolved))
    }
}

fn is_in_workspace(path_str: &str, workspace: &Path) -> bool {
    let resolved = resolve_path(path_str, workspace);
    let ws = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    resolved.starts_with(&ws)
}

fn has_paths_outside_workspace(
    tool_name: &str,
    args: &HashMap<String, Value>,
    workspace: &Path,
) -> bool {
    let paths = extract_tool_paths(tool_name, args);
    paths.iter().any(|p| !is_in_workspace(p, workspace))
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
            apply: mode.clone(),
            yolo: mode,
            restrict_to_workspace: false,
            workspace: PathBuf::new(),
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

    // --- heredoc content not treated as commands ---

    #[test]
    fn heredoc_content_not_split() {
        let cmd = "cat << 'EOF'\nhello world\nsome content\nEOF";
        assert_eq!(
            split_shell_commands(cmd),
            vec!["cat << 'EOF'\nhello world\nsome content\nEOF"]
        );
    }

    #[test]
    fn heredoc_with_pipe() {
        let cmd = "cat << 'EOF' | grep foo\nhello\nworld\nEOF";
        // The heredoc body should not produce extra commands
        let cmds = split_shell_commands(cmd);
        assert!(!cmds.iter().any(|c| c == "hello" || c == "world"));
    }

    #[test]
    fn heredoc_permission_check() {
        let p = perms_with_bash(&["cat *", "grep *"], &[], &["rm *"]);
        let cmd = "cat << 'EOF' | grep foo\nrm -rf /\nEOF";
        // "rm -rf /" is heredoc content, not a command — should not be denied
        assert_eq!(p.check_bash(Mode::Normal, cmd), Decision::Allow);
    }

    // --- 2>&1 not split on & ---

    #[test]
    fn redirect_stderr_not_split() {
        assert_eq!(
            split_shell_commands("cargo build 2>&1"),
            vec!["cargo build 2>&1"]
        );
    }

    #[test]
    fn redirect_stderr_permission() {
        let p = perms_with_bash(&["cargo *"], &[], &[]);
        assert_eq!(
            p.check_bash(Mode::Normal, "cargo build 2>&1"),
            Decision::Allow
        );
    }

    #[test]
    fn redirect_ampersand_greater() {
        // &> /dev/null
        assert_eq!(
            split_shell_commands("cargo build &> /dev/null"),
            vec!["cargo build &> /dev/null"]
        );
    }

    // --- newline as separator ---

    #[test]
    fn newline_treated_as_separator() {
        assert_eq!(split_shell_commands("ls\nrm -rf /"), vec!["ls", "rm -rf /"]);
    }

    // ── workspace restriction ────────────────────────────────────────

    fn perms_with_workspace(workspace: &str) -> Permissions {
        let mode = ModePerms {
            tools: {
                let mut m = HashMap::new();
                m.insert("read_file".to_string(), Decision::Allow);
                m.insert("write_file".to_string(), Decision::Allow);
                m.insert("edit_file".to_string(), Decision::Allow);
                m.insert("glob".to_string(), Decision::Allow);
                m.insert("grep".to_string(), Decision::Allow);
                m.insert("bash".to_string(), Decision::Allow);
                m
            },
            bash: RuleSet {
                allow: vec![glob::Pattern::new("*").unwrap()],
                ask: vec![],
                deny: vec![],
            },
            web_fetch: RuleSet {
                allow: vec![],
                ask: vec![],
                deny: vec![],
            },
        };
        Permissions {
            normal: mode.clone(),
            plan: mode.clone(),
            apply: mode.clone(),
            yolo: mode,
            restrict_to_workspace: true,
            workspace: PathBuf::from(workspace),
        }
    }

    fn args_with(key: &str, val: &str) -> HashMap<String, Value> {
        let mut m = HashMap::new();
        m.insert(key.to_string(), Value::String(val.to_string()));
        m
    }

    // --- path extraction ---

    #[test]
    fn extract_paths_from_file_tools() {
        assert_eq!(
            extract_tool_paths("read_file", &args_with("file_path", "/etc/passwd")),
            vec!["/etc/passwd"]
        );
        assert_eq!(
            extract_tool_paths("write_file", &args_with("file_path", "relative.txt")),
            vec!["relative.txt"]
        );
        assert_eq!(
            extract_tool_paths("edit_file", &args_with("file_path", "")),
            Vec::<String>::new()
        );
    }

    #[test]
    fn extract_paths_from_glob_grep() {
        assert_eq!(
            extract_tool_paths("glob", &args_with("path", "/tmp")),
            vec!["/tmp"]
        );
        assert_eq!(
            extract_tool_paths("grep", &args_with("path", "")),
            Vec::<String>::new()
        );
    }

    #[test]
    fn extract_paths_from_bash() {
        assert_eq!(
            extract_tool_paths("bash", &args_with("command", "rm -rf /tmp/foo")),
            vec!["/tmp/foo"]
        );
        assert_eq!(
            extract_tool_paths("bash", &args_with("command", "ls relative/dir")),
            Vec::<String>::new()
        );
        assert_eq!(
            extract_tool_paths("bash", &args_with("command", "cat ~/secret.txt")),
            vec!["~/secret.txt"]
        );
    }

    #[test]
    fn extract_paths_from_bash_strips_quotes() {
        assert_eq!(
            extract_tool_paths("bash", &args_with("command", "rm '/etc/passwd'")),
            vec!["/etc/passwd"]
        );
    }

    // --- is_in_workspace ---

    #[test]
    fn relative_path_in_workspace() {
        assert!(is_in_workspace(
            "src/main.rs",
            Path::new("/home/user/project")
        ));
    }

    #[test]
    fn absolute_path_in_workspace() {
        assert!(is_in_workspace(
            "/home/user/project/src/main.rs",
            Path::new("/home/user/project")
        ));
    }

    #[test]
    fn absolute_path_outside_workspace() {
        assert!(!is_in_workspace(
            "/etc/passwd",
            Path::new("/home/user/project")
        ));
    }

    #[test]
    fn dotdot_escape_outside_workspace() {
        assert!(!is_in_workspace(
            "/home/user/project/../../etc/passwd",
            Path::new("/home/user/project")
        ));
    }

    #[test]
    fn workspace_root_itself_is_in_workspace() {
        assert!(is_in_workspace(
            "/home/user/project",
            Path::new("/home/user/project")
        ));
    }

    // --- decide with workspace restriction ---

    #[test]
    fn workspace_allows_file_inside() {
        let p = perms_with_workspace("/home/user/project");
        let args = args_with("file_path", "/home/user/project/src/main.rs");
        assert_eq!(p.decide(Mode::Normal, "read_file", &args), Decision::Allow);
    }

    #[test]
    fn workspace_downgrades_file_outside() {
        let p = perms_with_workspace("/home/user/project");
        let args = args_with("file_path", "/etc/passwd");
        assert_eq!(p.decide(Mode::Normal, "read_file", &args), Decision::Ask);
    }

    #[test]
    fn workspace_allows_relative_path() {
        let p = perms_with_workspace("/home/user/project");
        let args = args_with("file_path", "src/main.rs");
        assert_eq!(p.decide(Mode::Normal, "write_file", &args), Decision::Allow);
    }

    #[test]
    fn workspace_downgrades_bash_outside() {
        let p = perms_with_workspace("/home/user/project");
        let args = args_with("command", "rm -rf /tmp/foo");
        assert_eq!(p.decide(Mode::Normal, "bash", &args), Decision::Ask);
    }

    #[test]
    fn workspace_allows_bash_inside() {
        let p = perms_with_workspace("/home/user/project");
        let args = args_with("command", "rm -rf /home/user/project/target");
        assert_eq!(p.decide(Mode::Normal, "bash", &args), Decision::Allow);
    }

    #[test]
    fn workspace_allows_bash_relative() {
        let p = perms_with_workspace("/home/user/project");
        let args = args_with("command", "cargo build");
        assert_eq!(p.decide(Mode::Normal, "bash", &args), Decision::Allow);
    }

    #[test]
    fn workspace_downgrades_yolo_outside() {
        let p = perms_with_workspace("/home/user/project");
        let args = args_with("command", "rm -rf /");
        // Even in yolo, out-of-workspace should ask
        assert_eq!(p.decide(Mode::Yolo, "bash", &args), Decision::Ask);
    }

    #[test]
    fn workspace_yolo_allows_inside() {
        let p = perms_with_workspace("/home/user/project");
        let args = args_with("file_path", "/home/user/project/foo.txt");
        assert_eq!(p.decide(Mode::Yolo, "write_file", &args), Decision::Allow);
    }

    #[test]
    fn workspace_restriction_off_allows_everything() {
        let mut p = perms_with_workspace("/home/user/project");
        p.restrict_to_workspace = false;
        let args = args_with("file_path", "/etc/passwd");
        assert_eq!(p.decide(Mode::Normal, "read_file", &args), Decision::Allow);
    }

    #[test]
    fn workspace_ask_stays_ask() {
        // If the tool is already Ask (not Allow), workspace restriction doesn't change it
        let mut p = perms_with_workspace("/home/user/project");
        // Remove write_file from allowed tools so it defaults to Ask
        p.normal.tools.remove("write_file");
        let args = args_with("file_path", "/home/user/project/foo.txt");
        // Even inside workspace, Ask stays Ask because the tool itself is Ask
        assert_eq!(p.decide(Mode::Normal, "write_file", &args), Decision::Ask);
    }

    #[test]
    fn workspace_glob_outside_downgrades() {
        let p = perms_with_workspace("/home/user/project");
        let args = args_with("path", "/tmp");
        assert_eq!(p.decide(Mode::Normal, "glob", &args), Decision::Ask);
    }

    #[test]
    fn workspace_no_path_tools_unaffected() {
        let p = perms_with_workspace("/home/user/project");
        let args = HashMap::new();
        // web_search has no paths, should not be affected
        assert_eq!(p.decide(Mode::Yolo, "web_search", &args), Decision::Allow);
    }

    // --- yolo mode is configurable ---

    #[test]
    fn yolo_defaults_to_allow() {
        let p = Permissions::load();
        assert_eq!(p.check_tool(Mode::Yolo, "bash"), Decision::Allow);
        assert_eq!(p.check_tool(Mode::Yolo, "edit_file"), Decision::Allow);
        assert_eq!(p.check_tool(Mode::Yolo, "write_file"), Decision::Allow);
        assert_eq!(p.check_tool(Mode::Yolo, "read_file"), Decision::Allow);
    }

    #[test]
    fn yolo_unknown_tool_defaults_allow() {
        let p = Permissions::load();
        assert_eq!(
            p.check_tool(Mode::Yolo, "some_unknown_tool"),
            Decision::Allow
        );
    }

    #[test]
    fn yolo_bash_allows_everything_by_default() {
        let p = Permissions::load();
        assert_eq!(p.check_bash(Mode::Yolo, "rm -rf /"), Decision::Allow);
    }

    #[test]
    fn normal_unknown_tool_defaults_ask() {
        let p = Permissions::load();
        assert_eq!(
            p.check_tool(Mode::Normal, "some_unknown_tool"),
            Decision::Ask
        );
    }

    // --- output redirection escalation ---

    #[test]
    fn has_output_redirection_simple_greater() {
        assert!(has_output_redirection("cat file > out.txt"));
    }

    #[test]
    fn has_output_redirection_double_greater() {
        assert!(has_output_redirection("cat file >> out.txt"));
    }

    #[test]
    fn has_output_redirection_ampersand_greater() {
        assert!(has_output_redirection("cargo build &> /dev/null"));
    }

    #[test]
    fn has_output_redirection_double_ampersand_greater() {
        assert!(has_output_redirection("cargo build &>> /dev/null"));
    }

    #[test]
    fn has_output_redirection_no_redirection() {
        assert!(!has_output_redirection("cat file"));
    }

    #[test]
    fn has_output_redirection_input_only() {
        assert!(!has_output_redirection("cat < input.txt"));
    }

    #[test]
    fn has_output_redirection_heredoc_only() {
        // << alone is not an output redirection
        assert!(!has_output_redirection("cat << EOF"));
    }

    #[test]
    fn has_output_redirection_heredoc_with_output_redirect() {
        // heredoc with output redirection to a file
        assert!(has_output_redirection("cat << 'EOF' > file.txt"));
    }

    #[test]
    fn has_output_redirection_inside_quotes_ignored() {
        // > inside quotes should not be detected as redirection
        assert!(!has_output_redirection(r#"echo ">" file.txt"#));
    }

    #[test]
    fn has_output_redirection_mixed_quotes() {
        assert!(has_output_redirection("cat file > 'out.txt'"));
    }

    #[test]
    fn has_output_redirection_stderr_redirect() {
        // 2>&1 is fd duplication, not file output redirection
        assert!(!has_output_redirection("cargo build 2>&1"));
    }

    #[test]
    fn auto_allowed_with_output_redirect_escalates_to_ask() {
        // cat * is in the default allowlist, but with > it should ask
        let p = perms_with_bash(&["cat *"], &[], &[]);
        assert_eq!(
            p.check_bash(Mode::Normal, "cat file.txt > output.txt"),
            Decision::Ask
        );
    }

    #[test]
    fn auto_allowed_with_append_redirect_escalates_to_ask() {
        let p = perms_with_bash(&["cat *"], &[], &[]);
        assert_eq!(
            p.check_bash(Mode::Normal, "cat file.txt >> output.txt"),
            Decision::Ask
        );
    }

    #[test]
    fn auto_allowed_heredoc_with_redirect_escalates_to_ask() {
        // cat << 'EOF' > file.txt matches cat * but has output redirection
        let p = perms_with_bash(&["cat *"], &[], &[]);
        let cmd = "cat << 'EOF' > long_file.txt\nhello\nworld\nEOF";
        assert_eq!(p.check_bash(Mode::Normal, cmd), Decision::Ask);
    }

    #[test]
    fn auto_allowed_no_redirect_stays_allow() {
        // Without redirection, cat * should still be allowed
        let p = perms_with_bash(&["cat *"], &[], &[]);
        assert_eq!(p.check_bash(Mode::Normal, "cat file.txt"), Decision::Allow);
    }

    #[test]
    fn chained_command_with_redirect_escalates() {
        let p = perms_with_bash(&["ls *", "cat *"], &[], &[]);
        // ls is allowed, cat with redirect should escalate
        assert_eq!(
            p.check_bash(Mode::Normal, "ls -la && cat file > out.txt"),
            Decision::Ask
        );
    }

    #[test]
    fn pipe_with_output_redirect_escalates() {
        let p = perms_with_bash(&["cat *", "grep *"], &[], &[]);
        // pipe is allowed, but output redirect at end should escalate
        assert_eq!(
            p.check_bash(Mode::Normal, "cat file | grep foo > out.txt"),
            Decision::Ask
        );
    }

    #[test]
    fn denied_command_with_redirect_stays_deny() {
        let p = perms_with_bash(&[], &[], &["rm *"]);
        // rm is denied regardless of redirection
        assert_eq!(
            p.check_bash(Mode::Normal, "rm file.txt > /dev/null"),
            Decision::Deny
        );
    }
}
