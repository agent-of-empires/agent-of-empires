//! Session lifecycle changes (created, archived, trashed, renamed, deleted ...) read off
//! committed rows.
//!
//! The tracker is the single source of truth: both the exact commit observer and the
//! coalesced disk diff report only what changes its snapshot, so nothing is reported twice.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::Instance;

/// The lifecycle-relevant part of one committed session row, plus the `sessions.list` entry to
/// report it with.
#[derive(Clone, Debug, PartialEq)]
pub struct LifecycleRow {
    pub id: String,
    /// The row's effective profile.
    pub profile: String,
    pub title: String,
    pub archived: bool,
    pub trashed: bool,
    /// Sorted paths of every checkout the session works in. Branches are left out: a worker
    /// reads them live, and a branch switch is not a different checkout.
    pub checkouts: Vec<String>,
    /// The `sessions.list` entry for this session, from the shared `session_summary`.
    pub summary: Value,
}

impl LifecycleRow {
    pub fn of(inst: &Instance, profile: &str) -> Self {
        let mut checkouts: Vec<String> = std::iter::once(inst.project_path.clone())
            .chain(inst.worktree_info.iter().map(|w| w.main_repo_path.clone()))
            .chain(inst.all_repos().iter().map(|r| r.source_path.clone()))
            .collect();
        checkouts.sort_unstable();
        Self {
            id: inst.id.clone(),
            profile: profile.to_string(),
            title: inst.title.clone(),
            archived: inst.is_archived(),
            trashed: inst.is_trashed(),
            checkouts,
            summary: crate::plugin::host_api::session_summary(inst),
        }
    }
}

pub fn rows_of(instances: &[Instance], profile: &str) -> Vec<LifecycleRow> {
    instances
        .iter()
        .map(|inst| LifecycleRow::of(inst, profile))
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleKind {
    Created,
    Deleted,
    Archived,
    Unarchived,
    Trashed,
    Restored,
    Renamed,
    WorkspaceChanged,
}

impl LifecycleKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Deleted => "deleted",
            Self::Archived => "archived",
            Self::Unarchived => "unarchived",
            Self::Trashed => "trashed",
            Self::Restored => "restored",
            Self::Renamed => "renamed",
            Self::WorkspaceChanged => "workspace_changed",
        }
    }
}

#[derive(Clone, Debug)]
pub struct LifecycleChange {
    pub session_id: String,
    pub effective_profile: String,
    pub kind: LifecycleKind,
    pub at: DateTime<Utc>,
    /// The session as of this change; `None` for a deleted session.
    pub summary: Option<Value>,
}

fn change(row: &LifecycleRow, kind: LifecycleKind) -> LifecycleChange {
    LifecycleChange {
        session_id: row.id.clone(),
        effective_profile: row.profile.clone(),
        kind,
        at: Utc::now(),
        summary: (kind != LifecycleKind::Deleted).then(|| row.summary.clone()),
    }
}

/// A session as one profile lists it. A session in two profiles at once, which a profile move
/// briefly writes, is two entries: each profile's plugins see their own list.
type Key = (String, String);

fn key(row: &LifecycleRow) -> Key {
    (row.profile.clone(), row.id.clone())
}

/// Every session of every profile, keyed by profile and id.
#[derive(Default)]
pub struct LifecycleTracker {
    rows: HashMap<Key, LifecycleRow>,
    /// Entries missing from the last `observe`; reported deleted only if missing again.
    suspects: HashSet<Key>,
    /// Set by the first `observe` or `listing_failed`.
    baselined: bool,
    /// Profiles whose rows the baseline could not record.
    unseeded: Unseeded,
    /// Commits applied so far, so a disk snapshot read before one is not applied after it.
    commits: u64,
}

/// The profiles the baseline could not read. Such a profile starts from whichever comes first:
/// its first successful read, adopted silently as the baseline would have, or the before rows
/// of a commit to it, which are the state a worker listing it last saw.
#[derive(Default)]
struct Unseeded {
    /// Even the profile list was unreadable, so every profile counts until seeded.
    all: bool,
    names: HashSet<String>,
    /// With `all`, the profiles a commit has seeded since.
    seeded: HashSet<String>,
}

impl Unseeded {
    fn contains(&self, profile: &str) -> bool {
        (self.all && !self.seeded.contains(profile)) || self.names.contains(profile)
    }

    fn seed(&mut self, profile: &str) {
        self.names.remove(profile);
        if self.all {
            self.seeded.insert(profile.to_string());
        }
    }
}

impl LifecycleTracker {
    pub fn commits(&self) -> u64 {
        self.commits
    }

    /// No baseline could be recorded, so nothing is known about any profile yet: each starts
    /// from its first complete read or its first commit, whichever comes first.
    pub fn baseline_unavailable(&mut self) {
        if !self.baselined {
            self.baselined = true;
            self.unseeded.all = true;
        }
    }

    /// Whether an `observe` should be repeated shortly to confirm or clear a missing row.
    /// Suspects of an `unread` profile cannot be looked at again until it reads, so they do
    /// not count.
    pub fn has_suspects(&self, unread: &[String]) -> bool {
        self.suspects
            .iter()
            .any(|(profile, _)| !unread.contains(profile))
    }

    /// Diff the rows of every profile against the snapshot. The first call only records them.
    ///
    /// An entry missing from one observe is only a suspect: a profile move writes the target
    /// before it removes the source, and a reader can catch the row in neither file. Entries
    /// of an `unread` profile, whose storage could not be read, are neither missing nor
    /// suspect, and a profile unread at the baseline is adopted silently once it reads.
    pub fn observe(&mut self, rows: &[LifecycleRow], unread: &[String]) -> Vec<LifecycleChange> {
        if !self.baselined {
            self.rows = rows.iter().map(|r| (key(r), r.clone())).collect();
            self.unseeded.names = unread.iter().cloned().collect();
            self.baselined = true;
            return Vec::new();
        }
        let mut out = Vec::new();
        for row in rows {
            if self.unseeded.contains(&row.profile) {
                self.rows.insert(key(row), row.clone());
                continue;
            }
            self.suspects.remove(&key(row));
            self.apply_row(row, &mut out);
        }
        // Every profile that read is seeded now; only the unread ones still wait. After a failed
        // profile list nothing was named yet, so the unread ones are named here.
        if self.unseeded.all {
            let seeded = std::mem::take(&mut self.unseeded.seeded);
            self.unseeded.names = unread
                .iter()
                .filter(|p| !seeded.contains(*p))
                .cloned()
                .collect();
            self.unseeded.all = false;
        } else {
            self.unseeded.names.retain(|p| unread.contains(p));
        }
        let present: HashSet<Key> = rows.iter().map(key).collect();
        let mut missing: Vec<Key> = self
            .rows
            .keys()
            .filter(|k| !present.contains(*k) && !unread.contains(&k.0))
            .cloned()
            .collect();
        missing.sort();
        for k in missing {
            if self.suspects.remove(&k) {
                if let Some(gone) = self.rows.remove(&k) {
                    out.push(change(&gone, LifecycleKind::Deleted));
                }
            } else {
                self.suspects.insert(k);
            }
        }
        out
    }

    /// Apply one committed write of a profile's rows. Exact: nothing is coalesced.
    pub fn commit(
        &mut self,
        before: &[LifecycleRow],
        after: &[LifecycleRow],
    ) -> Vec<LifecycleChange> {
        self.commits += 1;
        if !self.baselined {
            return Vec::new();
        }
        let mut out = Vec::new();
        if let Some(profile) = before.first().or(after.first()).map(|r| r.profile.as_str()) {
            if self.unseeded.contains(profile) {
                for row in before {
                    self.rows.insert(key(row), row.clone());
                }
                self.unseeded.seed(profile);
            }
        }
        for row in after {
            self.suspects.remove(&key(row));
            self.apply_row(row, &mut out);
        }
        let kept: HashSet<Key> = after.iter().map(key).collect();
        for row in before.iter().filter(|r| !kept.contains(&key(r))) {
            self.suspects.remove(&key(row));
            if let Some(gone) = self.rows.remove(&key(row)) {
                out.push(change(&gone, LifecycleKind::Deleted));
            }
        }
        out
    }

    fn apply_row(&mut self, row: &LifecycleRow, out: &mut Vec<LifecycleChange>) {
        let Some(tracked) = self.rows.get_mut(&key(row)) else {
            self.rows.insert(key(row), row.clone());
            out.push(change(row, LifecycleKind::Created));
            return;
        };
        let (old, new) = (&*tracked, row);
        if old.archived != new.archived {
            let kind = if new.archived {
                LifecycleKind::Archived
            } else {
                LifecycleKind::Unarchived
            };
            out.push(change(row, kind));
        }
        if old.trashed != new.trashed {
            let kind = if new.trashed {
                LifecycleKind::Trashed
            } else {
                LifecycleKind::Restored
            };
            out.push(change(row, kind));
        }
        if old.title != new.title {
            out.push(change(row, LifecycleKind::Renamed));
        }
        if old.checkouts != new.checkouts {
            out.push(change(row, LifecycleKind::WorkspaceChanged));
        }
        *tracked = row.clone();
    }
}

type CommitObserver = dyn Fn(&str, &[LifecycleRow], &[LifecycleRow]) + Send + Sync;

static COMMIT_OBSERVER: RwLock<Option<Arc<CommitObserver>>> = RwLock::new(None);

/// Uninstalls the commit observer on drop.
pub struct CommitObserverGuard(());

impl Drop for CommitObserverGuard {
    fn drop(&mut self) {
        *COMMIT_OBSERVER.write().unwrap_or_else(|p| p.into_inner()) = None;
    }
}

/// Call `observer(profile, rows_before, rows_after)` after every committed
/// `Storage::update` in this process. The observer runs under the storage lock, so it must
/// be quick and must not touch storage.
pub fn install_commit_observer(
    observer: impl Fn(&str, &[LifecycleRow], &[LifecycleRow]) + Send + Sync + 'static,
) -> CommitObserverGuard {
    *COMMIT_OBSERVER.write().unwrap_or_else(|p| p.into_inner()) = Some(Arc::new(observer));
    CommitObserverGuard(())
}

/// Whether an observer is installed, so a commit can skip building rows when none is.
pub(crate) fn observing() -> bool {
    COMMIT_OBSERVER
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .is_some()
}

pub(crate) fn notify_commit(profile: &str, before: &[LifecycleRow], after: &[LifecycleRow]) {
    let observer = COMMIT_OBSERVER
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    if let Some(observer) = observer {
        observer(profile, before, after);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Instance, Storage};
    use serde_json::json;

    fn row(id: &str, profile: &str, archived: bool, trashed: bool) -> LifecycleRow {
        LifecycleRow {
            id: id.to_string(),
            profile: profile.to_string(),
            title: format!("title-{id}"),
            archived,
            trashed,
            checkouts: vec![format!("/p/{id}")],
            summary: json!({ "id": id, "title": format!("title-{id}") }),
        }
    }

    fn live(id: &str) -> LifecycleRow {
        row(id, "p", false, false)
    }

    fn events(changes: &[LifecycleChange]) -> Vec<(&str, &str)> {
        changes
            .iter()
            .map(|c| (c.kind.as_str(), c.session_id.as_str()))
            .collect()
    }

    #[test]
    fn observe_seeds_silently_then_reports_each_change() {
        let mut tracker = LifecycleTracker::default();
        assert!(
            tracker.observe(&[live("a")], &[]).is_empty(),
            "the seed is silent"
        );

        let renamed = LifecycleRow {
            title: "new title".to_string(),
            ..live("a")
        };
        let moved = LifecycleRow {
            checkouts: vec!["/p/a".to_string(), "/repo/x".to_string()],
            ..renamed.clone()
        };
        // Only the recorded branch differs: the same checkouts.
        let switched = LifecycleRow {
            summary: json!({ "id": "a", "branch": "feature" }),
            ..moved.clone()
        };
        let steps: [(Vec<LifecycleRow>, &[(&str, &str)]); 9] = [
            (vec![live("a"), live("b")], &[("created", "b")]),
            (
                vec![row("a", "p", true, false), live("b")],
                &[("archived", "a")],
            ),
            (
                vec![row("a", "p", true, true), live("b")],
                &[("trashed", "a")],
            ),
            (
                vec![row("a", "p", false, true), live("b")],
                &[("unarchived", "a")],
            ),
            (vec![live("a"), live("b")], &[("restored", "a")]),
            (vec![renamed, live("b")], &[("renamed", "a")]),
            (vec![moved, live("b")], &[("workspace_changed", "a")]),
            (vec![switched, live("b")], &[]),
            (
                vec![live("a"), live("b")],
                &[("renamed", "a"), ("workspace_changed", "a")],
            ),
        ];
        for (rows, want) in steps {
            assert_eq!(events(&tracker.observe(&rows, &[])), want);
        }
    }

    #[test]
    fn a_change_carries_the_session_summary_except_a_deletion() {
        let mut tracker = LifecycleTracker::default();
        tracker.observe(&[live("a")], &[]);

        let created = tracker.observe(&[live("a"), live("b")], &[]);
        assert_eq!(
            created[0].summary,
            Some(json!({ "id": "b", "title": "title-b" }))
        );
        assert_eq!(created[0].effective_profile, "p");

        tracker.observe(&[live("a")], &[]);
        let gone = tracker.observe(&[live("a")], &[]);
        assert_eq!(events(&gone), [("deleted", "b")]);
        assert!(
            gone[0].summary.is_none(),
            "a deleted session has no summary"
        );
    }

    #[test]
    fn observe_confirms_a_deletion_on_the_second_miss() {
        let mut tracker = LifecycleTracker::default();
        tracker.observe(&[live("a"), live("b")], &[]);

        assert!(tracker.observe(&[live("a")], &[]).is_empty());
        assert!(tracker.has_suspects(&[]), "one miss only makes b a suspect");
        assert_eq!(
            events(&tracker.observe(&[live("a")], &[])),
            [("deleted", "b")]
        );
        assert!(!tracker.has_suspects(&[]));

        // A row that reappears (a profile move caught half-way) was never deleted.
        tracker.observe(&[live("a"), live("c")], &[]);
        assert!(tracker.observe(&[live("a")], &[]).is_empty());
        assert!(tracker.observe(&[live("a"), live("c")], &[]).is_empty());
        assert!(!tracker.has_suspects(&[]));
    }

    #[test]
    fn commit_is_exact_and_leaves_nothing_for_observe() {
        let mut tracker = LifecycleTracker::default();
        tracker.observe(&[live("a")], &[]);

        let created = tracker.commit(&[live("a")], &[live("a"), live("b")]);
        assert_eq!(events(&created), [("created", "b")]);
        assert!(tracker.observe(&[live("a"), live("b")], &[]).is_empty());

        let both = [live("a"), row("b", "p", true, false)];
        let archive = tracker.commit(&[live("a"), live("b")], &both);
        assert_eq!(events(&archive), [("archived", "b")]);
        assert!(tracker.observe(&both, &[]).is_empty());

        let deleted = tracker.commit(&both, &[live("a")]);
        assert_eq!(events(&deleted), [("deleted", "b")]);
        assert!(tracker.observe(&[live("a")], &[]).is_empty());
        assert!(!tracker.has_suspects(&[]));
    }

    #[test]
    fn commit_reports_every_flag_flip_and_a_create_then_delete() {
        let mut tracker = LifecycleTracker::default();
        tracker.observe(&[live("a")], &[]);

        let both = tracker.commit(&[live("a")], &[row("a", "p", true, true)]);
        assert_eq!(events(&both), [("archived", "a"), ("trashed", "a")]);

        // Two commits inside one debounce window both stay visible to the exact path.
        let made = tracker.commit(&[], &[live("n")]);
        let removed = tracker.commit(&[live("n")], &[]);
        assert_eq!(events(&made), [("created", "n")]);
        assert_eq!(events(&removed), [("deleted", "n")]);
    }

    #[test]
    fn commit_before_the_seed_is_ignored() {
        let mut tracker = LifecycleTracker::default();
        assert!(tracker.commit(&[], &[live("a")]).is_empty());
        // The later seed already contains it, so it is not reported as created.
        assert!(tracker.observe(&[live("a")], &[]).is_empty());
    }

    fn in_profile(changes: &[LifecycleChange]) -> Vec<(&'static str, String)> {
        changes
            .iter()
            .map(|c| (c.kind.as_str(), c.effective_profile.clone()))
            .collect()
    }

    fn in_p(kind: &'static str, profile: &str) -> (&'static str, String) {
        (kind, profile.to_string())
    }

    /// Delivery is per profile, so a plugin in the old profile sees the session leave and one
    /// in the new profile sees it arrive, however the move is written or observed.
    #[test]
    fn a_profile_move_leaves_the_old_profile_and_arrives_in_the_new_one() {
        let moved = |profile: &str| row("a", profile, false, false);

        // Written target first, then the source removed.
        let mut tracker = LifecycleTracker::default();
        tracker.observe(&[moved("p1")], &[]);
        let added = tracker.commit(&[], &[moved("p2")]);
        assert_eq!(in_profile(&added), [in_p("created", "p2")]);
        assert!(added[0].summary.is_some());
        let removed = tracker.commit(&[moved("p1")], &[]);
        assert_eq!(in_profile(&removed), [in_p("deleted", "p1")]);
        assert!(removed[0].summary.is_none());
        assert!(tracker.observe(&[moved("p2")], &[]).is_empty());

        // The source removed first, then the target written.
        let mut tracker = LifecycleTracker::default();
        tracker.observe(&[moved("p1")], &[]);
        let removed = tracker.commit(&[moved("p1")], &[]);
        assert_eq!(in_profile(&removed), [in_p("deleted", "p1")]);
        let added = tracker.commit(&[], &[moved("p2")]);
        assert_eq!(in_profile(&added), [in_p("created", "p2")]);

        // The disk diff alone: the arrival at once, the departure once the row is still gone
        // on the second look.
        let mut tracker = LifecycleTracker::default();
        tracker.observe(&[moved("p1")], &[]);
        let seen = tracker.observe(&[moved("p2")], &[]);
        assert_eq!(in_profile(&seen), [in_p("created", "p2")]);
        assert!(tracker.has_suspects(&[]));
        let seen = tracker.observe(&[moved("p2")], &[]);
        assert_eq!(in_profile(&seen), [in_p("deleted", "p1")]);
    }

    /// A move interrupted before the source is removed, or rolled back with the target kept,
    /// leaves the session in two profiles. Each profile lists its own copy, so repeating the
    /// same snapshot reports nothing and removing the source takes only that copy away.
    #[test]
    fn a_session_in_two_profiles_is_two_entries_that_do_not_flap() {
        let copy = |profile: &str| row("a", profile, false, false);
        for rows in [[copy("p1"), copy("p2")], [copy("p2"), copy("p1")]] {
            let mut tracker = LifecycleTracker::default();
            tracker.observe(&rows, &[]);
            for _ in 0..3 {
                assert!(tracker.observe(&rows, &[]).is_empty());
            }
            assert!(!tracker.has_suspects(&[]));

            // The source copy goes: only p1 loses it, and p2 never stopped having it.
            let target_only = [copy("p2")];
            assert!(tracker.observe(&target_only, &[]).is_empty());
            let gone = tracker.observe(&target_only, &[]);
            assert_eq!(in_profile(&gone), [in_p("deleted", "p1")]);
            assert!(tracker.observe(&target_only, &[]).is_empty());
        }
    }

    /// A worker that listed an unreadable-at-baseline profile saw its state before the first
    /// commit to it, which is the commit's before rows: the change is reported against them
    /// instead of being dropped and then absorbed by the next reload.
    #[test]
    fn a_commit_to_a_profile_unread_at_the_baseline_reports_against_its_before_rows() {
        let mut tracker = LifecycleTracker::default();
        let p1 = row("a", "p1", false, false);
        tracker.observe(std::slice::from_ref(&p1), &["p2".to_string()]);

        let before = row("b", "p2", false, false);
        let after = row("b", "p2", true, false);
        let changes = tracker.commit(std::slice::from_ref(&before), std::slice::from_ref(&after));
        assert_eq!(in_profile(&changes), [in_p("archived", "p2")]);

        // The profile reads again and agrees with the commit.
        assert!(tracker.observe(&[p1, after], &[]).is_empty());
    }

    /// With no profile list at the baseline, every profile starts from its first commit or its
    /// first read, whichever comes first, and none is announced as created by a later read.
    #[test]
    fn with_no_profile_list_at_the_baseline_each_profile_starts_from_its_first_commit_or_read() {
        let mut tracker = LifecycleTracker::default();
        tracker.baseline_unavailable();
        assert!(!tracker.has_suspects(&[]));

        // A commit to p2 reports against its before rows ...
        let before = row("b", "p2", false, false);
        let after = row("b", "p2", true, false);
        let changes = tracker.commit(std::slice::from_ref(&before), std::slice::from_ref(&after));
        assert_eq!(in_profile(&changes), [in_p("archived", "p2")]);

        // ... and the first complete read adopts the rest silently, p2 included.
        let a = row("a", "p1", false, false);
        assert!(tracker.observe(&[a.clone(), after.clone()], &[]).is_empty());
        let renamed = LifecycleRow {
            title: "new".to_string(),
            ..a.clone()
        };
        assert_eq!(
            in_profile(&tracker.observe(&[renamed, after], &[])),
            [in_p("renamed", "p1")]
        );
    }

    /// The profile list failing at the baseline, then a partial read, leaves the unread profile
    /// waiting for its own first read or commit rather than treating it as already known.
    #[test]
    fn a_profile_still_unread_after_a_failed_profile_list_is_adopted_silently_later() {
        let mut tracker = LifecycleTracker::default();
        tracker.baseline_unavailable();

        let p1 = row("a", "p1", false, false);
        let p2 = row("b", "p2", false, false);
        let unread = ["p2".to_string()];
        assert!(tracker
            .observe(std::slice::from_ref(&p1), &unread)
            .is_empty());

        // p2 reads at last: its sessions were always there.
        assert!(tracker.observe(&[p1.clone(), p2.clone()], &[]).is_empty());

        // And a commit to a profile still unread starts from its before rows.
        let mut tracker = LifecycleTracker::default();
        tracker.baseline_unavailable();
        tracker.observe(std::slice::from_ref(&p1), &unread);
        let archived = row("b", "p2", true, false);
        let changes = tracker.commit(std::slice::from_ref(&p2), std::slice::from_ref(&archived));
        assert_eq!(in_profile(&changes), [in_p("archived", "p2")]);
    }

    /// A profile that was unreadable at the baseline is adopted silently once it reads, so
    /// its existing sessions are not announced as created; later changes are reported.
    #[test]
    fn a_profile_unread_at_the_baseline_is_adopted_silently_when_it_recovers() {
        let mut tracker = LifecycleTracker::default();
        let p1 = row("a", "p1", false, false);
        let p2 = row("b", "p2", false, false);
        let unread = ["p2".to_string()];
        assert!(tracker
            .observe(std::slice::from_ref(&p1), &unread)
            .is_empty());

        // Still unreadable: nothing is missing, and nothing is announced for it.
        assert!(tracker
            .observe(std::slice::from_ref(&p1), &unread)
            .is_empty());
        assert!(tracker
            .observe(std::slice::from_ref(&p1), &unread)
            .is_empty());

        // It reads: its sessions were always there.
        assert!(tracker.observe(&[p1.clone(), p2.clone()], &[]).is_empty());
        let archived = row("b", "p2", true, false);
        assert_eq!(
            in_profile(&tracker.observe(&[p1, archived], &[])),
            [in_p("archived", "p2")]
        );
    }

    /// A row built from a real instance carries the shared `sessions.list` entry and the
    /// checkouts a worker needs, with branches left out of the diff key.
    #[test]
    fn a_row_of_an_instance_uses_the_shared_summary() {
        let mut inst = Instance::new("t", "/work/a");
        inst.worktree_info = Some(crate::session::WorktreeInfo {
            branch: "feature/x".to_string(),
            main_repo_path: "/work/main".to_string(),
            managed_by_aoe: true,
            created_at: chrono::Utc::now(),
            base_branch: None,
        });
        let row = LifecycleRow::of(&inst, "p");
        assert_eq!(row.checkouts, ["/work/a", "/work/main"]);
        assert_eq!(row.summary, crate::plugin::host_api::session_summary(&inst));
        assert_eq!(row.summary["worktree"]["branch"], json!("feature/x"));
    }

    /// Every commit through `Storage::update` reaches the installed observer with the rows
    /// before and after; with none installed nothing is computed.
    #[test]
    #[serial_test::serial]
    fn storage_update_reports_each_commit_to_the_observer() {
        let _home = crate::session::test_support::isolate_app_dir();
        let storage = Storage::new_unwatched("lifecycle-hook").unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        assert!(!observing());

        let guard = {
            let seen = Arc::clone(&seen);
            install_commit_observer(move |profile, before, after| {
                seen.lock()
                    .unwrap()
                    .push((profile.to_string(), before.len(), after.len()));
            })
        };
        assert!(observing());
        let inst = Instance::new("t", "/tmp/lifecycle-hook");
        storage
            .update(|rows, _| {
                rows.push(inst.clone());
                Ok(())
            })
            .unwrap();
        storage
            .update(|rows, _| {
                rows.retain(|r| r.id != inst.id);
                Ok(())
            })
            .unwrap();
        drop(guard);
        assert!(!observing(), "the guard uninstalls the observer");
        storage.update(|_, _| Ok(())).unwrap();

        assert_eq!(
            *seen.lock().unwrap(),
            [
                ("lifecycle-hook".to_string(), 0, 1),
                ("lifecycle-hook".to_string(), 1, 0)
            ]
        );
    }
}
