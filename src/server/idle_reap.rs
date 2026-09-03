//! Reaping sessions that have sat idle past their configured limit.

use std::sync::Arc;

use super::state::AppState;

/// How often the serve daemon evaluates plain tmux sessions for idle
/// auto-stop. Mirrors the acp reaper's cadence so a 2s status tick does
/// not drive a storage + tmux sweep on every iteration.
pub(super) const SESSION_IDLE_REAP_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(60);

/// Cap on concurrent `perform_stop` calls during one reap pass. `Instance::stop`
/// can block ~10s on `docker stop`; without a bound, a fleet of sessions all
/// crossing the threshold on the same tick would stampede the Docker daemon.
pub(super) const SESSION_IDLE_REAP_MAX_CONCURRENT: usize = 4;

/// Auto-stop plain (non-acp) tmux sessions that have been `Idle` past
/// their per-profile `session.auto_stop_idle_secs` (#1690). Gated to run at
/// most once per [`SESSION_IDLE_REAP_INTERVAL`]. Each candidate is claimed
/// under the per-profile storage lock (so a concurrently running TUI cannot
/// double-stop it) and stopped on a detached task with bounded concurrency,
/// keeping the status poll loop responsive.
pub(super) async fn reap_idle_sessions(
    state: &Arc<AppState>,
    last_reap: &mut Option<std::time::Instant>,
) {
    if last_reap.is_some_and(|t| t.elapsed() < SESSION_IDLE_REAP_INTERVAL) {
        return;
    }
    *last_reap = Some(std::time::Instant::now());

    // Live attach state. If the tmux query fails, skip this pass entirely
    // rather than risk reaping a session the user is attached to.
    let attached = match tokio::task::spawn_blocking(crate::tmux::attached_session_names).await {
        Ok(Ok(set)) => set,
        _ => return,
    };

    let now = chrono::Utc::now();
    let instances = { state.instances.read().await.clone() };

    // Resolve each distinct profile's threshold once, off the async runtime:
    // `resolve_config_or_warn` reads config files from disk, so building the
    // map directly here would block the poll loop.
    let profiles: Vec<String> = instances
        .iter()
        .filter(|inst| !inst.is_structured())
        .map(|inst| inst.effective_profile())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    let thresholds: std::collections::HashMap<String, u32> =
        tokio::task::spawn_blocking(move || {
            profiles
                .into_iter()
                .map(|p| {
                    let secs = crate::session::config::profile_config::resolve_config_or_warn(&p)
                        .session
                        .auto_stop_idle_secs;
                    (p, secs)
                })
                .collect()
        })
        .await
        .unwrap_or_default();

    let candidates =
        crate::session::idle_reap::idle_reap_candidates(&instances, now, &attached, |p| {
            thresholds.get(p).copied().unwrap_or(0)
        });

    let sem = Arc::new(tokio::sync::Semaphore::new(
        SESSION_IDLE_REAP_MAX_CONCURRENT,
    ));
    for cand in candidates {
        let sem = sem.clone();
        let file_watch = state.file_watch.clone();
        tokio::spawn(async move {
            let _permit = sem.acquire().await;
            let claim = {
                let cand = cand.clone();
                let file_watch = file_watch.clone();
                tokio::task::spawn_blocking(move || {
                    crate::session::idle_reap::claim_idle_stop(
                        &cand.profile,
                        file_watch,
                        &cand.session_id,
                        now,
                        cand.threshold_secs,
                    )
                })
                .await
            };
            let instance = match claim {
                Ok(Ok(Some(instance))) => instance,
                // Not eligible anymore (peer reaper won, user woke it) or a
                // storage error already logged downstream: nothing to do.
                _ => return,
            };
            let req = crate::session::stop::StopRequest {
                session_id: cand.session_id.clone(),
                instance,
            };
            let result =
                tokio::task::spawn_blocking(move || crate::session::stop::perform_stop(&req)).await;
            match result {
                Ok(r) if r.success => {
                    tracing::info!(
                        target: "server.idle_reap",
                        session = %cand.session_id,
                        profile = %cand.profile,
                        threshold_secs = cand.threshold_secs,
                        "auto-stopped idle tmux session",
                    );
                }
                _ => {
                    // The claim already persisted `Stopped`; a failed kill
                    // means tmux/container may still be alive, so flip to
                    // `Error` (matching the manual-stop failure path) instead
                    // of leaving a sticky-but-wrong `Stopped`.
                    let id = cand.session_id.clone();
                    let profile = cand.profile.clone();
                    let file_watch_for_storage = file_watch.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        if let Ok(storage) =
                            crate::session::Storage::new(&profile, file_watch_for_storage)
                        {
                            let _ = storage.update(|instances, _groups| {
                                if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
                                    inst.status = crate::session::Status::Error;
                                }
                                Ok(())
                            });
                        }
                    })
                    .await;
                    tracing::warn!(
                        target: "server.idle_reap",
                        session = %cand.session_id,
                        "idle auto-stop kill failed; marked Error",
                    );
                }
            }
        });
    }
}
