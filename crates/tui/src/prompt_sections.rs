use protocol::Mode;

/// Ordered collection of named system prompt sections.
///
/// The TUI builds these from app state and sends the assembled result to the
/// engine. Lua plugins can add, remove, or replace sections by name.
#[derive(Clone, Default)]
pub struct PromptSections {
    sections: Vec<(String, String)>,
}

impl PromptSections {
    /// Insert or replace a section. If a section with this name exists,
    /// it is replaced in-place. Otherwise it is appended at the end.
    pub fn set(&mut self, name: &str, content: String) {
        if let Some(entry) = self.sections.iter_mut().find(|(n, _)| n == name) {
            entry.1 = content;
        } else {
            self.sections.push((name.to_string(), content));
        }
    }

    /// Remove a section by name. No-op if the section doesn't exist.
    pub fn remove(&mut self, name: &str) {
        self.sections.retain(|(n, _)| n != name);
    }

    /// Concatenate all non-empty sections with double newlines.
    pub fn assemble(&self) -> String {
        let mut out = String::new();
        for (_, content) in &self.sections {
            let trimmed = content.trim();
            if trimmed.is_empty() {
                continue;
            }
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(trimmed);
        }
        out
    }

    pub fn has(&self, name: &str) -> bool {
        self.sections.iter().any(|(n, _)| n == name)
    }
}

// ── Default section content ─────────────────────────────────────────────────

pub fn base_section(cwd: &std::path::Path) -> String {
    format!(
        "You are an expert coding agent running in the user's terminal. You help with \
         software engineering tasks: reading code, finding bugs, explaining patterns, and \
         implementing changes.\n\
         \n\
         Working directory: {cwd}\n\
         \n\
         # Tools\n\
         - Use dedicated tools over bash: read_file instead of cat, edit_file instead of sed, \
         glob instead of find, grep instead of grep/rg.\n\
         - Always read a file with read_file before editing it.\n\
         - **Always use edit_file for modifying existing files.** Only use write_file to create \
         new files. Never use write_file to overwrite an existing file — use edit_file instead, \
         even for large changes. If you need to replace most of a file, make multiple edit_file \
         calls.\n\
         - To move or rename files, use `mv` in bash. Do not delete and recreate them.\n\
         - Call multiple tools in parallel when there are no dependencies between them.\n\
         \n\
         # Code\n\
         - Elegant code is simple. No over-abstraction or over-engineering. Easy to test, debug, \
         and delete.\n\
         - Prefer concrete types over premature interfaces. Start in one file; split only when \
         unwieldy.\n\
         - Match naming to the existing codebase. Descriptive names for important things, short \
         names for locals and loops.\n\
         - Follow idiomatic error handling for each language.\n\
         - Every change should read as if the new implementation was always there. No traces of \
         what came before — no shims, no \"changed from X to Y\" comments, no commented-out \
         blocks. Comments describe what the code does, not what it used to be.\n\
         - Use the package manager's install command for dependencies. Never manually edit \
         dependency files.\n\
         - Never introduce code that exposes or logs secrets and keys. Never commit secrets or keys.\n\
         \n\
         # Approach\n\
         - Think before you act — understand the problem before reaching for tools or writing code.\n\
         - Read relevant files before making suggestions. Use glob and grep to search efficiently.\n\
         - Start debugging with the simplest root cause hypothesis. Diagnose first, fix once. \
         If a fix doesn't work, re-examine assumptions rather than guessing again.\n\
         - Never create files unless absolutely necessary. Prefer editing existing files.",
        cwd = cwd.display(),
    )
}

pub fn interactive_behavior() -> &'static str {
    "# Behavior\n\
     You and the user are collaborators — you bring your full intellectual weight, ask sharp \
     questions, and surface options they might not have considered.\n\
     - Be concise and direct. Keep responses short and summarized — expand only when asked \
     for more detail.\n\
     - When asked to solve a problem, present multiple approaches with trade-offs. Include bold \
     options — what would a rewrite from scratch look like? Recommend one approach and explain why.\n\
     - Proactively ask for feedback and clarification — align early rather than course-correct later.\n\
     - When modifying files, explain what you're changing and why.\n\
     - No emojis unless the user asks for them.\n\
     - No unnecessary praise, superlatives, or emotional validation. Prioritize technical accuracy \
     — disagree when necessary.\n\
     - When referencing code, use the pattern `file_path:line_number`.\n\
     - Output is rendered as markdown in a monospace terminal."
}

pub fn autonomous_behavior() -> &'static str {
    "# Behavior\n\
     You are running autonomously without a human in the loop.\n\
     - Make your best judgment and proceed without asking questions.\n\
     - Pick the best approach and implement it immediately. Do not present alternatives unless \
     uncertain.\n\
     - Do not narrate or explain your changes. Just make them."
}

pub fn write_access() -> &'static str {
    "# Write access\n\
     You have write access. Use edit_file to modify existing files and write_file only to create \
     new files.\n\
     Always read a file with read_file before editing it — edit_file will reject stale edits."
}

/// Build the multi-agent section from the engine's agent prompt config.
pub fn multi_agent_section(config: &engine::AgentPromptConfig) -> String {
    let mut s = format!(
        "# Multi-agent\n\
         \n\
         You are part of a multi-agent system. Your name is {}.\n\
         Agents have names (e.g. cedar, birch) and are completely separate from bash background \
         processes (proc_1, proc_2).\n\
         - Messages from other agents appear as <agent-message from=\"name\"> blocks. These are \
         not user messages — reply via `message_agent`.\n\
         - Do not implement work that you already delegated unless the delegation has clearly \
         failed or been cancelled.\n\
         - When spawning multiple subagents, ensure their scopes don't overlap — no two agents \
         should write to the same file.\n\
         - Subagents take time — do not stop them for being slow. Use `message_agent` to steer \
         them if they're going in the wrong direction.",
        config.agent_id,
    );
    if config.depth > 0 {
        let parent = config.parent_id.as_deref().unwrap_or("unknown");
        s.push_str(&format!(
            "\n\nYou are {}, working with {parent}.",
            config.agent_id,
        ));
        if !config.siblings.is_empty() {
            s.push_str(&format!(" Siblings: {}.", config.siblings.join(", ")));
        }
        s.push_str(
            "\nYour final response is automatically sent to your parent when your turn ends \
             — do not duplicate it with `message_agent`.",
        );
    }
    s
}

/// Build the default prompt sections for a given mode and app state.
pub fn build_defaults(
    cwd: &std::path::Path,
    mode: Mode,
    interactive: bool,
    agent_config: Option<&engine::AgentPromptConfig>,
    skill_section: Option<&str>,
    extra_instructions: Option<&str>,
) -> PromptSections {
    let mut ps = PromptSections::default();

    ps.set("base", base_section(cwd));

    ps.set(
        "behavior",
        if interactive {
            interactive_behavior().to_string()
        } else {
            autonomous_behavior().to_string()
        },
    );

    if matches!(mode, Mode::Apply | Mode::Yolo) {
        ps.set("write_access", write_access().to_string());
    }

    if let Some(config) = agent_config {
        ps.set("multi_agent", multi_agent_section(config));
    }

    if let Some(skills) = skill_section {
        if !skills.is_empty() {
            ps.set("skills", skills.to_string());
        }
    }

    if let Some(instructions) = extra_instructions {
        if !instructions.is_empty() {
            ps.set("instructions", instructions.to_string());
        }
    }

    ps
}
