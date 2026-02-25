use crossterm::style::Color;

pub const TOOL_OK: Color = Color::Green;
pub const TOOL_ERR: Color = Color::Red;
pub const TOOL_PENDING: Color = Color::DarkGrey;
pub const APPLY: Color = Color::AnsiValue(141);
pub const ACCENT: Color = Color::AnsiValue(147); // bright lavender for `code` and commands
pub const USER_BG: Color = Color::AnsiValue(236);
pub const BAR: Color = Color::AnsiValue(237);
pub const HEADING: Color = Color::AnsiValue(214); // orange for markdown headings
pub const PRIMARY: Color = Color::AnsiValue(74); // steel blue for spinner and primary accents
