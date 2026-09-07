# Configuration Reference

Config file: `~/.config/smelt/init.lua` on Linux and macOS, or
`%APPDATA%\smelt\init.lua` on Windows. `$XDG_CONFIG_HOME` overrides the config
root on every platform.

Load a different file with `--config <path>`.

If no config file exists, an interactive setup wizard runs on first launch and
writes a starter `init.lua`.

## init.lua

`init.lua` is evaluated at startup before the engine starts. It can register
providers, MCP servers, settings, and permission rules by calling APIs on the
`smelt` table. Anything else you put in the file (custom commands, keymaps,
autocmds) behaves like a plugin and is also loaded at startup.

### Per-project config

After bundled autoload modules, the user `init.lua`, and global user plugins run,
smelt looks for `.smelt/init.lua` and `.smelt/plugins/*.lua` under the current
working directory and sources them.
Project-local config is especially useful on teams: after reviewing and trusting
it, a fresh clone carries the project's conventions and tooling. Run `/trust` to
record the current SHA-256 hash of `.smelt/`; any edit invalidates that hash and
requires review and trust again.

| Lua API              | Description                                          |
| -------------------- | ---------------------------------------------------- |
| `smelt.trust.mark`   | Trust the current cwd's `.smelt/` content            |
| `smelt.trust.status` | Return `"trusted"`, `"untrusted"`, or `"no_content"` |

### Reload behavior

smelt-managed configuration and registrations reload transactionally. smelt loads
and validates a fresh candidate before replacing the running generation. If any
early, autoload, user, project, or plugin file raises or contains invalid syntax,
the current commands, keymaps, tools, hooks, providers, settings, permissions,
MCP/LSP declarations, and watcher roots remain active. Candidate `on_ready` hooks
do not run.

This guarantee covers declarations and resources owned by smelt's generation
machinery. Lua configuration is trusted in-process code, not a sandbox: arbitrary
filesystem, process, network, or other external effects are not rolled back when
a candidate fails. APIs that directly affect the live application are unavailable
during candidate evaluation.

A successful `/reload` reconciles providers, models, settings, permissions, modes,
MCP, LSP, and watched config roots as one committed generation. Background
controller work is revisioned, so an older connection or metadata result cannot
replace newer config. Manual `/reload` also refreshes prompt inputs such as
`AGENTS.md`, skills, and `--system-prompt`; automatic Lua config reloads do not.

Settings and background model metadata affect requests created after the commit.
An active turn keeps its original target and static permission policy. Explicit
model, mode, or reasoning changes made by the user take effect at the next
provider-request boundary.

For diagnostics, `smelt.config.runtime_status()` returns sanitized generation and
runtime revisions, pending/failure reload state, model availability, managed
provider freshness, and desired/observed revisions for background controllers.
It never includes credential values or Lua source contents.

## Providers

Register a provider with `smelt.provider.register`:

```lua
smelt.provider.register("ollama", {
  type = "openai-compatible",
  api_base = "http://localhost:11434/v1",
  models = { "qwen3.6:27b" },
})

smelt.provider.register("openai", {
  type = "openai",
  api_base = "https://api.openai.com/v1",
  api_key_env = "OPENAI_API_KEY",
  models = { "gpt-5.5" },
})
```

| Field         | Description                                                                                                   |
| ------------- | ------------------------------------------------------------------------------------------------------------- |
| `type`        | `openai-compatible` (default), `openai`, `codex`, `anthropic-compatible`, `anthropic`, `copilot`, `kimi-code` |
| `api_base`    | API base URL, without `/chat/completions`, `/responses`, or `/messages`                                 |
| `api_key_env` | Environment variable holding the API key (omit for OAuth-backed `codex`, `copilot`, and `kimi-code`)          |
| `models`      | Array of model names (optional for OAuth-backed providers that fetch models via API)                          |

Re-registering the same name replaces the previous entry. Unknown `type` values
fall back to `openai-compatible`.

### Provider Types

| Type                   | Endpoint                                           | Compatible Services                            |
| ---------------------- | -------------------------------------------------- | ---------------------------------------------- |
| `openai-compatible`    | `/v1/chat/completions`                             | Ollama, vLLM, SGLang, llama.cpp, Google Gemini |
| `openai`               | `/v1/responses`                                    | OpenAI, OpenRouter                             |
| `codex`                | `chatgpt.com/backend-api/codex` (OAuth)            | OpenAI Codex (ChatGPT subscription)            |
| `anthropic-compatible` | `/v1/messages` + thinking                          | Anthropic-compatible APIs                      |
| `anthropic`            | `/v1/messages` + thinking                          | Anthropic                                      |
| `copilot`              | `api.*.githubcopilot.com/chat/completions` (OAuth) | GitHub Copilot subscription                    |
| `kimi-code`            | `api.kimi.com/coding/v1/messages` (OAuth)          | Kimi Code subscription                         |

### Model Configuration

Models can be plain strings or tables with per-model overrides:

```lua
smelt.provider.register("ollama", {
  type = "openai-compatible",
  api_base = "http://localhost:11434/v1",
  models = {
    { name = "qwen3.6:27b", temperature = 0.8, top_p = 0.95, top_k = 40, min_p = 0.01, repeat_penalty = 1.0 },
    { name = "custom-model", input_cost = 2.0, output_cost = 8.0, cache_read_cost = 0.5, cache_write_cost = 0.0 },
  },
})
```

Per-model overrides:

| Field                | Description                                                                                               |
| -------------------- | --------------------------------------------------------------------------------------------------------- |
| `name`               | Model id as it appears in API requests                                                                    |
| `temperature`        | Sampling temperature                                                                                      |
| `top_p`              | Top-p (nucleus) sampling                                                                                  |
| `top_k`              | Top-k sampling                                                                                            |
| `min_p`              | Min-p sampling (openai-compatible only)                                                                   |
| `repeat_penalty`     | Repetition penalty (openai-compatible only)                                                               |
| `tool_calling`       | Set to `false` to disable tools for this model                                                            |
| `input_cost`         | USD per 1M input tokens                                                                                   |
| `output_cost`        | USD per 1M output tokens                                                                                  |
| `cache_read_cost`    | USD per 1M cache-read tokens                                                                              |
| `cache_write_cost`   | USD per 1M cache-write tokens                                                                             |
| `max_tokens`         | Maximum output tokens for this model. Defaults to the model's own limit, falling back to 4096 if unknown. |
| `thinking_budgets`   | Per-level budgets for budget-based thinking: `{ low = 2048, medium = 8192, high = 16384, max = 16384 }`   |
| `context_window`     | Total context window in tokens. Overrides provider/catalog metadata when set.                             |
| `supports_reasoning` | Whether this model supports reasoning/thinking parameters. Overrides provider/catalog metadata when set.  |
| `supports_fast_mode` | Whether this model supports accelerated inference. Enables `fast_mode` and `/fast` when true.              |
| `input_modalities`   | Array of accepted inputs such as `{ "text", "image", "pdf" }`; overrides discovered metadata.         |

#### Pricing

Cost tracking is built in for popular models (GPT, Claude, DeepSeek).
Subscription-backed providers such as Codex, Copilot, and Kimi Code are shown as
zero-cost because they are included with your subscription. The session cost is
shown in the status bar and the running total appears in `/stats`.

For models not in the built-in table, or to override built-in prices, set cost
fields on the model config. All values are USD per 1 million tokens. Unknown
models default to zero cost.

## Model Selection

Model resolution follows this precedence on a fresh launch:

1. `--model` CLI flag
2. Last explicitly chosen model (recalled from `recent.json`)
3. `smelt.defaults.set{ model = "..." }` in `init.lua`
4. First picker-visible model in configuration or managed-provider priority order

If the catalog has no picker-visible entries, its first internal model is used as
the fallback. Switch models at runtime with `/model`. The choice is recorded in
`recent.json` in the [state directory](#storage-paths) and restored on the next
launch. To
always start from `smelt.defaults` and ignore the last pick, set
`smelt.remember.set({ model = false })` in `init.lua`.

OAuth-backed Codex, Copilot, and Kimi Code catalogs load from cache for the first
frame and refresh in the background. Fresh models and capability metadata become
available in the running picker without a restart. Models that providers mark
hidden stay out of picker and list surfaces but remain available by explicit
`provider/model` key. Login, logout, account changes, and authoritative empty
catalogs are reflected live without discarding the saved selection key.

The TUI can run while a requested model is pending or unavailable. In that state
`smelt.model.current()` returns `nil`, and model-backed actions fail with a clear
message instead of dispatching an empty model name. `smelt.model.status()` reports
the requested key, availability reason, and sanitized managed-provider refresh
status.

## Modes and Reasoning

Starting mode and reasoning effort can be set via CLI flags or in `init.lua`.
Both are toggleable at runtime: `Shift+Tab` cycles modes, `Ctrl+T` cycles
reasoning.

| CLI flag                     | Description                                                                 |
| ---------------------------- | --------------------------------------------------------------------------- |
| `--mode <MODE>`              | Starting mode: `normal`, `plan`, `apply`, `yolo`                            |
| `--mode-cycle <MODES>`       | Modes for `Shift+Tab` cycling (comma-separated)                             |
| `--reasoning-effort <LEVEL>` | Starting reasoning: `off`, `low`, `medium`, `high`, `xhigh`, `max`, `ultra` |
| `--reasoning-cycle <LEVELS>` | Levels for `Ctrl+T` cycling (comma-separated)                               |

Reasoning effort controls how deeply the model thinks before responding.
Supported by Anthropic (`thinking`), OpenAI (`reasoning`), and any
openai-compatible / anthropic-compatible provider that supports
`reasoning_effort`. The built-in labels are `off`, `low`, `medium`, `high`,
`xhigh`, `max`, and `ultra`, but non-empty provider-defined labels are also
accepted and preserved exactly on label-based provider APIs. Budget-based APIs
cannot interpret custom labels, so they use the configured `max` thinking
budget. Models explicitly marked as not supporting reasoning only accept `off`.

`/reasoning` opens a compact picker with the active model's native levels,
marking the current choice and the model default. Command hints and `Ctrl+T`
use the same model capabilities, not a universal effort scale. Provider-advertised
levels are authoritative, including labels unknown to this smelt version.
Known Claude models have model-specific built-ins. Budget-based presets remain
available even when their configured token budgets are identical.

Without advertised levels or a model-specific built-in, smelt reports that the
levels are unknown instead of guessing. `/reasoning <effort>` remains available
as an explicit override for these endpoints. A configured `--reasoning-cycle`
can supply a custom cycle for unknown models; for known models it is filtered
to supported levels. If none remain, cycling uses the model's supported list.
An explicitly empty cycle disables the shortcut.

When switching models, smelt keeps the current effort if supported; otherwise it
uses the new model's valid default, then its first supported level, and reports
the adjustment. Selecting an unsupported level explicitly returns an error
without changing the current effort.

Set thinking block presentation at runtime with
`/thinking [open|close|peek|toggle]`.

### Defaults vs. last-used

Smelt distinguishes two layers for model / mode / reasoning effort:

- **Defaults** in `init.lua` are the cold-start values, used when there is no
  recorded last-used pick.
- **Recent** (`recent.json` in the [state directory](#storage-paths)) is what you
  picked last session. Each launch restores it, so you don't have to re-pick.

Precedence on a fresh launch is
`CLI flag → recent → defaults → hardcoded fallback`. Resuming a session
(`--resume`) takes the session's own saved model / mode / effort, ignoring
`recent.json`.

Pin a cold-start value with `smelt.defaults.set{...}`:

```lua
smelt.defaults.set({
  model = "openai/gpt-5.5",
  mode = "plan",
  reasoning_effort = "high",
})
```

To make a key always start from `smelt.defaults` and ignore the last pick, opt
out per-key with `smelt.remember.set{...}`:

```lua
smelt.remember.set({
  mode = false,             -- always start in the default mode
  reasoning_effort = false, -- always start at the default effort
  -- model = true (default), still recalls the last model
})
```

### Settings and theme changes

`smelt.settings.*`, `smelt.theme.use(...)`, and `smelt.theme.apply(...)` apply
to the running session and never write to disk themselves. Put settings in
`init.lua`, and defer theme calls with `smelt.lifecycle.on_ready(...)`, to apply
them on every launch. Saved Lua config reloads automatically by default; run
`/reload` to apply changes manually without restarting.

## Per-plugin Model Preferences

Background features (title generation, compaction, prediction, `/btw`,
`web_fetch` extraction) live in bundled Lua plugins. Each plugin reads its
preferred model from `smelt.model.preferred("<name>")`, falling back to the
primary model when unset. Override one from `init.lua`:

```lua
smelt.model.preferred("title", "openai/gpt-5-mini")
smelt.model.preferred("compact", "anthropic/claude-haiku-4-5")
smelt.model.preferred("predict", "openai/gpt-5-mini")
```

Names used by the bundled plugins: `title`, `compact`, `predict`, `btw`,
`web_fetch`. Custom plugins can pick any name. References use the same
`provider/model` or bare-model resolution as the primary model.

## Settings

Set preferences in `init.lua` by writing to `smelt.settings`:

```lua
smelt.settings.vim = true
smelt.settings.auto_compact = true
smelt.settings.auto_continue = "goal"
smelt.settings.compact_threshold = 0.65
smelt.settings.compact_keep_recent_groups = 1
smelt.settings.show_tps = true
```

Set settings from `init.lua`, the `--set` CLI flag, or any Lua context. Unknown
keys raise at the access site; type mismatches raise on assignment.

<!-- SETTINGS_REFERENCE_BEGIN -->
<!-- This region is auto-generated by `cargo xtask gen-lua-docs`. Do not edit by hand. -->

Read or write via `smelt.settings.<key>` from `init.lua`. Saved Lua config reloads automatically by default; run `/reload` to apply changes manually. Override from the CLI with `--set KEY=VALUE`. Assigning an unknown key or wrong type raises at the call site.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `vim` | `boolean` | `false` | Vi keybindings in the prompt. |
| `system_clipboard` | `boolean` | `true` | Sync prompt kills and yanks with the OS clipboard. Disable to keep `C-w`/`C-k`/`C-u`/`C-y` and vim `y`/`p` internal when OSC 52 clipboard writes are unreliable. Bracketed terminal paste still works. |
| `auto_compact` | `boolean` | `true` | Auto-summarize when request context usage crosses `compact_threshold` (forced on in headless). |
| `auto_continue` | `"off"` \| `"goal"` \| `"always"` | `"goal"` | Idle auto-continue policy: `off` disables it, `goal` continues active auto goals, and `always` continues any idle session. |
| `show_tps` | `boolean` | `true` | Tokens/sec in status bar. |
| `show_tokens` | `boolean` | `true` | Context token count in status bar. |
| `show_cost` | `boolean` | `true` | Session cost in status bar. |
| `show_prediction` | `boolean` | `true` | Ghost-text input predictions in the prompt. |
| `show_tips` | `boolean` | `true` | Curated discovery tips in the start banner and prompt chrome. |
| `file_icons` | `boolean` | `false` | Show Nerd Font file-type icons before inline-code paths that point at existing files. |
| `file_icon_colors` | `boolean` | `true` | Color inline-code file icons with nvim-web-devicons colors when `file_icons` is enabled. |
| `show_slug` | `boolean` | `true` | Task-slug label in status bar. |
| `terminal_title` | `boolean` | `true` | Keep the terminal window/tab title in sync with the current session title. |
| `restrict_to_workspace` | `boolean` | `true` | Downgrade `Allow` to `Ask` for paths outside the workspace. |
| `redact_secrets` | `boolean` | `false` | Scrub detected secrets from user input and tool results before they reach the LLM. |
| `auto_reload` | `boolean` | `true` | Watch Lua config inputs (init.lua, plugins/, commands/, completers/, tools/, dialogs/, runtime overrides) and dispatch `/reload` when any of them changes. Prompt inputs such as AGENTS.md, SKILL.md, and `--system-prompt` stay manual via `/reload`. |
| `compact_threshold` | `number` | `0.8` | Fraction of the configured context window (0, 1] at which the bundled compact plugin auto-triggers before oversized requests. |
| `compact_keep_recent_groups` | `number` | `1` | Minimum number of trailing message groups kept verbatim after compaction. A group is a user message, a plain assistant message, or an assistant tool-use step together with its tool outputs. |
| `request_audit` | `"off"` \| `"summary"` \| `"full"` | `"summary"` | Request audit storage mode. `summary` keeps timing, token, cost, and size metadata only; `full` stores reconstructable provider payloads; `off` disables request audit writes. |
| `fast_mode` | `boolean` | `false` | Request the provider's accelerated inference mode when supported. |
| `cache_ttl_long` | `boolean` | `false` | Anthropic prompt cache TTL. `false` uses the 5-minute ephemeral TTL; `true` opts into the 1-hour TTL. Has no effect on non-Anthropic providers. |
| `web_search_provider` | `"auto"` \| `"duckduckgo"` \| `"brave"` | `"auto"` | Search provider used by `web_search`; `auto` prefers Brave when its API key is available and otherwise uses DuckDuckGo. |
| `brave_search_api_key_env` | string | `"BRAVE_SEARCH_API_KEY"` | Environment variable containing the Brave Search API key. |
| `web_fetch_render` | `"http"` \| `"auto"` \| `"browser"` | `"auto"` | JavaScript rendering policy for `web_fetch`: `http` never launches a browser, `auto` renders challenge and SPA-shell responses, and `browser` always renders. |
| `web_fetch_renderer_command` | string | `""` | Renderer executable used by `web_fetch`. It reads a JSON request from stdin and writes rendered HTML, status, and final URL as JSON to stdout. |
| `worktree_root` | string | `".worktrees"` | Root directory for managed git worktrees. Relative paths are resolved inside the git root and contain worktrees directly; absolute paths are external roots and get a per-repository bucket. Supports leading `~`, `$VAR`, and `${VAR}` expansion; relative roots may not escape the repo. |
| `autoupgrade` | `"off"` \| `"notify"` \| `"auto"` | `"notify"` | Autoupgrade behavior. `"off"` skips checks; `"notify"` shows a pill when an update is available; `"auto"` installs in background on detection. |
| `autoupgrade_channel` | `"stable"` \| `"unstable"` | `"stable"` | Release channel autoupgrade tracks: `"stable"` (published GitHub releases) or `"unstable"` (`main` HEAD). |
| `autoupgrade_interval` | `number` | `3600` | Seconds between background autoupgrade checks. The upgrade plugin clamps to a 60-second minimum to avoid hammering GitHub. |

<!-- SETTINGS_REFERENCE_END -->

The default `auto` search provider uses Brave when its configured API key is
available. To require Brave instead of allowing the keyless DuckDuckGo fallback,
select it explicitly and export the API key environment variable:

```lua
smelt.settings.web_search_provider = "brave"
smelt.settings.brave_search_api_key_env = "BRAVE_SEARCH_API_KEY"
```

```bash
export BRAVE_SEARCH_API_KEY=...
```

### Auto-continue and quota pauses

`smelt.settings.auto_continue` controls automatic continuation in interactive
sessions: `"off"` disables it, `"goal"` continues active goals with auto-continue
enabled, and `"always"` also allows continuation without a goal.

A quota or rate-limit error pauses the interrupted conversation. Queued messages
stay visible above the prompt and are not sent while paused. When the provider
supplies a reset time and auto-continue allows it, the same row shows
`quota exceeded · resuming at 14:30` in local time, showing the next attempt.
To detect early quota resets, retries back off from 1 minute to 2 minutes,
4 minutes, and then every 5 minutes. The original provider reset time is retained:
if it arrives before the next periodic retry, smelt tries just after that reset
instead. Later estimates cannot postpone an untried reset deadline, and a retry
response that omits reset metadata does not stop an established recovery schedule.
Resumption defers while a draft, dialog, or busy task needs attention.
If the initial error has no known reset time, the status reads
`quota exceeded · paused` and recovery requires a manual retry.

Press **Enter** with an empty prompt to retry the interrupted conversation
manually. **Esc Esc**, the usual cancel chord, stops a scheduled retry and cancels
foreground busy work without discarding queued messages (close any open picker
first). New input submitted during the pause is queued. Resuming retries the
original work first; turn-queued messages follow after it finishes. Request-queued
steering retains its separate delivery stage. Command-scoped model, sampling,
reasoning, and permission overrides survive retries and reloads without being
applied to subsequent queued turns. Changes to global settings while paused still
apply where the interrupted command has no override.

Background-process completions are recorded during the pause but cannot bypass
the retry schedule. A repeated, expired reset time does not trigger rapid retries;
periodic backoff continues. Reloading preserves the backoff and the next attempt.
Settings and goal changes are rechecked before resuming, and cancellation or a
session change invalidates the pending continuation. Successful recovery or a
manual resume starts a fresh backoff for any subsequent quota pause.

### Request audit

`smelt.settings.request_audit` controls the provider-request records stored in
each session database:

| Mode | Stored data |
| ---- | ----------- |
| `summary` | Timing, token, cost, transport, and payload-size metadata (default) |
| `full` | Summary metadata plus reconstructable request and response payloads |
| `off` | No new request-audit rows |

Full payloads can contain prompts, source excerpts, tool results, and model
responses. Treat a session database or `smelt export requests` output created in
this mode as sensitive. Set `SMELT_REQUEST_AUDIT=off|summary|full` to pin the mode
for a process; this environment override takes precedence over Lua config and
continues to win after `/reload`.

### Fast mode

`smelt.settings.fast_mode = true` requests accelerated inference at startup.
The active model must resolve `supports_fast_mode = true` from explicit model
config or provider metadata. `/fast on|off|toggle` changes the current session;
unsupported models reject the command. Provider-specific billing or quota rules
still apply.

### Notifications

Terminal notification preferences are a nested table, not scalar `--set` keys:

```lua
smelt.settings.notifications = {
  turn_end = true,
}
```

`turn_end` defaults to `false`. The `/notify` command can enable the next
notification or override the setting for the current session without editing
config.

`smelt.settings.transcript` is an additional Lua table for transcript display
preferences. It is not a scalar `--set` key. Use `view` to set default fold
states: `"collapsed"`, `"peek"`, or `"expanded"` for block kinds, tool names,
and group names; use `false` to disable a built-in group. Use `limits` for
UI-only row caps.

`show_timestamps` defaults to `true`. Set
`smelt.settings.transcript.show_timestamps = false` to hide timestamps on user
messages, tool calls, and tool groups without hiding tool execution durations.
This preference and its rendering policy are Lua-owned; it is not a `--set` key.
Changes apply to existing transcript blocks as well as new ones.

Timestamps use local time: `HH:MM:SS` today, `mon DD HH:MM:SS` on earlier days,
and `YYYY mon DD HH:MM:SS` in earlier years. Undated history stays undated.
User timestamps occupy the panel's top padding row, use the theme's
`SmeltUserTimestamp` foreground, and are excluded from copied message text.

```lua
smelt.settings.transcript = {
  show_timestamps = true,
  view = {
    blocks = { thinking = "peek" },
    tools = {
      load_skill = "collapsed",
      read_file = "collapsed",
      grep = "collapsed",
      glob = "collapsed",
      web_fetch = "collapsed",
      write_file = "expanded",
      edit_file = "collapsed",
      edit_notebook = "expanded",
    },
    groups = {
      explore = "collapsed",
      lsp = "collapsed",
      web = "collapsed",
      -- explore = false,
      -- lsp = false,
      -- web = false,
    },
  },
  limits = {
    tool_rows = 20,
    tool_header_rows = 20,
    tool_body_rows = 20,
    tool_output_rows = 20,
    collapsed_error_rows = 4,
    thinking_peek_rows = 4,
    thinking_peek_head_rows = 1,
  },
}
```

Override any setting from the CLI with `--set KEY=VALUE`. Boolean values must be
`true`/`false`; numeric values are parsed as floats; string values are passed
through as-is and validated against the schema's allowed-choice list (if any).

## Theme

The task slug color is session-specific; change it with `/color`.

A full colorscheme is a `ThemeSpec`: a Lua table with optional `name`,
`syntax`, and `light` metadata plus a required `groups` table. `groups` is keyed
by highlight-group name (`SmeltAccent`, `Comment`, `SmeltDiffAddBg`, …) whose
values are either a `StyleDecl` table (`{ fg = ..., bold = true }`) or a string
referencing another group in the same spec. Built-in colorschemes live at
`runtime/lua/smelt/colorschemes/<name>.lua`; custom ones drop in at
`~/.config/smelt/lua/smelt/colorschemes/<name>.lua` and load via
`smelt.theme.use("<name>")`. See the
[customization guide](../guide/customization.md#themes) for the full shape;
`runtime/lua/smelt/colorschemes/default.lua` is the worked example. The canonical
group names, style roles, ownership, and descriptions are listed in
`runtime/lua/smelt/colorschemes/_groups.lua`.

## MCP (Model Context Protocol)

Connect external tool servers that expose tools via MCP. Each server runs as a
child process communicating over stdio. MCP lets you extend smelt without
writing Lua: if a server exists for Postgres, Slack, or your internal API, the
agent can use it immediately.

```lua
smelt.mcp.register("filesystem", {
  description = "Read and write files under /tmp via MCP.",
  command = { "npx", "-y", "@modelcontextprotocol/server-filesystem", "/tmp" },
  env = { DEBUG = "true" },
  timeout = 30000,
  enabled = true,
})
```

| Field         | Description                                                        |
| ------------- | ------------------------------------------------------------------ |
| `type`        | Server kind. Only `"local"` (the default) is supported.            |
| `description` | Human-readable summary shown by `/mcp`.                            |
| `command`     | String or array of strings: executable and leading argv            |
| `args`        | Additional arguments (appended to `command`)                       |
| `env`         | Environment variables for the server process                       |
| `timeout`     | Connection and tool-call timeout in milliseconds. Default `30000`. |
| `enabled`     | Set to `false` to skip connecting on startup. Default `true`.      |

MCP tools appear in the agent's tool list with names prefixed by the server name
(e.g. `filesystem_read_file`). They default to "ask" permission.

### MCP Permissions

MCP tools use a separate `mcp` ruleset in the permissions config. Patterns are
matched against the qualified tool name (`servername_toolname`). See the
[Permissions Reference](permissions.md) for details.

## Skills

Skills are on-demand knowledge packs the agent can load via the `load_skill`
tool. They keep the system prompt lean: only the skills relevant to the current
task are injected, so the agent stays focused and you save context tokens.

They are scanned from these directories (later entries override):

1. `~/.config/smelt/skills/*/SKILL.md`, global user skills
2. `~/.claude/skills/*/SKILL.md`, global Claude-compatible skills
3. `~/.agents/skills/*/SKILL.md`, global Agent Skills-compatible skills
4. `.smelt/skills/*/SKILL.md`, project-local smelt skills
5. `.claude/skills/*/SKILL.md`, project-local Claude-compatible skills
6. `.agents/skills/*/SKILL.md`, project-local Agent Skills-compatible skills

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

smelt uses four platform directories: **config**, **state**, **data**, and
**cache**. Paths elsewhere in the documentation use these names rather than
assuming an operating system.

On Linux and macOS, they are stored under the XDG base directories:

| Directory                           | Contents                                                                          |
| ----------------------------------- | --------------------------------------------------------------------------------- |
| `$XDG_CONFIG_HOME/smelt/`           | `early.lua`, `init.lua`, autoloaded `plugins/`, reusable `lua/` modules, `commands/`, and `skills/` |
| `$XDG_STATE_HOME/smelt/sessions/`   | Canonical lineage databases, derived search projections, ownership locks, and the disposable session-list catalog |
| `$XDG_STATE_HOME/smelt/*_auth.json` | Private OAuth credential files for Codex, Copilot, and Kimi Code                  |
| `$XDG_STATE_HOME/smelt/recent.json` | Last-used picks (model, mode, reasoning effort)                                   |
| `$XDG_STATE_HOME/smelt/workspaces/` | Per-workspace saved permissions                                                   |
| `$XDG_STATE_HOME/smelt/history`     | Prompt history                                                                    |
| `$XDG_STATE_HOME/smelt/trust.json`  | Trusted project `.smelt/` hashes                                                  |
| `$XDG_STATE_HOME/smelt/logs/`       | Log files (rotated)                                                               |
| `$XDG_DATA_HOME/smelt/builtins/`    | Read-only mirror of the embedded Lua runtime and generated LuaCATS stubs          |
| `$XDG_DATA_HOME/smelt/runtime/`     | Extra Lua runtime roots (optional)                                                |
| `$XDG_CACHE_HOME/smelt/web/`        | HTTP/pricing cache                                                                |
| `$XDG_CACHE_HOME/smelt/`            | `copilot_models.json` and other discovered model caches                           |

Windows uses `%APPDATA%\smelt` for config and `%LOCALAPPDATA%\smelt\state`,
`%LOCALAPPDATA%\smelt\cache`, and `%LOCALAPPDATA%\smelt\data` for the other
roots. Explicit XDG variables override these defaults on every platform.

OAuth-backed providers load credentials in this order: a provider-specific
environment override, a private JSON file in the state directory, then the
operating system's native keyring. Background status and model-cache checks
stop before the keyring, so they never open an operating-system credential
prompt. A provider operation can recover a keyring-only credential and copy it
to the private file after one successful read. `smelt auth` writes both file and
keyring storage. The files are `codex_auth.json`, `copilot_auth.json`, and
`kimi_code_auth.json`, with mode `0600` on Unix. Logout removes both copies.

## Environment Variables

| Variable                   | Purpose |
| -------------------------- | ------- |
| `XDG_CONFIG_HOME`          | Override the config root (Unix default: `~/.config`; Windows default: `%APPDATA%`) |
| `XDG_STATE_HOME`           | Override the state root (Unix default: `~/.local/state`; Windows default: `%LOCALAPPDATA%\smelt\state`) |
| `XDG_CACHE_HOME`           | Override the cache root (Unix default: `~/.cache`; Windows default: `%LOCALAPPDATA%\smelt\cache`) |
| `XDG_DATA_HOME`            | Override the data root (Unix default: `~/.local/share`; Windows default: `%LOCALAPPDATA%\smelt\data`) |
| `XDG_RUNTIME_DIR`          | Public process-status root; falls back to the platform temp directory |
| `HOME`                     | Used as a fallback when XDG variables are unset |
| `SMELT_RUNTIME_DIR`        | Highest-priority bundled Lua runtime override, mainly for development |
| `SMELT_REQUEST_AUDIT`      | Pin request audit to `off`, `summary`, or `full` |
| `SMELT_CODEX_TOKENS`       | JSON OAuth credential override for Codex |
| `SMELT_COPILOT_TOKENS`     | JSON OAuth credential override for Copilot |
| `SMELT_KIMI_CODE_TOKENS`   | JSON OAuth credential override for Kimi Code |
| `BRAVE_SEARCH_API_KEY`     | Brave Search key using the default `brave_search_api_key_env` setting |
| `COLORFGBG`                | Terminal color hint (fallback for dark/light detection) |
| `TERM`                     | Terminal type (`dumb` skips color detection) |
| `NO_COLOR`                 | Disable ANSI colors |
| `FORCE_COLOR`              | Force ANSI colors regardless of TTY detection |
| `VISUAL`                   | Preferred editor for `Ctrl+X Ctrl+E` |
| `EDITOR`                   | Fallback editor when `VISUAL` is unset |

The OAuth override variables contain each provider's complete JSON credential
record, not a single API key. They have highest load priority and are intended
for controlled automation; avoid shell history and process environments that
other users can inspect.

## CLI Flags

CLI flags override config values for the current run. See the
[CLI Reference](cli.md) for the full list.
