-- Ctrl+R reverse history search.
--
-- Opens a filterable picker over past prompts ranked by the history
-- scorer (word-boundary matches + recency). Typing filters live;
-- Enter commits the selected entry to the prompt buffer; Esc restores
-- whatever the user had before opening.

local function entry_label(entry)
  for line in (entry or ""):gmatch("[^\r\n]+") do
    local trimmed = line:match("^%s*(.-)%s*$")
    if trimmed ~= "" then return trimmed end
  end
  return ""
end

local function open()
  local entries = smelt.history.entries()
  if #entries == 0 then return end

  local saved_text = smelt.prompt.text()

  -- Build items newest-first (reverse order). Attach the 1-based
  -- index into the original `entries` table in a hidden field so we
  -- can look up the full multi-line text on accept.
  local items = {}
  for i = #entries, 1, -1 do
    items[#items + 1] = {
      label        = entry_label(entries[i]),
      search_terms = entries[i],
    }
    items[#items].entry_idx = i
  end

  smelt.spawn(function()
    local result = smelt.prompt.open_picker({ items = items })
    if result and result.action == "enter" then
      local chosen = items[result.index]
      if chosen and chosen.entry_idx then
        smelt.prompt.set_text(entries[chosen.entry_idx])
      end
    else
      -- Esc / Tab: restore whatever was there before Ctrl+R.
      smelt.prompt.set_text(saved_text)
    end
  end)
end

for _, mode in ipairs({ "normal", "insert", "visual" }) do
  smelt.keymap.set(mode, "c-r", open)
end

smelt.cmd.register("history", open, { desc = "search prompt history" })
