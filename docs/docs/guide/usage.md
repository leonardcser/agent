# Usage

## The Basics

The agent streams its response and may call tools along the way.

- `Ctrl+J` or `Shift+Enter` inserts a newline (for multi-line messages)
- `Ctrl+R` fuzzy-searches your input history
- `Ctrl+X Ctrl+E` opens your external editor (`$VISUAL`, then `$EDITOR`)
- `Ctrl+C` clears the input, cancels the agent, or quits (context-dependent)
- `Ctrl+G` moves a foreground agent-started bash command to the background
- `Enter` submits; with an empty prompt, it continues the turn when history
  exists
- `F1` opens the help dialog; `F3` and `F12` toggle debug and performance panels

## Context Counter

When token display is enabled, the prompt bar shows the latest provider-reported
active-context token count and percentage of the active model's context window.
A `?` after the number or percentage means the count is stale: it was reported
by a different model or provider, for example after switching models or
rewinding to a turn created with another model. Smelt keeps the stale count
visible for orientation, but does not use it as the authoritative baseline for
compaction or request estimates; the marker disappears after the active model
reports fresh usage.

## Modes

Smelt ships with four modes by default, each with different permission defaults.
Press `Shift+Tab` to cycle through them. Modes let you change the agent's
behaviour to match the task: stay cautious in Normal mode, draft a durable plan
without code changes in Plan mode, speed through routine edits in Apply mode, or
stay hands-off on a trusted codebase in Yolo mode.

| Mode       | What it does                                                                                           |
| ---------- | ------------------------------------------------------------------------------------------------------ |
| **Normal** | Default. Asks before editing files or running commands. Read tools are auto-allowed.                   |
| **Plan**   | Read-only planning mode. Saves drafts with `present_plan`; writes are denied except saved plan drafts. |
| **Apply**  | File edits are auto-approved. Bash still asks.                                                         |
| **Yolo**   | Everything auto-approved. You can still deny specific patterns via config.                             |

The current mode is shown in the status bar. Set the starting mode with
`--mode`, and customize which modes appear in the cycle with `--mode-cycle`.

See [Permissions Reference](../reference/permissions.md) for the full default
matrix.

## Goals and Auto-Continue

A goal is a persistent session objective with a lifecycle, progress label, and
optional idle auto-continue. Create one only when you want smelt to keep pursuing
a defined objective across turns:

```
/goal implement the approved migration and validate every package
```

The goal bar shows its short summary and durable progress. Use `/goal status`,
`/goal pause`, `/goal resume`, `/goal block [reason]`, `/goal done`, and
`/goal clear` to control it. `/goal auto on|off` changes whether it continues
while idle. These controls take effect immediately, even while the agent is
working, without interrupting the current turn. For example, `/goal auto off`
stops future goal auto-continuation while letting the current turn finish.
Queued user messages always run before auto-continue.

The default `smelt.settings.auto_continue = "goal"` continues active auto goals
only. Set it to `"off"` to disable automatic continuation, or `"always"` to
continue any idle session. See [Commands](../reference/commands.md#goals-and-auto-continue)
for the full lifecycle.

## Reasoning Effort

Press `Ctrl+T` to cycle through reasoning levels (`off`, `low`, `medium`,
`high`, `max`). Lower levels are faster and cheaper; use them for routine
questions. Higher levels give the agent more compute for architecture reviews,
complex refactors, or debugging tangled bugs. Set the starting level with
`--reasoning-effort`, and configure which levels appear in the cycle with
`--reasoning-cycle`.

## Fast Mode

`/fast` toggles accelerated inference for the current session when the active
model advertises support. Use `/fast on`, `/fast off`, or `/fast toggle` for an
explicit action. Set `smelt.settings.fast_mode = true` to request it at startup;
unsupported models remain in their normal inference mode.

## Tools

The agent can read files, edit code, run shell commands, fetch URLs, and more.
When a tool requires permission, a **confirm dialog** appears showing what the
tool wants to do. You can approve once, for the session, or for the workspace.
Session and workspace approvals save you from repeatedly confirming the same
safe operation, for example, allowing every `git status` call in a repo you
trust. Press `Tab` to attach an optional message to your approval.

Long-running `bash` calls can continue as supervised background jobs. Press
`Ctrl+G` while one is foregrounded to stop following it, then use `/ps` to
inspect output or stop it. This is different from putting `&` in a shell
command, which the built-in tool rejects so the job supervisor retains ownership
of the entire process tree.

See [Tools Reference](../reference/tools.md) for the full list and
[Permissions](../reference/permissions.md) for details on approval scopes.

## File References

Type `@` followed by a path to attach file contents to your message. A fuzzy
file picker opens automatically:

```
explain @src/main.rs
```

`@` references let you point the agent at exactly the code you mean without
copy-pasting into the prompt. Multiple `@` references work in the same message.
Attaching the same file twice won't double-send it.

## Shell Escape

Prefix with `!` to run a shell command directly: `!git status`. Output appears
inline in the conversation. This is useful for quick checks, such as verifying
test output or reading a config value, without bloating the agent's context
window with a full tool call.

## Pasting

`Cmd+V` pastes from your clipboard. Images are attached inline, and multi-line
text is collapsed into a single attachment. Pasting images is handy for sharing
screenshots of UI bugs, diagrams, or design mock-ups without leaving the
terminal.

## Queuing and Steering

While the agent is responding, keep typing. Press `Enter` to queue a message for
later, or use `Ctrl+Enter` / `Ctrl+Q` to steer the response currently in
progress. In vim visual mode, `Enter`, `Ctrl+Enter`, and `Ctrl+Q` act on only
the selection.

- `Enter` while busy: queue this message for later
- `Ctrl+Enter` / `Ctrl+Q` while busy: steer the current response
- `Enter` on an empty prompt while responding: promote the next queued message to steering
- Another empty `Enter`: interrupt and start the first steering message as a fresh turn
- `Esc`: bring queued messages back into the prompt so you can edit them
- `Esc Esc`: cancel active work, or rewind when idle

During automatic compaction, that fresh turn waits for the in-flight compaction
instead of cancelling and restarting it. The selected message and remaining
queue are preserved. Explicit cancellation still stops compaction. Messages
queued during manual `/compact` run after compaction finishes.

## Copying Conversation Messages

`/copy` copies the latest conversation message to the system clipboard. Pass a
count, role filter, or headers when you need more context:

```
/copy --role assistant 2
/copy --headers 1
```

`/yank` is an alias with the same arguments. This operates on conversation
messages, while prompt selection copy and the kill ring are covered in the
[Keybindings Reference](../reference/keybindings.md).

## Sessions

Every conversation is automatically saved after each turn. Sessions let you
maintain parallel workstreams, one for the frontend refactor and another for the
API migration, and pick up exactly where you left off days later without
re-explaining the codebase.

Resume from the CLI:

```bash
smelt --resume              # open the session picker
smelt --resume <SESSION_ID> # resume a specific session
```

Or use `/resume` from within the TUI. Resuming, listing, and picker previews read
canonical lineage storage.

Use `/fork` to branch the current conversation into a new session, or `/rewind`
(also `Esc Esc` when idle) to roll back to an earlier turn. `/session` shows the
current id, paths, worktree, model, context usage, costs, compactions, and history
counts; press `c` there to copy the id or `y` to copy all metadata.

Use `smelt --ephemeral` for a temporary interactive session. Ephemeral sessions
can use tools and attachments normally, but they are stored in a temporary
directory, are removed when smelt exits, and do not appear in resume lists.

For debugging or auditing saved sessions and provider requests, run the local
inspector web UI:

```bash
smelt inspect
smelt inspect --session <SESSION_ID>
```

For database health checks, transactionally consistent backups, or storage
maintenance, use `smelt session doctor|backup|gc|vacuum`. See the
[CLI session-maintenance reference](../reference/cli.md#session-maintenance)
before running mutating maintenance against a lineage.

## Managed Worktrees

Managed worktrees isolate parallel implementations without moving the original
checkout. Start directly in one:

```bash
smelt --worktree docs-audit
```

Or create, pick, and enter them from a running session:

```
/worktree                         # picker and status
/worktree docs-audit --base main # create or enter
/wt docs-audit -b main           # short form
```

Names are normalized and deduplicated. The default location is
`<repo>/.worktrees/`; configure `smelt.settings.worktree_root` to use another
relative or absolute root. Entering one switches smelt's real process working
directory, then reloads project instructions, skills, trusted `.smelt/` config,
workspace permissions, and watcher roots for the new checkout.

An agent in a managed worktree does not push, open a pull request, merge into the
base checkout, or remove the worktree unless you explicitly ask. If you ask it to
land changes without choosing a strategy, the default is commit, rebase onto the
base branch, validate, and fast-forward the base checkout without a merge commit.

## Turn Notifications

`/notify` schedules a desktop terminal notification for the next completed turn.
Use `/notify on` or `/notify off` for a session override, `/notify clear` to return
to the configured default, and `/notify status` to inspect it. Persist the default
with:

```lua
smelt.settings.notifications = {
  turn_end = true,
}
```

Notification delivery depends on terminal and operating-system support.

## Compaction

Long conversations eat context. `/compact` replaces older messages with a
condensed summary, freeing space while preserving essential information.

```
/compact keep details about the auth refactor
```

The full transcript remains visible and scrollable. Only the model's context is
condensed. Smelt keeps recent turns verbatim, inserts a checkpoint marker where
compaction happened, and can compact automatically before a request would exceed
`compact_threshold` of the model's context window. Tune the behavior with
`smelt.settings.auto_compact`, `compact_threshold`, and
`compact_keep_recent_groups`; press `Esc Esc` to cancel an active compaction.

## Vim Mode

Set `smelt.settings.vim = true` in `init.lua` to enable vim mode. Supports
insert, normal, and visual modes. If you already live in Vim, this keeps your
muscle memory intact: navigate the transcript, edit the prompt, and select text
with the same chords you use in your editor. See the
[Keybindings Reference](../reference/keybindings.md#vim-mode) for details.

## Input Stashing

Press `Ctrl+S` to stash your current input and get a blank buffer. Press
`Ctrl+S` again to restore it. Stashing is useful when you are halfway through a
long prompt and need to ask a quick side question without losing your draft.

## Input Prediction

After each turn, the agent may suggest your next message as dim **ghost text**.
Press `Tab` to accept it, or just start typing to dismiss. Prediction saves
keystrokes on repetitive follow-ups like "also add tests", "fix the lint
errors", and so on. Set `smelt.settings.show_prediction = false` in `init.lua`
to disable prediction.
