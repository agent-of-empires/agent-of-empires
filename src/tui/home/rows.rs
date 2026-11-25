//! The rows on screen: reading instances and flattening them into the
//! tree, project, and org groupings.

use super::*;
use std::collections::HashSet;

/// Whether the `y` filter counts `inst` among the stopped rows to hide. Snoozed Sessions join
/// them when `hide_snoozed` is on; archived and trashed rows keep their own sections.
fn stopped_row_hidden_in_groups(inst: &Instance, hide_snoozed: bool) -> bool {
    inst.status == crate::session::Status::Stopped || (hide_snoozed && inst.is_snoozed())
}

/// Project-mode group key, including the distinct scratch sentinel.
pub(super) fn project_group_key(inst: &Instance) -> String {
    if inst.scratch {
        return crate::session::SCRATCH_GROUP_PATH.to_string();
    }

    crate::session::projects::repo_label(inst.repo_path())
}

/// Repository-mode tree path: the repository header, then the session's own Group nested
/// inside it. Repository labels are basenames, so the first segment is always the repository.
pub(super) fn project_view_path(inst: &Instance) -> String {
    let key = project_group_key(inst);
    if inst.scratch || inst.group_path.is_empty() {
        key
    } else {
        format!("{key}/{}", inst.group_path)
    }
}

/// Split a Repository-mode header path into its repository label and nested Group path.
pub(super) fn split_project_view_path(path: &str) -> (&str, Option<&str>) {
    match path.split_once('/') {
        Some((repo, group)) => (repo, Some(group)),
        None => (path, None),
    }
}

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
    /// shelf (the Suggested row and the synthetic Archived / Trash sections), or
    /// `None` when none is present. The shelf is always a contiguous suffix:
    /// `build_flat_items` appends them last, in that order, so the first such row
    /// marks where the workspace list ends. The renderer splits the sidebar here
    /// and hit-testing maps clicks in the shelf region back to this suffix.
    pub(in crate::tui) fn shelf_start(&self) -> Option<usize> {
        self.flat_items.iter().position(|it| match it {
            Item::Group { path, .. } => {
                crate::session::is_within_archived_section(path)
                    || crate::session::is_within_trash_section(path)
            }
            Item::Suggested { .. } => true,
            Item::Session { .. } => false,
        })
    }

    pub(in crate::tui) fn build_flat_items(&self) -> Vec<Item> {
        self.build_flat_items_hiding(self.hide_stopped_in_groups, self.needs_you_only)
    }

    /// Rebuild the rows and, while stopped sessions are hidden, every group header's full count.
    pub(super) fn refresh_flat_items(&mut self) {
        self.flat_items = self.build_flat_items();
        self.group_totals = if self.hide_stopped_in_groups {
            self.build_flat_items_hiding(false, self.needs_you_only)
                .into_iter()
                .filter_map(|item| match item {
                    Item::Group {
                        path,
                        profile,
                        session_count,
                        ..
                    } => Some(((path, profile), session_count)),
                    Item::Session { .. } | Item::Suggested { .. } => None,
                })
                .collect()
        } else {
            HashMap::new()
        };
        self.end_live_send_if_hidden();
    }

    pub(super) fn toggle_needs_you_only(&mut self) {
        self.needs_you_only = !self.needs_you_only;
        self.rebuild_flat_items_keeping_cursor();
        self.update_selected();
        self.flash_status(if self.needs_you_only {
            "Showing only sessions that need you"
        } else {
            "Showing every session"
        });
    }

    /// Sessions under the header at `path` that need you. Under Repository a header
    /// holds the rows at or beneath its `repo/group` path; under Groups, its own
    /// profile's rows at or beneath its group path.
    pub(super) fn needs_you_under(&self, path: &str, profile: Option<&str>) -> usize {
        let prefix = format!("{path}/");
        let within = |p: &str| p == path || p.starts_with(&prefix);
        let active = self.active_profile.as_deref();
        self.instances
            .values()
            .filter(|inst| inst.needs_you())
            .filter(|inst| active.is_none_or(|p| inst.source_profile == p))
            .filter(|inst| match self.group_by {
                GroupByMode::Project => within(&project_view_path(inst)),
                GroupByMode::Group => {
                    within(&inst.group_path) && profile.is_none_or(|p| inst.source_profile == p)
                }
            })
            .count()
    }

    pub(super) fn toggle_hide_stopped_in_groups(&mut self) {
        self.hide_stopped_in_groups = !self.hide_stopped_in_groups;
        self.rebuild_flat_items_keeping_cursor();
        self.update_selected();
        self.flash_status(if !self.hide_stopped_in_groups {
            "Showing stopped sessions"
        } else if self.hide_snoozed_with_stopped {
            // Keep the recovery key on screen at the supported 48-column minimum.
            "Stopped and snoozed hidden (y to show)"
        } else {
            "Stopped sessions in groups hidden (y to show)"
        });
    }

    /// Whether the `y` filter or the needs-you filter is keeping `inst` out of the list.
    pub(super) fn hidden_by_filter(&self, inst: &Instance) -> bool {
        (self.needs_you_only && !self.listed_by_needs_you(inst)) || self.hidden_as_stopped(inst)
    }

    /// Whether the needs-you filter lists `inst`: it needs you, or it is the selection kept.
    pub(super) fn listed_by_needs_you(&self, inst: &Instance) -> bool {
        inst.needs_you()
            || (self.needs_you_kept.as_deref() == Some(inst.id.as_str()) && !inst.is_dismissed())
    }

    /// Keep the selected session listed under the needs-you filter until the selection leaves
    /// it, as a mail client keeps a message just read; the row left behind drops out at once.
    pub(super) fn sync_needs_you_kept(&mut self) {
        let kept = self
            .selected_session
            .clone()
            .filter(|_| self.needs_you_only)
            .filter(|id| {
                self.get_instance(id)
                    .is_some_and(|inst| self.listed_by_needs_you(inst))
            });
        let released = std::mem::replace(&mut self.needs_you_kept, kept);
        if released.is_some()
            && released != self.needs_you_kept
            && self.needs_you_only
            && released
                .as_deref()
                .and_then(|id| self.get_instance(id))
                .is_some_and(|inst| !inst.needs_you())
        {
            self.rebuild_flat_items_keeping_cursor();
            // The rebuild can drop the header under the cursor, leaving the selection on a row
            // no longer listed.
            self.update_selected();
        }
    }

    pub(super) fn hidden_as_stopped(&self, inst: &Instance) -> bool {
        self.hide_stopped_in_groups
            && stopped_row_hidden_in_groups(inst, self.hide_snoozed_with_stopped)
            && self.could_hide(inst)
    }

    /// Whether hiding could take `inst` out of the list once it is stopped.
    fn could_hide(&self, inst: &Instance) -> bool {
        let grouped = match self.group_by {
            GroupByMode::Group => !inst.group_path.is_empty(),
            GroupByMode::Project => true,
        };
        grouped && !inst.is_archived() && !inst.is_trashed()
    }

    /// The row of the group header session `id` sits under while a filter hides it, so a
    /// selection that drops out moves to its own group rather than to whichever row now has its
    /// index. Read from the unfiltered rows of the current grouping, where a session follows its
    /// own group's header. When the filter drops that header too, the nearest listed header
    /// before the session, or else after it.
    pub(super) fn header_row_for_hidden_session(&self, id: &str) -> Option<usize> {
        if !self
            .get_instance(id)
            .is_some_and(|inst| self.hidden_by_filter(inst))
        {
            return None;
        }
        let unfiltered = self.build_flat_items_hiding(false, false);
        let at = unfiltered
            .iter()
            .position(|item| matches!(item, Item::Session { id: sid, .. } if sid == id))?;
        let (before, after) = unfiltered.split_at(at);
        before.iter().rev().chain(after).find_map(|item| {
            let Item::Group { path, profile, .. } = item else {
                return None;
            };
            self.flat_items.iter().position(|row| {
                matches!(row, Item::Group { path: p, profile: pr, .. } if p == path && pr == profile)
            })
        })
    }

    /// `pool` without the sessions the `y` filter hides, when `hide_stopped` is on: stopped rows
    /// in a group, plus snoozed ones under `session.hide_snoozed_with_stopped`. Archived and
    /// trashed sessions keep their own sections, and a session in no manual group stays visible.
    fn shown_in_groups(&self, pool: Vec<Instance>, hide_stopped: bool) -> Vec<Instance> {
        if !hide_stopped {
            return pool;
        }
        let hide_snoozed = self.hide_snoozed_with_stopped;
        pool.into_iter()
            .filter(|i| {
                !stopped_row_hidden_in_groups(i, hide_snoozed)
                    || i.group_path.is_empty()
                    || i.is_archived()
                    || i.is_trashed()
            })
            .collect()
    }

    fn build_flat_items_hiding(&self, hide_stopped: bool, needs_you_only: bool) -> Vec<Item> {
        if self.group_by == GroupByMode::Project {
            return self.build_flat_items_by_project(hide_stopped, needs_you_only);
        }

        let mut pool = self.shown_in_groups(self.cloned_instances_in_active_view(), hide_stopped);
        if needs_you_only {
            pool.retain(|inst| self.listed_by_needs_you(inst));
        }
        let mut items = if let Some(profile) = &self.active_profile {
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
        self.append_suggested(&mut items);
        append_archived_section(&mut items, &pool, self.archived_section_collapsed);
        append_trash_section(&mut items, &pool, self.trashed_section_collapsed);
        items
    }

    /// Repository mode: one header per repository with the session's Groups nested inside.
    /// Archived rows keep only their repository, which buckets the Archived section.
    fn build_flat_items_by_project(&self, hide_stopped: bool, needs_you_only: bool) -> Vec<Item> {
        let mut grouped: Vec<Instance> = self
            .cloned_instances_in_active_view()
            .into_iter()
            .map(|mut inst| {
                inst.group_path = if inst.is_archived() || inst.is_trashed() {
                    project_group_key(&inst)
                } else {
                    project_view_path(&inst)
                };
                inst
            })
            .collect();
        // Headers never moved by hand sit newest first, then the shared repository order.
        let rank = |repo: &str| {
            let repo = repo.trim_end_matches('/');
            (self.sort_order == SortOrder::Custom)
                .then(|| {
                    self.repository_order
                        .iter()
                        .position(|r| r.trim_end_matches('/') == repo)
                })
                .flatten()
        };
        grouped.sort_by_key(|inst| std::cmp::Reverse(inst.created_at));
        grouped.sort_by_key(|inst| {
            let rank = rank(inst.repo_path());
            (rank.is_some(), rank)
        });
        let live: Vec<&Instance> = grouped
            .iter()
            .filter(|i| !i.is_archived() && !i.is_trashed())
            .collect();
        let populated_labels: HashSet<String> = live
            .iter()
            .map(|i| split_project_view_path(&i.group_path).0.to_string())
            .collect();
        // An archived-only repository must not seed a header: it would render as an empty,
        // undeletable phantom in the main flow.
        let seeded: Vec<&Instance> = live
            .iter()
            .copied()
            .filter(|i| i.group_path != crate::session::SCRATCH_GROUP_PATH)
            .collect();
        // Pinned registered projects with no live session render as empty headers, placed
        // among the others by the shared repository order.
        let mut headers: Vec<(Option<usize>, Group)> = Vec::new();
        for inst in &seeded {
            let label = split_project_view_path(&inst.group_path).0;
            if !headers.iter().any(|(_, group)| group.path == label) {
                headers.push((rank(inst.repo_path()), Group::new(label, label)));
            }
        }
        let unpopulated = if needs_you_only {
            Vec::new()
        } else {
            crate::session::projects::unpopulated_projects(
                &populated_labels,
                &self.registered_projects,
            )
        };
        for project in unpopulated {
            headers.push((
                rank(&project.path),
                Group::new(&project.label, &project.label),
            ));
        }
        headers.sort_by_key(|(rank, _)| (rank.is_some(), *rank));
        let headers: Vec<Group> = headers.into_iter().map(|(_, group)| group).collect();
        // Under Custom a repository's own groups follow their rows' order, which a move
        // from Name or Newest first keeps as shown.
        let mut nested = seeded.clone();
        if self.sort_order == SortOrder::Custom {
            nested.sort_by_key(|i| i.custom_order_key());
        }
        let mut tree =
            GroupTree::new_with_group_paths(nested.iter().map(|i| i.group_path.as_str()), &headers);
        let scratch = populated_labels
            .contains(crate::session::SCRATCH_GROUP_PATH)
            .then(|| {
                Group::new(
                    crate::session::SCRATCH_GROUP_NAME,
                    crate::session::SCRATCH_GROUP_PATH,
                )
            });
        tree.append_groups(scratch);
        for (path, _) in self.project_group_collapsed.iter().filter(|(_, &c)| c) {
            tree.set_collapsed(path, true);
        }
        // Seeded before filtering, so a header keeps its place while its sessions are hidden.
        let listed_paths = needs_you_only.then(|| {
            grouped.retain(|inst| self.listed_by_needs_you(inst));
            let mut paths = HashSet::new();
            for inst in grouped
                .iter()
                .filter(|i| !i.is_archived() && !i.is_trashed())
            {
                let path = inst.group_path.as_str();
                paths.extend(
                    path.match_indices('/')
                        .map(|(at, _)| path[..at].to_string()),
                );
                paths.insert(path.to_string());
            }
            paths
        });
        let grouped = self.shown_in_groups(grouped, hide_stopped);
        let mut items = flatten_tree(&tree, &grouped, self.sort_order);
        if let Some(paths) = listed_paths {
            items.retain(|item| match item {
                Item::Group { path, .. } => paths.contains(path),
                Item::Session { .. } | Item::Suggested { .. } => true,
            });
        }
        self.append_suggested(&mut items);
        append_archived_section_by_project(
            &mut items,
            &grouped,
            self.archived_section_collapsed,
            &self.project_group_collapsed,
        );
        append_trash_section(&mut items, &grouped, self.trashed_section_collapsed);
        items
    }
}
