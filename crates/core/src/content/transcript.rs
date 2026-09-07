//! `Transcript` owns the block history. Streaming parsing lives in `StreamParser`; display projection in `tui`.

use crate::transcript_model::{Block, BlockHistory, BlockId, BlockOrigin, ToolState};

pub struct Transcript {
    pub history: BlockHistory,
}

impl Default for Transcript {
    fn default() -> Self {
        Self::new()
    }
}

impl Transcript {
    pub fn new() -> Self {
        Self {
            history: BlockHistory::new(),
        }
    }

    pub fn from_block_history(history: BlockHistory) -> Self {
        Self { history }
    }

    pub fn from_block_records(
        records: Vec<crate::transcript_model::TranscriptBlockRecord>,
    ) -> Self {
        Self {
            history: BlockHistory::from_block_records(records),
        }
    }

    pub fn from_block_records_with_ids(
        records: Vec<crate::transcript_model::TranscriptBlockRecordWithId>,
    ) -> Self {
        Self {
            history: BlockHistory::from_block_records_with_ids(records),
        }
    }

    // ── Accessors ─────────────────────────────────────────────────────

    pub fn block(&self, id: BlockId) -> Option<&Block> {
        self.history.block(id)
    }

    pub fn drain_finished_blocks(&mut self) -> Vec<BlockId> {
        self.history.drain_finished_blocks()
    }

    // ── Mutations ─────────────────────────────────────────────────────

    fn normalize_block(block: Block) -> Option<Block> {
        Some(match block {
            Block::Text { content } => {
                let content = content.into_trimmed();
                if content.is_empty() {
                    return None;
                }
                Block::Text { content }
            }
            Block::Thinking {
                title,
                summary_titles,
                content,
                kind,
            } => {
                let summary_titles: Vec<_> = summary_titles
                    .into_iter()
                    .map(|title| smelt_buffer::text::trim_whitespace(&title).to_string())
                    .filter(|title| !title.is_empty())
                    .collect();
                let title = summary_titles
                    .last()
                    .cloned()
                    .or_else(|| {
                        title.map(|title| smelt_buffer::text::trim_whitespace(&title).to_string())
                    })
                    .filter(|title| !title.is_empty());
                let content = content.into_trimmed();
                if title.is_none() && content.is_empty() {
                    return None;
                }
                Block::Thinking {
                    title,
                    summary_titles,
                    content,
                    kind,
                }
            }
            Block::Compacted { summary } => {
                let t = smelt_buffer::text::trim_whitespace(&summary);
                if t.is_empty() {
                    return None;
                }
                Block::Compacted {
                    summary: t.to_string(),
                }
            }
            Block::CompactionPreview { summary } => Block::CompactionPreview {
                summary: smelt_buffer::text::trim_whitespace(&summary).to_string(),
            },
            other => other,
        })
    }

    pub fn push(&mut self, block: Block) {
        let Some(block) = Self::normalize_block(block) else {
            return;
        };
        self.history.push(block);
    }

    pub fn push_compaction_preview(&mut self, summary: String) -> Option<BlockId> {
        let block = Self::normalize_block(Block::CompactionPreview { summary })?;
        Some(self.history.push(block))
    }

    pub fn rewrite_compaction_preview(&mut self, id: BlockId, summary: String) -> bool {
        let Some(block) = Self::normalize_block(Block::CompactionPreview { summary }) else {
            return false;
        };
        self.history.rewrite(id, block);
        true
    }

    pub fn remove_compaction_preview(&mut self, id: BlockId) {
        self.history.remove_block(id);
    }

    pub fn push_with_origin(&mut self, block: Block, origin: BlockOrigin) {
        let Some(block) = Self::normalize_block(block) else {
            return;
        };
        self.history.push_with_origin(block, origin);
    }

    pub fn push_hydrated_block_with_origin(&mut self, block: Block, origin: BlockOrigin) {
        let Some(block) = Self::normalize_block(block) else {
            return;
        };
        self.history.push_hydrated_block_with_origin(block, origin);
    }

    pub fn insert_checkpoint_marker(&mut self, history_index: usize, block: Block) {
        let Some(block) = Self::normalize_block(block) else {
            return;
        };
        self.history.insert_checkpoint_marker(history_index, block);
    }

    pub fn insert_checkpoint_marker_at(
        &mut self,
        block_index: usize,
        history_index: usize,
        block: Block,
    ) {
        let Some(block) = Self::normalize_block(block) else {
            return;
        };
        self.history
            .insert_checkpoint_marker_at(block_index, history_index, block);
    }

    pub fn remove_unoriginated_at(&mut self, block_idx: usize) -> Option<Block> {
        self.history.remove_unoriginated_at(block_idx)
    }

    pub fn push_tool_call(&mut self, block: Block, state: ToolState) {
        debug_assert!(matches!(block, Block::ToolCall { .. }));
        self.history.push_with_state(block, state);
    }

    pub fn push_tool_call_with_origin(
        &mut self,
        block: Block,
        state: ToolState,
        origin: BlockOrigin,
    ) {
        debug_assert!(matches!(block, Block::ToolCall { .. }));
        self.history
            .push_with_state_and_origin(block, state, origin);
    }

    pub fn push_hydrated_tool_block_with_origin(
        &mut self,
        block: Block,
        state: ToolState,
        origin: BlockOrigin,
    ) {
        let Some(block) = Self::normalize_block(block) else {
            return;
        };
        if block.tool_call_id().is_none() {
            return;
        }
        self.history
            .push_hydrated_block_with_state_and_origin(block, state, origin);
    }

    pub fn truncate_to(&mut self, block_idx: usize) {
        self.history.truncate(block_idx);
    }

    pub fn last_user_block_index(&self) -> Option<usize> {
        let _perf = smelt_perf::perf::begin("transcript:last_user_block_index");
        let mut blocks_scanned = 0u64;
        let index = self
            .history
            .order
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, id)| {
                blocks_scanned = blocks_scanned.saturating_add(1);
                (self.history.block_kind(*id) == Some("user")).then_some(index)
            });
        smelt_perf::perf::record_value(
            "transcript:last_user_block_index:blocks_scanned",
            blocks_scanned,
        );
        index
    }

    pub fn user_turns(&self) -> Vec<(usize, String)> {
        let _perf = smelt_perf::perf::begin("transcript:user_turns");
        smelt_perf::perf::record_value(
            "transcript:user_turns:blocks_scanned",
            self.history.order.len() as u64,
        );
        let mut text_bytes = 0u64;
        let turns = self
            .history
            .order
            .iter()
            .enumerate()
            .filter_map(|(i, id)| {
                if self.history.block_kind(*id) != Some("user") {
                    return None;
                }
                let text = match self.history.block(*id) {
                    Some(Block::User { text, .. }) => text.clone(),
                    _ => self.history.first_line(*id).unwrap_or_default(),
                };
                text_bytes = text_bytes.saturating_add(text.len() as u64);
                Some((i, text))
            })
            .collect::<Vec<_>>();
        smelt_perf::perf::record_value("transcript:user_turns:users_cloned", turns.len() as u64);
        smelt_perf::perf::record_value("transcript:user_turns:text_bytes_cloned", text_bytes);
        turns
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript_model::{ToolStatus, TranscriptBlockRecord};

    fn tool_state() -> ToolState {
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

    fn block_at(history: &BlockHistory, index: usize) -> &Block {
        history
            .materialized_block_at(index)
            .expect("materialized test block")
    }

    #[test]
    fn new_creates_empty_transcript() {
        let t = Transcript::new();
        assert!(t.history.is_empty());
    }

    #[test]
    fn default_matches_new() {
        let t = Transcript::default();
        assert!(t.history.is_empty());
    }

    #[test]
    fn push_text_block_records_it() {
        let mut t = Transcript::new();
        t.push(Block::Text {
            content: "hello".into(),
        });
        assert_eq!(t.history.len(), 1);
    }

    #[test]
    fn push_trims_text_content() {
        let mut t = Transcript::new();
        t.push(Block::Text {
            content: "  hello  ".into(),
        });
        match block_at(&t.history, 0) {
            Block::Text { content } => assert_eq!(content, "hello"),
            _ => panic!("expected Text"),
        }
    }

    #[test]
    fn push_trims_only_complete_whitespace_graphemes() {
        let mut t = Transcript::new();
        t.push(Block::Text {
            content: " \u{301}hello\u{600} ".into(),
        });
        match block_at(&t.history, 0) {
            Block::Text { content } => assert_eq!(content, " \u{301}hello\u{600} "),
            _ => panic!("expected Text"),
        }
    }

    #[test]
    fn push_drops_blank_text_block() {
        let mut t = Transcript::new();
        t.push(Block::Text {
            content: "   ".into(),
        });
        assert!(t.history.is_empty());
    }

    #[test]
    fn push_drops_blank_thinking_block() {
        let mut t = Transcript::new();
        t.push(Block::Thinking {
            title: None,
            summary_titles: Vec::new(),
            kind: protocol::ReasoningKind::Raw,
            content: "\n\n".into(),
        });
        assert!(t.history.is_empty());
    }

    #[test]
    fn push_keeps_title_only_thinking_block() {
        let mut t = Transcript::new();
        t.push(Block::Thinking {
            title: Some("Checking files".into()),
            summary_titles: vec!["Checking files".into()],
            kind: protocol::ReasoningKind::Summary,
            content: String::new().into(),
        });
        assert_eq!(
            block_at(&t.history, 0),
            &Block::Thinking {
                title: Some("Checking files".into()),
                summary_titles: vec!["Checking files".into()],
                kind: protocol::ReasoningKind::Summary,
                content: String::new().into(),
            }
        );
    }

    #[test]
    fn push_drops_blank_compacted_block() {
        let mut t = Transcript::new();
        t.push(Block::Compacted { summary: "".into() });
        assert!(t.history.is_empty());
    }

    #[test]
    fn push_keeps_blank_compaction_preview() {
        let mut t = Transcript::new();
        t.push(Block::CompactionPreview { summary: "".into() });
        assert_eq!(
            block_at(&t.history, 0),
            &Block::CompactionPreview { summary: "".into() }
        );
    }

    #[test]
    fn push_user_block_keeps_raw_text_untrimmed() {
        // Transcript normalization intentionally preserves user-authored spacing.
        let mut t = Transcript::new();
        t.push(Block::User {
            text: "  question  ".into(),
            image_labels: vec![],
            command: false,
            sent_at_ms: None,
        });
        match block_at(&t.history, 0) {
            Block::User { text, .. } => assert_eq!(text, "  question  "),
            _ => panic!("expected User"),
        }
    }

    #[test]
    fn push_tool_call_registers_state_and_block() {
        let mut t = Transcript::new();
        t.push_tool_call(
            Block::ToolCall {
                call_id: "id-1".into(),
                name: "exec".into(),
                summary: "ls".into(),
                args: Default::default(),
            },
            tool_state(),
        );
        assert_eq!(t.history.len(), 1);
        assert!(t.history.tool_state(t.history.order[0]).is_some());
    }

    #[test]
    fn block_returns_some_for_known_id_and_none_otherwise() {
        let mut t = Transcript::new();
        t.push(Block::Text {
            content: "x".into(),
        });
        let id = t.history.order[0];
        assert!(t.block(id).is_some());
        let missing = crate::transcript_model::BlockId::new(9999);
        assert!(t.block(missing).is_none());
    }

    #[test]
    fn truncate_to_drops_trailing_blocks() {
        let mut t = Transcript::new();
        t.push(Block::Text {
            content: "a".into(),
        });
        t.push(Block::Text {
            content: "b".into(),
        });
        t.push(Block::Text {
            content: "c".into(),
        });
        t.truncate_to(1);
        assert_eq!(t.history.len(), 1);
    }

    #[test]
    fn user_turns_returns_only_user_blocks_with_index() {
        let mut t = Transcript::new();
        t.push(Block::Text {
            content: "ignored".into(),
        });
        t.push(Block::User {
            text: "first".into(),
            image_labels: vec![],
            command: false,
            sent_at_ms: None,
        });
        t.push(Block::Text {
            content: "between".into(),
        });
        t.push(Block::User {
            text: "second".into(),
            image_labels: vec![],
            command: false,
            sent_at_ms: None,
        });
        let turns = t.user_turns();
        assert_eq!(
            turns,
            vec![(1usize, "first".into()), (3usize, "second".into())]
        );
    }

    #[test]
    fn user_turn_navigation_uses_compact_stored_metadata() {
        let mut t = Transcript::new();
        t.history = BlockHistory::from_block_records(vec![
            TranscriptBlockRecord {
                block: Block::User {
                    text: "first line\nfull stored prompt".into(),
                    image_labels: vec![],
                    command: false,
                    sent_at_ms: None,
                },
                content_hash: 0,
                origin: Some(BlockOrigin::History(0)),
                tool_state: None,
                tool_render_revision: 0,
            },
            TranscriptBlockRecord {
                block: Block::Text {
                    content: "assistant".into(),
                },
                content_hash: 0,
                origin: Some(BlockOrigin::History(1)),
                tool_state: None,
                tool_render_revision: 0,
            },
        ]);

        assert_eq!(t.last_user_block_index(), Some(0));
        assert_eq!(t.user_turns(), vec![(0, "first line".to_string())]);
        assert!(t.history.block(t.history.order[0]).is_none());
    }

    #[test]
    fn last_user_block_index_searches_from_the_tail_without_cloning_text() {
        let mut t = Transcript::new();
        t.push(Block::User {
            text: "first".into(),
            image_labels: vec![],
            command: false,
            sent_at_ms: None,
        });
        t.push(Block::Text {
            content: "assistant".into(),
        });
        t.push(Block::User {
            text: "second".into(),
            image_labels: vec![],
            command: false,
            sent_at_ms: None,
        });
        t.push(Block::Text {
            content: "tail".into(),
        });

        assert_eq!(t.last_user_block_index(), Some(2));

        t.truncate_to(2);
        assert_eq!(t.last_user_block_index(), Some(0));
    }

    #[test]
    fn drain_finished_blocks_returns_empty_when_none_streaming() {
        let mut t = Transcript::new();
        t.push(Block::Text {
            content: "x".into(),
        });
        let drained = t.drain_finished_blocks();
        assert!(drained.is_empty());
    }
}
