use crossterm::style::Color;
use std::sync::atomic::{AtomicU8, Ordering};

static ACCENT_VALUE: AtomicU8 = AtomicU8::new(147);

pub fn accent() -> Color {
    Color::AnsiValue(ACCENT_VALUE.load(Ordering::Relaxed))
}

pub fn set_accent(value: u8) {
    ACCENT_VALUE.store(value, Ordering::Relaxed);
}

pub fn accent_value() -> u8 {
    ACCENT_VALUE.load(Ordering::Relaxed)
}

pub const TOOL_OK: Color = Color::Green;
pub const TOOL_ERR: Color = Color::Red;
pub const TOOL_PENDING: Color = Color::DarkGrey;
pub const APPLY: Color = Color::AnsiValue(141);
pub const USER_BG: Color = Color::AnsiValue(236);
pub const CODE_BG: Option<Color> = None;
pub const BAR: Color = Color::AnsiValue(237);
pub const HEADING: Color = Color::AnsiValue(214); // orange for markdown headings
pub const MUTED: Color = Color::AnsiValue(244); // light gray for token count and other muted elements
pub const REASON_OFF: Color = Color::DarkGrey; // dim
pub const REASON_LOW: Color = Color::AnsiValue(75); // soft blue
pub const REASON_MED: Color = Color::AnsiValue(214); // warm amber
pub const REASON_HIGH: Color = Color::AnsiValue(203); // hot red-orange
pub const PLAN: Color = Color::AnsiValue(79); // teal-green for plan mode
pub const YOLO: Color = Color::AnsiValue(204); // rose for yolo mode
pub const EXEC: Color = Color::AnsiValue(197); // red-pink for exec mode
pub const SUCCESS: Color = Color::AnsiValue(114); // soft green for answered/success

/// Preset themes: (name, detail, ansi value)
pub const PRESETS: &[(&str, &str, u8)] = &[
    ("lavender", "default", 147),
    ("sky", "light blue", 117),
    ("mint", "soft green", 115),
    ("rose", "soft pink", 211),
    ("peach", "warm coral", 209),
    ("lilac", "purple", 183),
    ("gold", "warm yellow", 220),
    ("ember", "deep orange", 208),
    ("ice", "cool white-blue", 159),
    ("sage", "muted green", 108),
    ("coral", "salmon pink", 210),
    ("silver", "grey", 244),
];
