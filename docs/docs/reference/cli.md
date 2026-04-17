# CLI Reference

```
smelt [MESSAGE]
smelt auth
```

When a message is provided, it auto-submits on startup. Running with no
arguments and no config file launches the interactive setup wizard.

## Subcommands

| Subcommand   | Description                                                                          |
| ------------ | ------------------------------------------------------------------------------------ |
| `smelt auth` | Manage provider authentication (add providers, Codex or GitHub Copilot login/logout) |

## Connection

| Flag                  | Description                                                                                                                       |
| --------------------- | --------------------------------------------------------------------------------------------------------------------------------- |
| `--config <PATH>`     | Path to a custom config file                                                                                                      |
| `-m, --model <MODEL>` | Model to use. With configured providers, prefer `provider_name/model_name`; with `--api-base`, use the provider-native model name |
| `--api-base <URL>`    | API base URL (overrides config)                                                                                                   |
| `--api-key-env <VAR>` | Env var holding the API key                                                                                                       |
| `--type <TYPE>`       | Provider type (auto-detected from URL when omitted)                                                                               |

Auto-detection:

| URL contains        | Detected type       |
| ------------------- | ------------------- |
| `api.openai.com`    | `openai`            |
| `chatgpt.com`       | `codex`             |
| `api.anthropic.com` | `anthropic`         |
| `githubcopilot.com` | `copilot`           |
| anything else       | `openai-compatible` |

## Behavior

| Flag                         | Description                                                                                      |
| ---------------------------- | ------------------------------------------------------------------------------------------------ |
| `--mode <MODE>`              | Starting mode: `normal`, `plan`, `apply`, `yolo`                                                 |
| `--mode-cycle <MODES>`       | Modes for `Shift+Tab` cycling (comma-separated)                                                  |
| `--reasoning-effort <LEVEL>` | Starting reasoning: `off`, `low`, `medium`, `high`, `max`                                        |
| `--reasoning-cycle <LEVELS>` | Levels for `Ctrl+T` cycling (comma-separated)                                                    |
| `--no-tool-calling`          | Disable tools (chat-only)                                                                        |
| `--system-prompt <PROMPT>`   | Override the system prompt (string or file path)                                                 |
| `--no-system-prompt`         | Disable system prompt and AGENTS.md                                                              |
| `--set <KEY=VALUE>`          | Override a config setting (repeatable; see [Settings](configuration.md#settings) for valid keys) |

Reasoning effort controls how deeply the model thinks before responding.
Supported by Anthropic (`thinking`), OpenAI (`reasoning`), and openai-compatible
providers that support `reasoning_effort`. For OpenAI, `max` maps to `xhigh`.
Models that don't support thinking ignore this setting.

## Sampling

| Flag                   | Description              |
| ---------------------- | ------------------------ |
| `--temperature <TEMP>` | Sampling temperature     |
| `--top-p <VALUE>`      | Top-p (nucleus) sampling |
| `--top-k <VALUE>`      | Top-k sampling           |

## Sessions

| Flag                        | Description                        |
| --------------------------- | ---------------------------------- |
| `-r, --resume [SESSION_ID]` | Resume a session (picker if no ID) |

## Multi-Agent

| Flag                    | Description                            |
| ----------------------- | -------------------------------------- |
| `--multi-agent`         | Enable multi-agent mode                |
| `--no-multi-agent`      | Disable multi-agent (overrides config) |
| `--max-agent-depth <N>` | Max spawn depth (default: 1)           |
| `--max-agents <N>`      | Max concurrent agents (default: 8)     |

## Runtime

| Flag                  | Description                                                                    |
| --------------------- | ------------------------------------------------------------------------------ |
| `--headless`          | No TUI — requires a message argument. See [Headless](../advanced/headless.md). |
| `--format <FORMAT>`   | Headless output format: `text` (default) or `json` (JSONL events)              |
| `-v, --verbose`       | Show tool output in headless mode                                              |
| `--color <WHEN>`      | Color output: `auto` (default), `always`, `never`                              |
| `--log-level <LEVEL>` | `debug`, `info`, `warn`, `error` (default: `info`)                             |
| `--bench`             | Print timing summary on exit                                                   |

CLI flags always take precedence over config values.
