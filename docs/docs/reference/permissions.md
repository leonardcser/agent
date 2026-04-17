# Permissions Reference

The permission system controls what the agent can do without asking. Each
[mode](../guide/usage.md#modes) has its own rules.

## How Rules Work

Three categories: **tools**, **bash**, and **web_fetch**. Each category has
three rule lists:

- **allow** — execute silently
- **ask** — prompt for confirmation
- **deny** — block (deny always wins over allow)

Rules use glob patterns. Anything not matched defaults to **Ask** in
Normal/Plan/Apply or **Allow** in Yolo.

## Default Tool Permissions

| Tool                  | Normal | Plan  | Apply | Yolo  |
| --------------------- | ------ | ----- | ----- | ----- |
| `read_file`           | Allow  | Allow | Allow | Allow |
| `edit_file`           | Ask    | Ask   | Allow | Allow |
| `write_file`          | Ask    | Ask   | Allow | Allow |
| `edit_notebook`       | Ask    | Ask   | Ask   | Allow |
| `glob`                | Allow  | Allow | Allow | Allow |
| `grep`                | Allow  | Allow | Allow | Allow |
| `bash`                | Ask    | Ask   | Ask   | Allow |
| `web_fetch`           | Ask    | Ask   | Ask   | Allow |
| `web_search`          | Ask    | Ask   | Ask   | Allow |
| `ask_user_question`   | Allow  | Allow | Allow | Allow |
| `exit_plan_mode`      | —      | Ask   | —     | —     |
| `read_process_output` | Ask    | Ask   | Ask   | Allow |
| `stop_process`        | Ask    | Ask   | Ask   | Allow |
| `spawn_agent`\*       | Allow  | Allow | Allow | Allow |
| `list_agents`\*       | Allow  | Allow | Allow | Allow |
| `message_agent`\*     | Allow  | Allow | Allow | Allow |
| `peek_agent`\*        | Allow  | Allow | Allow | Allow |
| `load_skill`          | Ask    | Ask   | Ask   | Allow |
| `stop_agent`\*        | Allow  | Allow | Allow | Allow |

\*Only registered when `--multi-agent` is enabled. — = not available in that
mode.

## Default Bash Patterns

Read-only commands with no side effects are allowed by default. Commands that
can modify files, install packages, or affect system state require approval.

| Pattern       | Normal | Plan  | Apply | Yolo  |
| ------------- | ------ | ----- | ----- | ----- |
| `ls *`        | Allow  | Allow | Allow | Allow |
| `find *`      | Allow  | Allow | Allow | Allow |
| `tree *`      | Allow  | Allow | Allow | Allow |
| `cat *`       | Allow  | Allow | Allow | Allow |
| `head *`      | Allow  | Allow | Allow | Allow |
| `tail *`      | Allow  | Allow | Allow | Allow |
| `less *`      | Allow  | Allow | Allow | Allow |
| `grep *`      | Allow  | Allow | Allow | Allow |
| `sort *`      | Allow  | Allow | Allow | Allow |
| `uniq *`      | Allow  | Allow | Allow | Allow |
| `wc *`        | Allow  | Allow | Allow | Allow |
| `diff *`      | Allow  | Allow | Allow | Allow |
| `tr *`        | Allow  | Allow | Allow | Allow |
| `cut *`       | Allow  | Allow | Allow | Allow |
| `jq *`        | Allow  | Allow | Allow | Allow |
| `echo *`      | Allow  | Allow | Allow | Allow |
| `pwd *`       | Allow  | Allow | Allow | Allow |
| `which *`     | Allow  | Allow | Allow | Allow |
| `dirname *`   | Allow  | Allow | Allow | Allow |
| `basename *`  | Allow  | Allow | Allow | Allow |
| `realpath *`  | Allow  | Allow | Allow | Allow |
| `stat *`      | Allow  | Allow | Allow | Allow |
| `file *`      | Allow  | Allow | Allow | Allow |
| `test *`      | Allow  | Allow | Allow | Allow |
| `du *`        | Allow  | Allow | Allow | Allow |
| `df *`        | Allow  | Allow | Allow | Allow |
| `date *`      | Allow  | Allow | Allow | Allow |
| `whoami *`    | Allow  | Allow | Allow | Allow |
| `sha256sum *` | Allow  | Allow | Allow | Allow |
| `md5sum *`    | Allow  | Allow | Allow | Allow |
| `xxd *`       | Allow  | Allow | Allow | Allow |
| `hexdump *`   | Allow  | Allow | Allow | Allow |
| `strings *`   | Allow  | Allow | Allow | Allow |
| _other_       | Ask    | Ask   | Ask   | Allow |

!!! note

    In Normal and Plan modes, allowed bash commands that contain output
    redirection (`>`, `>>`, `&>`) are automatically escalated to Ask.

## Configuring Permissions

```yaml
permissions:
  default:
    tools:
      allow: [web_search]
    web_fetch:
      allow: ["*"]
    bash:
      allow: ["git log *", "git diff *"]
  apply:
    bash:
      allow: ["git commit *"]
```

`default` applies to all modes. Mode-specific rules (`normal`, `plan`, `apply`,
`yolo`) are merged on top: their allow/ask/deny lists are appended to the
default lists. Since deny always wins, a mode-level deny overrides a
default-level allow for the same entry.

## Approval Scopes

When the confirm dialog appears, you can choose how broadly to approve:

| Scope         | Lifetime                        | Storage                                                   |
| ------------- | ------------------------------- | --------------------------------------------------------- |
| **Once**      | This call only                  | —                                                         |
| **Session**   | Until `/clear`, `/new`, or exit | Memory                                                    |
| **Workspace** | All future sessions in this CWD | `~/.local/state/smelt/workspaces/<hash>/permissions.json` |

## Managing Permissions

Use `/permissions` to view and delete saved permissions:

- `j`/`k` to navigate
- `dd` or `Backspace` to delete
- `Esc` to close

## Workspace Restriction

When `restrict_to_workspace` is enabled (default), any tool call targeting a
path outside the current workspace has its permission downgraded from Allow to
Ask.

Workspace approvals stay narrow: approving a command pattern only approves that
pattern, and approving an outside directory only approves access to that
directory.

!!! warning

    **Best-effort safety measure.** Shell commands, symlinks, and indirect
    access can bypass workspace restriction.

## Isolation

Permissions and workspace restriction guard against accidental mistakes, not
against an agent that actively tries to escape. Any approved bash command runs
with your user's privileges, so a script like `rm -rf ~` works exactly as it
would if you typed it yourself.

For untrusted prompts, models, or MCP servers, run smelt inside a container or
VM. Anything else is defense in depth, not a sandbox.
