//! Shared live-terminal readiness, rescue and WebSocket close helpers.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::{CloseFrame, Message, WebSocket};

use super::AppState;
use crate::session::SessionStore;

pub(super) fn legacy_tool_scope(
    rows: &[crate::session::Instance],
    raw_tools: &std::collections::HashMap<String, Vec<String>>,
) -> crate::tmux::LegacyToolScope {
    let mut tool_names: Vec<_> = raw_tools.values().flatten().cloned().collect();
    tool_names.extend(rows.iter().flat_map(|row| {
        row.auxiliary
            .iter()
            .filter_map(|observation| match &observation.target {
                crate::session::AuxiliaryTarget::Tool { tool_name } => Some(tool_name.clone()),
                _ => None,
            })
    }));
    tool_names.sort_unstable();
    tool_names.dedup();
    crate::tmux::LegacyToolScope {
        rows: rows
            .iter()
            .map(|row| {
                (
                    row.source_profile.clone(),
                    row.id.clone(),
                    crate::tmux::Session::generate_name(&row.id, &row.title),
                )
            })
            .collect(),
        tools: tool_names
            .into_iter()
            .map(|name| {
                let prefix = crate::tmux::ToolSession::name_prefix(&name);
                (name, prefix)
            })
            .collect(),
    }
}

pub(super) fn legacy_tool_scope_for_panes(
    rows: &[crate::session::Instance],
    raw_tools: &std::collections::HashMap<String, Vec<String>>,
    panes: Option<&std::collections::HashMap<String, crate::tmux::PaneMetadata>>,
) -> Option<crate::tmux::LegacyToolScope> {
    let panes = panes?;
    panes
        .iter()
        .any(|(name, pane)| {
            name.starts_with(crate::tmux::TOOL_PREFIX)
                && matches!(pane.tool_owner, crate::tmux::ToolPaneOwner::Unmarked)
        })
        .then(|| legacy_tool_scope(rows, raw_tools))
}

pub(super) fn observe_auxiliary(
    id: &str,
    title: &str,
    target: &crate::session::AuxiliaryTarget,
    panes: Option<&std::collections::HashMap<String, crate::tmux::PaneMetadata>>,
    profile: &str,
    legacy_scope: Option<&crate::tmux::LegacyToolScope>,
) -> crate::session::PaneObservation {
    use crate::session::{AuxiliaryTarget, PaneObservation};
    let Some(panes) = panes else {
        return PaneObservation::default();
    };
    let metadata = match target {
        AuxiliaryTarget::Host { index } => {
            crate::tmux::TerminalSession::metadata_in(id, title, *index, panes)
        }
        AuxiliaryTarget::Container { index } => {
            crate::tmux::ContainerTerminalSession::metadata_in(id, title, *index, panes)
        }
        AuxiliaryTarget::Tool { tool_name } => {
            let owned = crate::tmux::ToolSession::metadata_in(id, title, tool_name, panes);
            if owned.is_err() {
                if let Some(scope) = legacy_scope {
                    if let Ok(Some((name, metadata))) = crate::tmux::ToolSession::legacy_metadata_in(
                        id, title, tool_name, profile, panes, scope,
                    ) {
                        if let (Ok(identity), Some(server)) = (
                            crate::tmux::ToolSession::legacy_identity(metadata),
                            metadata.server.as_ref(),
                        ) {
                            return PaneObservation {
                                physical: None,
                                state: crate::session::PanePresence::Unknown,
                                tmux_session: Some(name.to_owned()),
                                legacy_tool: Some(crate::session::LegacyToolObservation {
                                    identity,
                                    server: server.clone(),
                                }),
                            };
                        }
                    }
                }
            }
            owned
        }
    };
    observe_metadata(metadata)
}

fn observe_metadata(
    metadata: anyhow::Result<Option<(std::borrow::Cow<'_, str>, &crate::tmux::PaneMetadata)>>,
) -> crate::session::PaneObservation {
    use crate::session::{PaneObservation, PanePresence};
    match metadata {
        Ok(Some((name, metadata))) => PaneObservation {
            physical: metadata.physical_observation(),
            state: if metadata.pane_dead {
                PanePresence::Dead
            } else {
                PanePresence::Alive
            },
            tmux_session: Some(name.into_owned()),
            legacy_tool: None,
        },
        Ok(None) => PaneObservation {
            physical: None,
            state: PanePresence::Absent,
            tmux_session: None,
            legacy_tool: None,
        },
        Err(_) => PaneObservation::default(),
    }
}

fn observe_agent(
    instance: &crate::session::Instance,
    panes: Option<&std::collections::HashMap<String, crate::tmux::PaneMetadata>>,
) -> crate::session::PaneObservation {
    if instance.is_structured() {
        return observe_metadata(Ok(None));
    }
    let Some(panes) = panes else {
        return Default::default();
    };
    observe_metadata(
        crate::tmux::agent_pane_metadata_in(panes, &instance.id, &instance.title).map(|metadata| {
            metadata.map(|(name, metadata)| (std::borrow::Cow::Borrowed(name), metadata))
        }),
    )
}

pub(super) fn seed_pending_tool_targets(
    rows: &mut [crate::session::Instance],
    pending: &[(String, String)],
) {
    for row in rows {
        for (_, tool_name) in pending.iter().filter(|(id, _)| id == &row.id) {
            let target = crate::session::AuxiliaryTarget::Tool {
                tool_name: tool_name.clone(),
            };
            if !row
                .auxiliary
                .iter()
                .any(|observation| observation.target == target)
            {
                row.auxiliary.push(crate::session::AuxiliaryObservation {
                    target,
                    pane: Default::default(),
                });
            }
        }
    }
}

pub(super) fn sample_panes(
    instance: &mut crate::session::Instance,
    tools: &[String],
    panes: Option<&std::collections::HashMap<String, crate::tmux::PaneMetadata>>,
    legacy_scope: Option<&crate::tmux::LegacyToolScope>,
    pending: Option<&[(String, String)]>,
) {
    use crate::session::{AuxiliaryObservation, AuxiliaryTarget, PanePresence};
    instance.agent_pane = observe_agent(instance, panes);
    let sandboxed = instance.is_sandboxed();
    for target in [
        Some(AuxiliaryTarget::Host { index: 0 }),
        sandboxed.then_some(AuxiliaryTarget::Container { index: 0 }),
    ]
    .into_iter()
    .flatten()
    {
        if !instance
            .auxiliary
            .iter()
            .any(|observation| observation.target == target)
        {
            instance.auxiliary.push(AuxiliaryObservation {
                target,
                pane: Default::default(),
            });
        }
    }
    for name in tools.iter().chain(
        pending
            .into_iter()
            .flatten()
            .filter(|(id, _)| id == &instance.id)
            .map(|(_, tool)| tool),
    ) {
        if !instance.auxiliary.iter().any(|observation| matches!(&observation.target, AuxiliaryTarget::Tool { tool_name } if tool_name == name)) {
            instance.auxiliary.push(AuxiliaryObservation {
                target: AuxiliaryTarget::Tool { tool_name: name.clone() },
                pane: Default::default(),
            });
        }
    }
    for observation in &mut instance.auxiliary {
        observation.pane = observe_auxiliary(
            &instance.id,
            &instance.title,
            &observation.target,
            panes,
            &instance.source_profile,
            legacy_scope,
        );
        if pending.is_none() && matches!(&observation.target, AuxiliaryTarget::Tool { .. }) {
            observation.pane.state = PanePresence::Unknown;
        }
    }
    instance.auxiliary.retain(|observation| {
        observation.pane.state != PanePresence::Absent
            || match &observation.target {
                AuxiliaryTarget::Host { index: 0 } => true,
                AuxiliaryTarget::Container { index: 0 } => sandboxed,
                AuxiliaryTarget::Tool { tool_name } => {
                    tools.contains(tool_name)
                        || pending.is_none_or(|owners| {
                            owners
                                .iter()
                                .any(|(id, tool)| id == &instance.id && tool == tool_name)
                        })
                }
                _ => false,
            }
    });
}

pub(super) async fn initial_tool_retirement_inventory(
    state: &Arc<AppState>,
) -> Option<Vec<(String, String)>> {
    let inventory =
        tokio::task::spawn_blocking(crate::tmux::ToolSession::pending_retirement_targets)
            .await
            .map_err(anyhow::Error::from)
            .and_then(|inventory| inventory);
    match inventory {
        Ok(inventory) => Some(inventory),
        Err(error) => {
            tracing::warn!(target: "server.status", %error, "tool retirement ownership is unavailable; retaining read-only runtime inspection");
            let mut health = state.canonical_health.write().await;
            if *health == crate::daemon::RuntimeHealth::Healthy {
                *health = crate::daemon::RuntimeHealth::Degraded {
                    code: crate::daemon::ReloadFailureCode::Metadata,
                    profiles: Vec::new(),
                };
            }
            state.runtime.request_publish();
            None
        }
    }
}
pub(super) async fn seed_initial_observations(
    state: &Arc<AppState>,
    pending: Option<&[(String, String)]>,
) -> anyhow::Result<()> {
    let panes = tokio::task::spawn_blocking(crate::tmux::batch_pane_metadata).await?;
    if let Err(error) = &panes {
        tracing::warn!(target: "server.status", %error, "initial pane observations are unavailable");
    }
    let metadata = state.canonical_metadata.read().await;
    let mut rows = state.instances.write().await;
    if let Some(pending) = pending {
        seed_pending_tool_targets(&mut rows, pending);
    }
    let legacy_scope =
        legacy_tool_scope_for_panes(&rows, &metadata.raw_tool_names, panes.as_ref().ok());
    for row in &mut *rows {
        let tools = metadata
            .auxiliary_tools
            .get(&row.source_profile)
            .map(Vec::as_slice)
            .unwrap_or_default();
        sample_panes(
            row,
            tools,
            panes.as_ref().ok(),
            legacy_scope.as_ref(),
            pending,
        );
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
#[error("Auxiliary target is unavailable")]
pub(super) struct AuxiliaryTargetUnavailable;

struct SampledAuxiliary {
    _ownership: (crate::session::StorageFlock, crate::session::StorageFlock),
    tmux_session: String,
    physical: Arc<crate::session::LegacyToolObservation>,
}

// Retain these locks through any launch-owned state handoff.
fn sample_auxiliary_after_ensure(
    native: &super::session_store::NativeSessionStore,
    instance: &mut crate::session::Instance,
    target: crate::session::AuxiliaryTarget,
) -> anyhow::Result<SampledAuxiliary> {
    let generation = instance.lifecycle_generation;
    let title = instance.title.clone();
    let ownership = instance.acquire_auxiliary_locks_in(native)?;
    anyhow::ensure!(
        instance.lifecycle_generation == generation && instance.title == title,
        crate::session::LifecycleReservationError::Superseded
    );
    let panes = crate::tmux::batch_pane_metadata();
    let pane = observe_auxiliary(
        &instance.id,
        &instance.title,
        &target,
        panes.as_ref().ok(),
        &instance.source_profile,
        None,
    );
    let sampled = if pane.state == crate::session::PanePresence::Alive {
        pane.tmux_session
            .as_ref()
            .zip(pane.physical.as_ref())
            .map(|(name, physical)| (name.clone(), physical.clone()))
    } else {
        None
    };
    native.adopt_auxiliary_observations(
        instance,
        std::iter::once(crate::session::AuxiliaryObservation { target, pane }),
    )?;
    panes?;
    let (tmux_session, physical) = sampled.ok_or(AuxiliaryTargetUnavailable)?;
    Ok(SampledAuxiliary {
        _ownership: ownership,
        tmux_session,
        physical,
    })
}

pub(super) async fn publish_auxiliary_after_ensure(
    state: &Arc<AppState>,
    mut instance: crate::session::Instance,
    target: crate::session::AuxiliaryTarget,
    created: bool,
    native: super::session_store::NativeSessionStore,
) -> anyhow::Result<crate::daemon::MutationReceipt<crate::daemon::TerminalTarget>> {
    let namespace = state.profile_namespace.read().await;
    let lock = state.instance_lock(&instance.id).await;
    let guard = lock.lock().await;
    let id = instance.id.clone();
    let expected = (
        instance.lifecycle_generation,
        instance.source_profile.clone(),
    );
    let created_at = instance.created_at;
    let expected_target = target.clone();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let sampled = sample_auxiliary_after_ensure(&native, &mut instance, target)?;
        Ok(crate::daemon::TerminalTarget {
            tmux_session: sampled.tmux_session,
            physical: Some(sampled.physical),
            lifecycle_generation: instance.lifecycle_generation,
            profile: instance.source_profile,
            status: if created {
                crate::daemon::TerminalTargetStatus::Created
            } else {
                crate::daemon::TerminalTargetStatus::Exists
            },
        })
    })
    .await?;
    drop(guard);
    drop(namespace);
    let snapshot = state.runtime.publish(state).await?;
    let target = result?;
    anyhow::ensure!(
        snapshot.value.contents.health == crate::daemon::RuntimeHealth::Healthy,
        crate::session::NativeStoreUnavailable
    );
    let row = snapshot
        .value
        .contents
        .sessions
        .iter()
        .find(|row| {
            row.id == id
                && row.lifecycle_generation == expected.0
                && row.profile == expected.1
                && row.created_at.parse::<chrono::DateTime<chrono::Utc>>().ok() == Some(created_at)
        })
        .ok_or(crate::session::LifecycleReservationError::Superseded)?;
    row.auxiliary
        .iter()
        .find(|observation| observation.target == expected_target)
        .filter(|observation| {
            observation.pane.state == crate::session::PanePresence::Alive
                && observation.pane.physical == target.physical
                && observation.pane.tmux_session.as_deref() == Some(target.tmux_session.as_str())
        })
        .ok_or(AuxiliaryTargetUnavailable)?;
    Ok(crate::daemon::MutationReceipt {
        cursor: snapshot.value.cursor.clone(),
        outcome: target,
    })
}

pub(super) enum AuxiliaryStopRequest {
    Target(crate::daemon::StopAuxiliaryBody),
    PairedTerminals { index: u32 },
}

pub(super) async fn stop_native_auxiliary(
    state: &Arc<AppState>,
    id: &str,
    request: AuxiliaryStopRequest,
) -> anyhow::Result<crate::daemon::RuntimeCursor> {
    use crate::session::{
        AuxiliaryObservation, AuxiliaryTarget, LifecycleOperation, LifecycleReservationError,
        SessionStore,
    };
    let paired = matches!(request, AuxiliaryStopRequest::PairedTerminals { .. });
    let (targets, adoption) = match request {
        AuxiliaryStopRequest::Target(body) => {
            anyhow::ensure!(
                body.adoption.is_none() || matches!(&body.target, AuxiliaryTarget::Tool { .. }),
                AuxiliaryTargetUnavailable
            );
            ([Some(body.target), None], body.adoption)
        }
        AuxiliaryStopRequest::PairedTerminals { index } => {
            anyhow::ensure!(index > 0, AuxiliaryTargetUnavailable);
            (
                [
                    Some(AuxiliaryTarget::Host { index }),
                    Some(AuxiliaryTarget::Container { index }),
                ],
                None,
            )
        }
    };
    let tool_only = targets
        .iter()
        .flatten()
        .any(|target| matches!(target, AuxiliaryTarget::Tool { .. }));
    for target in targets.iter().flatten() {
        anyhow::ensure!(
            !matches!(target, AuxiliaryTarget::Host { index } | AuxiliaryTarget::Container { index } if *index > MAX_TERMINAL_INDEX),
            AuxiliaryTargetUnavailable
        );
    }
    let namespace = state.profile_namespace.read().await;
    let lock = state.instance_lock(id).await;
    let guard = lock.lock().await;
    let mut instance = state
        .instances
        .read()
        .await
        .iter()
        .find(|row| row.id == id)
        .cloned()
        .ok_or(LifecycleReservationError::Superseded)?;
    let worker_state = state.clone();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let native = super::session_store::NativeSessionStore::open(
            worker_state.clone(),
            &instance.source_profile,
            None,
        )?;
        let _ownership = instance.acquire_auxiliary_locks_in(&native)?;
        anyhow::ensure!(
            !worker_state.shutdown.is_cancelled(),
            crate::session::NativeStoreUnavailable
        );
        let config = tool_only
            .then(|| native.configuration(Some(native.storage().profile())))
            .transpose()?;
        for target in targets.iter().flatten() {
            let known = instance
                .auxiliary
                .iter()
                .any(|observation| &observation.target == target);
            match target {
                AuxiliaryTarget::Container { .. } => {
                    anyhow::ensure!(
                        paired || instance.is_sandboxed() || known,
                        AuxiliaryTargetUnavailable
                    )
                }
                AuxiliaryTarget::Tool { tool_name } => {
                    let config = config.as_ref().expect("tool request configuration");
                    anyhow::ensure!(
                        known
                            || crate::tmux::ToolSession::has_pending_retirement(
                                &instance.id,
                                tool_name
                            )?
                            || config
                                .tools
                                .get(tool_name)
                                .is_some_and(|tool| !tool.background && !tool.command.is_empty()),
                        AuxiliaryTargetUnavailable
                    );
                }
                AuxiliaryTarget::Host { .. } => {}
            }
        }
        let legacy_scope = if let Some(adoption) = &adoption {
            anyhow::ensure!(
                adoption.profile == instance.source_profile
                    && adoption.lifecycle_generation == instance.lifecycle_generation,
                LifecycleReservationError::Superseded
            );
            let loaded = super::reload::load_all_profiles(&worker_state.file_watch)
                .map_err(|error| error.source.context(crate::session::NativeStoreUnavailable))?;
            Some(legacy_tool_scope(
                &loaded.instances,
                &loaded.metadata.raw_tool_names,
            ))
        } else {
            None
        };
        if tool_only {
            instance.advance_auxiliary_generation_in(&native)?;
        } else {
            instance.acquire_lifecycle_reservation(&native, LifecycleOperation::Stop, None)?;
        }
        let effect = (|| -> anyhow::Result<()> {
            native.check_available()?;
            for target in targets.iter().flatten() {
                if let AuxiliaryTarget::Tool { tool_name } = target {
                    if crate::tmux::ToolSession::settle_pending_retirement(
                        &instance.id,
                        tool_name,
                        adoption.as_ref(),
                    )? {
                        return Ok(());
                    }
                }
            }
            let panes = crate::tmux::batch_pane_metadata()?;
            for target in targets.iter().flatten() {
                match target {
                    AuxiliaryTarget::Host { index } => crate::tmux::TerminalSession::from_snapshot(
                        &instance.id,
                        &instance.title,
                        *index,
                        &panes,
                    )
                    .map_err(|error| error.context(AuxiliaryTargetUnavailable))?
                    .kill(),
                    AuxiliaryTarget::Container { index } => {
                        crate::tmux::ContainerTerminalSession::from_snapshot(
                            &instance.id,
                            &instance.title,
                            *index,
                            &panes,
                        )
                        .map_err(|error| error.context(AuxiliaryTargetUnavailable))?
                        .kill()
                    }
                    AuxiliaryTarget::Tool { tool_name } => {
                        let (tool, identity) = if let Some(adoption) = &adoption {
                            crate::tmux::ToolSession::adopt_legacy_snapshot(
                                &instance.id,
                                &instance.title,
                                tool_name,
                                &instance.source_profile,
                                adoption,
                                &panes,
                                legacy_scope.as_ref().expect("adoption scope"),
                            )?
                        } else {
                            let tool = crate::tmux::ToolSession::from_snapshot(
                                &instance.id,
                                &instance.title,
                                tool_name,
                                &panes,
                            )
                            .map_err(|error| error.context(AuxiliaryTargetUnavailable))?;
                            let Some(metadata) = panes.get(tool.session_name()) else {
                                continue;
                            };
                            (tool, crate::tmux::ToolSession::legacy_identity(metadata)?)
                        };
                        let server = adoption
                            .as_ref()
                            .map(|adoption| adoption.server.as_ref())
                            .or_else(|| {
                                panes
                                    .get(tool.session_name())
                                    .and_then(|pane| pane.server.as_deref())
                            });
                        tool.kill_verified(&instance.id, tool_name, &identity, server)
                    }
                }?;
            }
            Ok(())
        })();
        let panes = crate::tmux::batch_pane_metadata();
        if !tool_only {
            instance.release_lifecycle_reservation(&native, LifecycleOperation::Stop)?;
        }
        let mut all_absent = true;
        native.adopt_auxiliary_observations(
            &instance,
            targets.into_iter().flatten().map(|target| {
                let pane = observe_auxiliary(
                    &instance.id,
                    &instance.title,
                    &target,
                    panes.as_ref().ok(),
                    &instance.source_profile,
                    legacy_scope.as_ref(),
                );
                all_absent &= pane.state == crate::session::PanePresence::Absent;
                AuxiliaryObservation { target, pane }
            }),
        )?;
        effect?;
        panes?;
        anyhow::ensure!(all_absent, "Auxiliary target remains present after stop");
        Ok(())
    })
    .await
    .map_err(anyhow::Error::from)
    .and_then(|result| result);
    drop(guard);
    drop(namespace);
    let snapshot = state.runtime.publish(state).await?;
    result?;
    anyhow::ensure!(
        snapshot.value.contents.health == crate::daemon::RuntimeHealth::Healthy,
        crate::session::NativeStoreUnavailable
    );
    anyhow::ensure!(
        snapshot
            .value
            .contents
            .sessions
            .iter()
            .any(|row| row.id == id),
        LifecycleReservationError::Superseded
    );
    Ok(snapshot.value.cursor.clone())
}

/// Upper bound on the paired-terminal index a client may request. The web
/// dashboard owns the live set of terminal tabs, so a stray or hostile request
/// could otherwise spawn unbounded tmux sessions; this caps the blast radius.
/// 31 is far above any plausible tab count. See #2437.
pub(crate) const MAX_TERMINAL_INDEX: u32 = 31;

/// Close code we send when the live capture loop found the underlying pane
/// gone. The web live hook treats this as "stop retrying immediately, surface
/// the manual reconnect banner" rather than burning the retry budget against a
/// permanently broken pane. Picked from the application-reserved 4000-4999
/// range; not used elsewhere. See #1107.
pub(crate) const CLOSE_CODE_PTY_DEAD: u16 = 4001;

/// WebSocket close code 1001 ("going away"). Sent when the daemon is
/// shutting down so the client can distinguish a server-side exit from
/// a transient transport error and skip its reconnect backoff for one
/// cycle. See #1198.
pub(crate) const CLOSE_CODE_GOING_AWAY: u16 = 1001;

/// WebSocket close code 1013 ("try again later"). Sent when the tmux
/// pane is not ready within the bounded readiness window. Browser
/// retries on the fast-start ladder. Distinct from 4001 (permanently dead
/// pane) so logs separate transient warm-up from genuine failure. See #1455.
pub(crate) const CLOSE_CODE_TRY_AGAIN_LATER: u16 = 1013;

/// Total time we'll spend waiting for the tmux session + pane to be
/// attachable before giving up and closing 1013. 2s covers tmux warm-up
/// across the slow machines we've seen reports from while staying short
/// enough that a truly dead pane doesn't hold the upgrade open for the
/// user. See #1455.
const TMUX_READY_TIMEOUT: Duration = Duration::from_millis(2000);

/// Poll interval for the readiness wait. 50ms gives ~40 probes inside
/// the 2s window; each probe shells out to `tmux has-session` and (if
/// that passes) `tmux list-panes`, which is cheap.
const TMUX_READY_POLL: Duration = Duration::from_millis(50);

/// Resolve the paired host shell for live viewing.
pub(crate) async fn respawn_paired_if_dead(
    state: &Arc<AppState>,
    id: &str,
    inst: &crate::session::Instance,
    index: u32,
) -> anyhow::Result<String> {
    let tmux_name =
        crate::tmux::TerminalSession::resolve_name_indexed(&inst.id, &inst.title, index);
    if state.read_only {
        return Ok(tmux_name);
    }

    let namespace = state.profile_namespace.read().await;
    let lock = state.instance_lock(id).await;
    let guard = lock.lock().await;
    let mut instance = state
        .instances
        .read()
        .await
        .iter()
        .find(|row| row.id == id)
        .cloned()
        .ok_or(crate::session::LifecycleReservationError::Superseded)?;
    let worker_state = state.clone();
    let name = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let native = super::session_store::NativeSessionStore::open(
            worker_state,
            &instance.source_profile,
            None,
        )?;
        instance.start_terminal_with_size_indexed_in(index, None, &native)?;
        let sampled = sample_auxiliary_after_ensure(
            &native,
            &mut instance,
            crate::session::AuxiliaryTarget::Host { index },
        )?;
        Ok(sampled.tmux_session)
    })
    .await??;
    drop(guard);
    drop(namespace);
    let snapshot = state.runtime.publish(state).await?;
    anyhow::ensure!(
        snapshot.value.contents.health == crate::daemon::RuntimeHealth::Healthy,
        crate::session::NativeStoreUnavailable
    );
    anyhow::ensure!(
        snapshot
            .value
            .contents
            .sessions
            .iter()
            .any(|row| row.id == id),
        crate::session::LifecycleReservationError::Superseded
    );
    Ok(name)
}

/// Container-terminal counterpart of [`respawn_paired_if_dead`].
pub(crate) async fn respawn_container_if_dead(
    state: &Arc<AppState>,
    id: &str,
    inst: &crate::session::Instance,
    index: u32,
) -> anyhow::Result<String> {
    let tmux_name =
        crate::tmux::ContainerTerminalSession::resolve_name_indexed(&inst.id, &inst.title, index);
    if state.read_only {
        return Ok(tmux_name);
    }

    let binding = crate::session::Storage::open_reference(&inst.source_profile)?;
    let expected = crate::daemon::NativePreparationIdentity::from_instance(
        inst,
        binding.bound_profile_identity()?,
    );
    let (target, _) =
        ensure_native_container_terminal(state, id, index, None, Some(expected)).await?;
    Ok(target.tmux_session)
}

pub(crate) async fn ensure_native_container_terminal(
    state: &Arc<AppState>,
    id: &str,
    index: u32,
    size: Option<(u16, u16)>,
    expected: Option<crate::daemon::NativePreparationIdentity>,
) -> anyhow::Result<(crate::daemon::TerminalTarget, crate::daemon::RuntimeCursor)> {
    let worker_state = state.clone();
    let worker_id = id.to_owned();
    let target = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let handle = tokio::runtime::Handle::current();
        let acquire_scope = || {
            let namespace = handle.block_on(worker_state.profile_namespace.read());
            let lock = handle.block_on(worker_state.instance_lock(&worker_id));
            let instance = handle.block_on(lock.lock_owned());
            (instance, namespace)
        };
        let mut scope = Some(acquire_scope());
        let mut instance = handle
            .block_on(worker_state.instances.read())
            .iter()
            .find(|row| row.id == worker_id)
            .cloned()
            .ok_or(crate::session::LifecycleReservationError::Superseded)?;
        anyhow::ensure!(
            expected
                .as_ref()
                .is_none_or(|expected| expected.matches(&instance)),
            crate::session::LifecycleReservationError::Superseded
        );
        let native = super::session_store::NativeSessionStore::open(
            worker_state.clone(),
            &instance.source_profile,
            None,
        )?;
        anyhow::ensure!(
            expected
                .as_ref()
                .is_none_or(|expected| expected.matches_source(native.storage())),
            crate::session::LifecycleReservationError::Superseded
        );
        let prior_identity_publisher = instance.identity_publisher_launched;
        let mut minted_env = false;
        let (_, created) = instance.start_container_terminal_with_hook_in(
            index,
            size,
            &native,
            |instance, config| {
                minted_env = true;
                drop(scope.take());
                let result = instance.mint_before_start_env(config);
                scope = Some(acquire_scope());
                result
            },
        )?;
        let SampledAuxiliary {
            _ownership,
            tmux_session,
            physical,
        } = sample_auxiliary_after_ensure(
            &native,
            &mut instance,
            crate::session::AuxiliaryTarget::Container { index },
        )?;
        let identity = (
            instance.lifecycle_generation,
            instance.source_profile.clone(),
        );
        let target = crate::daemon::TerminalTarget {
            physical: Some(physical),
            tmux_session,
            status: if created {
                crate::daemon::TerminalTargetStatus::Created
            } else {
                crate::daemon::TerminalTargetStatus::Exists
            },
            lifecycle_generation: identity.0,
            profile: identity.1,
        };
        let publication = handle.block_on(worker_state.publication.write());
        let mut rows = handle.block_on(worker_state.instances.write());
        let row = rows
            .iter_mut()
            .find(|row| row.id == worker_id)
            .ok_or(crate::session::LifecycleReservationError::Superseded)?;
        anyhow::ensure!(
            row.created_at == instance.created_at
                && row.lifecycle_generation == instance.lifecycle_generation
                && row.source_profile == instance.source_profile
                && row.title == instance.title,
            crate::session::LifecycleReservationError::Superseded
        );
        let identity_changed = instance.identity_publisher_launched != prior_identity_publisher;
        if identity_changed {
            row.identity_publisher_launched = instance.identity_publisher_launched;
        }
        if minted_env {
            if let (Some(current), Some(launched)) =
                (row.sandbox_info.as_mut(), instance.sandbox_info)
            {
                current.before_start_env = launched.before_start_env;
            }
        }
        if identity_changed || minted_env {
            worker_state
                .mutation_epoch
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            worker_state.runtime.request_publish();
        }
        drop(rows);
        drop(publication);
        Ok(target)
    })
    .await??;
    let snapshot = state.runtime.publish(state).await?;
    anyhow::ensure!(
        snapshot.value.contents.health == crate::daemon::RuntimeHealth::Healthy,
        crate::session::NativeStoreUnavailable
    );
    anyhow::ensure!(
        snapshot
            .value
            .contents
            .sessions
            .iter()
            .any(|row| row.id == id),
        crate::session::LifecycleReservationError::Superseded
    );
    Ok((target, snapshot.value.cursor.clone()))
}

/// Send a close frame on a socket we're about to drop before the main loop.
pub(crate) async fn close_early(socket: &mut WebSocket, code: u16, reason: &'static str) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: reason.into(),
        })))
        .await;
}

/// Outcome of one tmux-readiness probe. `Ready` lets the caller proceed
/// to capture the pane; `NotReady` means try again after the poll
/// interval; `Dead` short-circuits the wait when every pane is reported
/// dead (no point in polling further).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PaneReadiness {
    Ready,
    NotReady,
    Dead,
}

/// Parse `tmux list-panes -F "#{pane_dead}"` output: one line per pane,
/// each line `0` (alive) or `1` (dead). Empty output means the session
/// exists but has no panes yet (not ready). All-dead means the pane has
/// permanently exited.
fn parse_pane_dead_output(output: &str) -> PaneReadiness {
    let lines: Vec<&str> = output
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if lines.is_empty() {
        return PaneReadiness::NotReady;
    }
    if lines.contains(&"0") {
        PaneReadiness::Ready
    } else {
        PaneReadiness::Dead
    }
}

/// Poll `tmux has-session` + `tmux list-panes` at TMUX_READY_POLL until
/// the session has at least one alive pane, or until TMUX_READY_TIMEOUT
/// expires. Returns the final outcome so the caller can distinguish a
/// transient warm-up (`NotReady` -> retryable 1013) from a permanently
/// dead pane (`Dead` -> 4001 short-circuit). Bails out early on `Dead`
/// rather than polling further because no amount of waiting will make
/// an exited pane reattachable.
pub(crate) async fn wait_for_tmux_ready(tmux_name: &str) -> PaneReadiness {
    let deadline = Instant::now() + TMUX_READY_TIMEOUT;
    loop {
        match probe_tmux_readiness(tmux_name).await {
            PaneReadiness::Ready => return PaneReadiness::Ready,
            PaneReadiness::Dead => return PaneReadiness::Dead,
            PaneReadiness::NotReady => {
                if Instant::now() >= deadline {
                    return PaneReadiness::NotReady;
                }
                tokio::time::sleep(TMUX_READY_POLL).await;
            }
        }
    }
}

/// One probe iteration: `tmux has-session` then (on success) `tmux
/// list-panes -F "#{pane_dead}"`. Both shell out to the tmux binary;
/// they're cheap (microseconds in the happy path) so the 50ms poll
/// floor dominates wall time, not subprocess overhead.
async fn probe_tmux_readiness(tmux_name: &str) -> PaneReadiness {
    let name = tmux_name.to_string();
    tokio::task::spawn_blocking(move || {
        let has_session = crate::tmux::tmux_command()
            .args(["has-session", "-t", &name])
            .output();
        let has_session_ok = match has_session {
            Ok(o) => o.status.success(),
            Err(_) => false,
        };
        if !has_session_ok {
            return PaneReadiness::NotReady;
        }
        let panes = crate::tmux::tmux_command()
            .args(["list-panes", "-t", &name, "-F", "#{pane_dead}"])
            .output();
        match panes {
            Ok(o) if o.status.success() => {
                parse_pane_dead_output(&String::from_utf8_lossy(&o.stdout))
            }
            _ => PaneReadiness::NotReady,
        }
    })
    .await
    .unwrap_or(PaneReadiness::NotReady)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn assert_container_receipt_matches_sampled_physical(
        recreate_dead: bool,
    ) -> anyhow::Result<()> {
        use crate::session::{AuxiliaryObservation, AuxiliaryTarget, PanePresence};
        use crate::tmux::test_helpers::{only_pane_id, wait_for_pane_dead, TmuxTestSession};

        if !crate::tmux::is_tmux_available() {
            return Ok(());
        }
        let _home = crate::session::test_support::isolate_app_dir();
        crate::tmux::tool_retirement::initialize(&crate::session::get_app_dir()?)?;
        let mut row = crate::session::Instance::new("container receipt", "/tmp");
        row.source_profile = "default".into();
        row.sandbox_info = Some(crate::session::SandboxInfo {
            provider: None,
            enabled: true,
            container_id: None,
            image: "receipt-fixture".into(),
            container_name: "receipt-fixture".into(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            before_start_hook_env: Vec::new(),
            container_workdir: None,
        });
        let terminal = row.container_terminal_tmux_session_indexed(0)?;
        let _terminal = TmuxTestSession::from_name(terminal.name());
        // Inject only the pane command through the existing constructor. Ensure,
        // native-store reconciliation, observation and receipt production stay real.
        terminal.create_with_size("/tmp", Some("sleep 600"), None, "default")?;
        let target = AuxiliaryTarget::Container { index: 0 };
        let corpse = if recreate_dead {
            let pane_id = only_pane_id(terminal.name());
            assert!(crate::tmux::tmux_command()
                .args([
                    "set-option",
                    "-p",
                    "-t",
                    &pane_id,
                    "remain-on-exit",
                    "on",
                    ";",
                    "respawn-pane",
                    "-k",
                    "-t",
                    &pane_id,
                    "exit 0",
                ])
                .status()?
                .success());
            wait_for_pane_dead(&pane_id);
            let panes = crate::tmux::batch_pane_metadata()?;
            let observation = observe_auxiliary(
                &row.id,
                &row.title,
                &target,
                Some(&panes),
                &row.source_profile,
                None,
            );
            assert_eq!(observation.state, PanePresence::Dead);
            let physical = observation
                .physical
                .clone()
                .expect("confirmed corpse identity");
            row.auxiliary.push(AuxiliaryObservation {
                target: target.clone(),
                pane: observation,
            });
            terminal.kill()?;
            // Recreate through the same owned-tmux constructor, without requiring
            // a container engine. This isolates the receipt, not Docker launch.
            terminal.create_with_size("/tmp", Some("sleep 600"), None, "default")?;
            Some(physical)
        } else {
            assert!(row.auxiliary.is_empty());
            None
        };
        let panes = crate::tmux::batch_pane_metadata()?;
        let metadata = panes.get(terminal.name()).expect("owned fixture pane");
        assert_eq!(
            metadata.session_kind.as_deref(),
            Some(crate::tmux::SessionKind::ContainerTerminal.as_marker())
        );
        assert!(!metadata.pane_dead);
        let expected = metadata
            .physical_observation()
            .expect("live physical identity");
        if let Some(corpse) = &corpse {
            assert_ne!(corpse, &expected, "fixture must replace the corpse");
        }
        crate::server::test_support::seed_instances_on_disk_for_test("default", vec![row.clone()]);
        let state = crate::server::test_support::build_test_app_state(vec![row.clone()]);
        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;

        let result = ensure_native_container_terminal(&state, &row.id, 0, None, None).await;
        let sampled = state.instances.read().await[0]
            .auxiliary
            .iter()
            .find(|observation| observation.target == target)
            .expect("Ensure must adopt its actual sample")
            .pane
            .clone();
        assert_eq!(sampled.state, PanePresence::Alive);
        assert_eq!(sampled.physical.as_ref(), Some(&expected));
        let (receipt, _) =
            result.expect("first Ensure must return its newly sampled physical target");
        assert_eq!(receipt.tmux_session, terminal.name());
        assert_eq!(receipt.lifecycle_generation, row.lifecycle_generation);
        assert_eq!(receipt.profile, row.source_profile);
        if let Some(corpse) = corpse {
            assert_ne!(
                receipt.physical.as_ref(),
                Some(&corpse),
                "recreation receipt must not authorize the confirmed-dead pane"
            );
        }
        assert_eq!(
            receipt.physical.as_ref(),
            Some(&expected),
            "receipt must identify the physical pane sampled by this Ensure"
        );
        Ok(())
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn container_first_ensure_without_prior_observation_returns_sampled_physical(
    ) -> anyhow::Result<()> {
        assert_container_receipt_matches_sampled_physical(false).await
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn container_ensure_after_dead_fixture_recreation_returns_new_physical(
    ) -> anyhow::Result<()> {
        assert_container_receipt_matches_sampled_physical(true).await
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn unavailable_tool_journal_preserves_unknown_targets_and_refuses_native_writes(
    ) -> anyhow::Result<()> {
        for case in ["missing", "corrupt", "prior-profile-failure"] {
            let _home = crate::session::test_support::isolate_app_dir();
            if case != "missing" {
                std::fs::write(
                    crate::session::get_app_dir()?.join(crate::tmux::tool_retirement::FILE_NAME),
                    b"{",
                )?;
            }
            let mut row = crate::session::Instance::new("unavailable ownership", "/tmp");
            row.source_profile = "default".into();
            row.auxiliary.push(crate::session::AuxiliaryObservation {
                target: crate::session::AuxiliaryTarget::Tool {
                    tool_name: "removed-tool".into(),
                },
                pane: crate::session::PaneObservation {
                    physical: None,
                    state: crate::session::PanePresence::Alive,
                    tmux_session: Some("original-tool".into()),
                    legacy_tool: None,
                },
            });
            let state = crate::server::test_support::build_test_app_state(vec![row]);
            let prior = if case == "prior-profile-failure" {
                crate::daemon::RuntimeHealth::Degraded {
                    code: crate::daemon::ReloadFailureCode::ProfileData,
                    profiles: vec!["broken-profile".into()],
                }
            } else {
                crate::daemon::RuntimeHealth::Healthy
            };
            *state.canonical_health.write().await = prior.clone();
            assert!(initial_tool_retirement_inventory(&state).await.is_none());
            if prior == crate::daemon::RuntimeHealth::Healthy {
                assert!(matches!(
                    *state.canonical_health.read().await,
                    crate::daemon::RuntimeHealth::Degraded {
                        code: crate::daemon::ReloadFailureCode::Metadata,
                        ..
                    }
                ));
            } else {
                assert_eq!(*state.canonical_health.read().await, prior);
            }
            let panes = std::collections::HashMap::new();
            {
                let mut rows = state.instances.write().await;
                sample_panes(&mut rows[0], &[], Some(&panes), None, None);
                let target = rows[0].auxiliary.iter().find(|observation| matches!(
                    &observation.target, crate::session::AuxiliaryTarget::Tool { tool_name } if tool_name == "removed-tool"
                )).expect("unknown original target remains visible");
                assert_eq!(target.pane.state, crate::session::PanePresence::Unknown);
            }
            let error = tokio::task::spawn_blocking(move || {
                crate::server::session_store::NativeSessionStore::open(state, "default", None)
                    .err()
                    .expect("unknown ownership cannot authorize a native write")
            })
            .await?;
            assert!(error.is::<crate::session::NativeStoreUnavailable>());
        }
        Ok(())
    }
    fn prepare_tool_retirement(
        row: &crate::session::Instance,
    ) -> anyhow::Result<(
        crate::tmux::ToolSession,
        crate::tmux::test_helpers::TmuxTestSession,
        crate::process::CapturedProcessTree,
    )> {
        let name = crate::tmux::ToolSession::generate_name(&row.id, &row.title, "probe");
        let guard = crate::tmux::test_helpers::TmuxTestSession::from_name(&name);
        let tool = crate::tmux::ToolSession::new(&row.id, &row.title, "probe");
        tool.create_with_size("/tmp", "sleep 600", None, "default", &row.id, "probe")?;
        let panes = crate::tmux::batch_pane_metadata()?;
        let identity = crate::tmux::ToolSession::legacy_identity(panes.get(&name).unwrap())?;
        let proof = crate::process::CapturedProcessTree::capture(
            identity.pane_pid.expect("live tool fixture"),
        )?;
        let _interruption =
            crate::tmux::ToolSession::interrupt_retirement_after_prepare_for_test(&row.id);
        assert!(tool
            .kill_verified(&row.id, "probe", &identity, None)
            .unwrap_err()
            .to_string()
            .contains("after durable"));
        Ok((tool, guard, proof))
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn tool_stop_advances_generation_without_coarse_reservation_before_journal_lock(
    ) -> anyhow::Result<()> {
        if !crate::tmux::is_tmux_available() {
            return Ok(());
        }
        let _home = crate::session::test_support::isolate_app_dir();
        let root = crate::session::get_app_dir()?;
        crate::tmux::tool_retirement::initialize(&root)?;
        crate::session::config::update_config(|config| {
            config.tools.remove("probe");
        })?;
        let mut row = crate::session::Instance::new("initial stop boundary", "/tmp");
        row.source_profile = "default".into();
        row.auxiliary.push(crate::session::AuxiliaryObservation {
            target: crate::session::AuxiliaryTarget::Tool {
                tool_name: "probe".into(),
            },
            pane: Default::default(),
        });
        crate::server::test_support::seed_instances_on_disk_for_test("default", vec![row.clone()]);
        let tool = crate::tmux::ToolSession::new(&row.id, &row.title, "probe");
        let _target = crate::tmux::test_helpers::TmuxTestSession::from_name(tool.session_name());
        tool.create_with_size("/tmp", "sleep 600", None, "default", &row.id, "probe")?;
        let state = crate::server::test_support::build_test_app_state(vec![row.clone()]);
        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
        let prepared_state = state.clone();
        let prepared_native = tokio::task::spawn_blocking(move || {
            crate::server::session_store::NativeSessionStore::open(prepared_state, "default", None)
        })
        .await??;
        let storage = crate::session::Storage::open_unwatched("default")?;
        // A remembered target avoids the admission journal read. Holding the real
        // sidecar therefore pauses the actual handler after its generation commit,
        // before publication/effects and before any normal-return cleanup can run.
        let journal = crate::session::LockedDataFile::open(
            &root.join(crate::tmux::tool_retirement::FILE_NAME),
        )?;
        let worker_state = state.clone();
        let id = row.id.clone();
        let worker = tokio::spawn(async move {
            stop_native_auxiliary(
                &worker_state,
                &id,
                AuxiliaryStopRequest::Target(crate::daemon::StopAuxiliaryBody {
                    target: crate::session::AuxiliaryTarget::Tool {
                        tool_name: "probe".into(),
                    },
                    adoption: None,
                }),
            )
            .await
        });
        let committed = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let current = storage.load()?.remove(0);
                if current.lifecycle_generation > row.lifecycle_generation {
                    break anyhow::Ok(current);
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await??;
        assert_eq!(committed.lifecycle_generation, row.lifecycle_generation + 1);
        assert!(
            committed.lifecycle_reservation.is_none(),
            "a crash here must not leave fresh coarse Stop ownership ahead of journal recovery"
        );
        assert!(!worker.is_finished());
        assert!(storage.instance_lifecycle_lock_is_held_for_test(&row.id));
        assert!(state.instance_lock(&row.id).await.try_lock().is_err());
        assert!(state.profile_namespace.try_write().is_err());
        let recorded: serde_json::Value = serde_json::from_str(&journal.read()?.unwrap())?;
        assert_eq!(recorded["owners"], serde_json::json!([]));
        drop(journal);
        worker.await??;
        let committed = storage.load()?.remove(0);
        assert_eq!(committed.lifecycle_generation, row.lifecycle_generation + 1);
        assert!(committed.lifecycle_reservation.is_none());
        assert!(!crate::tmux::ToolSession::has_pending_retirement(
            &row.id, "probe"
        )?);
        let stale = publish_auxiliary_after_ensure(
            &state,
            row,
            crate::session::AuxiliaryTarget::Tool {
                tool_name: "probe".into(),
            },
            true,
            prepared_native,
        )
        .await
        .unwrap_err();
        assert_eq!(
            stale.downcast_ref::<crate::session::LifecycleReservationError>(),
            Some(&crate::session::LifecycleReservationError::Superseded)
        );
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[serial_test::serial]
    async fn native_tool_stop_recovers_prepared_and_proven_debt_without_coarse_owner_or_replacement_effect(
    ) -> anyhow::Result<()> {
        use crate::tmux::test_helpers::{pane_field, TmuxTestSession};
        struct CohortCleanup {
            root: Option<crate::process::CapturedProcessTree>,
            descendant: Option<crate::process::CapturedProcessTree>,
            pid: u32,
            birth: crate::process::ProcessBirthIdentity,
        }
        impl Drop for CohortCleanup {
            fn drop(&mut self) {
                if crate::process::process_birth_identity(self.pid)
                    .ok()
                    .flatten()
                    .as_ref()
                    == Some(&self.birth)
                {
                    if let Ok(tree) = crate::process::CapturedProcessTree::capture(self.pid) {
                        let _ = tree.settle(std::time::Duration::ZERO);
                    }
                }
                for tree in [self.root.take(), self.descendant.take()]
                    .into_iter()
                    .flatten()
                {
                    let _ = tree.settle(std::time::Duration::ZERO);
                }
            }
        }
        fn live_session_ids(sentinel: &str) -> anyhow::Result<std::collections::HashSet<String>> {
            let mut command = crate::tmux::tmux_query_command();
            command.args(["list-sessions", "-F", "#{session_id}"]);
            let output = crate::tmux::TmuxCommandDeadline::new().run(&mut command)?;
            anyhow::ensure!(
                output.status.success(),
                "independent session census failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let text = String::from_utf8(output.stdout)?;
            let mut ids = std::collections::HashSet::new();
            for id in text.lines() {
                anyhow::ensure!(
                    id.strip_prefix('$').is_some_and(|digits| !digits.is_empty()
                        && digits.bytes().all(|byte| byte.is_ascii_digit())),
                    "malformed session ID"
                );
                anyhow::ensure!(ids.insert(id.to_owned()), "duplicate session ID");
            }
            anyhow::ensure!(ids.contains(sentinel), "private sentinel is missing");
            Ok(ids)
        }
        if !crate::tmux::is_tmux_available() {
            return Ok(());
        }
        let _home = crate::session::test_support::isolate_app_dir();
        let root = crate::session::get_app_dir()?;
        crate::tmux::tool_retirement::initialize(&root)?;
        crate::session::config::update_config(|config| {
            config.tools.remove("probe");
        })?;
        let sentinel = TmuxTestSession::new("native_retirement_census");
        assert!(crate::tmux::tmux_command()
            .args(["new-session", "-d", "-s", sentinel.name(), "sleep 600"])
            .status()?
            .success());
        let sentinel_id = pane_field(sentinel.name(), "#{session_id}");
        for after_proof in [false, true] {
            let mut row = crate::session::Instance::new("native interrupted stop", "/tmp");
            row.source_profile = "default".into();
            row.auxiliary.push(crate::session::AuxiliaryObservation {
                target: crate::session::AuxiliaryTarget::Tool {
                    tool_name: "probe".into(),
                },
                pane: Default::default(),
            });
            crate::server::test_support::seed_instances_on_disk_for_test(
                "default",
                vec![row.clone()],
            );
            let fixture = tempfile::tempdir()?;
            let start = fixture.path().join("start.fifo");
            let wait = fixture.path().join("wait.fifo");
            let pid_file = fixture.path().join("child.pid");
            for fifo in [&start, &wait] {
                nix::unistd::mkfifo(
                    fifo,
                    nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
                )?;
            }
            let script = fixture.path().join("tool.sh");
            std::fs::write(&script, format!(
                "read -r line < '{}'\ntrap '' HUP TERM\nsetsid sh -c 'trap \"\" HUP TERM; echo $$ > \"$1\"; read -r line < \"$2\"' sh '{}' '{}' &\nwait\n",
                start.display(), pid_file.display(), wait.display(),
            ))?;
            let tool = crate::tmux::ToolSession::new(&row.id, &row.title, "probe");
            let _target = TmuxTestSession::from_name(tool.session_name());
            tool.create_with_size(
                fixture.path().to_str().unwrap(),
                &format!("sh '{}'", script.display()),
                None,
                "default",
                &row.id,
                "probe",
            )?;
            let original_id = pane_field(tool.session_name(), "#{session_id}");
            let root_pid = pane_field(tool.session_name(), "#{pane_pid}").parse()?;
            let mut cleanup = CohortCleanup {
                root: Some(crate::process::CapturedProcessTree::capture(root_pid)?),
                descendant: None,
                pid: root_pid,
                birth: crate::process::process_birth_identity(root_pid)?
                    .ok_or_else(|| anyhow::anyhow!("original root vanished before its barrier"))?,
            };
            // Freeze a root-only proof before allowing the resistant child to
            // exist. Removing the original root later must not settle that child.
            std::fs::write(&start, "start\n")?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let descendant = loop {
                if let Ok(pid) = std::fs::read_to_string(&pid_file)
                    .and_then(|text| text.trim().parse::<u32>().map_err(std::io::Error::other))
                {
                    break pid;
                }
                anyhow::ensure!(
                    std::time::Instant::now() < deadline,
                    "child never reached its barrier"
                );
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            };
            cleanup.descendant = Some(crate::process::CapturedProcessTree::capture(descendant)?);
            let state = crate::server::test_support::build_test_app_state(vec![row.clone()]);
            crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
            let prepared_interruption = (!after_proof).then(|| {
                crate::tmux::ToolSession::interrupt_retirement_after_prepare_for_test(&row.id)
            });
            let proven_interruption = after_proof.then(|| {
                crate::tmux::ToolSession::interrupt_retirement_before_ack_for_test(&row.id)
            });
            let error = stop_native_auxiliary(
                &state,
                &row.id,
                AuxiliaryStopRequest::Target(crate::daemon::StopAuxiliaryBody {
                    target: crate::session::AuxiliaryTarget::Tool {
                        tool_name: "probe".into(),
                    },
                    adoption: None,
                }),
            )
            .await
            .unwrap_err();
            assert!(format!("{error:#}").contains(if after_proof {
                "after original retirement proof before acknowledgement"
            } else {
                "after durable tool retirement preparation"
            }));
            drop((prepared_interruption, proven_interruption));
            let storage = crate::session::Storage::open_unwatched("default")?;
            let persisted = storage.load()?.remove(0);
            assert_eq!(persisted.lifecycle_generation, row.lifecycle_generation + 1);
            assert!(persisted.lifecycle_reservation.is_none());
            assert!(crate::tmux::ToolSession::has_pending_retirement(
                &row.id, "probe"
            )?);
            let child = cleanup.descendant.as_ref().unwrap();
            assert_eq!(child.all_exited()?, after_proof);
            if !after_proof {
                assert!(crate::tmux::tmux_command()
                    .args(["kill-session", "-t", tool.session_name()])
                    .status()?
                    .success());
                cleanup
                    .root
                    .take()
                    .unwrap()
                    .settle(std::time::Duration::ZERO)?;
                assert!(
                    !child.all_exited()?,
                    "original debt must retain the resistant child"
                );
            }
            assert!(!live_session_ids(&sentinel_id)?.contains(&original_id));
            assert!(crate::tmux::tmux_command()
                .args(["new-session", "-d", "-s", tool.session_name(), "sleep 600"])
                .status()?
                .success());
            let replacement_id = pane_field(tool.session_name(), "#{session_id}");
            let replacement = crate::process::CapturedProcessTree::capture(
                pane_field(tool.session_name(), "#{pane_pid}").parse()?,
            )?;
            drop(state);
            // Reconstruct the real native admission state from persisted rows,
            // with no remembered target or usable tool configuration.
            let restarted = crate::server::test_support::build_test_app_state(storage.load()?);
            crate::server::test_support::refresh_canonical_metadata_for_test(&restarted).await;
            assert!(restarted.instances.read().await[0].auxiliary.is_empty());
            stop_native_auxiliary(
                &restarted,
                &row.id,
                AuxiliaryStopRequest::Target(crate::daemon::StopAuxiliaryBody {
                    target: crate::session::AuxiliaryTarget::Tool {
                        tool_name: "probe".into(),
                    },
                    adoption: None,
                }),
            )
            .await
            .unwrap_err();
            assert!(child.all_exited()?);
            assert!(!replacement.all_exited()?);
            let ids = live_session_ids(&sentinel_id)?;
            assert!(!ids.contains(&original_id));
            assert!(ids.contains(&replacement_id));
            assert_eq!(pane_field(tool.session_name(), "#{@aoe_tool_owner}"), "");
            assert!(!crate::tmux::ToolSession::has_pending_retirement(
                &row.id, "probe"
            )?);
            let persisted = storage.load()?.remove(0);
            assert_eq!(persisted.lifecycle_generation, row.lifecycle_generation + 2);
            assert!(persisted.lifecycle_reservation.is_none());
        }
        Ok(())
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn stop_recovers_absent_unconfigured_debt_without_stopping_replacement(
    ) -> anyhow::Result<()> {
        if !crate::tmux::is_tmux_available() {
            return Ok(());
        }
        let _home = crate::session::test_support::isolate_app_dir();
        crate::tmux::tool_retirement::initialize(&crate::session::get_app_dir()?)?;
        crate::session::config::update_config(|config| {
            config.tools.remove("probe");
        })?;
        for (replacement, legacy_replacement) in [(false, false), (true, false), (true, true)] {
            let mut row = crate::session::Instance::new("restart debt", "/tmp");
            row.source_profile = "default".into();
            crate::server::test_support::seed_instances_on_disk_for_test(
                "default",
                vec![row.clone()],
            );
            let (tool, _guard, original) = prepare_tool_retirement(&row)?;
            assert!(crate::tmux::tmux_command()
                .args(["kill-session", "-t", tool.session_name()])
                .status()?
                .success());
            let replacement_proof = if replacement {
                if legacy_replacement {
                    assert!(crate::tmux::tmux_command()
                        .args(["new-session", "-d", "-s", tool.session_name(), "sleep 600",])
                        .status()?
                        .success());
                } else {
                    tool.create_with_size("/tmp", "sleep 600", None, "default", &row.id, "probe")?;
                }
                let panes = crate::tmux::batch_pane_metadata()?;
                Some(crate::process::CapturedProcessTree::capture(
                    panes.get(tool.session_name()).unwrap().pane_pid.unwrap(),
                )?)
            } else {
                None
            };
            let state = crate::server::test_support::build_test_app_state(vec![row.clone()]);
            crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
            let pending = initial_tool_retirement_inventory(&state).await;
            seed_initial_observations(&state, pending.as_deref()).await?;
            assert!(state.instances.read().await[0].auxiliary.iter().any(|observation| {
                matches!(&observation.target, crate::session::AuxiliaryTarget::Tool { tool_name } if tool_name == "probe")
                    && (replacement || observation.pane.state == crate::session::PanePresence::Absent)
            }));
            // Neither remembered observations nor current configuration admits this
            // request. Only the validated original debt makes explicit Stop reachable.
            state.instances.write().await[0].auxiliary.clear();
            let result = stop_native_auxiliary(
                &state,
                &row.id,
                AuxiliaryStopRequest::Target(crate::daemon::StopAuxiliaryBody {
                    target: crate::session::AuxiliaryTarget::Tool {
                        tool_name: "probe".into(),
                    },
                    adoption: None,
                }),
            )
            .await;
            assert_eq!(result.is_err(), replacement);
            assert!(original.all_exited()?);
            assert!(!crate::tmux::ToolSession::has_pending_retirement(
                &row.id, "probe"
            )?);
            if let Some(proof) = replacement_proof {
                assert!(
                    !proof.all_exited()?,
                    "original acknowledgement must not authorize replacement cleanup"
                );
                if legacy_replacement {
                    assert_eq!(
                        crate::tmux::test_helpers::pane_field(
                            tool.session_name(),
                            "#{@aoe_tool_owner}"
                        ),
                        ""
                    );
                }
            }
        }
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn whole_row_cleanup_drains_original_debt_before_tmux_absence() -> anyhow::Result<()> {
        if !crate::tmux::is_tmux_available() {
            return Ok(());
        }
        let _home = crate::session::test_support::isolate_app_dir();
        crate::tmux::tool_retirement::initialize(&crate::session::get_app_dir()?)?;
        let row = crate::session::Instance::new("parent cleanup debt", "/tmp");
        let (tool, _guard, original) = prepare_tool_retirement(&row)?;
        assert!(crate::tmux::tmux_command()
            .args(["kill-session", "-t", tool.session_name()])
            .status()?
            .success());
        crate::tmux::utils::kill_ancillary_sessions_for_id(&row.id)?;
        assert!(original.all_exited()?);
        assert!(!crate::tmux::ToolSession::has_pending_retirement(
            &row.id, "probe"
        )?);
        Ok(())
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn obsolete_boot_recovery_consumes_original_without_adopting_legacy_replacement(
    ) -> anyhow::Result<()> {
        if !crate::tmux::is_tmux_available() {
            return Ok(());
        }
        let _home = crate::session::test_support::isolate_app_dir();
        let root = crate::session::get_app_dir()?;
        crate::tmux::tool_retirement::initialize(&root)?;
        crate::session::config::update_config(|config| {
            config.tools.remove("probe");
        })?;
        let mut row = crate::session::Instance::new("obsolete debt replacement", "/tmp");
        row.source_profile = "default".into();
        crate::server::test_support::seed_instances_on_disk_for_test("default", vec![row.clone()]);
        let name = crate::tmux::ToolSession::generate_name(&row.id, &row.title, "probe");
        let _guard = crate::tmux::test_helpers::TmuxTestSession::from_name(&name);
        assert!(crate::tmux::tmux_command()
            .args(["new-session", "-d", "-s", &name, "sleep 600",])
            .status()?
            .success());
        let panes = crate::tmux::batch_pane_metadata()?;
        let replacement = crate::process::CapturedProcessTree::capture(
            panes.get(&name).unwrap().pane_pid.unwrap(),
        )?;
        let boot = format!("obsolete-{}", crate::process::boot_id().unwrap());
        let pid = i32::MAX as u32;
        // Handcraft authorized old-boot state whose recorded server cannot be
        // inspected. Today's unmarked replacement is not part of this cohort.
        std::fs::write(
            root.join(crate::tmux::tool_retirement::FILE_NAME),
            serde_json::to_vec(&serde_json::json!({
                "version": 1,
                "owners": [{
                    "token": uuid::Uuid::new_v4(),
                    "instance_id": row.id, "tool_name": "probe",
                    "socket": "/unreachable/original-tmux.sock", "server_pid": pid,
                    "server_birth": {"boot_id":boot, "start_id":"uninspectable-server"},
                    "receipt": {
                        "name": name,
                        "identity": {"session_id":"$999999", "pane_id":"%999999", "pane_pid":pid},
                        "owner": serde_json::to_string(&(&row.id, "probe"))?,
                        "tree": {"boot_id":boot, "processes":[{"pid":pid, "start_id":"uninspectable-original"}]},
                    },
                }],
            }))?,
        )?;
        let state = crate::server::test_support::build_test_app_state(vec![row.clone()]);
        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
        let pending = initial_tool_retirement_inventory(&state).await;
        seed_initial_observations(&state, pending.as_deref()).await?;
        let adoption = {
            let rows = state.instances.read().await;
            let pane = &rows[0].auxiliary.iter().find(|observation| {
                matches!(&observation.target, crate::session::AuxiliaryTarget::Tool { tool_name } if tool_name == "probe")
            }).unwrap().pane;
            crate::session::LegacyToolAdoption {
                tmux_session: pane.tmux_session.clone().unwrap(),
                identity: pane
                    .legacy_tool
                    .as_ref()
                    .expect("replacement remains confirmable")
                    .identity
                    .clone(),
                server: pane
                    .legacy_tool
                    .as_ref()
                    .expect("replacement remains confirmable")
                    .server
                    .clone(),
                profile: row.source_profile.clone(),
                lifecycle_generation: row.lifecycle_generation,
            }
        };
        let result = stop_native_auxiliary(
            &state,
            &row.id,
            AuxiliaryStopRequest::Target(crate::daemon::StopAuxiliaryBody {
                target: crate::session::AuxiliaryTarget::Tool {
                    tool_name: "probe".into(),
                },
                adoption: Some(adoption),
            }),
        )
        .await;
        assert!(
            result.is_err(),
            "physical replacement still prevents a successful Stop acknowledgement"
        );
        assert!(!crate::tmux::ToolSession::has_pending_retirement(
            &row.id, "probe"
        )?);
        assert!(!replacement.all_exited()?);
        assert_eq!(
            crate::tmux::test_helpers::pane_field(&name, "#{@aoe_tool_owner}"),
            ""
        );
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn ensure_refuses_changed_original_then_drains_debt_before_new_launch() -> anyhow::Result<()> {
        if !crate::tmux::is_tmux_available() {
            return Ok(());
        }
        let _home = crate::session::test_support::isolate_app_dir();
        crate::tmux::tool_retirement::initialize(&crate::session::get_app_dir()?)?;
        crate::session::config::update_config(|config| {
            config.tools.insert(
                "probe".into(),
                crate::session::config::ToolSessionConfig {
                    command: "sleep 600".into(),
                    ..Default::default()
                },
            );
        })?;
        let mut row = crate::session::Instance::new("launch debt", "/tmp");
        row.source_profile = "default".into();
        crate::server::test_support::seed_instances_on_disk_for_test("default", vec![row.clone()]);
        let store = crate::session::Storage::open_unwatched("default")?;
        let (tool, _guard, original) = prepare_tool_retirement(&row)?;
        let foreign = serde_json::to_string(&("foreign", "probe"))?;
        assert!(crate::tmux::tmux_command()
            .args([
                "set-option",
                "-t",
                tool.session_name(),
                "@aoe_tool_owner",
                &foreign,
            ])
            .status()?
            .success());
        assert!(row
            .start_tool_with_size_in("probe", None, &store, None, None)
            .is_err());
        assert!(!original.all_exited()?);
        assert!(crate::tmux::ToolSession::has_pending_retirement(
            &row.id, "probe"
        )?);
        assert!(crate::tmux::tmux_command()
            .args(["kill-session", "-t", tool.session_name()])
            .status()?
            .success());
        let (_, created) = row.start_tool_with_size_in("probe", None, &store, None, None)?;
        assert!(created);
        assert!(original.all_exited()?);
        assert!(!crate::tmux::ToolSession::has_pending_retirement(
            &row.id, "probe"
        )?);
        Ok(())
    }

    #[test]
    fn legacy_observation_is_confirmable_but_never_ready_until_owned() {
        use crate::session::{AuxiliaryTarget, PanePresence};
        use crate::tmux::{LegacyToolScope, PaneMetadata, ToolPaneOwner, ToolSession};
        let id = "abc12345deadbeef";
        let target = AuxiliaryTarget::Tool {
            tool_name: "yazi".into(),
        };
        let name = ToolSession::generate_name(id, "Before rename", "yazi");
        let scope = LegacyToolScope {
            rows: vec![(
                "work".into(),
                id.into(),
                crate::tmux::Session::generate_name(id, "After rename"),
            )],
            tools: vec![("yazi".into(), ToolSession::name_prefix("yazi"))],
        };
        let metadata = PaneMetadata {
            server: Some(std::sync::Arc::new(crate::session::TmuxServerIdentity {
                socket: "/fixture-tmux.sock".into(),
                pid: 4242,
                birth: crate::process::ProcessBirthIdentity {
                    boot_id: "fixture-boot".into(),
                    start_id: "fixture-server".into(),
                },
            })),
            session_id: "$42".into(),
            pane_id: "%42".into(),
            session_kind: None,
            tool_owner: ToolPaneOwner::Unmarked,
            pane_dead: false,
            pane_current_command: Some("sleep".into()),
            pane_start_command_is_protected: false,
            pane_pid: Some(4242),
            pane_title: None,
            window_activity: None,
            window_size: Some((80, 24)),
        };
        let mut panes = std::collections::HashMap::from([(name.clone(), metadata)]);
        let legacy = observe_auxiliary(
            id,
            "After rename",
            &target,
            Some(&panes),
            "work",
            Some(&scope),
        );
        assert_eq!(legacy.state, PanePresence::Unknown);
        assert_eq!(legacy.tmux_session.as_deref(), Some(name.as_str()));
        let identity = legacy.legacy_tool.unwrap().identity;
        assert_eq!(identity.session_id, "$42");
        assert_eq!(identity.pane_id, "%42");
        assert_eq!(identity.pane_pid, Some(4242));
        panes.get_mut(&name).unwrap().tool_owner = ToolPaneOwner::Invalid;
        let invalid = observe_auxiliary(
            id,
            "After rename",
            &target,
            Some(&panes),
            "work",
            Some(&scope),
        );
        assert_eq!(invalid.state, PanePresence::Unknown);
        assert!(invalid.legacy_tool.is_none());
        let metadata = panes.get_mut(&name).unwrap();
        metadata.tool_owner = ToolPaneOwner::Named {
            instance_id: id.into(),
            tool_name: "yazi".into(),
        };
        metadata.session_kind = Some("tool".into());
        let owned = observe_auxiliary(
            id,
            "After rename",
            &target,
            Some(&panes),
            "work",
            Some(&scope),
        );
        assert_eq!(owned.state, PanePresence::Alive);
        assert!(owned.legacy_tool.is_none());
        let missing = observe_auxiliary(id, "After rename", &target, None, "work", Some(&scope));
        assert_eq!(missing.state, PanePresence::Unknown);
        assert!(missing.legacy_tool.is_none());
    }

    #[test]
    fn parse_pane_dead_empty_is_not_ready() {
        assert_eq!(parse_pane_dead_output(""), PaneReadiness::NotReady);
        assert_eq!(parse_pane_dead_output("   \n  \n"), PaneReadiness::NotReady);
    }

    #[test]
    fn parse_pane_dead_single_alive_is_ready() {
        assert_eq!(parse_pane_dead_output("0\n"), PaneReadiness::Ready);
    }

    #[test]
    fn parse_pane_dead_single_dead_is_dead() {
        assert_eq!(parse_pane_dead_output("1\n"), PaneReadiness::Dead);
    }

    #[test]
    fn parse_pane_dead_mixed_is_ready() {
        assert_eq!(parse_pane_dead_output("1\n0\n1\n"), PaneReadiness::Ready);
    }

    #[test]
    fn parse_pane_dead_all_dead_is_dead() {
        assert_eq!(parse_pane_dead_output("1\n1\n"), PaneReadiness::Dead);
    }
}
