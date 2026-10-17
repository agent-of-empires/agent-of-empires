//! Durable lifecycle acquisition and commit helpers shared by every surface.

use super::{Instance, LifecycleOperation, LifecycleReservationError};
use anyhow::Context;
use chrono::{DateTime, Utc};
use std::sync::{Arc, Mutex, MutexGuard};

use super::move_journal::{
    EffectObservation, EffectPhase, JournalTransaction, MoveAuthority, MoveJournalEntry,
    ProfileAuthority, ResourceIntent, MOVE_JOURNAL_VERSION,
};

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
            && instance
                .lifecycle_reservation
                .as_ref()
                .is_some_and(|reservation| reservation.resource_intent.is_none())
    }

    pub(crate) fn release(&self) -> anyhow::Result<()> {
        let ownership = self.storage.acquire_ownership_read_bound()?;
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

/// A parent-only journal lease. Git and callbacks never inherit its flock.
pub(crate) struct ResourceReservation {
    storage: super::Storage,
    journal: JournalTransaction,
    entry: Mutex<MoveJournalEntry>,
    id: String,
}

pub(crate) struct ReservedResource {
    pub(crate) transaction: Arc<ResourceReservation>,
    pub(crate) instance: Instance,
}

impl ResourceReservation {
    /// The caller owns Root, identity and lifecycle admission guards.
    pub(crate) fn admit_with_ownership(
        storage: &super::Storage,
        root: &super::storage::OwnershipGuard,
        intent: ResourceIntent,
        mut reserved: Instance,
    ) -> anyhow::Result<ReservedResource> {
        root.require_exclusive()?;
        anyhow::ensure!(
            intent.owner.profile.matches(storage)?,
            "resource intent has another source profile"
        );
        anyhow::ensure!(
            reserved.id == intent.owner.id
                && reserved.created_at == intent.owner.created_at
                && reserved.lifecycle_generation == intent.owner.generation
                && reserved
                    .lifecycle_reservation_is_owned(intent.operation, intent.owner.generation),
            "resource intent has another lifecycle owner"
        );
        anyhow::ensure!(
            reserved
                .lifecycle_reservation
                .as_ref()
                .is_some_and(|lease| lease.resource_intent.is_none()),
            "a lifecycle reservation is already associated with an intent"
        );
        if intent.operation == LifecycleOperation::Create {
            anyhow::ensure!(
                reserved.status == super::Status::Creating
                    && intent
                        .after
                        .as_ref()
                        .is_some_and(|goal| super::move_journal::same_namespace(&reserved, goal)),
                "creation admission requires its planned Creating row"
            );
        }
        let rows = storage.load_strict_for_worktree_ownership()?;
        match rows.iter().find(|row| row.id == intent.owner.id) {
            Some(current) => {
                let before = intent
                    .before
                    .as_ref()
                    .context("creation collided with an existing identity")?;
                anyhow::ensure!(
                    !before
                        .lifecycle_reservation
                        .as_ref()
                        .is_some_and(|lease| lease.resource_intent.is_some()),
                    "resource owner already belongs to a durable intent"
                );
                anyhow::ensure!(
                    super::move_journal::row_resources_match(current, before)
                        && current.lifecycle_reservation == before.lifecycle_reservation,
                    "resource owner changed before admission"
                );
                anyhow::ensure!(
                    intent.owner.generation > before.lifecycle_generation
                        || (intent.owner.generation == before.lifecycle_generation
                            && before.lifecycle_reservation_is_owned(
                                intent.operation,
                                intent.owner.generation
                            )),
                    "resource admission has no new or already-reserved lifecycle epoch"
                );
            }
            None => anyhow::ensure!(
                intent.operation == LifecycleOperation::Create && intent.before.is_none(),
                "resource owner disappeared before admission"
            ),
        }
        drop(rows);
        let entry = MoveJournalEntry {
            version: MOVE_JOURNAL_VERSION,
            ids: vec![intent.owner.id.clone()],
            source_profile: intent.owner.profile.profile.clone(),
            target_profile: intent.target.profile.clone(),
            source_sessions_path: intent.owner.profile.sessions_path.clone(),
            target_sessions_path: intent.target.sessions_path.clone(),
            group_move_source_path: intent
                .before
                .as_ref()
                .map(|row| row.group_path.clone())
                .unwrap_or_default(),
            group_move_target_path: intent
                .after
                .as_ref()
                .map(|row| row.group_path.clone())
                .unwrap_or_default(),
            group_move_subtree: false,
            created_at_epoch_ms: u64::try_from(Utc::now().timestamp_millis())?,
            authority: MoveAuthority::Resource {
                intent: Box::new(intent),
            },
        };
        let journal = super::move_journal::record_live(root, &entry)?;
        let MoveAuthority::Resource { intent } = &entry.authority else {
            unreachable!()
        };
        if let Err(error) =
            super::deletion::ensure_resource_intent_available(root, &journal, intent)
        {
            // Nothing has been published or executed under this new intent.
            journal
                .consume()
                .context("removing an unadmitted resource intent")?;
            return Err(error);
        }
        reserved
            .lifecycle_reservation
            .as_mut()
            .expect("validated reservation")
            .resource_intent = Some(journal.id().to_owned());
        reserved.source_profile = storage.profile().to_owned();
        let before = intent.before.as_ref();
        let operation = intent.operation;
        let publication = storage.update_with_intent(root, &journal, |rows, _| {
            if let Some(current) = rows.iter_mut().find(|row| row.id == reserved.id) {
                let before =
                    before.context("resource creation collided with an existing identity")?;
                anyhow::ensure!(
                    super::move_journal::row_resources_match(current, before)
                        && current.lifecycle_reservation == before.lifecycle_reservation,
                    "resource owner changed before admission"
                );
                if operation == LifecycleOperation::Create {
                    let source_profile = std::mem::take(&mut current.source_profile);
                    reserved.merge_user_action_diff(before, current);
                    reserved.source_profile = source_profile;
                    *current = reserved;
                } else {
                    current.lifecycle_generation = reserved.lifecycle_generation;
                    current.lifecycle_reservation = reserved.lifecycle_reservation;
                }
                Ok(current.clone())
            } else {
                anyhow::ensure!(
                    operation == LifecycleOperation::Create && before.is_none(),
                    "resource owner disappeared before admission"
                );
                rows.push(reserved);
                Ok(rows.last().expect("creating row was inserted").clone())
            }
        });
        let instance = match publication {
            Ok(instance) => instance,
            Err(error) => {
                if let Ok(rows) = storage.load_strict_for_worktree_ownership() {
                    let admitted = rows.iter().any(|row| {
                        row.lifecycle_reservation.as_ref().is_some_and(|lease| {
                            lease.resource_intent.as_deref() == Some(journal.id())
                        })
                    });
                    if !admitted {
                        if let Err(cleanup) = journal.consume() {
                            return Err(error.context(format!(
                                "unadmitted intent could not be removed: {cleanup:#}"
                            )));
                        }
                    }
                }
                return Err(error);
            }
        };
        Ok(ReservedResource {
            instance,
            transaction: Arc::new(Self {
                storage: storage.clone(),
                id: entry.ids[0].clone(),
                journal,
                entry: Mutex::new(entry),
            }),
        })
    }

    pub(crate) fn journal(&self) -> &JournalTransaction {
        &self.journal
    }

    fn lock_entry(&self) -> anyhow::Result<MutexGuard<'_, MoveJournalEntry>> {
        self.entry
            .lock()
            .map_err(|_| anyhow::anyhow!("resource intent state is poisoned"))
    }

    pub(crate) fn update_creation_metadata_with_ownership(
        &self,
        root: &super::storage::OwnershipGuard,
        edited: &Instance,
    ) -> anyhow::Result<Instance> {
        root.require_exclusive()?;
        let mut entry = self.lock_entry()?;
        let MoveAuthority::Resource { intent } = &mut entry.authority else {
            anyhow::bail!("creation has no resource intent")
        };
        anyhow::ensure!(
            intent.operation == LifecycleOperation::Create && !intent.rollback,
            "resource intent is not an active creation"
        );
        anyhow::ensure!(
            intent.effects.iter().all(|effect| !matches!(
                effect.phase,
                EffectPhase::Pending | EffectPhase::Conflict
            )),
            "creation still has an unfinished external effect"
        );
        let profile = ProfileAuthority::capture(&self.storage)?;
        let before = intent
            .after
            .as_ref()
            .context("creation has no planned row")?;
        anyhow::ensure!(
            intent.owner.matches(&profile, edited)
                && super::move_journal::same_namespace(before, edited),
            "creation initialization changed its reserved namespace"
        );
        let ready_status = before.status;
        let latest = self
            .storage
            .update_with_intent(root, &self.journal, |rows, _| {
                let current = rows
                    .iter_mut()
                    .find(|row| row.id == self.id)
                    .context("Creating owner disappeared")?;
                anyhow::ensure!(
                    intent.row_is_reserved(&profile, current, self.journal.id()),
                    "Creating owner was superseded"
                );
                current.merge_creation_diff(before, edited)?;
                Ok(current.clone())
            })?;
        let mut after = latest.clone();
        after.status = ready_status;
        after.lifecycle_reservation = None;
        after.source_profile.clear();
        intent.after = Some(after);
        self.journal.rewrite(&entry)?;
        Ok(latest)
    }

    pub(crate) fn record_rename_preflight_with_ownership(
        &self,
        root: &super::storage::OwnershipGuard,
        observed: &Instance,
    ) -> anyhow::Result<()> {
        root.require_exclusive()?;
        let mut entry = self.lock_entry()?;
        let MoveAuthority::Resource { intent } = &mut entry.authority else {
            anyhow::bail!("rename has no resource intent")
        };
        let profile = ProfileAuthority::capture(&self.storage)?;
        anyhow::ensure!(
            intent.operation == LifecycleOperation::Rename
                && !intent.rollback
                && intent.owner.matches(&profile, observed)
                && intent.row_is_reserved(&profile, observed, self.journal.id())
                && !observed.status.blocks_worktree_edit(),
            "rename preflight has no owned quiescent observation"
        );
        intent.verify_after(true)?;
        super::deletion::ensure_resource_intent_available(root, &self.journal, intent)?;
        let before = intent
            .before
            .as_ref()
            .context("rename has no original row")?;
        self.storage
            .update_with_intent(root, &self.journal, |rows, _| {
                let current = rows
                    .iter_mut()
                    .find(|row| row.id == self.id)
                    .context("rename owner disappeared during preflight")?;
                anyhow::ensure!(
                    intent.row_is_reserved(&profile, current, self.journal.id()),
                    "rename owner was superseded during preflight"
                );
                anyhow::ensure!(
                    !current.status.blocks_worktree_edit()
                        || (current.status == before.status
                            && current.idle_entered_at == before.idle_entered_at
                            && current.last_accessed_at == before.last_accessed_at),
                    "session became active during rename preflight"
                );
                current.merge_passive_status_patch(
                    &self.id,
                    &super::instance::PassiveStatusPatch {
                        status: observed.status,
                        lifecycle_generation: intent.owner.generation,
                        idle_entered_at: observed.idle_entered_at,
                        last_accessed_at: None,
                    },
                );
                Ok(())
            })?;
        intent.quiesced = true;
        self.journal.rewrite(&entry)
    }

    fn owner_is_current(&self, intent: &ResourceIntent) -> anyhow::Result<bool> {
        let profile = ProfileAuthority::capture(&self.storage)?;
        if profile.sessions_path != intent.owner.profile.sessions_path {
            return Ok(false);
        }
        let rows = self.storage.load_strict_for_worktree_ownership()?;
        match rows.iter().find(|row| row.id == self.id) {
            Some(row) => Ok(intent.row_is_reserved(&profile, row, self.journal.id())),
            None => Ok(
                (intent.operation == LifecycleOperation::Purge && intent.after.is_none())
                    || (intent.rollback && intent.operation == LifecycleOperation::Create),
            ),
        }
    }

    /// Preparation is explicit so no helper upgrades a caller's Root lease.
    pub(crate) fn begin_effect_with_ownership(
        &self,
        root: &super::storage::OwnershipGuard,
        index: usize,
    ) -> anyhow::Result<()> {
        root.require_exclusive()?;
        let mut entry = self.lock_entry()?;
        let MoveAuthority::Resource { intent } = &mut entry.authority else {
            anyhow::bail!("resource transaction has no resource intent")
        };
        anyhow::ensure!(
            !matches!(intent.phase, EffectPhase::Committed | EffectPhase::Conflict),
            "resource intent is already terminal or conflicted"
        );
        anyhow::ensure!(
            self.owner_is_current(intent)?,
            "resource lifecycle owner was superseded"
        );
        super::deletion::ensure_resource_intent_available(root, &self.journal, intent)?;
        if intent
            .effects
            .get(index)
            .context("resource effect index is invalid")?
            .compensation
        {
            intent.verify_after(true)?;
        }
        let effect = intent
            .effects
            .get_mut(index)
            .context("resource effect index is invalid")?;
        anyhow::ensure!(
            effect.phase == EffectPhase::Prepared && effect.before.is_none(),
            "resource effect has already been started"
        );
        let before = effect.effect.observe()?;
        effect.before = Some(before);
        effect.phase = EffectPhase::Pending;
        intent.phase = EffectPhase::Pending;
        self.journal.rewrite(&entry)
    }

    /// Record the actual return, including an error after a partial Git effect.
    pub(crate) fn finish_effect_with_ownership(
        &self,
        root: &super::storage::OwnershipGuard,
        index: usize,
        error: Option<String>,
        observed: Option<EffectObservation>,
    ) -> anyhow::Result<()> {
        root.require_exclusive()?;
        let mut entry = self.lock_entry()?;
        let MoveAuthority::Resource { intent } = &mut entry.authority else {
            anyhow::bail!("resource transaction has no resource intent")
        };
        let effect = intent
            .effects
            .get_mut(index)
            .context("resource effect index is invalid")?;
        anyhow::ensure!(
            effect.phase == EffectPhase::Pending,
            "resource effect was not pending"
        );
        let observation = match observed {
            Some(observed) => Ok(observed),
            None => effect.effect.observe(),
        };
        effect.error = error;
        let observation_error = match observation {
            Ok(after) => {
                effect.after = Some(after);
                effect.phase = EffectPhase::Applied;
                None
            }
            Err(error) => {
                effect.phase = EffectPhase::Conflict;
                intent.phase = EffectPhase::Conflict;
                intent.conflict = Some(format!(
                    "effect returned but its state could not be observed: {error:#}"
                ));
                Some(error)
            }
        };
        if intent.phase != EffectPhase::Conflict {
            intent.phase = if intent
                .effects
                .iter()
                .any(|effect| effect.phase == EffectPhase::Pending)
            {
                EffectPhase::Pending
            } else {
                EffectPhase::Applied
            };
        }
        self.journal.rewrite(&entry)?;
        if let Some(error) = observation_error {
            return Err(error);
        }
        Ok(())
    }

    /// Outer errors leave the observed intent for recovery; inner errors are the effect's result.
    pub(crate) fn run_effect<T>(
        &self,
        index: usize,
        perform: impl FnOnce() -> anyhow::Result<(T, Option<EffectObservation>)>,
    ) -> anyhow::Result<anyhow::Result<T>> {
        {
            let root = self.storage.acquire_ownership_lock_bound()?;
            let _identity = super::storage::acquire_session_identity_lock_with_ownership(&root)?;
            let _lifecycle = self
                .storage
                .acquire_instance_lifecycle_lock_with_ownership(&root, &self.id)?;
            self.begin_effect_with_ownership(&root, index)?;
        }
        let (result, observed) = match perform() {
            Ok((value, observed)) => (Ok(value), observed),
            Err(error) => (Err(error), None),
        };
        let root = self.storage.acquire_ownership_lock_bound()?;
        let _identity = super::storage::acquire_session_identity_lock_with_ownership(&root)?;
        // A profile I/O error must not erase a known Git return from the central journal.
        let lifecycle = self
            .storage
            .acquire_instance_lifecycle_lock_with_ownership(&root, &self.id);
        if let Err(error) = self.finish_effect_with_ownership(
            &root,
            index,
            result.as_ref().err().map(|error| format!("{error:#}")),
            observed,
        ) {
            return Err(match &result {
                Ok(_) => error,
                Err(effect) => error.context(format!("external effect also failed: {effect:#}")),
            });
        }
        let _lifecycle = lifecycle?;
        let entry = self.lock_entry()?;
        let MoveAuthority::Resource { intent } = &entry.authority else {
            anyhow::bail!("resource transaction has no resource intent")
        };
        anyhow::ensure!(
            self.owner_is_current(intent)?,
            "resource lifecycle owner was superseded"
        );
        super::deletion::ensure_resource_intent_available(&root, &self.journal, intent)?;
        Ok(result)
    }

    pub(crate) fn from_recovery(
        storage: &super::Storage,
        journal: JournalTransaction,
        entry: MoveJournalEntry,
    ) -> anyhow::Result<Self> {
        let MoveAuthority::Resource { intent } = &entry.authority else {
            anyhow::bail!("journal does not describe a resource operation")
        };
        anyhow::ensure!(
            intent.owner.profile.matches(storage)?,
            "resource source profile is unavailable"
        );
        Ok(Self {
            storage: storage.clone(),
            id: intent.owner.id.clone(),
            journal,
            entry: Mutex::new(entry),
        })
    }

    pub(crate) fn request_rollback_with_ownership(
        &self,
        root: &super::storage::OwnershipGuard,
    ) -> anyhow::Result<()> {
        root.require_exclusive()?;
        let mut entry = self.lock_entry()?;
        let MoveAuthority::Resource { intent } = &mut entry.authority else {
            anyhow::bail!("resource transaction has no resource intent")
        };
        anyhow::ensure!(
            intent.effects.iter().all(|effect| !matches!(
                effect.phase,
                EffectPhase::Pending | EffectPhase::Conflict
            )),
            "an unfinished external effect cannot be compensated"
        );
        let profile = ProfileAuthority::capture(&self.storage)?;
        let rows = self.storage.load_strict_for_worktree_ownership()?;
        anyhow::ensure!(
            rows.iter()
                .any(|row| intent.row_is_reserved(&profile, row, self.journal.id())),
            "resource owner was superseded or already irreversibly removed"
        );
        super::deletion::ensure_resource_intent_available(root, &self.journal, intent)?;
        intent.verify_after(false)?;
        intent.rollback = true;
        intent.conflict = None;
        intent.phase = if intent
            .effects
            .iter()
            .any(|effect| effect.phase == EffectPhase::Applied)
        {
            EffectPhase::Applied
        } else {
            EffectPhase::Prepared
        };
        self.journal.rewrite(&entry)
    }

    pub(crate) fn append_compensation_with_ownership(
        &self,
        root: &super::storage::OwnershipGuard,
        effect: super::move_journal::ResourceEffect,
    ) -> anyhow::Result<usize> {
        root.require_exclusive()?;
        let mut entry = self.lock_entry()?;
        let MoveAuthority::Resource { intent } = &mut entry.authority else {
            anyhow::bail!("resource transaction has no resource intent")
        };
        anyhow::ensure!(
            intent.rollback,
            "compensation requires a recorded rollback decision"
        );
        anyhow::ensure!(
            self.owner_is_current(intent)?,
            "resource lifecycle owner was superseded"
        );
        intent.verify_after(true)?;
        let mut keys = Vec::new();
        effect.append_resource_keys(&mut keys)?;
        anyhow::ensure!(
            keys.iter().all(|key| intent.resources.contains(key)),
            "compensation would leave the original reserved resource scope"
        );
        super::deletion::ensure_resource_intent_available(root, &self.journal, intent)?;
        let index = intent.effects.len();
        intent.effects.push(super::move_journal::EffectRecord {
            effect,
            phase: EffectPhase::Prepared,
            before: None,
            after: None,
            error: None,
            compensation: true,
            retained: false,
        });
        self.journal.rewrite(&entry)?;
        Ok(index)
    }

    fn row_is_terminal(
        &self,
        intent: &ResourceIntent,
        goal: Option<&Instance>,
        target: &super::Storage,
    ) -> anyhow::Result<(bool, Option<Instance>)> {
        let source_rows = self.storage.load_strict_for_worktree_ownership()?;
        let source = source_rows.iter().find(|row| row.id == self.id);
        let target_rows;
        let current = if intent.owner.profile.sessions_path == intent.target.sessions_path
            || intent.rollback
        {
            source
        } else {
            if source.is_some() {
                return Ok((false, None));
            }
            target_rows = target.load_strict_for_worktree_ownership()?;
            target_rows.iter().find(|row| row.id == self.id)
        };
        match (current, goal) {
            (None, None) => Ok((true, None)),
            (Some(current), Some(goal))
                if current.created_at == intent.owner.created_at
                    && current.lifecycle_generation == intent.owner.generation
                    && current.lifecycle_reservation.is_none()
                    && super::move_journal::same_namespace(current, goal) =>
            {
                Ok((true, Some(current.clone())))
            }
            _ => Ok((false, None)),
        }
    }

    pub(crate) fn retire_for_purge_with_ownership(
        &self,
        root: &super::storage::OwnershipGuard,
    ) -> anyhow::Result<Instance> {
        root.require_exclusive()?;
        let entry = self.lock_entry()?;
        let MoveAuthority::Resource { intent } = &entry.authority else {
            anyhow::bail!("resource transaction has no resource intent")
        };
        anyhow::ensure!(
            intent.operation == LifecycleOperation::Purge
                && intent.after.is_none()
                && !intent.rollback,
            "resource transaction is not an irreversible purge"
        );
        anyhow::ensure!(
            !matches!(
                intent.phase,
                EffectPhase::Pending | EffectPhase::Conflict | EffectPhase::Committed
            ) && intent.effects.iter().all(|effect| !matches!(
                effect.phase,
                EffectPhase::Pending | EffectPhase::Conflict
            )),
            "purge has an unfinished external effect"
        );
        anyhow::ensure!(
            self.owner_is_current(intent)?,
            "resource lifecycle owner was superseded"
        );
        intent.verify_after(false)?;
        super::deletion::ensure_resource_intent_available(root, &self.journal, intent)?;
        let profile = ProfileAuthority::capture(&self.storage)?;
        let before = intent
            .before
            .as_ref()
            .context("purge has no original row")?;
        self.storage
            .update_with_intent(root, &self.journal, |rows, _| {
                let index = rows
                    .iter()
                    .position(|row| row.id == self.id && row.created_at == intent.owner.created_at)
                    .context("purge owner disappeared before retirement")?;
                anyhow::ensure!(
                    intent.row_is_reserved(&profile, &rows[index], self.journal.id())
                        && super::move_journal::same_namespace(&rows[index], before),
                    "purge owner changed before retirement"
                );
                Ok(rows.remove(index))
            })
    }

    /// Called with the canonical namespace guards after every external wait.
    pub(crate) fn publish_with_ownership<F>(
        &self,
        root: &super::storage::OwnershipGuard,
        mut validate: F,
    ) -> anyhow::Result<Option<Instance>>
    where
        F: FnMut(&[Instance], &Instance) -> anyhow::Result<()>,
    {
        root.require_exclusive()?;
        let mut entry = self.lock_entry()?;
        let MoveAuthority::Resource { intent } = &mut entry.authority else {
            anyhow::bail!("resource transaction has no resource intent")
        };
        anyhow::ensure!(
            intent
                .effects
                .iter()
                .all(|effect| effect.phase == EffectPhase::Applied
                    || (intent.rollback
                        && effect.phase == EffectPhase::Prepared
                        && effect.before.is_none())),
            "resource operation has unfinished external effects"
        );
        intent.verify_after(false)?;
        if intent.rollback {
            anyhow::ensure!(
                intent.next_compensation()?.is_none(),
                "resource compensation is not complete"
            );
        } else {
            for effect in &intent.effects {
                anyhow::ensure!(
                    effect.retained
                        || (effect.error.is_none() && intent.effect_goal_reached(effect)?),
                    "external effect did not reach its requested namespace state"
                );
            }
        }
        super::deletion::ensure_resource_intent_available(root, &self.journal, intent)?;
        let mut goal = if intent.rollback {
            if intent.operation == LifecycleOperation::Create {
                None
            } else {
                intent.before.clone()
            }
        } else {
            intent.after.clone()
        };
        if let Some(goal) = &mut goal {
            goal.lifecycle_generation = intent.owner.generation;
            goal.lifecycle_reservation = None;
        }
        let target = if intent.rollback
            || intent.owner.profile.sessions_path == intent.target.sessions_path
        {
            self.storage.clone()
        } else {
            super::Storage::open_with_ownership(
                &intent.target.profile,
                self.storage.file_watch(),
                root,
            )?
        };
        let destination = if intent.rollback {
            &intent.owner.profile
        } else {
            &intent.target
        };
        anyhow::ensure!(
            destination.matches(&target)?,
            "resource destination profile changed"
        );
        let (published, terminal) = self.row_is_terminal(intent, goal.as_ref(), &target)?;
        let committed = if published {
            super::storage::sync_parent_directory(self.storage.sessions_path())?;
            if target.sessions_path() != self.storage.sessions_path() {
                super::storage::sync_parent_directory(target.sessions_path())?;
            }
            terminal
        } else {
            anyhow::ensure!(
                self.owner_is_current(intent)?,
                "resource lifecycle owner was superseded"
            );
            if intent.rollback || intent.owner.profile.sessions_path == intent.target.sessions_path
            {
                let profile = ProfileAuthority::capture(&self.storage)?;
                self.storage
                    .update_with_intent(root, &self.journal, |rows, groups| {
                        let index = rows
                            .iter()
                            .position(|row| row.id == self.id)
                            .context("resource owner disappeared before publication")?;
                        anyhow::ensure!(
                            intent.row_is_reserved(&profile, &rows[index], self.journal.id()),
                            "resource owner changed before publication"
                        );
                        let Some(goal) = &goal else {
                            rows.remove(index);
                            return Ok(None);
                        };
                        if intent.operation == LifecycleOperation::Create && !intent.rollback {
                            rows[index].status = goal.status;
                            if rows[index].is_archived() {
                                rows[index].settle_archived_status();
                            }
                        } else {
                            let before = intent
                                .before
                                .as_ref()
                                .or(intent.after.as_ref())
                                .context("resource operation has no original row")?;
                            rows[index].merge_resource_diff(before, goal, false)?;
                        }
                        rows[index].lifecycle_generation = intent.owner.generation;
                        rows[index].lifecycle_reservation = None;
                        validate(rows, &rows[index])?;
                        let committed = rows[index].clone();
                        let mut tree = super::GroupTree::new_with_groups(rows, groups);
                        tree.create_group(&committed.group_path);
                        *groups = tree.get_all_groups();
                        Ok(Some(committed))
                    })?
            } else {
                let before = intent
                    .before
                    .as_ref()
                    .context("profile move has no original row")?;
                let after = goal.as_ref().context("profile move has no target row")?;
                Some(self.storage.move_instance_to_with_ownership(
                    root,
                    &target,
                    (before, after),
                    false,
                    |rows, candidate| validate(rows, candidate),
                    Some(&self.journal),
                )?)
            }
        };
        intent.phase = EffectPhase::Committed;
        intent.conflict = None;
        self.journal.rewrite(&entry)?;
        self.journal.consume()?;
        Ok(committed)
    }

    pub(crate) fn retain_unchanged_optional_with_ownership(
        &self,
        root: &super::storage::OwnershipGuard,
        index: usize,
    ) -> anyhow::Result<()> {
        root.require_exclusive()?;
        let mut entry = self.lock_entry()?;
        let MoveAuthority::Resource { intent } = &mut entry.authority else {
            anyhow::bail!("resource transaction has no resource intent")
        };
        anyhow::ensure!(
            intent.operation == LifecycleOperation::Purge,
            "only purge has optional cleanup"
        );
        intent.verify_after(true)?;
        super::deletion::ensure_resource_intent_available(root, &self.journal, intent)?;
        let effect = intent
            .effects
            .get_mut(index)
            .context("resource effect index is invalid")?;
        // A directory's continued existence cannot prove a recursive removal did nothing.
        anyhow::ensure!(
            matches!(
                effect.effect,
                super::move_journal::ResourceEffect::DeleteBranch { .. }
            ) && effect.phase == EffectPhase::Applied
                && effect.before.is_some()
                && effect.before == effect.after
                && effect.error.is_some(),
            "optional cleanup has no verified unchanged completed result"
        );
        effect.retained = true;
        self.journal.rewrite(&entry)
    }

    fn retry_purge_cleanup_with_ownership(
        &self,
        root: &super::storage::OwnershipGuard,
        index: usize,
    ) -> anyhow::Result<()> {
        root.require_exclusive()?;
        let mut entry = self.lock_entry()?;
        let MoveAuthority::Resource { intent } = &mut entry.authority else {
            anyhow::bail!("resource transaction has no resource intent")
        };
        anyhow::ensure!(
            intent.operation == LifecycleOperation::Purge
                && !intent.rollback
                && intent.phase != EffectPhase::Conflict
                && self.owner_is_current(intent)?
                && !self
                    .storage
                    .load_strict_for_worktree_ownership()?
                    .iter()
                    .any(|row| row.id == self.id),
            "cleanup is not an owned irreversible purge"
        );
        intent.verify_after(true)?;
        super::deletion::ensure_resource_intent_available(root, &self.journal, intent)?;
        let record = intent
            .effects
            .get_mut(index)
            .context("invalid cleanup index")?;
        anyhow::ensure!(
            record.phase == EffectPhase::Applied
                && record.before.is_some()
                && record.after.is_some()
                && record.error.is_some()
                && !record.retained
                && !record.compensation
                && super::deletion::retryable_purge_cleanup(&record.effect),
            "cleanup has no retryable completed failure"
        );
        record.phase = EffectPhase::Prepared;
        record.before = None;
        record.after = None;
        record.error = None;
        intent.phase = EffectPhase::Applied;
        intent.conflict = None;
        self.journal.rewrite(&entry)
    }

    pub(crate) fn note_conflict_with_ownership(
        &self,
        root: &super::storage::OwnershipGuard,
        reason: String,
    ) -> anyhow::Result<()> {
        root.require_exclusive()?;
        if !self.journal.path.try_exists()? {
            return Ok(());
        }
        let mut entry = self.lock_entry()?;
        let MoveAuthority::Resource { intent } = &mut entry.authority else {
            anyhow::bail!("resource transaction has no resource intent")
        };
        intent.conflict = Some(reason);
        if intent
            .effects
            .iter()
            .any(|effect| matches!(effect.phase, EffectPhase::Pending | EffectPhase::Conflict))
        {
            intent.phase = EffectPhase::Conflict;
        }
        self.journal.rewrite(&entry)
    }

    pub(crate) fn effect(
        &self,
        index: usize,
    ) -> anyhow::Result<super::move_journal::ResourceEffect> {
        let entry = self.lock_entry()?;
        let MoveAuthority::Resource { intent } = &entry.authority else {
            anyhow::bail!("resource transaction has no resource intent")
        };
        Ok(intent
            .effects
            .get(index)
            .context("resource effect index is invalid")?
            .effect
            .clone())
    }

    pub(crate) fn rollback(&self) -> anyhow::Result<()> {
        {
            let root = self.storage.acquire_ownership_lock_bound()?;
            let _identity = super::storage::acquire_session_identity_lock_with_ownership(&root)?;
            let _lifecycle = self
                .storage
                .acquire_instance_lifecycle_lock_with_ownership(&root, &self.id)?;
            let decided = {
                let entry = self.lock_entry()?;
                let MoveAuthority::Resource { intent } = &entry.authority else {
                    anyhow::bail!("resource transaction has no resource intent")
                };
                intent.rollback
            };
            if !decided {
                self.request_rollback_with_ownership(&root)?;
            }
        }
        loop {
            let (index, effect) = {
                let root = self.storage.acquire_ownership_lock_bound()?;
                let _identity =
                    super::storage::acquire_session_identity_lock_with_ownership(&root)?;
                let _lifecycle = self
                    .storage
                    .acquire_instance_lifecycle_lock_with_ownership(&root, &self.id)?;
                let effect = {
                    let entry = self.lock_entry()?;
                    let MoveAuthority::Resource { intent } = &entry.authority else {
                        anyhow::bail!("resource transaction has no resource intent")
                    };
                    anyhow::ensure!(
                        intent.effects.iter().all(|record| !matches!(
                            record.phase,
                            EffectPhase::Pending | EffectPhase::Conflict
                        )),
                        "an unfinished compensation cannot be replayed"
                    );
                    let release_creation_container = intent.operation == LifecycleOperation::Create
                        && intent.after.as_ref().is_some_and(Instance::is_sandboxed)
                        && intent.effects.iter().any(|record| {
                            !record.compensation
                                && matches!(
                                    record.effect,
                                    super::move_journal::ResourceEffect::Callback { .. }
                                )
                                && record.phase == EffectPhase::Applied
                        })
                        && !intent.effects.iter().any(|record| {
                            record.compensation
                                && matches!(
                                    record.effect,
                                    super::move_journal::ResourceEffect::ReleaseContainer
                                )
                                && record.phase == EffectPhase::Applied
                                && record
                                    .after
                                    .as_ref()
                                    .is_some_and(|after| after.container_released == Some(true))
                        });
                    if release_creation_container {
                        Some(super::move_journal::ResourceEffect::ReleaseContainer)
                    } else {
                        intent.next_compensation()?
                    }
                };
                let Some(effect) = effect else {
                    return self
                        .publish_with_ownership(&root, |_, _| Ok(()))
                        .map(|_| ());
                };
                let index = self.append_compensation_with_ownership(&root, effect.clone())?;
                (index, effect)
            };
            self.run_effect(index, || {
                if matches!(
                    effect,
                    super::move_journal::ResourceEffect::ReleaseContainer
                ) {
                    match crate::containers::DockerContainer::from_session_id(&self.id)
                        .teardown(&self.id)
                    {
                        crate::containers::Teardown::Removed
                        | crate::containers::Teardown::AlreadyGone => Ok((
                            Vec::new(),
                            Some(EffectObservation {
                                container_released: Some(true),
                                ..Default::default()
                            }),
                        )),
                        crate::containers::Teardown::Failed(error) => Err(error.into()),
                    }
                } else {
                    effect.execute().map(|warnings| (warnings, None))
                }
            })??;
        }
    }

    pub(crate) fn recover(&self) -> anyhow::Result<bool> {
        let (rollback, purge, count) = {
            let root = self.storage.acquire_ownership_lock_bound()?;
            let _identity = super::storage::acquire_session_identity_lock_with_ownership(&root)?;
            let _lifecycle = self
                .storage
                .acquire_instance_lifecycle_lock_with_ownership(&root, &self.id)?;
            let mut entry = self.lock_entry()?;
            let MoveAuthority::Resource { intent } = &mut entry.authority else {
                anyhow::bail!("resource transaction has no resource intent")
            };
            if intent
                .effects
                .iter()
                .any(|effect| matches!(effect.phase, EffectPhase::Pending | EffectPhase::Conflict))
            {
                intent.phase = EffectPhase::Conflict;
                let reason = "an external effect has no completed observation; its child may still be running";
                intent.conflict = Some(reason.to_owned());
                self.journal.rewrite(&entry)?;
                anyhow::bail!("{reason}");
            }
            anyhow::ensure!(
                intent.phase != EffectPhase::Conflict,
                "{}",
                intent
                    .conflict
                    .as_deref()
                    .unwrap_or("resource intent retains a conflict")
            );
            let rows = self.storage.load_strict_for_worktree_ownership()?;
            let current = rows.iter().find(|row| row.id == self.id);
            if intent
                .effects
                .iter()
                .all(|effect| effect.phase == EffectPhase::Prepared && effect.before.is_none())
                && (intent.operation != LifecycleOperation::Purge || current.is_some())
                && !current.is_some_and(|row| {
                    row.lifecycle_reservation.as_ref().is_some_and(|lease| {
                        lease.resource_intent.as_deref() == Some(self.journal.id())
                    })
                })
            {
                if intent.operation == LifecycleOperation::Create
                    && current.is_some_and(|row| {
                        intent.before.as_ref().is_some_and(|before| {
                            super::move_journal::row_resources_match(row, before)
                                && row.lifecycle_reservation == before.lifecycle_reservation
                        })
                    })
                {
                    self.storage
                        .update_with_intent(&root, &self.journal, |rows, _| {
                            rows.retain(|row| row.id != self.id);
                            Ok(())
                        })?;
                }
                self.journal.consume()?;
                return Ok(true);
            }
            let purge = intent.operation == LifecycleOperation::Purge && current.is_none();
            let mut incomplete = false;
            for effect in &intent.effects {
                if effect.phase != EffectPhase::Applied
                    || (!effect.retained
                        && (effect.error.is_some() || !intent.effect_goal_reached(effect)?))
                {
                    incomplete = true;
                    break;
                }
            }
            (
                intent.rollback || (!purge && incomplete),
                purge,
                intent.effects.len(),
            )
        };
        if rollback {
            self.rollback()?;
            return Ok(true);
        }
        if purge {
            let (owner, delete_sandbox) = {
                let entry = self.lock_entry()?;
                let MoveAuthority::Resource { intent } = &entry.authority else {
                    unreachable!()
                };
                anyhow::ensure!(
                    !intent.rollback && intent.phase != EffectPhase::Conflict,
                    "purge has a retained conflict"
                );
                let mut owner = intent
                    .before
                    .clone()
                    .context("purge has no original owner")?;
                anyhow::ensure!(
                    owner.id == intent.owner.id && owner.created_at == intent.owner.created_at,
                    "purge original incarnation differs from its owner"
                );
                owner
                    .source_profile
                    .clone_from(&intent.owner.profile.profile);
                anyhow::ensure!(intent.effects.iter().any(|record|
                    matches!(&record.effect, super::move_journal::ResourceEffect::Callback { name, .. }
                        if name == "purge-hooks")
                        && record.phase == EffectPhase::Applied && record.error.is_none()),
                    "purge hooks have no completed successful observation");
                let delete_sandbox = intent.effects.iter().any(|record| {
                    matches!(
                        record.effect,
                        super::move_journal::ResourceEffect::ReleaseContainer
                    )
                });
                (owner, delete_sandbox)
            };
            for index in 0..count {
                let (prepared, failed, retained) = {
                    let entry = self.lock_entry()?;
                    let MoveAuthority::Resource { intent } = &entry.authority else {
                        unreachable!()
                    };
                    let record = &intent.effects[index];
                    (
                        record.phase == EffectPhase::Prepared,
                        record.error.is_some(),
                        record.retained,
                    )
                };
                let effect = self.effect(index)?;
                if failed && !retained {
                    let root = self.storage.acquire_ownership_lock_bound()?;
                    let _identity =
                        super::storage::acquire_session_identity_lock_with_ownership(&root)?;
                    let _lifecycle = self
                        .storage
                        .acquire_instance_lifecycle_lock_with_ownership(&root, &self.id)?;
                    if super::deletion::retryable_purge_cleanup(&effect) {
                        self.retry_purge_cleanup_with_ownership(&root, index)?;
                    } else {
                        self.retain_unchanged_optional_with_ownership(&root, index)?;
                        continue;
                    }
                }
                if prepared || (failed && !retained) {
                    if let Err(error) = self.run_effect(index, || {
                        super::deletion::execute_purge_effect(
                            &owner,
                            &effect,
                            delete_sandbox,
                            |id| {
                                Ok(crate::containers::DockerContainer::from_session_id(id)
                                    .teardown(id))
                            },
                        )
                    })? {
                        if matches!(
                            effect,
                            super::move_journal::ResourceEffect::DeleteBranch { .. }
                        ) {
                            let root = self.storage.acquire_ownership_lock_bound()?;
                            self.retain_unchanged_optional_with_ownership(&root, index)
                                .with_context(|| format!("purge cleanup failed: {error:#}"))?;
                        } else {
                            return Err(error.context("purge cleanup retained for retry"));
                        }
                    }
                }
            }
        }
        let root = self.storage.acquire_ownership_lock_bound()?;
        let _identity = super::storage::acquire_session_identity_lock_with_ownership(&root)?;
        let _lifecycle = self
            .storage
            .acquire_instance_lifecycle_lock_with_ownership(&root, &self.id)?;
        self.publish_with_ownership(&root, |rows, candidate| {
            anyhow::ensure!(
                !super::is_duplicate_session(
                    rows.iter(),
                    &candidate.title,
                    &candidate.project_path,
                    Some(&candidate.id)
                ),
                "the recovered session title is already in use"
            );
            Ok(())
        })?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    #[serial_test::serial]
    fn abandonment_only_releases_the_original_reservation() {
        for operation in [
            LifecycleOperation::Trash,
            LifecycleOperation::Launch,
            LifecycleOperation::Rename,
        ] {
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
                    let mut replacement_row = original.clone();
                    replacement_row.created_at += chrono::Duration::seconds(1);
                    replacement
                        .update(|rows, _| {
                            rows.push(replacement_row);
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
                let before = if matches!(change, "corrupt" | "checkout") {
                    None
                } else {
                    Some(
                        observed_storage
                            .load_strict_for_worktree_ownership()
                            .unwrap(),
                    )
                };
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
                } else if let Some(before) = before {
                    let after = observed_storage
                        .load_strict_for_worktree_ownership()
                        .unwrap();
                    assert_eq!(after.len(), before.len(), "{operation:?}/{change}");
                    for (before, after) in before.iter().zip(&after) {
                        assert_eq!(after.id, before.id, "{operation:?}/{change}");
                        assert_eq!(
                            after.created_at, before.created_at,
                            "{operation:?}/{change}"
                        );
                        assert_eq!(
                            after.project_path, before.project_path,
                            "{operation:?}/{change}"
                        );
                        assert_eq!(
                            after.lifecycle_generation, before.lifecycle_generation,
                            "{operation:?}/{change}"
                        );
                        assert_eq!(
                            after.lifecycle_reservation, before.lifecycle_reservation,
                            "{operation:?}/{change}"
                        );
                    }
                } else {
                    assert_eq!(
                        std::fs::read(observed_storage.sessions_path()).unwrap(),
                        b"[{broken"
                    );
                    assert!(observed_storage
                        .load_strict_for_worktree_ownership()
                        .is_err());
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
    }
}
