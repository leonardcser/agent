-- Collapse long tool output by default. Registers a `block_done`
-- autocmd that flips the view state to Collapsed for anything
-- rendered by a bash tool.

smelt.on("block_done", function(event)
  -- In the v1 Lua surface we don't have block metadata yet; this hook
  -- is a placeholder for the scriptable "summarize tool output"
  -- workflow once `api::block::describe(id)` ships in a later slice.
  smelt.notify("block_done event fired: " .. event)
end)
