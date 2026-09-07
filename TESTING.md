# Testing Guide

This project uses several test layers. Pick the lowest layer that exercises the behavior faithfully.

## 1. Unit tests

Use unit tests for pure functions and small modules. They should not construct a full `TuiApp`.

Good fits:

- permission rule matching,
- provider request/stream parsing,
- text and UTF-8 boundary helpers,
- markdown/span parsing,
- small state machines.

## 2. Component tests

Use component tests when behavior needs real editor, buffer, window, layout, or grid machinery, but not a full app.

Good fits:

- `smelt_edit::Ui`, windows, buffers, cursor and selection behavior,
- vim motions/operators,
- terminal grid mutation/diff behavior,
- content rendering primitives.

## 3. App integration tests

Use `crate::app::test_harness::TestApp` for cross-cutting `TuiApp` behavior.

Good fits:

- prompt + engine + session interactions,
- Lua reload and TUI API integration,
- overlays, dialogs, pickers, pane focus,
- compaction and context-window behavior,
- event dispatch behavior that depends on app state.

Do not add concrete regressions directly to the harness implementation. Put them in topical app test modules and keep the harness focused on reusable driving/probing/invariant helpers.

## 4. Visual/storybook snapshots

Use storybook snapshots when the important assertion is what appears on screen.

Good fits:

- transcript block rendering,
- prompt rendering,
- dialogs and permissions UI,
- overlays,
- style/theme regressions,
- visible vim/editor sequences.

Avoid putting hidden business-logic assertions in storybook tests unless the visual output is the behavior being pinned.

## 5. Subprocess/headless integration tests

Use the binary + mocked provider/network harness when the seam under test crosses process, CLI, config, or provider boundaries.

Good fits:

- CLI argument/config resolution,
- provider registration from `init.lua`,
- mocked HTTP request/response behavior,
- headless JSON output shape.

## 6. Fuzz and fuzz regression replay

Use fuzzing for panic discovery, invariant checking, and broad state-space exploration. Fuzz targets should keep deterministic replay paths for found bugs.

Good fits:

- arbitrary TUI event/engine event sequences,
- Lua API lifecycle and resource handling,
- UTF-8 byte-offset mutation surfaces,
- provider parser robustness,
- permission rule combinations.

Every fuzz-found bug should get a committed seed under `fuzz/seeds/<target>/regression/`.

## Diff viewer performance

Run the optimized indexing and full-app overlay benchmarks with:

```bash
set -o pipefail; cargo xtask bench-diff 2>&1 | grep -v '^{"reason":' | tail -120
```

Three benchmarks separate acquisition, usable content, and completed syntax:

- **Native index:** 10,000 and 1,000,000 changed or unchanged Rust rows. Reports
  patch/index bytes, time to a usable index, immediate viewport/fold operations,
  and a separate fully highlighted, cached viewport measurement.
- **Full-app interactions:** the bundled Lua `/diff` plugin at 120 x 40, with
  Rust/Python and, in the 1,000/10,000-file cases, TypeScript/Lua files in an
  expanded directory tree. Takes 200 samples per operation: distant seeks, wheel
  scrolling, Ctrl-J/K file shortcuts, file clicks (including a scrolled sidebar),
  folder toggles, divider dragging, and warm scrolling
  that asserts syntax is ready. Samples include input dispatch, rendering,
  focus/scroll observers, tree synchronization, and repaint. Cold navigation can
  display plain diff colors while syntax catches up. Destination assertions check
  the actual result, not just how quickly an input returns. Million-line replacement
  workloads across 2 and 10,000 files also require inline highlighting in warm views.
- **Real Git loading:** creates repositories with unique Rust source lines, then
  types `/diff` through the app. Covers 10,000 rows across 2 untracked files and
  1,000,000 rows across 2, 1,000, or 10,000 untracked files, plus 1,000 tracked files.
  Reports usable source separately from completed first-viewport syntax, then
  stages and unstages a file while measuring frame latency and UI-thread allocation,
  verifying that a fresh snapshot moves it into the correct section.

Release gates enforce an index/real-Git usable load under one second and p95
interactions under 16 ms. The runner also reports peak process memory when
supported. The synthetic app load includes Lua fixture construction; the real Git
measurement excludes fixture setup but includes command dispatch and rendering.

Git acquisition and compact patch indexing run off the UI thread; patch indexing
is linear in input size. Git batches untracked files using one root pathspec and
an isolated temporary index and object directory. This avoids both per-file
subprocesses and all-pairs pathspec matching in large file sets. Syntax and inline
highlighting are demand-driven on a separate worker: 128-row chunks, at most 64
cached chunks, an 8 MiB token/scope/inline target (the current viewport stays
pinned), and at most 256 parser checkpoints. Replacement boundaries are indexed
once. Small blocks share `edit_file`'s alignment and grapheme policy; large blocks
pair only requested rows positionally. Alignment and pair comparisons have
input-size and time budgets; pairs over 8 KiB keep row-level emphasis. Inline
ranges publish before syntax reconstruction. Distant syntax seeks can require
scanning preceding source for exact multiline state; newer viewport requests
preempt that work, and cheap visible file starts take priority. Demand is tracked
per window, so independent views do not cancel each other. The first 128-256 rows
of the selected file's two neighbors are prefetched only after visible work.
Copying/searching reads cached rows without requesting enrichment, and speculative
work does not keep the UI repainting. Viewport reads never wait for this work.

The ordinary suite covers a real million-line `/diff` load, distant-seek
preemption by a newly selected file, bounded cache eviction/checkpoint reuse,
worker shutdown and repaint notification. A deterministic allocation guard
asserts fewer than 2 MB of UI-thread allocations per navigation/repaint, including
keyboard resizing and toggles in a 10,000-file tree, and at most a viewport of
retained rows in either pane. Inline tests cover million-line replacement pairing, cancellation,
no-newline markers, long-line fallback, syntax preservation, Unicode, horizontal
pan, and theme changes. Git equivalence tests cover attributes, encodings, clean
filters, symlinks, binary/empty files and literal paths, and verify the real index
and object storage are unchanged, including split-index linked worktrees. Timed
benchmarks are ignored in normal runs to avoid CPU-contention flakiness; use an
idle machine.

### Reference measurement

One Linux x86-64 KVM run on a Ryzen 9 7900X (8 exposed CPUs), using Git 2.53.0,
the release profile, and a 120 x 40 viewport. All three benchmarks passed their
regression gates. All table timings are milliseconds.

Interaction columns include input dispatch and repaint and report p95 over 200
samples. Warm scroll requires completed, cached syntax and, for replacements,
inline highlighting. Other interactions prioritize immediate content and allow
background highlighting. Replacement row counts include both old and new sides:

| Patch rows | Files | Changes | Fixture usable | Distant seek | Wheel scroll | Ctrl-J/K | File click | Folder toggle | Split resize | Warm scroll |
| ---: | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 10,000 | 2 | Additions | 6.37 | 1.28 | 1.38 | 1.83 | 3.00 | 2.16 | 4.57 | 1.73 |
| 1,000,000 | 2 | Additions | 241.38 | 1.43 | 1.50 | 1.15 | 2.69 | 2.07 | 4.59 | 2.16 |
| 1,000,000 | 1,000 | Additions | 187.41 | 1.16 | 2.11 | 2.58 | 2.02 | 3.11 | 5.02 | 2.50 |
| 1,000,000 | 10,000 | Additions | 226.28 | 1.58 | 1.74 | 2.54 | 2.10 | 2.17 | 4.70 | 2.17 |
| 1,000,000 | 2 | Replacements | 179.19 | 1.62 | 1.73 | 1.97 | 2.65 | 2.06 | 4.42 | 2.23 |
| 1,000,000 | 10,000 | Replacements | 231.74 | 1.68 | 2.11 | 2.09 | 2.73 | 3.37 | 5.15 | 1.86 |

Split resize samples include pointer down, drag, and release with repaints using
`smelt-term`'s shared geometry and interaction API.

Real repository loading uses unique source lines and freshly written fixtures in
the filesystem cache; fixture setup is excluded. Stage/unstage columns show total
elapsed time followed by frame p95 during the operation, including fresh grouped
snapshot acquisition and installation. Every operation verified the new section
membership:

| Source rows | Files | Worktree state | Usable source | First viewport syntax | Stage total / frame p95 | Unstage total / frame p95 |
| ---: | ---: | --- | ---: | ---: | ---: | ---: |
| 10,000 | 2 | Untracked | 25.04 | 31.52 | 32.51 / 1.41 | 30.53 / 1.14 |
| 1,000,000 | 2 | Untracked | 407.63 | 412.13 | 401.98 / 1.61 | 446.60 / 1.55 |
| 1,000,000 | 1,000 | Untracked | 311.82 | 316.49 | 421.10 / 1.51 | 442.75 / 1.89 |
| 1,000,000 | 1,000 | Tracked | 508.68 | 514.23 | 931.05 / 2.62 | 963.84 / 2.14 |
| 1,000,000 | 10,000 | Untracked | 613.03 | 617.48 | 874.38 / 2.00 | 728.69 / 1.93 |

All interaction p95s were below 5.16 ms, including full resize gestures;
non-resize p95s stayed below 3.38 ms. The longest sampled viewer gesture was an
8.35 ms highlighted scroll in the 1,000-file tree. UI-thread allocations stayed
below 0.67 MB per interaction, including folder toggles and resizing; staging
snapshot swaps stayed below 0.71 MB. They share the same 2 MB allocation gate as
other navigation. Every case retained at most 40 preview rows.

Native million-row indexing took 26.35 ms for additions and 27.31 ms for unchanged
context. Immediate viewport/fold reads had p95 below 0.017 ms; fully highlighted
cached viewport reads had p95 below 0.92 ms. Reported peak process RSS across each
multi-case benchmark was about 102 MiB for the native index, 318 MiB for the
Lua/app fixtures, and 319 MiB for real Git loading and snapshot replacement.

These are observations on a shared development host, not cross-machine or
cold-storage guarantees. The one-second loading, 16 ms p95 interaction, and
deterministic allocation assertions are the regression gates.

## Choosing a layer

- If a pure function can cover it, write a unit test.
- If real edit/layout/render components are enough, avoid `TestApp`.
- If behavior depends on app-level state, use `TestApp`.
- If the user-visible frame matters, prefer storybook.
- If config/CLI/provider process seams matter, use subprocess integration.
- If the state space is large or history-dependent, add fuzz coverage and commit regression seeds for bugs.
