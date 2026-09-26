//! Background status polling so tmux subprocess calls stay off the UI thread.
//!
//! One `tmux list-panes -a` call fetches pane metadata for every session at
//! once, and sessions are polled on adaptive tiers: hot (Running/Waiting/
//! Starting) every cycle, warm (Idle/Unknown) every 5, cold (Error) every 60,
//! frozen (Stopped/Deleting) never.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use crate::session::{DetectionState, Instance, Status};
use crate::tui::worker::Worker;

/// Adaptive polling intervals (in cycles). 0 = never poll.
const TIER_HOT: u64 = 1;
const TIER_WARM: u64 = 5;
const TIER_COLD: u64 = 60;

fn polling_tier(status: Status) -> u64 {
    match status {
        Status::Running | Status::Waiting | Status::Starting => TIER_HOT,
        Status::Idle | Status::Unknown => TIER_WARM,
        Status::Error => TIER_COLD,
        Status::Stopped | Status::Deleting | Status::Creating => 0,
    }
}

/// A producer's report of what to do with an `Instance`'s `idle_entered_at`,
/// distinguishing the three intents `Option<DateTime<Utc>>` conflates: a
/// transition into `Idle` at a timestamp, a transition out of it (the disk value
/// resets), and no observation at all (the disk value must not be touched, or a
/// transition seen on another path is silently clobbered).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum IdleIntent {
    /// Producer observed `Idle` at the carried timestamp; consumer sets
    /// the field to `Some(ts)`.
    Set(DateTime<Utc>),
    /// Producer observed a non-`Idle` status; consumer sets the field to
    /// `None`.
    Clear,
    /// Producer has no observation; consumer preserves the current value.
    #[default]
    Keep,
}

/// Result of a status check for a single session. `Default` is derived so test
/// fixtures can set only the fields under test.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct StatusUpdate {
    pub id: String,
    pub status: Status,
    pub last_error: Option<String>,
    /// Producer's intent for the real `Instance`'s `idle_entered_at`; see
    /// [`IdleIntent`].
    pub idle_entered_at: IdleIntent,
    /// Pulled from tmux `#{session_activity}`. Carried back so the main thread
    /// can persist it: the poller only mutates a clone.
    pub last_accessed_at: Option<DateTime<Utc>>,
    /// Cached `tmux::PaneMetadata.pane_dead`, written onto
    /// `Instance.pane_dead_observed` so the Attention sort can treat dead panes
    /// as tier 99 without re-querying tmux.
    pub pane_dead: bool,
    /// Snapshot of the polled clone's `live_status_baseline`. `None` from a
    /// producer that has none yet must not clear an established baseline, so the
    /// consumer applies this conditionally (#2690).
    pub live_status_baseline: Option<Status>,
    /// The polled clone's [`DetectionState`]. `None` means no detection was
    /// reached, so the consumer keeps what the real `Instance` holds; dropping it
    /// left a proposed `Running -> Idle` waiting on a poll that could never see
    /// it (#3642).
    pub detection: Option<DetectionState>,
}

pub(super) struct StatusPollState {
    container_check_interval: Duration,
    last_container_check: Instant,
    container_states: HashMap<String, bool>,
    credential_refresh_interval: Duration,
    last_credential_refresh: Instant,
    cycle_count: u64,
}

impl StatusPollState {
    pub(super) fn new() -> Self {
        let container_check_interval = Duration::from_secs(5);
        let credential_refresh_interval = Duration::from_secs(1800);

        Self {
            container_check_interval,
            last_container_check: Instant::now() - container_check_interval,
            container_states: HashMap::new(),
            credential_refresh_interval,
            last_credential_refresh: Instant::now(),
            cycle_count: TIER_COLD - 1,
        }
    }
}

pub(super) fn poll_statuses_once(
    instances: Vec<Instance>,
    state: &mut StatusPollState,
) -> Vec<StatusUpdate> {
    state.cycle_count = state.cycle_count.wrapping_add(1);

    // Pre-scan: check if any instance would actually be polled this cycle.
    // If not, skip the batch subprocess calls entirely.
    let any_pollable = instances.iter().any(|inst| {
        let tier = polling_tier(inst.status);
        tier != 0 && state.cycle_count % tier == 0
    });

    let pane_metadata = if any_pollable {
        crate::tmux::refresh_session_cache();
        match crate::tmux::refresh_pane_meta_cache() {
            Ok(metadata) => metadata,
            Err(error) => {
                tracing::warn!(
                    target: "session.status",
                    %error,
                    "skipping status refresh because tmux pane metadata is unavailable",
                );
                return Vec::new();
            }
        }
    } else {
        std::sync::Arc::new(HashMap::new())
    };

    // Refresh container health if any sandboxed session exists and interval elapsed
    let has_sandboxed = if any_pollable {
        let sandboxed = instances.iter().any(|i| i.is_sandboxed());
        if sandboxed && state.last_container_check.elapsed() >= state.container_check_interval {
            state.container_states = crate::containers::batch_container_health();
            state.last_container_check = Instant::now();
        }
        sandboxed
    } else {
        false
    };

    // Periodically seed a shared credential file that holds no
    // credential, and refresh the rest of each store. A file holding one
    // is left to the containers' own rotation.
    if has_sandboxed && state.last_credential_refresh.elapsed() >= state.credential_refresh_interval
    {
        state.last_credential_refresh = Instant::now();
        for instance in instances.iter().filter(|inst| {
            inst.is_sandboxed()
                && inst.sandbox_store_generation
                    >= crate::session::config::container_config::CURRENT_SANDBOX_STORE_GENERATION
        }) {
            // A running container built before its agent shared a credential
            // file is still rotating the copy in its store; folding that in
            // would log every sandbox on the shared file out.
            let container = crate::containers::DockerContainer::from_session_id(&instance.id);
            // An absent name is unknown, not stopped: the map is empty for a
            // runtime that cannot list states and for a failed list. Only a
            // container known to be stopped skips the check.
            let stopped = state
                .container_states
                .get(&crate::containers::DockerContainer::generate_name(
                    &instance.id,
                ))
                .copied()
                == Some(false);
            if !stopped {
                match instance.predates_shared_credential(&container, instance.get_tool_command()) {
                    Ok(false) => {}
                    Ok(true) => continue,
                    Err(error) => {
                        tracing::warn!(target: "session.profile",
                            "Skipping credential refresh for {}: {error:#}", instance.id);
                        continue;
                    }
                }
            }
            crate::session::config::container_config::refresh_agent_configs_for_instance(
                &instance.effective_profile(),
                &instance.id,
                &instance.tool,
                instance.get_tool_command().into(),
                crate::session::config::container_config::CredentialFold::SeedOnly,
                std::path::Path::new(&instance.container_workdir()),
            );
        }
    }

    project_status_updates(
        instances,
        state.cycle_count,
        &pane_metadata,
        &state.container_states,
    )
}

fn project_status_updates(
    instances: Vec<Instance>,
    cycle_count: u64,
    pane_metadata: &HashMap<String, crate::tmux::PaneMetadata>,
    container_states: &HashMap<String, bool>,
) -> Vec<StatusUpdate> {
    instances
        .into_iter()
        .filter_map(|mut inst| {
            // Adaptive polling: skip instances whose tier interval hasn't elapsed
            let tier = polling_tier(inst.status);
            if tier == 0 || cycle_count % tier != 0 {
                return None;
            }

            // Structured (ACP) rows have no tmux pane, and their sandbox
            // container belongs to the worker rather than a pane, so neither
            // probe below can say anything true about them: the daemon derives
            // their status from ACP events and `SessionFeed` carries it in.
            //
            // Bailing before the sandbox-dead branch is deliberate. That branch
            // used to pin sandboxed structured rows to a red `Error` with a
            // stale "Container is not running" message (cleaned up once by the
            // v023 migration). It also gives up the `Error -> Idle` heal, which
            // existed to undo tmux-derived errors this branch no longer
            // produces; the remaining source of `Error` here is the daemon
            // reporting `AgentStartupError`, a real failure that should stay
            // visible until the next daemon reading or an explicit stop/start.
            if inst.is_structured() {
                return None;
            }

            // For sandboxed sessions, check if the container is dead before
            // falling through to tmux-based status detection.
            if inst.is_sandboxed()
                && !matches!(
                    inst.status,
                    Status::Stopped | Status::Deleting | Status::Starting | Status::Creating
                )
            {
                if let Some(sandbox) = &inst.sandbox_info {
                    if let Some(&running) = container_states.get(&sandbox.container_name) {
                        if !running {
                            return Some(StatusUpdate {
                                id: inst.id,
                                status: Status::Error,
                                last_error: Some("Container is not running".to_string()),
                                idle_entered_at: IdleIntent::Clear,
                                last_accessed_at: inst.last_accessed_at,
                                // Sandboxed sessions don't have a tmux pane in the
                                // usual sense; the Error tier itself sinks the row.
                                pane_dead: false,
                                live_status_baseline: Some(Status::Error),
                                // No detection ran on this branch.
                                detection: None,
                            });
                        }
                    }
                }
            }

            // Look up pre-fetched metadata for this instance's tmux session,
            // resolving the name against this same snapshot so a session whose
            // title moved without its tmux session being renamed is still
            // found (and not reported as Error for a live pane).
            let session_name = crate::tmux::resolve_agent_session_name_in(
                pane_metadata,
                &inst.id,
                &crate::tmux::Session::generate_name(&inst.id, &inst.title),
            );
            let metadata = pane_metadata.get(&session_name);
            let pane_dead = metadata.map(|m| m.pane_dead).unwrap_or(false);

            let prev_status = inst.status;
            inst.update_status_with_metadata(metadata, Some(&session_name));
            // On the first turn's `Running -> Idle` edge, best-effort auto-name a
            // still-default-named terminal session from its first turn. Detached
            // and self-gating, so this is cheap for the common (ineligible) case.
            if prev_status == Status::Running && inst.status == Status::Idle {
                crate::session::smart_rename::maybe_spawn_terminal_smart_rename(&inst);
            }

            Some(StatusUpdate {
                id: inst.id,
                status: inst.status,
                last_error: inst.last_error,
                // This producer is authoritative on `idle_entered_at` and never
                // emits `IdleIntent::Keep`; `attached_status_hooks::snapshot` is
                // the sole `Keep`-emitter. Adding `Keep` here would erase the
                // baseline seed `update_status_with_metadata` writes.
                idle_entered_at: match inst.idle_entered_at {
                    Some(ts) => IdleIntent::Set(ts),
                    None => IdleIntent::Clear,
                },
                last_accessed_at: inst.last_accessed_at,
                pane_dead,
                live_status_baseline: inst.live_status_baseline,
                detection: Some(inst.detection),
            })
        })
        .collect()
}

pub struct StatusPoller {
    worker: Worker<Vec<Instance>, Vec<StatusUpdate>>,
}

impl StatusPoller {
    pub fn new() -> Self {
        // The adaptive-tier state lives in the handler closure so it carries
        // across refresh cycles for the lifetime of the worker thread.
        let mut state = StatusPollState::new();
        Self {
            worker: Worker::spawn("aoe-status-poller", move |instances| {
                poll_statuses_once(instances, &mut state)
            }),
        }
    }

    /// Request a status refresh for all given instances (non-blocking).
    pub fn request_refresh(&self, instances: Vec<Instance>) {
        self.worker.request(instances);
    }

    /// Try to receive status updates without blocking. Surfaces `Disconnected`
    /// so the caller can respawn the worker; swallowing it would freeze every
    /// session's live status.
    pub fn try_recv_updates(&self) -> Result<Vec<StatusUpdate>, std::sync::mpsc::TryRecvError> {
        self.worker.try_recv()
    }
}

impl Default for StatusPoller {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #2690 follow-up: `StatusUpdate::default()` must be a semantic no-op so
    /// fixtures can override only the fields under test. A future field with a
    /// non-trivial default would silently corrupt them.
    #[test]
    fn test_status_update_default_is_no_op() {
        let default = StatusUpdate::default();
        assert_eq!(default.id, String::new(), "id defaults to empty string");
        assert_eq!(default.status, Status::Idle, "status defaults to Idle");
        assert_eq!(
            default.last_error, None,
            "last_error defaults to None (no error observed)"
        );
        assert_eq!(
            default.idle_entered_at,
            IdleIntent::Keep,
            "idle_entered_at defaults to Keep (no observation)"
        );
        assert_eq!(
            default.last_accessed_at, None,
            "last_accessed_at defaults to None (no observation to carry back)"
        );
        assert!(
            !default.pane_dead,
            "pane_dead defaults to false (no dead-pane observation)"
        );
        assert_eq!(
            default.live_status_baseline, None,
            "live_status_baseline defaults to None (no baseline observed yet)"
        );
        assert_eq!(
            default.detection, None,
            "detection defaults to None (producer never detected)"
        );
    }

    #[test]
    fn polling_tiers_and_first_cycle_alignment() {
        for (status, tier) in [
            (Status::Running, TIER_HOT),
            (Status::Waiting, TIER_HOT),
            (Status::Starting, TIER_HOT),
            (Status::Idle, TIER_WARM),
            (Status::Unknown, TIER_WARM),
            (Status::Error, TIER_COLD),
            (Status::Stopped, 0),
            (Status::Deleting, 0),
        ] {
            assert_eq!(polling_tier(status), tier, "{status:?}");
        }
        // Hot sessions poll every cycle.
        assert_eq!(TIER_HOT, 1);
        // cycle_count starts at TIER_COLD - 1, so the first cycle polls every tier.
        let first_cycle = (TIER_COLD - 1).wrapping_add(1);
        assert_eq!(first_cycle % TIER_WARM, 0, "first cycle must poll warm");
        assert_eq!(first_cycle % TIER_COLD, 0, "first cycle must poll cold");
    }

    #[test]
    #[serial_test::serial]
    fn poll_statuses_once_never_emits_idle_intent_keep() {
        let _home = crate::session::test_support::isolate_app_dir();
        // This is the projection shared by the native poller, after acquisition.
        let mut running = Instance::new("running", "/tmp/running");
        running.status = Status::Running;
        let mut idle = Instance::new("idle", "/tmp/idle");
        idle.status = Status::Idle;
        idle.idle_entered_at = Some(Utc::now() - chrono::Duration::minutes(5));
        let mut error = Instance::new("error", "/tmp/error");
        error.status = Status::Error;

        let expected_ids = vec![running.id.clone(), idle.id.clone(), error.id.clone()];
        let updates = project_status_updates(
            vec![running, idle, error],
            TIER_COLD,
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(
            updates
                .iter()
                .map(|update| update.id.clone())
                .collect::<Vec<_>>(),
            expected_ids
        );
        for update in updates {
            assert!(
                !matches!(update.idle_entered_at, IdleIntent::Keep),
                "poll_statuses_once must never emit IdleIntent::Keep (session {}); got {:?}",
                update.id,
                update.idle_entered_at
            );
        }
    }

    fn dead_container_sandbox(name: &str) -> crate::session::SandboxInfo {
        crate::session::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "ubuntu:latest".to_string(),
            container_name: name.to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        }
    }

    fn dead_container_states(name: &str) -> HashMap<String, bool> {
        HashMap::from([(name.to_string(), false)])
    }

    #[test]
    #[serial_test::serial]
    fn structured_rows_never_get_a_container_error() {
        // The sandbox-dead branch used to run before the `is_structured()` bail,
        // landing a red `Error` with "Container is not running" on structured
        // rows. A structured session's container belongs to its ACP worker, not
        // a tmux pane, so that reading says nothing about the session.
        let mut inst = Instance::new("structured-sandboxed", "/tmp/structured");
        inst.view = crate::session::View::Structured;
        inst.status = Status::Idle;
        inst.sandbox_info = Some(dead_container_sandbox("aoe-sandbox-structured"));

        let updates = project_status_updates(
            vec![inst],
            TIER_COLD,
            &HashMap::new(),
            &dead_container_states("aoe-sandbox-structured"),
        );

        assert!(
            updates.is_empty(),
            "structured rows must produce no tmux/docker status update; got {updates:?}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn terminal_sandboxed_rows_still_get_a_container_error() {
        // The other half of the guard above: bailing on structured rows must
        // not disarm the sandbox-dead branch for terminal ones, where a dead
        // container really does mean the session is broken.
        let mut inst = Instance::new("terminal-sandboxed", "/tmp/terminal");
        inst.status = Status::Idle;
        inst.sandbox_info = Some(dead_container_sandbox("aoe-sandbox-terminal"));

        let updates = project_status_updates(
            vec![inst],
            TIER_COLD,
            &HashMap::new(),
            &dead_container_states("aoe-sandbox-terminal"),
        );

        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].status, Status::Error);
        assert_eq!(
            updates[0].last_error.as_deref(),
            Some("Container is not running")
        );
    }
}
