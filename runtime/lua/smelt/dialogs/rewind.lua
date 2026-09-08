-- Built-in /rewind command. Picks a past user turn and rewinds the transcript to it.

smelt.cmd.register("rewind", function(args)
  local turns = smelt.session.turns()
  if #turns == 0 then
    smelt.notify.error("nothing to rewind")
    return
  end

  -- "insert" arg forces vim Insert restoration after rewind (used by the Esc-Esc keymap).
  local restore_vim_insert = (args == "insert") or (smelt.vim.mode() == "insert")

  smelt.spawn(function()
    local items = {}
    for _, t in ipairs(turns) do table.insert(items, t.label or "") end
    table.insert(items, "(current)")

    local options_leaf = smelt.dialog.menu(items, {
      selected = #items,
      wrap     = true,
      -- The list often runs longer than nine items and rewinding is
      -- destructive, so digits only move the cursor - Enter confirms.
      shortcuts = "select",
    })

    local picked = smelt.dialog.open({
      title      = "rewind",
      max_height = "100%",
      min_height = 0,
      panels     = { { leaf = options_leaf } },
    })

    if not picked or not picked.index then return end

    if picked.index > #turns then
      if restore_vim_insert then smelt.vim.set_mode("insert") end
      return
    end

    local history_idx = turns[picked.index].history_idx
    smelt.session.rewind_to(history_idx, { restore_vim_insert = restore_vim_insert })
  end)
end, { desc = "rewind to a previous turn" })
