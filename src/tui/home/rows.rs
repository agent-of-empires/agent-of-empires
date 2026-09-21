//! The rows on screen: reading instances and flattening them into the
//! tree, project, and org groupings.

use std::collections::HashSet;

use super::*;

/// Project-mode group key: the repo label, or the `SCRATCH_GROUP_PATH`
/// sentinel so a real repo named `scratch` keeps its own identity.
pub(super) fn project_group_key(inst: &Instance) -> String {
    if inst.scratch {
        return crate::session::SCRATCH_GROUP_PATH.to_string();
    }
    crate::session::projects::repo_label(inst.repo_path())
}

/// Org-view label (and key) for sessions with no resolvable remote owner.
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

    pub(in crate::tui) fn selected_session_title(&self) -> Option<&str> {
        let id = self.selected_session.as_deref()?;
        Some(self.get_instance(id)?.title.as_str())
    }

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

    /// Build the id-keyed map from loaded instances. Ids duplicated across profiles (an
    /// interrupted profile move) are ambiguous, so every copy is excluded rather than routing
    /// lifecycle work to the wrong profile.
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
            match map.shift_remove(&inst.id) {
                Some(previous) => {
                    duplicate_ids.insert(inst.id.clone());
                    tracing::error!(
                        target: "tui.home",
                        id = %inst.id,
                        first_profile = %previous.source_profile,
                        second_profile = %inst.source_profile,
                        "duplicate session id across profiles; excluding every copy until durable reconciliation"
                    );
                }
                None => {
                    map.insert(inst.id.clone(), inst);
                }
            }
        }
        map
    }

    /// Instances in the active profile filter, or all of them.
    pub(in crate::tui) fn cloned_instances_in_active_view(&self) -> Vec<Instance> {
        match &self.active_profile {
            Some(profile) => self.cloned_instances_for_profile(profile),
            None => self.cloned_instances(),
        }
    }

    #[cfg(test)]
    #[track_caller]
    pub(in crate::tui) fn instance_at(&self, idx: usize) -> &Instance {
        let len = self.instances.len();
        self.instances
            .get_index(idx)
            .map(|(_, v)| v)
            .unwrap_or_else(|| panic!("instance_at: idx {idx} out of bounds (len={len})"))
    }

    #[cfg(test)]
    #[track_caller]
    pub(in crate::tui) fn instance_at_mut(&mut self, idx: usize) -> &mut Instance {
        let len = self.instances.len();
        self.instances
            .get_index_mut(idx)
            .map(|(_, v)| v)
            .unwrap_or_else(|| panic!("instance_at_mut: idx {idx} out of bounds (len={len})"))
    }

    /// True when any session animates a spinner, so the TUI must keep redrawing.
    pub fn has_animated_sessions(&self) -> bool {
        use crate::session::Status;
        self.instances.values().any(|inst| {
            matches!(
                inst.status,
                Status::Running | Status::Waiting | Status::Starting | Status::Creating
            )
        })
    }

    /// Index where the pinned Archived / Trash shelf begins. Both sections are
    /// appended last by `build_flat_items`, so the shelf is a contiguous suffix.
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
        // Project/org grouping keeps headers under every sort order, Attention included.
        match self.group_by {
            GroupByMode::Project => return self.build_flat_items_by_project(),
            GroupByMode::Org => return self.build_flat_items_by_org(),
            GroupByMode::Manual => {}
        }

        let pool = self.cloned_instances_in_active_view();
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
        (grouped, tree_seed)
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

    fn build_flat_items_by_project(&self) -> Vec<Item> {
        let (grouped, tree_seed) = self.regrouped_instances(project_group_key);
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

    fn build_flat_items_by_org(&self) -> Vec<Item> {
        let (grouped, tree_seed) = self.regrouped_instances(|inst| self.org_group_key(inst));
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
