# Decisions log

Architectural decisions made before any phase landed. Newest first.
After P0 lands, decisions move into the per-phase `P<n>.md` instead;
this file keeps only pre-P0 history.

## Pre-P0 (2026-04-28)

Scaffolding pass: REFACTOR.md / INVENTORY.md / FEATURES.md / STATUS.md
written, then 4-agent verification audit on INVENTORY (207/207 files
clean) and FEATURES (counts confirmed; phantoms fixed; missing core
agentic features added). Cross-file consistency pass applied: tool
count corrected from "21 core tools" to "15 Rust impls + Lua reorg" in
REFACTOR; `tui::fuzzy` added to P3.a + ARCHITECTURE.md + the puml;
P5.d hedge dropped with explicit rename list; `buffer_list.rs`
committed to deletion; `prompt_picker.lua` merge added to Unclear;
`smelt.timer` split into working `defer` row + `offline-pre-P3`
namespace row.

Then ten architectural decisions landed across REFACTOR / ARCHITECTURE
/ puml:

1. **Cells subsume autocmds.** One registry, one observer mechanism.
   `smelt.au.*` is sugar over `smelt.cell.*`. The standalone autocmd
   registry is gone from `LuaRuntime`; built-in events (`history`,
   `turn_complete`, `confirm_requested`, …) are typed `Cell<Payload>`s.
2. **All tools to Lua, FFI for intricate logic; engine becomes
   policy-free.** `tui::permissions` absorbs the entire
   `engine/permissions/` module (5 files + 1617 LOC of tests) plus
   `tui/workspace_permissions.rs`. Engine drops the `Permissions`
   aggregate, `decide()`, per-mode rule plumbing, `RuntimeApprovals`
   field on agent. Engine emits `RequestPermission` / consumes
   `PermissionDecision` and that's its full permission surface.
3. **Extmark-level YankSubst.** `enum YankSubst { Empty, Static(String) }`
   on `Extmark`; `Buffer::yank_text_for_range` is a pure helper
   walking extmarks. Hidden thinking → `Empty`; prompt attachment
   sigils → `Static(expanded_path)`. Default = literal source bytes
   (markdown bold copies as `**bold**`).
4. **Core / TuiApp / HeadlessApp + Host / UiHost.** `Core`
   (headless-safe) holds session/confirms/clipboard/timers/cells/lua/
   tools/engine_bridge; `TuiApp` adds `well_known + ui::Ui`;
   `HeadlessApp` adds a JSON/text sink. `Host` is Ui-agnostic;
   `UiHost: Host` carries the compositor surface. `HeadlessApp` impls
   only `Host` — UiHost-only Lua bindings (`smelt.ui / .win / .buf /
   .statusline`) error in headless.
5. **All config in `init.lua`.** Drop `config.yaml` and any TOML
   keymap. Permissions / providers / MCP / theme / keymap / model
   defaults all become Lua calls. Single config language end-to-end;
   Rust ships no YAML/TOML config parser. Lands as P5.d.
6. **All 6 engine "utility tool" files move to tui capabilities.**
   `engine/tools/{background,file_state,web_cache,web_shared,result_dedup}.rs`
   → `tui::process / tui::fs / tui::http / tui::tools::dedup`.
   `engine/tools/` shrinks to `ToolSchema + ToolDispatcher +
   ToolResult`. The Unclear row is closed.
7. **One binary, two entry points.** `smelt`, `smelt -p`, and
   `smelt --agent <id>` all dispatch in `main` to either `TuiApp` or
   `HeadlessApp`. No second binary. No `EngineConfig.interactive`
   flag — Lua tools call `smelt.frontend.is_interactive()`. Protocol
   rename pass shifted from P5.d to P5.e.
8. **`EngineHandle` is channels-only.** Drop `processes`,
   `permissions`, `runtime_approvals` public fields.
   `EngineHandle = cmd_tx + event_rx`, nothing more.
9. **Multi-agent → plugin pattern (Option B).** Engine drops the
   multi-agent concept entirely. Sub-agents are child processes
   spawned by Lua tools (`spawn_agent.lua` etc.) through a new
   `tui::subprocess` capability (spawn / send / on_event / wait /
   kill). Sub-agent replies are tool results; transcript renders them
   as ordinary tool calls (no special widget). Removed:
   `protocol::Role::Agent`, `protocol::AgentBlockData`,
   `EngineEvent::{AgentMessage, AgentExited, Spawned}`,
   `UiCommand::AgentMessage`, `EngineConfig.multi_agent`,
   `engine::tools::AgentMessageNotification`, `engine/registry.rs`,
   `engine/socket.rs` (relocated to `tui::subprocess::{registry,
   socket}`), `Session.agents/snapshots`,
   `transcript_present/agent.rs`. Multi-agent loop branch in
   `engine/agent.rs` (~400 LOC) deletes; `agent.rs` becomes
   single-agent only. `smelt.agent.mode` Lua API renamed to
   `smelt.mode` to avoid collision with `smelt.subprocess`.
10. **Drift-prevention check script (manual, not pre-commit).**
    `refactor/check.sh` runs invariants over the synced docs (P\<n\>
    headers exist, INVENTORY paths exist, puml validates, etc.).
    Run by hand at session boundaries.
