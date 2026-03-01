use crate::config;
use crate::provider::{Message, ReasoningEffort};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

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
    pub mode: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<ReasoningEffort>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
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
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<ReasoningEffort>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
}

impl Session {
    pub fn new() -> Self {
        let now = now_ms();
        let id = new_session_id(now);
        let cwd = std::env::current_dir()
            .ok()
            .and_then(|p| p.to_str().map(String::from));
        Self {
            id,
            title: None,
            first_user_message: None,
            created_at_ms: now,
            updated_at_ms: now,
            mode: None,
            reasoning_effort: None,
            model: None,
            cwd,
            messages: Vec::new(),
        }
    }

    pub fn meta(&self) -> SessionMeta {
        SessionMeta {
            id: self.id.clone(),
            title: self.title.clone(),
            first_user_message: self.first_user_message.clone(),
            created_at_ms: self.created_at_ms,
            updated_at_ms: self.updated_at_ms,
            mode: self.mode.clone(),
            reasoning_effort: self.reasoning_effort,
            model: self.model.clone(),
            cwd: self.cwd.clone(),
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
    let _perf = crate::perf::begin("session_save");
    let dir = sessions_dir();
    let _ = fs::create_dir_all(&dir);
    let ts = now_ms();

    // Write main session file
    let path = dir.join(format!("{}.json", session.id));
    let tmp = dir.join(format!("{}.{}.tmp", session.id, ts));
    if let Ok(json) = serde_json::to_string_pretty(session) {
        if fs::write(&tmp, json).is_ok() {
            let _ = fs::rename(&tmp, &path);
        }
    }

    // Write sidecar metadata file
    let meta_path = dir.join(format!("{}.meta.json", session.id));
    let meta_tmp = dir.join(format!("{}.meta.{}.tmp", session.id, ts));
    if let Ok(json) = serde_json::to_string(&session.meta()) {
        if fs::write(&meta_tmp, json).is_ok() {
            let _ = fs::rename(&meta_tmp, &meta_path);
        }
    }
}

pub fn load(id: &str) -> Option<Session> {
    let path = sessions_dir().join(format!("{}.json", id));
    let contents = fs::read_to_string(path).ok()?;
    serde_json::from_str(&contents).ok()
}

pub fn list_sessions() -> Vec<SessionMeta> {
    let _perf = crate::perf::begin("session_list");
    let dir = sessions_dir();
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };

    // Collect session IDs from .json files (excluding .meta.json and .tmp)
    let mut ids: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        if name.ends_with(".meta.json") || name.ends_with(".tmp") || !name.ends_with(".json") {
            continue;
        }
        let id = name.trim_end_matches(".json").to_string();
        if !id.is_empty() {
            ids.push(id);
        }
    }

    let mut out = Vec::new();
    for id in ids {
        // Try the fast sidecar file first
        let meta_path = dir.join(format!("{}.meta.json", id));
        if let Ok(contents) = fs::read_to_string(&meta_path) {
            if let Ok(meta) = serde_json::from_str::<SessionMeta>(&contents) {
                out.push(meta);
                continue;
            }
        }
        // Fall back to reading the full session file
        let path = dir.join(format!("{}.json", id));
        let Ok(contents) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(mut meta) = serde_json::from_str::<SessionMeta>(&contents) else {
            continue;
        };
        if meta.id.is_empty() {
            meta.id = id;
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

pub fn print_resume_hint(session_id: &str) {
    use crossterm::style::{Attribute, Print, SetAttribute};
    use crossterm::QueueableCommand;
    use std::io::Write;

    let mut out = std::io::stdout();
    let _ = out.queue(SetAttribute(Attribute::Dim));
    let _ = out.queue(Print(format!(
        "\nresume with:\nagent --resume {session_id}\n\n"
    )));
    let _ = out.queue(SetAttribute(Attribute::Reset));
    let _ = out.flush();
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
