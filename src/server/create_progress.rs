//! Live progress of in-flight `POST /api/sessions` creates, keyed by the
//! request's `idempotency_key` and polled by the web wizard.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use serde::Serialize;

use crate::session::config::repo_config::HookProgress;

const MAX_OUTPUT_LINES: usize = 200;
const MAX_LINE_CHARS: usize = 400;

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

#[derive(Default)]
pub struct CreateProgressRegistry(Arc<Mutex<HashMap<String, Arc<CreateProgress>>>>);

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
        self.0
            .lock()
            .expect("create progress registry poisoned")
            .insert(key.to_string(), Arc::clone(&progress));
        CreateProgressRegistration {
            map: Arc::clone(&self.0),
            key: key.to_string(),
            progress,
        }
    }

    pub fn snapshot(&self, key: &str) -> Option<CreateProgressSnapshot> {
        self.0
            .lock()
            .expect("create progress registry poisoned")
            .get(key)
            .map(|p| p.snapshot())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
