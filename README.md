# smelt

A Rust TUI coding agent. Connects to any OpenAI-compatible API (Ollama, OpenAI,
Anthropic, Google Gemini, OpenRouter, etc.) or your ChatGPT subscription via
OpenAI Codex, and provides an interactive terminal interface for code
generation, analysis, and assistance.

> [!WARNING]
>
> **This is an early-stage project.** Expect bugs, incomplete features, and
> breaking changes. Make sure to regularly update.

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

**With Anthropic:**

```bash
read -s ANTHROPIC_API_KEY && export ANTHROPIC_API_KEY
smelt --model claude-opus-4-5 --api-base https://api.anthropic.com/v1 --api-key-env ANTHROPIC_API_KEY
```

## Features

- **Tool use** — file read/write/edit, glob, grep, bash, notebooks, web
  fetch/search
- **Permission system** — granular allow/ask/deny per tool, bash pattern, URL
- **4 modes** — Normal, Plan, Apply, Yolo (`Shift+Tab` to cycle)
- **Vim mode** — full vi keybindings for the input editor
- **Sessions** — auto-save, resume, fork, rewind conversations
- **Compaction** — LLM-powered summarization to stay within context limits
- **Reasoning effort** — configurable thinking depth (off/low/medium/high/max)
- **File references** — attach files with `@path` syntax
- **Multi-agent** — parallel subagents with inter-agent messaging (opt-in)
- **Skills** — on-demand specialized knowledge via `SKILL.md` files
- **MCP** — connect external tool servers via the Model Context Protocol
- **Custom commands** — user-defined slash commands via markdown files
- **Custom instructions** — project-level `AGENTS.md` files
- **Input prediction** — ghost text suggesting your next message
- **Image support** — paste from clipboard or reference image files
- **Headless mode** — scriptable, no TUI
- **Interactive setup** — guided first-run wizard and `smelt auth` for managing
  providers

## Benchmarks

[SWE-bench Verified](https://www.swebench.com/) (500 instances):

| Agent                                               | Version | Model       | Resolved  | Date       |
| --------------------------------------------------- | ------- | ----------- | --------- | ---------- |
| **smelt**                                           | 0.4.0   | Qwen3.5-27B | **69.4%** | 2026-02-17 |
| [opencode](https://github.com/opencode-ai/opencode) | 1.3.9   | Qwen3.5-27B | 67.2%     | 2026-02-17 |

## Configuration

Config file: `~/.config/smelt/config.yaml` (respects `$XDG_CONFIG_HOME`).

```yaml
providers:
  - name: ollama
    type: openai-compatible # or: openai, anthropic, codex
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

defaults:
  model: ollama/qwen3.5:27b # provider_name/model_name

settings:
  vim_mode: false
  auto_compact: false
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
