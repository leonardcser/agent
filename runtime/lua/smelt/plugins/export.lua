-- Built-in /export command.
--
-- Opens a 2-option dialog: copy the conversation as markdown to the
-- clipboard, or write it to a timestamped file in the cwd. Pure Lua
-- composition over primitives (session metadata, engine.history(),
-- smelt.clipboard, io.open) — the host has no export-specific Rust.

local function format_timestamp(ms)
  if ms == nil or ms <= 0 then
    return os.date("%Y-%m-%dT%H:%M:%S")
  end
  return os.date("%Y-%m-%dT%H:%M:%S", math.floor(ms / 1000))
end

local function slugify(title)
  if not title or title == "" then
    return "conversation"
  end
  local out = title:lower():gsub("[^%w%-]+", "-"):gsub("^%-+", ""):gsub("%-+$", "")
  if #out > 40 then
    out = out:sub(1, 40):gsub("%-+$", "")
  end
  if out == "" then
    return "conversation"
  end
  return out
end

local function file_stamp(ms)
  local secs = (ms and ms > 0) and math.floor(ms / 1000) or os.time()
  return os.date("%Y%m%d-%H%M%S", secs)
end

local function default_export_path()
  local dir  = smelt.session.cwd() or "."
  local slug = slugify(smelt.session.title())
  local stamp = file_stamp(smelt.session.created_at_ms())
  local base = string.format("%s/smelt-%s-%s.md", dir, slug, stamp)
  -- Disambiguate against existing files.
  local path = base
  local n = 2
  while true do
    local f = io.open(path, "r")
    if not f then break end
    f:close()
    path = base:gsub("%.md$", string.format("-%d.md", n))
    n = n + 1
  end
  return path
end

local function format_markdown()
  local parts = {}
  local title = smelt.session.title()
  if title and title ~= "" then
    table.insert(parts, "# " .. title .. "\n")
  end

  local meta = {}
  local model = smelt.engine.model()
  if model and model ~= "" then
    table.insert(meta, "**Model:** " .. model)
  end
  local cwd = smelt.session.cwd()
  if cwd and cwd ~= "" then
    table.insert(meta, "**CWD:** `" .. cwd .. "`")
  end
  local created = smelt.session.created_at_ms()
  if created and created > 0 then
    table.insert(meta, "**Date:** " .. format_timestamp(created))
  end
  if #meta > 0 then
    table.insert(parts, table.concat(meta, " · ") .. "\n")
    table.insert(parts, "---\n")
  end

  -- Build a lookup: tool_call_id -> (content, is_error).
  local history = smelt.engine.history()
  local tool_results = {}
  for _, msg in ipairs(history) do
    if msg.role == "tool" and msg.tool_call_id and msg.content then
      tool_results[msg.tool_call_id] = { content = msg.content, is_error = msg.is_error }
    end
  end

  for _, msg in ipairs(history) do
    if msg.role == "system" then
      table.insert(parts, "## System\n")
      if msg.content then table.insert(parts, msg.content .. "\n") end
    elseif msg.role == "user" then
      table.insert(parts, "## User\n")
      if msg.content then table.insert(parts, msg.content .. "\n") end
    elseif msg.role == "assistant" then
      table.insert(parts, "## Assistant\n")
      if msg.content and msg.content ~= "" then
        table.insert(parts, msg.content .. "\n")
      end
      if msg.tool_calls then
        for _, call in ipairs(msg.tool_calls) do
          table.insert(parts, string.format("**Tool call:** `%s`\n", call.name))
          table.insert(parts, "```json")
          table.insert(parts, call.arguments)
          table.insert(parts, "```\n")
          local result = tool_results[call.id]
          if result then
            local tag = result.is_error and "Error" or "Result"
            table.insert(parts, string.format("**%s:**\n", tag))
            table.insert(parts, "```")
            table.insert(parts, result.content)
            table.insert(parts, "```\n")
          end
        end
      end
    end
    -- Tool messages handled inline under Assistant above; skip here.
  end

  return table.concat(parts, "\n")
end

smelt.cmd.register("export", function()
  if #smelt.engine.history() == 0 then
    smelt.notify_error("nothing to export")
    return
  end

  smelt.spawn(function()
    local result = smelt.ui.dialog.open({
      title  = "export",
      panels = {
        { kind = "options", items = {
          { label = "Copy to clipboard" },
          { label = "Write to file" },
        }},
      },
    })

    if result.action == "dismiss" or result.option_index == nil then
      return
    end

    local markdown = format_markdown()
    if result.option_index == 1 then
      smelt.clipboard(markdown)
      smelt.notify("conversation copied to clipboard")
    elseif result.option_index == 2 then
      local path = default_export_path()
      local f, err = io.open(path, "w")
      if not f then
        smelt.notify_error("export failed: " .. (err or "unknown"))
        return
      end
      f:write(markdown)
      f:close()
      local name = path:match("([^/]+)$") or path
      smelt.notify("exported to " .. name)
    end
  end)
end, { desc = "copy conversation to clipboard" })
