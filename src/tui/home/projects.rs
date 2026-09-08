//! Registered projects: their headers, their config, and what a click on
//! one does.

use super::*;

impl HomeView {
    /// Reload the merged project registry into `registered_projects`. Called on
    /// every storage reload and after a pin/unpin so the project view's empty
    /// headers and pin indicators track the on-disk registry.
    ///
    /// In all-profiles mode `build_flat_items_by_project` merges sessions from
    /// every loaded profile, so the registry must too: a profile-scoped pin
    /// would otherwise lose its header (and glyph) the moment its sessions are
    /// gone. Dedupe across profiles by canonical path since each
    /// `load_merged` repeats the global entries.
    pub(in crate::tui) fn refresh_registered_projects(&mut self) {
        use crate::session::projects::{canonical_key, load_merged};
        if self.active_profile.is_some() {
            self.registered_projects = load_merged(&self.config_profile()).unwrap_or_default();
            return;
        }
        let profiles: Vec<String> = self.storages.keys().cloned().collect();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut merged = Vec::new();
        for profile in &profiles {
            for p in load_merged(profile).unwrap_or_default() {
                if seen.insert(canonical_key(&p.path)) {
                    merged.push(p);
                }
            }
        }
        self.registered_projects = merged;
    }

    /// The canonical repo path of the first live (non-archived) session under
    /// project header `label`, or `None` when no live session populates the
    /// header (an empty pinned header). This is the header's stable repo
    /// identity, so two repos that merely share a basename are judged against
    /// their own paths rather than the shared display label.
    ///
    /// Archived sessions are excluded on purpose: an empty main-flow header is
    /// injected by LABEL match against the registry, so its pin state and
    /// unpin toggle must resolve by the same rule. Letting an archived row
    /// lend the header its path made a registry entry with a different
    /// recorded path (repo deleted or moved, so `canonical_key` compares raw
    /// strings) render an unpinnable phantom header: pinned by label, judged
    /// by path.
    ///
    /// A scratch session never matches here: its `project_group_key` is the
    /// `SCRATCH_GROUP_PATH` sentinel, which no display label equals, so the
    /// synthetic bucket has no backing repo path to lend (#3237).
    pub(in crate::tui) fn project_header_repo_path(&self, label: &str) -> Option<String> {
        self.instances
            .values()
            .find(|i| !i.is_archived() && project_group_key(i) == label)
            .map(|i| crate::session::projects::canonical_key(i.repo_path()))
    }

    /// Whether the project-view header `label` is backed by a registered AND
    /// pinned project. A registry entry is the "saved project"; the `pinned`
    /// flag is the separate decision to keep its header visible (#2208), so a
    /// saved-but-unpinned repo reads as not pinned (no glyph; the toggle pins
    /// it). A header with live sessions is pinned iff its own repo path is a
    /// pinned entry, so two repos sharing a basename are judged independently.
    /// An empty header exists only because a pinned project carries that
    /// basename, so the label match still requires the flag. Used for the pin
    /// indicator and the pin toggle.
    pub(in crate::tui) fn is_project_label_pinned(&self, label: &str) -> bool {
        match self.project_header_repo_path(label) {
            Some(path) => self
                .registered_projects
                .iter()
                .any(|p| p.pinned && crate::session::projects::canonical_key(&p.path) == path),
            None => self
                .registered_projects
                .iter()
                .any(|p| p.pinned && crate::session::projects::repo_label(&p.path) == label),
        }
    }

    /// The project-view header label under the cursor when it is a real,
    /// pinnable project: project grouping is active, the cursor is on a group
    /// header, and that header is not a synthetic one (the Archived/Trash
    /// shelves or the scratch bucket, none of which have a backing repo to pin).
    pub(in crate::tui) fn project_group_at_cursor(&self) -> Option<String> {
        if self.group_by != GroupByMode::Project {
            return None;
        }
        match self.flat_items.get(self.cursor) {
            Some(Item::Group { path, name, .. })
                if !crate::session::is_synthetic_project_header(path) =>
            {
                Some(name.clone())
            }
            _ => None,
        }
    }

    /// Resolve the effective `SessionConfig` for an existing session
    /// row, honoring per-profile overrides. Reads the instance's
    /// `source_profile` so the picked config matches whatever profile
    /// the session was filed under (the home view's active profile may
    /// already have moved on); falls back to `config_profile()` when
    /// the instance has no recorded profile. Returns `None` for
    /// structured view-mode sessions because the attach-mode / click-action
    /// settings all have structured view-specific bypass paths upstream;
    /// callers treat `None` as "skip this setting, the structured view path
    /// handles activation."
    pub(super) fn resolve_session_config_for(
        &self,
        session_id: &str,
    ) -> Option<crate::session::SessionConfig> {
        let inst = self.get_instance(session_id)?;
        if inst.is_structured() {
            return None;
        }
        let profile = if inst.source_profile.is_empty() {
            self.config_profile()
        } else {
            inst.source_profile.clone()
        };
        Some(crate::session::resolve_config_or_warn(&profile).session)
    }

    /// Whether renaming this session should also move its worktree directory
    /// leaf, per the resolved `session.tie_workdir_to_name` setting. True only
    /// for aoe-managed worktree sessions. Unlike `resolve_session_config_for`,
    /// this does not bypass structured-view sessions: the directory tie is
    /// orthogonal to the view. See #1927.
    pub(in crate::tui) fn tie_workdir_applies_for(&self, session_id: &str) -> bool {
        let Some(inst) = self.get_instance(session_id) else {
            return false;
        };
        let profile = if inst.source_profile.is_empty() {
            self.config_profile()
        } else {
            inst.source_profile.clone()
        };
        let tie = crate::session::resolve_config_or_warn(&profile)
            .session
            .tie_workdir_to_name;
        inst.tie_workdir_applies(tie)
    }

    /// Resolve `click_action` for an existing session row when the
    /// user single-clicks it in the Structured view. See
    /// `resolve_session_config_for` for resolution rules; `None`
    /// (structured view) is treated by the caller as "fall through to the
    /// historical live-send path," which `start_live_send` itself
    /// short-circuits for structured view anyway.
    pub(in crate::tui) fn click_action(
        &self,
        session_id: &str,
    ) -> Option<crate::session::ClickAction> {
        self.resolve_session_config_for(session_id)
            .map(|s| s.click_action)
    }

    /// Resolve `default_attach_mode` for an existing session row when
    /// the user activates it (Enter / double-click) in the Structured view.
    /// See `resolve_session_config_for` for resolution rules; callers
    /// short-circuit to the structured view-specific activation path before
    /// consulting this setting.
    pub(in crate::tui) fn default_attach_mode(
        &self,
        session_id: &str,
    ) -> Option<crate::session::AttachMode> {
        self.resolve_session_config_for(session_id)
            .map(|s| s.default_attach_mode)
    }
    /// Resolve the attach mode for a newly-created terminal-mode session.
    /// `MatchDefault` preserves the previous behavior by using the setting
    /// that activates existing rows.
    pub(in crate::tui) fn new_session_attach_mode(
        &self,
        session_id: &str,
    ) -> Option<crate::session::AttachMode> {
        self.resolve_session_config_for(session_id)
            .map(|s| match s.new_session_mode {
                crate::session::NewSessionMode::MatchDefault => s.default_attach_mode,
                crate::session::NewSessionMode::Tmux => crate::session::AttachMode::Tmux,
                crate::session::NewSessionMode::LiveSend => crate::session::AttachMode::LiveSend,
            })
    }
}
