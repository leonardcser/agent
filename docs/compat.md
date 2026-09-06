# Compatibility debt

Compatibility code we intend to remove while smelt is alpha is marked with
`COMPAT(<id>)` and documented here.

## `lua-session-turn-block-idx`

`smelt.session.turns()` exposes deprecated `block_idx` as an alias of the
canonical `history_idx`. This prevents older rewind dialogs from passing a
missing value to `smelt.session.rewind_to()` and accidentally rewinding to the
start of the session. Remove the alias after third-party dialogs have migrated
to `history_idx` and the old field has passed through a documented deprecation
window.

## `libfuzzer-shell-argv`

`fuzz/vendor/libfuzzer-sys` contains the published 0.4.13 crate, patched so
`Command::toString()` quotes literal arguments and output-redirection paths for
POSIX shells. Upstream's unquoted launcher breaks fork campaigns and corpus merges
when checkout, build or data paths contain spaces or shell metacharacters. The
build script also tracks headers, and upstream command tests match the quoting.
Windows execution is unchanged; Fuchsia already executes argv directly.

Production and the isolated integration fixture depend on this same local package.
The ordinary fixture parity test checks their canonical source paths and resolved
lockfile versions. The opt-in real integration lane exercises fork, merge, triage,
and shell argument/redirection round trips.

Remove the vendored package and this marker when a published upstream release
preserves literal POSIX argv and output paths and passes that integration lane.
Switch both dependencies together using Cargo, update both lockfiles, and retain
a resolved-runtime parity check. See `fuzz/README.md` for source provenance and
update instructions.
