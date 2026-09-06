# Regression seeds

Regression inputs committed under `<target>/regression/<bug-slug>` - one file
per fixed bug, exercised by:

```sh
cargo xtask fuzz replay-regression
```

and by CI on every PR. **Add a file here when you fix a fuzz-found bug.**
Each file should reproduce the bug *before* the fix and pass *after*.

The structured `smelt_loop` and `lua_loop` targets use JSON because named
variants survive changes to their `Arbitrary` encodings. All other targets use
the exact minimized libFuzzer artifact because they have no stable scenario
format. Both forms replay the same production path as their fuzz target.

## Adding a new regression

1. Run `cargo xtask fuzz triage <target> <artifact>` before changing the
   production code. Triage minimizes the input while preserving its panic
   source and normalized message.
2. Fix the bug and confirm the minimized input passes.
3. For `smelt_loop` or `lua_loop`, commit the minimized JSON as
   `<bug-slug>.json` with `_about` and `_fix` fields. For a byte target, commit
   the exact minimized artifact as `<bug-slug>` without rewriting it.
4. Re-run `cargo xtask fuzz replay-regression` to confirm green.

## Per-target conventions

- `lua_loop` - `LuaScenario` JSON; `LuaSnippet` is the human path.
- `smelt_loop` - `Scenario` JSON. No equivalent free-form op yet; for
  smelt_loop bugs, hand-write the minimal `ops` sequence using the
  existing `FuzzOp` variants.
- Every other registered target uses raw byte artifacts. The xtask registry
  classifies these targets. `cargo xtask fuzz replay-regression` builds selected
  targets once, then executes each seed directly with `-runs=1` and an independent
  30-second watchdog. Use `--timeout SECONDS` to change that deadline. Nested seed
  directories are supported; symlinks and non-regular inputs are rejected.
- A replay failure is reported with its target and relative seed path. Remaining
  seeds still run, and the command exits nonzero if any seed failed or timed out.
