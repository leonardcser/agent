-- Built-in `/vim` and `/thinking` toggles.
--
-- Direct shortcuts for the two most-toggled boolean settings. The
-- `/settings` picker covers the same ground via a menu; these are the
-- single-keystroke aliases users reach for during a session.

smelt.cmd.register("vim", function()
  smelt.settings.toggle("vim")
end, { desc = "toggle vim mode" })

smelt.cmd.register("thinking", function()
  smelt.settings.toggle("show_thinking")
end, { desc = "toggle thinking blocks" })
