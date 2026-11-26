//! Group tree management

use crate::time::Timestamp;
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
    pub archived_at: Option<Timestamp>,
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
        Self::new_with_group_paths(
            instances
                .iter()
                .map(|instance| instance.group_path.as_str()),
            existing_groups,
        )
    }

    pub(crate) fn new_with_group_paths<'a>(
        paths: impl IntoIterator<Item = &'a str>,
        existing_groups: &[Group],
    ) -> Self {
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

        for path in paths {
            if !path.is_empty() {
                tree.ensure_group_exists(path);
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

    /// Add `groups` after every existing one, keeping their display names; existing paths are skipped.
    pub fn append_groups(&mut self, groups: impl IntoIterator<Item = Group>) {
        for group in groups {
            if self.groups_by_path.contains_key(&group.path) {
                continue;
            }
            self.insertion_order.push(group.path.clone());
            self.groups_by_path.insert(group.path.clone(), group);
        }
        self.rebuild_tree();
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

    /// Move `path` one slot toward the start (`delta < 0`) or end (`delta > 0`) among the
    /// groups sharing its parent, which is the order `get_all_groups` and therefore
    /// `groups.json` preserve. With `shown`, only the siblings it names count, so hidden
    /// ones keep their slots. Returns false when it is already at that end, or unknown.
    pub fn move_group(&mut self, path: &str, delta: isize, shown: Option<&[String]>) -> bool {
        let parent = |p: &str| p.rsplit_once('/').map(|(head, _)| head.to_string());
        let Some(anchor_parent) = self.groups_by_path.get(path).map(|_| parent(path)) else {
            return false;
        };
        let siblings: Vec<usize> = self
            .insertion_order
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                parent(p) == anchor_parent
                    && (*p == path || shown.is_none_or(|shown| shown.contains(p)))
            })
            .map(|(idx, _)| idx)
            .collect();
        let Some(at) = siblings
            .iter()
            .position(|idx| self.insertion_order[*idx] == path)
        else {
            return false;
        };
        let Some(target) = at.checked_add_signed(delta).filter(|t| *t < siblings.len()) else {
            return false;
        };
        self.insertion_order.swap(siblings[at], siblings[target]);
        self.rebuild_tree();
        true
    }

    /// Put `path` and its siblings named in `shown` in that order, each in one of the
    /// slots they held. Entries that are not siblings are ignored.
    pub fn arrange_siblings(&mut self, path: &str, shown: &[String]) {
        let parent = |p: &str| p.rsplit_once('/').map(|(head, _)| head.to_string());
        let anchor_parent = parent(path);
        let mut wanted: Vec<&String> = Vec::new();
        for entry in shown {
            if self.groups_by_path.contains_key(entry)
                && parent(entry) == anchor_parent
                && !wanted.contains(&entry)
            {
                wanted.push(entry);
            }
        }
        let slots: Vec<usize> = self
            .insertion_order
            .iter()
            .enumerate()
            .filter(|(_, p)| wanted.contains(p))
            .map(|(idx, _)| idx)
            .collect();
        for (slot, entry) in slots.into_iter().zip(wanted) {
            self.insertion_order[slot] = entry.clone();
        }
        self.rebuild_tree();
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
                group.archived_at = Some(Timestamp::now());
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
                    group.archived_at = if archived {
                        Some(Timestamp::now())
                    } else {
                        None
                    };
                    changed = true;
                }
            }
        }
        if changed {
            self.rebuild_tree();
        }
    }

    pub fn group_archived_at(&self, path: &str) -> Option<Timestamp> {
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
        archived_at: Option<Timestamp>,
    },
    Session {
        id: String,
        depth: usize,
    },
    /// The Suggested row, just above Archived and Trash: `total` listed suggestions, and
    /// whether a provider failed to refresh. Its list shows in the preview pane.
    Suggested {
        total: usize,
        failed: bool,
    },
}

impl Item {
    pub fn depth(&self) -> usize {
        match self {
            Item::Group { depth, .. } => *depth,
            Item::Session { depth, .. } => *depth,
            Item::Suggested { .. } => 0,
        }
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
        SortOrder::Custom => {
            // The user placed these rows by hand; nothing else may reorder them, not even the
            // favorites pass. Rows never moved sit on top, newest first.
            sessions.sort_by_key(|i| i.custom_order_key());
            return;
        }
        SortOrder::Newest => sessions.sort_by_key(|i| Reverse(i.created_at)),
        SortOrder::Name => sessions.sort_by_key(|i| i.title.to_lowercase()),
    }
    if favorites_first {
        sessions.sort_by_key(|i| !holds_favorite_place(i));
    }
}

/// Sort group references by `sort_order`. Under Custom they keep the order they arrive in.
fn sort_groups<T, N, P>(
    items: &mut [T],
    sort_order: SortOrder,
    instances: &[Instance],
    name: N,
    path: P,
) where
    N: Fn(&T) -> &str,
    P: Fn(&T) -> &str,
{
    sort_groups_inner(
        items,
        sort_order,
        instances,
        name,
        path,
        crate::session::favorites_first(),
    );
}

/// Pure core of [`sort_groups`]. See [`sort_sessions_inner`] for why the
/// favorites bias is a second stable pass and why the flag is a parameter.
fn sort_groups_inner<T, N, P>(
    items: &mut [T],
    sort_order: SortOrder,
    instances: &[Instance],
    name: N,
    path: P,
    favorites_first: bool,
) where
    N: Fn(&T) -> &str,
    P: Fn(&T) -> &str,
{
    match sort_order {
        SortOrder::Custom => return,
        SortOrder::Newest => items.sort_by_key(|g| Reverse(newest_member(path(g), instances))),
        SortOrder::Name => items.sort_by_key(|g| name(g).to_lowercase()),
    }
    if favorites_first {
        items.sort_by_key(|g| !has_placed_favorite(path(g), instances));
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

/// Creation time of the newest session under `path`; an empty group sorts last.
fn newest_member(path: &str, instances: &[Instance]) -> Option<Timestamp> {
    group_members(path, instances).map(|i| i.created_at).max()
}

/// True for a favorited session that is actively pinnable: not archived, not trashed, not
/// snoozed, and not ignored.
pub(crate) fn is_live_favorite(inst: &Instance) -> bool {
    !inst.is_archived()
        && !inst.is_trashed()
        && !inst.is_snoozed()
        && !inst.is_ignored()
        && inst.is_favorited()
}

/// True for a favorite that keeps its pinned place. Unlike [`is_live_favorite`], a snoozed
/// favorite holds its place, so snoozing and waking never move rows.
fn holds_favorite_place(inst: &Instance) -> bool {
    !inst.is_archived() && !inst.is_trashed() && !inst.is_ignored() && inst.is_favorited()
}

/// True when the group at `path` (including nested sub-groups) has a member that
/// [`holds_favorite_place`].
fn has_placed_favorite(path: &str, instances: &[Instance]) -> bool {
    group_members(path, instances).any(holds_favorite_place)
}

/// Flatten instances from multiple profiles into a single flat list.
/// Every profile's rows from `instances`, with headers ranked by `ranked`'s members, so a
/// filter that hides rows leaves the headers in place.
pub fn flatten_tree_all_profiles(
    instances: &[Instance],
    ranked: &[Instance],
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
    let mut all_roots: Vec<(&str, &Group, Vec<Instance>, Vec<Instance>)> = Vec::new();
    // Iterating the map directly would order the profile blocks differently on every rebuild
    // under Custom, which keeps whatever order it is handed.
    let mut profiles: Vec<(&String, &GroupTree)> = group_trees.iter().collect();
    profiles.sort_by(|a, b| a.0.cmp(b.0));
    for (profile_name, tree) in profiles {
        let of_profile = |pool: &[Instance]| -> Vec<Instance> {
            pool.iter()
                .filter(|i| i.source_profile == *profile_name)
                .cloned()
                .collect()
        };
        let (listed, ranked) = (of_profile(instances), of_profile(ranked));
        for root in tree.get_roots() {
            all_roots.push((profile_name, root, listed.clone(), ranked.clone()));
        }
    }

    // Favorites are scoped to each root's own profile, so same-named groups in two profiles
    // never borrow each other's members.
    match sort_order {
        SortOrder::Custom => {}
        SortOrder::Newest => {
            all_roots.sort_by_key(|(_, g, _, ranked)| Reverse(newest_member(&g.path, ranked)));
        }
        SortOrder::Name => all_roots.sort_by_key(|(_, g, _, _)| g.name.to_lowercase()),
    }
    if sort_order != SortOrder::Custom && crate::session::favorites_first() {
        all_roots.sort_by_key(|(_, g, _, ranked)| !has_placed_favorite(&g.path, ranked));
    }

    for (profile_name, root, listed, ranked) in &all_roots {
        flatten_group(
            root,
            listed,
            ranked,
            &mut items,
            0,
            sort_order,
            Some(profile_name),
        );
    }

    items
}

/// The rows from `instances`, with headers ranked by `ranked`'s members, so a filter that
/// hides rows leaves the headers in place.
pub fn flatten_tree(
    group_tree: &GroupTree,
    instances: &[Instance],
    ranked: &[Instance],
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
        ranked,
        |g| &g.name,
        |g| &g.path,
    );

    for root in roots_to_iterate {
        flatten_group(root, instances, ranked, &mut items, 0, sort_order, None);
    }

    items
}

fn flatten_group(
    group: &Group,
    instances: &[Instance],
    ranked: &[Instance],
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
        ranked,
        |g| &g.name,
        |g| &g.path,
    );

    for child in children_to_iterate {
        flatten_group(
            child,
            instances,
            ranked,
            items,
            depth + 1,
            sort_order,
            profile,
        );
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
    stamp: impl Fn(&Instance) -> Option<Timestamp>,
) {
    if members.is_empty() {
        return;
    }
    members.sort_by(|a, b| stamp(b).cmp(&stamp(a)).then_with(|| a.id.cmp(&b.id)));
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
    sort_archived_project_buckets(&mut buckets);

    for (project_name, mut sessions) in buckets {
        sessions.sort_by(|a, b| {
            b.archived_at
                .cmp(&a.archived_at)
                .then_with(|| a.id.cmp(&b.id))
        });
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

/// Archived project sub-folders, most recently archived first under every sort. Ties fall
/// back to the name, since the buckets arrive in `HashMap` order.
fn sort_archived_project_buckets(buckets: &mut [(String, Vec<&Instance>)]) {
    buckets.sort_by_cached_key(|(name, sessions)| {
        let newest = sessions
            .iter()
            .filter_map(|i| i.archived_at)
            .max()
            .unwrap_or(Timestamp::MIN);
        (Reverse(newest), name.clone())
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::Duration;

    fn inst(title: &str, group: &str) -> Instance {
        let mut inst = Instance::new(title, &format!("/tmp/{title}"));
        inst.group_path = group.to_string();
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
    fn archived_buckets_and_rows_with_one_stamp_keep_a_fixed_order() {
        let stamp = Timestamp::now();
        let archived = |title: &str, repo: &str| {
            let mut i = inst(title, repo);
            i.archived_at = Some(stamp);
            i
        };
        let mut instances = vec![
            archived("c1", "gamma"),
            archived("a1", "alpha"),
            archived("b1", "beta"),
            archived("b2", "beta"),
        ];
        let mut want = None;
        for _ in 0..2 {
            let mut items = Vec::new();
            append_archived_section_by_project(&mut items, &instances, false, &HashMap::new());
            assert_eq!(group_names(&items)[1..], ["alpha", "beta", "gamma"]);
            let rows = session_titles(&items, &instances)
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>();
            assert_eq!(*want.get_or_insert_with(|| rows.clone()), rows);
            instances.reverse();
        }
    }

    /// Under `SortOrder::Custom` a session's stored `sort_index` is the order, and sessions never
    /// moved sit above every placed one, newest first. Favorites do not reorder either.
    #[test]
    fn custom_sort_puts_unplaced_first_then_sort_index() {
        let now = Timestamp::now();
        let mut placed_second = inst("b", "work");
        placed_second.sort_index = Some(1);
        let mut placed_first = inst("a", "work");
        placed_first.sort_index = Some(0);
        placed_first.created_at = now;
        let mut unplaced_old = inst("old", "work");
        unplaced_old.created_at = now - Duration::days(2);
        unplaced_old.favorite();
        let mut unplaced_new = inst("new", "work");
        unplaced_new.created_at = now - Duration::days(1);
        let instances = [placed_second, unplaced_old, placed_first, unplaced_new];

        let mut refs: Vec<&Instance> = instances.iter().collect();
        sort_sessions_inner(&mut refs, SortOrder::Custom, true);

        assert_eq!(titles(&refs), ["new", "old", "a", "b"]);
    }

    /// Moving a group swaps it with the sibling on that side and leaves every other group
    /// where it was; a group already at the end it is moving toward does not move at all.
    #[test]
    fn move_group_permutes_siblings_only() {
        let instances = vec![
            inst("s1", "alpha"),
            inst("s2", "beta"),
            inst("s3", "gamma"),
            inst("s4", "beta/nested"),
        ];
        let mut tree = GroupTree::new_with_groups(&instances, &[]);
        let paths = |t: &GroupTree| -> Vec<String> {
            t.get_all_groups().into_iter().map(|g| g.path).collect()
        };
        assert_eq!(paths(&tree), ["alpha", "beta", "gamma", "beta/nested"]);

        assert!(tree.move_group("gamma", -1, None));
        assert_eq!(paths(&tree), ["alpha", "gamma", "beta", "beta/nested"]);

        assert!(
            !tree.move_group("alpha", -1, None),
            "already first among its siblings"
        );
        assert!(
            !tree.move_group("beta/nested", 1, None),
            "only child of its parent"
        );
        assert!(!tree.move_group("missing", 1, None));
        assert_eq!(paths(&tree), ["alpha", "gamma", "beta", "beta/nested"]);

        // Only the shown siblings count: alpha passes beta, and the hidden gamma stays.
        let shown = ["alpha".to_string(), "beta".to_string()];
        assert!(tree.move_group("alpha", 1, Some(&shown)));
        assert_eq!(paths(&tree), ["beta", "gamma", "alpha", "beta/nested"]);
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

        let items = flatten_tree(&tree, &instances, &instances, SortOrder::Custom);
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
        let items = flatten_tree(&tree, &instances, &instances, SortOrder::Custom);
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
            let items = flatten_tree(&tree, instances, instances, SortOrder::Custom);
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
        // Each row is a day newer than the one before it.
        let aged = |rows: &[(&str, &str)]| -> Vec<Instance> {
            let count = rows.len() as i64;
            rows.iter()
                .enumerate()
                .map(|(at, (title, group))| {
                    let mut row = inst(title, group);
                    row.created_at = Timestamp::now() - Duration::days(count - at as i64);
                    row
                })
                .collect()
        };
        let groups = aged(&[("z", "zebra"), ("a", "apple"), ("m", "Mango")]);
        let tree = GroupTree::new_with_groups(&groups, &[]);
        let nested = aged(&[("p", "parent"), ("z", "parent/zeta"), ("a", "parent/alpha")]);
        let nested_tree = GroupTree::new_with_groups(&nested, &[]);
        for (order, want_groups, want_children) in [
            (
                SortOrder::Custom,
                ["zebra", "apple", "Mango"],
                ["zeta", "alpha"],
            ),
            (
                SortOrder::Name,
                ["apple", "Mango", "zebra"],
                ["alpha", "zeta"],
            ),
            (
                SortOrder::Newest,
                ["Mango", "apple", "zebra"],
                ["alpha", "zeta"],
            ),
        ] {
            let items = flatten_tree(&tree, &groups, &groups, order);
            assert_eq!(group_names(&items), want_groups, "{order:?}");
            let items = flatten_tree(&nested_tree, &nested, &nested, order);
            assert_eq!(group_names(&items)[1..], want_children, "{order:?}");
        }

        for group in ["", "work"] {
            let sessions = vec![
                inst("Mango", group),
                inst("Apple", group),
                inst("Zebra", group),
            ];
            let tree = GroupTree::new_with_groups(&sessions, &[]);
            let items = flatten_tree(&tree, &sessions, &sessions, SortOrder::Name);
            assert_eq!(
                session_titles(&items, &sessions),
                ["Apple", "Mango", "Zebra"],
                "{group:?}"
            );
        }
    }

    #[test]
    fn live_favorite_excludes_snoozed_ignored_archived_and_trashed() {
        let now = Timestamp::now();
        let mut fav = inst("fav", "work");
        fav.favorite();
        assert!(is_live_favorite(&fav));
        assert!(has_placed_favorite("work", std::slice::from_ref(&fav)));
        let mut nested = inst("nested", "work/frontend");
        nested.favorite();
        assert!(has_placed_favorite("work", std::slice::from_ref(&nested)));
        assert!(!has_placed_favorite(
            "personal",
            std::slice::from_ref(&nested)
        ));
        assert!(!has_placed_favorite("work", &[inst("plain", "work")]));

        fav.snooze(60);
        assert!(fav.is_favorited(), "snooze must not clear the star");
        assert!(!is_live_favorite(&fav), "a snoozed favorite drops its star");
        assert!(
            holds_favorite_place(&fav) && has_placed_favorite("work", std::slice::from_ref(&fav)),
            "a snoozed favorite keeps its place"
        );

        let mut archived = inst("archived", "");
        archived.favorited_at = Some(now);
        archived.archived_at = Some(now);
        let mut trashed = inst("trashed", "");
        trashed.favorited_at = Some(now);
        trashed.trashed_at = Some(now);
        assert!(!holds_favorite_place(&archived) && !holds_favorite_place(&trashed));

        let mut ignored = inst("ignored", "work");
        ignored.favorite();
        ignored.ignore();
        assert!(ignored.is_favorited(), "ignore must not clear the star");
        assert!(!is_live_favorite(&ignored) && !holds_favorite_place(&ignored));
        assert!(
            !has_placed_favorite("work", std::slice::from_ref(&ignored)),
            "an ignored favorite must not pin its group"
        );
    }

    #[test]
    fn favorites_first_session_sorts() {
        let aged = |title: &str, days: i64, favorite: bool| {
            let mut i = inst(title, "");
            i.created_at = Timestamp::now() - Duration::days(days);
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
            (SortOrder::Custom, false, ["yak", "apple", "zebra"]),
            (SortOrder::Custom, true, ["yak", "apple", "zebra"]),
            (SortOrder::Name, false, ["apple", "yak", "zebra"]),
            (SortOrder::Name, true, ["yak", "zebra", "apple"]),
            (SortOrder::Newest, false, ["yak", "apple", "zebra"]),
            (SortOrder::Newest, true, ["yak", "zebra", "apple"]),
        ] {
            let mut refs: Vec<&Instance> = instances.iter().collect();
            sort_sessions_inner(&mut refs, order, favorites_first);
            assert_eq!(titles(&refs), want, "{order:?} {favorites_first}");
        }

        let mut snoozed = aged("snoozed_fav", 10, true);
        snoozed.snooze(60);
        let pair = [snoozed, aged("new_plain", 0, false)];
        let mut refs: Vec<&Instance> = pair.iter().collect();
        sort_sessions_inner(&mut refs, SortOrder::Name, true);
        assert_eq!(
            refs[0].title, "snoozed_fav",
            "a snoozed favorite keeps its place"
        );
    }

    #[test]
    fn favorites_first_group_sorts() {
        let member = |title: &str, group: &str, days: i64| {
            let mut i = inst(title, group);
            i.created_at = Timestamp::now() - Duration::days(days);
            i
        };
        let sorted_first = |instances: &[Instance], favorites_first: bool| {
            let mut items = vec![Group::new("old", "old"), Group::new("new", "new")];
            sort_groups_inner(
                &mut items,
                SortOrder::Name,
                instances,
                |g: &Group| g.name.as_str(),
                |g: &Group| g.path.as_str(),
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
        archived[0].favorited_at = Some(Timestamp::now());
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
        old_fav.created_at = Timestamp::now() - Duration::days(10);
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
            let items = flatten_tree_all_profiles(&instances, &instances, &trees, SortOrder::Name);
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
        inst.archived_at = Some(Timestamp::now());
        inst.touch_last_accessed();
        assert!(
            !inst.is_archived() && !inst.is_snoozed(),
            "interaction wakes the session"
        );
        assert!(inst.is_favorited() && inst.last_accessed_at.is_some());
    }
}
