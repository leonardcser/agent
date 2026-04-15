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
    mcp: RawRuleSet,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawPerms {
    default: RawModePerms,
    normal: RawModePerms,
    plan: RawModePerms,
    apply: RawModePerms,
    yolo: RawModePerms,
}

fn merge_ruleset(default: &RawRuleSet, mode: &RawRuleSet) -> RawRuleSet {
    RawRuleSet {
        allow: default.allow.iter().chain(&mode.allow).cloned().collect(),
        ask: default.ask.iter().chain(&mode.ask).cloned().collect(),
        deny: default.deny.iter().chain(&mode.deny).cloned().collect(),
    }
}

fn merge_mode(default: &RawModePerms, mode: &RawModePerms) -> RawModePerms {
    RawModePerms {
        tools: merge_ruleset(&default.tools, &mode.tools),
        bash: merge_ruleset(&default.bash, &mode.bash),
        web_fetch: merge_ruleset(&default.web_fetch, &mode.web_fetch),
        mcp: merge_ruleset(&default.mcp, &mode.mcp),
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawConfig {
    permissions: RawPerms,
}

#[derive(Debug, Clone)]
pub struct RuleSet {
    allow: Vec<glob::Pattern>,
    ask: Vec<glob::Pattern>,
    deny: Vec<glob::Pattern>,
}

#[derive(Debug, Clone)]
struct ModePerms {
    tools: HashMap<String, Decision>,
    bash: RuleSet,
    web_fetch: RuleSet,
    mcp: RuleSet,
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
            "read_process_output",
            "stop_process",
            "exit_plan_mode",
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

        tools
            .entry("exit_plan_mode".to_string())
            .or_insert(Decision::Ask);
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

impl Permissions {
    pub fn load() -> Self {
        let path = crate::paths::config_dir().join("config.yaml");
        let contents = std::fs::read_to_string(&path).unwrap_or_default();
        let raw: RawConfig = serde_yml::from_str(&contents).unwrap_or_default();
        let def = &raw.permissions.default;
        Self {
            normal: build_mode(&merge_mode(def, &raw.permissions.normal), Mode::Normal),
            plan: build_mode(&merge_mode(def, &raw.permissions.plan), Mode::Plan),
            apply: build_mode(&merge_mode(def, &raw.permissions.apply), Mode::Apply),
            yolo: build_mode(&merge_mode(def, &raw.permissions.yolo), Mode::Yolo),
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
                for name in &tools.allow {
                    mode.tools.insert(name.clone(), Decision::Allow);
                }
                for name in &tools.ask {
                    mode.tools.insert(name.clone(), Decision::Ask);
                }
                for name in &tools.deny {
                    mode.tools.insert(name.clone(), Decision::Deny);
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

    /// Check permission for an MCP tool call. Matches the qualified tool name
    /// (e.g. `filesystem_read_file`) against glob patterns in the `mcp` ruleset.
    /// Defaults to Allow in yolo mode, Ask otherwise, if no pattern matches.
    pub fn check_mcp(&self, mode: Mode, qualified_name: &str) -> Decision {
        let perms = self.mode_perms(mode);
        let decision = check_ruleset(&perms.mcp, qualified_name);
        if decision == Decision::Ask && mode == Mode::Yolo {
            Decision::Allow
        } else {
            decision
        }
    }

    pub fn check_bash(&self, mode: Mode, command: &str) -> Decision {
        let perms = self.mode_perms(mode);
        let command = command.trim();
        // Escalate output redirection only in Normal/Plan modes.
        let escalate_redirect = matches!(mode, Mode::Normal | Mode::Plan);
        let subcmds = split_shell_commands(command);
        if subcmds.len() <= 1 {
            if is_cd_command(command) {
                return Decision::Allow;
            }
            let d = check_ruleset(&perms.bash, command);
            if escalate_redirect && d == Decision::Allow && has_output_redirection(command) {
                return Decision::Ask;
            }
            return d;
        }
        let mut worst = Decision::Allow;
        for subcmd in subcmds {
            // `cd` is always allowed at the command level; the workspace
            // path restriction in `decide()` handles outside-workspace paths.
            if is_cd_command(&subcmd) {
                continue;
            }
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
    /// When `is_mcp` is true, routes through the MCP ruleset instead of the
    /// normal tool/bash/web_fetch rulesets.
    pub fn decide(
        &self,
        mode: Mode,
        tool_name: &str,
        args: &HashMap<String, Value>,
        is_mcp: bool,
    ) -> Decision {
        let base = if is_mcp {
            self.check_mcp(mode, tool_name)
        } else {
            decide_base(self, mode, tool_name, args)
        };
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
    /// Get the bash ruleset for the given mode (used by RuntimeApprovals
    /// to check per-subcommand config decisions).
    pub fn bash_ruleset(&self, mode: Mode) -> &RuleSet {
        &self.mode_perms(mode).bash
    }

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

/// Runtime permission approvals that augment the static config-based rules.
/// Shared between the engine and TUI via `Arc<RwLock<RuntimeApprovals>>` so
/// the engine can check them during `decide()` and the TUI can update them
/// when the user approves a tool call.
#[derive(Debug, Default)]
pub struct RuntimeApprovals {
    session_tools: HashMap<String, Vec<glob::Pattern>>,
    session_dirs: Vec<PathBuf>,
    workspace_tools: HashMap<String, Vec<glob::Pattern>>,
    workspace_dirs: Vec<PathBuf>,
}

impl RuntimeApprovals {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add session-scoped tool approval patterns. Empty `patterns` = blanket approval.
    pub fn add_session_tool(&mut self, tool: &str, patterns: Vec<glob::Pattern>) {
        let entry = self.session_tools.entry(tool.to_string()).or_default();
        if patterns.is_empty() || entry.is_empty() {
            // Blanket approval: clear existing patterns.
            entry.clear();
        } else {
            entry.extend(patterns);
        }
    }

    /// Add workspace-scoped tool approval patterns. Empty `patterns` = blanket approval.
    pub fn add_workspace_tool(&mut self, tool: &str, patterns: Vec<glob::Pattern>) {
        let entry = self.workspace_tools.entry(tool.to_string()).or_default();
        if patterns.is_empty() || entry.is_empty() {
            entry.clear();
        } else {
            entry.extend(patterns);
        }
    }

    pub fn add_session_dir(&mut self, dir: PathBuf) {
        let dir = crate::paths::expand_tilde(&dir);
        if !self.session_dirs.contains(&dir) {
            self.session_dirs.push(dir);
        }
    }

    pub fn add_workspace_dir(&mut self, dir: PathBuf) {
        let dir = crate::paths::expand_tilde(&dir);
        if !self.workspace_dirs.contains(&dir) {
            self.workspace_dirs.push(dir);
        }
    }

    pub fn clear_session(&mut self) {
        self.session_tools.clear();
        self.session_dirs.clear();
    }

    /// Load workspace approvals from pre-compiled patterns (called at startup
    /// and after persisting new workspace rules).
    pub fn load_workspace(
        &mut self,
        tools: HashMap<String, Vec<glob::Pattern>>,
        dirs: Vec<PathBuf>,
    ) {
        self.workspace_tools = tools;
        self.workspace_dirs = dirs
            .into_iter()
            .map(|d| crate::paths::expand_tilde(&d))
            .collect();
    }

    /// Check whether a tool call that the config-based rules said "Ask" for
    /// should be auto-approved based on runtime patterns.
    ///
    /// For bash: splits the command into subcommands and checks each against
    /// runtime patterns AND the config's allow list (so that default-allowed
    /// commands like `head`, `cat`, etc. don't block auto-approval).
    ///
    /// For other tools with patterns (e.g. web_fetch URLs): checks the
    /// description against patterns.
    ///
    /// Returns `true` if the tool call should be auto-approved.
    pub fn is_approved(&self, tool_name: &str, desc: &str, config_bash: Option<&RuleSet>) -> bool {
        let session = self.session_tools.get(tool_name);
        let workspace = self.workspace_tools.get(tool_name);

        if session.is_none() && workspace.is_none() {
            return false;
        }

        // Blanket approval (empty pattern list).
        let blanket =
            session.is_some_and(|p| p.is_empty()) || workspace.is_some_and(|p| p.is_empty());
        if blanket {
            return true;
        }

        let subcmds = split_shell_commands(desc);
        if subcmds.is_empty() {
            return false;
        }

        let all_pats: Vec<&glob::Pattern> =
            session.into_iter().chain(workspace).flatten().collect();

        subcmds.iter().all(|sc| {
            // Check runtime approval patterns.
            all_pats.iter().any(|p| p.matches(sc))
            // For bash: also check if the config already allows this subcommand
            // (e.g. DEFAULT_BASH_ALLOW patterns like "head *", "cat *").
            // The full compound command was Ask because of OTHER subcommands,
            // not this one.
                || config_bash.is_some_and(|rs| check_ruleset(rs, sc) == Decision::Allow)
        })
    }

    /// Full runtime auto-approval for an Ask decision.
    ///
    /// Inside the workspace, tool approvals are enough.
    /// Outside the workspace, tool approvals and directory approvals must both
    /// match when `restrict_to_workspace` is enabled.
    pub fn is_auto_approved(
        &self,
        permissions: &Permissions,
        mode: Mode,
        tool_name: &str,
        args: &HashMap<String, Value>,
        desc: &str,
    ) -> bool {
        let config_bash = if tool_name == "bash" {
            Some(permissions.bash_ruleset(mode))
        } else {
            None
        };
        let tool_approved = self.is_approved(tool_name, desc, config_bash);
        let outside = permissions.outside_workspace_paths(tool_name, args);
        if outside.is_empty() {
            return tool_approved;
        }
        let dirs_ok = self.dirs_approved(&outside);
        if dirs_ok && tool_approved {
            return true;
        }
        // Directory approved + base permission is Allow (downgraded only
        // because of the workspace restriction) → auto-approve.
        if dirs_ok && permissions.was_downgraded(mode, tool_name, args) {
            return true;
        }
        false
    }

    /// Check whether a specific pattern string is already present in the
    /// runtime approvals for a tool (used to filter already-approved patterns
    /// from the confirm dialog options).
    pub fn has_pattern(&self, tool_name: &str, pattern: &str) -> bool {
        let check = |pats: Option<&Vec<glob::Pattern>>| -> bool {
            pats.is_some_and(|ps| ps.iter().any(|p| p.as_str() == pattern))
        };
        check(self.session_tools.get(tool_name)) || check(self.workspace_tools.get(tool_name))
    }

    /// Iterate session tool approvals (for display in status UI).
    pub fn session_tool_entries(&self) -> Vec<(String, Vec<String>)> {
        let mut tools: Vec<_> = self.session_tools.keys().cloned().collect();
        tools.sort();
        tools
            .into_iter()
            .map(|t| {
                let pats: Vec<String> = self.session_tools[&t]
                    .iter()
                    .map(|p| p.as_str().to_string())
                    .collect();
                (t, pats)
            })
            .collect()
    }

    /// Session directory approvals (for display in status UI).
    pub fn session_dirs(&self) -> &[PathBuf] {
        &self.session_dirs
    }

    /// Rebuild session state from flattened entries (used by permissions sync UI).
    pub fn set_session(&mut self, tools: HashMap<String, Vec<glob::Pattern>>, dirs: Vec<PathBuf>) {
        self.session_tools = tools;
        self.session_dirs = dirs
            .into_iter()
            .map(|d| crate::paths::expand_tilde(&d))
            .collect();
    }

    /// Check whether all given outside-workspace paths are covered by
    /// approved directories.  Stored dirs are always in expanded (absolute)
    /// form — only the incoming paths need tilde expansion.
    pub fn dirs_approved(&self, paths: &[String]) -> bool {
        if paths.is_empty() {
            return true;
        }
        let all_dirs: Vec<&PathBuf> = self
            .session_dirs
            .iter()
            .chain(self.workspace_dirs.iter())
            .collect();
        if all_dirs.is_empty() {
            return false;
        }
        paths.iter().all(|p| {
            let path = crate::paths::expand_tilde(Path::new(p));
            let dir = path.parent().unwrap_or(&path);
            all_dirs
                .iter()
                .any(|ad| dir.starts_with(ad.as_path()) || path.starts_with(ad.as_path()))
        })
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
                    if let Some((_header_end, body_end)) = parse_heredoc(cmd, i) {
                        i = body_end;
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

/// Parse a heredoc starting at `cmd[i..]` (which must begin with `<<`).
/// Returns `Some((header_end, body_end))` where:
///   - `header_end` is the byte offset (relative to `cmd`) just past the
///     delimiter word (e.g. after `'EOF'` in `<< 'EOF'`).
///   - `body_end` is the byte offset past the closing delimiter line
///     (the `\n` after `EOF`), or `cmd.len()` if no closing delimiter.
///
/// Returns `None` if no valid delimiter is found.
fn parse_heredoc(cmd: &str, i: usize) -> Option<(usize, usize)> {
    let rest = &cmd[i..];
    let bytes = cmd.as_bytes();
    let len = cmd.len();

    let mut hi = 2; // skip "<<"
    if hi < rest.len() && rest.as_bytes()[hi] == b'-' {
        hi += 1;
    }
    // Skip whitespace before delimiter.
    while hi < rest.len() && rest.as_bytes()[hi] == b' ' {
        hi += 1;
    }
    // Read the delimiter (strip quotes).
    let mut delim_start = hi;
    let mut strip_quotes = false;
    if hi < rest.len() && (rest.as_bytes()[hi] == b'\'' || rest.as_bytes()[hi] == b'"') {
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
    if delim.is_empty() {
        return None;
    }
    if strip_quotes && hi < rest.len() {
        hi += 1; // skip closing quote
    }
    let header_end = i + hi;

    // Scan for the closing delimiter on its own line.
    let mut si = header_end;
    while si < len {
        if bytes[si] == b'\n' {
            let line_start = si + 1;
            let line_end = cmd[line_start..]
                .find('\n')
                .map(|p| line_start + p)
                .unwrap_or(len);
            let line = cmd[line_start..line_end].trim();
            if line == delim {
                return Some((header_end, line_end));
            }
        }
        si += 1;
    }
    // No closing delimiter — consume rest.
    Some((header_end, len))
}

/// Strip heredoc bodies from a command string so that downstream parsing
/// (e.g. `extract_embedded_commands`) does not misinterpret content inside
/// heredocs as shell constructs like `(...)` subshells.
fn strip_heredoc_bodies(cmd: &str) -> String {
    let bytes = cmd.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len);
    let mut i = 0;

    while i < len {
        match bytes[i] {
            b'\'' => {
                let start = i;
                i += 1;
                while i < len && bytes[i] != b'\'' {
                    i += 1;
                }
                if i < len {
                    i += 1;
                }
                out.push_str(&cmd[start..i]);
            }
            b'"' => {
                let start = i;
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
                out.push_str(&cmd[start..i]);
            }
            b'\\' if i + 1 < len => {
                out.push_str(&cmd[i..i + 2]);
                i += 2;
            }
            _ => {
                let rest = &cmd[i..];
                if rest.starts_with("<<") {
                    if let Some((header_end, body_end)) = parse_heredoc(cmd, i) {
                        // Keep the header (e.g. "<< 'EOF'") and the closing
                        // delimiter line, but drop the body in between.
                        out.push_str(&cmd[i..header_end]);
                        // Find the closing delimiter line start (\n before it).
                        if body_end < len || cmd[header_end..body_end].contains('\n') {
                            // The last \nDELIM portion: find where the closing
                            // delimiter line begins.
                            if let Some(last_nl) = cmd[header_end..body_end].rfind('\n') {
                                out.push_str(&cmd[header_end + last_nl..body_end]);
                            }
                        }
                        i = body_end;
                        continue;
                    }
                }
                out.push(bytes[i] as char);
                i += 1;
            }
        }
    }
    out
}

/// Extract commands embedded in $(...), `...`, and (...) subshells.
/// Returns additional commands found inside these constructs.
/// The original command is kept as-is (for pattern matching); these are extras
/// that also need permission checks.
fn extract_embedded_commands(raw_cmd: &str) -> Vec<String> {
    let stripped = strip_heredoc_bodies(raw_cmd);
    let cmd: &str = &stripped;
    let mut extra = Vec::new();
    let bytes = cmd.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    let mut in_dquote = false;

    while i < len {
        match bytes[i] {
            // Single quotes are fully opaque — no expansions inside.
            // Inside double quotes single quotes are literal, so skip this.
            b'\'' if !in_dquote => {
                i += 1;
                while i < len && bytes[i] != b'\'' {
                    i += 1;
                }
                if i < len {
                    i += 1;
                }
            }
            // Toggle double-quote state. Bash expands $() and backticks
            // inside double quotes, so we keep scanning — but plain (...)
            // subshells are NOT expanded inside double quotes.
            b'"' => {
                in_dquote = !in_dquote;
                i += 1;
            }
            b'\\' if i + 1 < len => {
                i += 2;
            }
            // $( ... ) — valid both inside and outside double quotes
            b'$' if i + 1 < len && bytes[i + 1] == b'(' => {
                i += 2;
                if let Some((inner, end)) = find_matching_paren(cmd, i) {
                    for sub in split_shell_commands(inner) {
                        extra.push(sub);
                    }
                    i = end + 1;
                }
            }
            // backtick substitution — valid both inside and outside double quotes
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
            // ( ... ) subshell — only outside quotes
            b'(' if !in_dquote => {
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

/// Check whether a sub-command is a bare `cd` invocation (e.g. `cd /tmp`,
/// `cd`, `cd -`).  Permission for the target directory is handled by the
/// workspace path restriction in [`Permissions::decide`], so `cd` itself
/// is always allowed at the command level.
fn is_cd_command(subcmd: &str) -> bool {
    let trimmed = subcmd.trim();
    trimmed == "cd" || trimmed.starts_with("cd ") || trimmed.starts_with("cd\t")
}

/// Check whether a command contains an output redirection (`>`, `>>`, `&>`, `&>>`)
/// to a real file.  Redirects to `/dev/null` are harmless and ignored.
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
                } else {
                    // Input redirection <, not output.
                    i += 1;
                }
            }
            b'&' if i + 1 < len && bytes[i + 1] == b'>' => {
                // &> or &>> — output redirection.
                i += 1; // now on '>'
                if !redirect_is_dev_null(bytes, &mut i) {
                    return true;
                }
            }
            b'>' => {
                // >&N is fd duplication (e.g. 2>&1), not file output.
                if i + 1 < len && bytes[i + 1] == b'&' {
                    let j = i + 2;
                    if j < len && bytes[j].is_ascii_digit() {
                        i = j + 1;
                        continue;
                    }
                    // >& without digit — treat as real redirection.
                }
                if !redirect_is_dev_null(bytes, &mut i) {
                    return true;
                }
            }
            _ => {
                i += 1;
            }
        }
    }
    false
}

/// Starting at a `>` in `bytes[*pos]`, skip past `>` or `>>` and whitespace,
/// then check whether the target is `/dev/null`.  Advances `*pos` past the
/// target on match so the caller can continue scanning.
fn redirect_is_dev_null(bytes: &[u8], pos: &mut usize) -> bool {
    let len = bytes.len();
    let mut j = *pos;
    // Skip > or >>
    if j < len && bytes[j] == b'>' {
        j += 1;
    }
    if j < len && bytes[j] == b'>' {
        j += 1; // >>
    }
    // Skip whitespace
    while j < len && (bytes[j] == b' ' || bytes[j] == b'\t') {
        j += 1;
    }
    const DEV_NULL: &[u8] = b"/dev/null";
    if j + DEV_NULL.len() <= len && &bytes[j..j + DEV_NULL.len()] == DEV_NULL {
        let end = j + DEV_NULL.len();
        // Must be followed by a word boundary (whitespace, shell operator, or end).
        if end == len || !bytes[end].is_ascii_alphanumeric() && bytes[end] != b'/' {
            *pos = end;
            return true;
        }
    }
    // Not /dev/null — don't advance pos; caller will return true.
    *pos += 1;
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
            (Decision::Allow, Decision::Ask) => Decision::Ask,
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
    // Strip heredoc bodies — they are data, not shell commands.
    let cmd = strip_heredoc_bodies(cmd);
    let mut paths = Vec::new();
    for token in cmd.split_whitespace() {
        let clean = token.trim_matches(|c: char| c == '\'' || c == '"' || c == ';');
        if (clean.starts_with('/') && clean.len() > 1) || clean.starts_with("~/") {
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

    fn empty_ruleset() -> RuleSet {
        RuleSet {
            allow: vec![],
            ask: vec![],
            deny: vec![],
        }
    }

    fn perms_with_bash(allow: &[&str], ask: &[&str], deny: &[&str]) -> Permissions {
        let mode = ModePerms {
            tools: HashMap::new(),
            bash: ruleset(allow, ask, deny),
            web_fetch: empty_ruleset(),
            mcp: empty_ruleset(),
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

    #[track_caller]
    fn assert_bash(
        allow: &[&str],
        ask: &[&str],
        deny: &[&str],
        mode: Mode,
        cmd: &str,
        expected: Decision,
    ) {
        let p = perms_with_bash(allow, ask, deny);
        assert_eq!(p.check_bash(mode, cmd), expected);
    }

    #[test]
    fn yolo_allows_mcp_by_default() {
        let p = perms_with_bash(&[], &[], &[]);
        assert_eq!(
            p.check_mcp(Mode::Yolo, "filesystem_read_file"),
            Decision::Allow
        );
    }

    #[test]
    fn normal_mode_asks_for_mcp_by_default() {
        let p = perms_with_bash(&[], &[], &[]);
        assert_eq!(
            p.check_mcp(Mode::Normal, "filesystem_read_file"),
            Decision::Ask
        );
    }

    // --- simple commands ---

    #[test]
    fn simple_allowed() {
        assert_bash(&["ls *"], &[], &[], Mode::Normal, "ls -la", Decision::Allow);
    }

    #[test]
    fn simple_denied() {
        assert_bash(
            &[],
            &[],
            &["rm *"],
            Mode::Normal,
            "rm -rf /",
            Decision::Deny,
        );
    }

    #[test]
    fn simple_ask() {
        assert_bash(&[], &["rm *"], &[], Mode::Normal, "rm -rf /", Decision::Ask);
    }

    // --- deny rules with chained commands ---

    #[test]
    fn deny_rm_simple() {
        assert_bash(
            &[],
            &[],
            &["rm *"],
            Mode::Normal,
            "rm -rf /",
            Decision::Deny,
        );
    }

    #[test]
    fn deny_rm_after_ls() {
        assert_bash(
            &["ls *"],
            &[],
            &["rm *"],
            Mode::Normal,
            "ls && rm -rf /",
            Decision::Deny,
        );
    }

    #[test]
    fn deny_rm_before_ls() {
        assert_bash(
            &["ls *"],
            &[],
            &["rm *"],
            Mode::Normal,
            "rm -rf / && ls",
            Decision::Deny,
        );
    }

    // --- ask rules with chained commands ---

    #[test]
    fn ask_rm_simple() {
        assert_bash(&[], &["rm *"], &[], Mode::Normal, "rm -rf /", Decision::Ask);
    }

    #[test]
    fn ask_rm_after_ls() {
        assert_bash(
            &["ls *"],
            &["rm *"],
            &[],
            Mode::Normal,
            "ls && rm -rf /",
            Decision::Ask,
        );
    }

    #[test]
    fn ask_rm_before_ls() {
        assert_bash(
            &["ls *"],
            &["rm *"],
            &[],
            Mode::Normal,
            "rm -rf / && ls",
            Decision::Ask,
        );
    }

    // --- allow rule should not match chained commands ---

    #[test]
    fn allow_ls_does_not_allow_chained_rm() {
        assert_bash(
            &["ls *"],
            &[],
            &[],
            Mode::Normal,
            "ls && rm README.md",
            Decision::Ask,
        );
    }

    // --- both sub-commands allowed ---

    #[test]
    fn chained_both_allowed() {
        assert_bash(
            &["ls *", "rm *"],
            &[],
            &[],
            Mode::Normal,
            "ls && rm README.md",
            Decision::Allow,
        );
    }

    // --- pipes ---

    #[test]
    fn pipe_both_allowed() {
        assert_bash(
            &["cat *", "grep *"],
            &[],
            &[],
            Mode::Normal,
            "cat file.txt | grep foo",
            Decision::Allow,
        );
    }

    #[test]
    fn pipe_second_not_allowed() {
        assert_bash(
            &["cat *"],
            &[],
            &[],
            Mode::Normal,
            "cat file.txt | rm foo",
            Decision::Ask,
        );
    }

    // --- semicolon ---

    #[test]
    fn semicolon_second_denied() {
        assert_bash(
            &["echo *"],
            &[],
            &["rm *"],
            Mode::Normal,
            "echo hi; rm -rf /",
            Decision::Deny,
        );
    }

    // --- or chain ---

    #[test]
    fn or_chain_both_allowed() {
        assert_bash(
            &["make *"],
            &[],
            &[],
            Mode::Normal,
            "make || make install",
            Decision::Allow,
        );
    }

    // --- deny wins over allow ---

    #[test]
    fn deny_wins_over_allow() {
        assert_bash(
            &["rm *"],
            &[],
            &["rm *"],
            Mode::Normal,
            "rm foo",
            Decision::Deny,
        );
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
        assert_bash(&["ls *"], &[], &[], Mode::Normal, "", Decision::Ask);
    }

    #[test]
    fn whitespace_only_command() {
        assert_bash(&["ls *"], &[], &[], Mode::Normal, "   ", Decision::Ask);
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
        assert_bash(&["ls *"], &[], &[], Mode::Normal, "ls &&", Decision::Allow);
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
        assert_bash(&["ls *"], &[], &[], Mode::Normal, "ls", Decision::Allow);
    }

    #[test]
    fn trailing_space_no_false_positive() {
        assert_bash(&["ls *"], &[], &[], Mode::Normal, "lsof", Decision::Ask);
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
        assert_bash(
            &["ls *"],
            &[],
            &["rm *"],
            Mode::Normal,
            "ls ;; rm foo",
            Decision::Deny,
        );
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
    fn heredoc_with_chained_command_not_split() {
        let cmd = "cd /tmp && uv run python3 << 'EOF'\nfrom main import load\ndata = load(num_features=25)\nEOF";
        let cmds = split_shell_commands(cmd);
        assert_eq!(cmds.len(), 2, "Expected [cd, uv], got: {cmds:?}");
        assert!(cmds[0].starts_with("cd "), "first: {:?}", cmds[0]);
        assert!(cmds[1].starts_with("uv "), "second: {:?}", cmds[1]);
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
        assert_bash(
            &["cargo *"],
            &[],
            &[],
            Mode::Normal,
            "cargo build 2>&1",
            Decision::Allow,
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
            web_fetch: empty_ruleset(),
            mcp: empty_ruleset(),
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
        assert_eq!(
            p.decide(Mode::Normal, "read_file", &args, false),
            Decision::Allow
        );
    }

    #[test]
    fn workspace_downgrades_file_outside() {
        let p = perms_with_workspace("/home/user/project");
        let args = args_with("file_path", "/etc/passwd");
        assert_eq!(
            p.decide(Mode::Normal, "read_file", &args, false),
            Decision::Ask
        );
    }

    #[test]
    fn workspace_allows_relative_path() {
        let p = perms_with_workspace("/home/user/project");
        let args = args_with("file_path", "src/main.rs");
        assert_eq!(
            p.decide(Mode::Normal, "write_file", &args, false),
            Decision::Allow
        );
    }

    #[test]
    fn workspace_downgrades_bash_outside() {
        let p = perms_with_workspace("/home/user/project");
        let args = args_with("command", "rm -rf /tmp/foo");
        assert_eq!(p.decide(Mode::Normal, "bash", &args, false), Decision::Ask);
    }

    #[test]
    fn workspace_allows_bash_inside() {
        let p = perms_with_workspace("/home/user/project");
        let args = args_with("command", "rm -rf /home/user/project/target");
        assert_eq!(
            p.decide(Mode::Normal, "bash", &args, false),
            Decision::Allow
        );
    }

    #[test]
    fn workspace_allows_bash_relative() {
        let p = perms_with_workspace("/home/user/project");
        let args = args_with("command", "cargo build");
        assert_eq!(
            p.decide(Mode::Normal, "bash", &args, false),
            Decision::Allow
        );
    }

    #[test]
    fn workspace_downgrades_yolo_outside() {
        let p = perms_with_workspace("/home/user/project");
        let args = args_with("command", "rm -rf /etc");
        // Even in yolo, out-of-workspace should ask
        assert_eq!(p.decide(Mode::Yolo, "bash", &args, false), Decision::Ask);
    }

    #[test]
    fn workspace_yolo_allows_inside() {
        let p = perms_with_workspace("/home/user/project");
        let args = args_with("file_path", "/home/user/project/foo.txt");
        assert_eq!(
            p.decide(Mode::Yolo, "write_file", &args, false),
            Decision::Allow
        );
    }

    #[test]
    fn workspace_restriction_off_allows_everything() {
        let mut p = perms_with_workspace("/home/user/project");
        p.restrict_to_workspace = false;
        let args = args_with("file_path", "/etc/passwd");
        assert_eq!(
            p.decide(Mode::Normal, "read_file", &args, false),
            Decision::Allow
        );
    }

    #[test]
    fn workspace_ask_stays_ask() {
        // If the tool is already Ask (not Allow), workspace restriction doesn't change it
        let mut p = perms_with_workspace("/home/user/project");
        // Remove write_file from allowed tools so it defaults to Ask
        p.normal.tools.remove("write_file");
        let args = args_with("file_path", "/home/user/project/foo.txt");
        // Even inside workspace, Ask stays Ask because the tool itself is Ask
        assert_eq!(
            p.decide(Mode::Normal, "write_file", &args, false),
            Decision::Ask
        );
    }

    #[test]
    fn workspace_glob_outside_downgrades() {
        let p = perms_with_workspace("/home/user/project");
        let args = args_with("path", "/tmp");
        assert_eq!(p.decide(Mode::Normal, "glob", &args, false), Decision::Ask);
    }

    #[test]
    fn workspace_no_path_tools_unaffected() {
        let p = perms_with_workspace("/home/user/project");
        let args = HashMap::new();
        // web_search has no paths, should not be affected
        assert_eq!(
            p.decide(Mode::Yolo, "web_search", &args, false),
            Decision::Allow
        );
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
        assert!(has_output_redirection("cargo build &> output.log"));
    }

    #[test]
    fn has_output_redirection_double_ampersand_greater() {
        assert!(has_output_redirection("cargo build &>> output.log"));
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
    fn dev_null_redirect_not_escalated() {
        assert!(!has_output_redirection("find /tmp 2>/dev/null"));
    }

    #[test]
    fn dev_null_redirect_with_space() {
        assert!(!has_output_redirection("find /tmp 2> /dev/null"));
    }

    #[test]
    fn dev_null_stdout_redirect() {
        assert!(!has_output_redirection("echo hello > /dev/null"));
    }

    #[test]
    fn dev_null_append_redirect() {
        assert!(!has_output_redirection("echo hello >> /dev/null"));
    }

    #[test]
    fn dev_null_ampersand_redirect() {
        assert!(!has_output_redirection("cargo build &> /dev/null"));
    }

    #[test]
    fn dev_null_in_chain_not_escalated() {
        // 2>/dev/null is harmless, the whole command should stay allowed
        assert!(!has_output_redirection(
            "tree -L 3 /tmp 2>/dev/null || find /tmp -type d"
        ));
    }

    #[test]
    fn dev_null_mixed_with_real_redirect() {
        // One redirect to /dev/null, but another to a real file — should escalate
        assert!(has_output_redirection("cmd 2>/dev/null > out.txt"));
    }

    #[test]
    fn auto_allowed_with_dev_null_stays_allow() {
        assert_bash(
            &["find *"],
            &[],
            &[],
            Mode::Normal,
            "find /tmp 2>/dev/null",
            Decision::Allow,
        );
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
        assert_bash(
            &["cat *"],
            &[],
            &[],
            Mode::Normal,
            "cat file.txt >> output.txt",
            Decision::Ask,
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

    // --- specificity: specific ask beats broad allow ---

    #[test]
    fn specific_ask_beats_broad_allow() {
        let rs = ruleset(&["git *"], &["git push *"], &[]);
        assert_eq!(check_ruleset(&rs, "git push foo"), Decision::Ask);
    }

    #[test]
    fn broad_allow_still_works_for_non_specific() {
        let rs = ruleset(&["git *"], &["git push *"], &[]);
        assert_eq!(check_ruleset(&rs, "git status"), Decision::Allow);
    }

    // --- bash decide_base: tool Allow + pattern Ask = Ask ---

    #[test]
    fn bash_tool_allow_pattern_ask() {
        let mode = ModePerms {
            tools: {
                let mut m = HashMap::new();
                m.insert("bash".to_string(), Decision::Allow);
                m
            },
            bash: ruleset(&[], &["git push *"], &[]),
            web_fetch: empty_ruleset(),
            mcp: empty_ruleset(),
        };
        let perms = Permissions {
            normal: mode.clone(),
            plan: mode.clone(),
            apply: mode.clone(),
            yolo: mode,
            restrict_to_workspace: false,
            workspace: PathBuf::new(),
        };
        let args = args_with("command", "git push origin main");
        assert_eq!(
            decide_base(&perms, Mode::Yolo, "bash", &args),
            Decision::Ask
        );
    }

    // --- override tightening: base Allow, override Ask → Ask ---

    #[test]
    fn override_tightens_allow_to_ask() {
        let mode = ModePerms {
            tools: {
                let mut m = HashMap::new();
                m.insert("bash".to_string(), Decision::Allow);
                m
            },
            bash: empty_ruleset(),
            web_fetch: empty_ruleset(),
            mcp: empty_ruleset(),
        };
        let perms = Permissions {
            normal: mode.clone(),
            plan: mode.clone(),
            apply: mode.clone(),
            yolo: mode,
            restrict_to_workspace: false,
            workspace: PathBuf::new(),
        };
        let overrides = protocol::PermissionOverrides {
            tools: Some(protocol::RuleSetOverride {
                allow: vec![],
                ask: vec!["bash".to_string()],
                deny: vec![],
            }),
            bash: None,
            web_fetch: None,
        };
        let tightened = perms.with_overrides(&overrides);
        assert_eq!(tightened.check_tool(Mode::Yolo, "bash"), Decision::Ask);
    }

    // --- cd command handling ---

    #[test]
    fn cd_alone_is_allowed() {
        assert_bash(&[], &[], &[], Mode::Normal, "cd /tmp", Decision::Allow);
    }

    #[test]
    fn cd_no_args_is_allowed() {
        assert_bash(&[], &[], &[], Mode::Normal, "cd", Decision::Allow);
    }

    #[test]
    fn cd_in_chain_does_not_escalate() {
        // cd should not contribute to the worst decision; only ls matters
        let p = perms_with_bash(&["ls *"], &[], &[]);
        assert_eq!(
            p.check_bash(Mode::Normal, "cd /tmp && ls -la"),
            Decision::Allow
        );
    }

    #[test]
    fn cd_with_denied_command_still_denies() {
        assert_bash(
            &[],
            &[],
            &["rm *"],
            Mode::Normal,
            "cd /tmp && rm -rf foo",
            Decision::Deny,
        );
    }

    #[test]
    fn cd_outside_workspace_downgrades_to_ask() {
        // cd itself is Allow, but the workspace path restriction catches /tmp
        let p = perms_with_workspace("/home/user/project");
        let args = args_with("command", "cd /tmp && ls");
        assert_eq!(p.decide(Mode::Normal, "bash", &args, false), Decision::Ask);
    }

    #[test]
    fn cd_inside_workspace_stays_allowed() {
        let p = perms_with_workspace("/home/user/project");
        let args = args_with("command", "cd /home/user/project/src && ls");
        assert_eq!(
            p.decide(Mode::Normal, "bash", &args, false),
            Decision::Allow
        );
    }

    #[test]
    fn cd_workspace_root_stays_allowed() {
        let p = perms_with_workspace("/home/user/project");
        let args = args_with("command", "cd /home/user/project && cargo build");
        assert_eq!(
            p.decide(Mode::Normal, "bash", &args, false),
            Decision::Allow
        );
    }

    #[test]
    fn heredoc_paths_not_extracted() {
        // Paths inside heredoc bodies are data, not shell arguments.
        let cmd = "python3 << 'PYEOF'\nwith open('/tmp/secret') as f:\n    pass\nPYEOF";
        let paths = extract_paths_from_command(cmd);
        assert!(paths.is_empty(), "got: {paths:?}");
    }

    #[test]
    fn heredoc_paths_outside_body_still_extracted() {
        let cmd = "cd /tmp && python3 << 'EOF'\nopen('/etc/passwd')\nEOF";
        let paths = extract_paths_from_command(cmd);
        assert_eq!(paths, vec!["/tmp"]);
    }

    #[test]
    fn runtime_tool_approval_does_not_bypass_workspace_restriction() {
        let p = perms_with_workspace("/home/user/project");
        let mut rt = RuntimeApprovals::new();
        rt.add_session_tool("bash", vec![glob::Pattern::new("rm *").unwrap()]);
        let args = args_with("command", "rm -rf /tmp/foo");
        assert!(!rt.is_auto_approved(&p, Mode::Normal, "bash", &args, "rm -rf /tmp/foo"));
    }

    #[test]
    fn runtime_tool_and_dir_approval_allow_outside_workspace_request() {
        let p = perms_with_workspace("/home/user/project");
        let mut rt = RuntimeApprovals::new();
        rt.add_session_tool("bash", vec![glob::Pattern::new("rm *").unwrap()]);
        rt.add_session_dir(PathBuf::from("/tmp"));
        let args = args_with("command", "rm -rf /tmp/foo");
        assert!(rt.is_auto_approved(&p, Mode::Normal, "bash", &args, "rm -rf /tmp/foo"));
    }

    #[test]
    fn runtime_dir_approval_allows_default_allowed_command_outside_workspace() {
        let p = perms_with_workspace("/home/user/project");
        let mut rt = RuntimeApprovals::new();
        rt.add_session_dir(PathBuf::from("/tmp"));
        let args = args_with("command", "cat /tmp/foo");
        assert!(rt.is_auto_approved(&p, Mode::Normal, "bash", &args, "cat /tmp/foo"));
    }

    #[test]
    fn runtime_tool_approval_allows_inside_workspace_request() {
        let p = perms_with_workspace("/home/user/project");
        let mut rt = RuntimeApprovals::new();
        rt.add_session_tool("bash", vec![glob::Pattern::new("rm *").unwrap()]);
        let args = args_with("command", "rm -rf /home/user/project/target");
        assert!(rt.is_auto_approved(
            &p,
            Mode::Normal,
            "bash",
            &args,
            "rm -rf /home/user/project/target",
        ));
    }

    #[test]
    fn runtime_dir_approval_does_not_affect_inside_workspace_request() {
        let p = perms_with_workspace("/home/user/project");
        let mut rt = RuntimeApprovals::new();
        rt.add_session_dir(PathBuf::from("/tmp"));
        let args = args_with("command", "rm -rf /home/user/project/target");
        assert!(!rt.is_auto_approved(
            &p,
            Mode::Normal,
            "bash",
            &args,
            "rm -rf /home/user/project/target",
        ));
    }

    // --- tilde path normalization in dirs_approved ---

    #[test]
    fn dirs_approved_tilde_stored_absolute_queried() {
        let mut rt = RuntimeApprovals::new();
        rt.add_session_dir(PathBuf::from("~/syncthing"));
        let home = crate::paths::home_dir();
        let abs = format!("{}/syncthing/vault/file.txt", home.display());
        assert!(rt.dirs_approved(&[abs]));
    }

    #[test]
    fn dirs_approved_absolute_stored_tilde_queried() {
        let home = crate::paths::home_dir();
        let mut rt = RuntimeApprovals::new();
        rt.add_session_dir(home.join("syncthing"));
        assert!(rt.dirs_approved(&["~/syncthing/vault/file.txt".to_string()]));
    }

    #[test]
    fn dirs_approved_both_tilde() {
        let mut rt = RuntimeApprovals::new();
        rt.add_session_dir(PathBuf::from("~/syncthing"));
        assert!(rt.dirs_approved(&["~/syncthing/vault".to_string()]));
    }

    #[test]
    fn dirs_approved_both_absolute() {
        let mut rt = RuntimeApprovals::new();
        rt.add_session_dir(PathBuf::from("/tmp/data"));
        assert!(rt.dirs_approved(&["/tmp/data/subdir/file.txt".to_string()]));
    }

    #[test]
    fn dirs_approved_no_false_prefix_match() {
        let mut rt = RuntimeApprovals::new();
        rt.add_session_dir(PathBuf::from("~/sync"));
        // ~/syncthing should NOT match ~/sync (different directory)
        assert!(!rt.dirs_approved(&["~/syncthing/file.txt".to_string()]));
    }

    #[test]
    fn dirs_approved_exact_dir_match() {
        let mut rt = RuntimeApprovals::new();
        rt.add_session_dir(PathBuf::from("~/syncthing/vault"));
        // File directly in the approved dir
        assert!(rt.dirs_approved(&["~/syncthing/vault/file.txt".to_string()]));
    }

    #[test]
    fn dirs_approved_parent_not_covered() {
        let mut rt = RuntimeApprovals::new();
        rt.add_session_dir(PathBuf::from("~/syncthing/vault"));
        // Parent dir is NOT covered
        assert!(!rt.dirs_approved(&["~/syncthing/other/file.txt".to_string()]));
    }

    #[test]
    fn dirs_approved_path_is_dir_itself() {
        let mut rt = RuntimeApprovals::new();
        rt.add_session_dir(PathBuf::from("/tmp"));
        // Path that IS the approved dir (e.g. a directory argument)
        assert!(rt.dirs_approved(&["/tmp".to_string()]));
    }

    #[test]
    fn dirs_approved_multiple_paths_all_covered() {
        let mut rt = RuntimeApprovals::new();
        rt.add_session_dir(PathBuf::from("~/syncthing"));
        rt.add_session_dir(PathBuf::from("/tmp"));
        assert!(rt.dirs_approved(&[
            "~/syncthing/vault/a.txt".to_string(),
            "/tmp/b.txt".to_string(),
        ]));
    }

    #[test]
    fn dirs_approved_multiple_paths_one_uncovered() {
        let mut rt = RuntimeApprovals::new();
        rt.add_session_dir(PathBuf::from("~/syncthing"));
        assert!(!rt.dirs_approved(&[
            "~/syncthing/vault/a.txt".to_string(),
            "/tmp/b.txt".to_string(),
        ]));
    }

    // --- tilde normalization in is_auto_approved ---

    fn perms_with_workspace_default_bash(workspace: &str) -> Permissions {
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
                allow: compile_patterns(
                    &DEFAULT_BASH_ALLOW
                        .iter()
                        .map(|s| s.to_string())
                        .collect::<Vec<_>>(),
                ),
                ask: vec![],
                deny: vec![],
            },
            web_fetch: empty_ruleset(),
            mcp: empty_ruleset(),
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

    #[test]
    fn tilde_dir_approval_works_for_absolute_read_file() {
        let home = crate::paths::home_dir();
        let workspace = format!("{}/dev/project", home.display());
        let p = perms_with_workspace(&workspace);
        let mut rt = RuntimeApprovals::new();
        rt.add_session_dir(PathBuf::from("~/syncthing"));
        let file = format!("{}/syncthing/vault/notes.md", home.display());
        let args = args_with("file_path", &file);
        // read_file is Allow by default, downgraded to Ask by workspace restriction.
        // Dir approval should lift the restriction.
        assert!(rt.is_auto_approved(&p, Mode::Normal, "read_file", &args, &file));
    }

    #[test]
    fn absolute_dir_approval_works_for_tilde_bash() {
        let home = crate::paths::home_dir();
        let workspace = format!("{}/dev/project", home.display());
        let p = perms_with_workspace(&workspace);
        let mut rt = RuntimeApprovals::new();
        rt.add_session_dir(home.join("syncthing"));
        let args = args_with("command", "cat ~/syncthing/vault/notes.md");
        assert!(rt.is_auto_approved(
            &p,
            Mode::Normal,
            "bash",
            &args,
            "cat ~/syncthing/vault/notes.md",
        ));
    }

    #[test]
    fn dir_approval_alone_insufficient_for_ask_command_outside_workspace() {
        let home = crate::paths::home_dir();
        let workspace = format!("{}/dev/project", home.display());
        let p = perms_with_workspace_default_bash(&workspace);
        let mut rt = RuntimeApprovals::new();
        rt.add_session_dir(PathBuf::from("~/syncthing"));
        // `rm` is not in DEFAULT_BASH_ALLOW → Ask. Dir approval alone shouldn't
        // auto-approve a command that requires its own permission.
        let args = args_with("command", "rm ~/syncthing/vault/old.md");
        assert!(!rt.is_auto_approved(
            &p,
            Mode::Normal,
            "bash",
            &args,
            "rm ~/syncthing/vault/old.md",
        ));
    }

    #[test]
    fn dir_plus_tool_approval_for_ask_command_outside_workspace() {
        let home = crate::paths::home_dir();
        let workspace = format!("{}/dev/project", home.display());
        let p = perms_with_workspace_default_bash(&workspace);
        let mut rt = RuntimeApprovals::new();
        rt.add_session_dir(PathBuf::from("~/syncthing"));
        rt.add_session_tool("bash", vec![glob::Pattern::new("rm *").unwrap()]);
        let args = args_with("command", "rm ~/syncthing/vault/old.md");
        assert!(rt.is_auto_approved(
            &p,
            Mode::Normal,
            "bash",
            &args,
            "rm ~/syncthing/vault/old.md",
        ));
    }

    #[test]
    fn compound_command_default_allowed_with_dir_approval() {
        // `find | sort` — both are in DEFAULT_BASH_ALLOW.
        // With wildcard bash allow (perms_with_workspace), both are allowed
        // at the base level, so was_downgraded is true and dir approval suffices.
        let p = perms_with_workspace("/home/user/project");
        let mut rt = RuntimeApprovals::new();
        rt.add_session_dir(PathBuf::from("/tmp"));
        let args = args_with("command", "find /tmp/data -type f | sort");
        assert!(rt.is_auto_approved(
            &p,
            Mode::Normal,
            "bash",
            &args,
            "find /tmp/data -type f | sort",
        ));
    }

    #[test]
    fn compound_command_with_ask_subcommand_needs_tool_approval() {
        // `find | python3` — find is in DEFAULT_BASH_ALLOW, python3 is not.
        // Dir approval alone is insufficient; the Ask subcommand needs its own approval.
        let home = crate::paths::home_dir();
        let workspace = format!("{}/dev/project", home.display());
        let p = perms_with_workspace_default_bash(&workspace);
        let mut rt = RuntimeApprovals::new();
        rt.add_session_dir(PathBuf::from("/tmp"));
        let args = args_with("command", "find /tmp/data -name '*.py' | python3");
        assert!(!rt.is_auto_approved(
            &p,
            Mode::Normal,
            "bash",
            &args,
            "find /tmp/data -name '*.py' | python3",
        ));
    }

    #[test]
    fn compound_command_with_ask_subcommand_and_tool_approval() {
        let home = crate::paths::home_dir();
        let workspace = format!("{}/dev/project", home.display());
        let p = perms_with_workspace_default_bash(&workspace);
        let mut rt = RuntimeApprovals::new();
        rt.add_session_dir(PathBuf::from("/tmp"));
        rt.add_session_tool("bash", vec![glob::Pattern::new("python3 *").unwrap()]);
        let args = args_with("command", "find /tmp/data -name '*.py' | python3");
        assert!(rt.is_auto_approved(
            &p,
            Mode::Normal,
            "bash",
            &args,
            "find /tmp/data -name '*.py' | python3",
        ));
    }
}
