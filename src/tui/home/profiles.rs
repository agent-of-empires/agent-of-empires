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
        self.active_profile = new_profile;
        if let Some(profile) = self.active_profile.clone() {
            if !self.storages.contains_key(&profile) {
                self.storages.insert(
                    profile.clone(),
                    Storage::new(&profile, self.file_watch.clone())?,
                );
            }
            self.storages.retain(|name, _| name == &profile);
            self.rewire_disk_subscriptions(std::slice::from_ref(&profile));
        }
        // Reconcile config-watch subscriptions explicitly so this contract
        // is local to switch_profile rather than implicit through
        // reload_storage_only's transitive call. Idempotent set-diff: the
        // global subscription is install-once and per-profile entries
        // converge to the on-disk profile set; redundant invocations are
        // no-ops.
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
        // Clear selection before reload so stale session/group refs don't linger
        self.selected_session = None;
        self.selected_group = None;
        self.selected_group_profile = None;
        self.reload()?;
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
