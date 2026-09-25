use std::sync::Arc;

use anyhow::{Context, Result};

use crate::session::{Instance, NativeStoreUnavailable, SessionMutation, SessionStore, Storage};

use super::state::AppState;

// The caller retains namespace and operation exclusion on a blocking worker.
pub(crate) struct NativeSessionStore {
    storage: Storage,
    state: Arc<AppState>,
    runtime: tokio::runtime::Handle,
    status_id: Option<String>,
}

impl NativeSessionStore {
    pub(crate) fn open(
        state: Arc<AppState>,
        profile: &str,
        status_id: Option<String>,
    ) -> Result<Self> {
        ensure_healthy(&state)?;
        let storage = match Storage::open(profile, state.file_watch.clone()) {
            Ok(storage) => storage,
            Err(error) => {
                let publication = state.publication.blocking_write();
                mark_profile_failure(&state, profile, &error, &publication);
                return Err(error);
            }
        };
        Ok(Self {
            storage,
            state,
            runtime: tokio::runtime::Handle::current(),
            status_id,
        })
    }

    pub(crate) fn set_status_id(&mut self, id: String) {
        self.status_id = Some(id);
    }

    pub(crate) fn adopt_runtime_fields(&self, instance: &Instance) -> Result<()> {
        self.update_runtime_fields(instance, |row| {
            super::reload::merge_runtime_fields(instance.clone(), row);
        })
    }

    /// The caller retains title/lifecycle exclusion through observation and adoption.
    pub(crate) fn adopt_auxiliary_observations(
        &self,
        instance: &Instance,
        observations: impl IntoIterator<Item = crate::session::AuxiliaryObservation>,
    ) -> Result<()> {
        self.update_runtime_fields(instance, |row| {
            for observation in observations {
                if let Some(current) = row
                    .auxiliary
                    .iter_mut()
                    .find(|current| current.target == observation.target)
                {
                    *current = observation;
                } else {
                    row.auxiliary.push(observation);
                }
            }
        })
    }

    pub(crate) fn adopt_agent_observation(
        &self,
        instance: &Instance,
        observation: crate::session::PaneObservation,
    ) -> Result<()> {
        self.update_runtime_fields(instance, |row| row.agent_pane = observation)
    }

    pub(crate) fn refresh_pane_observations(&self, instance: &Instance) -> Result<()> {
        self.check_available()?;
        let panes = match crate::tmux::batch_pane_metadata() {
            Ok(panes) => Some(panes),
            Err(error) => {
                tracing::warn!(target: "server.session_store", session = %instance.id, "post-effect pane observation failed: {error}");
                None
            }
        };
        self.update_runtime_fields(instance, |row| {
            let metadata = self.state.canonical_metadata.blocking_read();
            let tools = metadata
                .auxiliary_tools
                .get(&row.source_profile)
                .map(Vec::as_slice)
                .unwrap_or_default();
            super::pane::sample_panes(row, tools, panes.as_ref());
        })
    }

    fn update_runtime_fields(
        &self,
        instance: &Instance,
        update: impl FnOnce(&mut Instance),
    ) -> Result<()> {
        self.check_available()?;
        let _publication = self.state.publication.blocking_write();
        ensure_healthy(&self.state)?;
        let mut rows = self.state.instances.blocking_write();
        let row = rows
            .iter_mut()
            .find(|row| row.id == instance.id)
            .ok_or(crate::session::LifecycleReservationError::Superseded)?;
        anyhow::ensure!(
            row.lifecycle_generation == instance.lifecycle_generation
                && row.source_profile == instance.source_profile
                && row.title == instance.title,
            crate::session::LifecycleReservationError::Superseded
        );
        update(row);
        self.state
            .mutation_epoch
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.state.runtime.request_publish();
        Ok(())
    }

    /// The validator must not run external effects under publication exclusion.
    pub(crate) fn move_instances_to<F>(
        &self,
        target: &Self,
        changes: &[(Instance, Instance)],
        group_move: &crate::session::GroupMovePlan,
        validate_target: F,
    ) -> Result<()>
    where
        F: FnOnce(&[Instance], &mut [Instance]) -> Result<()>,
    {
        anyhow::ensure!(
            Arc::ptr_eq(&self.state, &target.state),
            "profile move belongs to different runtimes"
        );
        ensure_healthy(&self.state)?;
        let transition = self.storage.acquire_write_transition();
        let publication = self.state.publication.blocking_write();
        ensure_healthy(&self.state)?;
        let unavailable = |error: anyhow::Error| {
            tracing::warn!(target: "server.session_store", source = self.storage.profile(), target = target.storage.profile(), %error, "profile move unavailable");
            *self.state.canonical_health.blocking_write() = error
                .downcast_ref::<super::reload::ReloadFailure>()
                .map(|failure| failure.health.clone())
                .unwrap_or_else(|| crate::daemon::RuntimeHealth::Degraded {
                    code: crate::daemon::ReloadFailureCode::ProfileData,
                    profiles: vec![
                        self.storage.profile().to_owned(),
                        target.storage.profile().to_owned(),
                    ],
                });
            self.state.runtime.request_publish();
            error.context(NativeStoreUnavailable)
        };
        let transition = transition.map_err(&unavailable)?;
        {
            let metadata = self.state.canonical_metadata.blocking_read();
            for profile in [self.storage.profile(), target.storage.profile()] {
                if !metadata.profiles.iter().any(|item| item.name == profile) {
                    return Err(unavailable(
                        super::reload::ReloadFailure {
                            health: crate::daemon::RuntimeHealth::Degraded {
                                code: crate::daemon::ReloadFailureCode::Metadata,
                                profiles: vec![profile.to_owned()],
                            },
                            source: anyhow::anyhow!(
                                "committed profile is missing from canonical metadata"
                            ),
                        }
                        .into(),
                    ));
                }
            }
        }
        let mut rejected = false;
        let committed = transition
            .move_instances_with_snapshot(
                &self.storage,
                &target.storage,
                changes,
                group_move,
                |existing, candidates| {
                    validate_target(existing, candidates).inspect_err(|_| rejected = true)
                },
            )
            .map_err(|error| {
                if rejected || error.is::<crate::session::ProfileMoveRejected>() {
                    error
                } else {
                    unavailable(error)
                }
            })?;
        self.runtime
            .block_on(super::reload::adopt_committed_profiles(
                &self.state,
                [
                    (
                        self.storage.profile(),
                        committed.source.0,
                        committed.source.1,
                    ),
                    (
                        target.storage.profile(),
                        committed.target.0,
                        committed.target.1,
                    ),
                ],
                |_| false,
                &publication,
            ))
            .map_err(|error| unavailable(error.into()))
    }

    fn run_sandbox_migration(&self, run: impl FnOnce() -> Result<()>) -> Result<()> {
        self.check_available()?;
        run().map_err(|error| {
            if error.is::<NativeStoreUnavailable>()
                || !error
                    .is::<crate::migrations::v027_isolate_sandbox_stores::SandboxCheckpointFailure>(
                    )
            {
                return error;
            }
            let publication = self.state.publication.blocking_write();
            mark_checkpoint_failure(&self.state, self.storage.profile(), &error, &publication);
            error.context(NativeStoreUnavailable)
        })
    }
}

impl SessionStore for NativeSessionStore {
    fn storage(&self) -> &Storage {
        &self.storage
    }

    fn defer_lifecycle_release(
        self,
        id: String,
        operation: crate::session::LifecycleOperation,
        generation: u64,
        lifecycle: Option<crate::session::StorageFlock>,
    ) {
        // Retain authority before the caller can release its last lease.
        let namespace = self.state.runtime.active_purge_namespace_lease();
        drop(lifecycle);
        let runtime = self.runtime.clone();
        let work = self.state.runtime.work.clone();
        let _entered = runtime.enter();
        work.spawn("purge reservation release", async move {
            let fresh_namespace = if namespace.is_none() {
                Some(self.state.profile_namespace.clone().read_owned().await)
            } else {
                None
            };
            let _namespace = (namespace, fresh_namespace);
            let result = tokio::task::spawn_blocking(move || {
                self.release_lifecycle_reservation(&id, operation, generation, None)
            }).await;
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::warn!(target: "session.lifecycle", %error, "native reservation release failed"),
                Err(error) => tracing::error!(target: "session.lifecycle", %error, "native reservation release task failed"),
            }
        });
    }

    fn load(&self) -> Result<Vec<Instance>> {
        ensure_healthy(&self.state)?;
        match self.storage.load_complete_with_groups() {
            Ok((rows, _)) => Ok(rows),
            Err(error) => {
                let publication = self.state.publication.blocking_write();
                mark_profile_failure(&self.state, self.storage.profile(), &error, &publication);
                Err(error)
            }
        }
    }

    fn check_available(&self) -> Result<()> {
        ensure_healthy(&self.state)?;
        self.storage.verify_bound_profile().map_err(|error| {
            let publication = self.state.publication.blocking_write();
            mark_profile_failure(&self.state, self.storage.profile(), &error, &publication);
            error.context(NativeStoreUnavailable)
        })
    }

    fn configuration(&self, profile: Option<&str>) -> Result<crate::session::config::Config> {
        self.check_available()?;
        let load = || -> Result<crate::session::config::Config> {
            let mut config = match profile {
                Some(profile) => {
                    if profile != self.storage.profile() {
                        Storage::open(profile, self.state.file_watch.clone())?;
                    }
                    crate::session::resolve_config(profile)?
                }
                None => crate::session::config::Config::load()?,
            };
            crate::session::config::profile_config::apply_cityhall_overrides(&mut config);
            Ok(config)
        };
        load().map_err(|error| {
            let publication = self.state.publication.blocking_write();
            if let Some(profile) = profile {
                mark_profile_failure(&self.state, profile, &error, &publication);
            } else {
                *self.state.canonical_health.blocking_write() =
                    crate::daemon::RuntimeHealth::Degraded {
                        code: crate::daemon::ReloadFailureCode::Metadata,
                        profiles: Vec::new(),
                    };
                self.state.runtime.request_publish();
            }
            error.context(NativeStoreUnavailable)
        })
    }

    fn commit(&self, mutation: &mut SessionMutation<'_>) -> Result<()> {
        ensure_healthy(&self.state)?;
        let transition = self.storage.acquire_write_transition();
        let publication = self.state.publication.blocking_write();
        ensure_healthy(&self.state)?;
        let transition = match transition {
            Ok(transition) => transition,
            Err(error) => {
                mark_profile_failure(&self.state, self.storage.profile(), &error, &publication);
                return Err(error);
            }
        };
        let mut rejected = false;
        let result = transition.update_with_snapshot(&self.storage, |rows, groups| {
            mutation(rows, groups).inspect_err(|_| rejected = true)
        });
        let ((), rows, groups) = match result {
            Ok(committed) => committed,
            Err(error) => {
                if !rejected {
                    mark_profile_failure(&self.state, self.storage.profile(), &error, &publication);
                }
                return Err(error);
            }
        };
        if let Err(error) = self
            .runtime
            .block_on(super::reload::adopt_committed_profiles(
                &self.state,
                [(self.storage.profile(), rows, groups)],
                |id| self.status_id.as_deref() == Some(id),
                &publication,
            ))
        {
            *self.state.canonical_health.blocking_write() = error.health.clone();
            self.state.runtime.request_publish();
            return Err(error.into());
        }
        Ok(())
    }

    fn migrate_sandbox_store(
        &self,
        id: &str,
        reporter: Option<crate::migrations::progress::Reporter>,
        runtime: &crate::containers::ContainerRuntime,
    ) -> Result<()> {
        self.run_sandbox_migration(|| {
            crate::migrations::migrate_sandbox_store_for_with(id, reporter, self, runtime)
        })
    }

    fn commit_sandbox_checkpoint(
        &self,
        checkpoint: crate::migrations::v027_isolate_sandbox_stores::LockedSandboxCheckpoint<'_>,
    ) -> Result<()> {
        ensure_healthy(&self.state)?;
        let prepared = (|| -> Result<_> {
            self.storage.verify_bound_profile()?;
            let prepared = checkpoint.prepare(&self.state.file_watch)?;
            let mut details = std::collections::HashMap::new();
            for storage in prepared.profiles() {
                details.insert(
                    storage.profile().to_owned(),
                    super::reload::load_profile_details(storage.profile()).map_err(|source| {
                        crate::migrations::v027_isolate_sandbox_stores::SandboxCheckpointFailure {
                            profile: Some(storage.profile().to_owned()),
                            source,
                        }
                    })?,
                );
            }
            Ok((prepared, details))
        })();
        let publication = self.state.publication.blocking_write();
        ensure_healthy(&self.state)?;
        let result = (|| -> Result<()> {
            let (prepared, mut details) = prepared?;
            self.storage.verify_bound_profile()?;
            let committed = prepared.commit()?;
            self.storage.verify_bound_profile()?;
            let mut metadata = self.state.canonical_metadata.blocking_write();
            let mut current = self.state.instances.blocking_write();
            for profile in &committed {
                anyhow::ensure!(
                    details.contains_key(profile.storage.profile()),
                    "checkpoint metadata is incomplete"
                );
            }
            for profile in committed {
                let name = profile.storage.profile();
                if !metadata.profiles.iter().any(|item| item.name == name) {
                    let details = details
                        .remove(name)
                        .context("checkpoint metadata is incomplete")?;
                    metadata
                        .status_hooks
                        .insert(name.to_owned(), details.status_hooks);
                    metadata
                        .auxiliary_tools
                        .insert(name.to_owned(), details.auxiliary_tools);
                    metadata.profiles.push(crate::daemon::ProfileSnapshot {
                        name: name.to_owned(),
                        description: details.description,
                        groups: Vec::new(),
                        projects: details.projects,
                    });
                }
                super::reload::replace_committed_profiles(
                    &mut current,
                    &mut metadata,
                    [(name, profile.rows, profile.groups)],
                    |_| None,
                )?;
            }
            self.state
                .mutation_epoch
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.state.runtime.request_publish();
            Ok(())
        })();
        result.map_err(|error| {
            mark_checkpoint_failure(&self.state, self.storage.profile(), &error, &publication);
            error.context(NativeStoreUnavailable)
        })
    }

    fn managed_capture_store_is_exclusive(
        &self,
        current: &Instance,
        backend: crate::agents::SessionCaptureBackend,
        current_store: &std::path::Path,
    ) -> Result<bool> {
        ensure_healthy(&self.state)?;
        if !current.is_sandboxed() {
            return Ok(false);
        }
        let check = || -> std::result::Result<bool, super::reload::ReloadFailure> {
            let loaded = super::reload::load_all_profiles(&self.state.file_watch)?;
            let profile_failure = |profile: &str, source| super::reload::ReloadFailure {
                health: crate::daemon::RuntimeHealth::Degraded {
                    code: crate::daemon::ReloadFailureCode::ProfileData,
                    profiles: vec![profile.to_owned()],
                },
                source,
            };
            if !loaded
                .metadata
                .profiles
                .iter()
                .any(|profile| profile.name == self.storage.profile())
            {
                return Err(profile_failure(
                    self.storage.profile(),
                    anyhow::anyhow!("capture profile disappeared"),
                ));
            }
            let load_config = |profile: &str| {
                self.configuration(Some(profile))
                    .map_err(|source| profile_failure(profile, source))
            };
            let mut configs = std::collections::HashMap::new();
            configs.insert(self.storage.profile(), load_config(self.storage.profile())?);
            let Some(home) = dirs::home_dir() else {
                return Ok(false);
            };
            let Ok(current_store) = std::fs::canonicalize(current_store) else {
                return Ok(false);
            };
            for peer in &loaded.instances {
                if !peer.is_managed_capture_peer(
                    &current.id,
                    peer.source_profile == self.storage.profile(),
                ) {
                    continue;
                }
                let config = match configs.entry(peer.source_profile.as_str()) {
                    std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        entry.insert(load_config(&peer.source_profile)?)
                    }
                };
                if peer.managed_capture_store_conflicts(backend, &current_store, config, &home) {
                    return Ok(false);
                }
            }
            Ok(true)
        };
        match check() {
            Ok(exclusive) => {
                ensure_healthy(&self.state)?;
                Ok(exclusive)
            }
            Err(error) => {
                if error.source.is::<NativeStoreUnavailable>() {
                    return Err(error.source);
                }
                let _publication = self.state.publication.blocking_write();
                tracing::warn!(target: "server.session_store", %error, "capture ownership unavailable");
                *self.state.canonical_health.blocking_write() = error.health.clone();
                self.state.runtime.request_publish();
                Err(anyhow::Error::new(error).context(NativeStoreUnavailable))
            }
        }
    }
}

fn ensure_healthy(state: &AppState) -> Result<()> {
    anyhow::ensure!(
        *state.canonical_health.blocking_read() == crate::daemon::RuntimeHealth::Healthy,
        NativeStoreUnavailable
    );
    Ok(())
}

fn mark_profile_failure(
    state: &AppState,
    profile: &str,
    error: &anyhow::Error,
    _publication: &tokio::sync::RwLockWriteGuard<'_, ()>,
) {
    tracing::warn!(target: "server.session_store", profile, %error, "session store unavailable");
    *state.canonical_health.blocking_write() = crate::daemon::RuntimeHealth::Degraded {
        code: crate::daemon::ReloadFailureCode::ProfileData,
        profiles: vec![profile.to_owned()],
    };
    state.runtime.request_publish();
}

fn mark_checkpoint_failure(
    state: &AppState,
    profile: &str,
    error: &anyhow::Error,
    publication: &tokio::sync::RwLockWriteGuard<'_, ()>,
) {
    if let Some(failure) = error
        .downcast_ref::<crate::migrations::v027_isolate_sandbox_stores::SandboxCheckpointFailure>(
    ) {
        tracing::warn!(target: "server.session_store", %error, "sandbox registry unavailable");
        *state.canonical_health.blocking_write() = crate::daemon::RuntimeHealth::Degraded {
            code: if failure.profile.is_some() {
                crate::daemon::ReloadFailureCode::ProfileData
            } else {
                crate::daemon::ReloadFailureCode::Metadata
            },
            profiles: failure.profile.iter().cloned().collect(),
        };
        state.runtime.request_publish();
    } else {
        mark_profile_failure(state, profile, error, publication);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn profile_move_fixture(
        read_only: bool,
        cityhall: bool,
    ) -> Result<(Storage, Storage, Arc<AppState>, Instance)> {
        let source = Storage::new_unwatched("source")?;
        let target = Storage::new_unwatched("target")?;
        let mut moving = Instance::new("moving", "/tmp/native-moving");
        moving.source_profile = "source".into();
        moving.group_path = "team/sub".into();
        moving.status = crate::session::Status::Stopped;
        let mut source_peer = Instance::new("source peer", "/tmp/native-source-peer");
        source_peer.status = crate::session::Status::Stopped;
        let mut target_peer = Instance::new("target peer", "/tmp/native-target-peer");
        target_peer.status = crate::session::Status::Stopped;
        source.update(|rows, groups| {
            rows.extend([moving.clone(), source_peer]);
            let mut team = crate::session::Group::new("team", "team");
            team.collapsed = true;
            groups.extend([team, crate::session::Group::new("sub", "team/sub")]);
            Ok(())
        })?;
        target.update(|rows, groups| {
            rows.push(target_peer);
            groups.push(crate::session::Group::new("target empty", "target empty"));
            Ok(())
        })?;
        let loaded =
            super::super::reload::load_all_profiles(&crate::file_watch::FileWatchService::noop())?;
        let state = crate::server::test_support::build_test_app_state_with_policy_configured(
            loaded.instances,
            vec!["localhost".into()],
            Vec::new(),
            None,
            |state| {
                state.read_only = read_only;
                state.cityhall_mode = cityhall;
            },
        );
        *state.canonical_metadata.write().await = loaded.metadata;
        state
            .instances
            .write()
            .await
            .iter_mut()
            .find(|row| row.id == moving.id)
            .unwrap()
            .status = crate::session::Status::Running;
        Ok((source, target, state, moving))
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn native_group_route_reflects_subtrees_and_empty_groups() -> Result<()> {
        use tower::ServiceExt;
        let _home = crate::session::test_support::isolate_app_dir();
        let (_, _, state, moving) = profile_move_fixture(false, false).await?;
        let app = crate::server::test_support::build_router_for_test(state.clone());
        for (from_profile, from, to_profile, to, row_path) in [
            ("source", "team", "target", "moved", "moved/sub"),
            ("target", "moved", "target", "renamed", "renamed/sub"),
            (
                "target",
                "target empty",
                "source",
                "empty moved",
                "renamed/sub",
            ),
        ] {
            let before = state.runtime.publish(&state).await?;
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .method("PATCH")
                        .uri("/api/groups")
                        .header("host", "localhost")
                        .header("content-type", "application/json")
                        .header(
                            crate::daemon::RUNTIME_EPOCH_HEADER,
                            &before.value.cursor.epoch,
                        )
                        .extension(axum::extract::ConnectInfo(
                            crate::server::peer::ConnectionPeer::UnixOwner {
                                uid: nix::unistd::geteuid().as_raw(),
                            },
                        ))
                        .body(axum::body::Body::from(
                            serde_json::json!({
                                "source": {"profile": from_profile, "path": from},
                                "target": {"profile": to_profile, "path": to},
                            })
                            .to_string(),
                        ))?,
                )
                .await?;
            assert_eq!(response.status(), axum::http::StatusCode::OK);
            let after = state.runtime.publish(&state).await?;
            assert_eq!(
                response.headers()[crate::daemon::RUNTIME_REVISION_HEADER],
                after.value.cursor.revision.to_string()
            );
            let row = after
                .value
                .contents
                .sessions
                .iter()
                .find(|row| row.id == moving.id)
                .unwrap();
            assert_eq!(row.group_path, row_path);
            assert_eq!(row.profile, "target");
            let source = after
                .value
                .contents
                .profiles
                .iter()
                .find(|profile| profile.name == from_profile)
                .unwrap();
            assert!(!source.groups.iter().any(|group| group.path == from));
            let target = after
                .value
                .contents
                .profiles
                .iter()
                .find(|profile| profile.name == to_profile)
                .unwrap();
            assert!(target.groups.iter().any(|group| group.path == to));
        }
        Ok(())
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn native_group_route_preserves_policy_and_rejects_empty_paths() -> Result<()> {
        use tower::ServiceExt;
        for (read_only, cityhall, structured, mixed, target_profile, target_path, expected) in [
            (true, false, false, false, "source", "renamed", 403),
            (false, true, false, false, "source", "renamed", 403),
            (false, true, true, false, "target", "renamed", 403),
            (false, true, true, true, "source", "renamed", 403),
            (false, true, true, false, "source", "renamed", 200),
            (false, false, false, false, "source", "", 400),
            (false, false, false, false, "source", "duplicate IDs", 409),
            (
                false,
                false,
                false,
                false,
                "source",
                crate::session::TRASH_SECTION_PATH,
                400,
            ),
        ] {
            let _home = crate::session::test_support::isolate_app_dir();
            let (source, _, state, moving) = profile_move_fixture(read_only, cityhall).await?;
            source.update(|rows, _| {
                if structured {
                    rows.iter_mut()
                        .find(|row| row.id == moving.id)
                        .unwrap()
                        .view = crate::session::View::Structured;
                }
                if mixed {
                    let mut hidden = Instance::new("hidden terminal", "/tmp/hidden-terminal");
                    hidden.group_path = "team/hidden".into();
                    hidden.trash();
                    rows.push(hidden);
                }
                if target_path == "duplicate IDs" {
                    rows.push(rows.iter().find(|row| row.id == moving.id).unwrap().clone());
                }
                Ok(())
            })?;
            let before = std::fs::read(source.sessions_path())?;
            let snapshot = state.runtime.publish(&state).await?;
            let response = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                crate::server::test_support::build_router_for_test(state.clone()).oneshot(
                    axum::http::Request::builder()
                        .method("PATCH")
                        .uri("/api/groups")
                        .header("host", "localhost")
                        .header("content-type", "application/json")
                        .header(
                            crate::daemon::RUNTIME_EPOCH_HEADER,
                            &snapshot.value.cursor.epoch,
                        )
                        .extension(axum::extract::ConnectInfo(
                            crate::server::peer::ConnectionPeer::UnixOwner {
                                uid: nix::unistd::geteuid().as_raw(),
                            },
                        ))
                        .body(axum::body::Body::from(
                            serde_json::json!({
                                "source": {"profile": "source", "path": "team"},
                                "target": {"profile": target_profile, "path": target_path},
                            })
                            .to_string(),
                        ))?,
                ),
            )
            .await??;
            assert_eq!(response.status().as_u16(), expected);
            if expected != 200 {
                assert_eq!(std::fs::read(source.sessions_path())?, before);
            } else {
                assert_eq!(
                    source
                        .load()?
                        .iter()
                        .find(|row| row.id == moving.id)
                        .unwrap()
                        .group_path,
                    "renamed/sub"
                );
            }
        }
        Ok(())
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn native_group_metadata_enforces_read_only_and_member_ownership() -> Result<()> {
        use tower::ServiceExt;
        for (read_only, cityhall, structured) in [
            (true, false, false),
            (false, false, false),
            (false, true, false),
            (false, true, true),
        ] {
            let _home = crate::session::test_support::isolate_app_dir();
            let (source, _, state, moving) = profile_move_fixture(read_only, cityhall).await?;
            if structured {
                source.update(|rows, _| {
                    rows.iter_mut()
                        .find(|row| row.id == moving.id)
                        .unwrap()
                        .view = crate::session::View::Structured;
                    Ok(())
                })?;
            }
            let snapshot = state.runtime.publish(&state).await?;
            let app = crate::server::test_support::build_router_for_test(state.clone());
            for (method, route, body, expected) in [
                (
                    "POST",
                    "/api/groups",
                    serde_json::json!({"profile": "source", "path": "created"}),
                    if read_only { 403 } else { 201 },
                ),
                (
                    "PATCH",
                    "/api/groups/collapse",
                    serde_json::json!({"group": {"profile": "source", "path": "team"}, "collapsed": false}),
                    if read_only || (cityhall && !structured) {
                        403
                    } else {
                        200
                    },
                ),
                (
                    "DELETE",
                    "/api/groups",
                    serde_json::json!({"group": {"profile": "source", "path": "team"}, "mode": "keep_sessions"}),
                    if read_only || (cityhall && !structured) {
                        403
                    } else {
                        200
                    },
                ),
            ] {
                let path = source.sessions_path().with_file_name("groups.json");
                let before = std::fs::read(&path)?;
                let response = app
                    .clone()
                    .oneshot(
                        axum::http::Request::builder()
                            .method(method)
                            .uri(route)
                            .header("host", "localhost")
                            .header("content-type", "application/json")
                            .header(
                                crate::daemon::RUNTIME_EPOCH_HEADER,
                                &snapshot.value.cursor.epoch,
                            )
                            .extension(axum::extract::ConnectInfo(
                                crate::server::peer::ConnectionPeer::UnixOwner {
                                    uid: nix::unistd::geteuid().as_raw(),
                                },
                            ))
                            .body(axum::body::Body::from(body.to_string()))?,
                    )
                    .await?;
                assert_eq!(response.status().as_u16(), expected);
                if expected == 403 {
                    assert_eq!(std::fs::read(path)?, before);
                } else {
                    let reflected = state.runtime.publish(&state).await?;
                    let profile = reflected
                        .value
                        .contents
                        .profiles
                        .iter()
                        .find(|profile| profile.name == "source")
                        .unwrap();
                    let name = if method == "POST" { "created" } else { "team" };
                    if method == "DELETE" {
                        assert!(!profile.groups.iter().any(|group| group.path == name));
                        assert!(reflected
                            .value
                            .contents
                            .sessions
                            .iter()
                            .find(|row| row.id == moving.id)
                            .unwrap()
                            .group_path
                            .is_empty());
                    } else {
                        let group = profile
                            .groups
                            .iter()
                            .find(|group| group.path == name)
                            .unwrap();
                        if method == "PATCH" {
                            assert!(!group.collapsed);
                        }
                    }
                    assert_eq!(
                        response.headers()[crate::daemon::RUNTIME_REVISION_HEADER],
                        reflected.value.cursor.revision.to_string()
                    );
                }
            }
        }
        Ok(())
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn native_worktree_reconciliation_publishes_complete_profiles_and_enforces_policy(
    ) -> Result<()> {
        for policy in [
            "normal",
            "read_only",
            "cityhall_terminal",
            "cityhall_structured",
            "broken_groups",
        ] {
            let _home = crate::session::test_support::isolate_app_dir();
            let (source, _, state, mut moving) =
                profile_move_fixture(policy == "read_only", policy.starts_with("cityhall")).await?;
            let root = tempfile::tempdir()?;
            let main = root.path().join("main");
            let live = root.path().join("live");
            let repo = git2::Repository::init(&main)?;
            let signature = git2::Signature::now("Test", "test@example.com")?;
            let tree = repo.find_tree(repo.index()?.write_tree()?)?;
            repo.commit(Some("HEAD"), &signature, &signature, "init", &tree, &[])?;
            crate::git::GitWorktree::new(main.clone())?
                .create_worktree("work", &live, true, None)?;
            moving.project_path = root.path().join("gone").to_string_lossy().into_owned();
            moving.worktree_info = Some(crate::session::WorktreeInfo {
                branch: "work".into(),
                main_repo_path: main.to_string_lossy().into_owned(),
                managed_by_aoe: true,
                created_at: chrono::Utc::now(),
                base_branch: None,
            });
            if policy == "cityhall_structured" {
                moving.view = crate::session::View::Structured;
            }
            {
                let mut rows = state.instances.write().await;
                let cached = rows.iter_mut().find(|row| row.id == moving.id).unwrap();
                *cached = moving.clone();
                cached.status = crate::session::Status::Running;
                if policy == "cityhall_terminal" {
                    cached.view = crate::session::View::Structured;
                }
            }
            source.update(|rows, groups| {
                *rows.iter_mut().find(|row| row.id == moving.id).unwrap() = moving.clone();
                let peer = rows.iter_mut().find(|row| row.id != moving.id).unwrap();
                peer.title = "external peer title".into();
                peer.view = crate::session::View::Structured;
                groups.push(crate::session::Group::new(
                    "external empty",
                    "external empty",
                ));
                Ok(())
            })?;
            let groups_path = source.sessions_path().with_file_name("groups.json");
            if policy == "broken_groups" {
                std::fs::write(&groups_path, b"{")?;
            }
            let before = std::fs::read(source.sessions_path())?;
            crate::server::api::reconcile_worktree_paths(&state).await;
            let snapshot = state.runtime.publish(&state).await?;
            let row = snapshot
                .value
                .contents
                .sessions
                .iter()
                .find(|row| row.id == moving.id)
                .unwrap();
            if matches!(policy, "normal" | "cityhall_structured") {
                assert_eq!(
                    std::path::Path::new(&row.project_path),
                    live.canonicalize()?,
                    "{policy}"
                );
                assert_eq!(
                    crate::session::Status::from_api_str(&row.status),
                    Some(crate::session::Status::Running),
                    "runtime state was overwritten"
                );
                let profile = snapshot
                    .value
                    .contents
                    .profiles
                    .iter()
                    .find(|profile| profile.name == "source")
                    .unwrap();
                assert!(
                    profile
                        .groups
                        .iter()
                        .any(|group| group.path == "external empty"),
                    "complete group metadata was not published"
                );
                assert!(
                    snapshot
                        .value
                        .contents
                        .sessions
                        .iter()
                        .any(|row| row.profile == "source" && row.title == "external peer title"),
                    "peer changes were not published"
                );
            } else {
                assert_eq!(
                    std::fs::read(source.sessions_path())?,
                    before,
                    "{policy} wrote session data"
                );
                assert_eq!(
                    row.project_path, moving.project_path,
                    "{policy} changed the mirror"
                );
            }
            if policy == "broken_groups" {
                assert_eq!(std::fs::read(groups_path)?, b"{");
                assert!(matches!(
                    &*state.canonical_health.read().await,
                    crate::daemon::RuntimeHealth::Degraded { .. }
                ));
            }
        }
        Ok(())
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn native_group_delete_keeps_hidden_members_and_respects_empty_only() -> Result<()> {
        use tower::ServiceExt;
        let _home = crate::session::test_support::isolate_app_dir();
        let (source, _, state, moving) = profile_move_fixture(false, false).await?;
        let mut hidden = Instance::new("hidden", "/tmp/group-hidden");
        hidden.group_path = "team/deep".into();
        hidden.trash();
        let hidden_id = hidden.id.clone();
        let mut outside = Instance::new("outside", "/tmp/group-outside");
        outside.group_path = "teamwork".into();
        let outside_id = outside.id.clone();
        source.update(|rows, _| {
            rows.extend([hidden, outside]);
            Ok(())
        })?;
        let app = crate::server::test_support::build_router_for_test(state.clone());
        for (profile, path, mode, expected) in [
            ("source", "team", "empty_only", 409),
            ("source", "team", "keep_sessions", 200),
            ("target", "target empty", "empty_only", 200),
        ] {
            let before = std::fs::read(source.sessions_path())?;
            let snapshot = state.runtime.publish(&state).await?;
            let response = app.clone().oneshot(axum::http::Request::builder()
                .method("DELETE").uri("/api/groups")
                .header("host", "localhost").header("content-type", "application/json")
                .header(crate::daemon::RUNTIME_EPOCH_HEADER, &snapshot.value.cursor.epoch)
                .extension(axum::extract::ConnectInfo(crate::server::peer::ConnectionPeer::UnixOwner { uid: nix::unistd::geteuid().as_raw() }))
                .body(axum::body::Body::from(serde_json::json!({"group": {"profile": profile, "path": path}, "mode": mode}).to_string()))?).await?;
            assert_eq!(response.status().as_u16(), expected);
            if expected == 409 {
                assert_eq!(std::fs::read(source.sessions_path())?, before);
                continue;
            }
            let snapshot = state.runtime.publish(&state).await?;
            assert_eq!(
                response.headers()[crate::daemon::RUNTIME_REVISION_HEADER],
                snapshot.value.cursor.revision.to_string()
            );
            let body: serde_json::Value =
                serde_json::from_slice(&axum::body::to_bytes(response.into_body(), 65536).await?)?;
            let results = body["sessions"].as_array().unwrap();
            if mode == "keep_sessions" {
                let ids: std::collections::HashSet<_> = results
                    .iter()
                    .map(|row| row["id"].as_str().unwrap())
                    .collect();
                assert_eq!(
                    ids,
                    std::collections::HashSet::from([moving.id.as_str(), hidden_id.as_str()])
                );
                for id in [&moving.id, &hidden_id] {
                    let row = snapshot
                        .value
                        .contents
                        .sessions
                        .iter()
                        .find(|row| &row.id == id)
                        .unwrap();
                    assert!(row.group_path.is_empty());
                }
                assert_eq!(
                    snapshot
                        .value
                        .contents
                        .sessions
                        .iter()
                        .find(|row| row.id == outside_id)
                        .unwrap()
                        .group_path,
                    "teamwork"
                );
            } else {
                assert!(results.is_empty());
            }
            let groups = &snapshot
                .value
                .contents
                .profiles
                .iter()
                .find(|entry| entry.name == profile)
                .unwrap()
                .groups;
            assert!(!groups
                .iter()
                .any(|group| group.path == path || group.path.starts_with(&format!("{path}/"))));
        }
        Ok(())
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn native_profile_move_publishes_both_complete_profiles() -> Result<()> {
        let _home = crate::session::test_support::isolate_app_dir();
        let (source, target, state, moving) = profile_move_fixture(false, false).await?;
        state.runtime.publish(&state).await?;
        source.update(|rows, groups| {
            rows.iter_mut()
                .find(|row| row.title == "source peer")
                .unwrap()
                .title = "source peer committed".into();
            groups.push(crate::session::Group::new("source empty", "source empty"));
            Ok(())
        })?;
        target.update(|rows, groups| {
            rows.iter_mut()
                .find(|row| row.title == "target peer")
                .unwrap()
                .title = "target peer committed".into();
            groups.push(crate::session::Group::new("peer empty", "peer empty"));
            Ok(())
        })?;
        let namespace = state.profile_namespace.read().await;
        let worker_state = state.clone();
        let worker_moving = moving.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let source = NativeSessionStore::open(worker_state.clone(), "source", None)?;
            let target = NativeSessionStore::open(worker_state, "target", None)?;
            let mut after = worker_moving.clone();
            after.group_path = "moved/sub".into();
            source.move_instances_to(
                &target,
                &[(worker_moving, after)],
                &crate::session::GroupMovePlan::subtree("team", "moved"),
                |_, _| Ok(()),
            )?;
            Ok(())
        })
        .await??;
        drop(namespace);
        let published = state.runtime.publish(&state).await?;
        let rows = &published.value.contents.sessions;
        let moved = rows.iter().find(|row| row.id == moving.id).unwrap();
        assert_eq!(moved.profile, "target");
        assert_eq!(moved.group_path, "moved/sub");
        assert_eq!(
            crate::session::Status::from_api_str(&moved.status),
            Some(crate::session::Status::Running)
        );
        assert_eq!(rows.iter().filter(|row| row.id == moving.id).count(), 1);
        assert!(rows.iter().any(|row| row.title == "source peer committed"));
        assert!(rows.iter().any(|row| row.title == "target peer committed"));
        let profiles = &published.value.contents.profiles;
        let source_profile = profiles
            .iter()
            .find(|profile| profile.name == "source")
            .unwrap();
        let target_profile = profiles
            .iter()
            .find(|profile| profile.name == "target")
            .unwrap();
        assert_eq!(source_profile.groups, source.load_complete_with_groups()?.1);
        assert_eq!(target_profile.groups, target.load_complete_with_groups()?.1);
        assert_eq!(
            source_profile
                .groups
                .iter()
                .map(|group| group.path.as_str())
                .collect::<Vec<_>>(),
            ["source empty"]
        );
        assert!(target_profile
            .groups
            .iter()
            .any(|group| group.path == "moved" && group.collapsed));
        assert!(target_profile
            .groups
            .iter()
            .any(|group| group.path == "peer empty"));
        Ok(())
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn native_profile_move_rejects_incomplete_or_replaced_authority() -> Result<()> {
        for fault in [
            "source rows",
            "target groups",
            "metadata",
            "target binding",
            #[cfg(unix)]
            "crossed registries",
        ] {
            let _home = crate::session::test_support::isolate_app_dir();
            let (source, target, state, moving) = profile_move_fixture(false, false).await?;
            let before = state.runtime.publish(&state).await?;
            let source_groups = source.sessions_path().with_file_name("groups.json");
            let target_groups = target.sessions_path().with_file_name("groups.json");
            match fault {
                #[cfg(unix)]
                "crossed registries" => {
                    let mut data: Vec<serde_json::Value> =
                        serde_json::from_slice(&std::fs::read(source.sessions_path())?)?;
                    for row in &mut data {
                        row["name"] = serde_json::json!("shared");
                        row["path"] = serde_json::json!("shared");
                    }
                    let bytes = serde_json::to_vec(&data)?;
                    let _: Vec<Instance> = serde_json::from_slice(&bytes)?;
                    let _: Vec<crate::session::Group> = serde_json::from_slice(&bytes)?;
                    std::fs::write(&target_groups, bytes)?;
                    std::fs::remove_file(source.sessions_path())?;
                    std::os::unix::fs::symlink(&target_groups, source.sessions_path())?;
                }
                "source rows" | "target groups" => {
                    let path = if fault == "source rows" {
                        source.sessions_path()
                    } else {
                        &target_groups
                    };
                    let mut data: Vec<serde_json::Value> =
                        serde_json::from_slice(&std::fs::read(path)?)?;
                    data.push(serde_json::json!({"id": 5, "path": 5}));
                    std::fs::write(path, serde_json::to_vec(&data)?)?;
                }
                "metadata" => state
                    .canonical_metadata
                    .write()
                    .await
                    .profiles
                    .retain(|profile| profile.name != "target"),
                _ => {}
            }
            let source_before = (
                std::fs::read(source.sessions_path())?,
                std::fs::read(&source_groups)?,
            );
            let target_before = (
                std::fs::read(target.sessions_path())?,
                std::fs::read(&target_groups)?,
            );
            let target_directory = target.sessions_path().parent().unwrap().to_path_buf();
            let detached = target_directory.with_file_name("detached-target");
            let replacement = b"replacement profile must not be overwritten";
            let namespace = state.profile_namespace.read().await;
            let worker_state = state.clone();
            let worker_target = target_directory.clone();
            let worker_detached = detached.clone();
            let result = tokio::task::spawn_blocking(move || -> Result<()> {
                let source = NativeSessionStore::open(worker_state.clone(), "source", None)?;
                let target = NativeSessionStore::open(worker_state, "target", None)?;
                let mut after = moving.clone();
                after.group_path = "moved/sub".into();
                source.move_instances_to(
                    &target,
                    &[(moving, after)],
                    &crate::session::GroupMovePlan::subtree("team", "moved"),
                    |_, _| {
                        if fault == "target binding" {
                            std::fs::rename(&worker_target, &worker_detached)?;
                            std::fs::create_dir(&worker_target)?;
                            std::fs::write(target.storage.sessions_path(), replacement)?;
                            std::fs::write(
                                target.storage.sessions_path().with_file_name("groups.json"),
                                b"[]",
                            )?;
                        }
                        Ok(())
                    },
                )?;
                Ok(())
            })
            .await?;
            drop(namespace);
            assert!(result.is_err(), "accepted {fault}");
            assert_eq!(std::fs::read(source.sessions_path())?, source_before.0);
            assert_eq!(std::fs::read(&source_groups)?, source_before.1);
            if fault == "target binding" {
                assert_eq!(std::fs::read(target.sessions_path())?, replacement);
                assert_eq!(std::fs::read(&target_groups)?, b"[]");
                assert_eq!(
                    std::fs::read(detached.join("sessions.json"))?,
                    target_before.0
                );
                assert_eq!(
                    std::fs::read(detached.join("groups.json"))?,
                    target_before.1
                );
            } else {
                assert_eq!(std::fs::read(target.sessions_path())?, target_before.0);
                assert_eq!(std::fs::read(&target_groups)?, target_before.1);
            }
            assert!(!source
                .sessions_path()
                .with_file_name("sessions.corrupt.jsonl")
                .exists());
            assert!(!target_groups
                .with_file_name("groups.corrupt.jsonl")
                .exists());
            let after = state.runtime.publish(&state).await?;
            assert!(matches!(
                &after.value.contents.health,
                crate::daemon::RuntimeHealth::Degraded { .. }
            ));
            assert_eq!(
                after.value.contents.sessions,
                before.value.contents.sessions
            );
        }
        Ok(())
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn shutdown_drains_abandoned_native_purge_before_releasing_authority() -> Result<()> {
        use crate::session::deletion::{DeletionRequest, PurgeReservation, PurgeTransaction};
        let _guard = crate::session::test_support::isolate_app_dir();
        let storage = Storage::new_unwatched("purge-release")?;
        let mut row = Instance::new("purge", "/tmp/purge-release");
        row.source_profile = storage.profile().to_owned();
        row.status = crate::session::Status::Stopped;
        row.trash();
        storage.update(|rows, _| {
            rows.push(row.clone());
            Ok(())
        })?;
        let id = row.id.clone();
        let state = crate::server::test_support::build_test_app_state(vec![row.clone()]);
        *state.canonical_metadata.write().await =
            super::super::reload::load_all_profiles(&state.file_watch)?.metadata;
        let (reserved, reservation) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let (dropped, drop_complete) = tokio::sync::oneshot::channel();
        let parent_state = state.clone();
        state.runtime.work.spawn("test.purge_parent", async move {
            let _namespace = parent_state
                .runtime
                .purge_namespace_lease(&parent_state.profile_namespace)
                .await;
            let worker_state = parent_state.clone();
            let transaction = tokio::task::spawn_blocking(move || {
                let backend = NativeSessionStore::open(
                    worker_state,
                    &row.source_profile,
                    Some(row.id.clone()),
                )
                .unwrap();
                match PurgeTransaction::reserve(
                    backend,
                    DeletionRequest {
                        session_id: row.id.clone(),
                        instance: row,
                        delete_worktree: false,
                        delete_branch: false,
                        delete_sandbox: false,
                        force_delete: false,
                        detach_hooks: true,
                        keep_scratch: true,
                    },
                    None,
                )
                .unwrap()
                {
                    PurgeReservation::Reserved(transaction) => transaction,
                    PurgeReservation::Rejected(_) => panic!("purge reservation refused"),
                }
            })
            .await
            .unwrap();
            reserved.send(()).unwrap();
            released.await.unwrap();
            drop(transaction);
            dropped.send(()).unwrap();
        });
        reservation.await?;
        state.runtime.publish(&state).await?;
        let publication = state.publication.write().await;
        let mut writer = Box::pin(state.profile_namespace.write());
        assert!(futures_util::poll!(writer.as_mut()).is_pending());
        state.runtime.work.shutdown.cancel();
        release.send(()).unwrap();
        drop_complete.await?;
        let mut drain = Box::pin(state.runtime.work.drain());
        assert!(
            futures_util::poll!(drain.as_mut()).is_pending(),
            "shutdown must wait for the reservation commit"
        );
        let writer_overtook_release = match futures_util::poll!(writer.as_mut()) {
            std::task::Poll::Ready(guard) => {
                drop(guard);
                true
            }
            std::task::Poll::Pending => false,
        };
        drop(publication);
        tokio::time::timeout(std::time::Duration::from_secs(5), drain).await?;
        if !writer_overtook_release {
            drop(tokio::time::timeout(std::time::Duration::from_secs(5), writer).await?);
        }
        assert!(
            !writer_overtook_release,
            "namespace writer overtook the deferred purge reservation release"
        );
        assert!(storage
            .load()?
            .iter()
            .find(|row| row.id == id)
            .unwrap()
            .lifecycle_reservation
            .is_none());
        assert!(state
            .instances
            .read()
            .await
            .iter()
            .find(|row| row.id == id)
            .unwrap()
            .lifecycle_reservation
            .is_none());
        let response =
            crate::server::runtime::get_runtime_snapshot(axum::extract::State(state.clone())).await;
        let bytes = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024).await?;
        let snapshot: crate::daemon::RuntimeSnapshot = serde_json::from_slice(&bytes)?;
        assert!(
            snapshot
                .contents
                .sessions
                .iter()
                .find(|row| row.id == id)
                .unwrap()
                .lifecycle_reservation
                .is_none(),
            "resnapshot must reflect the completed reservation release without a push loop"
        );
        Ok(())
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn sandbox_migration_publishes_planning_and_the_entire_committed_cohort() -> Result<()> {
        for damage in ["none", "groups", "sessions", "revoked"] {
            let _guard = crate::session::test_support::isolate_app_dir();
            let home = dirs::home_dir().unwrap();
            std::fs::create_dir_all(home.join(".gemini/sandbox/history"))?;
            std::fs::write(
                home.join(".gemini/sandbox/history/owned.json"),
                b"conversation",
            )?;
            let mut rows = Vec::new();
            let mut saved = Vec::new();
            for profile in ["native-migration-a", "native-migration-b"] {
                let storage = Storage::new_unwatched(profile)?;
                let mut row = Instance::new(profile, "/tmp/native-migration");
                row.source_profile = profile.to_owned();
                row.tool = "gemini".to_owned();
                row.status = crate::session::Status::Stopped;
                row.sandbox_store_generation = 0;
                row.sandbox_info = Some(crate::session::SandboxInfo {
                    enabled: true,
                    container_id: None,
                    image: "test-image".to_owned(),
                    container_name: format!("test-{}", row.id),
                    extra_env: None,
                    custom_instruction: None,
                    before_start_env: Vec::new(),
                    container_workdir: None,
                });
                let mut raw = serde_json::to_value(&row)?;
                raw["future_field"] = serde_json::json!({"preserved": true});
                let bytes = serde_json::to_vec(&vec![raw])?;
                std::fs::write(storage.sessions_path(), &bytes)?;
                saved.push((storage.sessions_path().to_owned(), bytes));
                rows.push(row);
            }
            let mut legacy = rows[0].clone();
            legacy.id = Instance::new("legacy", "/tmp/legacy").id;
            let legacy_path = crate::session::get_app_dir()?.join("sessions.json");
            let legacy_bytes = serde_json::to_vec(&vec![&legacy])?;
            std::fs::write(&legacy_path, &legacy_bytes)?;
            saved.push((legacy_path.clone(), legacy_bytes));
            let state = crate::server::test_support::build_test_app_state(rows.clone());
            *state.canonical_metadata.write().await =
                super::super::reload::load_all_profiles(&state.file_watch)?.metadata;
            let previous = serde_json::to_value(&*state.instances.read().await)?;
            let groups_path = saved[1].0.with_file_name("groups.json");
            if damage == "groups" {
                std::fs::write(&groups_path, br#"[{"path":5}]"#)?;
            }
            if damage == "sessions" {
                std::fs::write(&saved[1].0, b"{")?;
                saved[1].1 = b"{".to_vec();
            }
            let worker_state = state.clone();
            let owner = rows[0].clone();
            let (outcome, saw_plan) =
                tokio::task::spawn_blocking(move || -> Result<(Result<()>, bool)> {
                    let store = NativeSessionStore::open(
                        worker_state.clone(),
                        &owner.source_profile,
                        None,
                    )?;
                    let saw_plan = std::cell::Cell::new(false);
                    let outcome = store.run_sandbox_migration(|| {
                        crate::migrations::v027_isolate_sandbox_stores::migrate_instance_with(
                            &owner.id,
                            &|_| {
                                if worker_state
                                    .instances
                                    .blocking_read()
                                    .iter()
                                    .any(|row| !row.sandbox_store_transition_paths.is_empty())
                                {
                                    saw_plan.set(true);
                                    if damage == "revoked" {
                                        *worker_state.canonical_health.blocking_write() =
                                            crate::daemon::RuntimeHealth::Degraded {
                                                code: crate::daemon::ReloadFailureCode::Metadata,
                                                profiles: Vec::new(),
                                            };
                                    }
                                }
                                Ok(false)
                            },
                            &|_| Ok(true),
                            Some(&store),
                        )
                    });
                    Ok((outcome, saw_plan.get()))
                })
                .await??;
            if damage == "revoked" {
                assert!(outcome
                    .expect_err("revoked authority must stop the copy")
                    .is::<NativeStoreUnavailable>());
                assert!(saw_plan);
                let current = state.instances.read().await;
                for row in current.iter() {
                    assert!(row.sandbox_store_generation < 2);
                    assert!(!row.sandbox_store_transition_paths.is_empty());
                    let private = crate::session::config::container_config::sandbox_store_dir(
                        "gemini", &home, None, &row.id,
                    )?
                    .unwrap();
                    assert!(!private.join("history/owned.json").exists());
                }
                assert_eq!(
                    std::fs::read(home.join(".gemini/sandbox/history/owned.json"))?,
                    b"conversation"
                );
                continue;
            }
            if damage != "none" {
                let error =
                    outcome.expect_err("partial metadata must reject the entire checkpoint");
                assert!(error.is::<NativeStoreUnavailable>());
                assert_eq!(
                    serde_json::to_value(&*state.instances.read().await)?,
                    previous
                );
                assert_eq!(
                    *state.canonical_health.read().await,
                    crate::daemon::RuntimeHealth::Degraded {
                        code: crate::daemon::ReloadFailureCode::ProfileData,
                        profiles: vec!["native-migration-b".to_owned()],
                    }
                );
                for (path, bytes) in saved {
                    assert_eq!(std::fs::read(path)?, bytes);
                }
                if damage == "groups" {
                    assert_eq!(std::fs::read(&groups_path)?, br#"[{"path":5}]"#);
                }
                assert!(!groups_path.with_file_name("groups.corrupt.jsonl").exists());
                continue;
            }
            outcome?;
            assert!(
                saw_plan,
                "the pending transition must be visible before the copy finishes"
            );
            let current = state.instances.read().await;
            assert!(current.iter().all(|row| row.id != legacy.id));
            let root: serde_json::Value = serde_json::from_slice(&std::fs::read(legacy_path)?)?;
            assert_eq!(root[0]["sandbox_store_generation"], 2);
            for original in rows {
                let row = current.iter().find(|row| row.id == original.id).unwrap();
                assert_eq!(row.sandbox_store_generation, 2);
                assert!(row.sandbox_store_transition_paths.is_empty());
                assert_eq!(row.status, crate::session::Status::Stopped);
                let storage = Storage::new_unwatched(&original.source_profile)?;
                let disk: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(storage.sessions_path())?)?;
                assert_eq!(disk[0]["sandbox_store_generation"], 2);
                assert_eq!(
                    disk[0]["future_field"],
                    serde_json::json!({"preserved": true})
                );
                let private = crate::session::config::container_config::sandbox_store_dir(
                    "gemini", &home, None, &row.id,
                )?
                .unwrap();
                assert_eq!(
                    std::fs::read(private.join("history/owned.json"))?,
                    b"conversation"
                );
            }
        }
        Ok(())
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn capture_ownership_rejects_incomplete_peers_and_replaced_profiles() -> Result<()> {
        for replace_owner in [false, true] {
            let app = tempfile::tempdir()?;
            let _guard = crate::session::test_support::isolate_app_dir_at(app.path());
            let owner = Storage::new_unwatched("native-capture-owner")?;
            let peer = Storage::new_unwatched("native-capture-peer")?;
            let mut current = Instance::new("owner", "/tmp/native-capture-owner");
            current.source_profile = owner.profile().to_owned();
            current.tool = "gemini".to_owned();
            current.sandbox_info = Some(crate::session::SandboxInfo {
                enabled: true,
                container_id: None,
                image: "test-image".to_owned(),
                container_name: "test-owner".to_owned(),
                extra_env: None,
                custom_instruction: None,
                before_start_env: Vec::new(),
                container_workdir: None,
            });
            owner.update(|rows, _| {
                rows.push(current.clone());
                Ok(())
            })?;
            let mut peer_row = Instance::new("peer", "/tmp/native-capture-peer");
            peer_row.source_profile = peer.profile().to_owned();
            peer.update(|rows, _| {
                rows.push(peer_row.clone());
                Ok(())
            })?;
            let state =
                crate::server::test_support::build_test_app_state(vec![current.clone(), peer_row]);
            *state.canonical_metadata.write().await =
                super::super::reload::load_all_profiles(&state.file_watch)?.metadata;
            let previous = serde_json::to_value(&*state.instances.read().await)?;
            let capture_store = tempfile::tempdir()?;
            let capture_path = capture_store.path().to_owned();
            let displaced = app.path().join("displaced-owner");
            let worker_state = state.clone();
            let error = tokio::task::spawn_blocking(move || -> Result<anyhow::Error> {
                let store = NativeSessionStore::open(worker_state, owner.profile(), None)?;
                let damaged_path = if replace_owner {
                    std::fs::rename(owner.sessions_path().parent().unwrap(), displaced)?;
                    let replacement = Storage::new_unwatched(owner.profile())?;
                    replacement.update(|rows, _| {
                        rows.push(Instance::new("replacement", "/tmp/replacement"));
                        Ok(())
                    })?;
                    replacement.sessions_path().to_owned()
                } else {
                    std::fs::write(peer.sessions_path(), br#"[{"id":5}]"#)?;
                    peer.sessions_path().to_owned()
                };
                let bytes = std::fs::read(&damaged_path)?;
                let error = store
                    .managed_capture_store_is_exclusive(
                        &current,
                        crate::agents::SessionCaptureBackend::Gemini,
                        &capture_path,
                    )
                    .expect_err("incomplete ownership must not authorize capture");
                assert_eq!(std::fs::read(&damaged_path)?, bytes);
                assert!(!damaged_path
                    .with_file_name("sessions.corrupt.jsonl")
                    .exists());
                Ok(error)
            })
            .await??;
            assert!(error.downcast_ref::<NativeStoreUnavailable>().is_some());
            assert_eq!(
                serde_json::to_value(&*state.instances.read().await)?,
                previous
            );
            assert_eq!(
                *state.canonical_health.read().await,
                crate::daemon::RuntimeHealth::Degraded {
                    code: crate::daemon::ReloadFailureCode::ProfileData,
                    profiles: vec![if replace_owner {
                        "native-capture-owner"
                    } else {
                        "native-capture-peer"
                    }
                    .to_owned()],
                }
            );
        }
        Ok(())
    }
}
