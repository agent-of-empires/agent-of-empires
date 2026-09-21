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
        if let Some(detection) = update.detection {
            session.instance.detection = detection;
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
            // The watcher never polled of its own, so baseline is None and
            // idle_entered_at is whatever the parent clone carried at attach
            // time. `Keep` honors "producer has no observation" and stops the
            // snapshot clobbering a real value the main-thread poller wrote.
            // The window is bounded by `REFRESH_INTERVAL`: after the watcher's
            // first poll, baseline is `Some` and `Set`/`Clear` reflect a real
            // observation. Arm order matches the consumer sites and the enum
            // declaration: `Set`, `Clear`, `Keep`.
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
            // The watcher owns detection while the main loop is parked on
            // the attach, so a proposal raised during it is confirmed after
            // it only if the copy hands this back (#3642).
            detection: Some(session.instance.detection),
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
                detection: None,
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
        // `apply_updates` is the attached-hooks copy of the match
        // `HomeView::apply_status_update` performs. Locking the shape here
        // catches a Set<->Clear swap that would otherwise compile cleanly and
        // still pass `apply_updates_runs_status_hook_for_transition`, which only
        // exercises `Keep`.
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
                detection: None,
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
                detection: None,
            }],
            false,
        );
        assert_eq!(
            sessions[0].instance.idle_entered_at, None,
            "IdleIntent::Clear must reset idle_entered_at to None"
        );
    }
}
