//! Making a pane ready to receive input before a send.

use super::*;

use crate::session::storage::{NativeInputRevision, NativeInputStore, StorageFlock};

/// Outcome of `Instance::ensure_pane_ready`. Callers surface this so the user
/// knows what (if anything) happened on their behalf before a send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnsureReadyOutcome {
    /// Pane was already alive; no action taken.
    AlreadyAlive,
    /// A confirmed dead pane was conditionally removed and started without Restart intent.
    Respawned,
    /// Tmux session did not exist and was started via the resume-fallback
    /// path. Healthy resume and fresh launch both use this outcome;
    /// ambiguous probe failures use `ResumeFailed` instead.
    Started,
    /// Resume failed ambiguously while trying to start or respawn the pane.
    /// The durable sid remains stored for an explicit retry.
    ResumeFailed { sid: String },
    /// Readiness is unknown. No input may be sent on this outcome.
    Indeterminate { cause: LaunchIndeterminateCause },
}

/// Errors `ensure_pane_ready` can return. Separating transient lifecycle
/// states from real tmux failures lets HTTP callers map them to 409 (retry)
/// vs 500 (real failure) instead of lumping everything as a tmux error.
#[derive(Debug)]
pub enum EnsureReadyError {
    /// Instance is mid-lifecycle (Creating/Deleting). Caller should retry.
    Transient(Status),
    /// Instance is structured view-mode (no backing tmux pane); send is not supported.
    StructuredView,
    /// The pane needed a start, but the session is archived or trashed.
    Blocked(StartBlocked),
    /// Underlying tmux operation failed.
    Tmux(anyhow::Error),
}

impl From<anyhow::Error> for EnsureReadyError {
    fn from(error: anyhow::Error) -> Self {
        match error.downcast_ref::<StartBlocked>() {
            Some(blocked) => EnsureReadyError::Blocked(*blocked),
            None => EnsureReadyError::Tmux(error),
        }
    }
}

impl std::fmt::Display for EnsureReadyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EnsureReadyError::Transient(status) => {
                write!(
                    f,
                    "Session is mid-lifecycle ({status:?}); cannot send right now"
                )
            }
            EnsureReadyError::StructuredView => write!(
                f,
                "Acp-mode sessions have no tmux pane; send is not supported"
            ),
            EnsureReadyError::Blocked(blocked) => write!(f, "{blocked}"),
            EnsureReadyError::Tmux(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for EnsureReadyError {}

/// A peer purged the stored row while a caller still held its cached copy.
#[derive(Debug, thiserror::Error)]
#[error("session no longer exists")]
pub struct SessionGone;

#[derive(Debug, thiserror::Error)]
pub(crate) enum NativeInputError {
    #[error(
        "Native target ownership changed or is unavailable; input delivery stopped without retry"
    )]
    Ownership,
    #[error("Agent is not ready for programmatic input; no message was submitted")]
    AgentNotReady,
}

/// Immutable native admission scope shared by one captured physical input route.
pub(crate) struct NativeInputFence {
    store: NativeInputStore,
    instance_id: String,
    created_at: DateTime<Utc>,
    lifecycle_generation: u64,
    agent: bool,
    cache: std::sync::Mutex<NativeInputCache>,
}

#[derive(Default)]
struct NativeInputCache {
    witness: Option<NativeInputWitness>,
    invalidated: bool,
}

struct NativeInputWitness {
    revision: NativeInputRevision,
    row: Instance,
}

impl std::fmt::Debug for NativeInputFence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeInputFence")
            .field("profile", &self.store.profile())
            .field("instance_id", &self.instance_id)
            .field("created_at", &self.created_at)
            .field("lifecycle_generation", &self.lifecycle_generation)
            .field("agent", &self.agent)
            .finish_non_exhaustive()
    }
}

impl NativeInputFence {
    pub(crate) fn capture(
        profile: &str,
        instance_id: &str,
        created_at: DateTime<Utc>,
        lifecycle_generation: u64,
        agent: bool,
        expected_profile: Option<(u64, u64)>,
    ) -> Result<std::sync::Arc<Self>> {
        let fence = Self::capture_route(
            profile,
            instance_id,
            created_at,
            lifecycle_generation,
            agent,
            expected_profile,
        )?;
        drop(fence.lock()?);
        Ok(fence)
    }

    /// Pin the profile before enqueueing; row admission remains an effect-time check.
    pub(crate) fn capture_route(
        profile: &str,
        instance_id: &str,
        created_at: DateTime<Utc>,
        lifecycle_generation: u64,
        agent: bool,
        expected_profile: Option<(u64, u64)>,
    ) -> Result<std::sync::Arc<Self>> {
        let storage = crate::session::storage::Storage::open_reference(profile)
            .context(NativeInputError::Ownership)?;
        if let Some(expected) = expected_profile {
            anyhow::ensure!(
                storage.bound_profile_identity()? == expected,
                NativeInputError::Ownership
            );
        }
        let store = storage
            .bind_native_input(instance_id)
            .context(NativeInputError::Ownership)?;
        Ok(std::sync::Arc::new(Self {
            store,
            instance_id: instance_id.to_owned(),
            created_at,
            lifecycle_generation,
            agent,
            cache: std::sync::Mutex::new(NativeInputCache::default()),
        }))
    }

    /// Retain the guard through the native effect acknowledgement, not user hooks or waits.
    pub(crate) fn lock(&self) -> Result<StorageFlock> {
        self.lock_scoped(true)
    }

    /// Runtime observations compare incarnation, not input or recovery permission.
    pub(crate) fn lock_observation(&self) -> Result<StorageFlock> {
        self.lock_scoped(false)
    }

    /// Additional controller custody may only restrict an observation fence,
    /// never override its physical profile, creation or generation checks.
    pub(crate) fn lock_observation_matching(
        &self,
        matches: impl FnOnce(&Instance) -> bool,
    ) -> Result<StorageFlock> {
        self.lock_scoped_matching(false, matches, false)?
            .context(NativeInputError::Ownership)
    }

    pub(crate) fn try_lock_observation_matching(
        &self,
        matches: impl FnOnce(&Instance) -> bool,
    ) -> Result<Option<StorageFlock>> {
        self.lock_scoped_matching(false, matches, true)
    }

    fn lock_scoped(&self, input: bool) -> Result<StorageFlock> {
        self.lock_scoped_matching(input, |_| true, false)?
            .context(NativeInputError::Ownership)
    }

    fn lock_scoped_matching(
        &self,
        input: bool,
        matches: impl FnOnce(&Instance) -> bool,
        nonblocking: bool,
    ) -> Result<Option<StorageFlock>> {
        let mut cache = if nonblocking {
            match self.cache.try_lock() {
                Ok(cache) => cache,
                Err(std::sync::TryLockError::WouldBlock) => return Ok(None),
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    return Err(NativeInputError::Ownership.into())
                }
            }
        } else {
            self.cache.lock().map_err(|_| NativeInputError::Ownership)?
        };
        anyhow::ensure!(!cache.invalidated, NativeInputError::Ownership);
        let result = (|| {
            let lock = if nonblocking {
                let Some(lock) = self
                    .store
                    .try_acquire_lifecycle_lock()
                    .context(NativeInputError::Ownership)?
                else {
                    return Ok(None);
                };
                lock
            } else {
                self.store
                    .acquire_lifecycle_lock()
                    .context(NativeInputError::Ownership)?
            };
            let revision = self.store.revision().context(NativeInputError::Ownership)?;
            if cache
                .witness
                .as_ref()
                .is_none_or(|witness| witness.revision != revision)
            {
                let (revision, rows) = if nonblocking {
                    let Some(snapshot) = self
                        .store
                        .try_load_complete()
                        .context(NativeInputError::Ownership)?
                    else {
                        return Ok(None);
                    };
                    snapshot
                } else {
                    self.store
                        .load_complete()
                        .context(NativeInputError::Ownership)?
                };
                let mut matching = rows.into_iter().filter(|row| row.id == self.instance_id);
                let row = matching.next().ok_or(NativeInputError::Ownership)?;
                anyhow::ensure!(matching.next().is_none(), NativeInputError::Ownership);
                cache.witness = Some(NativeInputWitness { revision, row });
            }
            let row = &cache
                .witness
                .as_ref()
                .ok_or(NativeInputError::Ownership)?
                .row;
            let same_context = row.created_at == self.created_at
                && row.lifecycle_generation == self.lifecycle_generation
                && row.source_profile == self.store.profile();
            if !same_context {
                cache.invalidated = true;
                return Err(LifecycleReservationError::Superseded.into());
            }
            anyhow::ensure!(matches(row), LifecycleReservationError::Superseded);
            if !input {
                anyhow::ensure!(row.is_structured(), LifecycleReservationError::Superseded);
            } else {
                anyhow::ensure!(
                    !row.has_fresh_lifecycle_reservation(Utc::now()),
                    LifecycleReservationError::Superseded
                );
            }
            if input && self.agent {
                row.ensure_startable()?;
                anyhow::ensure!(!row.is_structured(), NativeInputError::AgentNotReady);
            }
            Ok(Some(lock))
        })();
        if input && result.is_err() {
            cache.invalidated = true;
        }
        result
    }
}

impl Instance {
    /// Ensure physical presence without inferring death from Status or a shell.
    /// The implicit launch path carries its own Missing/Dead proof across hooks.
    /// Readiness for programmatic input is a separate, effect-time admission.
    pub fn ensure_pane_ready(&mut self) -> Result<EnsureReadyOutcome, EnsureReadyError> {
        self.ensure_pane_ready_with_size(None)
    }

    /// Like [`ensure_pane_ready`](Self::ensure_pane_ready), but seeds a
    /// freshly created or respawned pane at `size` (cols, rows) instead of
    /// letting tmux fall back to its 80x24 default.
    ///
    /// Live-send entry passes the visible preview-pane size here so the agent
    /// boots at the width it will be shown at. Without it the agent boots
    /// narrow (80 cols) and depends on a single post-boot `resize-window`
    /// SIGWINCH to grow into the live area. That SIGWINCH races the agent's
    /// startup: if it lands before the agent installs its resize handler the
    /// reflow is lost, and because the per-frame resize loop is deduped on the
    /// (already-correct) tmux window size, nothing re-issues it. The pane then
    /// stays pinned at ~80 cols (≈50% of a wide live area) until live mode is
    /// exited and re-entered. Booting at the right size sidesteps the race.
    ///
    /// `None` keeps tmux's default for callers with no target geometry.
    pub fn ensure_pane_ready_with_size(
        &mut self,
        size: Option<(u16, u16)>,
    ) -> Result<EnsureReadyOutcome, EnsureReadyError> {
        if matches!(self.status, Status::Creating | Status::Deleting) {
            return Err(EnsureReadyError::Transient(self.status));
        }
        if self.is_structured() {
            return Err(EnsureReadyError::StructuredView);
        }
        let (_, _, physical) = self.observe_native_pane().map_err(EnsureReadyError::from)?;
        if physical == crate::tmux::utils::PaneProbe::Alive {
            return Ok(EnsureReadyOutcome::AlreadyAlive);
        }
        let generation = self.lifecycle_generation;
        let outcome = self.start_with_resume_fallback(size, false, ResumeAttemptPolicy::Allow)?;
        match outcome {
            StartOutcome::ResumeFailed { sid } => {
                return Ok(EnsureReadyOutcome::ResumeFailed { sid });
            }
            StartOutcome::Indeterminate { cause } => {
                return Ok(EnsureReadyOutcome::Indeterminate { cause });
            }
            StartOutcome::Resumed
            | StartOutcome::Fresh
            | StartOutcome::FreshAfterFailedResume { .. } => {}
        }
        // A peer may have supplied a live pane before launch admission.
        if self.lifecycle_generation == generation {
            return Ok(EnsureReadyOutcome::AlreadyAlive);
        }
        self.wait_for_pane_ready(&self.tmux_session().map_err(EnsureReadyError::Tmux)?);
        Ok(if physical == crate::tmux::utils::PaneProbe::Dead {
            EnsureReadyOutcome::Respawned
        } else {
            EnsureReadyOutcome::Started
        })
    }

    /// Conditional equality for queued native preparation, not permission to launch.
    /// Stream only selected context into a fixed digest; never expose the bindings.
    pub(crate) fn native_preparation_selection(&self) -> serde_json::Result<[u8; 32]> {
        use sha2::{Digest, Sha256};

        struct SelectionDigest(Sha256);
        impl std::io::Write for SelectionDigest {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.update(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let mut digest = SelectionDigest(Sha256::new());
        digest.0.update(b"aoe-native-preparation-selection-v1\0");
        serde_json::to_writer(
            &mut digest,
            &(
                &self.tool,
                &self.command,
                &self.extra_args,
                &self.agent_session_id,
                &self.agent_session_binding,
                &self.resume_intent,
                &self.resume_binding,
                &self.pi_session_path,
            ),
        )?;
        Ok(digest.0.finalize().into())
    }

    /// Capture authority once for a physical input route; never retarget it after invalidation.
    pub(crate) fn native_input_fence(
        &self,
        agent: bool,
    ) -> Result<std::sync::Arc<NativeInputFence>> {
        NativeInputFence::capture(
            &self.effective_profile(),
            &self.id,
            self.created_at,
            self.lifecycle_generation,
            agent,
            None,
        )
    }

    pub(crate) fn agent_input_session(
        &self,
        name: &str,
        physical: std::sync::Arc<LegacyToolObservation>,
        wait: bool,
        captured_fence: Option<std::sync::Arc<NativeInputFence>>,
    ) -> Result<tmux::Session> {
        self.ensure_startable()?;
        anyhow::ensure!(
            !self.is_structured(),
            "Session no longer has a terminal agent"
        );
        let fence = match captured_fence {
            Some(fence) => fence,
            None => self.native_input_fence(true)?,
        };
        let mut session =
            tmux::Session::from_observation(name, physical).with_native_input_fence(fence);
        if wait {
            session.wait_until_ready(
                std::time::Duration::from_secs(5),
                crate::agents::ready_marker(&self.tool),
            );
        }
        session.admit_agent_input(self.expects_shell())?;
        Ok(session)
    }

    /// Best-effort wait for a freshly-started pane to settle past its initial
    /// shell/splash so subsequent `send-keys` land in the agent instead of a
    /// boot prompt. Polls up to 3s in 50ms increments; returns even on
    /// timeout so a sluggish agent doesn't block the send indefinitely.
    ///
    /// Readiness signal:
    /// - Agents that expect a shell, run a custom command override, or have
    ///   an active hook status file: just wait for the pane to not be dead.
    ///   Wrapper scripts look like shells to tmux, so `is_pane_running_shell`
    ///   would never clear for them and we would eat the full 3s every time.
    ///   This mirrors the same guard chain `ensure_session` uses.
    /// - Real agents (e.g. claude, opencode): also wait for the pane to no
    ///   longer be running a shell, so a keystroke doesn't land in the boot
    ///   prompt that runs before the agent binary takes over.
    pub(crate) fn wait_for_pane_ready(&self, session: &tmux::Session) {
        let shell_check_unreliable = self.expects_shell()
            || self.has_command_override()
            || crate::hooks::read_hook_status(&self.id).is_some();
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(3000);
        loop {
            if !session.exists() {
                return;
            }
            let pane_alive = !session.is_pane_dead();
            if pane_alive && (shell_check_unreliable || !session.is_pane_running_shell()) {
                return;
            }
            if std::time::Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input_fixture(profile: &str) -> Result<(crate::session::storage::Storage, Instance)> {
        let storage = crate::session::storage::Storage::new_unwatched(profile)?;
        let mut row = Instance::new("input witness", "/tmp/input-witness");
        row.source_profile = profile.into();
        row.lifecycle_generation = 7;
        storage.update(|rows, _| {
            rows.push(row.clone());
            Ok(())
        })?;
        Ok((storage, row))
    }

    #[test]
    #[serial_test::serial]
    fn native_input_witness_requires_complete_rows_without_recovery() -> Result<()> {
        let _app = crate::session::test_support::isolate_app_dir();
        for refresh in [false, true] {
            let profile = if refresh {
                "input-refresh"
            } else {
                "input-capture"
            };
            let (storage, row) = input_fixture(profile)?;
            let original = std::fs::read(storage.sessions_path())?;
            let fence = refresh.then(|| row.native_input_fence(false)).transpose()?;
            let corrupt = serde_json::to_vec(&serde_json::json!([
                row,
                {"id": "undecodable-peer", "created_at": 17}
            ]))?;
            let quarantine = storage
                .sessions_path()
                .with_file_name("sessions.corrupt.jsonl");
            std::fs::write(&quarantine, b"prior evidence\n")?;
            crate::session::storage::atomic_write(storage.sessions_path(), &corrupt)?;
            let error = match &fence {
                Some(fence) => fence
                    .lock()
                    .err()
                    .expect("refresh must reject incomplete rows"),
                None => row.native_input_fence(false).unwrap_err(),
            };
            assert!(error.downcast_ref::<NativeInputError>().is_some());
            assert_eq!(std::fs::read(storage.sessions_path())?, corrupt);
            assert_eq!(std::fs::read(&quarantine)?, b"prior evidence\n");
            crate::session::storage::atomic_write(storage.sessions_path(), &original)?;
            if let Some(fence) = fence {
                assert!(fence.lock().is_err(), "invalidated scope must not rearm");
            }
            drop(row.native_input_fence(false)?.lock()?);
        }
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn native_input_witness_checks_atomic_replacement_not_retained_fd_or_mtime() -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        let _app = crate::session::test_support::isolate_app_dir();
        let (storage, row) = input_fixture("input-atomic-replacement")?;
        let fence = row.native_input_fence(false)?;
        let old_file = std::fs::File::open(storage.sessions_path())?;
        let old_metadata = old_file.metadata()?;
        let mut replacement = row.clone();
        replacement.lifecycle_generation += 1;
        let replacement = serde_json::to_vec_pretty(&vec![replacement])?;
        crate::session::storage::atomic_write(storage.sessions_path(), &replacement)?;
        let current = std::fs::File::options()
            .write(true)
            .open(storage.sessions_path())?;
        current.set_times(std::fs::FileTimes::new().set_modified(old_metadata.modified()?))?;
        assert_ne!(old_metadata.ino(), current.metadata()?.ino());
        assert_eq!(old_metadata.ino(), old_file.metadata()?.ino());
        assert_eq!(old_metadata.modified()?, current.metadata()?.modified()?);
        let error = fence
            .lock()
            .err()
            .expect("new anchored inode must refresh authority");
        assert_eq!(
            error.downcast_ref::<LifecycleReservationError>(),
            Some(&LifecycleReservationError::Superseded)
        );
        assert_eq!(std::fs::read(storage.sessions_path())?, replacement);
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn native_input_generation_change_rejects_stale_scope_but_allows_fresh_capture() -> Result<()> {
        let root = tempfile::tempdir()?;
        let _app = crate::session::test_support::isolate_app_dir_at(root.path());
        let storage = crate::session::storage::Storage::new_unwatched("input-generation")?;
        let mut row = super::super::test_helpers::reserved_claude_instance(
            root.path(),
            "11111111-2222-4333-8444-555555555555",
        );
        row.source_profile = storage.profile().into();
        row.lifecycle_generation = 7;
        row.status = Status::Error;
        row.started_at = Some(Utc::now());
        storage.update(|rows, _| {
            rows.push(row.clone());
            Ok(())
        })?;
        let stale = row.native_input_fence(true)?;
        let execution = row.active_execution.clone();
        let sid = row.agent_session_id.clone();
        let started_at = row.started_at;
        row.advance_auxiliary_generation_in(&storage)?;
        assert_eq!(row.active_execution, execution);
        assert_eq!(row.agent_session_id, sid);
        assert_eq!(row.started_at, started_at);
        let error = stale
            .lock()
            .err()
            .expect("same execution cannot revive stale generation");
        assert_eq!(
            error.downcast_ref::<LifecycleReservationError>(),
            Some(&LifecycleReservationError::Superseded)
        );
        drop(row.native_input_fence(true)?.lock()?);
        assert_eq!(storage.load_instances_complete()?[0].status, Status::Error);
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn native_input_same_generation_rename_and_peer_write_allow_raw_inactive_agent() -> Result<()> {
        let _app = crate::session::test_support::isolate_app_dir();
        let (storage, mut row) = input_fixture("input-display-refresh")?;
        row.view = View::Structured;
        row.status = Status::Stopped;
        row.archived_at = Some(Utc::now());
        row.terminal_info = Some(TerminalInfo { created: true });
        storage.update(|rows, _| {
            rows[0] = row.clone();
            Ok(())
        })?;
        let fence = row.native_input_fence(false)?;
        assert_eq!(
            row.native_input_fence(true)
                .unwrap_err()
                .downcast_ref::<StartBlocked>(),
            Some(&StartBlocked::Archived)
        );
        storage.update(|rows, _| {
            rows[0].title = "display rename".into();
            rows.push(Instance::new("unrelated peer", "/tmp/unrelated-peer"));
            Ok(())
        })?;
        drop(fence.lock()?);
        drop(fence.lock()?);
        assert_eq!(
            fence
                .cache
                .lock()
                .unwrap()
                .witness
                .as_ref()
                .unwrap()
                .row
                .title,
            "display rename"
        );
        storage.update(|rows, _| {
            rows[0].archived_at = None;
            Ok(())
        })?;
        assert!(matches!(
            row.native_input_fence(true)
                .unwrap_err()
                .downcast_ref::<NativeInputError>(),
            Some(NativeInputError::AgentNotReady)
        ));
        drop(fence.lock()?);
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn native_input_reservation_or_creation_change_permanently_invalidates_scope() -> Result<()> {
        let _app = crate::session::test_support::isolate_app_dir();
        for reservation in [false, true] {
            let profile = if reservation {
                "input-reservation"
            } else {
                "input-creation"
            };
            let (storage, row) = input_fixture(profile)?;
            let fence = row.native_input_fence(false)?;
            storage.update(|rows, _| {
                if reservation {
                    rows[0].lifecycle_reservation = Some(LifecycleReservation {
                        op: LifecycleOperation::Capture,
                        generation: row.lifecycle_generation,
                        at: Utc::now(),
                    });
                } else {
                    rows[0].created_at += chrono::Duration::seconds(1);
                }
                Ok(())
            })?;
            let error = fence
                .lock()
                .err()
                .expect("changed authority must reject input");
            assert_eq!(
                error.downcast_ref::<LifecycleReservationError>(),
                Some(&LifecycleReservationError::Superseded)
            );
            storage.update(|rows, _| {
                rows[0] = row.clone();
                Ok(())
            })?;
            assert!(
                fence.lock().is_err(),
                "restored data must not rearm old scope"
            );
            drop(row.native_input_fence(false)?.lock()?);
        }
        Ok(())
    }

    #[test]
    fn test_ensure_pane_ready_bails_on_creating() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.status = Status::Creating;
        match inst.ensure_pane_ready() {
            Err(EnsureReadyError::Transient(Status::Creating)) => {}
            other => panic!("expected Transient(Creating), got {other:?}"),
        }
    }

    #[test]
    fn test_ensure_pane_ready_bails_on_deleting() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.status = Status::Deleting;
        match inst.ensure_pane_ready() {
            Err(EnsureReadyError::Transient(Status::Deleting)) => {}
            other => panic!("expected Transient(Deleting), got {other:?}"),
        }
    }

    #[test]
    fn test_ensure_pane_ready_bails_on_structured() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.view = View::Structured;
        match inst.ensure_pane_ready() {
            Err(EnsureReadyError::StructuredView) => {}
            other => panic!("expected StructuredView, got {other:?}"),
        }
    }

    /// Real-tmux integration: an alive pane yields AlreadyAlive with no
    /// status/start_time mutations. Skipped if tmux isn't installed.
    // Serialized: this test creates and kills a real tmux session.
    #[test]
    #[serial_test::serial]
    fn test_ensure_pane_ready_alive_pane_is_noop() {
        if crate::tmux::tmux_command().arg("-V").output().is_err() {
            eprintln!("tmux not available; skipping");
            return;
        }

        let mut inst = Instance::new("ensure_alive_test", "/tmp/test");
        let tmux_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
        let _ = crate::tmux::tmux_command()
            .args(["kill-session", "-t", &tmux_name])
            .output();
        let created = crate::tmux::tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                &tmux_name,
                "-x",
                "80",
                "-y",
                "24",
                "sleep",
                "60",
            ])
            .status();
        if !created.map(|s| s.success()).unwrap_or(false) {
            eprintln!("tmux new-session failed; skipping");
            return;
        }
        crate::tmux::refresh_session_cache();

        inst.status = Status::Running;
        let prev_start = inst.last_start_time;
        let prev_status = inst.status;

        let outcome = inst.ensure_pane_ready().expect("ensure_pane_ready ok");
        assert_eq!(outcome, EnsureReadyOutcome::AlreadyAlive);
        assert_eq!(inst.last_start_time, prev_start);
        assert_eq!(inst.status, prev_status);

        let _ = crate::tmux::tmux_command()
            .args(["kill-session", "-t", &tmux_name])
            .output();
    }
}
