# Headless Mode

Run the agent without the TUI for scripting and automation.

## Usage

```bash
smelt --headless "explain this codebase"
```

A message argument is required.

## Output Format

### Text (default)

```bash
smelt --headless "summarize this repo"
```

- **stdout** — final assistant message only (written once the turn completes)
- **stderr** — tool activity, thinking, token usage, errors. Use `-v` /
  `--verbose` to include tool output

When both stdout and stderr are terminals (interactive use), the final message
is printed to stderr so it appears alongside tool output. When either stream is
piped or redirected, the final message goes to stdout — giving you a clean
answer suitable for files or downstream commands.

### JSON

```bash
smelt --headless --format json "summarize this repo"
```

Every `EngineEvent` is emitted as a JSON line (JSONL) to stdout.

## Color

ANSI colors in stderr output respect `NO_COLOR`, `TERM=dumb`, and TTY detection.
Override with `--color`:

```bash
smelt --headless --color=never "fix the bug" 2>log.txt
smelt --headless --color=always "fix the bug" 2>&1 | less -R
```

## Permissions

In headless mode, permission behavior depends on the mode:

- **Yolo mode** — all permissions auto-approved
- **Other modes** — unapproved tool calls are denied (no interactive prompt)

For fully autonomous scripting, combine with `--mode yolo`:

```bash
smelt --headless --mode yolo "fix the failing tests"
```

## Examples

Pipe the final answer to a file:

```bash
smelt --headless "summarize @src/main.rs" > summary.txt
```

Stream structured events for programmatic consumption:

```bash
smelt --headless --format json "fix the bug" | jq 'select(.type == "TurnComplete")'
```

Use in a CI pipeline:

```bash
smelt --headless --mode yolo "run cargo clippy and fix any warnings" 2>smelt.log
```
