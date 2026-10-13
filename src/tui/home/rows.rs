//! The rows on screen: reading instances and flattening them into the
//! tree, project, and org groupings.

use super::*;
use std::collections::HashSet;

/// `pool` without the stopped sessions that sit in a group, when `hide_stopped` is on. Archived
/// and trashed sessions keep their own sections.
fn shown_in_groups(pool: Vec<Instance>, hide_stopped: bool) -> Vec<Instance> {
    if !hide_stopped {
        return pool;
    }
    pool.into_iter()
        .filter(|i| {
            i.status != crate::session::Status::Stopped
                || i.group_path.is_empty()
                || i.is_archived()
                || i.is_trashed()
        })
        .collect()
}

/// Project-mode group key, including the distinct scratch sentinel.
pub(super) fn project_group_key(inst: &Instance) -> String {
    if inst.scratch {
        return crate::session::SCRATCH_GROUP_PATH.to_string();
    }

    crate::session::projects::repo_label(inst.repo_path())
}

/// Header label the synthetic "no resolvable owner" bucket renders under in
/// org view. This is a display label; a hosted remote whose owner is literally
/// "No organization" would share it, but that collision is vanishingly unlikely
/// (unlike `scratch`, a plausible real repo basename), so org does not give it a
/// sentinel identity the way project mode does for scratch (#3237).
pub(super) const NO_ORG_GROUP_LABEL: &str = "No organization";

impl HomeView {
    pub fn instances(&self) -> impl ExactSizeIterator<Item = &Instance> + '_ {
        self.instances.values()
    }

    pub(in crate::tui) fn has_instances(&self) -> bool {
        !self.instances.is_empty()
    }

    pub fn get_instance(&self, id: &str) -> Option<&Instance> {
        self.instances.get(id)
    }

    /// Title of the currently selected session row, if the cursor is on one.
    pub(in crate::tui) fn selected_session_title(&self) -> Option<&str> {
        let id = self.selected_session.as_deref()?;
        Some(self.get_instance(id)?.title.as_str())
    }

    /// Materialize `self.instances` into a `Vec` for callsites that hand off
    /// a `&[Instance]` slice to a downstream API. Single seam so the day
    /// `HomeView` grows a cache, only this helper needs to change.
    pub(in crate::tui) fn cloned_instances(&self) -> Vec<Instance> {
        self.instances.values().cloned().collect()
    }

    pub(in crate::tui) fn cloned_instances_for_profile(&self, profile: &str) -> Vec<Instance> {
        self.instances
            .values()
            .filter(|i| i.source_profile == profile)
            .cloned()
            .collect()
    }

    /// Build the id-keyed `IndexMap` from a `Vec<Instance>` (the storage-load
    /// shape). Duplicate ids across profiles are ambiguous after an interrupted
    /// profile move: selecting either row by iteration order can route lifecycle
    /// work to the wrong profile. Exclude every copy and fail closed; the
    /// reload path runs `reconcile_profile_duplicates` first, so only
    /// legacy duplicates without journal evidence ever reach this state.
    pub(super) fn build_instances_map(
        all_instances: Vec<Instance>,
    ) -> indexmap::IndexMap<String, Instance> {
        let mut map: indexmap::IndexMap<String, Instance> =
            indexmap::IndexMap::with_capacity(all_instances.len());
        let mut duplicate_ids = HashSet::new();
        for inst in all_instances {
            if duplicate_ids.contains(&inst.id) {
                continue;
            }
            if let Some(previous) = map.shift_remove(&inst.id) {
                duplicate_ids.insert(inst.id.clone());
                tracing::error!(
                    target: "tui.home",
                    id = %inst.id,
                    first_profile = %previous.source_profile,
                    second_profile = %inst.source_profile,
                    "duplicate session id across profiles; excluding every copy until durable reconciliation"
                );
            } else {
                map.insert(inst.id.clone(), inst);
            }
        }
        map
    }

    /// `cloned_instances_for_profile` on `self.active_profile` when set,
    /// else the unfiltered `cloned_instances`. The scope every UI-facing
    /// build path (flat items, project view) shares.
    pub(in crate::tui) fn cloned_instances_in_active_view(&self) -> Vec<Instance> {
        match &self.active_profile {
            Some(profile) => self.cloned_instances_for_profile(profile),
            None => self.cloned_instances(),
        }
    }

    #[cfg(test)]
    #[track_caller]
    pub(in crate::tui) fn instance_at(&self, idx: usize) -> &Instance {
        self.instances
            .get_index(idx)
            .map(|(_, v)| v)
            .unwrap_or_else(|| {
                panic!(
                    "instance_at: idx {idx} out of bounds (len={})",
                    self.instances.len()
                )
            })
    }

    /// Returns true if any session has an animated status (Running, Waiting, Starting,
    /// Creating), which means the TUI needs periodic redraws for spinner animation.
    pub fn has_animated_sessions(&self) -> bool {
        use crate::session::Status;
        self.instances.values().any(|inst| {
            matches!(
                inst.status,
                Status::Running | Status::Waiting | Status::Starting | Status::Creating
            )
        })
    }

    /// Index of the first `flat_items` row that belongs to the pinned bottom
    /// shelf (the synthetic Archived / Trash sections), or `None` when neither
    /// section is present. The shelf is always a contiguous suffix: both
    /// sections are appended last (Archived then Trash) by `build_flat_items`,
    /// and nothing non-shelf follows them, so the first row whose path sits
    /// within either section marks where the workspace list ends and the shelf
    /// begins. The renderer splits the sidebar here and hit-testing maps clicks
    /// in the shelf region back to this suffix.
    pub(in crate::tui) fn shelf_start(&self) -> Option<usize> {
        self.flat_items.iter().position(|it| match it {
            Item::Group { path, .. } => {
                crate::session::is_within_archived_section(path)
                    || crate::session::is_within_trash_section(path)
            }
            Item::Session { .. } => false,
        })
    }

    pub(in crate::tui) fn build_flat_items(&self) -> Vec<Item> {
        self.build_flat_items_hiding(self.hide_stopped_in_groups)
    }

    /// Rebuild the rows and, while stopped sessions are hidden, every group header's full count.
    pub(super) fn refresh_flat_items(&mut self) {
        self.flat_items = self.build_flat_items();
        self.group_totals = if self.hide_stopped_in_groups {
            self.build_flat_items_hiding(false)
                .into_iter()
                .filter_map(|item| match item {
                    Item::Group {
                        path,
                        profile,
                        session_count,
                        ..
                    } => Some(((path, profile), session_count)),
                    Item::Session { .. } => None,
                })
                .collect()
        } else {
            HashMap::new()
        };
        self.end_live_send_if_hidden();
    }

    pub(super) fn toggle_hide_stopped_in_groups(&mut self) {
        self.hide_stopped_in_groups = !self.hide_stopped_in_groups;
        self.rebuild_flat_items_keeping_cursor();
        self.update_selected();
        self.flash_status(if self.hide_stopped_in_groups {
            "Stopped sessions in groups hidden (y to show)"
        } else {
            "Showing stopped sessions"
        });
    }

    /// Whether the `y` filter is keeping `inst` out of the list.
    pub(super) fn hidden_by_filter(&self, inst: &Instance) -> bool {
        self.hide_stopped_in_groups
            && inst.status == crate::session::Status::Stopped
            && self.could_hide(inst)
    }

    /// Whether hiding could take `inst` out of the list once it is stopped.
    fn could_hide(&self, inst: &Instance) -> bool {
        let grouped = match self.group_by {
            GroupByMode::Manual => {
                !inst.group_path.is_empty() && self.sort_order != SortOrder::Attention
            }
            GroupByMode::Project | GroupByMode::Org => true,
        };
        grouped && !inst.is_archived() && !inst.is_trashed()
    }

    /// The row of the group header session `id` sits under while the `y` filter hides it, so a
    /// selection that drops out moves to its own group rather than to whichever row now has its
    /// index. Read from the unfiltered rows of the current grouping, where a session follows its
    /// own group's header.
    pub(super) fn header_row_for_hidden_session(&self, id: &str) -> Option<usize> {
        if !self
            .get_instance(id)
            .is_some_and(|inst| self.hidden_by_filter(inst))
        {
            return None;
        }
        let unfiltered = self.build_flat_items_hiding(false);
        let at = unfiltered
            .iter()
            .position(|item| matches!(item, Item::Session { id: sid, .. } if sid == id))?;
        let (path, profile) = unfiltered[..at].iter().rev().find_map(|item| match item {
            Item::Group { path, profile, .. } => Some((path, profile)),
            Item::Session { .. } => None,
        })?;
        self.flat_items.iter().position(|item| {
            matches!(item, Item::Group { path: p, profile: pr, .. } if p == path && pr == profile)
        })
    }

    fn build_flat_items_hiding(&self, hide_stopped: bool) -> Vec<Item> {
        // Project/org grouping keeps headers under every sort order, Attention included.
        match self.group_by {
            GroupByMode::Project => return self.build_flat_items_by_project(hide_stopped),
            GroupByMode::Org => return self.build_flat_items_by_org(hide_stopped),
            GroupByMode::Manual => {}
        }

        let pool = self.cloned_instances_in_active_view();
        // Manual grouping under Attention sort is flat, so it has no groups to hide in.
        let pool = if self.sort_order == SortOrder::Attention {
            pool
        } else {
            shown_in_groups(pool, hide_stopped)
        };
        // Manual grouping + Attention sort is a flat priority view so tiers
        // interleave across groups.
        let mut items = if self.sort_order == SortOrder::Attention {
            flatten_sessions_by_attention(&pool)
        } else if let Some(profile) = &self.active_profile {
            self.group_trees
                .get(profile)
                .map_or_else(Vec::new, |tree| flatten_tree(tree, &pool, self.sort_order))
        } else if self.storages.len() <= 1 {
            self.group_trees
                .values()
                .next()
                .map_or_else(Vec::new, |tree| flatten_tree(tree, &pool, self.sort_order))
        } else {
            flatten_tree_all_profiles(&pool, &self.group_trees, self.sort_order)
        };
        append_archived_section(&mut items, &pool, self.archived_section_collapsed);
        append_trash_section(&mut items, &pool, self.trashed_section_collapsed);
        items
    }

    /// Instances in the active view with `group_path` rewritten by `key`, plus the live
    /// subset that seeds the tree. An archived-only group must not seed a header: it would
    /// render as an empty, undeletable phantom in the main flow.
    fn regrouped_instances(
        &self,
        key: impl Fn(&Instance) -> String,
        hide_stopped: bool,
    ) -> (Vec<Instance>, Vec<Instance>) {
        let grouped: Vec<Instance> = self
            .cloned_instances_in_active_view()
            .into_iter()
            .map(|mut inst| {
                inst.group_path = key(&inst);
                inst
            })
            .collect();
        let tree_seed = grouped
            .iter()
            .filter(|i| !i.is_archived() && !i.is_trashed())
            .cloned()
            .collect();
        // Seeded before hiding, so a group whose sessions are all hidden keeps its header.
        (shown_in_groups(grouped, hide_stopped), tree_seed)
    }

    /// Flatten a derived (project or org) grouping with its collapse state,
    /// the per-group Archived section, and the flat Trash shelf.
    fn flatten_derived_groups(
        &self,
        grouped: &[Instance],
        tree_seed: &[Instance],
        seed_groups: &[crate::session::Group],
        collapsed: &HashMap<String, bool>,
    ) -> Vec<Item> {
        let mut tree = GroupTree::new_with_groups(tree_seed, seed_groups);
        for (path, _) in collapsed.iter().filter(|(_, &c)| c) {
            tree.set_collapsed(path, true);
        }
        let mut items = flatten_tree(&tree, grouped, self.sort_order);
        append_archived_section_by_project(
            &mut items,
            grouped,
            self.archived_section_collapsed,
            collapsed,
            self.sort_order,
        );
        append_trash_section(&mut items, grouped, self.trashed_section_collapsed);
        items
    }

    fn build_flat_items_by_project(&self, hide_stopped: bool) -> Vec<Item> {
        let (grouped, tree_seed) = self.regrouped_instances(project_group_key, hide_stopped);
        let populated_labels: HashSet<String> = tree_seed
            .iter()
            .map(|i| i.group_path.clone())
            .filter(|p| !p.is_empty())
            .collect();
        // Pinned registered projects with no live session render as empty headers.
        let mut seed_groups: Vec<Group> = crate::session::projects::unpopulated_projects(
            &populated_labels,
            &self.registered_projects,
        )
        .into_iter()
        .map(|p| Group::new(&p.label, &p.label))
        .collect();
        if populated_labels.contains(crate::session::SCRATCH_GROUP_PATH) {
            seed_groups.push(Group::new(
                crate::session::SCRATCH_GROUP_NAME,
                crate::session::SCRATCH_GROUP_PATH,
            ));
        }
        self.flatten_derived_groups(
            &grouped,
            &tree_seed,
            &seed_groups,
            &self.project_group_collapsed,
        )
    }

    /// Resolve `inst`'s org `(display owner, host-scoped key)`, memoized per
    /// repo path so grouping doesn't reopen a git repo on every rebuild.
    fn resolve_org(&self, inst: &Instance) -> (String, String) {
        let repo_path = inst.repo_path();
        let cached = self.remote_owner_cache.borrow().get(repo_path).cloned();
        let resolved = cached.unwrap_or_else(|| {
            let resolved = crate::git::get_remote_owner_with_key(std::path::Path::new(repo_path));
            self.remote_owner_cache
                .borrow_mut()
                .insert(repo_path.to_string(), resolved.clone());
            resolved
        });
        resolved.unwrap_or_else(|| {
            (
                NO_ORG_GROUP_LABEL.to_string(),
                NO_ORG_GROUP_LABEL.to_string(),
            )
        })
    }

    /// The org group's identity key ("owner@host"), host-scoped so same-named
    /// owners on different hosts never merge. Not the display name.
    pub(super) fn org_group_key(&self, inst: &Instance) -> String {
        self.resolve_org(inst).1
    }

    fn build_flat_items_by_org(&self, hide_stopped: bool) -> Vec<Item> {
        let (grouped, tree_seed) =
            self.regrouped_instances(|inst| self.org_group_key(inst), hide_stopped);
        // The key is not a display name, so seed each group with its owner name.
        let mut seen_keys = HashSet::new();
        let org_groups: Vec<Group> = tree_seed
            .iter()
            .filter(|i| !i.group_path.is_empty() && seen_keys.insert(i.group_path.clone()))
            .map(|i| Group::new(&self.resolve_org(i).0, &i.group_path))
            .collect();
        self.flatten_derived_groups(&grouped, &tree_seed, &org_groups, &self.org_group_collapsed)
    }
}
