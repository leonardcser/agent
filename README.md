# agent

A Rust TUI coding agent. Connects to any OpenAI-compatible API (Ollama, OpenAI,
etc.) and provides an interactive terminal interface for code generation,
analysis, and assistance.

## Installation

```bash
cargo install --path .
```

## Configuration

Config file: `~/.config/agent/config.yaml` (respects `$XDG_CONFIG_HOME`)

```yaml
providers:
  openai:
    api_base: https://api.openai.com/v1
    model: gpt-4o
    api_key_env: OPENAI_API_KEY # optional

default_provider: openai

settings:
  vim_mode: false # default
  auto_compact: false # default

# Permissions: control what tools and bash commands the agent can run without asking
permissions:
  normal:
    tools:
      allow: [read_file, glob, grep]
      ask: [edit_file, write_file]
      deny: []
    bash:
      allow: ["ls *", "grep *", "find *"]
      ask: []
      deny: []
  apply:
    tools:
      allow: [read_file, glob, grep, edit_file, write_file]
    bash:
      allow: ["ls *", "grep *", "find *"]
```

All fields are optional except `model` within the active provider, which must be
set via config or `--model`. If `default_provider` is omitted the first entry in
`providers` is used.

**Default tool permissions** (when `permissions` is omitted):

| Tool                | Normal mode | Apply mode |
| ------------------- | ----------- | ---------- |
| `read_file`         | Allow       | Allow      |
| `edit_file`         | Ask         | Allow      |
| `write_file`        | Ask         | Allow      |
| `glob`              | Allow       | Allow      |
| `grep`              | Allow       | Allow      |
| `ask_user_question` | Allow       | Allow      |
| `bash`              | Ask         | Ask        |

Bash commands not matching any rule default to **Ask**. Deny rules always win.

## CLI Flags

```
--provider <NAME>       Provider to use (overrides default_provider)
--model <MODEL>         Model to use (overrides provider config)
--api-base <URL>        API base URL (overrides provider config)
--api-key-env <VAR>     Env var to read the API key from (overrides provider config)
--log-level <LEVEL>     Log level: trace, debug, info, warn, error (default: info)
--bench                 Print performance timing summary on exit
```

CLI flags take precedence over config file values.

## Modes

Press `Shift+Tab` to cycle through modes:

- **Normal** — default; agent asks before editing files or running commands
- **Plan** — read-only tools only; agent thinks and plans without making changes
- **Apply** — agent edits files and runs pre-approved commands without asking
- **Yolo** — all permissions bypassed; agent runs anything without asking

## Keybindings

| Key         | Action                             |
| ----------- | ---------------------------------- |
| `Enter`     | Submit message                     |
| `Ctrl+J`    | Insert newline                     |
| `Ctrl+A`    | Move to beginning of line          |
| `Ctrl+E`    | Move to end of line                |
| `Ctrl+R`    | Fuzzy search history               |
| `Ctrl+S`    | Stash/unstash current input        |
| `Shift+Tab` | Cycle mode (normal → plan → apply) |
| `Esc Esc`   | Cancel running agent               |
| `↑ / ↓`     | Navigate input history             |
| `Tab`       | Accept completion                  |

## Slash Commands

Type `/` to open the command picker:

| Command            | Description                    |
| ------------------ | ------------------------------ |
| `/clear`, `/new`   | Start a new conversation       |
| `/resume`          | Resume a saved session         |
| `/compact`         | Compact conversation history   |
| `/vim`             | Toggle vim mode                |
| `/settings`        | Open settings menu             |
| `/export`          | Copy conversation to clipboard |
| `/exit` or `/quit` | Exit                           |

## File References

Type `@` followed by a path to attach file contents to your message. A fuzzy
file picker opens automatically. The file is appended to your message when
submitted.

```
explain @src/main.rs
```

## Sessions

Sessions are saved automatically to `~/.local/state/agent/sessions/` (respects
`$XDG_STATE_HOME`) and restored on SIGINT/SIGTERM. Use `/resume` to load a
previous session.

## Development

```bash
cargo build       # compile
cargo run         # run
cargo test        # run tests
cargo fmt         # format
cargo clippy      # lint
```
