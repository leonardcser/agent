//! `smelt.text` - terminal-cell measurement and pure text formatting.
//! These helpers do not access live UI state. Render helpers live in
//! `smelt.render`.

use mlua::prelude::*;
use smelt_buffer::{cell_width, text};
use smelt_core::content::width::{pad_left_to_cells, pad_right_to_cells, truncate_to_cells};
use smelt_core::lua::doc::Tier;
use smelt_core::lua::module::LuaMod;

fn lossy_utf8(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn head_bytes(bytes: &[u8], max_bytes: usize) -> String {
    let s = lossy_utf8(bytes);
    text::grapheme_prefix(&s, max_bytes).to_string()
}

fn tail_bytes(bytes: &[u8], max_bytes: usize) -> String {
    let s = lossy_utf8(bytes);
    text::grapheme_suffix(&s, max_bytes).to_string()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Keep {
    Head,
    Tail,
}

struct TruncateOptions {
    keep: Keep,
    prefix: String,
    suffix: String,
}

impl Default for TruncateOptions {
    fn default() -> Self {
        Self {
            keep: Keep::Head,
            prefix: String::new(),
            suffix: String::new(),
        }
    }
}

fn lua_string_lossy(s: mlua::LuaString) -> String {
    let bytes = s.as_bytes();
    lossy_utf8(bytes.as_ref())
}

fn table_string(t: &mlua::Table, key: &str) -> LuaResult<Option<String>> {
    match t.get::<mlua::Value>(key)? {
        mlua::Value::Nil => Ok(None),
        mlua::Value::String(s) => Ok(Some(lua_string_lossy(s))),
        other => Err(mlua::Error::FromLuaConversionError {
            from: other.type_name(),
            to: "string".into(),
            message: Some(format!("smelt.text.truncate: opts.{key} must be a string")),
        }),
    }
}

fn parse_truncate_options(opts: Option<mlua::Value>) -> LuaResult<TruncateOptions> {
    match opts {
        None | Some(mlua::Value::Nil) => Ok(TruncateOptions::default()),
        Some(mlua::Value::String(s)) => Ok(TruncateOptions {
            suffix: lua_string_lossy(s),
            ..TruncateOptions::default()
        }),
        Some(mlua::Value::Table(t)) => {
            let keep = match table_string(&t, "keep")?.as_deref() {
                None | Some("head") => Keep::Head,
                Some("tail") => Keep::Tail,
                Some(_) => {
                    return Err(mlua::Error::RuntimeError(
                        "smelt.text.truncate: opts.keep must be 'head' or 'tail'".into(),
                    ));
                }
            };
            Ok(TruncateOptions {
                keep,
                prefix: table_string(&t, "prefix")?.unwrap_or_default(),
                suffix: table_string(&t, "suffix")?.unwrap_or_default(),
            })
        }
        Some(other) => Err(mlua::Error::FromLuaConversionError {
            from: other.type_name(),
            to: "string or table".into(),
            message: Some("smelt.text.truncate: opts must be a suffix string or table".into()),
        }),
    }
}

fn truncate_bytes(bytes: &[u8], max_bytes: usize, opts: &TruncateOptions) -> String {
    let truncated = bytes.len() > max_bytes;
    match opts.keep {
        Keep::Head => {
            let mut out = head_bytes(bytes, max_bytes);
            if truncated {
                out.push_str(&opts.suffix);
            }
            out
        }
        Keep::Tail => {
            let mut out = String::new();
            if truncated {
                out.push_str(&opts.prefix);
            }
            out.push_str(&tail_bytes(bytes, max_bytes));
            out
        }
    }
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table) -> LuaResult<()> {
    let m = LuaMod::advanced(
        lua,
        smelt,
        "text",
        "Terminal-cell measurement and pure text formatting.",
        Tier::Host,
    )?;
    m.fn_(
        "width",
        "Return the visual column count of `s`. Lua's `#s` counts bytes; use this for sizing extmark ranges or computing column offsets so multi-byte and wide characters land correctly.",
        &["s"],
        |_, s: String| Ok(cell_width::text_width(s.as_str()) as u64),
    )?;
    m.fn_(
        "line_count",
        "Return the number of lines in `s`. Counts `\\n` separators and adds one if the last line is unterminated; an empty string returns `0`. Matches the line count users see in a renderer that splits on `\\n` without dropping the trailing partial line.",
        &["s"],
        |_, s: String| {
            if s.is_empty() {
                return Ok(0u64);
            }
            let newlines = s.bytes().filter(|b| *b == b'\n').count() as u64;
            let trailing = u64::from(!s.ends_with('\n'));
            Ok(newlines + trailing)
        },
    )?;
    m.fn_(
        "sanitize_utf8",
        "Return `s` as valid UTF-8, replacing malformed byte sequences with the Unicode replacement character. Useful when a Lua string came from raw bytes; prefer `smelt.text.truncate` when shortening text.",
        &["s"],
        |_, s: mlua::LuaString| {
            let bytes = s.as_bytes();
            Ok(lossy_utf8(bytes.as_ref()))
        },
    )?;
    m.fn_(
        "slugify",
        "Lowercase `s`, replace non-alphanumeric runs with `-`, drop empty segments. Same algorithm the title plugin uses for fallback slugs.",
        &["s"],
        |_, s: String| Ok(engine::provider::slugify(&s)),
    )?;
    m.fn_(
        "truncate",
        "Return a valid UTF-8 string shortened to a byte budget. By default keeps the head: `truncate(s, n)`. Passing a string third argument appends it as a suffix when truncation happens. Passing an opts table enables `{ keep = \"head\"|\"tail\", prefix?, suffix? }`; use `{ keep = \"tail\" }` for recent-message snippets. Lua string slicing is byte-based and can split Unicode text; this function keeps complete grapheme clusters and also accepts already-invalid Lua byte strings.",
        &["s", "max_bytes", "opts"],
        |_, (s, max_bytes, opts): (mlua::LuaString, usize, Option<mlua::Value>)| -> LuaResult<String> {
            let bytes = s.as_bytes();
            let opts = parse_truncate_options(opts)?;
            Ok(truncate_bytes(bytes.as_ref(), max_bytes, &opts))
        },
    )?;
    m.fn_(
        "truncate_cells",
        "Return `s` shortened to at most `width` display cells, appending `opts.suffix` (default `\"…\"`) when truncation happens. Unlike `smelt.text.fit`, this does not pad short strings.",
        &["s", "width", "opts"],
        |_, (s, width, opts): (String, usize, Option<mlua::Table>)| -> LuaResult<String> {
            let suffix = opts
                .as_ref()
                .and_then(|t| t.get::<Option<String>>("suffix").ok().flatten())
                .unwrap_or_else(|| "…".into());
            Ok(truncate_to_cells(&s, width, &suffix))
        },
    )?;
    m.fn_(
        "wrap_prefixed",
        "Wrap `text` into rows with `opts.prefix` on the first row and `opts.cont_prefix` on continuation rows. Prefer word boundaries, omitting the separating space at a wrap; oversized words split only between graphemes. Explicit newlines start continuation rows. Uses terminal-cell width and returns an array of strings. `width = 0` disables wrapping.",
        &["text", "width", "opts"],
        |_,
         (text, width, opts): (String, usize, Option<mlua::Table>)|
         -> LuaResult<Vec<String>> {
            let prefix = opts
                .as_ref()
                .and_then(|t| t.get::<Option<String>>("prefix").ok().flatten())
                .unwrap_or_default();
            let cont_prefix = opts
                .as_ref()
                .and_then(|t| t.get::<Option<String>>("cont_prefix").ok().flatten())
                .unwrap_or_else(|| " ".repeat(cell_width::text_width(&prefix)));
            Ok(smelt_buffer::wrap::wrap_prefixed(&prefix, &text, &cont_prefix, width))
        },
    )?;
    m.fn_(
        "fit",
        "Force `s` to occupy exactly `width` display cells: truncate when too long (appending `opts.suffix`, default `\"…\"`), pad when too short (with `opts.fill`, default `\" \"`). `opts.align` is `\"left\"` (default), `\"right\"`, or `\"center\"`. Use this for fixed-width UI slots - handles multi-byte and wide chars correctly so the result is always exactly `width` cells wide regardless of content.",
        &["s", "width", "opts"],
        |_, (s, width, opts): (String, usize, Option<mlua::Table>)| -> LuaResult<String> {
            let opt = |key: &str, default: &str| {
                opts.as_ref()
                    .and_then(|t| t.get::<Option<String>>(key).ok().flatten())
                    .unwrap_or_else(|| default.into())
            };
            let align = opt("align", "left");
            let suffix = opt("suffix", "…");
            let fill = opt("fill", " ");
            // Zero-width fill would never close the gap; reject early.
            let fill_w = cell_width::text_width(fill.as_str());
            if fill_w == 0 {
                return Err(mlua::Error::RuntimeError(
                    "smelt.text.fit: fill must have non-zero display width".into(),
                ));
            }
            let trimmed = truncate_to_cells(&s, width, &suffix);
            let body_width = cell_width::text_width(&trimmed);
            let gap = width.saturating_sub(body_width);
            let out = match align.as_str() {
                "right" => pad_left_to_cells(&trimmed, &fill, width),
                "center" => {
                    let with_left =
                        pad_left_to_cells(&trimmed, &fill, body_width + gap / 2);
                    pad_right_to_cells(&with_left, &fill, width)
                }
                _ => pad_right_to_cells(&trimmed, &fill, width),
            };
            debug_assert_eq!(cell_width::text_width(&out), width);
            Ok(out)
        },
    )?;
    m.fn_(
        "format_duration",
        "Format `seconds` as a short human-readable duration: `42s`, `3m 12s`, `1h 5m 0s`. Used by the prompt-bar working indicator; useful for any plugin surfacing elapsed time.",
        &["seconds"],
        |_, secs: u64| -> LuaResult<String> {
            Ok(if secs < 60 {
                format!("{secs}s")
            } else if secs < 3600 {
                format!("{}m {}s", secs / 60, secs % 60)
            } else {
                format!("{}h {}m {}s", secs / 3600, (secs % 3600) / 60, secs % 60)
            })
        },
    )?;
    m.fn_(
        "format_tokens",
        "Format a raw token count as `1.2k`, `3.4m`, or the bare integer for values under 1000. Useful for compact statusline / banner displays.",
        &["n"],
        |_, n: u64| -> LuaResult<String> {
            Ok(if n >= 1_000_000 {
                format!("{:.1}m", n as f64 / 1_000_000.0)
            } else if n >= 1_000 {
                format!("{:.1}k", n as f64 / 1_000.0)
            } else {
                n.to_string()
            })
        },
    )?;
    m.fn_(
        "format_cost",
        "Format a USD cost with precision that scales to the magnitude: `$0.0042` under one cent, `$0.123` under one dollar, `$1.23` otherwise. Mirrors the format the prompt bar uses for session cost.",
        &["usd"],
        |_, usd: f64| -> LuaResult<String> {
            Ok(if usd < 0.01 {
                format!("${:.4}", usd)
            } else if usd < 1.0 {
                format!("${:.3}", usd)
            } else {
                format!("${:.2}", usd)
            })
        },
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_bytes_keeps_utf8_boundary_at_head() {
        let opts = TruncateOptions::default();
        assert_eq!(truncate_bytes("abc😀x".as_bytes(), 5, &opts), "abc");
        assert_eq!(truncate_bytes("abc😀x".as_bytes(), 7, &opts), "abc😀");
    }

    #[test]
    fn truncate_bytes_can_keep_tail() {
        let opts = TruncateOptions {
            keep: Keep::Tail,
            ..TruncateOptions::default()
        };
        assert_eq!(truncate_bytes("x😀abc".as_bytes(), 6, &opts), "abc");
        assert_eq!(truncate_bytes("x😀abc".as_bytes(), 7, &opts), "😀abc");
    }

    #[test]
    fn truncate_bytes_keeps_grapheme_clusters_atomic() {
        let head = TruncateOptions::default();
        let tail = TruncateOptions {
            keep: Keep::Tail,
            ..TruncateOptions::default()
        };

        assert_eq!(truncate_bytes("e\u{301}x".as_bytes(), 1, &head), "");
        assert_eq!(truncate_bytes("xe\u{301}".as_bytes(), 2, &tail), "");
        assert_eq!(truncate_bytes("👩\u{200d}💻x".as_bytes(), 8, &head), "");
        assert_eq!(truncate_bytes("x🇨🇦".as_bytes(), 4, &tail), "");
    }

    #[test]
    fn lua_truncate_accepts_invalid_byte_strings() {
        let lua = Lua::new();
        let smelt = lua.create_table().unwrap();
        register(&lua, &smelt).unwrap();
        let text: mlua::Table = smelt.get("text").unwrap();
        let truncate: mlua::Function = text.get("truncate").unwrap();
        let invalid = lua.create_string([0xff, b'a']).unwrap();

        let out: String = truncate.call((invalid, 10usize, mlua::Value::Nil)).unwrap();
        assert_eq!(out, "\u{FFFD}a");
    }

    #[test]
    fn lua_truncate_supports_tail_option() {
        let lua = Lua::new();
        let smelt = lua.create_table().unwrap();
        register(&lua, &smelt).unwrap();
        lua.globals().set("smelt", smelt).unwrap();
        let out: String = lua
            .load("return smelt.text.truncate('x😀abc', 7, { keep = 'tail' })")
            .eval()
            .unwrap();
        assert_eq!(out, "😀abc");
    }

    #[test]
    fn lua_truncate_cells_uses_display_width_without_padding() {
        let lua = Lua::new();
        let smelt = lua.create_table().unwrap();
        register(&lua, &smelt).unwrap();
        lua.globals().set("smelt", smelt).unwrap();
        let out: String = lua
            .load("return smelt.text.truncate_cells('hello world', 8, { suffix = '…' })")
            .eval()
            .unwrap();
        assert_eq!(out, "hello w…");
    }

    #[test]
    fn wrap_prefixed_preserves_graphemes_crossing_prefix_boundaries() {
        for (prefix, text, width, first) in [
            ("e", "\u{301}x", 1, "e\u{301}"),
            ("9", "\u{fe0f}x", 2, "9\u{fe0f}"),
            ("👩", "\u{200d}💻x", 2, "👩\u{200d}💻"),
            ("🇨", "🇦x", 2, "🇨🇦"),
        ] {
            let rows = smelt_buffer::wrap::wrap_prefixed(prefix, text, "", width);
            assert_eq!(rows, vec![first, "x"], "{prefix:?} + {text:?}");
            assert!(
                rows.iter().all(|row| cell_width::text_width(row) <= width),
                "{rows:?}"
            );
        }
    }

    #[test]
    fn lua_fit_is_exact_for_wide_and_boundary_joining_fill() {
        let lua = Lua::new();
        let smelt = lua.create_table().unwrap();
        register(&lua, &smelt).unwrap();
        let text: mlua::Table = smelt.get("text").unwrap();
        let fit: mlua::Function = text.get("fit").unwrap();

        for (body, fill, align) in [
            ("x", "中", "left"),
            ("9", "\u{fe0f} ", "left"),
            ("💻", "x\u{200d}", "right"),
            ("9", "\u{fe0f} ", "center"),
        ] {
            let opts = lua.create_table().unwrap();
            opts.set("fill", fill).unwrap();
            opts.set("align", align).unwrap();
            let out: String = fit.call((body, 5usize, opts)).unwrap();
            assert_eq!(cell_width::text_width(&out), 5, "{out:?}");
        }
    }

    #[test]
    fn lua_api_accepts_invalid_byte_strings() {
        let lua = Lua::new();
        let smelt = lua.create_table().unwrap();
        register(&lua, &smelt).unwrap();
        let text: mlua::Table = smelt.get("text").unwrap();
        let sanitize: mlua::Function = text.get("sanitize_utf8").unwrap();
        let invalid = lua.create_string([0xff, b'a']).unwrap();

        let out: String = sanitize.call(invalid).unwrap();
        assert_eq!(out, "\u{FFFD}a");
    }
}
