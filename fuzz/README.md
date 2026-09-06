# smelt fuzz

Seventeen targets covering distinct surfaces:

| target                    | what it fuzzes                                                               | how                                                                                             |
| ------------------------- | ---------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------- |
| `smelt_loop`              | TUI event loop (terminal + engine events)                                    | structured `Scenario` ops, swarm weighting, and macro workloads                                 |
| `lua_loop`                | Lua FFI bindings (`smelt.*` API)                                             | structured `LuaScenario` ops + FFI ledger oracle                                                |
| `text_ops`                | `smelt_buffer::text` UTF-8 helpers                                           | direct calls + reference-model differential                                                     |
| `attached_ops`            | `smelt_buffer::attached::AttachedTextMut`                                    | segment-based reference model + invariant check                                                 |
| `cache_invariance`        | Anthropic prompt-cache prefix stability                                      | random history + `cache_control`-aware byte diff                                                |
| `openai_cache_invariance` | OpenAI / aux-model prompt-cache stability                                    | random history + `prompt_cache_key`-aware byte diff                                             |
| `snapshot_roundtrip`      | `SnapshotFrame::parse` round-trip                                            | random grid + style palette + assert `from_grid → text+styles → parse` is identity              |
| `grid_invariants`         | terminal grid mutation/diff invariants                                       | random cell writes/fills + wide-char and diff-replay oracles                                    |
| `ansi_parser`             | ANSI SGR parser + wrapped emission                                           | random bytes → lossy UTF-8 + `wrap_ansi` / `emit_ansi_row` UTF-8 boundary checks                |
| `edit_ops`                | editor/window core (`smelt_edit::Ui`, buffer edits, vim keys, mouse, resize) | focused shell around edit primitives + cursor/selection UTF-8 invariants                        |
| `transcript_render`       | transcript-producing engine events and renderer projection                   | focused `TestApp` shell: tool/text/thinking/process events + resize/render invariant checks     |
| `transcript_scroll_ops`   | sparse transcript scrolling, cursor motion, selection drag, and autoscroll   | resumed heterogeneous transcript + semantic scroll trace oracles                                |
| `provider_body`           | provider request construction and configuration                              | all body builders + routing, API-base, auth, catalog, extraction, and schema invariants          |
| `provider_stream`         | provider SSE/response parsers                                                | raw-byte partitioning + Chat Completions/OpenAI/Anthropic lifecycle and response oracles         |
| `permissions_rules`       | permission rule compilation/evaluation                                       | random rule sets, shell-aware subpatterns, mode behavior, workspace downgrade oracle            |
| `store_state`             | canonical session persistence and maintenance                                | file-backed writer/reader state machine + independent history, transcript-record, and revision model   |
| `engine_events`           | engine lifecycle and canonical-history application                           | focused event state machine + independent active-turn and canonical-suffix model                |

## Setup once

```sh
rustup toolchain install nightly
cargo install cargo-fuzz
```

Operational corpus, artifact, and coverage data lives outside the checkout under
`$XDG_CACHE_HOME/smelt/fuzz/<repository-id>/`. Every worktree for the same clone
uses that directory. Set `SMELT_FUZZ_HOME` to use an explicit location.
Relative `SMELT_FUZZ_HOME` paths are resolved from the invocation directory.
Regression seeds remain tracked under `fuzz/seeds/`. Initialize an empty shared
root with one bootstrap corpus input per registered target:

```sh
cargo xtask fuzz prepare
```

`run`, `coverage-snapshot`, and `verify` also prepare the corpora they need.
Import corpus and artifacts from the old checkout-local layout once:

```sh
cargo xtask fuzz import-data
# Or import from another clone or checkout:
cargo xtask fuzz import-data /path/to/repository/fuzz
```

## Day-to-day

Build all real fuzz targets without accidentally building helper bins:

```sh
cargo xtask fuzz build
cargo xtask fuzz build smelt_loop ansi_parser
```

Cargo metadata determines the build root, including `CARGO_TARGET_DIR` and
`.cargo/config.toml` overrides. Builds live under
`<target_directory>/smelt-fuzz/<mode>/<host>/`, where mode is `tools`, a sanitizer
name, or `coverage`. Without an override, the target directory is `fuzz/target`.
Tools and fuzz targets explicitly build for the host even if Cargo config selects
a different target. Sanitizer and coverage builds never replace one another.

Target and scenario-helper builds each have a 30-minute watchdog. Override it with
`--build-timeout SECONDS` for cold or resource-constrained builds:

```sh
cargo xtask fuzz build --build-timeout 3600 text_ops
cargo xtask fuzz --build-timeout=3600 triage text_ops /path/to/crash --timeout 60
```

Timeout values must be positive 32-bit integers in seconds. Both `--timeout=60`
and `--timeout 60` are accepted; repeated timeout options use the last value.
`--build-timeout` defaults to 1800 and may appear before or after the subcommand,
but before trailing libFuzzer flags or any `--` separator. Use `<subcommand> --help`
for command-specific options.
The deadline applies separately to each build command, not to the whole invocation.
It does not change replay, minimization, preflight, campaign, or tool-query limits.
`coverage-snapshot` uses its own `--timeout` for the combined coverage build and
replay, independently of `--build-timeout`.

Fuzz a single target until first crash/OOM/timeout or Ctrl-C:

```sh
cargo xtask fuzz run smelt_loop --fork 8
```

AddressSanitizer is the default. Use `--sanitizer none` only for a deliberate
high-throughput run, not as the only campaign for a target.

| flag               | what                                                       |
| ------------------ | ---------------------------------------------------------- |
| `--fork N`         | positive parallel worker count (default 1)                  |
| `--cmin`           | sweep and shrink the shared corpus first                    |
| `--sanitizer KIND` | address, leak, memory, thread, or none                       |
| trailing args      | libFuzzer `-flag=value` options, such as `-max_total_time=3600` |

The target builds once, then runs directly. A non-forking corpus preflight must
pass before any campaign starts. Trailing arguments are an explicit tuning allowlist;
worker, failure-ignore, exit-code, signal, parser, artifact-output, and operation-mode
controls cannot override xtask supervision. Supported tuning flags:

- Limits and sizes: `runs` (-1 or nonnegative), `max_total_time`, `max_len`,
  `len_control`, `timeout`, `rss_limit_mb`, `malloc_limit_mb`, `reload`,
  `report_slow_units` (nonnegative integers).
- Mutation and scheduling: `seed` (u32), `mutate_depth` (positive integer),
  `entropic_feature_frequency_threshold`, `entropic_number_of_rarest_features`
  (nonnegative integers).
- Boolean tuning (0 or 1): `cross_over`, `cross_over_uniform_dist`, `reduce_inputs`,
  `keep_seed`, `shuffle`, `prefer_small`, `only_ascii`, `use_counters`, `use_memmem`,
  `use_value_profile`, `use_cmp`, `entropic`, `entropic_scale_per_exec_time`,
  `fork_corpus_groups`.
- Diagnostics: `verbosity`, `print_funcs` (nonnegative integers); `print_pcs`,
  `print_final_stats`, `print_corpus_stats`, `print_coverage`, `print_full_coverage`
  (0 or 1).

Use `-name=value` syntax. Put wrapper options such as `--fork`, `--sanitizer`,
and `--build-timeout` before the trailing libFuzzer flags; an explicit `--` separator
is optional. Unknown flags and malformed values are errors.
Corpus preflight and optional minimization each have a five-minute watchdog;
the campaign itself runs until failure or cancellation.

`--cmin` copies and preflights a snapshot, then runs libFuzzer merge directly.
Every selected input is published before redundant snapshot inputs are removed.
A failed merge leaves the live corpus untouched; interrupted publication can leave
extra inputs, never fewer selected inputs. Concurrent additions and changed inputs
are retained. Competing minimizers for one target fail rather than racing to prune.
Corpus inputs are immutable once published: add new files instead of editing inputs
in place. Copying works even when the checkout and shared data use different filesystems.

All subprocesses run in an owned Unix process session or Windows job. Timeout,
SIGINT/Ctrl-C, SIGTERM on Unix, and normal launcher exit stop remaining descendants
and reap the launcher. Cancellation stays active throughout the invocation, including
filesystem work between subprocesses. Unix subprocesses must not detach into another
session. Build watchdogs are configurable with `--build-timeout`; tool queries
have a 30-second watchdog.
Captured diagnostics retain at most 4 MiB per stream and identify truncation;
truncated machine-readable output is never parsed as a successful result.

Target exit codes are preserved, including libFuzzer's crash code 77. External
watchdog expiry returns 124, Unix interruption returns 128 plus the signal number,
and invalid arguments or tooling/data errors return 2.

## Background fuzzing (agents)

The intended shape for long-running fuzz sessions in an agent loop:

1. Start the command in the agent's background execution mode, redirecting output
   to a per-target log when running unattended:

   ```sh
   cargo xtask fuzz run text_ops --fork 3
   ```

2. Don't poll - the harness fires a notification the moment any background
   process exits, and `cargo xtask fuzz run` exits on crash, OOM, timeout,
   corpus preflight failure, or Ctrl-C. An exit code 77 means libFuzzer caught a
   panic. The command prints the shared artifact directory on failure; `cargo
   xtask fuzz status` also prints the shared data root.
3. On notification: read the log, locate the failure artifact, and triage it:

   ```sh
   cargo xtask fuzz triage <target> /shared/fuzz/data/artifacts/<target>/<artifact>
   ```

4. Fix the bug, commit a regression seed under
   `fuzz/seeds/<target>/regression/`, then re-launch the target.

The agent doesn't sleep, schedule wake-ups, or check progress - the notification
on exit is the loop signal.

## When the fuzzer finds a crash

```sh
# Structured targets preserve panic identity while shrinking JSON operations.
cargo xtask fuzz triage lua_loop /shared/fuzz/data/artifacts/lua_loop/crash-<hex>

# Byte targets minimize and replay under AddressSanitizer, then verify the fingerprint.
cargo xtask fuzz triage provider_stream /shared/fuzz/data/artifacts/provider_stream/crash-<hex>
```

Triage independently replays both the original and minimized input, rejecting
unrecognized crashes, changed failure fingerprints, timeouts, and build failures.
Byte minimization runs libFuzzer mutation steps directly, without shell re-execution,
so paths with spaces remain literal arguments. Each candidate must be strictly
smaller and reproduce the original fingerprint; all mutation steps and intermediate
replays share one minimization deadline.
A successful triage publishes one uniquely named `<artifact>.triage-<id>/` directory
beside the original, containing `minimized.json` for structured targets or `minimized`
for byte targets, plus `metadata.json`. The metadata records the verified fingerprint,
commit, original path and input sizes; its minimized path is relative to that directory.
The complete pair appears in one rename, only after replay and metadata collection
succeed. Previous results are never overwritten or reused as minimizer output.
`--timeout SECONDS` sets each decode, shrink and replay watchdog (default 300 seconds,
excluding builds).
Then either fix the bug and commit a regression seed, or replay the minimized
scenario directly:

```sh
cargo run --manifest-path fuzz/Cargo.toml --features scenario-tools --bin replay_scenario -- \
  --target lua_loop /path/to/shrunk.json
# Or step through it visually (smelt_loop only):
cargo run --manifest-path fuzz/Cargo.toml --features scenario-tools --bin play_scenario -- /path/to/shrunk.json
```

## Regression seeds

Every fuzz-found bug gets a committed seed under
`fuzz/seeds/<target>/regression/`. Structured targets use JSON scenarios and
byte targets keep the exact minimized artifact. CI replays both forms on every
PR; running them locally is one command:

```sh
# Build every target, then replay all regression seeds:
cargo xtask fuzz replay-regression

# CI gate: prepare, compile, and replay every registered target:
cargo xtask fuzz verify

# Focus on selected targets and allow 60 seconds per seed:
cargo xtask fuzz replay-regression --timeout 60 text_ops lua_loop
```

Each seed runs in its own process with a 30-second default watchdog. A failing or
stuck seed does not prevent later seeds from replaying; interruption stops the
command immediately after subprocess cleanup. Nested regression directories are
supported, but symlinks, special files, unreadable entries, and non-JSON files for
structured targets are errors. Status and replay use the same seed enumeration.
`verify` accepts the same target filters and per-seed timeout as replay.

Seed presence never controls whether a target is compiled. The target registry
in `crates/xtask/src/fuzz/mod.rs` is checked against `fuzz/Cargo.toml` before any
fuzz command runs. See `fuzz/seeds/README.md` for the regression convention.

## Fuzz tooling integration lane

Ordinary workspace tests use isolated CLI fixtures and do not need nightly or
cargo-fuzz. The separate **Fuzz tooling integration** CI job explicitly runs an
ignored end-to-end test suite against real Cargo, cargo-fuzz, libFuzzer and ASan:

```sh
rustup toolchain install nightly
cargo install cargo-fuzz --version 0.13.2 --locked
rustup component add llvm-tools-preview --toolchain nightly
cargo test --locked -p xtask fuzz::real_cli_tests:: -- \
  --ignored --nocapture --test-threads=1
```

The tiny locked workspace in `crates/xtask/tests/fixtures/fuzz-triage` is copied
into a temporary repository with isolated build and data directories. It exercises
successful crash reduction, already-minimal input, complete metadata publication,
rejection of a changed failure identity without publication, and replay of the
published regression before and after fixing the fixture. Fork and merge tests
also verify real worker crashes and artifacts, and removal of redundant inputs.
The coverage test builds and replays the fixture, exports real LLVM totals, and
checks the published metadata, summary, and log.
Checkout, build, data, temporary and artifact paths contain spaces and shell
metacharacters. A C++17 probe round-trips empty arguments, quotes, backslashes,
Unicode, expansion syntax, newlines, and stdout/stderr redirection through the
runtime's command serializer and a real POSIX shell. No production targets or
shared corpus data are changed.

A deterministic custom mutator reduces the input to one byte. The `changed` mode
selects a distinct panic message at the same source location, while `fixed` mode
accepts the regression. These deliberate crashes exist only in the test fixture.
The fixture is outside the main workspace; check its formatting separately with
`cargo fmt --manifest-path crates/xtask/tests/fixtures/fuzz-triage/Cargo.toml -- --check`.

### Shared libFuzzer runtime

Production and the integration fixture both depend on `fuzz/vendor/libfuzzer-sys`.
This is the crates.io `libfuzzer-sys` 0.4.13 release (upstream
`rust-fuzz/libfuzzer` commit `719e4efb9b8857ebaa782ae59376c8cbb78fed0f`, crate
SHA-256 `a9fd2f41a1cba099f79a0b6b6c35656cf7c03351a7bae8ff0f28f25270f929d2`).
Upstream licenses and source provenance are retained in that directory.

The local `COMPAT(libfuzzer-shell-argv)` patch quotes argv and output-redirection
paths for libFuzzer's POSIX subprocess launcher. It covers fork, merge and other
`Command` consumers without replacing their driver algorithms. Windows behavior
is unchanged; Fuchsia uses direct argv execution. The build script tracks the
source directory, including patched headers, and upstream command tests account
for shell quoting. Those are the only modifications to the published crate.

The normal fixture parity test checks that both manifests refer to the same
canonical runtime source and both lockfiles resolve its version locally. Temporary
fixtures rebase that dependency to the same source, not a separate registry copy.
Real tests clear custom-runtime environment overrides to exercise this dependency.

To update, stage a published crate with `cargo vendor` in a temporary directory,
retain its licenses and provenance, and reapply the small patch if still needed.
The `.cargo-checksum.json` generated by `cargo vendor` is not retained: this is a
patched path dependency, not a Cargo registry source replacement. Update both
dependencies and lockfiles using `cargo add --path fuzz/vendor/libfuzzer-sys`
with each manifest, then run the normal parity test and the complete real lane.
See `docs/compat.md` for the removal condition.

## Coverage scoreboard

```sh
cargo xtask fuzz status

# Snapshot per-target source-code coverage to shared coverage-history/.
# Each run writes a text report plus JSON containing the commit, corpus digest,
# input count, status, and llvm-cov totals.
cargo xtask fuzz coverage-snapshot
cargo xtask fuzz coverage-snapshot --timeout 120 smelt_loop      # one target only
```

Install the matching LLVM tools once with
`rustup component add llvm-tools-preview --toolchain nightly`. Each selected
target gets an independent copy of its corpus; counts and digests describe those
exact bytes even while another campaign updates the live corpus. The timeout
(default 300 seconds) includes the cargo-fuzz coverage build and execution; LLVM
export has its own 30-second watchdog. Increase it for cold builds. A single
`llvm-cov export --summary-only` supplies typed totals for both JSON metadata and
the human-readable summary.

Each run publishes one uniquely named directory under `coverage-history/`, containing
`summary.txt`, `metadata.json` (schema 4), and per-target logs. Log paths in the metadata
are relative to that directory. The complete directory appears in one rename;
status ignores in-progress staging directories. Names remain unique across concurrent
worktrees and rapid successive runs.
Target failures, including corpus preparation and copying failures, still produce
metadata without discarding earlier results; later targets continue. Counts and
digests are null when no complete snapshot was created. Logs include setup failures
and bounded output from cargo-fuzz and LLVM export. Interruption publishes the results
collected so far before returning the signal exit code.
Overlapping coverage commands using the same checkout or build directory fail
rather than mixing binaries or profiles. Stale profiles are removed before each
run. Use the wrapper consistently: raw `cargo fuzz coverage` does not take its lock.
Artifacts go only to that target's shared artifact directory; unrelated checkout
files are never moved into another target's results.

## Lower-level

```sh
# Build every registered fuzz target in one cargo-fuzz invocation.
cargo xtask fuzz build

# Raw structured shrinker; the predicate preserves normalized panic identity.
cargo run --manifest-path fuzz/Cargo.toml --release --features scenario-tools --bin shrink_scenario -- \
  --target lua_loop in.json out.min.json

# Headless replay (exits non-zero on panic; what triage and CI use):
cargo run --manifest-path fuzz/Cargo.toml --features scenario-tools --bin replay_scenario -- \
  --target lua_loop in.json

# Prefer the wrappers so custom corpus and artifact paths are always supplied:
cargo xtask fuzz run smelt_loop --cmin --sanitizer none -max_total_time=3600
cargo xtask fuzz coverage-snapshot smelt_loop
```
