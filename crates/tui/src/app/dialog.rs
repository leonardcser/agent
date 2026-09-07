use std::time::Duration;

use super::{NotificationLifetime, SuspendedNotification, SuspendedNotificationLifetime, TuiApp};

impl TuiApp {
    pub(crate) fn open_docked_dialog(
        &mut self,
        layout: crate::lua::api::overlay_layout::LayoutNode,
        height: crate::smelt_edit::Constraint,
        min_height: Option<crate::smelt_edit::Constraint>,
        max_height: Option<crate::smelt_edit::Constraint>,
        blocks_agent: bool,
        resizable: bool,
    ) -> Result<(crate::smelt_edit::ContainerId, crate::smelt_edit::ModalId), String> {
        let mut leaves = Vec::new();
        let (_, tree) = crate::lua::ui_ops::build_layout_tree(self, &layout, &mut leaves)?;
        crate::lua::ui_ops::validate_split_mounts(&tree, self.ui.layout_trees())?;
        if leaves.is_empty() {
            return Err("dialog requires at least one window leaf".into());
        }

        let resize = if resizable {
            crate::smelt_edit::ResizeConfig {
                top: true,
                right: false,
                bottom: false,
                left: false,
                corners: false,
            }
        } else {
            crate::smelt_edit::ResizeConfig::none()
        };
        let first_dialog = self.ui.active_docked_surface().is_none();
        let (id, modal) = self.ui.docked_surface_open(
            tree,
            leaves,
            crate::smelt_edit::DockedSurfaceConfig {
                height,
                min_height,
                max_height,
                resize,
                fit_reserved_rows: crate::app::DOCKED_DIALOG_TRANSCRIPT_ROWS.saturating_add(1),
                blocks_agent,
            },
        );
        if first_dialog {
            self.suspend_notification_for_dialog();
        }
        self.refresh_main_layout();
        self.ui.focus_active_modal();
        Ok((id, modal))
    }

    pub(crate) fn close_docked_dialog(&mut self, id: crate::smelt_edit::ContainerId) {
        let Some(dialog) = self.ui.docked_surface_remove(id) else {
            return;
        };
        let leaves = self
            .ui
            .modal_leaves(dialog.modal())
            .unwrap_or_default()
            .to_vec();
        for callback in self.ui.modal_close(dialog.modal()) {
            self.lua.remove_callback(callback);
        }

        // Remove the dialog subtree before destroying its windows, so the root
        // composer never observes dangling leaves.
        self.refresh_main_layout();
        for leaf in leaves {
            self.prompt.clear_placeholder(leaf);
            for callback in self.ui.win_close(leaf) {
                self.lua.remove_callback(callback);
            }
        }
        if !self.ui.focus_active_modal() {
            match self.app_focus {
                crate::app::AppFocus::Prompt => {
                    self.ui.set_focus(crate::app::PROMPT_WIN);
                }
                crate::app::AppFocus::Content => {
                    self.ui.set_focus(crate::app::TRANSCRIPT_WIN);
                }
            }
        }
        if self.ui.active_docked_surface().is_none() {
            self.resume_notification_after_dialog();
        }
    }

    pub(crate) fn toggle_docked_dialog_expanded(&mut self, id: crate::smelt_edit::ContainerId) {
        if self.ui.docked_surface_toggle_expanded(id) {
            self.refresh_main_layout();
        }
    }

    pub(crate) fn active_docked_dialog(&self) -> Option<crate::smelt_edit::ContainerId> {
        self.ui.active_docked_surface()
    }

    pub(crate) fn has_docked_dialog(&self) -> bool {
        self.ui.active_docked_surface().is_some()
    }

    /// Build the canonical transcript-dialog stage placed by the root composer.
    pub(crate) fn docked_dialog_stage_layout(
        &mut self,
        id: crate::smelt_edit::ContainerId,
    ) -> Option<crate::smelt_edit::LayoutTree> {
        let transcript_height = if self.ui.docked_surface(id)?.expanded() {
            crate::smelt_edit::Constraint::Length(crate::app::DOCKED_DIALOG_TRANSCRIPT_ROWS)
        } else {
            crate::smelt_edit::Constraint::Fill
        };
        let dialog_height = self.ui.docked_surface_height(id)?;
        let dialog = self.ui.docked_surface_layout(id)?;
        Some(crate::smelt_edit::LayoutTree::vbox(vec![
            (
                transcript_height,
                crate::smelt_edit::LayoutTree::leaf(crate::app::TRANSCRIPT_WIN),
            ),
            (dialog_height, dialog),
        ]))
    }

    pub(crate) fn close_active_modal(&mut self) -> bool {
        let Some(owner) = self.ui.active_modal_owner() else {
            return false;
        };
        match owner {
            crate::smelt_edit::ModalOwner::Docked(dialog) => self.close_docked_dialog(dialog),
            crate::smelt_edit::ModalOwner::Overlay(overlay) => self.close_overlay(overlay),
        }
        true
    }

    fn suspend_notification_for_dialog(&mut self) {
        let Some(notification) = self.overlays.take_notification() else {
            return;
        };
        let now = self.core.clock.instant_now();
        let lifetime = match notification.lifetime {
            NotificationLifetime::Timed { expires_at } => {
                SuspendedNotificationLifetime::Timed(expires_at.saturating_duration_since(now))
            }
            NotificationLifetime::Sticky => SuspendedNotificationLifetime::Sticky,
        };
        self.close_overlay_leaf(notification.win);
        self.overlays
            .install_suspended_notification(SuspendedNotification {
                lifetime,
                kind: notification.kind,
                summary: notification.summary,
                scope: notification.scope,
            });
    }

    fn resume_notification_after_dialog(&mut self) {
        let Some(notification) = self.overlays.take_suspended_notification() else {
            return;
        };
        let lifetime = match notification.lifetime {
            SuspendedNotificationLifetime::Timed(remaining) => {
                if remaining == Duration::ZERO {
                    return;
                }
                NotificationLifetime::Timed {
                    expires_at: self.core.clock.instant_now() + remaining,
                }
            }
            SuspendedNotificationLifetime::Sticky => NotificationLifetime::Sticky,
        };
        self.open_notification(
            notification.kind,
            &notification.summary,
            lifetime,
            notification.scope,
        );
    }
}
