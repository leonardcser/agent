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
  - name: ollama
    type: openai-compatible
    api_base: http://localhost:11434/v1
    models:
      - glm-5
      - name: llama3.3:70b
        temperature: 0.8 # optional per-model overrides
        top_p: 0.95
        top_k: 40
        min_p: 0.01
        repeat_penalty: 1.0

  - name: anthropic
    type: openai-compatible
    api_base: https://api.anthropic.com/v1
    api_key_env: ANTHROPIC_API_KEY
    models:
      - claude-sonnet-4-20250514

default_model: ollama/glm-5 # provider_name/model_name

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
    web_fetch:
      allow: ["https://docs.rs/*", "https://github.com/*"]
      deny: ["https://evil.com/*"]
  apply:
    tools:
      allow: [read_file, glob, grep, edit_file, write_file]
    bash:
      allow: ["ls *", "grep *", "find *"]
```

Providers is a list of named connections. Each provider has a `type` (currently
only `openai-compatible`), connection info (`api_base`, `api_key_env`), and a
`models` list. Models can be plain strings or dicts with `name` and optional
sampling parameters (`temperature`, `top_p`, `top_k`, `min_p`,
`repeat_penalty`).

The `default_model` field selects which model to use at startup. It can be a
`provider_name/model_name` key or just a model name. If omitted, the first model
in the first provider is used. Use `/model` to switch models at runtime.

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
| `web_fetch`         | Ask         | Ask        |
| `web_search`        | Ask         | Ask        |

Bash commands and web fetch URLs not matching any rule default to **Ask**. Deny
rules always win.

**Domain permissions for `web_fetch`:** URL patterns use glob syntax to
allow/deny fetching specific domains. When the agent asks to fetch a URL, the
confirmation dialog offers three approval levels:

1. **yes** — approve this single request
2. **no** — deny this request
3. **allow \<domain\>** — approve all future fetches to this domain for the session

## CLI Flags

```
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
| `/model`           | Switch model                   |
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
