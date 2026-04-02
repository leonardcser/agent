use crate::config;
use protocol::{Message, ReasoningEffort, TurnMeta};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static SESSION_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Minimum prefix length shown in resume hints.
const MIN_PREFIX_LEN: usize = 4;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub slug: Option<String>,
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
    pub parent_id: Option<String>,
    #[serde(default)]
    pub messages: Vec<Message>,
    #[serde(default)]
    pub context_tokens: Option<u32>,
    #[serde(default)]
    pub token_snapshots: Vec<(usize, u32)>,
    /// Accumulated session cost in USD, keyed by history length.
    #[serde(default)]
    pub cost_snapshots: Vec<(usize, f64)>,
    /// Per-turn metadata keyed by history length at capture time, parallel
    /// to `token_snapshots`.
    #[serde(default)]
    pub turn_metas: Vec<(usize, TurnMeta)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub slug: Option<String>,
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
    pub parent_id: Option<String>,
    #[serde(default)]
    pub context_tokens: Option<u32>,
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
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
            slug: None,
            first_user_message: None,
            created_at_ms: now,
            updated_at_ms: now,
            mode: None,
            reasoning_effort: None,
            model: None,
            cwd,
            parent_id: None,
            messages: Vec::new(),
            context_tokens: None,
            token_snapshots: Vec::new(),
            cost_snapshots: Vec::new(),
            turn_metas: Vec::new(),
        }
    }

    pub fn meta(&self) -> SessionMeta {
        SessionMeta {
            id: self.id.clone(),
            title: self.title.clone(),
            slug: self.slug.clone(),
            first_user_message: self.first_user_message.clone(),
            created_at_ms: self.created_at_ms,
            updated_at_ms: self.updated_at_ms,
            mode: self.mode.clone(),
            reasoning_effort: self.reasoning_effort,
            model: self.model.clone(),
            cwd: self.cwd.clone(),
            parent_id: self.parent_id.clone(),
            context_tokens: self.context_tokens,
        }
    }

    /// Create a fork: same content, new ID, parent_id pointing back.
    pub fn fork(&self) -> Self {
        let now = now_ms();
        Self {
            id: new_session_id(now),
            title: self.title.clone(),
            slug: self.slug.clone(),
            first_user_message: self.first_user_message.clone(),
            created_at_ms: now,
            updated_at_ms: now,
            mode: self.mode.clone(),
            reasoning_effort: self.reasoning_effort,
            model: self.model.clone(),
            cwd: self.cwd.clone(),
            parent_id: Some(self.id.clone()),
            messages: self.messages.clone(),
            context_tokens: self.context_tokens,
            token_snapshots: self.token_snapshots.clone(),
            cost_snapshots: self.cost_snapshots.clone(),
            turn_metas: self.turn_metas.clone(),
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

// ── Save / Load / Delete ─────────────────────────────────────────────────────

/// Return the directory for a session on disk.
pub fn dir_for(session: &Session) -> PathBuf {
    sessions_dir().join(&session.id)
}

pub fn save(session: &Session, store: &crate::attachment::AttachmentStore) {
    let _perf = crate::perf::begin("session_save");
    let session_dir = dir_for(session);
    let _ = fs::create_dir_all(&session_dir);
    let ts = now_ms();

    // Write blob files for images and get the URL replacement map.
    let blob_dir = session_dir.join("blobs");
    let url_to_blob = store.save_blobs(&blob_dir);

    // Clone session and replace inline data URLs with blob refs.
    let session_out = if url_to_blob.is_empty() {
        std::borrow::Cow::Borrowed(session)
    } else {
        let mut s = session.clone();
        externalize_blobs(&mut s.messages, &url_to_blob);
        std::borrow::Cow::Owned(s)
    };

    // Write main session file
    let path = session_dir.join("session.json");
    let tmp = session_dir.join(format!("session.{ts}.tmp"));
    if let Ok(json) = serde_json::to_string_pretty(&*session_out) {
        if fs::write(&tmp, json).is_ok() {
            let _ = fs::rename(&tmp, &path);
        }
    }

    // Write sidecar metadata file
    let meta_path = session_dir.join("meta.json");
    let meta_tmp = session_dir.join(format!("meta.{ts}.tmp"));
    if let Ok(json) = serde_json::to_string(&session.meta()) {
        if fs::write(&meta_tmp, json).is_ok() {
            let _ = fs::rename(&meta_tmp, &meta_path);
        }
    }
}

/// Save the render cache alongside the session.
pub fn save_render_cache(session: &Session, cache: &crate::render::RenderCache) {
    let session_dir = dir_for(session);
    let _ = fs::create_dir_all(&session_dir);
    let path = session_dir.join("render_cache.bin");
    let _ = fs::write(path, cache.serialize());
}

/// Load the render cache for a session.
pub fn load_render_cache(session: &Session) -> Option<crate::render::RenderCache> {
    let path = dir_for(session).join("render_cache.bin");
    let data = fs::read(path).ok()?;
    crate::render::RenderCache::deserialize(&data)
}

/// Load a session by exact ID or unique prefix (git-style).
pub fn load(id_or_prefix: &str) -> Option<Session> {
    let id = resolve_prefix(id_or_prefix)?;
    load_exact(&id)
}

fn load_exact(id: &str) -> Option<Session> {
    let dir_path = sessions_dir().join(id);
    let contents = fs::read_to_string(dir_path.join("session.json")).ok()?;
    let mut session: Session = serde_json::from_str(&contents).ok()?;

    let blob_dir = dir_path.join("blobs");
    if blob_dir.is_dir() {
        let blob_to_url = crate::attachment::AttachmentStore::load_blobs(&blob_dir);
        if !blob_to_url.is_empty() {
            internalize_blobs(&mut session.messages, &blob_to_url);
        }
    }
    Some(session)
}

/// Resolve a prefix to a full session ID. Returns `None` if no match,
/// or if the prefix is ambiguous (matches multiple sessions).
fn resolve_prefix(prefix: &str) -> Option<String> {
    let dir = sessions_dir();

    // Exact match — fast path.
    if dir.join(prefix).join("session.json").is_file() {
        return Some(prefix.to_string());
    }

    // Prefix scan over session directories.
    let Ok(entries) = fs::read_dir(&dir) else {
        return None;
    };
    let mut matches = Vec::new();
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if name_str.starts_with(prefix) {
            matches.push(name_str.to_string());
        }
    }
    if matches.len() == 1 {
        Some(matches.into_iter().next().unwrap())
    } else {
        None
    }
}

pub fn delete(id: &str) {
    let session_dir = sessions_dir().join(id);
    if session_dir.is_dir() {
        let _ = fs::remove_dir_all(&session_dir);
    }
}

pub fn list_sessions() -> Vec<SessionMeta> {
    let _perf = crate::perf::begin("session_list");
    let dir = sessions_dir();
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };

        // Try fast sidecar meta first.
        if let Ok(contents) = fs::read_to_string(path.join("meta.json")) {
            if let Ok(meta) = serde_json::from_str::<SessionMeta>(&contents) {
                out.push(meta);
                continue;
            }
        }
        // Fall back to full session file.
        if let Ok(contents) = fs::read_to_string(path.join("session.json")) {
            if let Ok(mut meta) = serde_json::from_str::<SessionMeta>(&contents) {
                if meta.id.is_empty() {
                    meta.id = name.to_string();
                }
                out.push(meta);
            }
        }
    }
    out.sort_by_key(|b| std::cmp::Reverse(session_updated_at(b)));
    out
}

/// Replace inline `data:` URLs in messages with `blob:` refs.
fn externalize_blobs(
    messages: &mut [Message],
    url_to_blob: &std::collections::HashMap<String, String>,
) {
    for msg in messages {
        if let Some(protocol::Content::Parts(parts)) = &mut msg.content {
            for part in parts {
                if let protocol::ContentPart::ImageUrl { url, .. } = part {
                    if let Some(blob_ref) = url_to_blob.get(url.as_str()) {
                        *url = blob_ref.clone();
                    }
                }
            }
        }
    }
}

/// Replace `blob:` refs in messages with inline data URLs.
fn internalize_blobs(
    messages: &mut [Message],
    blob_to_url: &std::collections::HashMap<String, String>,
) {
    for msg in messages {
        if let Some(protocol::Content::Parts(parts)) = &mut msg.content {
            for part in parts {
                if let protocol::ContentPart::ImageUrl { url, .. } = part {
                    if let Some(data_url) = blob_to_url.get(url.as_str()) {
                        *url = data_url.clone();
                    }
                }
            }
        }
    }
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

    let short = shortest_unique_prefix(session_id);
    let mut out = std::io::stdout();
    let _ = out.queue(SetAttribute(Attribute::Dim));
    let _ = out.queue(Print(format!("resume with:\nsmelt --resume {short}\n")));
    let _ = out.queue(SetAttribute(Attribute::Reset));
    let _ = out.flush();
}

/// Find the shortest prefix of `id` that uniquely identifies it among all sessions.
fn shortest_unique_prefix(id: &str) -> &str {
    let dir = sessions_dir();
    let Ok(entries) = fs::read_dir(&dir) else {
        return &id[..id.len().min(MIN_PREFIX_LEN)];
    };

    let others: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            if !e.path().is_dir() {
                return None;
            }
            let name = e.file_name();
            let s = name.to_str()?.to_string();
            (s != id).then_some(s)
        })
        .collect();

    for len in MIN_PREFIX_LEN..=id.len() {
        let prefix = &id[..len];
        if others.iter().all(|o| !o.starts_with(prefix)) {
            return prefix;
        }
    }
    id
}

fn new_session_id(now_ms: u64) -> String {
    let counter = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let mut hasher = Sha256::new();
    hasher.update(now_ms.to_le_bytes());
    hasher.update(pid.to_le_bytes());
    hasher.update(counter.to_le_bytes());
    // Mix in some randomness from the stack address.
    let entropy = &hasher as *const _ as usize;
    hasher.update(entropy.to_le_bytes());
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn session_id_is_full_sha256_hex() {
        let id = new_session_id(123456789);
        assert_eq!(id.len(), 64);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn session_ids_are_unique() {
        let id1 = new_session_id(100);
        let id2 = new_session_id(100);
        assert_ne!(id1, id2);
    }

    #[test]
    fn shortest_prefix_with_no_others() {
        // When the sessions dir doesn't exist or is empty, returns MIN_PREFIX_LEN.
        let id = "abcdef1234567890";
        let prefix = &id[..id.len().min(MIN_PREFIX_LEN)];
        assert_eq!(prefix, "abcd");
    }
}
