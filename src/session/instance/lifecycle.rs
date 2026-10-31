//! Cross-process lifecycle reservations: the generation-stamped lock that
//! keeps two aoe processes from launching or killing the same session.

use super::*;

/// One durable ownership protocol for every session lifecycle transition.
///
/// A transition acquires the per-instance lifecycle flock, then records a
/// fresh generation under `Storage::update`. Terminal launch is the ordered
/// exception: it first takes the app-global per-session title flock so title
/// writers and launch cannot derive different tmux names. A whole-session
/// reservation stays held through hooks, external side effects, and the
/// exact-generation commit; callers may release outer flocks for reentrant hooks.
/// Targeted tool Stop retains both flocks and advances the durable generation
/// without a whole-session reservation; its original cohort journal owns recovery.
/// `status` is presentation state and never proves ownership.
///
/// Ordinary crashed reservations expire after their TTL. Explicit terminal
/// handoff debt requires matching-intent recovery, which advances generation
/// before taking over so late results cannot mutate or clear its replacement.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum LifecycleOperation {
    Launch,
    Capture,
    Stop,
    Purge,
    Restore,
    Trash,
}

impl LifecycleOperation {
    pub(crate) fn busy_reason(self) -> String {
        format!("busy with lifecycle operation {self:?}")
    }

    pub(crate) fn already_in_progress_reason(self) -> String {
        format!("lifecycle operation {self:?} is already in progress")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleReservationError {
    Busy(LifecycleOperation),
    GenerationOverflow,
    Superseded,
}

impl std::fmt::Display for LifecycleReservationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy(operation) => f.write_str(&operation.already_in_progress_reason()),
            Self::GenerationOverflow => f.write_str("lifecycle generation overflow"),
            Self::Superseded => f.write_str("lifecycle reservation was superseded"),
        }
    }
}

impl std::error::Error for LifecycleReservationError {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct LifecycleReservation {
    pub op: LifecycleOperation,
    pub generation: u64,
    pub at: DateTime<Utc>,
}

impl Instance {
    /// Longer than any bounded hook, teardown, or worktree move. A crashed
    /// owner cannot retain the reservation forever; a late owner is still
    /// harmless because every commit is generation-checked.
    pub const LIFECYCLE_RESERVATION_TTL: chrono::Duration = chrono::Duration::minutes(10);

    fn next_available_lifecycle_generation(
        &self,
        ttl: chrono::Duration,
        now: DateTime<Utc>,
    ) -> Result<u64, LifecycleReservationError> {
        if self.terminal_handoff.is_some() {
            return Err(LifecycleReservationError::Busy(LifecycleOperation::Launch));
        }
        if let Some(reservation) = self.lifecycle_reservation.as_ref().filter(|reservation| {
            reservation.generation == self.lifecycle_generation && (now - reservation.at) < ttl
        }) {
            return Err(LifecycleReservationError::Busy(reservation.op));
        }
        self.lifecycle_generation
            .checked_add(1)
            .ok_or(LifecycleReservationError::GenerationOverflow)
    }

    /// Acquire exclusive durable ownership of the next lifecycle generation.
    ///
    /// Even a reservation for the same operation belongs to a peer: operation
    /// kind is not an identity. A caller that already owns a reservation must
    /// retain its returned generation and use
    /// [`Self::lifecycle_reservation_is_owned`] rather than reacquiring by kind.
    pub fn try_acquire_lifecycle_reservation(
        &mut self,
        operation: LifecycleOperation,
        ttl: chrono::Duration,
        now: DateTime<Utc>,
    ) -> Result<u64, LifecycleReservationError> {
        let generation = self.next_available_lifecycle_generation(ttl, now)?;
        self.lifecycle_generation = generation;
        self.failed_purge = None;
        self.lifecycle_reservation = Some(LifecycleReservation {
            op: operation,
            generation,
            at: now,
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

    /// Whether a failed creation still owns its durable row, so its rollback
    /// may run. Launch reservation ownership is not usable as proof: every
    /// post-launch failure releases that reservation first. The row is ours
    /// when its durable generation is the one the creation published and any
    /// reservation still present belongs to that same generation; a newer
    /// generation means another owner took it, and the rollback is refused.
    pub fn creation_rollback_is_owned(&self, generation: u64) -> bool {
        self.lifecycle_generation == generation
            && self
                .lifecycle_reservation
                .as_ref()
                .is_none_or(|reservation| reservation.generation == generation)
    }

    pub fn has_fresh_lifecycle_reservation(&self, now: DateTime<Utc>) -> bool {
        matches!(
            &self.lifecycle_reservation,
            Some(reservation)
                if reservation.generation == self.lifecycle_generation
                    && (now - reservation.at) < Self::LIFECYCLE_RESERVATION_TTL
        )
    }

    pub(crate) fn has_fresh_launch_reservation(&self, now: DateTime<Utc>) -> bool {
        self.has_fresh_lifecycle_reservation(now)
            && self
                .lifecycle_reservation
                .as_ref()
                .is_some_and(|reservation| reservation.op == LifecycleOperation::Launch)
    }

    pub fn release_lifecycle_reservation_if_owned(
        &mut self,
        operation: LifecycleOperation,
        generation: u64,
    ) -> bool {
        if self.lifecycle_reservation_is_owned(operation, generation) {
            self.lifecycle_reservation = None;
            true
        } else {
            false
        }
    }

    /// Consume an exact native stop observation after original execution retirement.
    /// The caller validates the structured target and compares its observed registry stop token.
    pub(crate) fn resume_structured_after_retirement(
        &mut self,
        observed_generation: u64,
    ) -> Result<u64, LifecycleReservationError> {
        if self.lifecycle_generation != observed_generation {
            return Err(LifecycleReservationError::Superseded);
        }
        let stopped =
            if self.lifecycle_reservation_is_owned(LifecycleOperation::Stop, observed_generation) {
                self.lifecycle_reservation.take()
            } else {
                None
            };
        let generation = match self.try_acquire_lifecycle_reservation(
            LifecycleOperation::Launch,
            Self::LIFECYCLE_RESERVATION_TTL,
            Utc::now(),
        ) {
            Ok(generation) => generation,
            Err(error) => {
                if let Some(stopped) = stopped {
                    self.lifecycle_reservation = Some(stopped);
                }
                return Err(error);
            }
        };
        self.status = Status::Idle;
        self.idle_dormant_since = None;
        self.idle_entered_at = None;
        self.last_error = None;
        self.last_error_evidence = None;
        self.release_lifecycle_reservation_if_owned(LifecycleOperation::Launch, generation);
        Ok(generation)
    }

    /// Clear a crashed owner's expired reservation. The generation is
    /// deliberately retained as the monotonic cache/result revision.
    pub fn clear_expired_lifecycle_reservation(
        &mut self,
        ttl: chrono::Duration,
        now: DateTime<Utc>,
    ) -> bool {
        if self.terminal_handoff.is_some() {
            return false;
        }
        if matches!(
            &self.lifecycle_reservation,
            Some(reservation)
                if reservation.generation == self.lifecycle_generation
                    && (now - reservation.at) >= ttl
        ) {
            self.lifecycle_reservation = None;
            true
        } else {
            false
        }
    }

    pub(crate) fn commit_lifecycle_launch(
        &mut self,
        storage: &dyn crate::session::SessionStore,
        generation: u64,
        restart: bool,
    ) -> Result<()> {
        let committed = storage.update(|instances, _groups| {
            let Some(stored) = instances.iter_mut().find(|instance| instance.id == self.id) else {
                return Ok(false);
            };
            if !stored.lifecycle_reservation_is_owned(LifecycleOperation::Launch, generation) {
                return Ok(false);
            }
            stored.status = self.status;
            stored.idle_entered_at = self.idle_entered_at;
            stored.last_accessed_at = self.last_accessed_at;
            stored.sandbox_info = self.sandbox_info.clone();
            stored.capture_started_at = self.capture_started_at;
            stored.started_at = self.started_at;
            stored.launch_executable = self.launch_executable.clone();
            stored.active_execution = self.active_execution.clone();
            if restart && stored.agent_session_id == self.agent_session_id {
                stored.resume_probe_failed_sid = self.resume_probe_failed_sid.clone();
            }
            stored.first_launch_names_agent = false;
            stored.release_lifecycle_reservation_if_owned(LifecycleOperation::Launch, generation);
            Ok(true)
        })?;
        anyhow::ensure!(committed, LifecycleReservationError::Superseded);
        self.lifecycle_reservation = None;
        self.first_launch_names_agent = false;
        Ok(())
    }

    pub(super) fn release_indeterminate_launch(
        &mut self,
        storage: &dyn crate::session::SessionStore,
        generation: u64,
    ) -> Result<()> {
        let status = storage.update(|rows, _| {
            let row = rows
                .iter_mut()
                .find(|row| row.id == self.id)
                .ok_or(LifecycleReservationError::Superseded)?;
            anyhow::ensure!(
                row.lifecycle_reservation_is_owned(LifecycleOperation::Launch, generation),
                LifecycleReservationError::Superseded
            );
            if row.active_execution == self.active_execution {
                row.capture_started_at = self.capture_started_at;
                row.started_at = self.started_at;
                row.launch_executable = self.launch_executable.clone();
            }
            row.release_lifecycle_reservation_if_owned(LifecycleOperation::Launch, generation);
            Ok(row.status)
        })?;
        self.lifecycle_reservation = None;
        self.status = status;
        Ok(())
    }

    /// Publish the outcome of a creation that released its reservation when
    /// its registration window closed.
    ///
    /// A structured creation has nothing left to fence once the row, its
    /// worktree, branch, sandbox and scratch dir are durable, so it drops the
    /// reservation rather than hold a purge off for the whole ACP handshake,
    /// a wait with no durable effect. What remains after the release is
    /// ownership proof, and the durable generation is exactly that: every peer
    /// transition mints a newer one, so a mismatch means an archive, trash,
    /// restore, stop or purge took the row and this write is refused instead of
    /// stamping a status onto it. The reservation release is a no-op here, the
    /// creation having already given it up, and is kept so one commit serves
    /// both shapes.
    pub(crate) fn commit_created_launch(
        &mut self,
        storage: &dyn crate::session::SessionStore,
        generation: u64,
        status: Status,
    ) -> Result<()> {
        let idle = status == Status::Idle;
        let committed = storage.update(|instances, _groups| {
            let Some(stored) = instances.iter_mut().find(|instance| instance.id == self.id) else {
                return Ok(false);
            };
            if stored.lifecycle_generation != generation {
                return Ok(false);
            }
            stored.status = status;
            stored.idle_entered_at = if idle { self.idle_entered_at } else { None };
            stored.last_accessed_at = self.last_accessed_at;
            stored.sandbox_info = self.sandbox_info.clone();
            stored.capture_started_at = self.capture_started_at;
            stored.active_execution = self.active_execution.clone();
            stored.release_lifecycle_reservation_if_owned(LifecycleOperation::Launch, generation);
            Ok(true)
        })?;
        anyhow::ensure!(committed, LifecycleReservationError::Superseded);
        self.lifecycle_reservation = None;
        self.status = status;
        self.idle_entered_at = if idle { self.idle_entered_at } else { None };
        Ok(())
    }

    /// Settle a created launch that no longer owns a reservation, keeping a
    /// failure to commit from masking the refusal that caused it.
    fn stamp_created_launch_status(
        &mut self,
        storage: &dyn crate::session::SessionStore,
        generation: u64,
        status: Status,
    ) {
        if let Err(error) = self.commit_created_launch(storage, generation, status) {
            tracing::warn!(
                target: "session.store",
                session = %self.id,
                "could not settle a created launch as {status:?}: {error:#}"
            );
        }
    }

    /// Whether the row still carries the generation a creation published.
    fn creation_generation_is_current(
        &self,
        storage: &dyn crate::session::SessionStore,
        generation: u64,
    ) -> Result<bool> {
        Ok(storage
            .load()?
            .iter()
            .find(|instance| instance.id == self.id)
            .is_some_and(|stored| stored.lifecycle_generation == generation))
    }

    /// Invalidate old auxiliary continuations without claiming whole-session ownership.
    /// The caller retains title and lifecycle flocks through the resource effect.
    pub(crate) fn advance_auxiliary_generation_in(
        &mut self,
        storage: &dyn crate::session::SessionStore,
    ) -> Result<u64> {
        storage.check_available()?;
        let now = Utc::now();
        let generation = storage.update(|instances, _groups| {
            let stored = instances
                .iter_mut()
                .find(|instance| instance.id == self.id)
                .ok_or(LifecycleReservationError::Superseded)?;
            anyhow::ensure!(
                stored.lifecycle_generation == self.lifecycle_generation
                    && stored.title == self.title
                    && stored.source_profile == self.source_profile
                    && self.source_profile == storage.storage().profile(),
                LifecycleReservationError::Superseded
            );
            stored.ensure_startable()?;
            anyhow::ensure!(
                !matches!(stored.status, Status::Creating | Status::Deleting),
                LifecycleReservationError::Superseded
            );
            let generation =
                stored.next_available_lifecycle_generation(Self::LIFECYCLE_RESERVATION_TTL, now)?;
            stored.lifecycle_generation = generation;
            // Availability above rejects every fresh current-generation owner.
            // Only an expired or already-inert marker can be discarded here.
            stored.lifecycle_reservation = None;
            Ok(generation)
        })?;
        self.lifecycle_generation = generation;
        self.lifecycle_reservation = None;
        Ok(generation)
    }

    pub(crate) fn acquire_lifecycle_reservation(
        &mut self,
        storage: &dyn crate::session::SessionStore,
        operation: LifecycleOperation,
        status: Option<Status>,
    ) -> Result<u64> {
        let now = Utc::now();
        let mut acquired = None;
        storage.update(|instances, _groups| {
            let Some(stored) = instances.iter_mut().find(|instance| instance.id == self.id) else {
                return Ok(());
            };
            let generation = stored
                .try_acquire_lifecycle_reservation(operation, Self::LIFECYCLE_RESERVATION_TTL, now)
                .map_err(|error| match error {
                    LifecycleReservationError::Busy(holder) => anyhow::Error::new(error)
                        .context(format!("session {} is {}", self.id, holder.busy_reason())),
                    LifecycleReservationError::GenerationOverflow => anyhow::Error::new(error)
                        .context(format!("session {} lifecycle generation overflow", self.id)),
                    LifecycleReservationError::Superseded => anyhow::Error::new(error),
                })?;
            if let Some(status) = status {
                stored.status = status;
                if status != Status::Idle {
                    stored.idle_entered_at = None;
                }
            }
            acquired = Some((generation, stored.lifecycle_reservation.clone()));
            Ok(())
        })?;
        let Some((generation, reservation)) = acquired else {
            anyhow::bail!("session {} no longer exists", self.id);
        };
        self.lifecycle_generation = generation;
        self.failed_purge = None;
        self.lifecycle_reservation = reservation;
        if let Some(status) = status {
            self.status = status;
            if status != Status::Idle {
                self.idle_entered_at = None;
            }
        }
        Ok(generation)
    }

    pub(crate) fn finish_lifecycle_status(
        &mut self,
        operation: LifecycleOperation,
        generation: u64,
        status: Status,
    ) -> bool {
        if !self.release_lifecycle_reservation_if_owned(operation, generation) {
            return false;
        }
        self.status = status;
        if status != Status::Idle {
            self.idle_entered_at = None;
        }
        true
    }

    pub(super) fn commit_lifecycle_status(
        &mut self,
        storage: &dyn crate::session::SessionStore,
        operation: LifecycleOperation,
        generation: u64,
        status: Status,
    ) -> Result<()> {
        let committed = storage.update(|instances, _groups| {
            let Some(stored) = instances.iter_mut().find(|instance| instance.id == self.id) else {
                return Ok(false);
            };
            Ok(stored.finish_lifecycle_status(operation, generation, status))
        })?;
        anyhow::ensure!(committed, LifecycleReservationError::Superseded);
        self.lifecycle_reservation = None;
        self.status = status;
        if status != Status::Idle {
            self.idle_entered_at = None;
        }
        Ok(())
    }

    pub(crate) fn release_lifecycle_reservation(
        &mut self,
        storage: &dyn crate::session::SessionStore,
        operation: LifecycleOperation,
    ) -> Result<()> {
        let generation = self.lifecycle_generation;
        let released = storage.update(|instances, _groups| {
            let Some(stored) = instances.iter_mut().find(|instance| instance.id == self.id) else {
                return Ok(false);
            };
            Ok(stored.release_lifecycle_reservation_if_owned(operation, generation))
        })?;
        anyhow::ensure!(
            released,
            "session {} disappeared or lost its lifecycle reservation before release",
            self.id
        );
        self.lifecycle_reservation = None;
        Ok(())
    }

    /// Reacquire launch locks after user hooks, preserving the global
    /// title-before-lifecycle order and failing the reservation consistently.
    pub(crate) fn reacquire_launch_locks_after_hooks(
        &mut self,
        storage: &dyn crate::session::SessionStore,
        generation: u64,
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
                self.fail_reserved_launch(storage, generation, &error, false);
                return Err(error);
            }
        };
        let lifecycle_lock = match storage
            .storage()
            .acquire_instance_lifecycle_lock(&self.id)
            .context("failed to reacquire instance lifecycle lock after hooks")
        {
            Ok(lock) => lock,
            Err(error) => {
                self.fail_reserved_launch(storage, generation, &error, false);
                return Err(error);
            }
        };
        if let Err(error) = self.reconcile_from_store(storage) {
            self.fail_reserved_launch(storage, generation, &error, false);
            return Err(error);
        }
        // The hooks ran without the lifecycle lock, so a peer may have archived or trashed the
        // row meanwhile. The reconcile above reread it under the lock we just took back.
        if let Err(blocked) = self.ensure_startable() {
            self.release_blocked_launch(storage, generation);
            return Err(blocked.into());
        }
        if let Err(error) = hook_result {
            self.fail_reserved_launch(storage, generation, &error, false);
            return Err(error);
        }
        self.ensure_reservation_current_or_fail(storage, generation)?;
        Ok((title_lock, lifecycle_lock))
    }

    /// Reacquire the launch locks after the ACP handshake, for a creation
    /// whose reservation its registration window already released.
    ///
    /// The handshake is a wait, not a durable effect, and it is fenced by the
    /// supervisor instead: `acp::supervisor::launch::admit_durable_launch`
    /// re-reads the row under this same lifecycle lock before and after the
    /// handshake and retires the runner when a peer took it meanwhile. What
    /// this adds is the settle, which must not stamp a status onto a row a
    /// peer now owns, hence the generation the creation published in place of
    /// the reservation it gave up.
    pub(crate) fn reacquire_created_launch_locks_after_handshake(
        &mut self,
        storage: &dyn crate::session::SessionStore,
        generation: u64,
        handshake: Result<()>,
    ) -> Result<(
        crate::session::storage::StorageFlock,
        crate::session::storage::StorageFlock,
    )> {
        let title_lock = match crate::session::storage::acquire_session_title_lock(&self.id) {
            Ok(lock) => lock,
            Err(error) => {
                self.stamp_created_launch_status(storage, generation, Status::Error);
                return Err(error).context("failed to reacquire instance title lock after hooks");
            }
        };
        let lifecycle_lock = match storage.storage().acquire_instance_lifecycle_lock(&self.id) {
            Ok(lock) => lock,
            Err(error) => {
                self.stamp_created_launch_status(storage, generation, Status::Error);
                return Err(error)
                    .context("failed to reacquire instance lifecycle lock after hooks");
            }
        };
        // The supervisor's admission ran without these flocks, so reread the
        // row under the one just taken back before deciding anything from it.
        if let Err(error) = self.reconcile_from_store(storage) {
            self.stamp_created_launch_status(storage, generation, Status::Error);
            return Err(error);
        }
        // A structured launch owns no pane, so a refusal settles as stopped
        // where `release_blocked_launch` would have kept a live pane idle.
        if let Err(blocked) = self.ensure_startable() {
            self.stamp_created_launch_status(storage, generation, Status::Stopped);
            return Err(blocked.into());
        }
        if let Err(error) = handshake {
            self.stamp_created_launch_status(storage, generation, Status::Error);
            return Err(error);
        }
        anyhow::ensure!(
            self.creation_generation_is_current(storage, generation)?,
            LifecycleReservationError::Superseded
        );
        Ok((title_lock, lifecycle_lock))
    }

    /// A peer archived or trashed the row while hooks ran: drop the launch reservation
    /// without stamping an error, since the refusal is not a launch failure.
    fn release_blocked_launch(
        &mut self,
        storage: &dyn crate::session::SessionStore,
        generation: u64,
    ) {
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
            self.commit_lifecycle_status(storage, LifecycleOperation::Launch, generation, status)
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
        storage: &dyn crate::session::SessionStore,
        operation: LifecycleOperation,
        generation: u64,
    ) -> Result<bool> {
        Ok(storage
            .load()?
            .iter()
            .find(|instance| instance.id == self.id)
            .is_some_and(|stored| stored.lifecycle_reservation_is_owned(operation, generation)))
    }

    fn reservation_is_current(
        &self,
        storage: &dyn crate::session::SessionStore,
        generation: u64,
    ) -> Result<bool> {
        self.lifecycle_reservation_is_current(storage, LifecycleOperation::Launch, generation)
    }

    fn ensure_reservation_current(
        &self,
        storage: &dyn crate::session::SessionStore,
        generation: u64,
    ) -> Result<()> {
        if self.reservation_is_current(storage, generation)? {
            return Ok(());
        }
        Err(LifecycleReservationError::Superseded.into())
    }

    fn ensure_reservation_current_or_fail(
        &mut self,
        storage: &dyn crate::session::SessionStore,
        generation: u64,
    ) -> Result<()> {
        match self.ensure_reservation_current(storage, generation) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.fail_reserved_launch(storage, generation, &error, false);
                Err(error)
            }
        }
    }

    pub(crate) fn fail_reserved_launch(
        &mut self,
        storage: &dyn crate::session::SessionStore,
        generation: u64,
        error: &anyhow::Error,
        cleanup_pane: bool,
    ) {
        if !self
            .reservation_is_current(storage, generation)
            .unwrap_or(false)
        {
            return;
        }
        let scope_failure = if cleanup_pane {
            if let Err(error) = self.kill_clean_locked() {
                tracing::debug!(target: "session.lifecycle", session = %self.id, %error, "pane cleanup failed");
            }
            self.release_scope().err()
        } else {
            None
        };
        self.last_error_evidence = None;
        self.last_error = Some(
            if let Some(refusal) = error.downcast_ref::<ManagedResumeRefused>() {
                tracing::warn!(
                    target: "session.store",
                    session = %self.id,
                    cause = refusal.0.as_str(),
                    error = %format_args!("{error:#}"),
                    "managed resume refused"
                );
                let message = refusal.0.message();
                if let Some(scope) = scope_failure.as_ref() {
                    tracing::warn!(
                        target: "session.store",
                        session = %self.id,
                        error = %format_args!("{scope:#}"),
                        "scope retirement remains unproven after managed resume refusal"
                    );
                    format!("{message}\nScope retirement remains unproven.")
                } else {
                    message.into()
                }
            } else {
                match scope_failure {
                    Some(scope) => {
                        format!("{error:#}; scope retirement remains unproven: {scope:#}")
                    }
                    None => format!("{error:#}"),
                }
            },
        );
        if let Err(commit_error) = self.commit_lifecycle_status(
            storage,
            LifecycleOperation::Launch,
            generation,
            Status::Error,
        ) {
            tracing::warn!(target: "session.lifecycle", session = %self.id, error = %commit_error,
                "launch-failure status commit failed"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[serial_test::serial]
    fn managed_refusal_retains_cleanup_diagnostics_without_public_reflection() {
        use crate::process::worker_registry::ScopeOwnership;

        for cleanup_pane in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let _home = crate::session::test_support::isolate_home(root.path());
            let _env = crate::session::test_support::EnvGuard::unset(&["TMUX", "TMUX_PANE"])
                .and_set("AOE_TMUX_SOCKET", root.path().join("tmux.sock"));
            let storage = crate::session::Storage::new_unwatched("scope-diagnostic").unwrap();
            let mut row = Instance::new("scope diagnostic", root.path().to_str().unwrap());
            row.source_profile = storage.profile().into();
            row.terminal_scopes = Some(vec![ScopeOwnership::Owned {
                unit: "invalid-private-scope".into(),
            }]);
            let scope_diagnostic = format!("{:#}", row.release_scope().unwrap_err());
            storage
                .update(|rows, _| {
                    rows.push(row.clone());
                    Ok(())
                })
                .unwrap();
            let generation = row
                .acquire_lifecycle_reservation(
                    &storage,
                    LifecycleOperation::Launch,
                    Some(Status::Starting),
                )
                .unwrap();
            let cause = ManagedResumeCause::ExecutionContextUnattested;
            let error =
                anyhow::anyhow!("private-primary-diagnostic").context(ManagedResumeRefused(cause));
            let logs = crate::session::test_support::LogCapture::start();

            row.fail_reserved_launch(&storage, generation, &error, cleanup_pane);

            let public = row.last_error.as_deref().unwrap();
            assert!(!public.contains(&scope_diagnostic), "{public}");
            assert!(!public.contains("private-primary-diagnostic"), "{public}");
            assert!(public.starts_with(cause.message()), "{public}");
            let stored = storage.load().unwrap().remove(0);
            assert_eq!(stored.status, Status::Error);
            assert!(stored.lifecycle_reservation.is_none());
            let diagnostics = logs.contents();
            assert_eq!(
                diagnostics.matches("private-primary-diagnostic").count(),
                1,
                "{diagnostics}"
            );
            assert!(diagnostics.contains(cause.as_str()), "{diagnostics}");
            assert_eq!(
                diagnostics.contains(&scope_diagnostic),
                cleanup_pane,
                "{diagnostics}"
            );
            if cleanup_pane {
                assert!(
                    diagnostics
                        .lines()
                        .any(|line| line.contains(&scope_diagnostic) && line.contains("error=")),
                    "{diagnostics}"
                );
            }
            assert!(diagnostics.contains(&row.id), "{diagnostics}");
        }
    }

    #[test]
    fn observed_native_resume_is_generation_owned_and_preserves_failed_reservations() {
        for case in ["resume", "superseded", "overflow", "busy"] {
            let mut row = Instance::new(case, "/tmp/native-resume");
            row.view = crate::session::View::Structured;
            row.status = Status::Stopped;
            row.mark_idle_dormant();
            row.lifecycle_generation = if case == "overflow" { u64::MAX } else { 7 };
            row.lifecycle_reservation = Some(LifecycleReservation {
                op: if case == "busy" {
                    LifecycleOperation::Capture
                } else {
                    LifecycleOperation::Stop
                },
                generation: row.lifecycle_generation,
                at: Utc::now(),
            });
            let before = serde_json::to_value(&row).unwrap();
            let observed = if case == "superseded" {
                6
            } else {
                row.lifecycle_generation
            };
            let result = row.resume_structured_after_retirement(observed);
            if case == "resume" {
                assert_eq!(result.unwrap(), 8);
                assert_eq!(row.status, Status::Idle);
                assert!(!row.is_idle_dormant());
                assert!(row.lifecycle_reservation.is_none());
            } else {
                assert!(result.is_err(), "{case}");
                assert_eq!(serde_json::to_value(&row).unwrap(), before, "{case}");
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn auxiliary_generation_preserves_state_and_rejects_stale_identity() {
        let _home = crate::session::test_support::isolate_app_dir();
        let storage = crate::session::Storage::new_unwatched("auxiliary-generation").unwrap();
        for marker in [None, Some(false), Some(true)] {
            let mut row = Instance::new("auxiliary", "/tmp/test");
            row.source_profile = storage.profile().into();
            row.lifecycle_generation = 7;
            row.status = Status::Idle;
            row.idle_entered_at = Some(Utc::now() - chrono::Duration::minutes(5));
            row.lifecycle_reservation = marker.map(|inert| LifecycleReservation {
                op: LifecycleOperation::Stop,
                generation: if inert { 6 } else { 7 },
                at: if inert {
                    Utc::now()
                } else {
                    Utc::now() - Instance::LIFECYCLE_RESERVATION_TTL - chrono::Duration::seconds(1)
                },
            });
            storage
                .update(|rows, _| {
                    *rows = vec![row.clone()];
                    Ok(())
                })
                .unwrap();
            let mut expected = serde_json::to_value(&row).unwrap();
            expected["lifecycle_generation"] = 8.into();
            expected
                .as_object_mut()
                .unwrap()
                .remove("lifecycle_reservation");
            let mut stale = row.clone();
            assert_eq!(row.advance_auxiliary_generation_in(&storage).unwrap(), 8);
            assert!(row.lifecycle_reservation.is_none());
            assert_eq!(serde_json::to_value(&row).unwrap(), expected);
            let committed = storage.load().unwrap().remove(0);
            assert_eq!(serde_json::to_value(&committed).unwrap(), expected);
            let bytes = std::fs::read(storage.sessions_path()).unwrap();
            assert_eq!(
                stale
                    .advance_auxiliary_generation_in(&storage)
                    .unwrap_err()
                    .downcast_ref::<LifecycleReservationError>(),
                Some(&LifecycleReservationError::Superseded)
            );
            for mismatch in ["title", "profile", "missing"] {
                stale = committed.clone();
                match mismatch {
                    "title" => stale.title.push_str("-peer"),
                    "profile" => stale.source_profile = "other".into(),
                    _ => stale.id.push_str("-missing"),
                }
                assert_eq!(
                    stale
                        .advance_auxiliary_generation_in(&storage)
                        .unwrap_err()
                        .downcast_ref::<LifecycleReservationError>(),
                    Some(&LifecycleReservationError::Superseded)
                );
                assert_eq!(std::fs::read(storage.sessions_path()).unwrap(), bytes);
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn auxiliary_generation_refuses_fresh_owners_overflow_and_valid_failed_purge() {
        let _home = crate::session::test_support::isolate_app_dir();
        let storage = crate::session::Storage::new_unwatched("auxiliary-refusals").unwrap();
        for operation in [
            LifecycleOperation::Launch,
            LifecycleOperation::Capture,
            LifecycleOperation::Stop,
            LifecycleOperation::Purge,
            LifecycleOperation::Restore,
            LifecycleOperation::Trash,
        ] {
            let mut row = Instance::new("peer owner", "/tmp/test");
            row.source_profile = storage.profile().into();
            row.lifecycle_generation = 7;
            row.lifecycle_reservation = Some(LifecycleReservation {
                op: operation,
                generation: 7,
                at: Utc::now(),
            });
            storage
                .update(|rows, _| {
                    *rows = vec![row.clone()];
                    Ok(())
                })
                .unwrap();
            let bytes = std::fs::read(storage.sessions_path()).unwrap();
            assert_eq!(
                row.advance_auxiliary_generation_in(&storage)
                    .unwrap_err()
                    .downcast_ref::<LifecycleReservationError>(),
                Some(&LifecycleReservationError::Busy(operation))
            );
            assert_eq!(row.lifecycle_generation, 7);
            assert_eq!(std::fs::read(storage.sessions_path()).unwrap(), bytes);
        }
        let mut row = Instance::new("overflow", "/tmp/test");
        row.source_profile = storage.profile().into();
        row.lifecycle_generation = u64::MAX;
        storage
            .update(|rows, _| {
                *rows = vec![row.clone()];
                Ok(())
            })
            .unwrap();
        let bytes = std::fs::read(storage.sessions_path()).unwrap();
        assert_eq!(
            row.advance_auxiliary_generation_in(&storage)
                .unwrap_err()
                .downcast_ref::<LifecycleReservationError>(),
            Some(&LifecycleReservationError::GenerationOverflow)
        );
        assert_eq!(row.lifecycle_generation, u64::MAX);
        assert_eq!(std::fs::read(storage.sessions_path()).unwrap(), bytes);
        row.lifecycle_generation = 4;
        row.trash();
        row.record_failed_purge(true);
        storage
            .update(|rows, _| {
                *rows = vec![row.clone()];
                Ok(())
            })
            .unwrap();
        let bytes = std::fs::read(storage.sessions_path()).unwrap();
        assert_eq!(row.failed_purge_forced(), Some(true));
        assert!(row.advance_auxiliary_generation_in(&storage).is_err());
        assert_eq!(row.lifecycle_generation, 4);
        assert_eq!(row.failed_purge_forced(), Some(true));
        assert_eq!(storage.load().unwrap()[0].failed_purge_forced(), Some(true));
        assert_eq!(std::fs::read(storage.sessions_path()).unwrap(), bytes);
    }

    #[test]
    #[serial_test::serial]
    fn auxiliary_generation_failed_write_leaves_local_and_durable_generation_unchanged() {
        let _home = crate::session::test_support::isolate_app_dir();
        let storage = crate::session::Storage::new_unwatched("auxiliary-write-failure").unwrap();
        let mut row = Instance::new("write failure", "/tmp/test");
        row.source_profile = storage.profile().into();
        storage
            .update(|rows, _| {
                rows.push(row.clone());
                Ok(())
            })
            .unwrap();
        let before = serde_json::to_value(&row).unwrap();
        let bytes = std::fs::read(storage.sessions_path()).unwrap();
        let _failure = storage.fail_writes_for_test();
        assert!(row
            .advance_auxiliary_generation_in(&storage)
            .unwrap_err()
            .to_string()
            .contains("injected sessions write failure"));
        assert_eq!(serde_json::to_value(&row).unwrap(), before);
        assert_eq!(std::fs::read(storage.sessions_path()).unwrap(), bytes);
    }

    #[test]
    #[serial_test::serial]
    fn launch_revalidation_does_not_adopt_a_peer_generation() {
        let _home = crate::session::test_support::isolate_app_dir();
        let storage = crate::session::Storage::new_unwatched("hook-ownership").unwrap();
        for hook_failed in [false, true] {
            let mut instance = Instance::new("hook-owner", "/tmp/test");
            instance.source_profile = storage.profile().into();
            storage
                .update(|rows, _| {
                    rows.push(instance.clone());
                    Ok(())
                })
                .unwrap();
            let generation = instance
                .acquire_lifecycle_reservation(
                    &storage,
                    LifecycleOperation::Launch,
                    Some(Status::Starting),
                )
                .unwrap();
            storage
                .update(|rows, _| {
                    let row = rows.iter_mut().find(|row| row.id == instance.id).unwrap();
                    row.release_lifecycle_reservation_if_owned(
                        LifecycleOperation::Launch,
                        generation,
                    );
                    row.try_acquire_lifecycle_reservation(
                        LifecycleOperation::Launch,
                        Instance::LIFECYCLE_RESERVATION_TTL,
                        Utc::now(),
                    )?;
                    row.title = "peer-owned".into();
                    Ok(())
                })
                .unwrap();
            let peer = std::fs::read(storage.sessions_path()).unwrap();
            let hook_result = if hook_failed {
                Err(anyhow::anyhow!("hook failed"))
            } else {
                Ok(())
            };
            assert!(instance
                .reacquire_launch_locks_after_hooks(&storage, generation, hook_result)
                .is_err());
            assert_eq!(std::fs::read(storage.sessions_path()).unwrap(), peer);
        }
    }

    #[test]
    #[serial_test::serial]
    fn lifecycle_status_commit_releases_the_acquired_generation() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let storage = crate::session::storage::Storage::new_unwatched("lifecycle-lease").unwrap();
        let mut instance = Instance::new("session", "/tmp/test");

        instance
            .acquire_lifecycle_reservation(
                &storage,
                LifecycleOperation::Launch,
                Some(Status::Starting),
            )
            .unwrap_err();
        assert!(storage.load().unwrap().is_empty());
        assert!(instance.lifecycle_reservation.is_none());

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
            )
            .unwrap();
        let generation = instance.lifecycle_generation;
        instance
            .commit_lifecycle_status(
                &storage,
                LifecycleOperation::Launch,
                generation,
                Status::Error,
            )
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
                Some(LifecycleReservation {
                    op: LifecycleOperation::Launch,
                    generation: 1,
                    at: now,
                }),
                1,
                false,
            ),
            (
                "superseded",
                Status::Idle,
                Some(LifecycleReservation {
                    op: LifecycleOperation::Launch,
                    generation: 1,
                    at: now,
                }),
                2,
                true,
            ),
            (
                "expired",
                Status::Idle,
                Some(LifecycleReservation {
                    op: LifecycleOperation::Launch,
                    generation: 1,
                    at: stale,
                }),
                1,
                true,
            ),
            (
                "purge",
                Status::Idle,
                Some(LifecycleReservation {
                    op: LifecycleOperation::Purge,
                    generation: 1,
                    at: now,
                }),
                1,
                false,
            ),
            (
                "restore",
                Status::Stopped,
                Some(LifecycleReservation {
                    op: LifecycleOperation::Restore,
                    generation: 1,
                    at: now,
                }),
                1,
                false,
            ),
            (
                "trash",
                Status::Idle,
                Some(LifecycleReservation {
                    op: LifecycleOperation::Trash,
                    generation: 1,
                    at: now,
                }),
                1,
                false,
            ),
            (
                "capture",
                Status::Running,
                Some(LifecycleReservation {
                    op: LifecycleOperation::Capture,
                    generation: 1,
                    at: now,
                }),
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
            );
            assert_eq!(result.is_ok(), *allowed, "{}", instance.title);
        }

        let leased = &cases[0].0;
        assert!(leased
            .reservation_is_current(&storage, leased.lifecycle_generation)
            .unwrap());
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
        assert!(!leased
            .reservation_is_current(&storage, leased.lifecycle_generation)
            .unwrap());

        let mut busy = Instance::new("busy-leased", "/tmp/test");
        busy.source_profile = profile.to_string();
        busy.status = Status::Starting;
        busy.lifecycle_generation = 1;
        busy.lifecycle_reservation = Some(LifecycleReservation {
            op: LifecycleOperation::Launch,
            generation: 1,
            at: Utc::now(),
        });
        storage
            .update(|instances, _groups| {
                instances.push(busy.clone());
                Ok(())
            })
            .unwrap();

        let began = std::time::Instant::now();
        assert!(matches!(
            busy.stop()
                .unwrap_err()
                .downcast_ref::<LifecycleReservationError>(),
            Some(LifecycleReservationError::Busy(LifecycleOperation::Launch))
        ));
        assert!(began.elapsed() < std::time::Duration::from_secs(1));

        let mut recursive_start = busy.clone();
        let began = std::time::Instant::now();
        assert!(matches!(
            recursive_start
                .start_with_size_opts(None, true)
                .unwrap_err()
                .downcast_ref::<LifecycleReservationError>(),
            Some(LifecycleReservationError::Busy(LifecycleOperation::Launch))
        ));
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

        inst.ensure_reservation_current_or_fail(&storage, reserved_gen)
            .unwrap();
        let error = anyhow::anyhow!("launch failed after status drift");
        inst.fail_reserved_launch(&storage, reserved_gen, &error, false);

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

    /// A rolled-back launch sweeps the session's scope while its reservation
    /// still excludes a replacement. A rollback that keeps the pane, one whose
    /// reservation a peer took over, and a restart's pane teardown do not.
    #[test]
    #[serial_test::serial]
    fn a_rolled_back_launch_releases_its_scope_under_the_reservation() {
        use std::cell::RefCell;
        use std::rc::Rc;

        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let profile = "lifecycle-rollback-scope";
        let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
        let held_at_release = Rc::new(RefCell::new(Vec::new()));
        let seen = Rc::clone(&held_at_release);
        let _probe = crate::process::systemd::release_probe::install(move |id| {
            let held = crate::session::storage::Storage::new_unwatched(profile)
                .unwrap()
                .load()
                .unwrap()
                .into_iter()
                .find(|instance| instance.id == id)
                .and_then(|instance| instance.lifecycle_reservation)
                .map(|reservation| reservation.op);
            seen.borrow_mut().push((id.to_string(), held));
        });
        let reserved = |id: &str| {
            let mut inst = Instance::new(id, "/tmp/test");
            inst.source_profile = profile.to_string();
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
            )
            .unwrap();
            inst
        };
        let error = anyhow::anyhow!("pinned launch exited");

        let mut rolled_back = reserved("rolled-back");
        rolled_back.fail_reserved_launch(&storage, rolled_back.lifecycle_generation, &error, true);
        assert_eq!(
            *held_at_release.borrow(),
            [(rolled_back.id.clone(), Some(LifecycleOperation::Launch))]
        );
        let disk = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|instance| instance.id == rolled_back.id)
            .unwrap();
        assert_eq!(disk.status, Status::Error);
        assert!(disk.lifecycle_reservation.is_none());
        held_at_release.borrow_mut().clear();

        let mut kept_pane = reserved("kept-pane");
        kept_pane.fail_reserved_launch(&storage, kept_pane.lifecycle_generation, &error, false);
        let restarted = reserved("restarted");
        restarted.kill_clean_locked().unwrap();
        let mut superseded = reserved("superseded");
        storage
            .update(|instances, _groups| {
                let stored = instances
                    .iter_mut()
                    .find(|instance| instance.id == superseded.id)
                    .unwrap();
                stored.lifecycle_generation += 1;
                Ok(())
            })
            .unwrap();
        superseded.fail_reserved_launch(&storage, superseded.lifecycle_generation, &error, true);
        assert!(held_at_release.borrow().is_empty());
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
        let mut held_copy = first.clone();
        storage
            .update(|instances, _groups| {
                instances.push(first.clone());
                Ok(())
            })
            .unwrap();

        let generation = first
            .acquire_lifecycle_reservation(
                &storage,
                LifecycleOperation::Launch,
                Some(Status::Starting),
            )
            .unwrap();
        assert!(
            first.first_launch_names_agent,
            "the launch being built still carries it"
        );
        first
            .commit_lifecycle_launch(&storage, generation, false)
            .unwrap();
        let disk = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.id == first.id)
            .unwrap();
        assert!(!disk.first_launch_names_agent);
        assert!(!first.first_launch_names_agent);

        held_copy.reconcile_from_store(&storage).unwrap();
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
        let mut stale = Instance::new("stale", "/tmp/test");
        let mut overflow = Instance::new("overflow", "/tmp/test");
        overflow.lifecycle_generation = u64::MAX;
        storage
            .update(|instances, _groups| {
                instances.extend([committed.clone(), stale.clone(), overflow.clone()]);
                Ok(())
            })
            .unwrap();

        committed
            .acquire_lifecycle_reservation(
                &storage,
                LifecycleOperation::Launch,
                Some(Status::Starting),
            )
            .unwrap();
        let reserved_generation = committed.lifecycle_generation;
        let capture_floor = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_234_567);
        committed.status = Status::Running;
        committed.capture_started_at = Some(capture_floor);
        let launched_at = DateTime::from_timestamp(1_234_567, 0).unwrap();
        committed.started_at = Some(launched_at);
        committed.launch_executable = Some("/opt/agent/bin/claude".into());
        committed
            .commit_lifecycle_launch(&storage, reserved_generation, false)
            .unwrap();
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
        assert_eq!(disk.started_at, Some(launched_at));
        assert_eq!(
            disk.launch_executable.as_deref(),
            Some(std::path::Path::new("/opt/agent/bin/claude"))
        );

        stale
            .acquire_lifecycle_reservation(
                &storage,
                LifecycleOperation::Launch,
                Some(Status::Starting),
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
        let error = stale
            .commit_lifecycle_launch(&storage, stale_token, false)
            .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<LifecycleReservationError>(),
            Some(LifecycleReservationError::Superseded)
        ));
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

        assert!(matches!(
            overflow
                .acquire_lifecycle_reservation(
                    &storage,
                    LifecycleOperation::Launch,
                    Some(Status::Starting)
                )
                .unwrap_err()
                .downcast_ref::<LifecycleReservationError>(),
            Some(LifecycleReservationError::GenerationOverflow)
        ));
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
    fn lifecycle_reservation_roundtrips_and_legacy_rows_default_to_none() {
        let fresh = Instance::new("s", "/tmp/x");
        let fresh_json = serde_json::to_string(&fresh).expect("serialize fresh");
        assert!(!fresh_json.contains("lifecycle_reservation"));
        let parsed: Instance = serde_json::from_str(&fresh_json).expect("parse fresh");
        assert_eq!(parsed.lifecycle_reservation, None);

        let mut instance = Instance::new("s", "/tmp/x");
        let now = Utc::now();
        let generation = instance
            .try_acquire_lifecycle_reservation(
                LifecycleOperation::Purge,
                Instance::LIFECYCLE_RESERVATION_TTL,
                now,
            )
            .expect("free row grants the lease");
        let json = serde_json::to_string(&instance).expect("serialize");
        let back: Instance = serde_json::from_str(&json).expect("round-trip");
        assert_eq!(
            back.lifecycle_reservation,
            Some(LifecycleReservation {
                op: LifecycleOperation::Purge,
                generation,
                at: now,
            })
        );
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
}
