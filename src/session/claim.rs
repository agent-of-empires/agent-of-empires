//! Durable lifecycle acquisition and commit helpers shared by every surface.

use super::{Instance, LifecycleOperation, LifecycleReservationError};
use chrono::{DateTime, Utc};
use std::path::Path;

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
    source_profile: &str,
    sessions_path: &Path,
    now: DateTime<Utc>,
) -> Result<RestoreClaimDecision, LifecycleReservationError> {
    decide_restore_claim_inner(all, id, source_profile, sessions_path, None, now)
}

/// Replace the exact Trash reservation queued by this caller with a Restore
/// reservation. A mismatched generation remains peer-owned and busy.
pub(crate) fn decide_restore_claim_after_trash(
    all: &mut [Instance],
    id: &str,
    trash_generation: u64,
    source_profile: &str,
    sessions_path: &Path,
    now: DateTime<Utc>,
) -> Result<RestoreClaimDecision, LifecycleReservationError> {
    decide_restore_claim_inner(
        all,
        id,
        source_profile,
        sessions_path,
        Some(trash_generation),
        now,
    )
}

fn decide_restore_claim_inner(
    all: &mut [Instance],
    id: &str,
    source_profile: &str,
    sessions_path: &Path,
    owned_trash_generation: Option<u64>,
    now: DateTime<Utc>,
) -> Result<RestoreClaimDecision, LifecycleReservationError> {
    let Some(stored) = all.iter_mut().find(|instance| instance.id == id) else {
        return Ok(RestoreClaimDecision::AlreadyGone);
    };
    let owns_trash = owned_trash_generation.is_some_and(|generation| {
        stored.lifecycle_reservation_is_owned(LifecycleOperation::Trash, generation)
    });
    if let Some(reservation) = stored.lifecycle_reservation.as_ref().filter(|reservation| {
        reservation.generation == stored.lifecycle_generation
            && (now - reservation.at) < Instance::LIFECYCLE_RESERVATION_TTL
    }) {
        if !(owns_trash && reservation.op == LifecycleOperation::Trash) {
            return Ok(RestoreClaimDecision::Busy(reservation.op));
        }
    }
    if (stored.status == super::Status::Deleting
        || stored
            .lifecycle_reservation
            .as_ref()
            .is_some_and(|reservation| reservation.op == LifecycleOperation::Purge))
        && !has_recoverable_pre_teardown_purge(stored, source_profile, sessions_path, now)
    {
        return Ok(RestoreClaimDecision::Busy(LifecycleOperation::Purge));
    }
    if owns_trash {
        let generation = owned_trash_generation.expect("owned trash lease has a generation");
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

fn has_recoverable_pre_teardown_purge(
    stored: &Instance,
    source_profile: &str,
    sessions_path: &Path,
    now: DateTime<Utc>,
) -> bool {
    let Some(reservation) = stored.lifecycle_reservation.as_ref() else {
        return false;
    };
    if stored.status != super::Status::Deleting
        || reservation.op != LifecycleOperation::Purge
        || reservation.generation != stored.lifecycle_generation
        || now - reservation.at < Instance::LIFECYCLE_RESERVATION_TTL
    {
        return false;
    }

    let path = crate::session::lifecycle_journal::path_for(
        sessions_path,
        &stored.id,
        stored.lifecycle_generation,
    );
    if !std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_file()) {
        return false;
    }
    let Ok(Some(entry)) = crate::session::lifecycle_journal::read_deletion(&path) else {
        return false;
    };
    let mut row_before_purge = stored.clone();
    row_before_purge.status = entry.status_before;
    let row_matches = match (
        row_before_purge.lifecycle_reservation.as_mut(),
        entry.instance.lifecycle_reservation.as_ref(),
    ) {
        (Some(row_reservation), Some(journal_reservation))
            if row_reservation.op == journal_reservation.op
                && row_reservation.generation == journal_reservation.generation =>
        {
            row_reservation.at = journal_reservation.at;
            matches!(
                (
                    serde_json::to_value(&row_before_purge),
                    serde_json::to_value(&entry.instance)
                ),
                (Ok(row), Ok(journal)) if row == journal
            )
        }
        _ => false,
    };

    entry.is_current()
        && entry.session_id == stored.id
        && entry.instance.id == stored.id
        && entry.source_profile == source_profile
        && entry.instance.source_profile == source_profile
        && entry.sessions_path == sessions_path
        && entry.generation == stored.lifecycle_generation
        && row_matches
        && reservation.op == LifecycleOperation::Purge
        && reservation.generation == entry.generation
        && entry.instance.lifecycle_generation == entry.generation
        && entry.status_before == entry.instance.status
        && entry.instance.is_trashed()
        && stored.is_trashed()
        && entry.instance.trashed_at == stored.trashed_at
        && matches!(
            entry.phase,
            crate::session::lifecycle_journal::LifecyclePhase::Reserved
                | crate::session::lifecycle_journal::LifecyclePhase::HooksStarted
                | crate::session::lifecycle_journal::LifecyclePhase::HooksComplete
        )
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
    use crate::session::lifecycle_journal::{
        LifecycleDeletionOptions, LifecycleJournalEntry, LifecyclePhase,
    };
    use crate::session::Status;
    use chrono::Utc;
    use serial_test::serial;

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

    fn interrupted_purge(
        id: &str,
        profile: &str,
        sessions_path: &Path,
        phase: LifecyclePhase,
    ) -> (Instance, LifecycleJournalEntry) {
        let now = Utc::now();
        let mut snapshot = trashed(id);
        snapshot.source_profile = profile.to_string();
        let generation = snapshot
            .try_acquire_lifecycle_reservation(
                LifecycleOperation::Purge,
                Instance::LIFECYCLE_RESERVATION_TTL,
                now - Instance::LIFECYCLE_RESERVATION_TTL - chrono::Duration::seconds(1),
            )
            .unwrap();
        let entry = LifecycleJournalEntry::deletion(
            snapshot.clone(),
            snapshot.status,
            sessions_path.to_path_buf(),
            LifecycleDeletionOptions {
                generation,
                delete_worktree: false,
                delete_branch: false,
                delete_sandbox: false,
                force_delete: false,
                detach_hooks: false,
                keep_scratch: false,
                purge_acp_transcript: false,
            },
        )
        .with_phase(phase);
        snapshot.status = Status::Deleting;
        (snapshot, entry)
    }

    #[test]
    fn restore_can_supersede_only_a_matching_pre_teardown_purge() {
        let temp = tempfile::tempdir().unwrap();
        let sessions_path = temp.path().join("profiles/profile/sessions.json");
        std::fs::create_dir_all(sessions_path.parent().unwrap()).unwrap();

        for phase in [
            LifecyclePhase::Reserved,
            LifecyclePhase::HooksStarted,
            LifecyclePhase::HooksComplete,
        ] {
            let id = format!("recoverable-{phase:?}");
            let (mut row, entry) = interrupted_purge(&id, "profile", &sessions_path, phase);
            let path = crate::session::lifecycle_journal::record(&entry).unwrap();
            let previous_generation = row.lifecycle_generation;
            assert_eq!(
                decide_restore_claim(
                    std::slice::from_mut(&mut row),
                    &id,
                    "profile",
                    &sessions_path,
                    Utc::now(),
                )
                .unwrap(),
                RestoreClaimDecision::Claimed(previous_generation + 1),
                "phase {phase:?} remains before teardown"
            );
            assert_eq!(
                row.lifecycle_reservation.as_ref().unwrap().op,
                LifecycleOperation::Restore
            );
            crate::session::lifecycle_journal::consume(&path).unwrap();
        }

        for phase in [
            LifecyclePhase::TeardownStarted,
            LifecyclePhase::TeardownComplete,
            LifecyclePhase::RowRemoved,
            LifecyclePhase::Kept,
            LifecyclePhase::Abandoned,
        ] {
            let id = format!("irreversible-{phase:?}");
            let (mut row, entry) = interrupted_purge(&id, "profile", &sessions_path, phase);
            crate::session::lifecycle_journal::record(&entry).unwrap();
            let generation = row.lifecycle_generation;
            let reservation = row.lifecycle_reservation.clone();
            assert_eq!(
                decide_restore_claim(
                    std::slice::from_mut(&mut row),
                    &id,
                    "profile",
                    &sessions_path,
                    Utc::now(),
                )
                .unwrap(),
                RestoreClaimDecision::Busy(LifecycleOperation::Purge),
                "phase {phase:?} is irreversible"
            );
            assert_eq!(row.lifecycle_generation, generation);
            assert_eq!(row.lifecycle_reservation, reservation);
        }
    }

    #[test]
    fn restore_can_supersede_pre_teardown_purge_after_reservation_heartbeat() {
        let temp = tempfile::tempdir().unwrap();
        let sessions_path = temp.path().join("profiles/profile/sessions.json");
        std::fs::create_dir_all(sessions_path.parent().unwrap()).unwrap();
        let id = "recoverable-after-heartbeat";
        let (mut row, entry) =
            interrupted_purge(id, "profile", &sessions_path, LifecyclePhase::HooksStarted);
        let journal_at = entry.instance.lifecycle_reservation.as_ref().unwrap().at;
        let now = Utc::now();
        let heartbeat_at = now - Instance::LIFECYCLE_RESERVATION_TTL - chrono::Duration::seconds(1);
        assert!(heartbeat_at > journal_at);
        row.lifecycle_reservation.as_mut().unwrap().at = heartbeat_at;
        let generation = row.lifecycle_generation;
        crate::session::lifecycle_journal::record(&entry).unwrap();

        assert_eq!(
            decide_restore_claim(
                std::slice::from_mut(&mut row),
                id,
                "profile",
                &sessions_path,
                now,
            )
            .unwrap(),
            RestoreClaimDecision::Claimed(generation + 1),
            "a heartbeat may update the reservation timestamp without changing purge authority"
        );
    }

    #[test]
    fn restore_fails_closed_without_exact_purge_journal_authority() {
        for mismatch in [
            "missing",
            "corrupt",
            "profile",
            "path",
            "generation",
            "row",
            "reservation",
            "journal-reservation-operation",
            "journal-reservation-generation",
        ] {
            let temp = tempfile::tempdir().unwrap();
            let sessions_path = temp.path().join("profiles/profile/sessions.json");
            std::fs::create_dir_all(sessions_path.parent().unwrap()).unwrap();
            let id = format!("restore-{mismatch}");
            let (mut row, mut entry) = interrupted_purge(
                &id,
                "profile",
                &sessions_path,
                LifecyclePhase::HooksComplete,
            );
            match mismatch {
                "missing" => {}
                "corrupt" => {
                    let path = crate::session::lifecycle_journal::path_for(
                        &sessions_path,
                        &id,
                        row.lifecycle_generation,
                    );
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    std::fs::write(path, b"not-json").unwrap();
                }
                "profile" => {
                    let path = crate::session::lifecycle_journal::record(&entry).unwrap();
                    entry.source_profile = "other-profile".to_string();
                    entry.instance.source_profile = "other-profile".to_string();
                    crate::session::lifecycle_journal::update(&path, &entry).unwrap();
                }
                "path" => {
                    let path = crate::session::lifecycle_journal::record(&entry).unwrap();
                    entry.sessions_path = temp.path().join("other/sessions.json");
                    crate::session::lifecycle_journal::update(&path, &entry).unwrap();
                }
                "generation" => {
                    let path = crate::session::lifecycle_journal::record(&entry).unwrap();
                    entry.generation += 1;
                    crate::session::lifecycle_journal::update(&path, &entry).unwrap();
                }
                "row" => {
                    crate::session::lifecycle_journal::record(&entry).unwrap();
                    row.title.push_str(" changed");
                }
                "reservation" => {
                    crate::session::lifecycle_journal::record(&entry).unwrap();
                    row.lifecycle_reservation = None;
                }
                "journal-reservation-operation" => {
                    let path = crate::session::lifecycle_journal::record(&entry).unwrap();
                    entry.instance.lifecycle_reservation.as_mut().unwrap().op =
                        LifecycleOperation::Trash;
                    crate::session::lifecycle_journal::update(&path, &entry).unwrap();
                }
                "journal-reservation-generation" => {
                    let path = crate::session::lifecycle_journal::record(&entry).unwrap();
                    entry
                        .instance
                        .lifecycle_reservation
                        .as_mut()
                        .unwrap()
                        .generation += 1;
                    crate::session::lifecycle_journal::update(&path, &entry).unwrap();
                }
                _ => unreachable!(),
            }
            let generation = row.lifecycle_generation;
            assert_eq!(
                decide_restore_claim(
                    std::slice::from_mut(&mut row),
                    &id,
                    "profile",
                    &sessions_path,
                    Utc::now(),
                )
                .unwrap(),
                RestoreClaimDecision::Busy(LifecycleOperation::Purge),
                "{mismatch} journal must not authorize restore"
            );
            assert_eq!(row.lifecycle_generation, generation);
        }
    }

    #[test]
    #[serial]
    fn restore_waits_for_the_purge_owner_lock_before_claiming() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let storage = crate::session::Storage::new_unwatched("restore-owner-lock").unwrap();
        let sessions_path = storage.sessions_path().to_path_buf();
        let (row, entry) = interrupted_purge(
            "restore-owner-lock-session",
            storage.profile(),
            &sessions_path,
            LifecyclePhase::HooksComplete,
        );
        let generation = row.lifecycle_generation;
        crate::session::lifecycle_journal::record(&entry).unwrap();
        storage
            .update(|instances, _groups| {
                instances.push(row);
                Ok(())
            })
            .unwrap();

        let lifecycle_lock = storage
            .try_acquire_instance_lifecycle_lock("restore-owner-lock-session")
            .unwrap()
            .unwrap();
        assert!(storage
            .try_acquire_restore_locks("restore-owner-lock-session")
            .unwrap()
            .is_none());
        assert!(storage
            .try_acquire_purge_owner_lock("restore-owner-lock-session")
            .unwrap()
            .is_some());
        drop(lifecycle_lock);

        let owner_lock = storage
            .try_acquire_purge_owner_lock("restore-owner-lock-session")
            .unwrap()
            .unwrap();
        assert!(storage
            .try_acquire_restore_locks("restore-owner-lock-session")
            .unwrap()
            .is_none());
        drop(owner_lock);

        let Some((_owner_lock, _lifecycle_lock)) = storage
            .try_acquire_restore_locks("restore-owner-lock-session")
            .unwrap()
        else {
            panic!("Restore could not acquire released purge and lifecycle locks");
        };
        let decision = storage
            .update(|instances, _groups| {
                decide_restore_claim(
                    instances,
                    "restore-owner-lock-session",
                    storage.profile(),
                    &sessions_path,
                    Utc::now(),
                )
                .map_err(anyhow::Error::new)
            })
            .unwrap();
        assert_eq!(decision, RestoreClaimDecision::Claimed(generation + 1));
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
            decide_restore_claim(
                &mut instances,
                "busy",
                "test",
                Path::new("sessions.json"),
                now
            )
            .unwrap(),
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
                "test",
                Path::new("sessions.json"),
                now,
            )
            .unwrap(),
            RestoreClaimDecision::Busy(LifecycleOperation::Trash),
        );
        assert_eq!(
            decide_restore_claim_after_trash(
                &mut instances,
                "handed-off",
                trash_generation,
                "test",
                Path::new("sessions.json"),
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
    fn absent_rows_are_reported_without_mutation() {
        let mut instances = Vec::new();
        assert_eq!(
            decide_purge_claim(&mut instances, "gone", true, Utc::now()).unwrap(),
            PurgeClaimDecision::AlreadyGone,
        );
        assert_eq!(
            decide_restore_claim(
                &mut instances,
                "gone",
                "test",
                Path::new("sessions.json"),
                Utc::now(),
            )
            .unwrap(),
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
