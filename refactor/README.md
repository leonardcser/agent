# refactor/ — start here

Entry point for the smelt rebuild. Short by design. Re-read every few sessions.
If you're new to this — including future-you in two weeks — this is your map.

## What this is

A multi-phase rewrite of smelt's Rust + Lua surface to the target shape in
`tui-ui-architecture-target.puml`. Greenfield: no users, no migration story, no
backward compatibility. The only artefact that matters is the final shape.

## The plan is not final

This is the most important rule. **The refactor plan in `REFACTOR.md` is a
sketch, not a contract.** Reality will push back on it. We absolutely have not
thought of everything. There are decisions in these docs that will turn out
wrong, sub-phases that need to split or merge, abstractions that look elegant on
paper and ugly in code, and better designs you'll see only once you have a file
open.

When that happens:

- **Take the better path.** Don't follow the doc to the letter when a better way
  is plain. Update the docs in the same change. Log the decision in `P<n>.md`.
  The diagram and these docs are canonical for _current intent_; both can
  change.
- **Stop and ask** when the friction wants a user-visible behavior change you
  didn't agree to, or when two designs are equally defensible and you can't
  decide alone.

Bias toward decisive action when the right answer is plain; toward pausing when
it isn't. Either path ends with the docs updated.

## How we work through phases

- **A red tree is fine inside a phase.** No migration shims, no parallel "kept
  for now" implementations, no compat layers. A larger diff that lands the tree
  in its final shape beats a chain of small diffs that leave it half-migrated.
  We require green only at phase boundaries.
- **No feature gets dropped.** The UX changes shape; it doesn't shrink.
  `FEATURES.md` is the parity checklist — walk it at each phase boundary. If a
  phase deletes code and a feature isn't reachable in the new shape, the phase
  isn't done.
- **No throwaway scaffolding.** When a step rewrites a thing, the old thing goes
  in the same change.
- **Decide while coding.** When two designs feel close, pick one in the diff,
  don't pre-plan it here. Document what you picked in `P<n>.md` if you'd
  re-litigate it later.
- **No step is too big.** If a phase wants to delete 30 files and replace them,
  that's a phase.
- **Diagram is the spec for structure.** When `REFACTOR.md` and the puml drift,
  follow the puml.

## Verify when it makes sense

- **At phase boundaries:**
  `cargo fmt && cargo clippy --workspace --all-targets -- -D warnings && cargo nextest run --workspace`.
- **Whenever a `refactor/*` doc changes:** `refactor/check.sh`. Should be all
  green; warnings are fine; red `✗` means a doc lies.
- **For UI changes:** drive the binary in a tmux side-pane (see
  `ARCHITECTURE.md` → Testing TUI changes).
- **For puml edits:** regenerate the SVG
  (`plantuml -tsvg refactor/tui-ui-architecture-target.puml`).

Don't run all of these on every commit. Run what catches what you just changed.

## The documents

| File                              | What it is                                          | When to read                                    | When to update                                                    |
| --------------------------------- | --------------------------------------------------- | ----------------------------------------------- | ----------------------------------------------------------------- |
| `README.md` (this)                | Meta rules + index.                                 | First. Re-read every few sessions.              | When a meta-rule changes.                                         |
| `STATUS.md`                       | One-screen "where we are."                          | Every session start. 30 seconds.                | Every session end. After every decision.                          |
| `REFACTOR.md`                     | Sequencing — phases P0..P7 and their content.       | When you start or plan a phase.                 | When a phase's scope or sequence shifts.                          |
| `ARCHITECTURE.md`                 | Target intent — decisions, rationale, target rules. | When deciding what shape something should take. | When intent changes.                                              |
| `tui-ui-architecture-target.puml` | Canonical structure diagram.                        | When you want a structural picture.             | When types / fields / packages move. **Then regenerate the SVG.** |
| `INVENTORY.md`                    | Per-file ledger: every source file → its fate.      | When you want "what happens to file X."         | When file fates change or files appear/disappear.                 |
| `FEATURES.md`                     | User-facing parity checklist.                       | At phase boundaries to walk parity.             | When a feature's status shifts (offline / verified / regressed).  |
| `P<n>.md`                         | Per-phase log of what shipped + decisions made.     | When starting `P<n+1>`.                         | At the end of each phase. Template below.                         |
| `DECISIONS.md`                    | Pre-P0 architectural decisions log.                 | If you wonder "why is X this way?"              | Frozen once P0 lands. Decisions thereafter live in `P<n>.md`.     |
| `TRACE.md`                        | One vertical slice walked end-to-end through the target. Concrete `init.lua` + `bash.lua` example. | When you need a reality check on how a flow composes, or when designing a new Lua tool. | Add new slices when a different flow surfaces a design hole. |
| `check.sh`                        | Drift-detection invariants.                         | Session start; before declaring a phase done.   | When a new invariant is worth checking.                           |

## Doc sync rule

When a decision is made or the design shifts, update **every** affected doc in
the **same change**. Drifted docs are bugs. `check.sh` catches the mechanical
cases; the human-shaped cases (intent vs structure consistency) need attention.

A phase is not "landed" until `P<n>.md` is written and the affected companion
files are updated.

## Cold-start checklist

1. Read this file.
2. Run `refactor/check.sh` — should be all green.
3. Read `STATUS.md` — orient in 30 seconds.
4. Read the most recent `P<n>.md` to see what just happened.
5. Skim `INVENTORY.md` rows for the upcoming phase.
6. If anything feels stale, fix the docs before writing code.

## `P<n>.md` template

```md
# P<n> — <phase name>

**Status:** in-progress | done **Started:** YYYY-MM-DD **Landed:** YYYY-MM-DD
(or empty while in-progress)

## What shipped vs what was planned

One sentence per sub-phase, with deviations called out. If a sub-phase slipped
to the next phase, say so and link the row in INVENTORY.md.

## Decisions made while coding

For each non-obvious choice:

- **<short title of the decision>**
  - **Picked:** what we did.
  - **Alternative considered:** what else was on the table.
  - **Why this one:** the deciding factor.

The rule: if you would re-litigate this in `P<n+2>`, write it down.

## Files / types / functions changed

Compact diff-shape summary (INVENTORY.md is the source of truth):

- Added: …
- Deleted: …
- Renamed / moved: …
- Restructured: …

## Deferrals

What was supposed to be in this phase but landed in a later one (with the new
home). Without a clear destination, surface it in STATUS's open questions.

## Verification

- Tree status at end of phase: green | red (with reason).
- Tests run: `cargo nextest run --workspace` result.
- FEATURES.md walk: which rows verified, which still offline.
- Any human parity walk performed.

## Next phase entry conditions

What must be true before `P<n+1>` starts.
```
