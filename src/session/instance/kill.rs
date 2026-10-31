//! Tearing a session down.

use super::*;
pub(crate) struct PiSidecarUpdate {
    session_id: Option<String>,
    path: Option<String>,
}

impl PiSidecarUpdate {
    pub(crate) fn apply(self, instance: &mut Instance) {
        if let Some(id) = self.session_id {
            instance.agent_session_id = Some(id);
        }
        if let Some(path) = self.path {
            instance.pi_session_path = Some(path);
        }
    }
}

fn combine_teardown_results(pane: Result<()>, native: Result<()>) -> Result<()> {
    match (pane, native) {
        (Err(pane), Err(native)) => {
            Err(native.context(format!("tmux teardown also failed: {pane:#}")))
        }
        (Err(error), _) | (_, Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

impl Instance {
    pub(crate) fn flush_published_conversation(
        &self,
        storage: &dyn crate::session::SessionStore,
    ) -> Option<SidWrite> {
        let observation = if self.prime_runtime().is_some() {
            match self.prime_root_publication(true)? {
                crate::session::capture::PrimeRootPublication::Ready(id) => {
                    self.prime_root_observation(id)
                }
                crate::session::capture::PrimeRootPublication::Pending(id) => {
                    if self.is_capture_excluded(
                        &id,
                        self.active_execution.as_ref().map(|active| &active.binding),
                    ) {
                        return None;
                    }
                    let expected = self.conversation_state();
                    return Some(
                        storage
                            .update(|rows, _| {
                                let Some(row) = rows.iter_mut().find(|row| row.id == self.id)
                                else {
                                    return Ok(SidWrite::Failed);
                                };
                                if !expected.matches(row) {
                                    return Ok(SidWrite::Skipped);
                                }
                                if !row.resume_intent.is_default() {
                                    return Ok(SidWrite::PinnedForeign);
                                }
                                row.set_agent_conversation(None, None, None);
                                Ok(SidWrite::Applied)
                            })
                            .unwrap_or(SidWrite::Failed),
                    );
                }
            }
        } else {
            self.final_publication_observation()?
        };
        if self.is_capture_excluded(&observation.sid, observation.source()) {
            return None;
        }
        let outcome = super::sid_persist::persist_session_with_storage(
            storage,
            &self.id,
            &observation,
            &self.conversation_state(),
        )
        .outcome;
        if outcome != SidWrite::Skipped {
            return Some(outcome);
        }
        // Retry a peer CAS once; a second divergence retains the evidence.
        let Ok(rows) = storage.load() else {
            return Some(SidWrite::Failed);
        };
        let Some(retry) = rows.into_iter().find(|row| row.id == self.id) else {
            return Some(SidWrite::Failed);
        };
        Some(
            super::sid_persist::persist_session_with_storage(
                storage,
                &self.id,
                &observation,
                &retry.conversation_state(),
            )
            .outcome,
        )
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
                runtime: Some(_),
                ..
            })
        ) {
            self.prime_published_conversation()
        } else {
            None
        }
    }

    pub(crate) fn read_pi_sidecar_update(&self) -> Option<PiSidecarUpdate> {
        if !self.uses_pi_session_sidecar() {
            return None;
        }
        let observation = self.pi_published_conversation(true)?;
        let published = observation.sid;
        let path = observation.pi_session_path;
        if self.agent_session_id.as_deref() == Some(published.as_str()) {
            if path.is_none() || path == self.pi_session_path {
                return None;
            }
            return Some(PiSidecarUpdate {
                session_id: None,
                path,
            });
        }
        Some(PiSidecarUpdate {
            session_id: Some(published),
            path,
        })
    }

    pub(super) fn kill_clean_locked(&self) -> Result<()> {
        use crate::tmux::utils::{probe_pane, PaneProbe};
        self.capture_poller.stop();
        let pane = self.tmux_session().and_then(|session| {
            match probe_pane(session.name()) {
                PaneProbe::Missing => return Ok(()),
                PaneProbe::Unknown => anyhow::bail!(
                    "cannot prove tmux pane state for {} before teardown",
                    session.name()
                ),
                PaneProbe::Alive => {}
                PaneProbe::Dead => {
                    let shell = crate::session::environment::user_shell();
                    if let Err(error) = session.respawn_dead_pane(&self.project_path, Some(&shell)) {
                        tracing::warn!(target: "session.store", %error, "respawn_dead_pane failed before teardown");
                    }
                }
            }
            session.kill()?;
            std::thread::sleep(std::time::Duration::from_millis(100));
            Ok(())
        });
        let native = self
            .prime_runtime()
            .map_or(Ok(()), |runtime| runtime.cleanup());
        combine_teardown_results(pane, native)
    }

    pub(crate) fn kill_clean_in(&self, storage: &dyn crate::session::SessionStore) -> Result<u64> {
        storage.check_available()?;
        let _lifecycle_lock = storage
            .storage()
            .acquire_instance_lifecycle_lock(&self.id)
            .context("failed to acquire instance kill lock")?;
        let mut lifecycle = self.clone();
        let generation =
            lifecycle.acquire_lifecycle_reservation(storage, LifecycleOperation::Stop, None)?;
        match self.kill_clean_locked() {
            Ok(()) => {
                lifecycle.commit_lifecycle_status(
                    storage,
                    LifecycleOperation::Stop,
                    generation,
                    Status::Stopped,
                )?;
                Ok(generation)
            }
            Err(error) => {
                if let Err(commit_error) = lifecycle.commit_lifecycle_status(
                    storage,
                    LifecycleOperation::Stop,
                    generation,
                    Status::Error,
                ) {
                    tracing::warn!(target: "session.lifecycle", session = %self.id, error = %commit_error,
                        "failed to persist Error after stop failure"
                    );
                }
                Err(error)
            }
        }
    }

    pub(crate) fn kill_locked(&self) -> Result<()> {
        self.capture_poller.stop();
        let pane = self.tmux_session().and_then(|session| session.kill());
        let native = self
            .prime_runtime()
            .map_or(Ok(()), |runtime| runtime.cleanup());
        combine_teardown_results(pane, native)
    }

    pub fn kill(&self) -> Result<()> {
        let profile = self.effective_profile();
        let storage = crate::session::storage::Storage::new(&profile, self.resolve_file_watch())
            .context("failed to open lifecycle lock storage")?;
        let _lifecycle_lock = storage
            .acquire_instance_lifecycle_lock(&self.id)
            .context("failed to acquire instance kill lock")?;
        let mut lifecycle = storage
            .load()?
            .into_iter()
            .find(|row| row.id == self.id)
            .context("session disappeared before coordinated kill")?;
        let generation =
            lifecycle.acquire_lifecycle_reservation(&storage, LifecycleOperation::Stop, None)?;
        match lifecycle
            .kill_locked()
            .and_then(|()| lifecycle.release_scope())
        {
            Ok(()) => lifecycle.commit_lifecycle_status(
                &storage,
                LifecycleOperation::Stop,
                generation,
                Status::Stopped,
            ),
            Err(error) => {
                if let Err(commit_error) = lifecycle.commit_lifecycle_status(
                    &storage,
                    LifecycleOperation::Stop,
                    generation,
                    Status::Error,
                ) {
                    tracing::warn!(target: "session.lifecycle", session = %self.id, error = %commit_error,
                        "failed to persist Error after stop failure"
                    );
                }
                Err(error)
            }
        }
    }

    /// Best-effort coordinated stop of the agent and every ancillary tmux session.
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
        self.stop_all_tmux_sessions_locked(&storage);
    }

    /// [`Self::kill_all_tmux_sessions`] for a caller already holding this instance's lifecycle
    /// lock, so it can commit its next state (an archive) before a peer launch or send gets in.
    pub(crate) fn stop_all_tmux_sessions_locked(&self, storage: &crate::session::storage::Storage) {
        let mut lifecycle = match storage.load().and_then(|rows| {
            rows.into_iter()
                .find(|row| row.id == self.id)
                .context("session disappeared before coordinated stop")
        }) {
            Ok(row) => row,
            Err(error) => {
                tracing::warn!(target: "session.tmux_cleanup", session_id = %self.id, %error, "kill_all_tmux_sessions: current row unavailable");
                return;
            }
        };
        if let Err(error) =
            lifecycle.acquire_lifecycle_reservation(storage, LifecycleOperation::Stop, None)
        {
            tracing::warn!(
                target: "session.tmux_cleanup",
                session_id = %self.id,
                %error,
                "kill_all_tmux_sessions: lifecycle reservation failed"
            );
            return;
        }
        let stopped = lifecycle.kill_all_tmux_sessions_locked();
        if let Err(error) = &stopped {
            tracing::warn!(target: "session.tmux_cleanup", session_id = %self.id, %error, "tmux teardown failed");
        }
        if let Err(error) = lifecycle.commit_lifecycle_status(
            storage,
            LifecycleOperation::Stop,
            lifecycle.lifecycle_generation,
            if stopped.is_ok() {
                Status::Stopped
            } else {
                Status::Error
            },
        ) {
            tracing::warn!(
                target: "session.tmux_cleanup",
                session_id = %self.id,
                %error,
                "kill_all_tmux_sessions: lifecycle commit failed"
            );
        }
    }

    /// Caller holds the per-instance lifecycle lock through teardown and commit.
    pub(crate) fn kill_all_tmux_sessions_locked(&self) -> Result<()> {
        self.kill_all_tmux_sessions_uncoordinated()
    }

    /// Caller has excluded launches for an id without a durable lifecycle row.
    pub(crate) fn kill_all_tmux_sessions_without_lifecycle_row(&self) -> Result<()> {
        self.kill_all_tmux_sessions_uncoordinated()
    }

    fn kill_all_tmux_sessions_uncoordinated(&self) -> Result<()> {
        // The ancillary sessions run their own processes, so they are reaped
        // even when the agent kill fails: short-circuiting leaks a shell per
        // failed kill. The agent failure is what the caller reports.
        let agent = self.kill_locked();
        let ancillary = self.kill_ancillary_tmux_sessions_locked();
        let scope = self.release_scope();
        let stopped = match (agent, ancillary) {
            (Ok(()), Ok(())) => Ok(()),
            (Ok(()), Err(ancillary)) => Err(ancillary).context("ancillary tmux teardown failed"),
            (Err(agent), Ok(())) => Err(agent),
            (Err(agent), Err(ancillary)) => Err(agent.context(format!(
                "ancillary tmux teardown also failed: {ancillary:#}"
            ))),
        };
        match (stopped, scope) {
            (Ok(()), scope) => scope,
            (Err(error), Ok(())) => Err(error),
            (Err(error), Err(scope)) => {
                Err(error.context(format!("scope retirement also failed: {scope:#}")))
            }
        }
    }

    pub(crate) fn kill_ancillary_tmux_sessions_locked(&self) -> Result<()> {
        crate::tmux::utils::kill_ancillary_sessions_for_id(&self.id)
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
        if let Err(error) = self.kill_ancillary_tmux_sessions_locked() {
            tracing::warn!(target: "session.tmux_cleanup", session_id = %self.id, %error, "ancillary tmux teardown failed");
        }
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

    pub(crate) fn stop_resources_locked(&self) -> Result<()> {
        self.kill_locked()?;
        self.release_scope()?;
        crate::session::worktree_edit::stop_sandbox_container(&self.id, self.is_sandboxed())
    }

    /// Stops the session's scope, sweeping processes that escaped the pane.
    /// Structured sessions are released by the ACP teardown.
    pub(crate) fn release_scope(&self) -> Result<()> {
        if self.is_structured() {
            return Ok(());
        }
        let scopes = crate::process::systemd::resolve_unknown_scopes(
            &self.id,
            self.terminal_scopes.as_deref(),
        )?;
        crate::process::systemd::retire_scopes(&self.id, &scopes)
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
        let generation = lifecycle.acquire_lifecycle_reservation(
            &storage,
            LifecycleOperation::Stop,
            Some(Status::Stopped),
        )?;
        self.capture_poller.stop();
        let teardown = (|| {
            let mut current = storage
                .load()?
                .into_iter()
                .find(|row| row.id == self.id)
                .context("session disappeared during stop")?;
            current.source_profile = profile.clone();
            anyhow::ensure!(
                current.lifecycle_reservation_is_owned(LifecycleOperation::Stop, generation),
                "Session changed during stop; resources retained"
            );
            let flushed = current.flush_published_conversation(&storage);
            if current.prime_runtime().is_some()
                && matches!(flushed, Some(SidWrite::Failed | SidWrite::Skipped))
            {
                anyhow::bail!(
                    "could not persist final conversation publication; native evidence retained"
                );
            }
            current.kill_locked()?;
            current.release_scope()?;
            let container_result = crate::session::worktree_edit::stop_sandbox_container(
                &current.id,
                current.is_sandboxed(),
            );
            match flushed {
                Some(SidWrite::PinnedForeign) | Some(SidWrite::OwnershipConflict) => {
                    container_result
                }
                Some(SidWrite::Applied) | None => container_result,
                Some(SidWrite::Failed) | Some(SidWrite::Skipped) => {
                    container_result?;
                    anyhow::bail!(
                        "could not persist final conversation publication; hook evidence retained"
                    )
                }
            }
        })();
        match teardown {
            Ok(()) => {
                lifecycle.commit_lifecycle_status(
                    &storage,
                    LifecycleOperation::Stop,
                    generation,
                    Status::Stopped,
                )?;

                crate::hooks::cleanup_hook_status_dir(&self.id);
                Ok(())
            }
            Err(error) => {
                if let Err(commit_error) = lifecycle.commit_lifecycle_status(
                    &storage,
                    LifecycleOperation::Stop,
                    generation,
                    Status::Error,
                ) {
                    tracing::warn!(target: "session.lifecycle", session = %self.id, error = %commit_error,
                        "failed to persist Error after stop failure"
                    );
                }
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    #[cfg(target_os = "linux")]
    #[test]
    #[serial_test::serial]
    fn cached_terminal_stop_uses_the_newer_rows_scope_and_refuses_unproven_exit() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let bin = temp.path().join("manager-bin");
        std::fs::create_dir(&bin).unwrap();
        let marker = temp.path().join("manager-called");
        let manager = bin.join("systemctl");
        std::fs::write(
            &manager,
            format!(
                "#!/bin/sh\nprintf '%s\n' \"$*\" > '{}'\nexit 1\n",
                marker.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&manager, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let _path = crate::session::test_support::EnvGuard::set(&[("PATH", path.as_str())]);
        for operation in ["kill", "stop", "kill_all"] {
            let profile = "fresh-terminal-stop";
            let storage = crate::session::Storage::new_unwatched(profile).unwrap();
            let mut cached = Instance::new("cached scope", temp.path().to_str().unwrap());
            cached.source_profile = profile.into();
            storage
                .update(|rows, _| {
                    rows.push(cached.clone());
                    Ok(())
                })
                .unwrap();
            let unit = format!("aoe-session-{}-newer.scope", cached.id.replace('-', "_2d"));
            storage
                .update(|rows, _| {
                    let current = rows.iter_mut().find(|row| row.id == cached.id).unwrap();
                    current.terminal_scopes = Some(vec![
                        crate::process::worker_registry::ScopeOwnership::Owned {
                            unit: unit.clone(),
                        },
                    ]);
                    current.lifecycle_generation += 1;
                    Ok(())
                })
                .unwrap();
            let name = crate::tmux::Session::generate_name(&cached.id, &cached.title);
            let created = crate::tmux::tmux_command()
                .args(["new-session", "-d", "-s", &name, "sleep 60"])
                .output()
                .unwrap();
            assert!(
                created.status.success(),
                "{}",
                String::from_utf8_lossy(&created.stderr)
            );
            crate::tmux::refresh_session_cache();
            match operation {
                "kill_all" => cached.kill_all_tmux_sessions(),
                "stop" => {
                    cached
                        .stop()
                        .expect_err("the current scope lacks empty-cgroup proof");
                }
                "kill" => {
                    cached
                        .kill()
                        .expect_err("the current scope lacks empty-cgroup proof");
                }
                _ => unreachable!(),
            }
            assert!(
                !crate::tmux::session_exists(&name),
                "tmux teardown must really run before the manager refusal"
            );
            assert!(
                std::fs::read_to_string(&marker).unwrap().contains(&unit),
                "the authoritative newer unit, not the stale empty list, must be queried"
            );
            let current = storage
                .load()
                .unwrap()
                .into_iter()
                .find(|row| row.id == cached.id)
                .unwrap();
            assert_eq!(current.status, Status::Error);
            assert!(current.lifecycle_reservation.is_none());
            assert_eq!(
                current.terminal_scopes.unwrap(),
                vec![crate::process::worker_registry::ScopeOwnership::Owned { unit }]
            );
            std::fs::remove_file(&marker).unwrap();
        }
    }
    #[test]
    #[serial_test::serial]
    fn prime_cleanup_failure_still_tears_down_the_pane_and_retains_ownership() {
        if crate::tmux::tmux_command().arg("-V").output().is_err() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(root.path());
        let profile = "prime-pane-cleanup-failure";
        let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
        for clean in [false, true] {
            let mut inst = Instance::new("Prime cleanup failure", root.path().to_str().unwrap());
            inst.source_profile = profile.into();
            let runtime = crate::process::prime::PrimeRuntime::new(
                "/bin/false".into(),
                root.path().into(),
                None,
            )
            .unwrap();
            runtime.prepare().unwrap();
            inst.active_execution = Some(super::super::execution::ActiveExecution {
                launch_id: "failed-cleanup".into(),
                binding: super::super::execution::ExecutionBinding {
                    agent: "prime-agent".into(),
                    stores: vec![],
                    configuration: vec![],
                    cwd: root.path().into(),
                    cwd_filesystem: "host".into(),
                    filesystem: "host".into(),
                    exported_default_store: None,
                },
                capture: Some(CaptureContext::PrimeStatus(runtime.clone())),
                container: None,
            });
            storage
                .update(|instances, _| {
                    instances.push(inst.clone());
                    Ok(())
                })
                .unwrap();
            let name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
            struct Pane(String);
            impl Drop for Pane {
                fn drop(&mut self) {
                    let _ = crate::tmux::tmux_command()
                        .args(["kill-session", "-t", &self.0])
                        .output();
                }
            }
            let _pane = Pane(name.clone());
            assert!(crate::tmux::tmux_command()
                .args(["new-session", "-d", "-s", &name, "sleep 60"])
                .status()
                .unwrap()
                .success());
            crate::tmux::refresh_session_cache();
            let result = if clean {
                inst.kill_clean_in(&storage).map(|_| ())
            } else {
                inst.kill()
            };
            crate::tmux::refresh_session_cache();
            assert!(result.is_err());
            assert!(
                !crate::tmux::session_exists(&name),
                "cleanup error left the pane running"
            );
            let persisted = storage
                .load()
                .unwrap()
                .into_iter()
                .find(|row| row.id == inst.id)
                .unwrap();
            assert_eq!(persisted.status, Status::Error);
            assert_eq!(
                persisted.prime_runtime().unwrap().directory,
                runtime.directory
            );
            assert!(
                runtime.directory.exists(),
                "unproven native route must remain recoverable"
            );
            std::fs::remove_dir_all(&runtime.directory).unwrap();
        }
    }

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
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn prime_stop_flushes_the_owned_root_before_disposal_and_retains_failed_publication_evidence() {
        use std::os::unix::fs::PermissionsExt;
        for conflict in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
            let project = temp.path().join("project");
            let store = temp.path().join("prime-store");
            let sessions = store.join("sessions");
            std::fs::create_dir(&project).unwrap();
            std::fs::create_dir_all(&sessions).unwrap();
            let storage = crate::session::Storage::new_unwatched("prime-stop").unwrap();
            let sid = uuid::Uuid::new_v4().to_string();
            let transcript = sessions.join(format!("{sid}.jsonl"));
            std::fs::write(
                &transcript,
                serde_json::json!({"type":"session","id":sid,"rlmDepth":0,"cwd":project})
                    .to_string()
                    + "\n",
            )
            .unwrap();
            let observation = serde_json::json!({"sessions":[{
                "sessionId":sid,"sessionFile":transcript,"cwd":project,
                "activeSessionId":sid,"rlmDepth":0,"attachedClients":1,
                "workerState":"ready","lifecycle":"running","activity":"idle",
                "isStreaming":false,"isCompacting":false,"hasRunningSubagents":false,
                "isBashRunning":false,"isSessionActive":false
            }]});
            let at_shutdown = temp.path().join("at-shutdown.json");
            let program = temp.path().join("native-prime");
            std::fs::write(&program, format!(
                "#!/bin/sh\ncase \"$1\" in\nlist) printf '%s' {};;\nshutdown) cat {} > {}; printf '%s' '{{\"failed\":[]}}';;\n*) exit 2;;\nesac\n",
                crate::session::environment::shell_escape(&observation.to_string()),
                crate::session::environment::shell_escape(storage.sessions_path().to_str().unwrap()),
                crate::session::environment::shell_escape(at_shutdown.to_str().unwrap()),
            )).unwrap();
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
            let runtime =
                crate::process::prime::PrimeRuntime::new(program, store.clone(), None).unwrap();
            runtime.prepare().unwrap();
            struct PrivateRuntimeRoot(std::path::PathBuf);
            impl Drop for PrivateRuntimeRoot {
                fn drop(&mut self) {
                    let _ = std::fs::remove_dir_all(&self.0);
                }
            }
            let _runtime_root = PrivateRuntimeRoot(runtime.directory.clone());
            let binding = ExecutionBinding {
                agent: "prime-agent".into(),
                stores: vec![store.clone()],
                configuration: Vec::new(),
                cwd: project.clone(),
                cwd_filesystem: "host".into(),
                filesystem: "host".into(),
                exported_default_store: None,
            };
            let mut instance = Instance::new("owned Prime root", project.to_str().unwrap());
            instance.source_profile = "prime-stop".into();
            instance.tool = "prime-agent".into();
            instance.status = Status::Running;
            instance.terminal_scopes = Some(Vec::new());
            if conflict {
                let mut pinned = binding.clone();
                pinned.cwd = temp.path().join("other-project");
                instance.resume_intent = ResumeIntent::Use(sid.clone());
                instance.resume_binding = Some(ConversationBinding {
                    session_id: sid.clone(),
                    execution: Some(pinned),
                    provenance: ConversationProvenance::Asserted,
                    transcript_path: None,
                });
                instance.set_agent_conversation(
                    Some(sid.clone()),
                    Some(ConversationBinding {
                        session_id: sid.clone(),
                        execution: Some(binding.clone()),
                        provenance: ConversationProvenance::Observed,
                        transcript_path: None,
                    }),
                    None,
                );
            }
            instance.active_execution = Some(ActiveExecution {
                launch_id: uuid::Uuid::new_v4().to_string(),
                binding,
                capture: Some(CaptureContext::Prime {
                    plan: crate::session::instance::PrimeAgentCapturePlan {
                        store,
                        session_dir: "sessions".into(),
                        container_session_dir: sessions,
                        container_cwd: project.to_str().unwrap().into(),
                    },
                    runtime: Some(runtime.clone()),
                }),
                container: None,
            });
            storage
                .update(|rows, _| {
                    rows.push(instance.clone());
                    Ok(())
                })
                .unwrap();
            let result = instance.stop();
            if conflict {
                assert!(result.is_err());
                assert!(
                    runtime.directory.exists(),
                    "failed persistence retains its observation route"
                );
                assert!(
                    !at_shutdown.exists(),
                    "native cleanup cannot precede successful publication persistence"
                );
                assert_eq!(
                    storage.load().unwrap()[0].resume_intent,
                    ResumeIntent::Use(sid)
                );
                runtime.cleanup().unwrap();
            } else {
                result.unwrap();
                assert!(
                    !runtime.directory.exists(),
                    "the native route is actually disposed"
                );
                let before_cleanup: Vec<Instance> =
                    serde_json::from_slice(&std::fs::read(at_shutdown).unwrap()).unwrap();
                assert_eq!(
                    before_cleanup[0].agent_session_id.as_deref(),
                    Some(sid.as_str())
                );
                let stored = storage.load().unwrap();
                assert_eq!(stored[0].agent_session_id.as_deref(), Some(sid.as_str()));
                assert_eq!(stored[0].status, Status::Stopped);
            }
        }
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
            cwd: home.path().into(),
            cwd_filesystem: "host".into(),
            filesystem: "host".into(),
            exported_default_store: None,
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

    /// Agent failure must not prevent independent ancillary cleanup.
    #[test]
    #[serial_test::serial]
    #[cfg(unix)]
    fn a_failed_agent_kill_still_reaps_ancillary_sessions() {
        use std::os::unix::fs::PermissionsExt;
        let Ok(real_tmux) = which::which("tmux") else {
            return;
        };
        let _env_read = crate::session::test_support::EnvGuard::read_lock();
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let _socket = crate::session::test_support::EnvGuard::set(&[(
            "AOE_TMUX_SOCKET",
            temp.path().join("tmux.sock").display().to_string(),
        )]);
        crate::tmux::tool_retirement::initialize(&crate::session::get_app_dir().unwrap()).unwrap();
        let inst = Instance::new("Ancillary teardown", temp.path().to_str().unwrap());
        let agent = crate::tmux::Session::generate_name(&inst.id, &inst.title);
        let terminal = crate::tmux::TerminalSession::generate_name(&inst.id, "web terminal");
        let _agent = crate::tmux::test_helpers::TmuxTestSession::from_name(agent.clone());
        let _terminal = crate::tmux::test_helpers::TmuxTestSession::from_name(terminal.clone());
        for name in [&agent, &terminal] {
            let output = crate::tmux::tmux_command()
                .args([
                    "-f",
                    "/dev/null",
                    "new-session",
                    "-d",
                    "-s",
                    name,
                    "sleep 60",
                ])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let shim = temp.path().join("tmux");
        std::fs::write(
            &shim,
            format!(
                "#!/bin/sh\nblocked={}\ncase \" $* \" in\n  *\" kill-session \"*|*\" if-shell \"*)\n    case \"$*\" in\n      *\"$blocked\"*) echo 'permission denied' >&2; exit 1 ;;\n    esac\n    ;;\nesac\nexec {} \"$@\"\n",
                crate::session::environment::shell_escape(&agent),
                crate::session::environment::shell_escape(&real_tmux.to_string_lossy()),
            ),
        ).unwrap();
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = crate::session::test_support::path_prepended(temp.path());

        inst.kill_all_tmux_sessions_locked()
            .expect_err("the refused agent kill is reported to the caller");
        let remaining = crate::tmux::tmux_query_command()
            .args(["has-session", "-t", &format!("={terminal}")])
            .output()
            .unwrap();
        assert!(
            !remaining.status.success(),
            "the ancillary tmux session survived"
        );
        drop(path);
    }

    /// A takeover relaunch can replace the row's pane while its old poller keeps the same agent and
    /// the same absence of execution, so identity alone accepts it. The pane is what tells the two
    /// apart, and the poller goes before the pane does.
    #[test]
    #[serial_test::serial]
    fn tearing_down_a_pane_stops_the_poller_watching_it() {
        if crate::tmux::tmux_command()
            .arg("-V")
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true)
        {
            eprintln!("tmux not available; skipping");
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let profile = "pane-teardown-stops-poller";
        let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
        let mut inst = Instance::new("pane-teardown", "/tmp/test");
        inst.source_profile = profile.to_string();
        storage
            .update(|instances, _groups| {
                instances.push(inst.clone());
                Ok(())
            })
            .unwrap();

        let name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
        let _ = crate::tmux::tmux_command()
            .args(["kill-session", "-t", &name])
            .output();
        let created = crate::tmux::tmux_command()
            .args(["new-session", "-d", "-s", &name, "-x", "80", "-y", "24"])
            .output()
            .expect("tmux");
        assert!(created.status.success(), "the test needs a real pane");

        let mut poller = crate::session::poller::SessionPoller::new(
            name.clone(),
            inst.tool.clone(),
            inst.active_execution.clone(),
        );
        assert_eq!(
            poller.start(inst.id.clone(), Box::new(|| None), Box::new(|_| {}), None,),
            crate::session::poller::PollerSpawn::Spawned
        );
        inst.capture_poller
            .set_handle(Some(std::sync::Arc::new(std::sync::Mutex::new(poller))));
        assert!(inst.capture_poller.is_running(), "fixture: it is running");
        assert!(
            inst.active_execution.is_none(),
            "fixture: no execution on either side"
        );

        let _ = inst.kill_clean_locked();

        assert!(
            !inst.capture_poller.is_running(),
            "the poller stops before the pane it watches does"
        );
    }

    /// A TUI watching this row treats a pane that dies under a live status as an
    /// agent crash and marks it Error, so the stored status must already read
    /// Stopped when the pane receives its kill signal, as the TUI's own stop does.
    #[test]
    #[serial_test::serial]
    fn stop_stores_stopped_before_the_pane_is_killed() {
        if crate::tmux::tmux_command()
            .arg("-V")
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true)
        {
            eprintln!("tmux not available; skipping");
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let profile = "stop-stores-stopped-first";
        let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
        let mut inst = Instance::new("stop-order", "/tmp/test");
        inst.source_profile = profile.to_string();
        inst.status = Status::Running;
        storage
            .update(|instances, _groups| {
                instances.push(inst.clone());
                Ok(())
            })
            .unwrap();

        let name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
        let _ = crate::tmux::tmux_command()
            .args(["kill-session", "-t", &name])
            .output();
        let created = crate::tmux::tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                &name,
                "-x",
                "80",
                "-y",
                "24",
                "sleep 600",
            ])
            .output()
            .expect("tmux");
        assert!(created.status.success(), "the test needs a real pane");
        let _pane = crate::tmux::test_helpers::TmuxTestSession::from_name(name);
        crate::tmux::refresh_session_cache();

        // Read the stored rows synchronously as the first kill signal is about
        // to be sent; a pane-side trap would race the SIGKILL escalation.
        struct SnapshotAtFirstSignal {
            sessions: std::path::PathBuf,
            snapshot: std::sync::Arc<std::sync::Mutex<Option<String>>>,
        }
        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for SnapshotAtFirstSignal {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _: tracing_subscriber::layer::Context<'_, S>,
            ) {
                if event.metadata().target() != "process.signal" {
                    return;
                }
                let mut snapshot = self.snapshot.lock().unwrap();
                if snapshot.is_none() {
                    *snapshot = Some(std::fs::read_to_string(&self.sessions).unwrap());
                }
            }
        }
        let snapshot = std::sync::Arc::default();
        let layer = SnapshotAtFirstSignal {
            sessions: storage.sessions_path().to_path_buf(),
            snapshot: std::sync::Arc::clone(&snapshot),
        };
        {
            use tracing_subscriber::layer::SubscriberExt;
            let _guard = tracing::subscriber::set_default(
                tracing_subscriber::Registry::default().with(layer),
            );
            tracing::callsite::rebuild_interest_cache();
            inst.stop().expect("stop");
        }

        let at_kill = snapshot
            .lock()
            .unwrap()
            .take()
            .expect("stop signals the pane's process tree");
        let rows: Vec<serde_json::Value> = serde_json::from_str(&at_kill).unwrap();
        let row = rows
            .iter()
            .find(|row| row["id"] == inst.id.as_str())
            .expect("the row is stored");
        assert_eq!(
            row["status"],
            serde_json::to_value(Status::Stopped).unwrap(),
            "the stored status is Stopped before the pane dies"
        );
    }
}
