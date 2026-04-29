use crossterm::style::Color;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

/// Immutable snapshot of the theme atomics taken at the start of a paint
/// pass. Resolved span colors look up roles in this snapshot instead of
/// reading the global atomics, so a single redraw is theme-consistent.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub accent: Color,
    pub slug: Color,
    pub user_bg: Color,
    pub code_block_bg: Color,
    pub bar: Color,
    pub tool_pending: Color,
    pub reason_off: Color,
    pub muted: Color,
    pub is_light: bool,
}

pub fn snapshot() -> Theme {
    Theme {
        accent: accent(),
        slug: slug_color(),
        user_bg: user_bg(),
        code_block_bg: code_block_bg(),
        bar: bar(),
        tool_pending: tool_pending(),
        reason_off: reason_off(),
        muted: muted(),
        is_light: is_light(),
    }
}

pub const DEFAULT_ACCENT: u8 = 208;
pub const DEFAULT_ACCENT_LIGHT: u8 = 208;

static ACCENT_VALUE: AtomicU8 = AtomicU8::new(DEFAULT_ACCENT);
static ACCENT_LIGHT_VALUE: AtomicU8 = AtomicU8::new(DEFAULT_ACCENT_LIGHT);

pub fn accent() -> Color {
    if is_light() {
        Color::AnsiValue(ACCENT_LIGHT_VALUE.load(Ordering::Relaxed))
    } else {
        Color::AnsiValue(ACCENT_VALUE.load(Ordering::Relaxed))
    }
}

pub fn set_accent(value: u8) {
    ACCENT_VALUE.store(value, Ordering::Relaxed);
}

pub fn accent_value() -> u8 {
    ACCENT_VALUE.load(Ordering::Relaxed)
}

/// Session-only slug color. 0 means "use accent".
static SLUG_COLOR_VALUE: AtomicU8 = AtomicU8::new(0);

pub fn slug_color() -> Color {
    let v = SLUG_COLOR_VALUE.load(Ordering::Relaxed);
    if v == 0 {
        accent()
    } else {
        Color::AnsiValue(v)
    }
}

pub fn set_slug_color(value: u8) {
    SLUG_COLOR_VALUE.store(value, Ordering::Relaxed);
}

pub fn slug_color_value() -> u8 {
    SLUG_COLOR_VALUE.load(Ordering::Relaxed)
}

/// Look up a preset by name. Returns the ansi value if found.
pub fn preset_by_name(name: &str) -> Option<u8> {
    PRESETS
        .iter()
        .find(|(n, _, _)| *n == name)
        .map(|(_, _, v)| *v)
}

// ---------------------------------------------------------------------------
// Light / dark terminal detection
// ---------------------------------------------------------------------------

static LIGHT_THEME: AtomicBool = AtomicBool::new(false);

pub fn is_light() -> bool {
    LIGHT_THEME.load(Ordering::Relaxed)
}

pub fn set_light(light: bool) {
    LIGHT_THEME.store(light, Ordering::Relaxed);
}

/// Detect whether the terminal has a light background and store the result.
/// Must be called *before* entering the TUI's raw-mode / alternate screen
/// since we temporarily enable raw mode ourselves for the OSC query.
pub fn detect_background() {
    if let Some(light) = detect_light_background() {
        set_light(light);
    }
    // On failure, default stays `false` (dark).
}

/// Try OSC 11 query first, fall back to `$COLORFGBG`.
fn detect_light_background() -> Option<bool> {
    if let Some(luma) = osc_background_luma() {
        return Some(luma > 0.6);
    }
    colorfgbg_is_light()
}

/// Parse `$COLORFGBG` (format "fg;bg" or "fg;default;bg").
/// Returns `Some(true)` for light backgrounds.
fn colorfgbg_is_light() -> Option<bool> {
    let val = std::env::var("COLORFGBG").ok()?;
    let parts: Vec<&str> = val.split(';').collect();
    let bg = match parts.len() {
        2 => parts[1],
        3 => parts[2],
        _ => return None,
    };
    let code: u8 = bg.parse().ok()?;
    // ANSI colors 0-6 and 8 are dark; 7 and 9-15 are light.
    Some(matches!(code, 7 | 9..=15))
}

/// Query the terminal's background color via the OSC 11 "dynamic colors"
/// escape sequence and return its luma (0.0 = black, 1.0 = white).
#[cfg(unix)]
fn osc_background_luma() -> Option<f32> {
    use crossterm::terminal::{disable_raw_mode, enable_raw_mode, is_raw_mode_enabled};
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;

    // Don't query if TERM=dumb.
    if std::env::var("TERM").is_ok_and(|t| t == "dumb") {
        return None;
    }

    let switch_raw = !is_raw_mode_enabled().unwrap_or(false);
    if switch_raw {
        enable_raw_mode().ok()?;
    }

    let result = (|| -> Option<f32> {
        let mut stdout = std::io::stdout().lock();
        // Send OSC 11 query + DSR fence.
        write!(stdout, "\x1b]11;?\x07\x1b[5n").ok()?;
        stdout.flush().ok()?;

        let mut tty = File::open("/dev/tty").ok()?;
        let mut buf = [0u8; 100];
        let mut written = 0;

        // Read with timeout until we get the fence response ('n').
        while written < buf.len() {
            if !wait_for_input(tty.as_raw_fd(), 100) {
                break;
            }
            let n = tty.read(&mut buf[written..]).ok()?;
            if n == 0 {
                break;
            }
            written += n;
            // Check if we've received the fence response.
            if buf[..written].contains(&b'n') {
                break;
            }
        }

        let response = std::str::from_utf8(&buf[..written]).ok()?;
        parse_osc11_response(response)
    })();

    if switch_raw {
        let _ = disable_raw_mode();
    }

    result
}

#[cfg(not(unix))]
fn osc_background_luma() -> Option<f32> {
    None
}

/// Parse an OSC 11 response like `\x1b]11;rgb:ffff/ffff/ffff\x1b\\` and return luma.
fn parse_osc11_response(response: &str) -> Option<f32> {
    // Find the rgb: portion. The response is wrapped in ESC sequences.
    let rgb_start = response.find("rgb:")?;
    let raw = &response[rgb_start + 4..];
    // Format: RRRR/GGGG/BBBB or RR/GG/BB — we take the first 2 hex digits of each.
    let parts: Vec<&str> = raw.split('/').collect();
    if parts.len() < 3 {
        return None;
    }
    let r = u8::from_str_radix(parts[0].get(..2)?, 16).ok()?;
    let g = u8::from_str_radix(parts[1].get(..2)?, 16).ok()?;
    // The blue component may have trailing ESC/BEL, so take only first 2 chars.
    let blue_str: String = parts[2]
        .chars()
        .take_while(|c| c.is_ascii_hexdigit())
        .collect();
    let b = u8::from_str_radix(blue_str.get(..2)?, 16).ok()?;

    // Perceived luminance (sRGB coefficients).
    Some((0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32) / 255.0)
}

/// Wait for input on a file descriptor with a timeout in milliseconds.
/// Returns `true` if input is available.
#[cfg(target_os = "macos")]
fn wait_for_input(fd: std::os::fd::RawFd, timeout_ms: u64) -> bool {
    unsafe {
        let mut read_fds: libc::fd_set = std::mem::zeroed();
        libc::FD_SET(fd, &mut read_fds);
        let mut tv = libc::timeval {
            tv_sec: (timeout_ms / 1000) as libc::time_t,
            tv_usec: ((timeout_ms % 1000) * 1000) as libc::suseconds_t,
        };
        libc::select(
            fd + 1,
            &mut read_fds,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut tv,
        ) > 0
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn wait_for_input(fd: std::os::fd::RawFd, timeout_ms: u64) -> bool {
    unsafe {
        let mut pollfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        libc::poll(&mut pollfd, 1, timeout_ms as libc::c_int) > 0
    }
}

// ---------------------------------------------------------------------------
// Theme-aware colors
// ---------------------------------------------------------------------------

pub fn tool_pending() -> Color {
    if is_light() {
        Color::AnsiValue(250)
    } else {
        Color::DarkGrey
    }
}

pub const APPLY: Color = Color::AnsiValue(141);

pub fn user_bg() -> Color {
    if is_light() {
        Color::AnsiValue(254)
    } else {
        Color::AnsiValue(236)
    }
}

pub fn code_block_bg() -> Color {
    if is_light() {
        Color::AnsiValue(255)
    } else {
        Color::AnsiValue(233)
    }
}

pub fn bar() -> Color {
    if is_light() {
        Color::AnsiValue(252)
    } else {
        Color::AnsiValue(237)
    }
}

pub fn selection_bg() -> Color {
    if is_light() {
        Color::AnsiValue(189)
    } else {
        Color::AnsiValue(238)
    }
}

pub fn scrollbar_track() -> Color {
    if is_light() {
        Color::AnsiValue(254)
    } else {
        Color::AnsiValue(235)
    }
}

pub fn scrollbar_thumb() -> Color {
    if is_light() {
        Color::AnsiValue(247)
    } else {
        Color::AnsiValue(243)
    }
}

pub const HEADING: Color = Color::AnsiValue(117);

pub fn muted() -> Color {
    Color::AnsiValue(244)
}

pub fn reason_off() -> Color {
    if is_light() {
        Color::AnsiValue(250)
    } else {
        Color::DarkGrey
    }
}

pub const REASON_LOW: Color = Color::AnsiValue(75);
pub const REASON_MED: Color = Color::AnsiValue(214);
pub const REASON_HIGH: Color = Color::AnsiValue(203);
pub const REASON_MAX: Color = Color::AnsiValue(196);
pub const PLAN: Color = Color::AnsiValue(79);
pub const YOLO: Color = Color::AnsiValue(204);
pub const EXEC: Color = Color::AnsiValue(197);
pub const SUCCESS: Color = Color::AnsiValue(77);
pub const ERROR: Color = Color::Red;
pub const AGENT: Color = Color::AnsiValue(75);

/// Preset themes: (name, detail, ansi value)
pub const PRESETS: &[(&str, &str, u8)] = &[
    ("ember", "default", DEFAULT_ACCENT),
    ("coral", "salmon pink", 210),
    ("rose", "soft pink", 211),
    ("gold", "warm yellow", 220),
    ("ice", "cool white-blue", 159),
    ("sky", "light blue", 117),
    ("blue", "classic blue", 69),
    ("lavender", "cool purple", 147),
    ("lilac", "warm purple", 183),
    ("mint", "soft green", 115),
    ("sage", "muted green", 108),
    ("silver", "grey", 244),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_osc11_dark_background() {
        // Typical dark terminal (near-black)
        let resp = "\x1b]11;rgb:1c1c/1c1c/1c1c\x1b\\";
        let luma = parse_osc11_response(resp).unwrap();
        assert!(luma < 0.2, "luma {luma} should indicate dark");
    }

    #[test]
    fn parse_osc11_light_background() {
        // Typical light terminal (near-white)
        let resp = "\x1b]11;rgb:ffff/ffff/ffff\x1b\\";
        let luma = parse_osc11_response(resp).unwrap();
        assert!(luma > 0.9, "luma {luma} should indicate light");
    }

    #[test]
    fn parse_osc11_mid_tone() {
        let resp = "\x1b]11;rgb:8080/8080/8080\x1b\\";
        let luma = parse_osc11_response(resp).unwrap();
        assert!(
            (0.4..0.6).contains(&luma),
            "luma {luma} should be mid-range"
        );
    }

    #[test]
    fn parse_osc11_short_hex() {
        // Some terminals send 2-digit hex
        let resp = "\x1b]11;rgb:ff/ff/ff\x1b\\";
        let luma = parse_osc11_response(resp).unwrap();
        assert!(luma > 0.9);
    }

    #[test]
    fn parse_osc11_garbage() {
        assert!(parse_osc11_response("garbage").is_none());
        assert!(parse_osc11_response("").is_none());
    }
}
