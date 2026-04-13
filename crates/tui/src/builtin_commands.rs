use crate::custom_commands::CustomCommand;

struct BuiltinCommand {
    name: &'static str,
    content: &'static str,
}

const COMMANDS: &[BuiltinCommand] = &[
    BuiltinCommand {
        name: "reflect",
        content: include_str!("../../engine/src/prompts/commands/reflect.md"),
    },
    BuiltinCommand {
        name: "simplify",
        content: include_str!("../../engine/src/prompts/commands/simplify.md"),
    },
];

/// List all builtin commands: (name, description) pairs.
pub fn list() -> Vec<(String, String)> {
    COMMANDS
        .iter()
        .map(|cmd| {
            let (overrides, body) = crate::custom_commands::parse_frontmatter(cmd.content);
            let desc = overrides.description.unwrap_or_else(|| {
                body.lines()
                    .find(|l| !l.trim().is_empty())
                    .map(|l| {
                        let s = l.trim();
                        if s.len() > 60 {
                            format!("{}...", &s[..s.floor_char_boundary(57)])
                        } else {
                            s.to_string()
                        }
                    })
                    .unwrap_or_default()
            });
            (cmd.name.to_string(), desc)
        })
        .collect()
}

/// Resolve a builtin command by name, appending any extra arguments.
pub fn resolve(input: &str) -> Option<CustomCommand> {
    let after_slash = input.strip_prefix('/')?;
    let name = after_slash.split_whitespace().next()?;
    let extra = after_slash[name.len()..].trim();
    let cmd = COMMANDS.iter().find(|c| c.name == name)?;
    let (overrides, body) = crate::custom_commands::parse_frontmatter(cmd.content);
    let mut body = body.to_string();
    if !extra.is_empty() {
        body.push_str("\n\n## Additional Focus\n\n");
        body.push_str(extra);
    }
    Some(CustomCommand {
        name: name.to_string(),
        body,
        overrides,
    })
}

/// Check whether `input` matches a builtin command name.
pub fn is_builtin_command(input: &str) -> bool {
    let name = input
        .strip_prefix('/')
        .and_then(|s| s.split_whitespace().next())
        .unwrap_or("");
    COMMANDS.iter().any(|c| c.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_includes_simplify() {
        let items = list();
        assert!(items.iter().any(|(name, _)| name == "simplify"));
    }

    #[test]
    fn resolve_simplify() {
        let cmd = resolve("/simplify").unwrap();
        assert_eq!(cmd.name, "simplify");
        assert!(cmd.body.contains("Code Review and Cleanup"));
    }

    #[test]
    fn resolve_simplify_with_args() {
        let cmd = resolve("/simplify focus on error handling").unwrap();
        assert!(cmd.body.contains("focus on error handling"));
    }

    #[test]
    fn is_builtin() {
        assert!(is_builtin_command("/simplify"));
        assert!(is_builtin_command("/simplify extra args"));
        assert!(!is_builtin_command("/nonexistent"));
    }
}
