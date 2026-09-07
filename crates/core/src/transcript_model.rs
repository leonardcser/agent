//! Transcript domain model: content-addressed block store, layout cache,
//! and mutable sidecar state (tool output, exec output). Held inside
//! `app::transcript::Transcript`, which adds streaming and paint orchestration.

use crate::content::tool_draft::{ToolArguments, ToolDraft, ToolDraftAppend};
use crate::paused_timer::PausedTimer;
use crate::permissions::PermissionGrant;
use crate::transcript_content::{ContentId, ContentStore, TranscriptContent};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Handle to an in-flight tool call; full mutable state lives on its transcript block.
pub struct ActiveTool {
    pub invocation_id: protocol::InvocationId,
    pub(crate) block_id: BlockId,
    timer: PausedTimer,
}

impl ActiveTool {
    pub fn new(
        invocation_id: protocol::InvocationId,
        block_id: BlockId,
        start_time: Instant,
    ) -> Self {
        Self {
            invocation_id,
            block_id,
            timer: PausedTimer::new(start_time),
        }
    }

    pub fn elapsed_at(&self, now: Instant) -> Duration {
        self.timer.elapsed_at(now)
    }

    pub fn pause(&mut self, now: Instant) {
        self.timer.pause(now);
    }

    pub fn resume(&mut self, now: Instant) {
        self.timer.resume(now);
    }
}

#[derive(Clone)]
pub struct ConfirmRequest {
    pub invocation_id: protocol::InvocationId,
    pub call_id: String,
    pub tool_name: String,
    pub args: std::collections::HashMap<String, serde_json::Value>,
    pub tool_paths: Vec<crate::permissions::ToolPath>,
    /// A path callback failure carried to confirmation dispatch so a stale or
    /// synthetic permission request is denied instead of becoming pathless.
    pub tool_paths_error: Option<String>,
    pub approval_candidates: Vec<String>,
    pub grant_options: Vec<ConfirmApprovalOption>,
    /// Styled summary of the pending call. Sole source for the dialog
    /// body header.
    pub summary: protocol::StyledLines,
    pub request_id: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ToolStatus {
    Pending,
    Confirm,
    Ok,
    Err,
    Denied,
}

impl ToolStatus {
    pub fn label(self) -> &'static str {
        match self {
            ToolStatus::Pending => "pending",
            ToolStatus::Confirm => "confirm",
            ToolStatus::Ok => "ok",
            ToolStatus::Err => "err",
            ToolStatus::Denied => "denied",
        }
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToolOutputContentField {
    pub name: String,
    pub content: TranscriptContent,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToolOutput {
    pub content: TranscriptContent,
    pub is_error: bool,
    pub metadata: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content_fields: Vec<ToolOutputContentField>,
}

impl ToolOutput {
    pub fn new(
        content: impl Into<TranscriptContent>,
        is_error: bool,
        metadata: Option<serde_json::Value>,
    ) -> Self {
        Self {
            content: content.into(),
            is_error,
            metadata,
            content_fields: Vec::new(),
        }
    }

    pub fn from_display_content(
        content: impl Into<TranscriptContent>,
        is_error: bool,
        metadata: Option<serde_json::Value>,
        display_content: Vec<protocol::ToolDisplayContent>,
    ) -> Self {
        Self {
            content: content.into(),
            is_error,
            metadata,
            content_fields: display_content
                .into_iter()
                .map(|field| ToolOutputContentField {
                    name: field.name,
                    content: field.content.into(),
                })
                .collect(),
        }
    }

    pub fn content_field(&self, name: &str) -> Option<&TranscriptContent> {
        self.content_fields
            .iter()
            .find(|field| field.name == name)
            .map(|field| &field.content)
    }

    fn registered_contents(&self) -> impl Iterator<Item = &TranscriptContent> {
        std::iter::once(&self.content).chain(self.content_fields.iter().map(|field| &field.content))
    }
}

pub type ToolOutputRef = Box<ToolOutput>;

/// Typed updates to the mutable state of a committed tool call.
///
/// Elapsed synchronization is animation state. It remains observable to renderers
/// without changing the tool's retained presentation identity or dirtying its
/// persisted record. Every other mutation can change renderer structure and advances
/// the presentation revision.
#[derive(Debug)]
pub enum ToolStateMutation {
    SyncElapsed(Duration),
    SetElapsedActive {
        elapsed: Duration,
        active: bool,
    },
    SetStatus {
        status: ToolStatus,
        elapsed: Option<Duration>,
    },
    SetUserMessage(String),
    Finish {
        status: ToolStatus,
        output: Option<ToolOutputRef>,
        elapsed: Option<Duration>,
    },
}

impl ToolStateMutation {
    fn is_animation_only(&self) -> bool {
        matches!(self, Self::SyncElapsed(_))
    }

    fn search_changed(&self) -> bool {
        matches!(
            self,
            Self::SetStatus { .. } | Self::SetUserMessage(_) | Self::Finish { .. }
        )
    }

    fn apply(self, state: &mut ToolState) {
        match self {
            Self::SyncElapsed(elapsed) => state.elapsed = Some(elapsed),
            Self::SetElapsedActive { elapsed, active } => {
                state.elapsed = Some(elapsed);
                state.elapsed_active = active;
            }
            Self::SetStatus { status, elapsed } => {
                state.status = status;
                state.elapsed_active = status == ToolStatus::Pending;
                if let Some(elapsed) = elapsed {
                    state.elapsed = Some(elapsed);
                }
            }
            Self::SetUserMessage(message) => state.user_message = Some(message),
            Self::Finish {
                status,
                output,
                elapsed,
            } => {
                state.status = status;
                if let Some(output) = output {
                    state.output = Some(output);
                }
                state.elapsed = elapsed;
                state.elapsed_active = false;
                state.preview_output = None;
            }
        }
    }
}

/// Mutable side state for a committed `Block::ToolCall`, keyed by its `BlockId`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToolState {
    pub status: ToolStatus,
    pub elapsed: Option<Duration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub called_at_ms: Option<u64>,
    #[serde(default)]
    pub elapsed_active: bool,
    pub output: Option<ToolOutputRef>,
    pub user_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_output: Option<ToolOutputRef>,
}

impl ToolState {
    pub fn content(&self, channel: ContentChannel) -> Option<&TranscriptContent> {
        match channel {
            ContentChannel::ToolOutput => self.output.as_deref().map(|output| &output.content),
            ContentChannel::ToolPreview => {
                self.preview_output.as_deref().map(|output| &output.content)
            }
            _ => None,
        }
    }

    pub fn registered_contents(&self) -> impl Iterator<Item = &TranscriptContent> {
        self.output
            .iter()
            .chain(self.preview_output.iter())
            .flat_map(|output| output.registered_contents())
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status,
            ToolStatus::Ok | ToolStatus::Err | ToolStatus::Denied
        )
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Block {
    User {
        text: String,
        /// Accent-highlighted in the rendered message.
        image_labels: Vec<String>,
        /// Whether the leading slash-command token receives accent styling.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        command: bool,
        /// Submission time as Unix epoch milliseconds, absent for undated history.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sent_at_ms: Option<u64>,
    },
    Mode {
        text: String,
        icon: String,
        hl_group: String,
    },
    ProcessStatus {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        event: Option<protocol::ProcessStatusEvent>,
    },
    Thinking {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        summary_titles: Vec<String>,
        content: TranscriptContent,
        #[serde(default)]
        kind: protocol::ReasoningKind,
    },
    Text {
        content: TranscriptContent,
    },
    CodeLine {
        content: String,
        lang: String,
    },
    ToolDraft(ToolDraft),
    ToolCall {
        call_id: String,
        name: String,
        /// Styled summary, produced by the tool's `summary(args)` Lua
        /// hook. The renderer consumes the styled spans; for plain-text
        /// callers (copy, search, snapshots) call `summary.as_plain_text()`.
        summary: protocol::StyledLines,
        args: ToolArguments,
    },
    Exec {
        command: String,
        output: TranscriptContent,
    },
    Compacted {
        summary: String,
    },
    CompactionPreview {
        summary: String,
    },
}

impl Block {
    pub(crate) fn normalize_content(self) -> Self {
        match self {
            Block::Thinking {
                title,
                summary_titles,
                content,
                kind,
            } => Block::Thinking {
                title,
                summary_titles,
                content: crate::content::markdown_stream::normalize_thinking_title_spacing(content),
                kind,
            },
            other => other,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Block::User { .. } => "user",
            Block::Mode { .. } => "mode",
            Block::ProcessStatus { .. } => "process_status",
            Block::Thinking { .. } => "thinking",
            Block::Text { .. } => "assistant",
            Block::CodeLine { .. } => "code",
            Block::ToolDraft(_) | Block::ToolCall { .. } => "tool",
            Block::Exec { .. } => "exec",
            Block::Compacted { .. } => "compacted",
            Block::CompactionPreview { .. } => "compaction_preview",
        }
    }

    /// Whether a viewport content anchor inside this block can be reused across
    /// later projection frames. Live-rewritten blocks can shift their rendered
    /// rows while preserving `BlockId`, so row-based viewport gestures should
    /// fall back to the resolved row instead.
    pub fn is_stable_scroll_anchor(&self) -> bool {
        !matches!(
            self,
            Block::ToolDraft(ToolDraft {
                finished: false,
                ..
            })
        )
    }

    pub fn row_estimate_text(&self) -> Option<BlockText<'_>> {
        match self {
            Block::User { text, .. }
            | Block::ProcessStatus { text, .. }
            | Block::Compacted { summary: text }
            | Block::CompactionPreview { summary: text }
            | Block::CodeLine { content: text, .. } => Some(BlockText::Plain(text)),
            Block::Text { content } => Some(BlockText::Content(content)),
            Block::Thinking {
                title,
                summary_titles,
                content,
                ..
            } => Some(BlockText::Thinking {
                title: title.as_deref(),
                summary_titles,
                content,
            }),
            Block::Mode { text, icon, .. } => Some(BlockText::Prefixed { prefix: icon, text }),
            Block::Exec { command, output } => Some(BlockText::Exec { command, output }),
            Block::ToolDraft(_) | Block::ToolCall { .. } => None,
        }
    }

    /// Stable content hash of this block. Two blocks with the same
    /// content hash produce identical `LayoutIr` for the same
    /// `LayoutKey` and `ToolState`. For `ToolCall`, `ToolState` (status
    /// / output / elapsed) is deliberately *not* hashed - mutable tool
    /// state lives separately and is invalidated via
    /// `BlockHistory::invalidate_block_layout`.
    pub fn content_hash(&self) -> u64 {
        match self {
            Self::ToolDraft(draft) => draft.content_hash(),
            Self::ToolCall {
                call_id,
                name,
                summary,
                args,
            } => crate::utils::hash_serializable(&(
                "tool_call",
                call_id,
                name,
                summary,
                args.content_hash(),
            )),
            _ => crate::utils::hash_serializable(self),
        }
    }

    /// Raw source text for the block, before markdown rendering. Used
    /// by whole-block yank so copying a rendered markdown block returns
    /// the original `**bold**`, `` `code` ``, fenced ```` ``` ```` blocks,
    /// `|` tables, `---` rules, etc. - instead of walking display cells
    /// (which strips inline markup).
    ///
    /// Returns `None` for structured blocks (tool calls,
    /// confirm dialogs) that don't have a single "markdown source"; the
    /// caller falls back to cell-walking for those.
    pub fn raw_text(&self) -> Option<String> {
        match self {
            Block::User { text, .. } => Some(text.clone()),
            Block::Mode { text, icon, .. } => Some(format!("{icon}{text}")),
            Block::ProcessStatus { text, .. } => Some(text.clone()),
            Block::Text { content } => Some(content.snapshot()),
            Block::Thinking {
                title,
                summary_titles,
                content,
                ..
            } => Some(thinking_markdown_source(
                title.as_deref(),
                summary_titles,
                &content.snapshot(),
            )),
            Block::Compacted { summary } | Block::CompactionPreview { summary } => {
                Some(summary.clone())
            }
            Block::CodeLine { content, .. } => Some(content.clone()),
            Block::Exec { command, output } => Some(format!("$ {command}\n{}", output.snapshot())),
            Block::ToolDraft(_) | Block::ToolCall { .. } => None,
        }
    }

    pub fn raw_text_len(&self) -> Option<usize> {
        match self {
            Block::User { text, .. }
            | Block::ProcessStatus { text, .. }
            | Block::Compacted { summary: text }
            | Block::CompactionPreview { summary: text }
            | Block::CodeLine { content: text, .. } => Some(text.len()),
            Block::Mode { text, icon, .. } => Some(icon.len().saturating_add(text.len())),
            Block::Text { content } => Some(content.len()),
            Block::Thinking {
                title,
                summary_titles,
                content,
                ..
            } => Some(thinking_markdown_source_len(
                title.as_deref(),
                summary_titles,
                content.len(),
            )),
            Block::Exec { command, output } => Some(
                2usize
                    .saturating_add(command.len())
                    .saturating_add(1)
                    .saturating_add(output.len()),
            ),
            Block::ToolDraft(_) | Block::ToolCall { .. } => None,
        }
    }

    pub fn content(&self, channel: ContentChannel) -> Option<&TranscriptContent> {
        match (self, channel) {
            (Self::Text { content } | Self::Thinking { content, .. }, ContentChannel::Primary) => {
                Some(content)
            }
            (Self::ToolDraft(draft), ContentChannel::DraftArguments) => Some(&draft.raw_arguments),
            (Self::Exec { output, .. }, ContentChannel::ExecOutput) => Some(output),
            _ => None,
        }
    }

    pub fn registered_contents(&self) -> Vec<&TranscriptContent> {
        match self {
            Self::Text { content } | Self::Thinking { content, .. } => vec![content],
            Self::ToolDraft(draft) => draft.contents().collect(),
            Self::ToolCall { args, .. } => args.contents().collect(),
            Self::Exec { output, .. } => vec![output],
            _ => Vec::new(),
        }
    }

    pub fn tool_name(&self) -> Option<&str> {
        match self {
            Self::ToolDraft(draft) => Some(&draft.name),
            Self::ToolCall { name, .. } => Some(name),
            _ => None,
        }
    }

    pub fn tool_call_id(&self) -> Option<&str> {
        match self {
            Self::ToolDraft(draft) => draft.call_id.as_deref(),
            Self::ToolCall { call_id, .. } => Some(call_id),
            _ => None,
        }
    }

    pub fn process_field(&self, field: &str) -> Option<String> {
        match self {
            Self::ProcessStatus {
                event: Some(event), ..
            } => event.field_value(field),
            _ => None,
        }
    }

    pub fn arg_field(&self, arg: &str) -> Option<&serde_json::Value> {
        match self {
            Self::ToolDraft(draft) => draft.arguments.get(arg),
            Self::ToolCall { args, .. } => args.get(arg),
            _ => None,
        }
    }
}

pub(crate) fn thinking_markdown_source(
    title: Option<&str>,
    summary_titles: &[String],
    content: &str,
) -> String {
    let mut sections = Vec::with_capacity(summary_titles.len().saturating_add(1));
    if summary_titles.is_empty() {
        if let Some(title) = title {
            sections.push(format!("**{title}**"));
        }
    } else {
        sections.extend(summary_titles.iter().map(|title| format!("**{title}**")));
    }
    if !content.is_empty() {
        sections.push(content.to_string());
    }
    sections.join("\n")
}

fn thinking_markdown_source_len(
    title: Option<&str>,
    summary_titles: &[String],
    content_len: usize,
) -> usize {
    let (title_bytes, title_count) = if summary_titles.is_empty() {
        title.map_or((0, 0), |title| (title.len().saturating_add(4), 1))
    } else {
        (
            summary_titles.iter().fold(0usize, |total, title| {
                total.saturating_add(title.len().saturating_add(4))
            }),
            summary_titles.len(),
        )
    };
    let section_count = title_count.saturating_add(usize::from(content_len > 0));
    title_bytes
        .saturating_add(content_len)
        .saturating_add(section_count.saturating_sub(1))
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "scope")]
pub enum ApprovalTarget {
    Session,
    Workspace {
        #[serde(skip)]
        root: std::path::PathBuf,
    },
    Repository {
        #[serde(skip)]
        key: std::path::PathBuf,
    },
}

#[derive(Clone)]
pub struct PermissionEntry {
    pub tool: String,
    pub pattern: String,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ConfirmApprovalOption {
    pub id: String,
    pub label: String,
    #[serde(flatten)]
    pub target: ApprovalTarget,
    #[serde(skip)]
    pub grants: Vec<PermissionGrant>,
}

#[derive(Clone, PartialEq, serde::Serialize)]
pub enum ConfirmChoice {
    Yes,
    No,
    Grant(ConfirmApprovalOption),
}

/// Stable monotonic per-session handle. Mutating a block in place preserves its
/// `BlockId`; content changes are detected via `LayoutKey::content_hash`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct BlockId(pub(crate) u64);

impl BlockId {
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContentChannel {
    Primary,
    ToolOutput,
    ToolPreview,
    ExecOutput,
    DraftArguments,
    DraftField,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptPatchOperation {
    Insert {
        id: BlockId,
        index: usize,
    },
    Append {
        id: BlockId,
        content_id: ContentId,
        channel: ContentChannel,
        byte_range: std::ops::Range<usize>,
    },
    Replace {
        id: BlockId,
    },
    SetStatus {
        id: BlockId,
    },
    SetSideState {
        id: BlockId,
    },
    SetAnimationState {
        id: BlockId,
    },
    Remove {
        id: BlockId,
        index: usize,
    },
    Commit {
        id: BlockId,
    },
    Reset,
}

impl TranscriptPatchOperation {
    fn is_structural(&self) -> bool {
        matches!(
            self,
            Self::Insert { .. } | Self::Remove { .. } | Self::Reset
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptPatch {
    pub revision: u64,
    pub operations: Vec<TranscriptPatchOperation>,
    pub navigation_changed: bool,
    pub search_changed: bool,
    pub persistable_changed: bool,
}

impl TranscriptPatch {
    fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(
            self.operations
                .capacity()
                .saturating_mul(std::mem::size_of::<TranscriptPatchOperation>()),
        )
    }

    fn coalesce<'a>(revision: u64, patches: impl IntoIterator<Item = &'a TranscriptPatch>) -> Self {
        let mut navigation_changed = false;
        let mut search_changed = false;
        let mut persistable_changed = false;
        let mut replaced = HashSet::new();
        let mut status_changed = HashSet::new();
        let mut side_state_changed = HashSet::new();
        let mut animation_state_changed = HashSet::new();
        let mut committed = HashSet::new();
        let mut reset = false;
        for patch in patches {
            navigation_changed |= patch.navigation_changed;
            search_changed |= patch.search_changed;
            persistable_changed |= patch.persistable_changed;
            for operation in &patch.operations {
                match operation {
                    TranscriptPatchOperation::Append { id, .. }
                    | TranscriptPatchOperation::Replace { id } => {
                        replaced.insert(*id);
                    }
                    TranscriptPatchOperation::SetStatus { id } => {
                        status_changed.insert(*id);
                    }
                    TranscriptPatchOperation::SetSideState { id } => {
                        side_state_changed.insert(*id);
                    }
                    TranscriptPatchOperation::SetAnimationState { id } => {
                        animation_state_changed.insert(*id);
                    }
                    TranscriptPatchOperation::Commit { id } => {
                        committed.insert(*id);
                    }
                    TranscriptPatchOperation::Insert { .. }
                    | TranscriptPatchOperation::Remove { .. }
                    | TranscriptPatchOperation::Reset => reset = true,
                }
            }
        }
        let operations = if reset {
            vec![TranscriptPatchOperation::Reset]
        } else {
            let mut operations = Vec::new();
            let mut replaced = replaced.into_iter().collect::<Vec<_>>();
            replaced.sort_by_key(|id| id.get());
            for id in replaced {
                operations.push(TranscriptPatchOperation::Replace { id });
            }
            let mut status_changed = status_changed.into_iter().collect::<Vec<_>>();
            status_changed.sort_by_key(|id| id.get());
            for id in status_changed {
                operations.push(TranscriptPatchOperation::SetStatus { id });
            }
            let mut side_state_changed = side_state_changed.into_iter().collect::<Vec<_>>();
            side_state_changed.sort_by_key(|id| id.get());
            for id in &side_state_changed {
                operations.push(TranscriptPatchOperation::SetSideState { id: *id });
            }
            let mut animation_state_changed = animation_state_changed
                .into_iter()
                .filter(|id| !side_state_changed.contains(id))
                .collect::<Vec<_>>();
            animation_state_changed.sort_by_key(|id| id.get());
            for id in animation_state_changed {
                operations.push(TranscriptPatchOperation::SetAnimationState { id });
            }
            let mut committed = committed.into_iter().collect::<Vec<_>>();
            committed.sort_by_key(|id| id.get());
            for id in committed {
                operations.push(TranscriptPatchOperation::Commit { id });
            }
            operations
        };
        Self {
            revision,
            operations,
            navigation_changed,
            search_changed,
            persistable_changed,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum BlockOrigin {
    History(usize),
    Checkpoint { history_index: usize },
}

/// How the block is presented in the transcript. Independent of [`Status`] -
/// a streaming block can be `Collapsed`. The layout cache keys on this, so
/// flipping view state invalidates only that block.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
pub enum ViewState {
    /// Full content - default.
    #[default]
    Expanded,
    /// A compact live preview; renderers decide exact rows.
    Peek,
    /// One summary line only.
    Collapsed,
    /// Show the first `keep` rows of the block's content, elide the rest.
    TrimmedHead { keep: u16 },
    /// Show the last `keep` rows of the block's content, elide the rest.
    TrimmedTail { keep: u16 },
}

impl ViewState {
    pub fn measured_height(self, total_rows: u64) -> u64 {
        match self {
            Self::Expanded | Self::Peek => total_rows,
            Self::Collapsed => {
                if total_rows > 1 {
                    2
                } else {
                    total_rows
                }
            }
            Self::TrimmedHead { keep } | Self::TrimmedTail { keep } => {
                let keep = keep as u64;
                if total_rows > keep {
                    keep.saturating_add(1)
                } else {
                    total_rows
                }
            }
        }
    }

    pub fn elides_rows(self, total_rows: u64) -> bool {
        match self {
            Self::Expanded | Self::Peek => false,
            Self::Collapsed => total_rows > 1,
            Self::TrimmedHead { keep } | Self::TrimmedTail { keep } => total_rows > keep as u64,
        }
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
pub enum Status {
    Streaming,
    #[default]
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockText<'a> {
    Plain(&'a str),
    Content(&'a TranscriptContent),
    Prefixed {
        prefix: &'a str,
        text: &'a str,
    },
    Thinking {
        title: Option<&'a str>,
        summary_titles: &'a [String],
        content: &'a TranscriptContent,
    },
    Exec {
        command: &'a str,
        output: &'a TranscriptContent,
    },
}

impl BlockText<'_> {
    pub fn first_source_line(self) -> String {
        fn first_nonempty_line(text: &str) -> String {
            text.lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or_default()
                .to_string()
        }

        match self {
            Self::Plain(text) => first_nonempty_line(text),
            Self::Content(content) => content.read().first_nonempty_line(),
            Self::Prefixed { prefix, text } => {
                let first_text_line = text.lines().next().unwrap_or_default();
                let first_line = format!("{prefix}{first_text_line}");
                if first_line.trim().is_empty() {
                    text.lines()
                        .skip(1)
                        .find(|line| !line.trim().is_empty())
                        .unwrap_or_default()
                        .to_string()
                } else {
                    first_line
                }
            }
            Self::Thinking {
                title,
                summary_titles,
                content,
            } => {
                let title = summary_titles.first().map(String::as_str).or(title);
                title
                    .map(|title| format!("**{title}**"))
                    .unwrap_or_else(|| content.read().first_nonempty_line())
            }
            Self::Exec { command, .. } => format!("$ {command}"),
        }
    }
}

/// Cache key for a block's per-frame layout. When content changes, the new
/// `content_hash` misses the old entry - invalidation by keying, not eviction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct LayoutKey {
    pub width: u16,
    pub view_state: ViewState,
    pub content_hash: u64,
    pub sidecar_hash: u64,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TranscriptBlockRecord {
    pub block: Block,
    #[serde(default)]
    pub content_hash: u64,
    pub origin: Option<BlockOrigin>,
    pub tool_state: Option<ToolState>,
    #[serde(default)]
    pub tool_render_revision: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TranscriptBlockRecordWithId {
    pub block_id: BlockId,
    pub record: TranscriptBlockRecord,
}

#[derive(Clone)]
pub struct StoredBlockWithId {
    pub block_id: BlockId,
    pub stored: Arc<StoredBlockRef>,
}

impl TryFrom<smelt_store::StoredTranscriptBlock> for TranscriptBlockRecord {
    type Error = serde_json::Error;

    fn try_from(row: smelt_store::StoredTranscriptBlock) -> Result<Self, Self::Error> {
        let block: Block = serde_json::from_str(&row.block_json)?;
        let origin = row
            .origin_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?
            .or_else(|| {
                row.history_idx
                    .map(|idx| BlockOrigin::History(idx as usize))
            });
        let tool_state = row
            .tool_state_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?;
        let content_hash = row.content_hash.parse::<u64>().unwrap_or_default();
        Ok(Self {
            block,
            content_hash,
            origin,
            tool_state,
            tool_render_revision: row.tool_render_revision,
        })
    }
}

impl TryFrom<smelt_store::StoredTranscriptBlock> for TranscriptBlockRecordWithId {
    type Error = serde_json::Error;

    fn try_from(row: smelt_store::StoredTranscriptBlock) -> Result<Self, Self::Error> {
        let block_id = BlockId::new(row.block_idx);
        let record = TranscriptBlockRecord::try_from(row)?;
        Ok(Self { block_id, record })
    }
}

pub fn compact_block_rows(
    start_record_index: usize,
    rows: Vec<smelt_store::StoredTranscriptBlock>,
) -> Result<Vec<StoredBlockWithId>, serde_json::Error> {
    rows.into_iter()
        .enumerate()
        .map(|(offset, row)| {
            let estimated_text_bytes = row.estimated_text_bytes;
            let preview = row.preview_text.clone();
            let record = TranscriptBlockRecordWithId::try_from(row)?;
            let (block_id, stored) = StoredBlockRef::from_record(
                start_record_index.saturating_add(offset),
                record.block_id,
                &record.record,
                estimated_text_bytes,
                preview,
            );
            Ok(StoredBlockWithId { block_id, stored })
        })
        .collect()
}

const TRANSCRIPT_INDEXED_TEXT_MAX_BYTES: usize = 128 * 1024;
const TOOL_ARG_INDEXED_TEXT_MAX_BYTES: usize = 4 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptIndexedText {
    pub indexed_text: String,
    pub estimated_text_bytes: u64,
}

struct BoundedIndexedText {
    max_bytes: usize,
    total_bytes: usize,
    head: String,
    tail: String,
    tail_offset: usize,
    last_byte: Option<u8>,
}

impl BoundedIndexedText {
    fn new(max_bytes: usize) -> Self {
        debug_assert!(max_bytes > 0);
        Self {
            max_bytes,
            total_bytes: 0,
            head: String::new(),
            tail: String::new(),
            tail_offset: 0,
            last_byte: None,
        }
    }

    fn append_line(&mut self, text: Option<&str>) {
        let Some(text) = text.filter(|text| !text.is_empty()) else {
            return;
        };
        self.ensure_line_separator();
        self.append(text);
    }

    fn ensure_line_separator(&mut self) {
        if self.total_bytes != 0 && self.last_byte != Some(b'\n') {
            self.append("\n");
        }
    }

    fn append(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }

        let previous_total = self.total_bytes;
        if previous_total < self.max_bytes {
            let remaining = self.max_bytes - previous_total;
            let end = if text.len() <= remaining {
                text.len()
            } else {
                smelt_buffer::text::snap(text, remaining)
            };
            self.head.push_str(smelt_buffer::text::slice(text, 0..end));
        }

        if text.len() >= self.max_bytes {
            let minimum = text.len() - self.max_bytes;
            let start = next_char_boundary_at_or_after(text, minimum);
            self.tail.clear();
            self.tail
                .push_str(smelt_buffer::text::slice(text, start..text.len()));
            self.tail_offset = previous_total.saturating_add(start);
        } else {
            self.tail.push_str(text);
            if self.tail.len() > self.max_bytes {
                let minimum = self.tail.len() - self.max_bytes;
                let start = next_char_boundary_at_or_after(&self.tail, minimum);
                smelt_buffer::text::replace_range(&mut self.tail, 0..start, "");
                self.tail_offset = self.tail_offset.saturating_add(start);
            }
        }

        self.total_bytes = previous_total.saturating_add(text.len());
        self.last_byte = text.as_bytes().last().copied();
    }

    fn finish(self) -> TranscriptIndexedText {
        let estimated_text_bytes = self.total_bytes as u64;
        if self.total_bytes <= self.max_bytes {
            return TranscriptIndexedText {
                indexed_text: self.head,
                estimated_text_bytes,
            };
        }

        let head_end = smelt_buffer::text::snap(&self.head, self.max_bytes / 2);
        let tail_minimum = self
            .total_bytes
            .saturating_sub(self.max_bytes.saturating_sub(head_end));
        let local_minimum = tail_minimum.saturating_sub(self.tail_offset);
        let local_start = next_char_boundary_at_or_after(&self.tail, local_minimum);
        let tail_start = self.tail_offset.saturating_add(local_start);
        let omitted_bytes = tail_start.saturating_sub(head_end);
        let marker = format!("\n… {omitted_bytes} bytes omitted from persistent search index …\n");
        let mut indexed_text = String::with_capacity(
            head_end
                .saturating_add(marker.len())
                .saturating_add(self.tail.len().saturating_sub(local_start)),
        );
        indexed_text.push_str(smelt_buffer::text::slice(&self.head, 0..head_end));
        indexed_text.push_str(&marker);
        indexed_text.push_str(smelt_buffer::text::slice(
            &self.tail,
            local_start..self.tail.len(),
        ));
        TranscriptIndexedText {
            indexed_text,
            estimated_text_bytes,
        }
    }
}

fn next_char_boundary_at_or_after(text: &str, minimum: usize) -> usize {
    let snapped = smelt_buffer::text::snap(text, minimum);
    if snapped == minimum {
        snapped
    } else {
        smelt_buffer::text::next_char_boundary(text, minimum)
    }
}

pub fn transcript_indexed_text(
    block: &Block,
    tool_state: Option<&ToolState>,
) -> TranscriptIndexedText {
    let mut text = BoundedIndexedText::new(TRANSCRIPT_INDEXED_TEXT_MAX_BYTES);
    if block.tool_name().is_some() {
        append_tool_indexed_text(&mut text, block, tool_state);
    } else {
        append_block_raw_indexed_text(&mut text, block);
        text.append_line(thinking_summary(block).as_deref());
        text.append_line(compacted_label(block));
        text.append_line(compacted_separator(block));
    }
    text.finish()
}

fn append_block_raw_indexed_text(text: &mut BoundedIndexedText, block: &Block) {
    match block {
        Block::User { text: source, .. }
        | Block::ProcessStatus { text: source, .. }
        | Block::Compacted { summary: source }
        | Block::CompactionPreview { summary: source }
        | Block::CodeLine {
            content: source, ..
        } => text.append(source),
        Block::Mode {
            text: source, icon, ..
        } => {
            text.append(icon);
            text.append(source);
        }
        Block::Text { content } => append_raw_indexed_content(text, content),
        Block::Thinking {
            title,
            summary_titles,
            content,
            ..
        } => append_thinking_indexed_text(text, title.as_deref(), summary_titles, content),
        Block::Exec { command, output } => {
            text.append("$ ");
            text.append(command);
            text.append("\n");
            append_raw_indexed_content(text, output);
        }
        Block::ToolDraft(_) | Block::ToolCall { .. } => {}
    }
}

fn append_thinking_indexed_text(
    text: &mut BoundedIndexedText,
    title: Option<&str>,
    summary_titles: &[String],
    content: &TranscriptContent,
) {
    let mut has_section = false;
    if summary_titles.is_empty() {
        if let Some(title) = title {
            append_thinking_title(text, title, has_section);
            has_section = true;
        }
    } else {
        for title in summary_titles {
            append_thinking_title(text, title, has_section);
            has_section = true;
        }
    }
    if !content.is_empty() {
        if has_section {
            text.append("\n");
        }
        append_raw_indexed_content(text, content);
    }
}

fn append_thinking_title(text: &mut BoundedIndexedText, title: &str, separate: bool) {
    if separate {
        text.append("\n");
    }
    text.append("**");
    text.append(title);
    text.append("**");
}

fn append_tool_indexed_text(
    text: &mut BoundedIndexedText,
    block: &Block,
    tool_state: Option<&ToolState>,
) {
    text.append_line(block.tool_name());
    text.append_line(tool_state.map(|state| state.status.label()));
    text.append_line(tool_summary_text(block).as_deref());
    text.append_line(tool_arg_indexed_text(block).as_deref());
    text.append_line(tool_state.and_then(|state| state.user_message.as_deref()));
    if let Some(output) = tool_state.and_then(|state| state.preview_output.as_ref()) {
        append_indexed_content(text, Some(&output.content));
    }
    if let Some(output) = tool_state.and_then(|state| state.output.as_ref()) {
        append_indexed_content(text, Some(&output.content));
    }
    append_edit_file_indexed_text(text, block, tool_state);
    if let Some(display_count) = tool_state.and_then(display_count_indexed_text) {
        text.append_line(Some(&display_count));
    }
}

fn tool_summary_text(block: &Block) -> Option<String> {
    match block {
        Block::ToolDraft(draft) => Some(draft.summary.as_plain_text()),
        Block::ToolCall { summary, .. } => Some(summary.as_plain_text()),
        _ => None,
    }
    .filter(|summary| !summary.is_empty())
}

fn tool_arg_indexed_text(block: &Block) -> Option<String> {
    let (tool_name, args) = match block {
        Block::ToolDraft(draft) => (draft.name.as_str(), draft.arguments.preview()),
        Block::ToolCall { name, args, .. } => (name.as_str(), args.preview()),
        _ => return None,
    };
    if tool_name == "edit_file" {
        return None;
    }

    let mut fields = args.iter().collect::<Vec<_>>();
    fields.sort_by_key(|(key, _)| *key);
    let mut text = String::new();
    for (key, value) in fields {
        if !searchable_tool_arg_key(key) || bulky_tool_arg_key(key) {
            continue;
        }
        let Some(value) = tool_arg_value_text(value) else {
            continue;
        };
        if value.len() > TOOL_ARG_INDEXED_TEXT_MAX_BYTES {
            continue;
        }
        append_indexed_line(&mut text, Some(&format!("{key}: {value}")));
    }
    (!text.is_empty()).then_some(text)
}

fn searchable_tool_arg_key(key: &str) -> bool {
    matches!(
        key,
        "base"
            | "cell_number"
            | "cell_type"
            | "command"
            | "description"
            | "edit_mode"
            | "file_path"
            | "format"
            | "glob"
            | "name"
            | "notebook_path"
            | "output_mode"
            | "path"
            | "pattern"
            | "prompt"
            | "query"
            | "type"
            | "url"
    )
}

fn bulky_tool_arg_key(key: &str) -> bool {
    matches!(
        key,
        "content"
            | "new_content"
            | "new_source"
            | "new_string"
            | "old_content"
            | "old_string"
            | "source"
    )
}

fn tool_arg_value_text(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Number(number) => Some(number.to_string()),
        serde_json::Value::Bool(value) => Some(value.to_string()),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            serde_json::to_string(value).ok()
        }
        serde_json::Value::Null => None,
    }
}

fn append_indexed_line(out: &mut String, text: Option<&str>) {
    let Some(text) = text.filter(|text| !text.is_empty()) else {
        return;
    };
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(text);
}

fn append_indexed_content(out: &mut BoundedIndexedText, content: Option<&TranscriptContent>) {
    let Some(content) = content else {
        return;
    };
    let read = content.read();
    if read.is_empty() {
        return;
    }
    out.ensure_line_separator();
    for chunk in read.chunks() {
        out.append(chunk);
    }
}

fn append_raw_indexed_content(out: &mut BoundedIndexedText, content: &TranscriptContent) {
    let read = content.read();
    for chunk in read.chunks() {
        out.append(chunk);
    }
}

fn thinking_summary(block: &Block) -> Option<String> {
    let Block::Thinking { title, content, .. } = block else {
        return None;
    };
    let (inferred_label, line_count) = thinking_summary_label(content);
    let label = title.as_deref().unwrap_or(&inferred_label);
    let collapsed_lines = if title.is_some() || inferred_label == "thinking" {
        line_count
    } else {
        line_count.saturating_sub(1)
    };
    Some(format!(
        "{label}\n… {} …",
        pluralize(collapsed_lines, "line collapsed", "lines collapsed")
    ))
}

fn thinking_summary_label(content: &TranscriptContent) -> (String, usize) {
    let read = content.read();
    let mut label = None;
    let mut lines = 0usize;
    for line_index in 0..read.logical_line_count() {
        let Some(line) = read.line(line_index) else {
            continue;
        };
        let trimmed = smelt_buffer::text::trim_whitespace(&line);
        if trimmed.is_empty() {
            continue;
        }
        lines += 1;
        if label.is_none()
            && trimmed.starts_with("**")
            && trimmed.ends_with("**")
            && trimmed.len() > 4
        {
            label = trimmed
                .strip_prefix("**")
                .and_then(|inner| inner.strip_suffix("**"))
                .map(smelt_buffer::text::trim_whitespace)
                .filter(|inner| !inner.is_empty())
                .map(str::to_string);
        }
    }
    (label.unwrap_or_else(|| "thinking".to_string()), lines)
}

fn pluralize(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {plural}")
    }
}

fn compacted_label(block: &Block) -> Option<&'static str> {
    matches!(block, Block::Compacted { .. }).then_some("compacted")
}

fn compacted_separator(block: &Block) -> Option<&'static str> {
    matches!(block, Block::Compacted { .. }).then_some("─")
}

fn append_edit_file_indexed_text(
    text: &mut BoundedIndexedText,
    block: &Block,
    tool_state: Option<&ToolState>,
) {
    let Some(args) = edit_file_args(block) else {
        return;
    };
    let old_string = string_field(args, "old_string").unwrap_or_default();
    let new_string = string_field(args, "new_string").unwrap_or_default();
    text.append_line(Some(&replacement_line_detail(old_string, new_string)));
    text.append_line(string_field(args, "file_path"));

    let output = tool_state.and_then(|state| state.output.as_deref());
    let old_content = output.and_then(|output| output.content_field("old_content"));
    let new_content = output.and_then(|output| output.content_field("new_content"));
    append_indexed_content(text, old_content);
    append_indexed_content(text, new_content);
    if old_content.is_none() && new_content.is_none() {
        text.append_line(Some(old_string));
        text.append_line(Some(new_string));
    }
}

fn edit_file_args(block: &Block) -> Option<&std::collections::HashMap<String, serde_json::Value>> {
    match block {
        Block::ToolDraft(draft) if draft.name == "edit_file" => Some(draft.arguments.preview()),
        Block::ToolCall { name, args, .. } if name == "edit_file" => Some(args.preview()),
        _ => None,
    }
}

fn string_field<'a>(
    fields: &'a std::collections::HashMap<String, serde_json::Value>,
    key: &str,
) -> Option<&'a str> {
    fields.get(key).and_then(serde_json::Value::as_str)
}

fn replacement_line_detail(old_text: &str, new_text: &str) -> String {
    format!(
        "{}, {}",
        line_label(line_count(old_text), "old line"),
        line_label(line_count(new_text), "new line")
    )
}

fn line_count(text: &str) -> usize {
    text.lines().count()
}

fn line_label(count: usize, label: &str) -> String {
    format!("{} {}{}", count, label, if count == 1 { "" } else { "s" })
}

fn display_count_indexed_text(state: &ToolState) -> Option<String> {
    let metadata = state.output.as_ref()?.metadata.as_ref()?;
    let display_count = metadata.get("display_count")?.as_object()?;
    let (count, count_text) = display_count_count_text(display_count.get("value"));
    let unit = display_count
        .get("unit")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("item");
    let plural = display_count
        .get("plural")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            if unit == "match" {
                "matches".to_string()
            } else {
                format!("{unit}s")
            }
        });
    let label = if count == 1.0 { unit } else { &plural };
    Some(format!("{count_text} {label}"))
}

fn display_count_count_text(value: Option<&serde_json::Value>) -> (f64, String) {
    let count = value.and_then(display_count_count).unwrap_or(0.0);
    (count, format_lua_number(count))
}

fn display_count_count(value: &serde_json::Value) -> Option<f64> {
    match value {
        serde_json::Value::Number(number) => number.as_f64(),
        serde_json::Value::String(text) => text.parse::<f64>().ok(),
        _ => None,
    }
    .filter(|value| value.is_finite())
}

fn format_lua_number(value: f64) -> String {
    if value.fract() == 0.0 {
        (value as i64).to_string()
    } else {
        value.to_string()
    }
}

pub fn transcript_block_row(
    record_idx: usize,
    record: &TranscriptBlockRecord,
) -> Result<smelt_store::StoredTranscriptBlock, smelt_store::StoreError> {
    transcript_block_row_with_block_idx(record_idx, record_idx as u64, record)
}

pub fn transcript_block_row_with_block_idx(
    _record_idx: usize,
    block_idx: u64,
    record: &TranscriptBlockRecord,
) -> Result<smelt_store::StoredTranscriptBlock, smelt_store::StoreError> {
    let indexed_text = transcript_indexed_text(&record.block, record.tool_state.as_ref());
    let block_json = serde_json::to_string(&record.block)?;
    let origin_json = record
        .origin
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;
    let tool_state_json = record
        .tool_state
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;
    let history_idx = match record.origin {
        Some(BlockOrigin::History(idx)) => Some(idx as u64),
        _ => None,
    };
    Ok(smelt_store::StoredTranscriptBlock {
        block_idx,
        history_idx,
        kind: record.block.kind().to_string(),
        tool_call_id: record.block.tool_call_id().map(str::to_string),
        tool_name: record.block.tool_name().map(str::to_string),
        content_hash: record.content_hash.to_string(),
        estimated_text_bytes: indexed_text.estimated_text_bytes,
        preview_text: preview(&indexed_text.indexed_text, 512),
        indexed_text: indexed_text.indexed_text,
        block_json,
        origin_json,
        tool_state_json,
        tool_render_revision: record.tool_render_revision,
    })
}

fn preview(text: &str, max_bytes: usize) -> String {
    smelt_buffer::text::grapheme_prefix(text, max_bytes).to_string()
}

const STORED_SELECTOR_VALUE_MAX_BYTES: usize = 512;
const STORED_SELECTOR_FIELDS_MAX_BYTES: usize = 4 * 1024;
const GROUP_CHILD_SUMMARY_MAX_BYTES: usize = 4 * 1024;
const GROUP_CHILD_PROCESS_FIELD_MAX_BYTES: usize = 512;

fn bounded_selector_fields(
    args: &HashMap<String, serde_json::Value>,
) -> HashMap<String, serde_json::Value> {
    let mut fields = args.iter().collect::<Vec<_>>();
    fields.sort_unstable_by_key(|(key, _)| *key);
    let mut remaining = STORED_SELECTOR_FIELDS_MAX_BYTES;
    fields
        .into_iter()
        .filter_map(|(key, value)| {
            let encoded_len = serde_json::to_vec(value).ok()?.len();
            let retained_len = key.len().saturating_add(encoded_len);
            if encoded_len > STORED_SELECTOR_VALUE_MAX_BYTES || retained_len > remaining {
                return None;
            }
            remaining = remaining.saturating_sub(retained_len);
            Some((key.clone(), value.clone()))
        })
        .collect()
}

fn bounded_styled_text(lines: &protocol::StyledLines, max_bytes: usize) -> String {
    let mut text = String::with_capacity(max_bytes.min(256));
    for (line_index, line) in lines.0.iter().enumerate() {
        if line_index > 0 {
            if text.len() == max_bytes {
                break;
            }
            text.push('\n');
        }
        for span in line {
            let remaining = max_bytes.saturating_sub(text.len());
            if remaining == 0 {
                return text;
            }
            text.push_str(smelt_buffer::text::slice(&span.text, 0..remaining));
        }
    }
    text
}

fn group_child_summary(block: &Block) -> Option<String> {
    match block {
        Block::ToolDraft(draft) => Some(bounded_styled_text(
            &draft.summary,
            GROUP_CHILD_SUMMARY_MAX_BYTES,
        )),
        Block::ToolCall { summary, .. } => {
            Some(bounded_styled_text(summary, GROUP_CHILD_SUMMARY_MAX_BYTES))
        }
        _ => None,
    }
}

fn group_child_args(block: &Block) -> Option<HashMap<String, serde_json::Value>> {
    match block {
        Block::ToolDraft(draft) => Some(bounded_selector_fields(draft.arguments.preview())),
        Block::ToolCall { args, .. } => Some(bounded_selector_fields(args.preview())),
        _ => None,
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct TranscriptGroupChildOutputMetadata {
    pub content_lines: Option<usize>,
    pub is_error: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct TranscriptGroupChildProcessMetadata {
    pub process_id: Option<String>,
    pub exit_code: Option<i32>,
    pub termination: Option<protocol::JobTermination>,
}

/// Payload-independent semantic data exposed for one retained group child.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct TranscriptGroupChildMetadata {
    pub id: BlockId,
    pub kind: &'static str,
    pub name: Option<String>,
    pub status: Option<&'static str>,
    pub summary_text: Option<String>,
    pub called_at_ms: Option<u64>,
    pub args: Option<HashMap<String, serde_json::Value>>,
    pub output: TranscriptGroupChildOutputMetadata,
    pub event: Option<String>,
    pub process_id: Option<String>,
    pub exit_code: Option<i32>,
    pub termination: Option<protocol::JobTermination>,
    pub event_data: TranscriptGroupChildProcessMetadata,
}

#[derive(Clone, Debug, Default)]
struct StoredProcessMetadata {
    event: Option<String>,
    process_id: Option<String>,
    exit_code: Option<i32>,
    termination: Option<protocol::JobTermination>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoredBlockKind {
    User,
    Mode,
    ProcessStatus,
    Thinking,
    Assistant,
    Code,
    Tool,
    Exec,
    Compacted,
    CompactionPreview,
}

impl StoredBlockKind {
    fn from_kind(kind: &str) -> Option<Self> {
        Some(match kind {
            "user" => Self::User,
            "mode" => Self::Mode,
            "process_status" => Self::ProcessStatus,
            "thinking" => Self::Thinking,
            "assistant" => Self::Assistant,
            "code" => Self::Code,
            "tool" => Self::Tool,
            "exec" => Self::Exec,
            "compacted" => Self::Compacted,
            "compaction_preview" => Self::CompactionPreview,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Mode => "mode",
            Self::ProcessStatus => "process_status",
            Self::Thinking => "thinking",
            Self::Assistant => "assistant",
            Self::Code => "code",
            Self::Tool => "tool",
            Self::Exec => "exec",
            Self::Compacted => "compacted",
            Self::CompactionPreview => "compaction_preview",
        }
    }
}

/// Compact canonical locator and render-plan metadata for a durable block.
/// Full block JSON, tool output, and block text remain in SQLite.
#[derive(Clone, Debug)]
pub struct StoredBlockRef {
    pub record_index: usize,
    pub kind: StoredBlockKind,
    pub preview: String,
    pub estimated_text_bytes: u64,
    pub content_hash: u64,
    pub tool_call_id: Option<String>,
    pub tool_name: Option<String>,
    pub tool_status: Option<ToolStatus>,
    pub tool_render_revision: u64,
    pub origin: Option<BlockOrigin>,
    pub stable_scroll_anchor: bool,
    tool_draft: bool,
    group_summary_text: Option<String>,
    group_called_at_ms: Option<u64>,
    group_output: TranscriptGroupChildOutputMetadata,
    group_process: StoredProcessMetadata,
    starts_with_thinking_title: bool,
    ends_with_heading: bool,
    selector_fields: HashMap<String, serde_json::Value>,
    retained_bytes: usize,
}

impl StoredBlockRef {
    pub fn from_record(
        record_index: usize,
        block_id: BlockId,
        record: &TranscriptBlockRecord,
        estimated_text_bytes: u64,
        preview: String,
    ) -> (BlockId, Arc<Self>) {
        let block = &record.block;
        let tool_state = record.tool_state.as_ref();
        let kind = StoredBlockKind::from_kind(block.kind())
            .expect("transcript blocks use a known block kind");
        let tool_call_id = block.tool_call_id().map(str::to_string);
        let tool_name = block.tool_name().map(str::to_string);
        let selector_fields = group_child_args(block).unwrap_or_default();
        let tool_draft = matches!(block, Block::ToolDraft(_));
        let group_summary_text = group_child_summary(block);
        let group_called_at_ms = tool_state.and_then(|state| state.called_at_ms);
        let group_output = tool_state
            .and_then(|state| state.output.as_deref())
            .map(|output| TranscriptGroupChildOutputMetadata {
                content_lines: Some(output.content.read().logical_line_count()),
                is_error: Some(output.is_error),
            })
            .unwrap_or_default();
        let group_process = match block {
            Block::ProcessStatus {
                event: Some(event), ..
            } => StoredProcessMetadata {
                event: Some(event.event_type().to_string()),
                process_id: event.process_id().map(|value| {
                    smelt_buffer::text::slice(value, 0..GROUP_CHILD_PROCESS_FIELD_MAX_BYTES)
                        .to_string()
                }),
                exit_code: event.exit_code(),
                termination: Some(event.termination()),
            },
            _ => StoredProcessMetadata::default(),
        };
        let starts_with_thinking_title = match block {
            Block::Thinking { title, content, .. } => has_thinking_title(title.as_deref(), content),
            _ => false,
        };
        let ends_with_heading = match block {
            Block::Text { content } => content.ends_with_markdown_heading(),
            _ => false,
        };
        let retained_bytes = std::mem::size_of::<Self>()
            .saturating_add(preview.capacity())
            .saturating_add(tool_call_id.as_ref().map_or(0, String::capacity))
            .saturating_add(tool_name.as_ref().map_or(0, String::capacity))
            .saturating_add(group_summary_text.as_ref().map_or(0, String::capacity))
            .saturating_add(group_process.event.as_ref().map_or(0, String::capacity))
            .saturating_add(
                group_process
                    .process_id
                    .as_ref()
                    .map_or(0, String::capacity),
            )
            .saturating_add(
                selector_fields.capacity().saturating_mul(
                    std::mem::size_of::<(String, serde_json::Value)>()
                        .saturating_add(std::mem::size_of::<usize>()),
                ),
            )
            .saturating_add(
                selector_fields
                    .iter()
                    .map(|(key, value)| {
                        key.capacity()
                            .saturating_add(protocol::json_value_dynamic_retained_bytes(value))
                    })
                    .sum::<usize>(),
            );
        (
            block_id,
            Arc::new(Self {
                record_index,
                kind,
                preview,
                estimated_text_bytes,
                content_hash: if record.content_hash == 0 {
                    block.content_hash()
                } else {
                    record.content_hash
                },
                tool_call_id,
                tool_name,
                tool_status: match block {
                    Block::ToolCall { .. } => {
                        Some(tool_state.map_or(ToolStatus::Pending, |state| state.status))
                    }
                    _ => tool_state.map(|state| state.status),
                },
                tool_render_revision: record.tool_render_revision,
                origin: record.origin,
                stable_scroll_anchor: !matches!(
                    block,
                    Block::ToolDraft(ToolDraft {
                        finished: false,
                        ..
                    })
                ),
                tool_draft,
                group_summary_text,
                group_called_at_ms,
                group_output,
                group_process,
                starts_with_thinking_title,
                ends_with_heading,
                selector_fields,
                retained_bytes,
            }),
        )
    }

    pub fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    pub fn first_line(&self) -> &str {
        self.preview
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("")
    }

    fn arg_field(&self, arg: &str) -> Option<&serde_json::Value> {
        self.selector_fields.get(arg)
    }

    fn process_field(&self, field: &str) -> Option<String> {
        match field {
            "event" | "event_type" => self.group_process.event.clone(),
            "process_id" => self.group_process.process_id.clone(),
            "exit_code" => self.group_process.exit_code.map(|code| code.to_string()),
            "termination" => self
                .group_process
                .termination
                .map(|termination| termination.as_str().to_string()),
            _ => None,
        }
    }

    fn group_child_metadata(&self, id: BlockId) -> TranscriptGroupChildMetadata {
        let process_id = self.group_process.process_id.clone();
        let exit_code = self.group_process.exit_code;
        let termination = self.group_process.termination;
        TranscriptGroupChildMetadata {
            id,
            kind: self.kind.as_str(),
            name: self.tool_name.clone(),
            status: if self.tool_draft {
                Some("drafting")
            } else {
                self.tool_status.map(ToolStatus::label)
            },
            summary_text: self.group_summary_text.clone(),
            called_at_ms: self.group_called_at_ms,
            args: (self.kind == StoredBlockKind::Tool).then(|| self.selector_fields.clone()),
            output: self.group_output.clone(),
            event: self.group_process.event.clone(),
            process_id: process_id.clone(),
            exit_code,
            termination,
            event_data: TranscriptGroupChildProcessMetadata {
                process_id,
                exit_code,
                termination,
            },
        }
    }
}

fn materialized_group_child_metadata(
    id: BlockId,
    block: &Block,
    tool_state: Option<&ToolState>,
) -> TranscriptGroupChildMetadata {
    let output = tool_state
        .and_then(|state| state.output.as_deref())
        .map(|output| TranscriptGroupChildOutputMetadata {
            content_lines: Some(output.content.read().logical_line_count()),
            is_error: Some(output.is_error),
        })
        .unwrap_or_default();
    let (event, process_id, exit_code, termination) = match block {
        Block::ProcessStatus {
            event: Some(event), ..
        } => (
            Some(event.event_type().to_string()),
            event
                .process_id()
                .map(|value| preview(value, GROUP_CHILD_PROCESS_FIELD_MAX_BYTES)),
            event.exit_code(),
            Some(event.termination()),
        ),
        _ => (None, None, None, None),
    };
    TranscriptGroupChildMetadata {
        id,
        kind: block.kind(),
        name: block.tool_name().map(str::to_string),
        status: match block {
            Block::ToolDraft(_) => Some("drafting"),
            Block::ToolCall { .. } => Some(
                tool_state
                    .map_or(ToolStatus::Pending, |state| state.status)
                    .label(),
            ),
            _ => None,
        },
        summary_text: group_child_summary(block),
        called_at_ms: tool_state.and_then(|state| state.called_at_ms),
        args: group_child_args(block),
        output,
        event,
        process_id: process_id.clone(),
        exit_code,
        termination,
        event_data: TranscriptGroupChildProcessMetadata {
            process_id,
            exit_code,
            termination,
        },
    }
}

impl ToolOutputContentField {
    fn dynamic_retained_bytes(&self) -> usize {
        self.name
            .capacity()
            .saturating_add(self.content.dynamic_retained_bytes())
    }
}

impl ToolOutput {
    fn dynamic_retained_bytes(&self) -> usize {
        self.content
            .dynamic_retained_bytes()
            .saturating_add(
                self.metadata
                    .as_ref()
                    .map_or(0, protocol::json_value_dynamic_retained_bytes),
            )
            .saturating_add(
                self.content_fields
                    .capacity()
                    .saturating_mul(std::mem::size_of::<ToolOutputContentField>()),
            )
            .saturating_add(
                self.content_fields
                    .iter()
                    .map(ToolOutputContentField::dynamic_retained_bytes)
                    .sum::<usize>(),
            )
    }

    fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(self.dynamic_retained_bytes())
    }
}

impl ToolState {
    fn dynamic_retained_bytes(&self) -> usize {
        self.output
            .as_deref()
            .map_or(0, ToolOutput::retained_bytes)
            .saturating_add(self.user_message.as_ref().map_or(0, String::capacity))
            .saturating_add(
                self.preview_output
                    .as_deref()
                    .map_or(0, ToolOutput::retained_bytes),
            )
    }

    fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(self.dynamic_retained_bytes())
    }
}

impl Block {
    fn dynamic_retained_bytes(&self) -> usize {
        match self {
            Self::User {
                text, image_labels, ..
            } => text
                .capacity()
                .saturating_add(
                    image_labels
                        .capacity()
                        .saturating_mul(std::mem::size_of::<String>()),
                )
                .saturating_add(image_labels.iter().map(String::capacity).sum::<usize>()),
            Self::Mode {
                text,
                icon,
                hl_group,
            } => text
                .capacity()
                .saturating_add(icon.capacity())
                .saturating_add(hl_group.capacity()),
            Self::ProcessStatus { text, event } => {
                text.capacity()
                    .saturating_add(event.as_ref().map_or(0, |event| match event {
                        protocol::ProcessStatusEvent::BackgroundProcessCompleted {
                            process_id,
                            ..
                        } => process_id.capacity(),
                    }))
            }
            Self::Thinking {
                title,
                summary_titles,
                content,
                ..
            } => title
                .as_ref()
                .map_or(0, String::capacity)
                .saturating_add(
                    summary_titles
                        .capacity()
                        .saturating_mul(std::mem::size_of::<String>()),
                )
                .saturating_add(summary_titles.iter().map(String::capacity).sum::<usize>())
                .saturating_add(content.dynamic_retained_bytes()),
            Self::Text { content } => content.dynamic_retained_bytes(),
            Self::CodeLine { content, lang } => content.capacity().saturating_add(lang.capacity()),
            Self::ToolDraft(draft) => draft.dynamic_retained_bytes(),
            Self::ToolCall {
                call_id,
                name,
                summary,
                args,
            } => call_id
                .capacity()
                .saturating_add(name.capacity())
                .saturating_add(summary.dynamic_retained_bytes())
                .saturating_add(args.dynamic_retained_bytes()),
            Self::Exec { command, output } => command
                .capacity()
                .saturating_add(output.dynamic_retained_bytes()),
            Self::Compacted { summary } | Self::CompactionPreview { summary } => summary.capacity(),
        }
    }

    fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(self.dynamic_retained_bytes())
    }
}

pub fn block_retained_bytes(block: &Block) -> usize {
    block.retained_bytes()
}

pub fn tool_state_retained_bytes(state: &ToolState) -> usize {
    state.retained_bytes()
}

#[derive(Clone)]
enum ToolStateEntry {
    Live {
        state: ToolState,
        render_revision: u64,
    },
    Hydrated {
        state: ToolState,
        render_revision: u64,
    },
    Stored {
        status: ToolStatus,
        render_revision: u64,
    },
}

impl ToolStateEntry {
    fn state(&self) -> Option<&ToolState> {
        match self {
            Self::Live { state, .. } | Self::Hydrated { state, .. } => Some(state),
            Self::Stored { .. } => None,
        }
    }

    fn render_revision(&self) -> u64 {
        match self {
            Self::Live {
                render_revision, ..
            }
            | Self::Hydrated {
                render_revision, ..
            }
            | Self::Stored {
                render_revision, ..
            } => *render_revision,
        }
    }

    fn status(&self) -> ToolStatus {
        match self {
            Self::Live { state, .. } | Self::Hydrated { state, .. } => state.status,
            Self::Stored { status, .. } => *status,
        }
    }
}

#[derive(Clone, Copy, Default)]
struct BlockMetadata {
    content_hash: u64,
    live_revision: u64,
    navigation_open: bool,
    status: Status,
    origin: Option<BlockOrigin>,
}

struct LiveBlock {
    block: Block,
    metadata: BlockMetadata,
}

fn append_navigation_open(block: &Block) -> bool {
    match block {
        Block::Text { content } | Block::Thinking { content, .. } => !content.contains("\n"),
        _ => false,
    }
}

enum BlockEntry {
    Live(Box<LiveBlock>),
    Stored(Arc<StoredBlockRef>),
    Hydrated {
        stored: Arc<StoredBlockRef>,
        block: Box<Block>,
        block_weight: usize,
        tool_state_weight: usize,
    },
}

impl BlockEntry {
    fn block(&self) -> Option<&Block> {
        match self {
            Self::Live(live) => Some(&live.block),
            Self::Hydrated { block, .. } => Some(block),
            Self::Stored(_) => None,
        }
    }

    fn into_materialized(self) -> Option<Block> {
        match self {
            Self::Live(live) => Some(live.block),
            Self::Hydrated { block, .. } => Some(*block),
            Self::Stored(_) => None,
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::Live(live) => live.block.kind(),
            Self::Hydrated { block, .. } => block.kind(),
            Self::Stored(stored) => stored.kind.as_str(),
        }
    }

    fn tool_name(&self) -> Option<&str> {
        match self {
            Self::Stored(stored) => stored.tool_name.as_deref(),
            _ => self.block()?.tool_name(),
        }
    }

    fn tool_call_id(&self) -> Option<&str> {
        match self {
            Self::Stored(stored) => stored.tool_call_id.as_deref(),
            _ => self.block()?.tool_call_id(),
        }
    }

    fn process_field(&self, field: &str) -> Option<String> {
        match self {
            Self::Stored(stored) => stored.process_field(field),
            _ => match self.block()? {
                Block::ProcessStatus {
                    event: Some(event), ..
                } => event.field_value(field),
                _ => None,
            },
        }
    }

    fn arg_field(&self, arg: &str) -> Option<&serde_json::Value> {
        match self {
            Self::Stored(stored) => stored.arg_field(arg),
            _ => self.block()?.arg_field(arg),
        }
    }

    fn is_tool_draft(&self) -> bool {
        match self {
            Self::Stored(stored) => stored.tool_draft,
            _ => self
                .block()
                .is_some_and(|block| matches!(block, Block::ToolDraft(_))),
        }
    }

    fn is_persisted_block(&self) -> bool {
        match self {
            Self::Stored(stored) => {
                stored.kind != StoredBlockKind::CompactionPreview && !stored.tool_draft
            }
            Self::Live(live) => !matches!(
                live.block,
                Block::ToolDraft(_) | Block::CompactionPreview { .. }
            ),
            Self::Hydrated { block, .. } => !matches!(
                block.as_ref(),
                Block::ToolDraft(_) | Block::CompactionPreview { .. }
            ),
        }
    }

    fn row_estimate_text(&self) -> Option<BlockText<'_>> {
        self.block().and_then(Block::row_estimate_text).or_else(|| {
            let Self::Stored(stored) = self else {
                return None;
            };
            (!stored.preview.is_empty()).then_some(BlockText::Plain(stored.preview.as_str()))
        })
    }

    fn estimated_text_bytes(&self) -> u64 {
        match self {
            Self::Stored(stored) | Self::Hydrated { stored, .. } => stored.estimated_text_bytes,
            Self::Live(live) => live.block.raw_text_len().unwrap_or_default() as u64,
        }
    }

    fn navigation_signature(&self) -> (&'static str, String) {
        (
            self.kind(),
            self.row_estimate_text()
                .map(BlockText::first_source_line)
                .unwrap_or_default(),
        )
    }

    fn raw_text(&self) -> Option<String> {
        self.block().and_then(Block::raw_text)
    }

    fn cloned_block(&self) -> Option<Block> {
        self.block().cloned()
    }

    fn stored(&self) -> Option<&Arc<StoredBlockRef>> {
        match self {
            Self::Stored(stored) | Self::Hydrated { stored, .. } => Some(stored),
            Self::Live(_) => None,
        }
    }

    fn hydrated_weight(&self) -> usize {
        match self {
            Self::Hydrated {
                block_weight,
                tool_state_weight,
                ..
            } => (*block_weight).saturating_add(*tool_state_weight),
            _ => 0,
        }
    }

    fn hydrated_block_weight(&self) -> usize {
        match self {
            Self::Hydrated { block_weight, .. } => *block_weight,
            _ => 0,
        }
    }

    fn hydrated_tool_state_weight(&self) -> usize {
        match self {
            Self::Hydrated {
                tool_state_weight, ..
            } => *tool_state_weight,
            _ => 0,
        }
    }
}

const RECORD_CHANGE_LOG_CAPACITY: usize = 64;
const TRANSCRIPT_PATCH_LOG_BUDGET_BYTES: usize = 64 * 1024;

pub struct BlockHistory {
    pub order: Vec<BlockId>,
    order_indices: HashMap<BlockId, usize>,
    order_indices_valid: bool,
    entries: HashMap<BlockId, BlockEntry>,
    content_store: ContentStore,
    pub(crate) next_id: u64,
    tool_states: HashMap<BlockId, ToolStateEntry>,
    /// Blocks that transitioned `Streaming` → `Done` since last drain;
    /// drained by the app loop to emit `block_done` autocmds.
    pub finished_blocks: Vec<BlockId>,
    /// Bumped on every mutation; used by `TranscriptSnapshot` to detect staleness.
    generation: u64,
    /// High-water mark for payload-independent tool presentation revisions.
    tool_render_revision_clock: u64,
    /// Bumped only when transcript order changes, so projections can reuse
    /// block-to-node structure across content and sidecar updates.
    order_generation: u64,
    /// Bumped when semantic navigation coordinates, roles, or labels may change.
    navigation_generation: u64,
    /// Number of ordered entries represented by canonical transcript blocks.
    persisted_block_count: usize,
    /// Hydrated entries and their retained memory, maintained across entry transitions.
    hydrated_ids: HashSet<BlockId>,
    hydrated_block_bytes: usize,
    hydrated_tool_state_bytes: usize,
    /// Earliest transcript order index whose persisted block may be stale.
    record_dirty_from: Option<usize>,
    /// Bumped when a persisted block may have changed. Unlike `generation`,
    /// this ignores display-only status changes.
    record_dirty_generation: u64,
    /// Recent block mutation boundaries remain available after persistence
    /// clears `block_dirty_from`, allowing projections that have not rendered
    /// yet to update an appended suffix without scanning the unchanged prefix.
    record_changes: VecDeque<(u64, usize)>,
    patch_floor_revision: u64,
    patch_revision: u64,
    patch_retained_bytes: usize,
    patches: VecDeque<TranscriptPatch>,
}

impl BlockHistory {
    pub(crate) fn new() -> Self {
        Self {
            order: Vec::new(),
            order_indices: HashMap::new(),
            order_indices_valid: true,
            entries: HashMap::new(),
            content_store: ContentStore::default(),
            next_id: 0,
            tool_states: HashMap::new(),
            finished_blocks: Vec::new(),
            generation: 0,
            tool_render_revision_clock: 0,
            order_generation: 0,
            navigation_generation: 0,
            persisted_block_count: 0,
            hydrated_ids: HashSet::new(),
            hydrated_block_bytes: 0,
            hydrated_tool_state_bytes: 0,
            record_dirty_from: None,
            record_dirty_generation: 0,
            record_changes: VecDeque::new(),
            patch_floor_revision: 0,
            patch_revision: 0,
            patch_retained_bytes: 0,
            patches: VecDeque::new(),
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn order_generation(&self) -> u64 {
        self.order_generation
    }

    pub fn navigation_generation(&self) -> u64 {
        self.navigation_generation
    }

    pub fn patch_revision(&self) -> u64 {
        self.patch_revision
    }

    pub fn patches_since(&self, revision: u64) -> Option<impl Iterator<Item = &TranscriptPatch>> {
        if revision > self.patch_revision || revision < self.patch_floor_revision {
            return None;
        }
        Some(
            self.patches
                .iter()
                .skip_while(move |patch| patch.revision <= revision),
        )
    }

    fn record_patch(
        &mut self,
        operations: Vec<TranscriptPatchOperation>,
        navigation_changed: bool,
        search_changed: bool,
        persistable_changed: bool,
    ) {
        self.patch_revision = self
            .patch_revision
            .checked_add(1)
            .expect("transcript patch revision overflow");
        let patch = TranscriptPatch {
            revision: self.patch_revision,
            operations,
            navigation_changed,
            search_changed,
            persistable_changed,
        };
        self.patch_retained_bytes = self
            .patch_retained_bytes
            .saturating_add(patch.retained_bytes());
        self.patches.push_back(patch);
        if self.patch_retained_bytes > TRANSCRIPT_PATCH_LOG_BUDGET_BYTES {
            self.compact_patches();
        }
    }

    fn install_patches(&mut self, patches: VecDeque<TranscriptPatch>) {
        self.patch_retained_bytes = patches.iter().map(TranscriptPatch::retained_bytes).sum();
        self.patches = patches;
    }

    fn compact_patches(&mut self) {
        let Some(last_structural_index) = self.patches.iter().rposition(|patch| {
            patch
                .operations
                .iter()
                .any(TranscriptPatchOperation::is_structural)
        }) else {
            let compacted = TranscriptPatch::coalesce(self.patch_revision, self.patches.iter());
            self.install_patches(VecDeque::from([compacted]));
            return;
        };

        let structural_revision = self.patches[last_structural_index].revision;
        let mut compacted = VecDeque::new();
        compacted.push_back(TranscriptPatch::coalesce(
            structural_revision,
            self.patches.iter().take(last_structural_index + 1),
        ));
        if last_structural_index + 1 < self.patches.len() {
            compacted.push_back(TranscriptPatch::coalesce(
                self.patch_revision,
                self.patches.iter().skip(last_structural_index + 1),
            ));
        }
        self.patch_floor_revision = structural_revision.saturating_sub(1);
        self.install_patches(compacted);
    }

    fn rebuild_order_indices(&mut self) {
        self.order_indices.clear();
        self.order_indices.extend(
            self.order
                .iter()
                .enumerate()
                .map(|(index, id)| (*id, index)),
        );
        self.order_indices_valid = true;
        debug_assert_eq!(self.order.len(), self.order_indices.len());
    }

    fn ensure_order_indices(&mut self) {
        if !self.order_indices_valid {
            self.rebuild_order_indices();
        }
    }

    fn order_index(&mut self, id: BlockId) -> Option<usize> {
        self.ensure_order_indices();
        self.order_indices.get(&id).copied()
    }

    fn insert_order_id(&mut self, index: usize, id: BlockId) {
        let was_append = index == self.order.len();
        self.order.insert(index, id);
        if self.order_indices_valid && was_append {
            self.order_indices.insert(id, index);
            debug_assert_eq!(self.order.len(), self.order_indices.len());
        } else {
            self.order_indices_valid = false;
        }
    }

    fn remove_order_index(&mut self, index: usize) -> BlockId {
        let was_tail = index + 1 == self.order.len();
        let id = self.order.remove(index);
        if self.order_indices_valid && was_tail {
            self.order_indices.remove(&id);
            debug_assert_eq!(self.order.len(), self.order_indices.len());
        } else {
            self.order_indices_valid = false;
        }
        id
    }

    pub(crate) fn bump_generation(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    fn next_tool_render_revision(&mut self) -> u64 {
        self.tool_render_revision_clock = self
            .tool_render_revision_clock
            .checked_add(1)
            .expect("tool render revision overflow");
        self.tool_render_revision_clock
    }

    pub(crate) fn bump_order_generation(&mut self) {
        self.bump_generation();
        self.order_generation = self.order_generation.wrapping_add(1);
        self.bump_navigation_generation();
    }

    fn bump_navigation_generation(&mut self) {
        self.navigation_generation = self.navigation_generation.wrapping_add(1);
    }

    fn recount_persisted_blocks(&mut self) {
        self.persisted_block_count = self
            .order
            .iter()
            .filter(|id| {
                self.entries
                    .get(id)
                    .is_some_and(BlockEntry::is_persisted_block)
            })
            .count();
    }

    fn recount_hydrated_entries(&mut self) {
        self.hydrated_ids.clear();
        self.hydrated_block_bytes = 0;
        self.hydrated_tool_state_bytes = 0;
        for id in &self.order {
            let Some(entry @ BlockEntry::Hydrated { .. }) = self.entries.get(id) else {
                continue;
            };
            self.hydrated_ids.insert(*id);
            self.hydrated_block_bytes = self
                .hydrated_block_bytes
                .saturating_add(entry.hydrated_block_weight());
            self.hydrated_tool_state_bytes = self
                .hydrated_tool_state_bytes
                .saturating_add(entry.hydrated_tool_state_weight());
        }
    }

    fn insert_entry(&mut self, id: BlockId, entry: BlockEntry) -> Option<BlockEntry> {
        let block_bytes = entry.hydrated_block_weight();
        let tool_state_bytes = entry.hydrated_tool_state_weight();
        let hydrated = matches!(&entry, BlockEntry::Hydrated { .. });
        let persisted = entry.is_persisted_block();
        let new_contents = entry
            .block()
            .map(Block::registered_contents)
            .unwrap_or_default()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        let previous = self.entries.insert(id, entry);
        let previous_content_ids = previous
            .as_ref()
            .and_then(BlockEntry::block)
            .map(Block::registered_contents)
            .unwrap_or_default()
            .into_iter()
            .map(TranscriptContent::id)
            .collect::<Vec<_>>();
        for content in &new_contents {
            self.content_store.register(content);
        }
        for content_id in previous_content_ids {
            self.content_store.remove(content_id);
        }
        let was_persisted = previous
            .as_ref()
            .is_some_and(BlockEntry::is_persisted_block);
        match (was_persisted, persisted) {
            (false, true) => {
                self.persisted_block_count = self.persisted_block_count.saturating_add(1);
            }
            (true, false) => {
                self.persisted_block_count = self.persisted_block_count.saturating_sub(1);
            }
            _ => {}
        }
        if let Some(previous) = previous.as_ref() {
            self.hydrated_block_bytes = self
                .hydrated_block_bytes
                .saturating_sub(previous.hydrated_block_weight());
            self.hydrated_tool_state_bytes = self
                .hydrated_tool_state_bytes
                .saturating_sub(previous.hydrated_tool_state_weight());
        }
        if hydrated {
            self.hydrated_ids.insert(id);
            self.hydrated_block_bytes = self.hydrated_block_bytes.saturating_add(block_bytes);
            self.hydrated_tool_state_bytes = self
                .hydrated_tool_state_bytes
                .saturating_add(tool_state_bytes);
        } else {
            self.hydrated_ids.remove(&id);
        }
        previous
    }

    fn remove_entry(&mut self, id: BlockId) -> Option<BlockEntry> {
        let entry = self.entries.remove(&id)?;
        if let Some(block) = entry.block() {
            for content in block.registered_contents() {
                self.content_store.remove(content.id());
            }
        }
        if entry.is_persisted_block() {
            self.persisted_block_count = self.persisted_block_count.saturating_sub(1);
        }
        self.hydrated_ids.remove(&id);
        self.hydrated_block_bytes = self
            .hydrated_block_bytes
            .saturating_sub(entry.hydrated_block_weight());
        self.hydrated_tool_state_bytes = self
            .hydrated_tool_state_bytes
            .saturating_sub(entry.hydrated_tool_state_weight());
        Some(entry)
    }

    /// Marks externally-mutated history as changed so snapshots and projections rebuild.
    pub fn mark_changed(&mut self) {
        self.rebuild_order_indices();
        self.recount_persisted_blocks();
        self.recount_hydrated_entries();
        self.rebuild_content_store();
        self.bump_order_generation();
        self.mark_record_dirty_from(0);
        self.record_patch(vec![TranscriptPatchOperation::Reset], true, true, true);
    }

    pub fn persisted_block_count(&self) -> usize {
        self.persisted_block_count
    }

    pub fn record_dirty_from(&self) -> Option<usize> {
        self.record_dirty_from
    }

    pub fn record_dirty_generation(&self) -> u64 {
        self.record_dirty_generation
    }

    pub fn record_changed_from_since(&self, generation: u64) -> Option<usize> {
        if generation == self.record_dirty_generation {
            return Some(self.order.len());
        }
        let first_generation = generation.checked_add(1)?;
        let first = self
            .record_changes
            .iter()
            .position(|(change_generation, _)| *change_generation == first_generation)?;
        let mut expected_generation = first_generation;
        let mut changed_from = self.order.len();
        for (change_generation, index) in self.record_changes.iter().skip(first) {
            if *change_generation != expected_generation {
                return None;
            }
            changed_from = changed_from.min(*index);
            if *change_generation == self.record_dirty_generation {
                return Some(changed_from);
            }
            expected_generation = expected_generation.checked_add(1)?;
        }
        None
    }

    pub fn clear_record_dirty(&mut self) {
        self.record_dirty_from = None;
    }

    pub fn require_record_resave_from(&mut self, idx: usize) {
        self.record_dirty_generation = self
            .record_dirty_generation
            .checked_add(1)
            .expect("transcript block generation overflow");
        self.record_changes
            .push_back((self.record_dirty_generation, idx));
        if self.record_changes.len() > RECORD_CHANGE_LOG_CAPACITY {
            self.record_changes.pop_front();
        }
        self.record_dirty_from = Some(
            self.record_dirty_from
                .map_or(idx, |current| current.min(idx)),
        );
    }

    fn mark_record_dirty_from(&mut self, idx: usize) {
        self.require_record_resave_from(idx);
    }

    fn mark_record_dirty_for_id(&mut self, id: BlockId) {
        if let Some(idx) = self.order_index(id) {
            self.mark_record_dirty_from(idx);
        }
    }

    pub fn drain_finished_blocks(&mut self) -> Vec<BlockId> {
        std::mem::take(&mut self.finished_blocks)
    }

    pub fn content_hash(&self, id: BlockId) -> u64 {
        match self.entries.get(&id) {
            Some(BlockEntry::Live(live)) => live.metadata.content_hash,
            Some(BlockEntry::Stored(stored) | BlockEntry::Hydrated { stored, .. }) => {
                stored.content_hash
            }
            None => 0,
        }
    }

    fn layout_content_hash(&self, id: BlockId) -> u64 {
        match self.entries.get(&id) {
            Some(BlockEntry::Live(live)) if live.metadata.status == Status::Streaming => {
                live.metadata.content_hash
                    ^ id.0.wrapping_mul(0x9e37_79b9_7f4a_7c15)
                    ^ live.metadata.live_revision.wrapping_add(1).rotate_left(17)
            }
            _ => self.content_hash(id),
        }
    }

    pub fn content(&self, id: BlockId, channel: ContentChannel) -> Option<&TranscriptContent> {
        self.block(id)
            .and_then(|block| block.content(channel))
            .or_else(|| self.tool_state(id).and_then(|state| state.content(channel)))
    }

    pub fn content_by_id(&self, id: ContentId) -> Option<&TranscriptContent> {
        self.content_store.get(id)
    }

    fn unregister_tool_state_content(&mut self, state: &ToolState) {
        for content in state.registered_contents() {
            self.content_store.remove(content.id());
        }
    }

    fn rebuild_content_store(&mut self) {
        let mut contents = self
            .entries
            .values()
            .filter_map(BlockEntry::block)
            .flat_map(Block::registered_contents)
            .cloned()
            .collect::<Vec<_>>();
        for state in self.tool_states.values().filter_map(ToolStateEntry::state) {
            contents.extend(state.registered_contents().cloned());
        }
        self.content_store.clear();
        for content in contents {
            self.content_store.register(&content);
        }
    }

    fn set_tool_state_entry(&mut self, id: BlockId, entry: ToolStateEntry) {
        self.tool_render_revision_clock =
            self.tool_render_revision_clock.max(entry.render_revision());
        let new_contents = entry
            .state()
            .map(|state| state.registered_contents().cloned().collect::<Vec<_>>());
        if let Some(previous) = self.tool_states.insert(id, entry) {
            if let Some(state) = previous.state() {
                self.unregister_tool_state_content(state);
            }
        }
        if let Some(contents) = new_contents {
            for content in contents {
                self.content_store.register(&content);
            }
        }
    }

    fn remove_tool_state_entry(&mut self, id: BlockId) -> Option<ToolStateEntry> {
        let entry = self.tool_states.remove(&id)?;
        if let Some(state) = entry.state() {
            self.unregister_tool_state_content(state);
        }
        Some(entry)
    }

    pub fn tool_state(&self, id: BlockId) -> Option<&ToolState> {
        self.tool_states.get(&id).and_then(ToolStateEntry::state)
    }

    pub fn tool_status(&self, id: BlockId) -> Option<ToolStatus> {
        self.tool_states.get(&id).map(ToolStateEntry::status)
    }

    pub fn tool_states(&self) -> impl Iterator<Item = (BlockId, &ToolState)> {
        self.tool_states
            .iter()
            .filter_map(|(id, state)| state.state().map(|state| (*id, state)))
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// Returns a full block only when it is live or explicitly hydrated.
    /// Stored entries never perform I/O or hydrate through this accessor.
    pub fn block(&self, id: BlockId) -> Option<&Block> {
        self.entries.get(&id).and_then(BlockEntry::block)
    }

    pub fn is_materialized(&self, id: BlockId) -> bool {
        self.block(id).is_some()
    }

    pub fn is_live(&self, id: BlockId) -> bool {
        matches!(self.entries.get(&id), Some(BlockEntry::Live(_)))
    }

    pub fn is_hydrated(&self, id: BlockId) -> bool {
        matches!(self.entries.get(&id), Some(BlockEntry::Hydrated { .. }))
    }

    pub fn stored_ref(&self, id: BlockId) -> Option<&Arc<StoredBlockRef>> {
        self.entries.get(&id).and_then(BlockEntry::stored)
    }

    pub fn group_child_metadata(&self, id: BlockId) -> Option<TranscriptGroupChildMetadata> {
        match self.entries.get(&id)? {
            BlockEntry::Stored(stored) => Some(stored.group_child_metadata(id)),
            entry => Some(materialized_group_child_metadata(
                id,
                entry.block()?,
                self.tool_state(id),
            )),
        }
    }

    pub fn reindex_stored_records_from(&mut self, order_start: usize, record_start: usize) {
        let mut record_index = record_start;
        for id in self.order.iter().skip(order_start.min(self.order.len())) {
            let Some(entry) = self.entries.get_mut(id) else {
                continue;
            };
            if !entry.is_persisted_block() {
                continue;
            }
            match entry {
                BlockEntry::Stored(stored) | BlockEntry::Hydrated { stored, .. } => {
                    Arc::make_mut(stored).record_index = record_index;
                }
                BlockEntry::Live(_) => {}
            }
            record_index = record_index.saturating_add(1);
        }
    }

    pub fn row_estimate_text(&self, id: BlockId) -> Option<BlockText<'_>> {
        self.entries
            .get(&id)
            .and_then(BlockEntry::row_estimate_text)
    }

    pub fn estimated_text_bytes(&self, id: BlockId) -> u64 {
        self.entries
            .get(&id)
            .map_or(0, BlockEntry::estimated_text_bytes)
    }

    pub fn raw_text(&self, id: BlockId) -> Option<String> {
        self.entries.get(&id).and_then(BlockEntry::raw_text)
    }

    pub fn cloned_block(&self, id: BlockId) -> Option<Block> {
        self.entries.get(&id).and_then(BlockEntry::cloned_block)
    }

    pub fn first_line(&self, id: BlockId) -> Option<String> {
        if let Some(stored) = self.stored_ref(id) {
            return Some(stored.first_line().to_string());
        }
        self.raw_text(id).map(|text| {
            text.lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or("")
                .to_string()
        })
    }

    pub fn is_stable_scroll_anchor(&self, id: BlockId) -> bool {
        self.stored_ref(id).map_or_else(
            || self.block(id).is_some_and(Block::is_stable_scroll_anchor),
            |stored| stored.stable_scroll_anchor,
        )
    }

    pub fn block_kind(&self, id: BlockId) -> Option<&'static str> {
        self.entries.get(&id).map(BlockEntry::kind)
    }

    pub fn tool_name(&self, id: BlockId) -> Option<&str> {
        self.entries.get(&id).and_then(BlockEntry::tool_name)
    }

    pub fn tool_call_id(&self, id: BlockId) -> Option<&str> {
        self.entries.get(&id).and_then(BlockEntry::tool_call_id)
    }

    pub fn is_tool_draft(&self, id: BlockId) -> bool {
        self.entries.get(&id).is_some_and(BlockEntry::is_tool_draft)
    }

    pub fn is_persisted_block(&self, id: BlockId) -> bool {
        self.entries
            .get(&id)
            .is_some_and(BlockEntry::is_persisted_block)
    }

    pub fn process_field(&self, id: BlockId, field: &str) -> Option<String> {
        self.entries
            .get(&id)
            .and_then(|entry| entry.process_field(field))
    }

    pub fn arg_field(&self, id: BlockId, arg: &str) -> Option<&serde_json::Value> {
        self.entries.get(&id).and_then(|entry| entry.arg_field(arg))
    }

    pub fn block_records(&self) -> Vec<TranscriptBlockRecord> {
        self.block_records_from(0)
    }

    pub fn block_records_with_ids(&self) -> Vec<TranscriptBlockRecordWithId> {
        self.block_records_with_ids_from(0)
    }

    pub fn record_index_for_order_index(&self, start: usize) -> usize {
        let start = start.min(self.order.len());
        let suffix_len = self.order.len().saturating_sub(start);
        let record_index = if start <= suffix_len {
            self.order
                .iter()
                .take(start)
                .filter(|id| {
                    self.entries
                        .get(id)
                        .is_some_and(BlockEntry::is_persisted_block)
                })
                .count()
        } else {
            let suffix_blocks = self
                .order
                .iter()
                .skip(start)
                .filter(|id| {
                    self.entries
                        .get(id)
                        .is_some_and(BlockEntry::is_persisted_block)
                })
                .count();
            self.persisted_block_count.saturating_sub(suffix_blocks)
        };
        smelt_perf::perf::record_value(
            "transcript:block_record_index:entries_scanned",
            start.min(suffix_len) as u64,
        );
        record_index
    }

    pub fn block_records_from(&self, start: usize) -> Vec<TranscriptBlockRecord> {
        self.block_records_with_ids_from(start)
            .into_iter()
            .map(|record| record.record)
            .collect()
    }

    pub fn block_records_with_ids_from(&self, start: usize) -> Vec<TranscriptBlockRecordWithId> {
        self.order
            .iter()
            .skip(start.min(self.order.len()))
            .filter_map(|id| self.block_record_with_id(*id))
            .collect()
    }

    fn block_record_with_id(&self, id: BlockId) -> Option<TranscriptBlockRecordWithId> {
        let entry = self.entries.get(&id)?;
        if !entry.is_persisted_block() {
            return None;
        }
        let block = entry.cloned_block()?;
        let content_hash = match entry {
            BlockEntry::Live(_) => block.content_hash(),
            BlockEntry::Stored(stored) | BlockEntry::Hydrated { stored, .. } => stored.content_hash,
        };
        let tool_state = self.tool_state(id).cloned();
        Some(TranscriptBlockRecordWithId {
            block_id: id,
            record: TranscriptBlockRecord {
                block,
                content_hash,
                origin: self.block_origin(id),
                tool_state,
                tool_render_revision: self.sidecar_hash(id),
            },
        })
    }

    pub fn stored_ref_for_materialized(
        &self,
        id: BlockId,
        record_index: usize,
    ) -> Option<Arc<StoredBlockRef>> {
        if let Some(stored) = self.stored_ref(id) {
            if stored.record_index == record_index {
                return Some(Arc::clone(stored));
            }
        }
        let record = self.block_record_with_id(id)?;
        let indexed =
            transcript_indexed_text(&record.record.block, record.record.tool_state.as_ref());
        let (_, stored) = StoredBlockRef::from_record(
            record_index,
            id,
            &record.record,
            indexed.estimated_text_bytes,
            preview(&indexed.indexed_text, 512),
        );
        Some(stored)
    }

    pub fn from_block_records(records: Vec<TranscriptBlockRecord>) -> Self {
        let records = records
            .into_iter()
            .enumerate()
            .map(|(index, record)| TranscriptBlockRecordWithId {
                block_id: BlockId::new(index as u64),
                record,
            })
            .collect();
        Self::from_block_records_with_ids(records)
    }

    pub fn from_block_records_with_ids(records: Vec<TranscriptBlockRecordWithId>) -> Self {
        let mut history = Self::new();
        let stored = records
            .into_iter()
            .enumerate()
            .map(|(record_index, record)| {
                let indexed = transcript_indexed_text(
                    &record.record.block,
                    record.record.tool_state.as_ref(),
                );
                StoredBlockRef::from_record(
                    record_index,
                    record.block_id,
                    &record.record,
                    indexed.estimated_text_bytes,
                    preview(&indexed.indexed_text, 512),
                )
            })
            .collect::<Vec<_>>();
        history.install_stored_projection(stored);
        history.clear_record_dirty();
        history
    }

    pub fn install_stored_projection(
        &mut self,
        records: impl IntoIterator<Item = (BlockId, Arc<StoredBlockRef>)>,
    ) {
        let records = records.into_iter().collect::<Vec<_>>();
        let projected_ids = records
            .iter()
            .map(|(id, _)| *id)
            .collect::<HashSet<BlockId>>();
        let preserved_live = self
            .order
            .iter()
            .copied()
            .filter(|id| {
                !projected_ids.contains(id)
                    && matches!(self.entries.get(id), Some(BlockEntry::Live(_)))
            })
            .collect::<Vec<_>>();
        let mut next_order = Vec::with_capacity(records.len().saturating_add(preserved_live.len()));
        for (id, stored) in records {
            self.next_id = self.next_id.max(id.0.saturating_add(1));
            self.tool_render_revision_clock = self
                .tool_render_revision_clock
                .max(stored.tool_render_revision);
            next_order.push(id);
            let state_entry = self.tool_states.entry(id);
            if let std::collections::hash_map::Entry::Vacant(state_entry) = state_entry {
                if let Some(status) = stored.tool_status {
                    state_entry.insert(ToolStateEntry::Stored {
                        status,
                        render_revision: stored.tool_render_revision,
                    });
                }
            }
            match self.entries.remove(&id) {
                Some(entry @ BlockEntry::Live(_)) => {
                    self.entries.insert(id, entry);
                }
                Some(BlockEntry::Hydrated {
                    stored: previous,
                    block,
                    block_weight,
                    tool_state_weight,
                }) if previous.record_index == stored.record_index => {
                    self.entries.insert(
                        id,
                        BlockEntry::Hydrated {
                            stored,
                            block,
                            block_weight,
                            tool_state_weight,
                        },
                    );
                }
                _ => {
                    self.entries.insert(id, BlockEntry::Stored(stored));
                }
            }
        }
        next_order.extend(preserved_live);
        let retained_ids = next_order.iter().copied().collect::<HashSet<_>>();
        self.entries.retain(|id, _| retained_ids.contains(id));
        if self.order != next_order {
            self.order = next_order;
            self.rebuild_order_indices();
            self.bump_order_generation();
            self.bump_navigation_generation();
            self.record_patch(vec![TranscriptPatchOperation::Reset], true, true, false);
        }
        self.recount_persisted_blocks();
        self.recount_hydrated_entries();
        self.gc_tool_states();
    }

    pub fn install_hydrated_record(
        &mut self,
        id: BlockId,
        stored: Arc<StoredBlockRef>,
        record: TranscriptBlockRecord,
    ) -> bool {
        if stored.content_hash != 0
            && record.content_hash != 0
            && stored.content_hash != record.content_hash
        {
            return false;
        }
        let TranscriptBlockRecord {
            block, tool_state, ..
        } = record;
        let block = block.normalize_content();
        if block.kind() != stored.kind.as_str() {
            return false;
        }
        let block_hash = block.content_hash();
        if stored.content_hash != 0 && stored.content_hash != block_hash {
            return false;
        }
        let tool_state_weight = tool_state.as_ref().map_or(0, tool_state_retained_bytes);
        let block_weight = block_retained_bytes(&block);
        let weight = block_weight.saturating_add(tool_state_weight);
        let had_entry = self.entries.contains_key(&id);
        debug_assert_eq!(had_entry, self.order_index(id).is_some());
        if let Some(state) = tool_state {
            if !matches!(self.tool_states.get(&id), Some(ToolStateEntry::Live { .. })) {
                self.set_tool_state_entry(
                    id,
                    ToolStateEntry::Hydrated {
                        state,
                        render_revision: stored.tool_render_revision,
                    },
                );
            }
        }
        if !had_entry {
            let index = self.order.len();
            self.insert_order_id(index, id);
            self.bump_order_generation();
            if stored.kind == StoredBlockKind::User {
                self.bump_navigation_generation();
            }
            self.record_patch(
                vec![TranscriptPatchOperation::Insert { id, index }],
                true,
                true,
                false,
            );
        }
        self.next_id = self.next_id.max(id.0.saturating_add(1));
        self.insert_entry(
            id,
            BlockEntry::Hydrated {
                stored,
                block: Box::new(block),
                block_weight,
                tool_state_weight,
            },
        );
        smelt_perf::perf::record_value("transcript:block_cache:hydrated_bytes", weight as u64);
        true
    }

    pub fn promote_hydrated(&mut self, id: BlockId) -> bool {
        if !matches!(self.entries.get(&id), Some(BlockEntry::Hydrated { .. })) {
            return self.is_live(id);
        }
        let Some(BlockEntry::Hydrated { block, stored, .. }) = self.remove_entry(id) else {
            unreachable!("entry was checked as hydrated");
        };
        let metadata = BlockMetadata {
            content_hash: stored.content_hash,
            navigation_open: append_navigation_open(&block),
            origin: stored.origin,
            ..BlockMetadata::default()
        };
        if let Some(ToolStateEntry::Hydrated {
            state,
            render_revision,
        }) = self.remove_tool_state_entry(id)
        {
            self.set_tool_state_entry(
                id,
                ToolStateEntry::Live {
                    state,
                    render_revision,
                },
            );
        }
        self.insert_entry(
            id,
            BlockEntry::Live(Box::new(LiveBlock {
                block: *block,
                metadata,
            })),
        );
        true
    }

    pub fn evict_hydrated(&mut self, id: BlockId) -> usize {
        if !matches!(self.entries.get(&id), Some(BlockEntry::Hydrated { .. })) {
            return 0;
        }
        let Some(BlockEntry::Hydrated {
            stored,
            block_weight,
            tool_state_weight,
            ..
        }) = self.remove_entry(id)
        else {
            unreachable!("entry was checked as hydrated");
        };
        let weight = block_weight.saturating_add(tool_state_weight);
        if matches!(
            self.tool_states.get(&id),
            Some(ToolStateEntry::Hydrated { .. })
        ) {
            if let Some(status) = stored.tool_status {
                self.set_tool_state_entry(
                    id,
                    ToolStateEntry::Stored {
                        status,
                        render_revision: stored.tool_render_revision,
                    },
                );
            } else {
                self.remove_tool_state_entry(id);
            }
        }
        self.insert_entry(id, BlockEntry::Stored(stored));
        smelt_perf::perf::record_value("transcript:block_cache:evicted_bytes", weight as u64);
        weight
    }

    pub fn dematerialize_live(&mut self, id: BlockId, stored: Arc<StoredBlockRef>) -> usize {
        if !matches!(self.entries.get(&id), Some(BlockEntry::Live(_))) {
            return 0;
        }
        let Some(BlockEntry::Live(live)) = self.remove_entry(id) else {
            unreachable!("entry was checked as live");
        };
        let mut weight = block_retained_bytes(&live.block);
        if let Some(ToolStateEntry::Live { state, .. }) = self.remove_tool_state_entry(id) {
            weight = weight.saturating_add(tool_state_retained_bytes(&state));
        }
        if let Some(status) = stored.tool_status {
            self.set_tool_state_entry(
                id,
                ToolStateEntry::Stored {
                    status,
                    render_revision: stored.tool_render_revision,
                },
            );
        }
        self.insert_entry(id, BlockEntry::Stored(stored));
        weight
    }

    pub fn hydrated_blocks(&self) -> impl Iterator<Item = (BlockId, usize)> + '_ {
        self.hydrated_ids.iter().filter_map(|id| {
            self.entries
                .get(id)
                .map(|entry| (*id, entry.hydrated_weight()))
        })
    }

    pub fn materialized_retained_bytes(&self, id: BlockId) -> usize {
        match self.entries.get(&id) {
            Some(BlockEntry::Live(live)) => block_retained_bytes(&live.block).saturating_add(
                self.tool_states
                    .get(&id)
                    .and_then(|entry| match entry {
                        ToolStateEntry::Live { state, .. } => {
                            Some(tool_state_retained_bytes(state))
                        }
                        _ => None,
                    })
                    .unwrap_or_default(),
            ),
            Some(entry @ BlockEntry::Hydrated { .. }) => entry.hydrated_weight(),
            _ => 0,
        }
    }

    pub fn live_block_retained_bytes(&self) -> usize {
        self.entries
            .values()
            .filter_map(|entry| match entry {
                BlockEntry::Live(live) => Some(block_retained_bytes(&live.block)),
                _ => None,
            })
            .sum()
    }

    pub fn live_tool_state_retained_bytes(&self) -> usize {
        self.tool_states
            .values()
            .filter_map(|entry| match entry {
                ToolStateEntry::Live { state, .. } => Some(tool_state_retained_bytes(state)),
                _ => None,
            })
            .sum()
    }

    pub fn hydrated_block_retained_bytes(&self) -> usize {
        self.hydrated_block_bytes
    }

    pub fn hydrated_tool_state_retained_bytes(&self) -> usize {
        self.hydrated_tool_state_bytes
    }

    pub fn tool_state_index_retained_bytes(&self) -> usize {
        self.tool_states
            .capacity()
            .saturating_mul(std::mem::size_of::<(BlockId, ToolStateEntry)>())
    }

    pub fn block_metadata_retained_bytes(&self) -> usize {
        self.live_block_count()
            .saturating_mul(std::mem::size_of::<BlockMetadata>())
    }

    pub fn live_retained_bytes(&self) -> usize {
        self.live_block_retained_bytes()
            .saturating_add(self.live_tool_state_retained_bytes())
    }

    pub fn hydrated_retained_bytes(&self) -> usize {
        self.hydrated_block_bytes
            .saturating_add(self.hydrated_tool_state_bytes)
    }

    pub fn live_block_count(&self) -> usize {
        self.entries
            .values()
            .filter(|entry| matches!(entry, BlockEntry::Live(_)))
            .count()
    }

    pub fn stored_block_count(&self) -> usize {
        self.entries
            .values()
            .filter(|entry| matches!(entry, BlockEntry::Stored(_)))
            .count()
    }

    pub fn hydrated_block_count(&self) -> usize {
        self.hydrated_ids.len()
    }

    pub fn block_id_at(&self, i: usize) -> Option<BlockId> {
        let id = self.order.get(i).copied()?;
        assert!(
            self.entries.contains_key(&id),
            "block id in transcript order"
        );
        Some(id)
    }

    pub fn materialized_block_at(&self, i: usize) -> Option<&Block> {
        self.block(self.block_id_at(i)?)
    }

    pub fn last_block_id(&self) -> Option<BlockId> {
        let id = self.order.last().copied()?;
        assert!(
            self.entries.contains_key(&id),
            "block id in transcript order"
        );
        Some(id)
    }

    pub fn has_history_origin_at_or_after(&self, before_history_index: usize) -> bool {
        self.first_block_index_for_history_origin_at_or_after(before_history_index)
            .is_some()
    }

    pub fn block_origin(&self, id: BlockId) -> Option<BlockOrigin> {
        match self.entries.get(&id) {
            Some(BlockEntry::Live(live)) => live.metadata.origin,
            Some(BlockEntry::Stored(stored) | BlockEntry::Hydrated { stored, .. }) => stored.origin,
            None => None,
        }
    }

    pub fn block_origin_at(&self, i: usize) -> Option<BlockOrigin> {
        self.order.get(i).and_then(|id| self.block_origin(*id))
    }

    pub fn first_block_index_for_history_origin_at_or_after(
        &self,
        before_history_index: usize,
    ) -> Option<usize> {
        self.order.iter().position(|id| {
            matches!(
                self.block_origin(*id),
                Some(BlockOrigin::History(history_index)) if history_index >= before_history_index
            )
        })
    }

    pub fn status(&self, id: BlockId) -> Option<Status> {
        self.entries.get(&id).map(|entry| match entry {
            BlockEntry::Live(live) => live.metadata.status,
            BlockEntry::Stored(_) | BlockEntry::Hydrated { .. } => Status::Done,
        })
    }

    /// Status changes do not invalidate the layout cache (style concern only).
    pub(crate) fn set_status(&mut self, id: BlockId, status: Status) {
        let Some(BlockEntry::Live(live)) = self.entries.get_mut(&id) else {
            panic!("status can only change on a live transcript block: {id:?}");
        };
        let was_streaming = matches!(live.metadata.status, Status::Streaming);
        let committed = matches!(status, Status::Done) && was_streaming;
        live.metadata.status = status;
        if committed && live.metadata.live_revision != 0 {
            live.metadata.content_hash = live.block.content_hash();
            live.metadata.live_revision = 0;
        }
        if committed {
            self.finished_blocks.push(id);
        }
        self.bump_generation();
        let mut operations = vec![TranscriptPatchOperation::SetStatus { id }];
        if committed {
            operations.push(TranscriptPatchOperation::Commit { id });
        }
        self.record_patch(operations, false, false, committed);
    }

    fn add_block(
        &mut self,
        idx: Option<usize>,
        block: Block,
        origin: Option<BlockOrigin>,
    ) -> BlockId {
        let block = block.normalize_content();
        let hash = block.content_hash();
        let navigation_open = append_navigation_open(&block);
        let id = BlockId(self.next_id);
        self.next_id += 1;
        let order_index = idx.map_or(self.order.len(), |idx| idx.min(self.order.len()));
        let entry = BlockEntry::Live(Box::new(LiveBlock {
            block,
            metadata: BlockMetadata {
                content_hash: hash,
                navigation_open,
                origin,
                ..BlockMetadata::default()
            },
        }));
        self.insert_order_id(order_index, id);
        self.insert_entry(id, entry);
        self.bump_order_generation();
        self.mark_record_dirty_from(order_index);
        self.record_patch(
            vec![TranscriptPatchOperation::Insert {
                id,
                index: order_index,
            }],
            true,
            true,
            true,
        );
        id
    }

    fn add_hydrated_block(
        &mut self,
        idx: Option<usize>,
        block: Block,
        origin: Option<BlockOrigin>,
        content_hash: Option<u64>,
    ) -> BlockId {
        let id = BlockId(self.next_id);
        self.next_id += 1;
        self.add_hydrated_block_with_id(id, idx, block, origin, content_hash)
    }

    fn add_hydrated_block_with_id(
        &mut self,
        id: BlockId,
        idx: Option<usize>,
        block: Block,
        origin: Option<BlockOrigin>,
        content_hash: Option<u64>,
    ) -> BlockId {
        let normalized = block.normalize_content();
        let normalized_hash = normalized.content_hash();
        let hash = content_hash
            .filter(|hash| *hash == normalized_hash)
            .unwrap_or(normalized_hash);
        let block = normalized;
        self.next_id = self.next_id.max(id.0.saturating_add(1));
        let order_index = idx.map_or(self.order.len(), |idx| idx.min(self.order.len()));
        let tool_state = self.tool_state(id).cloned();
        let record = TranscriptBlockRecord {
            block,
            content_hash: hash,
            origin,
            tool_state,
            tool_render_revision: self.sidecar_hash(id),
        };
        let indexed = transcript_indexed_text(&record.block, record.tool_state.as_ref());
        let (_, stored) = StoredBlockRef::from_record(
            order_index,
            id,
            &record,
            indexed.estimated_text_bytes,
            preview(&indexed.indexed_text, 512),
        );
        let block_weight = block_retained_bytes(&record.block);
        let tool_state_weight = record
            .tool_state
            .as_ref()
            .map_or(0, tool_state_retained_bytes);
        let entry = BlockEntry::Hydrated {
            stored,
            block: Box::new(record.block),
            block_weight,
            tool_state_weight,
        };
        self.insert_order_id(order_index, id);
        self.insert_entry(id, entry);
        self.bump_order_generation();
        self.mark_record_dirty_from(order_index);
        self.record_patch(
            vec![TranscriptPatchOperation::Insert {
                id,
                index: order_index,
            }],
            true,
            true,
            true,
        );
        id
    }

    pub(crate) fn push(&mut self, block: Block) -> BlockId {
        self.add_block(None, block, None)
    }

    pub(crate) fn push_with_origin(&mut self, block: Block, origin: BlockOrigin) -> BlockId {
        self.add_block(None, block, Some(origin))
    }

    pub fn push_hydrated_block_with_origin(
        &mut self,
        block: Block,
        origin: BlockOrigin,
    ) -> BlockId {
        self.add_hydrated_block(None, block, Some(origin), None)
    }

    pub(crate) fn insert_checkpoint_marker(
        &mut self,
        before_history_index: usize,
        block: Block,
    ) -> BlockId {
        let idx = self
            .order
            .iter()
            .position(|id| {
                matches!(
                    self.block_origin(*id),
                    Some(BlockOrigin::History(history_index)) if history_index >= before_history_index
                )
            })
            .unwrap_or(self.order.len());
        self.add_block(
            Some(idx),
            block,
            Some(BlockOrigin::Checkpoint {
                history_index: before_history_index,
            }),
        )
    }

    pub(crate) fn insert_checkpoint_marker_at(
        &mut self,
        block_index: usize,
        history_index: usize,
        block: Block,
    ) -> BlockId {
        self.add_block(
            Some(block_index),
            block,
            Some(BlockOrigin::Checkpoint { history_index }),
        )
    }

    pub(crate) fn remove_unoriginated_at(&mut self, idx: usize) -> Option<Block> {
        let id = *self.order.get(idx)?;
        if self.block_origin(id).is_some() || !self.is_materialized(id) {
            return None;
        }
        self.remove_order_index(idx);
        self.remove_tool_state_entry(id);
        let block = self
            .remove_entry(id)
            .and_then(BlockEntry::into_materialized);
        self.bump_order_generation();
        self.mark_record_dirty_from(idx);
        self.record_patch(
            vec![TranscriptPatchOperation::Remove { id, index: idx }],
            true,
            true,
            true,
        );
        block
    }

    pub(crate) fn push_with_state(&mut self, block: Block, state: ToolState) -> BlockId {
        let id = self.push(block);
        let render_revision = self.next_tool_render_revision();
        self.set_tool_state_entry(
            id,
            ToolStateEntry::Live {
                state,
                render_revision,
            },
        );
        id
    }

    pub(crate) fn push_with_state_and_origin(
        &mut self,
        block: Block,
        state: ToolState,
        origin: BlockOrigin,
    ) -> BlockId {
        let id = self.push_with_origin(block, origin);
        let render_revision = self.next_tool_render_revision();
        self.set_tool_state_entry(
            id,
            ToolStateEntry::Live {
                state,
                render_revision,
            },
        );
        id
    }

    pub fn push_hydrated_block_with_state_and_origin(
        &mut self,
        block: Block,
        state: ToolState,
        origin: BlockOrigin,
    ) -> BlockId {
        let id = BlockId(self.next_id);
        let render_revision = self.next_tool_render_revision();
        self.set_tool_state_entry(
            id,
            ToolStateEntry::Hydrated {
                state,
                render_revision,
            },
        );
        self.add_hydrated_block_with_id(id, None, block, Some(origin), None)
    }

    pub fn apply_tool_state_mutation(&mut self, id: BlockId, mutation: ToolStateMutation) -> bool {
        let dirty_idx = self.order_index(id);
        if dirty_idx.is_some() {
            self.promote_hydrated(id);
        }
        if !matches!(self.tool_states.get(&id), Some(ToolStateEntry::Live { .. })) {
            return false;
        }

        if mutation.is_animation_only() {
            let Some(ToolStateEntry::Live { state, .. }) = self.tool_states.get_mut(&id) else {
                unreachable!("tool state was checked as live");
            };
            mutation.apply(state);
            self.bump_generation();
            self.record_patch(
                vec![TranscriptPatchOperation::SetAnimationState { id }],
                false,
                false,
                false,
            );
            return true;
        }

        let search_changed = mutation.search_changed();
        let next_render_revision = self.next_tool_render_revision();
        let (old_content_ids, new_contents) = {
            let Some(ToolStateEntry::Live {
                state,
                render_revision,
            }) = self.tool_states.get_mut(&id)
            else {
                unreachable!("tool state was checked as live");
            };
            let old_content_ids = state
                .registered_contents()
                .map(TranscriptContent::id)
                .collect::<Vec<_>>();
            mutation.apply(state);
            *render_revision = next_render_revision;
            let new_contents = state.registered_contents().cloned().collect::<Vec<_>>();
            (old_content_ids, new_contents)
        };
        for content_id in old_content_ids {
            self.content_store.remove(content_id);
        }
        for content in new_contents {
            self.content_store.register(&content);
        }
        self.bump_generation();
        if let Some(idx) = dirty_idx {
            self.mark_record_dirty_from(idx);
        }
        self.record_patch(
            vec![TranscriptPatchOperation::SetSideState { id }],
            false,
            search_changed,
            true,
        );
        true
    }

    pub(crate) fn append_tool_draft(
        &mut self,
        id: BlockId,
        call_id: Option<String>,
        name: Option<String>,
        delta: String,
    ) -> Option<ToolDraftAppend> {
        let (mut append, identity_changed) = {
            let Some(BlockEntry::Live(live)) = self.entries.get_mut(&id) else {
                return None;
            };
            let Block::ToolDraft(draft) = &mut live.block else {
                return None;
            };
            let identity_changed = draft.update_identity(call_id, name);
            let append = draft.append(delta);
            live.metadata.live_revision = live
                .metadata
                .live_revision
                .checked_add(1)
                .expect("live transcript revision overflow");
            (append, identity_changed)
        };
        append.presentation_changed |= identity_changed;
        for content in &append.new_fields {
            self.content_store.register(content);
        }
        for content_id in &append.removed_field_ids {
            self.content_store.remove(*content_id);
        }
        let mut operations = Vec::with_capacity(append.field_appends.len().saturating_add(2));
        if !append.raw_range.is_empty() {
            operations.push(TranscriptPatchOperation::Append {
                id,
                content_id: append.raw_content_id,
                channel: ContentChannel::DraftArguments,
                byte_range: append.raw_range.clone(),
            });
        }
        operations.extend(append.field_appends.iter().map(|field| {
            TranscriptPatchOperation::Append {
                id,
                content_id: field.content.id(),
                channel: ContentChannel::DraftField,
                byte_range: field.byte_range.clone(),
            }
        }));
        if append.presentation_changed {
            operations.push(TranscriptPatchOperation::Replace { id });
        }
        if operations.is_empty() {
            return Some(append);
        }
        self.bump_generation();
        self.record_patch(operations, false, true, false);
        Some(append)
    }

    pub(crate) fn finish_tool_draft(
        &mut self,
        id: BlockId,
        call_id: String,
        name: String,
        arguments: String,
    ) -> Option<ToolDraftAppend> {
        let (mut append, identity_changed) = {
            let Some(BlockEntry::Live(live)) = self.entries.get_mut(&id) else {
                return None;
            };
            let Block::ToolDraft(draft) = &mut live.block else {
                return None;
            };
            let identity_changed = draft.update_identity(Some(call_id), Some(name));
            let append = draft.finish(arguments);
            live.metadata.live_revision = live
                .metadata
                .live_revision
                .checked_add(1)
                .expect("live transcript revision overflow");
            (append, identity_changed)
        };
        append.presentation_changed |= identity_changed;
        for content in &append.new_fields {
            self.content_store.register(content);
        }
        for content_id in &append.removed_field_ids {
            self.content_store.remove(*content_id);
        }
        let mut operations = Vec::with_capacity(append.field_appends.len().saturating_add(2));
        if !append.raw_range.is_empty() {
            operations.push(TranscriptPatchOperation::Append {
                id,
                content_id: append.raw_content_id,
                channel: ContentChannel::DraftArguments,
                byte_range: append.raw_range.clone(),
            });
        }
        operations.extend(append.field_appends.iter().map(|field| {
            TranscriptPatchOperation::Append {
                id,
                content_id: field.content.id(),
                channel: ContentChannel::DraftField,
                byte_range: field.byte_range.clone(),
            }
        }));
        operations.push(TranscriptPatchOperation::Replace { id });
        self.bump_generation();
        self.record_patch(operations, false, true, false);
        Some(append)
    }

    pub(crate) fn set_tool_draft_summary(
        &mut self,
        id: BlockId,
        summary: protocol::StyledLines,
    ) -> bool {
        let changed = {
            let Some(BlockEntry::Live(live)) = self.entries.get_mut(&id) else {
                return false;
            };
            let Block::ToolDraft(draft) = &mut live.block else {
                return false;
            };
            if draft.summary == summary {
                return false;
            }
            draft.summary = summary;
            true
        };
        if changed {
            self.bump_generation();
            self.record_patch(
                vec![TranscriptPatchOperation::Replace { id }],
                false,
                true,
                false,
            );
        }
        changed
    }

    pub(crate) fn append_live_output_line(
        &mut self,
        id: BlockId,
        channel: ContentChannel,
        line: String,
    ) -> Option<std::ops::Range<usize>> {
        self.append_live_output_slice(
            id,
            channel,
            crate::transcript_content::SharedContentSlice::from_owned(line),
            true,
        )
    }

    pub(crate) fn append_live_output_slice(
        &mut self,
        id: BlockId,
        channel: ContentChannel,
        chunk: crate::transcript_content::SharedContentSlice,
        line_start: bool,
    ) -> Option<std::ops::Range<usize>> {
        if chunk.is_empty() {
            return None;
        }

        let dirty_idx = self.order_index(id);
        if channel == ContentChannel::ToolOutput && dirty_idx.is_some() {
            self.promote_hydrated(id);
        }

        let mut attached = false;
        let content = match channel {
            ContentChannel::ToolOutput => {
                let next_render_revision = self.next_tool_render_revision();
                let Some(ToolStateEntry::Live {
                    state,
                    render_revision,
                }) = self.tool_states.get_mut(&id)
                else {
                    return None;
                };
                attached = state.output.is_none();
                let output = state.output.get_or_insert_with(|| {
                    Box::new(ToolOutput {
                        content: TranscriptContent::new(),
                        is_error: false,
                        metadata: None,
                        content_fields: Vec::new(),
                    })
                });
                if attached {
                    *render_revision = next_render_revision;
                }
                output.content.clone()
            }
            ContentChannel::ExecOutput => {
                let Some(BlockEntry::Live(live)) = self.entries.get_mut(&id) else {
                    return None;
                };
                let Block::Exec { output, .. } = &live.block else {
                    return None;
                };
                output.clone()
            }
            _ => return None,
        };

        let start = content.len();
        let needs_separator = line_start
            && start != 0
            && match channel {
                ContentChannel::ToolOutput => content.joining_grapheme_prefix_len(&chunk) == 0,
                ContentChannel::ExecOutput => !content.ends_with('\n'),
                _ => unreachable!("output line channels were validated"),
            };
        if needs_separator {
            content.append_owned("\n".to_owned());
        }
        content.append_shared(chunk);
        let byte_range = start..content.len();

        if channel == ContentChannel::ExecOutput {
            let Some(BlockEntry::Live(live)) = self.entries.get_mut(&id) else {
                unreachable!("exec output target disappeared during append");
            };
            live.metadata.live_revision = live
                .metadata
                .live_revision
                .checked_add(1)
                .expect("live transcript revision overflow");
        }
        if attached {
            self.content_store.register(&content);
        }
        self.bump_generation();
        if let Some(idx) = dirty_idx {
            self.mark_record_dirty_from(idx);
        }
        self.record_patch(
            vec![TranscriptPatchOperation::Append {
                id,
                content_id: content.id(),
                channel,
                byte_range: byte_range.clone(),
            }],
            false,
            true,
            true,
        );
        Some(byte_range)
    }

    pub(crate) fn append_live_text_segments<'a>(
        &mut self,
        id: BlockId,
        segments: impl IntoIterator<Item = &'a str>,
    ) -> Option<std::ops::Range<usize>> {
        let (content_id, start, end, navigation_changed) = {
            let Some(BlockEntry::Live(live)) = self.entries.get_mut(&id) else {
                return None;
            };
            let content = match &mut live.block {
                Block::Text { content } | Block::Thinking { content, .. } => content,
                _ => return None,
            };
            let start = content.len();
            let navigation_changed = live.metadata.navigation_open;
            for segment in segments {
                if live.metadata.navigation_open && segment.contains('\n') {
                    live.metadata.navigation_open = false;
                }
                content.push_str(segment);
            }
            let end = content.len();
            if start == end {
                return None;
            }
            live.metadata.live_revision = live
                .metadata
                .live_revision
                .checked_add(1)
                .expect("live transcript revision overflow");
            (content.id(), start, end, navigation_changed)
        };

        self.bump_generation();
        if navigation_changed {
            self.bump_navigation_generation();
        }
        self.mark_record_dirty_for_id(id);
        let byte_range = start..end;
        self.record_patch(
            vec![TranscriptPatchOperation::Append {
                id,
                content_id,
                channel: ContentChannel::Primary,
                byte_range: byte_range.clone(),
            }],
            navigation_changed,
            true,
            true,
        );
        Some(byte_range)
    }

    pub(crate) fn trim_live_exec_output(&mut self, id: BlockId) -> bool {
        let changed = {
            let Some(BlockEntry::Live(live)) = self.entries.get_mut(&id) else {
                return false;
            };
            let Block::Exec { output, .. } = &mut live.block else {
                return false;
            };
            let trimmed_len = output.trimmed_end_len();
            if trimmed_len == output.len() {
                return false;
            }
            output.truncate(trimmed_len);
            live.metadata.live_revision = live
                .metadata
                .live_revision
                .checked_add(1)
                .expect("live transcript revision overflow");
            true
        };

        if changed {
            self.bump_generation();
            self.mark_record_dirty_for_id(id);
            self.record_patch(
                vec![TranscriptPatchOperation::Replace { id }],
                false,
                true,
                true,
            );
        }
        changed
    }

    /// Replace block content in place. Preserves `BlockId`, `Status`, and
    /// `ViewState`. No-ops when the block doesn't exist (e.g. truncated during
    /// a stream). Same content hash skips the generation bump.
    pub fn rewrite(&mut self, id: BlockId, block: Block) {
        let Some(previous_navigation) = self.entries.get(&id).map(BlockEntry::navigation_signature)
        else {
            return;
        };
        let origin = self.block_origin(id);
        let status = self
            .status(id)
            .expect("rewrite target disappeared after existence check");
        let previous_hash = self.content_hash(id);
        self.promote_hydrated(id);
        let block = block.normalize_content();
        let navigation = (
            block.kind(),
            block
                .row_estimate_text()
                .map(BlockText::first_source_line)
                .unwrap_or_default(),
        );
        let hash = block.content_hash();
        let navigation_open = append_navigation_open(&block);
        let entry = BlockEntry::Live(Box::new(LiveBlock {
            block,
            metadata: BlockMetadata {
                content_hash: hash,
                live_revision: 0,
                navigation_open,
                status,
                origin,
            },
        }));
        self.insert_entry(id, entry);
        if previous_hash == hash {
            return;
        }
        self.bump_generation();
        let navigation_changed = navigation != previous_navigation;
        if navigation_changed {
            self.bump_navigation_generation();
        }
        self.mark_record_dirty_for_id(id);
        self.record_patch(
            vec![TranscriptPatchOperation::Replace { id }],
            navigation_changed,
            true,
            true,
        );
    }

    pub(crate) fn rewrite_with_tool_state(&mut self, id: BlockId, block: Block, state: ToolState) {
        self.rewrite(id, block);
        let render_revision = self
            .generation
            .checked_add(1)
            .expect("tool render revision overflow");
        self.set_tool_state_entry(
            id,
            ToolStateEntry::Live {
                state,
                render_revision,
            },
        );
        self.bump_generation();
        self.mark_record_dirty_for_id(id);
        self.record_patch(
            vec![TranscriptPatchOperation::SetSideState { id }],
            false,
            true,
            true,
        );
    }

    pub(crate) fn remove_block(&mut self, id: BlockId) {
        let Some(index) = self.order_index(id) else {
            return;
        };
        self.remove_order_index(index);
        self.remove_entry(id);
        self.remove_tool_state_entry(id);
        self.bump_order_generation();
        self.mark_record_dirty_from(index);
        self.record_patch(
            vec![TranscriptPatchOperation::Remove { id, index }],
            true,
            true,
            true,
        );
        self.gc_tool_states();
    }

    pub fn clear(&mut self) {
        if self.order.is_empty() {
            self.order_indices.clear();
            self.order_indices_valid = true;
            self.entries.clear();
            self.content_store.clear();
            self.persisted_block_count = 0;
            self.hydrated_ids.clear();
            self.hydrated_block_bytes = 0;
            self.hydrated_tool_state_bytes = 0;
            self.next_id = 0;
            self.tool_states.clear();
            self.tool_render_revision_clock = 0;
            return;
        }
        self.order.clear();
        self.order_indices.clear();
        self.order_indices_valid = true;
        self.entries.clear();
        self.content_store.clear();
        self.persisted_block_count = 0;
        self.hydrated_ids.clear();
        self.hydrated_block_bytes = 0;
        self.hydrated_tool_state_bytes = 0;
        self.next_id = 0;
        self.tool_states.clear();
        self.tool_render_revision_clock = 0;
        self.bump_order_generation();
        self.mark_record_dirty_from(0);
        self.record_patch(vec![TranscriptPatchOperation::Reset], true, true, true);
    }

    pub fn block_gap(&self, i: usize) -> u16 {
        if i == 0 {
            return 0;
        }
        let Some(above) = self.order.get(i - 1).and_then(|id| self.entries.get(id)) else {
            return 0;
        };
        let Some(below) = self.order.get(i).and_then(|id| self.entries.get(id)) else {
            return 0;
        };
        gap_between_entries(above, below)
    }

    pub fn rendered_block_gap(&self, i: usize, rendered_rows: usize) -> u16 {
        if rendered_rows == 0 {
            0
        } else {
            self.block_gap(i)
        }
    }

    pub fn sidecar_hash(&self, id: BlockId) -> u64 {
        self.tool_states
            .get(&id)
            .map_or(0, ToolStateEntry::render_revision)
    }

    /// Substitute the actual per-block content and sidecar hash into a base
    /// `LayoutKey` so cache lookups and layout passes agree.
    pub fn resolve_key(&self, id: BlockId, base: LayoutKey) -> LayoutKey {
        LayoutKey {
            content_hash: self.layout_content_hash(id),
            sidecar_hash: self.sidecar_hash(id),
            ..base
        }
    }

    pub(crate) fn truncate(&mut self, idx: usize) {
        if idx >= self.order.len() {
            return;
        }
        let removed: Vec<BlockId> = self.order.drain(idx..).collect();
        let operations = removed
            .iter()
            .copied()
            .map(|id| TranscriptPatchOperation::Remove { id, index: idx })
            .collect();
        for id in removed {
            self.order_indices.remove(&id);
            self.remove_entry(id);
        }
        self.bump_order_generation();
        self.mark_record_dirty_from(idx);
        self.record_patch(operations, true, true, true);
        self.gc_tool_states();
    }

    pub(crate) fn gc_tool_states(&mut self) {
        let live: HashSet<BlockId> = self
            .order
            .iter()
            .copied()
            .filter(|id| self.tool_call_id(*id).is_some())
            .collect();
        self.tool_states.retain(|id, _| live.contains(id));
        self.rebuild_content_store();
    }
}

/// Blank row gap before `below` given the preceding block. Most block
/// transitions are separated by one blank row. Adjacent code lines collapse,
/// and markdown headings sit directly on top of their following content.
pub fn gap_between(above: &Block, below: &Block) -> u16 {
    gap_between_parts(
        above.kind(),
        below.kind(),
        starts_with_thinking_title(below),
        ends_with_heading(above),
    )
}

fn gap_between_entries(above: &BlockEntry, below: &BlockEntry) -> u16 {
    gap_between_parts(
        above.kind(),
        below.kind(),
        entry_starts_with_thinking_title(below),
        entry_ends_with_heading(above),
    )
}

fn gap_between_parts(
    above_kind: &str,
    below_kind: &str,
    below_starts_thinking_title: bool,
    above_ends_heading: bool,
) -> u16 {
    if above_kind == "code" && below_kind == "code" {
        return 0;
    }
    if above_kind == "thinking" && below_kind == "thinking" {
        return if below_starts_thinking_title { 1 } else { 0 };
    }
    if matches!(below_kind, "assistant" | "code") && above_ends_heading {
        return 0;
    }
    1
}

fn entry_starts_with_thinking_title(entry: &BlockEntry) -> bool {
    if let Some(stored) = entry.stored() {
        return stored.starts_with_thinking_title;
    }
    match entry.block() {
        Some(Block::Thinking { title, content, .. }) => {
            has_thinking_title(title.as_deref(), content)
        }
        _ => false,
    }
}

fn entry_ends_with_heading(entry: &BlockEntry) -> bool {
    if let Some(stored) = entry.stored() {
        return stored.ends_with_heading;
    }
    match entry.block() {
        Some(Block::Text { content }) => content.ends_with_markdown_heading(),
        _ => false,
    }
}

fn starts_with_thinking_title(block: &Block) -> bool {
    match block {
        Block::Thinking { title, content, .. } => has_thinking_title(title.as_deref(), content),
        _ => false,
    }
}

fn has_thinking_title(title: Option<&str>, content: &TranscriptContent) -> bool {
    title.is_some()
        || crate::content::markdown_stream::thinking_title(&content.read().first_nonempty_line())
            .is_some()
}

fn ends_with_heading(block: &Block) -> bool {
    let Block::Text { content } = block else {
        return false;
    };
    content.ends_with_markdown_heading()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_marker_origin_round_trips_with_history_boundary() {
        let origin = BlockOrigin::Checkpoint { history_index: 7 };
        let encoded = serde_json::to_value(origin).unwrap();

        assert_eq!(
            serde_json::from_value::<BlockOrigin>(encoded).unwrap(),
            origin
        );
    }

    #[test]
    fn persisted_block_identity_excludes_transient_blocks() {
        let mut history = BlockHistory::new();
        let persisted = history.push(Block::Text {
            content: "persisted".into(),
        });
        let preview = history.push(Block::CompactionPreview {
            summary: "preview".into(),
        });
        let tool_draft = history.push(Block::ToolDraft(ToolDraft::new(
            "stream".into(),
            None,
            "read_file".into(),
        )));

        assert!(history.is_persisted_block(persisted));
        assert!(!history.is_persisted_block(preview));
        assert!(!history.is_persisted_block(tool_draft));
        assert!(!history.is_persisted_block(BlockId::new(u64::MAX)));
    }

    fn indexed_tool_state_with_content(content: &str, metadata: serde_json::Value) -> ToolState {
        ToolState {
            status: ToolStatus::Ok,
            elapsed: None,
            called_at_ms: None,
            elapsed_active: false,
            output: Some(Box::new(ToolOutput {
                content: content.to_string().into(),
                is_error: false,
                metadata: Some(metadata),
                content_fields: Vec::new(),
            })),
            user_message: None,
            preview_output: None,
        }
    }

    fn indexed_tool_state(metadata: serde_json::Value) -> ToolState {
        indexed_tool_state_with_content("output", metadata)
    }

    #[test]
    fn content_store_follows_block_and_tool_lifecycles() {
        let block_content: TranscriptContent = "assistant".into();
        let block_content_id = block_content.id();
        let mut history = BlockHistory::new();
        let block_id = history.push(Block::Text {
            content: block_content,
        });
        assert_eq!(
            history
                .content_by_id(block_content_id)
                .map(TranscriptContent::snapshot),
            Some("assistant".into())
        );

        let replacement: TranscriptContent = "replacement".into();
        let replacement_id = replacement.id();
        let replacement_owner = replacement.clone();
        history.rewrite(
            block_id,
            Block::Text {
                content: replacement,
            },
        );
        let _replacement_owner_id = history.push(Block::Text {
            content: replacement_owner,
        });
        history.remove_block(block_id);
        assert!(history.content_by_id(block_content_id).is_none());
        assert!(history.content_by_id(replacement_id).is_some());

        let output: TranscriptContent = "output".into();
        let output_id = output.id();
        let preview: TranscriptContent = "preview".into();
        let preview_id = preview.id();
        let tool_id = history.push_with_state(
            Block::ToolCall {
                call_id: "call-1".into(),
                name: "bash".into(),
                summary: protocol::StyledLines::from_plain("run"),
                args: HashMap::new().into(),
            },
            ToolState {
                status: ToolStatus::Pending,
                elapsed: None,
                called_at_ms: None,
                elapsed_active: false,
                output: Some(Box::new(ToolOutput {
                    content: output,
                    is_error: false,
                    metadata: None,
                    content_fields: Vec::new(),
                })),
                user_message: None,
                preview_output: Some(Box::new(ToolOutput {
                    content: preview,
                    is_error: false,
                    metadata: None,
                    content_fields: Vec::new(),
                })),
            },
        );
        assert!(history.content_by_id(output_id).is_some());
        assert!(history.content_by_id(preview_id).is_some());

        let next_output: TranscriptContent = "next output".into();
        let next_output_id = next_output.id();
        assert!(history.apply_tool_state_mutation(
            tool_id,
            ToolStateMutation::Finish {
                status: ToolStatus::Err,
                output: Some(Box::new(ToolOutput {
                    content: next_output,
                    is_error: true,
                    metadata: Some(serde_json::json!({ "exit_code": 1 })),
                    content_fields: Vec::new(),
                })),
                elapsed: None,
            },
        ));
        assert!(history.content_by_id(output_id).is_none());
        assert!(history.content_by_id(preview_id).is_none());
        assert!(history.content_by_id(next_output_id).is_some());
        let state = history.tool_state(tool_id).expect("finished tool state");
        let output = state.output.as_ref().expect("final output");
        assert_eq!(state.status, ToolStatus::Err);
        assert_eq!(output.content.id(), next_output_id);
        assert_eq!(output.content.snapshot(), "next output");
        assert!(output.is_error);
        assert_eq!(output.metadata, Some(serde_json::json!({ "exit_code": 1 })));

        history.remove_block(tool_id);
        assert!(history.content_by_id(next_output_id).is_none());
        assert!(history.content_by_id(replacement_id).is_some());

        history.clear();
        assert!(history.content_by_id(replacement_id).is_none());
    }

    #[test]
    fn retained_tool_display_content_reuses_the_protocol_allocation() {
        let source = Arc::new("display payload".repeat(1_024));
        let source_ptr = source.as_ptr();
        let output = ToolOutput::from_display_content(
            "edited file",
            false,
            None,
            vec![protocol::ToolDisplayContent {
                name: "new_content".into(),
                content: Arc::clone(&source),
            }],
        );

        let content = output
            .content_field("new_content")
            .expect("display content");
        assert_eq!(content.read().chunks()[0].as_ptr(), source_ptr);
    }

    #[test]
    fn retained_accounting_tracks_reserved_payload_capacity() {
        let mut block_text = String::with_capacity(1024 * 1024);
        block_text.push_str("short code line");
        let block_capacity = block_text.capacity();
        let block = Block::CodeLine {
            content: block_text,
            lang: "rust".into(),
        };
        assert!(
            block_retained_bytes(&block)
                >= std::mem::size_of::<Block>().saturating_add(block_capacity)
        );

        let mut output_text = String::with_capacity(2 * 1024 * 1024);
        output_text.push_str("short output");
        let output_capacity = output_text.capacity();
        let state = ToolState {
            status: ToolStatus::Ok,
            elapsed: None,
            called_at_ms: None,
            elapsed_active: false,
            output: Some(Box::new(ToolOutput::new(output_text, false, None))),
            user_message: None,
            preview_output: None,
        };
        assert!(
            tool_state_retained_bytes(&state)
                >= std::mem::size_of::<ToolState>()
                    .saturating_add(std::mem::size_of::<ToolOutput>())
                    .saturating_add(output_capacity)
        );

        let mut process_id = String::with_capacity(4 * 1024);
        process_id.push_str("42");
        let process_capacity = process_id.capacity();
        let process = Block::ProcessStatus {
            text: "complete".into(),
            event: Some(protocol::ProcessStatusEvent::background_process_completed(
                process_id,
                Some(0),
                protocol::JobTermination::Exited,
            )),
        };
        assert!(
            block_retained_bytes(&process)
                >= std::mem::size_of::<Block>().saturating_add(process_capacity)
        );
    }

    #[test]
    fn retained_tool_display_content_is_registered_by_name_and_id() {
        let output = ToolOutput::from_display_content(
            "edited file",
            false,
            Some(serde_json::json!({ "path": "src/lib.rs" })),
            vec![
                protocol::ToolDisplayContent::new("old_content", "before".into()),
                protocol::ToolDisplayContent::new("new_content", "after".into()),
            ],
        );
        let old_id = output.content_field("old_content").unwrap().id();
        let new_id = output.content_field("new_content").unwrap().id();
        let mut history = BlockHistory::new();
        let block_id = history.push_with_state(
            Block::ToolCall {
                call_id: "edit-1".into(),
                name: "edit_file".into(),
                summary: protocol::StyledLines::from_plain("src/lib.rs"),
                args: HashMap::new().into(),
            },
            ToolState {
                status: ToolStatus::Ok,
                elapsed: None,
                called_at_ms: None,
                elapsed_active: false,
                output: Some(Box::new(output)),
                user_message: None,
                preview_output: None,
            },
        );

        assert_eq!(
            history
                .content_by_id(old_id)
                .map(TranscriptContent::snapshot),
            Some("before".into())
        );
        assert_eq!(
            history
                .content_by_id(new_id)
                .map(TranscriptContent::snapshot),
            Some("after".into())
        );
        history.remove_block(block_id);
        assert!(history.content_by_id(old_id).is_none());
        assert!(history.content_by_id(new_id).is_none());
    }

    #[test]
    fn mark_changed_rebuilds_content_store_after_external_mutation() {
        let original: TranscriptContent = "original".into();
        let original_id = original.id();
        let mut history = BlockHistory::new();
        let id = history.push(Block::Text { content: original });
        let replacement: TranscriptContent = "replacement".into();
        let replacement_id = replacement.id();
        let Some(BlockEntry::Live(live)) = history.entries.get_mut(&id) else {
            panic!("live block");
        };
        live.block = Block::Text {
            content: replacement,
        };

        history.mark_changed();

        assert!(history.content_by_id(original_id).is_none());
        assert!(history.content_by_id(replacement_id).is_some());
    }

    fn edit_file_block(old_string: &str, new_string: &str) -> Block {
        let mut args = std::collections::HashMap::new();
        args.insert(
            "file_path".to_string(),
            serde_json::Value::String("/tmp/example.rs".to_string()),
        );
        args.insert(
            "old_string".to_string(),
            serde_json::Value::String(old_string.to_string()),
        );
        args.insert(
            "new_string".to_string(),
            serde_json::Value::String(new_string.to_string()),
        );
        Block::ToolCall {
            call_id: "call-1".to_string(),
            name: "edit_file".to_string(),
            summary: protocol::StyledLines::from_plain("example.rs"),
            args: args.into(),
        }
    }

    fn multi_arg_tool_block() -> Block {
        Block::ToolCall {
            call_id: "call-1".into(),
            name: "bash".into(),
            summary: protocol::StyledLines::from_plain("run command"),
            args: HashMap::from([
                ("command".into(), serde_json::json!("echo hi")),
                ("description".into(), serde_json::json!("regression")),
                ("timeout_ms".into(), serde_json::json!(30_000)),
                ("background".into(), serde_json::json!(false)),
                ("alpha".into(), serde_json::json!({"nested": true})),
                ("bravo".into(), serde_json::json!([1, 2, 3])),
                ("charlie".into(), serde_json::json!(null)),
                ("delta".into(), serde_json::json!(4)),
            ])
            .into(),
        }
    }

    #[test]
    fn indexed_text_preserves_full_size_for_extent_estimation() {
        let block = Block::Text {
            content: "alpha λ".to_string().into(),
        };

        let indexed = transcript_indexed_text(&block, None);

        assert_eq!(indexed.indexed_text, "alpha λ");
        assert_eq!(indexed.estimated_text_bytes, "alpha λ".len() as u64);
    }

    #[test]
    fn raw_text_length_does_not_materialize_retained_content() {
        for block in [
            Block::Text {
                content: "assistant".into(),
            },
            Block::Thinking {
                title: Some("Reasoning".into()),
                summary_titles: Vec::new(),
                content: "details".into(),
                kind: protocol::ReasoningKind::default(),
            },
            Block::Thinking {
                title: None,
                summary_titles: vec!["First".into(), "Second".into()],
                content: "details".into(),
                kind: protocol::ReasoningKind::default(),
            },
            Block::Exec {
                command: "printf hi".into(),
                output: "hi".into(),
            },
        ] {
            assert_eq!(
                block.raw_text_len(),
                block.raw_text().map(|text| text.len()),
                "raw text length differs for {block:?}"
            );
        }
    }

    fn cap_indexed_text_reference(text: &str, max_bytes: usize) -> String {
        if text.len() <= max_bytes {
            return text.to_string();
        }
        let head_end = smelt_buffer::text::snap(text, max_bytes / 2);
        let tail_min = text.len().saturating_sub(max_bytes - head_end);
        let tail_start = next_char_boundary_at_or_after(text, tail_min);
        let omitted_bytes = tail_start.saturating_sub(head_end);
        format!(
            "{}\n… {omitted_bytes} bytes omitted from persistent search index …\n{}",
            smelt_buffer::text::slice(text, 0..head_end),
            smelt_buffer::text::slice(text, tail_start..text.len())
        )
    }

    #[test]
    fn bounded_indexed_text_matches_contiguous_utf8_cap() {
        let cases = [
            vec!["short"],
            vec!["α", "日", "本", "語", "ω"],
            vec!["head", "α日本語", "middle", "λ", "tail"],
            vec!["α日本語middleλtail"],
        ];
        for parts in cases {
            let full = parts.concat();
            for max_bytes in [5, 8, 11, 17] {
                let mut bounded = BoundedIndexedText::new(max_bytes);
                for part in &parts {
                    bounded.append(part);
                }
                assert_eq!(
                    bounded.finish().indexed_text,
                    cap_indexed_text_reference(&full, max_bytes),
                    "parts={parts:?}, max_bytes={max_bytes}"
                );
            }
        }
    }

    #[test]
    fn indexed_text_cap_is_utf8_safe_across_retained_chunks() {
        let content = TranscriptContent::new();
        content.append_owned(format!(
            "α{}",
            "a".repeat(TRANSCRIPT_INDEXED_TEXT_MAX_BYTES)
        ));
        content.append_owned("日".repeat(TRANSCRIPT_INDEXED_TEXT_MAX_BYTES));
        content.append_owned(format!(
            "{}ω",
            "b".repeat(TRANSCRIPT_INDEXED_TEXT_MAX_BYTES)
        ));
        let source = content.snapshot();
        let block = Block::Text { content };

        let indexed = transcript_indexed_text(&block, None);

        assert_eq!(
            indexed.indexed_text,
            cap_indexed_text_reference(&source, TRANSCRIPT_INDEXED_TEXT_MAX_BYTES)
        );
        assert_eq!(indexed.estimated_text_bytes, source.len() as u64);
        assert!(indexed.indexed_text.starts_with("α"));
        assert!(indexed.indexed_text.ends_with("ω"));
        assert!(indexed.indexed_text.contains("bytes omitted"));
    }

    #[test]
    fn block_row_computes_indexed_text_from_record() {
        let record = TranscriptBlockRecord {
            block: Block::ToolCall {
                call_id: "call-1".to_string(),
                name: "bash".to_string(),
                summary: protocol::StyledLines::from_plain("bash"),
                args: std::collections::HashMap::new().into(),
            },
            content_hash: 42,
            origin: None,
            tool_state: Some(indexed_tool_state_with_content(
                "alpha λ",
                serde_json::json!({}),
            )),
            tool_render_revision: 17,
        };

        let row = transcript_block_row(7, &record).expect("block row");

        assert_eq!(row.block_idx, 7);
        let expected = "bash\nok\nbash\nalpha λ";
        assert_eq!(row.indexed_text, expected);
        assert_eq!(row.estimated_text_bytes, expected.len() as u64);
    }

    #[test]
    fn hydration_rejects_block_json_that_does_not_match_content_hash() {
        let block = multi_arg_tool_block();
        let record = TranscriptBlockRecord {
            content_hash: block.content_hash(),
            block,
            origin: Some(BlockOrigin::History(0)),
            tool_state: None,
            tool_render_revision: 0,
        };
        let row = transcript_block_row(0, &record).unwrap();
        let stored = compact_block_rows(0, vec![row.clone()])
            .unwrap()
            .pop()
            .unwrap();
        let mut tampered_row = row;
        tampered_row.block_json = tampered_row
            .block_json
            .replacen("echo hi", "echo tampered", 1);
        let tampered = TranscriptBlockRecordWithId::try_from(tampered_row).unwrap();
        let mut history = BlockHistory::new();
        history.install_stored_projection([(stored.block_id, stored.stored)]);
        let stored = history.stored_ref(tampered.block_id).unwrap().clone();

        assert!(!history.install_hydrated_record(tampered.block_id, stored, tampered.record));
    }

    #[test]
    fn indexed_text_includes_tool_identity_args_and_bounded_output() {
        let mut args = std::collections::HashMap::new();
        args.insert(
            "command".to_string(),
            serde_json::Value::String("printf needle".to_string()),
        );
        args.insert(
            "api_key".to_string(),
            serde_json::Value::String("secret needle".to_string()),
        );
        let block = Block::ToolCall {
            call_id: "call-1".to_string(),
            name: "bash".to_string(),
            summary: protocol::StyledLines::from_plain("printf needle"),
            args: args.into(),
        };
        let output = format!(
            "visible head\n{}\nhidden needle\n{}\nvisible tail",
            "a".repeat(TRANSCRIPT_INDEXED_TEXT_MAX_BYTES * 2),
            "b".repeat(TRANSCRIPT_INDEXED_TEXT_MAX_BYTES * 2)
        );
        let state = indexed_tool_state_with_content(&output, serde_json::json!({}));

        let indexed = transcript_indexed_text(&block, Some(&state));

        assert!(indexed.indexed_text.contains("bash"));
        assert!(indexed.indexed_text.contains("command: printf needle"));
        assert!(!indexed.indexed_text.contains("secret needle"));
        assert!(indexed.indexed_text.contains("visible head"));
        assert!(indexed.indexed_text.contains("visible tail"));
        assert!(indexed.indexed_text.contains("bytes omitted"));
        assert!(!indexed.indexed_text.contains("hidden needle"));
        assert!((indexed.indexed_text.len() as u64) < indexed.estimated_text_bytes);
    }

    #[test]
    fn display_count_indexed_text_matches_default_lua_plural_rules() {
        let state = indexed_tool_state(serde_json::json!({
            "display_count": { "value": 2, "unit": "match" }
        }));
        assert_eq!(
            display_count_indexed_text(&state).as_deref(),
            Some("2 matches")
        );

        let state = indexed_tool_state(serde_json::json!({
            "display_count": { "value": 1, "unit": "file", "plural": "files" }
        }));
        assert_eq!(
            display_count_indexed_text(&state).as_deref(),
            Some("1 file")
        );
    }

    #[test]
    fn indexed_text_includes_thinking_summary_chrome() {
        let block = Block::Thinking {
            title: Some("Analyzing the bug".to_string()),
            summary_titles: vec![
                "Inspecting the report".to_string(),
                "Analyzing the bug".to_string(),
            ],
            content: "Checking files\nReviewing output".to_string().into(),
            kind: protocol::ReasoningKind::Summary,
        };
        assert_eq!(
            transcript_indexed_text(&block, None).indexed_text,
            "**Inspecting the report**\n**Analyzing the bug**\nChecking files\nReviewing output\nAnalyzing the bug\n… 2 lines collapsed …"
        );

        let block = Block::Thinking {
            title: None,
            summary_titles: Vec::new(),
            content: "Checking files\nReviewing output".to_string().into(),
            kind: protocol::ReasoningKind::Raw,
        };
        assert_eq!(
            transcript_indexed_text(&block, None).indexed_text,
            "Checking files\nReviewing output\nthinking\n… 2 lines collapsed …"
        );
    }

    #[test]
    fn indexed_text_includes_default_compacted_chrome() {
        let block = Block::Compacted {
            summary: "archived".to_string(),
        };
        assert_eq!(
            transcript_indexed_text(&block, None).indexed_text,
            "archived\ncompacted\n─"
        );
    }

    #[test]
    fn indexed_text_includes_retained_edit_file_content() {
        let block = edit_file_block("old needle", "new needle");
        let mut state = indexed_tool_state_with_content(
            "edited example.rs",
            serde_json::json!({ "path": "/tmp/example.rs" }),
        );
        state.output.as_mut().unwrap().content_fields = vec![
            ToolOutputContentField {
                name: "old_content".into(),
                content: "fn old_retained() {}\n".into(),
            },
            ToolOutputContentField {
                name: "new_content".into(),
                content: "fn new_retained() {}\n".into(),
            },
        ];

        assert_eq!(
            transcript_indexed_text(&block, Some(&state)).indexed_text,
            "edit_file\nok\nexample.rs\nedited example.rs\n1 old line, 1 new line\n/tmp/example.rs\nfn old_retained() {}\nfn new_retained() {}\n"
        );
    }

    #[test]
    fn indexed_text_includes_edit_file_planned_strings_without_retained_content() {
        let block = edit_file_block("alpha\nbeta", "gamma");

        assert_eq!(
            transcript_indexed_text(&block, None).indexed_text,
            "edit_file\nexample.rs\n2 old lines, 1 new line\n/tmp/example.rs\nalpha\nbeta\ngamma"
        );
    }

    #[test]
    fn rewrite_preserves_id_and_bumps_generation() {
        let mut history = BlockHistory::new();
        let id = history.push(Block::Text {
            content: "hello".into(),
        });

        let h0 = history.content_hash(id);
        let g0 = history.generation();

        history.rewrite(
            id,
            Block::Text {
                content: "hello world".into(),
            },
        );
        let h1 = history.content_hash(id);
        assert_ne!(h0, h1, "content hash must update on rewrite");
        assert_eq!(
            history.order.to_vec(),
            vec![id],
            "rewrite must not change order"
        );
        assert_ne!(history.generation(), g0, "rewrite must bump generation");
    }

    #[test]
    fn append_live_text_emits_precise_patch_and_hashes_on_commit() {
        let mut history = BlockHistory::new();
        let id = history.push(Block::Text {
            content: "hello".into(),
        });
        history.set_status(id, Status::Streaming);
        let canonical_before = history.content_hash(id);
        let layout_before = history.resolve_key(
            id,
            LayoutKey {
                width: 80,
                view_state: ViewState::Expanded,
                content_hash: 0,
                sidecar_hash: 0,
            },
        );
        let revision = history.patch_revision();

        assert_eq!(
            history.append_live_text_segments(id, [" world"]),
            Some(5..11)
        );
        assert_eq!(
            history.block(id).and_then(Block::raw_text),
            Some("hello world".into())
        );
        assert_eq!(history.content_hash(id), canonical_before);
        assert_ne!(
            history.resolve_key(
                id,
                LayoutKey {
                    width: 80,
                    view_state: ViewState::Expanded,
                    content_hash: 0,
                    sidecar_hash: 0,
                },
            ),
            layout_before
        );

        let patches = history
            .patches_since(revision)
            .expect("append revision is retained")
            .collect::<Vec<_>>();
        assert_eq!(patches.len(), 1);
        assert_eq!(
            patches[0].operations,
            vec![TranscriptPatchOperation::Append {
                id,
                content_id: history
                    .content(id, ContentChannel::Primary)
                    .expect("primary content")
                    .id(),
                channel: ContentChannel::Primary,
                byte_range: 5..11,
            }]
        );

        history.set_status(id, Status::Done);
        assert_eq!(
            history.content_hash(id),
            history.block(id).expect("live block").content_hash()
        );
        let commit = history.patches.back().expect("commit patch");
        assert_eq!(
            commit.operations,
            vec![
                TranscriptPatchOperation::SetStatus { id },
                TranscriptPatchOperation::Commit { id },
            ]
        );
    }

    #[test]
    fn tool_output_append_changes_structure_only_when_attaching_the_channel() {
        let mut history = BlockHistory::new();
        let id = history.push_with_state(
            Block::ToolCall {
                call_id: "call-1".to_string(),
                name: "bash".to_string(),
                summary: protocol::StyledLines::default(),
                args: HashMap::new().into(),
            },
            ToolState {
                status: ToolStatus::Pending,
                elapsed: None,
                called_at_ms: None,
                elapsed_active: false,
                output: None,
                user_message: None,
                preview_output: None,
            },
        );
        let before_attach = history.sidecar_hash(id);
        let revision = history.patch_revision();

        assert_eq!(
            history.append_live_output_line(id, ContentChannel::ToolOutput, "hello".to_string(),),
            Some(0..5)
        );
        let after_attach = history.sidecar_hash(id);
        assert_ne!(after_attach, before_attach);

        assert_eq!(
            history.append_live_output_line(id, ContentChannel::ToolOutput, "world".to_string(),),
            Some(5..11)
        );
        assert_eq!(history.sidecar_hash(id), after_attach);
        let content = history
            .content(id, ContentChannel::ToolOutput)
            .expect("tool output");
        assert_eq!(content.snapshot(), "hello\nworld");
        let content_id = content.id();
        assert_eq!(
            history
                .patches_since(revision)
                .expect("tool output patches")
                .flat_map(|patch| patch.operations.iter())
                .cloned()
                .collect::<Vec<_>>(),
            vec![
                TranscriptPatchOperation::Append {
                    id,
                    content_id,
                    channel: ContentChannel::ToolOutput,
                    byte_range: 0..5,
                },
                TranscriptPatchOperation::Append {
                    id,
                    content_id,
                    channel: ContentChannel::ToolOutput,
                    byte_range: 5..11,
                },
            ]
        );

        let revision = history.patch_revision();
        assert_eq!(
            history.append_live_output_line(id, ContentChannel::ToolOutput, String::new()),
            None
        );
        assert_eq!(history.patch_revision(), revision);
    }

    #[test]
    fn final_tool_output_replaces_a_large_streamed_preview() {
        let mut history = BlockHistory::new();
        let id = history.push_with_state(
            Block::ToolCall {
                call_id: "call-1".to_string(),
                name: "bash".to_string(),
                summary: protocol::StyledLines::default(),
                args: HashMap::new().into(),
            },
            ToolState {
                status: ToolStatus::Pending,
                elapsed: None,
                called_at_ms: None,
                elapsed_active: false,
                output: None,
                user_message: None,
                preview_output: None,
            },
        );
        for index in 0..3_000 {
            history.append_live_output_line(
                id,
                ContentChannel::ToolOutput,
                format!("streamed-{index}"),
            );
        }
        let streamed_id = history
            .content(id, ContentChannel::ToolOutput)
            .expect("streamed output")
            .id();

        assert!(history.apply_tool_state_mutation(
            id,
            ToolStateMutation::Finish {
                status: ToolStatus::Ok,
                output: Some(Box::new(ToolOutput::new("bounded final", false, None,))),
                elapsed: Some(Duration::from_secs(1)),
            },
        ));

        let state = history.tool_state(id).expect("finished tool state");
        assert_eq!(state.status, ToolStatus::Ok);
        assert_eq!(
            state.output.as_ref().unwrap().content.snapshot(),
            "bounded final"
        );
        assert!(state.preview_output.is_none());
        assert!(history.content_by_id(streamed_id).is_none());
    }

    #[test]
    fn exec_output_appends_owned_utf8_lines_to_the_stable_content_channel() {
        let mut history = BlockHistory::new();
        let id = history.push(Block::Exec {
            command: "printf".to_string(),
            output: TranscriptContent::new(),
        });
        let content_id = history
            .content(id, ContentChannel::ExecOutput)
            .expect("exec output")
            .id();
        let revision = history.patch_revision();

        assert_eq!(
            history.append_live_output_line(id, ContentChannel::ExecOutput, "hello".to_string(),),
            Some(0..5)
        );
        assert_eq!(
            history.append_live_output_line(id, ContentChannel::ExecOutput, "β".to_string()),
            Some(5..8)
        );
        let content = history
            .content(id, ContentChannel::ExecOutput)
            .expect("exec output");
        assert_eq!(content.id(), content_id);
        assert_eq!(content.snapshot(), "hello\nβ");
        assert_eq!(
            history
                .patches_since(revision)
                .expect("exec output patches")
                .flat_map(|patch| patch.operations.iter())
                .cloned()
                .collect::<Vec<_>>(),
            vec![
                TranscriptPatchOperation::Append {
                    id,
                    content_id,
                    channel: ContentChannel::ExecOutput,
                    byte_range: 0..5,
                },
                TranscriptPatchOperation::Append {
                    id,
                    content_id,
                    channel: ContentChannel::ExecOutput,
                    byte_range: 5..8,
                },
            ]
        );

        let revision = history.patch_revision();
        assert_eq!(
            history.append_live_output_line(id, ContentChannel::ExecOutput, String::new()),
            None
        );
        assert_eq!(history.patch_revision(), revision);
    }

    #[test]
    fn order_index_map_tracks_insert_remove_and_truncate() {
        let mut history = BlockHistory::new();
        let a = history.push(Block::Text {
            content: "a".into(),
        });
        let c = history.push(Block::Text {
            content: "c".into(),
        });
        let b = history.add_block(
            Some(1),
            Block::Text {
                content: "b".into(),
            },
            None,
        );

        history.ensure_order_indices();
        assert_eq!(history.order, vec![a, b, c]);
        assert_eq!(history.order_indices.get(&a), Some(&0));
        assert_eq!(history.order_indices.get(&b), Some(&1));
        assert_eq!(history.order_indices.get(&c), Some(&2));

        history.remove_block(b);
        history.ensure_order_indices();
        assert_eq!(history.order, vec![a, c]);
        assert_eq!(history.order_indices.get(&c), Some(&1));
        history.truncate(1);
        history.ensure_order_indices();
        assert_eq!(history.order, vec![a]);
        assert_eq!(history.order_indices, HashMap::from([(a, 0)]));
    }

    #[test]
    fn transcript_patches_replay_to_an_equivalent_replica() {
        #[derive(Default)]
        struct Replica {
            order: Vec<BlockId>,
            blocks: HashMap<BlockId, Block>,
            statuses: HashMap<BlockId, Status>,
        }

        fn detached_block(block: &Block) -> Block {
            serde_json::from_value(serde_json::to_value(block).expect("serialize block"))
                .expect("deserialize block")
        }

        fn apply_since(source: &BlockHistory, revision: &mut u64, replica: &mut Replica) {
            for patch in source
                .patches_since(*revision)
                .expect("replica revision is retained")
            {
                for operation in &patch.operations {
                    match operation {
                        TranscriptPatchOperation::Insert { id, index } => {
                            replica.order.insert(*index, *id);
                            replica.blocks.insert(
                                *id,
                                detached_block(source.block(*id).expect("inserted block")),
                            );
                        }
                        TranscriptPatchOperation::Append {
                            id,
                            content_id,
                            channel,
                            byte_range,
                        } => {
                            assert_eq!(*channel, ContentChannel::Primary);
                            let source_content = source
                                .content(*id, *channel)
                                .expect("appended content channel");
                            assert_eq!(source_content.id(), *content_id);
                            let suffix =
                                source_content.read().slice(byte_range.clone()).into_owned();
                            let replica_content = match replica.blocks.get_mut(id) {
                                Some(Block::Text { content })
                                | Some(Block::Thinking { content, .. }) => content,
                                _ => panic!("replica append target is text"),
                            };
                            assert_eq!(replica_content.len(), byte_range.start);
                            replica_content.append_owned(suffix);
                        }
                        TranscriptPatchOperation::Replace { id } => {
                            replica.blocks.insert(
                                *id,
                                detached_block(source.block(*id).expect("replaced block")),
                            );
                        }
                        TranscriptPatchOperation::SetStatus { id } => {
                            replica
                                .statuses
                                .insert(*id, source.status(*id).expect("block status"));
                        }
                        TranscriptPatchOperation::Remove { id, index } => {
                            assert_eq!(replica.order.remove(*index), *id);
                            replica.blocks.remove(id);
                            replica.statuses.remove(id);
                        }
                        TranscriptPatchOperation::SetSideState { .. }
                        | TranscriptPatchOperation::SetAnimationState { .. }
                        | TranscriptPatchOperation::Commit { .. } => {}
                        TranscriptPatchOperation::Reset => {
                            replica.order.clone_from(&source.order);
                            replica.blocks = source
                                .order
                                .iter()
                                .filter_map(|id| {
                                    source.block(*id).map(|block| (*id, detached_block(block)))
                                })
                                .collect();
                        }
                    }
                }
                *revision = patch.revision;
            }
        }

        let mut source = BlockHistory::new();
        let mut replica = Replica::default();
        let mut revision = 0;
        let first = source.push(Block::Text {
            content: "one".into(),
        });
        apply_since(&source, &mut revision, &mut replica);
        source.set_status(first, Status::Streaming);
        apply_since(&source, &mut revision, &mut replica);
        source.append_live_text_segments(first, [" β"]);
        apply_since(&source, &mut revision, &mut replica);
        source.rewrite(
            first,
            Block::Text {
                content: "replaced".into(),
            },
        );
        apply_since(&source, &mut revision, &mut replica);
        let second = source.push(Block::Text {
            content: "two".into(),
        });
        apply_since(&source, &mut revision, &mut replica);
        source.remove_block(first);
        apply_since(&source, &mut revision, &mut replica);

        assert_eq!(replica.order, source.order);
        assert_eq!(replica.order, vec![second]);
        assert_eq!(replica.blocks.get(&second), source.block(second));
        assert_eq!(revision, source.patch_revision());
    }

    #[test]
    fn transcript_patch_compaction_preserves_streaming_suffix_after_structural_prefix() {
        let mut history = BlockHistory::new();
        let id = history.push(Block::Text {
            content: String::new().into(),
        });
        let scene_revision = history.patch_revision();
        let mut mid_stream_revision = scene_revision;

        for index in 0..=2048 {
            let segment = format!("{index},");
            history.append_live_text_segments(id, [segment.as_str()]);
            if index == 10 {
                mid_stream_revision = history.patch_revision();
            }
        }

        assert!(
            history.patch_retained_bytes <= TRANSCRIPT_PATCH_LOG_BUDGET_BYTES,
            "patch compaction should return retained patch bytes to budget: {}",
            history.patch_retained_bytes
        );
        let patches = history
            .patches_since(scene_revision)
            .expect("streaming suffix remains replayable after compaction")
            .collect::<Vec<_>>();
        assert!(
            patches
                .iter()
                .flat_map(|patch| &patch.operations)
                .all(|operation| !matches!(operation, TranscriptPatchOperation::Reset)),
            "up-to-date transcript scenes should not rebuild after streaming compaction: {patches:?}"
        );
        assert!(
            patches
                .iter()
                .flat_map(|patch| &patch.operations)
                .any(|operation| matches!(operation, TranscriptPatchOperation::Replace { id: patch_id } if *patch_id == id)),
            "streaming compaction should preserve a block-local replacement for the visible block: {patches:?}"
        );
        let mid_stream_patches = history
            .patches_since(mid_stream_revision)
            .expect("mid-stream revision remains replayable after compaction")
            .collect::<Vec<_>>();
        let mut mid_stream_operations = mid_stream_patches
            .iter()
            .flat_map(|patch| &patch.operations);
        assert!(
            mid_stream_operations.all(|operation| !matches!(operation, TranscriptPatchOperation::Reset)),
            "mid-stream consumers should not need a full rebuild after streaming compaction: {mid_stream_patches:?}"
        );
        assert!(
            mid_stream_patches
                .iter()
                .flat_map(|patch| &patch.operations)
                .any(|operation| matches!(operation, TranscriptPatchOperation::Replace { id: patch_id } if *patch_id == id)),
            "mid-stream consumers need a revision-independent block replacement before later appends: {mid_stream_patches:?}"
        );
        assert!(
            history.patches_since(scene_revision - 1).is_some(),
            "the structural-prefix reset remains available for older revisions"
        );
    }

    #[test]
    fn animation_patches_coalesce_and_yield_to_presentation_state() {
        let presentation_id = BlockId::new(1);
        let animation_id = BlockId::new(2);
        let patches = [
            TranscriptPatch {
                revision: 1,
                operations: vec![TranscriptPatchOperation::SetAnimationState {
                    id: presentation_id,
                }],
                navigation_changed: false,
                search_changed: false,
                persistable_changed: false,
            },
            TranscriptPatch {
                revision: 2,
                operations: vec![
                    TranscriptPatchOperation::SetAnimationState {
                        id: presentation_id,
                    },
                    TranscriptPatchOperation::SetAnimationState { id: animation_id },
                ],
                navigation_changed: false,
                search_changed: false,
                persistable_changed: false,
            },
            TranscriptPatch {
                revision: 3,
                operations: vec![
                    TranscriptPatchOperation::SetAnimationState { id: animation_id },
                    TranscriptPatchOperation::SetSideState {
                        id: presentation_id,
                    },
                ],
                navigation_changed: false,
                search_changed: true,
                persistable_changed: true,
            },
        ];

        let compacted = TranscriptPatch::coalesce(3, patches.iter());

        assert_eq!(
            compacted.operations,
            vec![
                TranscriptPatchOperation::SetSideState {
                    id: presentation_id,
                },
                TranscriptPatchOperation::SetAnimationState { id: animation_id },
            ]
        );
        assert!(!compacted.navigation_changed);
        assert!(compacted.search_changed);
        assert!(compacted.persistable_changed);
    }

    #[test]
    fn status_changes_do_not_bump_block_dirty_generation() {
        let mut history = BlockHistory::new();
        let id = history.push(Block::Text {
            content: "hello".into(),
        });
        history.clear_record_dirty();
        let generation = history.generation();
        let block_generation = history.record_dirty_generation();

        history.set_status(id, Status::Streaming);

        assert_ne!(history.generation(), generation);
        assert_eq!(history.record_dirty_generation(), block_generation);
        assert_eq!(history.record_dirty_from(), None);
    }

    #[test]
    fn block_change_log_tracks_unrendered_mutations_after_persistence() {
        let mut history = BlockHistory::new();
        history.push(Block::Text {
            content: "first".into(),
        });
        history.clear_record_dirty();
        let generation = history.record_dirty_generation();

        assert_eq!(
            history.record_changed_from_since(generation),
            Some(history.order.len())
        );

        history.push(Block::Text {
            content: "second".into(),
        });
        history.clear_record_dirty();
        assert_eq!(history.record_changed_from_since(generation), Some(1));

        history.require_record_resave_from(0);
        assert_eq!(history.record_changed_from_since(generation), Some(0));

        for _ in 0..RECORD_CHANGE_LOG_CAPACITY {
            history.require_record_resave_from(0);
        }
        assert_eq!(history.record_changed_from_since(generation), None);
    }

    #[test]
    fn identical_blocks_get_distinct_ids() {
        // Each push mints a fresh monotonic `BlockId`. Identical content
        // at two positions no longer shares a slot in `blocks`.
        let mut history = BlockHistory::new();
        let a = history.push(Block::Text {
            content: "same".into(),
        });
        let b = history.push(Block::Text {
            content: "same".into(),
        });
        assert_ne!(a, b);
        assert_eq!(history.order.len(), 2);
        assert_eq!(history.entries.len(), 2);
        assert_eq!(history.content_hash(a), history.content_hash(b));
    }

    #[test]
    fn stored_block_metadata_does_not_hydrate_block() {
        let record = TranscriptBlockRecord {
            block: Block::ToolCall {
                call_id: "call-1".into(),
                name: "read_file".into(),
                summary: "read".into(),
                args: HashMap::from([("path".into(), serde_json::json!("/tmp/a"))]).into(),
            },
            content_hash: 0,
            origin: Some(BlockOrigin::History(0)),
            tool_state: None,
            tool_render_revision: 0,
        };
        let history = BlockHistory::from_block_records(vec![record]);
        let id = history.order[0];

        assert!(!history.is_materialized(id));
        assert_eq!(history.block_kind(id), Some("tool"));
        assert_eq!(history.tool_name(id), Some("read_file"));
        assert_eq!(history.tool_call_id(id), Some("call-1"));
        assert_eq!(
            history.arg_field(id, "path"),
            Some(&serde_json::json!("/tmp/a"))
        );
        assert_eq!(history.block_origin(id), Some(BlockOrigin::History(0)));
        assert_ne!(history.content_hash(id), 0);
        assert!(history.block(id).is_none());
        assert_eq!(history.last_block_id(), Some(id));
        assert!(history.materialized_block_at(0).is_none());
    }

    #[test]
    #[should_panic(expected = "block id in transcript order")]
    fn materialized_block_at_rejects_dangling_order_id() {
        let mut history = BlockHistory::new();
        history.order.push(BlockId::new(42));

        let _ = history.materialized_block_at(0);
    }

    #[test]
    fn hydration_moves_large_block_payload_without_cloning() {
        let mut content = String::with_capacity(2 * 1024 * 1024);
        content.push_str("retained code");
        let source_ptr = content.as_ptr();
        let id = BlockId::new(17);
        let record = TranscriptBlockRecord {
            block: Block::CodeLine {
                content,
                lang: "text".into(),
            },
            content_hash: 0,
            origin: Some(BlockOrigin::History(0)),
            tool_state: None,
            tool_render_revision: 0,
        };
        let (_, stored) = StoredBlockRef::from_record(
            0,
            id,
            &record,
            "retained code".len() as u64,
            "retained code".into(),
        );
        let mut history = BlockHistory::new();

        assert!(history.install_hydrated_record(id, stored, record));
        let Some(Block::CodeLine { content, .. }) = history.block(id) else {
            panic!("hydrated code block");
        };
        assert_eq!(content.as_ptr(), source_ptr);
    }

    #[test]
    fn explicit_hydration_and_eviction_preserve_exact_record() {
        let state = ToolState {
            status: ToolStatus::Ok,
            elapsed: None,
            called_at_ms: Some(1_742_573_823_000),
            elapsed_active: false,
            output: Some(Box::new(ToolOutput {
                content: "hi".into(),
                is_error: false,
                metadata: Some(serde_json::json!({"small": true})),
                content_fields: Vec::new(),
            })),
            user_message: None,
            preview_output: None,
        };
        let record = TranscriptBlockRecord {
            block: Block::ToolCall {
                call_id: "call-1".into(),
                name: "bash".into(),
                summary: "run".into(),
                args: HashMap::from([("command".into(), serde_json::json!("echo hi"))]).into(),
            },
            content_hash: 0,
            origin: Some(BlockOrigin::History(1)),
            tool_state: Some(state.clone()),
            tool_render_revision: 23,
        };
        let mut history = BlockHistory::from_block_records(vec![record.clone()]);
        assert_eq!(history.block_metadata_retained_bytes(), 0);
        let id = history.order[0];
        let stored = history.stored_ref(id).cloned().expect("stored ref");
        let expected_weight =
            block_retained_bytes(&record.block) + tool_state_retained_bytes(&state);

        assert!(history.install_hydrated_record(id, stored, record));
        assert!(history.is_hydrated(id));
        assert_eq!(history.block_metadata_retained_bytes(), 0);
        assert_eq!(history.hydrated_block_count(), 1);
        assert_eq!(history.hydrated_blocks().count(), 1);
        assert_eq!(history.hydrated_retained_bytes(), expected_weight);
        assert_eq!(
            history.hydrated_retained_bytes(),
            history
                .hydrated_block_retained_bytes()
                .saturating_add(history.hydrated_tool_state_retained_bytes())
        );
        assert!(matches!(history.block(id), Some(Block::ToolCall { name, .. }) if name == "bash"));
        let output = history
            .tool_state(id)
            .and_then(|tool_state| tool_state.output.as_ref())
            .expect("hydrated tool output");
        assert_eq!(output.content.snapshot(), "hi");
        let output_id = output.content.id();
        assert_eq!(
            history
                .content_by_id(output_id)
                .map(TranscriptContent::snapshot),
            Some("hi".into())
        );
        assert_eq!(
            history.tool_state(id).and_then(|state| state.called_at_ms),
            Some(1_742_573_823_000)
        );
        assert_eq!(history.block_origin(id), Some(BlockOrigin::History(1)));

        assert_eq!(history.evict_hydrated(id), expected_weight);
        assert!(!history.is_materialized(id));
        assert_eq!(history.block_metadata_retained_bytes(), 0);
        assert_eq!(history.hydrated_block_count(), 0);
        assert_eq!(history.hydrated_blocks().count(), 0);
        assert_eq!(history.hydrated_retained_bytes(), 0);
        assert_eq!(history.tool_status(id), Some(ToolStatus::Ok));
        assert!(history.tool_state(id).is_none());
        assert!(history.content_by_id(output_id).is_none());
        assert_eq!(history.block_origin(id), Some(BlockOrigin::History(1)));
    }

    #[test]
    fn state_specific_eviction_never_removes_other_entry_states() {
        let record = TranscriptBlockRecord {
            block: Block::Text {
                content: "stored".into(),
            },
            content_hash: 0,
            origin: None,
            tool_state: None,
            tool_render_revision: 0,
        };
        let mut stored_history = BlockHistory::from_block_records(vec![record.clone()]);
        let stored_id = stored_history.order[0];
        let stored = stored_history
            .stored_ref(stored_id)
            .cloned()
            .expect("stored ref");

        assert_eq!(stored_history.evict_hydrated(stored_id), 0);
        assert!(stored_history.stored_ref(stored_id).is_some());
        assert!(!stored_history.promote_hydrated(stored_id));
        assert!(stored_history.stored_ref(stored_id).is_some());
        assert_eq!(
            stored_history.dematerialize_live(stored_id, Arc::clone(&stored)),
            0
        );
        assert!(stored_history.stored_ref(stored_id).is_some());

        assert!(stored_history.install_hydrated_record(stored_id, Arc::clone(&stored), record));
        assert_eq!(stored_history.dematerialize_live(stored_id, stored), 0);
        assert!(stored_history.is_hydrated(stored_id));

        let mut live_history = BlockHistory::new();
        let live_id = live_history.push(Block::Text {
            content: "live".into(),
        });
        assert_eq!(live_history.evict_hydrated(live_id), 0);
        assert!(live_history.is_live(live_id));
        assert!(live_history.promote_hydrated(live_id));
        assert!(live_history.is_live(live_id));
    }

    #[test]
    fn hydrated_mutation_promotes_to_live_and_marks_block_dirty() {
        let record = TranscriptBlockRecord {
            block: Block::Text {
                content: "before".into(),
            },
            content_hash: 0,
            origin: Some(BlockOrigin::History(0)),
            tool_state: None,
            tool_render_revision: 0,
        };
        let mut history = BlockHistory::from_block_records(vec![record.clone()]);
        let id = history.order[0];
        let stored = history.stored_ref(id).cloned().expect("stored ref");
        assert!(history.install_hydrated_record(id, stored, record));

        history.rewrite(
            id,
            Block::Text {
                content: "after".into(),
            },
        );

        assert!(history.is_live(id));
        assert_eq!(history.hydrated_block_count(), 0);
        assert_eq!(history.hydrated_retained_bytes(), 0);
        assert_eq!(history.record_dirty_from(), Some(0));
        assert!(matches!(history.block(id), Some(Block::Text { content }) if content == "after"));
        assert_eq!(history.block_origin(id), Some(BlockOrigin::History(0)));
    }

    #[test]
    fn hydrated_tool_mutation_promotes_block_and_tool_state_to_live() {
        let state = ToolState {
            status: ToolStatus::Ok,
            elapsed: None,
            called_at_ms: None,
            elapsed_active: false,
            output: Some(Box::new(ToolOutput {
                content: "before".into(),
                is_error: false,
                metadata: None,
                content_fields: Vec::new(),
            })),
            user_message: None,
            preview_output: None,
        };
        let record = TranscriptBlockRecord {
            block: Block::ToolCall {
                call_id: "call-1".into(),
                name: "bash".into(),
                summary: "run".into(),
                args: HashMap::new().into(),
            },
            content_hash: 0,
            origin: Some(BlockOrigin::History(0)),
            tool_state: Some(state),
            tool_render_revision: 29,
        };
        let mut history = BlockHistory::from_block_records(vec![record.clone()]);
        let id = history.order[0];
        let stored = history.stored_ref(id).cloned().expect("stored ref");
        assert!(history.install_hydrated_record(id, stored, record));

        assert!(history.apply_tool_state_mutation(
            id,
            ToolStateMutation::Finish {
                status: ToolStatus::Ok,
                output: Some(Box::new(ToolOutput {
                    content: "after".into(),
                    is_error: false,
                    metadata: None,
                    content_fields: Vec::new(),
                })),
                elapsed: None,
            },
        ));

        assert!(history.is_live(id));
        assert_eq!(history.hydrated_block_count(), 0);
        assert_eq!(history.hydrated_retained_bytes(), 0);
        assert_eq!(history.record_dirty_from(), Some(0));
        assert_eq!(history.block_origin(id), Some(BlockOrigin::History(0)));
        assert_eq!(
            history
                .tool_state(id)
                .and_then(|tool_state| tool_state.output.as_ref())
                .map(|output| output.content.snapshot()),
            Some("after".to_string())
        );
        assert!(history.live_tool_state_retained_bytes() > 0);
        assert_eq!(history.hydrated_tool_state_retained_bytes(), 0);
    }

    #[test]
    fn block_records_skip_transient_tool_drafts() {
        let mut history = BlockHistory::new();
        history.push(Block::Text {
            content: "before".into(),
        });
        let mut draft = ToolDraft::new("stream-1".into(), Some("call-1".into()), "bash".into());
        draft.summary = protocol::StyledLines::from_plain("echo hi");
        draft.append("{\"command\":\"echo hi\"}".into());
        history.push(Block::ToolDraft(draft));
        history.push(Block::Text {
            content: "after".into(),
        });

        let records = history.block_records();

        assert_eq!(records.len(), 2);
        assert!(records
            .iter()
            .all(|record| !matches!(record.block, Block::ToolDraft(_))));
        assert_eq!(records[0].block.raw_text().as_deref(), Some("before"));
        assert_eq!(records[1].block.raw_text().as_deref(), Some("after"));
    }

    #[test]
    fn block_records_skip_compaction_previews() {
        let mut history = BlockHistory::new();
        history.push(Block::Text {
            content: "before".into(),
        });
        history.push(Block::CompactionPreview {
            summary: "streaming summary".into(),
        });
        history.push(Block::Text {
            content: "after".into(),
        });

        let records = history.block_records();

        assert_eq!(records.len(), 2);
        assert!(records
            .iter()
            .all(|record| !matches!(record.block, Block::CompactionPreview { .. })));
        assert_eq!(records[0].block.raw_text().as_deref(), Some("before"));
        assert_eq!(records[1].block.raw_text().as_deref(), Some("after"));
    }

    #[test]
    fn block_record_index_counts_only_persisted_prefix() {
        let mut history = BlockHistory::new();
        history.push(Block::Text {
            content: "before".into(),
        });
        let mut tool_draft =
            ToolDraft::new("stream-1".into(), Some("call-1".into()), "bash".into());
        tool_draft.summary = protocol::StyledLines::from_plain("echo hi");
        tool_draft.append("{\"command\":\"echo hi\"}".into());
        let draft = history
            .push_hydrated_block_with_origin(Block::ToolDraft(tool_draft), BlockOrigin::History(1));
        history.push(Block::CompactionPreview {
            summary: "streaming summary".into(),
        });
        history.push(Block::Text {
            content: "after".into(),
        });
        history.push(Block::Text {
            content: "tail".into(),
        });

        assert_eq!(history.record_index_for_order_index(0), 0);
        assert_eq!(history.record_index_for_order_index(1), 1);
        assert_eq!(history.record_index_for_order_index(2), 1);
        assert_eq!(history.record_index_for_order_index(3), 1);
        assert_eq!(history.record_index_for_order_index(4), 2);
        assert_eq!(history.record_index_for_order_index(5), 3);

        let records = history.block_records_from(3);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].block.raw_text().as_deref(), Some("after"));
        assert_eq!(records[1].block.raw_text().as_deref(), Some("tail"));

        history.rewrite(
            draft,
            Block::Text {
                content: "finished tool".into(),
            },
        );
        assert_eq!(history.record_index_for_order_index(5), 4);
        history.remove_block(draft);
        assert_eq!(history.record_index_for_order_index(4), 3);
        history.truncate(3);
        assert_eq!(history.record_index_for_order_index(3), 2);
    }

    #[test]
    fn block_dirty_range_is_owned_by_block_history_mutations() {
        let mut history = BlockHistory::new();
        assert_eq!(history.record_dirty_from(), None);

        let first = history.push(Block::Text {
            content: "first".into(),
        });
        history.push(Block::Text {
            content: "second".into(),
        });
        assert_eq!(history.record_dirty_from(), Some(0));
        history.clear_record_dirty();

        history.rewrite(
            first,
            Block::Text {
                content: "changed".into(),
            },
        );
        assert_eq!(history.record_dirty_from(), Some(0));
        history.clear_record_dirty();

        history.truncate(1);
        assert_eq!(history.record_dirty_from(), Some(1));

        let restored = BlockHistory::from_block_records(history.block_records());
        assert_eq!(restored.record_dirty_from(), None);
    }

    #[test]
    fn raw_text_preserves_markdown_markers() {
        // Whole-block yank must round-trip every inline / block
        // markdown construct - bold, italic, inline code, fenced code,
        // tables, horizontal rules - because the cell-walked fallback
        // strips the markers.
        let md = concat!(
            "**bold** and *italic* and `inline code`\n",
            "\n",
            "```rust\n",
            "let x = 1;\n",
            "```\n",
            "\n",
            "| col | val |\n",
            "| --- | --- |\n",
            "| a   | 1   |\n",
            "\n",
            "---\n",
        );
        let block = Block::Text { content: md.into() };
        assert_eq!(block.raw_text().as_deref(), Some(md));
    }

    #[test]
    fn raw_text_returns_user_text_verbatim() {
        let block = Block::User {
            text: "Explain **this** in detail.".into(),
            image_labels: vec!["[screenshot.png]".into()],
            command: false,
            sent_at_ms: None,
        };
        // Image labels are a render-time annotation, not part of the
        // user's typed message.
        assert_eq!(
            block.raw_text().as_deref(),
            Some("Explain **this** in detail.")
        );
    }

    #[test]
    fn raw_text_is_none_for_structured_blocks() {
        // Tool blocks don't have a single markdown source - yank falls back
        // to cell-walking for them.
        assert!(Block::ToolCall {
            call_id: "c1".into(),
            name: "bash".into(),
            summary: "ls".into(),
            args: HashMap::new().into(),
        }
        .raw_text()
        .is_none());
    }

    fn pending_state() -> ToolState {
        ToolState {
            status: ToolStatus::Pending,
            elapsed: None,
            called_at_ms: None,
            elapsed_active: false,
            output: None,
            user_message: None,
            preview_output: None,
        }
    }

    #[test]
    fn tool_state_is_terminal_for_ok_err_denied_only() {
        for status in [ToolStatus::Ok, ToolStatus::Err, ToolStatus::Denied] {
            let s = ToolState {
                status,
                ..pending_state()
            };
            assert!(s.is_terminal());
        }
        assert!(!pending_state().is_terminal());
    }

    #[test]
    fn tool_presentation_revision_ignores_elapsed_and_persists() {
        let mut state = pending_state();
        state.output = Some(Box::new(ToolOutput {
            content: "visible output".repeat(4_096).into(),
            is_error: false,
            metadata: Some(serde_json::json!({ "payload": "metadata".repeat(4_096) })),
            content_fields: Vec::new(),
        }));
        let mut history = BlockHistory::new();
        let id = history.push_with_state(
            Block::ToolCall {
                call_id: "call-1".into(),
                name: "bash".into(),
                summary: protocol::StyledLines::default(),
                args: HashMap::new().into(),
            },
            state,
        );
        let initial_revision = history.sidecar_hash(id);
        history.clear_record_dirty();
        assert!(history.apply_tool_state_mutation(
            id,
            ToolStateMutation::SyncElapsed(std::time::Duration::from_millis(1_100)),
        ));
        assert_eq!(history.sidecar_hash(id), initial_revision);
        assert_eq!(history.record_dirty_from(), None);
        assert!(history.apply_tool_state_mutation(
            id,
            ToolStateMutation::SetElapsedActive {
                elapsed: std::time::Duration::from_millis(1_100),
                active: true,
            },
        ));
        let updated_revision = history.sidecar_hash(id);
        assert_ne!(updated_revision, initial_revision);

        let record = history
            .block_record_with_id(id)
            .expect("persisted tool record")
            .record;
        assert_eq!(record.tool_render_revision, updated_revision);
        let row = transcript_block_row_with_block_idx(0, id.get(), &record).unwrap();
        let loaded = TranscriptBlockRecordWithId::try_from(row).unwrap();
        assert_eq!(loaded.record.tool_render_revision, updated_revision);
        assert_eq!(loaded.record.tool_state, record.tool_state);

        let loaded_record = loaded.record.clone();
        let mut restored = BlockHistory::from_block_records_with_ids(vec![loaded]);
        assert_eq!(restored.sidecar_hash(id), updated_revision);
        let stored = Arc::clone(restored.stored_ref(id).expect("stored tool projection"));
        assert!(restored.install_hydrated_record(id, Arc::clone(&stored), loaded_record.clone(),));
        assert_eq!(restored.sidecar_hash(id), updated_revision);
        assert!(restored.promote_hydrated(id));
        assert_eq!(restored.sidecar_hash(id), updated_revision);
        assert!(restored.dematerialize_live(id, Arc::clone(&stored)) > 0);
        assert_eq!(restored.sidecar_hash(id), updated_revision);
        assert!(restored.install_hydrated_record(id, Arc::clone(&stored), loaded_record.clone(),));
        assert!(restored.evict_hydrated(id) > 0);
        assert_eq!(restored.sidecar_hash(id), updated_revision);
        assert!(restored.install_hydrated_record(id, stored, loaded_record));
        assert!(restored.apply_tool_state_mutation(
            id,
            ToolStateMutation::SetUserMessage("still running".into()),
        ));
        assert!(restored.sidecar_hash(id) > updated_revision);
    }

    #[test]
    fn tool_presentation_revision_tracks_structural_state_not_payload_appends() {
        let mut history = BlockHistory::new();
        let id = history.push_with_state(
            Block::ToolCall {
                call_id: "call-1".into(),
                name: "bash".into(),
                summary: protocol::StyledLines::default(),
                args: HashMap::new().into(),
            },
            pending_state(),
        );

        let initial_revision = history.sidecar_hash(id);
        assert!(history.apply_tool_state_mutation(
            id,
            ToolStateMutation::SyncElapsed(std::time::Duration::from_secs(1)),
        ));
        assert_eq!(history.sidecar_hash(id), initial_revision);

        assert!(history
            .append_live_output_line(id, ContentChannel::ToolOutput, "first".into())
            .is_some());
        let attached_revision = history.sidecar_hash(id);
        assert!(attached_revision > initial_revision);
        assert!(history
            .append_live_output_line(id, ContentChannel::ToolOutput, "second".into())
            .is_some());
        assert_eq!(history.sidecar_hash(id), attached_revision);

        assert!(history
            .apply_tool_state_mutation(id, ToolStateMutation::SetUserMessage("working".into()),));
        let message_revision = history.sidecar_hash(id);
        assert!(message_revision > attached_revision);

        assert!(history.apply_tool_state_mutation(
            id,
            ToolStateMutation::SetStatus {
                status: ToolStatus::Confirm,
                elapsed: Some(std::time::Duration::from_secs(2)),
            },
        ));
        let status_revision = history.sidecar_hash(id);
        assert!(status_revision > message_revision);

        assert!(history.apply_tool_state_mutation(
            id,
            ToolStateMutation::Finish {
                status: ToolStatus::Err,
                output: Some(Box::new(ToolOutput {
                    content: "replacement".into(),
                    is_error: true,
                    metadata: Some(serde_json::json!({ "result": "changed" })),
                    content_fields: Vec::new(),
                })),
                elapsed: Some(std::time::Duration::from_secs(3)),
            },
        ));
        assert!(history.sidecar_hash(id) > status_revision);
    }

    #[test]
    fn raw_text_for_thinking_returns_content() {
        let block = Block::Thinking {
            title: None,
            summary_titles: Vec::new(),
            kind: protocol::ReasoningKind::Raw,
            content: "ponder".into(),
        };
        assert_eq!(block.raw_text().as_deref(), Some("ponder"));
    }

    #[test]
    fn reasoning_summary_raw_text_includes_title_history_and_real_body() {
        let block = Block::Thinking {
            title: Some("Latest title".into()),
            summary_titles: vec!["First title".into(), "Latest title".into()],
            content: "real body".into(),
            kind: protocol::ReasoningKind::Summary,
        };

        assert_eq!(
            block.raw_text().as_deref(),
            Some("**First title**\n**Latest title**\nreal body")
        );
    }

    #[test]
    fn block_history_normalizes_thinking_title_spacing() {
        let mut history = BlockHistory::new();
        let id = history.push(Block::Thinking {
            title: None,
            summary_titles: Vec::new(),
            kind: protocol::ReasoningKind::Raw,
            content: "**Plan**\n\nbody".into(),
        });

        assert_eq!(
            history.block(id),
            Some(&Block::Thinking {
                title: None,
                summary_titles: Vec::new(),
                kind: protocol::ReasoningKind::Raw,
                content: "**Plan**\nbody".into(),
            })
        );
    }

    #[test]
    fn hydrated_block_history_normalizes_thinking_title_spacing() {
        let mut history = BlockHistory::new();
        let id = history.push_hydrated_block_with_origin(
            Block::Thinking {
                title: None,
                summary_titles: Vec::new(),
                kind: protocol::ReasoningKind::Raw,
                content: "**Plan**\n\nbody".into(),
            },
            BlockOrigin::History(0),
        );

        assert_eq!(
            history.block(id),
            Some(&Block::Thinking {
                title: None,
                summary_titles: Vec::new(),
                kind: protocol::ReasoningKind::Raw,
                content: "**Plan**\nbody".into(),
            })
        );
    }

    #[test]
    fn raw_text_for_compacted_returns_summary() {
        let block = Block::Compacted {
            summary: "earlier session compacted".into(),
        };
        assert_eq!(
            block.raw_text().as_deref(),
            Some("earlier session compacted")
        );
    }

    #[test]
    fn raw_text_for_code_line_returns_content() {
        let block = Block::CodeLine {
            content: "let x = 1;".into(),
            lang: "rust".into(),
        };
        assert_eq!(block.raw_text().as_deref(), Some("let x = 1;"));
    }

    #[test]
    fn raw_text_for_exec_combines_command_and_output() {
        let block = Block::Exec {
            command: "ls".into(),
            output: "foo\nbar".into(),
        };
        assert_eq!(block.raw_text().as_deref(), Some("$ ls\nfoo\nbar"));
    }

    #[test]
    fn content_hash_returns_zero_for_unknown_id() {
        let history = BlockHistory::new();
        assert_eq!(history.content_hash(BlockId(9999)), 0);
        assert_eq!(history.status(BlockId(9999)), None);
    }

    #[test]
    #[should_panic(expected = "status can only change on a live transcript block")]
    fn set_status_rejects_stored_blocks() {
        let record = TranscriptBlockRecord {
            block: Block::Text {
                content: "persisted".into(),
            },
            content_hash: 0,
            origin: None,
            tool_state: None,
            tool_render_revision: 0,
        };
        let mut history = BlockHistory::from_block_records(vec![record]);
        let id = history.order[0];
        assert_eq!(history.status(id), Some(Status::Done));
        history.set_status(id, Status::Streaming);
    }

    #[test]
    #[should_panic(expected = "status can only change on a live transcript block")]
    fn set_status_rejects_hydrated_blocks() {
        let mut history = BlockHistory::new();
        let id = history.push_hydrated_block_with_origin(
            Block::Text {
                content: "hydrated".into(),
            },
            BlockOrigin::History(0),
        );
        assert_eq!(history.status(id), Some(Status::Done));
        history.set_status(id, Status::Streaming);
    }

    #[test]
    #[should_panic(expected = "status can only change on a live transcript block")]
    fn set_status_rejects_missing_blocks() {
        let mut history = BlockHistory::new();
        history.set_status(BlockId(9999), Status::Streaming);
    }

    #[test]
    fn set_status_streaming_then_done_pushes_finished_block() {
        let mut history = BlockHistory::new();
        let id = history.push(Block::Text {
            content: "x".into(),
        });
        history.set_status(id, Status::Streaming);
        assert_eq!(history.status(id), Some(Status::Streaming));
        history.set_status(id, Status::Done);
        assert_eq!(history.status(id), Some(Status::Done));
        assert!(history.finished_blocks.contains(&id));
    }

    #[test]
    fn set_status_done_without_prior_streaming_does_not_push_finished() {
        let mut history = BlockHistory::new();
        let id = history.push(Block::Text {
            content: "x".into(),
        });
        history.set_status(id, Status::Done);
        assert!(!history.finished_blocks.contains(&id));
    }

    #[test]
    fn metadata_updates_preserve_other_block_metadata() {
        let mut history = BlockHistory::new();
        let id = history.push_with_origin(
            Block::Text {
                content: "before".into(),
            },
            BlockOrigin::History(7),
        );
        let original_hash = history.content_hash(id);

        history.set_status(id, Status::Streaming);

        assert_eq!(history.content_hash(id), original_hash);
        assert_eq!(history.block_origin(id), Some(BlockOrigin::History(7)));

        history.rewrite(
            id,
            Block::Text {
                content: "after".into(),
            },
        );

        assert_ne!(history.content_hash(id), original_hash);
        assert_eq!(history.status(id), Some(Status::Streaming));
        assert_eq!(history.block_origin(id), Some(BlockOrigin::History(7)));
    }

    #[test]
    fn rewrite_with_same_hash_skips_generation_bump() {
        let mut history = BlockHistory::new();
        let id = history.push(Block::Text {
            content: "same".into(),
        });
        let g = history.generation();
        history.rewrite(
            id,
            Block::Text {
                content: "same".into(),
            },
        );
        assert_eq!(history.generation(), g);
    }

    #[test]
    fn rewrite_unknown_id_is_noop() {
        let mut history = BlockHistory::new();
        let g = history.generation();
        history.rewrite(
            BlockId(9999),
            Block::Text {
                content: "x".into(),
            },
        );
        assert_eq!(history.generation(), g);
    }

    #[test]
    fn clear_resets_everything() {
        let mut history = BlockHistory::new();
        let id = history.push(Block::Text {
            content: "a".into(),
        });
        history.push(Block::Text {
            content: "b".into(),
        });
        history.set_status(id, Status::Streaming);
        let g = history.generation();
        history.clear();
        assert!(history.is_empty());
        assert_eq!(history.next_id, 0);
        assert_eq!(history.block_metadata_retained_bytes(), 0);
        assert!(history.tool_states.is_empty());
        assert_ne!(history.generation(), g);
    }

    #[test]
    fn truncate_drops_tail_and_gcs_tool_states() {
        let mut history = BlockHistory::new();
        history.push(Block::Text {
            content: "a".into(),
        });
        let tool_id = history.push_with_state(
            Block::ToolCall {
                call_id: "tc1".into(),
                name: "x".into(),
                summary: "s".into(),
                args: HashMap::new().into(),
            },
            pending_state(),
        );
        history.push(Block::Text {
            content: "c".into(),
        });
        assert_eq!(history.len(), 3);
        assert!(history.tool_states.contains_key(&tool_id));
        // Truncate to before the ToolCall - the tool_state entry should be GC'd.
        history.truncate(1);
        assert_eq!(history.len(), 1);
        assert!(!history.tool_states.contains_key(&tool_id));
    }

    #[test]
    fn truncate_user_tail_invalidates_navigation() {
        let mut history = BlockHistory::new();
        history.push(Block::Text {
            content: "assistant".into(),
        });
        history.push(Block::User {
            text: "user".into(),
            image_labels: Vec::new(),
            command: false,
            sent_at_ms: None,
        });
        let navigation_generation = history.navigation_generation();

        history.truncate(1);

        assert_ne!(history.navigation_generation(), navigation_generation);
    }

    #[test]
    fn non_user_navigation_changes_invalidate_snapshots() {
        let mut history = BlockHistory::new();
        let first = history.push(Block::Text {
            content: "first assistant".into(),
        });
        let navigation_generation = history.navigation_generation();

        history.push(Block::Text {
            content: "second assistant".into(),
        });
        assert_ne!(history.navigation_generation(), navigation_generation);

        let navigation_generation = history.navigation_generation();
        history.rewrite(
            first,
            Block::Text {
                content: "rewritten assistant".into(),
            },
        );
        assert_ne!(history.navigation_generation(), navigation_generation);

        let navigation_generation = history.navigation_generation();
        history.rewrite(
            first,
            Block::Text {
                content: "rewritten assistant\ncontinued output".into(),
            },
        );
        assert_eq!(history.navigation_generation(), navigation_generation);

        let navigation_generation = history.navigation_generation();
        history.truncate(1);
        assert_ne!(history.navigation_generation(), navigation_generation);
    }

    #[test]
    fn remove_unoriginated_at_refuses_originated_blocks() {
        let mut history = BlockHistory::new();
        let a = history.push(Block::Text {
            content: "a".into(),
        });
        let b = history.push_with_origin(
            Block::Text {
                content: "b".into(),
            },
            BlockOrigin::History(1),
        );
        let g = history.generation();

        assert!(history.remove_unoriginated_at(1).is_none());
        assert_eq!(history.order, vec![a, b]);
        assert_eq!(history.generation(), g);
    }

    #[test]
    fn remove_unoriginated_at_gcs_tool_state() {
        let mut history = BlockHistory::new();
        let tool_id = history.push_with_state(
            Block::ToolCall {
                call_id: "tc1".into(),
                name: "x".into(),
                summary: "s".into(),
                args: HashMap::new().into(),
            },
            pending_state(),
        );
        assert!(history.tool_states.contains_key(&tool_id));

        assert!(matches!(
            history.remove_unoriginated_at(0),
            Some(Block::ToolCall { call_id, .. }) if call_id == "tc1"
        ));
        assert!(history.is_empty());
        assert!(!history.tool_states.contains_key(&tool_id));
    }

    #[test]
    fn truncate_past_end_is_noop() {
        let mut history = BlockHistory::new();
        history.push(Block::Text {
            content: "a".into(),
        });
        let g = history.generation();
        history.truncate(99);
        assert_eq!(history.len(), 1);
        assert_eq!(history.generation(), g);
    }

    #[test]
    fn block_gap_is_zero_for_first_block() {
        let mut history = BlockHistory::new();
        history.push(Block::Text {
            content: "a".into(),
        });
        assert_eq!(history.block_gap(0), 0);
    }

    #[test]
    fn block_gap_consults_gap_between_for_subsequent_blocks() {
        let mut history = BlockHistory::new();
        history.push(Block::Text {
            content: "a".into(),
        });
        history.push(Block::User {
            text: "q".into(),
            image_labels: vec![],
            command: false,
            sent_at_ms: None,
        });
        // Text -> User: 1
        assert_eq!(history.block_gap(1), 1);
    }

    #[test]
    fn block_gap_uses_blocks_without_materializing_blocks() {
        let blocks = vec![
            Block::Text {
                content: "# heading".into(),
            },
            Block::CodeLine {
                content: "let x = 1;".into(),
                lang: "rust".into(),
            },
            Block::Thinking {
                title: None,
                summary_titles: Vec::new(),
                kind: protocol::ReasoningKind::Raw,
                content: "plain thought".into(),
            },
            Block::Thinking {
                title: Some("New section".into()),
                summary_titles: vec!["New section".into()],
                kind: protocol::ReasoningKind::Summary,
                content: "body".into(),
            },
        ];
        let history = BlockHistory::from_block_records(
            blocks
                .into_iter()
                .enumerate()
                .map(|(index, block)| TranscriptBlockRecord {
                    block,
                    content_hash: 0,
                    origin: Some(BlockOrigin::History(index)),
                    tool_state: None,
                    tool_render_revision: 0,
                })
                .collect(),
        );

        assert_eq!(history.block_gap(1), 0);
        assert_eq!(history.block_gap(3), 1);
        assert_eq!(history.hydrated_block_count(), 0);
    }

    #[test]
    fn resolve_key_substitutes_content_and_sidecar_hashes() {
        let mut history = BlockHistory::new();
        let id = history.push(Block::Text {
            content: "x".into(),
        });
        let base = LayoutKey {
            width: 80,
            view_state: ViewState::Collapsed,
            content_hash: 0,
            sidecar_hash: 0,
        };
        let resolved = history.resolve_key(id, base);
        assert_eq!(resolved.view_state, ViewState::Collapsed);
        assert_eq!(resolved.content_hash, history.content_hash(id));
        assert_eq!(resolved.sidecar_hash, 0);
        assert_eq!(resolved.width, 80);
    }

    // ── gap_between coverage ──────────────────────────────────────────

    fn text(s: &str) -> Block {
        Block::Text { content: s.into() }
    }
    fn code(s: &str) -> Block {
        Block::CodeLine {
            content: s.into(),
            lang: "rust".into(),
        }
    }
    fn user(s: &str) -> Block {
        Block::User {
            text: s.into(),
            image_labels: vec![],
            command: false,
            sent_at_ms: None,
        }
    }
    fn thinking(s: &str) -> Block {
        Block::Thinking {
            title: None,
            summary_titles: Vec::new(),
            content: s.into(),
            kind: protocol::ReasoningKind::Raw,
        }
    }
    fn reasoning_summary(title: &str, content: &str) -> Block {
        Block::Thinking {
            title: Some(title.into()),
            summary_titles: vec![title.into()],
            content: content.into(),
            kind: protocol::ReasoningKind::Summary,
        }
    }
    fn tool(call_id: &str) -> Block {
        Block::ToolCall {
            call_id: call_id.into(),
            name: "x".into(),
            summary: "s".into(),
            args: HashMap::new().into(),
        }
    }
    fn exec() -> Block {
        Block::Exec {
            command: "ls".into(),
            output: "out".into(),
        }
    }
    fn compacted() -> Block {
        Block::Compacted {
            summary: "sum".into(),
        }
    }
    fn mode() -> Block {
        Block::Mode {
            text: "now in apply mode".into(),
            icon: "● ".into(),
            hl_group: "SmeltModeApply".into(),
        }
    }

    #[test]
    fn gap_between_codeline_to_codeline_is_zero() {
        assert_eq!(gap_between(&code("a"), &code("b")), 0);
    }

    #[test]
    fn gap_between_codeline_to_other_is_one() {
        assert_eq!(gap_between(&code("a"), &text("b")), 1);
    }

    #[test]
    fn gap_between_text_to_codeline_zero_after_heading() {
        assert_eq!(gap_between(&text("# heading"), &code("a")), 0);
        // Leading whitespace before # still counts as heading.
        assert_eq!(gap_between(&text("   ## sub"), &code("a")), 0);
    }

    #[test]
    fn gap_between_text_to_codeline_one_otherwise() {
        assert_eq!(gap_between(&text("plain"), &code("a")), 1);
    }

    #[test]
    fn gap_between_other_to_codeline_is_one() {
        assert_eq!(gap_between(&user("q"), &code("a")), 1);
    }

    #[test]
    fn gap_between_user_blocks_are_separated_by_one() {
        assert_eq!(gap_between(&user("a"), &text("b")), 1);
        assert_eq!(gap_between(&text("a"), &user("b")), 1);
    }

    #[test]
    fn gap_between_exec_blocks_are_separated_by_one() {
        assert_eq!(gap_between(&exec(), &text("a")), 1);
        assert_eq!(gap_between(&text("a"), &exec()), 1);
    }

    #[test]
    fn gap_between_thinking_blocks_only_separates_new_titles() {
        assert_eq!(gap_between(&thinking("a"), &thinking("b")), 0);
        assert_eq!(
            gap_between(
                &thinking("a"),
                &reasoning_summary("Assessing directory exclusions", "body")
            ),
            1
        );
        assert_eq!(
            gap_between(&reasoning_summary("First title", "body"), &thinking("body")),
            0
        );
    }

    #[test]
    fn gap_between_thinking_and_text_is_one() {
        assert_eq!(gap_between(&thinking("a"), &text("b")), 1);
        assert_eq!(gap_between(&text("a"), &thinking("b")), 1);
    }

    #[test]
    fn gap_between_tool_calls_separated_by_one() {
        assert_eq!(gap_between(&tool("a"), &tool("b")), 1);
        assert_eq!(gap_between(&text("a"), &tool("b")), 1);
        assert_eq!(gap_between(&tool("a"), &text("b")), 1);
    }

    #[test]
    fn gap_between_compacted_separates_both_sides() {
        assert_eq!(gap_between(&text("a"), &compacted()), 1);
        assert_eq!(gap_between(&compacted(), &text("a")), 1);
    }

    #[test]
    fn gap_between_text_after_heading_collapses() {
        assert_eq!(gap_between(&text("# heading"), &text("body")), 0);
    }

    #[test]
    fn gap_between_mode_blocks_are_separated_by_one() {
        assert_eq!(gap_between(&text("a"), &mode()), 1);
        assert_eq!(gap_between(&mode(), &text("b")), 1);
        assert_eq!(gap_between(&mode(), &tool("b")), 1);
    }
}
