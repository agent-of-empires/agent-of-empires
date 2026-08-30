//! The rows on screen: reading instances and flattening them into the
//! tree, project, and org groupings.

use super::*;

/// The GroupTree identity key for a session in project mode. Worktree sessions
/// key on `main_repo_path` (so all branches of a repo group together); other
/// sessions key on the last segment of `project_path`. Scratch sessions key on
/// the `SCRATCH_GROUP_PATH` sentinel, which no real repo basename can equal, so
/// a user's own repo named `scratch` keeps a distinct identity (#3237); the
/// bucket's display name is seeded separately in `build_flat_items_by_project`.
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
        let mut duplicate_ids = std::collections::HashSet::new();
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

    #[cfg(test)]
    #[track_caller]
    pub(in crate::tui) fn instance_at_mut(&mut self, idx: usize) -> &mut Instance {
        let len = self.instances.len();
        self.instances
            .get_index_mut(idx)
            .map(|(_, v)| v)
            .unwrap_or_else(|| panic!("instance_at_mut: idx {idx} out of bounds (len={len})"))
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
        // Project and org grouping are honored across every sort order.
        // Combined with Attention sort, sessions sort by tier within each
        // group and the headers float by their top-attention member (driven
        // by sort_groups + attention_group_key in flatten_tree). Check these
        // first so Project/Org + Attention doesn't fall through to the flat
        // Attention branch and lose the group headers.
        if self.group_by == GroupByMode::Project {
            return self.build_flat_items_by_project();
        }
        if self.group_by == GroupByMode::Org {
            return self.build_flat_items_by_org();
        }

        // Manual grouping + Attention sort is the cross-cutting flat
        // priority view: skip groups entirely so Waiting/Error rows from
        // different groups can interleave by tier instead of being walled
        // off behind group headers. Project/org grouping above opt into a
        // different shape on purpose (attention triage within explicit
        // group boundaries).
        if self.sort_order == SortOrder::Attention {
            let filtered: Vec<Instance> = self.cloned_instances_in_active_view();
            let mut items = flatten_sessions_by_attention(&filtered);
            append_archived_section(&mut items, &filtered, self.archived_section_collapsed);
            append_trash_section(&mut items, &filtered, self.trashed_section_collapsed);
            return items;
        }

        let archive_pool: Vec<Instance> = self.cloned_instances_in_active_view();
        let mut items = if let Some(profile) = &self.active_profile {
            match self.group_trees.get(profile) {
                Some(tree) => flatten_tree(tree, &archive_pool, self.sort_order),
                None => Vec::new(),
            }
        } else if self.storages.len() <= 1 {
            match self.group_trees.values().next() {
                Some(tree) => flatten_tree(tree, &archive_pool, self.sort_order),
                None => Vec::new(),
            }
        } else {
            flatten_tree_all_profiles(&archive_pool, &self.group_trees, self.sort_order)
        };

        // Pin the synthetic Archived section to the bottom regardless of
        // sort order. Archived rows were filtered out of the natural flow
        // inside `flatten_tree` / `flatten_tree_all_profiles`.
        append_archived_section(&mut items, &archive_pool, self.archived_section_collapsed);
        // Trash sits below Archived, also pinned to the bottom.
        append_trash_section(&mut items, &archive_pool, self.trashed_section_collapsed);
        items
    }

    fn build_flat_items_by_project(&self) -> Vec<Item> {
        // In project mode, always merge all sessions into one tree regardless of
        // profile count. Project grouping unifies by repo across profiles.
        let base_instances: Vec<Instance> = self.cloned_instances_in_active_view();

        let grouped: Vec<Instance> = base_instances
            .into_iter()
            .map(|mut inst| {
                inst.group_path = project_group_key(&inst);
                inst
            })
            .collect();

        // Project headers are derived purely from the live sessions, not a
        // persisted group list, so build the tree from non-archived members
        // only. An archived session already shows under the synthetic
        // Archived section (nested by project below); if it also seeded a
        // project node here, a project whose only remaining member is
        // archived would render an empty phantom header in the main flow.
        // That header is undeletable in project mode ("Project groups are
        // automatic"), leaving the user no way to clear it.
        let tree_seed: Vec<Instance> = grouped
            .iter()
            .filter(|i| !i.is_archived() && !i.is_trashed())
            .cloned()
            .collect();

        // Surface registered projects with no live session as empty "pinned"
        // headers, so a project can persist in the view without any sessions,
        // matching the WebUI where an empty project is just a registry entry.
        // Seed them as empty groups; their headers render even with zero
        // members (the phantom-header guard above only excludes archived-only
        // session groups, not deliberately pinned ones).
        let populated_labels: std::collections::HashSet<String> = tree_seed
            .iter()
            .map(|i| i.group_path.clone())
            .filter(|p| !p.is_empty())
            .collect();
        let mut seed_groups: Vec<crate::session::Group> =
            crate::session::projects::unpopulated_projects(
                &populated_labels,
                &self.registered_projects,
            )
            .into_iter()
            .map(|p| crate::session::Group::new(&p.label, &p.label))
            .collect();
        // The synthetic scratch bucket keys on a sentinel path so a real repo
        // named `scratch` keeps its own identity (#3237); seed its display name
        // the way org seeds host-scoped keys, but only when a live scratch
        // session populates it (an archived-only scratch group would otherwise
        // render an undeletable empty header, per the phantom-header note above).
        if populated_labels.contains(crate::session::SCRATCH_GROUP_PATH) {
            seed_groups.push(crate::session::Group::new(
                crate::session::SCRATCH_GROUP_NAME,
                crate::session::SCRATCH_GROUP_PATH,
            ));
        }
        let mut tree = GroupTree::new_with_groups(&tree_seed, &seed_groups);
        for (path, &collapsed) in &self.project_group_collapsed {
            if collapsed {
                tree.set_collapsed(path, true);
            }
        }
        let mut items = flatten_tree(&tree, &grouped, self.sort_order);
        append_archived_section_by_project(
            &mut items,
            &grouped,
            self.archived_section_collapsed,
            &self.project_group_collapsed,
            self.sort_order,
        );
        // Trash is a flat shelf even in project mode (recovery list, not a
        // workspace), pinned below the Archived section.
        append_trash_section(&mut items, &grouped, self.trashed_section_collapsed);
        items
    }

    /// Resolve `inst`'s org display owner and host-scoped identity key
    /// together, memoized per repo path in `remote_owner_cache` so grouping
    /// doesn't re-open a git repo on every rebuild. Scratch sessions need no
    /// special case: they live under `<app_dir>/scratch/<instance-id>/`,
    /// never a git checkout, so `get_remote_owner_with_key` already returns
    /// `None` for them and they fall into the "No organization" bucket for
    /// free, same as any other repo with no hosted `origin` remote.
    fn resolve_org(&self, inst: &Instance) -> (String, String) {
        let repo_path = inst.repo_path();
        if let Some(cached) = self.remote_owner_cache.borrow().get(repo_path) {
            return match cached {
                Some((owner, key)) => (owner.clone(), key.clone()),
                None => (
                    NO_ORG_GROUP_LABEL.to_string(),
                    NO_ORG_GROUP_LABEL.to_string(),
                ),
            };
        }
        let resolved = crate::git::get_remote_owner_with_key(std::path::Path::new(repo_path));
        self.remote_owner_cache
            .borrow_mut()
            .insert(repo_path.to_string(), resolved.clone());
        match resolved {
            Some((owner, key)) => (owner, key),
            None => (
                NO_ORG_GROUP_LABEL.to_string(),
                NO_ORG_GROUP_LABEL.to_string(),
            ),
        }
    }

    /// The org header's display text: the bare remote owner, or "No
    /// organization". Never use this for `group_path`/collapse-state
    /// matching; two owners of the same name on different hosts share this
    /// label but not the same identity, see `org_group_key`.
    fn org_group_name(&self, inst: &Instance) -> String {
        self.resolve_org(inst).0
    }

    /// The org group's identity key ("owner@host", or "No organization"),
    /// used for `group_path`, collapse-state matching, and group-membership
    /// filters. Host-scoped so same-named owners on different hosts (GitHub
    /// "acme" vs GitLab "acme") never merge into one bucket or one
    /// bulk-archive scope.
    pub(super) fn org_group_key(&self, inst: &Instance) -> String {
        self.resolve_org(inst).1
    }

    /// Org-mode counterpart of `build_flat_items_by_project`: groups sessions
    /// by their repo's resolved remote owner instead of by repo basename.
    /// Unlike project mode, there is no org registry to surface empty
    /// "pinned" headers for (#3283 explicitly scopes org grouping to live
    /// sessions only), so the tree is seeded with only the display-named
    /// groups derived below rather than `unpopulated_projects`.
    fn build_flat_items_by_org(&self) -> Vec<Item> {
        let base_instances: Vec<Instance> = self.cloned_instances_in_active_view();

        let grouped: Vec<Instance> = base_instances
            .into_iter()
            .map(|mut inst| {
                inst.group_path = self.org_group_key(&inst);
                inst
            })
            .collect();

        // Same phantom-header rationale as project mode: seed the tree from
        // non-archived, non-trashed members only, so an org whose only
        // remaining member is archived doesn't render an empty, undeletable
        // header in the main flow.
        let tree_seed: Vec<Instance> = grouped
            .iter()
            .filter(|i| !i.is_archived() && !i.is_trashed())
            .cloned()
            .collect();

        // `group_path` is now the host-scoped identity key ("owner@host"),
        // not the display text, so pre-seed a `Group` per distinct key
        // carrying the bare-owner display name; `GroupTree` renders these
        // verbatim instead of deriving a name by splitting the key on '/'
        // (which the key never contains, so it would otherwise stay a flat
        // group named after the whole key).
        let mut seen_keys = std::collections::HashSet::new();
        let org_labels: Vec<crate::session::Group> = tree_seed
            .iter()
            .filter(|i| !i.group_path.is_empty() && seen_keys.insert(i.group_path.clone()))
            .map(|i| crate::session::Group::new(&self.org_group_name(i), &i.group_path))
            .collect();

        let mut tree = GroupTree::new_with_groups(&tree_seed, &org_labels);
        for (path, &collapsed) in &self.org_group_collapsed {
            if collapsed {
                tree.set_collapsed(path, true);
            }
        }
        let mut items = flatten_tree(&tree, &grouped, self.sort_order);
        append_archived_section_by_project(
            &mut items,
            &grouped,
            self.archived_section_collapsed,
            &self.org_group_collapsed,
            self.sort_order,
        );
        // Trash is a flat shelf even in org mode (recovery list, not a
        // workspace), pinned below the Archived section.
        append_trash_section(&mut items, &grouped, self.trashed_section_collapsed);
        items
    }
}
