//! Group tree management

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::collections::HashMap;

use super::config::SortOrder;
use super::Instance;

/// Sentinel path used by the synthetic "Archived" sidebar section.
pub const ARCHIVED_SECTION_PATH: &str = "__aoe_archived_section__";
pub const ARCHIVED_SECTION_NAME: &str = "Archived";

/// Synthetic group path for the Trash shelf, sibling of the Archived section.
pub const TRASH_SECTION_PATH: &str = "__aoe_trash_section__";
pub const TRASH_SECTION_NAME: &str = "Trash";

/// Synthetic group identity for the scratch bucket in project mode.
pub const SCRATCH_GROUP_PATH: &str = "__aoe_scratch_group__";
/// Capitalized so the bucket reads as a system group rather than a repo basename, matching the web
/// sidebar's `Scratch` label.
pub const SCRATCH_GROUP_NAME: &str = "Scratch";

#[inline]
pub fn is_archived_section_path(path: &str) -> bool {
    path == ARCHIVED_SECTION_PATH
}

#[inline]
pub fn is_trash_section_path(path: &str) -> bool {
    path == TRASH_SECTION_PATH
}

/// Exact match for the synthetic scratch bucket's sentinel identity path.
#[inline]
fn is_scratch_group_path(path: &str) -> bool {
    path == SCRATCH_GROUP_PATH
}

/// True for the Trash section sentinel or anything nested under it. Mirrors
/// [`is_within_archived_section`] for the trash shelf.
#[inline]
pub fn is_within_trash_section(path: &str) -> bool {
    path == TRASH_SECTION_PATH || path.starts_with(&format!("{}/", TRASH_SECTION_PATH))
}

/// True for both the top-level Archived section sentinel and any synthetic child header pushed
/// under it (e.g. project sub-folders nested inside Archived in Project grouping mode).
#[inline]
pub fn is_within_archived_section(path: &str) -> bool {
    path == ARCHIVED_SECTION_PATH || path.starts_with(&format!("{}/", ARCHIVED_SECTION_PATH))
}

/// Build the synthetic sub-path for a per-project header rendered inside the Archived section.
#[inline]
pub fn archived_project_sub_path(project_name: &str) -> String {
    format!("{}/{}", ARCHIVED_SECTION_PATH, project_name)
}

/// True for any project-mode header that is synthetic rather than a real, pinnable repo: the
/// Archived/Trash shelves (and anything nested under them) and the scratch bucket.
#[inline]
pub fn is_synthetic_project_header(path: &str) -> bool {
    is_within_archived_section(path) || is_within_trash_section(path) || is_scratch_group_path(path)
}

/// Map a project-mode group identity key to its human display label, for the sites that render a
/// raw `group_path` to the user.
#[inline]
pub fn project_group_display_name(key: &str) -> &str {
    if is_scratch_group_path(key) {
        SCRATCH_GROUP_NAME
    } else {
        key
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Group {
    pub name: String,
    pub path: String,
    #[serde(default)]
    pub collapsed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<DateTime<Utc>>,
    #[serde(skip)]
    pub children: Vec<Group>,
}

impl Group {
    pub fn new(name: &str, path: &str) -> Self {
        Self {
            name: name.to_string(),
            path: path.to_string(),
            collapsed: false,
            archived_at: None,
            children: Vec::new(),
        }
    }

    pub fn is_archived(&self) -> bool {
        self.archived_at.is_some()
    }
}

#[derive(Debug, Clone)]
pub struct GroupTree {
    roots: Vec<Group>,
    groups_by_path: HashMap<String, Group>,
    /// Tracks the first-seen insertion order of group paths (used as a stable base for other sorts).
    insertion_order: Vec<String>,
}

impl GroupTree {
    pub fn new_with_groups(instances: &[Instance], existing_groups: &[Group]) -> Self {
        let mut tree = Self {
            roots: Vec::new(),
            groups_by_path: HashMap::new(),
            insertion_order: Vec::new(),
        };

        // Add existing groups in the order they appear on disk (preserves prior save order)
        for group in existing_groups {
            tree.groups_by_path
                .insert(group.path.clone(), group.clone());
            tree.insertion_order.push(group.path.clone());
        }

        // Ensure all instance groups exist
        for inst in instances {
            if !inst.group_path.is_empty() {
                tree.ensure_group_exists(&inst.group_path);
            }
        }

        // Build tree structure
        tree.rebuild_tree();

        tree
    }

    fn ensure_group_exists(&mut self, path: &str) {
        if self.groups_by_path.contains_key(path) {
            return;
        }

        // Create all parent groups
        let parts: Vec<&str> = path.split('/').collect();
        let mut current_path = String::new();

        for (i, part) in parts.iter().enumerate() {
            if i > 0 {
                current_path.push('/');
            }
            current_path.push_str(part);

            if !self.groups_by_path.contains_key(&current_path) {
                let group = Group::new(part, &current_path);
                self.groups_by_path.insert(current_path.clone(), group);
                self.insertion_order.push(current_path.clone());
            }
        }
    }

    fn rebuild_tree(&mut self) {
        self.roots.clear();

        // Build root groups in insertion order (no '/' in path); flatten_tree applies sort order.
        let root_paths: Vec<String> = self
            .insertion_order
            .iter()
            .filter(|p| self.groups_by_path.contains_key(*p) && !p.contains('/'))
            .cloned()
            .collect();

        let mut root_groups: Vec<Group> = root_paths
            .iter()
            .filter_map(|p| self.groups_by_path.get(p).cloned())
            .collect();

        for root in &mut root_groups {
            self.build_children(root);
        }

        self.roots = root_groups;
    }

    fn build_children(&self, parent: &mut Group) {
        let prefix = format!("{}/", parent.path);

        // Build children in insertion order
        let child_paths: Vec<String> = self
            .insertion_order
            .iter()
            .filter(|p| {
                self.groups_by_path.contains_key(*p)
                    && p.starts_with(&prefix)
                    && !p[prefix.len()..].contains('/')
            })
            .cloned()
            .collect();

        let mut children: Vec<Group> = child_paths
            .iter()
            .filter_map(|p| self.groups_by_path.get(p).cloned())
            .collect();

        for child in &mut children {
            self.build_children(child);
        }

        parent.children = children;
    }

    pub fn create_group(&mut self, path: &str) {
        self.ensure_group_exists(path);
        self.rebuild_tree();
    }

    pub fn delete_group(&mut self, path: &str) {
        // Remove group and all children
        let prefix = format!("{}/", path);
        let to_remove: Vec<String> = self
            .groups_by_path
            .keys()
            .filter(|p| *p == path || p.starts_with(&prefix))
            .cloned()
            .collect();

        for p in &to_remove {
            self.groups_by_path.remove(p);
        }
        self.insertion_order.retain(|p| !to_remove.contains(p));

        self.rebuild_tree();
    }

    pub fn group_exists(&self, path: &str) -> bool {
        self.groups_by_path.contains_key(path)
    }

    pub fn get_all_groups(&self) -> Vec<Group> {
        // Return in insertion order so groups.json preserves creation order
        self.insertion_order
            .iter()
            .filter_map(|p| self.groups_by_path.get(p).cloned())
            .collect()
    }

    pub fn get_roots(&self) -> &[Group] {
        &self.roots
    }

    pub fn toggle_collapsed(&mut self, path: &str) {
        if let Some(group) = self.groups_by_path.get_mut(path) {
            group.collapsed = !group.collapsed;
            self.rebuild_tree();
        }
    }

    pub fn set_collapsed(&mut self, path: &str, collapsed: bool) {
        if let Some(group) = self.groups_by_path.get_mut(path) {
            if group.collapsed != collapsed {
                group.collapsed = collapsed;
                self.rebuild_tree();
            }
        }
    }

    /// Toggle the archived state on the group itself.
    pub fn toggle_archived(&mut self, path: &str) -> Option<bool> {
        let new_state = {
            let group = self.groups_by_path.get_mut(path)?;
            if group.archived_at.is_some() {
                group.archived_at = None;
                false
            } else {
                group.archived_at = Some(Utc::now());
                true
            }
        };
        self.rebuild_tree();
        Some(new_state)
    }

    pub fn set_archived(&mut self, path: &str, archived: bool) {
        let mut changed = false;
        if let Some(group) = self.groups_by_path.get_mut(path) {
            // Skip redundant set_archived(true) so we don't churn the
            // timestamp on every UI re-archive.
            match (group.archived_at, archived) {
                (Some(_), true) | (None, false) => {}
                _ => {
                    group.archived_at = if archived { Some(Utc::now()) } else { None };
                    changed = true;
                }
            }
        }
        if changed {
            self.rebuild_tree();
        }
    }

    pub fn group_archived_at(&self, path: &str) -> Option<DateTime<Utc>> {
        self.groups_by_path.get(path).and_then(|g| g.archived_at)
    }

    /// Rename a group and all its descendants to a new path.
    /// If the target path already exists, the old group is merged into it.
    pub fn rename_group(&mut self, old_path: &str, new_path: &str) {
        if old_path == new_path || new_path.is_empty() {
            return;
        }

        let old_prefix = format!("{}/", old_path);

        // Collect all paths to rename: the group itself + descendants
        let paths_to_rename: Vec<String> = self
            .insertion_order
            .iter()
            .filter(|p| *p == old_path || p.starts_with(&old_prefix))
            .cloned()
            .collect();

        for old in &paths_to_rename {
            let new = if *old == old_path {
                new_path.to_string()
            } else {
                format!("{}{}", new_path, &old[old_path.len()..])
            };

            if let Some(mut group) = self.groups_by_path.remove(old) {
                if self.groups_by_path.contains_key(&new) {
                    // Target exists: merge (keep existing, drop old)
                } else {
                    // Derive new name from the last path segment
                    let new_name = new.rsplit('/').next().unwrap_or(&new).to_string();
                    group.name = new_name;
                    group.path = new.clone();
                    self.groups_by_path.insert(new.clone(), group);
                }
            }

            // Update insertion_order: replace old with new, or remove if merged
            if let Some(pos) = self.insertion_order.iter().position(|p| p == old) {
                if self.insertion_order.contains(&new) {
                    // Target already in order list (merge case)
                    self.insertion_order.remove(pos);
                } else {
                    self.insertion_order[pos] = new;
                }
            }
        }

        // Ensure all parent groups of new_path exist
        self.ensure_group_exists(new_path);

        self.rebuild_tree();
    }
}

/// Item represents either a group or an instance in the flattened tree view
#[derive(Debug, Clone)]
pub enum Item {
    Group {
        path: String,
        name: String,
        depth: usize,
        collapsed: bool,
        session_count: usize,
        /// Which profile this group belongs to (set in all-profiles mode)
        profile: Option<String>,
        /// When the group was archived (None = active).
        archived_at: Option<DateTime<Utc>>,
    },
    Session {
        id: String,
        depth: usize,
    },
}

impl Item {
    pub fn depth(&self) -> usize {
        match self {
            Item::Group { depth, .. } => *depth,
            Item::Session { depth, .. } => *depth,
        }
    }
}

fn sort_by_name<T, F>(items: &mut [T], sort_order: SortOrder, key: F)
where
    F: Fn(&T) -> &str,
{
    match sort_order {
        SortOrder::AZ => items.sort_by_key(|a| key(a).to_lowercase()),
        SortOrder::ZA => items.sort_by_key(|b| std::cmp::Reverse(key(b).to_lowercase())),
        SortOrder::Newest | SortOrder::Oldest | SortOrder::LastActivity | SortOrder::Attention => {}
    }
}

/// Sort a slice of session references by `sort_order`, reading the
/// favorites-first preference from config.
fn sort_sessions(sessions: &mut [&Instance], sort_order: SortOrder) {
    sort_sessions_inner(sessions, sort_order, crate::session::favorites_first());
}

/// Pure core of [`sort_sessions`]: takes the favorites-first flag explicitly so tests do not have
/// to mutate the process-global atomic, which would race with tests running in parallel.
fn sort_sessions_inner(sessions: &mut [&Instance], sort_order: SortOrder, favorites_first: bool) {
    match sort_order {
        SortOrder::Oldest => sessions.sort_by_key(|i| i.created_at),
        SortOrder::Newest => sessions.sort_by_key(|i| Reverse(i.created_at)),
        SortOrder::LastActivity => sessions.sort_by_key(|i| last_activity_session_key(i)),
        SortOrder::Attention => {
            sessions.sort_by_key(|i| attention_session_key(i));
            return;
        }
        SortOrder::AZ | SortOrder::ZA => sort_by_name(sessions, sort_order, |i| &i.title),
    }
    if favorites_first {
        sessions.sort_by_key(|i| !is_live_favorite(i));
    }
}

/// Sort a slice of group references by `sort_order`, using `instances` for timestamp-based
/// orderings.
fn sort_groups<T, N, P, A>(
    items: &mut [T],
    sort_order: SortOrder,
    instances: &[Instance],
    name: N,
    path: P,
    archived: A,
) where
    N: Fn(&T) -> &str,
    P: Fn(&T) -> &str,
    A: Fn(&T) -> Option<DateTime<Utc>>,
{
    sort_groups_inner(
        items,
        sort_order,
        instances,
        name,
        path,
        archived,
        crate::session::favorites_first(),
    );
}

/// Pure core of [`sort_groups`]. See [`sort_sessions_inner`] for why the
/// favorites bias is a second stable pass and why the flag is a parameter.
fn sort_groups_inner<T, N, P, A>(
    items: &mut [T],
    sort_order: SortOrder,
    instances: &[Instance],
    name: N,
    path: P,
    archived: A,
    favorites_first: bool,
) where
    N: Fn(&T) -> &str,
    P: Fn(&T) -> &str,
    A: Fn(&T) -> Option<DateTime<Utc>>,
{
    match sort_order {
        SortOrder::Oldest => {
            items.sort_by_key(|g| created_at_bounds(path(g), instances).1);
        }
        SortOrder::Newest => {
            items.sort_by_key(|g| Reverse(created_at_bounds(path(g), instances).0));
        }
        SortOrder::LastActivity => {
            items.sort_by_key(|g| last_activity_group_key(path(g), instances));
        }
        SortOrder::Attention => {
            items.sort_by_key(|g| attention_group_key(path(g), archived(g), instances));
            return;
        }
        SortOrder::AZ | SortOrder::ZA => sort_by_name(items, sort_order, name),
    }
    if favorites_first {
        items.sort_by_key(|g| !has_live_favorite(path(g), instances));
    }
}

/// Sessions (direct and nested) that belong to `path` and are not archived.
fn group_members<'a>(
    path: &'a str,
    instances: &'a [Instance],
) -> impl Iterator<Item = &'a Instance> + 'a {
    let prefix = format!("{}/", path);
    instances.iter().filter(move |i| {
        (i.group_path == path || i.group_path.starts_with(&prefix))
            && !i.is_archived()
            && !i.is_trashed()
    })
}

/// True for a favorited session that is actively pinnable: not archived, not trashed, and not
/// snoozed.
pub(crate) fn is_live_favorite(inst: &Instance) -> bool {
    !inst.is_archived() && !inst.is_trashed() && !inst.is_snoozed() && inst.is_favorited()
}

/// True when the group at `path` (including nested sub-groups) has at least one live favorited
/// member.
fn has_live_favorite(path: &str, instances: &[Instance]) -> bool {
    group_members(path, instances).any(is_live_favorite)
}

/// Newest and oldest `created_at` in a group, defaulted so an empty group sinks to
/// the bottom of either ordering.
fn created_at_bounds(path: &str, instances: &[Instance]) -> (DateTime<Utc>, DateTime<Utc>) {
    let stamps = || group_members(path, instances).map(|i| i.created_at);
    (
        stamps().max().unwrap_or(DateTime::<Utc>::MIN_UTC),
        stamps().min().unwrap_or(DateTime::<Utc>::MAX_UTC),
    )
}

/// Most recent `last_accessed_at` among all sessions (direct and nested) in a group.
fn max_last_accessed_in_group(path: &str, instances: &[Instance]) -> Option<DateTime<Utc>> {
    group_members(path, instances)
        .filter_map(|i| i.last_accessed_at)
        .max()
}

/// Key used to sort sessions by LastActivity in descending order, pushing sessions with no recorded
/// activity to the bottom.
fn last_activity_session_key(inst: &Instance) -> (bool, Reverse<Option<DateTime<Utc>>>) {
    (
        inst.last_accessed_at.is_none(),
        Reverse(inst.last_accessed_at),
    )
}

/// Key used to sort groups by LastActivity in descending order. Groups with no
/// activity sort to the bottom.
fn last_activity_group_key(
    path: &str,
    instances: &[Instance],
) -> (bool, Reverse<Option<DateTime<Utc>>>) {
    let ts = max_last_accessed_in_group(path, instances);
    (ts.is_none(), Reverse(ts))
}

/// Priority tier for the Attention sort.
fn attention_tier(inst: &Instance) -> u8 {
    use crate::session::Status::*;
    if inst.is_archived() || inst.is_snoozed() || inst.is_trashed() || inst.pane_dead_observed {
        // Tier 99 sinks: archived and snoozed (snoozed = temporary archive, wakes automatically
        // when timer expires).
        return 99;
    }
    match inst.status {
        Waiting => 0,                        // agent paused, needs human input, TOP priority
        Error => 1,                          // broken, needs attention
        Idle => 2,                           // turn complete, ready for next prompt
        Unknown => 3,                        // status undetermined, glance warranted
        Running => 4,                        // actively working, leave alone
        Stopped => 5, // dormant, sinks below running so the TUI shows live sessions first
        Starting | Creating | Deleting => 6, // transient, sink to bottom
    }
}

/// Attention bucket rank with the unread promoter folded in.
fn attention_rank(tier: u8, unread: bool, unread_enabled: bool) -> u8 {
    if tier == 99 {
        return 99;
    }
    if unread_enabled {
        if tier == 0 {
            0
        } else if unread {
            1
        } else {
            tier.saturating_add(1)
        }
    } else {
        tier
    }
}

/// Key used to sort sessions by Attention.
#[allow(clippy::type_complexity)]
fn attention_session_key(
    inst: &Instance,
) -> (
    bool,
    u8,
    bool,
    bool,
    std::cmp::Reverse<Option<DateTime<Utc>>>,
    Option<DateTime<Utc>>,
) {
    let tier = attention_tier(inst);
    // Urgent is the cross-tier promoter: an agent that has flagged itself urgent rises above all
    // non-urgent rows regardless of status tier.
    let urgent_bias = tier != 99 && inst.is_urgent();
    // Favorite pins to the top of its category; tier stays primary so a fav'd Running never leaps
    // above a plain Waiting.
    let favorite_bias = tier != 99 && inst.is_favorited();
    if tier == 99 {
        // Tier 99 unifies archived, snoozed, and pane-dead rows.
        let ts = inst
            .archived_at
            .or(inst.snoozed_until)
            .or(inst.last_accessed_at);
        return (
            !urgent_bias,
            tier,
            !favorite_bias,
            ts.is_none(),
            Reverse(ts),
            None,
        );
    }
    let rank = attention_rank(tier, inst.is_unread(), crate::session::unread_enabled());
    // Non-archived: "longest aging" = oldest last_accessed_at first (ASC).
    (
        !urgent_bias,
        rank,
        !favorite_bias,
        inst.last_accessed_at.is_none(),
        Reverse(None),
        inst.last_accessed_at,
    )
}

/// Key used to sort groups by Attention.
#[allow(clippy::type_complexity)]
fn attention_group_key(
    path: &str,
    group_archived_at: Option<DateTime<Utc>>,
    instances: &[Instance],
) -> (
    bool,
    u8,
    bool,
    bool,
    Reverse<Option<DateTime<Utc>>>,
    Option<DateTime<Utc>>,
) {
    let prefix = format!("{}/", path);
    let members: Vec<&Instance> = instances
        .iter()
        .filter(|i| i.group_path == path || i.group_path.starts_with(&prefix))
        .collect();

    if members.is_empty() {
        // Empty group: if marked archived, sink to tier 99 with the group's
        // own archived_at; otherwise leave at u8::MAX (existing behavior).
        if let Some(ts) = group_archived_at {
            return (true, 99, true, false, Reverse(Some(ts)), None);
        }
        return (true, u8::MAX, true, true, Reverse(None), None);
    }

    let min_tier = members
        .iter()
        .map(|i| attention_tier(i))
        .min()
        .unwrap_or(u8::MAX);

    // Group-level urgent bias: any non-sunk member with the urgent flag promotes the entire group
    // above non-urgent peers across all tiers.
    let urgent_bias = min_tier != 99 && members.iter().any(|i| i.is_urgent());

    // Group-level favorite bias: within its min_tier bucket, a group with any live favorited member
    // pins above peers.
    let favorite_bias = min_tier != 99 && has_live_favorite(path, instances);

    if min_tier == 99 {
        // All members archived: sort archived block by latest archived_at.
        let max_arch = members.iter().filter_map(|i| i.archived_at).max();
        let ts = max_arch.or(group_archived_at);
        return (
            !urgent_bias,
            99,
            !favorite_bias,
            ts.is_none(),
            Reverse(ts),
            None,
        );
    }

    // Non-archived: "longest aging" = oldest max(last_accessed_at) first.
    let max_last = members.iter().filter_map(|i| i.last_accessed_at).max();
    // Fold the unread promoter in at the group level too, so a project containing an unread session
    // floats up just like the flat Attention view does (a group with an unread Idle outranks a
    // group whose best member is a read Error).
    let unread = members
        .iter()
        .filter(|i| attention_tier(i) != 99)
        .any(|i| i.is_unread());
    let rank = attention_rank(min_tier, unread, crate::session::unread_enabled());
    (
        !urgent_bias,
        rank,
        !favorite_bias,
        max_last.is_none(),
        Reverse(None),
        max_last,
    )
}

/// Flatten instances from multiple profiles into a single flat list.
pub fn flatten_tree_all_profiles(
    instances: &[Instance],
    group_trees: &std::collections::HashMap<String, GroupTree>,
    sort_order: SortOrder,
) -> Vec<Item> {
    let mut items = Vec::new();

    // Archived sessions are excluded from the natural flow.
    let mut ungrouped: Vec<&Instance> = instances
        .iter()
        .filter(|i| i.group_path.is_empty() && !i.is_archived() && !i.is_trashed())
        .collect();

    sort_sessions(&mut ungrouped, sort_order);

    for inst in ungrouped {
        items.push(Item::Session {
            id: inst.id.clone(),
            depth: 0,
        });
    }

    // Collect and flatten groups from all profiles at depth 0
    let mut all_roots: Vec<(&str, &Group, Vec<Instance>)> = Vec::new();
    for (profile_name, tree) in group_trees {
        let profile_instances: Vec<Instance> = instances
            .iter()
            .filter(|i| i.source_profile == *profile_name)
            .cloned()
            .collect();
        for root in tree.get_roots() {
            all_roots.push((profile_name, root, profile_instances.clone()));
        }
    }

    // Sort using the per-profile instances stored in each tuple (element 2), not the global
    // instances slice, so groups from different profiles with the same name get sort keys scoped to
    // their own profile's sessions.
    match sort_order {
        SortOrder::Oldest => {
            all_roots.sort_by_key(|(_, g, insts)| created_at_bounds(&g.path, insts).1);
        }
        SortOrder::Newest => {
            all_roots.sort_by_key(|(_, g, insts)| Reverse(created_at_bounds(&g.path, insts).0));
        }
        SortOrder::LastActivity => {
            all_roots.sort_by_key(|(_, g, insts)| last_activity_group_key(&g.path, insts));
        }
        SortOrder::Attention => {
            all_roots
                .sort_by_key(|(_, g, insts)| attention_group_key(&g.path, g.archived_at, insts));
        }
        SortOrder::AZ | SortOrder::ZA => {
            sort_by_name(&mut all_roots, sort_order, |(_, g, _)| &*g.name)
        }
    }
    // Favorites-first second pass, matching `sort_groups_inner`.
    if sort_order != SortOrder::Attention && crate::session::favorites_first() {
        all_roots.sort_by_key(|(_, g, insts)| !has_live_favorite(&g.path, insts));
    }

    for (profile_name, root, profile_instances) in &all_roots {
        flatten_group(
            root,
            profile_instances,
            &mut items,
            0,
            sort_order,
            Some(profile_name),
        );
    }

    items
}

/// Flat session list for the Attention sort: skip group hierarchy entirely.
pub fn flatten_sessions_by_attention(instances: &[Instance]) -> Vec<Item> {
    // Archived rows are excluded from the natural attention flow; the caller appends them under the
    // synthetic "Archived" section via `append_archived_section`.
    let mut refs: Vec<&Instance> = instances
        .iter()
        .filter(|i| !i.is_archived() && !i.is_trashed())
        .collect();
    refs.sort_by_key(|i| attention_session_key(i));
    refs.into_iter()
        .map(|inst| Item::Session {
            id: inst.id.clone(),
            depth: 0,
        })
        .collect()
}

pub fn flatten_tree(
    group_tree: &GroupTree,
    instances: &[Instance],
    sort_order: SortOrder,
) -> Vec<Item> {
    let mut items = Vec::new();

    // Archived sessions are excluded from the natural flow.
    let mut ungrouped: Vec<&Instance> = instances
        .iter()
        .filter(|i| i.group_path.is_empty() && !i.is_archived() && !i.is_trashed())
        .collect();

    sort_sessions(&mut ungrouped, sort_order);

    for inst in ungrouped {
        items.push(Item::Session {
            id: inst.id.clone(),
            depth: 0,
        });
    }

    // Add groups and their sessions
    let roots = group_tree.get_roots();
    let mut roots_to_iterate: Vec<&Group> = roots.iter().collect();
    sort_groups(
        &mut roots_to_iterate,
        sort_order,
        instances,
        |g| &g.name,
        |g| &g.path,
        |g| g.archived_at,
    );

    for root in roots_to_iterate {
        flatten_group(root, instances, &mut items, 0, sort_order, None);
    }

    items
}

fn flatten_group(
    group: &Group,
    instances: &[Instance],
    items: &mut Vec<Item>,
    depth: usize,
    sort_order: SortOrder,
    profile: Option<&str>,
) {
    let session_count = count_sessions_in_group(&group.path, instances);

    items.push(Item::Group {
        path: group.path.clone(),
        name: group.name.clone(),
        depth,
        collapsed: group.collapsed,
        session_count,
        profile: profile.map(|s| s.to_string()),
        archived_at: group.archived_at,
    });

    if group.collapsed {
        return;
    }

    // Archived sessions are pulled out of the natural flow regardless of their group_path.
    let mut group_sessions: Vec<&Instance> = instances
        .iter()
        .filter(|i| i.group_path == group.path && !i.is_archived() && !i.is_trashed())
        .collect();

    sort_sessions(&mut group_sessions, sort_order);

    for inst in group_sessions {
        items.push(Item::Session {
            id: inst.id.clone(),
            depth: depth + 1,
        });
    }

    // Recursively add child groups (sort them if needed)
    let mut children_to_iterate: Vec<&Group> = group.children.iter().collect();
    sort_groups(
        &mut children_to_iterate,
        sort_order,
        instances,
        |g| &g.name,
        |g| &g.path,
        |g| g.archived_at,
    );

    for child in children_to_iterate {
        flatten_group(child, instances, items, depth + 1, sort_order, profile);
    }
}

/// Count of the sessions that render under a group header.
fn count_sessions_in_group(path: &str, instances: &[Instance]) -> usize {
    group_members(path, instances).count()
}

/// Append the synthetic "Archived" section to `items`, pinned to the bottom of the sidebar across
/// every sort mode.
/// Append one flat, reverse-chronological section: its header, then its sessions
/// unless the header is collapsed.
fn append_section(
    items: &mut Vec<Item>,
    mut members: Vec<&Instance>,
    path: &str,
    name: &str,
    collapsed: bool,
    stamp: impl Fn(&Instance) -> Option<DateTime<Utc>>,
) {
    if members.is_empty() {
        return;
    }
    members.sort_by_key(|i| Reverse(stamp(i)));
    items.push(Item::Group {
        path: path.to_string(),
        name: name.to_string(),
        depth: 0,
        collapsed,
        session_count: members.len(),
        profile: None,
        archived_at: None,
    });
    if collapsed {
        return;
    }
    items.extend(members.into_iter().map(|inst| Item::Session {
        id: inst.id.clone(),
        depth: 1,
    }));
}

pub fn append_archived_section(items: &mut Vec<Item>, instances: &[Instance], collapsed: bool) {
    let archived: Vec<&Instance> = instances
        .iter()
        .filter(|i| i.is_archived() && !i.is_trashed())
        .collect();
    append_section(
        items,
        archived,
        ARCHIVED_SECTION_PATH,
        ARCHIVED_SECTION_NAME,
        collapsed,
        |i| i.archived_at,
    );
}

pub fn append_trash_section(items: &mut Vec<Item>, instances: &[Instance], collapsed: bool) {
    let trashed: Vec<&Instance> = instances.iter().filter(|i| i.is_trashed()).collect();
    append_section(
        items,
        trashed,
        TRASH_SECTION_PATH,
        TRASH_SECTION_NAME,
        collapsed,
        |i| i.trashed_at,
    );
}

pub fn append_archived_section_by_project(
    items: &mut Vec<Item>,
    instances: &[Instance],
    section_collapsed: bool,
    project_collapsed: &HashMap<String, bool>,
    sort_order: SortOrder,
) {
    let archived: Vec<&Instance> = instances
        .iter()
        .filter(|i| i.is_archived() && !i.is_trashed())
        .collect();
    if archived.is_empty() {
        return;
    }

    items.push(Item::Group {
        path: ARCHIVED_SECTION_PATH.to_string(),
        name: ARCHIVED_SECTION_NAME.to_string(),
        depth: 0,
        collapsed: section_collapsed,
        session_count: archived.len(),
        profile: None,
        archived_at: None,
    });

    if section_collapsed {
        return;
    }

    let mut by_project: HashMap<String, Vec<&Instance>> = HashMap::new();
    for inst in archived {
        by_project
            .entry(inst.group_path.clone())
            .or_default()
            .push(inst);
    }

    let mut buckets: Vec<(String, Vec<&Instance>)> = by_project.into_iter().collect();
    sort_archived_project_buckets(&mut buckets, sort_order);

    for (project_name, mut sessions) in buckets {
        sessions.sort_by_key(|i| Reverse(i.archived_at));
        let sub_path = archived_project_sub_path(&project_name);
        let sub_collapsed = project_collapsed.get(&sub_path).copied().unwrap_or(false);
        items.push(Item::Group {
            path: sub_path,
            name: project_group_display_name(&project_name).to_string(),
            depth: 1,
            collapsed: sub_collapsed,
            session_count: sessions.len(),
            profile: None,
            archived_at: None,
        });
        if sub_collapsed {
            continue;
        }
        for inst in sessions {
            items.push(Item::Session {
                id: inst.id.clone(),
                depth: 2,
            });
        }
    }
}

/// Ordering helper for archived project sub-folders.
fn sort_archived_project_buckets(buckets: &mut [(String, Vec<&Instance>)], sort_order: SortOrder) {
    match sort_order {
        SortOrder::AZ => {
            buckets.sort_by_key(|b| project_group_display_name(&b.0).to_lowercase());
        }
        SortOrder::ZA => {
            buckets.sort_by_key(|b| Reverse(project_group_display_name(&b.0).to_lowercase()));
        }
        SortOrder::Oldest => {
            buckets.sort_by_key(|(_, sessions)| {
                sessions
                    .iter()
                    .filter_map(|i| i.archived_at)
                    .min()
                    .unwrap_or(DateTime::<Utc>::MAX_UTC)
            });
        }
        SortOrder::Newest | SortOrder::Attention => {
            buckets.sort_by_key(|(_, sessions)| {
                Reverse(
                    sessions
                        .iter()
                        .filter_map(|i| i.archived_at)
                        .max()
                        .unwrap_or(DateTime::<Utc>::MIN_UTC),
                )
            });
        }
        SortOrder::LastActivity => {
            buckets.sort_by_key(|(_, sessions)| {
                Reverse(
                    sessions
                        .iter()
                        .filter_map(|i| i.last_accessed_at)
                        .max()
                        .unwrap_or(DateTime::<Utc>::MIN_UTC),
                )
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Status;
    use chrono::Duration;

    fn inst(title: &str, group: &str) -> Instance {
        let mut inst = Instance::new(title, &format!("/tmp/{title}"));
        inst.group_path = group.to_string();
        inst
    }

    fn with_status(title: &str, group: &str, status: Status) -> Instance {
        let mut inst = inst(title, group);
        inst.status = status;
        inst
    }

    fn group_names(items: &[Item]) -> Vec<&str> {
        items
            .iter()
            .filter_map(|i| match i {
                Item::Group { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect()
    }

    fn session_titles<'a>(items: &[Item], instances: &'a [Instance]) -> Vec<&'a str> {
        items
            .iter()
            .filter_map(|i| match i {
                Item::Session { id, .. } => instances
                    .iter()
                    .find(|inst| &inst.id == id)
                    .map(|inst| inst.title.as_str()),
                _ => None,
            })
            .collect()
    }

    fn group_item<'a>(items: &'a [Item], wanted: &str) -> &'a Item {
        items
            .iter()
            .find(|i| matches!(i, Item::Group { path, .. } if path == wanted))
            .unwrap_or_else(|| panic!("group {wanted} missing"))
    }

    fn titles<'a>(sessions: &[&'a Instance]) -> Vec<&'a str> {
        sessions.iter().map(|i| i.title.as_str()).collect()
    }

    #[test]
    fn tree_creation_mutation_and_insertion_order() {
        let instances = vec![
            inst("test1", "work"),
            inst("test2", "work/frontend"),
            inst("test3", "personal"),
        ];
        let mut tree = GroupTree::new_with_groups(&instances, &[]);
        for path in ["work", "work/frontend", "personal"] {
            assert!(tree.group_exists(path), "{path}");
        }
        assert!(!tree.group_exists("nonexistent"));
        let roots: Vec<_> = tree.get_roots().iter().map(|g| g.name.as_str()).collect();
        assert_eq!(roots, ["work", "personal"]);

        tree.create_group("a/b/c");
        assert!(tree.group_exists("a") && tree.group_exists("a/b") && tree.group_exists("a/b/c"));
        tree.delete_group("work");
        assert!(!tree.group_exists("work") && !tree.group_exists("work/frontend"));
        tree.toggle_collapsed("nonexistent");

        let names = |tree: &GroupTree| -> Vec<String> {
            tree.get_all_groups().into_iter().map(|g| g.name).collect()
        };
        let instances = vec![inst("a", "alpha"), inst("b", "beta"), inst("g", "gamma")];
        let mut tree = GroupTree::new_with_groups(&instances, &[]);
        tree.delete_group("beta");
        tree.create_group("zeta");
        assert_eq!(names(&tree), ["alpha", "gamma", "zeta"]);

        let existing = [Group::new("gamma", "gamma"), Group::new("alpha", "alpha")];
        assert_eq!(
            names(&GroupTree::new_with_groups(&[], &existing)),
            ["gamma", "alpha"]
        );
    }

    #[test]
    fn collapse_hides_descendants_but_not_the_group() {
        let instances = vec![
            inst("parent-session", "parent"),
            inst("child-session", "parent/child"),
        ];
        let mut tree = GroupTree::new_with_groups(&instances, &[]);
        let count_sessions = |items: &[Item]| {
            items
                .iter()
                .filter(|i| matches!(i, Item::Session { .. }))
                .count()
        };

        let items = flatten_tree(&tree, &instances, SortOrder::Oldest);
        assert_eq!(group_names(&items), ["parent", "child"]);
        assert_eq!(count_sessions(&items), 2);
        let Item::Group {
            collapsed,
            session_count,
            depth,
            ..
        } = group_item(&items, "parent")
        else {
            unreachable!()
        };
        assert_eq!((*collapsed, *session_count, *depth), (false, 2, 0));
        assert!(matches!(
            group_item(&items, "parent/child"),
            Item::Group { depth: 1, .. }
        ));

        tree.toggle_collapsed("parent");
        let items = flatten_tree(&tree, &instances, SortOrder::Oldest);
        assert_eq!(group_names(&items), ["parent"]);
        assert_eq!(count_sessions(&items), 0);
        assert!(matches!(
            group_item(&items, "parent"),
            Item::Group {
                collapsed: true,
                ..
            }
        ));

        tree.toggle_collapsed("parent");
        assert!(!tree.groups_by_path["parent"].collapsed);
    }

    #[test]
    fn session_count_excludes_trashed_and_restores_on_untrash() {
        let mut instances = vec![inst("a", "work"), inst("b", "work")];
        let group_count = |instances: &[Instance]| {
            let tree = GroupTree::new_with_groups(instances, &[]);
            let items = flatten_tree(&tree, instances, SortOrder::Oldest);
            match group_item(&items, "work") {
                Item::Group { session_count, .. } => *session_count,
                _ => unreachable!(),
            }
        };
        assert_eq!(group_count(&instances), 2);
        instances[0].trash();
        assert_eq!(group_count(&instances), 1);
        instances[0].untrash();
        assert_eq!(group_count(&instances), 2);
    }

    #[test]
    fn rename_group() {
        let instances = vec![
            inst("test1", "work"),
            inst("test2", "work/frontend"),
            inst("test3", "existing"),
        ];
        let mut tree = GroupTree::new_with_groups(&instances, &[]);
        tree.rename_group("work", "work");
        tree.rename_group("work", "");
        assert!(tree.group_exists("work"), "same or empty target is a no-op");

        tree.rename_group("work", "projects");
        assert!(!tree.group_exists("work") && !tree.group_exists("work/frontend"));
        assert!(tree.group_exists("projects/frontend"));
        assert_eq!(tree.groups_by_path["projects"].name, "projects");

        tree.rename_group("projects", "existing");
        assert!(!tree.group_exists("projects") && tree.group_exists("existing"));
    }

    #[test]
    fn name_sorts_apply_to_groups_and_sessions() {
        let groups = vec![inst("z", "zebra"), inst("a", "apple"), inst("m", "Mango")];
        let tree = GroupTree::new_with_groups(&groups, &[]);
        let nested = vec![
            inst("p", "parent"),
            inst("z", "parent/zeta"),
            inst("a", "parent/alpha"),
        ];
        let nested_tree = GroupTree::new_with_groups(&nested, &[]);
        for (order, want_groups, want_children) in [
            (
                SortOrder::Oldest,
                ["zebra", "apple", "Mango"],
                ["zeta", "alpha"],
            ),
            (
                SortOrder::AZ,
                ["apple", "Mango", "zebra"],
                ["alpha", "zeta"],
            ),
            (
                SortOrder::ZA,
                ["zebra", "Mango", "apple"],
                ["zeta", "alpha"],
            ),
        ] {
            let items = flatten_tree(&tree, &groups, order);
            assert_eq!(group_names(&items), want_groups, "{order:?}");
            let items = flatten_tree(&nested_tree, &nested, order);
            assert_eq!(group_names(&items)[1..], want_children, "{order:?}");
        }

        for group in ["", "work"] {
            let sessions = vec![
                inst("Mango", group),
                inst("Apple", group),
                inst("Zebra", group),
            ];
            let tree = GroupTree::new_with_groups(&sessions, &[]);
            for (order, want) in [
                (SortOrder::Oldest, ["Mango", "Apple", "Zebra"]),
                (SortOrder::AZ, ["Apple", "Mango", "Zebra"]),
                (SortOrder::ZA, ["Zebra", "Mango", "Apple"]),
            ] {
                let items = flatten_tree(&tree, &sessions, order);
                assert_eq!(
                    session_titles(&items, &sessions),
                    want,
                    "{group:?} {order:?}"
                );
            }
        }
    }

    #[test]
    fn sort_order_cycles() {
        use SortOrder::*;
        let order = [Newest, Attention, LastActivity, Oldest, AZ, ZA];
        for (i, current) in order.iter().enumerate() {
            let next = order[(i + 1) % order.len()];
            assert_eq!(current.cycle(), next);
            assert_eq!(next.cycle_reverse(), *current);
        }
    }

    #[test]
    fn last_activity_sorts_descending_with_none_last() {
        let now = Utc::now();
        let mut recent = inst("recent", "");
        recent.last_accessed_at = Some(now);
        let mut older = inst("older", "");
        older.last_accessed_at = Some(now - Duration::hours(1));
        let instances = vec![inst("never", ""), older, recent];
        let tree = GroupTree::new_with_groups(&instances, &[]);
        let items = flatten_tree(&tree, &instances, SortOrder::LastActivity);
        assert_eq!(
            session_titles(&items, &instances),
            ["recent", "older", "never"]
        );
    }

    #[test]
    fn attention_tiers_archive_and_snooze() {
        let mut waiting = with_status("w", "", Status::Waiting);
        assert_eq!(attention_tier(&waiting), 0);
        waiting.archive();
        assert_eq!(attention_tier(&waiting), 99);

        let mut errored = with_status("e", "", Status::Error);
        assert_eq!(attention_tier(&errored), 1);
        errored.archive();
        assert_eq!(attention_tier(&errored), 99);
        errored.unarchive();
        assert_eq!(attention_tier(&errored), 1);

        let mut snoozed = with_status("s", "", Status::Waiting);
        snoozed.snooze(30);
        assert!(snoozed.is_snoozed() && snoozed.snooze_remaining().is_some());
        assert_eq!(attention_tier(&snoozed), 99);
        snoozed.archive();
        assert_eq!(attention_tier(&snoozed), 99);
        // `archive` clears the snooze and settles the live status, so restore
        // both before checking what an expired snooze alone does.
        assert!(snoozed.snoozed_until.is_none());
        snoozed.unarchive();
        snoozed.status = Status::Waiting;
        snoozed.snoozed_until = Some(Utc::now() - Duration::seconds(1));
        assert!(!snoozed.is_snoozed() && snoozed.snooze_remaining().is_none());
        assert_eq!(
            attention_tier(&snoozed),
            0,
            "an expired snooze releases the tier"
        );
        snoozed.snooze(30);
        snoozed.unsnooze();
        assert!(snoozed.snoozed_until.is_none());
    }

    #[test]
    fn attention_session_sort() {
        let now = Utc::now();
        let idle_at = |title: &str, ago: Option<Duration>| {
            let mut i = with_status(title, "", Status::Idle);
            i.last_accessed_at = ago.map(|ago| now - ago);
            i
        };
        let fresh = idle_at("fresh", Some(Duration::minutes(5)));
        let stale = idle_at("stale", Some(Duration::hours(11)));
        let middle = idle_at("middle", Some(Duration::minutes(40)));
        let untouched = idle_at("untouched", None);
        let mut sessions = vec![&fresh, &middle, &stale, &untouched];
        sort_sessions(&mut sessions, SortOrder::Attention);
        assert_eq!(titles(&sessions), ["stale", "middle", "fresh", "untouched"]);

        let waiting = with_status("w", "", Status::Waiting);
        let errored = with_status("e", "", Status::Error);
        let mut archived = with_status("aw", "", Status::Waiting);
        archived.archive();
        let idle = with_status("i", "", Status::Idle);
        let mut sessions = vec![&waiting, &errored, &archived, &idle];
        sort_sessions(&mut sessions, SortOrder::Attention);
        assert_eq!(titles(&sessions), ["w", "e", "i", "aw"]);
    }

    #[test]
    fn attention_rank_unread_promoter() {
        assert!(attention_rank(0, false, true) < attention_rank(2, true, true));
        assert!(attention_rank(2, true, true) < attention_rank(1, false, true));
        assert!(attention_rank(2, true, true) < attention_rank(4, false, true));
        for tier in [0, 2, 4] {
            assert_eq!(
                attention_rank(tier, true, false),
                tier,
                "indicator disabled"
            );
        }
        assert_eq!(
            attention_rank(99, true, true),
            99,
            "unread must not promote sunk rows"
        );
        assert_eq!(attention_rank(99, false, true), 99);
    }

    #[test]
    fn attention_group_key_cases() {
        let now = Utc::now();
        let mut fresh = with_status("f", "fresh", Status::Idle);
        fresh.last_accessed_at = Some(now - Duration::minutes(5));
        let mut stale = with_status("s", "stale", Status::Idle);
        stale.last_accessed_at = Some(now - Duration::hours(11));
        let instances = vec![fresh, stale];
        assert!(
            attention_group_key("stale", None, &instances)
                < attention_group_key("fresh", None, &instances)
        );

        let archived = |title: &str, status: Status| {
            let mut i = with_status(title, "work", status);
            i.archive();
            i
        };
        let all_archived = [archived("a", Status::Waiting), archived("b", Status::Idle)];
        assert_eq!(attention_group_key("work", None, &all_archived).1, 99);
        let one_active = [
            archived("aw", Status::Waiting),
            with_status("ai", "work", Status::Idle),
        ];
        assert_eq!(attention_group_key("work", None, &one_active).1, 3);

        let err = with_status("err", "a", Status::Error);
        let mut unread_idle = with_status("ui", "b", Status::Idle);
        unread_idle.mark_unread();
        assert!(
            attention_group_key("b", None, &[unread_idle]) < attention_group_key("a", None, &[err]),
            "an unread Idle group outranks a read Error group"
        );

        let active_read = with_status("ar", "work", Status::Idle);
        let mut archived_unread = with_status("au", "work", Status::Idle);
        archived_unread.mark_unread();
        archived_unread.archive();
        let key = attention_group_key("work", None, &[active_read.clone(), archived_unread]);
        let baseline =
            attention_group_key("work", None, &[active_read, archived("o", Status::Idle)]);
        assert_eq!(
            key.1, baseline.1,
            "an archived unread member must not promote"
        );

        assert_eq!(attention_group_key("empty", Some(now), &[]).1, 99);
        assert_eq!(attention_group_key("empty", None, &[]).1, u8::MAX);
    }

    #[test]
    fn favorite_pins_within_attention_tier_only() {
        for status in [Status::Waiting, Status::Running, Status::Stopped] {
            let mut fav = with_status("fav", "", status);
            fav.favorite();
            let plain = with_status("plain", "", status);
            assert!(
                attention_session_key(&fav) < attention_session_key(&plain),
                "{status:?}"
            );
        }
        let mut fav_idle = with_status("fav_idle", "", Status::Idle);
        fav_idle.favorite();
        let plain_waiting = with_status("plain_waiting", "", Status::Waiting);
        assert!(attention_session_key(&plain_waiting) < attention_session_key(&fav_idle));

        let instances = [fav_idle, plain_waiting];
        for favorites_first in [true, false] {
            let mut refs: Vec<&Instance> = instances.iter().collect();
            sort_sessions_inner(&mut refs, SortOrder::Attention, favorites_first);
            assert_eq!(titles(&refs), ["plain_waiting", "fav_idle"]);
        }

        let mut archived = with_status("t", "", Status::Waiting);
        archived.favorite();
        archived.archive();
        assert!(!archived.is_favorited(), "archive clears the favorite");
        let key = attention_session_key(&archived);
        assert_eq!(
            (key.1, key.2),
            (99, true),
            "sunk tier without favorite bias"
        );
    }

    #[test]
    fn live_favorite_excludes_snoozed_archived_and_trashed() {
        let now = Utc::now();
        let mut fav = inst("fav", "work");
        fav.favorite();
        assert!(is_live_favorite(&fav));
        assert!(has_live_favorite("work", std::slice::from_ref(&fav)));
        let mut nested = inst("nested", "work/frontend");
        nested.favorite();
        assert!(has_live_favorite("work", std::slice::from_ref(&nested)));
        assert!(!has_live_favorite(
            "personal",
            std::slice::from_ref(&nested)
        ));
        assert!(!has_live_favorite("work", &[inst("plain", "work")]));

        fav.snooze(60);
        assert!(fav.is_favorited(), "snooze must not clear the star");
        assert!(!is_live_favorite(&fav));
        assert!(!has_live_favorite("work", std::slice::from_ref(&fav)));

        let mut archived = inst("archived", "");
        archived.favorited_at = Some(now);
        archived.archived_at = Some(now);
        let mut trashed = inst("trashed", "");
        trashed.favorited_at = Some(now);
        trashed.trashed_at = Some(now);
        assert!(!is_live_favorite(&archived) && !is_live_favorite(&trashed));
    }

    #[test]
    fn favorites_first_session_sorts() {
        let aged = |title: &str, days: i64, favorite: bool| {
            let mut i = inst(title, "");
            i.created_at = Utc::now() - Duration::days(days);
            if favorite {
                i.favorite();
            }
            i
        };
        let instances = [
            aged("zebra", 10, true),
            aged("yak", 0, true),
            aged("apple", 1, false),
        ];
        for (order, favorites_first, want) in [
            (SortOrder::Newest, false, ["yak", "apple", "zebra"]),
            (SortOrder::Newest, true, ["yak", "zebra", "apple"]),
            (SortOrder::AZ, false, ["apple", "yak", "zebra"]),
            (SortOrder::AZ, true, ["yak", "zebra", "apple"]),
        ] {
            let mut refs: Vec<&Instance> = instances.iter().collect();
            sort_sessions_inner(&mut refs, order, favorites_first);
            assert_eq!(titles(&refs), want, "{order:?} {favorites_first}");
        }

        let mut snoozed = aged("snoozed_fav", 10, true);
        snoozed.snooze(60);
        let pair = [snoozed, aged("new_plain", 0, false)];
        let mut refs: Vec<&Instance> = pair.iter().collect();
        sort_sessions_inner(&mut refs, SortOrder::Newest, true);
        assert_eq!(refs[0].title, "new_plain", "snooze outranks the star");
    }

    #[test]
    fn favorites_first_group_sorts() {
        let member = |title: &str, group: &str, days: i64| {
            let mut i = inst(title, group);
            i.created_at = Utc::now() - Duration::days(days);
            i
        };
        let sorted_first = |instances: &[Instance], favorites_first: bool| {
            let mut items = vec![Group::new("old", "old"), Group::new("new", "new")];
            sort_groups_inner(
                &mut items,
                SortOrder::Newest,
                instances,
                |g: &Group| g.name.as_str(),
                |g: &Group| g.path.as_str(),
                |_: &Group| None,
                favorites_first,
            );
            items[0].path.clone()
        };
        let mut old_fav = member("old_fav", "old", 10);
        old_fav.favorite();
        let instances = vec![old_fav, member("new_plain", "new", 0)];
        assert_eq!(sorted_first(&instances, false), "new");
        assert_eq!(sorted_first(&instances, true), "old");

        let mut archived = instances.clone();
        archived[0].archive();
        archived[0].favorited_at = Some(Utc::now());
        assert_eq!(
            sorted_first(&archived, true),
            "new",
            "archived favorite must not pin"
        );
    }

    #[test]
    #[serial_test::serial]
    fn favorites_first_pins_root_groups_across_profiles() {
        let _flag = crate::session::test_support::FavoritesFirstGuard::new();
        let mut old_fav = inst("old_fav", "old");
        old_fav.source_profile = "p1".to_string();
        old_fav.created_at = Utc::now() - Duration::days(10);
        old_fav.favorite();
        let mut new_plain = inst("new_plain", "new");
        new_plain.source_profile = "p2".to_string();

        let tree = |inst: &Instance| GroupTree::new_with_groups(std::slice::from_ref(inst), &[]);
        let trees = std::collections::HashMap::from([
            ("p1".to_string(), tree(&old_fav)),
            ("p2".to_string(), tree(&new_plain)),
        ]);
        let instances = vec![old_fav, new_plain];
        for (enabled, want) in [(false, "new"), (true, "old")] {
            crate::session::set_favorites_first(enabled);
            let items = flatten_tree_all_profiles(&instances, &trees, SortOrder::Newest);
            assert_eq!(group_names(&items)[0], want, "favorites_first={enabled}");
        }
    }

    #[test]
    fn favorite_archive_snooze_interactions() {
        let mut inst = inst("t", "");
        inst.archive();
        inst.favorite();
        assert!(
            inst.is_favorited() && !inst.is_archived(),
            "favorite clears archive"
        );
        inst.snooze(30);
        inst.favorite();
        assert!(!inst.is_snoozed(), "favorite clears snooze");

        inst.snooze(30);
        inst.archived_at = Some(Utc::now());
        inst.touch_last_accessed();
        assert!(
            !inst.is_archived() && !inst.is_snoozed(),
            "interaction wakes the session"
        );
        assert!(inst.is_favorited() && inst.last_accessed_at.is_some());
    }

    #[test]
    fn optional_markers_serialize_only_when_set() {
        let plain = serde_json::to_string(&Instance::new("t", "/tmp/t")).unwrap();
        for field in ["archived_at", "snoozed_until"] {
            assert!(!plain.contains(field), "{field} must be omitted when None");
        }
        let legacy = r#"{"id":"abc","title":"old","project_path":"/tmp/old","created_at":"2026-01-01T00:00:00Z"}"#;
        let legacy: Instance = serde_json::from_str(legacy).unwrap();
        assert!(!legacy.is_archived() && legacy.archived_at.is_none());

        let mut archived = Instance::new("t", "/tmp/t");
        archived.archive();
        let mut snoozed = Instance::new("t", "/tmp/t");
        snoozed.snooze(30);
        let mut favorited = Instance::new("t", "/tmp/t");
        favorited.favorite();
        type Case = (Instance, &'static str, fn(&Instance) -> bool);
        let cases: [Case; 3] = [
            (archived, "archived_at", Instance::is_archived),
            (snoozed, "snoozed_until", Instance::is_snoozed),
            (favorited, "favorited_at", Instance::is_favorited),
        ];
        for (inst, field, check) in cases {
            let json = serde_json::to_string(&inst).unwrap();
            assert!(json.contains(field));
            let parsed: Instance = serde_json::from_str(&json).unwrap();
            assert!(check(&parsed), "{field} round-trips");
        }
    }

    #[test]
    fn archived_project_buckets_name_sort_uses_display_label() {
        let myrepo = Instance::new("a", "/repos/myrepo");
        let throwaway = Instance::new("b", "/app/scratch/x");
        let zeta = Instance::new("c", "/repos/zeta");
        let cases = [
            (SortOrder::AZ, ["myrepo", SCRATCH_GROUP_NAME, "zeta"]),
            (SortOrder::ZA, ["zeta", SCRATCH_GROUP_NAME, "myrepo"]),
        ];
        for (order, expected) in cases {
            let mut buckets: Vec<(String, Vec<&Instance>)> = vec![
                ("myrepo".to_string(), vec![&myrepo]),
                (SCRATCH_GROUP_PATH.to_string(), vec![&throwaway]),
                ("zeta".to_string(), vec![&zeta]),
            ];
            sort_archived_project_buckets(&mut buckets, order);
            let labels: Vec<&str> = buckets
                .iter()
                .map(|b| project_group_display_name(&b.0))
                .collect();
            assert_eq!(labels, expected, "{order:?}");
        }
    }
}
