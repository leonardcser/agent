-- Built-in input prediction plugin.
--
-- Hooks `turn_end` to predict the user's next message using a
-- lightweight background LLM call, displayed as ghost text.

local SYSTEM = "You predict what a user will type next in a coding assistant conversation. "
  .. "Reply with ONLY the predicted message — no quotes, no explanation, "
  .. "no preamble. Keep it short (one sentence max). If you cannot predict, "
  .. "reply with an empty string."

smelt.on("turn_end", function(_, data)
  if data.cancelled then
    return
  end

  smelt.ui.ghost_text.clear()

  local history = data.messages or {}

  -- Collect last 3 user messages + last assistant message.
  local user_msgs = {}
  local last_assistant = nil
  for i = #history, 1, -1 do
    local msg = history[i]
    if msg.role == "user" and #user_msgs < 3 then
      table.insert(user_msgs, 1, msg)
    elseif msg.role == "assistant" and not last_assistant then
      last_assistant = msg
    end
    if #user_msgs >= 3 and last_assistant then
      break
    end
  end

  if #user_msgs == 0 then
    return
  end

  -- Build context string, truncating each message.
  local parts = {}
  for _, msg in ipairs(user_msgs) do
    local text = msg.content or ""
    if #text > 500 then
      text = text:sub(-500)
    end
    table.insert(parts, "User: " .. text)
  end
  if last_assistant then
    local text = last_assistant.content or ""
    if #text > 500 then
      text = text:sub(-500)
    end
    table.insert(parts, "Assistant: " .. text)
  end

  local question = "Recent conversation:\n\n"
    .. table.concat(parts, "\n\n")
    .. "\n\nPredict the user's next message."

  smelt.engine.ask({
    system = SYSTEM,
    question = question,
    task = "prediction",
    on_response = function(content)
      local text = content:match("^%s*(.-)%s*$") or ""
      if text ~= "" then
        smelt.ui.ghost_text.set(text)
      end
    end,
  })
end)
