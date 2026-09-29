//! Live progress of in-flight `POST /api/sessions` creates, keyed by the
//! request's `idempotency_key` and polled by the web wizard.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::session::config::repo_config::HookProgress;

const MAX_OUTPUT_LINES: usize = 200;
const MAX_LINE_CHARS: usize = 400;
/// How long a failed create's response is replayed to a retry with its key. The
/// web client keeps retrying an unresolved create for as long (`pendingCreates.ts`).
const FAILURE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// Live failures remembered at once. A live one is never evicted, since a client may still
/// retry its key; at the cap, new keyed creates are refused until entries expire.
const MAX_FAILURES: usize = 4096;
const MAX_FAILURE_MESSAGE_CHARS: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CreateStage {
    Preparing,
    StartingContainer,
    RunningHooks,
    Starting,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreateProgressSnapshot {
    pub stage: CreateStage,
    /// The `on_create` command currently running.
    pub hook: Option<String>,
    /// Most recent hook output lines, oldest first.
    pub output: Vec<String>,
}

pub struct CreateProgress(Mutex<State>);

struct State {
    stage: CreateStage,
    hook: Option<String>,
    output: VecDeque<String>,
}

impl CreateProgress {
    fn new() -> Self {
        Self(Mutex::new(State {
            stage: CreateStage::Preparing,
            hook: None,
            output: VecDeque::new(),
        }))
    }

    pub fn set_stage(&self, stage: CreateStage) {
        self.0.lock().expect("create progress poisoned").stage = stage;
    }

    pub fn record(&self, progress: HookProgress) {
        let mut state = self.0.lock().expect("create progress poisoned");
        match progress {
            HookProgress::Started(cmd) => {
                state.stage = CreateStage::RunningHooks;
                state.hook = Some(cmd);
            }
            HookProgress::Output(line) => {
                if state.output.len() == MAX_OUTPUT_LINES {
                    state.output.pop_front();
                }
                state
                    .output
                    .push_back(line.chars().take(MAX_LINE_CHARS).collect());
            }
        }
    }

    pub fn snapshot(&self) -> CreateProgressSnapshot {
        let state = self.0.lock().expect("create progress poisoned");
        CreateProgressSnapshot {
            stage: state.stage,
            hook: state.hook.clone(),
            output: state.output.iter().cloned().collect(),
        }
    }
}

/// A failed create's response. A retry whose first response was lost gets it
/// back instead of running the create, and its hooks, again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateFailure {
    pub status: axum::http::StatusCode,
    pub code: &'static str,
    pub message: String,
}

#[derive(Default)]
pub struct CreateProgressRegistry {
    live: Arc<Mutex<HashMap<String, Arc<CreateProgress>>>>,
    failures: Mutex<HashMap<String, (Instant, CreateFailure)>>,
}

/// Removes its key from the registry when the create finishes.
pub struct CreateProgressRegistration {
    map: Arc<Mutex<HashMap<String, Arc<CreateProgress>>>>,
    key: String,
    pub progress: Arc<CreateProgress>,
}

impl Drop for CreateProgressRegistration {
    fn drop(&mut self) {
        let mut map = self.map.lock().expect("create progress registry poisoned");
        // A retry sharing the key may have replaced this entry; leave its entry alone.
        if map
            .get(&self.key)
            .is_some_and(|p| Arc::ptr_eq(p, &self.progress))
        {
            map.remove(&self.key);
        }
    }
}

impl CreateProgressRegistry {
    pub fn register(&self, key: &str) -> CreateProgressRegistration {
        let progress = Arc::new(CreateProgress::new());
        self.live
            .lock()
            .expect("create progress registry poisoned")
            .insert(key.to_string(), Arc::clone(&progress));
        CreateProgressRegistration {
            map: Arc::clone(&self.live),
            key: key.to_string(),
            progress,
        }
    }

    pub fn snapshot(&self, key: &str) -> Option<CreateProgressSnapshot> {
        self.live
            .lock()
            .expect("create progress registry poisoned")
            .get(key)
            .map(|p| p.snapshot())
    }

    /// Record before the create releases its idempotency lock, so a waiting retry sees it.
    /// The cap is soft: creates admitted by `has_failure_capacity` still record.
    pub fn record_failure(&self, key: &str, mut failure: CreateFailure) {
        failure.message = failure
            .message
            .chars()
            .take(MAX_FAILURE_MESSAGE_CHARS)
            .collect();
        let mut failures = self.failures.lock().expect("create failures poisoned");
        if failures.len() >= MAX_FAILURES {
            failures.retain(|_, (at, _)| at.elapsed() < FAILURE_TTL);
        }
        failures.insert(key.to_string(), (Instant::now(), failure));
    }

    /// Whether a new keyed create may run: false while the cap is full of live failures.
    pub fn has_failure_capacity(&self) -> bool {
        let mut failures = self.failures.lock().expect("create failures poisoned");
        if failures.len() >= MAX_FAILURES {
            failures.retain(|_, (at, _)| at.elapsed() < FAILURE_TTL);
        }
        failures.len() < MAX_FAILURES
    }

    pub fn recent_failure(&self, key: &str) -> Option<CreateFailure> {
        self.failures
            .lock()
            .expect("create failures poisoned")
            .get(key)
            .filter(|(at, _)| at.elapsed() < FAILURE_TTL)
            .map(|(_, failure)| failure.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_failure_map_keeps_every_live_failure_and_refuses_new_creates() {
        let registry = CreateProgressRegistry::default();
        let failure = |n: usize| CreateFailure {
            status: axum::http::StatusCode::BAD_REQUEST,
            code: "create_failed",
            message: format!("failure {n}"),
        };
        for n in 0..MAX_FAILURES {
            assert!(registry.has_failure_capacity());
            registry.record_failure(&format!("k{n}"), failure(n));
        }
        assert!(!registry.has_failure_capacity());
        // A create admitted before the cap filled still records, and nothing live is lost.
        registry.record_failure("late", failure(MAX_FAILURES));
        assert_eq!(registry.recent_failure("k0"), Some(failure(0)));
        assert_eq!(registry.recent_failure("late"), Some(failure(MAX_FAILURES)));

        registry.record_failure(
            "long",
            CreateFailure {
                message: "x".repeat(5000),
                ..failure(0)
            },
        );
        assert_eq!(
            registry.recent_failure("long").unwrap().message.len(),
            MAX_FAILURE_MESSAGE_CHARS
        );
    }

    #[test]
    fn progress_tracks_hooks_caps_output_and_unregisters_on_drop() {
        let registry = CreateProgressRegistry::default();
        let reg = registry.register("k");
        assert_eq!(
            registry.snapshot("k").unwrap().stage,
            CreateStage::Preparing
        );

        reg.progress
            .record(HookProgress::Started("npm install".into()));
        for i in 0..MAX_OUTPUT_LINES + 5 {
            reg.progress
                .record(HookProgress::Output(format!("line {i}")));
        }
        reg.progress
            .record(HookProgress::Output("x".repeat(MAX_LINE_CHARS * 2)));

        let snap = registry.snapshot("k").unwrap();
        assert_eq!(snap.stage, CreateStage::RunningHooks);
        assert_eq!(snap.hook.as_deref(), Some("npm install"));
        assert_eq!(snap.output.len(), MAX_OUTPUT_LINES);
        assert_eq!(snap.output[0], "line 6");
        assert_eq!(snap.output.last().unwrap().len(), MAX_LINE_CHARS);

        drop(reg);
        assert!(registry.snapshot("k").is_none());
    }
}
