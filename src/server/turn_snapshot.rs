//! The working tree of each session's repos as it stood before the agent's latest turn, for the
//! dashboard's "Last turn" diff.
//!
//! The baseline is captured when a session settles (Idle, Starting, Stopped or Error), so it exists
//! before the next turn can write. A turn starts when the Daemon's authoritative status leaves a
//! settled state for Running, or for Waiting when it asks for approval first, and publishes the
//! baseline captured since the last settle. Waiting to Running resumes the same turn, so it does not
//! start one. A turn that starts
//! with no finished baseline (the capture failed or was still running) clears Last turn rather than
//! comparing against an older turn, and so does a lag that skips status events. The Daemon sees a
//! turn start only at its next status poll, so a baseline that finished less than
//! [`DETECTION_SLACK`] before the turn was seen may already hold the turn's first writes and is
//! refused too. One capture runs per session, at most [`MAX_CONCURRENT_CAPTURES`] in all, and a repo
//! with more new or changed content than `snapshot_worktree` allows gets no baseline. Edits made
//! between turns count toward the next one. Only the tree ids live in Daemon memory and are lost on restart;
//! the trees and blobs are written to each repo's object store and stay until `git gc` prunes them.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::broadcast::error::RecvError;

use super::push::StatusChange;
use super::AppState;
use crate::session::Status;
use crate::time::{Duration, Timestamp};

/// The longest a turn can run before the Daemon reports it Running: a 2s status poll with no
/// dashboard attached, plus the poll's own time.
const DETECTION_SLACK: Duration = Duration::seconds(5);
/// Captures at once across sessions; each walks and hashes a whole working tree.
const MAX_CONCURRENT_CAPTURES: usize = 2;

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

/// A turn leaves a settled state for Running, or for Waiting when it asks for approval before the
/// status poll saw it run.
pub(crate) fn is_turn_start(change: &StatusChange) -> bool {
    matches!(change.new, Status::Running | Status::Waiting) && is_settled(change.old)
}

fn is_settle(change: &StatusChange) -> bool {
    is_settled(change.new) && change.old != change.new
}

type Trees = HashMap<String, git2::Oid>;

/// Baselines captured since each session last settled, for its next turn to publish.
#[derive(Default)]
struct Baselines {
    /// Bumped by every settle and turn start, so a capture that finishes late is discarded.
    generation: HashMap<String, u64>,
    /// Each finished baseline with when it finished.
    ready: HashMap<String, (Trees, Timestamp)>,
    /// Sessions with a capture running, and whether a settle since asked for another.
    capturing: HashMap<String, bool>,
}

impl Baselines {
    fn bump(&mut self, session_id: &str) -> u64 {
        self.ready.remove(session_id);
        let generation = self.generation.entry(session_id.to_owned()).or_default();
        *generation += 1;
        *generation
    }

    /// A settle: the generation to capture at, or `None` when a running capture will capture again.
    fn settle(&mut self, session_id: &str) -> Option<u64> {
        let generation = self.bump(session_id);
        match self.capturing.get_mut(session_id) {
            Some(again) => {
                *again = true;
                None
            }
            None => {
                self.capturing.insert(session_id.to_owned(), false);
                Some(generation)
            }
        }
    }

    /// After a capture: the generation to capture again at if a settle asked, else the session's capture ends.
    fn next_capture(&mut self, session_id: &str) -> Option<u64> {
        if self.capturing.get(session_id) == Some(&true) {
            self.capturing.insert(session_id.to_owned(), false);
            return self.generation.get(session_id).copied();
        }
        self.capturing.remove(session_id);
        None
    }

    /// Keep a finished capture unless a later settle or turn start superseded it.
    fn finish(&mut self, session_id: &str, generation: u64, trees: Trees, finished_at: Timestamp) {
        if self.generation.get(session_id) == Some(&generation) && !trees.is_empty() {
            self.ready
                .insert(session_id.to_owned(), (trees, finished_at));
        }
    }

    /// Forget sessions outside `live`. A running capture ends on its own; its result no longer matches a generation.
    fn retain(&mut self, live: &std::collections::HashSet<String>) {
        self.generation.retain(|id, _| live.contains(id));
        self.ready.retain(|id, _| live.contains(id));
    }

    /// Drop every baseline and in-flight capture: skipped events may have started and settled a turn since.
    fn forget_all(&mut self) {
        self.ready.clear();
        for generation in self.generation.values_mut() {
            *generation += 1;
        }
    }

    /// Start a turn seen at `at`: take the finished baseline, if it surely predates the turn's writes.
    fn start_turn(&mut self, session_id: &str, at: Timestamp) -> Option<Trees> {
        let ready = self.ready.remove(session_id);
        self.bump(session_id);
        if let Some(again) = self.capturing.get_mut(session_id) {
            *again = false;
        }
        ready
            .filter(|(_, finished_at)| *finished_at + DETECTION_SLACK <= at)
            .map(|(trees, _)| trees)
    }
}

/// Capture a baseline at every settle and publish it when the next turn starts.
pub fn spawn_recorder(state: Arc<AppState>) {
    let mut rx = state.status_tx.subscribe();
    let task_state = state.clone();
    let baselines = Arc::new(Mutex::new(Baselines::default()));
    let permits = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_CAPTURES));
    state.runtime.work.spawn("server.turn_snapshot", async move {
        loop {
            let change = tokio::select! {
                recv = rx.recv() => match recv {
                    Ok(change) => change,
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!(target: "server.turn_snapshot", lagged = n, "turn snapshot recorder lagged, skipped status changes");
                        forget_turns(&baselines, &task_state.turn_snapshots);
                        continue;
                    }
                    Err(RecvError::Closed) => return,
                },
                _ = task_state.shutdown.cancelled() => return,
            };
            let id = change.instance_id.clone();
            if is_turn_start(&change) {
                let ready = lock(&baselines).start_turn(&id, change.at);
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
                forget_removed(&task_state, &baselines).await;
                let Some(mut generation) = lock(&baselines).settle(&id) else {
                    continue;
                };
                let (state, baselines, permits) =
                    (task_state.clone(), baselines.clone(), permits.clone());
                task_state.runtime.work.spawn("server.turn_snapshot.capture", async move {
                    loop {
                        let permit = tokio::select! {
                            permit = permits.acquire() => permit,
                            _ = state.shutdown.cancelled() => return,
                        };
                        let trees = match permit {
                            Ok(_permit) => capture(&state, &id).await,
                            Err(_) => Trees::new(),
                        };
                        let mut b = lock(&baselines);
                        b.finish(&id, generation, trees, Timestamp::now());
                        match b.next_capture(&id) {
                            Some(next) => generation = next,
                            None => return,
                        }
                    }
                });
            }
        }
    });
}

/// Drop what the recorder holds for sessions that no longer exist.
async fn forget_removed(state: &AppState, baselines: &Mutex<Baselines>) {
    let live: std::collections::HashSet<String> = state
        .instances
        .read()
        .await
        .iter()
        .map(|i| i.id.clone())
        .collect();
    lock(baselines).retain(&live);
    state
        .turn_snapshots
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .retain(|id, _| live.contains(id));
}

/// After skipped events, no baseline or published snapshot is known to match a turn boundary.
fn forget_turns(baselines: &Mutex<Baselines>, snapshots: &TurnSnapshots) {
    lock(baselines).forget_all();
    snapshots
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
}

fn lock(baselines: &Mutex<Baselines>) -> std::sync::MutexGuard<'_, Baselines> {
    baselines
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

async fn capture(state: &Arc<AppState>, session_id: &str) -> Trees {
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
            (Status::Idle, Status::Waiting, true),
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
            |n: u8| Trees::from([("/r".to_owned(), git2::Oid::from_bytes(&[n; 20]).unwrap())]);
        let t0 = Timestamp::now();
        // A turn seen well after the capture finished, and one seen too soon for its writes to have waited.
        let later = t0 + DETECTION_SLACK;
        let soon = t0 + DETECTION_SLACK - Duration::milliseconds(1);
        let mut b = Baselines::default();

        let g = b.bump("s");
        b.finish("s", g, tree(1), t0);
        assert_eq!(b.start_turn("s", later), Some(tree(1)));

        // The next turn has no baseline of its own: it must not reuse the previous one.
        assert_eq!(b.start_turn("s", later), None);

        // A baseline that may hold the turn's first writes is refused.
        let g = b.bump("s");
        b.finish("s", g, tree(7), t0);
        assert_eq!(b.start_turn("s", soon), None);

        // A capture still running when the turn starts is discarded.
        let g = b.bump("s");
        assert_eq!(b.start_turn("s", later), None);
        b.finish("s", g, tree(2), t0);
        assert_eq!(b.start_turn("s", later), None);

        // A later settle supersedes an earlier capture that finishes after it.
        let early = b.bump("s");
        let late = b.bump("s");
        b.finish("s", early, tree(3), t0);
        b.finish("s", late, tree(4), t0);
        assert_eq!(b.start_turn("s", later), Some(tree(4)));

        // A lag drops finished and in-flight baselines and published snapshots.
        let g = b.bump("s");
        b.finish("s", g, tree(5), t0);
        let in_flight = b.bump("t");
        let (baselines, snapshots) = (Mutex::new(b), TurnSnapshots::default());
        snapshots.write().unwrap().insert(
            "s".into(),
            TurnSnapshot {
                at: Timestamp::now(),
                trees: tree(4),
            },
        );
        forget_turns(&baselines, &snapshots);
        let mut b = baselines.into_inner().unwrap();
        b.finish("t", in_flight, tree(6), t0);
        assert_eq!(
            (b.start_turn("s", later), b.start_turn("t", later)),
            (None, None)
        );
        assert!(snapshots.read().unwrap().is_empty());

        // A capture that got no trees leaves nothing to publish.
        let g = b.bump("s");
        b.finish("s", g, Trees::new(), t0);
        assert_eq!(b.start_turn("s", later), None);
    }

    #[test]
    fn a_session_runs_one_capture_and_a_settle_meanwhile_asks_it_for_another() {
        let mut b = Baselines::default();
        let first = b.settle("s").expect("the first settle starts a capture");
        // Settles while it runs start no second capture, and one more pass covers them all.
        assert_eq!(b.settle("s"), None);
        assert_eq!(b.settle("s"), None);
        let again = b
            .next_capture("s")
            .expect("a settle asked for another pass");
        assert!(again > first);
        assert_eq!(b.next_capture("s"), None);
        assert!(
            b.settle("s").is_some(),
            "with no capture running, a settle starts one"
        );

        // A turn start cancels the pending pass: the next settle captures for the next turn.
        assert_eq!(b.settle("s"), None);
        b.start_turn("s", Timestamp::now());
        assert_eq!(b.next_capture("s"), None);

        // A deleted session's baseline is dropped, and a capture still running for it keeps nothing.
        let g = b.settle("gone").unwrap();
        b.retain(&["s".to_owned()].into());
        b.finish(
            "gone",
            g,
            Trees::from([("/r".to_owned(), git2::Oid::from_bytes(&[9; 20]).unwrap())]),
            Timestamp::now(),
        );
        assert!(!b.ready.contains_key("gone") && !b.generation.contains_key("gone"));
    }
}
