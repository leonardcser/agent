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

    ModePerms {
        tools,
        bash: RuleSet {
            allow: compile_patterns(&raw.bash.allow),
            ask: compile_patterns(&raw.bash.ask),
            deny: compile_patterns(&raw.bash.deny),
        },
    }
}

impl Permissions {
    pub fn load() -> Self {
        let path = crate::config::config_dir().join("permissions.yaml");
        let contents = std::fs::read_to_string(&path).unwrap_or_default();
        let raw: RawConfig = serde_yml::from_str(&contents).unwrap_or_default();
        Self {
            normal: build_mode(&raw.permissions.normal, Mode::Normal),
            plan: build_mode(&raw.permissions.normal, Mode::Plan),
            apply: build_mode(&raw.permissions.apply, Mode::Apply),
        }
    }

    pub fn check_tool(&self, mode: Mode, tool_name: &str) -> Decision {
        let perms = match mode {
            Mode::Normal => &self.normal,
            Mode::Plan => &self.plan,
            Mode::Apply => &self.apply,
        };
        perms.tools.get(tool_name).cloned().unwrap_or(Decision::Ask)
    }

    pub fn check_bash(&self, mode: Mode, command: &str) -> Decision {
        let perms = match mode {
            Mode::Normal => &self.normal,
            Mode::Plan => &self.plan,
            Mode::Apply => &self.apply,
        };
        // Deny wins
        for pat in &perms.bash.deny {
            if pat.matches(command) {
                return Decision::Deny;
            }
        }
        for pat in &perms.bash.allow {
            if pat.matches(command) {
                return Decision::Allow;
            }
        }
        for pat in &perms.bash.ask {
            if pat.matches(command) {
                return Decision::Ask;
            }
        }
        Decision::Ask
    }
}
