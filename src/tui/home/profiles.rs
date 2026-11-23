//! Switching the active profile.

use super::*;

impl HomeView {
    /// The active profile filter name, or `None` when no filter is applied.
    /// Returning `None` lets callers (e.g. the list-pane title) omit the
    /// `[<profile>]` segment entirely instead of rendering a noisy `[all]`.
    pub fn active_profile_display(&self) -> Option<&str> {
        self.active_profile.as_deref()
    }

    /// Switch the active profile filter in-place without destroying the view.
    /// Pass `None` for all-profiles mode, or `Some(name)` to filter to one profile.
    pub fn switch_profile(&mut self, new_profile: Option<String>) -> anyhow::Result<()> {
        if self.active_profile != new_profile {
            self.cancel_native_attachment();
        }
        let previous_profile = std::mem::replace(&mut self.active_profile, new_profile);
        let config_targets = match crate::session::list_profiles() {
            Ok(profiles) => profiles,
            Err(e) => {
                tracing::warn!(
                    target: "tui.file_watch",
                    error = %e,
                    "list_profiles failed during switch_profile; reusing loaded storages for config rewire"
                );
                let mut keys: Vec<String> = self.storages.keys().cloned().collect();
                keys.sort();
                keys
            }
        };
        self.rewire_config_subscriptions(&config_targets);
        let projection = match self.session_feed.applied_snapshot() {
            Some(snapshot) => self.apply_canonical_projection(&snapshot, true),
            None => Ok(()),
        };
        if let Err(error) = projection {
            self.active_profile = previous_profile;
            return Err(error);
        }
        self.selected_session = None;
        self.selected_group = None;
        self.selected_group_profile = None;
        self.rebuild_flat_items_keeping_cursor();
        self.update_selected();
        self.refresh_from_config(ConfigRefreshOrigin::Interactive);
        // Invalidate preview caches since the visible sessions changed
        self.preview_cache = PreviewCache::default();
        self.terminal_preview_cache = PreviewCache::default();
        self.container_terminal_preview_cache = PreviewCache::default();
        self.tool_preview_cache = PreviewCache::default();
        self.preview_scroll_offset = 0;
        // Clear search since match indices are invalid with new flat_items
        if self.search_active {
            self.search_active = false;
            self.search_query = Input::default();
            self.search_matches.clear();
            self.search_match_index = 0;
        }
        Ok(())
    }
}
