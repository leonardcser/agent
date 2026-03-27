# Customization

## Config File

The config lives at `~/.config/agent/config.yaml` (respects `$XDG_CONFIG_HOME`).
Load a different file with `--config <path>`.

The [Getting Started](getting-started.md) guide covers basic provider setup.
See the [Configuration Reference](../reference/configuration.md) for every
option.

## Runtime Settings

Press `/settings` to toggle these at runtime:

| Setting | Default | Description |
| --- | --- | --- |
| `vim_mode` | off | Vi keybindings in the input editor |
| `auto_compact` | off | Auto-summarize at 80% context usage |
| `show_speed` | on | Tokens/sec in the status bar |
| `input_prediction` | on | Ghost text next-message suggestions |
| `task_slug` | on | Short task label in the status bar |
| `restrict_to_workspace` | on | Downgrade Allow → Ask for out-of-workspace paths |
| `multi_agent` | off | Multi-agent mode |
| `context_window` | auto | Override context window size (tokens) |

Set defaults in config:

```yaml
settings:
  vim_mode: true
  auto_compact: true
```

Or override from the CLI:

```bash
agent --set vim_mode=true --set auto_compact=true
```

## Themes

Twelve accent color presets:

> `lavender` · `sky` · `mint` · `rose` · `peach` · `lilac` · `gold` · `ember`
> · `ice` · `sage` · `coral` · `silver`

Set in config:

```yaml
theme:
  accent: mint
```

Or change at runtime with `/theme`. You can also use a raw ANSI color value
(0–255).

The task slug color is separate — change it per-session with `/color`.

### Dark/Light Detection

The TUI auto-detects your terminal's background color:

1. **OSC 11 query** — reads the terminal's reported background color
2. **`$COLORFGBG` fallback** — parses the environment variable
3. **Default** — assumes dark background

## Custom Commands

Create `.md` files in `~/.config/agent/commands/` and they become slash
commands. For example, `~/.config/agent/commands/commit.md`:

````markdown
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
````

Type `/commit` and the agent receives the evaluated prompt with shell outputs
inlined. Pass arguments too: `/commit fix typos` appends to the prompt body.

### Frontmatter

All fields are optional:

| Key | Description |
| --- | --- |
| `description` | Shown in the `/` picker |
| `model` | Override model for this command |
| `provider` | Resolve API connection from this provider |
| `temperature` | Sampling temperature |
| `top_p` | Top-p (nucleus) sampling |
| `top_k` | Top-k sampling |
| `min_p` | Min-p sampling |
| `repeat_penalty` | Repetition penalty |
| `reasoning_effort` | Thinking depth: `off`/`low`/`medium`/`high`/`max` |
| `tools` | `allow`/`ask`/`deny` lists for tool permissions |
| `bash` | `allow`/`ask`/`deny` glob patterns for bash |
| `web_fetch` | `allow`/`ask`/`deny` glob patterns for URLs |

### Shell Execution in Templates

- **Inline**: `` !`command` `` — output replaces the backtick expression
- **Fenced**: ` ```! ` code block — output replaces the block
- **Escape**: `` \!`command` `` — prevents execution

## Custom Instructions (AGENTS.md)

Place an `AGENTS.md` file in your project root (or `~/.config/agent/AGENTS.md`
for global instructions). Its contents are automatically appended to the system
prompt for every conversation in that directory.

Use it for project conventions, coding standards, or any persistent context
the agent should know about.

Disable with `--no-system-prompt`.
