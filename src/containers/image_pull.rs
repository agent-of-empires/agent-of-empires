//! One image pull per Container runtime and image in this process. Every caller waits on the
//! running pull as a waiter, so a second caller never starts a second runtime process.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard};
use std::time::Duration;

use serde::Serialize;
use tokio_util::sync::CancellationToken;

use super::error::{DockerError, Result};

/// How often a waiter checks its own cancellation.
const CANCEL_POLL: Duration = Duration::from_millis(50);

/// Why a pull failed, read from the runtime's stderr. Only Docker's stderr has been captured
/// (`tests/fixtures/pull-stderr`), so other runtimes' failures read as `Other` until they are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PullFailureReason {
    Timeout,
    Network,
    DiskSpace,
    Auth,
    Other,
}

impl PullFailureReason {
    /// The wording both clients show.
    pub fn user_text(self) -> String {
        let why = match self {
            Self::Timeout => "it took longer than 10 minutes",
            Self::Network => "no network connection",
            Self::DiskSpace => "not enough disk space",
            Self::Auth => "the registry needs you to sign in",
            Self::Other => "see aoe logs",
        };
        format!("Couldn't download the sandbox image: {why}.")
    }

    /// Network is checked before auth, so a connection error can't read as a sign-in problem.
    pub(crate) fn classify(stderr: &str) -> Self {
        let has = |phrases: &[&str]| phrases.iter().any(|p| stderr.contains(p));
        if has(&["no space left on device"]) {
            Self::DiskSpace
        } else if has(&["no such host", "dial tcp"]) {
            Self::Network
        } else if has(&[
            "pull access denied",
            "error from registry: denied",
            "unauthorized",
        ]) {
            Self::Auth
        } else {
            Self::Other
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PullState {
    Idle,
    Running,
    Done,
    /// `message` is `reason`'s shared wording; `error` is the runtime's output, for Details.
    Failed {
        reason: PullFailureReason,
        message: String,
        error: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct PullKey {
    pub runtime: &'static str,
    pub image: String,
}

struct Flight {
    /// Cancelled when the last waiter leaves; kills the runtime process.
    kill: CancellationToken,
    state: Mutex<FlightState>,
    finished: Condvar,
}

struct FlightState {
    waiters: usize,
    outcome: Option<Result<()>>,
}

impl Flight {
    /// Gives up one waiter's place; the last one out kills the pull. Returns whether it was last.
    fn leave(&self, state: &mut FlightState) -> bool {
        state.waiters -= 1;
        if state.waiters > 0 {
            return false;
        }
        self.kill.cancel();
        true
    }
}

enum Entry {
    Running(Arc<Flight>),
    Finished(PullState),
}

static FLIGHTS: LazyLock<Mutex<HashMap<PullKey, Entry>>> = LazyLock::new(Mutex::default);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// A caller's place in a running pull.
pub(crate) struct Waiter {
    flight: Arc<Flight>,
    image: String,
    /// Still counted in the flight's waiters.
    joined: bool,
}

/// Joins `key`'s running pull, or starts `pull` on its own thread when none is running.
pub(crate) fn join_with<F>(key: PullKey, pull: F) -> Waiter
where
    F: FnOnce(&CancellationToken) -> Result<()> + Send + 'static,
{
    let mut flights = lock(&FLIGHTS);
    if let Some(Entry::Running(flight)) = flights.get(&key) {
        let mut state = lock(&flight.state);
        // Under the flight's lock, so a pull its last waiter is killing can't gain a waiter.
        if !flight.kill.is_cancelled() {
            state.waiters += 1;
            return Waiter {
                flight: flight.clone(),
                image: key.image,
                joined: true,
            };
        }
    }
    let flight = Arc::new(Flight {
        kill: CancellationToken::new(),
        state: Mutex::new(FlightState {
            waiters: 1,
            outcome: None,
        }),
        finished: Condvar::new(),
    });
    flights.insert(key.clone(), Entry::Running(flight.clone()));
    drop(flights);
    let runner = flight.clone();
    let image = key.image.clone();
    std::thread::spawn(move || {
        let started = std::time::Instant::now();
        // A panic still finishes the flight; otherwise its waiters and every later joiner hang.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| pull(&runner.kill)))
            .unwrap_or_else(|_| {
                Err(DockerError::ImagePullFailed {
                    runtime: key.runtime,
                    image: key.image.clone(),
                    detail: "the pull panicked".into(),
                    reason: PullFailureReason::Other,
                })
            });
        let duration_ms = started.elapsed().as_millis();
        match &outcome {
            Ok(()) => {
                tracing::info!(target: "containers.image", image = %key.image, runtime = key.runtime, ok = true, duration_ms, "image pull finished")
            }
            Err(error) => {
                tracing::warn!(target: "containers.image", image = %key.image, runtime = key.runtime, ok = false, duration_ms, %error, "image pull finished")
            }
        }
        let mut flights = lock(&FLIGHTS);
        // A pull started after this one was killed owns the entry now.
        if matches!(flights.get(&key), Some(Entry::Running(f)) if Arc::ptr_eq(f, &runner)) {
            match finished_state(&outcome) {
                Some(state) => flights.insert(key, Entry::Finished(state)),
                None => flights.remove(&key),
            };
        }
        drop(flights);
        lock(&runner.state).outcome = Some(outcome);
        runner.finished.notify_all();
    });
    Waiter {
        flight,
        image,
        joined: true,
    }
}

fn finished_state(outcome: &Result<()>) -> Option<PullState> {
    match outcome {
        Ok(()) => Some(PullState::Done),
        Err(DockerError::Cancelled(_)) => None,
        Err(error) => {
            let reason = match error {
                DockerError::ImagePullFailed { reason, .. } => *reason,
                _ => PullFailureReason::Other,
            };
            Some(PullState::Failed {
                reason,
                message: reason.user_text(),
                error: error.to_string(),
            })
        }
    }
}

impl Waiter {
    /// Waits for the pull. A cancelled caller leaves it running for the other waiters. The last
    /// one kills it and waits for the outcome, so no runtime process outlives the caller and a
    /// pull that finishes inside the kill's grace still counts.
    pub(crate) fn wait(mut self, cancel: &CancellationToken) -> Result<()> {
        let mut state = lock(&self.flight.state);
        loop {
            if let Some(outcome) = &state.outcome {
                return match outcome {
                    Ok(()) => Ok(()),
                    Err(error) => Err(reissue(error)),
                };
            }
            if self.joined && cancel.is_cancelled() {
                self.joined = false;
                if !self.flight.leave(&mut state) {
                    return Err(DockerError::Cancelled(format!("pull of {}", self.image)));
                }
            }
            state = self
                .flight
                .finished
                .wait_timeout(state, CANCEL_POLL)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

/// A waiter dropped without `wait` leaves like a cancelled one, so it can't keep alive a pull
/// nobody waits for. It doesn't block for the outcome.
impl Drop for Waiter {
    fn drop(&mut self) {
        if !self.joined {
            return;
        }
        let mut state = lock(&self.flight.state);
        if state.outcome.is_none() {
            self.flight.leave(&mut state);
        }
    }
}

/// A copy of the shared outcome for one waiter; `DockerError` isn't `Clone`.
fn reissue(error: &DockerError) -> DockerError {
    match error {
        DockerError::ImagePullFailed {
            runtime,
            image,
            detail,
            reason,
        } => DockerError::ImagePullFailed {
            runtime,
            image: image.clone(),
            detail: detail.clone(),
            reason: *reason,
        },
        DockerError::Cancelled(what) => DockerError::Cancelled(what.clone()),
        DockerError::IoError(io) => {
            DockerError::IoError(std::io::Error::new(io.kind(), io.to_string()))
        }
        other => DockerError::IoError(std::io::Error::other(other.to_string())),
    }
}

/// The running pull's state, or the last one's outcome.
pub(crate) fn state(key: &PullKey) -> PullState {
    match lock(&FLIGHTS).get(key) {
        Some(Entry::Running(_)) => PullState::Running,
        Some(Entry::Finished(state)) => state.clone(),
        None => PullState::Idle,
    }
}

/// How many callers are waiting on `key`'s running pull; 0 when none is running.
#[cfg(test)]
pub(crate) fn waiters_for_test(key: &PullKey) -> usize {
    match lock(&FLIGHTS).get(key) {
        Some(Entry::Running(flight)) => lock(&flight.state).waiters,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    const DEADLINE: Duration = Duration::from_secs(10);

    /// Runs a blocking wait off the test thread so a hang fails the test.
    fn within<T: Send + 'static>(what: &str, wait: impl FnOnce() -> T + Send + 'static) -> T {
        let (done, result) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = done.send(wait());
        });
        result
            .recv_timeout(DEADLINE)
            .unwrap_or_else(|_| panic!("{what} did not finish within {DEADLINE:?}"))
    }

    fn key(runtime: &'static str, image: &str) -> PullKey {
        PullKey {
            runtime,
            image: image.to_string(),
        }
    }

    fn failed(reason: PullFailureReason) -> DockerError {
        DockerError::ImagePullFailed {
            runtime: "Docker",
            image: "img".into(),
            detail: "stderr".into(),
            reason,
        }
    }

    #[test]
    fn requesters_share_one_pull_and_its_outcome() {
        let shared = key("Docker", "shared");
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = |result: fn() -> Result<()>, release: Option<mpsc::Receiver<()>>| {
            let calls = calls.clone();
            move |_: &CancellationToken| {
                calls.fetch_add(1, Ordering::SeqCst);
                if let Some(release) = release {
                    release.recv().unwrap();
                }
                result()
            }
        };
        let (release, gate) = mpsc::channel();
        let first = join_with(
            shared.clone(),
            counted(|| Err(failed(PullFailureReason::Auth)), Some(gate)),
        );
        let second = join_with(shared.clone(), counted(|| Ok(()), None));
        assert_eq!(state(&shared), PullState::Running);
        release.send(()).unwrap();

        let never = CancellationToken::new();
        for waiter in [first, second] {
            let never = never.clone();
            assert!(matches!(
                within("a shared pull", move || waiter.wait(&never)),
                Err(DockerError::ImagePullFailed {
                    reason: PullFailureReason::Auth,
                    ..
                })
            ));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1, "one runtime process");
        assert!(matches!(
            state(&shared),
            PullState::Failed {
                reason: PullFailureReason::Auth,
                ..
            }
        ));

        let other_runtime = join_with(key("Podman", "shared"), counted(|| Ok(()), None));
        assert!(within("another runtime's pull", move || other_runtime.wait(&never)).is_ok());
        let retry = join_with(shared.clone(), counted(|| Ok(()), None));
        assert!(within("a retry", move || retry.wait(&CancellationToken::new())).is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 3, "per runtime, and a retry");
        assert_eq!(state(&shared), PullState::Done);
    }

    #[test]
    fn a_cancelled_waiter_detaches_and_the_last_one_kills_the_pull() {
        let killed = key("Docker", "killed");
        let (token_tx, token_rx) = mpsc::channel();
        let (dying_tx, dying_rx) = mpsc::channel();
        let (exit_tx, exit_rx) = mpsc::channel::<()>();
        // Sees the kill, then stays alive until the test lets it exit.
        let pull = move |kill: &CancellationToken| {
            token_tx.send(kill.clone()).unwrap();
            while !kill.is_cancelled() {
                std::thread::sleep(Duration::from_millis(5));
            }
            dying_tx.send(()).unwrap();
            exit_rx.recv().unwrap();
            Err(DockerError::Cancelled("pull".into()))
        };
        let first = join_with(killed.clone(), pull);
        let second = join_with(killed.clone(), |_: &CancellationToken| Ok(()));
        let kill = token_rx.recv_timeout(DEADLINE).expect("the pull starts");
        let cancelled = CancellationToken::new();
        cancelled.cancel();

        assert!(matches!(
            within("a detaching waiter", {
                let cancelled = cancelled.clone();
                move || first.wait(&cancelled)
            }),
            Err(DockerError::Cancelled(_))
        ));
        assert!(!kill.is_cancelled(), "another waiter still wants the pull");
        let (last_tx, last) = mpsc::channel();
        std::thread::spawn(move || last_tx.send(second.wait(&cancelled)));
        dying_rx
            .recv_timeout(DEADLINE)
            .expect("the last waiter kills the pull");

        let fresh = join_with(killed.clone(), |_: &CancellationToken| Ok(()));
        assert!(
            within("a newcomer's pull", move || fresh
                .wait(&CancellationToken::new()))
            .is_ok(),
            "a newcomer starts its own pull instead of joining a dying one"
        );
        // The pull is killed but has not exited, so the last waiter has nothing to return yet.
        assert!(
            last.recv_timeout(Duration::from_millis(200)).is_err(),
            "the last waiter waits for the kill"
        );
        exit_tx.send(()).unwrap();
        assert!(matches!(
            last.recv_timeout(DEADLINE).expect("the killed pull ends"),
            Err(DockerError::Cancelled(_))
        ));
        assert_eq!(
            state(&killed),
            PullState::Done,
            "the killed pull leaves the newcomer's outcome alone"
        );
    }

    #[test]
    fn a_waiter_dropped_without_waiting_leaves_the_last_cancel_to_kill() {
        let dropped = key("Docker", "dropped");
        let (token_tx, token_rx) = mpsc::channel();
        let first = join_with(dropped.clone(), move |kill: &CancellationToken| {
            token_tx.send(kill.clone()).unwrap();
            while !kill.is_cancelled() {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(DockerError::Cancelled("pull".into()))
        });
        let kill = token_rx.recv_timeout(DEADLINE).expect("the pull starts");
        drop(join_with(dropped.clone(), |_: &CancellationToken| Ok(())));
        let cancelled = CancellationToken::new();
        cancelled.cancel();

        assert!(matches!(
            within("the last waiter's cancel", move || first.wait(&cancelled)),
            Err(DockerError::Cancelled(_))
        ));
        assert!(
            kill.is_cancelled(),
            "the dropped waiter doesn't keep the pull alive"
        );
        assert_eq!(state(&dropped), PullState::Idle, "the killed pull ended");
    }

    #[test]
    fn a_panicking_pull_fails_its_waiters_and_frees_the_key() {
        let panicked = key("Docker", "panicked");
        let waiter = join_with(panicked.clone(), |_: &CancellationToken| -> Result<()> {
            panic!("runtime pull blew up")
        });
        assert!(matches!(
            within("a panicked pull's waiter", move || waiter
                .wait(&CancellationToken::new())),
            Err(DockerError::ImagePullFailed {
                reason: PullFailureReason::Other,
                ..
            })
        ));
        assert!(matches!(state(&panicked), PullState::Failed { .. }));

        let calls = Arc::new(AtomicUsize::new(0));
        let next = join_with(panicked.clone(), {
            let calls = calls.clone();
            move |_: &CancellationToken| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        });
        assert!(within("the next pull", move || next
            .wait(&CancellationToken::new()))
        .is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 1, "a fresh flight ran");
        assert_eq!(state(&panicked), PullState::Done);
    }

    #[test]
    fn each_stderr_fixture_reads_as_its_reason() {
        use PullFailureReason::*;
        for (fixture, text, reason) in [
            (
                "docker network",
                include_str!("../../tests/fixtures/pull-stderr/docker-29.5.2-network.txt"),
                Network,
            ),
            (
                "docker auth, Docker Hub",
                include_str!("../../tests/fixtures/pull-stderr/docker-29.5.2-auth-dockerhub.txt"),
                Auth,
            ),
            (
                "docker auth, ghcr.io",
                include_str!("../../tests/fixtures/pull-stderr/docker-29.5.2-auth-ghcr.txt"),
                Auth,
            ),
            (
                "disk space",
                include_str!("../../tests/fixtures/pull-stderr/source-quoted-disk-space.txt"),
                DiskSpace,
            ),
            (
                "docker other, tag not found",
                include_str!("../../tests/fixtures/pull-stderr/docker-29.5.2-other-not-found.txt"),
                Other,
            ),
            (
                // Synthetic: two real captures joined, so a failure carrying both phrase sets
                // reads as network, which is checked before auth.
                "synthetic: docker network, then auth ghcr.io",
                concat!(
                    include_str!("../../tests/fixtures/pull-stderr/docker-29.5.2-network.txt"),
                    include_str!("../../tests/fixtures/pull-stderr/docker-29.5.2-auth-ghcr.txt"),
                ),
                Network,
            ),
        ] {
            let stderr: Vec<&str> = text.lines().filter(|l| !l.starts_with('#')).collect();
            assert!(!stderr.is_empty(), "{fixture}");
            assert_eq!(
                PullFailureReason::classify(&stderr.join("\n")),
                reason,
                "{fixture}"
            );
        }
    }
}
