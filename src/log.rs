use crate::config;
use serde::Serialize;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();
static LOG_LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Debug = 0,
    Info = 1,
    Warn = 2,
    Error = 3,
}

impl Level {
    fn enabled(self) -> bool {
        self as u8 >= LOG_LEVEL.load(Ordering::Relaxed)
    }
}

pub fn set_level(level: Level) {
    LOG_LEVEL.store(level as u8, Ordering::Relaxed);
}

pub fn parse_level(s: &str) -> Option<Level> {
    match s.trim().to_lowercase().as_str() {
        "debug" => Some(Level::Debug),
        "info" => Some(Level::Info),
        "warn" | "warning" => Some(Level::Warn),
        "error" => Some(Level::Error),
        _ => None,
    }
}

fn log_path() -> &'static PathBuf {
    LOG_PATH.get_or_init(|| {
        let dir = dirs();
        let _ = fs::create_dir_all(&dir);
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        dir.join(format!("{ts}.jsonl"))
    })
}

fn dirs() -> PathBuf {
    config::state_dir().join("logs")
}

pub fn path() -> &'static PathBuf {
    log_path()
}

pub fn entry(level: Level, event: &str, data: &impl Serialize) {
    if !level.enabled() {
        return;
    }
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();

    let payload = serde_json::json!({
        "ts": ts,
        "level": format!("{:?}", level).to_lowercase(),
        "event": event,
        "data": data,
    });

    let Ok(line) = serde_json::to_string(&payload) else {
        return;
    };

    let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path())
    else {
        return;
    };

    let _ = writeln!(f, "{line}");
}
