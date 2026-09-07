-- Shared Smelt colorscheme generator for the bundled two-face syntax themes.
-- Each public file in this directory calls `theme(<syntax-name>)` so users can
-- pick a Smelt UI palette and matching syntect palette with one name.

local M = {}

local function rgb(r, g, b) return { rgb = { r, g, b } } end
local function fg(color, extra)
  local s = { fg = color }
  if extra then for k, v in pairs(extra) do s[k] = v end end
  return s
end
local function bg(color) return { bg = color } end
local function mix(a, b, t)
  return rgb(
    math.floor(a[1] + (b[1] - a[1]) * t + 0.5),
    math.floor(a[2] + (b[2] - a[2]) * t + 0.5),
    math.floor(a[3] + (b[3] - a[3]) * t + 0.5)
  )
end
local function color(c) return rgb(c[1], c[2], c[3]) end

local raw = {
  { name = "ansi", module = "ansi", light = false, bg = { 0, 0, 0 }, fg = { 229, 229, 229 }, muted = { 128, 128, 128 }, accent = { 0, 175, 255 }, link = { 95, 175, 255 }, success = { 95, 215, 95 }, heading = { 135, 206, 250 }, warn = { 255, 215, 0 }, error = { 255, 95, 95 } },
  { name = "base16", module = "base16", light = false, bg = { 24, 24, 24 }, fg = { 216, 216, 216 }, muted = { 128, 128, 128 }, accent = { 171, 121, 103 }, link = { 126, 162, 190 }, success = { 161, 181, 108 }, heading = { 126, 162, 190 }, warn = { 240, 198, 116 }, error = { 171, 97, 107 } },
  { name = "base16-eighties.dark", module = "base16-eighties-dark", light = false, bg = { 45, 45, 45 }, fg = { 211, 208, 200 }, muted = { 116, 115, 105 }, accent = { 249, 145, 87 }, link = { 102, 153, 204 }, success = { 153, 204, 153 }, heading = { 102, 153, 204 }, warn = { 255, 204, 102 }, error = { 242, 119, 122 } },
  { name = "base16-mocha.dark", module = "base16-mocha-dark", light = false, bg = { 59, 50, 40 }, fg = { 208, 200, 198 }, muted = { 126, 112, 90 }, accent = { 210, 139, 113 }, link = { 139, 165, 161 }, success = { 190, 181, 91 }, heading = { 139, 165, 161 }, warn = { 245, 202, 153 }, error = { 203, 96, 95 } },
  { name = "base16-ocean.dark", module = "base16-ocean-dark", light = false, bg = { 43, 48, 59 }, fg = { 192, 197, 206 }, muted = { 101, 115, 126 }, accent = { 208, 135, 112 }, link = { 143, 161, 179 }, success = { 163, 190, 140 }, heading = { 143, 161, 179 }, warn = { 235, 203, 139 }, error = { 191, 97, 106 } },
  { name = "base16-ocean.light", module = "base16-ocean-light", light = true, bg = { 239, 241, 245 }, fg = { 79, 91, 102 }, muted = { 167, 173, 186 }, accent = { 208, 135, 112 }, link = { 52, 101, 164 }, success = { 77, 128, 77 }, heading = { 52, 101, 164 }, warn = { 181, 137, 0 }, error = { 172, 57, 57 } },
  { name = "base16-256", module = "base16-256", light = false, bg = { 18, 18, 18 }, fg = { 218, 218, 218 }, muted = { 118, 118, 118 }, accent = { 215, 135, 95 }, link = { 95, 175, 215 }, success = { 135, 175, 95 }, heading = { 95, 175, 215 }, warn = { 215, 175, 95 }, error = { 215, 95, 95 } },
  { name = "Catppuccin Frappe", module = "catppuccin-frappe", light = false, bg = { 48, 52, 70 }, fg = { 198, 208, 245 }, muted = { 124, 127, 147 }, accent = { 202, 158, 230 }, link = { 140, 170, 238 }, success = { 166, 209, 137 }, heading = { 140, 170, 238 }, warn = { 229, 200, 144 }, error = { 231, 130, 132 } },
  { name = "Catppuccin Latte", module = "catppuccin-latte", light = true, bg = { 239, 241, 245 }, fg = { 76, 79, 105 }, muted = { 124, 127, 147 }, accent = { 136, 57, 239 }, link = { 30, 102, 245 }, success = { 64, 160, 43 }, heading = { 30, 102, 245 }, warn = { 223, 142, 29 }, error = { 210, 15, 57 } },
  { name = "Catppuccin Macchiato", module = "catppuccin-macchiato", light = false, bg = { 36, 39, 58 }, fg = { 202, 211, 245 }, muted = { 147, 154, 183 }, accent = { 198, 160, 246 }, link = { 138, 173, 244 }, success = { 166, 218, 149 }, heading = { 138, 173, 244 }, warn = { 238, 212, 159 }, error = { 237, 135, 150 } },
  { name = "Catppuccin Mocha", module = "catppuccin-mocha", light = false, bg = { 30, 30, 46 }, fg = { 205, 214, 244 }, muted = { 108, 112, 134 }, accent = { 203, 166, 247 }, link = { 137, 180, 250 }, success = { 166, 227, 161 }, heading = { 137, 180, 250 }, warn = { 249, 226, 175 }, error = { 243, 139, 168 } },
  { name = "Coldark-Cold", module = "coldark-cold", light = true, bg = { 227, 234, 242 }, fg = { 17, 27, 39 }, muted = { 60, 82, 109 }, accent = { 160, 73, 0 }, link = { 0, 90, 142 }, success = { 17, 107, 0 }, heading = { 0, 90, 142 }, warn = { 117, 95, 0 }, error = { 140, 38, 38 } },
  { name = "Coldark-Dark", module = "coldark-dark", light = false, bg = { 17, 27, 39 }, fg = { 227, 234, 242 }, muted = { 141, 161, 185 }, accent = { 233, 174, 126 }, link = { 108, 184, 230 }, success = { 145, 208, 118 }, heading = { 108, 184, 230 }, warn = { 230, 211, 122 }, error = { 230, 110, 110 } },
  { name = "DarkNeon", module = "darkneon", light = false, bg = { 0, 0, 0 }, fg = { 255, 255, 255 }, muted = { 124, 124, 124 }, accent = { 255, 115, 253 }, link = { 102, 204, 255 }, success = { 204, 255, 102 }, heading = { 102, 204, 255 }, warn = { 255, 204, 102 }, error = { 255, 92, 87 } },
  { name = "Dracula", module = "dracula", light = false, bg = { 40, 42, 54 }, fg = { 248, 248, 242 }, muted = { 98, 114, 164 }, accent = { 255, 121, 198 }, link = { 139, 233, 253 }, success = { 80, 250, 123 }, heading = { 139, 233, 253 }, warn = { 241, 250, 140 }, error = { 255, 85, 85 } },
  { name = "GitHub", module = "github", light = true, bg = { 255, 255, 255 }, fg = { 51, 51, 51 }, muted = { 150, 152, 150 }, accent = { 167, 29, 93 }, link = { 24, 54, 145 }, success = { 37, 128, 37 }, heading = { 24, 54, 145 }, warn = { 183, 117, 0 }, error = { 203, 36, 49 } },
  { name = "gruvbox-dark", module = "gruvbox-dark", light = false, bg = { 40, 40, 40 }, fg = { 251, 241, 199 }, muted = { 146, 131, 116 }, accent = { 251, 73, 52 }, link = { 131, 165, 152 }, success = { 184, 187, 38 }, heading = { 131, 165, 152 }, warn = { 250, 189, 47 }, error = { 251, 73, 52 } },
  { name = "gruvbox-light", module = "gruvbox-light", light = true, bg = { 251, 241, 199 }, fg = { 40, 40, 40 }, muted = { 146, 131, 116 }, accent = { 157, 0, 6 }, link = { 7, 102, 120 }, success = { 121, 116, 14 }, heading = { 7, 102, 120 }, warn = { 181, 118, 20 }, error = { 157, 0, 6 } },
  { name = "InspiredGitHub", module = "inspired-github", light = true, bg = { 255, 255, 255 }, fg = { 50, 50, 50 }, muted = { 150, 152, 150 }, accent = { 167, 29, 93 }, link = { 24, 54, 145 }, success = { 37, 128, 37 }, heading = { 24, 54, 145 }, warn = { 183, 117, 0 }, error = { 203, 36, 49 } },
  { name = "1337", module = "leet", light = false, bg = { 25, 25, 25 }, fg = { 248, 248, 242 }, muted = { 109, 109, 109 }, accent = { 255, 94, 94 }, link = { 253, 176, 130 }, success = { 251, 227, 191 }, heading = { 253, 176, 130 }, warn = { 255, 204, 102 }, error = { 255, 94, 94 } },
  { name = "Monokai Extended", module = "monokai-extended", light = false, bg = { 34, 34, 34 }, fg = { 248, 248, 242 }, muted = { 117, 113, 94 }, accent = { 249, 38, 114 }, link = { 102, 217, 239 }, success = { 166, 226, 46 }, heading = { 102, 217, 239 }, warn = { 230, 219, 116 }, error = { 249, 38, 114 } },
  { name = "Monokai Extended Bright", module = "monokai-extended-bright", light = false, bg = { 39, 40, 34 }, fg = { 248, 248, 242 }, muted = { 117, 113, 94 }, accent = { 249, 38, 114 }, link = { 102, 217, 239 }, success = { 166, 226, 46 }, heading = { 102, 217, 239 }, warn = { 230, 219, 116 }, error = { 249, 38, 114 } },
  { name = "Monokai Extended Light", module = "monokai-extended-light", light = true, bg = { 250, 250, 250 }, fg = { 73, 72, 62 }, muted = { 117, 113, 94 }, accent = { 249, 0, 90 }, link = { 0, 128, 160 }, success = { 102, 140, 0 }, heading = { 0, 128, 160 }, warn = { 153, 143, 47 }, error = { 249, 0, 90 } },
  { name = "Monokai Extended Origin", module = "monokai-extended-origin", light = false, bg = { 39, 40, 34 }, fg = { 248, 248, 242 }, muted = { 117, 113, 94 }, accent = { 249, 38, 114 }, link = { 102, 217, 239 }, success = { 166, 226, 46 }, heading = { 102, 217, 239 }, warn = { 230, 219, 116 }, error = { 249, 38, 114 } },
  { name = "Nord", module = "nord", light = false, bg = { 46, 52, 64 }, fg = { 236, 239, 244 }, muted = { 97, 110, 136 }, accent = { 180, 142, 173 }, link = { 129, 161, 193 }, success = { 163, 190, 140 }, heading = { 136, 192, 208 }, warn = { 235, 203, 139 }, error = { 191, 97, 106 } },
  { name = "OneHalfDark", module = "one-half-dark", light = false, bg = { 40, 44, 52 }, fg = { 220, 223, 228 }, muted = { 92, 99, 112 }, accent = { 198, 120, 221 }, link = { 97, 175, 239 }, success = { 152, 195, 121 }, heading = { 97, 175, 239 }, warn = { 229, 192, 123 }, error = { 224, 108, 117 } },
  { name = "OneHalfLight", module = "one-half-light", light = true, bg = { 250, 250, 250 }, fg = { 56, 58, 66 }, muted = { 160, 161, 167 }, accent = { 166, 38, 164 }, link = { 64, 120, 242 }, success = { 80, 161, 79 }, heading = { 64, 120, 242 }, warn = { 193, 132, 1 }, error = { 228, 86, 73 } },
  { name = "Solarized (dark)", module = "solarized-dark", light = false, bg = { 0, 43, 54 }, fg = { 131, 148, 150 }, muted = { 88, 110, 117 }, accent = { 203, 75, 22 }, link = { 38, 139, 210 }, success = { 133, 153, 0 }, heading = { 38, 139, 210 }, warn = { 181, 137, 0 }, error = { 220, 50, 47 } },
  { name = "Solarized (light)", module = "solarized-light", light = true, bg = { 253, 246, 227 }, fg = { 101, 123, 131 }, muted = { 147, 161, 161 }, accent = { 203, 75, 22 }, link = { 38, 139, 210 }, success = { 133, 153, 0 }, heading = { 38, 139, 210 }, warn = { 181, 137, 0 }, error = { 220, 50, 47 } },
  { name = "Sublime Snazzy", module = "sublime-snazzy", light = false, bg = { 40, 42, 54 }, fg = { 248, 248, 242 }, muted = { 104, 104, 104 }, accent = { 255, 92, 87 }, link = { 87, 199, 255 }, success = { 90, 247, 142 }, heading = { 87, 199, 255 }, warn = { 243, 249, 157 }, error = { 255, 92, 87 } },
  { name = "TwoDark", module = "two-dark", light = false, bg = { 40, 44, 52 }, fg = { 171, 178, 191 }, muted = { 92, 99, 112 }, accent = { 198, 120, 221 }, link = { 97, 175, 239 }, success = { 152, 195, 121 }, heading = { 97, 175, 239 }, warn = { 229, 192, 123 }, error = { 224, 108, 117 } },
  { name = "zenburn", module = "zenburn", light = false, bg = { 63, 63, 63 }, fg = { 220, 220, 204 }, muted = { 135, 174, 134 }, accent = { 254, 214, 175 }, link = { 135, 214, 213 }, success = { 160, 207, 161 }, heading = { 135, 214, 213 }, warn = { 240, 223, 175 }, error = { 214, 134, 134 } },
}

M.schemes = {}
M.by_name = {}
M.by_module = {}
for _, p in ipairs(raw) do
  local item = { name = p.name, module = p.module, syntax = p.name, light = p.light }
  M.schemes[#M.schemes + 1] = item
  M.by_name[p.name] = item
  M.by_module[p.module] = item
end

local function build(p)
  local bg0, fg0, accent = p.bg, p.fg, p.accent
  local surface = p.light and mix(bg0, { 0, 0, 0 }, 0.05) or mix(bg0, { 255, 255, 255 }, 0.05)
  local surface2 = p.light and mix(bg0, { 0, 0, 0 }, 0.10) or mix(bg0, { 255, 255, 255 }, 0.10)
  local select = p.light and mix(accent, bg0, 0.68) or mix(accent, bg0, 0.72)
  local add = p.light and mix(p.success, bg0, 0.74) or mix(p.success, bg0, 0.82)
  local del = p.light and mix(p.error, bg0, 0.78) or mix(p.error, bg0, 0.82)
  local add_inline = p.light and mix(p.success, bg0, 0.55) or mix(p.success, bg0, 0.65)
  local del_inline = p.light and mix(p.error, bg0, 0.60) or mix(p.error, bg0, 0.65)
  local ultra = p.light and rgb(145, 35, 140) or rgb(218, 112, 214)
  local groups = {
    Normal              = fg(color(fg0)),
    Comment             = fg(color(p.muted), { italic = true }),
    Visual              = bg(select),
    Search              = { fg = color(bg0), bg = color(accent), bold = true },
    YankFlash           = bg(mix(p.warn, bg0, p.light and 0.45 or 0.62)),
    CursorLine          = bg(surface),
    ErrorMsg            = fg(color(p.error), { bold = true }),
    WarningMsg          = fg(color(p.warn), { bold = true }),
    GhostText           = fg(color(p.muted), { dim = true }),

    SmeltAccent         = fg(color(accent)),
    SmeltSlug           = fg(p.light and rgb(255, 255, 255) or rgb(0, 0, 0)),
    SmeltSuccess        = fg(color(p.success)),
    SmeltHeading        = fg(color(p.heading), { bold = true }),
    SmeltLink           = fg(color(p.link), { underline = true }),
    SmeltProcess        = fg(color(p.heading)),
    SmeltGoalBanner     = { fg = color(bg0), bg = color(p.heading) },
    SmeltGoalBannerLabel = { fg = color(bg0), bg = color(p.heading), bold = true },
    SmeltGoalBannerMode = { fg = color(bg0), bg = color(p.heading), bold = true },
    SmeltGoalBannerPausedLabel = { fg = color(bg0), bg = color(p.warn), bold = true },
    SmeltGoalBannerBlockedLabel = { fg = color(bg0), bg = color(p.error), bold = true },

    SmeltStatusBg       = bg(surface),
    SmeltUserBg         = bg(surface2),
    SmeltUserTimestamp  = fg(mix(surface2.rgb, fg0, p.light and 0.65 or 0.45)),
    SmeltScrollPillBg   = bg(surface),
    SmeltCodeBlockBg    = bg(surface),
    SmeltSeparator      = fg(mix(accent, bg0, 0.55)),
    SmeltResizeHandle   = fg(color(p.muted), { bold = true }),
    SmeltScrollbarTrack = bg(surface),
    SmeltScrollbarThumb = bg(mix(p.muted, bg0, 0.35)),

    SmeltToolPending    = "Comment",
    SmeltReasonLow      = fg(color(p.link)),
    SmeltReasonMedium   = fg(color(p.warn)),
    SmeltReasonHigh     = fg(color(p.accent)),
    SmeltReasonMax      = fg(color(p.error), { bold = true }),
    SmeltReasonUltra    = fg(ultra),

    SmeltVimNormal      = { fg = color(p.link), bg = surface },
    SmeltVimInsert      = { fg = color(p.success), bg = surface },
    SmeltVimVisual      = { fg = color(p.accent), bg = surface },
    SmeltModePlan       = { fg = color(p.success), bg = surface },
    SmeltModeApply      = { fg = color(p.accent), bg = surface },
    SmeltModeYolo       = { fg = color(p.warn), bg = surface },
    SmeltModeDefault    = { fg = color(p.muted), bg = surface },
    SmeltExecPrefix     = fg(color(p.error), { bold = true }),

    SmeltDiffAddBg          = bg(add),
    SmeltDiffDeleteBg       = bg(del),
    SmeltDiffAddInlineBg    = bg(add_inline),
    SmeltDiffDeleteInlineBg = bg(del_inline),
  }
  return {
    name = p.module,
    syntax = p.name,
    light = p.light,
    groups = groups,
  }
end

function M.theme(name)
  for _, p in ipairs(raw) do
    if p.name == name or p.module == name then return build(p) end
  end
  error("unknown bundled theme: " .. tostring(name), 2)
end

return M
