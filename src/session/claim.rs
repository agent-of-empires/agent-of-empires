//! Cross-process purge/restore claim decisions, shared by the CLI, the serve
//! daemon, and the TUI. Every destructive/irreversible phase (purge teardown,
//! restore worktree move) runs its slow work on an UNLOCKED snapshot; these
//! helpers make the claim check-and-set (and the final commit) atomic under the
//! storage flock, the only serialization point visible across processes. Living
//! in the neutral `session` layer keeps the three surfaces from reaching into
//! `cli` for shared logic. See #2534, #2541.

use super::{ClaimOp, Instance};
use chrono::{DateTime, Utc};

/// Decides whether a permanent purge must KEEP a row it had targeted, because
/// the row was restored after the purge snapshot was taken. A purge runs its
/// destructive teardown on an unlocked snapshot and only removes the row under
/// the lock; if it targeted a trashed session and a concurrent restore
/// untrashed it in between, the restore wins and the row is kept. A purge of a
/// row that was not trashed at snapshot time (a direct `rm --purge` of a live
/// session) has no restore to lose to, so it is never kept on this basis.
/// See #2534.
pub(crate) fn purge_restored_row_must_be_kept(targeted_trashed: bool, still_trashed: bool) -> bool {
    targeted_trashed && !still_trashed
}

/// Outcome of the purge claim decision, run under the storage flock before the
/// unlocked teardown at every purge site (CLI, server, TUI). Shared so all
/// three surfaces close the same race windows identically. See #2534, #2541.
#[derive(Debug, PartialEq)]
pub(crate) enum PurgeClaimDecision {
    /// Claim won (free, expired, or already ours); teardown may proceed. The
    /// row's Purge claim is set as a side effect.
    Claimed,
    /// The targeted-trashed row was un-trashed between the snapshot and this
    /// claim, so it must not be torn down (a genuine `--purge` of a live
    /// session passes `was_trashed=false` and never lands here).
    Restored,
    /// A peer holds a fresh Restore claim on the row.
    RestoreInProgress,
    /// The row is gone from disk (a peer already removed it).
    AlreadyGone,
}

/// Decide whether a purge may claim and tear down `id`, run inside a
/// `storage.update` closure (under the flock). Closes the cross-process race by
/// refusing when a fresh Restore claim holds the row and when a peer restore
/// un-trashed the row between snapshot and claim. On `Claimed` the
/// Purge claim is set. See #2534, #2541.
pub(crate) fn decide_purge_claim(
    all: &mut [Instance],
    id: &str,
    was_trashed: bool,
    now: DateTime<Utc>,
) -> PurgeClaimDecision {
    match all.iter_mut().find(|i| i.id == id) {
        None => PurgeClaimDecision::AlreadyGone,
        Some(stored) if purge_restored_row_must_be_kept(was_trashed, stored.is_trashed()) => {
            PurgeClaimDecision::Restored
        }
        Some(stored) => match stored.try_claim(ClaimOp::Purge, Instance::OP_CLAIM_TTL, now) {
            Ok(()) => PurgeClaimDecision::Claimed,
            Err(ClaimOp::Restore) => PurgeClaimDecision::RestoreInProgress,
            // `try_claim(Purge)` only ever refuses with the OTHER op.
            Err(ClaimOp::Purge) => unreachable!("try_claim(Purge) cannot be refused by Purge"),
        },
    }
}

/// Outcome of the final locked row removal in a purge. See #2534.
#[derive(Debug, PartialEq)]
pub(crate) enum PurgeCommit {
    /// The row was dropped from storage.
    Removed,
    /// A concurrent restore won; the (now untrashed) row was kept.
    KeptRestored,
    /// A peer already removed the row before this purge reached the lock.
    AlreadyGone,
}

/// The final locked row removal for a purge, run inside a `storage.update`
/// closure at every purge site. Applies the #2534 restore-race recheck: a row a
/// peer restored mid-purge is kept and its Purge claim released
/// (ownership-guarded so a peer's fresh Restore claim is never cleared);
/// otherwise the row is dropped. See #2534, #2541.
pub(crate) fn finalize_purge_removal(
    all: &mut Vec<Instance>,
    id: &str,
    was_trashed: bool,
) -> PurgeCommit {
    match all.iter().position(|i| i.id == id) {
        None => PurgeCommit::AlreadyGone,
        Some(idx) if purge_restored_row_must_be_kept(was_trashed, all[idx].is_trashed()) => {
            all[idx].clear_op_claim_if_owned(ClaimOp::Purge);
            PurgeCommit::KeptRestored
        }
        Some(idx) => {
            all.remove(idx);
            PurgeCommit::Removed
        }
    }
}

/// Outcome of the restore claim decision, run under the flock before the
/// unlocked worktree move. Symmetric with [`decide_purge_claim`]. See #2541.
#[derive(Debug, PartialEq)]
pub(crate) enum RestoreClaimDecision {
    /// Claim won (free, expired, or already ours); the worktree move may
    /// proceed. The Restore claim is set as a side effect.
    Claimed,
    /// A peer holds a fresh Purge claim, so the restore is refused.
    PurgeInProgress,
    /// The trashed row is gone from disk.
    AlreadyGone,
}

/// Decide whether a restore may claim and relocate `id`, run inside a
/// `storage.update` closure (under the flock). Refuses when a fresh Purge claim
/// holds the row. On `Claimed` the Restore claim is set. See #2541.
pub(crate) fn decide_restore_claim(
    all: &mut [Instance],
    id: &str,
    now: DateTime<Utc>,
) -> RestoreClaimDecision {
    match all.iter_mut().find(|i| i.id == id) {
        None => RestoreClaimDecision::AlreadyGone,
        Some(stored) => match stored.try_claim(ClaimOp::Restore, Instance::OP_CLAIM_TTL, now) {
            Ok(()) => RestoreClaimDecision::Claimed,
            Err(ClaimOp::Purge) => RestoreClaimDecision::PurgeInProgress,
            Err(ClaimOp::Restore) => {
                unreachable!("try_claim(Restore) cannot be refused by Restore")
            }
        },
    }
}

/// Outcome of the final locked restore commit. See #2541.
#[derive(Debug, PartialEq)]
pub(crate) enum RestoreCommit {
    /// Untrashed + Restore claim released; the restore landed.
    Committed,
    /// A stale-override purge stole the claim mid-move, so the restore bailed
    /// and let the purge win (degrades to #2534, never worse than the status
    /// quo).
    PurgeStoleClaim,
    /// The row is gone from disk.
    AlreadyGone,
}

/// The final locked restore commit, run inside a `storage.update` closure at
/// every restore site. Untrashes the row and releases the Restore claim
/// (ownership-guarded), unless a stale-override purge stole the claim while the
/// worktree moved, in which case it bails. See #2541.
pub(crate) fn finalize_restore_commit(
    all: &mut [Instance],
    id: &str,
    project_path: &str,
    pre_trash_project_path: &Option<String>,
) -> RestoreCommit {
    let Some(stored) = all.iter_mut().find(|i| i.id == id) else {
        return RestoreCommit::AlreadyGone;
    };
    if matches!(&stored.op_claim, Some(c) if c.op == ClaimOp::Purge) {
        return RestoreCommit::PurgeStoleClaim;
    }
    stored.project_path = project_path.to_string();
    stored.pre_trash_project_path = pre_trash_project_path.clone();
    stored.untrash();
    stored.clear_op_claim_if_owned(ClaimOp::Restore);
    RestoreCommit::Committed
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn trashed(id: &str) -> Instance {
        let mut inst = Instance::new("s", "/tmp/x");
        inst.id = id.to_string();
        inst.trash();
        inst
    }

    #[test]
    fn decide_purge_claim_bails_when_row_untrashed_since_snapshot() {
        let mut row = trashed("a");
        row.untrash(); // restored between snapshot and claim
        let mut all = vec![row];
        assert_eq!(
            decide_purge_claim(&mut all, "a", true, Utc::now()),
            PurgeClaimDecision::Restored
        );
        assert_eq!(all[0].op_claim, None, "no claim is set on a restored row");
    }

    #[test]
    fn decide_purge_claim_refused_by_fresh_restore() {
        let mut row = trashed("a");
        row.try_claim(ClaimOp::Restore, Instance::OP_CLAIM_TTL, Utc::now())
            .unwrap();
        let mut all = vec![row];
        assert_eq!(
            decide_purge_claim(&mut all, "a", true, Utc::now()),
            PurgeClaimDecision::RestoreInProgress
        );
    }

    #[test]
    fn decide_restore_claim_refused_by_fresh_purge() {
        let mut row = trashed("a");
        row.try_claim(ClaimOp::Purge, Instance::OP_CLAIM_TTL, Utc::now())
            .unwrap();
        let mut all = vec![row];
        assert_eq!(
            decide_restore_claim(&mut all, "a", Utc::now()),
            RestoreClaimDecision::PurgeInProgress
        );
    }

    #[test]
    fn decide_restore_claim_grants_and_sets_claim() {
        let mut all = vec![trashed("a")];
        assert_eq!(
            decide_restore_claim(&mut all, "a", Utc::now()),
            RestoreClaimDecision::Claimed
        );
        assert_eq!(
            all[0].op_claim.as_ref().map(|c| c.op),
            Some(ClaimOp::Restore)
        );
    }

    // Normal restore commit: untrash + release the Restore claim.
    #[test]
    fn finalize_restore_commit_untrashes_and_clears() {
        let mut row = trashed("a");
        row.try_claim(ClaimOp::Restore, Instance::OP_CLAIM_TTL, Utc::now())
            .unwrap();
        let mut all = vec![row];
        assert_eq!(
            finalize_restore_commit(&mut all, "a", "/new/path", &Some("/pre".to_string())),
            RestoreCommit::Committed
        );
        assert!(!all[0].is_trashed());
        assert_eq!(all[0].project_path, "/new/path");
        assert_eq!(all[0].op_claim, None);
    }

    // Stale-override: a purge stole the claim while the worktree moved. The
    // commit must bail (not untrash) and leave the purge's claim intact, so the
    // purge wins (degrades to #2534). This is the commit-time bail the three
    // restore surfaces share. See #2541.
    #[test]
    fn finalize_restore_commit_bails_when_purge_stole_the_claim() {
        let mut row = trashed("a");
        row.try_claim(ClaimOp::Purge, Instance::OP_CLAIM_TTL, Utc::now())
            .unwrap();
        let mut all = vec![row];
        assert_eq!(
            finalize_restore_commit(&mut all, "a", "/new/path", &None),
            RestoreCommit::PurgeStoleClaim
        );
        assert!(all[0].is_trashed(), "the row must stay trashed");
        assert_eq!(
            all[0].op_claim.as_ref().map(|c| c.op),
            Some(ClaimOp::Purge),
            "the peer's Purge claim must survive"
        );
    }

    // The final removal keeps a row a peer restored mid-purge and releases the
    // owned Purge claim (anti-wedge regression). See #2534, #2541.
    #[test]
    fn finalize_purge_removal_clears_claim_on_kept_restored_row() {
        let mut row = trashed("a");
        row.try_claim(ClaimOp::Purge, Instance::OP_CLAIM_TTL, Utc::now())
            .unwrap();
        row.untrash(); // a peer restored it mid-purge
        let mut all = vec![row];
        assert_eq!(
            finalize_purge_removal(&mut all, "a", true),
            PurgeCommit::KeptRestored
        );
        assert_eq!(all.len(), 1, "the restored row is kept");
        assert_eq!(all[0].op_claim, None, "our purge claim is released");
    }

    // A still-trashed row is removed by the final commit (the peer restore, if
    // any, has not landed on disk yet, so the row is dropped and that restore
    // then bails on AlreadyGone).
    #[test]
    fn finalize_purge_removal_removes_still_trashed_row() {
        let mut row = trashed("a");
        row.try_claim(ClaimOp::Purge, Instance::OP_CLAIM_TTL, Utc::now())
            .unwrap();
        let mut all = vec![row];
        assert_eq!(
            finalize_purge_removal(&mut all, "a", true),
            PurgeCommit::Removed
        );
        assert!(all.is_empty());
    }

    // Stale-override: a purge overran the TTL, a peer restore un-trashed the row
    // and set a fresh Restore claim. The final commit keeps the row and must NOT
    // clear the peer's Restore claim (the ownership guard). See #2541.
    #[test]
    fn finalize_purge_removal_preserves_peer_restore_claim_on_kept_row() {
        let mut row = Instance::new("s", "/tmp/x"); // peer restored it (untrashed)
        row.id = "a".to_string();
        row.try_claim(ClaimOp::Restore, Instance::OP_CLAIM_TTL, Utc::now())
            .unwrap();
        let mut all = vec![row];
        assert_eq!(
            finalize_purge_removal(&mut all, "a", true),
            PurgeCommit::KeptRestored
        );
        assert_eq!(
            all[0].op_claim.as_ref().map(|c| c.op),
            Some(ClaimOp::Restore),
            "the ownership guard must not clear the peer's fresh Restore claim"
        );
    }

    // Sequenced substitute for the real cross-process race, which is not
    // unit-testable (the true serialization is the storage flock across
    // processes). Routes through the composed flock-closure helpers a purge site
    // actually runs: a purge claims via `decide_purge_claim`, a concurrent
    // restore's `try_claim` is refused, and `finalize_purge_removal` removes the
    // still-trashed row. See #2541.
    #[test]
    fn sequenced_purge_blocks_restore_then_removes() {
        let ttl = Instance::OP_CLAIM_TTL;
        let now = Utc::now();
        let mut all = vec![trashed("a")];

        // Purge wins the claim first (the composed decision, not a bare try_claim).
        assert_eq!(
            decide_purge_claim(&mut all, "a", true, now),
            PurgeClaimDecision::Claimed
        );

        // A concurrent restore, reaching the flock afterwards, is refused.
        assert_eq!(
            all[0].try_claim(ClaimOp::Restore, ttl, now),
            Err(ClaimOp::Purge),
            "restore must bail while a fresh purge claim holds"
        );

        // The restore having bailed, the row is still trashed, so the purge's
        // #2534 final-commit recheck removes it.
        assert_eq!(
            finalize_purge_removal(&mut all, "a", true),
            PurgeCommit::Removed
        );
        assert!(all.is_empty());
    }

    // Each decide/finalize helper reports AlreadyGone when a peer already removed
    // the target row before this operation reached the flock. See #2534, #2541.
    #[test]
    fn decide_purge_claim_on_absent_row_is_already_gone() {
        let mut all: Vec<Instance> = vec![];
        assert_eq!(
            decide_purge_claim(&mut all, "gone", true, Utc::now()),
            PurgeClaimDecision::AlreadyGone
        );
    }

    #[test]
    fn finalize_purge_removal_on_absent_row_is_already_gone() {
        let mut all: Vec<Instance> = vec![];
        assert_eq!(
            finalize_purge_removal(&mut all, "gone", true),
            PurgeCommit::AlreadyGone
        );
    }

    #[test]
    fn decide_restore_claim_on_absent_row_is_already_gone() {
        let mut all: Vec<Instance> = vec![];
        assert_eq!(
            decide_restore_claim(&mut all, "gone", Utc::now()),
            RestoreClaimDecision::AlreadyGone
        );
    }

    #[test]
    fn finalize_restore_commit_on_absent_row_is_already_gone() {
        let mut all: Vec<Instance> = vec![];
        assert_eq!(
            finalize_restore_commit(&mut all, "gone", "/new/path", &None),
            RestoreCommit::AlreadyGone
        );
    }
}
