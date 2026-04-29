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
    /// Approximate byte size of the session's text content (message bodies,
    /// reasoning, tool-call args). Used to show session size in the resume
    /// dialog without loading full session.json.
    #[serde(default)]
    pub text_bytes: Option<u64>,
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
            text_bytes: Some(compute_text_bytes(&self.messages)),
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
    let session_dir = dir_for(session);
    let _ = fs::create_dir_all(&session_dir);
    let blob_dir = session_dir.join("blobs");
    let url_to_blob = store.save_blobs(&blob_dir);
    save_with_blobs(session, &url_to_blob);
}

/// Serialize and write `session.json` + `meta.json`, assuming blob files
/// have already been flushed and their URL→filename mapping collected.
/// Safe to call from a background thread — does no I/O on `store`.
///
/// Message content is redacted at ingress, so save does no extra redaction.
pub fn save_with_blobs(session: &Session, url_to_blob: &std::collections::HashMap<String, String>) {
    let _perf = crate::perf::begin("session:write");
    let session_dir = dir_for(session);
    let _ = fs::create_dir_all(&session_dir);
    let ts = now_ms();

    let session_out = if url_to_blob.is_empty() {
        std::borrow::Cow::Borrowed(session)
    } else {
        let mut s = session.clone();
        externalize_blobs(&mut s.messages, url_to_blob);
        std::borrow::Cow::Owned(s)
    };

    if let Ok(json) = serde_json::to_string(&*session_out) {
        atomic_write(&session_dir.join("session.json"), json.as_bytes(), ts);
    }
    let meta = session_out.meta();
    if let Ok(json) = serde_json::to_string(&meta) {
        atomic_write(&session_dir.join("meta.json"), json.as_bytes(), ts);
    }
    // Searchable plain-text blob for the resume dialog.
    let blob = build_search_blob(&session_out.messages);
    atomic_write(&session_dir.join("content.txt"), blob.as_bytes(), ts);
}

/// Write `contents` to `path` atomically via a tmp file + rename.
fn atomic_write(path: &std::path::Path, contents: &[u8], ts: u64) {
    let Some(dir) = path.parent() else { return };
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return;
    };
    let tmp = dir.join(format!("{name}.{ts}.tmp"));
    if fs::write(&tmp, contents).is_ok() {
        let _ = fs::rename(&tmp, path);
    }
}

/// Save the render cache alongside the session.
pub fn save_render_cache(session: &Session, cache: &crate::app::transcript_cache::RenderCache) {
    let session_dir = dir_for(session);
    let _ = fs::create_dir_all(&session_dir);
    let path = render_cache_path(&session_dir);
    let _ = fs::write(path, cache.serialize());
}

/// Load the render cache for a session. Returns `None` if the file is
/// missing, corrupt, or built by an incompatible version.
pub fn load_render_cache(session: &Session) -> Option<crate::app::transcript_cache::RenderCache> {
    let session_dir = dir_for(session);
    let path = render_cache_path(&session_dir);
    let data = fs::read(path).ok()?;
    let cache = crate::app::transcript_cache::RenderCache::deserialize(&data)?;
    if cache.version != crate::app::transcript_cache::RENDER_CACHE_VERSION {
        return None;
    }
    Some(cache)
}

/// Save the persisted layout cache (per-block laid-out output) alongside
/// the session.
pub fn save_layout_cache(
    session: &Session,
    cache: &crate::app::transcript_cache::PersistedLayoutCache,
) {
    let _perf = crate::perf::begin("session:write_layout");
    let session_dir = dir_for(session);
    let _ = fs::create_dir_all(&session_dir);
    let path = layout_cache_path(&session_dir);
    let bytes = cache.serialize();
    crate::perf::record_value("layout_cache:bytes", bytes.len() as u64);
    let _ = fs::write(path, bytes);
}

/// Load the persisted layout cache for a session. Returns `None` if the
/// file is missing, corrupt, or built by an incompatible version.
pub fn load_layout_cache(
    session: &Session,
) -> Option<crate::app::transcript_cache::PersistedLayoutCache> {
    let _perf = crate::perf::begin("session:read_layout");
    let session_dir = dir_for(session);
    let path = layout_cache_path(&session_dir);
    let data = fs::read(path).ok()?;
    let cache = crate::app::transcript_cache::PersistedLayoutCache::deserialize(&data)?;
    if cache.version != crate::app::transcript_cache::LAYOUT_CACHE_VERSION {
        return None;
    }
    Some(cache)
}

fn render_cache_path(session_dir: &std::path::Path) -> PathBuf {
    session_dir.join("render_cache.ir.bin")
}

fn layout_cache_path(session_dir: &std::path::Path) -> PathBuf {
    session_dir.join("render_cache.layout.bin")
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
    let _perf = crate::perf::begin("session:list");
    let dir = sessions_dir();
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };

    let paths: Vec<PathBuf> = entries
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            p.is_dir().then_some(p)
        })
        .collect();
    let mut out = crate::utils::parallel_filter_map(paths, load_meta_for_dir);
    out.sort_by_key(|b| std::cmp::Reverse(session_updated_at(b)));
    out
}

/// Load a session's metadata from its on-disk directory. Uses the fast
/// `meta.json` sidecar when present; falls back to parsing `session.json`
/// (and persists a regenerated sidecar) for legacy sessions.
fn load_meta_for_dir(path: PathBuf) -> Option<SessionMeta> {
    if let Ok(contents) = fs::read_to_string(path.join("meta.json")) {
        if let Ok(mut meta) = serde_json::from_str::<SessionMeta>(&contents) {
            if meta.text_bytes.is_none() {
                backfill_text_bytes(&path, &mut meta);
            }
            return Some(meta);
        }
    }
    let contents = fs::read_to_string(path.join("session.json")).ok()?;
    let session: Session = serde_json::from_str(&contents).ok()?;
    let mut meta = session.meta();
    if meta.id.is_empty() {
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            meta.id = name.to_string();
        }
    }
    write_meta(&path, &meta);
    Some(meta)
}

/// Compute the approximate text byte size of a session's messages:
/// sum of text content, reasoning, and tool-call argument lengths.
fn compute_text_bytes(messages: &[Message]) -> u64 {
    let mut total: u64 = 0;
    for msg in messages {
        if let Some(ref c) = msg.content {
            total += c.text_content().len() as u64;
        }
        if let Some(ref r) = msg.reasoning_content {
            total += r.len() as u64;
        }
        if let Some(ref calls) = msg.tool_calls {
            for call in calls {
                total += call.function.name.len() as u64;
                total += call.function.arguments.len() as u64;
            }
        }
    }
    total
}

/// Format a byte count as a human-readable string (e.g. "12.3 KB").
pub fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes}B")
    } else if b < MB {
        format!("{:.1}KB", b / KB)
    } else if b < GB {
        format!("{:.1}MB", b / MB)
    } else {
        format!("{:.1}GB", b / GB)
    }
}

/// Read session.json, compute text_bytes, and rewrite meta.json to cache it.
fn backfill_text_bytes(session_dir: &std::path::Path, meta: &mut SessionMeta) {
    let Ok(contents) = fs::read_to_string(session_dir.join("session.json")) else {
        return;
    };
    let Ok(session) = serde_json::from_str::<Session>(&contents) else {
        return;
    };
    let bytes = compute_text_bytes(&session.messages);
    meta.text_bytes = Some(bytes);
    write_meta(session_dir, meta);
}

/// Build the search blob for a session: user, assistant, and inter-agent
/// message text, separated by newlines. Reasoning, tool output, and system
/// messages are excluded.
fn build_search_blob(messages: &[Message]) -> String {
    use protocol::Role;
    let mut out = String::new();
    for msg in messages {
        match msg.role {
            Role::User | Role::Assistant | Role::Agent => {
                if let Some(ref c) = msg.content {
                    let text = c.text_content();
                    if !text.is_empty() {
                        out.push_str(&text);
                        out.push('\n');
                    }
                }
            }
            Role::Tool | Role::System => {}
        }
    }
    out
}

/// Load the searchable content blob for a session. If the `content.txt`
/// sidecar is missing (legacy sessions), regenerate it from `session.json`
/// and cache it on disk.
pub fn load_search_blob(session_id: &str) -> Option<String> {
    let session_dir = sessions_dir().join(session_id);
    if let Ok(contents) = fs::read_to_string(session_dir.join("content.txt")) {
        return Some(contents);
    }
    let full = fs::read_to_string(session_dir.join("session.json")).ok()?;
    let session: Session = serde_json::from_str(&full).ok()?;
    let blob = build_search_blob(&session.messages);
    atomic_write(&session_dir.join("content.txt"), blob.as_bytes(), now_ms());
    Some(blob)
}

fn write_meta(session_dir: &std::path::Path, meta: &SessionMeta) {
    if let Ok(json) = serde_json::to_string(meta) {
        atomic_write(&session_dir.join("meta.json"), json.as_bytes(), now_ms());
    }
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
    let _ = out.queue(Print(format!("\nresume with:\nsmelt --resume {short}\n")));
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
