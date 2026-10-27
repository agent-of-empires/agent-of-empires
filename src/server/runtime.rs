//! Native runtime identity, immutable snapshots, and stream delivery.

use std::{future::Future, sync::Arc, time::Duration};

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::{IntoResponse, Response},
    Extension, Json,
};
use tokio::{
    sync::{oneshot, watch, Mutex, Notify},
    task::JoinSet,
};
use tracing::Instrument;

use super::{principal::Principal, AppState};
use crate::daemon::{
    CreationProgress, RuntimeCapabilities, RuntimeContents, RuntimeCursor, RuntimeFrame,
    RuntimeHealth, RuntimeInfo, RuntimeSnapshot, RUNTIME_PROTOCOL_VERSION,
};

pub(crate) struct NativeRuntime {
    epoch: String,
    namespace: String,
    tmux_socket: Option<String>,
    snapshots: watch::Sender<Option<Arc<PublishedSnapshot>>>,
    /// Latest advisory creation progress. Only local-owner streams are sent
    /// it, so it stays a `watch` full-state value rather than a log.
    creation_progress: watch::Sender<Arc<Vec<CreationProgress>>>,
    owner_diagnostics: watch::Sender<Arc<Vec<crate::daemon::OwnerLoadDiagnostic>>>,
    purge_namespace: watch::Sender<std::sync::Weak<tokio::sync::OwnedRwLockReadGuard<()>>>,
    publish_lane: Mutex<()>,
    wake: Notify,
    terminal_wake: Notify,
    hooks: crate::status_hooks::StatusHooks,
    pub(crate) work: Arc<RuntimeWork>,
}

pub(crate) struct PublishedSnapshot {
    pub value: RuntimeSnapshot,
    frame: Message,
    mutation_epoch: u64,
}

#[derive(Default)]
pub(crate) struct RuntimeWork {
    tasks: std::sync::Mutex<JoinSet<()>>,
    pub(crate) shutdown: tokio_util::sync::CancellationToken,
}

#[derive(Debug)]
pub(crate) enum RuntimeWorkError {
    ShuttingDown,
    Interrupted,
}

impl RuntimeWork {
    fn enqueue_work<F>(work: &mut JoinSet<()>, name: &'static str, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        while let Some(result) = work.try_join_next() {
            if let Err(error) = result {
                tracing::error!(target: "task.panic", %error, "runtime work failed");
            }
        }
        work.spawn(crate::task_util::supervise(
            name,
            crate::task_util::PanicPolicy::Log,
            future.instrument(tracing::Span::current()),
        ));
    }

    pub(crate) fn spawn<F>(&self, name: &'static str, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        Self::enqueue_work(
            &mut self.tasks.lock().expect("runtime work registry poisoned"),
            name,
            future,
        );
    }

    pub(crate) async fn run<F, R>(
        &self,
        name: &'static str,
        future: F,
    ) -> Result<R, RuntimeWorkError>
    where
        F: Future<Output = R> + Send + 'static,
        R: Send + 'static,
    {
        let response = {
            let mut work = self.tasks.lock().expect("runtime work registry poisoned");
            if self.shutdown.is_cancelled() {
                return Err(RuntimeWorkError::ShuttingDown);
            }
            let (sender, receiver) = oneshot::channel();
            Self::enqueue_work(&mut work, name, async move {
                let result = future.await;
                let _ = sender.send(result);
            });
            receiver
        };
        response.await.map_err(|_| RuntimeWorkError::Interrupted)
    }

    // Stop admissions first. Descendants may enqueue while their parents drain.
    pub(crate) async fn drain(&self) {
        loop {
            let mut pending = {
                let mut work = self.tasks.lock().expect("runtime work registry poisoned");
                if work.is_empty() {
                    return;
                }
                std::mem::take(&mut *work)
            };
            while let Some(result) = pending.join_next().await {
                if let Err(error) = result {
                    tracing::error!(target: "task.panic", %error, "runtime work failed");
                }
            }
        }
    }
}

impl NativeRuntime {
    pub(crate) fn new(
        namespace: String,
        tmux_socket: Option<String>,
        work: Arc<RuntimeWork>,
    ) -> Self {
        Self {
            epoch: uuid::Uuid::new_v4().to_string(),
            namespace,
            tmux_socket,
            snapshots: watch::channel(None).0,
            creation_progress: watch::channel(Arc::new(Vec::new())).0,
            owner_diagnostics: watch::channel(Arc::new(Vec::new())).0,
            purge_namespace: watch::channel(std::sync::Weak::new()).0,
            publish_lane: Mutex::new(()),
            wake: Notify::new(),
            terminal_wake: Notify::new(),
            hooks: Default::default(),
            work,
        }
    }

    pub(crate) async fn purge_namespace_lease(
        &self,
        namespace: &Arc<tokio::sync::RwLock<()>>,
    ) -> Arc<tokio::sync::OwnedRwLockReadGuard<()>> {
        let mut fresh = Some(namespace.clone().read_owned().await);
        let mut lease = None;
        self.purge_namespace.send_if_modified(|current| {
            if let Some(active) = current.upgrade() {
                lease = Some(active);
                false
            } else {
                let active = Arc::new(fresh.take().expect("fresh namespace guard"));
                *current = Arc::downgrade(&active);
                lease = Some(active);
                true
            }
        });
        lease.expect("namespace lease initialized")
    }

    pub(crate) fn active_purge_namespace_lease(
        &self,
    ) -> Option<Arc<tokio::sync::OwnedRwLockReadGuard<()>>> {
        self.purge_namespace.borrow().upgrade()
    }

    // A fresh read can wait behind a writer blocked by the purge being abandoned.
    pub(crate) async fn abandon_namespace_lease(
        &self,
        namespace: &Arc<tokio::sync::RwLock<()>>,
    ) -> Arc<tokio::sync::OwnedRwLockReadGuard<()>> {
        let mut leases = self.purge_namespace.subscribe();
        let acquire = self.purge_namespace_lease(namespace);
        tokio::pin!(acquire);
        loop {
            if let Some(active) = leases.borrow_and_update().upgrade() {
                return active;
            }
            tokio::select! {
                acquired = &mut acquire => return acquired,
                _ = leases.changed() => {}
            }
        }
    }

    fn capabilities(&self, state: &AppState, health: &RuntimeHealth) -> RuntimeCapabilities {
        let mutations = !state.read_only && *health == RuntimeHealth::Healthy;
        RuntimeCapabilities {
            mutations,
            native_interaction: mutations && !state.cityhall_mode && self.tmux_socket.is_some(),
        }
    }

    // Call only after releasing commit guards. Capture never acquires storage or identity locks.
    pub(crate) async fn publish(
        &self,
        state: &Arc<AppState>,
    ) -> anyhow::Result<Arc<PublishedSnapshot>> {
        let _lane = self.publish_lane.lock().await;
        let _publication = state.publication.read().await;
        let mutation_epoch = state
            .mutation_epoch
            .load(std::sync::atomic::Ordering::SeqCst);
        let health = state.canonical_health.read().await.clone();
        let sessions = super::api::sessions::project_sessions(state).await;
        let metadata = state.canonical_metadata.read().await;
        let workspace_ordering =
            super::api::sessions::compute_merged_ordering(&sessions, &metadata.workspace_ordering);
        let contents = RuntimeContents {
            capabilities: self.capabilities(state, &health),
            health,
            default_profile: metadata.default_profile.clone(),
            sessions,
            profiles: metadata.profiles.clone(),
            workspace_ordering,
            global_projects: metadata.global_projects.clone(),
        };
        if contents.health == RuntimeHealth::Healthy {
            self.owner_diagnostics.send_if_modified(|current| {
                if current.is_empty() {
                    return false;
                }
                *current = Arc::new(Vec::new());
                true
            });
        } else {
            let diagnostics = state
                .canonical_diagnostics
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.owner_diagnostics.send_if_modified(|current| {
                if current.as_ref() == &*diagnostics {
                    return false;
                }
                *current = Arc::new(diagnostics.clone());
                true
            });
        }
        let previous = self.snapshots.borrow().clone();
        if let Some(previous) = &previous {
            if previous.mutation_epoch == mutation_epoch && previous.value.contents == contents {
                self.hooks
                    .reconcile(&contents.sessions, &metadata.status_hooks);
                return Ok(previous.clone());
            }
        }
        let revision = previous
            .as_ref()
            .map_or(Some(1), |previous| {
                previous.value.cursor.revision.checked_add(1)
            })
            .ok_or_else(|| anyhow::anyhow!("runtime revision exhausted"))?;
        let value = RuntimeSnapshot {
            cursor: RuntimeCursor {
                epoch: self.epoch.clone(),
                revision,
            },
            contents,
        };
        let frame = Message::Text(serde_json::to_string(&RuntimeFrame::Snapshot(&value))?.into());
        let published = Arc::new(PublishedSnapshot {
            value,
            frame,
            mutation_epoch,
        });
        self.snapshots.send_replace(Some(published.clone()));
        self.hooks
            .reconcile(&published.value.contents.sessions, &metadata.status_hooks);
        let mut hooks = Vec::new();
        if let Some(previous) =
            previous.filter(|_| metadata.status_hooks.values().any(|config| config.enabled))
        {
            let old_statuses: std::collections::HashMap<_, _> = previous
                .value
                .contents
                .sessions
                .iter()
                .map(|row| (row.id.as_str(), row.status.as_str()))
                .collect();
            let changed_at = chrono::Utc::now();
            for row in &published.value.contents.sessions {
                let Some(config) = metadata
                    .status_hooks
                    .get(&row.profile)
                    .filter(|config| config.enabled)
                else {
                    continue;
                };
                let Some(old) = old_statuses
                    .get(row.id.as_str())
                    .and_then(|status| crate::session::Status::from_api_str(status))
                else {
                    continue;
                };
                let Some(new) =
                    crate::session::Status::from_api_str(&row.status).filter(|new| *new != old)
                else {
                    continue;
                };
                let context = crate::status_hooks::StatusHookContext {
                    session_id: row.id.clone(),
                    session_title: row.title.clone(),
                    project_path: row.project_path.clone(),
                    profile: row.profile.clone(),
                    tool: row.tool.clone(),
                    group_path: row.group_path.clone(),
                    old_status: old,
                    new_status: new,
                    changed_at,
                };
                if let Some(hook) =
                    self.hooks
                        .prepare_transition(context, config, self.work.shutdown.clone())
                {
                    hooks.push(hook);
                }
            }
        }
        drop(metadata);
        drop(_publication);
        drop(_lane);
        for hook in hooks {
            self.work.spawn("status.hook", hook);
        }
        Ok(published)
    }

    pub(crate) async fn snapshot(
        &self,
        state: &Arc<AppState>,
    ) -> anyhow::Result<Arc<PublishedSnapshot>> {
        let snapshot = self.snapshots.borrow().clone();
        match snapshot {
            Some(snapshot) => Ok(snapshot),
            None => self.publish(state).await,
        }
    }

    pub(crate) fn has_subscribers(&self) -> bool {
        self.snapshots.receiver_count() != 0
    }

    /// Replace the advisory creation progress. Never fails and never blocks:
    /// the owning creation is mid-flight and the value is display-only.
    pub(crate) fn publish_creation_progress(&self, progress: Vec<CreationProgress>) {
        self.creation_progress.send_replace(Arc::new(progress));
    }

    pub(crate) fn request_publish(&self) {
        self.wake.notify_one();
    }

    pub(super) async fn wait_for_terminal_subscriber(&self) {
        self.terminal_wake.notified().await;
    }

    fn info(
        &self,
        state: &AppState,
        local: Option<&Principal>,
        snapshot: &RuntimeSnapshot,
    ) -> RuntimeInfo {
        let profiles = snapshot
            .contents
            .profiles
            .iter()
            .map(|profile| profile.name.clone())
            .collect();
        let local_owner = matches!(local, Some(Principal::LocalPeer { .. }));
        let mut interaction_capabilities = snapshot.contents.capabilities.clone();
        interaction_capabilities.native_interaction &= local_owner;
        RuntimeInfo {
            protocol_version: RUNTIME_PROTOCOL_VERSION,
            epoch: self.epoch.clone(),
            namespace: self.namespace.clone(),
            tmux_socket: self.tmux_socket.clone(),
            local_owner,
            profiles,
            read_only: state.read_only,
            cityhall_mode: state.cityhall_mode,
            health: snapshot.contents.health.clone(),
            interaction_capabilities,
            owner_diagnostics: if local_owner && snapshot.contents.health != RuntimeHealth::Healthy
            {
                state
                    .canonical_diagnostics
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone()
            } else {
                Vec::new()
            },
            owner_instance_id: if local_owner {
                std::env::var("AOE_SERVE_INSTANCE_ID").ok()
            } else {
                None
            },
        }
    }
}

pub(super) async fn mutation_scope(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if request.method().is_safe() {
        return next.run(request).await;
    }
    match state
        .runtime
        .work
        .run("server.mutation", next.run(request))
        .await
    {
        Ok(response) => response,
        Err(RuntimeWorkError::ShuttingDown) => {
            axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
        Err(RuntimeWorkError::Interrupted) => {
            axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// A native request cannot cross daemon lifetimes.
pub(super) async fn epoch_gate(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let mut epochs = request
        .headers()
        .get_all(crate::daemon::RUNTIME_EPOCH_HEADER)
        .iter();
    if let Some(epoch) = epochs.next() {
        if epochs.next().is_some() || epoch.as_bytes() != state.runtime.epoch.as_bytes() {
            let code = crate::daemon::ApiErrorCode::RuntimeEpochMismatch;
            return (
                code.status(),
                code.header(),
                Json(serde_json::json!({
                    "error": code.as_str(),
                    "message": "Reconnect before sending changes to this daemon.",
                })),
            )
                .into_response();
        }
    }
    next.run(request).await
}

/// Flush only after releasing namespace, identity and publication guards.
pub(crate) async fn session_mutation_response<T: serde::Serialize>(
    state: &Arc<AppState>,
    id: &str,
    outcome: Option<T>,
) -> Response {
    #[derive(serde::Serialize)]
    struct Body<'a, T> {
        #[serde(flatten)]
        session: &'a crate::daemon::SessionResponse,
        #[serde(skip_serializing_if = "Option::is_none")]
        outcome: Option<T>,
    }
    let snapshot = match state.runtime.publish(state).await {
        Ok(snapshot) => snapshot,
        Err(_) => return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let Some(row) = snapshot
        .value
        .contents
        .sessions
        .iter()
        .find(|row| row.id == id)
    else {
        return super::api::session_gone_after_persist();
    };
    mutation_response(
        &snapshot.value.cursor,
        Json(Body {
            session: row,
            outcome,
        }),
    )
}

pub(crate) fn mutation_response(cursor: &RuntimeCursor, body: impl IntoResponse) -> Response {
    let revision = cursor.revision.to_string();
    (
        [
            (crate::daemon::RUNTIME_EPOCH_HEADER, cursor.epoch.as_str()),
            (crate::daemon::RUNTIME_REVISION_HEADER, revision.as_str()),
        ],
        body,
    )
        .into_response()
}

pub(crate) async fn get_runtime_info(
    State(state): State<Arc<AppState>>,
    local: Option<Extension<Principal>>,
) -> Response {
    match state.runtime.publish(&state).await {
        Ok(snapshot) => Json(
            state
                .runtime
                .info(&state, local.as_deref(), &snapshot.value),
        )
        .into_response(),
        Err(_) => axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

pub(crate) async fn get_runtime_snapshot(State(state): State<Arc<AppState>>) -> Response {
    match state.runtime.publish(&state).await {
        Ok(snapshot) => Json(&snapshot.value).into_response(),
        Err(_) => axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

pub(crate) async fn runtime_ws(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    local: Option<Extension<Principal>>,
) -> Response {
    ws.max_message_size(1024)
        .max_frame_size(1024)
        .on_upgrade(move |socket| follow_runtime(socket, state, local.map(|local| local.0)))
}

async fn send_frame(socket: &mut WebSocket, state: &AppState, frame: Message) -> bool {
    tokio::select! {
        _ = state.shutdown.cancelled() => false,
        result = tokio::time::timeout(Duration::from_secs(15), socket.send(frame)) => matches!(result, Ok(Ok(()))),
    }
}

/// Encode and send one creation-progress frame; false when the socket is gone.
async fn send_creation_progress(
    socket: &mut WebSocket,
    state: &AppState,
    progress: &Arc<Vec<CreationProgress>>,
) -> bool {
    // One copy is inherent: the frame owns its payload. The caller's second
    // copy is not, and the watch channel already holds the vector behind an
    // `Arc` precisely so subscribers can share it.
    let frame = RuntimeFrame::<RuntimeSnapshot>::Creation(progress.as_ref().clone());
    let Ok(frame) = serde_json::to_string(&frame) else {
        return false;
    };
    send_frame(socket, state, Message::Text(frame.into())).await
}

/// Creation progress carries hook output, which is the documented secret
/// channel for session environment values, so only a peer the daemon
/// authorized as the local owner may receive it. A loopback TCP bearer client
/// is deliberately excluded: it is the same machine, not the same identity.
fn creation_progress_granted(local: Option<&Principal>) -> bool {
    matches!(local, Some(Principal::LocalPeer { .. }))
}

async fn follow_runtime(mut socket: WebSocket, state: Arc<AppState>, local: Option<Principal>) {
    let first_subscriber = !state.runtime.has_subscribers();
    let mut snapshots = state.runtime.snapshots.subscribe();
    let progress_granted = creation_progress_granted(local.as_ref());
    let mut creation_progress = state.runtime.creation_progress.subscribe();
    let mut owner_diagnostics = state.runtime.owner_diagnostics.subscribe();
    if first_subscriber {
        state.runtime.terminal_wake.notify_one();
    }
    state.runtime.request_publish();
    if state.runtime.publish(&state).await.is_err() {
        return;
    }
    let Some(initial) = snapshots.borrow_and_update().clone() else {
        return;
    };
    let hello = RuntimeFrame::<RuntimeSnapshot>::Hello(state.runtime.info(
        &state,
        local.as_ref(),
        &initial.value,
    ));
    let Ok(hello) = serde_json::to_string(&hello) else {
        return;
    };
    if !send_frame(&mut socket, &state, Message::Text(hello.into())).await
        || !send_frame(&mut socket, &state, initial.frame.clone()).await
    {
        return;
    }
    if progress_granted {
        let current = creation_progress.borrow_and_update().clone();
        if !send_creation_progress(&mut socket, &state, &current).await {
            return;
        }
    }
    if progress_granted {
        let current = owner_diagnostics.borrow_and_update().clone();
        let frame = RuntimeFrame::<RuntimeSnapshot>::OwnerDiagnostics {
            epoch: state.runtime.epoch.clone(),
            diagnostics: current.as_ref().clone(),
        };
        let Ok(frame) = serde_json::to_string(&frame) else {
            return;
        };
        if !send_frame(&mut socket, &state, Message::Text(frame.into())).await {
            return;
        }
    }
    let mut heartbeat = tokio::time::interval(Duration::from_secs(30));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_pong = tokio::time::Instant::now();
    loop {
        tokio::select! {
            _ = state.shutdown.cancelled() => break,
            changed = snapshots.changed() => {
                if changed.is_err() { break; }
                let snapshot = snapshots.borrow_and_update().clone();
                if let Some(snapshot) = snapshot {
                    if !send_frame(&mut socket, &state, snapshot.frame.clone()).await { break; }
                }
            }
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Pong(_))) => last_pong = tokio::time::Instant::now(),
                Some(Ok(Message::Ping(payload))) => {
                    if !send_frame(&mut socket, &state, Message::Pong(payload)).await { break; }
                }
                _ => break,
            },
            changed = creation_progress.changed(), if progress_granted => {
                if changed.is_err() { break; }
                let current = creation_progress.borrow_and_update().clone();
                if !send_creation_progress(&mut socket, &state, &current).await { break; }
            }
            changed = owner_diagnostics.changed(), if progress_granted => {
                if changed.is_err() { break; }
                let current = owner_diagnostics.borrow_and_update().clone();
                let frame = RuntimeFrame::<RuntimeSnapshot>::OwnerDiagnostics {
                    epoch: state.runtime.epoch.clone(), diagnostics: current.as_ref().clone(),
                };
                let Ok(frame) = serde_json::to_string(&frame) else { break; };
                if !send_frame(&mut socket, &state, Message::Text(frame.into())).await { break; }
            }
            _ = heartbeat.tick() => {
                if last_pong.elapsed() >= Duration::from_secs(90)
                    || !send_frame(&mut socket, &state, Message::Ping(Vec::new().into())).await { break; }
            }
        }
    }
}

pub(crate) async fn publish_loop(state: Arc<AppState>) {
    loop {
        let period = if state.runtime.has_subscribers() {
            Duration::from_millis(500)
        } else {
            Duration::from_secs(2)
        };
        tokio::select! {
            _ = state.shutdown.cancelled() => break,
            _ = tokio::time::sleep(period) => {},
            _ = state.runtime.wake.notified() => {},
        }
        if let Err(error) = state.runtime.publish(&state).await {
            tracing::error!(target: "server.runtime", %error, "runtime publication failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial]
    async fn shutdown_drains_an_abandoned_routed_native_commit() -> anyhow::Result<()> {
        use crate::session::{Instance, Storage};
        use anyhow::Context;
        use fs2::FileExt;
        use tower::ServiceExt;

        let _guard = crate::session::test_support::isolate_app_dir();
        let mut row = Instance::new("retained-commit", "/repo");
        row.source_profile = "test".into();
        let id = row.id.clone();
        let state = crate::server::test_support::build_test_app_state_with_policy(
            Vec::new(),
            vec!["localhost".into()],
            Vec::new(),
            None,
        );
        let storage = Storage::new("test", state.file_watch.clone())?;
        let (_, committed, _) = storage.update_with_snapshot(|rows, _| {
            rows.push(row);
            Ok(())
        })?;
        *state.instances.write().await = committed;
        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
        let router = crate::server::test_support::build_router_for_test(state.clone());
        let work = &state.runtime.work;
        assert!(
            work.tasks.lock().unwrap().is_empty(),
            "the isolated fixture must not register background producers"
        );
        let transition_probe = std::fs::OpenOptions::new().read(true).write(true).open(
            crate::session::get_app_dir()?
                .join(crate::migrations::v027_isolate_sandbox_stores::LOCK),
        )?;
        transition_probe.try_lock_exclusive()?;
        fs2::FileExt::unlock(&transition_probe)?;
        let request = |color: &str| {
            axum::http::Request::builder()
                .method(axum::http::Method::PATCH)
                .uri(format!("/api/sessions/{id}/color"))
                .header(axum::http::header::HOST, "localhost")
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .extension(axum::extract::ConnectInfo(
                    crate::server::peer::ConnectionPeer::UnixOwner {
                        uid: nix::unistd::geteuid().as_raw(),
                    },
                ))
                .body(axum::body::Body::from(
                    serde_json::json!({ "color": color }).to_string(),
                ))
                .unwrap()
        };
        let publication = state.publication.write().await;
        let mut requester = tokio::spawn(router.clone().oneshot(request("red")));
        // The native writer takes the shared migration transition before
        // publication, but the profile flock only afterwards. Observe that
        // physical transition rather than inferring admission from a read.
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if requester.is_finished() {
                    let response = (&mut requester)
                        .await
                        .context("color PATCH requester failed before native admission")??;
                    let status = response.status();
                    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
                    anyhow::bail!(
                        "color PATCH completed before native transition admission: status {status}, body {}",
                        String::from_utf8_lossy(&body)
                    );
                }
                match transition_probe.try_lock_exclusive() {
                    Ok(()) => fs2::FileExt::unlock(&transition_probe)?,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert_eq!(
                            work.tasks.lock().unwrap().len(),
                            1,
                            "the real mutation middleware must own the sole producer"
                        );
                        return Ok::<_, anyhow::Error>(());
                    }
                    Err(error) => return Err(error.into()),
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .context("native color writer did not acquire its shared storage transition")??;
        requester.abort();
        let cancelled = tokio::time::timeout(Duration::from_secs(3), requester)
            .await
            .context("aborted HTTP requester did not finish cancellation")?;
        assert!(matches!(cancelled, Err(error) if error.is_cancelled()));
        work.shutdown.cancel();
        let mut drain = Box::pin(work.drain());
        assert!(
            futures_util::poll!(&mut drain).is_pending(),
            "shutdown must retain the admitted writer while publication is held"
        );
        let refused =
            tokio::time::timeout(Duration::from_secs(3), router.oneshot(request("green")))
                .await
                .context("shutdown admission fence did not answer the second color PATCH")??;
        assert_eq!(
            refused.status(),
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "shutdown must close the actual mutation admission path"
        );
        drop(publication);
        tokio::time::timeout(Duration::from_secs(3), drain)
            .await
            .context("admitted native color writer did not drain after publication was released")?;
        assert_eq!(
            storage
                .load()?
                .iter()
                .find(|row| row.id == id)
                .unwrap()
                .color
                .as_deref(),
            Some("red")
        );
        assert_eq!(
            state
                .instances
                .read()
                .await
                .iter()
                .find(|row| row.id == id)
                .unwrap()
                .color
                .as_deref(),
            Some("red")
        );
        Ok(())
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn net_zero_commits_do_not_acknowledge_an_older_snapshot() -> anyhow::Result<()> {
        use crate::session::{Instance, Storage};
        let _guard = crate::session::test_support::isolate_app_dir();
        let mut row = Instance::new("receipt", "/repo");
        row.source_profile = "test".into();
        let id = row.id.clone();
        let state = crate::server::test_support::build_test_app_state(Vec::new());
        let storage = Storage::new("test", state.file_watch.clone())?;
        let (_, committed, _) = storage.update_with_snapshot(|rows, _| {
            rows.push(row);
            Ok(())
        })?;
        *state.instances.write().await = committed;
        *state.canonical_metadata.write().await =
            super::super::reload::load_all_profiles(&state.file_watch)?.metadata;
        let before = state.runtime.publish(&state).await?;
        let delayed = state.runtime.publish_lane.lock().await;
        let send_pin = |pinned| {
            let state = state.clone();
            let id = id.clone();
            tokio::spawn(async move {
                crate::server::api::update_session_pin(
                    State(state),
                    axum::extract::Path(id),
                    Ok(Json(crate::daemon::UpdatePinBody { pinned })),
                )
                .await
                .into_response()
            })
        };
        async fn wait_for_pin(storage: &Storage, id: &str, pinned: bool) -> anyhow::Result<()> {
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    if storage
                        .load()?
                        .iter()
                        .any(|row| row.id == id && row.pinned_at.is_some() == pinned)
                    {
                        return Ok::<_, anyhow::Error>(());
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await??;
            Ok(())
        }
        let pin = send_pin(true);
        wait_for_pin(&storage, &id, true).await?;
        let unpin = send_pin(false);
        wait_for_pin(&storage, &id, false).await?;
        drop(delayed);
        for response in [pin.await?, unpin.await?] {
            assert_eq!(response.status(), axum::http::StatusCode::OK);
            let revision = response.headers()[crate::daemon::RUNTIME_REVISION_HEADER]
                .to_str()?
                .parse::<u64>()?;
            assert!(
                revision > before.value.cursor.revision,
                "a completed mutation acknowledged a snapshot predating both commits"
            );
        }
        assert_eq!(
            state.runtime.snapshot(&state).await?.value.contents,
            before.value.contents
        );
        Ok(())
    }

    /// Only kernel-authenticated Unix owners receive hook output.
    #[test]
    fn creation_progress_is_granted_only_to_the_local_owner() {
        assert!(creation_progress_granted(Some(&Principal::LocalPeer {
            uid: 501
        })));
        assert!(!creation_progress_granted(None));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn owner_diagnostics_name_actual_source_without_parser_contents() {
        let _home = crate::session::test_support::isolate_app_dir();
        let app = crate::session::get_app_dir().unwrap();
        crate::session::get_profile_dir("test").unwrap();
        crate::session::config::update_config(|config| config.default_profile = "test".into())
            .unwrap();
        let storage = crate::session::Storage::new_unwatched("test").unwrap();
        storage.update(|_, _| Ok(())).unwrap();
        let state = crate::server::test_support::build_test_app_state_with_policy(
            Vec::new(),
            vec!["localhost".into()],
            Vec::new(),
            None,
        );
        for relative in [
            "config.toml",
            "projects.json",
            "profiles/test/config.toml",
            "profiles/test/projects.json",
            "profiles/test/groups.json",
            "profiles/test/sessions.json",
        ] {
            let path = app.join(relative);
            let previous = std::fs::read(&path).ok();
            std::fs::write(&path, b"{{SECRET_DIAGNOSTIC_PAYLOAD_DO_NOT_RENDER\n").unwrap();
            let failure = crate::server::reload::load_all_profiles(&state.file_watch)
                .err()
                .expect("actual source must fail");
            let diagnostics = failure.owner_diagnostics();
            assert_eq!(diagnostics.len(), 1, "{relative}");
            assert_eq!(diagnostics[0].path, path.to_string_lossy(), "{relative}");
            *state.canonical_health.write().await = failure.health;
            *state.canonical_diagnostics.write().unwrap() = diagnostics;
            let snapshot = state.runtime.publish(&state).await.unwrap();
            for owner in [false, true] {
                let authorization = owner.then(|| Principal::LocalPeer {
                    uid: nix::unistd::geteuid().as_raw(),
                });
                let info = state
                    .runtime
                    .info(&state, authorization.as_ref(), &snapshot.value);
                let encoded = serde_json::to_string(&info).unwrap();
                assert!(!encoded.contains("SECRET_DIAGNOSTIC_PAYLOAD_DO_NOT_RENDER"));
                assert_eq!(!info.owner_diagnostics.is_empty(), owner);
                if !owner {
                    assert!(!encoded.contains(path.to_str().unwrap()));
                }
            }
            match previous {
                Some(bytes) => std::fs::write(&path, bytes).unwrap(),
                None => std::fs::remove_file(&path).unwrap(),
            }
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn diagnostic_only_source_change_reaches_existing_owner_stream_without_new_cursor() {
        use crate::daemon::{RuntimeConnection, RuntimeEvent};
        let _home = crate::session::test_support::isolate_app_dir();
        let app = crate::session::get_app_dir()
            .unwrap()
            .canonicalize()
            .unwrap();
        crate::session::get_profile_dir("test").unwrap();
        crate::session::config::update_config(|config| config.default_profile = "test".into())
            .unwrap();
        let storage = crate::session::Storage::new_unwatched("test").unwrap();
        storage.update(|_, _| Ok(())).unwrap();
        let original = crate::server::test_support::build_test_app_state_with_policy(
            Vec::new(),
            vec!["localhost".into()],
            Vec::new(),
            None,
        );
        crate::server::test_support::refresh_canonical_metadata_for_test(&original).await;
        let mut owned = Arc::try_unwrap(original)
            .ok()
            .expect("fixture has one strong state owner");
        let work = owned.runtime.work.clone();
        owned.runtime = NativeRuntime::new(app.to_string_lossy().into_owned(), None, work);
        let state = Arc::new(owned);
        let socket = app.join("daemon/api.sock");
        let router = crate::server::test_support::build_router_for_test(state.clone());
        let server =
            crate::server::test_support::serve_owner_router_for_test(&socket, router).await;
        let endpoint = crate::acp::client::DaemonEndpoint::local_unix(socket);
        let mut connection = RuntimeConnection::connect(&endpoint, Some("test"))
            .await
            .unwrap();
        let profile = crate::session::get_profile_dir("test").unwrap();
        let groups = profile.join("groups.json");
        let projects = profile.join("projects.json");
        std::fs::write(&groups, "[]").unwrap();
        let valid_groups = std::fs::read(&groups).unwrap();
        std::fs::write(&groups, "{ SECRET_GROUP_CONTENT").unwrap();
        let first = crate::server::reload::load_all_profiles(&state.file_watch)
            .err()
            .unwrap();
        *state.canonical_diagnostics.write().unwrap() = first.owner_diagnostics();
        *state.canonical_health.write().await = first.health;
        let degraded = state.runtime.publish(&state).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            let mut source_seen = false;
            loop {
                if let RuntimeEvent::OwnerDiagnostics(diagnostics) =
                    connection.next_event().await.unwrap()
                {
                    source_seen = diagnostics
                        .iter()
                        .any(|diagnostic| diagnostic.path == groups.to_string_lossy());
                }
                if source_seen && connection.snapshot().cursor == degraded.value.cursor {
                    break;
                }
            }
        })
        .await
        .expect("first degraded source and canonical frame arrive on existing stream");
        std::fs::write(&projects, "{ SECRET_PROJECT_CONTENT").unwrap();
        std::fs::write(&groups, valid_groups).unwrap();
        let second = crate::server::reload::load_all_profiles(&state.file_watch)
            .err()
            .unwrap();
        *state.canonical_diagnostics.write().unwrap() = second.owner_diagnostics();
        *state.canonical_health.write().await = second.health;
        let same = state.runtime.publish(&state).await.unwrap();
        assert_eq!(same.value.cursor, degraded.value.cursor);
        assert_eq!(same.value.contents, degraded.value.contents);
        let diagnostics = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match connection.next_event().await.unwrap() {
                    RuntimeEvent::OwnerDiagnostics(diagnostics)
                        if diagnostics
                            .iter()
                            .any(|diagnostic| diagnostic.path == projects.to_string_lossy()) =>
                    {
                        break diagnostics
                    }
                    RuntimeEvent::Snapshot(_) => panic!(
                        "diagnostic-only source switch must not manufacture a canonical revision"
                    ),
                    _ => {}
                }
            }
        })
        .await
        .expect("diagnostic source switch cannot wait for another row mutation or reconnect");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].path, projects.to_string_lossy());
        let serialized = serde_json::to_string(&diagnostics).unwrap();
        assert!(
            !serialized.contains("SECRET_GROUP_CONTENT")
                && !serialized.contains("SECRET_PROJECT_CONTENT")
        );
        connection.close().await;
        state.shutdown.cancel();
        server.abort();
        let _ = server.await;
    }
}
