//! Status hook polling while the TUI is blocked inside tmux attach.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crate::session::{Instance, Status};
use crate::status_hooks::StatusHookConfig;

use super::status_poller::{poll_statuses_once, IdleIntent, StatusPollState, StatusUpdate};

const REFRESH_INTERVAL: Duration = Duration::from_millis(500);

pub(super) struct AttachedStatusHookSession {
    pub(super) instance: Instance,
    pub(super) hook_config: StatusHookConfig,
}

pub(super) struct AttachedStatusHookWatcher {
    stop_tx: mpsc::Sender<()>,
    snapshot_rx: mpsc::Receiver<Vec<StatusUpdate>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl AttachedStatusHookWatcher {
    pub(super) fn start(mut sessions: Vec<AttachedStatusHookSession>) -> Option<Self> {
        sessions
            .retain(|session| crate::status_hooks::has_configured_commands(&session.hook_config));
        if sessions.is_empty() {
            return None;
        }

        let (stop_tx, stop_rx) = mpsc::channel();
        let (snapshot_tx, snapshot_rx) = mpsc::channel();
        let handle = match thread::Builder::new()
            .name("aoe-attached-status-hooks".to_string())
            .spawn(move || run_loop(sessions, stop_rx, snapshot_tx))
        {
            Ok(handle) => handle,
            Err(e) => {
                tracing::warn!(
                    target: "hooks.status_hooks",
                    "failed to start attached status hook watcher: {}",
                    e
                );
                return None;
            }
        };

        Some(Self {
            stop_tx,
            snapshot_rx,
            handle: Some(handle),
        })
    }

    pub(super) fn stop(mut self) -> Vec<StatusUpdate> {
        let _ = self.stop_tx.send(());
        if let Some(handle) = self.handle.take() {
            if let Err(e) = handle.join() {
                tracing::warn!(
                    target: "hooks.status_hooks",
                    "attached status hook watcher panicked: {:?}",
                    e
                );
            }
        }

        let mut latest = Vec::new();
        while let Ok(snapshot) = self.snapshot_rx.try_recv() {
            latest = snapshot;
        }
        latest
    }
}

fn run_loop(
    mut sessions: Vec<AttachedStatusHookSession>,
    stop_rx: mpsc::Receiver<()>,
    snapshot_tx: mpsc::Sender<Vec<StatusUpdate>>,
) {
    let mut state = StatusPollState::new();

    loop {
        if stop_rx.try_recv().is_ok() {
            break;
        }

        let instances = sessions
            .iter()
            .map(|session| session.instance.clone())
            .collect();
        let updates = poll_statuses_once(instances, &mut state);
        apply_updates(&mut sessions, updates, true);

        if stop_rx.recv_timeout(REFRESH_INTERVAL).is_ok() {
            break;
        }
    }

    let _ = snapshot_tx.send(snapshot(&sessions));
}

fn apply_updates(
    sessions: &mut [AttachedStatusHookSession],
    updates: Vec<StatusUpdate>,
    run_hooks: bool,
) {
    for update in updates {
        let Some(session) = sessions
            .iter_mut()
            .find(|session| session.instance.id == update.id)
        else {
            continue;
        };

        let old = session.instance.status;
        if matches!(old, Status::Deleting | Status::Creating | Status::Stopped)
            || update.status == Status::Stopped
        {
            continue;
        }

        session.instance.status = update.status;
        session.instance.last_error = update.last_error;
        match update.idle_entered_at {
            IdleIntent::Set(ts) => session.instance.idle_entered_at = Some(ts),
            IdleIntent::Clear => session.instance.idle_entered_at = None,
            IdleIntent::Keep => {}
        }
        if let Some(baseline) = update.live_status_baseline {
            session.instance.live_status_baseline = Some(baseline);
        }

        if run_hooks && old != update.status {
            crate::status_hooks::run_for_transition(
                &session.instance,
                old,
                update.status,
                &session.hook_config,
            );
        }
    }
}

fn snapshot(sessions: &[AttachedStatusHookSession]) -> Vec<StatusUpdate> {
    sessions
        .iter()
        .map(|session| StatusUpdate {
            id: session.instance.id.clone(),
            status: session.instance.status,
            last_error: session.instance.last_error.clone(),
            // Watcher never observed a poll of its own => baseline is None
            // and idle_entered_at is whatever the parent clone carried at
            // attach-time. Emitting `Keep` here honors "producer has no
            // observation" and prevents the snapshot from clobbering a
            // real value the main-thread poller already wrote onto the
            // real Instance during attach. This window is bounded by
            // `REFRESH_INTERVAL` at the top of the module: the watcher
            // seeds baseline on its first poll, after which `Some(_)` +
            // `apply_updates`'s writes cover the field. Once the watcher
            // has polled at least once, baseline is Some and
            // idle_entered_at was written by `apply_updates` above, so
            // `Set(ts)` / `Clear` reflect the real observation.
            //
            // Arm order matches consumer sites (`apply_updates` above,
            // `apply_status_update` in `src/tui/home/mod.rs`) and the
            // enum declaration in `src/tui/status_poller.rs`:
            // `Set` first, `Clear` second, `Keep` last.
            idle_entered_at: match (
                session.instance.live_status_baseline,
                session.instance.idle_entered_at,
            ) {
                (Some(_), Some(ts)) => IdleIntent::Set(ts),
                (Some(_), None) => IdleIntent::Clear,
                (None, _) => IdleIntent::Keep,
            },
            last_accessed_at: session.instance.last_accessed_at,
            pane_dead: session.instance.pane_dead_observed,
            live_status_baseline: session.instance.live_status_baseline,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status_hooks::{take_recorded_launches, StatusHookConfig};
    use chrono::Utc;
    use serial_test::serial;

    #[test]
    #[serial]
    fn apply_updates_runs_status_hook_for_transition() {
        let instance = Instance::new("Hook Target", "/tmp/hook-target");
        let id = instance.id.clone();
        let mut sessions = vec![AttachedStatusHookSession {
            instance,
            hook_config: StatusHookConfig {
                enabled: true,
                debounce_ms: 0,
                on_waiting: Some("notify-waiting".to_string()),
                ..Default::default()
            },
        }];
        take_recorded_launches();

        apply_updates(
            &mut sessions,
            vec![StatusUpdate {
                id: id.clone(),
                status: Status::Waiting,
                last_error: None,
                idle_entered_at: IdleIntent::Keep,
                last_accessed_at: None,
                pane_dead: false,
                live_status_baseline: None,
            }],
            true,
        );

        let launches = take_recorded_launches();
        assert_eq!(launches.len(), 1);
        assert_eq!(launches[0].command, "notify-waiting");
        assert_eq!(launches[0].context.session_id, id);
        assert_eq!(launches[0].context.old_status, Status::Idle);
        assert_eq!(launches[0].context.new_status, Status::Waiting);
    }

    #[test]
    #[serial]
    fn apply_updates_maps_idle_intent_set_and_clear_arms() {
        // Regression guard for #2690: the Set/Clear arm mapping in
        // `apply_updates` (the `match update.idle_entered_at` arms
        // above) is the attached-hooks copy of the same match that
        // `HomeView::apply_status_update` performs in
        // `src/tui/home/mod.rs`. The `home` copy is covered by
        // `apply_status_update_clears_idle_entered_at_on_idle_to_running`
        // in `src/tui/home/tests.rs`. This test locks the equivalent
        // shape here so a Set<->Clear swap in either consumer is
        // caught at review time; without it, a swap in this file
        // compiles cleanly and passes the existing
        // `apply_updates_runs_status_hook_for_transition` test (which
        // only exercises `Keep`).
        let mut instance = Instance::new("Idle Target", "/tmp/idle-target");
        instance.status = Status::Running;
        instance.idle_entered_at = None;
        let id = instance.id.clone();
        let mut sessions = vec![AttachedStatusHookSession {
            instance,
            hook_config: StatusHookConfig::default(),
        }];

        let stop_time = Utc::now() - chrono::Duration::minutes(3);
        apply_updates(
            &mut sessions,
            vec![StatusUpdate {
                id: id.clone(),
                status: Status::Idle,
                last_error: None,
                idle_entered_at: IdleIntent::Set(stop_time),
                last_accessed_at: None,
                pane_dead: false,
                live_status_baseline: Some(Status::Idle),
            }],
            false,
        );
        assert_eq!(
            sessions[0].instance.idle_entered_at,
            Some(stop_time),
            "IdleIntent::Set(ts) must write Some(ts) onto idle_entered_at"
        );

        apply_updates(
            &mut sessions,
            vec![StatusUpdate {
                id: id.clone(),
                status: Status::Running,
                last_error: None,
                idle_entered_at: IdleIntent::Clear,
                last_accessed_at: None,
                pane_dead: false,
                live_status_baseline: Some(Status::Running),
            }],
            false,
        );
        assert_eq!(
            sessions[0].instance.idle_entered_at, None,
            "IdleIntent::Clear must reset idle_entered_at to None"
        );
    }
}
