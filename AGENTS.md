## Commands

```bash
# build
cargo build

# test (requires `cargo install cargo-nextest` — much faster than `cargo test`)
cargo nextest run --workspace

# format and lint
cargo fmt && cargo clippy --workspace --all-targets -- -D warnings
```

Whenever you add a new user-facing feature or change user-facing behavior,
update the README.md and the docs/ folder. Don't document internal
implementation details — only things end users need to know.
