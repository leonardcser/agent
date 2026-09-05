# Commands

Type `/` to open the command picker with fuzzy search.

## Built-in Commands

| Command                                  | Description |
| ---------------------------------------- | ----------- |
| `/goal [objective|subcommand]`           | Create, inspect, pause, resume, block, complete, or clear a persistent session goal |
| `/clear`, `/new`                         | Start a new conversation |
| `/rewind`                                | Rewind to a previous turn (same as `Esc Esc`) |
| `/resume`                                | Resume a saved session |
| `/session`                               | Show session, worktree, model, usage, and history metadata |
| `/compact [instructions]`                | Summarize older history to free context |
| `/fork`, `/branch`                       | Fork the current session |
| `/export`                                | Export conversation; prompts for clipboard or file |
| `/copy [--role ROLE] [--headers] [N]`    | Copy the newest matching conversation messages to the system clipboard |
| `/yank ...`                              | Alias for `/copy` |
| `/model [provider/model]`                | Switch model (opens picker if no name given) |
| `/fast [on|off|toggle]`                  | Toggle accelerated inference when the active model supports it |
| `/theme [name]`                          | Preview a bundled UI and syntax theme for this session |
| `/color [name]`                          | Set session color |
| `/stats`                                 | Show token usage statistics |
| `/usage`, `/cost`                        | Show session cost and active-provider usage limits |
| `/thinking [mode]`                       | Set thinking block presentation: `open`, `close`, `peek`, `toggle` |
| `/reasoning [level]`                     | Pick a model-supported reasoning effort, or set one explicitly |
| `/permissions`                           | Manage saved permissions |
| `/ps`                                    | Manage background processes |
| `/notify [once|on|off|clear|status]`      | Override turn-end terminal notifications for this session |
| `/history`                               | Fuzzy-search prompt history (also `Ctrl+R`) |
| `/nohl`                                  | Clear active search highlights |
| `/messages`                              | Show recorded errors, warnings, and notices |
| `/skills`                                | Show loaded skills and their source locations |
| `/mcp`                                   | Show MCP servers, lifecycle state, and tool names |
| `/worktree [name] [--base ref]`          | Pick, create, or enter a managed git worktree |
| `/wt [name] [-b ref]`                    | Alias for `/worktree` |
| `/help`                                  | Show keybindings (also `F1`) |
| `/docs`                                  | Open the smelt documentation in your browser |
| `/inspect`                               | Open the local session/request inspector when the opt-in plugin is enabled |
| `/btw <question>`                        | Ask a side question; answer streams into a dialog |
| `/brief [scope] [focus]`                 | Summarize planned or completed changes compactly |
| `/handoff [focus]`                       | Write a continuation handoff for another agent |
| `/reflect [focus]`                       | Step back and rethink recent changes before moving on |
| `/simplify [focus]`                      | Review changed code for reuse, quality, and efficiency |
| `/trust`                                 | Trust the current project's `.smelt/` content |
| `/reload`                                | Re-evaluate user Lua (init + plugins) without restarting (also `F5`) |
| `/version`                               | Show the running build identity as a notification |
| `/changelog`                             | Open the release notes for the latest cached build |
| `/upgrade [check]`                       | Install the newest smelt build (or refresh the cache with `check`) |
| `/exit`, `/quit`                         | Exit (also `:q`, `:qa`, `:wq`, `:wqa`) |

### Goals and auto-continue

`/goal <objective>` creates an auto-continuing goal and immediately asks the
agent to pursue it. `/goal set <objective>` is equivalent. An unfinished goal
must be completed or cleared before another can be created.

| Form | Effect |
| ---- | ------ |
| `/goal` or `/goal status` | Show objective, state, progress, id, and auto-continue state |
| `/goal progress <label>` | Set the durable progress label shown in the goal bar |
| `/goal summary <label>` | Set a shorter stable goal-bar summary |
| `/goal pause` | Pause the goal and disable its auto-continue |
| `/goal resume` | Reactivate the goal, enable auto-continue, and schedule continuation |
| `/goal block [reason]` | Mark the goal blocked and disable auto-continue |
| `/goal done` | Mark the goal complete and disable auto-continue |
| `/goal clear` | Remove the goal from the session |
| `/goal auto on`, `/goal auto off` | Enable or disable auto-continue; this also activates or pauses the goal |

Auto-continue runs only while idle. Queued user messages run first. By default,
`smelt.settings.auto_continue = "goal"`, so only active auto goals continue; if
the same goal remains active afterward, auto-continue resumes. When a provider
returns a quota or rate-limit reset time, an eligible auto-continue schedules its
next continue after that reset. Set `smelt.settings.auto_continue = "off"` to
disable idle continuation, or `"always"` to continue any idle session even when
no goal is active.

### Conversation copy

`/copy` copies the latest non-empty conversation message. Pass `N` to copy the
latest N messages in chronological order, or filter first with `--role user` or
`--role assistant` (`-r` also works). Multiple messages include `User:` and
`Assistant:` headers automatically; `--headers` also adds a header to a single
message. `/yank` accepts the same arguments.

### Managed worktrees

Run `/worktree` with no arguments to pick an existing worktree, start creating
one, or view worktree status. `/worktree <name> [--base <ref>]` creates or enters
a managed worktree; `/wt <name> [-b <ref>]` is the short form. Entering a
worktree switches smelt's real process working directory and reloads project
context. See [Managed worktrees](../guide/usage.md#managed-worktrees).

### Fast mode and notifications

`/fast` toggles accelerated provider inference for the current session. Explicit
`on`, `off`, and `toggle` forms are available. The command reports an error
rather than silently changing state when the active model does not advertise
fast-mode support.

`/notify` defaults to `once`, which notifies after the next completed turn.
`on` enables notifications for this session, `off` disables them for this
session, `clear` removes the session override, and `status` reports the effective
mode. Persistent defaults live in `smelt.settings.notifications.turn_end`.

The `/inspect` command comes from the opt-in `smelt.plugins.inspect` plugin. Add
`require("smelt.plugins.inspect")` to `init.lua` to register it.

## Shell Escape

Prefix with `!` to run a shell command directly, without going through the
agent. Output appears inline in the conversation. Shell escapes are useful for
quick checks, such as verifying test output or reading a config value, without
bloating the agent's context window with a full tool call.

```
!git status
!cargo test
```

Shell escapes also work while the agent is running.

## Custom Commands

Create `.md` files in `~/.config/smelt/commands/` and they become prompt-template
slash commands. Their filename is the command name, and their body is sent to the
agent after template expansion. See the
[Customization guide](../guide/customization.md#custom-commands) for an example.

For commands implemented as Lua handlers instead of agent prompts, put a module
under `~/.config/smelt/plugins/` and call `smelt.cmd.register(...)`. Lua handlers
can open UI, mutate runtime state, and invoke other commands; markdown commands
are better for reusable model instructions.

### Frontmatter

All fields are optional:

| Key                | Description                                                                                               |
| ------------------ | --------------------------------------------------------------------------------------------------------- |
| `description`      | Shown in the `/` picker                                                                                   |
| `model`            | Override model for this command. Prefer `provider_name/model_name`; bare names work only when unambiguous |
| `provider`         | Provider name used to resolve a bare `model` reference                                                    |
| `temperature`      | Sampling temperature                                                                                      |
| `top_p`            | Top-p (nucleus) sampling                                                                                  |
| `top_k`            | Top-k sampling                                                                                            |
| `min_p`            | Min-p sampling                                                                                            |
| `repeat_penalty`   | Repetition penalty                                                                                        |
| `reasoning_effort` | Known or provider-defined reasoning label supported by the command's selected model                      |
| `agent_skill`      | When true, expose this command as a loadable [skill](api/skills.md)                                       |
| `tools`            | `allow`/`ask`/`deny` lists for tool permissions                                                           |
| `bash`             | `allow`/`ask`/`deny` glob patterns for bash                                                               |
| `web_fetch`        | `allow`/`ask`/`deny` glob patterns for URLs                                                               |

### Shell Execution in Templates

- **Inline**: `` !`command` ``, output replaces the backtick expression
- **Fenced**: ` ```! ` code block, output replaces the block
- **Escape**: `` \!`command` ``, prevents execution
