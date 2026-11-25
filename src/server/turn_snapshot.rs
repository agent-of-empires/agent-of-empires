//! The working tree of each session's repos as it stood when the agent's latest turn started, for
//! the dashboard's "Last turn" diff.
//!
//! A turn starts when the Daemon's authoritative status goes to Running from a settled state. Waiting
//! to Running resumes the same turn after an approval, and Unknown to Running is a first observation,
//! so neither records. Snapshots live in Daemon memory: after a restart, Last turn waits for the next
//! turn rather than diffing against a tree recorded by a previous run.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::broadcast::error::RecvError;

use super::push::StatusChange;
use super::AppState;
use crate::session::Status;
use crate::time::Timestamp;

#[derive(Debug, Clone)]
pub struct TurnSnapshot {
    pub at: Timestamp,
    /// Tree per repo path, for the repos that could be snapshotted.
    pub trees: HashMap<String, git2::Oid>,
}

/// Session id to its latest turn's snapshot.
pub(crate) type TurnSnapshots = std::sync::RwLock<HashMap<String, TurnSnapshot>>;

pub(crate) fn is_turn_start(change: &StatusChange) -> bool {
    change.new == Status::Running
        && matches!(
            change.old,
            Status::Idle | Status::Starting | Status::Stopped | Status::Error
        )
}

/// Record a snapshot for every turn that starts from now on.
pub fn spawn_recorder(state: Arc<AppState>) {
    let mut rx = state.status_tx.subscribe();
    let task_state = state.clone();
    state.runtime.work.spawn("server.turn_snapshot", async move {
        loop {
            let change = tokio::select! {
                recv = rx.recv() => match recv {
                    Ok(change) => change,
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!(target: "server.turn_snapshot", lagged = n, "turn snapshot recorder lagged, skipped status changes");
                        continue;
                    }
                    Err(RecvError::Closed) => return,
                },
                _ = task_state.shutdown.cancelled() => return,
            };
            if is_turn_start(&change) {
                record(&task_state, &change.instance_id, change.at).await;
            }
        }
    });
}

async fn record(state: &Arc<AppState>, session_id: &str, at: Timestamp) {
    let repos: Vec<String> = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == session_id) else {
            return;
        };
        crate::server::api::sessions::diff_repos_of(inst)
            .into_iter()
            .map(|repo| repo.path)
            .collect()
    };
    let trees = tokio::task::spawn_blocking(move || {
        repos
            .into_iter()
            .filter(|path| !crate::git::is_outside_repository(std::path::Path::new(path)))
            .filter_map(|path| {
                match crate::git::diff::snapshot_worktree(std::path::Path::new(&path)) {
                    Ok(tree) => Some((path, tree)),
                    Err(error) => {
                        tracing::warn!(target: "server.turn_snapshot", %error, repo = %path, "could not snapshot the working tree at turn start");
                        None
                    }
                }
            })
            .collect::<HashMap<_, _>>()
    })
    .await
    .unwrap_or_default();
    if trees.is_empty() {
        return;
    }
    state
        .turn_snapshots
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(session_id.to_owned(), TurnSnapshot { at, trees });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_turn_from_a_settled_state_records() {
        let change = |old, new| StatusChange {
            instance_id: "s".into(),
            instance_title: "s".into(),
            effective_profile: "default".into(),
            old,
            new,
            at: Timestamp::now(),
        };
        for (old, new, starts) in [
            (Status::Idle, Status::Running, true),
            (Status::Starting, Status::Running, true),
            (Status::Stopped, Status::Running, true),
            (Status::Error, Status::Running, true),
            (Status::Waiting, Status::Running, false),
            (Status::Unknown, Status::Running, false),
            (Status::Running, Status::Idle, false),
        ] {
            assert_eq!(
                is_turn_start(&change(old, new)),
                starts,
                "{old:?} -> {new:?}"
            );
        }
    }
}
