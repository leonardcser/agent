# Configuration Reference

Config file: `~/.config/smelt/config.yaml` (respects `$XDG_CONFIG_HOME`).

If no config file exists, an interactive setup wizard runs on first launch. If
the file exists but fails to parse, a warning is printed and defaults are used.

## Providers

Each entry under `providers` defines a connection to an LLM API.

| Field         | Description                                            |
| ------------- | ------------------------------------------------------ |
| `name`        | Unique identifier (used in `defaults.model` as prefix) |
| `type`        | `openai`, `codex`, `anthropic`, or `openai-compatible` |
| `api_base`    | API endpoint URL                                       |
| `api_key_env` | Environment variable holding the API key               |
| `models`      | List of available models                               |

### Provider Types

| Type                | Endpoint                                | Compatible Services                            |
| ------------------- | --------------------------------------- | ---------------------------------------------- |
| `openai`            | `/v1/responses`                         | OpenAI, OpenRouter                             |
| `codex`             | `chatgpt.com/backend-api/codex` (OAuth) | OpenAI Codex (ChatGPT subscription)            |
| `anthropic`         | `/v1/messages` + thinking               | Anthropic                                      |
| `openai-compatible` | `/v1/chat/completions`                  | Ollama, vLLM, SGLang, llama.cpp, Google Gemini |

### Model Configuration

Models can be strings or objects with per-model overrides:

```yaml
models:
  - gpt-5.4 # simple form
  - name: qwen3.5:27b # object form
    temperature: 0.8
    top_p: 0.95
    top_k: 40 # openai-compatible & anthropic only
    min_p: 0.01 # openai-compatible only
    repeat_penalty: 1.0 # openai-compatible only
    tool_calling: false # disable tools for this model
  - name: custom-model
    input_cost: 2.0 # $/1M input tokens
    output_cost: 8.0 # $/1M output tokens
    cache_read_cost: 0.5 # $/1M cache-read tokens
    cache_write_cost: 0.0 # $/1M cache-write tokens
```

#### Pricing

Cost tracking is built in for popular models (GPT, Claude, DeepSeek). Codex
models are zero-cost (included with your ChatGPT subscription). The session cost
is shown in the status bar and total cost appears in `/stats`.

For models not in the built-in table, or to override built-in prices, set cost
fields on the model config. All values are USD per 1 million tokens. Unknown
models default to zero cost.

## Defaults

```yaml
defaults:
  model: ollama/glm-5 # provider_name/model_name
  mode: normal # starting mode
  mode_cycle: [normal, plan, apply, yolo] # Shift+Tab cycle
  reasoning_effort: "off" # starting level
  reasoning_cycle: ["off", "low", "medium", "high", "max"] # Ctrl+T cycle
```

Reasoning effort controls how deeply the model thinks before responding.
Supported by Anthropic (`thinking`), OpenAI (`reasoning`), and openai-compatible
providers that support `reasoning_effort`. For OpenAI, `max` maps to `xhigh`.
Models that don't support thinking ignore this setting.

Model selection follows this precedence:

1. `--model` CLI flag
2. `defaults.model` in config
3. Last used model (cached from previous session)
4. First model in the providers list

If `defaults.model` is set, the cached selection is ignored.

## Settings

All toggleable at runtime via `/settings`.

| Key                     | Default | Description                                                          |
| ----------------------- | ------- | -------------------------------------------------------------------- |
| `vim_mode`              | `false` | Vi keybindings                                                       |
| `auto_compact`          | `false` | Auto-summarize at 80% context usage (always on in headless/subagent) |
| `show_tps`              | `true`  | Tokens/sec in status bar                                             |
| `show_tokens`           | `true`  | Context token count in status bar                                    |
| `show_cost`             | `true`  | Session cost in status bar                                           |
| `input_prediction`      | `true`  | Ghost text suggestions                                               |
| `task_slug`             | `true`  | Task label in status bar                                             |
| `show_thinking`         | `true`  | Show full thinking/reasoning blocks (false shows a single summary)   |
| `restrict_to_workspace` | `true`  | Downgrade Allow → Ask outside workspace                              |
| `multi_agent`           | `false` | Enable multi-agent mode                                              |
| `context_window`        | auto    | Override context window size (tokens); auto-detected from API        |

## Theme

```yaml
theme:
  accent: ember
```

Presets: `ember`, `coral`, `rose`, `gold`, `ice`, `sky`, `blue`, `lavender`,
`lilac`, `mint`, `sage`, `silver`. Or a raw ANSI value (0–255).

## MCP (Model Context Protocol)

Connect external tool servers that expose tools via the MCP protocol. Each
server runs as a child process communicating over stdio.

```yaml
mcp:
  filesystem:
    type: local
    command: ["npx", "-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
    env:
      DEBUG: "true"
    timeout: 30000 # ms, default 30000
    enabled: true # default true
```

| Field     | Description                                      |
| --------- | ------------------------------------------------ |
| `type`    | `local` (stdio child process)                    |
| `command` | Command and arguments to spawn the MCP server    |
| `env`     | Environment variables for the server process     |
| `timeout` | Connection and tool call timeout in milliseconds |
| `enabled` | Set to `false` to skip connecting on startup     |

MCP tools appear in the agent's tool list with names prefixed by the server name
(e.g., `filesystem_read_file`). They default to "ask" permission.

### MCP Permissions

MCP tools use a separate `mcp` ruleset in the permissions config. Patterns are
matched against the qualified tool name (`servername_toolname`):

```yaml
permissions:
  normal:
    mcp:
      allow: ["filesystem_*"] # allow all filesystem server tools
      deny: ["dangerous_server_*"] # block an entire server
  yolo:
    mcp:
      allow: ["*"] # allow all MCP tools in yolo mode
```

## Skills

Skills are on-demand knowledge packs the agent can load via the `load_skill`
tool.

```yaml
skills:
  paths:
    - ~/my-skills
    - ./project-skills
```

### Discovery Locations

Skills are scanned from these directories (later entries override):

1. `~/.config/smelt/skills/*/SKILL.md` — global user skills
2. `.smelt/skills/*/SKILL.md` — project-local skills
3. Paths from `skills.paths` in config

### Skill Format

Each skill is a directory containing a `SKILL.md` file:

```
skills/
  frontend-design/
    SKILL.md
    reference/
      examples.html
```

`SKILL.md` uses YAML frontmatter:

```markdown
---
name: frontend-design
description: Create production-grade frontend interfaces
---

## Instructions

Detailed instructions for the agent...
```

## Permissions

See [Permissions Reference](permissions.md) for full details.

## Storage Paths

All runtime data is stored under the XDG base directories:

| Directory                           | Contents                                            |
| ----------------------------------- | --------------------------------------------------- |
| `$XDG_CONFIG_HOME/smelt/`           | `config.yaml`, custom commands, global skills       |
| `$XDG_STATE_HOME/smelt/sessions/`   | Saved sessions (`session.json`, `meta.json`, blobs) |
| `$XDG_STATE_HOME/smelt/state.json`  | Persisted state (last model, mode, accent color)    |
| `$XDG_STATE_HOME/smelt/registry/`   | Multi-agent registry entries                        |
| `$XDG_STATE_HOME/smelt/workspaces/` | Per-workspace saved permissions                     |
| `$XDG_STATE_HOME/smelt/logs/`       | Log files (rotated, max 20)                         |
| `$XDG_CACHE_HOME/smelt/`            | Cache                                               |

Codex OAuth tokens are stored in the system keyring (service:
`smelt-codex-auth`). If the keyring is unavailable, tokens fall back to
`$XDG_STATE_HOME/smelt/codex_auth.json` (mode `0600`).

## Environment Variables

| Variable          | Purpose                                                 |
| ----------------- | ------------------------------------------------------- |
| `XDG_CONFIG_HOME` | Config directory (default: `~/.config`)                 |
| `XDG_STATE_HOME`  | State directory (default: `~/.local/state`)             |
| `XDG_CACHE_HOME`  | Cache directory (default: `~/.cache`)                   |
| `COLORFGBG`       | Terminal color hint (fallback for dark/light detection) |
| `TERM`            | Terminal type (`dumb` skips color detection)            |
| `EDITOR`          | Editor for `Ctrl+X Ctrl+E` and vim `v`                  |
| `NO_COLOR`        | Disable ANSI colors (respected in headless mode)        |

## Full Example

```yaml
providers:
  - name: ollama
    type: openai-compatible
    api_base: http://localhost:11434/v1
    models:
      - glm-5
      - name: qwen3.5:27b
        temperature: 0.8
        top_p: 0.95
        top_k: 40
        min_p: 0.01
        repeat_penalty: 1.0
      - name: llama3:8b
        tool_calling: false

  - name: openai
    type: openai
    api_base: https://api.openai.com/v1
    api_key_env: OPENAI_API_KEY
    models:
      - gpt-5.4

  - name: codex
    type: codex # models fetched automatically from the API
    api_base: https://chatgpt.com/backend-api/codex

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

defaults:
  model: ollama/glm-5
  mode: normal
  mode_cycle: [normal, plan, apply, yolo]
  reasoning_effort: "off"
  reasoning_cycle: ["off", "low", "medium", "high", "max"]

settings:
  vim_mode: false
  auto_compact: false
  show_tps: true
  show_tokens: true
  show_cost: true
  input_prediction: true
  task_slug: true
  show_thinking: true
  restrict_to_workspace: true
  multi_agent: false

theme:
  accent: lavender

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
    mcp:
      allow: ["*"]

mcp:
  filesystem:
    type: local
    command: ["npx", "-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
    timeout: 30000

skills:
  paths:
    - ~/my-skills
```
