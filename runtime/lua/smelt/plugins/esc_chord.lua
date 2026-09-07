-- Esc-Esc: cancel in-flight foreground/background work (`smelt.work.busy`
-- tokens, e.g. /compact), or rewind to the previous turn when idle.

smelt.keymap.set("", "<Esc><Esc>", function(ctx)
  local restore_insert = ctx.vim_mode_at_chord_start == "insert"

  local continuation = smelt.engine.continuation_state()
  local paused = continuation.paused and continuation.error_kind ~= "cancelled"
  if paused or smelt.engine.has_active_turn() or smelt.work.is_busy() then
    if smelt.engine.is_running() and __smelt_internal.session._rewind_active_turn_if_clean({ restore_vim_insert = restore_insert }) then
      return
    end
    smelt.engine.cancel()
    if restore_insert then
      smelt.vim.set_mode("insert")
    end
    return
  end

  if #smelt.session.turns() == 0 then
    return
  end

  if restore_insert then
    smelt.cmd.run("rewind insert")
  else
    smelt.cmd.run("rewind")
  end
end, { desc = "cancel work / rewind" })
