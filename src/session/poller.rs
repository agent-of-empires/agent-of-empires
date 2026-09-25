//! Adaptive polling interval and command channel for session monitoring

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, LazyLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Default ceiling on concurrent session-id poller threads.
pub const DEFAULT_SESSION_ID_POLLER_MAX_THREADS: u32 = 50;

/// A budget of session-id poller threads: how many are running and the ceiling they may not exceed.
#[derive(Debug)]
pub struct PollerBudget {
    active: AtomicU32,
    max: AtomicU32,
}

impl PollerBudget {
    const fn new(max: u32) -> Self {
        Self {
            active: AtomicU32::new(0),
            max: AtomicU32::new(max),
        }
    }

    /// Set the ceiling. 0 means "unset" and keeps the default, so an empty
    /// or zeroed config key can never freeze every session id.
    fn set_max(&self, max: u32) {
        let max = if max == 0 {
            DEFAULT_SESSION_ID_POLLER_MAX_THREADS
        } else {
            max
        };
        self.max.store(max, Ordering::SeqCst);
    }

    fn max(&self) -> u32 {
        self.max.load(Ordering::SeqCst)
    }

    fn active(&self) -> u32 {
        self.active.load(Ordering::SeqCst)
    }

    /// Atomically check the ceiling and take a slot. `None` at capacity.
    fn try_acquire(self: &Arc<Self>) -> Option<PollerCountGuard> {
        let mut current = self.active.load(Ordering::SeqCst);
        loop {
            if current >= self.max() {
                return None;
            }
            match self.active.compare_exchange_weak(
                current,
                current + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => {
                    return Some(PollerCountGuard {
                        budget: Arc::clone(self),
                    })
                }
                Err(actual) => current = actual,
            }
        }
    }
}

/// The process-wide budget.
static PROCESS_BUDGET: LazyLock<Arc<PollerBudget>> =
    LazyLock::new(|| Arc::new(PollerBudget::new(DEFAULT_SESSION_ID_POLLER_MAX_THREADS)));

/// The budget pollers created on this thread draw from: the process budget,
/// unless a test has pinned a private one.
fn current_budget() -> Arc<PollerBudget> {
    #[cfg(test)]
    if let Some(budget) = test_support::pinned_budget() {
        return budget;
    }
    Arc::clone(&PROCESS_BUDGET)
}

/// Apply the configured poller-thread ceiling for this process.
pub fn configure_session_id_poller_max_threads(max: u32) {
    current_budget().set_max(max);
}

/// The current poller-thread ceiling.
pub fn session_id_poller_max_threads() -> u32 {
    current_budget().max()
}

/// Resolve the global ceiling for a process launched under `profile`.
/// Applied once at startup; changes require a restart.
pub fn configured_session_id_poller_max_threads(profile: &str) -> u32 {
    crate::session::resolve_config_or_warn(profile)
        .session
        .session_id_poller_max_threads
}

/// `(active, max)` for the session-id poller thread budget.
pub fn session_id_poller_budget() -> (u32, u32) {
    let budget = current_budget();
    (budget.active(), budget.max())
}

/// First retry delay after a poller could not be (re)started.
const POLLER_REPAIR_INITIAL_DELAY: Duration = Duration::from_secs(5);
/// Longest retry delay; the schedule doubles up to this and then holds.
const POLLER_REPAIR_MAX_DELAY: Duration = Duration::from_secs(60);
/// At the capped delay, log a reminder every this many deferrals
/// (60 s × 10 = one line per ten minutes per session).
const POLLER_REPAIR_REMIND_EVERY: u32 = 10;

/// Retry schedule for one session whose session-id poller could not be (re)started — typically
/// because the process-wide thread budget is spent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PollerRepairBackoff {
    next_attempt: Option<Instant>,
    delay: Option<Duration>,
    deferrals: u32,
}

impl PollerRepairBackoff {
    /// True when a repair attempt may run at `now`.
    pub fn due(&self, now: Instant) -> bool {
        match self.next_attempt {
            None => true,
            Some(at) => now >= at,
        }
    }

    /// Record a failed (or skipped-for-budget) attempt at `now` and schedule the next one: 5 s,
    /// then doubling to a 60 s ceiling.
    pub fn defer(&mut self, now: Instant) -> Option<Duration> {
        let previous = self.delay;
        let delay = match previous {
            None => POLLER_REPAIR_INITIAL_DELAY,
            Some(d) => (d * 2).min(POLLER_REPAIR_MAX_DELAY),
        };
        self.delay = Some(delay);
        self.next_attempt = Some(now + delay);
        self.deferrals += 1;
        let escalated = previous != Some(delay);
        let reminder =
            delay == POLLER_REPAIR_MAX_DELAY && self.deferrals % POLLER_REPAIR_REMIND_EVERY == 0;
        (escalated || reminder).then_some(delay)
    }

    /// Clear the schedule after a successful start.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Number of consecutive deferrals since the last reset.
    pub fn deferrals(&self) -> u32 {
        self.deferrals
    }

    /// The delay scheduled by the most recent deferral, if any.
    pub fn current_delay(&self) -> Option<Duration> {
        self.delay
    }

    /// Make the next attempt due immediately without clearing the schedule
    /// (tests simulate elapsed time with this).
    #[cfg(test)]
    pub(crate) fn expire(&mut self) {
        self.next_attempt = self
            .next_attempt
            .map(|_| Instant::now() - Duration::from_millis(1));
    }
}

/// RAII guard that returns its slot to the budget on drop.
struct PollerCountGuard {
    budget: Arc<PollerBudget>,
}

impl Drop for PollerCountGuard {
    fn drop(&mut self) {
        self.budget.active.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Whether a poller could be spawned right now.
pub(crate) fn session_id_poller_budget_available() -> bool {
    let budget = current_budget();
    budget.active() < budget.max()
}

const POLL_INITIAL_INTERVAL: Duration = Duration::from_secs(2);
const POLL_MAX_INTERVAL: Duration = Duration::from_secs(60);
const POLL_BACKOFF_FACTOR: f64 = 1.5;
const POLL_STABLE_THRESHOLD: u32 = 3;

/// Outcome of [`SessionPoller::start`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollerSpawn {
    /// The polling thread is running.
    Spawned,
    /// This poller already owns a thread; the duplicate start was ignored.
    AlreadyStarted,
    /// The process-wide poller-thread budget is spent; nothing was spawned.
    BudgetExhausted,
    /// The OS refused to create the thread.
    SpawnFailed,
}

/// Manages adaptive polling intervals that back off when no changes are detected
#[derive(Debug)]
struct AdaptiveInterval {
    initial: Duration,
    current: Duration,
    max: Duration,
    backoff_factor: f64,
    stable_threshold: u32,
    stable_count: u32,
}

impl AdaptiveInterval {
    /// Create a new adaptive interval with custom parameters
    fn new(initial: Duration, max: Duration, backoff_factor: f64, stable_threshold: u32) -> Self {
        Self {
            initial,
            current: initial,
            max,
            backoff_factor,
            stable_threshold,
            stable_count: 0,
        }
    }

    fn current(&self) -> Duration {
        self.current
    }

    /// Record that no changes were detected; increases backoff if threshold is reached.
    fn record_no_change(&mut self) {
        self.stable_count += 1;
        if self.stable_count >= self.stable_threshold {
            let next_secs = self.current.as_secs_f64() * self.backoff_factor;
            let next_duration = Duration::from_secs_f64(next_secs);
            self.current = next_duration.min(self.max);
            self.stable_count = 0;
        }
    }

    /// Record that a change was detected; reset to initial interval
    fn record_change(&mut self) {
        self.current = self.initial;
        self.stable_count = 0;
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SessionIdGuard {
    Unguarded,
    /// Pre-launch-marker compat path: OMP sessions captured before the launch marker generation
    /// existed carry no generation to CAS against, so they persist unguarded.
    OmpLegacy,
    OmpGeneration(String),
    /// Read from a per-instance sidecar the agent itself wrote (Pi's extension), so the observation
    /// names this pane rather than being inferred from a store. The transcript path published
    /// beside the id is part of it, so a path written after the id is reported too.
    InstanceSidecar {
        transcript: Option<String>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SessionIdObservation {
    pub(crate) sid: String,
    pub(crate) guard: SessionIdGuard,
    pub(crate) execution: Option<crate::session::instance::ActiveExecution>,
    pub(crate) source: Option<crate::session::ExecutionBinding>,
    pub(crate) transcript_path: Option<std::path::PathBuf>,
    pub(crate) pi_session_path: Option<String>,
}

pub(crate) type SessionIdPollFn =
    Box<dyn Fn(&str) -> Option<SessionIdObservation> + Send + 'static>;

impl SessionIdObservation {
    pub(crate) fn unguarded(sid: String) -> Self {
        Self {
            sid,
            guard: SessionIdGuard::Unguarded,
            execution: None,
            source: None,
            transcript_path: None,
            pi_session_path: None,
        }
    }

    pub(crate) fn instance_sidecar(sid: String, transcript: Option<String>) -> Self {
        Self {
            sid,
            guard: SessionIdGuard::InstanceSidecar { transcript },
            execution: None,
            source: None,
            transcript_path: None,
            pi_session_path: None,
        }
    }

    pub(crate) fn omp_legacy(sid: String) -> Self {
        Self {
            sid,
            guard: SessionIdGuard::OmpLegacy,
            execution: None,
            source: None,
            transcript_path: None,
            pi_session_path: None,
        }
    }
    pub(crate) fn omp(sid: String, generation: String) -> Self {
        Self {
            sid,
            guard: SessionIdGuard::OmpGeneration(generation),
            execution: None,
            source: None,
            transcript_path: None,
            pi_session_path: None,
        }
    }
    pub(crate) fn conversation_binding(&self) -> Option<crate::session::ConversationBinding> {
        self.execution.as_ref()?;
        self.source
            .as_ref()
            .map(|source| crate::session::ConversationBinding {
                session_id: self.sid.clone(),
                execution: Some(source.clone()),
                provenance: crate::session::ConversationProvenance::Observed,
                transcript_path: self.transcript_path.clone(),
            })
    }
    pub(crate) fn confirms_omp_pin(&self, intent: &crate::session::ResumeIntent) -> bool {
        self.execution.is_some()
            && self.source.is_some()
            && matches!(&self.guard, SessionIdGuard::OmpGeneration(_))
            && matches!(intent, crate::session::ResumeIntent::Use(pinned) if pinned == &self.sid)
    }

    pub(crate) fn conversation_key(&self) -> Option<crate::session::instance::ConversationKey<'_>> {
        self.source.as_ref().map(|source| source.key(&self.sid))
    }
}

/// Command sent to the session poller thread
#[derive(Debug, Clone, Copy)]
enum PollCommand {
    /// Stop the poller thread.
    Stop,
    /// Forget the last report so an observation whose durable write failed or
    /// lost a CAS can be emitted again.
    RetryLast,
}

/// How long a poller tolerates a target tmux cannot resolve before treating it as gone, when it has
/// never seen the pane alive.
const MISSING_TARGET_GRACE: Duration = Duration::from_secs(60);

/// Tracks whether a poll target has become terminal for the poller.
struct TargetLiveness {
    seen_alive: bool,
    started_at: Instant,
    gone_candidate: Option<String>,
}

impl TargetLiveness {
    fn new(now: Instant) -> Self {
        Self {
            seen_alive: false,
            started_at: now,
            gone_candidate: None,
        }
    }

    /// Fold one probe in, returning `(target_is_gone, should_stop)`.
    fn record(
        &mut self,
        target: &str,
        probe: crate::tmux::utils::PaneProbe,
        now: Instant,
    ) -> (bool, bool) {
        use crate::tmux::utils::PaneProbe;
        let gone = match probe {
            PaneProbe::Alive => {
                self.seen_alive = true;
                false
            }
            PaneProbe::Dead => true,
            PaneProbe::Missing => {
                self.seen_alive
                    || now.saturating_duration_since(self.started_at) >= MISSING_TARGET_GRACE
            }
            // tmux itself could not be run, which says nothing about the pane.
            PaneProbe::Unknown => false,
        };
        if !gone {
            self.gone_candidate = None;
            return (false, false);
        }
        let should_stop = self.gone_candidate.as_deref() == Some(target);
        if !should_stop {
            self.gone_candidate = Some(target.to_string());
        }
        (true, should_stop)
    }
}

/// Resolve and observe one poll target, returning `(target, should_stop, observation)`.
fn poll_resolved_target<T>(
    instance_id: &str,
    initial_session_name: &str,
    resolve_target: impl FnOnce(&str, &str) -> String,
    probe_target: impl FnOnce(&str) -> crate::tmux::utils::PaneProbe,
    observe: impl FnOnce(&str) -> Option<T>,
    liveness: &mut TargetLiveness,
    now: Instant,
) -> (String, bool, Option<T>) {
    let target = resolve_target(instance_id, initial_session_name);
    let (gone, should_stop) = liveness.record(&target, probe_target(&target), now);
    if gone {
        return (target, should_stop, None);
    }

    let observation = observe(&target);
    (target, false, observation)
}

/// Manages polling thread lifecycle and inter-thread communication via mpsc channels.
pub struct SessionPoller {
    session_name: String,
    /// The budget this poller's thread is counted against, fixed at
    /// construction so the slot is returned to the budget it was taken from.
    budget: Arc<PollerBudget>,
    cmd_tx: mpsc::Sender<PollCommand>,
    cmd_rx: Option<mpsc::Receiver<PollCommand>>,
    result_tx: mpsc::Sender<(String, SessionIdObservation)>,
    result_rx: Option<mpsc::Receiver<(String, SessionIdObservation)>>,
    pending_observation: Option<(String, SessionIdObservation)>,
    handle: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for SessionPoller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionPoller")
            .field("session_name", &self.session_name)
            .field("running", &self.handle.is_some())
            .finish()
    }
}

impl SessionPoller {
    /// Create a new poller (does not start the thread)
    pub fn new(session_name: String) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        Self {
            session_name,
            budget: current_budget(),
            cmd_tx,
            cmd_rx: Some(cmd_rx),
            result_tx,
            result_rx: Some(result_rx),
            pending_observation: None,
            handle: None,
        }
    }

    /// Start the polling thread with the given callbacks.
    pub fn start(
        &mut self,
        instance_id: String,
        poll_fn: Box<dyn Fn() -> Option<String> + Send + 'static>,
        on_change: Box<dyn Fn(&str) + Send + 'static>,
        initial_known: Option<String>,
    ) -> PollerSpawn {
        self.start_observations(
            instance_id,
            Box::new(move |_| poll_fn().map(SessionIdObservation::unguarded)),
            on_change,
            initial_known.map(SessionIdObservation::unguarded),
        )
    }

    pub(crate) fn start_observations(
        &mut self,
        instance_id: String,
        poll_fn: SessionIdPollFn,
        on_change: Box<dyn Fn(&str) + Send + 'static>,
        initial_known: Option<SessionIdObservation>,
    ) -> PollerSpawn {
        let cmd_rx = match self.cmd_rx.take() {
            Some(rx) => rx,
            None => {
                tracing::warn!(target: "session.create",
                    "Poller for {} already started, ignoring duplicate start",
                    instance_id
                );
                return PollerSpawn::AlreadyStarted;
            }
        };

        let _guard = match self.budget.try_acquire() {
            Some(g) => g,
            None => {
                // The caller's repair schedule owns the warning and throttles
                // it; warning here too would fire on every deferred attempt.
                let (active, max) = (self.budget.active(), self.budget.max());
                tracing::debug!(target: "session.create",
                    "Session-id poller budget exhausted ({}/{}), skipping poller for {}",
                    active,
                    max,
                    instance_id,
                );
                self.cmd_rx = Some(cmd_rx);
                return PollerSpawn::BudgetExhausted;
            }
        };

        let initial_session_name = self.session_name.clone();
        let thread_label = format!("aoe-poller/{}", instance_id);
        let result_tx = self.result_tx.clone();

        let handle = std::thread::Builder::new()
            .name(thread_label.clone())
            .stack_size(128 * 1024)
            .spawn(move || {
                // Rebind so the closure captures `_guard` and the counter only decrements when the
                // thread exits (including via panic).
                let _guard = _guard;

                let mut last_known = initial_known;
                let mut interval = AdaptiveInterval::new(
                    POLL_INITIAL_INTERVAL,
                    POLL_MAX_INTERVAL,
                    POLL_BACKOFF_FACTOR,
                    POLL_STABLE_THRESHOLD,
                );

                let report = |new_observation: Option<SessionIdObservation>,
                              last: &mut Option<SessionIdObservation>,
                              interval: &mut AdaptiveInterval| {
                    match new_observation {
                        Some(observation) if last.as_ref() != Some(&observation) => {
                            on_change(&observation.sid);
                            let _ =
                                result_tx.send((instance_id.clone(), observation.clone()));
                            *last = Some(observation);
                            interval.record_change();
                        }
                        _ => interval.record_no_change(),
                    }
                };

                let mut liveness = TargetLiveness::new(Instant::now());
                let mut poll_tick = || {
                    poll_resolved_target(
                        &instance_id,
                        &initial_session_name,
                        crate::tmux::live_agent_session_name,
                        crate::tmux::utils::probe_pane,
                        |target| poll_fn(target),
                        &mut liveness,
                        Instant::now(),
                    )
                };

                // Immediate first poll (e.g. pre-existing sessions loaded from disk).
                let (target, should_stop, observation) = poll_tick();
                if should_stop {
                    tracing::info!(target: "session.create", "Pane gone or dead for {}, stopping poller", target);
                    return;
                }
                report(observation, &mut last_known, &mut interval);
                loop {
                    match cmd_rx.recv_timeout(interval.current()) {
                        Ok(PollCommand::Stop) => {
                            // Capture the pane's final observable state before the owner joins and
                            // drains this poller.
                            let (_, should_stop, observation) = poll_tick();
                            if !should_stop {
                                report(observation, &mut last_known, &mut interval);
                            }
                            break;
                        }
                        Ok(PollCommand::RetryLast) => {
                            last_known = None;
                            interval.record_change();
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => break,
                    }

                    let (target, should_stop, observation) = poll_tick();
                    if should_stop {
                        tracing::info!(target: "session.create", "Pane gone or dead for {}, stopping poller", target);
                        break;
                    }

                    report(observation, &mut last_known, &mut interval);
                }
            });

        match handle {
            Ok(h) => {
                self.handle = Some(h);
                PollerSpawn::Spawned
            }
            Err(e) => {
                tracing::warn!(target: "session.create", "Failed to spawn poller thread {}: {}", thread_label, e);
                // Restore channels to allow retrying spawn
                let (cmd_tx, cmd_rx) = mpsc::channel();
                self.cmd_tx = cmd_tx;
                self.cmd_rx = Some(cmd_rx);
                let (result_tx, result_rx) = mpsc::channel();
                self.result_tx = result_tx;
                self.result_rx = Some(result_rx);
                self.pending_observation = None;
                PollerSpawn::SpawnFailed
            }
        }
    }

    pub(crate) fn try_recv_observation(&self) -> Option<(String, SessionIdObservation)> {
        self.result_rx.as_ref()?.try_recv().ok()
    }

    /// Drain newly queued observations into a sticky one-slot mailbox and lease the newest value
    /// without consuming it.
    pub(crate) fn latest_observation(&mut self) -> Option<(String, SessionIdObservation)> {
        while let Some(observation) = self.try_recv_observation() {
            self.pending_observation = Some(observation);
        }
        self.pending_observation.clone()
    }

    /// Drain newly queued observations into the sticky mailbox, then test the pending one without
    /// cloning it. The predicate sees only the newest observation.
    pub(crate) fn pending_observation_matches(
        &mut self,
        predicate: impl FnOnce(&SessionIdObservation) -> bool,
    ) -> bool {
        while let Some(observation) = self.try_recv_observation() {
            self.pending_observation = Some(observation);
        }
        self.pending_observation
            .as_ref()
            .is_some_and(|(_, observation)| predicate(observation))
    }

    /// Acknowledge only the observation that reached a terminal outcome. A
    /// stale writer must not erase a newer correction queued in the meantime.
    pub(crate) fn acknowledge_observation(
        &mut self,
        expected: &(String, SessionIdObservation),
    ) -> bool {
        if self.pending_observation.as_ref() == Some(expected) {
            self.pending_observation = None;
            true
        } else {
            false
        }
    }

    #[cfg(any(test, debug_assertions))]
    pub fn inject_test_update(&self, instance_id: &str, session_id: &str) {
        self.result_tx
            .send((
                instance_id.to_string(),
                SessionIdObservation::unguarded(session_id.to_string()),
            ))
            .expect("inject_test_update: result channel disconnected");
    }

    #[cfg(test)]
    pub(crate) fn inject_test_sidecar_update(
        &self,
        instance_id: &str,
        session_id: &str,
        transcript: Option<&str>,
    ) {
        self.result_tx
            .send((
                instance_id.to_string(),
                SessionIdObservation::instance_sidecar(
                    session_id.to_string(),
                    transcript.map(str::to_owned),
                ),
            ))
            .expect("inject_test_sidecar_update: result channel disconnected");
    }

    #[cfg(test)]
    pub(crate) fn inject_test_observation(
        &self,
        instance_id: &str,
        observation: SessionIdObservation,
    ) {
        self.result_tx
            .send((instance_id.to_owned(), observation))
            .expect("inject_test_observation: result channel disconnected");
    }

    #[cfg(test)]
    pub(crate) fn inject_test_omp_legacy_update(&self, instance_id: &str, session_id: &str) {
        self.result_tx
            .send((
                instance_id.to_string(),
                SessionIdObservation::omp_legacy(session_id.to_string()),
            ))
            .expect("inject_test_omp_legacy_update: result channel disconnected");
    }

    pub(crate) fn retry_last_observation(&self) {
        let _ = self.cmd_tx.send(PollCommand::RetryLast);
    }

    /// Stop the poller thread and wait for it to finish
    pub fn stop(&mut self) {
        let _ = self.cmd_tx.send(PollCommand::Stop);
        if let Some(handle) = self.handle.take() {
            if let Err(e) = handle.join() {
                tracing::warn!(target: "session.create", "Poller thread panicked: {:?}", e);
            }
        }
    }

    /// Check if the poller thread is running
    pub fn is_running(&self) -> bool {
        match &self.handle {
            Some(handle) => !handle.is_finished(),
            None => false,
        }
    }
}

impl Default for SessionPoller {
    fn default() -> Self {
        Self::new("default".to_string())
    }
}

/// Test-only budget isolation.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::cell::RefCell;

    thread_local! {
        static PINNED: RefCell<Option<Arc<PollerBudget>>> = const { RefCell::new(None) };
    }

    pub(super) fn pinned_budget() -> Option<Arc<PollerBudget>> {
        PINNED.with(|slot| slot.borrow().clone())
    }

    /// A private budget for the current thread, unpinned on drop.
    pub(crate) struct IsolatedBudget {
        budget: Arc<PollerBudget>,
    }

    impl IsolatedBudget {
        /// Pin a fresh budget (no active pollers) with the given ceiling.
        pub(crate) fn with_ceiling(max: u32) -> Self {
            let budget = Arc::new(PollerBudget::new(max));
            PINNED.with(|slot| {
                assert!(
                    slot.borrow().is_none(),
                    "a budget is already pinned on this thread"
                );
                *slot.borrow_mut() = Some(Arc::clone(&budget));
            });
            Self { budget }
        }

        /// Pin a budget that is already spent (ceiling 1, one slot taken).
        pub(crate) fn exhausted() -> Self {
            let pinned = Self::with_ceiling(1);
            pinned.set_active(1);
            pinned
        }

        pub(crate) fn active(&self) -> u32 {
            self.budget.active()
        }

        pub(crate) fn exhausted_now(&self) -> bool {
            self.budget.active() >= self.budget.max()
        }

        /// Pretend `n` pollers are running (slots no guard will return).
        pub(crate) fn set_active(&self, n: u32) {
            self.budget.active.store(n, Ordering::SeqCst);
        }

        pub(super) fn try_acquire(&self) -> Option<PollerCountGuard> {
            self.budget.try_acquire()
        }
    }

    impl Drop for IsolatedBudget {
        fn drop(&mut self) {
            PINNED.with(|slot| *slot.borrow_mut() = None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::sync::{Arc, Mutex, MutexGuard};

    fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn configured_ceiling_bounds_the_budget() {
        let budget =
            test_support::IsolatedBudget::with_ceiling(DEFAULT_SESSION_ID_POLLER_MAX_THREADS);

        configure_session_id_poller_max_threads(3);
        assert_eq!(session_id_poller_max_threads(), 3);
        budget.set_active(2);
        assert!(!budget.exhausted_now());
        assert_eq!(session_id_poller_budget(), (2, 3));
        let guard = budget.try_acquire();
        assert!(
            guard.is_some(),
            "third slot is within the configured ceiling"
        );
        assert!(budget.exhausted_now());
        assert!(
            budget.try_acquire().is_none(),
            "fourth slot exceeds the configured ceiling"
        );
        drop(guard);
        assert!(!budget.exhausted_now());

        budget.set_active(50);
        configure_session_id_poller_max_threads(400);
        assert!(!budget.exhausted_now());
        assert!(budget.try_acquire().is_some());

        configure_session_id_poller_max_threads(0);
        assert_eq!(
            session_id_poller_max_threads(),
            DEFAULT_SESSION_ID_POLLER_MAX_THREADS,
            "zero keeps the default"
        );
    }

    #[test]
    #[serial]
    fn configured_ceiling_ignores_profile_overrides() {
        let home = tempfile::tempdir().unwrap();
        let _app_dir = crate::session::test_support::isolate_app_dir_at(home.path());
        let app = crate::session::get_app_dir().unwrap();
        std::fs::write(
            app.join("config.toml"),
            "[session]\nsession_id_poller_max_threads = 7\n",
        )
        .unwrap();
        let tuned = app.join("profiles").join("tuned");
        std::fs::create_dir_all(&tuned).unwrap();
        std::fs::write(
            tuned.join("config.toml"),
            "[session]\nsession_id_poller_max_threads = 9\n",
        )
        .unwrap();

        assert_eq!(configured_session_id_poller_max_threads("tuned"), 7);
        assert_eq!(
            configured_session_id_poller_max_threads("untouched"),
            7,
            "a profile without an override inherits the global value"
        );
    }

    #[test]
    fn isolated_budget_does_not_share_slots_with_the_process_budget() {
        let budget = test_support::IsolatedBudget::with_ceiling(1);
        assert_eq!(session_id_poller_budget(), (0, 1));

        let mut first = SessionPoller::new("iso-a".to_string());
        assert_eq!(
            first.start(
                "iso-a".to_string(),
                Box::new(|| Some("id".to_string())),
                Box::new(|_| {}),
                None,
            ),
            PollerSpawn::Spawned
        );
        assert_eq!(budget.active(), 1);
        assert_eq!(session_id_poller_budget(), (1, 1));

        let mut second = SessionPoller::new("iso-b".to_string());
        assert_eq!(
            second.start(
                "iso-b".to_string(),
                Box::new(|| Some("id".to_string())),
                Box::new(|_| {}),
                None,
            ),
            PollerSpawn::BudgetExhausted,
            "the isolated ceiling is the one this thread's pollers see"
        );

        let elsewhere = std::thread::spawn(|| {
            let mut poller = SessionPoller::new("process".to_string());
            let outcome = poller.start(
                "process".to_string(),
                Box::new(|| Some("id".to_string())),
                Box::new(|_| {}),
                None,
            );
            poller.stop();
            outcome
        })
        .join()
        .unwrap();
        assert_eq!(elsewhere, PollerSpawn::Spawned);

        first.stop();
        assert_eq!(budget.active(), 0, "the guard returns the isolated slot");
    }

    #[test]
    fn repair_backoff_doubles_to_a_minute_reminds_at_the_cap_and_resets() {
        let mut b = PollerRepairBackoff::default();
        let now = Instant::now();
        assert!(b.due(now), "a fresh schedule is due immediately");

        let expected = [5u64, 10, 20, 40, 60, 60, 60];
        for (i, secs) in expected.iter().enumerate() {
            let logged = b.defer(now);
            assert_eq!(
                b.current_delay(),
                Some(Duration::from_secs(*secs)),
                "deferral {} should schedule {}s",
                i + 1,
                secs
            );
            assert_eq!(b.deferrals(), i as u32 + 1);
            let should_log = i < 5; // first + four escalations; then quiet
            assert_eq!(
                logged.is_some(),
                should_log,
                "deferral {} log decision",
                i + 1
            );
            assert!(!b.due(now), "just deferred: not due at the same instant");
            assert!(
                b.due(now + Duration::from_secs(*secs)),
                "due once the scheduled delay has elapsed"
            );
            assert!(
                !b.due(now + Duration::from_secs(*secs) - Duration::from_millis(1)),
                "not due one millisecond early"
            );
        }

        let mut logged_at = Vec::new();
        for _ in 0..23 {
            if b.defer(now).is_some() {
                logged_at.push(b.deferrals());
            }
        }
        assert_eq!(
            logged_at,
            vec![10, 20, 30],
            "reminds every tenth at the cap"
        );

        b.reset();
        assert_eq!(b, PollerRepairBackoff::default());
        assert!(b.due(now));
        assert_eq!(b.defer(now), Some(Duration::from_secs(5)), "restarts at 5s");
    }

    #[test]
    fn adaptive_interval_backs_off_to_the_cap_and_resets_on_change() {
        let mut interval = AdaptiveInterval::new(
            POLL_INITIAL_INTERVAL,
            POLL_MAX_INTERVAL,
            POLL_BACKOFF_FACTOR,
            POLL_STABLE_THRESHOLD,
        );
        assert_eq!(interval.current(), Duration::from_secs(2));
        for _ in 1..POLL_STABLE_THRESHOLD {
            interval.record_no_change();
        }
        assert_eq!(
            interval.current(),
            Duration::from_secs(2),
            "below the threshold"
        );
        interval.record_no_change();
        assert_eq!(
            (interval.current(), interval.stable_count),
            (Duration::from_secs(3), 0)
        );
        for _ in 0..POLL_STABLE_THRESHOLD {
            interval.record_no_change();
        }
        assert_eq!(interval.current(), Duration::from_secs_f64(4.5));

        interval.record_no_change();
        interval.record_change();
        assert_eq!(
            (interval.current(), interval.stable_count),
            (Duration::from_secs(2), 0)
        );

        for _ in 0..1000 {
            interval.record_no_change();
            assert!(interval.current() <= POLL_MAX_INTERVAL);
        }
        assert_eq!(interval.current(), POLL_MAX_INTERVAL);
    }

    #[test]
    fn rename_between_resolution_and_liveness_retries_the_new_name() {
        use crate::tmux::utils::PaneProbe;
        let initial = "aoe_Old_12345678";
        let renamed = "aoe_New_12345678";
        let current = std::cell::RefCell::new(initial.to_string());
        let resolution_count = std::cell::Cell::new(0);
        let observed = std::cell::RefCell::new(Vec::new());
        let now = Instant::now();
        let mut liveness = TargetLiveness::new(now);

        let (target, should_stop, observation) = poll_resolved_target(
            "12345678abcdef",
            initial,
            |_, derived| {
                assert_eq!(derived, initial);
                resolution_count.set(resolution_count.get() + 1);
                current.borrow().clone()
            },
            |target| {
                assert_eq!(target, initial);
                *current.borrow_mut() = renamed.to_string();
                PaneProbe::Dead
            },
            |_| -> Option<&str> {
                panic!("a name that became dead during the tick must not be observed")
            },
            &mut liveness,
            now,
        );
        assert_eq!(target, initial);
        assert!(!should_stop, "one dead tick may be an in-flight rename");
        assert!(observation.is_none());

        let (target, should_stop, observation) = poll_resolved_target(
            "12345678abcdef",
            initial,
            |_, _| {
                resolution_count.set(resolution_count.get() + 1);
                current.borrow().clone()
            },
            |_| PaneProbe::Alive,
            |target| {
                observed.borrow_mut().push(target.to_string());
                Some("sid-after-rename")
            },
            &mut liveness,
            now,
        );
        assert_eq!(target, renamed);
        assert!(!should_stop);
        assert_eq!(observation, Some("sid-after-rename"));
        assert_eq!(observed.into_inner(), vec![renamed.to_string()]);
        assert_eq!(resolution_count.get(), 2, "one resolution per tick");
        assert!(liveness.gone_candidate.is_none());
    }

    #[test]
    fn missing_target_is_terminal_only_after_the_pane_was_seen_or_the_grace_passed() {
        use crate::tmux::utils::PaneProbe;
        let name = "aoe_Gone_12345678";
        let t0 = Instant::now();
        let after_grace = t0 + MISSING_TARGET_GRACE;

        let mut fresh = TargetLiveness::new(t0);
        assert_eq!(fresh.record(name, PaneProbe::Missing, t0), (false, false));

        let mut stale = TargetLiveness::new(t0);
        assert_eq!(
            stale.record(name, PaneProbe::Missing, after_grace),
            (true, false)
        );
        assert_eq!(
            stale.record(name, PaneProbe::Missing, after_grace),
            (true, true)
        );

        let mut killed = TargetLiveness::new(t0);
        assert_eq!(killed.record(name, PaneProbe::Alive, t0), (false, false));
        assert_eq!(killed.record(name, PaneProbe::Missing, t0), (true, false));
        assert_eq!(killed.record(name, PaneProbe::Missing, t0), (true, true));

        let mut unknown = TargetLiveness::new(t0);
        assert_eq!(unknown.record(name, PaneProbe::Alive, t0), (false, false));
        assert_eq!(
            unknown.record(name, PaneProbe::Unknown, after_grace),
            (false, false)
        );

        let mut flaky = TargetLiveness::new(t0);
        assert_eq!(flaky.record(name, PaneProbe::Alive, t0), (false, false));
        assert_eq!(flaky.record(name, PaneProbe::Missing, t0), (true, false));
        assert_eq!(flaky.record(name, PaneProbe::Alive, t0), (false, false));
        assert_eq!(flaky.record(name, PaneProbe::Missing, t0), (true, false));
    }

    #[test]
    #[serial]
    fn test_poller_detects_change() {
        let call_count = Arc::new(Mutex::new(0u32));
        let call_count_clone = call_count.clone();
        let poll_fn: Box<dyn Fn() -> Option<String> + Send + 'static> = Box::new(move || {
            let mut count = lock_unpoisoned(&call_count_clone);
            *count += 1;
            Some(if *count == 1 { "id-1" } else { "id-2" }.to_string())
        });
        let changed_ids: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let changed_ids_clone = changed_ids.clone();
        let on_change: Box<dyn Fn(&str) + Send + 'static> = Box::new(move |id: &str| {
            lock_unpoisoned(&changed_ids_clone).push(id.to_string());
        });

        let mut poller = SessionPoller::new("test-session".to_string());
        assert_eq!(
            poller.start(
                "test-change".to_string(),
                poll_fn,
                on_change,
                Some("id-1".to_string()),
            ),
            PollerSpawn::Spawned
        );
        // Commands queue behind the immediate first poll, and each retry forces a tick, so no
        // interval elapses: poll 1 sees the known id-1, polls 2 and 3 see id-2.
        poller.retry_last_observation();
        poller.retry_last_observation();
        poller.stop();

        assert_eq!(
            *lock_unpoisoned(&changed_ids),
            ["id-2", "id-2"],
            "the known id is suppressed and a retry re-emits the same observation"
        );
    }

    #[test]
    fn test_thread_budget_cap() {
        let budget = test_support::IsolatedBudget::exhausted();

        let mut poller = SessionPoller::new("test-session".to_string());
        let outcome = poller.start(
            "test-budget".to_string(),
            Box::new(|| Some("id".to_string())),
            Box::new(|_| {}),
            None,
        );

        assert_eq!(outcome, PollerSpawn::BudgetExhausted);
        assert!(
            !poller.is_running(),
            "poller should not have spawned when budget exhausted"
        );
        assert!(
            poller.cmd_rx.is_some(),
            "cmd_rx should be returned when budget exhausted"
        );
        assert!(
            !session_id_poller_budget_available(),
            "pre-check must report the exhausted budget the acquire rejected"
        );

        budget.set_active(0);
        assert!(
            session_id_poller_budget_available(),
            "one free slot must read as available"
        );
    }

    #[test]
    fn test_budget_exhaustion_leaves_the_warning_to_the_repair_path() {
        let logs = crate::session::test_support::LogCapture::start();
        let _budget = test_support::IsolatedBudget::exhausted();

        let mut poller = SessionPoller::new("test-session".to_string());
        let outcome = poller.start(
            "test-budget-quiet".to_string(),
            Box::new(|| Some("id".to_string())),
            Box::new(|_| {}),
            None,
        );
        assert_eq!(outcome, PollerSpawn::BudgetExhausted);

        let logs = logs.contents();
        let lines = || logs.lines().filter(|l| l.contains("test-budget-quiet"));
        assert_eq!(lines().filter(|l| l.contains("DEBUG")).count(), 1, "{logs}");
        assert_eq!(
            lines().filter(|l| l.contains("WARN")).count(),
            0,
            "start warned on an exhausted budget: {logs}"
        );
    }

    #[test]
    fn test_duplicate_start_is_reported_not_spawned() {
        let _budget = test_support::IsolatedBudget::with_ceiling(1);
        let mut poller = SessionPoller::new("test-session".to_string());
        assert_eq!(
            poller.start(
                "test-dup".to_string(),
                Box::new(|| Some("id".to_string())),
                Box::new(|_| {}),
                None,
            ),
            PollerSpawn::Spawned
        );
        assert_eq!(
            poller.start(
                "test-dup".to_string(),
                Box::new(|| Some("id".to_string())),
                Box::new(|_| {}),
                None,
            ),
            PollerSpawn::AlreadyStarted,
            "a second start on a live poller is ignored, not a spawn failure"
        );
        assert!(poller.is_running());
        poller.stop();
    }

    #[test]
    fn test_poller_cleanup_decrements_counter() {
        let budget = test_support::IsolatedBudget::with_ceiling(1);
        let sid = Arc::new(Mutex::new("initial-id".to_string()));
        let observed_sid = sid.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let mut poller = SessionPoller::new("test-session".to_string());
        poller.cmd_tx.send(PollCommand::Stop).expect("queue stop");
        assert_eq!(
            poller.start(
                "test-cleanup".to_string(),
                Box::new(move || Some(lock_unpoisoned(&observed_sid).clone())),
                Box::new(move |value| {
                    if value == "initial-id" {
                        started_tx.send(()).expect("report initial observation");
                        let _ = release_rx.recv();
                    }
                }),
                None,
            ),
            PollerSpawn::Spawned
        );
        started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("initial observation");
        let active_before_stop = budget.active();
        *lock_unpoisoned(&sid) = "final-id".to_string();
        drop(release_tx);
        poller.stop();
        assert_eq!(active_before_stop, 1);
        assert_eq!(budget.active(), 0);
        assert_eq!(
            poller.latest_observation(),
            Some((
                "test-cleanup".to_string(),
                SessionIdObservation::unguarded("final-id".to_string()),
            ))
        );
    }

    #[test]
    #[serial]
    fn test_poller_publishes_before_waiting_for_commands() {
        let mut poller = SessionPoller::new("test-session".to_string());
        let (cmd_tx, cmd_rx) = mpsc::channel();
        drop(cmd_tx);
        poller.cmd_rx = Some(cmd_rx);
        let (observed_tx, observed_rx) = mpsc::channel();

        assert_eq!(
            poller.start(
                "test-immediate".to_string(),
                Box::new(|| Some("ses_polled".to_string())),
                Box::new(move |id| observed_tx.send(id.to_string()).unwrap()),
                None,
            ),
            PollerSpawn::Spawned
        );
        poller.stop();

        assert_eq!(observed_rx.try_recv().unwrap(), "ses_polled");
    }
}
