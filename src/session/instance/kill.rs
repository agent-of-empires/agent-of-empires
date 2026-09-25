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

    pub(crate) fn flush_published_conversation(
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
        // fork intent or another conversation skips again. The retry emits
        // `PinnedForeign` itself when the pin is the cause, so no post-hoc
        // inference is needed here: any remaining `Skipped` keeps the doubt.
        // A retry that cannot read the row leaves the publication in doubt, so
        // it must fail the flush: `None` would read as "nothing to publish" and
        // let teardown delete the evidence.
        let Ok(rows) = storage.load() else {
            return Some(SidWrite::Failed);
        };
        let Some(retry) = rows.into_iter().find(|row| row.id == self.id) else {
            return Some(SidWrite::Failed);
        };
        Some(super::sid_persist::persist_session_with_storage(
            storage,
            &self.id,
            &observation,
            &retry.conversation_state(),
        ))
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
        } else if matches!(
            self.active_execution
                .as_ref()
                .and_then(|active| active.capture.as_ref()),
            Some(CaptureContext::Prime {
                sidecar: Some(_),
                ..
            })
        ) {
            self.prime_published_conversation()
        } else {
            None
        }
    }

    /// Tear down the current tmux session cleanly so a fresh `start_with_size_opts` can recreate
    /// it.
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

    /// Kill every tmux session owned by this instance (agent, web terminal, container terminal,
    /// tool sub-sessions).
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

    /// Kill every tmux session owned by this instance while the caller holds the selected profile's
    /// per-instance lifecycle lock.
    pub(crate) fn kill_all_tmux_sessions_locked(&self) {
        self.kill_all_tmux_sessions_uncoordinated();
    }

    /// Tear down tmux resources when no durable lifecycle row exists.
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

    use super::*;

    /// Real-tmux integration for #3157.
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
    #[test]
    #[serial_test::serial]
    fn stop_flushes_six_hour_old_pi_publication_for_builtin_and_direct_command_alias() {
        let (_guard, _base, _tmp) = crate::hooks::test_support::BaseGuard::ready();
        let home = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(home.path());
        let published = "01a0538e-5868-7c22-84bc-40cfd7a09ab1";
        for (tool, command, already_current) in [
            ("pi", "", false),
            ("company-pi", "pi", false),
            ("pi", "", true),
        ] {
            let profile = format!("pi-old-sidecar-stop-{tool}-{already_current}");
            let mut inst = Instance::new(tool, home.path().to_str().unwrap());
            inst.source_profile = profile.clone();
            inst.tool = tool.into();
            inst.command = command.into();
            inst.detect_as = "pi".into();
            inst.agent_session_id = Some(if already_current {
                published.into()
            } else {
                "22f13307-461c-4161-908e-95a247fac750".into()
            });
            let storage = crate::session::storage::Storage::new_unwatched(&profile).unwrap();
            storage
                .update(|rows, _| {
                    rows.push(inst.clone());
                    Ok(())
                })
                .unwrap();
            let transcript = crate::session::instance::test_helpers::publish_host_pi_transcript(
                &inst.id,
                published,
                home.path(),
            );
            let sidecar = crate::hooks::ensure_instance_dir_path(&inst.id)
                .unwrap()
                .join("session_id");
            std::fs::File::options()
                .write(true)
                .open(&sidecar)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(
                    std::time::SystemTime::now() - std::time::Duration::from_secs(6 * 3600),
                ))
                .unwrap();
            assert_eq!(crate::hooks::read_hook_session_id(&inst.id), None);
            assert_eq!(
                inst.final_publication_observation()
                    .as_ref()
                    .map(|o| o.sid.as_str()),
                Some(published),
                "{tool}"
            );
            inst.stop()
                .unwrap_or_else(|error| panic!("{tool}: {error}"));
            let disk = storage.load().unwrap();
            assert_eq!(
                disk.iter()
                    .find(|row| row.id == inst.id)
                    .unwrap()
                    .agent_session_id
                    .as_deref(),
                Some(published),
                "{tool}"
            );
            assert_eq!(
                disk.iter()
                    .find(|row| row.id == inst.id)
                    .unwrap()
                    .pi_session_path
                    .as_deref(),
                transcript.to_str(),
                "{tool}, already_current={already_current}"
            );
            assert!(
                !sidecar.exists(),
                "sidecar must be cleaned after path persists"
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn failed_stop_publication_keeps_hook_evidence() {
        let (_guard, _base, _tmp) = crate::hooks::test_support::BaseGuard::ready();
        let home = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(home.path());
        let profile = "failed-stop-evidence";
        let mut inst = Instance::new("claude-stop", home.path().to_str().unwrap());
        inst.source_profile = profile.into();
        inst.tool = "claude".into();
        let sid = "22f13307-461c-4161-908e-95a247fac780";
        let launch = uuid::Uuid::new_v4().to_string();
        let sidecar = crate::hooks::ensure_instance_dir_path(&inst.id)
            .unwrap()
            .join(
                crate::hooks::session_id_leaf(Some(&launch))
                    .unwrap()
                    .as_ref(),
            );
        let mut execution = crate::session::ExecutionBinding {
            agent: "claude".into(),
            stores: vec![home.path().join("store")],
            configuration: Vec::new(),
            exported_default_store: false,
            cwd: home.path().into(),
            cwd_filesystem: "host".into(),
            filesystem: "host".into(),
        };
        inst.resume_intent = ResumeIntent::Use(sid.into());
        inst.resume_binding = Some(crate::session::ConversationBinding {
            session_id: sid.into(),
            execution: Some(execution.clone()),
            provenance: crate::session::ConversationProvenance::Asserted,
            transcript_path: None,
        });
        execution.cwd = home.path().join("elsewhere");
        inst.set_agent_conversation(
            Some(sid.into()),
            Some(crate::session::ConversationBinding {
                session_id: sid.into(),
                execution: Some(execution.clone()),
                provenance: crate::session::ConversationProvenance::Observed,
                transcript_path: None,
            }),
            None,
        );
        inst.active_execution = Some(ActiveExecution {
            launch_id: launch.clone(),
            binding: execution,
            capture: Some(CaptureContext::Hooks(sidecar.clone())),
            container: None,
        });
        let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
        storage
            .update(|rows, _| {
                rows.push(inst.clone());
                Ok(())
            })
            .unwrap();
        crate::hooks::write_session_id_via_guard(&inst.id, sid, Some(&launch)).unwrap();
        assert!(sidecar.exists());
        assert!(inst.stop().is_err());
        assert!(sidecar.exists(), "stop must retain recovery evidence");
        assert_eq!(
            storage.load().unwrap()[0].resume_intent,
            ResumeIntent::Use(sid.into())
        );
    }
    #[test]
    #[serial_test::serial]
    fn stop_cleans_foreign_publication_evidence_without_changing_pin() {
        let (_guard, _base, _tmp) = crate::hooks::test_support::BaseGuard::ready();
        let home = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(home.path());
        let profile = "stop-pinned-foreign";
        let published = "01a0538e-5868-7c22-84bc-40cfd7a09ab1";
        let pinned = "22f13307-461c-4161-908e-95a247fac750";
        let mut inst = Instance::new("pi-pinned", home.path().to_str().unwrap());
        inst.source_profile = profile.into();
        inst.tool = "pi".into();
        inst.agent_session_id = Some(pinned.into());
        inst.resume_intent = ResumeIntent::Use(pinned.into());
        let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
        storage
            .update(|rows, _| {
                rows.push(inst.clone());
                Ok(())
            })
            .unwrap();
        let _transcript = crate::session::instance::test_helpers::publish_host_pi_transcript(
            &inst.id,
            published,
            home.path(),
        );
        let sidecar = crate::hooks::ensure_instance_dir_path(&inst.id)
            .unwrap()
            .join("session_id");
        assert!(sidecar.exists());
        assert_eq!(inst.final_publication_observation().unwrap().sid, published);
        inst.stop().unwrap();
        let row = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|row| row.id == inst.id)
            .unwrap();
        assert_eq!(row.agent_session_id.as_deref(), Some(pinned));
        assert_eq!(row.resume_intent, ResumeIntent::Use(pinned.into()));
        assert_eq!(row.status, Status::Stopped);
        assert!(!sidecar.exists());
    }
}
