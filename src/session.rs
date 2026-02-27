use crate::provider::Message;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use crate::config;

static SESSION_COUNTER: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub first_user_message: Option<String>,
    #[serde(default)]
    pub created_at_ms: u64,
    #[serde(default)]
    pub updated_at_ms: u64,
    #[serde(default)]
    pub messages: Vec<Message>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub first_user_message: Option<String>,
    #[serde(default)]
    pub created_at_ms: u64,
    #[serde(default)]
    pub updated_at_ms: u64,
}

impl Session {
    pub fn new() -> Self {
        let now = now_ms();
        let id = new_session_id(now);
        Self {
            id,
            title: None,
            first_user_message: None,
            created_at_ms: now,
            updated_at_ms: now,
            messages: Vec::new(),
        }
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn time_ago(ts_ms: u64, now_ms: u64) -> String {
    let delta = now_ms.saturating_sub(ts_ms) / 1000;
    if delta < 60 {
        return format!("{}s ago", delta.max(1));
    }
    if delta < 60 * 60 {
        return format!("{}m ago", (delta / 60).max(1));
    }
    if delta < 60 * 60 * 24 {
        return format!("{}h ago", (delta / 3600).max(1));
    }
    if delta < 60 * 60 * 24 * 7 {
        return format!("{}d ago", (delta / 86400).max(1));
    }
    if delta < 60 * 60 * 24 * 30 {
        return format!("{}w ago", (delta / 604800).max(1));
    }
    format!("{}mo ago", (delta / 2592000).max(1))
}

pub fn save(session: &Session) {
    let dir = sessions_dir();
    let _ = fs::create_dir_all(&dir);
    let path = dir.join(format!("{}.json", session.id));
    let tmp = dir.join(format!("{}.{}.tmp", session.id, now_ms()));
    if let Ok(json) = serde_json::to_string_pretty(session) {
        if fs::write(&tmp, json).is_ok() {
            let _ = fs::rename(&tmp, &path);
        }
    }
}

pub fn load(id: &str) -> Option<Session> {
    let path = sessions_dir().join(format!("{}.json", id));
    let contents = fs::read_to_string(path).ok()?;
    serde_json::from_str(&contents).ok()
}

pub fn list_sessions() -> Vec<SessionMeta> {
    let dir = sessions_dir();
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(contents) = fs::read_to_string(&path) else {
            continue;
        };
        let mut meta: SessionMeta = match serde_json::from_str(&contents) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.id.is_empty() {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                meta.id = stem.to_string();
            }
        }
        if meta.id.is_empty() {
            continue;
        }
        out.push(meta);
    }
    out.sort_by_key(|b| std::cmp::Reverse(session_updated_at(b)));
    out
}

fn session_updated_at(meta: &SessionMeta) -> u64 {
    if meta.updated_at_ms > 0 {
        meta.updated_at_ms
    } else {
        meta.created_at_ms
    }
}

fn sessions_dir() -> PathBuf {
    config::state_dir().join("sessions")
}

fn new_session_id(now_ms: u64) -> String {
    let counter = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    format!("{now_ms}-{pid}-{counter}")
}

#[cfg(test)]
mod tests {
    use super::time_ago;

    #[test]
    fn time_ago_formats() {
        let now = 10_000_000_000u64;
        assert_eq!(time_ago(now - 1_000, now), "1s ago");
        assert_eq!(time_ago(now - 60_000, now), "1m ago");
        assert_eq!(time_ago(now - 3_600_000, now), "1h ago");
        assert_eq!(time_ago(now - 86_400_000, now), "1d ago");
        assert_eq!(time_ago(now - 604_800_000, now), "1w ago");
        assert_eq!(time_ago(now - 2_592_000_000, now), "1mo ago");
    }
}
