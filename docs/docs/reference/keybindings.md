# Keybindings

## Input

### General

| Key                             | Action                             |
| ------------------------------- | ---------------------------------- |
| `Enter`                         | Submit message                     |
| `Ctrl+J` / `Shift+Enter`        | Insert newline                     |
| `Ctrl+R`                        | Fuzzy search history               |
| `Ctrl+S`                        | Stash / unstash input              |
| `Ctrl+C`                        | Clear input / cancel agent / quit  |
| `Ctrl+L`                        | Redraw screen                      |
| `Ctrl+T`                        | Cycle reasoning effort             |
| `Shift+Tab`                     | Cycle mode                         |
| `Enter` (empty prompt)          | Pop and send next queued message   |
| `Esc`                           | Unqueue messages or dismiss dialog |
| `Esc Esc`                       | Cancel agent / compaction / rewind |
| `↑` / `↓` / `Ctrl+P` / `Ctrl+N` | Navigate input history             |
| `Tab`                           | Accept completion / ghost text     |
| `?`                             | Open help (empty input only)       |
| `Cmd+V`                         | Paste clipboard (image or text)    |

### Cursor

| Key                            | Action                 |
| ------------------------------ | ---------------------- |
| `Ctrl+A` / `Home` / `Cmd+Left` | Beginning of line      |
| `Ctrl+E` / `End` / `Cmd+Right` | End of line            |
| `Ctrl+F` / `Right`             | Forward one character  |
| `Ctrl+B` / `Left`              | Backward one character |
| `Alt+F` / `Alt+Right`          | Forward one word       |
| `Alt+B` / `Alt+Left`           | Backward one word      |
| `Cmd+Up`                       | Start of buffer        |
| `Cmd+Down`                     | End of buffer          |

### Editing

| Key                                           | Action                     |
| --------------------------------------------- | -------------------------- |
| `Backspace`                                   | Delete backward            |
| `Delete` / `Ctrl+D`                           | Delete forward             |
| `Alt+Backspace` / `Ctrl+W` / `Ctrl+Backspace` | Delete word backward       |
| `Alt+D` / `Alt+Delete`                        | Delete word forward        |
| `Cmd+Backspace`                               | Delete to start of line    |
| `Ctrl+K`                                      | Kill to end of line        |
| `Ctrl+U`                                      | Kill to start of line      |
| `Ctrl+Y`                                      | Yank (paste killed text)   |
| `Alt+Y`                                       | Yank-pop (cycle kill ring) |
| `Alt+U`                                       | Uppercase word             |
| `Alt+L`                                       | Lowercase word             |
| `Alt+C`                                       | Capitalize word            |
| `Ctrl+_`                                      | Undo                       |
| `Ctrl+X Ctrl+E`                               | Edit in `$EDITOR`          |

### Selection (non-vim)

| Key                                                                           | Action                  |
| ----------------------------------------------------------------------------- | ----------------------- |
| `Shift+Left` / `Shift+Right`                                                  | Select character        |
| `Shift+Alt+Left` / `Shift+Alt+Right` / `Shift+Ctrl+Left` / `Shift+Ctrl+Right` | Select word             |
| `Shift+Home` / `Shift+End`                                                    | Select to line boundary |
| `Cmd+C`                                                                       | Copy                    |
| `Cmd+X`                                                                       | Cut                     |

!!! note

    `Cmd` keybindings depend on terminal support. Some terminals intercept
    `Cmd` combinations for their own features (tabs, scrollback). If a `Cmd`
    binding doesn't work, check your terminal's settings.

## Vim Mode

Toggle with `/vim` or set `settings.vim_mode` in config. Supports insert,
normal, and visual modes. When in normal mode, these keys change behavior:

| Key      | Vim normal        | Insert / non-vim      |
| -------- | ----------------- | --------------------- |
| `Ctrl+U` | Half-page up      | Kill to start of line |
| `Ctrl+D` | Half-page down    | Delete forward        |
| `Ctrl+J` | History next      | Insert newline        |
| `Ctrl+K` | History prev      | Kill to end of line   |
| `Ctrl+R` | Redo              | History search        |
| `v`      | Edit in `$EDITOR` | —                     |
| `Ctrl+A` | No-op             | Start of line         |
| `Ctrl+E` | No-op             | End of line           |
| `Ctrl+W` | No-op             | Delete word backward  |
| `Ctrl+Y` | No-op             | Yank                  |

Full vim support: motions, operators (`d`, `c`, `y`), text objects (`iw`,
`a(`…), find (`f`, `t`, `F`, `T`, `;`, `,`), and commands (`x`, `s`, `r`, `p`,
`u`, `~`, `J`, etc.).

Yank / paste are mirrored with the system clipboard: `y` / `yy` / `d` / `x`
push to the clipboard, and `p` / `P` read from it. If the clipboard was
updated externally since your last yank, `p` pastes the external text
(charwise); otherwise vim's linewise flag from `yy` is preserved. The emacs
`Ctrl+Y` yank behaves the same way.

## Dialogs

### Common

| Key                                   | Action        |
| ------------------------------------- | ------------- |
| `↑` / `k` / `Ctrl+P`                  | Previous item |
| `↓` / `j` / `Ctrl+N`                  | Next item     |
| `Ctrl+U` / `Ctrl+D` / `PgUp` / `PgDn` | Page scroll   |
| `Enter`                               | Confirm       |
| `Esc` / `Ctrl+C`                      | Cancel        |

### Per-Dialog

| Dialog      | Key                               | Action                     |
| ----------- | --------------------------------- | -------------------------- |
| Help        | `q` / `?`                         | Close                      |
| Confirm     | `Tab`                             | Attach message to approval |
| Question    | `Space`                           | Toggle option              |
| Question    | `1`–`9`                           | Jump to option             |
| Question    | `←`/`→`/`h`/`l`/`Tab`/`Shift+Tab` | Switch questions           |
| Permissions | `dd` / `⌫`                        | Delete permission          |
| Permissions | `q`                               | Close                      |
| Ps          | `⌫`                               | Kill process               |
| Resume      | `dd` / `⌫`                        | Delete session             |
| Resume      | `Ctrl+W`                          | Toggle workspace filter    |
| Resume      | `q`                               | Close                      |
| Resume      | (type)                            | Fuzzy search               |
| Rewind      | `↑` / `↓`                         | Select turn                |
| Rewind      | `Enter`                           | Rewind to turn             |

## Completer

| Key                       | Action                |
| ------------------------- | --------------------- |
| `Tab`                     | Accept                |
| `Enter`                   | Accept + submit       |
| `↑` / `Ctrl+K` / `Ctrl+P` | Previous              |
| `↓` / `Ctrl+J` / `Ctrl+N` | Next                  |
| `Ctrl+R`                  | Cycle history matches |
| `Esc`                     | Cancel                |

## Menu (Settings / Model / Theme)

| Key                  | Action                           |
| -------------------- | -------------------------------- |
| `↑` / `k` / `Ctrl+P` | Previous                         |
| `↓` / `j` / `Ctrl+N` | Next                             |
| `Enter`              | Select / toggle                  |
| `Space`              | Toggle (settings)                |
| `Tab`                | Cycle auxiliary (e.g. reasoning) |
| `Esc` / `q`          | Dismiss                          |
