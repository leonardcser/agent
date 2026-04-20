# Lua plugin examples

Drop any of these files into `~/.config/smelt/init.lua` (or `dofile` them from your own `init.lua`) to try them out.

- **per_project.lua** — auto-load `$PWD/.smelt/init.lua` on top of the user config.
- **mode_keybinds.lua** — `<C-y>` copies transcript or prompt depending on focused window, demonstrating `smelt.api.win.focus()` for context-aware keybinds.
- **yank_block.lua** — `<Space>y` yanks the block under the cursor using `/yank-block`.
- **statusline.lua** — custom status bar showing the current directory path, git branch, and clock via `smelt.statusline(fn)`.
- **override.lua** — register a custom command (`/hello`) and remap a keybind (`<C-s>` to `/fork`).

The Lua surface (`smelt.api.version`, `smelt.notify`, `smelt.api.cmd.register`, `smelt.api.cmd.run`, `smelt.api.cmd.list`, `smelt.keymap`, `smelt.on`, `smelt.defer`, `smelt.clipboard`, `smelt.api.transcript.text`, `smelt.api.buf.text`, `smelt.api.win.focus`, `smelt.api.win.mode`, `smelt.statusline`) is documented in `crates/tui/src/lua.rs`.

`smelt.keymap(mode, chord, fn)` — mode is `"n"` (Normal), `"i"` (Insert), `"v"` (Visual), or `""` (any mode).
