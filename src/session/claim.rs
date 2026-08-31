//! Durable lifecycle acquisition and commit helpers shared by every surface.
//!
//! All acquisitions run inside `Storage::update`. Slow preparation may run
//! unlocked, but the caller must then hold the per-instance lifecycle flock
//! while an exact generation check, irreversible side effects, and the durable
//! commit execute. Operation kind is never used as ownership identity.
//!
//! Vocabulary: `reservation` is the durable ownership primitive on `Instance`;
//! `claim`/`decide_*` are the decision helpers this module layers on top.

use super::{Instance, LifecycleOperation, LifecycleReservationError};
use chrono::{DateTime, Utc};

pub(crate) fn purge_restored_row_must_be_kept(targeted_trashed: bool, still_trashed: bool) -> bool {
    targeted_trashed && !still_trashed
}

#[derive(Debug, PartialEq)]
pub(crate) enum PurgeClaimDecision {
    Claimed(u64),
    Restored,
    Busy(LifecycleOperation),
    AlreadyGone,
}

pub(crate) fn decide_purge_claim(
    all: &mut [Instance],
    id: &str,
    was_trashed: bool,
    now: DateTime<Utc>,
) -> Result<PurgeClaimDecision, LifecycleReservationError> {
    let Some(stored) = all.iter_mut().find(|instance| instance.id == id) else {
        return Ok(PurgeClaimDecision::AlreadyGone);
    };
    if purge_restored_row_must_be_kept(was_trashed, stored.is_trashed()) {
        return Ok(PurgeClaimDecision::Restored);
    }
    match stored.try_acquire_lifecycle_reservation(
        LifecycleOperation::Purge,
        Instance::LIFECYCLE_RESERVATION_TTL,
        now,
    ) {
        Ok(generation) => Ok(PurgeClaimDecision::Claimed(generation)),
        Err(LifecycleReservationError::Busy(holder)) => Ok(PurgeClaimDecision::Busy(holder)),
        Err(error) => Err(error),
    }
}

#[derive(Debug, PartialEq)]
pub(crate) enum RestoreClaimDecision {
    Claimed(u64),
    Busy(LifecycleOperation),
    AlreadyGone,
}

pub(crate) fn decide_restore_claim(
    all: &mut [Instance],
    id: &str,
    now: DateTime<Utc>,
) -> Result<RestoreClaimDecision, LifecycleReservationError> {
    decide_restore_claim_inner(all, id, None, now)
}

/// Replace the exact Trash reservation queued by this caller with a Restore
/// reservation. A mismatched generation remains peer-owned and busy.
pub(crate) fn decide_restore_claim_after_trash(
    all: &mut [Instance],
    id: &str,
    trash_generation: u64,
    now: DateTime<Utc>,
) -> Result<RestoreClaimDecision, LifecycleReservationError> {
    decide_restore_claim_inner(all, id, Some(trash_generation), now)
}

fn decide_restore_claim_inner(
    all: &mut [Instance],
    id: &str,
    owned_trash_generation: Option<u64>,
    now: DateTime<Utc>,
) -> Result<RestoreClaimDecision, LifecycleReservationError> {
    let Some(stored) = all.iter_mut().find(|instance| instance.id == id) else {
        return Ok(RestoreClaimDecision::AlreadyGone);
    };
    if let Some(generation) = owned_trash_generation {
        stored.release_lifecycle_reservation_if_owned(LifecycleOperation::Trash, generation);
    }
    match stored.try_acquire_lifecycle_reservation(
        LifecycleOperation::Restore,
        Instance::LIFECYCLE_RESERVATION_TTL,
        now,
    ) {
        Ok(generation) => Ok(RestoreClaimDecision::Claimed(generation)),
        Err(LifecycleReservationError::Busy(holder)) => Ok(RestoreClaimDecision::Busy(holder)),
        Err(error) => Err(error),
    }
}

#[derive(Debug, PartialEq)]
pub(crate) enum RestoreCommit {
    Committed,
    Superseded,
    AlreadyGone,
}

pub(crate) fn finalize_restore_commit(
    all: &mut [Instance],
    id: &str,
    generation: u64,
    project_path: &str,
    pre_trash_project_path: &Option<String>,
) -> RestoreCommit {
    let Some(stored) = all.iter_mut().find(|instance| instance.id == id) else {
        return RestoreCommit::AlreadyGone;
    };
    if !stored.lifecycle_reservation_is_owned(LifecycleOperation::Restore, generation) {
        return RestoreCommit::Superseded;
    }
    stored.project_path = project_path.to_string();
    stored.pre_trash_project_path = pre_trash_project_path.clone();
    stored.untrash();
    stored.release_lifecycle_reservation_if_owned(LifecycleOperation::Restore, generation);
    RestoreCommit::Committed
}

pub(crate) fn release_trash_reservation(all: &mut [Instance], id: &str, generation: u64) {
    if let Some(row) = all.iter_mut().find(|instance| instance.id == id) {
        row.release_lifecycle_reservation_if_owned(LifecycleOperation::Trash, generation);
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum RelocationCommit {
    Persisted,
    Superseded,
    AlreadyGone,
}

pub(crate) fn commit_trash_relocation(
    all: &mut [Instance],
    id: &str,
    generation: u64,
    relocation: &crate::session::trash::TrashRelocation,
) -> RelocationCommit {
    let Some(row) = all.iter_mut().find(|instance| instance.id == id) else {
        return RelocationCommit::AlreadyGone;
    };
    if !row.is_trashed()
        || !row.lifecycle_reservation_is_owned(LifecycleOperation::Trash, generation)
    {
        return RelocationCommit::Superseded;
    }
    row.project_path = relocation.new_project_path.clone();
    row.pre_trash_project_path = relocation.pre_trash_project_path.clone();
    row.release_lifecycle_reservation_if_owned(LifecycleOperation::Trash, generation);
    RelocationCommit::Persisted
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn trashed(id: &str) -> Instance {
        let mut instance = Instance::new("session", "/tmp/worktree");
        instance.id = id.to_string();
        instance.trash();
        instance
    }

    fn relocation() -> crate::session::trash::TrashRelocation {
        crate::session::trash::TrashRelocation {
            new_project_path: "/tmp/.aoe-trash/session".to_string(),
            pre_trash_project_path: Some("/tmp/worktree".to_string()),
        }
    }

    #[test]
    fn decisions_grant_or_reject_one_unified_reservation() {
        let now = Utc::now();
        let mut restored = trashed("restored");
        restored.untrash();
        let mut busy = trashed("busy");
        let busy_generation = busy
            .try_acquire_lifecycle_reservation(
                LifecycleOperation::Restore,
                Instance::LIFECYCLE_RESERVATION_TTL,
                now,
            )
            .unwrap();
        let mut instances = vec![trashed("free"), restored, busy];

        let purge_generation = match decide_purge_claim(&mut instances, "free", true, now).unwrap()
        {
            PurgeClaimDecision::Claimed(generation) => generation,
            outcome => panic!("unexpected purge decision: {outcome:?}"),
        };
        assert_eq!(purge_generation, 1);
        assert_eq!(
            decide_purge_claim(&mut instances, "restored", true, now).unwrap(),
            PurgeClaimDecision::Restored,
        );
        assert_eq!(
            decide_purge_claim(&mut instances, "busy", true, now).unwrap(),
            PurgeClaimDecision::Busy(LifecycleOperation::Restore),
        );
        assert_eq!(
            decide_restore_claim(&mut instances, "busy", now).unwrap(),
            RestoreClaimDecision::Busy(LifecycleOperation::Restore),
        );
        assert_eq!(busy_generation, 1);
        let mut handed_off = trashed("handed-off");
        let trash_generation = handed_off
            .try_acquire_lifecycle_reservation(
                LifecycleOperation::Trash,
                Instance::LIFECYCLE_RESERVATION_TTL,
                now,
            )
            .unwrap();
        instances.push(handed_off);
        assert_eq!(
            decide_restore_claim_after_trash(
                &mut instances,
                "handed-off",
                trash_generation + 1,
                now,
            )
            .unwrap(),
            RestoreClaimDecision::Busy(LifecycleOperation::Trash),
        );
        assert_eq!(
            decide_restore_claim_after_trash(&mut instances, "handed-off", trash_generation, now,)
                .unwrap(),
            RestoreClaimDecision::Claimed(trash_generation + 1),
        );
    }

    #[test]
    fn relocation_commit_requires_exact_trash_generation() {
        let now = Utc::now();
        let mut row = trashed("session");
        let generation = row
            .try_acquire_lifecycle_reservation(
                LifecycleOperation::Trash,
                Instance::LIFECYCLE_RESERVATION_TTL,
                now,
            )
            .unwrap();
        let mut instances = vec![row];

        assert_eq!(
            commit_trash_relocation(&mut instances, "session", generation + 1, &relocation(),),
            RelocationCommit::Superseded,
        );
        assert_eq!(instances[0].project_path, "/tmp/worktree");
        assert_eq!(
            commit_trash_relocation(&mut instances, "session", generation, &relocation()),
            RelocationCommit::Persisted,
        );
        assert_eq!(instances[0].project_path, "/tmp/.aoe-trash/session");
        assert_eq!(instances[0].lifecycle_reservation, None);
    }

    #[test]
    fn restore_commit_requires_exact_generation() {
        let now = Utc::now();
        let mut row = trashed("session");
        let generation = row
            .try_acquire_lifecycle_reservation(
                LifecycleOperation::Restore,
                Instance::LIFECYCLE_RESERVATION_TTL,
                now,
            )
            .unwrap();
        let mut instances = vec![row];

        assert_eq!(
            finalize_restore_commit(
                &mut instances,
                "session",
                generation + 1,
                "/tmp/restored",
                &None,
            ),
            RestoreCommit::Superseded,
        );
        assert!(instances[0].is_trashed());
        assert_eq!(
            finalize_restore_commit(
                &mut instances,
                "session",
                generation,
                "/tmp/restored",
                &None,
            ),
            RestoreCommit::Committed,
        );
        assert!(!instances[0].is_trashed());
        assert_eq!(instances[0].project_path, "/tmp/restored");
        assert_eq!(instances[0].lifecycle_reservation, None);
    }

    #[test]
    fn absent_rows_are_reported_without_mutation() {
        let mut instances = Vec::new();
        assert_eq!(
            decide_purge_claim(&mut instances, "gone", true, Utc::now()).unwrap(),
            PurgeClaimDecision::AlreadyGone,
        );
        assert_eq!(
            decide_restore_claim(&mut instances, "gone", Utc::now()).unwrap(),
            RestoreClaimDecision::AlreadyGone,
        );
        assert_eq!(
            finalize_restore_commit(&mut instances, "gone", 1, "/tmp/restored", &None),
            RestoreCommit::AlreadyGone,
        );
        assert_eq!(
            commit_trash_relocation(&mut instances, "gone", 1, &relocation()),
            RelocationCommit::AlreadyGone,
        );
    }
}
