use crossterm::event::Event;
use smelt_core::history::History;

use super::queue::{InputQueues, QueueStage, QueuedInput, QueuedRow};
use super::{PlaceholderOpts, PlaceholderState, PromptHeightState, PromptResizeDrag};
use crate::input::{Action, PromptCtx, PromptCtxRef, PromptState, SubmitEdit};
use crate::smelt_edit::{Clipboard, VimMode, WinId, Window};

/// Owns prompt editing sidecars, history, queues, placeholders, and height state.
///
/// The canonical prompt buffer and window remain in `Ui`; every operation that can
/// mutate them receives the relevant prompt context explicitly.
pub(crate) struct PromptRuntime {
    history: History,
    input: PromptState,
    queues: InputQueues,
    queue_revision: u64,
    last_published_text: String,
    height: PromptHeightState,
    placeholders: PlaceholderState,
}

/// Queue state held outside the prompt runtime only while turn cancellation runs.
/// Its private payload prevents callers from inspecting or reordering queued input.
#[must_use = "interrupted prompt queues must be restored"]
pub(super) struct InterruptedQueues {
    unsteer_count: usize,
    next: Option<QueuedInput>,
    remaining: InputQueues,
}

impl InterruptedQueues {
    pub(super) fn unsteer_count(&self) -> usize {
        self.unsteer_count
    }
}

impl PromptRuntime {
    pub(crate) fn new(
        history: History,
        input: PromptState,
        placeholders: PlaceholderState,
    ) -> Self {
        Self {
            history,
            input,
            queues: InputQueues::default(),
            queue_revision: 0,
            last_published_text: String::new(),
            height: PromptHeightState::default(),
            placeholders,
        }
    }

    pub(crate) fn set_cwd(&mut self, cwd: std::path::PathBuf) {
        self.input.set_cwd(cwd);
    }

    pub(crate) fn handle_event(
        &mut self,
        ctx: &mut PromptCtx<'_>,
        event: Event,
        use_history: bool,
        clipboard: &mut Clipboard,
        now: std::time::Instant,
    ) -> Action {
        let history = use_history.then_some(&mut self.history);
        self.input.handle_event(ctx, event, history, clipboard, now)
    }

    pub(crate) fn key_context(
        &self,
        ctx: PromptCtxRef<'_>,
        turn_input_active: bool,
    ) -> crate::keymap::KeyContext {
        self.input.key_context(ctx, turn_input_active)
    }

    pub(crate) fn apply_submit_edit(&mut self, ctx: &mut PromptCtx<'_>, edit: SubmitEdit) {
        self.input.apply_submit_edit(ctx, edit);
    }

    pub(crate) fn prepend_text(&mut self, ctx: &mut PromptCtx<'_>, prefix: String) {
        self.input.prepend_text(ctx, prefix);
    }

    pub(crate) fn replace_text(&mut self, ctx: &mut PromptCtx<'_>, text: String) {
        self.input.replace_text(ctx, text);
    }

    pub(crate) fn clear_with_undo(&mut self, ctx: &mut PromptCtx<'_>) {
        self.input.clear_with_undo(ctx);
    }

    pub(crate) fn clear_for_session_change(&mut self, ctx: &mut PromptCtx<'_>) {
        self.input.clear(ctx);
        self.input.store.lock().unwrap().clear();
    }

    pub(crate) fn restore_stash(&mut self, ctx: &mut PromptCtx<'_>) {
        self.input.restore_stash(ctx);
    }

    pub(crate) fn restore_from_rewind(
        &mut self,
        ctx: &mut PromptCtx<'_>,
        text: String,
        images: Vec<(String, String)>,
    ) {
        self.input.restore_from_rewind(ctx, text, images);
    }

    pub(crate) fn has_stash(&self) -> bool {
        self.input.stash.is_some()
    }

    pub(crate) fn skip_shell_escape(&self) -> bool {
        self.input.skip_shell_escape()
    }

    pub(crate) fn vim_enabled(&self, win: &Window) -> bool {
        self.input.vim_enabled(win)
    }

    pub(crate) fn vim_mode(&self, win: &Window) -> VimMode {
        self.input.vim_mode(win)
    }

    pub(crate) fn set_vim_enabled(&mut self, win: &mut Window, enabled: bool) {
        self.input.set_vim_enabled(win, enabled);
    }

    pub(crate) fn set_vim_mode(&mut self, win: &mut Window, mode: VimMode) {
        self.input.set_vim_mode(win, mode);
    }

    pub(crate) fn sync_display_coords(&mut self, ctx: &mut PromptCtx<'_>, viewport_rows: u16) {
        self.input.sync_display_coords(ctx, viewport_rows);
    }

    pub(crate) fn render_input(&self) -> &PromptState {
        &self.input
    }

    pub(crate) fn attachment_store(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<smelt_core::attachment::AttachmentStore>> {
        std::sync::Arc::clone(&self.input.store)
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn insert_image_for_harness(
        &mut self,
        ctx: &mut PromptCtx<'_>,
        label: String,
        data_url: String,
    ) {
        self.input.insert_image(ctx, label, data_url);
    }

    pub(crate) fn push_history(&mut self, entry: String) {
        self.history.push(entry);
    }

    pub(crate) fn history_entries(&self) -> impl ExactSizeIterator<Item = &str> + '_ {
        self.history.entries()
    }

    pub(crate) fn try_queue_turn(&mut self, queued: QueuedInput) -> bool {
        let queued = self.queues.try_push_turn(queued);
        if queued {
            self.bump_queue_revision();
        }
        queued
    }

    pub(crate) fn try_queue_request(&mut self, queued: QueuedInput) -> bool {
        let queued = self.queues.try_push_request(queued);
        if queued {
            self.bump_queue_revision();
        }
        queued
    }

    pub(crate) fn queue_front(&mut self, stage: QueueStage, queued: QueuedInput) {
        self.queues.push_front(stage, queued);
        self.bump_queue_revision();
    }

    pub(crate) fn pop_next_for_turn(&mut self) -> Option<(QueueStage, QueuedInput)> {
        let queued = self.queues.pop_next_for_turn_with_stage();
        if queued.is_some() {
            self.bump_queue_revision();
        }
        queued
    }

    pub(crate) fn promote_turn_to_request(&mut self) -> Option<&QueuedInput> {
        if self.queues.front_turn_is_request() {
            self.bump_queue_revision();
        }
        self.queues.promote_turn_to_request()
    }

    pub(super) fn suspend_for_interrupt(&mut self) -> InterruptedQueues {
        if !self.queues.is_empty() {
            self.bump_queue_revision();
        }
        let (unsteer_count, next, remaining) = self.queues.take_for_interrupt();
        InterruptedQueues {
            unsteer_count,
            next,
            remaining,
        }
    }

    pub(super) fn restore_after_interrupt(
        &mut self,
        interrupted: InterruptedQueues,
    ) -> Option<QueuedInput> {
        let queue_changed = !self.queues.is_empty() || !interrupted.remaining.is_empty();
        self.queues = interrupted.remaining;
        if queue_changed {
            self.bump_queue_revision();
        }
        interrupted.next
    }

    pub(crate) fn drain_for_prompt(&mut self) -> (usize, Vec<QueuedInput>) {
        if !self.queues.is_empty() {
            self.bump_queue_revision();
        }
        self.queues.drain_for_prompt()
    }

    pub(crate) fn acknowledge_requests(&mut self, count: usize) -> Vec<QueuedInput> {
        let acknowledged = self.queues.drain_request_ack(count);
        if !acknowledged.is_empty() {
            self.bump_queue_revision();
        }
        acknowledged
    }

    pub(crate) fn clear_queue(&mut self) {
        if !self.queues.is_empty() {
            self.queues.clear();
            self.bump_queue_revision();
        }
    }

    pub(crate) fn queue_revision(&self) -> u64 {
        self.queue_revision
    }

    fn bump_queue_revision(&mut self) {
        self.queue_revision = self.queue_revision.wrapping_add(1);
    }

    pub(crate) fn queue_is_empty(&self) -> bool {
        self.queues.is_empty()
    }

    pub(crate) fn queued_request_inputs(
        &self,
    ) -> impl Iterator<Item = protocol::StartTurnInput> + '_ {
        self.queues.request_inputs()
    }

    pub(crate) fn has_queued_request(&self) -> bool {
        self.queues.has_request()
    }

    pub(crate) fn front_turn_can_be_request(&self) -> bool {
        self.queues.front_turn_is_request()
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn queued_len(&self) -> usize {
        self.queues.len()
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn queued_request_len(&self) -> usize {
        self.queues.request_len()
    }

    pub(crate) fn queued_rows(&self) -> Vec<QueuedRow> {
        self.queues.display_rows()
    }

    pub(crate) fn queued_texts(&self) -> Vec<String> {
        self.queues.display_texts()
    }

    #[cfg(test)]
    pub(crate) fn queued_kinds(&self) -> Vec<String> {
        self.queues.display_kinds()
    }

    pub(crate) fn publish_text_if_changed(&mut self, current: &str) -> bool {
        if self.last_published_text == current {
            return false;
        }
        current.clone_into(&mut self.last_published_text);
        true
    }

    pub(crate) fn resolve_height(&mut self, wrapped_rows: u16, terminal_height: u16) -> u16 {
        self.height.resolve_rows(wrapped_rows, terminal_height)
    }

    pub(crate) fn active_resize_chrome(&self) -> &'static str {
        self.height.active_chrome()
    }

    pub(crate) fn resize_drag(&self) -> Option<PromptResizeDrag> {
        self.height.drag()
    }

    pub(crate) fn resize_drag_to(&mut self, row: u16, terminal_height: u16) {
        self.height.resize_drag_to(row, terminal_height);
    }

    pub(crate) fn finish_resize_drag(&mut self) {
        self.height.finish_drag();
    }

    pub(crate) fn register_resize_click(
        &mut self,
        row: u16,
        column: u16,
        now: std::time::Instant,
    ) -> bool {
        self.height.register_click(row, column, now)
    }

    pub(crate) fn start_resize_drag(&mut self, chrome: &'static str, row: u16) {
        self.height.start_drag(chrome, row);
    }

    #[cfg(test)]
    pub(crate) fn set_resize_drag_for_harness(&mut self, drag: Option<PromptResizeDrag>) {
        self.height.set_drag(drag);
    }

    #[cfg(test)]
    pub(crate) fn prompt_rows_for_harness(&self) -> u16 {
        self.height.rows()
    }

    #[cfg(test)]
    pub(crate) fn manual_prompt_rows_for_harness(&self) -> Option<u16> {
        self.height.manual_rows()
    }

    #[cfg(test)]
    pub(crate) fn set_manual_prompt_rows_for_harness(&mut self, rows: Option<u16>) {
        self.height.set_manual_rows(rows);
    }

    pub(crate) fn sync_prompt_placeholder_display(&mut self) -> bool {
        self.placeholders.sync_prompt_display()
    }

    pub(crate) fn placeholder_text(&self, win: WinId) -> Option<&str> {
        self.placeholders.text(win)
    }

    pub(crate) fn set_placeholder_text(&mut self, win: WinId, text: String) {
        self.placeholders.set_text(win, text);
    }

    pub(crate) fn set_placeholder_options(&mut self, win: WinId, options: PlaceholderOpts) {
        self.placeholders.set_options(win, options);
    }

    pub(crate) fn placeholder_options(&self, win: WinId) -> Option<&PlaceholderOpts> {
        self.placeholders.options(win)
    }

    pub(crate) fn clear_placeholder(&mut self, win: WinId) {
        self.placeholders.clear(win);
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn placeholder_option_windows(&self) -> impl Iterator<Item = WinId> + '_ {
        self.placeholders.option_windows()
    }

    #[cfg(any(test, feature = "harness"))]
    pub(crate) fn has_placeholder_options(&self, win: WinId) -> bool {
        self.placeholders.contains_options(win)
    }

    pub(crate) fn fork_lua_placeholders(&self, ui: &crate::smelt_edit::Ui) -> PlaceholderState {
        self.placeholders.fork_for_lua_generation(ui)
    }

    pub(crate) fn swap_lua_placeholders(
        &mut self,
        placeholders: PlaceholderState,
    ) -> PlaceholderState {
        std::mem::replace(&mut self.placeholders, placeholders)
    }
}
