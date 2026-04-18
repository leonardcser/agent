# Lua plugin examples

Drop any of these files into `~/.config/smelt/init.lua` (or `dofile` them from your own `init.lua`) to try them out.

- **leader.lua** — vim-style `<Space>nn` / `<Space>ll` leader chords.
- **block_summarizer.lua** — hooks the `block_done` autocmd to react when a transcript block finishes streaming.
- **per_project.lua** — auto-load `$PWD/.smelt/init.lua` on top of the user config.

The Lua surface (`smelt.api.version`, `smelt.notify`, `smelt.api.cmd.register`, `smelt.keymap`, `smelt.on`, `smelt.defer`) is documented in `crates/tui/src/lua.rs`.
