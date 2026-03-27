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

| Tool | Normal | Plan | Apply | Yolo |
| --- | --- | --- | --- | --- |
| `read_file` | Allow | Allow | Allow | Allow |
| `edit_file` | Ask | Ask | Allow | Allow |
| `write_file` | Ask | Ask | Allow | Allow |
| `notebook_edit` | Ask | Ask | Ask | Allow |
| `glob` | Allow | Allow | Allow | Allow |
| `grep` | Allow | Allow | Allow | Allow |
| `bash` | Ask | Ask | Ask | Allow |
| `web_fetch` | Ask | Ask | Ask | Allow |
| `web_search` | Ask | Ask | Ask | Allow |
| `ask_user_question` | Allow | Allow | Allow | Allow |
| `exit_plan_mode` | — | Ask | — | — |
| `read_process_output` | Ask | Ask | Ask | Allow |
| `stop_process` | Ask | Ask | Ask | Allow |
| `spawn_agent`\* | Allow | Allow | Allow | Allow |
| `list_agents`\* | Allow | Allow | Allow | Allow |
| `message_agent`\* | Allow | Allow | Allow | Allow |
| `peek_agent`\* | Allow | Allow | Allow | Allow |
| `load_skill` | Ask | Ask | Ask | Allow |
| `stop_agent`\* | Allow | Allow | Allow | Allow |

\*Only registered when `--multi-agent` is enabled.
— = not available in that mode.

## Default Bash Patterns

| Pattern | Normal | Plan | Apply | Yolo |
| --- | --- | --- | --- | --- |
| `ls *` | Allow | Allow | Allow | Allow |
| `grep *` | Allow | Allow | Allow | Allow |
| `find *` | Allow | Allow | Allow | Allow |
| `cat *` | Allow | Allow | Allow | Allow |
| `tail *` | Allow | Allow | Allow | Allow |
| `head *` | Allow | Allow | Allow | Allow |
| _other_ | Ask | Ask | Ask | Allow |

!!! note

    In Normal and Plan modes, allowed bash commands that contain output
    redirection (`>`, `>>`, `&>`) are automatically escalated to Ask.

## Configuring Permissions

```yaml
permissions:
  normal:
    tools:
      allow: [read_file, glob, grep]
      ask: [edit_file, write_file]
      deny: []
    bash:
      allow: ["ls *", "grep *", "find *"]
      ask: []
      deny: []
    web_fetch:
      allow: ["https://docs.rs/*"]
      deny: ["https://evil.com/*"]
```

Each mode (`normal`, `plan`, `apply`, `yolo`) has the same structure. Omitted
categories use their defaults.

## Approval Scopes

When the confirm dialog appears, you can choose how broadly to approve:

| Scope | Lifetime | Storage |
| --- | --- | --- |
| **Once** | This call only | — |
| **Session** | Until `/clear`, `/new`, or exit | Memory |
| **Workspace** | All future sessions in this CWD | `~/.local/state/agent/workspaces/<hash>/permissions.json` |

The workspace hash is a SHA256 prefix of the working directory path.

## Managing Permissions

Use `/permissions` to view and delete saved permissions:

- `j`/`k` to navigate
- `dd` or `Backspace` to delete
- `Esc` to close

## Workspace Restriction

When `restrict_to_workspace` is enabled (default), any tool call targeting a
path outside the current workspace has its permission downgraded from Allow to
Ask.

!!! warning

    **Best-effort safety measure.** Shell commands, symlinks, and indirect
    access can bypass workspace restriction. Use a container for strong
    isolation.
