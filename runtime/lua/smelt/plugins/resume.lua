-- Built-in /resume command.
--
-- Live-filtered session picker. The input panel above captures typing;
-- the list panel below shows matches. Enter loads the highlighted
-- session, Ctrl-d deletes it, Alt-w toggles the workspace-only
-- filter. Fuzzy search uses `smelt.fuzzy.score`.

local function is_junk(s)
  if s == nil then return true end
  local t = s:match("^%s*(.-)%s*$") or ""
  if t == "" then return true end
  if t:lower() == "untitled" then return true end
  local first = t:sub(1, 1)
  if first == "/" or first == "\0" then return true end
  return false
end

local function display_title(entry)
  local raw
  if not is_junk(entry.title) then
    raw = entry.title
  elseif not is_junk(entry.subtitle) then
    raw = entry.subtitle
  else
    return "Untitled"
  end
  local line = raw:match("([^\n]*)") or raw
  return (line:match("^%s*(.-)%s*$") or line)
end

local function format_size(bytes)
  if bytes == nil or bytes <= 0 then return "" end
  if bytes < 1024 then return string.format("%dB", bytes) end
  if bytes < 1024 * 1024 then return string.format("%.1fK", bytes / 1024) end
  return string.format("%.1fM", bytes / 1024 / 1024)
end

local function time_ago(ts_ms, now_ms)
  if ts_ms == nil or ts_ms <= 0 then return "" end
  local delta = math.max(0, (now_ms - ts_ms) / 1000)
  if delta < 60 then return string.format("%ds", math.floor(delta)) end
  if delta < 3600 then return string.format("%dm", math.floor(delta / 60)) end
  if delta < 86400 then return string.format("%dh", math.floor(delta / 3600)) end
  return string.format("%dd", math.floor(delta / 86400))
end

local LEADING, SIZE_COL, TIME_COL, GAP = 2, 8, 7, 2

local function format_row(entry, now_ms)
  local title = display_title(entry)
  local size_str = format_size(entry.size_bytes)
  local time_str = time_ago(
    (entry.updated_at_ms > 0) and entry.updated_at_ms or entry.created_at_ms,
    now_ms
  )
  -- size right-aligned (column edge), time left-aligned so the
  -- size↔time gap is exactly GAP. Title still starts at a fixed
  -- column because TIME_COL pads on the right.
  return string.format(
    "%s%" .. SIZE_COL .. "s%s%-" .. TIME_COL .. "s%s%s",
    string.rep(" ", LEADING),
    size_str,
    string.rep(" ", GAP),
    time_str,
    string.rep(" ", GAP),
    title
  )
end

local function filter_entries(entries, query, workspace_only, current_cwd)
  local filtered = {}
  for _, e in ipairs(entries) do
    local keep = true
    if workspace_only then
      keep = (e.cwd == current_cwd)
    end
    if keep and query ~= "" then
      -- Fuzzy-score the title+subtitle. Non-match returns nil.
      local hay = display_title(e) .. " " .. (e.subtitle or "")
      keep = smelt.fuzzy.score(hay, query) ~= nil
    end
    if keep then
      table.insert(filtered, e)
    end
  end
  return filtered
end

local function refresh_list(buf_id, filtered, now_ms)
  local lines = {}
  if #filtered == 0 then
    table.insert(lines, "  (no matching sessions)")
    smelt.buf.set_lines(buf_id, lines)
    return
  end
  for _, e in ipairs(filtered) do
    table.insert(lines, format_row(e, now_ms))
  end
  smelt.buf.set_lines(buf_id, lines)
  -- Dim the size + duration columns so the title reads as the primary
  -- content. Columns: `LEADING + SIZE_COL + GAP + TIME_COL` spans
  -- [0, LEADING+SIZE_COL+GAP+TIME_COL), everything after is the title.
  local meta_end = LEADING + SIZE_COL + GAP + TIME_COL
  for i = 1, #filtered do
    smelt.buf.add_dim(buf_id, i, 0, meta_end)
  end
end

smelt.cmd.register("resume", function()
  smelt.spawn(function()
    local entries = smelt.session.list()
    if #entries == 0 then
      smelt.notify_error("no saved sessions")
      return
    end

    local current_cwd = smelt.session.cwd()
    local now_ms = os.time() * 1000
    local workspace_only = true
    local query = ""
    local filtered = filter_entries(entries, query, workspace_only, current_cwd)

    local list_buf = smelt.buf.create()
    -- Seed the buffer on the same tick the dialog opens. The op
    -- reducer drains this BufSetLines before `dialog.open` is
    -- serviced, so the list shows its initial rows without a flicker.
    refresh_list(list_buf, filtered, now_ms)

    local function selected_entry(idx)
      return (idx and idx > 0) and filtered[idx] or nil
    end

    local result = smelt.ui.dialog.open({
      title   = "resume",
      panels  = {
        { kind = "input", name = "query",
          placeholder = "filter by title…",
          on_change = function(ctx)
            query = (ctx.inputs and ctx.inputs.query) or ""
            filtered = filter_entries(entries, query, workspace_only, current_cwd)
            refresh_list(list_buf, filtered, now_ms)
          end },
        { kind = "list", buf = list_buf, height = "fill", focus = true },
      },
      keymaps = {
        { key = "alt-w", hint = "⌥w: toggle workspace filter", on_press = function(ctx)
            workspace_only = not workspace_only
            filtered = filter_entries(entries, query, workspace_only, current_cwd)
            refresh_list(list_buf, filtered, now_ms)
          end },
        { key = "ctrl-d", hint = "^d: delete", on_press = function(ctx)
            local e = selected_entry(ctx.selected_index)
            if e then
              smelt.session.delete(e.id)
              for i, x in ipairs(entries) do
                if x.id == e.id then table.remove(entries, i); break end
              end
              filtered = filter_entries(entries, query, workspace_only, current_cwd)
              refresh_list(list_buf, filtered, now_ms)
            end
          end },
      },
    })

    if result.action == "dismiss" then return end
    local idx = result.option_index
    local e = selected_entry(idx)
    if e then
      smelt.session.load(e.id)
    end
  end)
end, { desc = "resume saved session" })
