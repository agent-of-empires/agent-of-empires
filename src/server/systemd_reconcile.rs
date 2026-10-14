//! Daemon-side systemd status only: `STATUS=` updates from the status stream.
//! Creating or deleting a session without a status transition does not push,
//! so the counts can lag briefly.

use std::sync::Arc;

use tokio::sync::broadcast::error::RecvError;

use super::state::AppState;
use crate::session::Status;

fn count_statuses(it: impl Iterator<Item = Status>) -> (usize, usize, usize) {
    it.fold((0, 0, 0), |(r, w, i), s| match s {
        Status::Running => (r + 1, w, i),
        Status::Waiting => (r, w + 1, i),
        Status::Idle => (r, w, i + 1),
        _ => (r, w, i),
    })
}

pub(super) async fn push_status(state: &AppState) {
    if !cfg!(target_os = "linux") {
        return;
    }
    let (r, w, i) = count_statuses(state.instances.read().await.iter().map(|inst| inst.status));
    let line = crate::process::systemd::status_line(r, w, i);
    #[cfg(unix)]
    let _ = crate::process::systemd::notify::send(format!("STATUS={line}\n").as_bytes());
    #[cfg(not(unix))]
    let _ = line;
}

/// Inert outside Linux.
pub(super) fn spawn(state: Arc<AppState>) {
    if !cfg!(target_os = "linux") {
        return;
    }
    crate::task_util::spawn_supervised(
        "server.systemd_reconcile",
        crate::task_util::PanicPolicy::Log,
        async move {
            // Subscribe before the first push so no change between them is lost.
            let mut rx = state.status_tx.subscribe();
            push_status(&state).await;
            loop {
                match rx.recv().await {
                    Ok(_) | Err(RecvError::Lagged(_)) => {
                        // Coalesce bursts: only the latest counts matter.
                        while rx.try_recv().is_ok() {}
                        push_status(&state).await;
                    }
                    Err(RecvError::Closed) => return,
                }
            }
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_group_statuses() {
        use Status::*;
        let c = count_statuses([Running, Running, Waiting, Idle, Stopped, Error].into_iter());
        assert_eq!(c, (2, 1, 1));
    }
}
