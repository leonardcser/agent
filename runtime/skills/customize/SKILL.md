---
name: customize
description:
  Customize smelt. Change theme/colors, rebind keys, add slash commands,
  register tools/plugins, toggle settings, write skills. Includes a Lua
  capability map and pointers to exact signatures in the on-disk built-ins.
---

# Customizing smelt

Smelt is configured in Lua. Anything the user asks you to tweak (colors,
keybinds, slash commands, settings, tools, plugins, skills) happens by editing
Lua files under the user's config directory and reloading the runtime.

This skill gives you:

1. The core principle for how to respond
2. Where customization files live
3. Concrete recipes for the common asks
4. A compact Lua capability map with pointers to exact generated signatures

## Core principle: persist the change, don't punt to slash commands

When the user asks to change something, **edit the config file so the change
survives a restart**. Do not answer with "use a slash command" for durable
customization. Slash commands are runtime actions a user runs themselves; they
don't update `init.lua`, so preference changes are lost on the next launch.

Your default workflow is:

1. **Read** the user's `~/.config/smelt/init.lua` (create it if missing).
2. **Edit** in the specific Lua call that effects the change (e.g.
   `smelt.theme.set("SmeltAccent", { fg = { ansi = 208 } })`). Preserve every
   existing line.
3. **Call `smelt_reload`** so the edit takes effect now without waiting for the
   user to restart smelt.
4. Briefly tell the user what changed and where.

Mention runtime slash commands only for explicitly session-scoped choices such
as `/color`.

## When to use this skill

Load it whenever the user asks to persistently modify how smelt itself behaves:

- "change the prompt color to X" / "switch theme to <color>" / "make accents
  <color>"
- "rebind <chord> to <action>" / "add a shortcut for X"
- "add a /command that does X"
- "set <setting> to X" / "turn on vim mode" / "disable thinking blocks"
- "add an MCP server" / "register a provider" / "use OpenRouter"
- "register a tool that <does X>"
- "write a plugin that <does X>"
- "write a skill that <does X>"

Do **not** load this skill for ordinary coding work in the user's project. Only
load it when the target of the change is _smelt's own config_.

## Where things live

### User-editable

Smelt reads Lua from these locations, later sources overriding earlier:

| Path                                                   | Purpose                                  |
| ------------------------------------------------------ | ---------------------------------------- |
| Embedded in the binary                                 | Built-in plugins, commands, colorschemes |
| `$XDG_CONFIG_HOME/smelt/` (default `~/.config/smelt/`) | Global user config                       |
| `<cwd>/.smelt/`                                        | Project-local config (gated by `/trust`) |

Inside each user dir, the layout is:

```
~/.config/smelt/
  init.lua                       -- main entry point, runs at startup
  plugins/*.lua                  -- extra Lua chunks loaded after init.lua
  commands/*.lua                 -- /command definitions
  completers/*.lua               -- prompt completers
  tools/*.lua                    -- agent tool registrations
  dialogs/*.lua                  -- custom dialogs
  colorschemes/<name>.lua        -- colorschemes (load via smelt.theme.use)
  skills/<name>/SKILL.md         -- skills (user-authored)
  AGENTS.md                      -- extra system-prompt content
```

Use `~/.config/smelt/` for changes that should apply everywhere. Use `./.smelt/`
for changes that should only apply in the current project (run `/trust` once
after creating it).

### Read-only built-in source (for reference)

On every launch, smelt mirrors its embedded `runtime/lua/smelt/` tree to:

```
$XDG_DATA_HOME/smelt/builtins/lua/smelt/
```

Default: `~/.local/share/smelt/builtins/lua/smelt/`. The mirror is
version-stamped, so an upgrade refreshes it. **Treat this directory as
read-only**: edits there are wiped on the next launch after a smelt upgrade. Use
it for reading examples. Write changes to `~/.config/smelt/` (or `./.smelt/`).

What's inside that's useful when authoring custom Lua:

| Path under `builtins/lua/smelt/` | What it gives you                                                          |
| -------------------------------- | -------------------------------------------------------------------------- |
| `_meta/<module>.lua`             | LuaCATS type stubs. Best place to look up exact signatures + field shapes. |
| `_meta/_types.lua`               | Every class / alias used in `_meta/*.lua` (e.g. `smelt.theme.ThemeSpec`).  |
| `commands/<name>.lua`            | Worked `smelt.cmd.register` examples (e.g. `docs.lua`).                    |
| `colorschemes/<name>.lua`        | Worked `ThemeSpec` tables. `default.lua` is the canonical reference.       |
| `plugins/<name>.lua`             | Bundled plugin patterns (e.g. background commands, plan mode).             |
| `completers/<name>.lua`          | Built-in completer patterns.                                               |
| `dialogs/<name>.lua`             | Built-in dialog patterns.                                                  |

Reach for these before writing new Lua: copy the closest worked example, then
adapt.

### Reference docs

For deep details on a specific function (parameter shapes, return tables, opts
records), read the LuaCATS type stubs under the builtins mirror:

```
$XDG_DATA_HOME/smelt/builtins/lua/smelt/_meta/
```

One file per namespace (`cmd.lua`, `keymap.lua`, `theme.lua`, etc.) plus
`_types.lua` for shared classes/aliases referenced by signatures. The stubs use
LuaCATS syntax (`---@class`, `---@field`, `---@type fun(...)`), which is dense
but precise. The capability map further down helps locate the relevant namespace.

### Finding the config + builtins dirs from a shell

```bash
# config dir (where init.lua lives):
echo "${XDG_CONFIG_HOME:-$HOME/.config}/smelt"

# builtins dir (read-only worked examples):
echo "${XDG_DATA_HOME:-$HOME/.local/share}/smelt/builtins/lua/smelt"
```

If `init.lua` doesn't exist, create the directory and the file. The first-launch
wizard does this normally, but you may be invoked when neither has happened yet.

## Hot reload

Lua config edits are hot-reloaded automatically by default. The reload re-runs
`init.lua`, all autoloaded plugins/commands/tools, and the user's `plugins/`.
Persistent state (`smelt.state`, `smelt.signal`) survives across the cycle;
module-local state in plugins is reset.

**You (the agent) trigger the reload by calling the `smelt_reload` tool.** Do
this once, at the end of your turn after every set of edits to files under
`~/.config/smelt/` or `./.smelt/`. The tool schedules the reload to fire when
the current turn completes, so it never cancels its own in-flight call. Multiple
calls in the same turn collapse into one reload.

Do **not** silently flip `smelt.settings.auto_reload` in the user's config. It is
enabled by default, but if the user disabled it, respect that choice.

For reference (don't recommend these unless the user asks):

| How                   | Triggered by   | When it fires                                                  |
| --------------------- | -------------- | -------------------------------------------------------------- |
| `smelt_reload` tool   | You, the agent | At end of the current turn (the path you should use)           |
| `/reload`             | User typing    | Immediately (blocks if agent is busy)                          |
| `F5`                  | User keypress  | Immediately (blocks if agent is busy)                          |
| `auto_reload` setting | User setting   | Lua-file changes, debounced and deferred until the agent is idle |

Prompt inputs (`AGENTS.md`, `SKILL.md`, `--system-prompt`) and markdown custom-command registration stay manual via `/reload`. Existing markdown custom commands read their file body on each invocation, so body edits do not need reload.

## Recipes

### Rebind a key

`smelt.keymap.set(mode, chord, handler) -> Reg`. Modes are `"n"` (normal), `"i"`
(insert), `"v"` (visual), or `""` (all). Chord syntax follows nvim conventions:
`<C-y>`, `<S-Tab>`, `<leader>x`, etc.

```lua
-- In ~/.config/smelt/init.lua
smelt.keymap.set_leader("<space>")
smelt.keymap.set("n", "<leader>r", function()
  smelt.cmd.run("resume")
end)
```

`<leader>` expands when the binding is registered; existing bindings keep the
leader value they were created with. The default leader is a single backslash
(`\\`).

```lua
-- In ~/.config/smelt/init.lua
smelt.keymap.set("n", "<C-y>", function()
  smelt.transcript.loaded_text_expensive(function(text)
    smelt.clipboard.write(text)
    smelt.notify.info("copied transcript")
  end)
end)
```

To remove a binding programmatically use `smelt.keymap.unset(mode, chord)`. For
permanent removal, just delete the registration line.

`smelt.keymap.list()` returns every currently-bound chord. Useful when the user
asks "what's bound to X?".

### Change the accent color

Edit `~/.config/smelt/init.lua` and add (or update) a `smelt.theme.set` call for
the relevant highlight group:

```lua
-- In ~/.config/smelt/init.lua
smelt.theme.set("SmeltAccent", { fg = { ansi = 208 }, bold = true })
```

`smelt.theme.set(group, style)` overrides one highlight group; the override
persists across reloads because it lives in `init.lua`. Common group names:
`SmeltAccent` (primary accent), `Comment`, `SmeltDiffAddBg`,
`SmeltDiffDeleteBg`, `SmeltSlug` (task-slug pill), `SmeltSeparator` (prompt bars,
borders, and statusline separators). Use `smelt.theme.snapshot()` to dump every
currently-set group when the user needs to discover names.

If the user names a session-color preset (`"ember"`, `"coral"`, `"rose"`,
`"gold"`, `"ice"`, `"sky"`, `"blue"`, `"lavender"`, `"lilac"`, `"mint"`,
`"sage"`, `"silver"`), set the relevant highlight group in `init.lua` with
`smelt.theme.set(...)` so the preference survives a restart:

```lua
smelt.theme.set("SmeltAccent", { fg = { ansi = 208 } })
```

The runtime command `/color <preset>` is session-only and exists to distinguish
parallel sessions. Use it only when the user explicitly asks for a
current-session color.

### Write a full colorscheme

Drop a Lua file at `~/.config/smelt/lua/smelt/colorschemes/<name>.lua` that
returns a `ThemeSpec` table:

```lua
return {
  groups = {
    SmeltAccent      = { fg = { ansi = 208 }, bold = true },
    Comment          = { fg = { ansi = 244 } },
    SmeltDiffAddBg   = { bg = { ansi = 22 } },
    -- string-valued entries reference another group in this spec:
    SmeltProcess     = "SmeltAccent",
  },
}
```

Then load it: `smelt.theme.use("<name>")`. Read
`$XDG_DATA_HOME/smelt/builtins/lua/smelt/colorschemes/default.lua` for the
canonical worked example. Light/dark variants share the same file: gate by
`smelt.theme.is_light()` if needed.

### Add a /command

Drop a file at `~/.config/smelt/commands/<name>.lua` (or inline in `init.lua`):

```lua
smelt.cmd.register("greet", function(arg)
  smelt.notify.info("hello " .. (arg or "world"))
end, {
  desc = "say hello",
  args = "[name]",      -- shown in /command help
  busy = "run",         -- run (default), reject, queue_request, or queue_command
  startup_ok = false,   -- runnable before init is complete (default false)
  hidden = false,       -- hide from completer + help (default false)
})
```

The handler receives the trailing argument string (or `nil`). Errors are
surfaced as in-app notifications.

Worked example: read `$XDG_DATA_HOME/smelt/builtins/lua/smelt/commands/docs.lua`
to see how `/docs` uses `smelt.os.open_url`, `smelt.clipboard.write`, and
`smelt.notify.info` for a graceful clipboard fallback.

### Toggle/set a setting

Write the assignment into `~/.config/smelt/init.lua`. Assigning an unknown key
or wrong type raises at the call site.

```lua
-- In ~/.config/smelt/init.lua
smelt.settings.vim = true
smelt.settings.auto_compact = true
smelt.settings.compact_threshold = 0.65
smelt.settings.show_tps = true
```

The full table of settings (key, type, default, description) is in the
**Settings** section further down; use it to confirm a key exists and is spelled
correctly before writing.

`/settings` and `--set KEY=VALUE` exist as runtime/CLI overrides but do not
persist; always make the change in `init.lua` so it survives a restart.

### Customize the status line

The status line is a pure-Lua module that ships as `smelt.statusline`. Register
a named source via `M.add(name, handler)`. The retained renderer calls sources
when status signals or explicit plugin invalidation make their output dirty:

```lua
local sl = require("smelt.statusline")
sl.add("greeting", function()
  return { { text = "hi ", hl_group = "SmeltAccent" } }
end)
```

The handler returns a segment table or a list of them. Each segment is
`{ text, hl_group?, style?, priority?, align_right?, truncatable?, separated? }`.
`sl.add` and `sl.remove` invalidate automatically. If a source closes over
plugin-owned state that changes independently of a built-in signal, call
`sl.invalidate()` after changing it. The canonical worked example lives at
`$XDG_DATA_HOME/smelt/builtins/lua/smelt/statusline.lua`; its `core` source is
what draws the built-in segments (vim mode, agent mode, tps, task label, etc.).

### Pin startup defaults (model / mode / reasoning)

```lua
smelt.defaults.set({
  model = "openai/gpt-5.5",
  mode = "plan",
  reasoning_effort = "high",
})
```

CLI flags still win over this. Each field is optional.

### Register a provider

```lua
smelt.provider.register("openai", {
  type = "openai",
  api_base = "https://api.openai.com/v1",
  api_key_env = "OPENAI_API_KEY",
  models = { "gpt-5.5", { name = "gpt-5-mini", temperature = 0.7 } },
})
```

`type` is one of `openai-compatible`, `openai`, `codex`, `anthropic-compatible`,
`anthropic`, `copilot`. Re-registering the same name replaces the previous
entry.

### Register an MCP server

```lua
smelt.mcp.register("filesystem", {
  command = { "npx", "-y", "@modelcontextprotocol/server-filesystem", "/tmp" },
  env = { DEBUG = "true" },
  timeout = 30000,
  enabled = true,
})
```

MCP tool names get prefixed with the server name (`filesystem_read_file`) and
default to ask-permission.

### Register an agent tool

```lua
smelt.tools.register({
  name = "weather",
  description = "Look up the current weather for a city.",
  parameters = {
    type = "object",
    properties = {
      city = { type = "string", description = "City name" },
    },
    required = { "city" },
  },
  execute = function(args, ctx)
    local resp = smelt.http.get("https://wttr.in/" .. args.city .. "?format=3")
    return resp.body
  end,
  summary = function(args) return "weather: " .. args.city end,
  -- optional: approval_patterns, preflight, render, decide, override
})
```

Pass `override = true` to replace a built-in tool with the same name.

### Permission rules

```lua
smelt.permissions.extend({
  default = { patterns = { bash = { allow = { "git log *", "git status *" } } } },
  apply   = { patterns = { bash = { allow = { "git commit *", "git push *" } } } },
  yolo    = { patterns = { mcp  = { allow = { "*" } } } },
})
```

Modes: `default`, `plan`, `apply`, `yolo`. Tool kinds include `bash` and `mcp`.
For the full match-rule grammar, read
`$XDG_DATA_HOME/smelt/builtins/lua/smelt/_meta/permissions.lua` or locate
`smelt.permissions` in the capability map further down.

### Write a project-local skill

When the user wants the agent to know something only inside this project:

```
<project>/.smelt/skills/<name>/SKILL.md
```

With frontmatter:

```markdown
---
name: my-skill
description: One-line summary the agent reads to decide whether to load.
---

# Skill body in markdown

Any reference content the agent should consult on demand.
```

Then run `/trust` once after creating `.smelt/`. The agent will see the skill in
its `# Skills` listing and can load it via `load_skill`.

## Bundled plugins

<!-- PLUGINS_BEGIN -->
<!-- This region is auto-generated by `cargo xtask gen-lua-docs`. Do not edit by hand. -->

Bundled with smelt. Drop a file under `~/.config/smelt/plugins/` to add your own.

### Autoloaded

Loaded on every launch unless opted out via `smelt.builtins.disable({ plugins = { "<name>" } })` in `early.lua`.

| Plugin | Summary |
| --- | --- |
| `smelt.plugins.banner` | Empty-state logo decoration + shutdown logo/resume-hint banner. |
| `smelt.plugins.compact` | Compacts older history while preserving a live recent suffix. |
| `smelt.plugins.debug_panel` | F3 debug panel. |
| `smelt.plugins.diff` | /diff: continuous, virtualized Git diff viewer. |
| `smelt.plugins.esc_chord` | Esc-Esc: cancel in-flight foreground/background work (`smelt.work.busy` tokens, e.g. /compact), or rewind to the previous turn when idle. |
| `smelt.plugins.goal` | Goal lifecycle plugin. |
| `smelt.plugins.perf_panel` | F12 perf panel. |
| `smelt.plugins.plan_mode` | Plan-mode plugin: registers the `plan` mode and `present_plan` tool. |
| `smelt.plugins.predict` | Input prediction plugin. |
| `smelt.plugins.process_control` | Ctrl-G: stop following a foreground bash job while it keeps running. |
| `smelt.plugins.scroll_pills` | Clickable scroll-pill overlays navigate the transcript. |
| `smelt.plugins.terminal_title` | Keeps the terminal window/tab title in sync with smelt. |
| `smelt.plugins.title` | Session title plugin. |
| `smelt.plugins.turn_notifications` | Optional terminal desktop notification when an agent turn ends. |
| `smelt.plugins.upgrade` | Autoupgrade plugin. |
| `smelt.plugins.version` | /version - surface the running smelt build identity as a notification. |

### Opt-in

Shipped but not autoloaded. Add `require("smelt.plugins.<name>")` to `~/.config/smelt/init.lua` to enable.

| Plugin | Summary |
| --- | --- |
| `smelt.plugins.inspect` | Optional plugin: `/inspect` opens a local web UI for browsing sessions, their history, and provider request/response audit data. |
| `smelt.plugins.lsp` | Optional LSP tool facade for agent code navigation. |
| `smelt.plugins.which_key` | Which-key style popup for pending global Lua keymaps. |

<!-- PLUGINS_END -->

## Settings

<!-- SETTINGS_BEGIN -->
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

<!-- SETTINGS_END -->

## Tier rule: Host vs UiHost

Two tiers exist:

- **Host**: works everywhere, including headless (`smelt -p '...'`).
- **UiHost**: needs the TUI; calling these from a headless run raises.

If you're writing a plugin that might run headless (CI scripts, batch prompts),
avoid UiHost calls or gate them on `smelt.frontend.is_interactive()`. The
capability map below groups namespaces by tier.

## Gotchas

- **`Reg`-returning registrations.** `smelt.keymap.set`, `smelt.cmd.register`,
  `smelt.tools.register`, etc. return a `Reg` table with `:remove()`. Hold onto
  it if you want to unregister later; otherwise reload simply re-runs `init.lua`
  and re-registers.
- **Unknown setting/chord/mode keys raise.** Better to fail loud than silently
  no-op. Check the recipes above for valid values.
- **Hot reload wipes the Lua context.** Don't rely on module-local state
  surviving a `/reload`. Use `smelt.state` / `smelt.signal` for state that needs
  to persist.
- **`.smelt/` requires trust.** After creating or editing project config, the
  user must run `/trust`. The next reload after an edit fails until they do.
- **Don't hand-edit `_meta/` stubs or `docs/docs/reference/api/`**. Those are
  auto-generated by `cargo xtask gen-lua-docs` from the Rust side. The same is
  true for the capability-map region of this skill.
- **The builtins dir is read-only.** Edits to `$XDG_DATA_HOME/smelt/builtins/`
  are wiped on the next smelt upgrade. Make changes under `~/.config/smelt/` or
  `./.smelt/`.
- **Preserve existing config when editing `init.lua`.** Always `Read` the file
  first, then `Edit` the specific lines you're changing. Never overwrite the
  whole file from a template, you'll wipe the user's providers, MCP servers,
  permissions, and custom plugins. If `init.lua` doesn't exist, create a minimal
  one with only the lines the request needs.

## Lua capability map

This compact map lists public namespaces by runtime tier and purpose. For exact
functions, signatures, parameter shapes, and return records, open the matching
`_meta/<stem>.lua` stub under the builtins directory
(`$XDG_DATA_HOME/smelt/builtins/lua/smelt/_meta/`).

<!-- API_INDEX_BEGIN -->
<!-- This region is auto-generated by `cargo xtask gen-lua-docs`. Do not edit by hand. -->

Use the generated Lua API reference for complete signatures and types.

### Host tier

- `smelt.agent` - Agent-facing prompt customization for Lua plugins.
- `smelt.auth` - Authenticated provider helpers.
- `smelt.build` - Compile-time build identity and version metadata for plugins.
- `smelt.builtins` - Opt out of bundled `smelt.<dotted>` modules.
- `smelt.cli` - Declare and read CLI flags from Lua.
- `smelt.clipboard` - Read and write the system clipboard.
- `smelt.defaults` - Startup fallbacks for new sessions.
- `smelt.diff` - Compact unified-patch indexing, metadata and expandable context folds.
- `smelt.document` - Indexed read-only documents shared by native data providers and Lua-created windows.
- `smelt.events` - Occurrence-oriented subscriptions over event-shaped signals such as `turn_start`, `tool_start`, and `turn_complete`.
- `smelt.files` - Workspace file search.
- `smelt.frontend` - Query which frontend is active (TUI vs headless).
- `smelt.fs` - Sync filesystem primitives.
- `smelt.fs.file_state` - Cached file-state tracker used by tools to detect external modifications between reads and writes.
- `smelt.fuzzy` - Fuzzy-match scoring backed by neo_frizbee (SIMD Smith-Waterman).
- `smelt.git` - Asynchronous Git snapshots and explicit file-level staging.
- `smelt.grep` - Ripgrep wrapper for searching files.
- `smelt.html` - HTML parsing: title extraction, link scraping, to_text, to_markdown, DDG results.
- `smelt.http` - Asynchronous HTTP get/post.
- `smelt.http.cache` - Runtime-owned HTTP response cache.
- `smelt.image` - Image file detection and base64 data-URL loading.
- `smelt.json` - Encode/decode JSON for Lua plugins.
- `smelt.keymap` - Register chord→callback bindings and inspect the layered help index.
- `smelt.layout` - Declarative, width-independent content layout primitives for transcript/tool display.
- `smelt.lifecycle` - Host-phase hooks keyed by event name.
- `smelt.log` - Structured JSONL log entries written to the engine log file.
- `smelt.lsp` - Generic stdio Language Server Protocol client.
- `smelt.mcp` - Config-time MCP server registration.
- `smelt.messages` - Persistent message log with full bodies and tracebacks.
- `smelt.notebook` - Parse and read notebook cells, apply edits, and compute preview data for the edit_notebook tool.
- `smelt.os` - Environment and system primitives: getenv, setenv, platform, cwd, pid, etc.
- `smelt.parse` - Pure parsers: frontmatter extraction from markdown documents.
- `smelt.path` - Pure path arithmetic: normalize, join, relative, expand, display, etc.
- `smelt.perf` - Lightweight scope timers that feed `smelt.metrics.perf_snapshot`.
- `smelt.process` - Run subprocesses and manage contained shell jobs.
- `smelt.provider` - List built-in model providers and register custom ones.
- `smelt.reg` - Helpers for constructing `Reg` handles.
- `smelt.remember` - Per-key opt-in to last-used recall on launch.
- `smelt.shell` - Shell command splitting and interactive/background-operator validators.
- `smelt.signal` - Named reactive values.
- `smelt.skills` - List and load skill content from the SkillLoader populated at startup.
- `smelt.state` - Per-plugin state.
- `smelt.task` - Yield-then-resume coroutine bridge: alloc and resume external tasks.
- `smelt.text` - Terminal-cell measurement and pure text formatting.
- `smelt.tick` - Reload-safe periodic work.
- `smelt.time` - Wall-clock time parsing and formatting.
- `smelt.timer` - One-shot and recurring timer callbacks.
- `smelt.tools` - Register, unregister, and resolve plugin tools for the engine.
- `smelt.transcript.defaults` - Bundled default transcript entry points for rendering semantic blocks and raw tool output.
- `smelt.transcript.groups` - Declarative transcript display grouping.
- `smelt.trust` - Query and mutate the per-project content trust store.

### UiHost tier

- `smelt.buf` - Buffer handle constructor.
- `smelt.config` - Resolved application configuration introspection.
- `smelt.confirm` - Confirm dialog primitives - preview dispatch, back-tab cycling, and choice resolution.
- `smelt.dialog` - Root-docked modal dialog primitives.
- `smelt.history` - Prompt history entries and search.
- `smelt.input` - Single-line input handle constructor.
- `smelt.inspect` - Local session introspection web UI.
- `smelt.list` - Picker-style virtual list widget.
- `smelt.metrics` - Metrics ledger access and live perf instrumentation.
- `smelt.metrics.perf` - Perf instrumentation toggle, clear, and snapshot.
- `smelt.overlay` - Overlay handle constructor.
- `smelt.paint` - Register Lua callbacks against custom paint regions.
- `smelt.picker` - Picker facade.
- `smelt.render` - Paint text / markdown / syntax-highlighted code / split diffs into a `Buf`.
- `smelt.search` - Search controls for the active UI session.
- `smelt.session` - Current session metadata, turn list, message snapshots, rewind, and persisted session management.
- `smelt.session.messages` - Session messages.
- `smelt.session.slug` - Session slug.
- `smelt.session.title` - Session title.
- `smelt.settings` - Metatable-backed proxy table for preferences.
- `smelt.terminal` - Terminal integration helpers.
- `smelt.ui` - Screen-composition primitives: main layout composer and per-window renderer registration.
- `smelt.ui.layout` - Composable layout-tree primitives for the retained main TUI layout.
- `smelt.vim` - Read and write the vim mode of the focused vim-enabled surface.
- `smelt.win` - Window handle constructor.
- `smelt.work` - Push background work-state tokens.

### Mixed tier

- `smelt` - Root smelt namespace.
- `smelt.cmd` - Register and list slash commands.
- `smelt.engine` - LLM engine control - cancel, ask, inherited ask, submit commands, and request tool approval.
- `smelt.mode` - Agent-mode selector.
- `smelt.model` - Model selector.
- `smelt.notify` - Status-area toasts.
- `smelt.permissions` - Inspect, revoke, and extend permission policy state, or synchronize live session and persisted grants.
- `smelt.prompt` - The main editable input surface: win handle, text get/set, and cursor control.
- `smelt.reasoning` - Reasoning-effort selector.
- `smelt.theme` - List bundled colorschemes, or apply, read, and override the active one.
- `smelt.transcript` - Transcript display policy and rendered transcript inspection.

<!-- API_INDEX_END -->
