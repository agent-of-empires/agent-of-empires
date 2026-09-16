//! Tearing a session down.

use super::*;

impl Instance {
    pub(super) fn flush_published_if_present(&mut self) {
        if !self.uses_pi_session_sidecar()
            && !matches!(
                self.active_execution
                    .as_ref()
                    .and_then(|active| active.capture.as_ref()),
                Some(CaptureContext::Hooks(_))
            )
        {
            return;
        }
        let profile = self.effective_profile();
        let Ok(storage) =
            crate::session::storage::Storage::new(&profile, self.resolve_file_watch())
        else {
            return;
        };
        if self.flush_published_conversation(&storage) == Some(SidWrite::Failed) {
            tracing::warn!(target: "session.store", instance = %self.id, "could not persist final conversation publication");
            return;
        }
        if let Ok(instances) = storage.load() {
            if let Some(row) = instances.iter().find(|i| i.id == self.id) {
                self.adopt_conversation_state(row.conversation_state());
            }
        }
    }

    pub(super) fn flush_published_conversation(
        &self,
        storage: &crate::session::storage::Storage,
    ) -> Option<SidWrite> {
        let observation = self.final_publication_observation()?;
        if self.is_capture_excluded(&observation.sid, observation.source.as_ref()) {
            return None;
        }
        let outcome = super::sid_persist::persist_session_with_storage(
            storage,
            &self.id,
            &observation,
            &self.conversation_state(),
        );
        if outcome != SidWrite::Skipped {
            return Some(outcome);
        }
        // `Skipped` reports a peer write between the caller's read and the CAS.
        // Retry once against the row as it now stands: a peer that committed
        // the same final observation makes this publication durable, while a
        // fork intent or another conversation skips again.
        // A retry that cannot read the row leaves the publication in doubt, so
        // it must fail the flush: `None` would read as "nothing to publish" and
        // let teardown delete the evidence.
        let Ok(rows) = storage.load() else {
            return Some(SidWrite::Failed);
        };
        let Some(retry) = rows.into_iter().find(|row| row.id == self.id) else {
            return Some(SidWrite::Failed);
        };
        super::sid_persist::persist_session_with_storage(
            storage,
            &self.id,
            &observation,
            &retry.conversation_state(),
        )
        .or_pinned_foreign_publication(observation.sid.as_str(), &retry)
    }

    /// The conversation this pane published last, for the final flush.
    pub(super) fn final_publication_observation(
        &self,
    ) -> Option<crate::session::poller::SessionIdObservation> {
        if matches!(
            self.active_execution
                .as_ref()
                .and_then(|active| active.capture.as_ref()),
            Some(CaptureContext::Hooks(_))
        ) {
            super::execution::hook_session_observation(
                &self.id,
                self.active_execution.as_ref(),
                None,
            )
        } else if self.uses_pi_session_sidecar() {
            self.pi_published_conversation(true)
        } else {
            None
        }
    }

    /// Tear down the current tmux session cleanly so a fresh
    /// `start_with_size_opts` can recreate it.
    ///
    /// `remain-on-exit on` keeps the tmux session alive after the agent
    /// process exits, leaving a frozen pane. The plain kill-session +
    /// new-session flow can race against the session cache
    /// (kill_process_tree on a defunct pid stalls on macOS, and the
    /// subsequent kill can run while start's exists() check still sees the
    /// cached entry), leaving the dead pane in place. Respawning the pane
    /// into a shell first puts it back in a live state so the kill path
    /// proceeds cleanly. The kill below then sees a live pane and tears it
    /// down. Caller is responsible for the subsequent
    /// `start_with_size_opts` to recreate the session with the agent
    /// command.
    pub(super) fn kill_clean_locked(&self) -> Result<()> {
        let session = self.tmux_session()?;
        if !session.exists() {
            return Ok(());
        }
        if session.is_pane_dead() {
            tracing::info!(target: "session.store",
                "restart: pane dead for session {} (remain-on-exit), \
                 respawning shell before recreate",
                session.name()
            );
            let shell = crate::session::environment::user_shell();
            if let Err(e) = session.respawn_dead_pane(&self.project_path, Some(&shell)) {
                tracing::warn!(target: "session.store",
                    "respawn_dead_pane failed for {}: {}; falling back to kill+start",
                    session.name(),
                    e
                );
            }
        }
        session.kill()?;
        std::thread::sleep(std::time::Duration::from_millis(100));
        Ok(())
    }

    pub(crate) fn kill_clean(&self) -> Result<()> {
        let profile = self.effective_profile();
        let storage = crate::session::storage::Storage::new(&profile, self.resolve_file_watch())
            .context("failed to open lifecycle lock storage")?;
        let _lifecycle_lock = storage
            .acquire_instance_lifecycle_lock(&self.id)
            .context("failed to acquire instance kill lock")?;
        let mut lifecycle = self.clone();
        lifecycle.acquire_lifecycle_reservation(&storage, LifecycleOperation::Stop, None)?;
        match self.kill_clean_locked() {
            Ok(()) => lifecycle.commit_lifecycle_status(
                &storage,
                LifecycleOperation::Stop,
                Status::Stopped,
            ),
            Err(error) => {
                let _ = lifecycle.commit_lifecycle_status(
                    &storage,
                    LifecycleOperation::Stop,
                    Status::Error,
                );
                Err(error)
            }
        }
    }

    pub(crate) fn kill_locked(&self) -> Result<()> {
        self.stop_poller();
        let session = self.tmux_session()?;
        if session.exists() {
            session.kill()?;
        }
        Ok(())
    }

    pub fn kill(&self) -> Result<()> {
        let profile = self.effective_profile();
        let storage = crate::session::storage::Storage::new(&profile, self.resolve_file_watch())
            .context("failed to open lifecycle lock storage")?;
        let _lifecycle_lock = storage
            .acquire_instance_lifecycle_lock(&self.id)
            .context("failed to acquire instance kill lock")?;
        let mut lifecycle = self.clone();
        lifecycle.acquire_lifecycle_reservation(&storage, LifecycleOperation::Stop, None)?;
        match self.kill_locked() {
            Ok(()) => lifecycle.commit_lifecycle_status(
                &storage,
                LifecycleOperation::Stop,
                Status::Stopped,
            ),
            Err(error) => {
                let _ = lifecycle.commit_lifecycle_status(
                    &storage,
                    LifecycleOperation::Stop,
                    Status::Error,
                );
                Err(error)
            }
        }
    }

    /// Kill every tmux session owned by this instance (agent, web
    /// terminal, container terminal, tool sub-sessions). Best-effort
    /// and silent; agent/terminal/container terminal failures log at
    /// `debug!` target `session.tmux_cleanup`. Tool sub-sessions are
    /// silent by design via `kill_all_tool_sessions_for_id`.
    pub fn kill_all_tmux_sessions(&self) {
        let profile = self.effective_profile();
        let storage =
            match crate::session::storage::Storage::new(&profile, self.resolve_file_watch()) {
                Ok(storage) => storage,
                Err(error) => {
                    tracing::warn!(
                        target: "session.tmux_cleanup",
                        session_id = %self.id,
                        %error,
                        "kill_all_tmux_sessions: lifecycle storage failed"
                    );
                    return;
                }
            };
        let _lifecycle_lock = match storage.acquire_instance_lifecycle_lock(&self.id) {
            Ok(lock) => lock,
            Err(error) => {
                tracing::warn!(
                    target: "session.tmux_cleanup",
                    session_id = %self.id,
                    %error,
                    "kill_all_tmux_sessions: lifecycle lock failed"
                );
                return;
            }
        };
        let mut lifecycle = self.clone();
        if let Err(error) =
            lifecycle.acquire_lifecycle_reservation(&storage, LifecycleOperation::Stop, None)
        {
            tracing::warn!(
                target: "session.tmux_cleanup",
                session_id = %self.id,
                %error,
                "kill_all_tmux_sessions: lifecycle reservation failed"
            );
            return;
        }
        self.kill_all_tmux_sessions_locked();
        if let Err(error) =
            lifecycle.commit_lifecycle_status(&storage, LifecycleOperation::Stop, Status::Stopped)
        {
            tracing::warn!(
                target: "session.tmux_cleanup",
                session_id = %self.id,
                %error,
                "kill_all_tmux_sessions: lifecycle commit failed"
            );
        }
    }

    /// Kill every tmux session owned by this instance while the caller holds
    /// the selected profile's per-instance lifecycle lock.
    ///
    /// Destructive deletion keeps that guard across tmux/container/worktree
    /// teardown and the durable row removal, so it must use this helper rather
    /// than reacquiring the non-reentrant lock via [`Self::kill_all_tmux_sessions`].
    pub(crate) fn kill_all_tmux_sessions_locked(&self) {
        self.kill_all_tmux_sessions_uncoordinated();
    }

    /// Tear down tmux resources when no durable lifecycle row exists.
    ///
    /// Used after force-removal and when rolling back an instance that failed
    /// before its row was committed. With no row, lifecycle reservation is
    /// impossible; callers must already know the id cannot race a launch.
    pub(crate) fn kill_all_tmux_sessions_without_lifecycle_row(&self) {
        self.kill_all_tmux_sessions_uncoordinated();
    }

    fn kill_all_tmux_sessions_uncoordinated(&self) {
        if let Err(e) = self.kill_locked() {
            tracing::debug!(
                target: "session.tmux_cleanup",
                session_id = %self.id,
                kind = "agent",
                error = %e,
                "kill_all_tmux_sessions_uncoordinated: kill failed"
            );
        }
        self.kill_ancillary_tmux_sessions_locked();
    }

    pub(crate) fn kill_ancillary_tmux_sessions_locked(&self) {
        crate::tmux::kill_all_terminals_for_id(&self.id);
        crate::tmux::kill_all_tool_sessions_for_id(&self.id);
    }

    /// Kill every tmux session owned by this instance EXCEPT the agent
    /// session (web terminal, container terminal, tool sub-sessions).
    pub fn kill_ancillary_tmux_sessions(&self) {
        let profile = self.effective_profile();
        let storage =
            match crate::session::storage::Storage::new(&profile, self.resolve_file_watch()) {
                Ok(storage) => storage,
                Err(error) => {
                    tracing::warn!(
                        target: "session.tmux_cleanup",
                        session_id = %self.id,
                        %error,
                        "kill_ancillary_tmux_sessions: lifecycle storage failed"
                    );
                    return;
                }
            };
        let _lifecycle_lock = match storage.acquire_instance_lifecycle_lock(&self.id) {
            Ok(lock) => lock,
            Err(error) => {
                tracing::warn!(
                    target: "session.tmux_cleanup",
                    session_id = %self.id,
                    %error,
                    "kill_ancillary_tmux_sessions: lifecycle lock failed"
                );
                return;
            }
        };
        let mut lifecycle = self.clone();
        if let Err(error) =
            lifecycle.acquire_lifecycle_reservation(&storage, LifecycleOperation::Stop, None)
        {
            tracing::warn!(
                target: "session.tmux_cleanup",
                session_id = %self.id,
                %error,
                "kill_ancillary_tmux_sessions: lifecycle reservation failed"
            );
            return;
        }
        self.kill_ancillary_tmux_sessions_locked();
        if let Err(error) =
            lifecycle.release_lifecycle_reservation(&storage, LifecycleOperation::Stop)
        {
            tracing::warn!(
                target: "session.tmux_cleanup",
                session_id = %self.id,
                %error,
                "kill_ancillary_tmux_sessions: lifecycle release failed"
            );
        }
    }

    /// Stop the session and its sandbox container under the same lifecycle
    /// lock used by launch/restart.
    pub fn stop(&self) -> Result<()> {
        let profile = self.effective_profile();
        let storage = crate::session::storage::Storage::new(&profile, self.resolve_file_watch())
            .context("failed to open lifecycle lock storage")?;
        let _lifecycle_lock = storage
            .acquire_instance_lifecycle_lock(&self.id)
            .context("failed to acquire instance stop lock")?;
        let mut lifecycle = storage
            .load()?
            .into_iter()
            .find(|row| row.id == self.id)
            .context("session disappeared before stop")?;
        lifecycle.source_profile = profile.clone();
        lifecycle.acquire_lifecycle_reservation(&storage, LifecycleOperation::Stop, None)?;
        self.stop_poller();
        let teardown = lifecycle.kill_locked().and_then(|()| {
            let mut current = storage
                .load()?
                .into_iter()
                .find(|row| row.id == self.id)
                .context("session disappeared during stop")?;
            current.source_profile = profile.clone();
            let flushed = current.flush_published_conversation(&storage);
            // A pinned-foreign publication is an explicit refusal, not a
            // doubtful write: the user pinned another conversation and this
            // pane's id must not overwrite it. Any other failure still keeps
            // the hook evidence, but the sandbox container always stops: the
            // store is a host bind, not container state.
            let container_result = crate::session::worktree_edit::stop_sandbox_container(
                &current.id,
                current.is_sandboxed(),
            );
            match flushed {
                Some(SidWrite::PinnedForeign) => container_result,
                Some(SidWrite::Applied) | None => container_result,
                Some(SidWrite::Failed) | Some(SidWrite::Skipped) => {
                    container_result?;
                    anyhow::bail!(
                        "could not persist final conversation publication; hook evidence retained"
                    )
                }
            }
        });
        match teardown {
            Ok(()) => {
                lifecycle.commit_lifecycle_status(
                    &storage,
                    LifecycleOperation::Stop,
                    Status::Stopped,
                )?;
                crate::hooks::cleanup_hook_status_dir(&self.id);
                Ok(())
            }
            Err(error) => {
                let _ = lifecycle.commit_lifecycle_status(
                    &storage,
                    LifecycleOperation::Stop,
                    Status::Error,
                );
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    #[serial_test::serial]
    fn stop_promotes_same_sid_hook_publication_before_removing_evidence() {
        use super::super::execution::{ActiveExecution, CaptureContext};
        use crate::session::{ConversationBinding, ConversationProvenance, ExecutionBinding};
        let (_guard, _base, _tmp) = crate::hooks::test_support::BaseGuard::ready();
        let home = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(home.path());
        let profile = "hook-stop-publication";
        let mut inst = Instance::new("hook-stop", home.path().to_str().unwrap());
        inst.source_profile = profile.into();
        inst.tool = "claude".into();
        let sid = "22f13307-461c-4161-908e-95a247fac750";
        let launch = uuid::Uuid::new_v4().to_string();
        let source = crate::hooks::ensure_instance_dir_path(&inst.id)
            .unwrap()
            .join(
                crate::hooks::session_id_leaf(Some(&launch))
                    .unwrap()
                    .as_ref(),
            );
        let binding = ExecutionBinding {
            agent: "claude".into(),
            stores: vec![home.path().join("store")],
            configuration: Vec::new(),
            cwd: home.path().into(),
            cwd_filesystem: "host".into(),
            filesystem: "host".into(),
        };
        inst.agent_session_id = Some(sid.into());
        inst.agent_session_binding = Some(ConversationBinding {
            session_id: sid.into(),
            execution: Some(binding.clone()),
            provenance: ConversationProvenance::Preallocated,
            transcript_path: None,
        });
        inst.active_execution = Some(ActiveExecution {
            launch_id: launch.clone(),
            binding,
            capture: Some(CaptureContext::Hooks(source.clone())),
            container: None,
        });
        let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
        storage
            .update(|rows, _| {
                *rows = vec![inst.clone()];
                Ok(())
            })
            .unwrap();
        crate::hooks::write_session_id_via_guard(&inst.id, sid, Some(&launch)).unwrap();
        inst.stop().unwrap();
        let rows = storage.load().unwrap();
        assert_eq!(rows[0].agent_session_id.as_deref(), Some(sid));
        assert_eq!(
            rows[0].agent_session_binding.as_ref().unwrap().provenance,
            ConversationProvenance::Observed
        );
        assert!(!source.exists());
        let intermediate = "22f13307-461c-4161-908e-95a247fac751";
        let final_sid = "22f13307-461c-4161-908e-95a247fac752";
        storage
            .update(|rows, _| {
                let mut binding = rows[0].agent_session_binding.clone().unwrap();
                binding.session_id = intermediate.into();
                rows[0].set_agent_conversation(Some(intermediate.into()), Some(binding), None);
                Ok(())
            })
            .unwrap();
        crate::hooks::ensure_instance_dir_path(&inst.id).unwrap();
        crate::hooks::write_session_id_via_guard(&inst.id, final_sid, Some(&launch)).unwrap();
        inst.stop().unwrap();
        assert_eq!(
            storage.load().unwrap()[0].agent_session_id.as_deref(),
            Some(final_sid)
        );
        assert!(!source.exists());
    }

    #[test]
    #[serial_test::serial]
    fn skipped_final_publication_retries_against_the_row_a_peer_wrote() {
        // A peer can commit between the caller's read and the CAS. The stop
        // flush must reload and retry: a stale expectation alone is not proof
        // the final observation was lost.
        use super::super::execution::{ActiveExecution, CaptureContext};
        use crate::session::{ConversationBinding, ConversationProvenance, ExecutionBinding};
        let (_guard, _base, _tmp) = crate::hooks::test_support::BaseGuard::ready();
        let home = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(home.path());
        let profile = "hook-stop-skipped";
        let mut inst = Instance::new("hook-skip", home.path().to_str().unwrap());
        inst.source_profile = profile.into();
        inst.tool = "claude".into();
        let sid = "22f13307-461c-4161-908e-95a247fac750";
        let stale = "22f13307-461c-4161-908e-95a247fac751";
        let launch = uuid::Uuid::new_v4().to_string();
        let source = crate::hooks::ensure_instance_dir_path(&inst.id)
            .unwrap()
            .join(
                crate::hooks::session_id_leaf(Some(&launch))
                    .unwrap()
                    .as_ref(),
            );
        let binding = ExecutionBinding {
            agent: "claude".into(),
            stores: vec![home.path().join("store")],
            configuration: Vec::new(),
            cwd: home.path().into(),
            cwd_filesystem: "host".into(),
            filesystem: "host".into(),
        };
        inst.agent_session_id = Some(stale.into());
        inst.active_execution = Some(ActiveExecution {
            launch_id: launch.clone(),
            binding: binding.clone(),
            capture: Some(CaptureContext::Hooks(source.clone())),
            container: None,
        });

        let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
        let mut row = inst.clone();
        row.set_agent_conversation(
            Some("22f13307-461c-4161-908e-95a247fac752".into()),
            Some(ConversationBinding {
                session_id: "22f13307-461c-4161-908e-95a247fac752".into(),
                execution: Some(binding),
                provenance: ConversationProvenance::Observed,
                transcript_path: None,
            }),
            None,
        );
        storage
            .update(|rows, _| {
                *rows = vec![row.clone()];
                Ok(())
            })
            .unwrap();
        crate::hooks::write_session_id_via_guard(&inst.id, sid, Some(&launch)).unwrap();

        assert_eq!(
            inst.flush_published_conversation(&storage),
            Some(SidWrite::Applied)
        );
        assert_eq!(
            storage.load().unwrap()[0].agent_session_id.as_deref(),
            Some(sid)
        );
    }
    #[test]
    #[serial_test::serial]
    fn stop_with_pinned_foreign_publication_stops_container_and_keeps_pin() {
        // A row pinned to another conversation deterministically skips the
        // final flush: teardown must not fail, must not overwrite the pin,
        // and must still release the sandbox container.
        use super::super::execution::{ActiveExecution, CaptureContext};
        use crate::session::{ConversationBinding, ConversationProvenance, ExecutionBinding};
        let (_guard, _base, _tmp) = crate::hooks::test_support::BaseGuard::ready();
        let home = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(home.path());
        let profile = "hook-stop-pinned";
        let mut inst = Instance::new("hook-pinned", home.path().to_str().unwrap());
        inst.source_profile = profile.into();
        inst.tool = "claude".into();
        let pinned = "22f13307-461c-4161-908e-95a247fac760";
        let published = "22f13307-461c-4161-908e-95a247fac761";
        let launch = uuid::Uuid::new_v4().to_string();
        let source = crate::hooks::ensure_instance_dir_path(&inst.id)
            .unwrap()
            .join(
                crate::hooks::session_id_leaf(Some(&launch))
                    .unwrap()
                    .as_ref(),
            );
        let binding = ExecutionBinding {
            agent: "claude".into(),
            stores: vec![home.path().join("store")],
            configuration: Vec::new(),
            cwd: home.path().into(),
            cwd_filesystem: "host".into(),
            filesystem: "host".into(),
        };
        inst.resume_intent = ResumeIntent::Use(pinned.into());
        inst.resume_binding = Some(ConversationBinding {
            session_id: pinned.into(),
            execution: Some(binding.clone()),
            provenance: ConversationProvenance::Asserted,
            transcript_path: None,
        });
        inst.agent_session_id = Some(published.into());
        inst.active_execution = Some(ActiveExecution {
            launch_id: launch.clone(),
            binding: binding.clone(),
            capture: Some(CaptureContext::Hooks(source.clone())),
            container: None,
        });
        let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
        storage
            .update(|rows, _| {
                *rows = vec![inst.clone()];
                Ok(())
            })
            .unwrap();
        crate::hooks::write_session_id_via_guard(&inst.id, published, Some(&launch)).unwrap();
        assert_eq!(
            inst.flush_published_conversation(&storage),
            Some(SidWrite::PinnedForeign)
        );
        let rows = storage.load().unwrap();
        assert!(matches!(
            &rows[0].resume_intent,
            ResumeIntent::Use(pin) if pin == pinned
        ));
    }

    #[test]
    #[serial_test::serial]
    fn stop_with_pinned_binding_mismatch_stays_error() {
        // The same pin with the observation's own sid but a divergent
        // execution binding stays a namespace doubt: stop must fail and keep
        // the evidence.
        use super::super::execution::{ActiveExecution, CaptureContext};
        use crate::session::{ConversationBinding, ConversationProvenance, ExecutionBinding};
        let (_guard, _base, _tmp) = crate::hooks::test_support::BaseGuard::ready();
        let home = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(home.path());
        let profile = "hook-stop-pinned-mismatch";
        let mut inst = Instance::new("hook-pinned-mismatch", home.path().to_str().unwrap());
        inst.source_profile = profile.into();
        inst.tool = "claude".into();
        let sid = "22f13307-461c-4161-908e-95a247fac770";
        let launch = uuid::Uuid::new_v4().to_string();
        let source = crate::hooks::ensure_instance_dir_path(&inst.id)
            .unwrap()
            .join(
                crate::hooks::session_id_leaf(Some(&launch))
                    .unwrap()
                    .as_ref(),
            );
        let mut binding = ExecutionBinding {
            agent: "claude".into(),
            stores: vec![home.path().join("store")],
            configuration: Vec::new(),
            cwd: home.path().into(),
            cwd_filesystem: "host".into(),
            filesystem: "host".into(),
        };
        inst.resume_intent = ResumeIntent::Use(sid.into());
        inst.resume_binding = Some(ConversationBinding {
            session_id: sid.into(),
            execution: Some(binding.clone()),
            provenance: ConversationProvenance::Asserted,
            transcript_path: None,
        });
        binding.cwd = home.path().join("elsewhere");
        inst.agent_session_id = Some(sid.into());
        inst.agent_session_binding = Some(ConversationBinding {
            session_id: sid.into(),
            execution: Some(binding.clone()),
            provenance: ConversationProvenance::Observed,
            transcript_path: None,
        });
        inst.active_execution = Some(ActiveExecution {
            launch_id: launch.clone(),
            binding: binding.clone(),
            capture: Some(CaptureContext::Hooks(source.clone())),
            container: None,
        });
        let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
        storage
            .update(|rows, _| {
                *rows = vec![inst.clone()];
                Ok(())
            })
            .unwrap();
        crate::hooks::write_session_id_via_guard(&inst.id, sid, Some(&launch)).unwrap();
        assert!(matches!(
            inst.flush_published_conversation(&storage),
            Some(SidWrite::Skipped)
        ));
    }

    #[test]
    #[serial_test::serial]
    fn pi_stop_persists_a_conversation_published_long_ago() {
        // An idle pane's `/new` can be hours old by the time it stops. The
        // freshness window that guards a resume must not apply to the last
        // read before the sidecar is deleted.
        let (_guard, _base, _tmp) = crate::hooks::test_support::BaseGuard::ready();
        let home = tempfile::tempdir().unwrap();
        let _home_guard = crate::session::test_support::isolate_app_dir_at(home.path());

        let profile = "pi-sidecar-stale";
        let mut inst = Instance::new("pi-stale", "/tmp/pi-stale");
        inst.source_profile = profile.to_string();
        inst.tool = "pi".to_string();
        inst.agent_session_id = Some("22f13307-461c-4161-908e-95a247fac750".to_string());
        inst.mark_pi_extension_launched_for_test();

        let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
        let seed = inst.clone();
        storage
            .update(|instances, _| {
                *instances = vec![seed.clone()];
                Ok(())
            })
            .unwrap();

        let published = "01a0538e-5868-7c22-84bc-40cfd7a09ab1";
        super::super::test_helpers::publish_host_pi_transcript(&inst.id, published, home.path());
        let sidecar = crate::hooks::ensure_instance_dir_path(&inst.id)
            .unwrap()
            .join("session_id");
        let hours_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(6 * 3600);
        std::fs::File::options()
            .write(true)
            .open(&sidecar)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(hours_ago))
            .unwrap();
        assert_eq!(
            crate::hooks::read_hook_session_id(&inst.id),
            None,
            "the fixture must be past the freshness window"
        );

        let _ = inst.flush_published_conversation(&storage);

        assert_eq!(
            storage.load().unwrap()[0].agent_session_id.as_deref(),
            Some(published),
            "a stale sidecar is still the pane's own last word"
        );
    }

    #[test]
    #[serial_test::serial]
    fn pi_stop_persists_the_conversation_the_extension_published() {
        // A `/new` inside a CLI-launched pane is observed by nobody: no poller
        // outlives the CLI, and the instance dir is cleaned up at stop. The
        // flush is the only thing that keeps it.
        let (_guard, _base, _tmp) = crate::hooks::test_support::BaseGuard::ready();
        let home = tempfile::tempdir().unwrap();
        let _home_guard = crate::session::test_support::isolate_app_dir_at(home.path());

        let profile = "pi-sidecar-flush";
        let mut inst = Instance::new("pi-flush", "/tmp/pi-flush");
        inst.source_profile = profile.to_string();
        inst.tool = "pi".to_string();
        inst.agent_session_id = Some("22f13307-461c-4161-908e-95a247fac750".to_string());
        inst.mark_pi_extension_launched_for_test();

        let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
        let seed = inst.clone();
        storage
            .update(|instances, _| {
                *instances = vec![seed.clone()];
                Ok(())
            })
            .unwrap();

        let published = "01a05234-8889-72e2-a7c9-7ebc27b25b78";
        super::super::test_helpers::publish_host_pi_transcript(&inst.id, published, home.path());

        let _ = inst.flush_published_conversation(&storage);

        assert_eq!(
            storage.load().unwrap()[0].agent_session_id.as_deref(),
            Some(published),
            "the conversation the pane published must outlive its instance dir"
        );
    }

    #[test]
    #[serial_test::serial]
    fn pi_alias_flushes_the_published_conversation() {
        let (_guard, _base, _tmp) = crate::hooks::test_support::BaseGuard::ready();
        let home = tempfile::tempdir().unwrap();
        let _home_guard = crate::session::test_support::isolate_app_dir_at(home.path());
        let profile = "pi-alias-sidecar-flush";
        let mut inst = Instance::new("pi alias", "/tmp/pi-alias");
        inst.source_profile = profile.to_string();
        inst.tool = "company-pi".to_string();
        inst.detect_as = "pi".to_string();
        inst.command = "pi".to_string();
        inst.agent_session_id = Some("old-id".to_string());
        inst.mark_pi_extension_launched_for_test();
        let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
        storage
            .update(|instances, _| {
                *instances = vec![inst.clone()];
                Ok(())
            })
            .unwrap();
        let published = "11111111-1111-4111-8111-111111111111";
        super::super::test_helpers::publish_host_pi_transcript(&inst.id, published, home.path());

        inst.flush_published_if_present();

        assert_eq!(
            storage.load().unwrap()[0].agent_session_id.as_deref(),
            Some(published)
        );
    }

    use super::*;

    /// Real-tmux integration for #3157: a session whose stored title moved
    /// without its tmux session being renamed (smart rename, or a manual
    /// rename whose tmux rename failed) must still be resolvable, so teardown
    /// stops the running agent instead of a name that never existed, and a
    /// later start adopts the live session instead of spawning a second one.
    // Serialized for the same reason as its neighbours: it creates and kills a
    // real tmux session on the shared test server.
    #[test]
    #[serial_test::serial]
    fn retitled_session_is_still_resolved_and_torn_down() {
        if crate::tmux::tmux_command().arg("-V").output().is_err() {
            eprintln!("tmux not available; skipping");
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let profile = "retitled-session-teardown";
        let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();

        let mut inst = Instance::new("Vikings", "/tmp/test");
        inst.source_profile = profile.to_string();
        storage
            .update(|instances, _groups| {
                instances.push(inst.clone());
                Ok(())
            })
            .unwrap();
        let created_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
        let _ = crate::tmux::tmux_command()
            .args(["kill-session", "-t", &created_name])
            .output();
        let created = crate::tmux::tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                &created_name,
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

        // The rename that never reached tmux.
        inst.title = "Refactor billing module".to_string();
        let derived = crate::tmux::Session::generate_name(&inst.id, &inst.title);
        assert_ne!(derived, created_name, "the derived name must have moved");

        let session = inst.tmux_session().expect("tmux_session");
        assert_eq!(
            session.name(),
            created_name,
            "lifecycle ops must resolve onto the live session, not the new derived name"
        );
        assert!(
            session.exists(),
            "the live session is reachable under the new title, so `create` adopts it \
             rather than spawning a second agent"
        );

        inst.kill().expect("kill");
        crate::tmux::refresh_session_cache();
        assert!(
            !crate::tmux::session_exists(&created_name),
            "teardown must stop the agent that is actually running"
        );
    }
}
