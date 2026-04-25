//! Two persisted caches per session, one layered above the other:
//!
//! - `RenderCache` (`render_cache.ir.bin`) — tool-output intermediate
//!   representations: pre-computed `CachedInlineDiff`s for `edit_file` /
//!   `edit_notebook`. The IR is width-independent and survives layout
//!   invalidation, so a terminal resize can re-lay out diff blocks
//!   without re-running the LCS / syntect passes.
//!
//! - `PersistedLayoutCache` (`render_cache.layout.bin`) — fully laid-out
//!   `DisplayBlock` span trees per `BlockId`. A cache hit here skips
//!   layout entirely; the paint stage just walks the span tree.
//!
//! Layered fall-through: layout cache → IR cache → cold rebuild. The
//! layout cache survives paint (theme changes resolve `ColorRole::*`
//! freshly each frame). The IR cache survives layout invalidation
//! (resize prunes layouts but not the underlying diff IR).
use super::transcript_model::BlockArtifact;
use crate::content::highlight::{build_inline_diff_cache_ext, CachedInlineDiff};
use engine::tools::NotebookRenderData;
use protocol::Message;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

pub const RENDER_CACHE_VERSION: u32 = 1;
pub const LAYOUT_CACHE_VERSION: u32 = 6;

/// Content-addressed on-disk snapshot of `BlockHistory::artifacts` for
/// one session. Keyed by block content hash (not `BlockId`, which is
/// monotonic per-session), so forked sessions with identical blocks
/// share cached layouts. Per-layout width validity is enforced lazily
/// by `DisplayBlock::is_valid_at` during the next paint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedLayoutCache {
    pub version: u32,
    /// Light/dark mode at the time of capture. Syntect picks a different
    /// syntax theme based on `theme::is_light()`, and those colors get
    /// baked into `DisplayBlock` spans at layout time. Resuming a session
    /// in a terminal with a different background detection produces
    /// stale syntax-highlight colors, so we drop the cache on mismatch.
    #[serde(default)]
    pub is_light: bool,
    pub blocks: HashMap<u64, BlockArtifact>,
}

impl PersistedLayoutCache {
    pub fn new(is_light: bool) -> Self {
        Self {
            version: LAYOUT_CACHE_VERSION,
            is_light,
            blocks: HashMap::new(),
        }
    }

    pub fn serialize(&self) -> Vec<u8> {
        use flate2::write::DeflateEncoder;
        use flate2::Compression;

        let payload = serde_json::to_vec(self).unwrap_or_default();
        let mut out = Vec::with_capacity(4 + payload.len() / 4);
        out.extend_from_slice(b"LCi5");
        let mut enc = DeflateEncoder::new(out, Compression::fast());
        std::io::Write::write_all(&mut enc, &payload).ok();
        enc.finish().unwrap_or_default()
    }

    pub fn deserialize(data: &[u8]) -> Option<Self> {
        use flate2::read::DeflateDecoder;
        use std::io::Read;

        if data.len() < 4 || &data[..4] != b"LCi5" {
            return None;
        }
        let mut dec = DeflateDecoder::new(&data[4..]);
        let mut payload = Vec::new();
        dec.read_to_end(&mut payload).ok()?;
        serde_json::from_slice(&payload).ok()
    }

    /// Compatible iff version matches AND the persisted light/dark mode
    /// matches current detection. Mismatched modes mean stale syntect
    /// colors throughout the cache.
    pub fn is_compatible(&self, current_is_light: bool) -> bool {
        self.version == LAYOUT_CACHE_VERSION && self.is_light == current_is_light
    }
}

impl Default for PersistedLayoutCache {
    fn default() -> Self {
        Self::new(false)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RenderCache {
    pub version: u32,
    pub session_hash: String,
    #[serde(default)]
    pub tool_outputs: HashMap<String, ToolOutputRenderCache>,
}

impl RenderCache {
    pub fn new(session_hash: String) -> Self {
        Self {
            version: RENDER_CACHE_VERSION,
            session_hash,
            tool_outputs: HashMap::new(),
        }
    }

    pub fn serialize(&self) -> Vec<u8> {
        use flate2::write::DeflateEncoder;
        use flate2::Compression;

        let payload = serde_json::to_vec(self).unwrap_or_default();
        let mut out = Vec::with_capacity(4 + payload.len() / 4);
        out.extend_from_slice(b"RCi1");
        let mut enc = DeflateEncoder::new(out, Compression::fast());
        std::io::Write::write_all(&mut enc, &payload).ok();
        enc.finish().unwrap_or_default()
    }

    pub fn deserialize(data: &[u8]) -> Option<Self> {
        use flate2::read::DeflateDecoder;
        use std::io::Read;

        if data.len() < 4 || &data[..4] != b"RCi1" {
            return None;
        }
        let mut dec = DeflateDecoder::new(&data[4..]);
        let mut payload = Vec::new();
        dec.read_to_end(&mut payload).ok()?;
        serde_json::from_slice(&payload).ok()
    }

    pub fn is_compatible(&self, session_hash: &str) -> bool {
        self.version == RENDER_CACHE_VERSION && self.session_hash == session_hash
    }

    pub fn get_tool_output(&self, call_id: &str) -> Option<&ToolOutputRenderCache> {
        self.tool_outputs.get(call_id)
    }

    pub fn insert_tool_output(&mut self, call_id: String, cache: ToolOutputRenderCache) {
        self.tool_outputs.insert(call_id, cache);
    }

    pub fn retain_history(&mut self, history: &[Message]) {
        let active: HashSet<&str> = history
            .iter()
            .filter_map(|msg| msg.tool_calls.as_ref())
            .flat_map(|calls| calls.iter().map(|call| call.id.as_str()))
            .collect();
        self.tool_outputs
            .retain(|call_id, _| active.contains(call_id.as_str()));
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToolOutputRenderCache {
    InlineDiff(CachedInlineDiff),
    NotebookEdit(CachedNotebookEdit),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedNotebookEdit {
    pub data: NotebookRenderData,
    pub diff: Option<CachedInlineDiff>,
}

pub fn build_tool_output_render_cache(
    name: &str,
    args: &HashMap<String, serde_json::Value>,
    content: &str,
    is_error: bool,
    metadata: Option<&serde_json::Value>,
) -> Option<ToolOutputRenderCache> {
    if is_error {
        return None;
    }
    match name {
        "edit_file" => {
            let old = args
                .get("old_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let new = args
                .get("new_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if new.is_empty() {
                return None;
            }
            let path = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
            Some(ToolOutputRenderCache::InlineDiff(
                build_inline_diff_cache_ext(old, new, path, new, None),
            ))
        }
        "edit_notebook" => {
            let meta = metadata?;
            let data = serde_json::from_value::<NotebookRenderData>(meta.clone()).ok()?;
            let diff = if data.edit_mode == "insert" {
                None
            } else {
                Some(build_inline_diff_cache_ext(
                    &data.old_source,
                    &data.new_source,
                    &data.path,
                    &data.old_source,
                    Some(data.syntax_ext()),
                ))
            };
            Some(ToolOutputRenderCache::NotebookEdit(CachedNotebookEdit {
                data,
                diff,
            }))
        }
        // Preserve successful spawn_agent output lines exactly as returned.
        // No extra IR is needed here.
        _ => {
            let _ = content;
            None
        }
    }
}
