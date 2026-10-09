//! Durable lifecycle exclusion is independent of native execution authority.

use super::*;

/// One durable ownership protocol for every session lifecycle transition.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LifecycleOperation {
    Launch,
    Capture,
    Stop,
    Purge,
    Restore,
    Trash,
    Attach,
    Create,
}

impl LifecycleOperation {
    pub(crate) fn busy_reason(self) -> String {
        format!("busy with lifecycle operation {self:?}")
    }

    pub(crate) fn already_in_progress_reason(self) -> String {
        format!("lifecycle operation {self:?} is already in progress")
    }
}

pub(crate) const NEWER_GENERATION_BUSY_REASON: &str = "busy with a newer lifecycle generation";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleReservationError {
    Busy(LifecycleOperation),
    GenerationOverflow,
}

impl std::fmt::Display for LifecycleReservationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy(operation) => f.write_str(&operation.already_in_progress_reason()),
            Self::GenerationOverflow => f.write_str("lifecycle generation overflow"),
        }
    }
}

impl std::error::Error for LifecycleReservationError {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", content = "paths", rename_all = "snake_case")]
pub enum WorktreePathClaims {
    None,
    Pending(Vec<std::path::PathBuf>),
    Unknown(Option<Vec<std::path::PathBuf>>),
}

impl WorktreePathClaims {
    pub(crate) fn is_pending(&self) -> bool {
        !matches!(self, Self::None)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LifecycleReservation {
    pub op: LifecycleOperation,
    pub generation: u64,
    pub at: DateTime<Utc>,
    pub path_claims: WorktreePathClaims,
    pub custodian: Option<crate::process::OriginalCustodianBirth>,
}

impl Instance {
    /// Claims-free lifecycle leases expire after a crashed owner.
    pub const LIFECYCLE_RESERVATION_TTL: chrono::Duration = chrono::Duration::minutes(10);

    /// Acquire exclusive durable ownership of the next lifecycle generation.
    pub fn try_acquire_lifecycle_reservation(
        &mut self,
        operation: LifecycleOperation,
        ttl: chrono::Duration,
        now: DateTime<Utc>,
    ) -> Result<u64, LifecycleReservationError> {
        if let Some(reservation) = self.lifecycle_reservation.as_ref().filter(|reservation| {
            reservation.path_claims.is_pending()
                || (reservation.generation == self.lifecycle_generation
                    && (now - reservation.at) < ttl)
        }) {
            return Err(LifecycleReservationError::Busy(reservation.op));
        }

        let generation = self
            .lifecycle_generation
            .checked_add(1)
            .ok_or(LifecycleReservationError::GenerationOverflow)?;
        self.lifecycle_generation = generation;
        self.lifecycle_reservation = Some(LifecycleReservation {
            op: operation,
            generation,
            at: now,
            path_claims: WorktreePathClaims::None,
            custodian: None,
        });
        Ok(generation)
    }

    pub fn lifecycle_reservation_is_owned(
        &self,
        operation: LifecycleOperation,
        generation: u64,
    ) -> bool {
        self.lifecycle_generation == generation
            && matches!(
                &self.lifecycle_reservation,
                Some(reservation)
                    if reservation.op == operation && reservation.generation == generation
            )
    }

    pub fn has_active_lifecycle_reservation(&self, now: DateTime<Utc>) -> bool {
        matches!(
            &self.lifecycle_reservation,
            Some(reservation)
                if reservation.path_claims.is_pending()
                    || (reservation.generation == self.lifecycle_generation
                        && (now - reservation.at) < Self::LIFECYCLE_RESERVATION_TTL)
        )
    }

    /// Reconciliation must not respawn a purge-reserved session.
    pub fn is_purge_reserved(&self, now: DateTime<Utc>) -> bool {
        self.has_active_lifecycle_reservation(now)
            && self
                .lifecycle_reservation
                .as_ref()
                .is_some_and(|reservation| reservation.op == LifecycleOperation::Purge)
    }

    pub fn release_lifecycle_reservation_if_owned(
        &mut self,
        operation: LifecycleOperation,
        generation: u64,
    ) -> bool {
        if self.lifecycle_reservation_is_owned(operation, generation)
            && !self.has_pending_worktree_path_claims()
        {
            self.lifecycle_reservation = None;
            true
        } else {
            false
        }
    }

    pub(crate) fn has_pending_worktree_path_claims(&self) -> bool {
        self.lifecycle_reservation
            .as_ref()
            .is_some_and(|reservation| reservation.path_claims.is_pending())
    }

    /// Clock expiry cannot settle pending filesystem effects.
    pub fn clear_expired_lifecycle_reservation(
        &mut self,
        ttl: chrono::Duration,
        now: DateTime<Utc>,
    ) -> bool {
        if matches!(
            &self.lifecycle_reservation,
            Some(reservation)
                if !reservation.path_claims.is_pending()
                    && reservation.generation == self.lifecycle_generation
                    && (now - reservation.at) >= ttl
        ) {
            self.lifecycle_reservation = None;
            true
        } else {
            false
        }
    }

    pub(super) fn commit_lifecycle_launch(
        &mut self,
        storage: &crate::session::storage::Storage,
        restart: bool,
    ) -> Result<()> {
        let generation = self.lifecycle_generation;
        storage.update_metadata(
            crate::session::MetadataSelection::Session(self.id.as_str().into()),
            |instances, _groups| {
                let stored = instances
                    .iter_mut()
                    .find(|instance| instance.id == self.id)
                    .with_context(|| {
                        format!("session {} disappeared before launch commit", self.id)
                    })?;
                anyhow::ensure!(
                    stored.release_lifecycle_reservation_if_owned(
                        LifecycleOperation::Launch,
                        generation
                    ),
                    "session {} lost its launch reservation or retains pending path claims",
                    self.id
                );
                stored.status = self.status;
                stored.idle_entered_at = self.idle_entered_at;
                stored.last_accessed_at = self.last_accessed_at;
                stored.sandbox_info = self.sandbox_info.clone();
                stored.capture_started_at = self.capture_started_at;
                stored.active_execution = self.active_execution.clone();
                if restart && stored.agent_session_id == self.agent_session_id {
                    stored.resume_probe_failed_sid = self.resume_probe_failed_sid.clone();
                }
                stored.first_launch_names_agent = false;
                Ok(())
            },
        )?;
        self.lifecycle_reservation = None;
        self.first_launch_names_agent = false;
        Ok(())
    }

    pub(super) fn acquire_lifecycle_reservation(
        &mut self,
        storage: &crate::session::storage::Storage,
        operation: LifecycleOperation,
        status: Option<Status>,
        acknowledgement: Option<&mut Option<std::sync::Arc<crate::session::LaunchOrigin>>>,
    ) -> Result<u64> {
        let now = Utc::now();
        let mut acquired = None;
        let mut receipt = None;
        storage.update_metadata(
            crate::session::MetadataSelection::Session(self.id.as_str().into()),
            |instances, _groups| {
                let Some(stored) = instances.iter_mut().find(|instance| instance.id == self.id)
                else {
                    return Ok(());
                };
                let generation = stored
                    .try_acquire_lifecycle_reservation(
                        operation,
                        Self::LIFECYCLE_RESERVATION_TTL,
                        now,
                    )
                    .map_err(|error| match error {
                        LifecycleReservationError::Busy(holder) => {
                            anyhow::anyhow!("session {} is {}", self.id, holder.busy_reason())
                        }
                        LifecycleReservationError::GenerationOverflow => {
                            anyhow::anyhow!("session {} lifecycle generation overflow", self.id)
                        }
                    })?;
                if let Some(status) = status {
                    stored.status = status;
                    if status != Status::Idle {
                        stored.idle_entered_at = None;
                    }
                }
                if acknowledgement.is_some() {
                    let mut emitted = stored.clone();
                    emitted.storage_origin = Some(std::sync::Arc::new(storage.clone()));
                    receipt = Some(crate::session::LaunchOrigin::capture(&emitted)?);
                }
                acquired = Some((generation, stored.lifecycle_reservation.clone()));
                Ok(())
            },
        )?;
        let Some((generation, reservation)) = acquired else {
            anyhow::bail!("session {} no longer exists", self.id);
        };
        self.lifecycle_generation = generation;
        self.lifecycle_reservation = reservation;
        if let Some(status) = status {
            self.status = status;
            if status != Status::Idle {
                self.idle_entered_at = None;
            }
        }
        if let Some(destination) = acknowledgement {
            *destination = receipt;
        }
        Ok(generation)
    }

    pub(super) fn commit_lifecycle_status(
        &mut self,
        storage: &crate::session::storage::Storage,
        operation: LifecycleOperation,
        status: Status,
    ) -> Result<()> {
        let generation = self.lifecycle_generation;
        storage.update_metadata(
            crate::session::MetadataSelection::Session(self.id.as_str().into()),
            |instances, _groups| {
                let stored = instances
                    .iter_mut()
                    .find(|instance| instance.id == self.id)
                    .with_context(|| {
                        format!("session {} disappeared before lifecycle commit", self.id)
                    })?;
                anyhow::ensure!(
                    stored.release_lifecycle_reservation_if_owned(operation, generation),
                    "session {} lost its lifecycle reservation or retains pending path claims",
                    self.id
                );
                stored.status = status;
                if status != Status::Idle {
                    stored.idle_entered_at = None;
                }
                Ok(())
            },
        )?;
        self.lifecycle_reservation = None;
        self.status = status;
        if status != Status::Idle {
            self.idle_entered_at = None;
        }
        Ok(())
    }

    pub(super) fn release_lifecycle_reservation(
        &mut self,
        storage: &crate::session::storage::Storage,
        operation: LifecycleOperation,
    ) -> Result<()> {
        let generation = self.lifecycle_generation;
        storage.update_metadata(
            crate::session::MetadataSelection::Session(self.id.as_str().into()),
            |instances, _groups| {
                let stored = instances
                    .iter_mut()
                    .find(|instance| instance.id == self.id)
                    .with_context(|| {
                        format!("session {} disappeared before lifecycle release", self.id)
                    })?;
                anyhow::ensure!(
                    stored.release_lifecycle_reservation_if_owned(operation, generation),
                    "session {} lost its lifecycle reservation or retains pending path claims",
                    self.id
                );
                Ok(())
            },
        )?;
        self.lifecycle_reservation = None;
        Ok(())
    }

    /// Reacquire launch locks after user hooks, preserving the global
    /// title-before-lifecycle order and failing the reservation consistently.
    pub(super) fn reacquire_launch_locks_after_hooks(
        &mut self,
        storage: &crate::session::storage::Storage,
        hook_result: Result<()>,
    ) -> Result<(
        crate::session::storage::StorageFlock,
        crate::session::storage::StorageFlock,
    )> {
        let title_lock = match crate::session::storage::acquire_session_title_lock(&self.id)
            .context("failed to reacquire instance title lock after hooks")
        {
            Ok(lock) => lock,
            Err(error) => {
                self.fail_reserved_launch(storage, &error, false);
                return Err(error);
            }
        };
        let lifecycle_lock = match storage
            .acquire_instance_lifecycle_lock(&self.id)
            .context("failed to reacquire instance lifecycle lock after hooks")
        {
            Ok(lock) => lock,
            Err(error) => {
                self.fail_reserved_launch(storage, &error, false);
                return Err(error);
            }
        };
        self.reconcile_from_disk();
        if let Err(blocked) = self.ensure_startable() {
            self.release_blocked_launch(storage);
            return Err(blocked.into());
        }
        if let Err(error) = hook_result {
            self.fail_reserved_launch(storage, &error, false);
            return Err(error);
        }
        self.ensure_reservation_current_or_fail(storage)?;
        Ok((title_lock, lifecycle_lock))
    }

    /// A peer archived or trashed the row while hooks ran: drop the launch reservation
    /// without stamping an error, since the refusal is not a launch failure.
    fn release_blocked_launch(&mut self, storage: &crate::session::storage::Storage) {
        let live_pane = self
            .tmux_session()
            .is_ok_and(|session| session.exists() && !session.is_pane_dead());
        let status = if live_pane {
            Status::Idle
        } else {
            Status::Stopped
        };
        // The commit releases only a reservation this launch still owns.
        if let Err(error) =
            self.commit_lifecycle_status(storage, LifecycleOperation::Launch, status)
        {
            tracing::warn!(
                target: "session.store",
                session = %self.id,
                "could not release the launch reservation of a refused start: {error:#}"
            );
        }
    }

    fn lifecycle_reservation_is_current(
        &self,
        storage: &crate::session::storage::Storage,
        operation: LifecycleOperation,
    ) -> Result<bool> {
        let generation = self.lifecycle_generation;
        storage.update_metadata(
            crate::session::MetadataSelection::Session(self.id.as_str().into()),
            |instances, _groups| {
                Ok(instances
                    .iter()
                    .find(|instance| instance.id == self.id)
                    .is_some_and(|stored| {
                        stored.lifecycle_reservation_is_owned(operation, generation)
                    }))
            },
        )
    }

    fn reservation_is_current(&self, storage: &crate::session::storage::Storage) -> Result<bool> {
        self.lifecycle_reservation_is_current(storage, LifecycleOperation::Launch)
    }

    fn ensure_reservation_current(&self, storage: &crate::session::storage::Storage) -> Result<()> {
        if self.reservation_is_current(storage)? {
            return Ok(());
        }
        anyhow::bail!(
            "session {} changed while launch hooks were running",
            self.id
        )
    }

    fn ensure_reservation_current_or_fail(
        &mut self,
        storage: &crate::session::storage::Storage,
    ) -> Result<()> {
        match self.ensure_reservation_current(storage) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.fail_reserved_launch(storage, &error, false);
                Err(error)
            }
        }
    }

    pub(super) fn fail_reserved_launch(
        &mut self,
        storage: &crate::session::storage::Storage,
        error: &anyhow::Error,
        cleanup_pane: bool,
    ) {
        if !self.reservation_is_current(storage).unwrap_or(false) {
            return;
        }
        if cleanup_pane {
            let _ = self.kill_clean_locked();
        }
        self.last_error = Some(format!("{error:#}"));
        let _ = self.commit_lifecycle_status(storage, LifecycleOperation::Launch, Status::Error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn held(op: LifecycleOperation, at: DateTime<Utc>) -> Option<LifecycleReservation> {
        Some(LifecycleReservation {
            op,
            generation: 1,
            at,
            path_claims: crate::session::WorktreePathClaims::None,
            custodian: None,
        })
    }

    #[test]
    #[serial_test::serial]
    fn refused_durable_release_keeps_pending_claims_and_memory_reservation() {
        let _home = crate::session::test_support::isolate_app_dir();
        let storage = crate::session::storage::Storage::new_unwatched("refused-release").unwrap();
        for operation in [LifecycleOperation::Launch, LifecycleOperation::Stop] {
            for claims in [
                WorktreePathClaims::Pending(vec!["/protected".into()]),
                WorktreePathClaims::Unknown(None),
            ] {
                let mut original = Instance::new("session", "/protected");
                original.status = Status::Starting;
                original.lifecycle_generation = 1;
                original.lifecycle_reservation = held(
                    operation,
                    DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
                );
                original.lifecycle_reservation.as_mut().unwrap().path_claims = claims;
                original.first_launch_names_agent = true;
                let source = serde_json::to_vec(&vec![original.clone()]).unwrap();
                std::fs::write(storage.sessions_path(), &source).unwrap();
                let mut candidate = storage.load().unwrap().remove(0);
                candidate.status = Status::Idle;
                let reservation = candidate.lifecycle_reservation.clone();
                let result = if operation == LifecycleOperation::Launch {
                    candidate.commit_lifecycle_launch(&storage, true)
                } else {
                    candidate.commit_lifecycle_status(&storage, operation, Status::Stopped)
                };
                assert!(result.is_err());
                assert_eq!(candidate.lifecycle_reservation, reservation);
                assert_eq!(candidate.status, Status::Idle);
                assert_eq!(std::fs::read(storage.sessions_path()).unwrap(), source);
                let persisted = storage.load().unwrap().remove(0);
                assert_eq!(persisted.status, Status::Starting);
                assert!(persisted.first_launch_names_agent);
                assert_eq!(
                    persisted.lifecycle_reservation,
                    original.lifecycle_reservation
                );
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn lifecycle_status_commit_releases_the_acquired_generation() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let storage = crate::session::storage::Storage::new_unwatched("lifecycle-lease").unwrap();
        let mut instance = Instance::new("session", "/tmp/test");

        let missing = instance
            .acquire_lifecycle_reservation(
                &storage,
                LifecycleOperation::Launch,
                Some(Status::Starting),
                None,
            )
            .unwrap_err();
        assert!(missing.to_string().contains("no longer exists"));

        storage
            .update(|instances, _groups| {
                instances.push(instance.clone());
                Ok(())
            })
            .unwrap();
        instance
            .acquire_lifecycle_reservation(
                &storage,
                LifecycleOperation::Launch,
                Some(Status::Starting),
                None,
            )
            .unwrap();
        let generation = instance.lifecycle_generation;
        instance
            .commit_lifecycle_status(&storage, LifecycleOperation::Launch, Status::Error)
            .unwrap();

        let reloaded = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.id == instance.id)
            .unwrap();
        assert_eq!(reloaded.lifecycle_generation, generation);
        assert_eq!(reloaded.lifecycle_reservation, None);
        assert_eq!(reloaded.status, Status::Error);
    }

    #[test]
    #[serial_test::serial]
    fn lifecycle_reservation_rejects_busy_state_without_blocking_first_launch() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let profile = "lifecycle-busy-reservation";
        let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
        let now = Utc::now();
        let stale = now - Instance::LIFECYCLE_RESERVATION_TTL - chrono::Duration::seconds(1);
        let mut cases = [
            ("unleased", Status::Starting, None, 0, true),
            (
                "leased_peer",
                Status::Starting,
                held(LifecycleOperation::Launch, now),
                1,
                false,
            ),
            (
                "superseded",
                Status::Idle,
                held(LifecycleOperation::Launch, now),
                2,
                true,
            ),
            (
                "expired",
                Status::Idle,
                held(LifecycleOperation::Launch, stale),
                1,
                true,
            ),
            (
                "purge",
                Status::Idle,
                held(LifecycleOperation::Purge, now),
                1,
                false,
            ),
            (
                "restore",
                Status::Stopped,
                held(LifecycleOperation::Restore, now),
                1,
                false,
            ),
            (
                "trash",
                Status::Idle,
                held(LifecycleOperation::Trash, now),
                1,
                false,
            ),
            (
                "capture",
                Status::Running,
                held(LifecycleOperation::Capture, now),
                1,
                false,
            ),
            ("creating", Status::Creating, None, 0, true),
            ("idle", Status::Idle, None, 0, true),
            ("stopped", Status::Stopped, None, 0, true),
        ]
        .map(
            |(title, status, lifecycle_reservation, lifecycle_generation, allowed)| {
                let mut instance = Instance::new(title, "/tmp/test");
                instance.source_profile = profile.to_string();
                instance.status = status;
                instance.lifecycle_generation = lifecycle_generation;
                instance.lifecycle_reservation = lifecycle_reservation;
                (instance, allowed)
            },
        );
        storage
            .update(|instances, _groups| {
                instances.extend(cases.iter().map(|(instance, _)| instance.clone()));
                Ok(())
            })
            .unwrap();

        for (instance, allowed) in &mut cases {
            let result = instance.acquire_lifecycle_reservation(
                &storage,
                LifecycleOperation::Launch,
                Some(Status::Starting),
                None,
            );
            assert_eq!(result.is_ok(), *allowed, "{}", instance.title);
        }

        let leased = &cases[0].0;
        assert!(leased.reservation_is_current(&storage).unwrap());
        storage
            .update(|instances, _groups| {
                let peer = instances
                    .iter_mut()
                    .find(|candidate| candidate.id == leased.id)
                    .unwrap();
                peer.lifecycle_generation += 1;
                peer.status = Status::Stopped;
                Ok(())
            })
            .unwrap();
        assert!(!leased.reservation_is_current(&storage).unwrap());

        let mut busy = Instance::new("busy-leased", "/tmp/test");
        busy.source_profile = profile.to_string();
        busy.status = Status::Starting;
        busy.lifecycle_generation = 1;
        busy.lifecycle_reservation = held(LifecycleOperation::Launch, Utc::now());
        storage
            .update(|instances, _groups| {
                instances.push(busy.clone());
                Ok(())
            })
            .unwrap();

        let began = std::time::Instant::now();
        assert!(busy.stop().unwrap_err().to_string().contains("busy"));
        assert!(began.elapsed() < std::time::Duration::from_secs(1));

        let mut recursive_start = busy.clone();
        let began = std::time::Instant::now();
        assert!(recursive_start
            .start_with_size_opts(None, true)
            .unwrap_err()
            .to_string()
            .contains("busy"));
        assert!(began.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    #[serial_test::serial]
    fn failed_launch_releases_reservation_after_status_drift() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let profile = "lifecycle-fail-drift";
        let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
        let mut inst = Instance::new("drift", "/tmp/test");
        inst.source_profile = profile.to_string();
        inst.status = Status::Idle;
        storage
            .update(|instances, _groups| {
                instances.push(inst.clone());
                Ok(())
            })
            .unwrap();

        inst.acquire_lifecycle_reservation(
            &storage,
            LifecycleOperation::Launch,
            Some(Status::Starting),
            None,
        )
        .unwrap();
        let reserved_gen = inst.lifecycle_generation;

        // A same-generation passive status patch changes presentation state
        // without changing ownership while prepare_launch runs unlocked.
        storage
            .update(|instances, _groups| {
                let stored = instances.iter_mut().find(|i| i.id == inst.id).unwrap();
                assert_eq!(stored.lifecycle_generation, reserved_gen);
                assert!(stored.lifecycle_reservation.is_some());
                stored.status = Status::Stopped;
                Ok(())
            })
            .unwrap();

        // The launch guard still recognizes the exact-generation reservation. A later launch
        // failure must release it rather than stranding the marker until its TTL.
        inst.ensure_reservation_current_or_fail(&storage).unwrap();
        let error = anyhow::anyhow!("launch failed after status drift");
        inst.fail_reserved_launch(&storage, &error, false);

        let leftover = storage
            .update(|instances, _groups| {
                Ok(instances
                    .iter()
                    .find(|instance| instance.id == inst.id)
                    .and_then(|instance| instance.lifecycle_reservation.clone()))
            })
            .unwrap();
        assert!(
            leftover.is_none(),
            "a failed launch must clear its reservation even after a same-generation status drift"
        );
    }

    /// The typed title names the agent on the first launch only: the commit clears it on disk,
    /// and a copy still holding it (the TUI's row) reloads from disk before its own launch.
    #[test]
    #[serial_test::serial]
    fn the_first_launch_commit_spends_the_agent_name() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let profile = "lifecycle-agent-name";
        let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
        let mut first = Instance::new("first", "/tmp/test");
        first.source_profile = profile.into();
        first.first_launch_names_agent = true;
        storage
            .update(|instances, _groups| {
                instances.push(first.clone());
                Ok(())
            })
            .unwrap();
        first = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.id == first.id)
            .unwrap();
        let mut held_copy = first.clone();

        first
            .acquire_lifecycle_reservation(
                &storage,
                LifecycleOperation::Launch,
                Some(Status::Starting),
                None,
            )
            .unwrap();
        assert!(
            first.first_launch_names_agent,
            "the launch being built still carries it"
        );
        first.commit_lifecycle_launch(&storage, false).unwrap();
        let disk = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.id == first.id)
            .unwrap();
        assert!(!disk.first_launch_names_agent);
        assert!(!first.first_launch_names_agent);

        assert!(held_copy.try_reconcile_from_disk(false).unwrap());
        assert!(
            !held_copy.first_launch_names_agent,
            "a restart from a stale copy must not name the agent again"
        );
    }

    #[test]
    #[serial_test::serial]
    fn lifecycle_launch_commit_keeps_reserved_generation_and_rejects_stale_or_overflowed_tokens() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let storage =
            crate::session::storage::Storage::new_unwatched("lifecycle-launch-commit").unwrap();
        let mut committed = Instance::new("committed", "/tmp/test");
        committed.storage_origin = Some(std::sync::Arc::new(storage.clone()));
        let mut stale = Instance::new("stale", "/tmp/test");
        let mut overflow = Instance::new("overflow", "/tmp/test");
        overflow.lifecycle_generation = u64::MAX;
        storage
            .update(|instances, _groups| {
                instances.extend([committed.clone(), stale.clone(), overflow.clone()]);
                Ok(())
            })
            .unwrap();

        let mut own_reservation = None;
        committed
            .acquire_lifecycle_reservation(
                &storage,
                LifecycleOperation::Launch,
                Some(Status::Starting),
                Some(&mut own_reservation),
            )
            .unwrap();
        let own_reservation =
            own_reservation.expect("successful real CAS must return its exact receipt");
        let reserved_cache = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|row| row.id == committed.id)
            .unwrap();
        let cached_scope = crate::session::LaunchOrigin::capture(&reserved_cache).unwrap();
        let reserved_generation = committed.lifecycle_generation;
        let capture_floor = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_234_567);
        committed.status = Status::Running;
        committed.capture_started_at = Some(capture_floor);
        committed.sandbox_info = Some(super::super::test_helpers::test_sandbox(
            "committed",
            Some("/workspace/committed"),
        ));
        committed.commit_lifecycle_launch(&storage, false).unwrap();
        let disk = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.id == committed.id)
            .unwrap();
        assert_eq!(committed.lifecycle_generation, reserved_generation);
        assert_eq!(disk.lifecycle_generation, committed.lifecycle_generation);
        assert_eq!(disk.status, Status::Running);
        assert_eq!(disk.capture_started_at, Some(capture_floor));
        let own_commit = crate::session::LaunchOrigin::capture(&committed).unwrap();
        assert_eq!(own_reservation.generation(), own_commit.generation());
        assert!(own_reservation.recognizes_published_snapshot(&cached_scope));
        assert!(
            !own_commit.recognizes_published_snapshot(&cached_scope),
            "same counter alone must not translate a different execution plan"
        );
        assert!(
            own_reservation.with_storage(|_, _| Ok(())).is_err(),
            "the earlier actual ACK must not authorize the new canonical plan"
        );
        assert!(
            own_commit.with_storage(|_, _| Ok(())).is_ok(),
            "only the actual committed plan may publish canonical effects"
        );

        stale
            .acquire_lifecycle_reservation(
                &storage,
                LifecycleOperation::Launch,
                Some(Status::Starting),
                None,
            )
            .unwrap();
        let stale_token = stale.lifecycle_generation;
        stale.status = Status::Running;
        stale.capture_started_at =
            Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(9_999_999));
        storage
            .update(|instances, _groups| {
                let peer = instances
                    .iter_mut()
                    .find(|candidate| candidate.id == stale.id)
                    .unwrap();
                peer.lifecycle_generation = stale_token + 1;
                peer.status = Status::Stopped;
                peer.capture_started_at = Some(capture_floor);
                Ok(())
            })
            .unwrap();
        let error = stale.commit_lifecycle_launch(&storage, false).unwrap_err();
        assert!(error.to_string().contains("lost its lifecycle reservation"));
        let disk = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.id == stale.id)
            .unwrap();
        assert_eq!(stale.lifecycle_generation, stale_token);
        assert_eq!(disk.lifecycle_generation, stale_token + 1);
        assert_eq!(disk.status, Status::Stopped);
        assert_eq!(
            disk.capture_started_at,
            Some(capture_floor),
            "a stale launch token must not overwrite the winning floor"
        );

        assert!(overflow
            .acquire_lifecycle_reservation(
                &storage,
                LifecycleOperation::Launch,
                Some(Status::Starting),
                None,
            )
            .unwrap_err()
            .to_string()
            .contains("overflow"));
        let disk = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.id == overflow.id)
            .unwrap();
        assert_eq!(overflow.lifecycle_generation, u64::MAX);
        assert_eq!(overflow.status, Status::Idle);
        assert_eq!(disk.lifecycle_generation, u64::MAX);
        assert_eq!(disk.status, Status::Idle);
    }

    #[test]
    fn lifecycle_reservation_excludes_peers_and_uses_generation_as_identity() {
        let now = Utc::now();
        let mut instance = Instance::new("s", "/tmp/x");
        let generation = instance
            .try_acquire_lifecycle_reservation(
                LifecycleOperation::Purge,
                Instance::LIFECYCLE_RESERVATION_TTL,
                now,
            )
            .expect("first operation acquires");

        for contender in [
            LifecycleOperation::Launch,
            LifecycleOperation::Stop,
            LifecycleOperation::Purge,
            LifecycleOperation::Restore,
            LifecycleOperation::Trash,
        ] {
            assert_eq!(
                instance.try_acquire_lifecycle_reservation(
                    contender,
                    Instance::LIFECYCLE_RESERVATION_TTL,
                    now + chrono::Duration::seconds(1),
                ),
                Err(LifecycleReservationError::Busy(LifecycleOperation::Purge)),
                "{contender:?} must not replace a live peer reservation",
            );
        }
        assert!(!instance
            .release_lifecycle_reservation_if_owned(LifecycleOperation::Purge, generation + 1,));
        assert!(instance.lifecycle_reservation_is_owned(LifecycleOperation::Purge, generation));
        assert!(
            instance.release_lifecycle_reservation_if_owned(LifecycleOperation::Purge, generation)
        );
    }

    #[test]
    fn expired_lifecycle_reservation_is_recoverable_without_reusing_generation() {
        let ttl = Instance::LIFECYCLE_RESERVATION_TTL;
        let now = Utc::now();
        let mut instance = Instance::new("s", "/tmp/x");
        let old_generation = instance
            .try_acquire_lifecycle_reservation(
                LifecycleOperation::Purge,
                ttl,
                now - ttl - chrono::Duration::seconds(1),
            )
            .expect("first operation acquires");
        let new_generation = instance
            .try_acquire_lifecycle_reservation(LifecycleOperation::Restore, ttl, now)
            .expect("expired lease can be replaced");

        assert!(new_generation > old_generation);
        assert!(!instance
            .release_lifecycle_reservation_if_owned(LifecycleOperation::Purge, old_generation,));
        assert!(
            instance.lifecycle_reservation_is_owned(LifecycleOperation::Restore, new_generation,)
        );
    }
    /// The reconciler skips a row a purge holds, so the next tick does not
    /// respawn the runner the purge just settled from the registry. A
    /// reservation that has expired, or that another operation owns, is not a
    /// purge and must still let the row resume.
    #[test]
    fn only_a_fresh_purge_reservation_marks_the_row_as_purging() {
        let now = Utc::now();
        let mut instance = Instance::new("Reconciler", "/tmp/reconciler");
        assert!(
            !instance.is_purge_reserved(now),
            "a row with no reservation is resumable"
        );

        instance
            .try_acquire_lifecycle_reservation(
                LifecycleOperation::Launch,
                Instance::LIFECYCLE_RESERVATION_TTL,
                now,
            )
            .unwrap();
        assert!(
            !instance.is_purge_reserved(now),
            "a launch reservation is not a purge and must not hide the row"
        );

        assert!(
            instance.release_lifecycle_reservation_if_owned(
                LifecycleOperation::Launch,
                instance.lifecycle_generation,
            ),
            "the launch reservation is released before the purge takes the row"
        );
        instance
            .try_acquire_lifecycle_reservation(
                LifecycleOperation::Purge,
                Instance::LIFECYCLE_RESERVATION_TTL,
                now,
            )
            .unwrap();
        assert!(
            instance.is_purge_reserved(now),
            "a fresh purge reservation must keep the reconciler off this row"
        );

        let later = now + Instance::LIFECYCLE_RESERVATION_TTL + chrono::Duration::seconds(1);
        assert!(
            !instance.is_purge_reserved(later),
            "an expired purge reservation releases the row again"
        );
    }
}
