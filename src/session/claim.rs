//! Cross-process purge/restore/trash claim decisions, shared by the CLI, the
//! serve daemon, and the TUI. Every destructive/irreversible phase (purge
//! teardown, restore worktree move, trash container stop + relocation) runs
//! its slow work on an UNLOCKED snapshot; these helpers make the claim
//! check-and-set (and the final commit) atomic under the storage flock, the
//! only serialization point visible across processes. Living in the neutral
//! `session` layer keeps the three surfaces from reaching into `cli` for
//! shared logic. See #2534, #2541.
//!
//! Trash claim lifecycle: each trash site (TUI `trash_session_by_id` via the
//! same-flock `apply_user_action_with` hook, the server trash handler's
//! persist closure, CLI `remove`) sets the claim with `Instance::try_claim`
//! in the SAME `storage.update` that writes the trash marker, so a peer can
//! never read a trashed row without its in-flight claim. A refused claim
//! (fresh peer purge/restore) still tears down, gated by the pre-move
//! re-check and the locked relocation commit. Release happens on every
//! terminal teardown path: [`commit_trash_relocation`] for relocation
//! outcomes, [`release_trash_claim`] for the no-relocation ones.
//!
//! TTL asymmetries, all deliberate: the teardown's gates
//! (`Instance::is_seized_by_fresh_peer_claim`) only yield to a FRESH peer
//! claim, while seizure of a Trash claim ignores its age entirely, so both a
//! stale seizer and a teardown whose worker died without a release (the
//! TUI's `Disconnected` drain, the server's join-error arm) converge through
//! the `Instance::OP_CLAIM_TTL` self-heal rather than an explicit handoff.
//! The load-time `reconcile_trashed_location` pass does not consult
//! `op_claim` at all; racing an in-flight peer teardown is benign because
//! both sides attempt the same idempotent move and the loser fails on
//! "target exists".

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
    /// Claim won (free or expired); teardown may proceed. The row's Purge
    /// claim is set as a side effect.
    Claimed,
    /// The targeted-trashed row was un-trashed between the snapshot and this
    /// claim, so it must not be torn down (a genuine `--purge` of a live
    /// session passes `was_trashed=false` and never lands here).
    Restored,
    /// A peer holds a fresh Restore claim on the row.
    RestoreInProgress,
    /// A peer holds a fresh Purge claim. Without a separate identity this must
    /// be treated as foreign rather than refreshing its TTL.
    Busy,
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
        Some(stored)
            if stored.op_claim.as_ref().is_some_and(|claim| {
                claim.op == ClaimOp::Purge && (now - claim.at) < Instance::OP_CLAIM_TTL
            }) =>
        {
            PurgeClaimDecision::Busy
        }
        Some(stored) => match stored.try_claim(ClaimOp::Purge, Instance::OP_CLAIM_TTL, now) {
            Ok(()) => PurgeClaimDecision::Claimed,
            Err(ClaimOp::Restore) => PurgeClaimDecision::RestoreInProgress,
            Err(ClaimOp::Purge) => PurgeClaimDecision::Busy,
            Err(ClaimOp::Trash) => unreachable!("try_claim seizes a fresh Trash claim"),
        },
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
            Err(ClaimOp::Trash) => unreachable!("try_claim seizes a fresh Trash claim"),
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

/// Release the Trash claim on a teardown's no-relocation terminal paths
/// (`Skipped` / `Failed`), run inside a `storage.update` closure.
/// Ownership-guarded, so a claim a purge or restore seized in the meantime is
/// never cleared. The relocation paths release through
/// [`commit_trash_relocation`] instead. The full claim lifecycle and its TTL
/// asymmetries are documented on the module header.
pub(crate) fn release_trash_claim(all: &mut [Instance], id: &str) {
    if let Some(row) = all.iter_mut().find(|i| i.id == id) {
        row.clear_op_claim_if_owned(ClaimOp::Trash);
    }
}

/// Outcome of the final locked commit of a background trash relocation. See
/// [`commit_trash_relocation`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum RelocationCommit {
    /// The row still reads trashed and unclaimed; the relocated paths were
    /// written onto it.
    Persisted,
    /// The row was restored, or a fresh purge/restore claim holds it, so the
    /// relocation must not be recorded. The caller undoes the disk move.
    Superseded,
    /// The row is gone from disk (a peer purged it); nothing to record. The
    /// holding-area entry falls to the purge teardown and the scoped
    /// `delete_branch` reaping.
    AlreadyGone,
}

/// The final locked commit of a background trash teardown's worktree
/// relocation, run inside a `storage.update` closure at every surface that
/// persists one (TUI poller drain, server trash handler, CLI remove).
///
/// The teardown's pre-move re-check (`teardown_may_relocate`) narrows the
/// restore race to the span between that check and this commit; this gate
/// closes the pointer half of it by re-taking the decision on the durable row
/// under the flock. A restore that landed in between wins: the caller gets
/// [`RelocationCommit::Superseded`] and moves the worktree back instead of
/// repointing a live row into the holding area. A row held by a fresh peer
/// claim is also superseded: a mid-flight restore loaded the row before this
/// commit (its `finalize_restore_commit` would clobber these paths), and a
/// mid-flight purge tears down against its own snapshot; in both cases
/// undoing the move converges with the peer's commit, and if the peer bails
/// the un-relocated trashed row is re-relocated by the next reconcile pass.
/// See #2534, #2541.
pub(crate) fn commit_trash_relocation(
    all: &mut [Instance],
    id: &str,
    relocation: &crate::session::trash::TrashRelocation,
    now: DateTime<Utc>,
) -> RelocationCommit {
    match all.iter_mut().find(|i| i.id == id) {
        None => RelocationCommit::AlreadyGone,
        Some(row) => {
            // The teardown's own Trash claim does not block its commit; a
            // fresh Purge or Restore claim (which seized it) does. Either
            // way this commit is a terminal teardown path, so any Trash
            // claim still owned is released (ownership-guarded).
            if !row.is_trashed() || row.is_seized_by_fresh_peer_claim(now) {
                row.clear_op_claim_if_owned(ClaimOp::Trash);
                return RelocationCommit::Superseded;
            }
            row.project_path = relocation.new_project_path.clone();
            row.pre_trash_project_path = relocation.pre_trash_project_path.clone();
            row.clear_op_claim_if_owned(ClaimOp::Trash);
            RelocationCommit::Persisted
        }
    }
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

    fn reloc() -> crate::session::trash::TrashRelocation {
        crate::session::trash::TrashRelocation {
            new_project_path: "/wt/.aoe-trash/a".to_string(),
            pre_trash_project_path: Some("/wt/feature".to_string()),
        }
    }

    #[test]
    fn commit_relocation_persists_on_unclaimed_trashed_row() {
        let mut all = vec![trashed("a")];
        assert_eq!(
            commit_trash_relocation(&mut all, "a", &reloc(), Utc::now()),
            RelocationCommit::Persisted
        );
        assert_eq!(all[0].project_path, "/wt/.aoe-trash/a");
        assert_eq!(
            all[0].pre_trash_project_path.as_deref(),
            Some("/wt/feature")
        );
    }

    #[test]
    fn commit_relocation_superseded_by_restored_row() {
        let mut row = trashed("a");
        row.untrash();
        let mut all = vec![row];
        assert_eq!(
            commit_trash_relocation(&mut all, "a", &reloc(), Utc::now()),
            RelocationCommit::Superseded
        );
        assert_eq!(all[0].project_path, "/tmp/x", "restored row is untouched");
        assert!(all[0].pre_trash_project_path.is_none());
    }

    #[test]
    fn commit_relocation_superseded_by_fresh_claim() {
        for op in [ClaimOp::Restore, ClaimOp::Purge] {
            let mut row = trashed("a");
            row.try_claim(op, Instance::OP_CLAIM_TTL, Utc::now())
                .unwrap();
            let mut all = vec![row];
            assert_eq!(
                commit_trash_relocation(&mut all, "a", &reloc(), Utc::now()),
                RelocationCommit::Superseded,
                "a fresh {op:?} claim must supersede the relocation"
            );
            assert_eq!(all[0].project_path, "/tmp/x", "claimed row is untouched");
        }
    }

    #[test]
    fn commit_relocation_ignores_expired_claim() {
        let mut row = trashed("a");
        let stale = Utc::now() - Instance::OP_CLAIM_TTL - chrono::Duration::minutes(1);
        row.try_claim(ClaimOp::Restore, Instance::OP_CLAIM_TTL, stale)
            .unwrap();
        let mut all = vec![row];
        assert_eq!(
            commit_trash_relocation(&mut all, "a", &reloc(), Utc::now()),
            RelocationCommit::Persisted,
            "an expired claim is treated as absent, mirroring try_claim"
        );
    }

    #[test]
    fn commit_relocation_already_gone_when_row_missing() {
        let mut all: Vec<Instance> = Vec::new();
        assert_eq!(
            commit_trash_relocation(&mut all, "a", &reloc(), Utc::now()),
            RelocationCommit::AlreadyGone
        );
    }

    #[test]
    fn commit_relocation_persists_over_own_trash_claim_and_releases_it() {
        let mut row = trashed("a");
        row.try_claim(ClaimOp::Trash, Instance::OP_CLAIM_TTL, Utc::now())
            .unwrap();
        let mut all = vec![row];
        assert_eq!(
            commit_trash_relocation(&mut all, "a", &reloc(), Utc::now()),
            RelocationCommit::Persisted,
            "the teardown's own Trash claim must not block its commit"
        );
        assert_eq!(all[0].project_path, "/wt/.aoe-trash/a");
        assert_eq!(all[0].op_claim, None, "commit is terminal: claim released");
    }

    #[test]
    fn commit_relocation_superseded_releases_leftover_trash_claim() {
        // A restored row that somehow still carries the teardown's Trash claim
        // (restore raced between seize and commit) is cleaned up here; a claim
        // a peer owns is never touched (ownership-guarded clear).
        let mut row = trashed("a");
        row.try_claim(ClaimOp::Trash, Instance::OP_CLAIM_TTL, Utc::now())
            .unwrap();
        row.untrash();
        let mut all = vec![row];
        assert_eq!(
            commit_trash_relocation(&mut all, "a", &reloc(), Utc::now()),
            RelocationCommit::Superseded
        );
        assert_eq!(all[0].op_claim, None, "leftover own Trash claim released");
    }

    #[test]
    fn trash_claim_never_overwrites_a_fresh_peer_claim() {
        // The acquisition sites call `try_claim(Trash)` directly (in the same
        // flock write as the trash marker); a fresh peer claim must refuse it
        // and stay intact.
        let mut row = trashed("b");
        row.try_claim(ClaimOp::Purge, Instance::OP_CLAIM_TTL, Utc::now())
            .unwrap();
        assert_eq!(
            row.try_claim(ClaimOp::Trash, Instance::OP_CLAIM_TTL, Utc::now()),
            Err(ClaimOp::Purge)
        );
        assert_eq!(
            row.op_claim.as_ref().map(|c| c.op),
            Some(ClaimOp::Purge),
            "a peer-held claim is never overwritten by trash"
        );
    }

    #[test]
    fn release_trash_claim_is_ownership_guarded() {
        let mut row = trashed("a");
        row.try_claim(ClaimOp::Trash, Instance::OP_CLAIM_TTL, Utc::now())
            .unwrap();
        let mut all = vec![row];
        release_trash_claim(&mut all, "a");
        assert_eq!(all[0].op_claim, None);

        let mut row = trashed("b");
        row.try_claim(ClaimOp::Restore, Instance::OP_CLAIM_TTL, Utc::now())
            .unwrap();
        let mut all = vec![row];
        release_trash_claim(&mut all, "b");
        assert_eq!(
            all[0].op_claim.as_ref().map(|c| c.op),
            Some(ClaimOp::Restore),
            "a seized (Restore-owned) claim must survive the release"
        );
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
    fn fresh_peer_purge_is_busy_without_refreshing_identity() {
        let mut row = trashed("a");
        let claimed_at = Utc::now();
        row.try_claim(ClaimOp::Purge, Instance::OP_CLAIM_TTL, claimed_at)
            .unwrap();
        let mut all = vec![row];
        assert_eq!(
            decide_purge_claim(
                &mut all,
                "a",
                true,
                claimed_at + chrono::Duration::seconds(1)
            ),
            PurgeClaimDecision::Busy
        );
        assert_eq!(
            all[0].op_claim.as_ref().map(|claim| claim.at),
            Some(claimed_at)
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

    // Claim decisions and restore finalization report AlreadyGone when a peer
    // removed the target row before this operation reached the flock.
    #[test]
    fn decide_purge_claim_on_absent_row_is_already_gone() {
        let mut all: Vec<Instance> = vec![];
        assert_eq!(
            decide_purge_claim(&mut all, "gone", true, Utc::now()),
            PurgeClaimDecision::AlreadyGone
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
