-- Built-in /btw (side question) plugin.
--
-- Registers the `/btw` command. Opens a dialog, sends the question to
-- the engine via `engine.ask()`, and streams the response into the
-- dialog's buffer. Uses only generic buf/dialog primitives — zero
-- btw-specific Rust code.

local SYSTEM = "You are a helpful assistant. The user is asking a quick side question "
  .. "while working on something else. Answer concisely and directly. "
  .. "You have the conversation history for context."

smelt.api.cmd.register("btw", function(args)
  local question = args or ""
  if question == "" then
    smelt.api.ui.notify_error("usage: /btw <question>")
    return
  end

  smelt.task(function()
    local buf = smelt.api.buf.create()
    smelt.api.buf.set_lines(buf, { "thinking…" })

    local history = smelt.api.engine.history()
    local messages = {}
    for _, msg in ipairs(history) do
      table.insert(messages, { role = msg.role, content = msg.content or "" })
    end
    table.insert(messages, { role = "user", content = question })

    smelt.api.engine.ask({
      system = SYSTEM,
      messages = messages,
      task = "btw",
      on_response = function(content)
        local lines = {}
        for line in (content .. "\n"):gmatch("([^\n]*)\n") do
          table.insert(lines, line)
        end
        smelt.api.buf.set_lines(buf, lines)
      end,
    })

    smelt.api.dialog.open({
      title = question,
      panels = {
        { kind = "content", buf = buf, height = "fill" },
      },
    })
  end)
end, { desc = "ask a side question" })
