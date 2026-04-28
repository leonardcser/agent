# Inventory — current codebase state and target fate

Every source file in the repo, mapped against the refactor plan. Updated as
phases land. **This is the master list — when you're about to delete or move
something, check here first.**

Maintenance rules:

- When a phase lands, update the **Status** column for every row it touched
  (mark `done`, note the actual destination).
- When a fate decision changes mid-phase, update the row immediately and log the
  decision in the current `P<n>.md`.
- A new file added to the codebase = a new row here, in the same commit.
- "Unclear" rows demand a decision before the relevant phase begins.

Legend for **Fate**:

- `kept` — survives roughly as-is (may have internal edits).
- `renamed` — same logic, new name/path.
- `merged` — folds into another file/type.
- `restructured` — splits responsibilities across multiple new locations.
- `deleted` — gone entirely.
- `moved-to-lua` — responsibility moves to `runtime/lua/smelt/`.
- `moved-to-capability` — promoted to `tui::<name>` capability module.
- `unclear` — fate not yet decided; needs explicit decision before phase.

Legend for **Status**: `pending` (not yet touched), `in-progress`, `done`.

---

## `crates/ui/src/`

| File                  | LOC  | Purpose                                       | Fate         | Phase     | Status  | Notes                                                                                               |
| --------------------- | ---- | --------------------------------------------- | ------------ | --------- | ------- | --------------------------------------------------------------------------------------------------- |
| `buffer.rs`           | 583  | Lines, spans, marks, virtual text             | restructured | P1.a      | pending | Gains namespaces / extmarks / `attach(spec)` / `yank_text_for_range` / soft-wrap state              |
| `buffer_list.rs`      | 480  | List-of-buffers widget                        | deleted      | P0        | pending | Reduce to Window-over-Buffer recipe in P4 if a consumer survives                                    |
| `buffer_view.rs`      | 471  | Renderer wrapper around Buffer                | deleted      | P0        | pending | Window reads Buffer directly post-P1                                                                |
| `callback.rs`         | 367  | Window callbacks + WinEvent routing           | kept         | P2        | pending | Drain pipeline for Lua callbacks; pattern preserved                                                 |
| `clipboard.rs`        | 30   | Clipboard sink interface                      | merged       | P2        | pending | Folds into `tui::Clipboard` subsystem                                                               |
| `cmdline.rs`          | 541  | Cmdline input widget                          | moved-to-lua | P4        | pending | Becomes `widgets/cmdline.lua` recipe over a Window                                                  |
| `component.rs`        | 116  | `Component` trait + `WidgetEvent`             | deleted      | P0        | pending | Window is the only interactive unit                                                                 |
| `compositor.rs`       | 614  | Layer manager + z-order + dispatch            | restructured | P1.g      | pending | Folds into `Ui` facade with splits/overlays                                                         |
| `dialog.rs`           | 1890 | Multi-panel dialog with PanelWidget           | deleted      | P0/P1     | pending | Replaced by Overlay + LayoutTree + N Windows                                                        |
| `edit_buffer.rs`      | 323  | EditBuffer trait abstraction                  | merged       | P1.a      | pending | Rolled into Buffer (per-buffer edit history)                                                        |
| `flush.rs`            | 182  | Diff → SGR writer                             | kept         | P1        | pending | Grid diff persists as primitive                                                                     |
| `grid.rs`             | 419  | Terminal frame (cells + style)                | kept         | P1        | pending | Primitive                                                                                           |
| `id.rs`               | 39   | `BufId`, `WinId` newtypes                     | kept         | P1        | pending | Add `OverlayId`                                                                                     |
| `kill_ring.rs`        | 241  | Kill ring                                     | merged       | P2        | pending | Into `Clipboard` subsystem                                                                          |
| `layout.rs`           | 413  | `Placement(6)` + Constraint + Border + Anchor | restructured | P1.b/P1.c | pending | Becomes `LayoutTree` + `Overlay` + `Anchor` enum (Screen/Cursor/Win)                                |
| `lib.rs`              | 1419 | `Ui` facade                                   | restructured | P1.g      | pending | Rewritten with splits/overlays/focus/hit_test API                                                   |
| `notification.rs`     | 316  | Toast widget                                  | moved-to-lua | P4        | pending | `widgets/notification.lua` over Overlay                                                             |
| `option_list.rs`      | 545  | Options widget                                | moved-to-lua | P4        | pending | `widgets/options.lua` recipe                                                                        |
| `picker.rs`           | 501  | Picker widget                                 | moved-to-lua | P4        | pending | `widgets/picker.lua` recipe                                                                         |
| `status_bar.rs`       | 174  | Statusline widget                             | moved-to-lua | P4        | pending | `widgets/statusline.lua` + Cells                                                                    |
| `style.rs`            | 18   | Style helpers                                 | kept         | P1        | pending | Theme registry references hl ids; raw style stays                                                   |
| `text.rs`             | 242  | Text utilities (word boundaries)              | kept         | P1        | pending | Utility                                                                                             |
| `text_input.rs`       | 522  | Editable text widget                          | moved-to-lua | P4        | pending | `widgets/input.lua` recipe                                                                          |
| `undo.rs`             | 66   | Undo/redo snapshots                           | merged       | P1.a      | pending | Into Buffer per-buffer edit history                                                                 |
| `vim/mod.rs`          | 3085 | Vim state machine                             | deleted      | P1.d      | pending | VimMode → App; registers/dot-repeat/undo → Buffer; cursor/selection → Window; kill ring → Clipboard |
| `vim/motions.rs`      | 331  | Motion operators                              | deleted      | P1.d      | pending | With state machine                                                                                  |
| `vim/text_objects.rs` | 159  | Text objects                                  | deleted      | P1.d      | pending | With state machine                                                                                  |
| `window.rs`           | 1218 | Window viewport                               | restructured | P1.d      | pending | Single interactive unit; `handle(event, ctx, host) -> Status`                                       |
| `window_cursor.rs`    | 90   | Cursor anchor state                           | merged       | P1.d      | pending | Into Window                                                                                         |

## `crates/tui/src/`

| File                                      | LOC  | Purpose                                                     | Fate                | Phase     | Status  | Notes                                                                                                       |
| ----------------------------------------- | ---- | ----------------------------------------------------------- | ------------------- | --------- | ------- | ----------------------------------------------------------------------------------------------------------- |
| `alloc.rs`                                | 66   | Allocation tracking                                         | kept                | none      | pending | Utility                                                                                                     |
| `api.rs`                                  | 123  | Public crate API surface                                    | kept                | P3        | pending | May fold into Lua bindings                                                                                  |
| `app/mod.rs`                              | 1884 | God-struct App + event loop                                 | restructured        | P2.a/P2.b | pending | Carved into AppConfig/WellKnown/Session/Confirms/Clipboard/Timers/Cells/LuaRuntime/ToolRuntime/EngineBridge |
| `app/agent.rs`                            | 1427 | Multi-agent lifecycle, message relay                        | restructured        | P5.b/P5.c | pending | Drops to a Lua-side registry (`plugins/multi_agent.lua`) over `tui::subprocess`; tracking state moves to Lua cells |
| `app/cmdline.rs`                          | 209  | Cmdline rendering glue                                      | moved-to-lua        | P4        | pending | `widgets/cmdline.lua`                                                                                       |
| `app/commands.rs`                         | 529  | `RUST_COMMANDS` table                                       | moved-to-lua        | P4.e      | pending | `runtime/lua/smelt/commands.lua`                                                                            |
| `app/content_keys.rs`                     | 238  | Content pane keymap                                         | restructured        | P4        | pending | Becomes Window keymap recipe                                                                                |
| `app/dialogs/confirm.rs`                  | 157  | Confirm dialog                                              | moved-to-lua        | P4.d      | pending | `dialogs/confirm.lua`                                                                                       |
| `app/dialogs/confirm_preview.rs`          | 298  | Diff preview pane                                           | moved-to-lua        | P4        | pending | `diff.lua` extmark presentation                                                                             |
| `app/dialogs/mod.rs`                      | 7    | Module exports                                              | deleted             | P4.d      | pending | Empty after migrations                                                                                      |
| `app/dialogs/text_modal.rs`               | 45   | Text input modal                                            | moved-to-lua        | P4        | pending | Generic input dialog                                                                                        |
| `app/events.rs`                           | 861  | Event dispatch (key/mouse/engine)                           | restructured        | P2.b/P2.e | pending | Single `select!` loop                                                                                       |
| `app/headless.rs`                         | 193  | Headless coordinator                                        | kept                | P2        | pending | Carve-up must keep this working — first-class consumer of subsystems                                        |
| `app/history.rs`                          | 652  | Rewind/fork/compact state                                   | merged              | P2        | pending | Into Session subsystem                                                                                      |
| `app/lua_bridge.rs`                       | 152  | Lua FFI dispatch glue                                       | restructured        | P3        | pending | Per-namespace binding files                                                                                 |
| `app/lua_handlers.rs`                     | 131  | Lua async handlers                                          | kept                | P2/P3     | pending | Callback queue drain                                                                                        |
| `app/mouse.rs`                            | 691  | Mouse routing + drag + scrollbar                            | restructured        | P1/P2     | pending | Hit-test moves to Ui; dispatch via Host                                                                     |
| `app/pane_focus.rs`                       | 114  | Prompt vs Content focus cycling                             | restructured        | P2        | pending | Replaced by Ui::focus_next/focus_prev                                                                       |
| `app/render_loop.rs`                      | 447  | Per-frame render                                            | kept                | P2.e      | pending | Hooked into single select! loop; diff-based                                                                 |
| `app/status_bar.rs`                       | 296  | Statusline rendering                                        | moved-to-lua        | P4.c      | pending | Cells-driven spec                                                                                           |
| `app/transcript.rs`                       | 871  | Transcript line/block management                            | restructured        | P2/P4     | pending | Buffer + extmarks + `transcript.lua`                                                                        |
| `app/transcript_cache.rs`                 | 224  | Block layout cache                                          | deleted             | P4.b      | pending | Extmarks replace IR cache                                                                                   |
| `app/transcript_model.rs`                 | 968  | Block / ViewState / ToolState                               | restructured        | P2/P4     | pending | Splits into Session.history + on_block extmark population                                                   |
| `app/transcript_present/mod.rs`           | 1153 | Transcript render orchestrator                              | moved-to-lua        | P4.b      | pending | `transcript.lua` controller                                                                                 |
| `app/transcript_present/agent.rs`         | 118  | Agent message presentation                                  | deleted             | P5.c      | pending | Sub-agent replies are tool calls; transcript renders them as ordinary tool blocks. No dedicated widget.     |
| `app/transcript_present/markdown.rs`      | 351  | Markdown presentation                                       | moved-to-lua        | P4.b      | pending | `transcript.lua` on_block + `tui::parse`                                                                    |
| `app/transcript_present/tools.rs`         | 477  | Tool result formatting                                      | moved-to-lua        | P4.b      | pending | Lua extmarks                                                                                                |
| `app/transcript_present/tool_previews.rs` | 146  | Tool preview panels                                         | moved-to-lua        | P4.b      | pending | Lua extmarks                                                                                                |
| `app/working.rs`                          | 366  | Spinner + phase tracking                                    | merged              | P2.c/P2.d | pending | `spinner_frame` cell + Timers + EngineBridge fires                                                          |
| `attachment.rs`                           | 237  | Prompt attachments                                          | restructured        | P1/P4     | pending | Extmarks in `attachments` namespace + Lua input widget                                                      |
| `builtin_commands.rs`                     | 121  | Builtin slash commands                                      | moved-to-lua        | P4.e      | pending | `commands.lua`                                                                                              |
| `completer/command.rs`                    | 113  | Command completion                                          | restructured        | P4        | pending | Lua input widget helper                                                                                     |
| `completer/file.rs`                       | 151  | File path completion                                        | restructured        | P4        | pending | Lua input widget helper                                                                                     |
| `completer/history.rs`                    | 158  | History completion                                          | restructured        | P4        | pending | Lua input widget helper                                                                                     |
| `completer/mod.rs`                        | 192  | Completer state machine                                     | restructured        | P1.d/P4   | pending | Decomposes: ghost-text extmark + Overlay picker + prompt keymap recipe                                      |
| `completer/score.rs`                      | 62   | Fuzzy scorer                                                | moved-to-capability | P3        | pending | Into `tui::fuzzy`                                                                                           |
| `config.rs`                               | 636  | Config loader                                               | merged              | P2.a      | pending | Into AppConfig                                                                                              |
| `content/context.rs`                      | 24   | Content context struct                                      | merged              | P1        | pending | Into Window render path                                                                                     |
| `content/display.rs`                      | 210  | Display rendering utility                                   | restructured        | P1        | pending | Window render pipeline                                                                                      |
| `content/highlight/diff.rs`               | 497  | Diff highlight logic                                        | restructured        | P3/P4     | pending | LCS to `tui::parse`; presentation to `diff.lua`                                                             |
| `content/highlight/inline.rs`             | 1134 | Markdown / inline code parsing                              | restructured        | P3/P4     | pending | Parse to `tui::parse`; extmark population in Lua                                                            |
| `content/highlight/mod.rs`                | 48   | Highlight module exports                                    | deleted             | P4        | pending | Replaced by Buffer.attach + extmarks                                                                        |
| `content/highlight/syntax.rs`             | 303  | syntect adapter                                             | moved-to-capability | P3        | pending | Into `tui::parse`                                                                                           |
| `content/highlight/util.rs`               | 196  | Highlight utilities                                         | restructured        | P3/P4     | pending | Some to `tui::parse`, some to Lua                                                                           |
| `content/layout.rs`                       | 135  | Content layout computation                                  | restructured        | P1.b      | pending | Into LayoutTree resolution                                                                                  |
| `content/layout_out.rs`                   | 281  | Layout output                                               | restructured        | P1.b      | pending | Into Ui render path                                                                                         |
| `content/mod.rs`                          | 144  | Module exports                                              | deleted             | P4        | pending | Most submodules go away                                                                                     |
| `content/prompt_data.rs`                  | 971  | Prompt content (formatting, completion)                     | restructured        | P2/P4     | pending | Prompt Buffer + Window + widget recipe                                                                      |
| `content/prompt_wrap.rs`                  | 233  | Prompt wrap state                                           | deleted             | P1.a      | pending | Wrap state lives on Buffer                                                                                  |
| `content/scrollbar.rs`                    | 143  | Scrollbar rendering                                         | restructured        | P1        | pending | Window render output + HitTarget routing                                                                    |
| `content/selection.rs`                    | 518  | Selection tracking                                          | restructured        | P1/P2     | pending | Window selection + theme.get("Visual")                                                                      |
| `content/status.rs`                       | 208  | Statusline content model                                    | moved-to-lua        | P4.c      | pending | Spec segments + cell bindings                                                                               |
| `content/stream_parser.rs`                | 785  | Streaming markdown/diff parser                              | restructured        | P3        | pending | Into `tui::parse`, called from Buffer.attach                                                                |
| `content/to_buffer.rs`                    | 248  | Content → terminal buffer                                   | restructured        | P1        | pending | Window render to Grid                                                                                       |
| `content/transcript.rs`                   | 1115 | Transcript display                                          | moved-to-lua        | P4.b      | pending | `transcript.lua`                                                                                            |
| `content/transcript_buf.rs`               | 85   | Transcript Buffer projection                                | deleted             | P4.b      | pending | Replaced by Buffer + extmarks                                                                               |
| `content/viewport.rs`                     | 265  | Viewport scroll state                                       | merged              | P1.d      | pending | Into Window scroll state                                                                                    |
| `content/window_view.rs`                  | 352  | Window-specific content view                                | merged              | P1.d      | pending | Into Window render                                                                                          |
| `custom_commands.rs`                      | 322  | User-defined slash commands                                 | moved-to-lua        | P4.e      | pending | Plugin commands.lua                                                                                         |
| `format.rs`                               | 276  | Text formatting helpers                                     | kept                | none      | pending | Utility                                                                                                     |
| `fuzzy.rs`                                | 168  | Fuzzy matching                                              | moved-to-capability | P3        | pending | `tui::fuzzy` capability or stays here                                                                       |
| `input/buffer.rs`                         | 403  | Prompt edit buffer                                          | restructured        | P1.a/P4   | pending | Becomes prompt Buffer + Window                                                                              |
| `input/completer_bridge.rs`               | 281  | Completer integration                                       | restructured        | P1.d/P4   | pending | Ghost-text extmark + picker Overlay                                                                         |
| `input/history.rs`                        | 88   | Input history                                               | merged              | P2        | pending | Into Session.history                                                                                        |
| `input/mod.rs`                            | 2077 | Input subsystem (god-struct)                                | restructured        | P1/P2/P4  | pending | Carved into prompt Window + completer + attachments                                                         |
| `input/vim_bridge.rs`                     | 101  | Vim mode in input                                           | restructured        | P2        | pending | App.vim_mode + Window keymap recipe                                                                         |
| `instructions.rs`                         | 61   | System instructions model                                   | kept                | P2        | pending | Config data                                                                                                 |
| `keymap.rs`                               | 860  | Global keymap registry                                      | restructured        | P3/P4     | pending | Recipes are Lua; registry holds (mode, key) → recipe id                                                     |
| `lib.rs`                                  | 75   | Crate root                                                  | kept                | none      | pending | Re-exports update as modules move                                                                           |
| `lua/api/mod.rs`                          | 293  | Lua API module organization                                 | restructured        | P3.b      | pending | Becomes per-namespace dir                                                                                   |
| `lua/api/dispatch.rs`                     | 433  | Lua FFI dispatch                                            | restructured        | P3.b      | pending | Splits to per-namespace files                                                                               |
| `lua/api/state.rs`                        | 677  | Lua state-getter bindings                                   | restructured        | P3.b      | pending | Splits to per-namespace files                                                                               |
| `lua/api/widgets.rs`                      | 571  | Lua widget creation API                                     | restructured        | P3.b      | pending | Splits into ui.rs / win.rs / buf.rs                                                                         |
| `lua/app_ref.rs`                          | 80   | TLS app pointer                                             | kept                | P2.b      | pending | Exposes Host surface, not raw &mut App                                                                      |
| `lua/confirm_ops.rs`                      | 195  | Lua confirm bindings                                        | moved-to-lua        | P4.d      | pending | Folds into `dialogs/confirm.lua`                                                                            |
| `lua/mod.rs`                              | 1909 | Lua runtime + autocmd dispatch                              | restructured        | P2.a/P3   | pending | Becomes LuaRuntime subsystem; autocmd registry folded into Cells (subscriptions = autocmds; `smelt.au.*` = sugar) |
| `lua/render_ops.rs`                       | 170  | Lua render helpers (smelt.diff/syntax/bash/notebook.render) | restructured        | P3.b      | pending | Splits into per-capability binding files                                                                    |
| `lua/task.rs`                             | 471  | Single coroutine task wrapper                               | kept                | P2        | pending | Coroutine task system                                                                                       |
| `lua/tasks.rs`                            | 576  | Coroutine task registry + plugin tool env                   | restructured        | P2/P5     | pending | Becomes ToolRuntime impl after P5                                                                           |
| `lua/ui_ops.rs`                           | 421  | Lua UI ops (overlay open/close)                             | restructured        | P3.b      | pending | Splits into ui.rs / win.rs                                                                                  |
| `metrics.rs`                              | 571  | Frame timing                                                | kept                | P7        | pending | Review for dead code at end                                                                                 |
| `perf.rs`                                 | 286  | Profiling guards                                            | kept                | P7        | pending | Review at end                                                                                               |
| `persist.rs`                              | 153  | Session save/load                                           | merged              | P2        | pending | Into Session subsystem                                                                                      |
| `prompt_sections.rs`                      | 206  | System prompt composition                                   | kept                | P2        | pending | Engine glue                                                                                                 |
| `session.rs`                              | 643  | Session struct (metadata, costs)                            | merged              | P2.a      | pending | Into Session subsystem                                                                                      |
| `sleep_inhibit.rs`                        | 140  | OS sleep suppression                                        | kept                | P2        | pending | Subsystem or utility                                                                                        |
| `state.rs`                                | 340  | App state enums + ResolvedSettings                          | restructured        | P2.a      | pending | Splits across AppConfig/Session                                                                             |
| `theme.rs`                                | 394  | Theme constants module                                      | deleted             | P0        | pending | Replaced by Theme registry in `ui` (tracked task `20260426-083607`)                                         |
| `utils.rs`                                | 87   | General utilities                                           | kept                | none      | pending | Utility                                                                                                     |
| `vim/mod.rs`                              | 1    | Vim module stub                                             | deleted             | P1.d      | pending | Trivial; with state machine                                                                                 |
| `window.rs`                               | 35   | Window wrapper (gutters, follow_tail)                       | merged              | P1.d      | pending | Into ui::Window                                                                                             |
| `workspace_permissions.rs`                | 202  | Workspace approval rules                                    | moved-to-capability | P3.a      | pending | Folds into `tui::permissions::store` (workspace JSON store); Lua tool hooks call via FFI                    |

## `crates/engine/src/`

| File                           | LOC  | Purpose                        | Fate         | Phase | Status  | Notes                                                           |
| ------------------------------ | ---- | ------------------------------ | ------------ | ----- | ------- | --------------------------------------------------------------- |
| `agent.rs`                     | 2129 | Multi-agent loop, turn driver  | restructured | P5    | pending | Drops `permissions: Permissions` field + `decide()` call; drops the multi-agent loop branch (~400 LOC); calls `ToolDispatcher::evaluate_hooks` only. Ends as single-agent only. |
| `auth.rs`                      | 133  | Auth config + token storage    | kept         | none  | pending | Provider infra                                                  |
| `cancel.rs`                    | 53   | Cancellation token             | kept         | P6    | pending | Used by cooperative cancel                                      |
| `compact.rs`                   | 553  | Context compaction             | kept         | none  | pending | LLM-side context reduction                                      |
| `config.rs`                    | 27   | Top-level engine config        | kept         | none  | pending |                                                                 |
| `config_file.rs`               | 122  | Config TOML loader             | kept         | none  | pending |                                                                 |
| `image.rs`                     | 105  | Image embedding                | kept         | none  | pending | Provider feature                                                |
| `lib.rs`                       | 440  | Engine exports + EngineHandle  | restructured | P5.a/P5.c  | pending | Tool trait slimmed; ToolDispatcher introduced; drop `EngineConfig.interactive`, `permissions`, `runtime_approvals` fields; `EngineHandle` becomes channels-only |
| `log.rs`                       | 120  | Trace logging                  | kept         | none  | pending |                                                                 |
| `mcp/mod.rs`                   | 286  | MCP manager                    | kept         | none  | pending | Multi-agent capability                                          |
| `mcp/tool_adapter.rs`          | 71   | MCP tool wrapper               | kept         | P5    | pending | Schema layer survives                                           |
| `paths.rs`                     | 151  | Workspace/config dir discovery | kept         | none  | pending |                                                                 |
| `permissions/approvals.rs`     | 228  | Runtime approval tracking      | moved-to-capability | P5.c  | pending | → `tui::permissions::approvals` (RuntimeApprovals queried by Lua hooks) |
| `permissions/bash.rs`          | 515  | Bash subcommand parsing        | moved-to-capability | P5.c  | pending | → `tui::permissions::bash` (FFI for `bash.lua` hook)            |
| `permissions/mod.rs`           | 298  | Permissions aggregate + decide | deleted             | P5.c  | pending | `Permissions` aggregate + per-mode `decide()` go away; Lua hooks compose pieces |
| `permissions/rules.rs`         | 308  | Rule matching                  | moved-to-capability | P5.c  | pending | → `tui::permissions::rules` (glob compile + ruleset check)      |
| `permissions/tests.rs`         | 1617 | Permission unit tests          | moved-to-capability | P5.c  | pending | Move with the code to `tui::permissions::tests`                 |
| `permissions/workspace.rs`     | 98   | Path extraction + boundary     | moved-to-capability | P5.c  | pending | → `tui::permissions::workspace` (path extract + workspace check) |
| `pricing.rs`                   | 204  | Token cost calc                | kept         | none  | pending | Session/budget                                                  |
| `provider/anthropic.rs`        | 291  | Anthropic API                  | kept         | none  | pending |                                                                 |
| `provider/auth_storage.rs`     | 76   | Credential storage             | kept         | none  | pending |                                                                 |
| `provider/chat_completions.rs` | 215  | Streaming abstraction          | kept         | none  | pending |                                                                 |
| `provider/codex.rs`            | 701  | Codex provider                 | kept         | none  | pending |                                                                 |
| `provider/copilot.rs`          | 654  | Copilot provider               | kept         | none  | pending |                                                                 |
| `provider/extract.rs`          | 328  | JSON extraction                | kept         | none  | pending |                                                                 |
| `provider/mod.rs`              | 1135 | Provider trait + dispatch      | kept         | none  | pending | Core LLM boundary                                               |
| `provider/openai.rs`           | 351  | OpenAI provider                | kept         | none  | pending |                                                                 |
| `provider/sse.rs`              | 48   | SSE parsing                    | kept         | none  | pending |                                                                 |
| `redact.rs`                    | 922  | Content redaction              | kept         | none  | pending | Privacy/logging                                                 |
| `registry.rs`                  | 262  | Multi-agent on-disk registry (`RegistryEntry`, agent PIDs/sockets) | moved-to-capability | P5.c  | pending | → `tui::subprocess::registry` (the agent-tracking JSON file Lua tools maintain) |
| `skills.rs`                    | 213  | Skill loader                   | kept         | P5    | pending | `load_skill` tool wraps this                                    |
| `socket.rs`                    | 345  | Inter-agent IPC sockets        | moved-to-capability | P3.a/P5.c | pending | → `tui::subprocess::socket` (wire layer for sub-smelt subprocess type)         |
| `tools/background.rs`          | 228  | Background process registry    | moved-to-capability | P3.a  | pending | → `tui::process` (registry + spawn/group/kill); Lua bash tool registers with it |
| `tools/bash.rs`                | 355  | Bash tool                      | moved-to-lua | P5.b  | pending | `tools/bash.lua` composes `tui::process`                        |
| `tools/edit_file.rs`           | 233  | Edit tool                      | moved-to-lua | P5.b  | pending | `tools/edit_file.lua` composes `tui::fs`                        |
| `tools/file_state.rs`          | 340  | File metadata tracking         | moved-to-capability | P3.a  | pending | → `tui::fs::file_state` (mtime tracking for edit_file race detection)            |
| `tools/glob.rs`                | 101  | Glob tool                      | moved-to-lua | P5.b  | pending | `tools/glob.lua`                                                |
| `tools/grep.rs`                | 262  | Grep tool                      | moved-to-lua | P5.b  | pending | `tools/grep.lua` over `tui::grep`                               |
| `tools/list_agents.rs`         | 90   | List agents tool               | moved-to-lua | P5.b  | pending | `tools/list_agents.lua`                                         |
| `tools/load_skill.rs`          | 53   | Load skill tool                | moved-to-lua | P5.b  | pending | `tools/load_skill.lua`                                          |
| `tools/message_agent.rs`       | 98   | Message agent tool             | moved-to-lua | P5.b  | pending | `tools/message_agent.lua`                                       |
| `tools/mod.rs`                 | 567  | Tool trait + ToolResult + ctx  | restructured | P5.a  | pending | Trait slims (loses needs_confirm/preflight/approval_patterns)   |
| `tools/notebook.rs`            | 677  | Notebook tool                  | moved-to-lua | P5.b  | pending | `tools/notebook_edit.lua` over `tui::notebook`                  |
| `tools/peek_agent.rs`          | 66   | Peek agent tool                | moved-to-lua | P5.b  | pending | `tools/peek_agent.lua`                                          |
| `tools/read_file.rs`           | 320  | Read file tool                 | moved-to-lua | P5.b  | pending | `tools/read_file.lua`                                           |
| `tools/result_dedup.rs`        | 169  | Streaming result dedup         | moved-to-capability | P3.a  | pending | → `tui::tools::dedup` helper (or Lua-only if usage is small)                     |
| `tools/spawn_agent.rs`         | 232  | Spawn agent tool               | moved-to-lua | P5.b  | pending | `tools/spawn_agent.lua`                                         |
| `tools/stop_agent.rs`          | 52   | Stop agent tool                | moved-to-lua | P5.b  | pending | `tools/stop_agent.lua`                                          |
| `tools/web_cache.rs`           | 51   | HTTP cache                     | moved-to-capability | P3.a  | pending | → `tui::http::cache`                                                             |
| `tools/web_fetch.rs`           | 268  | Web fetch tool                 | moved-to-lua | P5.b  | pending | `tools/web_fetch.lua` over `tui::http`+`tui::html`              |
| `tools/web_search.rs`          | 194  | Web search tool                | moved-to-lua | P5.b  | pending | `tools/web_search.lua`                                          |
| `tools/web_shared.rs`          | 436  | Shared HTTP helpers            | moved-to-capability | P3.a  | pending | → `tui::http` (fetch + redirects, the bulk of the module)                        |
| `tools/write_file.rs`          | 190  | Write file tool                | moved-to-lua | P5.b  | pending | `tools/write_file.lua`                                          |

## `crates/protocol/src/`

| File         | LOC | Purpose                                 | Fate         | Phase | Status  | Notes                                                                                                |
| ------------ | --- | --------------------------------------- | ------------ | ----- | ------- | ---------------------------------------------------------------------------------------------------- |
| `content.rs` | 171 | Content / ContentPart types             | kept         | none  | pending | Wire types                                                                                           |
| `event.rs`   | 414 | EngineEvent + UiCommand                 | restructured | P5.c/P5.e | pending | P5.c: drop `EngineEvent::{AgentMessage, AgentExited, Spawned}`, `UiCommand::AgentMessage`, and `AgentBlockData`. P5.e: rename pass (`SetMode`→`SetAgentMode`, `ExecutePluginTool`→`ToolDispatch`, etc.) |
| `lib.rs`     | 26  | Re-exports                              | kept         | none  | pending |                                                                                                      |
| `message.rs` | 200 | Message / Role / ToolCall / ToolOutcome | restructured | P5.c  | pending | Drop `Role::Agent` — sub-agent replies are `Role::Tool` results                                      |
| `mode.rs`    | 116 | `Mode` enum + ReasoningEffort           | renamed      | P5.e  | pending | `Mode` → `AgentMode` to match diagram                                                                |
| `usage.rs`   | 102 | TokenUsage / TurnMeta / overrides       | kept         | none  | pending | Wire types                                                                                           |

## `runtime/lua/smelt/`

| File                              | LOC | Purpose                               | Fate         | Phase | Status  | Notes                                                              |
| --------------------------------- | --- | ------------------------------------- | ------------ | ----- | ------- | ------------------------------------------------------------------ |
| `_bootstrap.lua`                  | 71  | Yield primitives, fuzzy ranking       | kept         | none  | pending | Autoload                                                           |
| `cmd.lua`                         | 77  | Command framework wrappers            | kept         | P4.e  | pending | Possibly merges into `commands.lua`; decide while coding           |
| `confirm.lua`                     | 119 | Confirm dialog renderer               | moved        | P4.d  | pending | → `dialogs/confirm.lua`                                            |
| `dialog.lua`                      | 236 | Dialog handle factory + framework     | kept         | P4.a  | pending | Primary dialog abstraction                                         |
| `picker.lua`                      | 68  | Generic picker helper                 | moved        | P4.a  | pending | → `widgets/picker.lua`                                             |
| `prompt_picker.lua`               | 164 | Prompt-docked picker                  | moved        | P4.a  | pending | → `widgets/prompt_picker.lua` (or merge into `widgets/picker.lua`) |
| `plugins/agents.lua`              | 248 | `/agents` dialog                      | moved        | P4.d  | pending | → `dialogs/agents.lua`                                             |
| `plugins/ask_user_question.lua`   | 119 | ask_user_question tool                | moved        | P5.b  | pending | → `tools/ask_user_question.lua`                                    |
| `plugins/background_commands.lua` | 224 | Bash run-in-background tools + `/ps`  | restructured | P5.b  | pending | Tool registrations → `tools/`; `/ps` → `commands.lua` or stays     |
| `plugins/btw.lua`                 | 58  | `/btw` side-question                  | moved        | P4.e  | pending | → `commands.lua` (or stays as plugin)                              |
| `plugins/color.lua`               | 30  | `/color` slug accent                  | moved        | P4.e  | pending | → `commands.lua`                                                   |
| `plugins/export.lua`              | 159 | `/export` markdown                    | moved        | P4.e  | pending | → `commands.lua`                                                   |
| `plugins/help.lua`                | 47  | `/help` keybinding viewer             | moved        | P4.e  | pending | → `commands.lua`                                                   |
| `plugins/history_search.lua`      | 50  | Ctrl+R history search                 | moved        | P4.e  | pending | → `commands.lua`                                                   |
| `plugins/model.lua`               | 37  | `/model` picker                       | moved        | P4.e  | pending | → `commands.lua`                                                   |
| `plugins/permissions.lua`         | 97  | `/permissions` dialog                 | moved        | P4.d  | pending | → `dialogs/permissions.lua`                                        |
| `plugins/plan_mode.lua`           | 181 | Plan mode hooks + exit_plan_mode tool | restructured | P5.b  | pending | Tool → `tools/`; mode hook stays in `modes.lua` or `plugins/`      |
| `plugins/predict.lua`             | 71  | Input prediction                      | kept         | P4    | pending | Stays as plugin (Lua-only feature)                                 |
| `plugins/resume.lua`              | 172 | `/resume` session picker              | moved        | P4.d  | pending | → `dialogs/resume.lua`                                             |
| `plugins/rewind.lua`              | 57  | `/rewind` turn picker                 | moved        | P4.d  | pending | → `dialogs/rewind.lua`                                             |
| `plugins/settings.lua`            | 38  | `/settings` toggles                   | moved        | P4.e  | pending | → `commands.lua`                                                   |
| `plugins/theme.lua`               | 32  | `/theme` accent picker                | moved        | P4    | pending | → `colorschemes/<n>.lua` + `commands.lua` thin wrapper             |
| `plugins/toggles.lua`             | 13  | `/vim`, `/thinking` toggles           | moved        | P4.e  | pending | → `commands.lua`                                                   |
| `plugins/yank_block.lua`          | 12  | Optional `/yank-block`                | kept         | P4    | pending | Stays as opt-in plugin                                             |

**To be created (P4.a):**

- `widgets/{input,options,list,picker,cmdline,statusline,notification}.lua`
- `dialogs/{confirm,permissions,agents,rewind,resume}.lua`
- `colorschemes/<default>.lua`
- `transcript.lua`, `diff.lua`, `status.lua`, `modes.lua`, `commands.lua`

**To be created (P5.b) under `tools/`:**

- `bash.lua`, `read_file.lua`, `write_file.lua`, `edit_file.lua`, `glob.lua`,
  `grep.lua`, `web_fetch.lua`, `web_search.lua`, `notebook_edit.lua`,
  `spawn_agent.lua`, `stop_agent.lua`, `message_agent.lua`, `peek_agent.lua`,
  `list_agents.lua`, `load_skill.lua`, `ask_user_question.lua` (moved from
  plugins), `exit_plan_mode.lua` (extracted from `plan_mode.lua`)

## `src/`

| File         | LOC | Purpose                               | Fate | Phase | Status  | Notes                                 |
| ------------ | --- | ------------------------------------- | ---- | ----- | ------- | ------------------------------------- |
| `main.rs`    | 690 | CLI entry, args, dispatch             | kept | P2    | pending | Minor edits for App carve-up          |
| `setup.rs`   | 277 | Interactive setup flows               | kept | none  | pending | dialoguer-based first-run wizard      |
| `startup.rs` | 403 | Resolve config/keys/models pre-engine | kept | P2    | pending | Produces ResolvedStartup for App init |

## To be created (Rust capability modules, P3.a)

Live as `crates/tui/src/<name>.rs` (or small folder if needed):

| Module          | Purpose                           | Composes from today's                                                   |
| --------------- | --------------------------------- | ----------------------------------------------------------------------- |
| `tui::parse`    | markdown / diff / syntax          | `content/highlight/{inline,diff,syntax,util}` + `content/stream_parser` |
| `tui::process`  | short-lived spawn / streaming / kill | `engine/tools/background` + per-tool subprocess code (foreground)   |
| `tui::subprocess` | long-lived child: spawn · send · on_event · wait · kill | `engine/socket.rs` + `engine/registry.rs` + the multi-agent IPC pieces of `engine/agent.rs` |
| `tui::fs`       | read / write / edit / glob / lock | `engine/tools/{read_file,write_file,edit_file,glob,file_state}`         |
| `tui::http`     | fetch / cache / redirects         | `engine/tools/{web_cache,web_shared}`                                   |
| `tui::html`     | html → markdown                   | extracted from `engine/tools/web_fetch`                                 |
| `tui::notebook` | Jupyter JSON ops                  | extracted from `engine/tools/notebook`                                  |
| `tui::grep`     | ripgrep wrapper                   | `engine/tools/grep`                                                     |
| `tui::path`     | normalize / canonical / relative  | new (small)                                                             |
| `tui::fuzzy`    | fuzzy matching/scoring            | `tui/fuzzy` + `tui/completer/score`                                     |
| `tui::permissions` | all permission policy: bash AST · pattern match · workspace check · runtime approvals · workspace JSON store | folds entire `engine/permissions/{approvals,bash,rules,workspace,tests}.rs` + `tui/workspace_permissions.rs` |

## Unclear — needs explicit decision

| File / question                                                                           | Phase needed by | Decision needed                                                                                                                                           |
| ----------------------------------------------------------------------------------------- | --------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `runtime/lua/smelt/cmd.lua`                                                               | P4.e            | Merge into `commands.lua`, or keep as the framework helper that `commands.lua` calls?                                                                     |
| `runtime/lua/smelt/prompt_picker.lua`                                                     | P4.a            | Merge into `widgets/picker.lua`, or stay separate as `widgets/prompt_picker.lua`? Decide while coding P4.a.                                              |
| `plugins/{btw, color, export, help, history_search, model, settings, theme, toggles}.lua` | P4.e            | All move to `commands.lua` (one master file), or stay as one-file-per-command in `plugins/` with `commands.lua` as registration index?                    |
| `plugins/predict.lua`                                                                     | P4              | Stay in `plugins/` (it's a hook, not a command/dialog/tool), or move under a new `hooks/` dir?                                                            |
| `plugins/plan_mode.lua`                                                                   | P4/P5           | Split: tool → `tools/exit_plan_mode.lua`, hook → `modes.lua` or `plugins/`. Confirm split shape.                                                          |
| `lua/render_ops.rs` (smelt.diff/syntax/bash/notebook.render)                              | P3.b            | Live under one binding file each (`parse.rs`?), or per-language as-is (`diff.rs`/`syntax.rs`)?                                                            |

## Maintenance log

Append each phase's update here as a one-line summary:

- _(no phases landed yet)_
