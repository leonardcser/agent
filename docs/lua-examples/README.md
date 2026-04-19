# Lua plugin examples

Drop any of these files into `~/.config/smelt/init.lua` (or `dofile` them from your own `init.lua`) to try them out.

- **leader.lua** — vim-style `<Space>nn` / `<Space>ll` leader chords.
- **block_summarizer.lua** — hooks the `block_done` autocmd to react when a transcript block finishes streaming.
- **per_project.lua** — auto-load `$PWD/.smelt/init.lua` on top of the user config.
- **double_compact.lua** — register a `/double_compact` user command that calls `smelt.api.cmd.run("/compact")` twice via the command queue.
- **copy_transcript.lua** — `/copy-transcript` copies the full conversation to the system clipboard using `smelt.api.transcript.text()` and `smelt.clipboard()`.
- **mode_keybinds.lua** — `<C-y>` copies transcript or prompt depending on focused window, demonstrating `smelt.api.win.focus()` for context-aware keybinds.
- **yank_block.lua** — `<Space>y` yanks the block under the cursor using `/yank-block`.
- **statusline.lua** — custom status bar showing the current directory path, git branch, and clock via `smelt.statusline(fn)`.

The Lua surface (`smelt.api.version`, `smelt.notify`, `smelt.api.cmd.register`, `smelt.api.cmd.run`, `smelt.keymap`, `smelt.on`, `smelt.defer`, `smelt.clipboard`, `smelt.api.transcript.text`, `smelt.api.buf.text`, `smelt.api.win.focus`, `smelt.api.win.mode`, `smelt.statusline`) is documented in `crates/tui/src/lua.rs`.

`smelt.keymap(mode, chord, fn)` — mode is `"n"` (Normal), `"i"` (Insert), `"v"` (Visual), or `""` (any mode).
