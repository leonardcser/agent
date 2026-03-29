# Usage

## The Basics

The agent streams its response and may call tools along the way.

- `Ctrl+J` or `Shift+Enter` inserts a newline (for multi-line messages)
- `Ctrl+R` fuzzy-searches your input history
- `Ctrl+X Ctrl+E` opens your `$EDITOR` for longer messages
- `Ctrl+C` clears the input, cancels the agent, or quits (context-dependent)
- `?` (with empty input) opens the help dialog

## Modes

The agent has four modes, each with different permission defaults. Press
`Shift+Tab` to cycle through them.

| Mode       | What it does                                                                                                                      |
| ---------- | --------------------------------------------------------------------------------------------------------------------------------- |
| **Normal** | Default. Asks before editing files or running commands. Read tools are auto-allowed.                                              |
| **Plan**   | Read-only. The agent produces a plan and calls `exit_plan_mode` when done. You review the summary and approve to switch to Apply. |
| **Apply**  | File edits are auto-approved. Bash still asks.                                                                                    |
| **Yolo**   | Everything auto-approved. You can still deny specific patterns via config.                                                        |

The current mode is shown in the status bar. Set the starting mode with `--mode`
or `defaults.mode` in config. Customize which modes appear in the cycle with
`--mode-cycle` or `defaults.mode_cycle`.

See [Permissions Reference](../reference/permissions.md) for the full default
matrix.

## Reasoning Effort

Press `Ctrl+T` to cycle through reasoning levels (`off`, `low`, `medium`,
`high`, `max`). Configure which levels appear with `defaults.reasoning_cycle` in
config.

## Tools

The agent can read files, edit code, run shell commands, fetch URLs, and more.
When a tool requires permission, a **confirm dialog** appears showing what the
tool wants to do. You can approve once, for the session, or for the workspace.
Press `Tab` to attach an optional message to your approval.

See [Tools Reference](../reference/tools.md) for the full list and
[Permissions](../reference/permissions.md) for details on approval scopes.

## File References

Type `@` followed by a path to attach file contents to your message. A fuzzy
file picker opens automatically:

```
explain @src/main.rs
```

Multiple `@` references work in the same message. Attaching the same file twice
won't double-send it.

## Shell Escape

Prefix with `!` to run a shell command directly: `!git status`. Output appears
inline in the conversation.

## Pasting

`Cmd+V` pastes from your clipboard — images are attached inline, and multi-line
text is collapsed into a single attachment.

## Message Queuing

While the agent is responding, keep typing. Messages queue up and are sent
sequentially when the agent finishes.

- `Esc` — unqueue pending messages so you can edit them
- `Esc Esc` — cancel the agent _and_ unqueue everything

## Sessions

Every conversation is automatically saved after each turn.

Resume from the CLI:

```bash
smelt --resume              # open the session picker
smelt --resume <SESSION_ID> # resume a specific session
```

Or use `/resume` from within the TUI. Use `/fork` to branch the current
conversation into a new session.

## Compaction

Long conversations eat context. `/compact` replaces older messages with a
condensed summary, freeing space while preserving essential information.

```
/compact keep details about the auth refactor
```

Your last 2 turns are always kept verbatim.

When `auto_compact` is enabled (via `/settings`), compaction triggers
automatically when context is running low. Press `Esc Esc` to cancel.

## Vim Mode

Toggle with `/vim` or set `settings.vim_mode` in config. Supports insert,
normal, and visual modes. See the
[Keybindings Reference](../reference/keybindings.md#vim-mode) for details.

## Input Stashing

Press `Ctrl+S` to stash your current input and get a blank buffer. Press
`Ctrl+S` again to restore it. Useful for firing off a quick message without
losing what you were composing.

## Input Prediction

After each turn, the agent may suggest your next message as dim **ghost text**.
Press `Tab` to accept it, or just start typing to dismiss. Toggle in `/settings`
→ `input_prediction` or set `settings.input_prediction` in config.
