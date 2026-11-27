//! Home's Suggested row and the suggestions list it shows in the preview pane.

use ratatui::layout::Rect;
use ratatui::Frame;

use super::*;
use crate::tui::dialogs::{PaletteAction, PaletteCommand, PaletteGroup};
use crate::tui::styles::Theme;
use crate::tui::suggestions::SuggestionsView;

impl HomeView {
    /// Appends the Suggested row when the daemon's "Show in the sidebar" pref
    /// is on. Absent with nothing to list, unless a provider failed, so its
    /// error is not hidden with the row.
    pub(super) fn append_suggested(&self, items: &mut Vec<Item>) {
        let feed = &self.suggestions_feed;
        let total = feed.keys().len();
        let failed = feed.has_provider_failure();
        if feed.shown_on_home() && (total > 0 || failed) {
            items.push(Item::Suggested { total, failed });
        }
    }

    pub(super) fn on_suggested_row(&self) -> bool {
        matches!(
            self.flat_items.get(self.cursor),
            Some(Item::Suggested { .. })
        )
    }

    pub(super) fn suggested_row(&self) -> Option<usize> {
        self.flat_items
            .iter()
            .position(|item| matches!(item, Item::Suggested { .. }))
    }

    /// Creates the list the preview pane shows, keeping one that exists so its
    /// selection survives the cursor leaving the row.
    pub(super) fn ensure_suggestions_view(&mut self) {
        if self.suggestions_view.is_none() {
            let mut view = SuggestionsView::new(&self.suggestions_feed);
            // The local config stands in until a load reads the daemon's setting.
            view.pretrusted = self.suggestions_feed.daemon_pre_trust().unwrap_or_else(|| {
                crate::session::config::Config::load()
                    .is_ok_and(|c| c.session.pre_trust_agent_folders)
            });
            self.suggestions_view = Some(view);
        }
        if let Some(view) = &mut self.suggestions_view {
            view.strict = self.strict_hotkeys;
        }
    }

    /// Enter or Right on the Suggested row moves the keys into its pane.
    /// False when the cursor is elsewhere.
    pub(super) fn focus_suggestions_pane(&mut self) -> bool {
        if !self.on_suggested_row() {
            return false;
        }
        self.focus_suggestions();
        true
    }

    /// Shows the suggestions list in the preview pane with the keys in it,
    /// with the cursor on the Suggested row when the sidebar lists it.
    pub(super) fn focus_suggestions(&mut self) {
        if let Some(state) = self.live_send.clone() {
            self.exit_live_send_and_restore_sizing(&state);
        }
        if let Some(row) = self.suggested_row() {
            self.cursor = row;
            self.update_selected();
        }
        self.ensure_suggestions_view();
        self.system_health_open = false;
        // A Trash transcript reader would otherwise take the keys first.
        self.retained_focus = None;
        self.suggestions_focused = true;
    }

    /// A click on the suggestions pane moves the keys into it; true when it
    /// was on the pane.
    pub fn handle_suggestions_pane_click(&mut self, col: u16, row: u16) -> bool {
        if self.has_non_live_send_overlay()
            || !self.suggestions_pane_shown()
            || !self.hit_preview(col, row)
        {
            return false;
        }
        self.focus_suggestions();
        true
    }

    /// The suggestions pane has the keys, so app-level keys such as `q` reach it.
    pub(in crate::tui) fn suggestions_pane_focused(&self) -> bool {
        self.suggestions_focused
    }

    /// The preview pane shows the suggestions list.
    pub(super) fn suggestions_pane_shown(&self) -> bool {
        self.suggestions_view.is_some() && (self.suggestions_focused || self.on_suggested_row())
    }

    /// Selects the Session a suggestion was started as, expanding the Archived
    /// or Trash section and the project or org subgroup it sits in. The reason
    /// when it cannot be shown (including another profile's).
    pub(super) fn try_open_linked_session(&mut self, id: &str) -> Result<(), String> {
        let Some(row) = self.session_feed.applied_session(id) else {
            return Err("That session no longer exists".into());
        };
        let (profile, trashed, archived) = (
            row.profile.clone(),
            row.trashed_at.is_some(),
            row.archived_at.is_some(),
        );
        if self.active_profile.as_deref().is_some_and(|p| p != profile) {
            return Err(format!(
                "That session is in profile {profile}. Switch to it to open it"
            ));
        }
        let sections = (
            self.trashed_section_collapsed,
            self.archived_section_collapsed,
        );
        let subgroups = (
            self.project_group_collapsed.clone(),
            self.org_group_collapsed.clone(),
        );
        if trashed {
            self.trashed_section_collapsed = false;
        } else if archived {
            self.archived_section_collapsed = false;
            self.expand_archived_subgroup(id);
        }
        self.rebuild_flat_items();
        self.jump_to_session_id(id);
        if self.selected_session.as_deref() == Some(id) {
            return Ok(());
        }
        (
            self.trashed_section_collapsed,
            self.archived_section_collapsed,
        ) = sections;
        (self.project_group_collapsed, self.org_group_collapsed) = subgroups;
        self.rebuild_flat_items();
        Err("That session could not be shown".into())
    }

    /// Expands the archived Project or Org subgroup holding session `id`.
    fn expand_archived_subgroup(&mut self, id: &str) {
        let Some(inst) = self.get_instance(id) else {
            return;
        };
        match self.group_by {
            GroupByMode::Project => {
                let path =
                    crate::session::archived_project_sub_path(&super::project_group_key(inst));
                self.project_group_collapsed.insert(path, false);
            }
            GroupByMode::Org => {
                let path = crate::session::archived_project_sub_path(&self.org_group_key(inst));
                self.org_group_collapsed.insert(path, false);
            }
            _ => {}
        }
    }

    /// "Open in <provider>" for a suggestion, when its `source_url` is openable.
    pub(super) fn open_source_command(&self, key: &(String, String)) -> Option<PaletteCommand> {
        let (provider, url) = self.suggestions_feed.source_link(key)?;
        Some(PaletteCommand {
            id: "open-suggestion-source",
            title: format!("Open in {provider}"),
            group: PaletteGroup::Actions,
            keywords: vec!["open", "link", "source", "browser"],
            hotkey: String::new(),
            payload: PaletteAction::OpenLink(url?),
        })
    }

    /// Paints the suggestions list into the preview pane. False when the pane
    /// shows something else.
    pub(super) fn render_suggestions_pane(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        theme: &Theme,
    ) -> bool {
        if !self.suggestions_pane_shown() {
            return false;
        }
        let Some(view) = &self.suggestions_view else {
            return false;
        };
        let activate_key = match super::bindings::label(
            super::bindings::ActionId::Activate,
            self.strict_hotkeys,
        ) {
            label if label == "Enter" => "↵".to_string(),
            label => label,
        };
        view.render(
            frame,
            area,
            theme,
            &self.suggestions_feed,
            self.session_feed.mutation_block(),
            (!self.suggestions_focused).then_some(activate_key.as_str()),
        );
        self.preview_outer_area = area;
        self.preview_area = area;
        self.preview_pane_area = area;
        self.preview_visible_rows = area.height as usize;
        self.sync_preview_capture_worker(None);
        true
    }
}
