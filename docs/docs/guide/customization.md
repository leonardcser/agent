# Customization

## Config File

The config lives at `~/.config/smelt/config.yaml` (respects `$XDG_CONFIG_HOME`).
Load a different file with `--config <path>`.

The [Getting Started](getting-started.md) guide covers basic provider setup. See
the [Configuration Reference](../reference/configuration.md) for every option.

## Runtime Settings

Toggle settings at runtime with `/settings`, set defaults in config under
`settings:`, or override from the CLI with `--set key=value`. See the
[Configuration Reference](../reference/configuration.md#settings) for all
available settings.

## Themes

Thirteen accent color presets:

> `ember` · `coral` · `rose` · `gold` · `ice` · `sky` · `blue` · `lavender` ·
> `lilac` · `mint` · `sage` · `silver`

Set in config:

```yaml
theme:
  accent: mint
```

Or change at runtime with `/theme`. You can also use a raw ANSI color value
(0–255).

The task slug color is separate — change it per-session with `/color`.

## Custom Commands

Create `.md` files in `~/.config/smelt/commands/` and they become slash
commands. For example, `~/.config/smelt/commands/commit.md`:

```markdown
---
description: commit staged changes
model: gpt-4o
temperature: 0.2
reasoning_effort: low
bash:
  allow: ["git *"]
---

Create a conventional commit for the staged changes.

Staged diff:

!`git diff --cached`

Recent commits for style reference:

!`git log --oneline -5`
```

Type `/commit` and the agent receives the evaluated prompt with shell outputs
inlined. Pass arguments too: `/commit fix typos` appends to the prompt body.

See [Custom Commands](../reference/commands.md#custom-commands) in the Slash
Commands Reference for all frontmatter fields and template syntax.

## Skills

Skills are on-demand knowledge packs the agent can load during a conversation.
Place a `SKILL.md` file in `~/.config/smelt/skills/<name>/` (global) or
`.smelt/skills/<name>/` (project-local). See the
[Configuration Reference](../reference/configuration.md#skills) for the full
format.

## External Tools (MCP)

Connect external tool servers via the
[Model Context Protocol](https://modelcontextprotocol.io). Servers run as child
processes and their tools become available to the agent. See the
[Configuration Reference](../reference/configuration.md#mcp-model-context-protocol)
for setup.

## Custom Instructions (AGENTS.md)

Place an `AGENTS.md` file in your project root (or `~/.config/smelt/AGENTS.md`
for global instructions). Its contents are automatically appended to the system
prompt for every conversation in that directory.

Use it for project conventions, coding standards, or any persistent context the
agent should know about.

Disable with `--no-system-prompt`.
