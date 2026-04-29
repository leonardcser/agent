# Testing — strategy

How smelt is tested. Three layers, each owns its own scope. Updated when a
layer's harness changes shape.

For meta-rules and the doc index, see `README.md`.

## The three layers

| Layer  | Scope                                                                 | Harness                                                                          | Assertion                                                                            |
| ------ | --------------------------------------------------------------------- | -------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------ |
| **L1** | `ui` primitives: Buffer, Window, vim recipe, cells, layout, theme, extmarks | pure unit, no event loop                                                         | marker DSL: `("foo #[bar\|]# baz", "diw", "foo #[ \|]# baz")` + `assert_eq!`         |
| **L2** | engine ↔ Lua ↔ tools ↔ permissions ↔ persistence                       | `HeadlessApp` + wiremock'd LLM + custom `init.lua` via `XDG_CONFIG_HOME`         | `insta` snapshot of persisted `Vec<Message>`                                         |
| **L3** | widgets, dialogs, focus chain, mouse hit-testing, drag-select         | (a) `Grid::with_lines([...])` render; (b) tiny Pilot driving real `TuiApp`       | `assert_eq!(grid, expected)` / widget-state queries                                  |

Each layer tests what it owns. Don't snapshot rendering inside L1; don't drive
events through `Buffer` mutations; don't mock the renderer.

## What we don't test

- **PTY-based** (real subprocess, virtual terminal). Defer until terminal
  contracts are pinned. At most 1–2 smoke tests, only if `main.rs` grows logic.
- **Visual snapshots** (text or SVG). Inline `Grid` literals beat snapshot files
  for densest assertion. Revisit if widget output gets too large to inline.
- **Real LLM calls.** Wiremock only. Real-provider tests can be opt-in nightly,
  not in CI.

## L1 — model state (Helix marker DSL)

Same lexicon as Helix: `#[primary|]#`, `#(secondary|)#`, `|` marks head vs
anchor. Three-tuple `(input, keys, expected)`:

```rust
vim_test(("foo #[bar|]# baz", "diw",  "foo #[ |]# baz")).await?;
buf_test(("hello#[|]#",       "Aworld<esc>", "helloworld#[|]#"));
```

Lives in `#[cfg(test)] mod tests` next to the code under test:
- `crates/ui/src/buffer.rs`, `window.rs`, `layout.rs`, `cells.rs`, `theme.rs`
- vim recipe tests once it lands as keymaps over Window (P1.d)

No event loop, no rendering. Pure data round-trip.

## L2 — engine integration (headless + wiremock)

Drive `HeadlessApp` against a wiremock'd LLM. Each scenario:

1. Spin up wiremock with canned SSE responses (cassettes).
2. Tempdir + `XDG_CONFIG_HOME` → write `init.lua` (registers test tools, sets
   permissions, etc.).
3. Construct `HeadlessApp` pointing at the mock server.
4. Drive with prompt or `UiCommand` sequence.
5. Wait for `TurnComplete`.
6. Snapshot the persisted session `Vec<Message>` via `insta`.

Layout:

```
crates/tui/tests/
  common/
    harness.rs          # spawn HeadlessApp pointing at wiremock
    cassettes/          # canned SSE streams
    init_lua/           # per-scenario init.lua fragments
  scenarios/
    plain_turn.rs
    tool_confirm_allow.rs
    tool_confirm_deny.rs
    retry_then_succeed.rs
    midblock_turn_end.rs
    multi_turn.rs
    compact.rs
    fork.rs
    plugin_hook_needs_confirm.rs
    multi_agent_subprocess.rs
    mode_cycle_plan_to_apply.rs
  snapshots/            # insta defaults
```

Deps (dev-only): `wiremock`, `insta`, `tempfile`.

The persisted `Vec<Message>` shape lives in the `protocol` crate, which the
refactor barely touches — goldens written today survive P0–P7. They are the
"no feature dropped" gate, mechanised.

## L3 — rendering + interactions

Two sub-parts.

### L3a — widget render (ratatui-style)

Build a widget, render into a fake `Grid`, `assert_eq!`. Smelt's `Grid` is
ratatui-`Buffer`-shaped; we add `Grid: PartialEq` + `Grid::with_lines([...])`.

```rust
let mut expected = Grid::with_lines([
    "│ ☐ Run bash command   │",
    "│   echo hello         │",
    "│ [ Allow ] [ Deny ]   │",
]);
expected.set_style(Rect::new(2, 2, 9, 1), styles::FOCUS_RING);
let mut actual = Grid::empty(expected.area());
ConfirmDialog::new(...).render(actual.area(), &mut actual);
assert_eq!(actual, expected);
```

Lives next to each widget under `#[cfg(test)] mod tests`.

### L3b — Pilot (Textual-style, ~80 LOC)

Multi-step gestures spanning widgets. Selector-by-id (not absolute coords).
Coordinates are widget-relative — tests survive layout shuffles.

```rust
let mut pilot = Pilot::new(test_app(), 100, 40);
pilot.press("y").await;                          // open prompt
pilot.click(WinId::Confirm, (5, 2)).await;       // click "Allow"
pilot.drain().await;
assert_eq!(app.cell("confirms_pending").get(), 0);
```

Lives in `crates/tui/tests/interaction/`.

L3a answers "did this widget draw correctly"; L3b answers "did the gesture do
the right thing to the model." Two surfaces, two assertions.

## Determinism rules

- **Fixed terminal size** per L3 test (e.g. 100×40). Coordinates become stable.
- **Freeze time** — mock `Instant::now` / `SystemTime::now`. Cleanest place:
  introduce clock-injection seam in P2 alongside `EngineBridge`.
- **Pin spinner phase** — drive `now` / `spinner_frame` cells to known values.
- **`tokio::time::pause` + `advance`** — never real `sleep` in tests.
- **`insta` filters** — strip dynamic IDs, durations, paths, timestamps.
- **No real network** — wiremock only; CI fails on outbound HTTP.

## Sequencing across the refactor

| Phase                | What lands                                                                                            |
| -------------------- | ----------------------------------------------------------------------------------------------------- |
| **Pre-P0**           | L2 harness + 5–10 baseline scenarios on today's binary. Locks behaviour before demolition.            |
| **P1.d** (vim decomposes) | L1 marker DSL parser; existing vim tests ported to 3-tuple form.                                  |
| **P2** (EngineBridge) | L2 re-pointed at refactored engine; clock injection lands. Goldens reviewed with `cargo insta review`. |
| **P4** (widgets stabilise) | L3a per widget. L2 snapshots reviewed at boundary.                                              |
| **P5** (Lua dialogs/tools) | L3b Pilot for dialog interactions.                                                              |
| **P7**               | Full suite is the parity gate; FEATURES.md walked against green tests.                                |

L2 lands first — it's the parity gate for the demolition. L1 follows when vim
breaks open. L3 follows when widgets stabilise.

## How to add a test

- **L1** — add `#[test]` next to the function under test. Use marker DSL.
- **L2** — add `crates/tui/tests/scenarios/<flow>.rs`. New cassette in
  `cassettes/`, new init.lua fragment in `init_lua/`. Run `cargo insta review`
  to bless the snapshot.
- **L3a** — add `#[test]` in the widget's module.
- **L3b** — add `crates/tui/tests/interaction/<flow>.rs`.

Run all: `cargo nextest run --workspace`. Review snapshot diffs:
`cargo insta review`.
