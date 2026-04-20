-- Built-in /btw (side question) plugin.
--
-- Registers the `/btw` command. Sends the question to the engine via
-- `engine.ask()` with task="btw" and displays the response in the btw
-- overlay.

local SYSTEM = "You are a helpful assistant. The user is asking a quick side question "
  .. "while working on something else. Answer concisely and directly. "
  .. "You have the conversation history for context."

smelt.api.cmd.register("btw", function(args)
  local question = args.args or ""
  if question == "" then
    smelt.api.ui.notify_error("usage: /btw <question>")
    return
  end

  -- Build messages from history for context.
  local history = smelt.api.engine.history()
  local messages = {}
  for _, msg in ipairs(history) do
    table.insert(messages, { role = msg.role, content = msg.content or "" })
  end
  table.insert(messages, { role = "user", content = question })

  smelt.api.ui.open_btw({ question = question })

  smelt.api.engine.ask({
    system = SYSTEM,
    messages = messages,
    task = "btw",
    on_response = function(content)
      smelt.api.ui.set_btw_response(content)
    end,
  })
end)
