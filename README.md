# agent

A Rust TUI coding agent. Connects to any OpenAI-compatible API (Ollama, OpenAI,
etc.) and provides an interactive terminal interface for code generation,
analysis, and assistance.

> [!WARNING]
>
> **This is an early-stage project.** Expect bugs, incomplete features, and
> breaking changes. Make sure to regularly update.

<p align="center">
  <img src="assets/demo.gif" alt="demo" width="800">
</p>

## Quick Start

```bash
cargo install --git https://github.com/leonardcser/agent.git
```

**With Ollama (local):**

```bash
ollama pull qwen3.5:0.8b
agent --model qwen3.5:0.8b --api-base http://localhost:11434/v1
```

**With OpenAI:**

```bash
read -s OPENAI_API_KEY && export OPENAI_API_KEY
agent --model gpt-5.4 --api-base https://api.openai.com/v1 --api-key-env OPENAI_API_KEY
```

## Features

- **Tool use** — file read/write/edit, glob, grep, bash execution, web fetch and
  search
- **Permission system** — granular allow/ask/deny rules per tool, bash pattern,
  and URL domain
- **4 modes** — Normal, Plan, Apply, Yolo with different permission defaults
- **Vim mode** — full vi keybindings for the input editor
- **Session management** — auto-save, resume, and fork conversations
- **Task slug** — short label on the status bar showing what the agent is
  working on, generated from the conversation
- **Input prediction** — ghost text suggesting your next message after a turn
- **Auto-compact** — LLM-powered conversation summarization to reduce token
  usage
- **Reasoning effort** — configurable thinking depth (off/low/medium/high/max)
- **File references** — attach file contents with `@path` syntax
- **Background processes** — async bash execution with completion tracking
- **Custom commands** — user-defined slash commands via markdown files
- **Custom instructions** — project-level `AGENTS.md` files
- **Image support** — paste images from clipboard or reference them in messages
- **Shell escape** — run shell commands directly with `!<command>`

## Installation

```bash
cargo install --git https://github.com/leonardcser/agent.git
# or locally
cargo install --path .
```

## Configuration

Config file: `~/.config/agent/config.yaml` (respects `$XDG_CONFIG_HOME`)

```yaml
# Providers: named connections to LLM APIs
#
# Provider types:
#   openai-compatible — /v1/chat/completions (Ollama, vLLM, SGLang, llama.cpp)
#   openai            — /v1/responses (OpenAI, OpenRouter)
#   anthropic         — /v1/chat/completions with thinking/adaptive (Anthropic)
providers:
  - name: ollama
    type: openai-compatible
    api_base: http://localhost:11434/v1
    models:
      - glm-5 # simple string form
      - name: qwen3.5:27b # object form with sampling overrides
        temperature: 0.8
        top_p: 0.95
        top_k: 40
        min_p: 0.01
        repeat_penalty: 1.0
      - name: llama3:8b # disable tool calling for models that don't support it
        tool_calling: false

  - name: openai
    type: openai
    api_base: https://api.openai.com/v1
    api_key_env: OPENAI_API_KEY
    models:
      - gpt-5.4

  - name: anthropic
    type: anthropic
    api_base: https://api.anthropic.com/v1
    api_key_env: ANTHROPIC_API_KEY
    models:
      - claude-sonnet-4-6

  - name: openrouter
    type: openai
    api_base: https://openrouter.ai/api/v1
    api_key_env: OPENROUTER_API_KEY
    models:
      - anthropic/claude-sonnet-4-6
      - openai/gpt-5.4

# Startup defaults
# These only apply on fresh startup when no explicit model is selected.
# If defaults.model is set, it takes priority and overrides any cached selection.
# If defaults.model is NOT set, the last used model from the previous session is used.
defaults:
  model: ollama/glm-5 # provider_name/model_name
  reasoning_effort: "off" # "off" | "low" | "medium" | "high" | "max"
  reasoning_efforts: ["off", "low", "medium", "high", "max"] # available levels for cycling

# Runtime settings (all toggleable via /settings)
settings:
  vim_mode: false # vi keybindings (default: false)
  auto_compact: false # auto-summarize long conversations (default: false)
  show_speed: true # show tokens/sec (default: true)
  input_prediction: true # ghost text next-message prediction (default: true)
  task_slug: true # short task label on the status bar (default: true)
  restrict_to_workspace: true # downgrade Allow→Ask for out-of-workspace paths (default: true)

# Visual appearance
theme:
  accent:
    "lavender" # preset: lavender|sky|mint|rose|peach|lilac|gold|ember|ice|sage|coral|silver
    # or ANSI value: 0-255

# Permissions: control what the agent can do without asking
# Each mode (normal, apply, yolo) has three categories: tools, bash, web_fetch
# Each category has three rule lists: allow, ask, deny
# Rules use glob patterns — deny always wins
# Unmatched tools/patterns default to Ask in normal/apply, Allow in yolo
permissions:
  normal:
    tools:
      allow: [read_file, glob, grep]
      ask: [edit_file, write_file]
      deny: []
    bash:
      allow: ["ls *", "grep *", "find *", "cat *", "tail *", "head *"]
      ask: []
      deny: []
    web_fetch:
      allow: ["https://docs.rs/*", "https://github.com/*"]
      deny: ["https://evil.com/*"]
  plan:
    tools:
      allow: [read_file, glob, grep]
    bash:
      allow: ["ls *", "grep *", "find *", "cat *", "tail *", "head *"]
  apply:
    tools:
      allow: [read_file, glob, grep, edit_file, write_file]
    bash:
      allow: ["ls *", "grep *", "find *", "cat *", "tail *", "head *"]
  yolo:
    tools:
      deny: []
    bash:
      deny: ["rm -rf /*"]
```

**Default permissions** (when `permissions` is omitted):

| Tool                  | Normal | Plan  | Apply | Yolo  |
| --------------------- | ------ | ----- | ----- | ----- |
| `read_file`           | Allow  | Allow | Allow | Allow |
| `edit_file`           | Ask    | Ask   | Allow | Allow |
| `write_file`          | Ask    | Ask   | Allow | Allow |
| `glob`                | Allow  | Allow | Allow | Allow |
| `grep`                | Allow  | Allow | Allow | Allow |
| `bash`                | Ask    | Ask   | Ask   | Allow |
| `web_fetch`           | Ask    | Ask   | Ask   | Allow |
| `web_search`          | Ask    | Ask   | Ask   | Allow |
| `ask_user_question`   | Allow  | Allow | Allow | Allow |
| `exit_plan_mode`      | Deny   | Ask   | Deny  | Deny  |
| `read_process_output` | Ask    | Ask   | Ask   | Allow |
| `stop_process`        | Ask    | Ask   | Ask   | Allow |

**Default bash patterns** (when `permissions.{mode}.bash` is omitted):

| Pattern  | Normal | Plan  | Apply | Yolo  |
| -------- | ------ | ----- | ----- | ----- |
| `ls *`   | Allow  | Allow | Allow | Allow |
| `grep *` | Allow  | Allow | Allow | Allow |
| `find *` | Allow  | Allow | Allow | Allow |
| `cat *`  | Allow  | Allow | Allow | Allow |
| `tail *` | Allow  | Allow | Allow | Allow |
| `head *` | Allow  | Allow | Allow | Allow |
| _other_  | Ask    | Ask   | Ask   | Allow |

> **Note:** in Normal and Plan modes, allowed commands that contain output
> redirection (`>`, `>>`, `&>`) are automatically escalated to Ask.

### Workspace Permissions

When a tool requests permission, the confirm dialog offers two "always allow"
options:

- **always allow** — session-scoped; lasts until `/clear`, `/new`, or exit
- **always allow (workspace)** — persisted to disk; applies to all future
  sessions started in the same working directory

Workspace permissions are stored in
`~/.local/state/agent/workspaces/<hash>/permissions.json` (keyed by a SHA256
prefix of the CWD).

Use `/permissions` to view and manage both session and workspace permissions.
Navigate with `j`/`k`, delete with `dd` or `Backspace`, close with `Esc`.

## CLI Flags

```
--model <MODEL>                Model to use (overrides config)
--api-base <URL>               API base URL (overrides config)
--api-key-env <VAR>            Env var for API key (overrides config)
--type <TYPE>                  Provider type: openai-compatible, openai, anthropic
                               (auto-detected from --api-base when omitted)
--reasoning-effort <EFFORT>    Starting reasoning effort (off/low/medium/high/max)
--reasoning-efforts <LEVELS>   Available reasoning levels for cycling
                               (comma-separated: off,low,medium,high,max)
--temperature <TEMP>           Sampling temperature
--top-p <VALUE>                Top-p (nucleus) sampling
--top-k <VALUE>                Top-k sampling
--no-tool-calling              Disable tool calling (model becomes chat-only)
--system-prompt <PROMPT>       Override the system prompt
--no-system-prompt             Disable system prompt and AGENTS.md instructions
--log-level <LEVEL>            trace | debug | info | warn | error (default: info)
--bench                        Print performance timing on exit
```

CLI flags take precedence over config file values.

## Modes

Press `Shift+Tab` to cycle through modes:

- **Normal** — default; agent asks before editing files or running commands
- **Plan** — read-only tools only; agent creates a plan file and calls
  `exit_plan_mode` when ready for approval; plan files are stored in
  `~/.local/state/agent/plans/<session_id>/`
- **Apply** — agent edits files and runs pre-approved commands without asking
- **Yolo** — all permissions default to Allow; configurable via
  `permissions.yolo` in config

## Keybindings

### Input

| Key                                           | Action                                    |
| --------------------------------------------- | ----------------------------------------- |
| **General**                                   |                                           |
| `Enter`                                       | Submit message                            |
| `Ctrl+J` / `Shift+Enter`                      | Insert newline                            |
| `Ctrl+R`                                      | Fuzzy search history                      |
| `Ctrl+S`                                      | Stash / unstash current input             |
| `Ctrl+C`                                      | Clear input / cancel agent / quit         |
| `Ctrl+L`                                      | Redraw screen                             |
| `Ctrl+T`                                      | Cycle reasoning effort                    |
| `Shift+Tab`                                   | Cycle mode (normal → plan → apply → yolo) |
| `Esc`                                         | Unqueue messages or dismiss dialog        |
| `Esc Esc`                                     | Cancel agent / compaction / rewind        |
| `↑` / `↓` / `Ctrl+P` / `Ctrl+N`               | Navigate input history                    |
| `Tab`                                         | Accept completion / ghost text            |
| `?`                                           | Open help (when input is empty)           |
| `Cmd+V`                                       | Paste image from clipboard                |
| **Cursor movement**                           |                                           |
| `Ctrl+A` / `Home` / `Cmd+Left`                | Move to beginning of line                 |
| `Ctrl+E` / `End` / `Cmd+Right`                | Move to end of line                       |
| `Ctrl+F` / `Right`                            | Move forward one character                |
| `Ctrl+B` / `Left`                             | Move backward one character               |
| `Alt+F` / `Alt+Right`                         | Move forward one word                     |
| `Alt+B` / `Alt+Left`                          | Move backward one word                    |
| `Cmd+Up`                                      | Move to start of buffer                   |
| `Cmd+Down`                                    | Move to end of buffer                     |
| **Deletion**                                  |                                           |
| `Backspace`                                   | Delete character backward                 |
| `Delete` / `Ctrl+D`                           | Delete character forward                  |
| `Alt+Backspace` / `Ctrl+W` / `Ctrl+Backspace` | Delete word backward                      |
| `Alt+D` / `Alt+Delete`                        | Delete word forward                       |
| `Cmd+Backspace`                               | Delete to start of line                   |
| **Kill & yank**                               |                                           |
| `Ctrl+K`                                      | Kill to end of line                       |
| `Ctrl+U`                                      | Kill to start of line                     |
| `Ctrl+Y`                                      | Yank (paste killed text)                  |
| `Alt+Y`                                       | Yank-pop (cycle kill ring)                |
| **Case & undo**                               |                                           |
| `Alt+U`                                       | Uppercase word                            |
| `Alt+L`                                       | Lowercase word                            |
| `Alt+C`                                       | Capitalize word                           |
| `Ctrl+_`                                      | Undo                                      |
| **Editor**                                    |                                           |
| `Ctrl+X Ctrl+E`                               | Edit in `$EDITOR`                         |

### Vim normal mode overrides

When vim mode is enabled and in normal mode, these keys change behavior:

| Key      | Vim normal mode | Insert / non-vim      |
| -------- | --------------- | --------------------- |
| `Ctrl+U` | Half-page up    | Kill to start of line |
| `Ctrl+D` | Half-page down  | Delete char forward   |
| `Ctrl+J` | History next    | Insert newline        |
| `Ctrl+K` | History prev    | Kill to end of line   |
| `Ctrl+R` | Redo            | History search        |
| `v`      | Edit in $EDITOR | —                     |
| `Ctrl+A` | No-op           | Start of line         |
| `Ctrl+E` | No-op           | End of line           |
| `Ctrl+W` | No-op           | Delete word backward  |
| `Ctrl+Y` | No-op           | Yank (paste)          |

All standard vim motions, operators (`d`, `c`, `y`), text objects (`iw`, `a(`…),
find (`f`, `t`, `F`, `T`, `;`, `,`), and commands (`x`, `s`, `r`, `p`, `u`, `~`,
`J`, etc.) are supported. See `/vim` to toggle.

### Dialogs

All dialogs share these common keys:

| Key                                                       | Action                        |
| --------------------------------------------------------- | ----------------------------- |
| `↑` / `k` / `Ctrl+P`                                      | Previous item                 |
| `↓` / `j` / `Ctrl+N`                                      | Next item                     |
| `Ctrl+U` / `Ctrl+D` / `Alt+↑` / `Alt+↓` / `PgUp` / `PgDn` | Page scroll                   |
| `Enter`                                                   | Confirm                       |
| `Esc` / `Ctrl+C`                                          | Cancel                        |
| `Tab`                                                     | Edit message (confirm dialog) |

Additional dialog-specific keys:

| Dialog      | Key                                         | Action                       |
| ----------- | ------------------------------------------- | ---------------------------- |
| Help        | `q` / `?`                                   | Close                        |
| Confirm     | `Tab`                                       | Enter message editing mode   |
| Question    | `Space`                                     | Toggle option (multi-select) |
| Question    | `1`–`9`                                     | Select option by number      |
| Question    | `←` / `→` / `h` / `l` / `Tab` / `Shift+Tab` | Switch between questions     |
| Permissions | `dd`                                        | Delete selected permission   |
| Permissions | `⌫`                                         | Delete immediately           |
| Permissions | `q`                                         | Close                        |
| Ps          | `⌫`                                         | Kill selected process        |
| Resume      | `dd` / `⌫`                                  | Delete session               |
| Resume      | `Ctrl+W`                                    | Toggle workspace filter      |
| Resume      | `q`                                         | Close                        |
| Resume      | Type to search                              | Fuzzy filter sessions        |
| Rewind      | `↑` / `↓`                                   | Select turn                  |
| Rewind      | `Enter`                                     | Rewind to selected turn      |

### Completer

| Key                       | Action                  |
| ------------------------- | ----------------------- |
| `Tab`                     | Accept completion       |
| `Enter`                   | Accept + submit/replace |
| `↑` / `Ctrl+K` / `Ctrl+P` | Previous result         |
| `↓` / `Ctrl+J` / `Ctrl+N` | Next result             |
| `Ctrl+R`                  | Cycle history matches   |
| `Esc`                     | Cancel                  |

### Menu (settings / model / theme)

| Key                  | Action                           |
| -------------------- | -------------------------------- |
| `↑` / `k` / `Ctrl+P` | Previous item                    |
| `↓` / `j` / `Ctrl+N` | Next item                        |
| `Enter`              | Select / toggle                  |
| `Space`              | Toggle (settings)                |
| `Tab`                | Cycle auxiliary (e.g. reasoning) |
| `Esc` / `q`          | Dismiss                          |

## Slash Commands

Type `/` to open the command picker:

| Command                    | Description                            |
| -------------------------- | -------------------------------------- |
| `/btw <question>`          | Ask a quick side question              |
| `/clear`, `/new`           | Start a new conversation               |
| `/resume`                  | Resume a saved session                 |
| `/model`                   | Switch model                           |
| `/compact [focus]`         | Compact conversation history           |
| `/vim`                     | Toggle vim mode                        |
| `/theme [name]`            | Change accent color                    |
| `/color [name]`            | Set task slug color (session)          |
| `/settings`                | Open settings menu                     |
| `/export`                  | Copy conversation to clipboard         |
| `/fork`, `/branch`         | Fork current session                   |
| `/stats`                   | Show token usage statistics            |
| `/permissions`             | Manage session & workspace permissions |
| `/ps`                      | Manage background processes            |
| `/exit`, `/quit`           | Exit                                   |
| `:q`, `:qa`, `:wq`, `:wqa` | Exit (vim-style)                       |

Prefix with `!` to run a shell command directly (e.g. `!git status`).

## Compaction (`/compact`)

Use `/compact` to summarize older conversation history, freeing up context
window space. The summary appears as a dim divider in the conversation and
replaces the older messages sent to the API.

Optionally provide a focus to guide what the summary preserves:

```
/compact keep details about the auth refactor
```

When `auto_compact` is enabled in settings, compaction triggers automatically
when context usage exceeds 80%. Press `Esc Esc` to cancel an in-flight
compaction.

## Side Questions (`/btw`)

Use `/btw <question>` to ask quick side questions without interrupting your main
conversation. The response appears in a dialog above the prompt and can be
dismissed with `Esc` or scrolled with `↑`/`↓`. The side question and answer are
not added to the main conversation history.

**Example:**

```
/btw what does the glob crate do?
```

## Message Queuing

When the agent is busy responding, you can continue typing messages. They will
be queued and processed sequentially. Press `Esc` once to unqueue and edit your
pending messages, or twice to cancel the agent and unqueue everything.

## File References

Type `@` followed by a path to attach file contents to your message. A fuzzy
file picker opens automatically.

```
explain @src/main.rs
```

## Custom Commands

Create markdown files in `~/.config/agent/commands/` to define reusable slash
commands. Each `.md` file becomes a `/command` you can invoke from the TUI.

**Example** — `~/.config/agent/commands/commit.md`:

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

Type `/commit` and the agent receives the evaluated prompt with command outputs
inlined, while the chat only shows `/commit`.

**Frontmatter options** (all optional):

| Key                | Description                                       |
| ------------------ | ------------------------------------------------- |
| `description`      | Short description shown in the `/` command picker |
| `model`            | Override model for this command                   |
| `provider`         | Resolve `api_base`/`api_key` from this provider   |
| `temperature`      | Sampling temperature                              |
| `top_p`            | Top-p (nucleus) sampling                          |
| `top_k`            | Top-k sampling                                    |
| `min_p`            | Min-p sampling                                    |
| `repeat_penalty`   | Repetition penalty                                |
| `reasoning_effort` | Thinking depth: off/low/medium/high               |
| `tools`            | `allow`/`ask`/`deny` lists for tool permissions   |
| `bash`             | `allow`/`ask`/`deny` glob patterns for bash       |
| `web_fetch`        | `allow`/`ask`/`deny` glob patterns for URLs       |

**Shell execution**: use `` !`command` `` inline or ` ```! ` fenced code blocks
to execute shell commands before sending. The output replaces the command in the
template. Escape with `\` to prevent execution.

## Custom Instructions

Place an `AGENTS.md` file in your project root or in `~/.config/agent/` for
global instructions. The agent includes these in its system prompt
automatically.

## Sessions

Sessions are saved automatically to `~/.local/state/agent/sessions/` (respects
`$XDG_STATE_HOME`) and restored on SIGINT/SIGTERM. Use `/resume` to load a
previous session and `/fork` to branch from the current one.

## Development

```bash
cargo build       # compile
cargo run         # run
cargo test        # run tests
cargo fmt         # format
cargo clippy      # lint
```

## Acknowledgments

This project was inspired by
[Claude Code](https://github.com/anthropics/claude-code).

## Contributing

Contributions are welcome! Please open an issue or a pull request.

Feel free to open issues for bugs :)

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file
for details.
