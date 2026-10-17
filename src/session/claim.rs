//! Durable lifecycle acquisition and commit helpers shared by every surface.

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
    storage: &super::Storage,
    all: &mut [Instance],
    id: &str,
    now: DateTime<Utc>,
) -> Result<RestoreClaimDecision, LifecycleReservationError> {
    decide_restore_claim_inner(storage, all, id, None, now)
}

/// Replace the exact Trash reservation queued by this caller with a Restore
/// reservation. A mismatched generation remains peer-owned and busy.
pub(crate) fn decide_restore_claim_after_trash(
    storage: &super::Storage,
    all: &mut [Instance],
    id: &str,
    trash_generation: u64,
    now: DateTime<Utc>,
) -> Result<RestoreClaimDecision, LifecycleReservationError> {
    decide_restore_claim_inner(storage, all, id, Some(trash_generation), now)
}

fn decide_restore_claim_inner(
    storage: &super::Storage,
    all: &mut [Instance],
    id: &str,
    owned_trash_generation: Option<u64>,
    now: DateTime<Utc>,
) -> Result<RestoreClaimDecision, LifecycleReservationError> {
    let Some(stored) = all.iter_mut().find(|instance| instance.id == id) else {
        return Ok(RestoreClaimDecision::AlreadyGone);
    };
    if stored.status == super::Status::Deleting
        && abandon_expired_pre_teardown(storage, stored, now).is_err()
    {
        return Ok(RestoreClaimDecision::Busy(LifecycleOperation::Purge));
    }
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

/// Caller holds the lifecycle flock. Never wait for owner in reverse lock order.
fn abandon_expired_pre_teardown(
    storage: &super::Storage,
    stored: &mut Instance,
    now: DateTime<Utc>,
) -> anyhow::Result<()> {
    use super::lifecycle_journal::{self, LifecyclePhase};
    use anyhow::Context as _;
    let reservation = stored
        .lifecycle_reservation
        .as_ref()
        .context("missing purge reservation")?;
    anyhow::ensure!(
        reservation.op == LifecycleOperation::Purge
            && reservation.generation == stored.lifecycle_generation
            && now - reservation.at >= Instance::LIFECYCLE_RESERVATION_TTL,
        "purge reservation remains authoritative"
    );
    let _owner = storage
        .try_acquire_purge_owner_lock(&stored.id)?
        .context("purge owner is still active")?;
    let path = lifecycle_journal::path_for(
        storage.sessions_path(),
        &stored.id,
        stored.lifecycle_generation,
    );
    let key = lifecycle_journal::filename_key(&path)?;
    let metadata = std::fs::symlink_metadata(&path)?;
    anyhow::ensure!(
        metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
        "purge journal is not a regular file"
    );
    let mut entry = lifecycle_journal::read_deletion(&path)?.context("missing purge journal")?;
    anyhow::ensure!(
        entry.is_current()
            && key.session_id() == stored.id
            && key.generation == stored.lifecycle_generation
            && entry.session_id == stored.id
            && entry.instance.id == stored.id
            && entry.generation == stored.lifecycle_generation
            && entry.instance.lifecycle_generation == stored.lifecycle_generation
            && entry.instance.lifecycle_reservation_is_owned(
                LifecycleOperation::Purge,
                stored.lifecycle_generation
            )
            && entry.source_profile == storage.profile()
            && entry.instance.source_profile == storage.profile()
            && entry.sessions_path == storage.sessions_path()
            && entry.instance.project_path == stored.project_path
            && entry.instance.trashed_at == stored.trashed_at
            && entry.instance.trash_lifecycle_id == stored.trash_lifecycle_id
            && matches!(
                entry.phase,
                LifecyclePhase::Reserved
                    | LifecyclePhase::HooksStarted
                    | LifecyclePhase::HooksComplete
            ),
        "purge journal cannot authorize restore"
    );
    if entry.instance.requires_acp_settlement() {
        crate::process::worker_registry::clear_purge_fence_if_owned(
            &stored.id,
            storage.profile(),
            stored.lifecycle_generation,
        )?;
    }
    entry = entry.with_phase(LifecyclePhase::Abandoned);
    lifecycle_journal::update(&path, &entry)?;
    stored.status = entry.status_before;
    stored.lifecycle_reservation = None;
    Ok(())
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

pub(crate) fn rollback_trash_reservation(
    all: &mut [Instance],
    id: &str,
    generation: u64,
    was_trashed: bool,
) {
    if let Some(row) = all.iter_mut().find(|instance| {
        instance.id == id
            && instance.lifecycle_reservation_is_owned(LifecycleOperation::Trash, generation)
    }) {
        if !was_trashed {
            row.untrash();
        }
        row.release_lifecycle_reservation_if_owned(LifecycleOperation::Trash, generation);
    }
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
    fn failed_trash_rollback_cannot_change_a_successor_claim() {
        for operation in [
            LifecycleOperation::Restore,
            LifecycleOperation::Trash,
            LifecycleOperation::Purge,
        ] {
            for was_trashed in [false, true] {
                let mut row = trashed("session");
                let old_generation = row
                    .try_acquire_lifecycle_reservation(
                        LifecycleOperation::Trash,
                        Instance::LIFECYCLE_RESERVATION_TTL,
                        Utc::now(),
                    )
                    .unwrap();
                row.release_lifecycle_reservation_if_owned(
                    LifecycleOperation::Trash,
                    old_generation,
                );
                row.untrash();
                let generation = row
                    .try_acquire_lifecycle_reservation(
                        operation,
                        Instance::LIFECYCLE_RESERVATION_TTL,
                        Utc::now(),
                    )
                    .unwrap();
                if operation != LifecycleOperation::Restore {
                    row.trash();
                }
                if operation == LifecycleOperation::Purge {
                    row.status = crate::session::Status::Deleting;
                }
                let before = serde_json::to_value(&row).unwrap();
                let mut rows = vec![row];
                rollback_trash_reservation(&mut rows, "session", old_generation, was_trashed);
                assert_eq!(serde_json::to_value(&rows[0]).unwrap(), before);
                assert!(rows[0].lifecycle_reservation_is_owned(operation, generation));
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn decisions_grant_or_reject_one_unified_reservation() {
        let temp = tempfile::tempdir().unwrap();
        let _home = super::super::test_support::isolate_app_dir_at(temp.path());
        let storage = super::super::Storage::new_unwatched("restore-claims").unwrap();
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
            decide_restore_claim(&storage, &mut instances, "busy", now).unwrap(),
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
                &storage,
                &mut instances,
                "handed-off",
                trash_generation + 1,
                now,
            )
            .unwrap(),
            RestoreClaimDecision::Busy(LifecycleOperation::Trash),
        );
        assert_eq!(
            decide_restore_claim_after_trash(
                &storage,
                &mut instances,
                "handed-off",
                trash_generation,
                now,
            )
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
    #[serial_test::serial]
    fn absent_rows_are_reported_without_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let _home = super::super::test_support::isolate_app_dir_at(temp.path());
        let storage = super::super::Storage::new_unwatched("restore-claims").unwrap();
        let mut instances = Vec::new();
        assert_eq!(
            decide_purge_claim(&mut instances, "gone", true, Utc::now()).unwrap(),
            PurgeClaimDecision::AlreadyGone,
        );
        assert_eq!(
            decide_restore_claim(&storage, &mut instances, "gone", Utc::now()).unwrap(),
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
    #[test]
    #[serial_test::serial]
    fn restore_requires_exact_expired_owner_free_pre_teardown_authority() {
        use super::super::lifecycle_journal::{
            self, LifecycleDeletionOptions, LifecycleJournalEntry, LifecyclePhase,
        };
        for case in [
            "reserved",
            "hooks",
            "live-owner",
            "fresh",
            "teardown",
            "missing",
            "malformed",
            "version",
            "profile",
            "path",
            "generation",
            "snapshot",
        ] {
            let root = tempfile::tempdir().unwrap();
            let _home = super::super::test_support::isolate_app_dir_at(root.path());
            let storage = super::super::Storage::new_unwatched("restore-authority").unwrap();
            let now = Utc::now();
            let mut row = trashed("restore-row");
            row.source_profile = storage.profile().into();
            row.status = super::super::Status::Idle;
            row.try_acquire_lifecycle_reservation(
                LifecycleOperation::Purge,
                Instance::LIFECYCLE_RESERVATION_TTL,
                if case == "fresh" {
                    now
                } else {
                    now - chrono::Duration::days(1)
                },
            )
            .unwrap();
            let mut entry = LifecycleJournalEntry::deletion(
                row.clone(),
                row.status,
                storage.sessions_path().into(),
                LifecycleDeletionOptions {
                    generation: row.lifecycle_generation,
                    delete_worktree: false,
                    delete_branch: false,
                    delete_sandbox: false,
                    force_delete: false,
                    detach_hooks: true,
                    keep_scratch: true,
                    purge_acp_transcript: false,
                },
            );
            entry.phase = match case {
                "hooks" => LifecyclePhase::HooksComplete,
                "teardown" => LifecyclePhase::TeardownStarted,
                _ => LifecyclePhase::Reserved,
            };
            match case {
                "version" => entry.version += 1,
                "profile" => entry.source_profile = "foreign".into(),
                "path" => entry.sessions_path = root.path().join("foreign.json"),
                "generation" => entry.generation += 1,
                "snapshot" => entry.instance.id = "foreign".into(),
                _ => {}
            }
            let path = lifecycle_journal::path_for(
                storage.sessions_path(),
                &row.id,
                row.lifecycle_generation,
            );
            if case != "missing" {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                let bytes = if case == "malformed" {
                    b"{".to_vec()
                } else {
                    serde_json::to_vec(&lifecycle_journal::LifecycleJournalRecord::Deleting(
                        Box::new(entry),
                    ))
                    .unwrap()
                };
                std::fs::write(&path, bytes).unwrap();
            }
            let before = std::fs::read(&path).ok();
            row.status = super::super::Status::Deleting;
            storage
                .update(|rows, _| {
                    rows.push(row.clone());
                    Ok(())
                })
                .unwrap();
            let _owner = if case == "live-owner" {
                storage.try_acquire_purge_owner_lock(&row.id).unwrap()
            } else {
                None
            };
            let _lifecycle = storage.acquire_instance_lifecycle_lock(&row.id).unwrap();
            let decision = storage
                .update(|rows, _| Ok(decide_restore_claim(&storage, rows, &row.id, now)?))
                .unwrap();
            let durable = storage.load().unwrap().remove(0);
            if matches!(case, "reserved" | "hooks") {
                assert_eq!(
                    decision,
                    RestoreClaimDecision::Claimed(row.lifecycle_generation + 1),
                    "{case}"
                );
                assert_eq!(durable.status, super::super::Status::Idle);
                assert_eq!(
                    lifecycle_journal::read_deletion(&path)
                        .unwrap()
                        .unwrap()
                        .phase,
                    LifecyclePhase::Abandoned
                );
            } else {
                assert_eq!(
                    decision,
                    RestoreClaimDecision::Busy(LifecycleOperation::Purge),
                    "{case}"
                );
                assert_eq!(
                    durable.lifecycle_generation, row.lifecycle_generation,
                    "{case}"
                );
                assert_eq!(durable.status, super::super::Status::Deleting, "{case}");
                assert_eq!(std::fs::read(&path).ok(), before, "{case}");
            }
        }
    }
}
