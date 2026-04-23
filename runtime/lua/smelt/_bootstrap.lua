-- Task-yielding primitives. Each checks `coroutine.isyieldable()` so
-- calls from a non-task context raise a clear error instead of yielding
-- into the void. Autoloaded before user init.lua so every plugin sees
-- `smelt.sleep`, and before `smelt.dialog` / `smelt.picker` so those
-- runtime files can reference yield helpers safely.

function smelt.sleep(ms)
  if not coroutine.isyieldable() then
    error("smelt.sleep: call from inside smelt.spawn(fn) or tool.execute", 2)
  end
  return coroutine.yield({ __yield = "sleep", ms = ms })
end
