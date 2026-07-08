//! Group tree management

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::collections::HashMap;

use super::config::SortOrder;
use super::Instance;

/// Sentinel path used by the synthetic "Archived" sidebar section. Not a
/// real GroupTree entry; lives only in the flattened `Item` list to give
/// archived sessions a stable bottom-of-sidebar home across every sort
/// mode. Code that walks `flat_items` and dispatches on `Item::Group.path`
/// must skip this path before invoking GroupTree-mutating ops (rename,
/// delete, archive, etc.) since no matching group exists.
pub const ARCHIVED_SECTION_PATH: &str = "__aoe_archived_section__";
pub const ARCHIVED_SECTION_NAME: &str = "Archived";

/// Synthetic group path for the Trash shelf, sibling of the Archived
/// section. Same caveat: code walking `flat_items` must skip this sentinel
/// before invoking GroupTree-mutating ops, since no matching group exists.
pub const TRASH_SECTION_PATH: &str = "__aoe_trash_section__";
pub const TRASH_SECTION_NAME: &str = "Trash";

#[inline]
pub fn is_archived_section_path(path: &str) -> bool {
    path == ARCHIVED_SECTION_PATH
}

#[inline]
pub fn is_trash_section_path(path: &str) -> bool {
    path == TRASH_SECTION_PATH
}

/// True for the Trash section sentinel or anything nested under it. Mirrors
/// [`is_within_archived_section`] for the trash shelf.
#[inline]
pub fn is_within_trash_section(path: &str) -> bool {
    path == TRASH_SECTION_PATH || path.starts_with(&format!("{}/", TRASH_SECTION_PATH))
}

/// True for both the top-level Archived section sentinel and any synthetic
/// child header pushed under it (e.g. project sub-folders nested inside
/// Archived in Project grouping mode). Use this in places that disarm
/// keybinds or skip palette entries for anything that lives inside the
/// shelf; reserve `is_archived_section_path` for the exact-match cases
/// (collapse-toggle routing, count assertions in tests).
#[inline]
pub fn is_within_archived_section(path: &str) -> bool {
    path == ARCHIVED_SECTION_PATH || path.starts_with(&format!("{}/", ARCHIVED_SECTION_PATH))
}

/// Build the synthetic sub-path for a per-project header rendered inside
/// the Archived section. Kept in one place so the path format stays in
/// sync between the appender (groups.rs) and the collapse-state map
/// (HomeView::project_group_collapsed).
#[inline]
pub fn archived_project_sub_path(project_name: &str) -> String {
    format!("{}/{}", ARCHIVED_SECTION_PATH, project_name)
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

    /// Toggle the archived state on the group itself. Returns the new
    /// archived state (true = now archived, false = now unarchived), or
    /// None if the group does not exist. Note: this does NOT cascade to
    /// child instances; cascading is the caller's responsibility (see
    /// HomeView::toggle_archive_at_cursor in operations.rs).
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
        /// When the group was archived (None = active). Used by the row
        /// renderer to apply italic+dim styling. Sort behavior is handled
        /// upstream in `attention_group_key` based on member archive state.
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

/// Sort a slice of session references by `sort_order`.
fn sort_sessions(sessions: &mut [&Instance], sort_order: SortOrder) {
    match sort_order {
        SortOrder::Oldest => sessions.sort_by_key(|i| i.created_at),
        SortOrder::Newest => sessions.sort_by_key(|i| Reverse(i.created_at)),
        SortOrder::LastActivity => sessions.sort_by_key(|i| last_activity_session_key(i)),
        SortOrder::Attention => sessions.sort_by_key(|i| attention_session_key(i)),
        SortOrder::AZ | SortOrder::ZA => sort_by_name(sessions, sort_order, |i| &i.title),
    }
}

/// Sort a slice of group references by `sort_order`, using `instances` for
/// timestamp-based orderings. The `archived` closure returns the group's
/// own `archived_at` (only consulted by the Attention sort for empty
/// archived groups).
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
    match sort_order {
        SortOrder::Oldest => {
            items.sort_by_key(|g| min_created_at_in_group(path(g), instances));
        }
        SortOrder::Newest => {
            items.sort_by_key(|g| Reverse(max_created_at_in_group(path(g), instances)));
        }
        SortOrder::LastActivity => {
            items.sort_by_key(|g| last_activity_group_key(path(g), instances));
        }
        SortOrder::Attention => {
            items.sort_by_key(|g| attention_group_key(path(g), archived(g), instances));
        }
        SortOrder::AZ | SortOrder::ZA => sort_by_name(items, sort_order, name),
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

/// Get the most recent created_at among all sessions (direct and nested) in a group.
/// Returns DateTime::MIN_UTC if the group has no sessions.
fn max_created_at_in_group(path: &str, instances: &[Instance]) -> DateTime<Utc> {
    group_members(path, instances)
        .map(|i| i.created_at)
        .max()
        .unwrap_or(DateTime::<Utc>::MIN_UTC)
}

/// Get the oldest created_at among all sessions (direct and nested) in a group.
/// Returns DateTime::MAX_UTC if the group has no sessions (so empty groups sink to the bottom).
fn min_created_at_in_group(path: &str, instances: &[Instance]) -> DateTime<Utc> {
    group_members(path, instances)
        .map(|i| i.created_at)
        .min()
        .unwrap_or(DateTime::<Utc>::MAX_UTC)
}

/// Get the most recent last_accessed_at among all sessions (direct and nested) in a group.
/// Groups with no sessions (or whose sessions have never reported activity) sort to the bottom
/// for descending order.
fn max_last_accessed_in_group(path: &str, instances: &[Instance]) -> Option<DateTime<Utc>> {
    group_members(path, instances)
        .filter_map(|i| i.last_accessed_at)
        .max()
}

/// Key used to sort sessions by LastActivity in descending order, pushing
/// sessions with no recorded activity to the bottom.
///
/// Rust's default ordering on `Option` places `None` BEFORE `Some(..)`; we
/// invert by wrapping in `Reverse` AND bucketing `None` into the "has no
/// activity" tier via the leading bool.
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

/// Priority tier for the Attention sort. Lower = higher priority = closer to
/// the top of the list. See `docs/plans/2026-04-21-aoe-attention-sort.md` for
/// the full rationale on tier choices.
///
/// Archived sessions short-circuit to tier 99 so they always sink to the
/// bottom regardless of their current status. They remain visible (rendered
/// in italic+dim by the row formatter); only the sort order is suppressed.
fn attention_tier(inst: &Instance) -> u8 {
    use crate::session::Status::*;
    if inst.is_archived() || inst.is_snoozed() || inst.is_trashed() || inst.pane_dead_observed {
        // Tier 99 sinks: archived and snoozed (snoozed = temporary archive,
        // wakes automatically when timer expires). Both read as "do not
        // bother me with this row" so they share the bottom tier.
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

/// Attention bucket rank with the unread promoter folded in. Pure (no global
/// flag read) so it can be unit-tested directly. Waiting (tier 0) stays top;
/// tier 99 stays sunk regardless of unread state. When `unread` and the
/// feature is on, any other non-waiting row is promoted to rank 1, just below
/// Waiting and above every other tier, with the remaining tiers shifted up by
/// one (2..=7). When the feature is off it returns `tier` unchanged; the shift
/// is monotonic so a feature-on run with no unread rows orders identically to
/// before.
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

/// Key used to sort sessions by Attention. Primary = urgent-bias (the agent
/// has flagged the session via `attention-urgent`); secondary = priority tier
/// ascending; tertiary = favorite within tier; rest = "longest aging first":
/// within a tier, the session that has been ignored the longest bubbles to
/// the top. A Waiting session that has been sitting untouched for 2 days
/// should rank above one that was just bumped a minute ago, because the
/// stale one is the one most likely to have been forgotten.
///
/// Sessions with no `last_accessed_at` (never polled / just created) bucket
/// into the "no activity" slot AFTER the dated ones, so fresh-but-untouched
/// rows don't falsely claim the top.
///
/// Within tier 99, preserve the reverse convention: most-recently sunk first,
/// since the archive block is a recency view, not an attention view. Urgent is
/// suppressed for tier 99 so a sunk row can't claw back to the top.
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
    // Urgent is the cross-tier promoter: an agent that has flagged itself
    // urgent rises above all non-urgent rows regardless of status tier.
    // Encoded `!urgent_bias` since `false` sorts before `true`. Tier 99
    // suppresses urgent (is_urgent() already short-circuits on archived/
    // snoozed; the redundant guard here mirrors favorite's pattern).
    let urgent_bias = tier != 99 && inst.is_urgent();
    // Favorite pins to the top of its category; tier stays primary so a
    // fav'd Running never leaps above a plain Waiting. Within a tier,
    // favorited rows bubble first (encoded `!favorite_bias` since `false`
    // sorts before `true`). Tier 99 (archive/snooze) opts out: archive()
    // clears favorited_at via mutex, and a snoozed favorite stays sunk per
    // design; within the sunk block, recency ordering is the intent.
    let favorite_bias = tier != 99 && inst.is_favorited();
    if tier == 99 {
        // Tier 99 unifies archived, snoozed, and pane-dead rows.
        // Secondary sort timestamp falls through: archived_at first, then
        // snoozed_until, then last_accessed_at (the only signal pane-dead
        // rows have, since they were never explicitly archived).
        // Reverse() makes most-recent sink-time bubble to the top of the
        // sunk block, matching "recency view" intent across all four
        // sub-categories.
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
    // The `Reverse` slot is forced to `Reverse(None)` (= sorts after all
    // Some() in the Reverse ordering) so it doesn't contribute; the real
    // tiebreak is the trailing ASC field.
    (
        !urgent_bias,
        rank,
        !favorite_bias,
        inst.last_accessed_at.is_none(),
        Reverse(None),
        inst.last_accessed_at,
    )
}

/// Key used to sort groups by Attention. Uses the highest-priority (lowest
/// tier) among the group's direct and nested sessions. Empty groups sink.
///
/// Within a tier, the group's aging signal = max(member.last_accessed_at),
/// the MOST RECENT activity across any member. The group whose most-recent
/// activity is itself oldest = the group nobody has touched in the longest
/// time = the one most likely forgotten. So this sorts ASC (oldest-first),
/// mirroring the within-tier rule in `attention_session_key`. Groups with
/// no timestamped members sink after dated ones.
///
/// Archive handling: members already short-circuit to tier 99 if archived
/// (see `attention_tier`). When all members are archived (or the group is
/// empty AND the group itself is marked archived), the group sorts at
/// tier 99 with the latest archived_at timestamp (group's own timestamp
/// is the fallback for empty groups). The archived block stays a recency
/// view via `Reverse(ts)`; intentional asymmetry with the non-archived
/// aging rule, same as the session-level key.
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

    // Group-level urgent bias: any non-sunk member with the urgent flag
    // promotes the entire group above non-urgent peers across all tiers.
    // Mirrors the session-level cross-tier behavior so a buried group
    // containing a live device-code prompt floats up.
    let urgent_bias = min_tier != 99 && members.iter().any(|i| i.is_urgent());

    // Group-level favorite bias: within its min_tier bucket, a group with
    // any live favorited member pins above peers. Mirrors the session key's
    // tier-primary shape so favorite never promotes a group across tiers.
    let favorite_bias = min_tier != 99
        && members
            .iter()
            .any(|i| !i.is_archived() && !i.is_snoozed() && !i.is_trashed() && i.is_favorited());

    if min_tier == 99 {
        // All members archived: sort archived block by latest archived_at.
        // Falls back to the group's own archived_at if no member has one
        // (shouldn't happen given is_archived, but defensive).
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
    // The `Reverse` slot is forced to `Reverse(None)` so it doesn't
    // contribute; the real tiebreak is the trailing ASC field. Shape
    // mirrors `attention_session_key` so the intent is uniform.
    let max_last = members.iter().filter_map(|i| i.last_accessed_at).max();
    // Fold the unread promoter in at the group level too, so a project
    // containing an unread session floats up just like the flat Attention
    // view does (a group with an unread Idle outranks a group whose best
    // member is a read Error). Mirrors `attention_session_key`'s rank.
    //
    // Only non-sunk members count: archive/snooze don't clear `unread`, so an
    // archived or snoozed (tier-99) member must not promote the group, or a
    // dismissed session would drag its group back to the top. This matches the
    // session key, which short-circuits tier-99 rows before the unread rank.
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
/// Merges all profiles' sessions and groups at depth 0 (no profile headers).
/// Uses per-profile GroupTrees so collapsed state is isolated per profile.
pub fn flatten_tree_all_profiles(
    instances: &[Instance],
    group_trees: &std::collections::HashMap<String, GroupTree>,
    sort_order: SortOrder,
) -> Vec<Item> {
    let mut items = Vec::new();

    // Archived sessions are excluded from the natural flow. The caller
    // appends them under the synthetic "Archived" section via
    // `append_archived_section`.
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

    // Sort using the per-profile instances stored in each tuple (element 2),
    // not the global instances slice, so groups from different profiles with
    // the same name get sort keys scoped to their own profile's sessions.
    match sort_order {
        SortOrder::Oldest => {
            all_roots.sort_by_key(|(_, g, insts)| min_created_at_in_group(&g.path, insts));
        }
        SortOrder::Newest => {
            all_roots.sort_by_key(|(_, g, insts)| Reverse(max_created_at_in_group(&g.path, insts)));
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
/// Attention is a cross-cutting priority view, not a folder tree, so a
/// Waiting session in group A should sort next to a Waiting session in
/// group B without a header row breaking the tier ordering. Pre-filter
/// `instances` by profile/storage at the call site; this function honors
/// whatever slice it receives.
pub fn flatten_sessions_by_attention(instances: &[Instance]) -> Vec<Item> {
    // Archived rows are excluded from the natural attention flow; the
    // caller appends them under the synthetic "Archived" section via
    // `append_archived_section`. Snoozed and pane-dead rows still sink to
    // tier 99 inline because they are transient attention sinks, not
    // lifecycle terminals.
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

    // Archived sessions are excluded from the natural flow. The caller
    // appends them under the synthetic "Archived" section via
    // `append_archived_section`.
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

    // Archived sessions are pulled out of the natural flow regardless of
    // their group_path. They reappear under the synthetic "Archived"
    // section appended by the caller.
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

/// Count of the sessions that render under a group header. Delegates to
/// `group_members` so the header badge and the visible rows share one
/// predicate (excluding archived and trashed sessions).
fn count_sessions_in_group(path: &str, instances: &[Instance]) -> usize {
    group_members(path, instances).count()
}

/// Append the synthetic "Archived" section to `items`, pinned to the
/// bottom of the sidebar across every sort mode. The section contains
/// every session with `is_archived() == true`, ordered by most-recently
/// archived first (the row a user just shelved is the one they're most
/// likely to look for). When `collapsed` is true, only the header is
/// pushed; the header still shows the total count.
///
/// No-op when there are no archived sessions, so users who never archive
/// anything don't see a phantom "Archived (0)" header.
pub fn append_archived_section(items: &mut Vec<Item>, instances: &[Instance], collapsed: bool) {
    let mut archived: Vec<&Instance> = instances
        .iter()
        .filter(|i| i.is_archived() && !i.is_trashed())
        .collect();
    if archived.is_empty() {
        return;
    }
    archived.sort_by_key(|i| Reverse(i.archived_at));

    items.push(Item::Group {
        path: ARCHIVED_SECTION_PATH.to_string(),
        name: ARCHIVED_SECTION_NAME.to_string(),
        depth: 0,
        collapsed,
        session_count: archived.len(),
        profile: None,
        archived_at: None,
    });

    if collapsed {
        return;
    }

    for inst in archived {
        items.push(Item::Session {
            id: inst.id.clone(),
            depth: 1,
        });
    }
}

/// Append the synthetic Trash section to `items`: a depth-0 header followed
/// by every `is_trashed()` session, most-recently-trashed first (the row a
/// user just deleted is the one they are most likely to want back). When
/// `collapsed` is true only the header is pushed; the header still shows the
/// count. No-op when nothing is trashed, so users who never delete don't see
/// a phantom "Trash (0)" header. Rendered as a sibling of the Archived
/// section, pinned to the very bottom (see `HomeView::build_flat_items`).
/// Flat in every grouping mode: trash is a recovery shelf, not a workspace,
/// so it is not nested by project the way the Archived section is.
pub fn append_trash_section(items: &mut Vec<Item>, instances: &[Instance], collapsed: bool) {
    let mut trashed: Vec<&Instance> = instances.iter().filter(|i| i.is_trashed()).collect();
    if trashed.is_empty() {
        return;
    }
    trashed.sort_by_key(|i| Reverse(i.trashed_at));

    items.push(Item::Group {
        path: TRASH_SECTION_PATH.to_string(),
        name: TRASH_SECTION_NAME.to_string(),
        depth: 0,
        collapsed,
        session_count: trashed.len(),
        profile: None,
        archived_at: None,
    });

    if collapsed {
        return;
    }

    for inst in trashed {
        items.push(Item::Session {
            id: inst.id.clone(),
            depth: 1,
        });
    }
}

/// Project-grouping variant of `append_archived_section`: nests archived
/// sessions under a sub-header per project. Caller must have already
/// rewritten `inst.group_path` to the project name (see
/// `HomeView::build_flat_items_by_project`) so we can read it back here
/// to bucket each archived row.
///
/// Layout:
/// - Archived (depth 0)
///   - `<project name>` (depth 1)
///     - session row (depth 2)
///
/// `section_collapsed` hides everything below the top header; per-project
/// `sub_collapsed` (read from `project_collapsed` keyed by the synthetic
/// `archived_project_sub_path`) hides only that sub-folder's session rows.
/// Sub-headers honor the user's `sort_order`, mirroring how
/// `flatten_tree` orders active project headers; within a sub-folder,
/// sessions still surface most-recently-archived first regardless of
/// sort_order (archived rows are a "park this" affordance and the
/// archive timestamp is the natural recency signal once the row is sunk).
///
/// Returns without pushing anything when no archived sessions exist, so
/// users who never archive don't see a phantom "Archived" header in the
/// project-mode view either.
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
            name: project_name,
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

/// Ordering helper for archived project sub-folders. Mirrors the spirit
/// of `sort_groups` but operates on a `(project_name, sessions)` pair
/// since the archive sub-folders are synthetic and not in any
/// `GroupTree`. AZ/ZA sort by project name; recency sorts (Newest,
/// LastActivity) use the max `archived_at` within the group; Oldest
/// uses the min `archived_at` ascending. Attention falls back to
/// most-recently-archived because archived rows are all tier 99 in the
/// attention bucket and the tier offers no discriminator inside the shelf.
fn sort_archived_project_buckets(buckets: &mut [(String, Vec<&Instance>)], sort_order: SortOrder) {
    match sort_order {
        SortOrder::AZ => buckets.sort_by_key(|b| b.0.to_lowercase()),
        SortOrder::ZA => buckets.sort_by_key(|b| Reverse(b.0.to_lowercase())),
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

    #[test]
    fn test_group_tree_creation() {
        let mut inst1 = Instance::new("test1", "/tmp/1");
        inst1.group_path = "work".to_string();
        let mut inst2 = Instance::new("test2", "/tmp/2");
        inst2.group_path = "work/frontend".to_string();
        let mut inst3 = Instance::new("test3", "/tmp/3");
        inst3.group_path = "personal".to_string();

        let instances = vec![inst1, inst2, inst3];
        let tree = GroupTree::new_with_groups(&instances, &[]);

        assert!(tree.group_exists("work"));
        assert!(tree.group_exists("work/frontend"));
        assert!(tree.group_exists("personal"));
        assert!(!tree.group_exists("nonexistent"));
    }

    #[test]
    fn test_flatten_tree() {
        let ungrouped = Instance::new("ungrouped", "/tmp/u");
        let mut inst1 = Instance::new("test1", "/tmp/1");
        inst1.group_path = "work".to_string();
        let mut inst2 = Instance::new("test2", "/tmp/2");
        inst2.group_path = "work".to_string();

        let instances = vec![ungrouped, inst1, inst2];
        let tree = GroupTree::new_with_groups(&instances, &[]);
        let items = flatten_tree(&tree, &instances, SortOrder::Oldest);

        assert!(!items.is_empty());

        // First item should be ungrouped session
        assert!(matches!(items[0], Item::Session { .. }));
    }

    #[test]
    fn test_toggle_collapsed() {
        let mut inst = Instance::new("test", "/tmp/t");
        inst.group_path = "work".to_string();
        let instances = vec![inst];
        let mut tree = GroupTree::new_with_groups(&instances, &[]);

        let group = tree.groups_by_path.get("work").unwrap();
        assert!(!group.collapsed);

        tree.toggle_collapsed("work");

        let group = tree.groups_by_path.get("work").unwrap();
        assert!(group.collapsed);

        tree.toggle_collapsed("work");

        let group = tree.groups_by_path.get("work").unwrap();
        assert!(!group.collapsed);
    }

    #[test]
    fn test_toggle_collapsed_nonexistent_group() {
        let instances: Vec<Instance> = vec![];
        let mut tree = GroupTree::new_with_groups(&instances, &[]);
        tree.toggle_collapsed("nonexistent");
    }

    #[test]
    fn test_collapsed_group_hides_sessions_in_flatten() {
        let mut inst1 = Instance::new("work-session", "/tmp/w");
        inst1.group_path = "work".to_string();
        let instances = vec![inst1];
        let mut tree = GroupTree::new_with_groups(&instances, &[]);

        let items_expanded = flatten_tree(&tree, &instances, SortOrder::Oldest);
        let session_count_expanded = items_expanded
            .iter()
            .filter(|i| matches!(i, Item::Session { .. }))
            .count();
        assert_eq!(session_count_expanded, 1);

        tree.toggle_collapsed("work");
        let items_collapsed = flatten_tree(&tree, &instances, SortOrder::Oldest);
        let session_count_collapsed = items_collapsed
            .iter()
            .filter(|i| matches!(i, Item::Session { .. }))
            .count();
        assert_eq!(session_count_collapsed, 0);
    }

    #[test]
    fn test_collapsed_group_still_shows_in_flatten() {
        let mut inst = Instance::new("test", "/tmp/t");
        inst.group_path = "work".to_string();
        let instances = vec![inst];
        let mut tree = GroupTree::new_with_groups(&instances, &[]);

        tree.toggle_collapsed("work");
        let items = flatten_tree(&tree, &instances, SortOrder::Oldest);

        let group_items: Vec<_> = items
            .iter()
            .filter(|i| matches!(i, Item::Group { .. }))
            .collect();
        assert_eq!(group_items.len(), 1);
    }

    #[test]
    fn test_collapsed_state_in_flattened_item() {
        let mut inst = Instance::new("test", "/tmp/t");
        inst.group_path = "work".to_string();
        let instances = vec![inst];
        let mut tree = GroupTree::new_with_groups(&instances, &[]);

        let items = flatten_tree(&tree, &instances, SortOrder::Oldest);
        if let Some(Item::Group { collapsed, .. }) = items
            .iter()
            .find(|i| matches!(i, Item::Group { path, .. } if path == "work"))
        {
            assert!(!collapsed);
        }

        tree.toggle_collapsed("work");
        let items = flatten_tree(&tree, &instances, SortOrder::Oldest);
        if let Some(Item::Group { collapsed, .. }) = items
            .iter()
            .find(|i| matches!(i, Item::Group { path, .. } if path == "work"))
        {
            assert!(*collapsed);
        }
    }

    #[test]
    fn test_nested_group_collapse_hides_children() {
        let mut inst1 = Instance::new("parent-session", "/tmp/p");
        inst1.group_path = "parent".to_string();
        let mut inst2 = Instance::new("child-session", "/tmp/c");
        inst2.group_path = "parent/child".to_string();
        let instances = vec![inst1, inst2];
        let mut tree = GroupTree::new_with_groups(&instances, &[]);

        let items = flatten_tree(&tree, &instances, SortOrder::Oldest);
        let group_count = items
            .iter()
            .filter(|i| matches!(i, Item::Group { .. }))
            .count();
        assert_eq!(group_count, 2);

        tree.toggle_collapsed("parent");
        let items = flatten_tree(&tree, &instances, SortOrder::Oldest);
        let group_count_collapsed = items
            .iter()
            .filter(|i| matches!(i, Item::Group { .. }))
            .count();
        assert_eq!(group_count_collapsed, 1);
    }

    #[test]
    fn test_session_count_includes_nested() {
        let mut inst1 = Instance::new("parent-session", "/tmp/p");
        inst1.group_path = "parent".to_string();
        let mut inst2 = Instance::new("child-session", "/tmp/c");
        inst2.group_path = "parent/child".to_string();
        let instances = vec![inst1, inst2];
        let tree = GroupTree::new_with_groups(&instances, &[]);

        let items = flatten_tree(&tree, &instances, SortOrder::Oldest);
        if let Some(Item::Group { session_count, .. }) = items
            .iter()
            .find(|i| matches!(i, Item::Group { path, .. } if path == "parent"))
        {
            assert_eq!(*session_count, 2);
        }
    }

    #[test]
    fn test_session_count_excludes_trashed_and_restores_on_untrash() {
        let mut a = Instance::new("a", "/tmp/a");
        a.group_path = "work".to_string();
        let mut b = Instance::new("b", "/tmp/b");
        b.group_path = "work".to_string();
        let mut instances = vec![a, b];

        let group_count = |instances: &[Instance]| {
            let tree = GroupTree::new_with_groups(instances, &[]);
            let items = flatten_tree(&tree, instances, SortOrder::Oldest);
            items
                .iter()
                .find_map(|i| match i {
                    Item::Group {
                        path,
                        session_count,
                        ..
                    } if path == "work" => Some(*session_count),
                    _ => None,
                })
                .expect("work group present")
        };

        assert_eq!(group_count(&instances), 2);

        // Trashing a session drops it from both the visible rows and the
        // header badge, matching the row filter in `flatten_group`.
        instances[0].trash();
        assert_eq!(group_count(&instances), 1);

        instances[0].untrash();
        assert_eq!(group_count(&instances), 2);
    }

    #[test]
    fn test_delete_group() {
        let mut inst = Instance::new("test", "/tmp/t");
        inst.group_path = "work".to_string();
        let instances = vec![inst];
        let mut tree = GroupTree::new_with_groups(&instances, &[]);

        assert!(tree.group_exists("work"));
        tree.delete_group("work");
        assert!(!tree.group_exists("work"));
    }

    #[test]
    fn test_delete_group_removes_children() {
        let mut inst1 = Instance::new("parent-session", "/tmp/p");
        inst1.group_path = "parent".to_string();
        let mut inst2 = Instance::new("child-session", "/tmp/c");
        inst2.group_path = "parent/child".to_string();
        let instances = vec![inst1, inst2];
        let mut tree = GroupTree::new_with_groups(&instances, &[]);

        assert!(tree.group_exists("parent"));
        assert!(tree.group_exists("parent/child"));

        tree.delete_group("parent");

        assert!(!tree.group_exists("parent"));
        assert!(!tree.group_exists("parent/child"));
    }

    #[test]
    fn test_create_group() {
        let instances: Vec<Instance> = vec![];
        let mut tree = GroupTree::new_with_groups(&instances, &[]);

        assert!(!tree.group_exists("new-group"));
        tree.create_group("new-group");
        assert!(tree.group_exists("new-group"));
    }

    #[test]
    fn test_create_nested_group_creates_parents() {
        let instances: Vec<Instance> = vec![];
        let mut tree = GroupTree::new_with_groups(&instances, &[]);

        tree.create_group("a/b/c");
        assert!(tree.group_exists("a"));
        assert!(tree.group_exists("a/b"));
        assert!(tree.group_exists("a/b/c"));
    }

    #[test]
    fn test_item_depth() {
        let ungrouped = Instance::new("ungrouped", "/tmp/u");
        let mut inst1 = Instance::new("root-level", "/tmp/r");
        inst1.group_path = "root".to_string();
        let mut inst2 = Instance::new("nested", "/tmp/n");
        inst2.group_path = "root/child".to_string();
        let instances = vec![ungrouped, inst1, inst2];
        let tree = GroupTree::new_with_groups(&instances, &[]);
        let items = flatten_tree(&tree, &instances, SortOrder::Oldest);

        for item in &items {
            match item {
                Item::Session { id, depth } if !id.is_empty() => {
                    if *depth == 0 {
                        continue;
                    }
                    assert!(*depth >= 1);
                }
                Item::Group { path, depth, .. } => {
                    if path == "root" {
                        assert_eq!(*depth, 0);
                    } else if path == "root/child" {
                        assert_eq!(*depth, 1);
                    }
                }
                _ => {}
            }
        }
    }

    #[test]
    fn test_get_roots_returns_only_top_level() {
        let mut inst1 = Instance::new("test1", "/tmp/1");
        inst1.group_path = "alpha".to_string();
        let mut inst2 = Instance::new("test2", "/tmp/2");
        inst2.group_path = "alpha/nested".to_string();
        let mut inst3 = Instance::new("test3", "/tmp/3");
        inst3.group_path = "beta".to_string();
        let instances = vec![inst1, inst2, inst3];
        let tree = GroupTree::new_with_groups(&instances, &[]);

        let roots = tree.get_roots();
        assert_eq!(roots.len(), 2);

        let root_names: Vec<_> = roots.iter().map(|g| &g.name).collect();
        assert!(root_names.contains(&&"alpha".to_string()));
        assert!(root_names.contains(&&"beta".to_string()));
    }

    #[test]
    fn test_delete_group_removes_from_insertion_order() {
        let mut inst1 = Instance::new("alpha-session", "/tmp/a");
        inst1.group_path = "alpha".to_string();
        let mut inst2 = Instance::new("beta-session", "/tmp/b");
        inst2.group_path = "beta".to_string();
        let mut inst3 = Instance::new("gamma-session", "/tmp/g");
        inst3.group_path = "gamma".to_string();
        let instances = vec![inst1, inst2, inst3];
        let mut tree = GroupTree::new_with_groups(&instances, &[]);

        let initial_groups_vec = tree.get_all_groups();
        let initial_groups: Vec<_> = initial_groups_vec.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(initial_groups, vec!["alpha", "beta", "gamma"]);

        tree.delete_group("beta");

        let after_delete_vec = tree.get_all_groups();
        let after_delete: Vec<_> = after_delete_vec.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(after_delete, vec!["alpha", "gamma"]);

        tree.create_group("zeta");

        let after_create_vec = tree.get_all_groups();
        let after_create: Vec<_> = after_create_vec.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(after_create, vec!["alpha", "gamma", "zeta"]);
    }

    #[test]
    fn test_group_sort_order_in_flatten_tree() {
        // Groups are created in order: zebra, apple, mango (by instance order)
        let mut inst1 = Instance::new("z-session", "/tmp/z");
        inst1.group_path = "zebra".to_string();
        let mut inst2 = Instance::new("a-session", "/tmp/a");
        inst2.group_path = "apple".to_string();
        let mut inst3 = Instance::new("m-session", "/tmp/m");
        inst3.group_path = "mango".to_string();
        let instances = vec![inst1, inst2, inst3];
        let tree = GroupTree::new_with_groups(&instances, &[]);

        // SortOrder::Oldest: groups sorted by oldest session (zebra, apple, mango)
        let items_oldest = flatten_tree(&tree, &instances, SortOrder::Oldest);
        let group_names_none: Vec<_> = items_oldest
            .iter()
            .filter_map(|i| match i {
                Item::Group { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(group_names_none, vec!["zebra", "apple", "mango"]);

        // SortOrder::AZ: groups appear alphabetically
        let items_az = flatten_tree(&tree, &instances, SortOrder::AZ);
        let group_names_az: Vec<_> = items_az
            .iter()
            .filter_map(|i| match i {
                Item::Group { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(group_names_az, vec!["apple", "mango", "zebra"]);

        // SortOrder::ZA: groups appear reverse alphabetically
        let items_za = flatten_tree(&tree, &instances, SortOrder::ZA);
        let group_names_za: Vec<_> = items_za
            .iter()
            .filter_map(|i| match i {
                Item::Group { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(group_names_za, vec!["zebra", "mango", "apple"]);
    }

    #[test]
    fn test_sort_order_cycle() {
        assert_eq!(SortOrder::Newest.cycle(), SortOrder::Attention);
        assert_eq!(SortOrder::Attention.cycle(), SortOrder::LastActivity);
        assert_eq!(SortOrder::LastActivity.cycle(), SortOrder::Oldest);
        assert_eq!(SortOrder::Oldest.cycle(), SortOrder::AZ);
        assert_eq!(SortOrder::AZ.cycle(), SortOrder::ZA);
        assert_eq!(SortOrder::ZA.cycle(), SortOrder::Newest);
    }

    #[test]
    fn test_sort_order_cycle_reverse() {
        assert_eq!(SortOrder::Newest.cycle_reverse(), SortOrder::ZA);
        assert_eq!(SortOrder::ZA.cycle_reverse(), SortOrder::AZ);
        assert_eq!(SortOrder::AZ.cycle_reverse(), SortOrder::Oldest);
        assert_eq!(SortOrder::Oldest.cycle_reverse(), SortOrder::LastActivity);
        assert_eq!(
            SortOrder::LastActivity.cycle_reverse(),
            SortOrder::Attention
        );
        assert_eq!(SortOrder::Attention.cycle_reverse(), SortOrder::Newest);
    }

    #[test]
    fn test_sort_last_activity_descending_with_none_last() {
        use chrono::Duration;
        let now = Utc::now();
        let mut inst_recent = Instance::new("recent", "/tmp/r");
        inst_recent.last_accessed_at = Some(now);
        let mut inst_older = Instance::new("older", "/tmp/o");
        inst_older.last_accessed_at = Some(now - Duration::hours(1));
        let inst_never = Instance::new("never", "/tmp/n");
        let instances = vec![inst_never, inst_older, inst_recent];
        let tree = GroupTree::new_with_groups(&instances, &[]);

        let items = flatten_tree(&tree, &instances, SortOrder::LastActivity);
        let titles: Vec<_> = items
            .iter()
            .filter_map(|i| match i {
                Item::Session { id, .. } => instances
                    .iter()
                    .find(|inst| &inst.id == id)
                    .map(|inst| inst.title.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(titles, vec!["recent", "older", "never"]);
    }

    #[test]
    fn test_ungrouped_session_sort_oldest_preserves_insertion_order() {
        let inst1 = Instance::new("Mango", "/tmp/m");
        let inst2 = Instance::new("Apple", "/tmp/a");
        let inst3 = Instance::new("Zebra", "/tmp/z");
        let instances = vec![inst1, inst2, inst3];
        let tree = GroupTree::new_with_groups(&instances, &[]);

        let items = flatten_tree(&tree, &instances, SortOrder::Oldest);
        let session_titles: Vec<_> = items
            .iter()
            .filter_map(|i| match i {
                Item::Session { id, .. } => instances
                    .iter()
                    .find(|inst| &inst.id == id)
                    .map(|inst| inst.title.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(session_titles, vec!["Mango", "Apple", "Zebra"]);
    }

    #[test]
    fn test_ungrouped_session_sort_az() {
        let inst1 = Instance::new("Mango", "/tmp/m");
        let inst2 = Instance::new("Apple", "/tmp/a");
        let inst3 = Instance::new("Zebra", "/tmp/z");
        let instances = vec![inst1, inst2, inst3];
        let tree = GroupTree::new_with_groups(&instances, &[]);

        let items = flatten_tree(&tree, &instances, SortOrder::AZ);
        let session_titles: Vec<_> = items
            .iter()
            .filter_map(|i| match i {
                Item::Session { id, .. } => instances
                    .iter()
                    .find(|inst| &inst.id == id)
                    .map(|inst| inst.title.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(session_titles, vec!["Apple", "Mango", "Zebra"]);
    }

    #[test]
    fn test_ungrouped_session_sort_za() {
        let inst1 = Instance::new("Mango", "/tmp/m");
        let inst2 = Instance::new("Apple", "/tmp/a");
        let inst3 = Instance::new("Zebra", "/tmp/z");
        let instances = vec![inst1, inst2, inst3];
        let tree = GroupTree::new_with_groups(&instances, &[]);

        let items = flatten_tree(&tree, &instances, SortOrder::ZA);
        let session_titles: Vec<_> = items
            .iter()
            .filter_map(|i| match i {
                Item::Session { id, .. } => instances
                    .iter()
                    .find(|inst| &inst.id == id)
                    .map(|inst| inst.title.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(session_titles, vec!["Zebra", "Mango", "Apple"]);
    }

    #[test]
    fn test_session_sort_oldest_within_group_preserves_insertion_order() {
        let mut inst1 = Instance::new("Mango", "/tmp/m");
        inst1.group_path = "work".to_string();
        let mut inst2 = Instance::new("Apple", "/tmp/a");
        inst2.group_path = "work".to_string();
        let mut inst3 = Instance::new("Zebra", "/tmp/z");
        inst3.group_path = "work".to_string();
        let instances = vec![inst1, inst2, inst3];
        let tree = GroupTree::new_with_groups(&instances, &[]);

        let items = flatten_tree(&tree, &instances, SortOrder::Oldest);
        let session_titles: Vec<_> = items
            .iter()
            .filter_map(|i| match i {
                Item::Session { id, .. } => instances
                    .iter()
                    .find(|inst| &inst.id == id)
                    .map(|inst| inst.title.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(session_titles, vec!["Mango", "Apple", "Zebra"]);
    }

    #[test]
    fn test_session_sort_az_within_group() {
        let mut inst1 = Instance::new("Mango", "/tmp/m");
        inst1.group_path = "work".to_string();
        let mut inst2 = Instance::new("Apple", "/tmp/a");
        inst2.group_path = "work".to_string();
        let mut inst3 = Instance::new("Zebra", "/tmp/z");
        inst3.group_path = "work".to_string();
        let instances = vec![inst1, inst2, inst3];
        let tree = GroupTree::new_with_groups(&instances, &[]);

        let items = flatten_tree(&tree, &instances, SortOrder::AZ);
        let session_titles: Vec<_> = items
            .iter()
            .filter_map(|i| match i {
                Item::Session { id, .. } => instances
                    .iter()
                    .find(|inst| &inst.id == id)
                    .map(|inst| inst.title.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(session_titles, vec!["Apple", "Mango", "Zebra"]);
    }

    #[test]
    fn test_session_sort_za_within_group() {
        let mut inst1 = Instance::new("Mango", "/tmp/m");
        inst1.group_path = "work".to_string();
        let mut inst2 = Instance::new("Apple", "/tmp/a");
        inst2.group_path = "work".to_string();
        let mut inst3 = Instance::new("Zebra", "/tmp/z");
        inst3.group_path = "work".to_string();
        let instances = vec![inst1, inst2, inst3];
        let tree = GroupTree::new_with_groups(&instances, &[]);

        let items = flatten_tree(&tree, &instances, SortOrder::ZA);
        let session_titles: Vec<_> = items
            .iter()
            .filter_map(|i| match i {
                Item::Session { id, .. } => instances
                    .iter()
                    .find(|inst| &inst.id == id)
                    .map(|inst| inst.title.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(session_titles, vec!["Zebra", "Mango", "Apple"]);
    }

    #[test]
    fn test_nested_child_groups_sort_order() {
        let mut inst_parent = Instance::new("parent-session", "/tmp/parent");
        inst_parent.group_path = "parent".to_string();
        let mut inst_zeta = Instance::new("zeta-session", "/tmp/zeta");
        inst_zeta.group_path = "parent/zeta".to_string();
        let mut inst_alpha = Instance::new("alpha-session", "/tmp/alpha");
        inst_alpha.group_path = "parent/alpha".to_string();
        let instances = vec![inst_parent, inst_zeta, inst_alpha];
        let tree = GroupTree::new_with_groups(&instances, &[]);

        let items_oldest = flatten_tree(&tree, &instances, SortOrder::Oldest);
        let child_names_oldest: Vec<_> = items_oldest
            .iter()
            .skip(1)
            .filter_map(|i| match i {
                Item::Group { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(child_names_oldest, vec!["zeta", "alpha"]);

        let items_az = flatten_tree(&tree, &instances, SortOrder::AZ);
        let child_names_az: Vec<_> = items_az
            .iter()
            .skip(1)
            .filter_map(|i| match i {
                Item::Group { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(child_names_az, vec!["alpha", "zeta"]);

        let items_za = flatten_tree(&tree, &instances, SortOrder::ZA);
        let child_names_za: Vec<_> = items_za
            .iter()
            .skip(1)
            .filter_map(|i| match i {
                Item::Group { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(child_names_za, vec!["zeta", "alpha"]);
    }

    #[test]
    fn test_sort_az_is_case_insensitive() {
        let mut inst1 = Instance::new("z-session", "/tmp/z");
        inst1.group_path = "Zebra".to_string();
        let mut inst2 = Instance::new("a-session", "/tmp/a");
        inst2.group_path = "apple".to_string();
        let instances = vec![inst1, inst2];
        let tree = GroupTree::new_with_groups(&instances, &[]);

        let items = flatten_tree(&tree, &instances, SortOrder::AZ);
        let group_names: Vec<_> = items
            .iter()
            .filter_map(|i| match i {
                Item::Group { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(group_names, vec!["apple", "Zebra"]);
    }

    #[test]
    fn test_existing_groups_vec_order_preserved_on_load() {
        let gamma_group = Group::new("gamma", "gamma");
        let alpha_group = Group::new("alpha", "alpha");
        let existing_groups = vec![gamma_group, alpha_group];

        let instances: Vec<Instance> = vec![];
        let tree = GroupTree::new_with_groups(&instances, &existing_groups);

        let roots = tree.get_roots();
        let root_names: Vec<_> = roots.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(root_names, vec!["gamma", "alpha"]);

        let all_groups: Vec<_> = tree
            .get_all_groups()
            .into_iter()
            .map(|g| g.name.as_str().to_string())
            .collect();
        assert_eq!(all_groups, vec!["gamma".to_string(), "alpha".to_string()]);
    }

    #[test]
    fn test_rename_group_simple() {
        let mut inst = Instance::new("test", "/tmp/t");
        inst.group_path = "work".to_string();
        let instances = vec![inst];
        let mut tree = GroupTree::new_with_groups(&instances, &[]);

        tree.rename_group("work", "projects");

        assert!(!tree.group_exists("work"));
        assert!(tree.group_exists("projects"));
        assert_eq!(
            tree.groups_by_path.get("projects").unwrap().name,
            "projects"
        );
    }

    #[test]
    fn test_rename_group_with_children() {
        let mut inst1 = Instance::new("test1", "/tmp/1");
        inst1.group_path = "work".to_string();
        let mut inst2 = Instance::new("test2", "/tmp/2");
        inst2.group_path = "work/frontend".to_string();
        let instances = vec![inst1, inst2];
        let mut tree = GroupTree::new_with_groups(&instances, &[]);

        tree.rename_group("work", "projects");

        assert!(!tree.group_exists("work"));
        assert!(!tree.group_exists("work/frontend"));
        assert!(tree.group_exists("projects"));
        assert!(tree.group_exists("projects/frontend"));
    }

    #[test]
    fn test_rename_group_merge_into_existing() {
        let mut inst1 = Instance::new("test1", "/tmp/1");
        inst1.group_path = "old".to_string();
        let mut inst2 = Instance::new("test2", "/tmp/2");
        inst2.group_path = "existing".to_string();
        let instances = vec![inst1, inst2];
        let mut tree = GroupTree::new_with_groups(&instances, &[]);

        tree.rename_group("old", "existing");

        assert!(!tree.group_exists("old"));
        assert!(tree.group_exists("existing"));
    }

    #[test]
    fn test_rename_group_noop_same_path() {
        let mut inst = Instance::new("test", "/tmp/t");
        inst.group_path = "work".to_string();
        let instances = vec![inst];
        let mut tree = GroupTree::new_with_groups(&instances, &[]);

        tree.rename_group("work", "work");

        assert!(tree.group_exists("work"));
    }

    #[test]
    fn test_rename_group_noop_empty_target() {
        let mut inst = Instance::new("test", "/tmp/t");
        inst.group_path = "work".to_string();
        let instances = vec![inst];
        let mut tree = GroupTree::new_with_groups(&instances, &[]);

        tree.rename_group("work", "");

        assert!(tree.group_exists("work"));
    }

    // ─── Archive feature tests ───────────────────────────────────────────

    #[test]
    fn test_attention_tier_archived_returns_99() {
        let mut waiting = Instance::new("w", "/tmp/w");
        waiting.status = crate::session::Status::Waiting;
        assert_eq!(attention_tier(&waiting), 0);

        // Archive a Waiting session; must short-circuit to 99
        waiting.archive();
        assert_eq!(attention_tier(&waiting), 99);

        // Same for Error
        let mut errored = Instance::new("e", "/tmp/e");
        errored.status = crate::session::Status::Error;
        assert_eq!(attention_tier(&errored), 1);
        errored.archive();
        assert_eq!(attention_tier(&errored), 99);

        // Unarchive restores the original tier
        errored.unarchive();
        assert_eq!(attention_tier(&errored), 1);
    }

    #[test]
    fn test_attention_sort_within_tier_aging_ascending() {
        // Longest-aging-first rule: within a single priority tier, the session
        // whose last_accessed_at is oldest bubbles to the top (the one most
        // likely to have been forgotten). Untouched (None) rows bucket after
        // dated ones so a brand-new session doesn't falsely claim top.
        use chrono::Duration;
        let now = chrono::Utc::now();

        let mut fresh = Instance::new("fresh", "/tmp/fresh");
        fresh.status = crate::session::Status::Idle;
        fresh.last_accessed_at = Some(now - Duration::minutes(5));

        let mut stale = Instance::new("stale", "/tmp/stale");
        stale.status = crate::session::Status::Idle;
        stale.last_accessed_at = Some(now - Duration::hours(11));

        let mut middle = Instance::new("middle", "/tmp/middle");
        middle.status = crate::session::Status::Idle;
        middle.last_accessed_at = Some(now - Duration::minutes(40));

        let mut untouched = Instance::new("untouched", "/tmp/untouched");
        untouched.status = crate::session::Status::Idle;
        untouched.last_accessed_at = None;

        let mut sessions: Vec<&Instance> = vec![&fresh, &middle, &stale, &untouched];
        sort_sessions(&mut sessions, SortOrder::Attention);

        let titles: Vec<&str> = sessions.iter().map(|i| i.title.as_str()).collect();
        assert_eq!(
            titles,
            vec!["stale", "middle", "fresh", "untouched"],
            "oldest last_accessed_at should sort first within a tier; None last"
        );
    }

    #[test]
    fn test_attention_group_key_within_tier_aging_ascending() {
        // Same rule at group level: the group whose most-recent member
        // activity is oldest ranks first. Mirrors the session-level aging
        // tiebreak so tree view and flat view stay consistent.
        use chrono::Duration;
        let now = chrono::Utc::now();

        let mut fresh_member = Instance::new("f", "/tmp/f");
        fresh_member.group_path = "fresh".to_string();
        fresh_member.status = crate::session::Status::Idle;
        fresh_member.last_accessed_at = Some(now - Duration::minutes(5));

        let mut stale_member = Instance::new("s", "/tmp/s");
        stale_member.group_path = "stale".to_string();
        stale_member.status = crate::session::Status::Idle;
        stale_member.last_accessed_at = Some(now - Duration::hours(11));

        let instances = vec![fresh_member, stale_member];
        let fresh_key = attention_group_key("fresh", None, &instances);
        let stale_key = attention_group_key("stale", None, &instances);

        assert!(
            stale_key < fresh_key,
            "stale group (11h) should sort before fresh group (5m); got stale={:?} fresh={:?}",
            stale_key,
            fresh_key
        );
    }

    #[test]
    fn test_attention_sort_archived_sinks_to_bottom() {
        // Build: 1 Waiting, 1 Error, 1 archived (was Waiting), 1 Idle
        let mut waiting = Instance::new("w", "/tmp/w");
        waiting.status = crate::session::Status::Waiting;
        let mut errored = Instance::new("e", "/tmp/e");
        errored.status = crate::session::Status::Error;
        let mut archived_waiting = Instance::new("aw", "/tmp/aw");
        archived_waiting.status = crate::session::Status::Waiting;
        archived_waiting.archive();
        let mut idle = Instance::new("i", "/tmp/i");
        idle.status = crate::session::Status::Idle;

        let mut sessions: Vec<&Instance> = vec![&waiting, &errored, &archived_waiting, &idle];
        sort_sessions(&mut sessions, SortOrder::Attention);

        // Order should be: Waiting(0), Error(1), Idle(2), Archived(99)
        let titles: Vec<&str> = sessions.iter().map(|i| i.title.as_str()).collect();
        assert_eq!(titles, vec!["w", "e", "i", "aw"]);
    }

    #[test]
    fn test_group_toggle_archived() {
        let mut inst = Instance::new("t", "/tmp/t");
        inst.group_path = "work".to_string();
        let instances = vec![inst];
        let mut tree = GroupTree::new_with_groups(&instances, &[]);

        // Initial: not archived
        assert!(tree.group_archived_at("work").is_none());

        // Toggle on
        let result = tree.toggle_archived("work");
        assert_eq!(result, Some(true));
        assert!(tree.group_archived_at("work").is_some());

        // Toggle off
        let result = tree.toggle_archived("work");
        assert_eq!(result, Some(false));
        assert!(tree.group_archived_at("work").is_none());

        // Nonexistent group returns None
        assert_eq!(tree.toggle_archived("nope"), None);
    }

    #[test]
    fn test_attention_group_key_all_members_archived() {
        let mut a = Instance::new("a", "/tmp/a");
        a.group_path = "work".to_string();
        a.status = crate::session::Status::Waiting; // would be tier 0 if not archived
        a.archive();
        let mut b = Instance::new("b", "/tmp/b");
        b.group_path = "work".to_string();
        b.status = crate::session::Status::Idle;
        b.archive();

        let instances = vec![a, b];
        let key = attention_group_key("work", None, &instances);
        assert_eq!(key.1, 99, "all-archived group should sort to tier 99");
    }

    #[test]
    fn test_attention_group_key_one_active_pulls_group_up() {
        // Mixed group: 1 archived Waiting, 1 active Idle. The active member
        // pulls the group out of the archive tier (99) into the Idle bucket.
        // With the unread promoter enabled (default), a non-unread Idle group
        // encodes to rank 3 (Waiting=0, unread=1, other tiers shifted +1, so
        // Idle tier 2 -> 3); the point is it's well above the archive tier.
        let mut archived_waiting = Instance::new("aw", "/tmp/aw");
        archived_waiting.group_path = "work".to_string();
        archived_waiting.status = crate::session::Status::Waiting;
        archived_waiting.archive();
        let mut active_idle = Instance::new("ai", "/tmp/ai");
        active_idle.group_path = "work".to_string();
        active_idle.status = crate::session::Status::Idle;

        let instances = vec![archived_waiting, active_idle];
        let key = attention_group_key("work", None, &instances);
        assert_eq!(
            key.1, 3,
            "active Idle group should sort in the Idle bucket, far above archive tier 99"
        );
    }

    #[test]
    fn test_attention_group_key_promotes_unread_below_waiting() {
        // Group A holds a read Error (tier 1); Group B holds an unread Idle
        // (tier 2). With the unread promoter on (default), B's unread member
        // floats it to rank 1, above A's Error (rank 2), matching how the flat
        // Attention view promotes unread just below Waiting.
        let mut err = Instance::new("err", "/tmp/err");
        err.group_path = "a".to_string();
        err.status = crate::session::Status::Error;

        let mut unread_idle = Instance::new("ui", "/tmp/ui");
        unread_idle.group_path = "b".to_string();
        unread_idle.status = crate::session::Status::Idle;
        unread_idle.mark_unread();

        let key_a = attention_group_key("a", None, std::slice::from_ref(&err));
        let key_b = attention_group_key("b", None, std::slice::from_ref(&unread_idle));
        assert!(
            key_b < key_a,
            "group with an unread Idle must outrank a group whose best member is a read Error: b={key_b:?} a={key_a:?}"
        );
    }

    #[test]
    fn test_attention_group_key_sunk_unread_member_does_not_promote() {
        // A group with one active *read* Idle member and one *archived* member
        // that still carries an unread marker (archive doesn't clear `unread`)
        // must NOT be promoted: the dismissed member can't drag the group up.
        // Rank should be the plain Idle bucket, identical to the same group
        // without any unread anywhere.
        let mut active_read = Instance::new("ar", "/tmp/ar");
        active_read.group_path = "g".to_string();
        active_read.status = crate::session::Status::Idle;

        let mut archived_unread = Instance::new("au", "/tmp/au");
        archived_unread.group_path = "g".to_string();
        archived_unread.status = crate::session::Status::Idle;
        archived_unread.mark_unread();
        archived_unread.archive(); // archived_at set; unread intentionally kept

        let members = vec![active_read.clone(), archived_unread];
        let key = attention_group_key("g", None, &members);

        // Same group but with the second member simply read: ranks identical,
        // proving the sunk unread member contributed nothing.
        let read_baseline = vec![active_read.clone(), {
            let mut other = Instance::new("o", "/tmp/o");
            other.group_path = "g".to_string();
            other.status = crate::session::Status::Idle;
            other.archive();
            other
        }];
        let baseline = attention_group_key("g", None, &read_baseline);
        assert_eq!(
            key.1, baseline.1,
            "an archived unread member must not promote the group: rank={} baseline={}",
            key.1, baseline.1
        );
    }

    #[test]
    fn test_attention_group_key_empty_archived_group() {
        let now = chrono::Utc::now();
        let key = attention_group_key("empty", Some(now), &[]);
        assert_eq!(key.1, 99, "empty archived group sinks to tier 99");

        let key_unarchived = attention_group_key("empty", None, &[]);
        assert_eq!(
            key_unarchived.1,
            u8::MAX,
            "empty unarchived group keeps prior u8::MAX behavior"
        );
    }

    #[test]
    fn test_favorite_pins_waiting_above_non_favorited_waiting() {
        // Two Waiting sessions. The favorited one must sort above the
        // non-favorited one despite having no aging difference.
        let mut fav = Instance::new("fav", "/tmp/fav");
        fav.status = crate::session::Status::Waiting;
        fav.favorite();
        let plain = {
            let mut p = Instance::new("plain", "/tmp/plain");
            p.status = crate::session::Status::Waiting;
            p
        };
        let fav_key = attention_session_key(&fav);
        let plain_key = attention_session_key(&plain);
        assert!(
            fav_key < plain_key,
            "favorited+Waiting should sort before non-favorited+Waiting: fav={fav_key:?} plain={plain_key:?}"
        );
    }

    #[test]
    fn test_favorite_does_not_cross_tiers() {
        // Revised spec (2026-04-23): "favorites should be top of their
        // respective category." Tier stays primary; a favorited lower-
        // priority row never leaps above a non-favorited higher-priority
        // peer. Fav+Idle (tier 2) must sink below plain+Waiting (tier 0).
        let mut fav_idle = Instance::new("fav_idle", "/tmp/fi");
        fav_idle.status = crate::session::Status::Idle;
        fav_idle.favorite();
        let mut plain_waiting = Instance::new("plain_waiting", "/tmp/pw");
        plain_waiting.status = crate::session::Status::Waiting;
        let fav_key = attention_session_key(&fav_idle);
        let plain_key = attention_session_key(&plain_waiting);
        assert!(
            plain_key < fav_key,
            "plain+Waiting (tier 0) must sort above fav+Idle (tier 2): plain={plain_key:?} fav={fav_key:?}"
        );
    }

    #[test]
    fn test_attention_rank_unread_promoter() {
        // Tiers: Waiting=0, Error=1, Idle=2, Unknown=3, Running=4.
        // Feature ON: an unread row ranks just below Waiting (0) and above
        // every other tier, so an unread Idle beats a read Error/Running.
        let waiting = attention_rank(0, false, true);
        let unread_idle = attention_rank(2, true, true);
        let read_error = attention_rank(1, false, true);
        let read_running = attention_rank(4, false, true);
        assert!(waiting < unread_idle, "Waiting must stay above unread");
        assert!(
            unread_idle < read_error,
            "unread Idle must rank above a read Error"
        );
        assert!(
            unread_idle < read_running,
            "unread Idle must rank above a read Running"
        );

        // Feature OFF: rank is the raw tier, so ordering is unchanged and an
        // (impossible-when-off, but defensive) unread flag does not promote.
        assert_eq!(attention_rank(0, false, false), 0);
        assert_eq!(attention_rank(2, true, false), 2);
        assert_eq!(attention_rank(4, false, false), 4);

        assert_eq!(
            attention_rank(99, true, true),
            99,
            "unread must not promote sunk rows"
        );
        assert_eq!(
            attention_rank(99, false, true),
            99,
            "read sunk rows must keep rank 99"
        );

        // OFF ordering matches the pre-feature tier order for read rows.
        assert!(attention_rank(0, false, false) < attention_rank(1, false, false));
        assert!(attention_rank(1, false, false) < attention_rank(2, false, false));
    }

    #[test]
    fn test_favorite_pins_within_running_tier() {
        // User rule: "favorites should be top of their respective category."
        // Running sessions (tier 4, the "processing" bucket) now pin fav
        // above non-fav peers; previously fav was a no-op for Running.
        let mut fav_running = Instance::new("fav_r", "/tmp/fr");
        fav_running.status = crate::session::Status::Running;
        fav_running.favorite();
        let mut plain_running = Instance::new("plain_r", "/tmp/pr");
        plain_running.status = crate::session::Status::Running;
        let fav_key = attention_session_key(&fav_running);
        let plain_key = attention_session_key(&plain_running);
        assert!(
            fav_key < plain_key,
            "favorited Running pins above plain Running: fav={fav_key:?} plain={plain_key:?}"
        );
    }

    #[test]
    fn test_favorite_pins_within_stopped_tier() {
        // Same rule applied to Stopped (tier 5). "Respective category"
        // means every non-sunk tier; favorite is a universal within-tier
        // pin, not a needs-help-only pin.
        let mut fav_stopped = Instance::new("fav_s", "/tmp/fs");
        fav_stopped.status = crate::session::Status::Stopped;
        fav_stopped.favorite();
        let mut plain_stopped = Instance::new("plain_s", "/tmp/ps");
        plain_stopped.status = crate::session::Status::Stopped;
        let fav_key = attention_session_key(&fav_stopped);
        let plain_key = attention_session_key(&plain_stopped);
        assert!(
            fav_key < plain_key,
            "favorited Stopped pins above plain Stopped: fav={fav_key:?} plain={plain_key:?}"
        );
    }

    #[test]
    fn test_archive_clears_favorite() {
        // Mutual exclusion: archive() explicitly clears favorited_at. The
        // user's rule is "archived removes fav"; pinning a sunk row is
        // incoherent, so archive hard-wins. Previous behavior (both flags
        // coexisting with archive-beats-favorite at sort time) produced
        // confusing "favorite icon on an archived row" JSON output.
        let mut inst = Instance::new("t", "/tmp/t");
        inst.status = crate::session::Status::Waiting;
        inst.favorite();
        assert!(inst.is_favorited(), "pre-condition: fav is set");
        inst.archive();
        assert!(inst.is_archived(), "archive set");
        assert!(!inst.is_favorited(), "archive cleared favorite");
        let key = attention_session_key(&inst);
        assert_eq!(key.1, 99, "tier 99 (archived)");
        assert!(key.2, "no favorite bias (bias bool is 'true' = !pinned)");
    }

    #[test]
    fn test_favorite_clears_archive() {
        // User's rule: "marking as favorite unarchives." favorite()
        // explicitly clears archived_at so pressing `f` on an archived
        // row actually surfaces it. Without this, tier 99 suppresses the
        // favorite bias and the row stays buried.
        let mut inst = Instance::new("t", "/tmp/t");
        inst.archive();
        assert!(inst.is_archived(), "pre-condition: archived");
        inst.favorite();
        assert!(inst.is_favorited(), "fav set");
        assert!(!inst.is_archived(), "fav cleared archive");
    }

    #[test]
    fn test_favorite_clears_snooze() {
        // Snooze shares tier 99 with archive, so a snoozed session is
        // equally buried and equally defeats the favorite bias. Favorite's
        // clear-everything-that-hides-me rule extends to snooze.
        let mut inst = Instance::new("t", "/tmp/t");
        inst.snooze(30);
        assert!(inst.is_snoozed(), "pre-condition: snoozed");
        inst.favorite();
        assert!(inst.is_favorited(), "fav set");
        assert!(!inst.is_snoozed(), "fav cleared snooze");
    }

    #[test]
    fn test_user_interaction_wakes_archive_and_snooze() {
        // `touch_last_accessed` is called on every user-initiated
        // interaction (send message, attach). User rule: "messaging should
        // unarchive." A user talking to a session is explicit evidence they
        // care about it; leaving it sunk at tier 99 is incoherent.
        // Favorite stays (orthogonal signal).
        let mut inst = Instance::new("t", "/tmp/t");
        inst.favorite();
        inst.archive();
        // archive cleared fav per mutex; resurrect fav for this test
        inst.favorite();
        inst.snooze(30);
        // snooze preserves fav; archive was cleared by fav above. Re-archive
        // to cover both flags simultaneously (even though the mutex prevents
        // the CLI paths from producing this state, the test exercises the
        // wake logic directly).
        inst.archived_at = Some(chrono::Utc::now());
        assert!(
            inst.is_archived() && inst.is_snoozed() && inst.is_favorited(),
            "pre-condition: all three set"
        );
        inst.touch_last_accessed();
        assert!(!inst.is_archived(), "user interaction cleared archive");
        assert!(!inst.is_snoozed(), "user interaction cleared snooze");
        assert!(inst.is_favorited(), "user interaction preserved favorite");
        assert!(inst.last_accessed_at.is_some(), "timestamp stamped");
    }

    #[test]
    fn test_favorited_session_serde_roundtrip() {
        let mut inst = Instance::new("t", "/tmp/t");
        inst.favorite();
        let json = serde_json::to_string(&inst).unwrap();
        assert!(
            json.contains("favorited_at"),
            "favorited_at must serialize when set"
        );
        let parsed: Instance = serde_json::from_str(&json).unwrap();
        assert!(
            parsed.is_favorited(),
            "favorited_at round-trips through JSON"
        );
    }

    #[test]
    fn test_archived_session_serde_roundtrip() {
        let mut inst = Instance::new("t", "/tmp/t");
        inst.archive();
        let json = serde_json::to_string(&inst).unwrap();
        assert!(json.contains("archived_at"));

        let parsed: Instance = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_archived());
    }

    #[test]
    fn test_unarchived_session_skips_field_in_json() {
        // skip_serializing_if = "Option::is_none" means archived_at is
        // omitted entirely when None. Backward-compatible for existing
        // sessions.json files.
        let inst = Instance::new("t", "/tmp/t");
        let json = serde_json::to_string(&inst).unwrap();
        assert!(
            !json.contains("archived_at"),
            "archived_at should be omitted when None: {}",
            json
        );
    }

    #[test]
    fn test_snoozed_session_is_snoozed_while_future() {
        let mut inst = Instance::new("s", "/tmp/s");
        inst.snooze(30);
        assert!(inst.is_snoozed(), "fresh 30m snooze is active");
        assert!(inst.snooze_remaining().is_some());
    }

    #[test]
    fn test_expired_snooze_reports_not_snoozed() {
        let mut inst = Instance::new("s", "/tmp/s");
        // Stale past timestamp; the lazy predicate should reject it so
        // the row rejoins the active Attention sort on next render.
        inst.snoozed_until = Some(Utc::now() - chrono::Duration::minutes(5));
        assert!(
            !inst.is_snoozed(),
            "past snoozed_until must read as NOT snoozed"
        );
        assert!(inst.snooze_remaining().is_none());
    }

    #[test]
    fn test_unsnooze_clears_timestamp() {
        let mut inst = Instance::new("s", "/tmp/s");
        inst.snooze(30);
        inst.unsnooze();
        assert!(!inst.is_snoozed());
        assert!(inst.snoozed_until.is_none());
    }

    #[test]
    fn test_snooze_pushes_to_tier_99() {
        let mut inst = Instance::new("s", "/tmp/s");
        inst.status = crate::session::Status::Waiting;
        assert_eq!(attention_tier(&inst), 0, "baseline: waiting is tier 0");
        inst.snooze(30);
        assert_eq!(
            attention_tier(&inst),
            99,
            "snoozed waiting sinks to archive tier"
        );
    }

    #[test]
    fn test_expired_snooze_does_not_hold_tier_99() {
        let mut inst = Instance::new("s", "/tmp/s");
        inst.status = crate::session::Status::Waiting;
        inst.snoozed_until = Some(Utc::now() - chrono::Duration::seconds(1));
        assert_eq!(
            attention_tier(&inst),
            0,
            "once the timer elapses, tier returns to the natural status bucket"
        );
    }

    #[test]
    fn test_snoozed_session_serde_roundtrip() {
        let mut inst = Instance::new("s", "/tmp/s");
        inst.snooze(30);
        let json = serde_json::to_string(&inst).unwrap();
        assert!(
            json.contains("snoozed_until"),
            "snoozed_until must serialize when set"
        );
        let parsed: Instance = serde_json::from_str(&json).unwrap();
        assert!(
            parsed.is_snoozed(),
            "snoozed_until round-trips through JSON"
        );
    }

    #[test]
    fn test_non_snoozed_session_skips_field_in_json() {
        let inst = Instance::new("s", "/tmp/s");
        let json = serde_json::to_string(&inst).unwrap();
        assert!(
            !json.contains("snoozed_until"),
            "snoozed_until should be omitted when None: {}",
            json
        );
    }

    #[test]
    fn test_archive_beats_snooze_on_prefix() {
        // Precedence rule: archive wins over snooze. Both flags set
        // together should still honor archive (tier 99 is shared; the
        // prefix rule is tested in render, but here we just verify the
        // predicates disagree cleanly).
        let mut inst = Instance::new("s", "/tmp/s");
        inst.archive();
        inst.snooze(30);
        assert!(inst.is_archived());
        assert!(inst.is_snoozed());
        assert_eq!(attention_tier(&inst), 99);
    }

    #[test]
    fn test_legacy_json_without_archived_at_deserializes() {
        // Existing sessions.json files predate this field. Verify they load
        // cleanly with archived_at defaulting to None.
        let legacy = r#"{
            "id": "abc",
            "title": "old",
            "project_path": "/tmp/old",
            "created_at": "2026-01-01T00:00:00Z"
        }"#;
        let inst: Instance = serde_json::from_str(legacy).unwrap();
        assert!(!inst.is_archived());
        assert!(inst.archived_at.is_none());
    }
}
