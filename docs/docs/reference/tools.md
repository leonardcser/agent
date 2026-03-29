# Tools Reference

## File I/O

### `read_file`

Reads a file and returns its contents with line numbers. Supports text files and
image files (png, jpg, gif, webp, bmp, tiff, svg). Use `offset` and `limit` to
read specific ranges of large files.

### `write_file`

Creates or overwrites a file. In the confirm dialog, a syntax-highlighted
preview of the new content is shown.

### `edit_file`

Applies diff-based edits to an existing file. The confirm dialog shows a
scrollable inline diff (old vs. new).

### `notebook_edit`

Edits Jupyter notebook (`.ipynb`) cells. Supports replacing, inserting, and
deleting cells. Identify cells by `cell_id` or `cell_number` (0-indexed).

| Parameter       | Description                                                                              |
| --------------- | ---------------------------------------------------------------------------------------- |
| `notebook_path` | Absolute path to the notebook                                                            |
| `cell_number`   | 0-indexed cell number (used when `cell_id` is omitted)                                   |
| `cell_id`       | Cell ID (takes precedence over `cell_number`; for insert, new cell goes after this cell) |
| `new_source`    | New source content (required for replace and insert)                                     |
| `cell_type`     | `code` or `markdown` (required for insert, defaults to current type for replace)         |
| `edit_mode`     | `replace` (default), `insert`, or `delete`                                               |

When reading, cells are displayed as numbered blocks with their type, source
content, and outputs. Supports offset/limit slicing.

## Search

### `glob`

Finds files matching glob patterns (e.g., `**/*.rs`, `src/**/*.ts`). Returns
matching paths.

### `grep`

Searches file contents with regex. Returns matching lines with context.

## Execution

### `bash`

Runs a shell command and streams output in real-time.

**Behavior details:**

- Default timeout: 120 seconds (max: 600 seconds)
- Interactive commands are blocked (vim, nano, less, etc.)
- Shell backgrounding (`&`) is rejected — use `run_in_background` instead
- Output is line-buffered (stdout and stderr multiplexed)
- Non-zero exit codes are flagged as errors
- Cancellable via the UI

Set `run_in_background` to `true` to run the command asynchronously. Returns
immediately with a process ID. Use `read_process_output` and `stop_process` to
manage it. Monitor all background processes with `/ps`.

### `read_process_output`

Reads buffered output from a background process. Supports blocking reads with an
optional timeout.

### `stop_process`

Kills a running background process.

## Web

### `web_fetch`

Fetches a URL and extracts content based on your `prompt`.

| Parameter | Description                                            |
| --------- | ------------------------------------------------------ |
| `url`     | URL to fetch (required)                                |
| `prompt`  | What to extract from the page (required)               |
| `format`  | Output format: `markdown` (default), `text`, or `html` |
| `timeout` | Timeout in seconds (default: 30, max: 120)             |

**Limits:**

- Response body capped at 5 MB
- Output capped at 2,000 lines or 50 KB (truncation noted)
- Results are cached by URL and format for 15 minutes

### `web_search`

Searches the web via DuckDuckGo. Returns results with title, URL, and
description. Results are cached for 15 minutes.

## Interaction

### `ask_user_question`

Asks you a question with selectable options. Supports single-select and
multi-select modes (up to 4 questions per call). Available in interactive mode
only.

## Knowledge

### `load_skill`

Loads a skill by name to give the agent specialized instructions and knowledge.
See [Skills](configuration.md#skills) in the configuration reference for how to
create and organize skills.

## Multi-Agent

These tools are only available when `--multi-agent` is enabled.

### `spawn_agent`

Spawns a new subagent to work on a task. Give it a well-scoped task with all the
context it needs. Set `wait` to `true` to block until the agent finishes.
Subagents persist and build context — reuse them via `message_agent`.

### `list_agents`

Lists agents in the current workspace with their name, status, and task slug.
Shows both owned subagents and discovered peers.

### `message_agent`

Sends a message to one or more agents by name. Use to steer subagents, provide
information, or coordinate work.

### `peek_agent`

Inspects another agent's context without interrupting it. Useful for checking
what a subagent knows or is working on.

### `stop_agent`

Terminates a subagent and all its children.

## Mode-Specific

### `exit_plan_mode`

Plan mode only. Called by the agent when its plan is ready for your review.
Takes a required `plan_summary` parameter. The confirm dialog renders the plan
as markdown. Approving switches to Apply mode.
