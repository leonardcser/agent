# Commands

Type `/` to open the command picker with fuzzy search.

## Built-in Commands

| Command                   | Description                                   |
| ------------------------- | --------------------------------------------- |
| `/clear`, `/new`          | Start a new conversation                      |
| `/rewind`                 | Rewind to a previous turn (same as `Esc Esc`) |
| `/resume`                 | Resume a saved session                        |
| `/compact [instructions]` | Summarize older history to free context       |
| `/fork`, `/branch`        | Fork the current session                      |
| `/export`                 | Export conversation (clipboard or file)       |
| `/model [provider/model]` | Switch model (opens picker if no name given)  |
| `/settings`               | Toggle runtime settings                       |
| `/theme [name]`           | Change accent color                           |
| `/color [name]`           | Set task slug color                           |
| `/stats`                  | Show token usage, cost, and activity history  |
| `/cost`                   | Show current session cost                     |
| `/vim`                    | Toggle vim mode                               |
| `/thinking`               | Toggle display of thinking blocks             |
| `/permissions`            | Manage saved permissions                      |
| `/ps`                     | Manage background processes                   |
| `/agents`                 | Manage running agents (multi-agent only)      |
| `/btw <question>`         | Ask a side question (not added to history)    |
| `/exit`, `/quit`          | Exit (also `:q`, `:qa`, `:wq`, `:wqa`)        |

## Shell Escape

Prefix with `!` to run a shell command directly, without going through the
agent. Output appears inline in the conversation.

```
!git status
!cargo test
```

Shell escapes also work while the agent is running.

## Custom Commands

Create `.md` files in `~/.config/smelt/commands/` and they become slash
commands. See the
[Customization guide](../guide/customization.md#custom-commands) for an example.

### Frontmatter

All fields are optional:

| Key                | Description                                                                                               |
| ------------------ | --------------------------------------------------------------------------------------------------------- |
| `description`      | Shown in the `/` picker                                                                                   |
| `model`            | Override model for this command. Prefer `provider_name/model_name`; bare names work only when unambiguous |
| `provider`         | Legacy compatibility field to narrow a bare `model` reference                                             |
| `temperature`      | Sampling temperature                                                                                      |
| `top_p`            | Top-p (nucleus) sampling                                                                                  |
| `top_k`            | Top-k sampling                                                                                            |
| `min_p`            | Min-p sampling                                                                                            |
| `repeat_penalty`   | Repetition penalty                                                                                        |
| `reasoning_effort` | Thinking depth: `off`/`low`/`medium`/`high`/`max`                                                         |
| `tools`            | `allow`/`ask`/`deny` lists for tool permissions                                                           |
| `bash`             | `allow`/`ask`/`deny` glob patterns for bash                                                               |
| `web_fetch`        | `allow`/`ask`/`deny` glob patterns for URLs                                                               |

### Shell Execution in Templates

- **Inline**: `` !`command` `` — output replaces the backtick expression
- **Fenced**: ` ```! ` code block — output replaces the block
- **Escape**: `` \!`command` `` — prevents execution
