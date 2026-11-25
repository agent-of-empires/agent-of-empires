//! The working tree of each session's repos as it stood before the agent's latest turn, for the
//! dashboard's "Last turn" diff.
//!
//! The baseline is captured when a session settles (Idle, Starting, Stopped or Error), so it exists
//! before the next turn can write. A turn starts when the Daemon's authoritative status goes to
//! Running from a settled state, and publishes the baseline captured since the last settle. Waiting
//! to Running resumes the same turn after an approval, so it does not start one. A turn that starts
//! with no finished baseline (the capture failed or was still running) clears Last turn rather than
//! comparing against an older turn. Edits made between turns count toward the next one. Snapshots
//! live in Daemon memory and are lost on restart.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

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

fn is_settled(status: Status) -> bool {
    matches!(
        status,
        Status::Idle | Status::Starting | Status::Stopped | Status::Error
    )
}

pub(crate) fn is_turn_start(change: &StatusChange) -> bool {
    change.new == Status::Running && is_settled(change.old)
}

fn is_settle(change: &StatusChange) -> bool {
    is_settled(change.new) && change.old != change.new
}

/// Baselines captured since each session last settled, for its next turn to publish.
#[derive(Default)]
struct Baselines {
    /// Bumped by every settle and turn start, so a capture that finishes late is discarded.
    generation: HashMap<String, u64>,
    ready: HashMap<String, HashMap<String, git2::Oid>>,
}

impl Baselines {
    fn bump(&mut self, session_id: &str) -> u64 {
        self.ready.remove(session_id);
        let generation = self.generation.entry(session_id.to_owned()).or_default();
        *generation += 1;
        *generation
    }

    /// Keep a finished capture unless a later settle or turn start superseded it.
    fn finish(&mut self, session_id: &str, generation: u64, trees: HashMap<String, git2::Oid>) {
        if self.generation.get(session_id) == Some(&generation) && !trees.is_empty() {
            self.ready.insert(session_id.to_owned(), trees);
        }
    }

    /// Drop every baseline and in-flight capture: skipped events may have started and settled a turn since.
    fn forget_all(&mut self) {
        self.ready.clear();
        for generation in self.generation.values_mut() {
            *generation += 1;
        }
    }

    /// Start a turn: take the finished baseline, if any.
    fn start_turn(&mut self, session_id: &str) -> Option<HashMap<String, git2::Oid>> {
        let ready = self.ready.remove(session_id);
        self.bump(session_id);
        ready
    }
}

/// Capture a baseline at every settle and publish it when the next turn starts.
pub fn spawn_recorder(state: Arc<AppState>) {
    let mut rx = state.status_tx.subscribe();
    let task_state = state.clone();
    let baselines = Arc::new(Mutex::new(Baselines::default()));
    state.runtime.work.spawn("server.turn_snapshot", async move {
        loop {
            let change = tokio::select! {
                recv = rx.recv() => match recv {
                    Ok(change) => change,
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!(target: "server.turn_snapshot", lagged = n, "turn snapshot recorder lagged, skipped status changes");
                        lock(&baselines).forget_all();
                        continue;
                    }
                    Err(RecvError::Closed) => return,
                },
                _ = task_state.shutdown.cancelled() => return,
            };
            let id = change.instance_id.clone();
            if is_turn_start(&change) {
                let ready = lock(&baselines).start_turn(&id);
                let mut snapshots = task_state
                    .turn_snapshots
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match ready {
                    Some(trees) => {
                        snapshots.insert(id, TurnSnapshot { at: change.at, trees });
                    }
                    None => {
                        snapshots.remove(&id);
                    }
                }
            } else if is_settle(&change) {
                let generation = lock(&baselines).bump(&id);
                let (state, baselines) = (task_state.clone(), baselines.clone());
                tokio::spawn(async move {
                    let trees = capture(&state, &id).await;
                    lock(&baselines).finish(&id, generation, trees);
                });
            }
        }
    });
}

fn lock(baselines: &Mutex<Baselines>) -> std::sync::MutexGuard<'_, Baselines> {
    baselines
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

async fn capture(state: &Arc<AppState>, session_id: &str) -> HashMap<String, git2::Oid> {
    let repos: Vec<String> = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == session_id) else {
            return HashMap::new();
        };
        crate::server::api::sessions::diff_repos_of(inst)
            .into_iter()
            .map(|repo| repo.path)
            .collect()
    };
    tokio::task::spawn_blocking(move || {
        repos
            .into_iter()
            .filter(|path| !crate::git::is_outside_repository(std::path::Path::new(path)))
            .filter_map(|path| {
                match crate::git::diff::snapshot_worktree(std::path::Path::new(&path)) {
                    Ok(tree) => Some((path, tree)),
                    Err(error) => {
                        tracing::warn!(target: "server.turn_snapshot", %error, repo = %path, "could not snapshot the working tree for the next turn");
                        None
                    }
                }
            })
            .collect::<HashMap<_, _>>()
    })
    .await
    .unwrap_or_default()
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
        for (old, new, settles) in [
            (Status::Running, Status::Idle, true),
            (Status::Unknown, Status::Idle, true),
            (Status::Idle, Status::Stopped, true),
            (Status::Running, Status::Waiting, false),
            (Status::Idle, Status::Idle, false),
        ] {
            assert_eq!(is_settle(&change(old, new)), settles, "{old:?} -> {new:?}");
        }
    }

    #[test]
    fn a_turn_publishes_only_a_baseline_finished_since_the_last_settle() {
        let tree =
            |n: u8| HashMap::from([("/r".to_owned(), git2::Oid::from_bytes(&[n; 20]).unwrap())]);
        let mut b = Baselines::default();

        let g = b.bump("s");
        b.finish("s", g, tree(1));
        assert_eq!(b.start_turn("s"), Some(tree(1)));

        // The next turn has no baseline of its own: it must not reuse the previous one.
        assert_eq!(b.start_turn("s"), None);

        // A capture still running when the turn starts is discarded.
        let g = b.bump("s");
        assert_eq!(b.start_turn("s"), None);
        b.finish("s", g, tree(2));
        assert_eq!(b.start_turn("s"), None);

        // A later settle supersedes an earlier capture that finishes after it.
        let early = b.bump("s");
        let late = b.bump("s");
        b.finish("s", early, tree(3));
        b.finish("s", late, tree(4));
        assert_eq!(b.start_turn("s"), Some(tree(4)));

        // A lag drops both finished and in-flight baselines.
        let g = b.bump("s");
        b.finish("s", g, tree(5));
        let in_flight = b.bump("t");
        b.forget_all();
        b.finish("t", in_flight, tree(6));
        assert_eq!((b.start_turn("s"), b.start_turn("t")), (None, None));

        // A capture that got no trees leaves nothing to publish.
        let g = b.bump("s");
        b.finish("s", g, HashMap::new());
        assert_eq!(b.start_turn("s"), None);
    }
}
