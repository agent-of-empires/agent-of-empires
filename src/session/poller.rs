//! Adaptive polling interval and command channel for session monitoring

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, LazyLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Default ceiling on concurrent session-id poller threads.
///
/// Each session that loses its poller stops refreshing the agent session id
/// shown in its TUI row, so this cap doubles as a "how many concurrent
/// sessions can keep their identity live" budget. A fleet that registers
/// more live sessions than this raises it through
/// `[session] session_id_poller_max_threads`.
pub const DEFAULT_SESSION_ID_POLLER_MAX_THREADS: u32 = 50;

/// A budget of session-id poller threads: how many are running and the
/// ceiling they may not exceed.
///
/// One instance serves the whole process (`PROCESS_BUDGET`); tests that
/// assert exact counts pin a private one (`test_support::IsolatedBudget`)
/// so they neither observe nor disturb pollers started elsewhere.
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

/// The process-wide budget. Seeded with the default ceiling; the configured
/// value is applied once at startup by
/// [`configure_session_id_poller_max_threads`].
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
///
/// A value of 0 is treated as "unset" and keeps the default, so an empty or
/// zeroed config key can never freeze every session id.
pub fn configure_session_id_poller_max_threads(max: u32) {
    current_budget().set_max(max);
}

/// The current poller-thread ceiling.
pub fn session_id_poller_max_threads() -> u32 {
    current_budget().max()
}

/// The configured ceiling for a process launched under `profile`: the
/// global `[session] session_id_poller_max_threads` with that profile's
/// override applied, the way every other global-only session field is
/// consumed. The dashboard writes global-only core fields through
/// `PATCH /api/profiles/<name>/settings`, so a value saved from Settings
/// lives in the profile's `config.toml` and a global-file-only read would
/// miss it. The value is applied once at startup; a change needs a restart.
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

/// Retry schedule for one session whose session-id poller could not be
/// (re)started — typically because the process-wide thread budget is spent.
///
/// The daemon and the TUI both walk every registered session on a ~2 s
/// tick and ask `Instance::repair_session_id_poller_if_needed`
/// to replace a missing poller. Without a schedule, a fleet over budget
/// retries and warns for every over-budget session on every tick — the
/// 2026-09-04 fleet logged ~2.5 warning lines per second from two sessions.
/// This state lives on the instance (`#[serde(skip)]`) and is carried across
/// reloads like the poller handle itself.
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

    /// Record a failed (or skipped-for-budget) attempt at `now` and schedule
    /// the next one: 5 s, then doubling to a 60 s ceiling.
    ///
    /// Returns the new delay when the caller should log — on the first
    /// deferral, on every escalation, and as a periodic reminder once the
    /// delay is capped — and `None` for the quiet in-between deferrals.
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
///
/// Ensures the count is always decremented even if the poller thread panics,
/// preventing permanent budget exhaustion.
struct PollerCountGuard {
    budget: Arc<PollerBudget>,
}

impl Drop for PollerCountGuard {
    fn drop(&mut self) {
        self.budget.active.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Whether a poller could be spawned right now. Advisory: the real acquire
/// still happens in [`SessionPoller::start_observations`], and a racing spawn
/// can consume the last slot between the two. Callers use it to skip the
/// expensive spawn *setup* (a `sessions.json` load plus a `canonicalize` per
/// stored peer, in `retroactive_capture_exclusion_set`) when the acquire is
/// going to fail anyway. The TUI attempts a repair pass over every instance
/// twice a second, so once the budget is full that setup was stalling the
/// thread that also serves keystrokes for ~100ms per instance.
pub(crate) fn session_id_poller_budget_available() -> bool {
    let budget = current_budget();
    budget.active() < budget.max()
}

const POLL_INITIAL_INTERVAL: Duration = Duration::from_secs(2);
const POLL_MAX_INTERVAL: Duration = Duration::from_secs(60);
const POLL_BACKOFF_FACTOR: f64 = 1.5;
const POLL_STABLE_THRESHOLD: u32 = 3;

/// Outcome of [`SessionPoller::start`].
///
/// Only [`Spawned`](Self::Spawned) leaves a thread running. The other
/// variants are distinct so a caller can tell a spent budget (retry later)
/// from an OS spawn failure (warn) and from a duplicate start (ignore).
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
    ///
    /// Uses `Duration::from_secs_f64` for sub-second precision in the backoff calculation
    /// (e.g., 2.0s * 1.5 = 3.0s, 3.0s * 1.5 = 4.5s).
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
    /// Pre-launch-marker compat path: OMP sessions captured before the launch
    /// marker generation existed carry no generation to CAS against, so they
    /// persist unguarded. Kept so panes launched by an older aoe stay
    /// attributable across an upgrade; new launches always emit
    /// `OmpGeneration`. Removable once no unmarked OMP pane can outlive an
    /// upgrade.
    OmpLegacy,
    OmpGeneration(String),
    /// Read from a per-instance sidecar the agent itself wrote (Pi's
    /// extension), so the observation names this pane rather than being
    /// inferred from a store. Sources that cannot do that are refused for Pi;
    /// see `session::sync`.
    InstanceSidecar,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SessionIdObservation {
    pub(crate) sid: String,
    pub(crate) guard: SessionIdGuard,
}

pub(crate) type SessionIdPollFn =
    Box<dyn Fn(&str) -> Option<SessionIdObservation> + Send + 'static>;

impl SessionIdObservation {
    pub(crate) fn unguarded(sid: String) -> Self {
        Self {
            sid,
            guard: SessionIdGuard::Unguarded,
        }
    }

    pub(crate) fn instance_sidecar(sid: String) -> Self {
        Self {
            sid,
            guard: SessionIdGuard::InstanceSidecar,
        }
    }

    pub(crate) fn omp_legacy(sid: String) -> Self {
        Self {
            sid,
            guard: SessionIdGuard::OmpLegacy,
        }
    }
    pub(crate) fn omp(sid: String, generation: String) -> Self {
        Self {
            sid,
            guard: SessionIdGuard::OmpGeneration(generation),
        }
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

/// How long a poller tolerates a target tmux cannot resolve before treating it
/// as gone, when it has never seen the pane alive. A poller is started
/// alongside the tmux session, so the first probes can legitimately race the
/// session's registration; a target that is still unresolvable after this
/// window is a session that is never going to appear.
const MISSING_TARGET_GRACE: Duration = Duration::from_secs(60);

/// Tracks whether a poll target has become terminal for the poller.
///
/// A missing session must be terminal. `is_pane_dead` reports `false` for one
/// (tmux answers a `display-message` it cannot resolve with exit 0 and an
/// empty stdout), so a poller whose tmux session was killed never saw a stop
/// condition and polled forever, holding one of the 50 budget slots for the
/// life of the process. A daemon up for three days accumulated 32 such orphans
/// and starved every live session of a poller.
///
/// Two guards keep that from tearing down a poller that should live:
/// - a target only counts as terminal after it has held that state for two
///   consecutive ticks, so a tmux rename between resolution and probe (which
///   makes the old name look absent for one tick) is retried under the new
///   name;
/// - a *missing* target is only terminal once the pane has been seen alive, or
///   once [`MISSING_TARGET_GRACE`] has passed, so a poller started microseconds
///   before its tmux session registers is not reaped during that race.
///
/// Stopping is otherwise the safe direction: both the TUI and the daemon run a
/// repair pass that restarts a poller for any instance with a live tmux pane,
/// so a poller stopped in error comes back within a refresh cycle, while a
/// leaked budget slot never comes back.
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

    /// Fold one probe in, returning `(target_is_gone, should_stop)`. A gone
    /// target skips observation even on the debounce tick: there is nothing
    /// behind the name to read.
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

/// Resolve and observe one poll target, returning
/// `(target, should_stop, observation)`. Termination policy lives in
/// [`TargetLiveness`].
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
///
/// # Cleanup
///
/// Cleanup is performed explicitly via `stop()` rather than `Drop` because
/// `Drop` alone cannot guarantee prompt shutdown. The poller thread holds
/// the `cmd_rx` receiver; when `SessionPoller` drops, the corresponding
/// `cmd_tx` sender is dropped too and `recv_timeout` returns `Disconnected`
/// immediately; so in the common case the thread exits promptly.
///
/// `stop()` sends an explicit `PollCommand::Stop` and joins the thread,
/// providing a deterministic shutdown path for callers like `Instance::kill`
/// and `Instance::restart_with_size`.
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
    ///
    /// Returns [`PollerSpawn::Spawned`] when the thread is running; the other
    /// variants say why it is not (duplicate start, budget spent, or the OS
    /// refused the thread).
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
                // Rebind so the closure captures `_guard` and the counter only
                // decrements when the thread exits (including via panic). Without
                // this, `move` would not capture an unreferenced binding and the
                // counter would decrement as soon as `start()` returned.
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
                            // Capture the pane's final observable state before
                            // the owner joins and drains this poller. Restart
                            // relies on this boundary to preserve a session
                            // switch that landed just before teardown.
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

    /// Drain newly queued observations into a sticky one-slot mailbox and
    /// lease the newest value without consuming it. The value remains
    /// available to a concurrent stop/final flush until a terminal outcome
    /// acknowledges it.
    pub(crate) fn latest_observation(&mut self) -> Option<(String, SessionIdObservation)> {
        while let Some(observation) = self.try_recv_observation() {
            self.pending_observation = Some(observation);
        }
        self.pending_observation.clone()
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

    #[cfg(any(test, feature = "test-support"))]
    pub fn inject_test_update(&self, instance_id: &str, session_id: &str) {
        self.result_tx
            .send((
                instance_id.to_string(),
                SessionIdObservation::unguarded(session_id.to_string()),
            ))
            .expect("inject_test_update: result channel disconnected");
    }

    #[cfg(test)]
    pub(crate) fn inject_test_sidecar_update(&self, instance_id: &str, session_id: &str) {
        self.result_tx
            .send((
                instance_id.to_string(),
                SessionIdObservation::instance_sidecar(session_id.to_string()),
            ))
            .expect("inject_test_sidecar_update: result channel disconnected");
    }

    #[cfg(test)]
    pub(crate) fn inject_test_omp_update(
        &self,
        instance_id: &str,
        session_id: &str,
        generation: &str,
    ) {
        self.result_tx
            .send((
                instance_id.to_string(),
                SessionIdObservation::omp(session_id.to_string(), generation.to_string()),
            ))
            .expect("inject_test_omp_update: result channel disconnected");
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
///
/// `#[serial]` only coordinates tests that take the same lock; an
/// unannotated test that starts a real poller still shares the process
/// budget, so pinning that budget's counters would race it. A test that
/// needs exact counts pins a private [`PollerBudget`] for its own thread
/// instead: pollers it constructs draw from that budget, every other thread
/// keeps using the process one.
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
    use tracing_test::traced_test;

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

        // Raising the ceiling at runtime admits the waiting session.
        budget.set_active(50);
        configure_session_id_poller_max_threads(400);
        assert!(!budget.exhausted_now());
        assert!(budget.try_acquire().is_some());
    }

    #[test]
    fn zero_ceiling_keeps_the_default() {
        let _budget = test_support::IsolatedBudget::with_ceiling(7);
        configure_session_id_poller_max_threads(0);
        assert_eq!(
            session_id_poller_max_threads(),
            DEFAULT_SESSION_ID_POLLER_MAX_THREADS
        );
    }

    /// The ceiling a process applies is the effective one for its launch
    /// profile — global `[session]` plus that profile's override — the way
    /// every other global-only session field is consumed. The dashboard
    /// writes global-only core fields through
    /// `PATCH /api/profiles/<name>/settings`, so a read of the global file
    /// alone would miss a value saved from Settings.
    #[test]
    #[serial]
    fn configured_ceiling_is_the_launch_profiles_effective_value() {
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

        assert_eq!(configured_session_id_poller_max_threads("tuned"), 9);
        assert_eq!(
            configured_session_id_poller_max_threads("untouched"),
            7,
            "a profile without an override inherits the global value"
        );
    }

    /// A test pins a budget of its own: pollers created on its thread draw
    /// from it, pollers on any other thread keep drawing from the process
    /// budget, so exact-count assertions hold without a serial lock and a
    /// pinned ceiling never starves an unrelated test.
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

        // Another thread (an unannotated test, in practice) is unaffected by
        // this thread's pinned, exhausted budget.
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
    fn repair_backoff_doubles_to_a_minute_and_holds() {
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
    }

    #[test]
    fn repair_backoff_reminds_every_tenth_deferral_at_the_cap() {
        let mut b = PollerRepairBackoff::default();
        let now = Instant::now();
        // Walk to the cap: deferrals 1..=5 log (5,10,20,40,60).
        for _ in 0..5 {
            b.defer(now);
        }
        let mut logged_at = Vec::new();
        for _ in 0..25 {
            if b.defer(now).is_some() {
                logged_at.push(b.deferrals());
            }
        }
        assert_eq!(logged_at, vec![10, 20, 30]);
    }

    #[test]
    fn repair_backoff_reset_clears_the_schedule() {
        let mut b = PollerRepairBackoff::default();
        let now = Instant::now();
        b.defer(now);
        b.defer(now);
        assert!(!b.due(now));
        b.reset();
        assert_eq!(b, PollerRepairBackoff::default());
        assert!(b.due(now));
        assert_eq!(b.defer(now), Some(Duration::from_secs(5)), "restarts at 5s");
    }

    #[test]
    fn test_adaptive_interval_initial() {
        let interval =
            AdaptiveInterval::new(Duration::from_secs(2), Duration::from_secs(60), 1.5, 3);
        assert_eq!(interval.current(), Duration::from_secs(2));
    }

    #[test]
    fn test_adaptive_interval_record_no_change_increments_count() {
        let mut interval =
            AdaptiveInterval::new(Duration::from_secs(2), Duration::from_secs(60), 1.5, 3);
        assert_eq!(interval.stable_count, 0);
        interval.record_no_change();
        assert_eq!(interval.stable_count, 1);
        interval.record_no_change();
        assert_eq!(interval.stable_count, 2);
    }

    #[test]
    fn test_adaptive_interval_backoff_at_threshold() {
        let mut interval =
            AdaptiveInterval::new(Duration::from_secs(2), Duration::from_secs(60), 1.5, 3);
        interval.record_no_change();
        interval.record_no_change();
        interval.record_no_change();
        // After 3 calls: 2 * 1.5 = 3 seconds
        assert_eq!(interval.current(), Duration::from_secs(3));
        assert_eq!(interval.stable_count, 0);
    }

    #[test]
    fn test_adaptive_interval_multiple_backoffs() {
        let mut interval =
            AdaptiveInterval::new(Duration::from_secs(2), Duration::from_secs(60), 1.5, 3);
        // First backoff: 2 -> 3
        for _ in 0..3 {
            interval.record_no_change();
        }
        assert_eq!(interval.current(), Duration::from_secs(3));

        // Second backoff: 3 -> 4.5 (with sub-second precision)
        for _ in 0..3 {
            interval.record_no_change();
        }
        let expected_secs = 3.0 * 1.5;
        assert_eq!(interval.current(), Duration::from_secs_f64(expected_secs));
    }

    #[test]
    fn test_adaptive_interval_respects_max() {
        let mut interval = AdaptiveInterval::new(
            Duration::from_secs(2),
            Duration::from_secs(60),
            1.5,
            1, // threshold of 1 for faster test
        );
        interval.record_no_change(); // 2 * 1.5 = 3.0
        interval.record_no_change(); // 3.0 * 1.5 = 4.5
        interval.record_no_change(); // 4.5 * 1.5 = 6.75
        interval.record_no_change(); // 6.75 * 1.5 = 10.125
        interval.record_no_change(); // 10.125 * 1.5 = 15.1875
        interval.record_no_change(); // 15.1875 * 1.5 = 22.78125
        interval.record_no_change(); // 22.78125 * 1.5 = 34.171875
        interval.record_no_change(); // 34.171875 * 1.5 = 51.2578125
        interval.record_no_change(); // 51.2578125 * 1.5 = 76.88671875 > 60, capped at 60
        assert!(interval.current() <= Duration::from_secs(60));
    }

    #[test]
    fn test_adaptive_interval_record_change_resets() {
        let mut interval =
            AdaptiveInterval::new(Duration::from_secs(2), Duration::from_secs(60), 1.5, 3);
        for _ in 0..3 {
            interval.record_no_change();
        }
        assert_eq!(interval.current(), Duration::from_secs(3));

        interval.record_change();
        assert_eq!(interval.current(), Duration::from_secs(2));
        assert_eq!(interval.stable_count, 0);
    }

    #[test]
    fn test_session_poller_new() {
        let poller = SessionPoller::new("test-session".to_string());
        assert!(!poller.is_running());
    }

    #[test]
    fn test_session_poller_stop_when_no_thread() {
        let mut poller = SessionPoller::new("test-session".to_string());
        poller.stop(); // Should not panic
        assert!(!poller.is_running());
    }

    #[test]
    fn test_session_poller_double_stop_safe() {
        let mut poller = SessionPoller::new("test-session".to_string());
        poller.stop();
        poller.stop(); // Should not panic
        assert!(!poller.is_running());
    }

    #[test]
    fn test_session_poller_drop_is_clean() {
        let poller = SessionPoller::new("test-session".to_string());
        drop(poller); // Should not panic
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

    /// The orphan case: once the pane has been seen alive, a target
    /// tmux can no longer resolve terminates the poller, so its budget slot is
    /// released instead of being held for the life of the process. Before the
    /// startup grace expires, the same probe is tolerated: a poller can be
    /// started microseconds before its tmux session registers.
    #[test]
    fn missing_target_is_terminal_only_after_the_pane_was_seen_or_the_grace_passed() {
        use crate::tmux::utils::PaneProbe;
        let name = "aoe_Gone_12345678";
        let t0 = Instant::now();
        let after_grace = t0 + MISSING_TARGET_GRACE;

        // Never seen alive, inside the grace: keep polling.
        let mut fresh = TargetLiveness::new(t0);
        assert_eq!(fresh.record(name, PaneProbe::Missing, t0), (false, false));

        // Never seen alive, past the grace: the session is never going to
        // appear. Two consecutive ticks to stop.
        let mut stale = TargetLiveness::new(t0);
        assert_eq!(
            stale.record(name, PaneProbe::Missing, after_grace),
            (true, false)
        );
        assert_eq!(
            stale.record(name, PaneProbe::Missing, after_grace),
            (true, true)
        );

        // Seen alive, then killed: terminal immediately (still debounced).
        let mut killed = TargetLiveness::new(t0);
        assert_eq!(killed.record(name, PaneProbe::Alive, t0), (false, false));
        assert_eq!(killed.record(name, PaneProbe::Missing, t0), (true, false));
        assert_eq!(killed.record(name, PaneProbe::Missing, t0), (true, true));

        // A probe that could not run says nothing about the pane, and must not
        // reap a poller for a session that is alive.
        let mut unknown = TargetLiveness::new(t0);
        assert_eq!(unknown.record(name, PaneProbe::Alive, t0), (false, false));
        assert_eq!(
            unknown.record(name, PaneProbe::Unknown, after_grace),
            (false, false)
        );

        // A recovered tick resets the debounce rather than carrying it, so an
        // intermittently unresolvable target never accumulates toward a stop.
        let mut flaky = TargetLiveness::new(t0);
        assert_eq!(flaky.record(name, PaneProbe::Alive, t0), (false, false));
        assert_eq!(flaky.record(name, PaneProbe::Missing, t0), (true, false));
        assert_eq!(flaky.record(name, PaneProbe::Alive, t0), (false, false));
        assert_eq!(flaky.record(name, PaneProbe::Missing, t0), (true, false));
    }

    #[test]
    fn test_adaptive_interval_with_constants() {
        let mut interval = AdaptiveInterval::new(
            POLL_INITIAL_INTERVAL,
            POLL_MAX_INTERVAL,
            POLL_BACKOFF_FACTOR,
            POLL_STABLE_THRESHOLD,
        );
        assert_eq!(interval.current(), Duration::from_secs(2));
        for _ in 0..POLL_STABLE_THRESHOLD {
            interval.record_no_change();
        }
        assert_eq!(interval.current(), Duration::from_secs(3));
    }

    #[test]
    fn test_poller_detects_change() {
        let call_count = Arc::new(Mutex::new(0u32));
        let call_count_clone = call_count.clone();

        let poll_fn: Box<dyn Fn() -> Option<String> + Send + 'static> = Box::new(move || {
            let mut count = lock_unpoisoned(&call_count_clone);
            *count += 1;
            if *count <= 1 {
                Some("id-1".to_string())
            } else {
                Some("id-2".to_string())
            }
        });

        let changed_ids: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let changed_ids_clone = changed_ids.clone();
        let on_change: Box<dyn Fn(&str) + Send + 'static> = Box::new(move |id: &str| {
            lock_unpoisoned(&changed_ids_clone).push(id.to_string());
        });

        let mut poller = SessionPoller::new("test-session".to_string());
        poller.start(
            "test-change".to_string(),
            poll_fn,
            on_change,
            Some("id-1".to_string()),
        );

        // Wait for the change rather than for the 2s interval to elapse: each
        // tick forks tmux, so a fixed sleep races that under load. Same reason
        // `test_poller_starts_polling_immediately` polls.
        let observed = |want: usize| {
            for _ in 0..200 {
                if lock_unpoisoned(&changed_ids).len() >= want {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            false
        };
        assert!(observed(1), "the poller must observe the changed id");
        poller.retry_last_observation();
        assert!(
            observed(2),
            "a retry must be able to re-emit the same observation"
        );
        poller.stop();

        let ids = lock_unpoisoned(&changed_ids);
        assert!(
            !ids.contains(&"id-1".to_string()),
            "on_change should NOT have been called with id-1 (initial known)"
        );
        assert_eq!(
            ids.iter().filter(|id| id.as_str() == "id-2").count(),
            2,
            "a failed durable write must be able to request the same observation again"
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
        // The advisory pre-check callers use to skip spawn setup must agree
        // with the acquire that just failed; a disagreement puts the ~100ms
        // exclusion-set build back on the TUI's repair pass.
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

    /// The repair path (`defer_poller_repair`) owns the exhausted-budget
    /// warning and throttles it; `start` itself must stay below WARN or
    /// every deferred attempt at the cap warns again.
    #[traced_test]
    #[test]
    fn test_budget_exhaustion_leaves_the_warning_to_the_repair_path() {
        tracing::callsite::rebuild_interest_cache();
        let _budget = test_support::IsolatedBudget::exhausted();

        let mut poller = SessionPoller::new("test-session".to_string());
        let outcome = poller.start(
            "test-budget-quiet".to_string(),
            Box::new(|| Some("id".to_string())),
            Box::new(|_| {}),
            None,
        );
        assert_eq!(outcome, PollerSpawn::BudgetExhausted);

        logs_assert(|lines: &[&str]| {
            let warned = lines
                .iter()
                .filter(|l| l.contains("WARN"))
                .filter(|l| l.contains("test-budget-quiet"))
                .count();
            match warned {
                0 => Ok(()),
                n => Err(format!("start warned {n} time(s) on an exhausted budget")),
            }
        });
    }

    #[test]
    #[serial]
    fn test_poller_is_running_after_start() {
        let mut poller = SessionPoller::new("test-session".to_string());
        let outcome = poller.start(
            "test-running".to_string(),
            Box::new(|| {
                std::thread::sleep(Duration::from_millis(10));
                Some("id".to_string())
            }),
            Box::new(|_| {}),
            None,
        );

        assert_eq!(outcome, PollerSpawn::Spawned);
        assert!(poller.is_running(), "poller should be running after start");
        poller.stop();
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
        let budget =
            test_support::IsolatedBudget::with_ceiling(DEFAULT_SESSION_ID_POLLER_MAX_THREADS);
        let poll_count = Arc::new(Mutex::new(0u32));
        let poll_count_clone = poll_count.clone();

        let mut poller = SessionPoller::new("test-session".to_string());
        poller.start(
            "test-cleanup".to_string(),
            Box::new(move || {
                *lock_unpoisoned(&poll_count_clone) += 1;
                Some("id".to_string())
            }),
            Box::new(|_| {}),
            None,
        );

        // Wait for the immediate first poll to run
        std::thread::sleep(Duration::from_millis(100));

        let count_before_stop = budget.active();
        poller.stop();
        let count_after_stop = budget.active();

        assert!(
            count_after_stop < count_before_stop,
            "counter should decrement after stop (before_stop={}, after_stop={})",
            count_before_stop,
            count_after_stop
        );
        assert!(
            *lock_unpoisoned(&poll_count) >= 2,
            "stop must perform a final poll after the immediate first poll"
        );
    }

    #[test]
    fn test_interval_exact_at_threshold() {
        let mut interval = AdaptiveInterval::new(
            Duration::from_secs(2),
            Duration::from_secs(60),
            1.5,
            POLL_STABLE_THRESHOLD,
        );

        for _ in 0..POLL_STABLE_THRESHOLD {
            interval.record_no_change();
        }
        // 2 * 1.5 = 3
        assert_eq!(interval.current(), Duration::from_secs(3));
        assert_eq!(interval.stable_count, 0);

        interval.record_no_change();
        assert_eq!(interval.current(), Duration::from_secs(3));
        assert_eq!(interval.stable_count, 1);
    }

    #[test]
    fn test_interval_max_clamping_precision() {
        let mut interval = AdaptiveInterval::new(
            Duration::from_secs(2),
            POLL_MAX_INTERVAL,
            POLL_BACKOFF_FACTOR,
            POLL_STABLE_THRESHOLD,
        );

        for _ in 0..1000 {
            interval.record_no_change();
            assert!(
                interval.current() <= POLL_MAX_INTERVAL,
                "interval {} exceeded max {}",
                interval.current().as_secs(),
                POLL_MAX_INTERVAL.as_secs()
            );
        }
        assert_eq!(interval.current(), POLL_MAX_INTERVAL);
    }

    #[test]
    fn test_interval_change_mid_backoff() {
        let mut interval = AdaptiveInterval::new(
            Duration::from_secs(2),
            Duration::from_secs(60),
            1.5,
            POLL_STABLE_THRESHOLD,
        );

        interval.record_no_change();
        interval.record_no_change();
        assert_eq!(interval.stable_count, 2);
        assert_eq!(interval.current(), Duration::from_secs(2));

        interval.record_change();
        assert_eq!(interval.current(), Duration::from_secs(2));
        assert_eq!(interval.stable_count, 0);
    }

    #[test]
    fn test_poller_starts_polling_immediately() {
        let poll_count = Arc::new(Mutex::new(0u32));
        let poll_count_clone = poll_count.clone();

        let poll_fn: Box<dyn Fn() -> Option<String> + Send + 'static> = Box::new(move || {
            let mut count = lock_unpoisoned(&poll_count_clone);
            *count += 1;
            Some("ses_polled".to_string())
        });

        let on_change: Box<dyn Fn(&str) + Send + 'static> = Box::new(|_| {});

        let mut poller = SessionPoller::new("test-session".to_string());
        poller.start("test-immediate".to_string(), poll_fn, on_change, None);

        // Wait for the first poll rather than sleeping a fixed window: the
        // tick forks tmux twice (name resolution, pane probe), and against a
        // socket that is not there those forks can outlast a 100ms sleep,
        // which made this assertion flake.
        let mut count = 0;
        for _ in 0..100 {
            count = *lock_unpoisoned(&poll_count);
            if count > 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            count > 0,
            "poller should have started polling immediately (count={})",
            count
        );

        poller.stop();
    }
}
