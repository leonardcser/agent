<p align="center">
  <img src="docs/docs/logo.png" alt="smelt logo" width="160">
</p>

<h1 align="center">smelt</h1>

<p align="center">
  A Rust TUI coding agent. Connects to any OpenAI-compatible API (Ollama, OpenAI,
  Anthropic, Google Gemini, OpenRouter, etc.), your ChatGPT subscription via
  OpenAI Codex, or your GitHub Copilot subscription, and provides an interactive
  terminal interface for code generation, analysis, and assistance.
</p>

<p align="center">
  <img src="assets/demo.gif" alt="demo" width="800">
</p>

## Install

**Prebuilt binaries:**

Download from [GitHub Releases](https://github.com/leonardcser/smelt/releases).

**From source:**

```bash
cargo install --git https://github.com/leonardcser/smelt.git
```

Running `smelt` with no config file will launch an **interactive setup wizard**
that walks you through selecting a provider and model.

**With Ollama (local):**

```bash
ollama pull qwen3.5:0.8b
smelt --model qwen3.5:0.8b --api-base http://localhost:11434/v1
```

**With OpenAI:**

```bash
read -s OPENAI_API_KEY && export OPENAI_API_KEY
smelt --model gpt-5.4 --api-base https://api.openai.com/v1 --api-key-env OPENAI_API_KEY
```

**With OpenAI Codex (ChatGPT Pro/Plus subscription):**

```bash
smelt auth          # log in with your ChatGPT account
smelt --model gpt-5.4    # use any Codex-supported model
```

**With GitHub Copilot:**

```bash
smelt auth                          # pick "GitHub Copilot", follow device-code prompt
smelt --model claude-sonnet-4.5     # use any model your Copilot plan exposes
```

**With Anthropic:**

```bash
read -s ANTHROPIC_API_KEY && export ANTHROPIC_API_KEY
smelt --model claude-opus-4-5 --api-base https://api.anthropic.com/v1 --api-key-env ANTHROPIC_API_KEY
```

## Features

- **Tool use** — file read/write/edit, glob, grep, bash, notebooks, web
  fetch/search
- **Permission system** — granular allow/ask/deny per tool, bash pattern, URL,
  and workspace-scoped approvals
- **4 modes** — Normal, Plan, Apply, Yolo (`Shift+Tab` to cycle)
- **Vim mode** — full vi keybindings for the input editor
- **Sessions** — auto-save, resume, fork, rewind conversations
- **Compaction** — LLM-powered summarization to stay within context limits
  (auto-trigger threshold configurable via `SMELT_COMPACT_THRESHOLD_PERCENT`,
  default `80`)
- **Reasoning effort** — configurable thinking depth (off/low/medium/high/max)
- **File references** — attach files with `@path` syntax
- **Multi-agent** — parallel subagents with inter-agent messaging (opt-in)
- **Skills** — on-demand specialized knowledge via `SKILL.md` files
- **MCP** — connect external tool servers via the Model Context Protocol
- **Custom commands** — user-defined commands via markdown files
- **Lua scripting** — extend with `~/.config/smelt/init.lua` (keymaps, commands, autocmds, engine control, lifecycle events)
- **Custom instructions** — project-level `AGENTS.md` files
- **Input prediction** — ghost text suggesting your next message
- **Image support** — paste from clipboard or reference image files
- **Headless mode** — scriptable, no TUI
- **Interactive setup** — guided first-run wizard and `smelt auth` for managing
  providers

## Configuration

Config file: `~/.config/smelt/config.yaml` (respects `$XDG_CONFIG_HOME`).

```yaml
providers:
  - name: ollama
    type: openai-compatible # or: openai, anthropic, codex, copilot
    api_base: http://localhost:11434/v1
    models:
      - qwen3.5:27b

  - name: openai
    type: openai
    api_base: https://api.openai.com/v1
    api_key_env: OPENAI_API_KEY
    models:
      - gpt-5.4

  - name: codex
    type: codex # uses ChatGPT subscription — models fetched automatically
    api_base: https://chatgpt.com/backend-api/codex

  - name: copilot
    type: copilot # uses GitHub Copilot subscription — models fetched automatically
    api_base: https://api.individual.githubcopilot.com

defaults:
  model: ollama/qwen3.5:27b # provider_name/model_name

auxiliary:
  model: openai/gpt-5.4-mini
  use_for:
    btw: false

settings:
  vim_mode: false
  auto_compact: false
  redact_secrets: true # on by default — scrubs secrets from user input and tool results before they reach the LLM
```

See the [full documentation](https://leonardcser.github.io/smelt/) for all
config options, CLI flags, keybindings, permissions, and more.

## Documentation

Full docs are available at
[leonardcser.github.io/smelt](https://leonardcser.github.io/smelt/) and can be
built locally with [Zensical](https://github.com/zensical/zensical):

```bash
uv tool install zensical
cd docs && zensical serve
```

## Development

```bash
cargo build       # compile
cargo run         # run
cargo test        # run tests
cargo fmt         # format
cargo clippy      # lint
```

## Acknowledgments

Inspired by [Claude Code](https://github.com/anthropics/claude-code).

## Contributing

Contributions welcome! Open an issue or pull request.

## License

MIT — see [LICENSE](LICENSE).
