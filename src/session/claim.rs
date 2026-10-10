//! Durable lifecycle acquisition and commit helpers shared by every surface.

use super::{Instance, LifecycleOperation, LifecycleReservationError};
use chrono::{DateTime, Utc};

/// Declare before fenced guards so abandonment releases after their locks drop.
pub(crate) struct ReservationCleanup<'a> {
    storage: &'a super::Storage,
    id: &'a str,
    created_at: DateTime<Utc>,
    operation: LifecycleOperation,
    generation: u64,
    active: bool,
}

impl<'a> ReservationCleanup<'a> {
    pub(crate) fn new(
        storage: &'a super::Storage,
        instance: &'a Instance,
        operation: LifecycleOperation,
        generation: u64,
    ) -> Self {
        Self {
            storage,
            id: &instance.id,
            created_at: instance.created_at,
            operation,
            generation,
            active: true,
        }
    }

    pub(crate) fn disarm(&mut self) {
        self.active = false;
    }

    fn owns(&self, instance: &Instance) -> bool {
        instance.id == self.id
            && instance.created_at == self.created_at
            && instance.lifecycle_reservation_is_owned(self.operation, self.generation)
    }

    fn release(&self) -> anyhow::Result<()> {
        let ownership = super::storage::acquire_ownership_read()?;
        let _lifecycle = self
            .storage
            .acquire_instance_lifecycle_lock_with_ownership(&ownership, self.id)?;
        if !self
            .storage
            .load_strict_for_worktree_ownership()?
            .iter()
            .any(|instance| self.owns(instance))
        {
            return Ok(());
        }
        self.storage
            .update_with_ownership(&ownership, |instances, _groups| {
                let instance = instances
                    .iter_mut()
                    .find(|instance| self.owns(instance))
                    .ok_or_else(|| anyhow::anyhow!("abandoned reservation was superseded"))?;
                instance.release_lifecycle_reservation_if_owned(self.operation, self.generation);
                Ok(())
            })
    }
}

impl Drop for ReservationCleanup<'_> {
    fn drop(&mut self) {
        if self.active {
            if let Err(error) = self.release() {
                tracing::warn!(target: "session.lifecycle", session = %self.id, operation = ?self.operation, "Could not release abandoned reservation: {error:#}");
            }
        }
    }
}

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

    #[test]
    #[serial_test::serial]
    fn ownership_review_abandonment_only_releases_the_original_reservation() {
        for operation in [LifecycleOperation::Trash, LifecycleOperation::Launch] {
            for change in [
                "checkout",
                "generation",
                "incarnation",
                "missing",
                "corrupt",
                "profile",
            ] {
                let _home = crate::session::test_support::isolate_app_dir();
                let storage = super::super::Storage::new_unwatched("cleanup").unwrap();
                let mut original = Instance::new("candidate", "/checkout/original");
                original.source_profile = storage.profile().into();
                let generation = original
                    .try_acquire_lifecycle_reservation(
                        operation,
                        Instance::LIFECYCLE_RESERVATION_TTL,
                        Utc::now(),
                    )
                    .unwrap();
                storage
                    .update(|rows, _| {
                        rows.push(original.clone());
                        Ok(())
                    })
                    .unwrap();
                let mut current = original.clone();
                match change {
                    "checkout" => current.project_path = "/checkout/changed".into(),
                    "generation" => {
                        current.release_lifecycle_reservation_if_owned(operation, generation);
                        current
                            .try_acquire_lifecycle_reservation(
                                operation,
                                Instance::LIFECYCLE_RESERVATION_TTL,
                                Utc::now(),
                            )
                            .unwrap();
                    }
                    "incarnation" => current.created_at += chrono::Duration::seconds(1),
                    _ => {}
                }
                storage
                    .update(|rows, _| {
                        *rows = if change == "missing" {
                            Vec::new()
                        } else {
                            vec![current]
                        };
                        Ok(())
                    })
                    .unwrap();
                let replacement;
                let observed_storage = if change == "profile" {
                    let profile_dir = crate::session::get_profile_dir(storage.profile()).unwrap();
                    std::fs::rename(&profile_dir, profile_dir.with_extension("retired")).unwrap();
                    replacement = super::super::Storage::new_unwatched(storage.profile()).unwrap();
                    replacement
                        .update(|rows, _| {
                            rows.push(original.clone());
                            Ok(())
                        })
                        .unwrap();
                    &replacement
                } else {
                    &storage
                };
                if change == "corrupt" {
                    std::fs::write(storage.sessions_path(), b"[{broken").unwrap();
                }
                let before = std::fs::read(observed_storage.sessions_path()).unwrap();
                drop(ReservationCleanup::new(
                    &storage, &original, operation, generation,
                ));
                if change == "checkout" {
                    let mut rows = storage.load_strict_for_worktree_ownership().unwrap();
                    assert_eq!(rows[0].project_path, "/checkout/changed");
                    assert!(rows[0].lifecycle_reservation.is_none());
                    assert!(matches!(
                        decide_restore_claim(&mut rows, &original.id, Utc::now()),
                        Ok(RestoreClaimDecision::Claimed(_))
                    ));
                } else {
                    assert_eq!(
                        std::fs::read(observed_storage.sessions_path()).unwrap(),
                        before,
                        "{operation:?}/{change}"
                    );
                }
            }
        }
    }

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
