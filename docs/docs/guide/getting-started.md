# Getting Started

## Installation

=== "Prebuilt Binaries"

    Download the latest binary for your platform from
    [GitHub Releases](https://github.com/leonardcser/smelt/releases) and place
    it somewhere on your `$PATH`:

    ```bash
    tar xzf smelt-*.tar.gz
    sudo mv smelt /usr/local/bin/
    ```

=== "From Source"

    ```bash
    cargo install --git https://github.com/leonardcser/smelt.git
    ```

    Or clone and build locally:

    ```bash
    git clone https://github.com/leonardcser/smelt.git
    cd smelt
    cargo install --path .
    ```

## First-Time Setup

Just run `smelt`. It will create `~/.config/smelt/config.yaml` and you're ready
to go.

You can also skip the wizard and connect directly with CLI flags.

### Local Models (Ollama)

```bash
ollama pull qwen3.5:0.8b
smelt --model qwen3.5:0.8b --api-base http://localhost:11434/v1
```

Any server that speaks the OpenAI chat completions API works: Ollama, vLLM,
SGLang, llama.cpp.

### Cloud Providers

=== ":fontawesome-brands-openai: OpenAI"

    ```bash
    read -s OPENAI_API_KEY && export OPENAI_API_KEY
    smelt --model gpt-5.4 \
          --api-base https://api.openai.com/v1 \
          --api-key-env OPENAI_API_KEY
    ```

=== ":fontawesome-brands-openai: OpenAI Codex"

    No API key needed — authenticate with your ChatGPT Pro/Plus subscription:

    ```bash
    smelt auth   # log in via browser OAuth
    smelt --model gpt-5.4
    ```

    The Codex provider uses OAuth to connect to your ChatGPT subscription.
    Tokens are stored locally and refreshed automatically.

=== ":simple-anthropic: Anthropic"

    ```bash
    read -s ANTHROPIC_API_KEY && export ANTHROPIC_API_KEY
    smelt --model claude-opus-4-6 \
          --api-base https://api.anthropic.com/v1 \
          --api-key-env ANTHROPIC_API_KEY
    ```

=== ":simple-openrouter: OpenRouter"

    ```bash
    read -s OPENROUTER_API_KEY && export OPENROUTER_API_KEY
    smelt --model anthropic/claude-sonnet-4-6 \
          --api-base https://openrouter.ai/api/v1 \
          --api-key-env OPENROUTER_API_KEY
    ```

## Writing a Config File

Once you have a setup you like, save it to `~/.config/smelt/config.yaml` so you
don't need CLI flags every time:

```yaml
providers:
  - name: ollama
    type: openai-compatible
    api_base: http://localhost:11434/v1
    models:
      - qwen3.5:27b

  - name: openai
    type: openai
    api_base: https://api.openai.com/v1
    api_key_env: OPENAI_API_KEY
    models:
      - gpt-5.4

  - name: anthropic
    type: openai-compatible
    api_base: https://api.anthropic.com/v1
    api_key_env: ANTHROPIC_API_KEY
    models:
      - claude-opus-4-6

defaults:
  model: ollama/qwen3.5:27b # provider_name/model_name
```

Now just run `smelt` — it connects to your default model automatically. Switch
models at runtime with `/model`. See the
[Configuration Reference](../reference/configuration.md) for all options.

## Next Steps

- [Usage Guide](usage.md) — modes, tools, sessions, and the full daily workflow
- [Customization](customization.md) — themes, settings, custom commands
- [CLI Reference](../reference/cli.md) — all command-line flags
