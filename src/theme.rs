use crossterm::style::Color;

pub const TOOL_OK: Color = Color::Green;
pub const TOOL_ERR: Color = Color::Red;
pub const TOOL_PENDING: Color = Color::DarkGrey;
pub const APPLY: Color = Color::AnsiValue(141);
pub const ACCENT: Color = Color::AnsiValue(147); // bright lavender for `code` and commands
pub const USER_BG: Color = Color::AnsiValue(236);
pub const BAR: Color = Color::AnsiValue(237);
pub const HEADING: Color = Color::AnsiValue(214); // orange for markdown headings
pub const MUTED: Color = Color::AnsiValue(244); // light gray for token count and other muted elements
pub const PLAN: Color = Color::AnsiValue(79); // teal-green for plan mode
pub const YOLO: Color = Color::AnsiValue(204); // rose for yolo mode
pub const EXEC: Color = Color::AnsiValue(197); // red-pink for exec mode
pub const SUCCESS: Color = Color::AnsiValue(114); // soft green for answered/success
