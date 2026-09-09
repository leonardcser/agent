# Commands

Type `/` to open the command picker with fuzzy search.

## Built-in Commands

| Command                                  | Description |
| ---------------------------------------- | ----------- |
| `/goal [objective|subcommand]`           | Create, inspect, pause, resume, block, complete, or clear a persistent session goal |
| `/clear`, `/new`                         | Start a new conversation |
| `/rewind`                                | Rewind to a previous turn (same as `Esc Esc`) |
| `/resume`                                | Resume a saved session |
| `/diff`                                  | View staged, unstaged, and untracked local Git changes |
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

`/goal <objective>` creates an auto-continuing goal and asks the agent to pursue
it. `/goal set <objective>` is equivalent. While the agent is working, creation
stays queued: Enter waits for the next turn; Ctrl+Enter waits for acknowledgment
at the next request in the current turn. The goal is not created or replaced
until that queued command is consumed. Withdrawing the command back into the
prompt and discarding it leaves the existing goal unchanged.
The agent's `create_goal` tool requires an unfinished goal to be completed or
cleared before another can be created.

| Form | Effect |
| ---- | ------ |
| `/goal` or `/goal status` | Show objective, state, progress, id, and auto-continue state |
| `/goal progress <label>` | Set the durable progress label shown in the goal bar |
| `/goal summary <label>` | Set a shorter stable goal-bar summary |
| `/goal pause` | Pause the goal and disable its auto-continue |
| `/goal resume` | Reactivate the goal, enable auto-continue, and schedule continuation |
| `/goal block [reason]`, `/goal blocked [reason]` | Mark the goal blocked and disable auto-continue |
| `/goal done` | Mark the goal complete and disable auto-continue |
| `/goal clear`, `/goal stop` | Remove the goal from the session |
| `/goal auto on`, `/goal auto off` | Enable or disable auto-continue; this also activates or pauses the goal |

All goal controls above run immediately, including with Enter or Ctrl+Enter
while the agent is working. They neither enter the message queue nor cancel
the current turn. For example, `/goal auto off` disables future goal
continuations while letting the current turn finish; `/goal resume` enables
continuation once idle. Existing queued user requests are left unchanged.

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

### Local diff viewer

`/diff` opens two centered side-by-side panes: a collapsible changed-file tree and
one continuous, syntax-highlighted unified diff. Click a file to jump to its
changes, or click a folder to expand/collapse it. Pressing selects a folder;
releasing over the same row toggles it without changing the preview or its position.
Folders and unchanged-context folds use `▶` / `▼` triangles. Both panes share
directory-first file order. Scrolling the preview, changing focus, and refreshing
the current file leave deliberately collapsed folders closed; explicit
next/previous-file navigation reveals the destination's parents.
Drag the shared pane divider to trade file-tree width for preview width. The
split survives refreshes and staging operations, keeps both panes usable, and
does not move focus, selection, or the current preview position.
There is no enclosing frame, branch header, or command footer.

The tree has two fixed, non-collapsible lowercase sections separated by a blank row,
each with its own file count and line totals:

- `unstaged`: index to worktree changes, including untracked files marked `?`.
- `staged`: `HEAD` to index changes, using the empty tree before the first commit.

Partially staged files appear in both sections with distinct patches and counts.
Even changes that cancel out against HEAD remain visible in their respective
sections. Ignored files are excluded. The read-only preview follows section and
directory order, with a blank row and dim full-width divider between files.

Fugitive-style shortcuts stage or unstage one file without editing the worktree.
An asynchronous refresh moves it between sections and updates patches and totals.
Selection advances within the source section, falling back to the previous file.
Clearing a section shows an empty state rather than following the file into the
other section, so repeated staging cannot immediately undo the final operation.
Empty sidebar sections show only their heading and `(0)` count. The preview shows
`all staged` or `nothing staged` when the active section is cleared; a repository
with no local changes shows `clean`. Status labels are lowercase and omit sentence
punctuation.
Sidebar wheel and scrollbar scrolling move only the viewport: the selected file
and preview stay unchanged, even when selection scrolls offscreen. Keyboard
navigation reveals the selection; clicking selects the indicated file. Staging or
unstaging also reveals an offscreen selection immediately, while an already-visible
selection keeps its screen position.

Filenames stay neutral. Status letters are colored: added/untracked green,
modified yellow, deleted/conflicted red, and renamed blue. Unresolved paths are
marked `U`; their worktree patch compares against the ours merge stage, with a
metadata row when Git has no patch. Nonzero line counts follow each filename or
section label with single spaces, using softer green/red theme colors. Zero
counts are hidden and binary files show `binary`.
Preview `+` / `-` markers retain the bright green/red foregrounds of `edit_file`.
Added/deleted backgrounds fill the preview width, including line numbers and
trailing space. Stronger inline backgrounds identify changed text without
replacing syntax colors. Narrow panes abbreviate large counts and prioritize
filenames over indentation.

| Key | Action |
| --- | ------ |
| `Tab` / `Shift-Tab` | Switch between files and diff |
| `Ctrl-W >` / `Ctrl-W <` | Grow/shrink the focused pane by four columns |
| `Ctrl-W =` | Give both panes equal width, subject to minimum sizes |
| `j` / `k`, arrows | Navigate rows; counts such as `12j` / `10k` work in either pane |
| `gg` / `G`, `Ctrl-U` / `Ctrl-D`, `Ctrl-B` / `Ctrl-F` | Start/end, half-page, full-page navigation |
| `h` / `l`, `w` / `b` / `e`, `0` / `$` | Vim text motions in the diff |
| `H` / `L`, `Shift-Left` / `Shift-Right`, `zh` / `zl` | Pan horizontally without wrapping |
| `Ctrl-J` / `Ctrl-K` | Next/previous file from either pane, skipping folders and keeping focus |
| `[` / `]` | Previous/next file |
| `s` / `u` | Stage/unstage the selected file |
| `-` | Stage an entry in `unstaged`; unstage an entry in `staged` |
| `{` / `}` | Previous/next hunk |
| `Enter` | Expand/collapse unchanged context; toggle a folder or open a file from the tree |
| `h` / `l`, Left / Right (tree) | Collapse/go to parent; expand/enter a folder |
| Click (tree) | Jump to a file, or expand/collapse a folder |
| `v` / `V`, `y` | Visual selection and copy |
| `r` | Refresh local changes |
| `q`, `Esc`, `Ctrl-C` | Close; Esc leaves a Vim selection or pending motion first |

Git acquisition and compact patch indexing run off the UI thread. Untracked
files are batched through a private temporary index and object directory, keeping
Git's own ignore rules, filters, encodings, and attributes. One root pathspec avoids
all-pairs matching in large file sets, without a subprocess per file or writes to
the repository's index or object store.

Text and navigation become available without waiting for whole-file syntax.
A background worker prioritizes visible rows, caches bounded syntax and inline
chunks, and repaints as colors arrive. Inline comparisons use the same character
and grapheme policy as `edit_file`. Large replacement blocks use viewport-only
positional pairing; comparisons over 8 KiB retain plain row emphasis, and time
budgets bound difficult comparisons. Inline results publish before distant syntax
seeks. Syntax follows each file's language and
tracks old/new source independently, including multiline comments and strings
inside collapsed context. A distant jump into a huge file may show plain diff
colors while the worker reconstructs syntax state; it never blocks scrolling
or file selection. Parser checkpoints accelerate revisits. After visible work,
the worker prefetches the first 128-256 rows of the previous and next files so
Ctrl-J/Ctrl-K navigation can display cached syntax on its first frame.

Both panes retain only visible rows, even for million-line patches and trees with
tens of thousands of files. Folder toggles update compact node indices, not
rendered file tables. Binary, rename, mode-only, and no-newline changes have
explicit metadata rows. Reading and indexing the patch remain linear in input
size; expanding context needs no further Git commands. Closing cancels pending
Git work and releases the syntax worker. Staging shortcuts apply in idle Normal
mode or the sidebar, not inside a Vim selection or pending motion.

Refresh preserves expanded context, source-relative cursor/scroll anchors and
horizontal pan. The title identifies loading, refreshing, staging and unstaging.
If refresh fails, the previous snapshot stays visible and navigable, marked
`stale - r refresh`. A successful index update followed by a failed refresh is
reported as partial success, not as a failed stage/unstage. Stale snapshots cannot
perform another index operation until a successful refresh.

The viewer is the bundled `smelt.plugins.diff` Lua plugin. Opt out through
`smelt.builtins.disable({ plugins = { "diff" } })` in `early.lua`.

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
