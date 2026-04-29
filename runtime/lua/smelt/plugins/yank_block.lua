-- Optional /yank-block command (not autoloaded).
--
-- Copies the transcript block under the cursor to the clipboard.
-- Thin wrapper around `smelt.transcript.yank_block()` which
-- handles the extract + copy + notify flow in Rust.
--
-- To enable, add to your init.lua:
--   require("smelt.plugins.yank_block")

smelt.cmd.register("yank-block", function()
  smelt.transcript.yank_block()
end, { desc = "copy transcript block under cursor" })
