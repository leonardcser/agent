-- Built-in /yank-block command.
--
-- Copies the transcript block under the cursor to the clipboard.
-- Thin wrapper around `smelt.api.transcript.yank_block()` which
-- handles the extract + copy + notify flow in Rust.

smelt.api.cmd.register("yank-block", function()
  smelt.api.transcript.yank_block()
end, { desc = "copy transcript block under cursor" })
