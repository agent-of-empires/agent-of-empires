//! Shared live-terminal readiness, rescue and WebSocket close helpers.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::{CloseFrame, Message, WebSocket};

use super::AppState;

pub(super) fn observe_auxiliary(
    id: &str,
    title: &str,
    target: &crate::session::AuxiliaryTarget,
    panes: Option<&std::collections::HashMap<String, crate::tmux::PaneMetadata>>,
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
            crate::tmux::ToolSession::metadata_in(id, title, tool_name, panes)
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
            state: if metadata.pane_dead {
                PanePresence::Dead
            } else {
                PanePresence::Alive
            },
            tmux_session: Some(name.into_owned()),
        },
        Ok(None) => PaneObservation {
            state: PanePresence::Absent,
            tmux_session: None,
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

pub(super) fn sample_panes(
    instance: &mut crate::session::Instance,
    tools: &[String],
    panes: Option<&std::collections::HashMap<String, crate::tmux::PaneMetadata>>,
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
    for name in tools {
        if !instance.auxiliary.iter().any(|observation| matches!(&observation.target, AuxiliaryTarget::Tool { tool_name } if tool_name == name)) {
            instance.auxiliary.push(AuxiliaryObservation {
                target: AuxiliaryTarget::Tool { tool_name: name.clone() },
                pane: Default::default(),
            });
        }
    }
    for observation in &mut instance.auxiliary {
        observation.pane =
            observe_auxiliary(&instance.id, &instance.title, &observation.target, panes);
    }
    instance.auxiliary.retain(|observation| {
        observation.pane.state != PanePresence::Absent
            || match &observation.target {
                AuxiliaryTarget::Host { index: 0 } => true,
                AuxiliaryTarget::Container { index: 0 } => sandboxed,
                AuxiliaryTarget::Tool { tool_name } => tools.contains(tool_name),
                _ => false,
            }
    });
}

#[derive(Debug, thiserror::Error)]
#[error("Auxiliary target is unavailable")]
pub(super) struct AuxiliaryTargetUnavailable;

// Retain these locks through any launch-owned state handoff.
fn sample_auxiliary_after_ensure(
    native: &super::session_store::NativeSessionStore,
    instance: &mut crate::session::Instance,
    target: crate::session::AuxiliaryTarget,
) -> anyhow::Result<(crate::session::StorageFlock, crate::session::StorageFlock)> {
    let generation = instance.lifecycle_generation;
    let title = instance.title.clone();
    let ownership = instance.acquire_auxiliary_locks_in(native)?;
    anyhow::ensure!(
        instance.lifecycle_generation == generation && instance.title == title,
        crate::session::LifecycleReservationError::Superseded
    );
    let panes = crate::tmux::batch_pane_metadata();
    let pane = observe_auxiliary(&instance.id, &instance.title, &target, panes.as_ref().ok());
    let state = pane.state;
    native.adopt_auxiliary_observations(
        instance,
        std::iter::once(crate::session::AuxiliaryObservation { target, pane }),
    )?;
    panes?;
    anyhow::ensure!(
        state == crate::session::PanePresence::Alive,
        AuxiliaryTargetUnavailable
    );
    Ok(ownership)
}

pub(super) async fn publish_auxiliary_after_ensure(
    state: &Arc<AppState>,
    mut instance: crate::session::Instance,
    target: crate::session::AuxiliaryTarget,
) -> anyhow::Result<crate::daemon::RuntimeCursor> {
    let namespace = state.profile_namespace.read().await;
    let lock = state.instance_lock(&instance.id).await;
    let guard = lock.lock().await;
    let id = instance.id.clone();
    let worker_state = state.clone();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let native = super::session_store::NativeSessionStore::open(
            worker_state,
            &instance.source_profile,
            None,
        )?;
        let _ownership = sample_auxiliary_after_ensure(&native, &mut instance, target)?;
        Ok(())
    })
    .await?;
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
        crate::session::LifecycleReservationError::Superseded
    );
    Ok(snapshot.value.cursor.clone())
}

pub(super) enum AuxiliaryStopRequest {
    Target(crate::session::AuxiliaryTarget),
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
    let targets = match request {
        AuxiliaryStopRequest::Target(target) => [Some(target), None],
        AuxiliaryStopRequest::PairedTerminals { index } => {
            anyhow::ensure!(index > 0, AuxiliaryTargetUnavailable);
            [
                Some(AuxiliaryTarget::Host { index }),
                Some(AuxiliaryTarget::Container { index }),
            ]
        }
    };
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
        let config = targets
            .iter()
            .flatten()
            .any(|target| matches!(target, AuxiliaryTarget::Tool { .. }))
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
        instance.acquire_lifecycle_reservation(&native, LifecycleOperation::Stop, None)?;
        let effect = (|| -> anyhow::Result<()> {
            let panes = crate::tmux::batch_pane_metadata()?;
            native.check_available()?;
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
                    AuxiliaryTarget::Tool { tool_name } => crate::tmux::ToolSession::from_snapshot(
                        &instance.id,
                        &instance.title,
                        tool_name,
                        &panes,
                    )
                    .map_err(|error| error.context(AuxiliaryTargetUnavailable))?
                    .kill(),
                }?;
            }
            Ok(())
        })();
        let panes = crate::tmux::batch_pane_metadata();
        instance.release_lifecycle_reservation(&native, LifecycleOperation::Stop)?;
        let mut all_absent = true;
        native.adopt_auxiliary_observations(
            &instance,
            targets.into_iter().flatten().map(|target| {
                let pane =
                    observe_auxiliary(&instance.id, &instance.title, &target, panes.as_ref().ok());
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
        let (terminal, _) = instance.start_terminal_with_size_indexed_in(index, None, &native)?;
        let _ownership = sample_auxiliary_after_ensure(
            &native,
            &mut instance,
            crate::session::AuxiliaryTarget::Host { index },
        )?;
        Ok(terminal.name().to_owned())
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

    let (target, _) = ensure_native_container_terminal(state, id, index, None).await?;
    Ok(target.tmux_session)
}

pub(crate) async fn ensure_native_container_terminal(
    state: &Arc<AppState>,
    id: &str,
    index: u32,
    size: Option<(u16, u16)>,
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
        let native = super::session_store::NativeSessionStore::open(
            worker_state.clone(),
            &instance.source_profile,
            None,
        )?;
        let prior_identity_publisher = instance.identity_publisher_launched;
        let mut minted_env = false;
        let (terminal, created) = instance.start_container_terminal_with_hook_in(
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
        let _ownership = sample_auxiliary_after_ensure(
            &native,
            &mut instance,
            crate::session::AuxiliaryTarget::Container { index },
        )?;
        let target = crate::daemon::TerminalTarget {
            tmux_session: terminal.name().to_owned(),
            status: if created {
                crate::daemon::TerminalTargetStatus::Created
            } else {
                crate::daemon::TerminalTargetStatus::Exists
            },
        };
        let publication = handle.block_on(worker_state.publication.write());
        let mut rows = handle.block_on(worker_state.instances.write());
        let row = rows
            .iter_mut()
            .find(|row| row.id == worker_id)
            .ok_or(crate::session::LifecycleReservationError::Superseded)?;
        anyhow::ensure!(
            row.lifecycle_generation == instance.lifecycle_generation
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
